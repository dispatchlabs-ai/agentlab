use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::review::{self, DispositionCounts, ReviewRecord, WorkspaceOperation};
use crate::run::{self, Artifact};
use crate::snapshot::{self, Manifest};
use crate::store::{Store, create_new_file};

pub const APPLY_SCHEMA_VERSION: &str = "agentlab.apply/v1";

#[derive(Debug, Clone)]
pub struct ApplyOptions {
    pub review_id: String,
    pub workspace: PathBuf,
    pub acknowledge_conflicts: bool,
    pub acknowledge_unresolved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplyCounts {
    pub proposed: usize,
    pub rejected: usize,
    pub conflicted: usize,
    pub unresolved: usize,
    pub applied: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplyRecord {
    pub schema_version: String,
    pub digest: String,
    pub apply_id: String,
    pub review_id: String,
    pub review_digest: String,
    pub run_id: String,
    pub result_digest: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub workspace: String,
    pub before_workspace_snapshot_digest: String,
    pub intended_workspace_snapshot_digest: String,
    pub after_workspace_snapshot_digest: String,
    pub acknowledged_conflicts: bool,
    pub acknowledged_unresolved: bool,
    pub counts: ApplyCounts,
    pub operations: Vec<WorkspaceOperation>,
    pub backup_artifact: Artifact,
    pub source_workspace_matched_review: bool,
    pub result_workspace_verified: bool,
    pub warnings: Vec<String>,
    pub integrity: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct ApplyIdentity<'a> {
    schema_version: &'a str,
    apply_id: &'a str,
    review_id: &'a str,
    review_digest: &'a str,
    run_id: &'a str,
    result_digest: &'a str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    workspace: &'a str,
    before_workspace_snapshot_digest: &'a str,
    intended_workspace_snapshot_digest: &'a str,
    after_workspace_snapshot_digest: &'a str,
    acknowledged_conflicts: bool,
    acknowledged_unresolved: bool,
    counts: &'a ApplyCounts,
    operations: &'a [WorkspaceOperation],
    backup_artifact: &'a Artifact,
    source_workspace_matched_review: bool,
    result_workspace_verified: bool,
    warnings: &'a [String],
    integrity: &'a BTreeMap<String, String>,
}

pub fn apply(store: &Store, options: &ApplyOptions) -> Result<ApplyRecord> {
    let review = review::find(store, &options.review_id)?;
    review::verify(store, &review)?;
    let _lock = ApplyLock::acquire(store, &review)?;
    let record_path = apply_record_path(&review.review_id);
    if store.run_file_exists(&review.run_id, &record_path)? {
        bail!(
            "review {:?} already has an accepted apply record",
            review.review_id
        );
    }

    authorize_dispositions(&review, options)?;
    let operations = proposed_workspace_operations(&review);
    if operations.is_empty() {
        bail!(
            "review {:?} proposes no workspace operations",
            review.review_id
        );
    }

    let current = snapshot::create(&options.workspace, store)?;
    let workspace = current.workspace;
    let before = current.manifest;
    if workspace != Path::new(&review.source_workspace) {
        bail!(
            "review {:?} was created for workspace {}, not {}; run `agentlab review` for the selected workspace",
            review.review_id,
            review.source_workspace,
            workspace.display()
        );
    }
    if before.digest != review.request.anchors.current_workspace_snapshot_digest {
        bail!(
            "current workspace is stale for review {:?}: reviewed {}, found {}; run `agentlab review` again",
            review.review_id,
            review.request.anchors.current_workspace_snapshot_digest,
            before.digest
        );
    }
    let candidate = snapshot::load(
        store,
        &review.request.anchors.candidate_workspace_snapshot_digest,
    )?;
    snapshot::verify(store, &candidate)?;
    validate_operations_against_candidate(&operations, &candidate)?;

    let staging = tempfile::tempdir().context("create private apply staging directory")?;
    let staged_workspace = staging.path().join("workspace");
    snapshot::materialize(store, &before, &staged_workspace)?;
    apply_manifest_state(
        store,
        &candidate,
        &staged_workspace,
        operation_paths(&operations),
    )
    .context("construct reviewed workspace result in private staging")?;
    let intended = snapshot::create(&staged_workspace, store)?.manifest;
    snapshot::verify(store, &intended)?;

    let confirmed = snapshot::create(&workspace, store)?.manifest;
    if confirmed.digest != before.digest {
        bail!(
            "current workspace changed while preparing apply for review {:?}; no changes were applied",
            review.review_id
        );
    }

    let started_at = Utc::now();
    let apply_id = Uuid::new_v4().to_string();
    let backup_bytes = store.read_snapshot(&before.digest)?;
    let backup_artifact = write_artifact(
        store,
        &review.run_id,
        &format!("reviews/{}/apply/backup-manifest.json", review.review_id),
        &backup_bytes,
    )?;

    if let Err(error) =
        apply_manifest_state(store, &candidate, &workspace, operation_paths(&operations))
    {
        return rollback_error(
            store,
            &before,
            &workspace,
            &operations,
            &format!("apply failed: {error:#}"),
        );
    }

    let after = match snapshot::create(&workspace, store) {
        Ok(result) => result.manifest,
        Err(error) => {
            return rollback_error(
                store,
                &before,
                &workspace,
                &operations,
                &format!("verify applied workspace: {error:#}"),
            );
        }
    };
    if after.digest != intended.digest {
        return rollback_error(
            store,
            &before,
            &workspace,
            &operations,
            &format!(
                "applied workspace did not match the privately staged result: expected {}, found {}",
                intended.digest, after.digest
            ),
        );
    }
    if let Err(error) = snapshot::verify(store, &after) {
        return rollback_error(
            store,
            &before,
            &workspace,
            &operations,
            &format!("verify applied snapshot: {error:#}"),
        );
    }

    let completed_at = Utc::now();
    let counts = apply_counts(&review, operations.len());
    let mut integrity = BTreeMap::new();
    integrity.insert(backup_artifact.path.clone(), backup_artifact.digest.clone());
    let warnings = vec![
        "AgentLab applied only workspace operations authorized by the selected review receipt"
            .to_owned(),
        format!(
            "the complete pre-apply workspace remains recoverable from snapshot {}",
            before.digest
        ),
        "review and apply records may contain sensitive workspace paths and recommendations"
            .to_owned(),
    ];
    let workspace = workspace
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("workspace path is not valid UTF-8"))?;
    let identity = ApplyIdentity {
        schema_version: APPLY_SCHEMA_VERSION,
        apply_id: &apply_id,
        review_id: &review.review_id,
        review_digest: &review.digest,
        run_id: &review.run_id,
        result_digest: &review.result_digest,
        started_at,
        completed_at,
        workspace: &workspace,
        before_workspace_snapshot_digest: &before.digest,
        intended_workspace_snapshot_digest: &intended.digest,
        after_workspace_snapshot_digest: &after.digest,
        acknowledged_conflicts: options.acknowledge_conflicts,
        acknowledged_unresolved: options.acknowledge_unresolved,
        counts: &counts,
        operations: &operations,
        backup_artifact: &backup_artifact,
        source_workspace_matched_review: true,
        result_workspace_verified: true,
        warnings: &warnings,
        integrity: &integrity,
    };
    let record = ApplyRecord {
        schema_version: APPLY_SCHEMA_VERSION.to_owned(),
        digest: run::sha256_bytes(&serde_json::to_vec(&identity)?),
        apply_id,
        review_id: review.review_id,
        review_digest: review.digest,
        run_id: review.run_id,
        result_digest: review.result_digest,
        started_at,
        completed_at,
        workspace,
        before_workspace_snapshot_digest: before.digest.clone(),
        intended_workspace_snapshot_digest: intended.digest.clone(),
        after_workspace_snapshot_digest: after.digest.clone(),
        acknowledged_conflicts: options.acknowledge_conflicts,
        acknowledged_unresolved: options.acknowledge_unresolved,
        counts,
        operations,
        backup_artifact,
        source_workspace_matched_review: true,
        result_workspace_verified: true,
        warnings,
        integrity,
    };
    let record_bytes = run::pretty_json(&record)?;
    if let Err(error) = store.write_run_file(&record.run_id, &record_path, &record_bytes) {
        return rollback_error(
            store,
            &before,
            Path::new(&record.workspace),
            &record.operations,
            &format!("persist apply receipt: {error:#}"),
        );
    }
    if let Err(error) = verify(store, &record) {
        let stored_record = store.run_path(&record.run_id, &record_path)?;
        let _ = fs::remove_file(stored_record);
        return rollback_error(
            store,
            &before,
            Path::new(&record.workspace),
            &record.operations,
            &format!("verify persisted apply receipt: {error:#}"),
        );
    }
    Ok(record)
}

struct ApplyLock {
    path: PathBuf,
    _file: File,
}

impl ApplyLock {
    fn acquire(store: &Store, review: &ReviewRecord) -> Result<Self> {
        let relative = format!("reviews/{}/apply.lock", review.review_id);
        let path = store.run_path(&review.run_id, &relative)?;
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => bail!(
                "apply for review {:?} is already in progress or was interrupted; inspect {} before retrying",
                review.review_id,
                path.display()
            ),
            Err(error) => return Err(error).context("create exclusive apply lock"),
        };
        writeln!(
            file,
            "review_id={}\npid={}\nstarted_at={}",
            review.review_id,
            std::process::id(),
            Utc::now()
        )?;
        file.sync_all()?;
        Ok(Self { path, _file: file })
    }
}

impl Drop for ApplyLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn list(store: &Store, run_id: &str) -> Result<Vec<ApplyRecord>> {
    let mut records: Vec<ApplyRecord> = Vec::new();
    for review in review::list(store, run_id)? {
        let relative = apply_record_path(&review.review_id);
        if store.run_file_exists(run_id, &relative)? {
            records.push(serde_json::from_slice(
                &store.read_run_file(run_id, &relative)?,
            )?);
        }
    }
    records.sort_by(|left, right| {
        left.completed_at
            .cmp(&right.completed_at)
            .then_with(|| left.apply_id.cmp(&right.apply_id))
    });
    Ok(records)
}

pub fn find(store: &Store, apply_id: &str) -> Result<ApplyRecord> {
    Uuid::parse_str(apply_id).context("apply ID is not a UUID")?;
    let mut found = None;
    for run_id in store.list_run_ids()? {
        for record in list(store, &run_id)? {
            if record.apply_id != apply_id {
                continue;
            }
            if found.is_some() {
                bail!("apply ID {apply_id:?} is not unique");
            }
            found = Some(record);
        }
    }
    found.with_context(|| format!("apply {apply_id:?} not found"))
}

pub fn verify_all(store: &Store, run_id: &str) -> Result<()> {
    for record in list(store, run_id)? {
        verify(store, &record)?;
    }
    Ok(())
}

pub fn verify(store: &Store, record: &ApplyRecord) -> Result<()> {
    if record.schema_version != APPLY_SCHEMA_VERSION {
        bail!("unsupported apply schema {:?}", record.schema_version);
    }
    Uuid::parse_str(&record.apply_id).context("apply ID is not a UUID")?;
    if !Path::new(&record.workspace).is_absolute() {
        bail!("apply record workspace path is not absolute");
    }
    if record.completed_at < record.started_at {
        bail!("apply record completion precedes its start");
    }
    let review = review::find(store, &record.review_id)?;
    review::verify(store, &review)?;
    if record.review_digest != review.digest
        || record.run_id != review.run_id
        || record.result_digest != review.result_digest
        || record.workspace != review.source_workspace
        || record.before_workspace_snapshot_digest
            != review.request.anchors.current_workspace_snapshot_digest
    {
        bail!("apply record does not agree with its review receipt");
    }
    let expected_operations = proposed_workspace_operations(&review);
    if record.operations != expected_operations {
        bail!("apply record operations do not match its review proposal");
    }
    let expected_counts = apply_counts(&review, expected_operations.len());
    if record.counts != expected_counts {
        bail!("apply record disposition counts are inconsistent");
    }
    if record.counts.conflicted > 0 && !record.acknowledged_conflicts {
        bail!("apply record did not acknowledge conflicted candidates");
    }
    if record.counts.unresolved > 0 && !record.acknowledged_unresolved {
        bail!("apply record did not acknowledge unresolved candidates");
    }
    if !record.source_workspace_matched_review || !record.result_workspace_verified {
        bail!("apply record does not assert its required workspace checks");
    }
    if record.intended_workspace_snapshot_digest != record.after_workspace_snapshot_digest {
        bail!("apply record intended and actual workspace snapshots differ");
    }
    let expected_backup_path = format!("reviews/{}/apply/backup-manifest.json", record.review_id);
    if record.backup_artifact.path != expected_backup_path {
        bail!("apply backup artifact path is inconsistent");
    }
    for digest in [
        &record.before_workspace_snapshot_digest,
        &record.intended_workspace_snapshot_digest,
        &record.after_workspace_snapshot_digest,
    ] {
        let manifest = snapshot::load(store, digest)?;
        snapshot::verify(store, &manifest)?;
    }
    let backup_bytes = store.read_run_file(&record.run_id, &record.backup_artifact.path)?;
    if run::sha256_bytes(&backup_bytes) != record.backup_artifact.digest
        || backup_bytes.len() as u64 != record.backup_artifact.size
    {
        bail!("apply backup artifact integrity mismatch");
    }
    let backup: Manifest =
        serde_json::from_slice(&backup_bytes).context("decode apply backup manifest")?;
    if backup.digest != record.before_workspace_snapshot_digest {
        bail!("apply backup artifact does not describe the before workspace");
    }
    if backup_bytes != store.read_snapshot(&record.before_workspace_snapshot_digest)? {
        bail!("apply backup artifact is not the canonical before-workspace manifest");
    }
    let expected_integrity = BTreeMap::from([(
        record.backup_artifact.path.clone(),
        record.backup_artifact.digest.clone(),
    )]);
    if record.integrity != expected_integrity {
        bail!("apply record integrity map is inconsistent");
    }
    for (relative, expected) in &record.integrity {
        let actual = run::sha256_bytes(&store.read_run_file(&record.run_id, relative)?);
        if &actual != expected {
            bail!("apply artifact integrity mismatch for {relative:?}");
        }
    }
    let identity = ApplyIdentity {
        schema_version: APPLY_SCHEMA_VERSION,
        apply_id: &record.apply_id,
        review_id: &record.review_id,
        review_digest: &record.review_digest,
        run_id: &record.run_id,
        result_digest: &record.result_digest,
        started_at: record.started_at,
        completed_at: record.completed_at,
        workspace: &record.workspace,
        before_workspace_snapshot_digest: &record.before_workspace_snapshot_digest,
        intended_workspace_snapshot_digest: &record.intended_workspace_snapshot_digest,
        after_workspace_snapshot_digest: &record.after_workspace_snapshot_digest,
        acknowledged_conflicts: record.acknowledged_conflicts,
        acknowledged_unresolved: record.acknowledged_unresolved,
        counts: &record.counts,
        operations: &record.operations,
        backup_artifact: &record.backup_artifact,
        source_workspace_matched_review: record.source_workspace_matched_review,
        result_workspace_verified: record.result_workspace_verified,
        warnings: &record.warnings,
        integrity: &record.integrity,
    };
    if run::sha256_bytes(&serde_json::to_vec(&identity)?) != record.digest {
        bail!("apply record integrity mismatch");
    }
    Ok(())
}

fn authorize_dispositions(review: &ReviewRecord, options: &ApplyOptions) -> Result<()> {
    let counts = &review.proposal.counts;
    if counts.conflicted > 0 && !options.acknowledge_conflicts {
        bail!(
            "review contains {} conflicted candidate(s); rerun with --acknowledge-conflicts to apply only the proposed workspace operations",
            counts.conflicted
        );
    }
    if counts.unresolved > 0 && !options.acknowledge_unresolved {
        bail!(
            "review contains {} unresolved candidate(s); rerun with --acknowledge-unresolved to apply only the proposed workspace operations",
            counts.unresolved
        );
    }
    Ok(())
}

fn proposed_workspace_operations(review: &ReviewRecord) -> Vec<WorkspaceOperation> {
    let mut operations: Vec<_> = review
        .proposal
        .dispositions
        .iter()
        .filter(|item| item.disposition == "proposed")
        .filter_map(|item| item.workspace_operation.clone())
        .collect();
    operations.sort_by(|left, right| left.path.cmp(&right.path));
    operations
}

fn apply_counts(review: &ReviewRecord, applied: usize) -> ApplyCounts {
    let DispositionCounts {
        proposed,
        rejected,
        conflicted,
        unresolved,
    } = &review.proposal.counts;
    ApplyCounts {
        proposed: *proposed,
        rejected: *rejected,
        conflicted: *conflicted,
        unresolved: *unresolved,
        applied,
    }
}

fn validate_operations_against_candidate(
    operations: &[WorkspaceOperation],
    candidate: &Manifest,
) -> Result<()> {
    let entries: BTreeMap<_, _> = candidate
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut paths = BTreeSet::new();
    for operation in operations {
        snapshot::validate_relative_path(&operation.path)?;
        if !paths.insert(operation.path.as_str()) {
            bail!("duplicate apply operation path {:?}", operation.path);
        }
        match (
            operation.operation.as_str(),
            entries.get(operation.path.as_str()),
        ) {
            ("delete", None) | ("replace", Some(_)) => {}
            ("delete", Some(_)) => bail!(
                "delete operation {:?} still exists in candidate workspace",
                operation.path
            ),
            ("replace", None) => bail!(
                "replace operation {:?} is missing from candidate workspace",
                operation.path
            ),
            (value, _) => bail!("unsupported apply operation {value:?}"),
        }
    }
    Ok(())
}

fn operation_paths(operations: &[WorkspaceOperation]) -> Vec<String> {
    operations
        .iter()
        .map(|operation| operation.path.clone())
        .collect()
}

fn apply_manifest_state(
    store: &Store,
    desired: &Manifest,
    workspace: &Path,
    paths: Vec<String>,
) -> Result<()> {
    let desired_entries: BTreeMap<_, _> = desired
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut paths = paths;
    paths.sort();
    paths.dedup();
    for path in &paths {
        snapshot::validate_relative_path(path)?;
        make_exact_directory_writable(workspace, path)?;
    }

    let mut clear_paths: Vec<_> = paths
        .iter()
        .filter(|path| {
            desired_entries
                .get(path.as_str())
                .is_none_or(|entry| entry.kind != "directory")
        })
        .collect();
    clear_paths.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));
    for path in clear_paths {
        remove_target(workspace, path)?;
    }

    let mut directories: Vec<_> = paths
        .iter()
        .filter_map(|path| {
            desired_entries
                .get(path.as_str())
                .filter(|entry| entry.kind == "directory")
        })
        .collect();
    directories.sort_by_key(|entry| entry.path.matches('/').count());
    for entry in &directories {
        let target = checked_target(workspace, &entry.path, false)?;
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                fs::remove_file(&target)
                    .with_context(|| format!("replace {:?} with a directory", entry.path))?;
                fs::create_dir(&target)
                    .with_context(|| format!("create directory {:?}", entry.path))?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&target)
                    .with_context(|| format!("create directory {:?}", entry.path))?;
            }
            Err(error) => return Err(error).with_context(|| format!("inspect {:?}", entry.path)),
        }
    }

    for path in &paths {
        let Some(entry) = desired_entries.get(path.as_str()) else {
            continue;
        };
        if entry.kind == "directory" {
            continue;
        }
        let target = checked_target(workspace, path, false)?;
        match entry.kind.as_str() {
            "file" => {
                let mut source = store.open_blob(&entry.digest)?;
                let mut destination: File = create_new_file(&target)?;
                io::copy(&mut source, &mut destination)
                    .with_context(|| format!("write reviewed file {path:?}"))?;
                destination.sync_all()?;
                set_mode(&target, entry.mode)?;
            }
            "symlink" => create_symlink(&entry.link_target, &target)
                .with_context(|| format!("create reviewed symlink {path:?}"))?,
            value => bail!("unsupported desired workspace entry type {value:?}"),
        }
    }

    directories.sort_by_key(|entry| std::cmp::Reverse(entry.path.matches('/').count()));
    for entry in directories {
        set_mode(&snapshot::safe_join(workspace, &entry.path)?, entry.mode)?;
    }
    Ok(())
}

fn checked_target(workspace: &Path, relative: &str, allow_missing_parent: bool) -> Result<PathBuf> {
    let target = snapshot::safe_join(workspace, relative)?;
    let mut ancestor = workspace.to_path_buf();
    let components: Vec<_> = relative.split('/').collect();
    for component in &components[..components.len() - 1] {
        ancestor.push(component);
        match fs::symlink_metadata(&ancestor) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => bail!(
                "workspace path {:?} has a non-directory ancestor {}",
                relative,
                ancestor.display()
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound && allow_missing_parent => {
                return Ok(target);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
                "workspace parent {} for {:?} does not exist or was not authorized by the review",
                ancestor.display(),
                relative
            ),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect workspace ancestor {}", ancestor.display()));
            }
        }
    }
    Ok(target)
}

fn remove_target(workspace: &Path, relative: &str) -> Result<()> {
    let target = checked_target(workspace, relative, true)?;
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir(&target).with_context(|| {
            format!(
                "remove reviewed directory {:?}; it contains content not authorized for removal",
                relative
            )
        })?,
        Ok(_) => fs::remove_file(&target)
            .with_context(|| format!("remove reviewed path {relative:?}"))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect reviewed path {relative:?}")),
    }
    Ok(())
}

fn make_exact_directory_writable(workspace: &Path, relative: &str) -> Result<()> {
    let target = checked_target(workspace, relative, true)?;
    let metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspect {relative:?}")),
    };
    if metadata.file_type().is_dir() {
        make_writable(&target, &metadata)?;
    }
    Ok(())
}

fn rollback_error<T>(
    store: &Store,
    before: &Manifest,
    workspace: &Path,
    operations: &[WorkspaceOperation],
    cause: &str,
) -> Result<T> {
    let rollback = apply_manifest_state(store, before, workspace, operation_paths(operations))
        .and_then(|()| snapshot::create(workspace, store).map(|result| result.manifest))
        .and_then(|restored| {
            if restored.digest == before.digest {
                Ok(())
            } else {
                bail!(
                    "restored workspace digest {} does not match backup {}",
                    restored.digest,
                    before.digest
                )
            }
        });
    match rollback {
        Ok(()) => bail!("{cause}; AgentLab restored the reviewed workspace paths from the backup"),
        Err(error) => bail!(
            "{cause}; automatic rollback failed: {error:#}; recover from backup snapshot {}",
            before.digest
        ),
    }
}

fn apply_record_path(review_id: &str) -> String {
    format!("reviews/{review_id}/apply.json")
}

fn write_artifact(store: &Store, run_id: &str, relative: &str, bytes: &[u8]) -> Result<Artifact> {
    store.write_run_file(run_id, relative, bytes)?;
    Ok(Artifact {
        path: relative.to_owned(),
        digest: run::sha256_bytes(bytes),
        size: bytes.len() as u64,
    })
}

#[cfg(unix)]
fn create_symlink(target: &str, path: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(windows)]
fn create_symlink(target: &str, path: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, path)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn make_writable(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode() | 0o700;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_writable(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    let mut permissions = metadata.permissions();
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_manifest_operations_preserve_unauthorized_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        let candidate = temporary.path().join("candidate");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&candidate).unwrap();
        fs::write(workspace.join("replace.txt"), "old\n").unwrap();
        fs::write(workspace.join("delete.txt"), "delete\n").unwrap();
        fs::write(workspace.join("keep.txt"), "keep\n").unwrap();
        fs::write(candidate.join("replace.txt"), "new\n").unwrap();
        fs::write(candidate.join("keep.txt"), "keep\n").unwrap();
        let store = Store::open(Some(&state)).unwrap();
        let desired = snapshot::create(&candidate, &store).unwrap().manifest;

        apply_manifest_state(
            &store,
            &desired,
            &workspace,
            vec!["replace.txt".to_owned(), "delete.txt".to_owned()],
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(workspace.join("replace.txt")).unwrap(),
            "new\n"
        );
        assert!(!workspace.join("delete.txt").exists());
        assert_eq!(
            fs::read_to_string(workspace.join("keep.txt")).unwrap(),
            "keep\n"
        );
        assert_eq!(
            snapshot::create(&workspace, &store)
                .unwrap()
                .manifest
                .digest,
            desired.digest
        );
    }

    #[test]
    fn rollback_restores_earlier_paths_when_a_later_operation_is_unsafe() {
        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        let candidate = temporary.path().join("candidate");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&candidate).unwrap();
        fs::write(workspace.join("a-first.txt"), "old\n").unwrap();
        fs::create_dir(workspace.join("z-blocked")).unwrap();
        fs::write(workspace.join("z-blocked/unauthorized.txt"), "keep\n").unwrap();
        fs::write(candidate.join("a-first.txt"), "new\n").unwrap();
        fs::write(candidate.join("z-blocked"), "replacement\n").unwrap();
        let store = Store::open(Some(&state)).unwrap();
        let before = snapshot::create(&workspace, &store).unwrap().manifest;
        let desired = snapshot::create(&candidate, &store).unwrap().manifest;
        let operations = vec![
            WorkspaceOperation {
                operation: "replace".to_owned(),
                path: "a-first.txt".to_owned(),
            },
            WorkspaceOperation {
                operation: "replace".to_owned(),
                path: "z-blocked".to_owned(),
            },
        ];

        let apply_error =
            apply_manifest_state(&store, &desired, &workspace, operation_paths(&operations))
                .unwrap_err();
        assert!(format!("{apply_error:#}").contains("not authorized for removal"));
        let rollback = rollback_error::<()>(
            &store,
            &before,
            &workspace,
            &operations,
            "fixture apply failed",
        )
        .unwrap_err();
        assert!(format!("{rollback:#}").contains("restored"));
        assert_eq!(
            fs::read_to_string(workspace.join("a-first.txt")).unwrap(),
            "old\n"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("z-blocked/unauthorized.txt")).unwrap(),
            "keep\n"
        );
        assert_eq!(
            snapshot::create(&workspace, &store)
                .unwrap()
                .manifest
                .digest,
            before.digest
        );
    }
}
