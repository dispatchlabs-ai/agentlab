use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::lock::AdvisoryLock;
use crate::review::{self, DispositionCounts, ReviewRecord, WorkspaceOperation};
use crate::run::{self, Artifact};
use crate::snapshot::{self, Manifest};
use crate::store::Store;

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
    #[cfg(unix)]
    {
        apply_unix(store, options)
    }

    #[cfg(not(unix))]
    {
        let _ = (store, options);
        bail!("safe workspace apply currently requires a Unix host")
    }
}

#[cfg(unix)]
fn apply_unix(store: &Store, options: &ApplyOptions) -> Result<ApplyRecord> {
    let review = review::find(store, &options.review_id)?;
    review::verify(store, &review)?;
    let pinned_workspace = snapshot::PinnedWorkspace::open(&options.workspace)?;
    let workspace_lock = acquire_workspace_lock(store, &pinned_workspace)?;
    pinned_workspace.verify_path_identity()?;
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

    let current =
        snapshot::create_from_pinned(&pinned_workspace, store, snapshot::CaptureMode::All)?;
    let workspace = pinned_workspace.path().to_path_buf();
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

    let confirmed =
        snapshot::create_from_pinned(&pinned_workspace, store, snapshot::CaptureMode::All)?
            .manifest;
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

    let workspace_root = WorkspaceRoot::from_pinned(&pinned_workspace)?;
    workspace_root.pin_existing_parents(&operation_paths(&operations))?;
    let mut workspace_transaction =
        workspace_lock.begin(&review, &before, pinned_workspace.path(), &backup_artifact)?;
    if let Err(error) = apply_manifest_state_on_root(
        store,
        &candidate,
        &workspace_root,
        operation_paths(&operations),
    ) {
        return rollback_error(
            store,
            &before,
            &pinned_workspace,
            &workspace_root,
            &mut workspace_transaction,
            &operations,
            &format!("apply failed: {error:#}"),
        );
    }

    let after =
        match snapshot::create_from_pinned(&pinned_workspace, store, snapshot::CaptureMode::All) {
            Ok(result) => result.manifest,
            Err(error) => {
                return rollback_error(
                    store,
                    &before,
                    &pinned_workspace,
                    &workspace_root,
                    &mut workspace_transaction,
                    &operations,
                    &format!("verify applied workspace: {error:#}"),
                );
            }
        };
    if after.digest != intended.digest {
        return rollback_error(
            store,
            &before,
            &pinned_workspace,
            &workspace_root,
            &mut workspace_transaction,
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
            &pinned_workspace,
            &workspace_root,
            &mut workspace_transaction,
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
            &pinned_workspace,
            &workspace_root,
            &mut workspace_transaction,
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
            &pinned_workspace,
            &workspace_root,
            &mut workspace_transaction,
            &record.operations,
            &format!("verify persisted apply receipt: {error:#}"),
        );
    }
    workspace_transaction.complete()?;
    Ok(record)
}

#[cfg(unix)]
fn acquire_workspace_lock(
    store: &Store,
    workspace: &snapshot::PinnedWorkspace,
) -> Result<WorkspaceApplyLock> {
    let key = run::sha256_bytes(workspace.lock_identity()?.as_bytes());
    let directory = store.root().join("locks").join("workspaces");
    let key = key.trim_start_matches("sha256:");
    let lock_path = directory.join(format!("{key}.lock"));
    let recovery_path = directory.join(format!("{key}.transaction.json"));
    let advisory = AdvisoryLock::acquire(
        &lock_path,
        &format!(
            "AgentLab apply for workspace {}",
            workspace.path().display()
        ),
    )?;
    if recovery_path.exists() {
        bail!(
            "workspace {} has an interrupted AgentLab apply transaction; inspect {} and its recorded backup before applying another review",
            workspace.path().display(),
            recovery_path.display()
        );
    }
    Ok(WorkspaceApplyLock {
        _advisory: advisory,
        recovery_path,
    })
}

#[cfg(unix)]
struct WorkspaceApplyLock {
    _advisory: AdvisoryLock,
    recovery_path: PathBuf,
}

#[cfg(unix)]
#[derive(Serialize)]
struct WorkspaceTransactionRecord<'a> {
    schema_version: &'static str,
    review_id: &'a str,
    run_id: &'a str,
    workspace: &'a str,
    before_workspace_snapshot_digest: &'a str,
    backup_artifact_path: &'a str,
    started_at: DateTime<Utc>,
}

#[cfg(unix)]
impl WorkspaceApplyLock {
    fn begin(
        &self,
        review: &ReviewRecord,
        before: &Manifest,
        workspace: &Path,
        backup_artifact: &Artifact,
    ) -> Result<WorkspaceTransaction> {
        let workspace = workspace
            .to_str()
            .context("workspace path is not valid UTF-8")?;
        let record = WorkspaceTransactionRecord {
            schema_version: "agentlab.workspace-transaction/v1",
            review_id: &review.review_id,
            run_id: &review.run_id,
            workspace,
            before_workspace_snapshot_digest: &before.digest,
            backup_artifact_path: &backup_artifact.path,
            started_at: Utc::now(),
        };
        let bytes = run::pretty_json(&record)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.recovery_path)
            .with_context(|| {
                format!(
                    "create workspace transaction marker {}",
                    self.recovery_path.display()
                )
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        File::open(
            self.recovery_path
                .parent()
                .context("workspace transaction marker has no parent")?,
        )?
        .sync_all()?;
        Ok(WorkspaceTransaction {
            path: self.recovery_path.clone(),
            active: true,
        })
    }
}

#[cfg(unix)]
struct WorkspaceTransaction {
    path: PathBuf,
    active: bool,
}

#[cfg(unix)]
impl WorkspaceTransaction {
    fn complete(&mut self) -> Result<()> {
        if self.active {
            fs::remove_file(&self.path).with_context(|| {
                format!(
                    "clear completed workspace transaction {}",
                    self.path.display()
                )
            })?;
            File::open(
                self.path
                    .parent()
                    .context("workspace transaction marker has no parent")?,
            )?
            .sync_all()?;
            self.active = false;
        }
        Ok(())
    }
}

#[cfg(unix)]
struct ApplyLock {
    path: PathBuf,
    _file: File,
}

#[cfg(unix)]
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

#[cfg(unix)]
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
    #[cfg(unix)]
    {
        apply_manifest_state_unix(store, desired, workspace, paths)
    }

    #[cfg(not(unix))]
    {
        let _ = (store, desired, workspace, paths);
        bail!("safe workspace apply currently requires a Unix host");
    }
}

#[cfg(unix)]
fn apply_manifest_state_unix(
    store: &Store,
    desired: &Manifest,
    workspace: &Path,
    paths: Vec<String>,
) -> Result<()> {
    let workspace = WorkspaceRoot::open(workspace)?;
    apply_manifest_state_on_root(store, desired, &workspace, paths)
}

#[cfg(unix)]
fn apply_manifest_state_on_root(
    store: &Store,
    desired: &Manifest,
    workspace: &WorkspaceRoot,
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
    workspace.pin_existing_parents(&paths)?;
    for path in &paths {
        snapshot::validate_relative_path(path)?;
        workspace.make_exact_directory_writable(path)?;
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
        workspace.remove_target(path)?;
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
        workspace.ensure_directory(&entry.path, entry.mode)?;
    }

    for path in &paths {
        let Some(entry) = desired_entries.get(path.as_str()) else {
            continue;
        };
        if entry.kind == "directory" {
            continue;
        }
        match entry.kind.as_str() {
            "file" => {
                let mut source = store.open_blob(&entry.digest, entry.size)?;
                let destination = workspace.create_file(path, entry.mode)?;
                let mut destination = File::from(destination);
                io::copy(&mut source, &mut destination)
                    .with_context(|| format!("write reviewed file {path:?}"))?;
                destination.sync_all()?;
                rustix::fs::fchmod(&destination, unix_mode(entry.mode))
                    .with_context(|| format!("set reviewed file mode for {path:?}"))?;
            }
            "symlink" => workspace.create_symlink(path, &entry.link_target)?,
            value => bail!("unsupported desired workspace entry type {value:?}"),
        }
    }

    directories.sort_by_key(|entry| std::cmp::Reverse(entry.path.matches('/').count()));
    for entry in directories {
        workspace.set_directory_mode(&entry.path, entry.mode)?;
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetType {
    Directory,
    Other,
}

#[cfg(unix)]
struct WorkspaceRoot {
    root: std::os::fd::OwnedFd,
    directories: RefCell<BTreeMap<String, std::os::fd::OwnedFd>>,
}

#[cfg(unix)]
impl WorkspaceRoot {
    fn open(workspace: &Path) -> Result<Self> {
        use rustix::fs::{Mode, OFlags};

        let root = rustix::fs::open(
            workspace,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| {
            format!(
                "open workspace root {} without following symlinks",
                workspace.display()
            )
        })?;
        Ok(Self {
            root,
            directories: RefCell::new(BTreeMap::new()),
        })
    }

    fn from_pinned(workspace: &snapshot::PinnedWorkspace) -> Result<Self> {
        Ok(Self {
            root: workspace.duplicate_root()?,
            directories: RefCell::new(BTreeMap::new()),
        })
    }

    fn pin_existing_parents(&self, paths: &[String]) -> Result<()> {
        for relative in paths {
            snapshot::validate_relative_path(relative)?;
            let parent = relative
                .rsplit_once('/')
                .map(|(parent, _)| parent)
                .unwrap_or("");
            let _ = self.open_directory(parent, true)?;
        }
        Ok(())
    }

    fn open_directory(
        &self,
        relative: &str,
        allow_missing: bool,
    ) -> Result<Option<std::os::fd::OwnedFd>> {
        use rustix::fs::{Mode, OFlags};

        if relative.is_empty() {
            return rustix::io::dup(&self.root)
                .map(Some)
                .context("duplicate workspace root handle");
        }
        snapshot::validate_relative_path(relative)?;
        if let Some(directory) = self.directories.borrow().get(relative) {
            return rustix::io::dup(directory)
                .map(Some)
                .with_context(|| format!("duplicate pinned workspace directory {relative:?}"));
        }

        let mut directory =
            rustix::io::dup(&self.root).context("duplicate workspace root handle")?;
        let mut prefix = String::new();
        for component in relative.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);

            if let Some(cached) = self.directories.borrow().get(&prefix) {
                directory = rustix::io::dup(cached)
                    .with_context(|| format!("duplicate pinned workspace directory {prefix:?}"))?;
                continue;
            }

            match rustix::fs::openat(
                &directory,
                component,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(next) => {
                    self.directories.borrow_mut().insert(
                        prefix.clone(),
                        rustix::io::dup(&next)
                            .with_context(|| format!("pin workspace directory {prefix:?}"))?,
                    );
                    directory = next;
                }
                Err(error) if error == rustix::io::Errno::NOENT && allow_missing => {
                    return Ok(None);
                }
                Err(error) if error == rustix::io::Errno::NOENT => {
                    bail!(
                        "workspace parent for {:?} does not exist or was not authorized by the review",
                        relative
                    );
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "open workspace ancestor {:?} for {:?} without following symlinks",
                            component, relative
                        )
                    });
                }
            }
        }
        Ok(Some(directory))
    }

    fn cache_directory(&self, relative: &str, directory: &std::os::fd::OwnedFd) -> Result<()> {
        self.directories.borrow_mut().insert(
            relative.to_owned(),
            rustix::io::dup(directory)
                .with_context(|| format!("pin workspace directory {relative:?}"))?,
        );
        Ok(())
    }

    fn invalidate_directory_subtree(&self, relative: &str) {
        let prefix = format!("{relative}/");
        self.directories
            .borrow_mut()
            .retain(|path, _| path != relative && !path.starts_with(&prefix));
    }

    fn open_parent(
        &self,
        relative: &str,
        allow_missing: bool,
    ) -> Result<Option<(std::os::fd::OwnedFd, String)>> {
        snapshot::validate_relative_path(relative)?;
        let mut components: Vec<_> = relative.split('/').collect();
        let name = components
            .pop()
            .context("validated workspace path has no final component")?
            .to_owned();
        let parent = components.join("/");
        Ok(self
            .open_directory(&parent, allow_missing)?
            .map(|directory| (directory, name)))
    }

    fn target_type(
        &self,
        parent: &std::os::fd::OwnedFd,
        name: &str,
        relative: &str,
    ) -> Result<Option<TargetType>> {
        use rustix::fs::{AtFlags, FileType};

        match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(
                if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
                    TargetType::Directory
                } else {
                    TargetType::Other
                },
            )),
            Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
            Err(error) => Err(error).with_context(|| format!("inspect reviewed path {relative:?}")),
        }
    }

    fn remove_target(&self, relative: &str) -> Result<()> {
        use rustix::fs::AtFlags;

        let Some((parent, name)) = self.open_parent(relative, true)? else {
            return Ok(());
        };
        match self.target_type(&parent, &name, relative)? {
            Some(TargetType::Directory) => {
                rustix::fs::unlinkat(&parent, &name, AtFlags::REMOVEDIR).with_context(|| {
                    format!(
                        "remove reviewed directory {:?}; it contains content not authorized for removal",
                        relative
                    )
                })?;
            }
            Some(TargetType::Other) => {
                rustix::fs::unlinkat(&parent, &name, AtFlags::empty())
                    .with_context(|| format!("remove reviewed path {relative:?}"))?;
            }
            None => {}
        }
        self.invalidate_directory_subtree(relative);
        Ok(())
    }

    fn make_exact_directory_writable(&self, relative: &str) -> Result<()> {
        use rustix::fs::{Mode, OFlags};

        let Some((parent, name)) = self.open_parent(relative, true)? else {
            return Ok(());
        };
        if self.target_type(&parent, &name, relative)? != Some(TargetType::Directory) {
            return Ok(());
        }
        let directory = rustix::fs::openat(
            &parent,
            &name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("open reviewed directory {relative:?}"))?;
        let stat = rustix::fs::fstat(&directory)
            .with_context(|| format!("inspect reviewed directory {relative:?}"))?;
        rustix::fs::fchmod(&directory, Mode::from_raw_mode(stat.st_mode) | Mode::RWXU)
            .with_context(|| format!("make reviewed directory {relative:?} writable"))?;
        self.cache_directory(relative, &directory)?;
        Ok(())
    }

    fn ensure_directory(&self, relative: &str, mode: u32) -> Result<()> {
        use rustix::fs::AtFlags;

        let (parent, name) = self
            .open_parent(relative, false)?
            .context("required workspace parent unexpectedly missing")?;
        match self.target_type(&parent, &name, relative)? {
            Some(TargetType::Directory) => {}
            Some(TargetType::Other) => {
                rustix::fs::unlinkat(&parent, &name, AtFlags::empty())
                    .with_context(|| format!("replace {relative:?} with a directory"))?;
                rustix::fs::mkdirat(&parent, &name, unix_mode(mode))
                    .with_context(|| format!("create directory {relative:?}"))?;
            }
            None => {
                rustix::fs::mkdirat(&parent, &name, unix_mode(mode))
                    .with_context(|| format!("create directory {relative:?}"))?;
            }
        }
        let directory = rustix::fs::openat(
            &parent,
            &name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .with_context(|| format!("pin reviewed directory {relative:?}"))?;
        self.cache_directory(relative, &directory)?;
        Ok(())
    }

    fn create_file(&self, relative: &str, mode: u32) -> Result<std::os::fd::OwnedFd> {
        let (parent, name) = self
            .open_parent(relative, false)?
            .context("required workspace parent unexpectedly missing")?;
        self.invalidate_directory_subtree(relative);
        create_file_in_parent(&parent, &name, mode, relative)
    }

    fn create_symlink(&self, relative: &str, target: &str) -> Result<()> {
        let (parent, name) = self
            .open_parent(relative, false)?
            .context("required workspace parent unexpectedly missing")?;
        self.invalidate_directory_subtree(relative);
        rustix::fs::symlinkat(target, &parent, &name)
            .with_context(|| format!("create reviewed symlink {relative:?}"))
    }

    fn set_directory_mode(&self, relative: &str, mode: u32) -> Result<()> {
        let directory = self
            .open_directory(relative, false)?
            .context("required workspace directory unexpectedly missing")?;
        rustix::fs::fchmod(&directory, unix_mode(mode))
            .with_context(|| format!("set reviewed directory mode for {relative:?}"))
    }
}

#[cfg(unix)]
fn unix_mode(mode: u32) -> rustix::fs::Mode {
    rustix::fs::Mode::from_raw_mode(mode as _)
}

#[cfg(unix)]
fn create_file_in_parent(
    parent: &std::os::fd::OwnedFd,
    name: &str,
    mode: u32,
    relative: &str,
) -> Result<std::os::fd::OwnedFd> {
    use rustix::fs::OFlags;

    rustix::fs::openat(
        parent,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        unix_mode(mode),
    )
    .with_context(|| format!("create reviewed file {relative:?}"))
}

#[cfg(unix)]
fn rollback_error<T>(
    store: &Store,
    before: &Manifest,
    workspace: &snapshot::PinnedWorkspace,
    workspace_root: &WorkspaceRoot,
    transaction: &mut WorkspaceTransaction,
    operations: &[WorkspaceOperation],
    cause: &str,
) -> Result<T> {
    let rollback =
        apply_manifest_state_on_root(store, before, workspace_root, operation_paths(operations))
            .and_then(|()| {
                snapshot::create_from_pinned(workspace, store, snapshot::CaptureMode::All)
                    .map(|result| result.manifest)
            })
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
        Ok(()) => {
            transaction.complete()?;
            bail!("{cause}; AgentLab restored the reviewed workspace paths from the backup")
        }
        Err(error) => bail!(
            "{cause}; automatic rollback failed: {error:#}; recover from backup snapshot {}; the workspace transaction marker remains at {}",
            before.digest,
            transaction.path.display()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
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

    #[cfg(unix)]
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

        let pinned = snapshot::PinnedWorkspace::open(&workspace).unwrap();
        let root = WorkspaceRoot::from_pinned(&pinned).unwrap();
        let apply_error =
            apply_manifest_state_on_root(&store, &desired, &root, operation_paths(&operations))
                .unwrap_err();
        assert!(format!("{apply_error:#}").contains("not authorized for removal"));
        let transaction_path = temporary.path().join("workspace-transaction.json");
        fs::write(&transaction_path, b"{}\n").unwrap();
        let mut transaction = WorkspaceTransaction {
            path: transaction_path.clone(),
            active: true,
        };
        let rollback = rollback_error::<()>(
            &store,
            &before,
            &pinned,
            &root,
            &mut transaction,
            &operations,
            "fixture apply failed",
        )
        .unwrap_err();
        assert!(format!("{rollback:#}").contains("restored"));
        assert!(!transaction_path.exists());
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

    #[cfg(unix)]
    #[test]
    fn pinned_parent_handle_does_not_follow_a_racing_symlink() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let outside = temporary.path().join("outside");
        let original_parent = workspace.join("original-parent");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::create_dir(workspace.join("parent")).unwrap();

        let root = WorkspaceRoot::open(&workspace).unwrap();
        let (parent, name) = root
            .open_parent("parent/result.txt", false)
            .unwrap()
            .unwrap();

        fs::rename(workspace.join("parent"), &original_parent).unwrap();
        symlink(&outside, workspace.join("parent")).unwrap();

        let mut output =
            File::from(create_file_in_parent(&parent, &name, 0o644, "parent/result.txt").unwrap());
        output.write_all(b"reviewed\n").unwrap();
        output.sync_all().unwrap();

        assert!(!outside.join("result.txt").exists());
        assert_eq!(
            fs::read_to_string(original_parent.join("result.txt")).unwrap(),
            "reviewed\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pinned_root_never_switches_to_a_replacement_workspace() {
        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        let original = temporary.path().join("original-workspace");
        let candidate = temporary.path().join("candidate");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&candidate).unwrap();
        fs::write(workspace.join("value.txt"), "original\n").unwrap();
        fs::write(candidate.join("value.txt"), "reviewed\n").unwrap();
        let store = Store::open(Some(&state)).unwrap();
        let desired = snapshot::create(&candidate, &store).unwrap().manifest;
        let pinned = snapshot::PinnedWorkspace::open(&workspace).unwrap();
        let root = WorkspaceRoot::from_pinned(&pinned).unwrap();

        fs::rename(&workspace, &original).unwrap();
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("value.txt"), "replacement\n").unwrap();

        apply_manifest_state_on_root(&store, &desired, &root, vec!["value.txt".to_owned()])
            .unwrap();

        assert_eq!(
            fs::read_to_string(original.join("value.txt")).unwrap(),
            "reviewed\n"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("value.txt")).unwrap(),
            "replacement\n"
        );
        assert!(
            snapshot::create_from_pinned(&pinned, &store, snapshot::CaptureMode::All)
                .unwrap_err()
                .to_string()
                .contains("renamed or replaced")
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_apply_lock_survives_workspace_rename() {
        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        let renamed = temporary.path().join("renamed-workspace");
        fs::create_dir(&workspace).unwrap();
        let store = Store::open(Some(&state)).unwrap();
        let first_workspace = snapshot::PinnedWorkspace::open(&workspace).unwrap();
        let first_lock = acquire_workspace_lock(&store, &first_workspace).unwrap();

        fs::rename(&workspace, &renamed).unwrap();
        let same_workspace = snapshot::PinnedWorkspace::open(&renamed).unwrap();
        let error = acquire_workspace_lock(&store, &same_workspace)
            .err()
            .expect("second lock must be rejected");
        assert!(error.to_string().contains("already in progress"));

        drop(first_lock);
        let recovered_lock = acquire_workspace_lock(&store, &same_workspace).unwrap();
        fs::write(&recovered_lock.recovery_path, b"interrupted fixture\n").unwrap();
        let recovery_path = recovered_lock.recovery_path.clone();
        drop(recovered_lock);

        let error = acquire_workspace_lock(&store, &same_workspace)
            .err()
            .expect("interrupted transaction must block another apply");
        assert!(error.to_string().contains("interrupted AgentLab apply"));
        assert!(
            error
                .to_string()
                .contains(&recovery_path.display().to_string())
        );
    }
}
