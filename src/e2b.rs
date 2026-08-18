use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tar::{Builder, Header};
use uuid::Uuid;

use crate::acceptance;
use crate::config::{AgentLabConfig, BackendDriver, SelectedBackend};
use crate::rootfs::{self, RootFsEntry};
use crate::run::{
    self, Artifact, E2bEvidence, E2bSnapshotEvidence, IgnoredChange, ObservationStatus,
    ResourceLimits, RunOptions, RunResult, RunSpec, RunSummary, WorkspaceSource,
};
use crate::snapshot;
use crate::store::{Store, hex_digest};

const REMOTE_HELPER: &[u8] = include_bytes!("e2b_helper.mjs");
const REMOTE_SCANNER: &[u8] = include_bytes!("e2b_snapshot.py");
const REMOTE_CONTROL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const REMOTE_RUN_TIMEOUT: Duration = Duration::from_secs(25 * 60 * 60);
const REMOTE_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
const E2B_GUEST_TIMEOUT_SECONDS: u64 = 58 * 60;
const E2B_SANDBOX_TIMEOUT_MS: u64 = 60 * 60 * 1000;

#[derive(Debug, Deserialize, Serialize)]
struct ProbeResult {
    sdk_version: String,
    template: String,
    template_build_id: String,
    isolation: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RemoteSnapshot {
    snapshot_id: String,
    build_id: String,
    names: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RemoteCommandResult {
    exit_code: i64,
    timed_out: bool,
    stdout_total_bytes: u64,
    stderr_total_bytes: u64,
    stdout_retained_bytes: u64,
    stderr_retained_bytes: u64,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct RemoteRunResult {
    sdk_version: String,
    template: String,
    template_build_id: String,
    sandbox_id: String,
    architecture: String,
    base_snapshot: RemoteSnapshot,
    result_snapshot: RemoteSnapshot,
    command: RemoteCommandResult,
    sandbox_info: Value,
    final_sandbox_info: Value,
}

struct RemoteSupport {
    alias: String,
    sdk_directory: String,
    remote_root: String,
    helper: String,
    scanner: String,
    mount_binary: String,
}

struct FailedRunDirectoryCleanup {
    store: Store,
    run_id: String,
    armed: bool,
}

impl Drop for FailedRunDirectoryCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.store.remove_run_directory(&self.run_id);
        }
    }
}

struct RemoteRunCleanup {
    profile: SelectedBackend,
    staging: String,
    snapshots: Vec<RemoteSnapshot>,
    armed: bool,
}

struct ChildProcessGuard {
    child: Child,
    armed: bool,
}

impl ChildProcessGuard {
    fn new(child: Child) -> Self {
        Self { child, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ChildProcessGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = crate::process::terminate_process_group(&mut self.child);
            let _ = self.child.wait();
        }
    }
}

impl Drop for RemoteRunCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = delete_snapshots(&self.profile, &self.snapshots);
        let _ = remove_remote_staging(&self.profile, &self.staging);
    }
}

pub(crate) fn execute_with_observer(
    options: &RunOptions,
    store: &Store,
    observer: &mut dyn run::RunObserver,
    profile: &SelectedBackend,
) -> Result<RunSummary> {
    run::validate_options(options)?;
    if options.memory.is_some() || options.cpus.is_some() {
        bail!(
            "the E2B backend does not silently translate --memory or --cpus; choose a mapped E2B template with the required resources"
        );
    }

    let started_at = Utc::now();
    let run_id = Uuid::new_v4().to_string();
    let run_directory = store.create_run_directory(&run_id)?;
    let mut failed_run_directory_cleanup = FailedRunDirectoryCleanup {
        store: store.clone(),
        run_id: run_id.clone(),
        armed: true,
    };
    let mut lifecycle = vec![run::event("run_created")];
    run::report_stage(observer, &format!("Run created: {run_id}"))?;

    let (workspace_manifest, workspace_warnings) = match &options.workspace {
        WorkspaceSource::Directory(workspace) => {
            run::report_stage(
                observer,
                &format!(
                    "Capturing workspace ({}): {}",
                    options.workspace_capture_mode.as_str(),
                    workspace.display()
                ),
            )?;
            let captured =
                snapshot::create_with_mode(workspace, store, options.workspace_capture_mode)?;
            lifecycle.push(run::event("workspace_snapshotted"));
            run::report_stage(
                observer,
                &format!(
                    "Workspace captured: {} paths, {} bytes, excluded {} ({})",
                    captured.included_paths,
                    captured.logical_bytes,
                    captured.excluded_paths,
                    captured.manifest.digest
                ),
            )?;
            (captured.manifest, captured.warnings)
        }
        WorkspaceSource::Snapshot(digest) => {
            run::report_stage(observer, &format!("Verifying workspace snapshot: {digest}"))?;
            let manifest = snapshot::load(store, digest)?;
            snapshot::verify(store, &manifest)?;
            lifecycle.push(run::event("workspace_snapshot_loaded"));
            run::report_stage(
                observer,
                &format!(
                    "Workspace snapshot verified: {} paths, {}",
                    manifest.entries.len(),
                    manifest.digest
                ),
            )?;
            (manifest, Vec::new())
        }
    };

    run::report_stage(observer, &format!("Checking E2B backend: {}", profile.name))?;
    let support = ensure_remote_support(profile, observer)?;
    let template = profile.config.e2b_template(&options.image)?.to_owned();
    let probe_request = json!({
        "action": "probe",
        "sdk_directory": support.sdk_directory,
        "template": template,
        "expected_template_build": profile.config.expected_template_build(&options.image),
    });
    let probe: ProbeResult = serde_json::from_value(invoke_helper(
        &support,
        &probe_request,
        observer,
        REMOTE_CONTROL_TIMEOUT,
    )?)
    .context("decode E2B backend probe")?;
    validate_probe(
        &probe,
        &template,
        profile.config.expected_template_build(&options.image),
    )?;
    lifecycle.push(run::event("environment_resolved"));
    run::report_stage(
        observer,
        &format!(
            "Environment resolved: {} (build {})",
            probe.template, probe.template_build_id
        ),
    )?;

    let (change_ignore, change_ignore_rules) =
        run::resolve_change_ignore(options, &workspace_manifest, store)?;
    let runtime_environment = profile.config.runtime_environment(&options.image);
    let runtime_environment_bytes =
        serde_json::to_vec(&runtime_environment).context("encode E2B runtime environment")?;
    let runtime_environment_digest = run::sha256_bytes(&runtime_environment_bytes);
    let native_environment = format!(
        "{}@build:{}+runtime-env:{}",
        probe.template, probe.template_build_id, runtime_environment_digest
    );
    let image_resolved_digest = format!(
        "sha256:{}",
        hex_digest(&Sha256::digest(native_environment.as_bytes()))
    );
    let mut spec = RunSpec {
        schema_version: run::E2B_RUN_SCHEMA_VERSION.to_owned(),
        run_id: run_id.clone(),
        accepted_input: options.accepted_input.clone(),
        run_input_digest: String::new(),
        workspace_snapshot_digest: workspace_manifest.digest.clone(),
        image_requested: options.image.clone(),
        image_resolved_digest: image_resolved_digest.clone(),
        docker_image_id: String::new(),
        backend_profile: Some(profile.name.clone()),
        backend_driver: Some("e2b".to_owned()),
        backend_native_environment: Some(native_environment.clone()),
        target_platform: "linux/amd64".to_owned(),
        workspace_guest_path: options.workspace_guest_path.clone(),
        command: options.command.clone(),
        working_directory: options.workspace_guest_path.clone(),
        legacy_factors: BTreeMap::new(),
        resource_limits: ResourceLimits {
            memory: None,
            cpus: None,
        },
        network_policy: options.network.clone(),
        captures: options.captures.clone(),
        secret_injections: run::secret_injection_names(options),
        workspace_ignore_digest: workspace_manifest.ignore_rules_digest.clone(),
        change_ignore: change_ignore.clone(),
        backend_name: "e2b-sdk".to_owned(),
        backend_version: probe.sdk_version.clone(),
        agentlab_version: crate::build_version(),
    };
    if let Some(reference) = &spec.accepted_input {
        acceptance::verify_run_input(
            store,
            reference,
            &spec.workspace_snapshot_digest,
            &spec.image_resolved_digest,
            &spec.target_platform,
            &spec.workspace_guest_path,
            &spec.workspace_ignore_digest,
        )?;
        lifecycle.push(run::event("accepted_input_verified"));
    }
    spec.run_input_digest = run::compute_run_input_digest(&spec)?;
    let spec_bytes = run::pretty_json(&spec)?;
    let spec_digest = run::sha256_bytes(&spec_bytes);
    store.write_run_file(&run_id, "spec.json", &spec_bytes)?;
    let environment_evidence = json!({
        "schema_version": "agentlab.e2b-environment/v1",
        "probe": &probe,
        "runtime_environment": &runtime_environment,
        "runtime_environment_digest": &runtime_environment_digest,
    });
    let environment_bytes = run::pretty_json(&environment_evidence)?;
    store.write_run_file(&run_id, "evidence/environment.json", &environment_bytes)?;
    if let Some(rules) = &change_ignore_rules {
        store.write_run_file(&run_id, "change-ignore.rules", rules)?;
    }

    run::report_stage(observer, "Materializing private workspace")?;
    let materialized = tempfile::tempdir().context("create private materialization directory")?;
    snapshot::materialize(store, &workspace_manifest, materialized.path())?;
    let transfer = tempfile::tempdir().context("create private E2B transfer directory")?;
    let workspace_archive = transfer.path().join("workspace.tar");
    archive_directory(materialized.path(), &workspace_archive)?;
    let secret_names = run::secret_injection_names(options);
    let secrets_archive = if secret_names.is_empty() {
        None
    } else {
        let path = transfer.path().join("secrets.tar");
        archive_secrets(options, &path)?;
        Some(path)
    };
    lifecycle.push(run::event("workspace_materialized"));

    let staging = format!("{}/staging/{run_id}", support.remote_root);
    create_remote_staging(&support, &staging)?;
    let mut remote_cleanup = RemoteRunCleanup {
        profile: profile.clone(),
        staging: staging.clone(),
        snapshots: Vec::new(),
        armed: true,
    };
    copy_to_remote(
        &support,
        &workspace_archive,
        &format!("{staging}/workspace.tar"),
    )?;
    if let Some(path) = &secrets_archive {
        copy_to_remote(&support, path, &format!("{staging}/secrets.tar"))?;
    }

    let compact_id = run_id.replace('-', "");
    let snapshot_prefix = format!("agentlab-{compact_id}");
    let remote_request = json!({
        "action": "run",
        "sdk_directory": support.sdk_directory,
        "remote_root": support.remote_root,
        "staging": staging,
        "run_id": run_id,
        "profile": profile.name,
        "template": template,
        "expected_template_build": probe.template_build_id,
        "environment": runtime_environment,
        "workspace_guest_path": options.workspace_guest_path,
        "command": options.command,
        "network": options.network,
        "secret_injections": secret_names,
        "pi_auth": options.pi_auth.is_some(),
        "snapshot_prefix": snapshot_prefix,
        "output_limit": crate::process::MAX_RUN_OUTPUT_BYTES,
        "command_timeout_seconds": E2B_GUEST_TIMEOUT_SECONDS,
        "sandbox_timeout_ms": E2B_SANDBOX_TIMEOUT_MS,
    });
    let remote: RemoteRunResult = serde_json::from_value(invoke_helper(
        &support,
        &remote_request,
        observer,
        REMOTE_RUN_TIMEOUT,
    )?)
    .context("decode E2B run result")?;
    validate_remote_run(&remote, &probe)?;
    remote_cleanup.snapshots = vec![remote.base_snapshot.clone(), remote.result_snapshot.clone()];
    lifecycle.push(run::event("command_completed"));
    lifecycle.push(run::event("provider_snapshots_created"));

    let stdout_path = run_directory.join("artifacts/stdout.bin");
    let stderr_path = run_directory.join("artifacts/stderr.bin");
    copy_from_remote(&support, &format!("{staging}/stdout.bin"), &stdout_path)?;
    copy_from_remote(&support, &format!("{staging}/stderr.bin"), &stderr_path)?;
    let stdout = run::artifact_for_file("artifacts/stdout.bin", &stdout_path)?;
    let stderr = run::artifact_for_file("artifacts/stderr.bin", &stderr_path)?;
    verify_command_artifacts(&remote.command, &stdout, &stderr)?;

    run::report_stage(observer, "Reading exact base filesystem snapshot")?;
    let base_inventory = collect_inventory(
        &support,
        &run_id,
        &staging,
        &remote.base_snapshot.build_id,
        "base",
        &run_directory,
        observer,
    )?;
    let base_entries: Vec<RootFsEntry> =
        serde_json::from_slice(&fs::read(&base_inventory.1).context("read E2B base inventory")?)
            .context("decode E2B base inventory")?;
    let base_manifest = rootfs::manifest_from_entries(base_entries)?;
    let base_manifest_bytes = run::pretty_json(&base_manifest)?;
    store.write_run_file(&run_id, "base-rootfs.json", &base_manifest_bytes)?;

    run::report_stage(observer, "Reading exact result filesystem snapshot")?;
    let result_inventory = collect_inventory(
        &support,
        &run_id,
        &staging,
        &remote.result_snapshot.build_id,
        "result",
        &run_directory,
        observer,
    )?;
    let result_entries: Vec<RootFsEntry> = serde_json::from_slice(
        &fs::read(&result_inventory.1).context("read E2B result inventory")?,
    )
    .context("decode E2B result inventory")?;
    let result_manifest = rootfs::manifest_from_entries(result_entries)?;
    let result_manifest_bytes = run::pretty_json(&result_manifest)?;
    store.write_run_file(&run_id, "result-rootfs.json", &result_manifest_bytes)?;
    lifecycle.push(run::event("root_filesystems_observed"));

    let all_changes = rootfs::compare(&base_manifest, &result_manifest);
    let ignored = match &change_ignore_rules {
        Some(rules) => run::evaluate_change_ignore_bytes(rules, &all_changes)?,
        None => Default::default(),
    };
    let mut portable_changes = Vec::new();
    let mut ignored_changes = Vec::new();
    for change in &all_changes {
        if ignored.contains(&change.path) {
            ignored_changes.push(IgnoredChange {
                path: change.path.clone(),
                change: change.change.clone(),
            });
        } else {
            portable_changes.push(change.clone());
        }
    }
    let raw_delta = run::make_delta(
        &base_manifest,
        &result_manifest,
        &change_ignore,
        all_changes.clone(),
        Vec::new(),
    )?;
    let portable_delta = run::make_delta(
        &base_manifest,
        &result_manifest,
        &change_ignore,
        portable_changes,
        ignored_changes,
    )?;
    let raw_delta_bytes = run::pretty_json(&raw_delta)?;
    let portable_delta_bytes = run::pretty_json(&portable_delta)?;
    store.write_run_file(&run_id, "delta.raw.json", &raw_delta_bytes)?;
    store.write_run_file(&run_id, "delta.json", &portable_delta_bytes)?;

    run::report_stage(observer, "Retaining changed and workspace file content")?;
    let required_base = run::required_base_file_paths(&all_changes);
    let required_result = run::required_result_file_paths(
        &result_manifest,
        &all_changes,
        &options.workspace_guest_path,
    );
    let base_bundle_path = run_directory.join("artifacts/base-content.tar");
    collect_bundle(
        &support,
        &run_id,
        &staging,
        &remote.base_snapshot.build_id,
        &required_base,
        "base-content.tar",
        &base_bundle_path,
        observer,
    )?;
    rootfs::store_required_file_blobs(&base_bundle_path, &base_manifest, &required_base, store)?;
    let base_bundle = run::artifact_for_file("artifacts/base-content.tar", &base_bundle_path)?;

    let result_bundle_path = run_directory.join("artifacts/result-content.tar");
    collect_bundle(
        &support,
        &run_id,
        &staging,
        &remote.result_snapshot.build_id,
        &required_result,
        "result-content.tar",
        &result_bundle_path,
        observer,
    )?;
    rootfs::store_required_file_blobs(
        &result_bundle_path,
        &result_manifest,
        &required_result,
        store,
    )?;
    let result_bundle =
        run::artifact_for_file("artifacts/result-content.tar", &result_bundle_path)?;

    let captures = collect_captures(
        &support,
        &run_id,
        &staging,
        &remote.result_snapshot.build_id,
        options,
        &run_directory,
        observer,
    )?;
    lifecycle.push(run::event("portable_evidence_stored"));

    let provider_metadata_value = json!({
        "profile": profile.name,
        "transport": "ssh",
        "ssh_alias": support.alias,
        "isolation": "firecracker",
        "template_requested": options.image,
        "template_resolved": remote.template,
        "template_build_id": remote.template_build_id,
        "sandbox_id": remote.sandbox_id,
        "base_snapshot": remote.base_snapshot,
        "result_snapshot": remote.result_snapshot,
        "sandbox_info": remote.sandbox_info,
        "final_sandbox_info": remote.final_sandbox_info,
        "command_output": remote.command,
    });
    let provider_metadata_bytes = run::pretty_json(&provider_metadata_value)?;
    let provider_metadata = run::write_artifact(
        store,
        &run_id,
        "evidence/e2b-provider.json",
        &provider_metadata_bytes,
    )?;

    let base_snapshot = snapshot_evidence(&remote.base_snapshot);
    let result_snapshot = snapshot_evidence(&remote.result_snapshot);
    let e2b = E2bEvidence {
        profile: profile.name.clone(),
        transport: "ssh".to_owned(),
        ssh_alias: support.alias.clone(),
        sdk_version: remote.sdk_version.clone(),
        isolation: "firecracker".to_owned(),
        template_requested: options.image.clone(),
        template_resolved: remote.template.clone(),
        template_build_id: remote.template_build_id.clone(),
        sandbox_id: remote.sandbox_id.clone(),
        base_snapshot,
        result_snapshot: result_snapshot.clone(),
        retained_snapshot_state: "immutable_snapshot".to_owned(),
        provider_metadata,
        base_inventory: base_inventory.0,
        result_inventory: result_inventory.0,
        base_content_bundle: base_bundle,
        result_content_bundle: result_bundle,
    };

    let mut warnings = workspace_warnings;
    if remote.command.stdout_truncated {
        warnings.push(format!(
            "stdout exceeded {} bytes and was truncated after being drained",
            crate::process::MAX_RUN_OUTPUT_BYTES
        ));
    }
    if remote.command.stderr_truncated {
        warnings.push(format!(
            "stderr exceeded {} bytes and was truncated after being drained",
            crate::process::MAX_RUN_OUTPUT_BYTES
        ));
    }
    if remote.command.timed_out {
        warnings.push(format!(
            "guest command exceeded the {} second deadline and its process group was terminated",
            E2B_GUEST_TIMEOUT_SECONDS
        ));
    }
    warnings.extend(run::sensitive_path_warnings(&all_changes));
    let observations = ObservationStatus {
        persistent_root_filesystem: "captured_from_filesystem_only_e2b_checkpoints".to_owned(),
        ignored_portable_changes: if portable_delta.ignored_changes.is_empty() {
            "none".to_owned()
        } else {
            "observed_but_deliberately_ignored".to_owned()
        },
        pseudo_filesystems: "runtime_only_nonportable".to_owned(),
        live_process_memory: "not_retained; result_is_filesystem_snapshot".to_owned(),
        writable_external_mounts: "none".to_owned(),
    };
    let completed_at = Utc::now();
    lifecycle.push(run::event("result_finalized"));

    let mut integrity = BTreeMap::new();
    for artifact in [
        &stdout,
        &stderr,
        &e2b.provider_metadata,
        &e2b.base_inventory,
        &e2b.result_inventory,
        &e2b.base_content_bundle,
        &e2b.result_content_bundle,
    ] {
        integrity.insert(artifact.path.clone(), artifact.digest.clone());
    }
    integrity.insert("spec.json".to_owned(), spec_digest.clone());
    integrity.insert(
        "evidence/environment.json".to_owned(),
        run::sha256_bytes(&environment_bytes),
    );
    if let Some(rules) = &change_ignore_rules {
        integrity.insert("change-ignore.rules".to_owned(), run::sha256_bytes(rules));
    }
    integrity.insert(
        "base-rootfs.json".to_owned(),
        run::sha256_bytes(&base_manifest_bytes),
    );
    integrity.insert(
        "result-rootfs.json".to_owned(),
        run::sha256_bytes(&result_manifest_bytes),
    );
    integrity.insert(
        "delta.raw.json".to_owned(),
        run::sha256_bytes(&raw_delta_bytes),
    );
    integrity.insert(
        "delta.json".to_owned(),
        run::sha256_bytes(&portable_delta_bytes),
    );
    for capture in &captures {
        integrity.insert(capture.path.clone(), capture.digest.clone());
    }

    let mut result = RunResult {
        schema_version: run::E2B_RESULT_SCHEMA_VERSION.to_owned(),
        digest: String::new(),
        run_id: run_id.clone(),
        run_spec_digest: spec_digest,
        started_at,
        completed_at,
        lifecycle,
        exit_code: remote.command.exit_code,
        stdout,
        stderr,
        captures,
        base_filesystem_digest: base_manifest.digest,
        result_filesystem_digest: result_manifest.digest,
        raw_delta_digest: raw_delta.digest,
        portable_delta_digest: portable_delta.digest,
        docker: None,
        e2b: Some(e2b),
        observations,
        warnings,
        integrity,
    };
    result.digest = run::compute_result_identity_digest(&result)?;
    store.write_run_file(&run_id, "result.json", &run::pretty_json(&result)?)?;

    let source_workspace_status =
        verify_source_workspace(options, store, &workspace_manifest, observer);
    remove_remote_staging(profile, &staging)?;
    remote_cleanup.armed = false;
    failed_run_directory_cleanup.armed = false;
    run::report_stage(observer, &format!("Run finalized: {run_id}"))?;

    Ok(RunSummary {
        run_id,
        result_digest: result.digest,
        run_input_digest: spec.run_input_digest,
        workspace_snapshot_digest: workspace_manifest.digest,
        image_resolved_digest,
        exit_code: remote.command.exit_code,
        changes: portable_delta.changes.len(),
        ignored_changes: portable_delta.ignored_changes.len(),
        retained_container_name: String::new(),
        retained_container_id: String::new(),
        backend_profile: profile.name.clone(),
        backend_driver: "e2b".to_owned(),
        retained_resource_kind: "snapshot".to_owned(),
        retained_resource_name: result_snapshot.snapshot_id,
        retained_resource_id: result_snapshot.build_id,
        source_workspace_status,
        accepted_input: spec.accepted_input,
    })
}

fn validate_probe(probe: &ProbeResult, template: &str, expected: Option<&str>) -> Result<()> {
    if probe.template != template || probe.isolation != "firecracker" {
        bail!("E2B backend probe did not match the selected template and isolation");
    }
    Uuid::parse_str(&probe.template_build_id).context("E2B template build ID is not a UUID")?;
    if expected.is_some_and(|value| value != probe.template_build_id) {
        bail!("E2B template build changed during resolution");
    }
    if probe.sdk_version.trim().is_empty() {
        bail!("E2B backend omitted its SDK version");
    }
    Ok(())
}

fn validate_remote_run(remote: &RemoteRunResult, probe: &ProbeResult) -> Result<()> {
    if remote.sdk_version != probe.sdk_version
        || remote.template != probe.template
        || remote.template_build_id != probe.template_build_id
    {
        bail!("E2B environment identity changed between probe and execution");
    }
    if remote.architecture != "x86_64" {
        bail!(
            "E2B template ran on unsupported architecture {:?}; expected x86_64",
            remote.architecture
        );
    }
    if remote.sandbox_id.is_empty()
        || remote
            .sandbox_id
            .bytes()
            .any(|byte| !byte.is_ascii_alphanumeric())
    {
        bail!("E2B returned an invalid sandbox ID");
    }
    validate_remote_snapshot(&remote.base_snapshot)?;
    validate_remote_snapshot(&remote.result_snapshot)?;
    if remote.base_snapshot.build_id == remote.result_snapshot.build_id
        || remote.base_snapshot.snapshot_id == remote.result_snapshot.snapshot_id
    {
        bail!("E2B base and result snapshots are not distinct");
    }
    for info in [&remote.sandbox_info, &remote.final_sandbox_info] {
        if info
            .get("volumeMounts")
            .and_then(Value::as_array)
            .is_some_and(|mounts| !mounts.is_empty())
        {
            bail!("E2B sandbox unexpectedly contains writable external volume mounts");
        }
    }
    Ok(())
}

fn validate_remote_snapshot(snapshot: &RemoteSnapshot) -> Result<()> {
    Uuid::parse_str(&snapshot.build_id).context("E2B snapshot build ID is not a UUID")?;
    if snapshot.snapshot_id.is_empty()
        || snapshot.snapshot_id.len() > 512
        || snapshot.snapshot_id.chars().any(char::is_control)
    {
        bail!("E2B returned an invalid snapshot reference");
    }
    if snapshot
        .names
        .iter()
        .any(|name| name.is_empty() || name.len() > 512 || name.chars().any(char::is_control))
    {
        bail!("E2B returned an invalid snapshot name");
    }
    Ok(())
}

fn snapshot_evidence(snapshot: &RemoteSnapshot) -> E2bSnapshotEvidence {
    E2bSnapshotEvidence {
        snapshot_id: snapshot.snapshot_id.clone(),
        build_id: snapshot.build_id.clone(),
        names: snapshot.names.clone(),
    }
}

fn verify_command_artifacts(
    command: &RemoteCommandResult,
    stdout: &Artifact,
    stderr: &Artifact,
) -> Result<()> {
    if stdout.size != command.stdout_retained_bytes
        || stderr.size != command.stderr_retained_bytes
        || command.stdout_total_bytes < command.stdout_retained_bytes
        || command.stderr_total_bytes < command.stderr_retained_bytes
        || command.stdout_truncated != (command.stdout_total_bytes > command.stdout_retained_bytes)
        || command.stderr_truncated != (command.stderr_total_bytes > command.stderr_retained_bytes)
    {
        bail!("E2B command output metadata does not match the retained artifacts");
    }
    Ok(())
}

fn verify_source_workspace(
    options: &RunOptions,
    store: &Store,
    before: &snapshot::Manifest,
    observer: &mut dyn run::RunObserver,
) -> String {
    let WorkspaceSource::Directory(workspace) = &options.workspace else {
        return "not_applicable".to_owned();
    };
    let _ = run::report_stage(observer, "Verifying source workspace remained unchanged");
    match snapshot::create_with_mode(workspace, store, options.workspace_capture_mode) {
        Ok(after) if after.manifest.digest == before.digest => {
            let _ = run::report_stage(observer, "Source workspace unchanged");
            "unchanged".to_owned()
        }
        Ok(after) => {
            let _ = run::report_stage(
                observer,
                &format!(
                    "Source workspace changed independently: {} -> {}",
                    before.digest, after.manifest.digest
                ),
            );
            "changed".to_owned()
        }
        Err(error) => {
            let _ = run::report_stage(
                observer,
                &format!("Source workspace verification failed: {error:#}"),
            );
            "verification_failed".to_owned()
        }
    }
}

fn archive_directory(source: &Path, destination: &Path) -> Result<()> {
    let file = File::create(destination).with_context(|| {
        format!(
            "create workspace transfer archive {}",
            destination.display()
        )
    })?;
    let mut archive = Builder::new(file);
    archive.follow_symlinks(false);
    archive
        .append_dir_all(".", source)
        .context("archive materialized workspace")?;
    archive
        .finish()
        .context("finish workspace transfer archive")?;
    Ok(())
}

fn archive_secrets(options: &RunOptions, destination: &Path) -> Result<()> {
    run::validate_secret_files(&options.secret_files, options.pi_auth.as_deref())?;
    let file =
        File::create(destination).context("create private runtime-secret transfer archive")?;
    let mut archive = Builder::new(file);
    if let Some(path) = &options.pi_auth {
        append_secret(&mut archive, "pi-auth.json", path)?;
    }
    for secret in &options.secret_files {
        append_secret(&mut archive, &secret.name, &secret.source)?;
    }
    archive
        .finish()
        .context("finish runtime-secret transfer archive")?;
    Ok(())
}

fn append_secret(archive: &mut Builder<File>, name: &str, source: &Path) -> Result<()> {
    let mut file = File::open(source)
        .with_context(|| format!("open runtime secret source {}", source.display()))?;
    let size = file.metadata()?.len();
    let mut header = Header::new_gnu();
    header.set_path(name)?;
    header.set_mode(0o600);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(size);
    header.set_cksum();
    archive
        .append(&header, &mut file)
        .with_context(|| format!("archive runtime secret {name:?}"))?;
    Ok(())
}

fn ensure_remote_support(
    profile: &SelectedBackend,
    observer: &mut dyn run::RunObserver,
) -> Result<RemoteSupport> {
    let alias = profile.config.ssh_alias()?.to_owned();
    let sdk_directory = profile.config.sdk_directory()?.to_owned();
    let orchestrator = profile.config.orchestrator_directory()?.to_owned();
    let remote_root = profile.config.remote_root()?.to_owned();
    let helper = format!("{remote_root}/bin/agentlab-e2b-helper.mjs");
    let scanner = format!("{remote_root}/bin/agentlab-e2b-snapshot.py");
    let mount_binary = format!("{remote_root}/bin/mount-build-rootfs");
    let support = RemoteSupport {
        alias,
        sdk_directory,
        remote_root,
        helper,
        scanner,
        mount_binary,
    };

    let directories = [
        support.remote_root.clone(),
        format!("{}/bin", support.remote_root),
        format!("{}/staging", support.remote_root),
        format!("{}/mounts", support.remote_root),
        format!("{}/storage", support.remote_root),
    ];
    let mut mkdir = ssh_command(&support);
    mkdir.arg("mkdir").arg("-p").arg("--").args(&directories);
    control_success(
        &mut mkdir,
        REMOTE_CONTROL_TIMEOUT,
        "create E2B backend directories",
    )?;
    let mut chmod_command = ssh_command(&support);
    chmod_command
        .arg("chmod")
        .arg("700")
        .arg("--")
        .args(&directories);
    control_success(
        &mut chmod_command,
        REMOTE_CONTROL_TIMEOUT,
        "secure E2B backend directories",
    )?;

    deploy_remote_file(&support, REMOTE_HELPER, &support.helper, 0o700)?;
    deploy_remote_file(&support, REMOTE_SCANNER, &support.scanner, 0o700)?;

    let mut credential_permissions = ssh_command(&support);
    credential_permissions
        .arg("chmod")
        .arg("600")
        .arg("--")
        .arg(format!("{}/.env.local", support.sdk_directory));
    control_success(
        &mut credential_permissions,
        REMOTE_CONTROL_TIMEOUT,
        "secure E2B credential file",
    )?;

    let mut sudo_probe = ssh_command(&support);
    sudo_probe.args(["sudo", "-n", "true"]);
    control_success(
        &mut sudo_probe,
        REMOTE_CONTROL_TIMEOUT,
        "verify non-interactive E2B rootfs evidence access",
    )?;

    let mut binary_probe = ssh_command(&support);
    binary_probe.args(["test", "-x", &support.mount_binary]);
    if !control_status(&mut binary_probe, REMOTE_CONTROL_TIMEOUT)?.success() {
        run::report_stage(
            observer,
            "Preparing E2B snapshot evidence helper (first run only)",
        )?;
        let mut build = ssh_command(&support);
        build.args([
            "go",
            "-C",
            &orchestrator,
            "build",
            "-o",
            &support.mount_binary,
            "./cmd/mount-build-rootfs",
        ]);
        control_success(
            &mut build,
            REMOTE_CONTROL_TIMEOUT,
            "build E2B rootfs evidence helper",
        )?;
    }

    let template_storage = format!("{orchestrator}/tmp/local-template-storage");
    let storage_link = format!("{}/storage/templates", support.remote_root);
    let mut link_state = ssh_command(&support);
    link_state.args(["test", "-e", &storage_link]);
    if control_status(&mut link_state, REMOTE_CONTROL_TIMEOUT)?.success() {
        let mut link_type = ssh_command(&support);
        link_type.args(["test", "-L", &storage_link]);
        if !control_status(&mut link_type, REMOTE_CONTROL_TIMEOUT)?.success() {
            bail!("E2B template storage path {storage_link} exists and is not AgentLab's symlink");
        }
    }
    let mut link = ssh_command(&support);
    link.args(["ln", "-sfn", &template_storage, &storage_link]);
    control_success(
        &mut link,
        REMOTE_CONTROL_TIMEOUT,
        "connect E2B local template storage",
    )?;
    Ok(support)
}

fn deploy_remote_file(
    support: &RemoteSupport,
    contents: &[u8],
    destination: &str,
    mode: u32,
) -> Result<()> {
    let temporary =
        tempfile::NamedTempFile::new().context("create backend helper transfer file")?;
    fs::write(temporary.path(), contents).context("write backend helper transfer file")?;
    let incoming = format!("{destination}.incoming-{}", Uuid::new_v4().simple());
    copy_to_remote(support, temporary.path(), &incoming)?;
    let mode = format!("{mode:o}");
    let mut permissions = ssh_command(support);
    permissions.args(["chmod", &mode, "--", &incoming]);
    control_success(
        &mut permissions,
        REMOTE_CONTROL_TIMEOUT,
        "secure incoming E2B backend helper",
    )?;
    let mut install = ssh_command(support);
    install.args(["mv", "-f", "--", &incoming, destination]);
    control_success(
        &mut install,
        REMOTE_CONTROL_TIMEOUT,
        "install E2B backend helper",
    )
}

fn create_remote_staging(support: &RemoteSupport, staging: &str) -> Result<()> {
    let mut command = ssh_command(support);
    command
        .arg("install")
        .args(["-d", "-m", "700", "--", staging]);
    control_success(
        &mut command,
        REMOTE_CONTROL_TIMEOUT,
        "create private E2B run staging directory",
    )
}

fn collect_inventory(
    support: &RemoteSupport,
    run_id: &str,
    staging: &str,
    build_id: &str,
    label: &str,
    run_directory: &Path,
    observer: &mut dyn run::RunObserver,
) -> Result<(Artifact, PathBuf)> {
    let remote = format!("{staging}/{label}-entries.json");
    snapshot_operation(
        support,
        run_id,
        staging,
        build_id,
        json!({ "operation": "scan", "output": remote }),
        observer,
    )?;
    let relative = format!("evidence/{label}-rootfs-entries.json");
    let local = run_directory.join(&relative);
    copy_from_remote(support, &remote, &local)?;
    Ok((run::artifact_for_file(&relative, &local)?, local))
}

#[allow(clippy::too_many_arguments)]
fn collect_bundle(
    support: &RemoteSupport,
    run_id: &str,
    staging: &str,
    build_id: &str,
    paths: &std::collections::BTreeSet<String>,
    name: &str,
    local: &Path,
    observer: &mut dyn run::RunObserver,
) -> Result<()> {
    let remote = format!("{staging}/{name}");
    snapshot_operation(
        support,
        run_id,
        staging,
        build_id,
        json!({
            "operation": "bundle",
            "output": remote,
            "paths": paths,
        }),
        observer,
    )?;
    copy_from_remote(support, &remote, local)
}

fn collect_captures(
    support: &RemoteSupport,
    run_id: &str,
    staging: &str,
    build_id: &str,
    options: &RunOptions,
    run_directory: &Path,
    observer: &mut dyn run::RunObserver,
) -> Result<Vec<Artifact>> {
    if options.captures.is_empty() {
        return Ok(Vec::new());
    }
    let remote_captures: Vec<_> = options
        .captures
        .iter()
        .map(|capture| {
            json!({
                "guest_path": capture.guest_path,
                "output": format!("{staging}/capture-{}.tar", capture.name),
            })
        })
        .collect();
    snapshot_operation(
        support,
        run_id,
        staging,
        build_id,
        json!({
            "operation": "captures",
            "captures": remote_captures,
        }),
        observer,
    )?;
    let mut artifacts = Vec::new();
    for capture in &options.captures {
        let relative = format!("artifacts/capture-{}.tar", capture.name);
        let local = run_directory.join(&relative);
        copy_from_remote(
            support,
            &format!("{staging}/capture-{}.tar", capture.name),
            &local,
        )?;
        artifacts.push(run::artifact_for_file(&relative, &local)?);
    }
    Ok(artifacts)
}

fn snapshot_operation(
    support: &RemoteSupport,
    run_id: &str,
    staging: &str,
    build_id: &str,
    fields: Value,
    observer: &mut dyn run::RunObserver,
) -> Result<()> {
    let mut request = json!({
        "action": "snapshot_evidence",
        "remote_root": support.remote_root,
        "mount_binary": support.mount_binary,
        "scanner": support.scanner,
        "staging": staging,
        "run_id": run_id,
        "build_id": build_id,
    });
    let object = request
        .as_object_mut()
        .context("snapshot evidence request is not an object")?;
    for (key, value) in fields
        .as_object()
        .context("snapshot evidence fields are not an object")?
    {
        object.insert(key.clone(), value.clone());
    }
    invoke_helper(support, &request, observer, REMOTE_RUN_TIMEOUT)?;
    Ok(())
}

fn invoke_helper(
    support: &RemoteSupport,
    request: &Value,
    observer: &mut dyn run::RunObserver,
    timeout: Duration,
) -> Result<Value> {
    let request_bytes = serde_json::to_vec(request).context("encode E2B helper request")?;
    if request_bytes.len() > 16 * 1024 * 1024 {
        bail!("E2B helper request exceeds the 16 MiB safety limit");
    }
    let mut command = ssh_command(support);
    command
        .args(["node", &support.helper])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::process::isolate_process_group(&mut command);
    let child = command
        .spawn()
        .context("start E2B backend helper over SSH")?;
    let mut child = ChildProcessGuard::new(child);
    let mut stdin = child.child.stdin.take().context("open E2B helper input")?;
    stdin
        .write_all(&request_bytes)
        .context("write E2B helper request")?;
    drop(stdin);

    let stdout = child
        .child
        .stdout
        .take()
        .context("open E2B helper output")?;
    let stderr = child
        .child
        .stderr
        .take()
        .context("open E2B helper diagnostics")?;
    let (sender, receiver) = mpsc::sync_channel(64);
    let stdout_reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) if line.len() > 1024 * 1024 => {
                    let _ = sender.send(Err("E2B helper protocol line exceeded 1 MiB".to_owned()));
                    break;
                }
                Ok(_) => {
                    if sender.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(format!("read E2B helper protocol: {error}")));
                    break;
                }
            }
        }
    });
    let stderr_reader =
        std::thread::spawn(move || crate::process::read_bounded(stderr, REMOTE_OUTPUT_LIMIT));

    let started = Instant::now();
    let mut result = None;
    let mut remote_error = None;
    let mut protocol_closed = false;
    loop {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(line)) => {
                let message: Value =
                    serde_json::from_slice(&line).context("decode E2B helper protocol message")?;
                match message.get("type").and_then(Value::as_str) {
                    Some("stage") => {
                        let text = message
                            .get("message")
                            .and_then(Value::as_str)
                            .context("E2B stage message omitted text")?;
                        run::report_stage(observer, text)?;
                    }
                    Some("command_stdout" | "command_stderr") => {
                        let encoded = message
                            .get("data")
                            .and_then(Value::as_str)
                            .context("E2B output message omitted data")?;
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(encoded)
                            .context("decode E2B command output event")?;
                        if message["type"] == "command_stdout" {
                            observer
                                .command_stdout(&bytes)
                                .context("write E2B command stdout")?;
                        } else {
                            observer
                                .command_stderr(&bytes)
                                .context("write E2B command stderr")?;
                        }
                    }
                    Some("result") => {
                        if result.is_some() {
                            bail!("E2B helper returned more than one result");
                        }
                        result = Some(
                            message
                                .get("result")
                                .cloned()
                                .context("E2B helper result omitted payload")?,
                        );
                    }
                    Some("error") => {
                        remote_error = Some(
                            message
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("E2B backend failed")
                                .to_owned(),
                        );
                    }
                    other => bail!("unknown E2B helper protocol message {other:?}"),
                }
            }
            Ok(Err(error)) => {
                remote_error = Some(error);
                protocol_closed = true;
            }
            Err(RecvTimeoutError::Disconnected) => protocol_closed = true,
            Err(RecvTimeoutError::Timeout) => {}
        }

        if crate::cancel::requested() {
            bail!(
                "E2B run interrupted; the remote helper was asked to revoke credentials and remove the active sandbox"
            );
        }
        if started.elapsed() > timeout {
            bail!("E2B backend operation exceeded its fail-safe deadline");
        }
        if protocol_closed {
            if let Some(status) = child.child.try_wait().context("poll E2B backend helper")? {
                let _ = crate::process::terminate_process_group(&mut child.child);
                child.disarm();
                let stderr = stderr_reader
                    .join()
                    .map_err(|_| anyhow::anyhow!("E2B helper diagnostic reader panicked"))??;
                stdout_reader
                    .join()
                    .map_err(|_| anyhow::anyhow!("E2B helper protocol reader panicked"))?;
                if stderr.truncated {
                    bail!("E2B helper diagnostics exceeded the safe capture limit");
                }
                if !status.success() || remote_error.is_some() {
                    let diagnostic = remote_error.unwrap_or_else(|| {
                        String::from_utf8_lossy(&stderr.bytes).trim().to_owned()
                    });
                    bail!("E2B backend operation failed: {diagnostic}");
                }
                return result.context("E2B backend helper exited without a result");
            }
        }
    }
}

fn delete_snapshots(profile: &SelectedBackend, snapshots: &[RemoteSnapshot]) -> Result<()> {
    if snapshots.is_empty() {
        return Ok(());
    }
    let support = support_from_profile(profile)?;
    let request = json!({
        "action": "delete",
        "sdk_directory": support.sdk_directory,
        "snapshots": snapshots,
    });
    invoke_helper(
        &support,
        &request,
        &mut run::SilentRunObserver,
        REMOTE_CONTROL_TIMEOUT,
    )?;
    Ok(())
}

pub(crate) fn remove_retained_snapshots(store: &Store, evidence: &E2bEvidence) -> Result<()> {
    let config = AgentLabConfig::load(store)?;
    let profile = config.selected_backend(Some(&evidence.profile))?;
    if profile.driver() != BackendDriver::E2b {
        bail!(
            "recorded backend profile {:?} no longer selects the E2B driver; refusing provider deletion",
            evidence.profile
        );
    }
    if profile.config.ssh_alias()? != evidence.ssh_alias
        || profile.config.transport.as_deref() != Some(evidence.transport.as_str())
        || profile.config.expected_isolation.as_deref() != Some(evidence.isolation.as_str())
    {
        bail!(
            "recorded E2B backend connection no longer matches profile {:?}; refusing provider deletion",
            evidence.profile
        );
    }
    ensure_remote_support(&profile, &mut run::SilentRunObserver)?;
    let snapshots = [
        RemoteSnapshot {
            snapshot_id: evidence.base_snapshot.snapshot_id.clone(),
            build_id: evidence.base_snapshot.build_id.clone(),
            names: evidence.base_snapshot.names.clone(),
        },
        RemoteSnapshot {
            snapshot_id: evidence.result_snapshot.snapshot_id.clone(),
            build_id: evidence.result_snapshot.build_id.clone(),
            names: evidence.result_snapshot.names.clone(),
        },
    ];
    delete_snapshots(&profile, &snapshots)
}

fn remove_remote_staging(profile: &SelectedBackend, staging: &str) -> Result<()> {
    let support = support_from_profile(profile)?;
    let request = json!({
        "action": "cleanup_staging",
        "remote_root": support.remote_root,
        "staging": staging,
    });
    invoke_helper(
        &support,
        &request,
        &mut run::SilentRunObserver,
        REMOTE_CONTROL_TIMEOUT,
    )?;
    Ok(())
}

fn support_from_profile(profile: &SelectedBackend) -> Result<RemoteSupport> {
    let remote_root = profile.config.remote_root()?.to_owned();
    Ok(RemoteSupport {
        alias: profile.config.ssh_alias()?.to_owned(),
        sdk_directory: profile.config.sdk_directory()?.to_owned(),
        helper: format!("{remote_root}/bin/agentlab-e2b-helper.mjs"),
        scanner: format!("{remote_root}/bin/agentlab-e2b-snapshot.py"),
        mount_binary: format!("{remote_root}/bin/mount-build-rootfs"),
        remote_root,
    })
}

fn copy_to_remote(support: &RemoteSupport, local: &Path, remote: &str) -> Result<()> {
    let mut command = Command::new("scp");
    command
        .args(["-q", "-p", "--"])
        .arg(local)
        .arg(format!("{}:{remote}", support.alias));
    control_success(
        &mut command,
        REMOTE_CONTROL_TIMEOUT,
        "upload E2B backend input",
    )
}

fn copy_from_remote(support: &RemoteSupport, remote: &str, local: &Path) -> Result<()> {
    let mut command = Command::new("scp");
    command
        .args(["-q", "-p", "--"])
        .arg(format!("{}:{remote}", support.alias))
        .arg(local);
    control_success(
        &mut command,
        REMOTE_CONTROL_TIMEOUT,
        "download E2B backend evidence",
    )
}

fn ssh_command(support: &RemoteSupport) -> Command {
    let mut command = Command::new("ssh");
    command.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        &support.alias,
    ]);
    command
}

struct ControlOutput {
    status: ExitStatus,
    stdout: crate::process::BoundedCapture,
    stderr: crate::process::BoundedCapture,
}

fn control_status(command: &mut Command, timeout: Duration) -> Result<ExitStatus> {
    Ok(control_output(command, timeout)?.status)
}

fn control_success(command: &mut Command, timeout: Duration, context: &str) -> Result<()> {
    let output = control_output(command, timeout).with_context(|| context.to_owned())?;
    if output.stdout.truncated || output.stderr.truncated {
        bail!("{context}: command output exceeded the safe capture limit");
    }
    if !output.status.success() {
        bail!(
            "{context}: {}",
            String::from_utf8_lossy(&output.stderr.bytes).trim()
        );
    }
    Ok(())
}

fn control_output(command: &mut Command, timeout: Duration) -> Result<ControlOutput> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    crate::process::isolate_process_group(command);
    let child = command.spawn().context("start remote control command")?;
    let mut child = ChildProcessGuard::new(child);
    let stdout = child
        .child
        .stdout
        .take()
        .context("open remote control stdout")?;
    let stderr = child
        .child
        .stderr
        .take()
        .context("open remote control stderr")?;
    let stdout_reader =
        std::thread::spawn(move || crate::process::read_bounded(stdout, REMOTE_OUTPUT_LIMIT));
    let stderr_reader =
        std::thread::spawn(move || crate::process::read_bounded(stderr, REMOTE_OUTPUT_LIMIT));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child
            .child
            .try_wait()
            .context("poll remote control command")?
        {
            break status;
        }
        if crate::cancel::requested() || started.elapsed() > timeout {
            if crate::cancel::requested() {
                bail!("remote control command interrupted");
            }
            bail!("remote control command exceeded its fail-safe deadline");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = crate::process::terminate_process_group(&mut child.child);
    child.disarm();
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("remote control stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("remote control stderr reader panicked"))??;
    Ok(ControlOutput {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_mismatched_command_artifact_sizes() {
        let command = RemoteCommandResult {
            exit_code: 0,
            timed_out: false,
            stdout_total_bytes: 2,
            stderr_total_bytes: 0,
            stdout_retained_bytes: 2,
            stderr_retained_bytes: 0,
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let stdout = Artifact {
            path: "stdout".to_owned(),
            digest: "sha256:test".to_owned(),
            size: 1,
        };
        let stderr = Artifact {
            path: "stderr".to_owned(),
            digest: "sha256:test".to_owned(),
            size: 0,
        };
        assert!(verify_command_artifacts(&command, &stdout, &stderr).is_err());
    }
}
