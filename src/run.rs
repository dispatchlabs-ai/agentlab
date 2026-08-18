use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::acceptance;
use crate::build_version;
use crate::lock::AdvisoryLock;
use crate::rootfs::{self, ChangeKind, RootFsChange, RootFsManifest};
use crate::snapshot;
use crate::store::{Store, hex_digest};

pub const RUN_SCHEMA_VERSION: &str = "agentlab.run/v3";
pub const LEGACY_RUN_SCHEMA_VERSION: &str = "agentlab.run/v1";
pub const LEGACY_RUN_SCHEMA_VERSION_V2: &str = "agentlab.run/v2";
pub const RUN_INPUT_SCHEMA_VERSION: &str = "agentlab.run-input/v1";
pub const DELTA_SCHEMA_VERSION: &str = "agentlab.delta/v1";
pub const RESULT_SCHEMA_VERSION: &str = "agentlab.result/v1";
pub(crate) const PI_AUTH_SECRET_NAME: &str = "pi-auth";
const PI_AUTH_SECRET_DIRECTORY: &str = "/run/agentlab-secrets";
const PI_AUTH_SECRET_PATH: &str = "/run/agentlab-secrets/pi-auth.json";
const RUNTIME_SECRET_LEASE_PATH: &str = "runtime-secret-lease.json";
const RUNTIME_SECRET_LEASE_LOCK_PATH: &str = "runtime-secret-lease.lock";
const RUNTIME_SECRET_LEASE_SCHEMA: &str = "agentlab.runtime-secret-lease/v1";
pub(crate) const PI_AUTH_TMPFS_MOUNT: &str =
    "type=tmpfs,destination=/run/agentlab-secrets,tmpfs-mode=0711,tmpfs-size=1048576";
const MAX_RUNTIME_SECRET_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub workspace: WorkspaceSource,
    pub workspace_capture_mode: snapshot::CaptureMode,
    pub image: String,
    pub command: Vec<String>,
    pub workspace_guest_path: String,
    pub network: String,
    pub memory: Option<String>,
    pub cpus: Option<String>,
    pub pi_auth: Option<PathBuf>,
    pub secret_files: Vec<SecretFileSpec>,
    pub change_ignore: Option<PathBuf>,
    pub captures: Vec<CaptureSpec>,
    pub accepted_input: Option<AcceptedInputReference>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFileSpec {
    pub name: String,
    pub source: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceSource {
    Directory(PathBuf),
    Snapshot(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceptedInputReference {
    pub acceptance_id: String,
    pub acceptance_digest: String,
    pub accepted_input_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptureSpec {
    pub guest_path: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IgnoreIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunSpec {
    pub schema_version: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_input: Option<AcceptedInputReference>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub run_input_digest: String,
    pub workspace_snapshot_digest: String,
    pub image_requested: String,
    pub image_resolved_digest: String,
    pub docker_image_id: String,
    pub target_platform: String,
    pub workspace_guest_path: String,
    pub command: Vec<String>,
    pub working_directory: String,
    #[serde(
        default,
        rename = "factors",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub legacy_factors: BTreeMap<String, String>,
    pub resource_limits: ResourceLimits,
    pub network_policy: String,
    pub captures: Vec<CaptureSpec>,
    pub secret_injections: Vec<String>,
    pub workspace_ignore_digest: String,
    pub change_ignore: IgnoreIdentity,
    pub backend_name: String,
    pub backend_version: String,
    pub agentlab_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeltaManifest {
    pub schema_version: String,
    pub digest: String,
    pub base_filesystem_digest: String,
    pub result_filesystem_digest: String,
    pub change_ignore: IgnoreIdentity,
    pub changes: Vec<RootFsChange>,
    pub ignored_changes: Vec<IgnoredChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IgnoredChange {
    pub path: String,
    pub change: ChangeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Artifact {
    pub path: String,
    pub digest: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LifecycleEvent {
    pub event: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DockerEvidence {
    pub prepared_image_id: String,
    pub retained_container_id: String,
    pub retained_container_name: String,
    pub retained_container_state: String,
    pub preparation_inspect: Artifact,
    pub result_inspect: Artifact,
    pub docker_diff: Artifact,
    pub base_rootfs_export: Artifact,
    pub result_rootfs_export: Artifact,
    pub docker_diff_uncovered_changes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservationStatus {
    pub persistent_root_filesystem: String,
    pub ignored_portable_changes: String,
    pub pseudo_filesystems: String,
    pub live_process_memory: String,
    pub writable_external_mounts: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunResult {
    pub schema_version: String,
    pub digest: String,
    pub run_id: String,
    pub run_spec_digest: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub lifecycle: Vec<LifecycleEvent>,
    pub exit_code: i64,
    pub stdout: Artifact,
    pub stderr: Artifact,
    pub captures: Vec<Artifact>,
    pub base_filesystem_digest: String,
    pub result_filesystem_digest: String,
    pub raw_delta_digest: String,
    pub portable_delta_digest: String,
    pub docker: DockerEvidence,
    pub observations: ObservationStatus,
    pub warnings: Vec<String>,
    pub integrity: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub result_digest: String,
    pub run_input_digest: String,
    pub workspace_snapshot_digest: String,
    pub image_resolved_digest: String,
    pub exit_code: i64,
    pub changes: usize,
    pub ignored_changes: usize,
    pub retained_container_name: String,
    pub retained_container_id: String,
    pub source_workspace_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_input: Option<AcceptedInputReference>,
}

pub trait RunObserver {
    fn stage(&mut self, message: &str) -> std::io::Result<()>;
    fn command_stdout(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    fn command_stderr(&mut self, bytes: &[u8]) -> std::io::Result<()>;
}

pub(crate) struct SilentRunObserver;

impl RunObserver for SilentRunObserver {
    fn stage(&mut self, _message: &str) -> std::io::Result<()> {
        Ok(())
    }

    fn command_stdout(&mut self, _bytes: &[u8]) -> std::io::Result<()> {
        Ok(())
    }

    fn command_stderr(&mut self, _bytes: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunComparison {
    pub left_run_id: String,
    pub right_run_id: String,
    pub left_run_input_digest: String,
    pub right_run_input_digest: String,
    pub same_run_input: bool,
    pub same_workspace_snapshot: bool,
    pub same_resolved_image: bool,
    pub same_portable_base: bool,
    pub distinct_private_containers: bool,
    pub controlled_input_differences: Vec<String>,
    pub comparison_kind: String,
    pub comparable_repetition: bool,
    pub portable_outcomes_equal: bool,
}

#[derive(Serialize)]
struct RunInputIdentity<'a> {
    schema_version: &'static str,
    workspace_snapshot_digest: &'a str,
    image_resolved_digest: &'a str,
    target_platform: &'a str,
    workspace_guest_path: &'a str,
    command: &'a [String],
    working_directory: &'a str,
    resource_limits: &'a ResourceLimits,
    network_policy: &'a str,
    captures: &'a [CaptureSpec],
    secret_injections: &'a [String],
    workspace_ignore_digest: &'a str,
    change_ignore_digest: &'a str,
    backend_name: &'a str,
    backend_version: &'a str,
    agentlab_version: &'a str,
}

#[derive(Serialize)]
struct DeltaIdentity<'a> {
    schema_version: &'a str,
    base_filesystem_digest: &'a str,
    result_filesystem_digest: &'a str,
    change_ignore: &'a IgnoreIdentity,
    changes: &'a [RootFsChange],
    ignored_changes: &'a [IgnoredChange],
}

#[derive(Serialize)]
struct ResultIdentity<'a> {
    schema_version: &'a str,
    run_id: &'a str,
    run_spec_digest: &'a str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    lifecycle: &'a [LifecycleEvent],
    exit_code: i64,
    stdout: &'a Artifact,
    stderr: &'a Artifact,
    captures: &'a [Artifact],
    base_filesystem_digest: &'a str,
    result_filesystem_digest: &'a str,
    raw_delta_digest: &'a str,
    portable_delta_digest: &'a str,
    docker: &'a DockerEvidence,
    observations: &'a ObservationStatus,
    warnings: &'a [String],
    integrity: &'a BTreeMap<String, String>,
}

struct ResolvedImage {
    resolved_digest: String,
    execution_reference: String,
    docker_image_id: String,
    platform: String,
    inspect: Vec<u8>,
}

struct FailedRunCleanup {
    preparation_name: String,
    retained_name: String,
    prepared_tag: String,
    armed: bool,
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

impl Drop for FailedRunCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for container in [&self.retained_name, &self.preparation_name] {
            let _ = Command::new("docker")
                .args(["rm", "--force", container])
                .output();
        }
        let _ = Command::new("docker")
            .args(["image", "rm", &self.prepared_tag])
            .output();
    }
}

pub fn execute(options: &RunOptions, store: &Store) -> Result<RunSummary> {
    execute_with_observer(options, store, &mut SilentRunObserver)
}

pub fn execute_with_observer(
    options: &RunOptions,
    store: &Store,
    observer: &mut dyn RunObserver,
) -> Result<RunSummary> {
    validate_options(options)?;
    let started_at = Utc::now();
    let run_id = Uuid::new_v4().to_string();
    let run_directory = store.create_run_directory(&run_id)?;
    let mut failed_run_directory_cleanup = FailedRunDirectoryCleanup {
        store: store.clone(),
        run_id: run_id.clone(),
        armed: true,
    };
    let mut lifecycle = vec![event("run_created")];
    report_stage(observer, &format!("Run created: {run_id}"))?;

    let (workspace_manifest, workspace_warnings) = match &options.workspace {
        WorkspaceSource::Directory(workspace) => {
            report_stage(
                observer,
                &format!(
                    "Capturing workspace ({}): {}",
                    options.workspace_capture_mode.as_str(),
                    workspace.display()
                ),
            )?;
            let captured =
                snapshot::create_with_mode(workspace, store, options.workspace_capture_mode)?;
            lifecycle.push(event("workspace_snapshotted"));
            report_stage(
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
            report_stage(observer, &format!("Verifying workspace snapshot: {digest}"))?;
            let manifest = snapshot::load(store, digest)?;
            snapshot::verify(store, &manifest)?;
            lifecycle.push(event("workspace_snapshot_loaded"));
            report_stage(
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
    report_stage(observer, &format!("Resolving image: {}", options.image))?;
    let resolved_image = resolve_image(&options.image)?;
    lifecycle.push(event("image_resolved"));
    report_stage(
        observer,
        &format!(
            "Image resolved: {} ({})",
            resolved_image.resolved_digest, resolved_image.platform
        ),
    )?;
    let (change_ignore, change_ignore_rules) =
        resolve_change_ignore(options, &workspace_manifest, store)?;

    let mut spec = RunSpec {
        schema_version: RUN_SCHEMA_VERSION.to_string(),
        run_id: run_id.clone(),
        accepted_input: options.accepted_input.clone(),
        run_input_digest: String::new(),
        workspace_snapshot_digest: workspace_manifest.digest.clone(),
        image_requested: options.image.clone(),
        image_resolved_digest: resolved_image.resolved_digest.clone(),
        docker_image_id: resolved_image.docker_image_id.clone(),
        target_platform: resolved_image.platform.clone(),
        workspace_guest_path: options.workspace_guest_path.clone(),
        command: options.command.clone(),
        working_directory: options.workspace_guest_path.clone(),
        legacy_factors: BTreeMap::new(),
        resource_limits: ResourceLimits {
            memory: options.memory.clone(),
            cpus: options.cpus.clone(),
        },
        network_policy: options.network.clone(),
        captures: options.captures.clone(),
        secret_injections: secret_injection_names(options),
        workspace_ignore_digest: workspace_manifest.ignore_rules_digest.clone(),
        change_ignore: change_ignore.clone(),
        backend_name: "docker-cli".to_string(),
        backend_version: docker_version()?,
        agentlab_version: build_version(),
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
        lifecycle.push(event("accepted_input_verified"));
        report_stage(
            observer,
            &format!("Accepted input verified: {}", reference.acceptance_id),
        )?;
    }
    spec.run_input_digest = compute_run_input_digest(&spec)?;
    let spec_bytes = pretty_json(&spec)?;
    let spec_digest = sha256_bytes(&spec_bytes);
    store.write_run_file(&run_id, "spec.json", &spec_bytes)?;
    store.write_run_file(
        &run_id,
        "evidence/image-inspect.json",
        &resolved_image.inspect,
    )?;
    if let Some(rules) = &change_ignore_rules {
        store.write_run_file(&run_id, "change-ignore.rules", rules)?;
    }

    report_stage(observer, "Materializing private workspace")?;
    let materialized = tempfile::tempdir().context("create private materialization directory")?;
    snapshot::materialize(store, &workspace_manifest, materialized.path())?;
    lifecycle.push(event("workspace_materialized"));

    let compact_id = run_id.replace('-', "");
    let short_id = &compact_id[..12];
    let preparation_name = format!("agentlab-prep-{short_id}");
    let retained_name = format!("agentlab-run-{short_id}");
    let prepared_tag = format!("agentlab-prepared:{short_id}");
    let mut failed_run_cleanup = FailedRunCleanup {
        preparation_name: preparation_name.clone(),
        retained_name: retained_name.clone(),
        prepared_tag: prepared_tag.clone(),
        armed: true,
    };

    report_stage(observer, "Preparing isolated Docker filesystem")?;
    docker_success(
        Command::new("docker")
            .args(["create", "--name", &preparation_name])
            .args(["--label", &format!("agentlab.run_id={run_id}")])
            .args([&resolved_image.execution_reference, "/bin/true"]),
        "create preparation container",
    )?;
    lifecycle.push(event("preparation_container_created"));

    let copy_source = format!("{}/.", materialized.path().display());
    docker_status(
        Command::new("docker").args([
            "cp",
            &copy_source,
            &format!("{preparation_name}:{}", options.workspace_guest_path),
        ]),
        "copy private workspace into preparation container",
    )?;
    lifecycle.push(event("workspace_copied_to_private_rootfs"));
    let runtime_root = tempfile::tempdir().context("create runtime mountpoint fixture")?;
    let secret_directory = runtime_root.path().join("agentlab-secrets");
    fs::create_dir(&secret_directory).context("create runtime secret mountpoint")?;
    docker_status(
        Command::new("docker").args([
            "cp",
            secret_directory
                .to_str()
                .context("runtime mountpoint path is not UTF-8")?,
            &format!("{preparation_name}:{PI_AUTH_SECRET_DIRECTORY}"),
        ]),
        "prepare runtime secret mountpoint",
    )?;
    lifecycle.push(event("runtime_secret_mountpoint_prepared"));

    let preparation_inspect_bytes = docker_output_bytes(
        Command::new("docker").args(["inspect", &preparation_name]),
        "inspect preparation container",
    )?;
    ensure_no_external_mounts(&preparation_inspect_bytes)?;
    let preparation_inspect = write_artifact(
        store,
        &run_id,
        "evidence/preparation-inspect.json",
        &preparation_inspect_bytes,
    )?;

    let base_export_path = run_directory.join("artifacts/base-rootfs.tar");
    docker_status(
        Command::new("docker").args([
            "export",
            "--output",
            base_export_path.to_str().context("run path is not UTF-8")?,
            &preparation_name,
        ]),
        "export prepared base root filesystem",
    )?;
    lifecycle.push(event("prepared_rootfs_exported"));

    let prepared_image_id = docker_success(
        Command::new("docker").args(["commit", &preparation_name, &prepared_tag]),
        "commit prepared base image",
    )?;
    lifecycle.push(event("prepared_base_established"));
    report_stage(observer, "Prepared immutable container base")?;

    let mut create = Command::new("docker");
    create
        .args(["create", "--name", &retained_name])
        .args(["--label", &format!("agentlab.run_id={run_id}")])
        .args(["--label", "agentlab.lifecycle=v1"])
        .args(["--label", &format!("agentlab.image_tag={prepared_tag}")])
        .args(["--workdir", &options.workspace_guest_path])
        .args(["--network", &options.network]);
    if let Some(memory) = &options.memory {
        create.args(["--memory", memory]);
    }
    if let Some(cpus) = &options.cpus {
        create.args(["--cpus", cpus]);
    }
    create.args(["--mount", PI_AUTH_TMPFS_MOUNT]);
    create.arg(&prepared_image_id).args([
        "/bin/sh",
        "-c",
        "trap 'exit 0' TERM INT; while :; do sleep 3600 & wait $!; done",
    ]);
    let retained_id = docker_success(&mut create, "create retained run container")?;
    lifecycle.push(event("retained_container_created"));
    let pre_run_inspect = docker_output_bytes(
        Command::new("docker").args(["inspect", &retained_name]),
        "inspect retained container before execution",
    )?;
    ensure_no_external_mounts(&pre_run_inspect)?;
    docker_status(
        Command::new("docker").args(["rm", &preparation_name]),
        "remove preparation container",
    )?;

    docker_status(
        Command::new("docker").args(["start", &retained_name]),
        "start retained container supervisor",
    )?;
    lifecycle.push(event("retained_container_started"));
    let mut runtime_secret_lease = if options.pi_auth.is_some() || !options.secret_files.is_empty()
    {
        report_stage(
            observer,
            "Opening a command-scoped runtime credential lease",
        )?;
        Some(RuntimeSecretLease::begin(
            store,
            &run_id,
            &retained_name,
            secret_injection_names(options),
            None,
        )?)
    } else {
        None
    };
    let mut secret_file_guard = if options.secret_files.is_empty() {
        None
    } else {
        report_stage(observer, "Injecting runtime secret files for this command")?;
        Some(inject_secret_files(&retained_name, &options.secret_files)?)
    };
    let mut pi_auth_guard = if let Some(auth_path) = &options.pi_auth {
        report_stage(
            observer,
            "Injecting host Pi authentication for this command",
        )?;
        Some(inject_pi_auth(&retained_name, auth_path)?)
    } else {
        None
    };
    lifecycle.push(event("command_started"));
    report_stage(
        observer,
        &format!("Running command: {}", display_command(&options.command)),
    )?;
    let command_output = execute_guest_command(&retained_name, &options.command, observer);
    if command_output
        .as_ref()
        .is_ok_and(|output| output.cancelled || output.timed_out)
    {
        docker_status(
            Command::new("docker").args(["stop", "--time", "1", &retained_name]),
            "stop retained container after interrupted guest execution",
        )?;
        docker_status(
            Command::new("docker").args(["start", &retained_name]),
            "restart retained container with empty runtime memory",
        )?;
    }
    let auth_cleanup = match &mut pi_auth_guard {
        Some(guard) => guard.cleanup(),
        None => Ok(()),
    };
    let secret_file_cleanup = match &mut secret_file_guard {
        Some(guard) => guard.cleanup(),
        None => Ok(()),
    };
    let lease_cleanup = match &mut runtime_secret_lease {
        Some(lease) => lease.complete(),
        None => Ok(()),
    };
    let command_output = command_output?;
    auth_cleanup?;
    secret_file_cleanup?;
    lease_cleanup?;
    if command_output.cancelled {
        if runtime_secret_lease.is_some() {
            bail!("agent command interrupted; runtime credentials were revoked");
        }
        bail!("agent command interrupted");
    }
    let exit_code = command_output.exit_code;
    let mut command_output_warnings = Vec::new();
    if command_output.stdout.truncated {
        command_output_warnings.push(format!(
            "guest stdout exceeded {} bytes ({} bytes received); live display and retained stdout are truncated",
            crate::process::MAX_RUN_OUTPUT_BYTES,
            command_output.stdout.total_bytes
        ));
    }
    if command_output.stderr.truncated {
        command_output_warnings.push(format!(
            "guest stderr exceeded {} bytes ({} bytes received); live display and retained stderr are truncated",
            crate::process::MAX_RUN_OUTPUT_BYTES,
            command_output.stderr.total_bytes
        ));
    }
    if command_output.timed_out {
        command_output_warnings.push(format!(
            "guest command exceeded the automatic {} second safety timeout and was terminated",
            crate::process::DEFAULT_GUEST_TIMEOUT_SECONDS
        ));
    }
    if command_output.stdout.truncated || command_output.stderr.truncated {
        report_stage(
            observer,
            "Guest output reached AgentLab's capture limit; filesystem capture is continuing",
        )?;
    }
    if command_output.timed_out {
        report_stage(
            observer,
            "Guest command reached AgentLab's 24-hour fail-safe deadline; finalizing its filesystem evidence",
        )?;
    }
    lifecycle.push(event("command_completed"));
    report_stage(
        observer,
        &format!("Command completed with exit code {exit_code}"),
    )?;
    let stdout = write_artifact(
        store,
        &run_id,
        "artifacts/stdout.bin",
        &command_output.stdout.bytes,
    )?;
    let stderr = write_artifact(
        store,
        &run_id,
        "artifacts/stderr.bin",
        &command_output.stderr.bytes,
    )?;

    report_stage(observer, "Finalizing one immutable result filesystem")?;
    let mut quiesced = quiesce_container(&retained_name)?;
    lifecycle.push(event("result_container_quiesced"));
    let stopped_inspect = docker_output_bytes(
        Command::new("docker").args(["inspect", &retained_name]),
        "inspect quiesced retained container",
    )?;
    ensure_no_external_mounts(&stopped_inspect)?;
    let docker_diff_bytes = docker_output_bytes(
        Command::new("docker").args(["diff", &retained_name]),
        "collect Docker filesystem diff",
    )?;
    let docker_diff = write_artifact(
        store,
        &run_id,
        "evidence/docker-diff.txt",
        &docker_diff_bytes,
    )?;
    lifecycle.push(event("docker_evidence_collected"));

    let result_export_path = run_directory.join("artifacts/result-rootfs.tar");
    docker_status(
        Command::new("docker").args([
            "export",
            "--output",
            result_export_path
                .to_str()
                .context("run path is not UTF-8")?,
            &retained_name,
        ]),
        "export result root filesystem",
    )?;
    lifecycle.push(event("result_rootfs_exported"));
    let captures = export_captures(store, &run_id, &retained_name, &options.captures)?;
    lifecycle.push(event("requested_captures_exported"));
    quiesced.restart()?;
    lifecycle.push(event("retained_container_restarted_after_capture"));
    let result_inspect_bytes = docker_output_bytes(
        Command::new("docker").args(["inspect", &retained_name]),
        "inspect retained container after immutable capture",
    )?;
    ensure_no_external_mounts(&result_inspect_bytes)?;
    let (_, retained_state) = container_status(&result_inspect_bytes)?;
    let result_inspect = write_artifact(
        store,
        &run_id,
        "evidence/result-inspect.json",
        &result_inspect_bytes,
    )?;

    report_stage(observer, "Scanning complete base filesystem")?;
    let base_manifest = rootfs::scan_export(&base_export_path)?;
    report_stage(
        observer,
        &format!(
            "Base filesystem scanned: {} paths",
            base_manifest.entries.len()
        ),
    )?;
    report_stage(observer, "Scanning complete result filesystem")?;
    let result_manifest = rootfs::scan_export(&result_export_path)?;
    report_stage(
        observer,
        &format!(
            "Result filesystem scanned: {} paths",
            result_manifest.entries.len()
        ),
    )?;
    let base_manifest_bytes = pretty_json(&base_manifest)?;
    let result_manifest_bytes = pretty_json(&result_manifest)?;
    store.write_run_file(&run_id, "base-rootfs.json", &base_manifest_bytes)?;
    store.write_run_file(&run_id, "result-rootfs.json", &result_manifest_bytes)?;
    lifecycle.push(event("portable_rootfs_manifests_created"));

    let all_changes = rootfs::compare(&base_manifest, &result_manifest);
    report_stage(
        observer,
        &format!(
            "Complete filesystem comparison: {} changes",
            all_changes.len()
        ),
    )?;
    let required_base_blob_paths = required_base_file_paths(&all_changes);
    if !required_base_blob_paths.is_empty() {
        report_stage(
            observer,
            &format!(
                "Preserving before-content for {} changed files",
                required_base_blob_paths.len()
            ),
        )?;
        rootfs::store_required_file_blobs(
            &base_export_path,
            &base_manifest,
            &required_base_blob_paths,
            store,
        )?;
    }
    let required_blob_paths = required_result_file_paths(
        &result_manifest,
        &all_changes,
        &options.workspace_guest_path,
    );
    report_stage(
        observer,
        &format!(
            "Preserving content for {} required files",
            required_blob_paths.len()
        ),
    )?;
    let blob_storage = rootfs::store_required_file_blobs(
        &result_export_path,
        &result_manifest,
        &required_blob_paths,
        store,
    )?;
    report_stage(
        observer,
        &format!(
            "Required content ready: {} unique blobs, {} reused, {} new",
            blob_storage.unique_blobs, blob_storage.reused_blobs, blob_storage.created_blobs
        ),
    )?;
    let ignored_paths = match &change_ignore_rules {
        Some(rules) => evaluate_change_ignore_bytes(rules, &all_changes)?,
        None => HashSet::new(),
    };
    let mut portable_changes = Vec::new();
    let mut ignored_changes = Vec::new();
    for change in &all_changes {
        if ignored_paths.contains(&change.path) {
            ignored_changes.push(IgnoredChange {
                path: change.path.clone(),
                change: change.change.clone(),
            });
        } else {
            portable_changes.push(change.clone());
        }
    }
    let raw_delta = make_delta(
        &base_manifest,
        &result_manifest,
        &change_ignore,
        all_changes.clone(),
        Vec::new(),
    )?;
    let portable_delta = make_delta(
        &base_manifest,
        &result_manifest,
        &change_ignore,
        portable_changes,
        ignored_changes,
    )?;
    let raw_delta_bytes = pretty_json(&raw_delta)?;
    let portable_delta_bytes = pretty_json(&portable_delta)?;
    store.write_run_file(&run_id, "delta.raw.json", &raw_delta_bytes)?;
    store.write_run_file(&run_id, "delta.json", &portable_delta_bytes)?;
    lifecycle.push(event("portable_delta_created"));

    let base_export = artifact_for_file("artifacts/base-rootfs.tar", &base_export_path)?;
    let result_export = artifact_for_file("artifacts/result-rootfs.tar", &result_export_path)?;
    let uncovered = uncovered_by_docker_diff(&all_changes, &docker_diff_bytes);
    let mut warnings = workspace_warnings;
    warnings.extend(command_output_warnings);
    warnings.push(
        "the container was quiesced before result capture; background processes were terminated"
            .to_owned(),
    );
    if !uncovered.is_empty() {
        warnings.push(format!(
            "{} normalized rootfs changes were not covered by a Docker diff path; see docker_diff_uncovered_changes",
            uncovered.len()
        ));
    }
    warnings.extend(sensitive_path_warnings(&all_changes));

    let docker = DockerEvidence {
        prepared_image_id,
        retained_container_id: retained_id,
        retained_container_name: retained_name.clone(),
        retained_container_state: retained_state,
        preparation_inspect,
        result_inspect,
        docker_diff,
        base_rootfs_export: base_export,
        result_rootfs_export: result_export,
        docker_diff_uncovered_changes: uncovered,
    };
    let observations = ObservationStatus {
        persistent_root_filesystem: "observed_and_captured".to_string(),
        ignored_portable_changes: if portable_delta.ignored_changes.is_empty() {
            "none".to_string()
        } else {
            "observed_but_deliberately_ignored".to_string()
        },
        pseudo_filesystems: "runtime_only_nonportable".to_string(),
        live_process_memory: "not_observed_by_backend".to_string(),
        writable_external_mounts: "rejected".to_string(),
    };
    let completed_at = Utc::now();
    lifecycle.push(event("result_finalized"));
    let mut integrity = BTreeMap::new();
    for artifact in [
        &stdout,
        &stderr,
        &docker.preparation_inspect,
        &docker.result_inspect,
        &docker.docker_diff,
        &docker.base_rootfs_export,
        &docker.result_rootfs_export,
    ] {
        integrity.insert(artifact.path.clone(), artifact.digest.clone());
    }
    integrity.insert("spec.json".to_string(), spec_digest.clone());
    integrity.insert(
        "evidence/image-inspect.json".to_string(),
        sha256_bytes(&resolved_image.inspect),
    );
    if let Some(rules) = &change_ignore_rules {
        integrity.insert("change-ignore.rules".to_string(), sha256_bytes(rules));
    }
    integrity.insert(
        "base-rootfs.json".to_string(),
        sha256_bytes(&base_manifest_bytes),
    );
    integrity.insert(
        "result-rootfs.json".to_string(),
        sha256_bytes(&result_manifest_bytes),
    );
    integrity.insert("delta.raw.json".to_string(), sha256_bytes(&raw_delta_bytes));
    integrity.insert(
        "delta.json".to_string(),
        sha256_bytes(&portable_delta_bytes),
    );
    for capture in &captures {
        integrity.insert(capture.path.clone(), capture.digest.clone());
    }
    let identity = ResultIdentity {
        schema_version: RESULT_SCHEMA_VERSION,
        run_id: &run_id,
        run_spec_digest: &spec_digest,
        started_at,
        completed_at,
        lifecycle: &lifecycle,
        exit_code,
        stdout: &stdout,
        stderr: &stderr,
        captures: &captures,
        base_filesystem_digest: &base_manifest.digest,
        result_filesystem_digest: &result_manifest.digest,
        raw_delta_digest: &raw_delta.digest,
        portable_delta_digest: &portable_delta.digest,
        docker: &docker,
        observations: &observations,
        warnings: &warnings,
        integrity: &integrity,
    };
    let result_digest = sha256_bytes(&serde_json::to_vec(&identity)?);
    let result = RunResult {
        schema_version: RESULT_SCHEMA_VERSION.to_string(),
        digest: result_digest.clone(),
        run_id: run_id.clone(),
        run_spec_digest: spec_digest,
        started_at,
        completed_at,
        lifecycle,
        exit_code,
        stdout,
        stderr,
        captures,
        base_filesystem_digest: base_manifest.digest,
        result_filesystem_digest: result_manifest.digest,
        raw_delta_digest: raw_delta.digest,
        portable_delta_digest: portable_delta.digest,
        docker,
        observations,
        warnings,
        integrity,
    };
    store.write_run_file(&run_id, "result.json", &pretty_json(&result)?)?;
    failed_run_cleanup.armed = false;
    failed_run_directory_cleanup.armed = false;
    let source_workspace_status = match &options.workspace {
        WorkspaceSource::Directory(workspace) => {
            report_stage(observer, "Verifying source workspace remained unchanged")?;
            match snapshot::create_with_mode(workspace, store, options.workspace_capture_mode) {
                Ok(after) if after.manifest.digest == workspace_manifest.digest => {
                    report_stage(observer, "Source workspace unchanged")?;
                    "unchanged".to_owned()
                }
                Ok(after) => {
                    report_stage(
                        observer,
                        &format!(
                            "Source workspace changed independently: {} -> {}",
                            workspace_manifest.digest, after.manifest.digest
                        ),
                    )?;
                    "changed".to_owned()
                }
                Err(error) => {
                    report_stage(
                        observer,
                        &format!("Source workspace verification failed: {error:#}"),
                    )?;
                    "verification_failed".to_owned()
                }
            }
        }
        WorkspaceSource::Snapshot(_) => "not_applicable".to_owned(),
    };
    report_stage(observer, &format!("Run finalized: {run_id}"))?;
    Ok(RunSummary {
        run_id,
        result_digest,
        run_input_digest: spec.run_input_digest,
        workspace_snapshot_digest: workspace_manifest.digest,
        image_resolved_digest: resolved_image.resolved_digest,
        exit_code,
        changes: portable_delta.changes.len(),
        ignored_changes: portable_delta.ignored_changes.len(),
        retained_container_name: retained_name,
        retained_container_id: result.docker.retained_container_id,
        source_workspace_status,
        accepted_input: spec.accepted_input,
    })
}

pub fn load_result(store: &Store, run_id: &str) -> Result<RunResult> {
    serde_json::from_slice(&store.read_run_file(run_id, "result.json")?)
        .context("decode run result")
}

pub fn load_spec(store: &Store, run_id: &str) -> Result<RunSpec> {
    let spec: RunSpec = serde_json::from_slice(&store.read_run_file(run_id, "spec.json")?)
        .context("decode run spec")?;
    if !matches!(
        spec.schema_version.as_str(),
        RUN_SCHEMA_VERSION | LEGACY_RUN_SCHEMA_VERSION_V2 | LEGACY_RUN_SCHEMA_VERSION
    ) {
        bail!(
            "unsupported run specification schema {:?}",
            spec.schema_version
        );
    }
    let computed = compute_run_input_digest(&spec)?;
    if matches!(
        spec.schema_version.as_str(),
        RUN_SCHEMA_VERSION | LEGACY_RUN_SCHEMA_VERSION_V2
    ) && spec.run_input_digest != computed
    {
        bail!(
            "run input identity mismatch: recorded {}, computed {computed}",
            spec.run_input_digest
        );
    }
    Ok(spec)
}

pub fn load_delta(store: &Store, run_id: &str, raw: bool) -> Result<DeltaManifest> {
    let name = if raw { "delta.raw.json" } else { "delta.json" };
    let bytes = store.read_run_file(run_id, name)?;
    let delta: DeltaManifest =
        serde_json::from_slice(&bytes).with_context(|| format!("decode {name}"))?;
    verify_delta(&delta)?;

    let result = load_result(store, run_id)?;
    verify_result_identity(store, &result)?;
    let recorded_delta = if raw {
        &result.raw_delta_digest
    } else {
        &result.portable_delta_digest
    };
    if &delta.digest != recorded_delta
        || delta.base_filesystem_digest != result.base_filesystem_digest
        || delta.result_filesystem_digest != result.result_filesystem_digest
    {
        bail!("{name} does not match run result {run_id:?}");
    }
    let expected_artifact = result
        .integrity
        .get(name)
        .with_context(|| format!("run result does not authenticate {name}"))?;
    let actual_artifact = sha256_bytes(&bytes);
    if &actual_artifact != expected_artifact {
        bail!(
            "run artifact integrity mismatch for {name:?}: expected {expected_artifact}, got {actual_artifact}"
        );
    }
    Ok(delta)
}

pub fn verify_result(store: &Store, result: &RunResult) -> Result<()> {
    verify_result_identity(store, result)?;
    let spec = load_spec(store, &result.run_id)?;
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
    }
    for (relative, expected) in &result.integrity {
        let bytes = store
            .read_run_file(&result.run_id, relative)
            .with_context(|| format!("verify run artifact {relative:?}"))?;
        let actual = sha256_bytes(&bytes);
        if &actual != expected {
            bail!(
                "run artifact integrity mismatch for {relative:?}: expected {expected}, got {actual}"
            );
        }
    }
    Ok(())
}

pub(crate) fn verify_result_identity(store: &Store, result: &RunResult) -> Result<()> {
    if result.schema_version != RESULT_SCHEMA_VERSION {
        bail!("unsupported run result schema {:?}", result.schema_version);
    }
    let spec = load_spec(store, &result.run_id)?;
    if spec.run_id != result.run_id {
        bail!("run specification and result IDs do not agree");
    }
    let actual_spec_digest = sha256_bytes(&store.read_run_file(&result.run_id, "spec.json")?);
    if actual_spec_digest != result.run_spec_digest {
        bail!(
            "run specification digest mismatch: result records {}, got {actual_spec_digest}",
            result.run_spec_digest
        );
    }
    let identity = ResultIdentity {
        schema_version: RESULT_SCHEMA_VERSION,
        run_id: &result.run_id,
        run_spec_digest: &result.run_spec_digest,
        started_at: result.started_at,
        completed_at: result.completed_at,
        lifecycle: &result.lifecycle,
        exit_code: result.exit_code,
        stdout: &result.stdout,
        stderr: &result.stderr,
        captures: &result.captures,
        base_filesystem_digest: &result.base_filesystem_digest,
        result_filesystem_digest: &result.result_filesystem_digest,
        raw_delta_digest: &result.raw_delta_digest,
        portable_delta_digest: &result.portable_delta_digest,
        docker: &result.docker,
        observations: &result.observations,
        warnings: &result.warnings,
        integrity: &result.integrity,
    };
    let actual = sha256_bytes(&serde_json::to_vec(&identity)?);
    if actual != result.digest {
        bail!(
            "run result integrity mismatch: expected {}, got {actual}",
            result.digest
        );
    }
    Ok(())
}

pub(crate) fn verify_delta(delta: &DeltaManifest) -> Result<()> {
    if delta.schema_version != DELTA_SCHEMA_VERSION {
        bail!("unsupported delta schema {:?}", delta.schema_version);
    }
    let identity = DeltaIdentity {
        schema_version: DELTA_SCHEMA_VERSION,
        base_filesystem_digest: &delta.base_filesystem_digest,
        result_filesystem_digest: &delta.result_filesystem_digest,
        change_ignore: &delta.change_ignore,
        changes: &delta.changes,
        ignored_changes: &delta.ignored_changes,
    };
    let actual = sha256_bytes(&serde_json::to_vec(&identity)?);
    if actual != delta.digest {
        bail!(
            "delta integrity mismatch: expected {}, got {actual}",
            delta.digest
        );
    }
    Ok(())
}

pub fn compare_runs(store: &Store, left_run_id: &str, right_run_id: &str) -> Result<RunComparison> {
    if left_run_id == right_run_id {
        bail!("compare requires two distinct runs");
    }
    let left_result = load_result(store, left_run_id)?;
    let right_result = load_result(store, right_run_id)?;
    verify_result(store, &left_result)?;
    verify_result(store, &right_result)?;
    let left = load_spec(store, left_run_id)?;
    let right = load_spec(store, right_run_id)?;
    if left.run_id != left_result.run_id || right.run_id != right_result.run_id {
        bail!("run specification and result IDs do not agree");
    }

    let mut controlled = Vec::new();
    compare_field(
        &mut controlled,
        "workspace_snapshot_digest",
        &left.workspace_snapshot_digest,
        &right.workspace_snapshot_digest,
    );
    compare_field(
        &mut controlled,
        "image_resolved_digest",
        &left.image_resolved_digest,
        &right.image_resolved_digest,
    );
    compare_field(
        &mut controlled,
        "target_platform",
        &left.target_platform,
        &right.target_platform,
    );
    compare_field(
        &mut controlled,
        "workspace_guest_path",
        &left.workspace_guest_path,
        &right.workspace_guest_path,
    );
    compare_field(&mut controlled, "command", &left.command, &right.command);
    compare_field(
        &mut controlled,
        "working_directory",
        &left.working_directory,
        &right.working_directory,
    );
    compare_field(
        &mut controlled,
        "resource_limits",
        &left.resource_limits,
        &right.resource_limits,
    );
    compare_field(
        &mut controlled,
        "network_policy",
        &left.network_policy,
        &right.network_policy,
    );
    compare_field(&mut controlled, "captures", &left.captures, &right.captures);
    compare_field(
        &mut controlled,
        "secret_injections",
        &left.secret_injections,
        &right.secret_injections,
    );
    compare_field(
        &mut controlled,
        "workspace_ignore_digest",
        &left.workspace_ignore_digest,
        &right.workspace_ignore_digest,
    );
    compare_field(
        &mut controlled,
        "change_ignore_digest",
        &left.change_ignore.digest,
        &right.change_ignore.digest,
    );
    compare_field(
        &mut controlled,
        "backend_name",
        &left.backend_name,
        &right.backend_name,
    );
    compare_field(
        &mut controlled,
        "backend_version",
        &left.backend_version,
        &right.backend_version,
    );
    compare_field(
        &mut controlled,
        "agentlab_version",
        &left.agentlab_version,
        &right.agentlab_version,
    );

    let left_run_input_digest = compute_run_input_digest(&left)?;
    let right_run_input_digest = compute_run_input_digest(&right)?;
    let same_run_input = left_run_input_digest == right_run_input_digest;
    let same_workspace_snapshot = left.workspace_snapshot_digest == right.workspace_snapshot_digest;
    let same_resolved_image = left.image_resolved_digest == right.image_resolved_digest;
    let same_portable_base =
        left_result.base_filesystem_digest == right_result.base_filesystem_digest;
    let distinct_private_containers = left_result.docker.retained_container_id
        != right_result.docker.retained_container_id
        && left_result.docker.retained_container_name
            != right_result.docker.retained_container_name;
    let comparable_repetition = same_run_input
        && controlled.is_empty()
        && same_workspace_snapshot
        && same_resolved_image
        && same_portable_base
        && distinct_private_containers;
    let comparison_kind = if comparable_repetition {
        "comparable_repetition"
    } else if !same_run_input || !controlled.is_empty() {
        "different_inputs"
    } else {
        "same_inputs_not_independent"
    };
    Ok(RunComparison {
        left_run_id: left_run_id.to_owned(),
        right_run_id: right_run_id.to_owned(),
        left_run_input_digest,
        right_run_input_digest,
        same_run_input,
        same_workspace_snapshot,
        same_resolved_image,
        same_portable_base,
        distinct_private_containers,
        controlled_input_differences: controlled,
        comparison_kind: comparison_kind.to_owned(),
        comparable_repetition,
        portable_outcomes_equal: left_result.result_filesystem_digest
            == right_result.result_filesystem_digest,
    })
}

pub fn compute_run_input_digest(spec: &RunSpec) -> Result<String> {
    let identity = RunInputIdentity {
        schema_version: RUN_INPUT_SCHEMA_VERSION,
        workspace_snapshot_digest: &spec.workspace_snapshot_digest,
        image_resolved_digest: &spec.image_resolved_digest,
        target_platform: &spec.target_platform,
        workspace_guest_path: &spec.workspace_guest_path,
        command: &spec.command,
        working_directory: &spec.working_directory,
        resource_limits: &spec.resource_limits,
        network_policy: &spec.network_policy,
        captures: &spec.captures,
        secret_injections: &spec.secret_injections,
        workspace_ignore_digest: &spec.workspace_ignore_digest,
        change_ignore_digest: &spec.change_ignore.digest,
        backend_name: &spec.backend_name,
        backend_version: &spec.backend_version,
        agentlab_version: &spec.agentlab_version,
    };
    Ok(sha256_bytes(&serde_json::to_vec(&identity)?))
}

fn compare_field<T: PartialEq>(differences: &mut Vec<String>, name: &str, left: &T, right: &T) {
    if left != right {
        differences.push(name.to_owned());
    }
}

fn validate_options(options: &RunOptions) -> Result<()> {
    if options.image.trim().is_empty() {
        bail!("run requires --image IMAGE");
    }
    if options.command.is_empty() {
        bail!("run requires a command after --");
    }
    if options.accepted_input.is_some()
        && !matches!(options.workspace, WorkspaceSource::Snapshot(_))
    {
        bail!("an accepted input must run from its immutable stored snapshot");
    }
    validate_guest_path(&options.workspace_guest_path)?;
    if !matches!(options.network.as_str(), "none" | "bridge") {
        bail!("network policy must be either none or bridge in Milestone 2");
    }
    if let Some(path) = &options.pi_auth {
        validate_pi_auth(path)?;
    }
    validate_secret_files(&options.secret_files, options.pi_auth.as_deref())?;
    let mut capture_names = HashSet::new();
    for capture in &options.captures {
        validate_guest_path(&capture.guest_path)?;
        if capture.name.is_empty()
            || capture.name.contains('/')
            || capture.name == "."
            || capture.name == ".."
        {
            bail!("invalid capture name {:?}", capture.name);
        }
        if !capture_names.insert(capture.name.as_str()) {
            bail!("duplicate capture name {:?}", capture.name);
        }
    }
    Ok(())
}

pub fn immutable_image_reference(store: &Store, run_id: &str) -> Result<String> {
    let spec = load_spec(store, run_id)?;
    let inspect = store.read_run_file(run_id, "evidence/image-inspect.json")?;
    let value: Value = serde_json::from_slice(&inspect).context("decode Docker image evidence")?;
    let image = value
        .as_array()
        .and_then(|values| values.first())
        .context("Docker image evidence contains no image")?;
    let evidence_id = image["Id"]
        .as_str()
        .context("Docker image evidence omitted Id")?;
    if evidence_id != spec.docker_image_id {
        bail!("Docker image evidence does not agree with the run specification");
    }
    let mut repo_digests: Vec<_> = image["RepoDigests"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    repo_digests.sort();
    let reference = repo_digests
        .into_iter()
        .next()
        .unwrap_or_else(|| spec.docker_image_id.clone());
    let resolved = reference
        .rsplit_once('@')
        .map(|(_, digest)| digest)
        .unwrap_or(reference.as_str());
    if resolved != spec.image_resolved_digest {
        bail!("immutable image reference does not agree with the run specification");
    }
    Ok(reference)
}

fn validate_guest_path(path: &str) -> Result<()> {
    if !path.starts_with('/')
        || path == "/"
        || path
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("guest path must be an absolute normalized non-root path: {path:?}");
    }
    Ok(())
}

fn resolve_image(image: &str) -> Result<ResolvedImage> {
    let inspect = match docker_image_inspect(image) {
        Ok(value) => value,
        Err(_) => {
            docker_status(
                Command::new("docker").args(["pull", image]),
                "pull requested OCI image",
            )?;
            docker_image_inspect(image)?
        }
    };
    let value: Value = serde_json::from_slice(&inspect).context("decode Docker image inspect")?;
    let image_value = value
        .as_array()
        .and_then(|values| values.first())
        .context("Docker image inspect returned no image")?;
    if image_value
        .pointer("/Config/Volumes")
        .and_then(Value::as_object)
        .is_some_and(|volumes| !volumes.is_empty())
    {
        bail!(
            "image {image:?} declares persistent volumes that Docker export cannot capture completely"
        );
    }
    let docker_image_id = image_value["Id"]
        .as_str()
        .context("Docker image inspect omitted Id")?
        .to_string();
    let mut repo_digests: Vec<_> = image_value["RepoDigests"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    repo_digests.sort();
    let execution_reference = repo_digests
        .into_iter()
        .next()
        .unwrap_or_else(|| docker_image_id.clone());
    let resolved_digest = execution_reference
        .rsplit_once('@')
        .map(|(_, digest)| digest.to_owned())
        .unwrap_or_else(|| execution_reference.clone());
    let os = image_value["Os"].as_str().unwrap_or("unknown");
    let architecture = image_value["Architecture"].as_str().unwrap_or("unknown");
    let variant = image_value["Variant"].as_str().unwrap_or("");
    Ok(ResolvedImage {
        resolved_digest,
        execution_reference,
        docker_image_id,
        platform: if variant.is_empty() {
            format!("{os}/{architecture}")
        } else {
            format!("{os}/{architecture}/{variant}")
        },
        inspect,
    })
}

fn docker_image_inspect(image: &str) -> Result<Vec<u8>> {
    docker_output_bytes(
        Command::new("docker").args(["image", "inspect", image]),
        "inspect requested OCI image",
    )
}

fn docker_version() -> Result<String> {
    docker_success(
        Command::new("docker").args(["version", "--format", "{{.Server.Version}}"]),
        "resolve Docker server version",
    )
}

pub(crate) fn ensure_no_external_mounts(inspect: &[u8]) -> Result<()> {
    let value: Value = serde_json::from_slice(inspect).context("decode Docker inspect evidence")?;
    let mounts = value
        .as_array()
        .and_then(|values| values.first())
        .and_then(|container| container["Mounts"].as_array())
        .context("Docker inspect omitted Mounts")?;
    let unsupported_mounts = mounts
        .iter()
        .filter(|mount| {
            mount["Type"].as_str() != Some("tmpfs")
                || mount["Destination"].as_str() != Some(PI_AUTH_SECRET_DIRECTORY)
        })
        .count();
    let secret_mounts = mounts
        .iter()
        .filter(|mount| {
            mount["Type"].as_str() == Some("tmpfs")
                && mount["Destination"].as_str() == Some(PI_AUTH_SECRET_DIRECTORY)
        })
        .count();
    if unsupported_mounts != 0 || secret_mounts > 1 {
        bail!(
            "agent-writable mounts outside the exported rootfs are unsupported: {unsupported_mounts} unsupported mount(s) found"
        );
    }
    Ok(())
}

pub(crate) fn ensure_pi_auth_tmpfs(inspect: &[u8]) -> Result<()> {
    ensure_no_external_mounts(inspect)?;
    let value: Value = serde_json::from_slice(inspect).context("decode Docker inspect evidence")?;
    let container = value
        .as_array()
        .and_then(|values| values.first())
        .context("Docker inspect returned no container")?;
    let configured = container
        .pointer("/HostConfig/Mounts")
        .and_then(Value::as_array)
        .is_some_and(|mounts| {
            mounts.iter().any(|mount| {
                mount["Type"].as_str() == Some("tmpfs")
                    && mount["Target"].as_str() == Some(PI_AUTH_SECRET_DIRECTORY)
                    && mount
                        .pointer("/TmpfsOptions/SizeBytes")
                        .and_then(Value::as_u64)
                        == Some(MAX_RUNTIME_SECRET_BYTES)
                    && mount.pointer("/TmpfsOptions/Mode").and_then(Value::as_u64) == Some(0o711)
            })
        });
    if !configured {
        bail!(
            "retained run predates secure continuation credentials; create a new run before using resume --pi-auth"
        );
    }
    Ok(())
}

fn validate_pi_auth(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("read host Pi authentication metadata {}", path.display()))?;
    if !metadata.is_file() {
        bail!("host Pi authentication path is not a regular file");
    }
    if metadata.len() > MAX_RUNTIME_SECRET_BYTES {
        bail!("host Pi authentication file exceeds the 1 MiB safety limit");
    }
    let bytes = fs::read(path).context("read host Pi authentication file")?;
    let value: Value =
        serde_json::from_slice(&bytes).context("decode host Pi authentication JSON")?;
    if !value.is_object() {
        bail!("host Pi authentication JSON must be an object");
    }
    Ok(())
}

fn secret_injection_names(options: &RunOptions) -> Vec<String> {
    let mut names: Vec<_> = options
        .secret_files
        .iter()
        .map(|secret| secret.name.clone())
        .collect();
    if options.pi_auth.is_some() {
        names.push(PI_AUTH_SECRET_NAME.to_owned());
    }
    names.sort();
    names
}

pub(crate) fn validate_secret_files(
    secret_files: &[SecretFileSpec],
    pi_auth: Option<&Path>,
) -> Result<()> {
    let mut names = HashSet::new();
    let mut total_bytes = match pi_auth {
        Some(path) => fs::metadata(path)
            .with_context(|| format!("read host Pi authentication metadata {}", path.display()))?
            .len(),
        None => 0,
    };
    for secret in secret_files {
        validate_secret_name(&secret.name)?;
        if !names.insert(secret.name.as_str()) {
            bail!("duplicate runtime secret name {:?}", secret.name);
        }
        let metadata = fs::metadata(&secret.source).with_context(|| {
            format!(
                "read runtime secret file metadata {}",
                secret.source.display()
            )
        })?;
        if !metadata.is_file() {
            bail!(
                "runtime secret source {} is not a regular file",
                secret.source.display()
            );
        }
        total_bytes = total_bytes
            .checked_add(metadata.len())
            .context("runtime secret size overflow")?;
    }
    if total_bytes > MAX_RUNTIME_SECRET_BYTES {
        bail!("runtime secret files exceed the combined 1 MiB safety limit");
    }
    Ok(())
}

fn validate_secret_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name == PI_AUTH_SECRET_NAME
        || name == "pi-auth.json"
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("invalid runtime secret name {name:?}; use letters, digits, '.', '_', or '-'");
    }
    Ok(())
}

fn container_user_identity(container: &str, purpose: &str) -> Result<(String, String, String)> {
    let identity = docker_success(
        Command::new("docker").args([
            "exec",
            container,
            "/bin/sh",
            "-c",
            "set -eu; uid=$(id -u); gid=$(id -g); home=${HOME:-}; if [ -z \"$home\" ]; then home=$(awk -F: -v uid=\"$uid\" '$3 == uid { print $6; exit }' /etc/passwd); fi; case \"$home\" in /*) ;; *) exit 14 ;; esac; printf '%s\\n%s\\n%s\\n' \"$uid\" \"$gid\" \"$home\"",
        ]),
        purpose,
    )?;
    let mut lines = identity.lines();
    let uid = lines.next().context("container user lookup omitted uid")?;
    let gid = lines.next().context("container user lookup omitted gid")?;
    let home = lines
        .next()
        .context("container user lookup omitted home directory")?;
    if lines.next().is_some()
        || uid.parse::<u32>().is_err()
        || gid.parse::<u32>().is_err()
        || !home.starts_with('/')
    {
        bail!("container returned an invalid user identity for {purpose}");
    }
    Ok((uid.to_owned(), gid.to_owned(), home.to_owned()))
}

#[derive(Serialize, Deserialize)]
struct RuntimeSecretLeaseRecord {
    schema_version: String,
    run_id: String,
    container_name: String,
    container_id: String,
    started_at: DateTime<Utc>,
    secret_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    incomplete_artifact_directory: Option<String>,
}

/// Durable indication that a retained container may still contain a runtime
/// credential. The marker is written before injection and removed only after
/// the reserved tmpfs and Pi symlink have both been scrubbed.
pub(crate) struct RuntimeSecretLease {
    store: Store,
    run_id: String,
    container: String,
    active: bool,
    _lock: AdvisoryLock,
}

impl RuntimeSecretLease {
    pub(crate) fn begin(
        store: &Store,
        run_id: &str,
        container: &str,
        mut secret_names: Vec<String>,
        incomplete_artifact_directory: Option<String>,
    ) -> Result<Self> {
        recover_all_interrupted_secret_leases(store)?;
        let lock_path = store.run_path(run_id, RUNTIME_SECRET_LEASE_LOCK_PATH)?;
        let lease_lock = AdvisoryLock::acquire(
            &lock_path,
            &format!("runtime credential lease for run {run_id}"),
        )?;
        if store.run_file_exists(run_id, RUNTIME_SECRET_LEASE_PATH)? {
            bail!(
                "run {run_id:?} has an interrupted runtime credential lease; recover it before injecting credentials again"
            );
        }
        let inspect = docker_output_bytes(
            Command::new("docker").args(["inspect", container]),
            "inspect container before opening runtime credential lease",
        )?;
        let container_id = validate_runtime_secret_container(&inspect, run_id, container, None)?;
        scrub_runtime_secrets(container)?;
        secret_names.sort();
        secret_names.dedup();
        let record = RuntimeSecretLeaseRecord {
            schema_version: RUNTIME_SECRET_LEASE_SCHEMA.to_owned(),
            run_id: run_id.to_owned(),
            container_name: container.to_owned(),
            container_id,
            started_at: Utc::now(),
            secret_names,
            incomplete_artifact_directory,
        };
        store.write_run_file(run_id, RUNTIME_SECRET_LEASE_PATH, &pretty_json(&record)?)?;
        Ok(Self {
            store: store.clone(),
            run_id: run_id.to_owned(),
            container: container.to_owned(),
            active: true,
            _lock: lease_lock,
        })
    }

    pub(crate) fn complete(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        scrub_runtime_secrets(&self.container)?;
        remove_secret_lease_marker(&self.store, &self.run_id)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for RuntimeSecretLease {
    fn drop(&mut self) {
        let _ = self.complete();
    }
}

pub(crate) fn interrupted_secret_lease_exists(store: &Store, run_id: &str) -> Result<bool> {
    store.run_file_exists(run_id, RUNTIME_SECRET_LEASE_PATH)
}

pub(crate) fn recover_interrupted_secret_lease(
    store: &Store,
    run_id: &str,
    container: &str,
) -> Result<bool> {
    if !interrupted_secret_lease_exists(store, run_id)? {
        return Ok(false);
    }
    let lock_path = store.run_path(run_id, RUNTIME_SECRET_LEASE_LOCK_PATH)?;
    let _lock = AdvisoryLock::acquire(
        &lock_path,
        &format!("runtime credential lease for run {run_id}"),
    )?;
    if !interrupted_secret_lease_exists(store, run_id)? {
        return Ok(false);
    }
    recover_interrupted_secret_lease_locked(store, run_id, container)
}

fn recover_interrupted_secret_lease_locked(
    store: &Store,
    run_id: &str,
    container: &str,
) -> Result<bool> {
    let bytes = store.read_run_file(run_id, RUNTIME_SECRET_LEASE_PATH)?;
    let record: RuntimeSecretLeaseRecord =
        serde_json::from_slice(&bytes).context("decode interrupted runtime secret lease")?;
    if record.schema_version != RUNTIME_SECRET_LEASE_SCHEMA
        || record.run_id != run_id
        || record.container_name != container
    {
        bail!("runtime secret lease does not match the selected run and container");
    }

    let inspect = docker_output_bytes(
        Command::new("docker").args(["inspect", container]),
        "inspect container with interrupted runtime credentials",
    )?;
    validate_runtime_secret_container(&inspect, run_id, container, Some(&record.container_id))?;
    if record.incomplete_artifact_directory.is_none()
        && !store.run_file_exists(run_id, "result.json")?
        && !store.run_file_exists(run_id, "fork.json")?
    {
        remove_interrupted_initial_run(store, run_id, container, &inspect)?;
        return Ok(true);
    }
    let (_, state) = container_status(&inspect)?;
    let was_running = state == "running";
    if was_running {
        docker_status(
            Command::new("docker").args(["stop", "--time", "1", container]),
            "stop container to revoke interrupted runtime credentials",
        )?;
    }
    docker_status(
        Command::new("docker").args(["start", container]),
        "restart container with an empty runtime-secret tmpfs",
    )?;
    scrub_runtime_secrets(container)?;
    if !was_running {
        docker_status(
            Command::new("docker").args(["stop", "--time", "1", container]),
            "restore stopped container state after revoking interrupted runtime credentials",
        )?;
    }
    remove_secret_lease_marker(store, run_id)?;
    if let Some(relative) = &record.incomplete_artifact_directory {
        let receipt = format!("{relative}/continuation.json");
        if !store.run_file_exists(run_id, &receipt)? {
            let directory = store.run_path(run_id, relative)?;
            match fs::remove_dir_all(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).context("remove interrupted continuation artifacts");
                }
            }
        }
    }
    Ok(true)
}

fn remove_interrupted_initial_run(
    store: &Store,
    run_id: &str,
    container: &str,
    inspect: &[u8],
) -> Result<()> {
    let value: Value = serde_json::from_slice(inspect).context("decode interrupted run inspect")?;
    let image_tag = value
        .as_array()
        .and_then(|values| values.first())
        .and_then(|container| container.pointer("/Config/Labels/agentlab.image_tag"))
        .and_then(Value::as_str)
        .context("interrupted AgentLab container omitted its private image tag")?;
    docker_status(
        Command::new("docker").args(["rm", "--force", container]),
        "remove interrupted credentialed run container",
    )?;
    let removal = crate::process::output_bounded(
        Command::new("docker").args(["image", "rm", image_tag]),
        crate::process::MAX_IGNORE_RULE_BYTES,
    )
    .context("remove interrupted credentialed run image")?;
    if !removal.status.success()
        && !String::from_utf8_lossy(&removal.stderr.bytes).contains("No such image")
    {
        bail!(
            "remove interrupted credentialed run image: {}",
            bounded_lossy_summary(&removal.stderr.bytes, 4096)
        );
    }
    store.remove_run_directory(run_id)?;
    Ok(())
}

fn validate_runtime_secret_container(
    inspect: &[u8],
    run_id: &str,
    container_name: &str,
    expected_id: Option<&str>,
) -> Result<String> {
    ensure_pi_auth_tmpfs(inspect)?;
    let value: Value =
        serde_json::from_slice(inspect).context("decode credential lease inspect")?;
    let container = value
        .as_array()
        .and_then(|values| values.first())
        .context("Docker inspect returned no credential lease container")?;
    let actual_id = container["Id"]
        .as_str()
        .context("credential lease container omitted ID")?;
    let actual_name = container["Name"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches('/');
    let actual_run = container
        .pointer("/Config/Labels/agentlab.run_id")
        .and_then(Value::as_str);
    if actual_name != container_name
        || actual_run != Some(run_id)
        || expected_id.is_some_and(|expected| expected != actual_id)
    {
        bail!("runtime credential lease container does not match the selected AgentLab run");
    }
    Ok(actual_id.to_owned())
}

fn recover_all_interrupted_secret_leases(store: &Store) -> Result<()> {
    for run_id in store.list_run_ids()? {
        if !interrupted_secret_lease_exists(store, &run_id)? {
            continue;
        }
        let lock_path = store.run_path(&run_id, RUNTIME_SECRET_LEASE_LOCK_PATH)?;
        let Some(_lock) = AdvisoryLock::try_acquire(
            &lock_path,
            &format!("runtime credential lease for run {run_id}"),
        )?
        else {
            // Another AgentLab process still owns this command-scoped lease.
            continue;
        };
        let bytes = store.read_run_file(&run_id, RUNTIME_SECRET_LEASE_PATH)?;
        let record: RuntimeSecretLeaseRecord = serde_json::from_slice(&bytes)
            .with_context(|| format!("decode runtime credential lease for run {run_id}"))?;
        recover_interrupted_secret_lease_locked(store, &run_id, &record.container_name)?;
    }
    Ok(())
}

fn remove_secret_lease_marker(store: &Store, run_id: &str) -> Result<()> {
    let path = store.run_path(run_id, RUNTIME_SECRET_LEASE_PATH)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("remove completed runtime secret lease"),
    }
}

pub(crate) fn scrub_runtime_secrets(container: &str) -> Result<()> {
    let (_, _, home) = container_user_identity(container, "scrub runtime secret locations")?;
    docker_status(
        Command::new("docker").args([
            "exec",
            "--user",
            "0",
            container,
            "/bin/sh",
            "-c",
            "set -eu; home=$1; target=\"$home/.pi/agent/auth.json\"; if [ -L \"$target\" ] && [ \"$(readlink \"$target\")\" = /run/agentlab-secrets/pi-auth.json ]; then rm -f -- \"$target\"; fi; for path in /run/agentlab-secrets/* /run/agentlab-secrets/.[!.]* /run/agentlab-secrets/..?*; do if [ -e \"$path\" ] || [ -L \"$path\" ]; then rm -f -- \"$path\"; fi; done",
            "agentlab-secret-scrub",
            &home,
        ]),
        "scrub reserved runtime secret locations",
    )
}

pub(crate) struct SecretFileGuard {
    container: String,
    paths: Vec<String>,
    active: bool,
}

impl SecretFileGuard {
    pub(crate) fn cleanup(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        if !self.paths.is_empty() {
            docker_status(
                Command::new("docker")
                    .args(["exec", "--user", "0", &self.container, "rm", "-f", "--"])
                    .args(&self.paths),
                "remove runtime secret files",
            )?;
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for SecretFileGuard {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

pub(crate) fn inject_secret_files(
    container: &str,
    secret_files: &[SecretFileSpec],
) -> Result<SecretFileGuard> {
    validate_secret_files(secret_files, None)?;
    let (uid, gid, _) = container_user_identity(container, "runtime secret injection")?;
    let owner = format!("{uid}:{gid}");
    let mut guard = SecretFileGuard {
        container: container.to_owned(),
        paths: Vec::new(),
        active: true,
    };
    for secret in secret_files {
        let target = format!("{PI_AUTH_SECRET_DIRECTORY}/{}", secret.name);
        guard.paths.push(target.clone());
        let source = File::open(&secret.source)
            .with_context(|| format!("open runtime secret file {}", secret.source.display()))?;
        docker_status(
            Command::new("docker")
                .args([
                    "exec",
                    "-i",
                    "--user",
                    "0",
                    container,
                    "/bin/sh",
                    "-c",
                    "set -eu; umask 077; target=$1; owner=$2; test -d /run/agentlab-secrets; test ! -e \"$target\"; test ! -L \"$target\"; cat > \"$target\"; chown \"$owner\" \"$target\"; chmod 600 \"$target\"",
                    "agentlab-secret-file-copy",
                    &target,
                    &owner,
                ])
                .stdin(Stdio::from(source)),
            &format!("copy runtime secret {:?} into runtime memory", secret.name),
        )?;
    }
    Ok(guard)
}

pub(crate) struct PiAuthGuard {
    container: String,
    home: String,
    active: bool,
}

impl PiAuthGuard {
    pub(crate) fn cleanup(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        docker_status(
            Command::new("docker").args([
                "exec",
                "--user",
                "0",
                &self.container,
                "/bin/sh",
                "-c",
                "set -eu; rm -f -- \"$1/.pi/agent/auth.json\" /run/agentlab-secrets/pi-auth.json",
                "agentlab-pi-auth-cleanup",
                &self.home,
            ]),
            "remove runtime Pi authentication",
        )?;
        self.active = false;
        Ok(())
    }
}

impl Drop for PiAuthGuard {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

pub(crate) fn inject_pi_auth(container: &str, source: &Path) -> Result<PiAuthGuard> {
    validate_pi_auth(source)?;
    let (uid, gid, home) =
        container_user_identity(container, "resolve container user for Pi authentication")?;

    let auth_file = File::open(source).context("open host Pi authentication file")?;
    let owner = format!("{uid}:{gid}");
    docker_status(
        Command::new("docker")
            .args([
                "exec",
                "-i",
                "--user",
                "0",
                container,
                "/bin/sh",
                "-c",
                "set -eu; umask 077; test -d /run/agentlab-secrets; test ! -e /run/agentlab-secrets/pi-auth.json; cat > /run/agentlab-secrets/pi-auth.json; chown \"$1\" /run/agentlab-secrets/pi-auth.json; chmod 600 /run/agentlab-secrets/pi-auth.json",
                "agentlab-pi-auth-copy",
                &owner,
            ])
            .stdin(Stdio::from(auth_file)),
        "copy Pi authentication into runtime memory",
    )?;

    let link_result = docker_status(
        Command::new("docker").args([
            "exec",
            container,
            "/bin/sh",
            "-c",
            "set -eu; target=\"$1/.pi/agent/auth.json\"; if [ -e \"$target\" ] || [ -L \"$target\" ]; then echo 'Pi authentication target already exists' >&2; exit 15; fi; mkdir -p \"$1/.pi/agent\"; ln -s /run/agentlab-secrets/pi-auth.json \"$target\"",
            "agentlab-pi-auth-link",
            &home,
        ]),
        "link runtime Pi authentication",
    );
    if let Err(error) = link_result {
        let _ = docker_status(
            Command::new("docker").args([
                "exec",
                "--user",
                "0",
                container,
                "rm",
                "-f",
                PI_AUTH_SECRET_PATH,
            ]),
            "remove incomplete Pi authentication injection",
        );
        return Err(error);
    }

    Ok(PiAuthGuard {
        container: container.to_owned(),
        home,
        active: true,
    })
}

pub(crate) fn container_status(inspect: &[u8]) -> Result<(i64, String)> {
    let value: Value = serde_json::from_slice(inspect).context("decode Docker inspect evidence")?;
    let container = value
        .as_array()
        .and_then(|values| values.first())
        .context("Docker inspect returned no container")?;
    Ok((
        container
            .pointer("/State/ExitCode")
            .and_then(Value::as_i64)
            .unwrap_or(-1),
        container
            .pointer("/State/Status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
    ))
}

fn resolve_change_ignore(
    options: &RunOptions,
    workspace: &snapshot::Manifest,
    store: &Store,
) -> Result<(IgnoreIdentity, Option<Vec<u8>>)> {
    let (source, bytes) = if let Some(path) = &options.change_ignore {
        let metadata = fs::metadata(path)
            .with_context(|| format!("inspect change-ignore rules {}", path.display()))?;
        if metadata.len() > crate::process::MAX_IGNORE_RULE_BYTES as u64 {
            bail!(
                "change-ignore rules {} exceed the {} byte limit",
                path.display(),
                crate::process::MAX_IGNORE_RULE_BYTES
            );
        }
        (
            path.display().to_string(),
            fs::read(path)
                .with_context(|| format!("read change-ignore rules {}", path.display()))?,
        )
    } else if let Some(entry) = workspace
        .entries
        .iter()
        .find(|entry| entry.path == ".agentlabignore" && entry.kind == "file")
    {
        if entry.size > crate::process::MAX_IGNORE_RULE_BYTES as u64 {
            bail!(
                "snapshotted .agentlabignore exceeds the {} byte limit",
                crate::process::MAX_IGNORE_RULE_BYTES
            );
        }
        let mut bytes = Vec::new();
        store
            .open_blob(&entry.digest, entry.size)?
            .read_to_end(&mut bytes)
            .context("read snapshotted .agentlabignore")?;
        ("workspace:.agentlabignore".to_owned(), bytes)
    } else {
        return Ok((
            IgnoreIdentity {
                source: None,
                digest: format!("sha256:{}", hex_digest(&Sha256::digest([]))),
            },
            None,
        ));
    };
    Ok((
        IgnoreIdentity {
            source: Some(source),
            digest: format!("sha256:{}", hex_digest(&Sha256::digest(&bytes))),
        },
        Some(bytes),
    ))
}

pub(crate) fn evaluate_change_ignore_bytes(
    rules: &[u8],
    changes: &[RootFsChange],
) -> Result<HashSet<String>> {
    evaluate_change_ignore_bytes_version(rules, changes, true)
}

pub(crate) fn evaluate_change_ignore_bytes_legacy(
    rules: &[u8],
    changes: &[RootFsChange],
) -> Result<HashSet<String>> {
    evaluate_change_ignore_bytes_version(rules, changes, false)
}

fn evaluate_change_ignore_bytes_version(
    rules: &[u8],
    changes: &[RootFsChange],
    mark_directories: bool,
) -> Result<HashSet<String>> {
    let temporary = tempfile::tempdir()?;
    fs::write(temporary.path().join(".gitignore"), rules)?;
    let git_directory = temporary.path().join("ignore.git");
    git_status(
        Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&git_directory),
        "initialize change-ignore evaluator",
    )?;
    let mut command = Command::new("git");
    command
        .arg(format!("--git-dir={}", git_directory.display()))
        .arg(format!("--work-tree={}", temporary.path().display()))
        .args(["-c", "core.excludesFile=/dev/null"])
        .args(["check-ignore", "--no-index", "-z", "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::process::isolate_process_group(&mut command);
    let mut input_bytes = Vec::new();
    for change in changes {
        input_bytes.extend_from_slice(change.path.trim_start_matches('/').as_bytes());
        if mark_directories
            && change
                .after
                .as_ref()
                .or(change.before.as_ref())
                .is_some_and(|entry| entry.kind == "directory")
        {
            input_bytes.push(b'/');
        }
        input_bytes.push(0);
        if input_bytes.len() > crate::process::MAX_COMMAND_METADATA_BYTES {
            bail!(
                "change-ignore path input exceeds the {} byte limit",
                crate::process::MAX_COMMAND_METADATA_BYTES
            );
        }
    }
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().context("open change-ignore input")?;
    let stdout = child.stdout.take().context("open change-ignore stdout")?;
    let stderr = child.stderr.take().context("open change-ignore stderr")?;
    let input_writer = std::thread::spawn(move || stdin.write_all(&input_bytes));
    let stdout_reader = std::thread::spawn(move || {
        crate::process::read_bounded(stdout, crate::process::MAX_COMMAND_METADATA_BYTES)
    });
    let stderr_reader = std::thread::spawn(move || {
        crate::process::read_bounded(stderr, crate::process::MAX_IGNORE_RULE_BYTES)
    });
    let status = child.wait()?;
    let _ = crate::process::terminate_process_group(&mut child);
    input_writer
        .join()
        .map_err(|_| anyhow::anyhow!("change-ignore input writer panicked"))??;
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("change-ignore stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("change-ignore stderr reader panicked"))??;
    if stdout.truncated || stderr.truncated {
        bail!("change-ignore evaluator output exceeded its safe capture limit");
    }
    if !matches!(status.code(), Some(0 | 1)) {
        bail!(
            "evaluate change-ignore rules: {}",
            bounded_lossy_summary(&stderr.bytes, 4096)
        );
    }
    let mut ignored = HashSet::new();
    for path in stdout.bytes.split(|byte| *byte == 0) {
        if !path.is_empty() {
            ignored.insert(format!(
                "/{}",
                std::str::from_utf8(path)
                    .context("Git returned non-UTF-8 ignored path")?
                    .trim_end_matches('/')
            ));
        }
    }
    Ok(ignored)
}

pub(crate) fn required_result_file_paths(
    result: &RootFsManifest,
    changes: &[RootFsChange],
    workspace_guest_path: &str,
) -> BTreeSet<String> {
    let workspace = workspace_guest_path.trim_start_matches('/');
    let workspace_prefix = format!("{workspace}/");
    let mut paths: BTreeSet<_> = result
        .entries
        .iter()
        .filter(|entry| {
            entry.kind == "file"
                && (entry.path == workspace || entry.path.starts_with(&workspace_prefix))
        })
        .map(|entry| entry.path.clone())
        .collect();
    for entry in changes
        .iter()
        .filter_map(|change| change.after.as_ref())
        .filter(|entry| entry.kind == "file")
    {
        paths.insert(entry.path.clone());
    }
    paths
}

pub(crate) fn required_base_file_paths(changes: &[RootFsChange]) -> BTreeSet<String> {
    changes
        .iter()
        .filter_map(|change| change.before.as_ref())
        .filter(|entry| entry.kind == "file")
        .map(|entry| entry.path.clone())
        .collect()
}

fn git_status(command: &mut Command, context: &str) -> Result<()> {
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0");
    let output = crate::process::output_bounded(command, crate::process::MAX_IGNORE_RULE_BYTES)
        .with_context(|| context.to_string())?;
    reject_truncated_command_output(context, &output, crate::process::MAX_IGNORE_RULE_BYTES)?;
    if !output.status.success() {
        bail!(
            "{context}: {}",
            bounded_lossy_summary(&output.stderr.bytes, 4096)
        );
    }
    Ok(())
}

pub(crate) fn make_delta(
    base: &RootFsManifest,
    result: &RootFsManifest,
    change_ignore: &IgnoreIdentity,
    changes: Vec<RootFsChange>,
    ignored_changes: Vec<IgnoredChange>,
) -> Result<DeltaManifest> {
    let identity = DeltaIdentity {
        schema_version: DELTA_SCHEMA_VERSION,
        base_filesystem_digest: &base.digest,
        result_filesystem_digest: &result.digest,
        change_ignore,
        changes: &changes,
        ignored_changes: &ignored_changes,
    };
    Ok(DeltaManifest {
        schema_version: DELTA_SCHEMA_VERSION.to_string(),
        digest: sha256_bytes(&serde_json::to_vec(&identity)?),
        base_filesystem_digest: base.digest.clone(),
        result_filesystem_digest: result.digest.clone(),
        change_ignore: change_ignore.clone(),
        changes,
        ignored_changes,
    })
}

fn export_captures(
    store: &Store,
    run_id: &str,
    container: &str,
    captures: &[CaptureSpec],
) -> Result<Vec<Artifact>> {
    let mut artifacts = Vec::new();
    for capture in captures {
        let relative = format!("artifacts/capture-{}.tar", capture.name);
        let path = store.run_directory(run_id)?.join(&relative);
        let output_file = File::create(&path)?;
        let output = Command::new("docker")
            .args(["cp", &format!("{container}:{}", capture.guest_path), "-"])
            .stdout(Stdio::from(output_file))
            .stderr(Stdio::piped())
            .output()
            .context("export requested capture")?;
        if !output.status.success() {
            bail!(
                "export capture {}: {}",
                capture.guest_path,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        artifacts.push(artifact_for_file(&relative, &path)?);
    }
    Ok(artifacts)
}

fn uncovered_by_docker_diff(changes: &[RootFsChange], docker_diff: &[u8]) -> Vec<String> {
    let paths: Vec<_> = String::from_utf8_lossy(docker_diff)
        .lines()
        .filter_map(|line| {
            line.split_once(' ')
                .map(|(_, path)| path.trim().to_string())
        })
        .collect();
    changes
        .iter()
        .filter(|change| {
            !paths.iter().any(|path| {
                change.path == *path
                    || change
                        .path
                        .strip_prefix(path)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
        })
        .map(|change| change.path.clone())
        .collect()
}

fn sensitive_path_warnings(changes: &[RootFsChange]) -> Vec<String> {
    let markers = ["auth.json", "credentials", "id_rsa", ".ssh/", ".aws/"];
    changes
        .iter()
        .filter(|change| markers.iter().any(|marker| change.path.contains(marker)))
        .map(|change| {
            format!(
                "possible sensitive persistent path captured without displaying contents: {}",
                change.path
            )
        })
        .collect()
}

fn report_stage(observer: &mut dyn RunObserver, message: &str) -> Result<()> {
    observer.stage(message).context("write run progress output")
}

fn display_command(command: &[String]) -> String {
    command
        .iter()
        .map(|argument| {
            if argument
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-._/:=@".contains(character))
            {
                argument.clone()
            } else {
                format!("{argument:?}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

enum GuestOutputChunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    ReadError(String),
}

pub(crate) struct GuestCommandOutput {
    pub(crate) exit_code: i64,
    pub(crate) stdout: crate::process::BoundedCapture,
    pub(crate) stderr: crate::process::BoundedCapture,
    pub(crate) timed_out: bool,
    pub(crate) cancelled: bool,
}

pub(crate) fn execute_guest_command(
    container: &str,
    command: &[String],
    observer: &mut dyn RunObserver,
) -> Result<GuestCommandOutput> {
    let mut invocation = Command::new("docker");
    invocation.args(["exec", container]).args(command);
    execute_streaming_command(
        &mut invocation,
        observer,
        Duration::from_secs(crate::process::DEFAULT_GUEST_TIMEOUT_SECONDS),
        crate::process::MAX_RUN_OUTPUT_BYTES,
    )
    .context("execute command in retained run container")
}

fn execute_streaming_command(
    invocation: &mut Command,
    observer: &mut dyn RunObserver,
    timeout: Duration,
    output_limit: usize,
) -> Result<GuestCommandOutput> {
    invocation.stdout(Stdio::piped()).stderr(Stdio::piped());
    crate::process::isolate_process_group(invocation);
    let mut child = invocation.spawn().context("start streamed command")?;
    let child_stdout = child.stdout.take().context("capture guest stdout")?;
    let child_stderr = child.stderr.take().context("capture guest stderr")?;
    // At most sixteen 8 KiB chunks may wait for the terminal consumer. The
    // retained byte budget alone is not a memory bound if producers can queue
    // unbounded data before the consumer applies it.
    let (sender, receiver) = mpsc::sync_channel(16);
    let mut stdout = crate::process::BoundedCapture::default();
    let mut stderr = crate::process::BoundedCapture::default();
    let mut observer_error = None;
    let mut status = None;
    let mut timed_out = false;
    let mut cancelled = false;
    let started = Instant::now();

    std::thread::scope(|scope| {
        let stdout_sender = sender.clone();
        scope.spawn(move || read_guest_stream(child_stdout, true, stdout_sender));
        let stderr_sender = sender.clone();
        scope.spawn(move || read_guest_stream(child_stderr, false, stderr_sender));
        drop(sender);

        let mut disconnected = false;
        loop {
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => match chunk {
                    GuestOutputChunk::Stdout(bytes) => {
                        let retained = stdout.push(&bytes, output_limit);
                        if retained > 0 && observer_error.is_none() {
                            if let Err(error) = observer.command_stdout(&bytes[..retained]) {
                                observer_error = Some(error);
                            }
                        }
                    }
                    GuestOutputChunk::Stderr(bytes) => {
                        let retained = stderr.push(&bytes, output_limit);
                        if retained > 0 && observer_error.is_none() {
                            if let Err(error) = observer.command_stderr(&bytes[..retained]) {
                                observer_error = Some(error);
                            }
                        }
                    }
                    GuestOutputChunk::ReadError(error) => {
                        if observer_error.is_none() {
                            observer_error = Some(std::io::Error::other(error));
                        }
                    }
                },
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => disconnected = true,
            }

            if status.is_none() {
                cancelled = crate::cancel::requested();
                timed_out = !cancelled && started.elapsed() >= timeout;
                if cancelled || timed_out || observer_error.is_some() {
                    let _ = crate::process::terminate_process_group(&mut child);
                    status = child.wait().ok();
                } else {
                    status = child.try_wait().context("poll streamed command")?;
                    if status.is_some() {
                        let _ = crate::process::terminate_process_group(&mut child);
                    }
                }
            }

            if status.is_some() && disconnected {
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    })?;
    let status = status
        .or_else(|| child.wait().ok())
        .context("wait for streamed command")?;
    if let Some(error) = observer_error {
        return Err(error).context("stream guest command output");
    }
    Ok(GuestCommandOutput {
        exit_code: if cancelled {
            130
        } else if timed_out {
            124
        } else {
            status.code().map(i64::from).unwrap_or(-1)
        },
        stdout,
        stderr,
        timed_out,
        cancelled,
    })
}

fn read_guest_stream(mut stream: impl Read, stdout: bool, sender: SyncSender<GuestOutputChunk>) {
    let mut buffer = [0_u8; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return,
            Ok(size) => {
                let bytes = buffer[..size].to_vec();
                let chunk = if stdout {
                    GuestOutputChunk::Stdout(bytes)
                } else {
                    GuestOutputChunk::Stderr(bytes)
                };
                if sender.send(chunk).is_err() {
                    return;
                }
            }
            Err(error) => {
                let _ = sender.send(GuestOutputChunk::ReadError(error.to_string()));
                return;
            }
        }
    }
}

fn event(name: &str) -> LifecycleEvent {
    LifecycleEvent {
        event: name.to_string(),
        timestamp: Utc::now(),
    }
}

fn write_artifact(store: &Store, run_id: &str, relative: &str, bytes: &[u8]) -> Result<Artifact> {
    store.write_run_file(run_id, relative, bytes)?;
    Ok(Artifact {
        path: relative.to_string(),
        digest: sha256_bytes(bytes),
        size: bytes.len() as u64,
    })
}

pub(crate) fn artifact_for_file(relative: &str, path: &Path) -> Result<Artifact> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let size = std::io::copy(&mut file, &mut hasher)?;
    Ok(Artifact {
        path: relative.to_string(),
        digest: format!("sha256:{}", hex_digest(&hasher.finalize())),
        size,
    })
}

pub(crate) fn pretty_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{}", hex_digest(&Sha256::digest(bytes)))
}

pub(crate) fn docker_success(command: &mut Command, context: &str) -> Result<String> {
    let output = crate::process::output_bounded(command, crate::process::MAX_IGNORE_RULE_BYTES)
        .with_context(|| context.to_string())?;
    reject_truncated_command_output(context, &output, crate::process::MAX_IGNORE_RULE_BYTES)?;
    if !output.status.success() {
        bail!(
            "{context}: {}",
            bounded_lossy_summary(&output.stderr.bytes, 4096)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout.bytes)
        .trim()
        .to_string())
}

pub(crate) struct QuiescedContainer {
    container: String,
    restart: bool,
}

impl QuiescedContainer {
    pub(crate) fn restart(&mut self) -> Result<()> {
        if self.restart {
            docker_status(
                Command::new("docker").args(["start", &self.container]),
                "restart retained container after immutable filesystem capture",
            )?;
            self.restart = false;
        }
        Ok(())
    }
}

impl Drop for QuiescedContainer {
    fn drop(&mut self) {
        if self.restart {
            let _ = Command::new("docker")
                .args(["start", &self.container])
                .output();
        }
    }
}

pub(crate) fn quiesce_container(container: &str) -> Result<QuiescedContainer> {
    let inspect = docker_output_bytes(
        Command::new("docker").args(["inspect", container]),
        "inspect retained container before immutable capture",
    )?;
    let (_, state) = container_status(&inspect)?;
    let restart = state == "running";
    if restart {
        docker_status(
            Command::new("docker").args(["stop", "--time", "1", container]),
            "quiesce retained container for immutable filesystem capture",
        )?;
    } else if state != "exited" && state != "created" {
        bail!("cannot capture container {container:?} from state {state:?}");
    }
    Ok(QuiescedContainer {
        container: container.to_owned(),
        restart,
    })
}

pub(crate) fn docker_status(command: &mut Command, context: &str) -> Result<()> {
    docker_success(command, context).map(|_| ())
}

pub(crate) fn docker_output_bytes(command: &mut Command, context: &str) -> Result<Vec<u8>> {
    let output =
        crate::process::output_bounded(command, crate::process::MAX_COMMAND_METADATA_BYTES)
            .with_context(|| context.to_string())?;
    reject_truncated_command_output(context, &output, crate::process::MAX_COMMAND_METADATA_BYTES)?;
    if !output.status.success() {
        bail!(
            "{context}: {}",
            bounded_lossy_summary(&output.stderr.bytes, 4096)
        );
    }
    Ok(output.stdout.bytes)
}

fn reject_truncated_command_output(
    context: &str,
    output: &crate::process::BoundedCommandOutput,
    limit: usize,
) -> Result<()> {
    if output.stdout.truncated || output.stderr.truncated {
        bail!(
            "{context}: command output exceeded the {} byte per-stream metadata limit (stdout {} bytes, stderr {} bytes)",
            limit,
            output.stdout.total_bytes,
            output.stderr.total_bytes
        );
    }
    Ok(())
}

fn bounded_lossy_summary(bytes: &[u8], limit: usize) -> String {
    let retained = &bytes[..bytes.len().min(limit)];
    let mut summary = String::from_utf8_lossy(retained).trim().to_owned();
    if bytes.len() > limit {
        summary.push_str("… [truncated]");
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rootfs::RootFsEntry;
    use std::time::Duration;

    fn options(workspace: PathBuf, captures: Vec<CaptureSpec>) -> RunOptions {
        RunOptions {
            workspace: WorkspaceSource::Directory(workspace),
            workspace_capture_mode: snapshot::CaptureMode::All,
            image: "unused:latest".to_owned(),
            command: vec!["/bin/true".to_owned()],
            workspace_guest_path: "/workspace".to_owned(),
            network: "none".to_owned(),
            memory: None,
            cpus: None,
            pi_auth: None,
            secret_files: Vec::new(),
            change_ignore: None,
            captures,
            accepted_input: None,
        }
    }

    #[test]
    fn duplicate_capture_names_are_rejected() {
        let error = validate_options(&options(
            PathBuf::from("."),
            vec![
                CaptureSpec {
                    guest_path: "/one".to_owned(),
                    name: "duplicate".to_owned(),
                },
                CaptureSpec {
                    guest_path: "/two".to_owned(),
                    name: "duplicate".to_owned(),
                },
            ],
        ))
        .unwrap_err();

        assert_eq!(error.to_string(), "duplicate capture name \"duplicate\"");
    }

    #[test]
    fn runtime_secret_names_are_safe_unique_and_do_not_record_host_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        fs::write(&first, b"one").unwrap();
        fs::write(&second, b"two").unwrap();

        let valid = vec![SecretFileSpec {
            name: "aws-credentials".to_owned(),
            source: first.clone(),
        }];
        validate_secret_files(&valid, None).unwrap();
        let mut configured = options(PathBuf::from("."), Vec::new());
        configured.secret_files = valid;
        assert_eq!(secret_injection_names(&configured), ["aws-credentials"]);
        assert!(
            !secret_injection_names(&configured)[0].contains(temporary.path().to_str().unwrap())
        );

        let duplicate = vec![
            SecretFileSpec {
                name: "credential".to_owned(),
                source: first,
            },
            SecretFileSpec {
                name: "credential".to_owned(),
                source: second.clone(),
            },
        ];
        assert_eq!(
            validate_secret_files(&duplicate, None)
                .unwrap_err()
                .to_string(),
            "duplicate runtime secret name \"credential\""
        );

        let unsafe_name = vec![SecretFileSpec {
            name: "../credential".to_owned(),
            source: second,
        }];
        assert!(
            validate_secret_files(&unsafe_name, None)
                .unwrap_err()
                .to_string()
                .contains("invalid runtime secret name")
        );
    }

    #[test]
    fn failed_run_removes_incomplete_run_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(Some(&temporary.path().join("state"))).unwrap();
        let error = execute(
            &options(temporary.path().join("missing-workspace"), Vec::new()),
            &store,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("resolve workspace"));
        assert!(store.list_run_ids().unwrap().is_empty());
    }

    #[test]
    fn directory_only_git_ignore_patterns_match_directory_changes() {
        let directory = RootFsChange {
            path: "/secrets".to_owned(),
            change: ChangeKind::Added,
            before: None,
            after: Some(RootFsEntry {
                path: "secrets".to_owned(),
                kind: "directory".to_owned(),
                mode: 0o700,
                size: 0,
                digest: String::new(),
                link_target: String::new(),
            }),
        };
        let ordinary = RootFsChange {
            path: "/visible".to_owned(),
            change: ChangeKind::Added,
            before: None,
            after: Some(RootFsEntry {
                path: "visible".to_owned(),
                kind: "directory".to_owned(),
                mode: 0o755,
                size: 0,
                digest: String::new(),
                link_target: String::new(),
            }),
        };
        let ignored = evaluate_change_ignore_bytes(b"/secrets/\n", &[directory, ordinary]).unwrap();
        assert_eq!(ignored, ["/secrets".to_owned()].into_iter().collect());
    }

    #[cfg(unix)]
    #[test]
    fn streamed_execution_bounds_both_output_queues_and_retained_bytes() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "dd if=/dev/zero bs=1024 count=256 2>/dev/null; (dd if=/dev/zero bs=1024 count=256 2>/dev/null) >&2",
        ]);

        let output = execute_streaming_command(
            &mut command,
            &mut SilentRunObserver,
            Duration::from_secs(5),
            4096,
        )
        .unwrap();

        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout.bytes.len(), 4096);
        assert_eq!(output.stderr.bytes.len(), 4096);
        assert_eq!(output.stdout.total_bytes, 256 * 1024);
        assert_eq!(output.stderr.total_bytes, 256 * 1024);
        assert!(output.stdout.truncated);
        assert!(output.stderr.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn streamed_execution_timeout_kills_the_complete_process_group() {
        let temporary = tempfile::tempdir().unwrap();
        let delayed_write = temporary.path().join("descendant-survived");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "(sleep 1; printf leaked > \"$1\") & wait",
                "agentlab-timeout-test",
            ])
            .arg(&delayed_write);

        let started = Instant::now();
        let output = execute_streaming_command(
            &mut command,
            &mut SilentRunObserver,
            Duration::from_millis(100),
            4096,
        )
        .unwrap();

        assert!(output.timed_out);
        assert_eq!(output.exit_code, 124);
        assert!(started.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(1100));
        assert!(!delayed_write.exists());
    }
}
