use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::acceptance;
use crate::lock::AdvisoryLock;
use crate::rootfs::{self, RootFsManifest};
use crate::run::{
    self, Artifact, CaptureSpec, IgnoreIdentity, ResourceLimits, RunResult, RunSpec, SecretFileSpec,
};
use crate::store::Store;

pub const FORK_SCHEMA_VERSION: &str = "agentlab.fork/v1";
pub const CONTINUATION_SCHEMA_VERSION: &str = "agentlab.continuation/v1";
pub const LIFECYCLE_EVENT_SCHEMA_VERSION: &str = "agentlab.lifecycle-event/v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRun {
    pub run_id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    pub anchor_digest: String,
    pub container_name: String,
    pub container_id: String,
    pub container_state: String,
    pub lifecycle_capable: bool,
    pub continuation_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForkRecord {
    pub schema_version: String,
    pub digest: String,
    pub run_id: String,
    pub parent_run_id: String,
    pub parent_anchor_digest: String,
    pub created_at: DateTime<Utc>,
    pub container_name: String,
    pub container_id: String,
    pub image_tag: String,
    pub image_id: String,
    pub workspace_guest_path: String,
    pub network_policy: String,
    pub resource_limits: ResourceLimits,
    pub captures: Vec<CaptureSpec>,
    pub change_ignore: IgnoreIdentity,
    pub base_filesystem_digest: String,
    pub base_rootfs_export: Artifact,
    pub container_inspect: Artifact,
    pub filesystem_state_copied: bool,
    pub process_memory_copied: bool,
    pub integrity: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContinuationResult {
    pub schema_version: String,
    pub digest: String,
    pub continuation_id: String,
    pub run_id: String,
    pub anchor_digest: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_injections: Vec<String>,
    pub exit_code: i64,
    pub stdout: Artifact,
    pub stderr: Artifact,
    pub captures: Vec<Artifact>,
    pub base_filesystem_digest: String,
    pub result_filesystem_digest: String,
    pub raw_delta_digest: String,
    pub portable_delta_digest: String,
    pub container_name: String,
    pub container_id: String,
    pub container_state: String,
    pub container_restarted: bool,
    pub filesystem_state_reused: bool,
    pub process_memory_restored: bool,
    pub result_rootfs_export: Artifact,
    pub container_inspect: Artifact,
    pub docker_diff: Artifact,
    pub warnings: Vec<String>,
    pub integrity: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LifecycleEventRecord {
    pub schema_version: String,
    pub digest: String,
    pub event_id: String,
    pub run_id: String,
    pub event: String,
    pub timestamp: DateTime<Utc>,
    pub container_id: String,
    pub container_state: String,
    pub filesystem_state_preserved: bool,
    pub process_memory_restored: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResumeSummary {
    pub run_id: String,
    pub container_name: String,
    pub container_id: String,
    pub container_state: String,
    pub container_restarted: bool,
    pub filesystem_state_reused: bool,
    pub process_memory_restored: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation: Option<ContinuationResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemovalSummary {
    pub run_id: String,
    pub container_name: String,
    pub container_id: String,
    pub image_tag: String,
    pub run_directory_removed: bool,
}

#[derive(Serialize)]
struct ForkIdentity<'a> {
    schema_version: &'a str,
    run_id: &'a str,
    parent_run_id: &'a str,
    parent_anchor_digest: &'a str,
    created_at: DateTime<Utc>,
    container_name: &'a str,
    container_id: &'a str,
    image_tag: &'a str,
    image_id: &'a str,
    workspace_guest_path: &'a str,
    network_policy: &'a str,
    resource_limits: &'a ResourceLimits,
    captures: &'a [CaptureSpec],
    change_ignore: &'a IgnoreIdentity,
    base_filesystem_digest: &'a str,
    base_rootfs_export: &'a Artifact,
    container_inspect: &'a Artifact,
    filesystem_state_copied: bool,
    process_memory_copied: bool,
    integrity: &'a BTreeMap<String, String>,
}

#[derive(Serialize)]
struct ContinuationIdentity<'a> {
    schema_version: &'a str,
    continuation_id: &'a str,
    run_id: &'a str,
    anchor_digest: &'a str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    command: &'a [String],
    #[serde(skip_serializing_if = "slice_is_empty")]
    secret_injections: &'a [String],
    exit_code: i64,
    stdout: &'a Artifact,
    stderr: &'a Artifact,
    captures: &'a [Artifact],
    base_filesystem_digest: &'a str,
    result_filesystem_digest: &'a str,
    raw_delta_digest: &'a str,
    portable_delta_digest: &'a str,
    container_name: &'a str,
    container_id: &'a str,
    container_state: &'a str,
    container_restarted: bool,
    filesystem_state_reused: bool,
    process_memory_restored: bool,
    result_rootfs_export: &'a Artifact,
    container_inspect: &'a Artifact,
    docker_diff: &'a Artifact,
    warnings: &'a [String],
    integrity: &'a BTreeMap<String, String>,
}

#[derive(Serialize)]
struct EventIdentity<'a> {
    schema_version: &'a str,
    event_id: &'a str,
    run_id: &'a str,
    event: &'a str,
    timestamp: DateTime<Utc>,
    container_id: &'a str,
    container_state: &'a str,
    filesystem_state_preserved: bool,
    process_memory_restored: bool,
}

struct Subject {
    run_id: String,
    kind: String,
    parent_run_id: Option<String>,
    anchor_digest: String,
    container_name: String,
    container_id: String,
    image_tag: String,
    workspace_guest_path: String,
    network_policy: String,
    resource_limits: ResourceLimits,
    captures: Vec<CaptureSpec>,
    change_ignore: IgnoreIdentity,
    base_manifest: RootFsManifest,
}

fn slice_is_empty<T>(value: &&[T]) -> bool {
    value.is_empty()
}

struct ForkCleanup {
    container_name: String,
    image_tag: String,
    run_id: String,
    store: Store,
    armed: bool,
}

struct FailedContinuationCleanup {
    directory: PathBuf,
    armed: bool,
}

impl Drop for FailedContinuationCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

impl Drop for ForkCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = Command::new("docker")
            .args(["rm", "--force", &self.container_name])
            .output();
        let _ = Command::new("docker")
            .args(["image", "rm", &self.image_tag])
            .output();
        let _ = self.store.remove_run_directory(&self.run_id);
    }
}

pub fn list(store: &Store) -> Result<Vec<ManagedRun>> {
    let mut runs = Vec::new();
    for run_id in store.list_run_ids()? {
        if let Ok(subject) = load_subject(store, &run_id) {
            runs.push(managed_run(store, &subject));
        }
    }
    Ok(runs)
}

pub fn inspect(store: &Store, run_id: &str, verify: bool) -> Result<ManagedRun> {
    let subject = load_subject(store, run_id)?;
    if verify {
        verify_all(store, run_id)?;
    }
    Ok(managed_run(store, &subject))
}

pub fn stop(store: &Store, run_id: &str) -> Result<ManagedRun> {
    let _lock = acquire_run_lock(store, run_id)?;
    let subject = load_subject(store, run_id)?;
    run::recover_interrupted_secret_lease(store, run_id, &subject.container_name)?;
    let inspect = assert_owned_container(&subject)?;
    let (_, state) = run::container_status(&inspect)?;
    if state == "running" {
        run::docker_status(
            Command::new("docker").args(["stop", &subject.container_name]),
            "stop retained run container",
        )?;
    }
    let inspect = assert_owned_container(&subject)?;
    let (_, state) = run::container_status(&inspect)?;
    write_event(store, &subject, "container_stopped", &state)?;
    Ok(managed_run(store, &subject))
}

pub fn resume(store: &Store, run_id: &str, command: &[String]) -> Result<ResumeSummary> {
    resume_with_secrets(store, run_id, command, None, &[])
}

pub fn resume_with_pi_auth(
    store: &Store,
    run_id: &str,
    command: &[String],
    pi_auth: Option<&Path>,
) -> Result<ResumeSummary> {
    resume_with_secrets(store, run_id, command, pi_auth, &[])
}

pub fn resume_with_secrets(
    store: &Store,
    run_id: &str,
    command: &[String],
    pi_auth: Option<&Path>,
    secret_files: &[SecretFileSpec],
) -> Result<ResumeSummary> {
    let _lock = acquire_run_lock(store, run_id)?;
    let subject = load_subject(store, run_id)?;
    let recovered_credential_lease =
        run::recover_interrupted_secret_lease(store, run_id, &subject.container_name)?;
    let inspect = assert_owned_container(&subject)?;
    let (_, state) = run::container_status(&inspect)?;
    let restarted = recovered_credential_lease || state != "running";
    if state != "running" {
        run::docker_status(
            Command::new("docker").args(["start", &subject.container_name]),
            "restart retained run container",
        )?;
        write_event(store, &subject, "container_restarted", "running")?;
    }
    let continuation = if command.is_empty() {
        None
    } else {
        Some(execute_continuation(
            store,
            &subject,
            command,
            restarted,
            pi_auth,
            secret_files,
        )?)
    };
    let inspect = assert_owned_container(&subject)?;
    let (_, state) = run::container_status(&inspect)?;
    Ok(ResumeSummary {
        run_id: run_id.to_owned(),
        container_name: subject.container_name,
        container_id: subject.container_id,
        container_state: state,
        container_restarted: restarted,
        filesystem_state_reused: true,
        process_memory_restored: false,
        continuation,
    })
}

pub fn fork(store: &Store, parent_run_id: &str) -> Result<ForkRecord> {
    let _lock = acquire_run_lock(store, parent_run_id)?;
    let parent = load_subject(store, parent_run_id)?;
    run::recover_interrupted_secret_lease(store, parent_run_id, &parent.container_name)?;
    assert_owned_container(&parent)?;
    let run_id = Uuid::new_v4().to_string();
    let directory = store.create_run_directory(&run_id)?;
    let compact = run_id.replace('-', "");
    let short_id = &compact[..12];
    let container_name = format!("agentlab-fork-{short_id}");
    let image_tag = format!("agentlab-fork:{short_id}");
    let mut cleanup = ForkCleanup {
        container_name: container_name.clone(),
        image_tag: image_tag.clone(),
        run_id: run_id.clone(),
        store: store.clone(),
        armed: true,
    };

    let mut parent_quiesced = run::quiesce_container(&parent.container_name)?;
    let image_id = run::docker_success(
        Command::new("docker").args(["commit", &parent.container_name, &image_tag]),
        "commit one quiesced filesystem state for fork",
    )?;
    parent_quiesced.restart()?;

    let mut create = Command::new("docker");
    create
        .args(["create", "--name", &container_name])
        .args(["--label", &format!("agentlab.run_id={run_id}")])
        .args(["--label", "agentlab.lifecycle=v1"])
        .args(["--label", &format!("agentlab.image_tag={image_tag}")])
        .args([
            "--label",
            &format!("agentlab.parent_run_id={parent_run_id}"),
        ])
        .args(["--workdir", &parent.workspace_guest_path])
        .args(["--network", &parent.network_policy]);
    if let Some(memory) = &parent.resource_limits.memory {
        create.args(["--memory", memory]);
    }
    if let Some(cpus) = &parent.resource_limits.cpus {
        create.args(["--cpus", cpus]);
    }
    create.args(["--mount", run::PI_AUTH_TMPFS_MOUNT]);
    create.arg(&image_id).args([
        "/bin/sh",
        "-c",
        "trap 'exit 0' TERM INT; while :; do sleep 3600 & wait $!; done",
    ]);
    let container_id = run::docker_success(&mut create, "create filesystem fork container")?;

    // Export the stopped child created from the committed image. The bytes
    // described by the fork manifest are therefore the exact bytes the child
    // will start from, not an earlier export of a live parent.
    let export_path = directory.join("artifacts/base-rootfs.tar");
    run::docker_status(
        Command::new("docker").args([
            "export",
            "--output",
            export_path.to_str().context("fork path is not UTF-8")?,
            &container_name,
        ]),
        "export immutable filesystem fork base",
    )?;
    let base_manifest = rootfs::scan_export(&export_path)?;
    let required_blob_paths =
        run::required_result_file_paths(&base_manifest, &[], &parent.workspace_guest_path);
    rootfs::store_required_file_blobs(&export_path, &base_manifest, &required_blob_paths, store)?;
    let base_manifest_bytes = run::pretty_json(&base_manifest)?;
    store.write_run_file(&run_id, "base-rootfs.json", &base_manifest_bytes)?;
    let base_export = run::artifact_for_file("artifacts/base-rootfs.tar", &export_path)?;

    run::docker_status(
        Command::new("docker").args(["start", &container_name]),
        "start filesystem fork container",
    )?;
    let inspect_bytes = assert_owned_container_fields(&run_id, &container_name, &container_id)?;
    let inspect_artifact = write_bytes_artifact(
        store,
        &run_id,
        "evidence/container-inspect.json",
        &inspect_bytes,
    )?;
    if let Ok(rules) = store.read_run_file(parent_run_id, "change-ignore.rules") {
        store.write_run_file(&run_id, "change-ignore.rules", &rules)?;
    }

    let created_at = Utc::now();
    let mut integrity = BTreeMap::new();
    integrity.insert(
        "base-rootfs.json".to_owned(),
        run::sha256_bytes(&base_manifest_bytes),
    );
    integrity.insert(base_export.path.clone(), base_export.digest.clone());
    integrity.insert(
        inspect_artifact.path.clone(),
        inspect_artifact.digest.clone(),
    );
    if let Ok(rules) = store.read_run_file(&run_id, "change-ignore.rules") {
        integrity.insert("change-ignore.rules".to_owned(), run::sha256_bytes(&rules));
    }
    let identity = ForkIdentity {
        schema_version: FORK_SCHEMA_VERSION,
        run_id: &run_id,
        parent_run_id,
        parent_anchor_digest: &parent.anchor_digest,
        created_at,
        container_name: &container_name,
        container_id: &container_id,
        image_tag: &image_tag,
        image_id: &image_id,
        workspace_guest_path: &parent.workspace_guest_path,
        network_policy: &parent.network_policy,
        resource_limits: &parent.resource_limits,
        captures: &parent.captures,
        change_ignore: &parent.change_ignore,
        base_filesystem_digest: &base_manifest.digest,
        base_rootfs_export: &base_export,
        container_inspect: &inspect_artifact,
        filesystem_state_copied: true,
        process_memory_copied: false,
        integrity: &integrity,
    };
    let record = ForkRecord {
        schema_version: FORK_SCHEMA_VERSION.to_owned(),
        digest: run::sha256_bytes(&serde_json::to_vec(&identity)?),
        run_id: run_id.clone(),
        parent_run_id: parent_run_id.to_owned(),
        parent_anchor_digest: parent.anchor_digest,
        created_at,
        container_name,
        container_id,
        image_tag,
        image_id,
        workspace_guest_path: parent.workspace_guest_path,
        network_policy: parent.network_policy,
        resource_limits: parent.resource_limits,
        captures: parent.captures,
        change_ignore: parent.change_ignore,
        base_filesystem_digest: base_manifest.digest,
        base_rootfs_export: base_export,
        container_inspect: inspect_artifact,
        filesystem_state_copied: true,
        process_memory_copied: false,
        integrity,
    };
    store.write_run_file(&run_id, "fork.json", &run::pretty_json(&record)?)?;
    cleanup.armed = false;
    Ok(record)
}

pub fn remove(store: &Store, run_id: &str) -> Result<RemovalSummary> {
    let _lock = acquire_run_lock(store, run_id)?;
    let acceptances = acceptance::referencing_run(store, run_id)?;
    if !acceptances.is_empty() {
        bail!(
            "run {run_id:?} is preserved by accepted lineage {}; accepted evidence cannot be removed",
            acceptances.join(", ")
        );
    }
    let subject = load_subject(store, run_id)?;
    assert_owned_container(&subject)?;
    run::docker_status(
        Command::new("docker").args(["rm", "--force", &subject.container_name]),
        "remove selected retained container",
    )?;
    remove_image_tag(&subject.image_tag)?;
    store.remove_run_directory(run_id)?;
    Ok(RemovalSummary {
        run_id: run_id.to_owned(),
        container_name: subject.container_name,
        container_id: subject.container_id,
        image_tag: subject.image_tag,
        run_directory_removed: true,
    })
}

fn acquire_run_lock(store: &Store, run_id: &str) -> Result<AdvisoryLock> {
    let path = store.run_path(run_id, "operation.lock")?;
    AdvisoryLock::acquire(&path, &format!("AgentLab lifecycle for run {run_id}"))
}

pub fn load_fork(store: &Store, run_id: &str) -> Result<ForkRecord> {
    serde_json::from_slice(&store.read_run_file(run_id, "fork.json")?).context("decode fork record")
}

pub fn verify_fork(store: &Store, record: &ForkRecord) -> Result<()> {
    if record.schema_version != FORK_SCHEMA_VERSION {
        bail!("unsupported fork schema {:?}", record.schema_version);
    }
    for (relative, expected) in &record.integrity {
        let actual = run::sha256_bytes(&store.read_run_file(&record.run_id, relative)?);
        if &actual != expected {
            bail!("fork artifact integrity mismatch for {relative:?}");
        }
    }
    let identity = ForkIdentity {
        schema_version: FORK_SCHEMA_VERSION,
        run_id: &record.run_id,
        parent_run_id: &record.parent_run_id,
        parent_anchor_digest: &record.parent_anchor_digest,
        created_at: record.created_at,
        container_name: &record.container_name,
        container_id: &record.container_id,
        image_tag: &record.image_tag,
        image_id: &record.image_id,
        workspace_guest_path: &record.workspace_guest_path,
        network_policy: &record.network_policy,
        resource_limits: &record.resource_limits,
        captures: &record.captures,
        change_ignore: &record.change_ignore,
        base_filesystem_digest: &record.base_filesystem_digest,
        base_rootfs_export: &record.base_rootfs_export,
        container_inspect: &record.container_inspect,
        filesystem_state_copied: record.filesystem_state_copied,
        process_memory_copied: record.process_memory_copied,
        integrity: &record.integrity,
    };
    let actual = run::sha256_bytes(&serde_json::to_vec(&identity)?);
    if actual != record.digest {
        bail!("fork record integrity mismatch");
    }
    Ok(())
}

pub fn verify_all(store: &Store, run_id: &str) -> Result<()> {
    if store.run_file_exists(run_id, "result.json")? {
        run::verify_result(store, &run::load_result(store, run_id)?)?;
    } else if store.run_file_exists(run_id, "fork.json")? {
        verify_fork(store, &load_fork(store, run_id)?)?;
    }
    let continuations = store.run_path(run_id, "continuations")?;
    if continuations.is_dir() {
        for entry in fs::read_dir(continuations)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("continuation ID is not valid UTF-8"))?;
            let relative = format!("continuations/{id}/continuation.json");
            let continuation: ContinuationResult =
                serde_json::from_slice(&store.read_run_file(run_id, &relative)?)?;
            verify_continuation(store, &continuation)?;
        }
    }
    let events = store.run_path(run_id, "lifecycle")?;
    if events.is_dir() {
        for entry in fs::read_dir(events)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let record: LifecycleEventRecord = serde_json::from_slice(&fs::read(entry.path())?)?;
            verify_event(&record)?;
        }
    }
    Ok(())
}

pub fn verify_continuation(store: &Store, result: &ContinuationResult) -> Result<()> {
    if result.schema_version != CONTINUATION_SCHEMA_VERSION {
        bail!(
            "unsupported continuation schema {:?}",
            result.schema_version
        );
    }
    for (relative, expected) in &result.integrity {
        let actual = run::sha256_bytes(&store.read_run_file(&result.run_id, relative)?);
        if &actual != expected {
            bail!("continuation artifact integrity mismatch for {relative:?}");
        }
    }
    let identity = ContinuationIdentity {
        schema_version: CONTINUATION_SCHEMA_VERSION,
        continuation_id: &result.continuation_id,
        run_id: &result.run_id,
        anchor_digest: &result.anchor_digest,
        started_at: result.started_at,
        completed_at: result.completed_at,
        command: &result.command,
        secret_injections: &result.secret_injections,
        exit_code: result.exit_code,
        stdout: &result.stdout,
        stderr: &result.stderr,
        captures: &result.captures,
        base_filesystem_digest: &result.base_filesystem_digest,
        result_filesystem_digest: &result.result_filesystem_digest,
        raw_delta_digest: &result.raw_delta_digest,
        portable_delta_digest: &result.portable_delta_digest,
        container_name: &result.container_name,
        container_id: &result.container_id,
        container_state: &result.container_state,
        container_restarted: result.container_restarted,
        filesystem_state_reused: result.filesystem_state_reused,
        process_memory_restored: result.process_memory_restored,
        result_rootfs_export: &result.result_rootfs_export,
        container_inspect: &result.container_inspect,
        docker_diff: &result.docker_diff,
        warnings: &result.warnings,
        integrity: &result.integrity,
    };
    if run::sha256_bytes(&serde_json::to_vec(&identity)?) != result.digest {
        bail!("continuation record integrity mismatch");
    }
    Ok(())
}

fn verify_event(record: &LifecycleEventRecord) -> Result<()> {
    if record.schema_version != LIFECYCLE_EVENT_SCHEMA_VERSION {
        bail!(
            "unsupported lifecycle event schema {:?}",
            record.schema_version
        );
    }
    let identity = EventIdentity {
        schema_version: LIFECYCLE_EVENT_SCHEMA_VERSION,
        event_id: &record.event_id,
        run_id: &record.run_id,
        event: &record.event,
        timestamp: record.timestamp,
        container_id: &record.container_id,
        container_state: &record.container_state,
        filesystem_state_preserved: record.filesystem_state_preserved,
        process_memory_restored: record.process_memory_restored,
    };
    if run::sha256_bytes(&serde_json::to_vec(&identity)?) != record.digest {
        bail!("lifecycle event integrity mismatch");
    }
    Ok(())
}

fn execute_continuation(
    store: &Store,
    subject: &Subject,
    command: &[String],
    restarted: bool,
    pi_auth: Option<&Path>,
    secret_files: &[SecretFileSpec],
) -> Result<ContinuationResult> {
    let started_at = Utc::now();
    let continuation_id = Uuid::new_v4().to_string();
    let prefix = format!("continuations/{continuation_id}");
    let directory = store.run_path(&subject.run_id, &prefix)?;
    let mut failed_continuation_cleanup = FailedContinuationCleanup {
        directory: directory.clone(),
        armed: true,
    };
    fs::create_dir_all(directory.join("artifacts"))?;
    fs::create_dir_all(directory.join("evidence"))?;

    run::validate_secret_files(secret_files, pi_auth)?;
    if pi_auth.is_some() || !secret_files.is_empty() {
        run::ensure_pi_auth_tmpfs(&assert_owned_container(subject)?)?;
    }
    let mut secret_names: Vec<_> = secret_files
        .iter()
        .map(|secret| secret.name.clone())
        .collect();
    if pi_auth.is_some() {
        secret_names.push(run::PI_AUTH_SECRET_NAME.to_owned());
    }
    let mut runtime_secret_lease = if secret_names.is_empty() {
        None
    } else {
        Some(run::RuntimeSecretLease::begin(
            store,
            &subject.run_id,
            &subject.container_name,
            secret_names,
            Some(prefix.clone()),
        )?)
    };
    let mut secret_file_guard = if secret_files.is_empty() {
        None
    } else {
        Some(run::inject_secret_files(
            &subject.container_name,
            secret_files,
        )?)
    };
    let mut pi_auth_guard = if let Some(source) = pi_auth {
        Some(run::inject_pi_auth(&subject.container_name, source)?)
    } else {
        None
    };
    let output = run::execute_guest_command(
        &subject.container_name,
        command,
        &mut run::SilentRunObserver,
    );
    if output
        .as_ref()
        .is_ok_and(|output| output.cancelled || output.timed_out)
    {
        run::docker_status(
            Command::new("docker").args(["stop", "--time", "1", &subject.container_name]),
            "stop retained container after interrupted continuation",
        )?;
        run::docker_status(
            Command::new("docker").args(["start", &subject.container_name]),
            "restart retained container with empty runtime memory",
        )?;
    }
    if let Some(guard) = &mut pi_auth_guard {
        guard.cleanup()?;
    }
    if let Some(guard) = &mut secret_file_guard {
        guard.cleanup()?;
    }
    if let Some(lease) = &mut runtime_secret_lease {
        lease.complete()?;
    }
    let output = output?;
    if output.cancelled {
        if runtime_secret_lease.is_some() {
            bail!("continuation interrupted; runtime credentials were revoked");
        }
        bail!("continuation interrupted");
    }
    let exit_code = output.exit_code;
    let mut secret_injections: Vec<_> = secret_files
        .iter()
        .map(|secret| secret.name.clone())
        .collect();
    if pi_auth.is_some() {
        secret_injections.push(run::PI_AUTH_SECRET_NAME.to_owned());
    }
    secret_injections.sort();
    let stdout = write_bytes_artifact(
        store,
        &subject.run_id,
        &format!("{prefix}/artifacts/stdout.bin"),
        &output.stdout.bytes,
    )?;
    let stderr = write_bytes_artifact(
        store,
        &subject.run_id,
        &format!("{prefix}/artifacts/stderr.bin"),
        &output.stderr.bytes,
    )?;

    let mut quiesced = run::quiesce_container(&subject.container_name)?;
    let result_export_path = directory.join("artifacts/result-rootfs.tar");
    run::docker_status(
        Command::new("docker").args([
            "export",
            "--output",
            result_export_path
                .to_str()
                .context("continuation path is not UTF-8")?,
            &subject.container_name,
        ]),
        "export continued root filesystem",
    )?;
    let diff_bytes = run::docker_output_bytes(
        Command::new("docker").args(["diff", &subject.container_name]),
        "collect continued Docker diff",
    )?;
    let captures = export_captures(store, subject, &prefix)?;
    quiesced.restart()?;
    let result_manifest = rootfs::scan_export(&result_export_path)?;
    let result_manifest_bytes = run::pretty_json(&result_manifest)?;
    store.write_run_file(
        &subject.run_id,
        &format!("{prefix}/result-rootfs.json"),
        &result_manifest_bytes,
    )?;
    let all_changes = rootfs::compare(&subject.base_manifest, &result_manifest);
    let required_base_blob_paths = run::required_base_file_paths(&all_changes);
    if !required_base_blob_paths.is_empty() {
        let base_export_path = store.run_path(&subject.run_id, "artifacts/base-rootfs.tar")?;
        rootfs::store_required_file_blobs(
            &base_export_path,
            &subject.base_manifest,
            &required_base_blob_paths,
            store,
        )?;
    }
    let required_blob_paths = run::required_result_file_paths(
        &result_manifest,
        &all_changes,
        &subject.workspace_guest_path,
    );
    rootfs::store_required_file_blobs(
        &result_export_path,
        &result_manifest,
        &required_blob_paths,
        store,
    )?;
    let ignored = match store.read_run_file(&subject.run_id, "change-ignore.rules") {
        Ok(rules) => run::evaluate_change_ignore_bytes(&rules, &all_changes)?,
        Err(_) if subject.change_ignore.source.is_none() => Default::default(),
        Err(error) => return Err(error).context("load preserved change-ignore rules"),
    };
    let mut portable_changes = Vec::new();
    let mut ignored_changes = Vec::new();
    for change in &all_changes {
        if ignored.contains(&change.path) {
            ignored_changes.push(run::IgnoredChange {
                path: change.path.clone(),
                change: change.change.clone(),
            });
        } else {
            portable_changes.push(change.clone());
        }
    }
    let raw_delta = run::make_delta(
        &subject.base_manifest,
        &result_manifest,
        &subject.change_ignore,
        all_changes,
        Vec::new(),
    )?;
    let portable_delta = run::make_delta(
        &subject.base_manifest,
        &result_manifest,
        &subject.change_ignore,
        portable_changes,
        ignored_changes,
    )?;
    let raw_delta_bytes = run::pretty_json(&raw_delta)?;
    let portable_delta_bytes = run::pretty_json(&portable_delta)?;
    store.write_run_file(
        &subject.run_id,
        &format!("{prefix}/delta.raw.json"),
        &raw_delta_bytes,
    )?;
    store.write_run_file(
        &subject.run_id,
        &format!("{prefix}/delta.json"),
        &portable_delta_bytes,
    )?;
    let inspect_bytes = assert_owned_container(subject)?;
    let (_, state) = run::container_status(&inspect_bytes)?;
    let inspect_artifact = write_bytes_artifact(
        store,
        &subject.run_id,
        &format!("{prefix}/evidence/container-inspect.json"),
        &inspect_bytes,
    )?;
    let diff_artifact = write_bytes_artifact(
        store,
        &subject.run_id,
        &format!("{prefix}/evidence/docker-diff.txt"),
        &diff_bytes,
    )?;
    let result_export = run::artifact_for_file(
        &format!("{prefix}/artifacts/result-rootfs.tar"),
        &result_export_path,
    )?;
    let completed_at = Utc::now();
    let mut warnings = vec![
        "filesystem state was reused; the prior process tree and live memory were not restored"
            .to_owned(),
        "the container was quiesced before result capture; background processes were terminated"
            .to_owned(),
    ];
    if output.stdout.truncated {
        warnings.push(format!(
            "continuation stdout exceeded {} bytes ({} bytes received); retained output is truncated",
            crate::process::MAX_RUN_OUTPUT_BYTES,
            output.stdout.total_bytes
        ));
    }
    if output.stderr.truncated {
        warnings.push(format!(
            "continuation stderr exceeded {} bytes ({} bytes received); retained output is truncated",
            crate::process::MAX_RUN_OUTPUT_BYTES,
            output.stderr.total_bytes
        ));
    }
    if output.timed_out {
        warnings.push(format!(
            "continuation exceeded the automatic {} second safety timeout and was terminated",
            crate::process::DEFAULT_GUEST_TIMEOUT_SECONDS
        ));
    }
    let mut integrity = BTreeMap::new();
    for artifact in [
        &stdout,
        &stderr,
        &result_export,
        &inspect_artifact,
        &diff_artifact,
    ] {
        integrity.insert(artifact.path.clone(), artifact.digest.clone());
    }
    for capture in &captures {
        integrity.insert(capture.path.clone(), capture.digest.clone());
    }
    integrity.insert(
        format!("{prefix}/result-rootfs.json"),
        run::sha256_bytes(&result_manifest_bytes),
    );
    integrity.insert(
        format!("{prefix}/delta.raw.json"),
        run::sha256_bytes(&raw_delta_bytes),
    );
    integrity.insert(
        format!("{prefix}/delta.json"),
        run::sha256_bytes(&portable_delta_bytes),
    );
    let identity = ContinuationIdentity {
        schema_version: CONTINUATION_SCHEMA_VERSION,
        continuation_id: &continuation_id,
        run_id: &subject.run_id,
        anchor_digest: &subject.anchor_digest,
        started_at,
        completed_at,
        command,
        secret_injections: &secret_injections,
        exit_code,
        stdout: &stdout,
        stderr: &stderr,
        captures: &captures,
        base_filesystem_digest: &subject.base_manifest.digest,
        result_filesystem_digest: &result_manifest.digest,
        raw_delta_digest: &raw_delta.digest,
        portable_delta_digest: &portable_delta.digest,
        container_name: &subject.container_name,
        container_id: &subject.container_id,
        container_state: &state,
        container_restarted: restarted,
        filesystem_state_reused: true,
        process_memory_restored: false,
        result_rootfs_export: &result_export,
        container_inspect: &inspect_artifact,
        docker_diff: &diff_artifact,
        warnings: &warnings,
        integrity: &integrity,
    };
    let result = ContinuationResult {
        schema_version: CONTINUATION_SCHEMA_VERSION.to_owned(),
        digest: run::sha256_bytes(&serde_json::to_vec(&identity)?),
        continuation_id,
        run_id: subject.run_id.clone(),
        anchor_digest: subject.anchor_digest.clone(),
        started_at,
        completed_at,
        command: command.to_vec(),
        secret_injections,
        exit_code,
        stdout,
        stderr,
        captures,
        base_filesystem_digest: subject.base_manifest.digest.clone(),
        result_filesystem_digest: result_manifest.digest,
        raw_delta_digest: raw_delta.digest,
        portable_delta_digest: portable_delta.digest,
        container_name: subject.container_name.clone(),
        container_id: subject.container_id.clone(),
        container_state: state,
        container_restarted: restarted,
        filesystem_state_reused: true,
        process_memory_restored: false,
        result_rootfs_export: result_export,
        container_inspect: inspect_artifact,
        docker_diff: diff_artifact,
        warnings,
        integrity,
    };
    store.write_run_file(
        &subject.run_id,
        &format!("{prefix}/continuation.json"),
        &run::pretty_json(&result)?,
    )?;
    failed_continuation_cleanup.armed = false;
    Ok(result)
}

fn load_subject(store: &Store, run_id: &str) -> Result<Subject> {
    if store.run_file_exists(run_id, "result.json")? {
        let result: RunResult = run::load_result(store, run_id)?;
        let spec: RunSpec = run::load_spec(store, run_id)?;
        let compact = run_id.replace('-', "");
        let base_manifest =
            serde_json::from_slice(&store.read_run_file(run_id, "base-rootfs.json")?)?;
        return Ok(Subject {
            run_id: run_id.to_owned(),
            kind: "run".to_owned(),
            parent_run_id: None,
            anchor_digest: result.digest,
            container_name: result.docker.retained_container_name,
            container_id: result.docker.retained_container_id,
            image_tag: format!("agentlab-prepared:{}", &compact[..12]),
            workspace_guest_path: spec.workspace_guest_path,
            network_policy: spec.network_policy,
            resource_limits: spec.resource_limits,
            captures: spec.captures,
            change_ignore: spec.change_ignore,
            base_manifest,
        });
    }
    if store.run_file_exists(run_id, "fork.json")? {
        let fork = load_fork(store, run_id)?;
        let base_manifest =
            serde_json::from_slice(&store.read_run_file(run_id, "base-rootfs.json")?)?;
        return Ok(Subject {
            run_id: run_id.to_owned(),
            kind: "fork".to_owned(),
            parent_run_id: Some(fork.parent_run_id),
            anchor_digest: fork.digest,
            container_name: fork.container_name,
            container_id: fork.container_id,
            image_tag: fork.image_tag,
            workspace_guest_path: fork.workspace_guest_path,
            network_policy: fork.network_policy,
            resource_limits: fork.resource_limits,
            captures: fork.captures,
            change_ignore: fork.change_ignore,
            base_manifest,
        });
    }
    bail!("run {run_id:?} has no completed result or fork record")
}

fn managed_run(store: &Store, subject: &Subject) -> ManagedRun {
    let inspect = docker_inspect_optional(&subject.container_name);
    let (container_state, lifecycle_capable) = inspect
        .as_deref()
        .and_then(|bytes| inspect_metadata(bytes).ok())
        .map(|metadata| (metadata.state, metadata.lifecycle_capable))
        .unwrap_or_else(|| ("missing".to_owned(), false));
    let continuation_count = store
        .run_path(&subject.run_id, "continuations")
        .ok()
        .and_then(|path| fs::read_dir(path).ok())
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().join("continuation.json").is_file())
                .count()
        })
        .unwrap_or(0);
    ManagedRun {
        run_id: subject.run_id.clone(),
        kind: subject.kind.clone(),
        parent_run_id: subject.parent_run_id.clone(),
        anchor_digest: subject.anchor_digest.clone(),
        container_name: subject.container_name.clone(),
        container_id: subject.container_id.clone(),
        container_state,
        lifecycle_capable,
        continuation_count,
    }
}

struct InspectMetadata {
    state: String,
    lifecycle_capable: bool,
}

fn inspect_metadata(bytes: &[u8]) -> Result<InspectMetadata> {
    let value: Value = serde_json::from_slice(bytes)?;
    let container = value
        .as_array()
        .and_then(|values| values.first())
        .context("Docker inspect returned no container")?;
    Ok(InspectMetadata {
        state: container
            .pointer("/State/Status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        lifecycle_capable: container
            .pointer("/Config/Labels/agentlab.lifecycle")
            .and_then(Value::as_str)
            == Some("v1"),
    })
}

fn assert_owned_container(subject: &Subject) -> Result<Vec<u8>> {
    assert_owned_container_fields(
        &subject.run_id,
        &subject.container_name,
        &subject.container_id,
    )
}

fn assert_owned_container_fields(run_id: &str, name: &str, expected_id: &str) -> Result<Vec<u8>> {
    let bytes = run::docker_output_bytes(
        Command::new("docker").args(["inspect", name]),
        "inspect retained container",
    )?;
    run::ensure_no_external_mounts(&bytes)?;
    let value: Value = serde_json::from_slice(&bytes)?;
    let container = value
        .as_array()
        .and_then(|values| values.first())
        .context("Docker inspect returned no container")?;
    let actual_id = container["Id"]
        .as_str()
        .context("Docker inspect omitted container ID")?;
    if actual_id != expected_id {
        bail!("retained container ID does not match AgentLab record");
    }
    let labels = container
        .pointer("/Config/Labels")
        .and_then(Value::as_object)
        .context("Docker inspect omitted labels")?;
    if labels.get("agentlab.run_id").and_then(Value::as_str) != Some(run_id) {
        bail!("container is not owned by selected AgentLab run");
    }
    if labels.get("agentlab.lifecycle").and_then(Value::as_str) != Some("v1") {
        bail!("run predates lifecycle support and cannot be safely restarted");
    }
    Ok(bytes)
}

fn docker_inspect_optional(name: &str) -> Option<Vec<u8>> {
    Command::new("docker")
        .args(["inspect", name])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| output.stdout)
}

fn write_event(
    store: &Store,
    subject: &Subject,
    event: &str,
    state: &str,
) -> Result<LifecycleEventRecord> {
    let event_id = Uuid::new_v4().to_string();
    let timestamp = Utc::now();
    let identity = EventIdentity {
        schema_version: LIFECYCLE_EVENT_SCHEMA_VERSION,
        event_id: &event_id,
        run_id: &subject.run_id,
        event,
        timestamp,
        container_id: &subject.container_id,
        container_state: state,
        filesystem_state_preserved: true,
        process_memory_restored: false,
    };
    let record = LifecycleEventRecord {
        schema_version: LIFECYCLE_EVENT_SCHEMA_VERSION.to_owned(),
        digest: run::sha256_bytes(&serde_json::to_vec(&identity)?),
        event_id: event_id.clone(),
        run_id: subject.run_id.clone(),
        event: event.to_owned(),
        timestamp,
        container_id: subject.container_id.clone(),
        container_state: state.to_owned(),
        filesystem_state_preserved: true,
        process_memory_restored: false,
    };
    store.write_run_file(
        &subject.run_id,
        &format!("lifecycle/{event_id}.json"),
        &run::pretty_json(&record)?,
    )?;
    Ok(record)
}

fn export_captures(store: &Store, subject: &Subject, prefix: &str) -> Result<Vec<Artifact>> {
    let mut artifacts = Vec::new();
    for capture in &subject.captures {
        let relative = format!("{prefix}/artifacts/capture-{}.tar", capture.name);
        let path = store.run_path(&subject.run_id, &relative)?;
        let output_file = File::create(&path)?;
        let output = Command::new("docker")
            .args([
                "cp",
                &format!("{}:{}", subject.container_name, capture.guest_path),
                "-",
            ])
            .stdout(Stdio::from(output_file))
            .stderr(Stdio::piped())
            .output()?;
        if !output.status.success() {
            bail!(
                "export continuation capture {}: {}",
                capture.guest_path,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        artifacts.push(run::artifact_for_file(&relative, &path)?);
    }
    Ok(artifacts)
}

fn write_bytes_artifact(
    store: &Store,
    run_id: &str,
    relative: &str,
    bytes: &[u8],
) -> Result<Artifact> {
    store.write_run_file(run_id, relative, bytes)?;
    Ok(Artifact {
        path: relative.to_owned(),
        digest: run::sha256_bytes(bytes),
        size: bytes.len() as u64,
    })
}

fn remove_image_tag(tag: &str) -> Result<()> {
    let output = Command::new("docker").args(["image", "rm", tag]).output()?;
    if output.status.success() || String::from_utf8_lossy(&output.stderr).contains("No such image")
    {
        return Ok(());
    }
    bail!(
        "remove selected AgentLab image tag: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}
