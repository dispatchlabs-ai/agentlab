use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::evaluation;
use crate::lifecycle;
use crate::rootfs::{ChangeKind, RootFsEntry, RootFsManifest};
use crate::run::{self, Artifact, DeltaManifest};
use crate::snapshot::{self, Entry, Manifest, Repository};
use crate::store::{Store, create_new_file, normalize_digest};

pub const ADOPTION_REQUEST_SCHEMA_VERSION: &str = "agentlab.adoption-request/v1";
pub const ADOPTION_PROPOSAL_SCHEMA_VERSION: &str = "agentlab.adoption-proposal/v1";
pub const ADOPTION_REVIEW_SCHEMA_VERSION: &str = "agentlab.adoption-review/v1";

#[derive(Debug, Clone)]
pub struct ReviewOptions {
    pub run_id: String,
    pub workspace: PathBuf,
    pub reviewer_command: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdoptionAnchors {
    pub run_id: String,
    pub result_digest: String,
    pub run_input_digest: String,
    pub base_workspace_snapshot_digest: String,
    pub candidate_workspace_snapshot_digest: String,
    pub current_workspace_snapshot_digest: String,
    pub base_filesystem_digest: String,
    pub candidate_filesystem_digest: String,
    pub portable_delta_digest: String,
    pub raw_delta_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdoptionRepositories {
    pub base: Vec<Repository>,
    pub candidate: Vec<Repository>,
    pub current: Vec<Repository>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdoptionCandidate {
    pub path: String,
    pub change: ChangeKind,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    pub current_relation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdoptionRequest {
    pub schema_version: String,
    pub review_id: String,
    pub anchors: AdoptionAnchors,
    pub workspace_guest_path: String,
    pub reviewer_command: Vec<String>,
    pub input_artifacts: BTreeMap<String, String>,
    pub repositories: AdoptionRepositories,
    pub candidates: Vec<AdoptionCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceOperation {
    pub operation: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdoptionDisposition {
    pub path: String,
    pub disposition: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_operation: Option<WorkspaceOperation>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DispositionCounts {
    pub proposed: usize,
    pub rejected: usize,
    pub conflicted: usize,
    pub unresolved: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdoptionProposal {
    pub schema_version: String,
    pub review_id: String,
    pub anchors: AdoptionAnchors,
    pub counts: DispositionCounts,
    pub dispositions: Vec<AdoptionDisposition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdoptionReviewRecord {
    pub schema_version: String,
    pub digest: String,
    pub review_id: String,
    pub run_id: String,
    pub result_digest: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub reviewer_exit_code: i64,
    pub request: AdoptionRequest,
    pub proposal: AdoptionProposal,
    pub request_artifact: Artifact,
    pub proposal_artifact: Artifact,
    pub stdout: Artifact,
    pub stderr: Artifact,
    pub source_workspace_unchanged: bool,
    pub agentlab_applied_changes: bool,
    pub warnings: Vec<String>,
    pub integrity: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct AdoptionReviewIdentity<'a> {
    schema_version: &'a str,
    review_id: &'a str,
    run_id: &'a str,
    result_digest: &'a str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    reviewer_exit_code: i64,
    request: &'a AdoptionRequest,
    proposal: &'a AdoptionProposal,
    request_artifact: &'a Artifact,
    proposal_artifact: &'a Artifact,
    stdout: &'a Artifact,
    stderr: &'a Artifact,
    source_workspace_unchanged: bool,
    agentlab_applied_changes: bool,
    warnings: &'a [String],
    integrity: &'a BTreeMap<String, String>,
}

pub fn review(store: &Store, options: &ReviewOptions) -> Result<AdoptionReviewRecord> {
    if options.reviewer_command.is_empty() {
        bail!("adopt review requires a reviewer command after --");
    }
    lifecycle::verify_all(store, &options.run_id)?;
    evaluation::verify_all(store, &options.run_id)?;
    verify_all(store, &options.run_id)?;

    let result = run::load_result(store, &options.run_id)?;
    let spec = run::load_spec(store, &options.run_id)?;
    let portable_delta = run::load_delta(store, &options.run_id, false)?;
    let raw_delta = run::load_delta(store, &options.run_id, true)?;
    let base_workspace = snapshot::load(store, &spec.workspace_snapshot_digest)?;
    snapshot::verify(store, &base_workspace)?;
    let candidate_rootfs: RootFsManifest =
        serde_json::from_slice(&store.read_run_file(&options.run_id, "result-rootfs.json")?)
            .context("decode candidate root filesystem manifest")?;
    if candidate_rootfs.digest != result.result_filesystem_digest {
        bail!("candidate root filesystem does not match selected run result");
    }

    let current_before = snapshot::create(&options.workspace, store)?.manifest;
    let review_id = Uuid::new_v4().to_string();
    let bundle = tempfile::tempdir().context("create private adoption review bundle")?;
    let base_directory = bundle.path().join("base");
    let candidate_directory = bundle.path().join("candidate");
    let current_directory = bundle.path().join("current");
    let machine_changes_directory = bundle.path().join("machine-changes");
    snapshot::materialize(store, &base_workspace, &base_directory)?;
    materialize_candidate_workspace(
        store,
        &candidate_rootfs,
        &spec.workspace_guest_path,
        &candidate_directory,
    )?;
    let candidate_workspace = snapshot::create(&candidate_directory, store)?.manifest;
    snapshot::materialize(store, &current_before, &current_directory)?;

    let anchors = AdoptionAnchors {
        run_id: options.run_id.clone(),
        result_digest: result.digest.clone(),
        run_input_digest: run::compute_run_input_digest(&spec)?,
        base_workspace_snapshot_digest: base_workspace.digest.clone(),
        candidate_workspace_snapshot_digest: candidate_workspace.digest.clone(),
        current_workspace_snapshot_digest: current_before.digest.clone(),
        base_filesystem_digest: result.base_filesystem_digest.clone(),
        candidate_filesystem_digest: result.result_filesystem_digest.clone(),
        portable_delta_digest: portable_delta.digest.clone(),
        raw_delta_digest: raw_delta.digest.clone(),
    };
    let input_artifacts = write_bundle_inputs(
        store,
        &options.run_id,
        bundle.path(),
        &base_workspace,
        &candidate_workspace,
        &current_before,
    )?;
    let candidates = adoption_candidates(
        &raw_delta,
        &spec.workspace_guest_path,
        &base_workspace,
        &current_before,
    )?;
    materialize_machine_changes(store, &raw_delta, &machine_changes_directory)?;
    let request = AdoptionRequest {
        schema_version: ADOPTION_REQUEST_SCHEMA_VERSION.to_owned(),
        review_id: review_id.clone(),
        anchors: anchors.clone(),
        workspace_guest_path: spec.workspace_guest_path.clone(),
        reviewer_command: options.reviewer_command.clone(),
        input_artifacts,
        repositories: AdoptionRepositories {
            base: base_workspace.repositories.clone(),
            candidate: candidate_workspace.repositories.clone(),
            current: current_before.repositories.clone(),
        },
        candidates,
    };
    let request_bytes = run::pretty_json(&request)?;
    fs::write(bundle.path().join("request.json"), &request_bytes)
        .context("write adoption request into review bundle")?;

    let started_at = Utc::now();
    let output = Command::new(&options.reviewer_command[0])
        .args(&options.reviewer_command[1..])
        .current_dir(&current_directory)
        .env("AGENTLAB_RUN_ID", &options.run_id)
        .env("AGENTLAB_ADOPTION_REVIEW_ID", &review_id)
        .env("AGENTLAB_ADOPTION_BUNDLE_DIR", bundle.path())
        .env(
            "AGENTLAB_ADOPTION_REQUEST_PATH",
            bundle.path().join("request.json"),
        )
        .env(
            "AGENTLAB_ADOPTION_BASE_MANIFEST_PATH",
            bundle.path().join("base-manifest.json"),
        )
        .env(
            "AGENTLAB_ADOPTION_CANDIDATE_MANIFEST_PATH",
            bundle.path().join("candidate-manifest.json"),
        )
        .env(
            "AGENTLAB_ADOPTION_CURRENT_MANIFEST_PATH",
            bundle.path().join("current-manifest.json"),
        )
        .env(
            "AGENTLAB_ADOPTION_DELTA_PATH",
            bundle.path().join("delta.json"),
        )
        .env(
            "AGENTLAB_ADOPTION_RAW_DELTA_PATH",
            bundle.path().join("delta.raw.json"),
        )
        .env(
            "AGENTLAB_ADOPTION_RUN_SPEC_PATH",
            bundle.path().join("spec.json"),
        )
        .env(
            "AGENTLAB_ADOPTION_RUN_RESULT_PATH",
            bundle.path().join("result.json"),
        )
        .env(
            "AGENTLAB_ADOPTION_BASE_ROOTFS_MANIFEST_PATH",
            bundle.path().join("base-rootfs.json"),
        )
        .env(
            "AGENTLAB_ADOPTION_CANDIDATE_ROOTFS_MANIFEST_PATH",
            bundle.path().join("candidate-rootfs.json"),
        )
        .env("AGENTLAB_ADOPTION_BASE_DIR", &base_directory)
        .env("AGENTLAB_ADOPTION_CANDIDATE_DIR", &candidate_directory)
        .env("AGENTLAB_ADOPTION_CURRENT_DIR", &current_directory)
        .env(
            "AGENTLAB_ADOPTION_MACHINE_CHANGES_DIR",
            &machine_changes_directory,
        )
        .output()
        .with_context(|| {
            format!(
                "execute adoption reviewer command {:?}",
                options.reviewer_command[0]
            )
        })?;
    let completed_at = Utc::now();

    verify_bundle_inputs(bundle.path(), &request, &request_bytes)?;
    lifecycle::verify_all(store, &options.run_id)
        .context("adoption reviewer mutated immutable run or lifecycle artifacts")?;
    evaluation::verify_all(store, &options.run_id)
        .context("adoption reviewer mutated evaluation artifacts")?;
    verify_all(store, &options.run_id)
        .context("adoption reviewer mutated prior adoption records")?;
    snapshot::verify(store, &base_workspace)
        .context("adoption reviewer mutated base workspace snapshot content")?;
    snapshot::verify(store, &candidate_workspace)
        .context("adoption reviewer mutated candidate workspace snapshot content")?;
    snapshot::verify(store, &current_before)
        .context("adoption reviewer mutated current workspace snapshot content")?;
    let current_after = snapshot::create(&options.workspace, store)?.manifest;
    if current_after.digest != current_before.digest {
        bail!(
            "adoption reviewer changed the selected current workspace; review receipt was not accepted"
        );
    }
    let exit_code = output.status.code().map(i64::from).unwrap_or(-1);
    if !output.status.success() {
        bail!(
            "adoption reviewer exited with status {exit_code}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let proposal: AdoptionProposal = serde_json::from_slice(&output.stdout)
        .context("adoption reviewer stdout was not a valid proposal JSON object")?;
    validate_proposal(&request, &proposal)?;

    let prefix = format!("adoptions/{review_id}");
    let request_artifact = write_artifact(
        store,
        &options.run_id,
        &format!("{prefix}/request.json"),
        &request_bytes,
    )?;
    let proposal_bytes = run::pretty_json(&proposal)?;
    let proposal_artifact = write_artifact(
        store,
        &options.run_id,
        &format!("{prefix}/proposal.json"),
        &proposal_bytes,
    )?;
    let stdout = write_artifact(
        store,
        &options.run_id,
        &format!("{prefix}/artifacts/stdout.bin"),
        &output.stdout,
    )?;
    let stderr = write_artifact(
        store,
        &options.run_id,
        &format!("{prefix}/artifacts/stderr.bin"),
        &output.stderr,
    )?;
    let mut integrity = BTreeMap::new();
    for artifact in [&request_artifact, &proposal_artifact, &stdout, &stderr] {
        integrity.insert(artifact.path.clone(), artifact.digest.clone());
    }
    let warnings = vec![
        "the reviewer ran as a trusted host process with the invoking user's authority".to_owned(),
        "review mode recorded a proposal only; AgentLab applied no changes".to_owned(),
        "captured workspaces, machine deltas, reviewer output, and receipts may contain sensitive information"
            .to_owned(),
    ];
    let identity = AdoptionReviewIdentity {
        schema_version: ADOPTION_REVIEW_SCHEMA_VERSION,
        review_id: &review_id,
        run_id: &options.run_id,
        result_digest: &result.digest,
        started_at,
        completed_at,
        reviewer_exit_code: exit_code,
        request: &request,
        proposal: &proposal,
        request_artifact: &request_artifact,
        proposal_artifact: &proposal_artifact,
        stdout: &stdout,
        stderr: &stderr,
        source_workspace_unchanged: true,
        agentlab_applied_changes: false,
        warnings: &warnings,
        integrity: &integrity,
    };
    let record = AdoptionReviewRecord {
        schema_version: ADOPTION_REVIEW_SCHEMA_VERSION.to_owned(),
        digest: run::sha256_bytes(&serde_json::to_vec(&identity)?),
        review_id,
        run_id: options.run_id.clone(),
        result_digest: result.digest,
        started_at,
        completed_at,
        reviewer_exit_code: exit_code,
        request,
        proposal,
        request_artifact,
        proposal_artifact,
        stdout,
        stderr,
        source_workspace_unchanged: true,
        agentlab_applied_changes: false,
        warnings,
        integrity,
    };
    store.write_run_file(
        &options.run_id,
        &format!("{prefix}/review.json"),
        &run::pretty_json(&record)?,
    )?;
    verify(store, &record)?;
    Ok(record)
}

pub fn list(store: &Store, run_id: &str) -> Result<Vec<AdoptionReviewRecord>> {
    let directory = store.run_path(run_id, "adoptions")?;
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut records: Vec<AdoptionReviewRecord> = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("adoption review ID is not valid UTF-8"))?;
        let relative = format!("adoptions/{id}/review.json");
        if store.run_file_exists(run_id, &relative)? {
            records.push(serde_json::from_slice(
                &store.read_run_file(run_id, &relative)?,
            )?);
        }
    }
    records.sort_by(|left, right| {
        left.completed_at
            .cmp(&right.completed_at)
            .then_with(|| left.review_id.cmp(&right.review_id))
    });
    Ok(records)
}

pub fn verify_all(store: &Store, run_id: &str) -> Result<()> {
    for record in list(store, run_id)? {
        verify(store, &record)?;
    }
    Ok(())
}

pub fn verify(store: &Store, record: &AdoptionReviewRecord) -> Result<()> {
    if record.schema_version != ADOPTION_REVIEW_SCHEMA_VERSION {
        bail!(
            "unsupported adoption review schema {:?}",
            record.schema_version
        );
    }
    if record.review_id != record.request.review_id
        || record.run_id != record.request.anchors.run_id
        || record.result_digest != record.request.anchors.result_digest
        || record.proposal.review_id != record.review_id
    {
        bail!("adoption review record fields do not agree with request/proposal anchors");
    }
    for (relative, expected) in &record.integrity {
        let actual = run::sha256_bytes(&store.read_run_file(&record.run_id, relative)?);
        if &actual != expected {
            bail!("adoption review artifact integrity mismatch for {relative:?}");
        }
    }
    let stored_request: AdoptionRequest = serde_json::from_slice(
        &store.read_run_file(&record.run_id, &record.request_artifact.path)?,
    )?;
    let stored_proposal: AdoptionProposal = serde_json::from_slice(
        &store.read_run_file(&record.run_id, &record.proposal_artifact.path)?,
    )?;
    if stored_request != record.request || stored_proposal != record.proposal {
        bail!("adoption review record and stored request/proposal do not agree");
    }
    validate_proposal(&record.request, &record.proposal)?;
    let identity = AdoptionReviewIdentity {
        schema_version: ADOPTION_REVIEW_SCHEMA_VERSION,
        review_id: &record.review_id,
        run_id: &record.run_id,
        result_digest: &record.result_digest,
        started_at: record.started_at,
        completed_at: record.completed_at,
        reviewer_exit_code: record.reviewer_exit_code,
        request: &record.request,
        proposal: &record.proposal,
        request_artifact: &record.request_artifact,
        proposal_artifact: &record.proposal_artifact,
        stdout: &record.stdout,
        stderr: &record.stderr,
        source_workspace_unchanged: record.source_workspace_unchanged,
        agentlab_applied_changes: record.agentlab_applied_changes,
        warnings: &record.warnings,
        integrity: &record.integrity,
    };
    if run::sha256_bytes(&serde_json::to_vec(&identity)?) != record.digest {
        bail!("adoption review record integrity mismatch");
    }
    Ok(())
}

fn validate_proposal(request: &AdoptionRequest, proposal: &AdoptionProposal) -> Result<()> {
    validate_request(request)?;
    if proposal.schema_version != ADOPTION_PROPOSAL_SCHEMA_VERSION {
        bail!(
            "unsupported adoption proposal schema {:?}",
            proposal.schema_version
        );
    }
    if proposal.review_id != request.review_id {
        bail!("adoption proposal review ID does not match request");
    }
    if proposal.anchors != request.anchors {
        bail!("adoption proposal anchors do not match request");
    }
    let candidates: BTreeMap<_, _> = request
        .candidates
        .iter()
        .map(|candidate| (candidate.path.as_str(), candidate))
        .collect();
    let mut seen = BTreeSet::new();
    let mut counts = DispositionCounts::default();
    for disposition in &proposal.dispositions {
        if !seen.insert(disposition.path.as_str()) {
            bail!("duplicate adoption disposition for {:?}", disposition.path);
        }
        let candidate = candidates.get(disposition.path.as_str()).with_context(|| {
            format!(
                "adoption disposition references non-candidate path {:?}",
                disposition.path
            )
        })?;
        if disposition.reason.trim().is_empty() {
            bail!(
                "adoption disposition {:?} requires a reason",
                disposition.path
            );
        }
        match disposition.disposition.as_str() {
            "proposed" => counts.proposed += 1,
            "rejected" => counts.rejected += 1,
            "conflicted" => counts.conflicted += 1,
            "unresolved" => counts.unresolved += 1,
            value => bail!("invalid adoption disposition {value:?}"),
        }
        if disposition.disposition != "proposed" && disposition.workspace_operation.is_some() {
            bail!(
                "only a proposed disposition may contain a workspace operation at {:?}",
                disposition.path
            );
        }
        if let Some(operation) = &disposition.workspace_operation {
            let workspace_path = candidate.workspace_path.as_deref().with_context(|| {
                format!(
                    "environment candidate {:?} cannot contain a workspace operation",
                    disposition.path
                )
            })?;
            validate_operation_path(&operation.path)?;
            if operation.path != workspace_path || workspace_path == "." {
                bail!(
                    "workspace operation path {:?} does not match candidate {:?}",
                    operation.path,
                    workspace_path
                );
            }
            let expected = if candidate.change == ChangeKind::Deleted {
                "delete"
            } else {
                "replace"
            };
            if operation.operation != expected {
                bail!(
                    "workspace operation for {:?} must be {expected:?}",
                    disposition.path
                );
            }
        }
        if disposition.disposition == "proposed"
            && candidate.scope == "environment"
            && disposition
                .recommendation
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        {
            bail!(
                "proposed environment candidate {:?} requires a declarative recommendation",
                disposition.path
            );
        }
    }
    if seen.len() != candidates.len() {
        let missing: Vec<_> = candidates
            .keys()
            .filter(|path| !seen.contains(**path))
            .copied()
            .collect();
        bail!("adoption proposal omitted candidate paths: {missing:?}");
    }
    if proposal.counts != counts {
        bail!("adoption proposal disposition counts are inconsistent");
    }
    Ok(())
}

fn validate_request(request: &AdoptionRequest) -> Result<()> {
    if request.schema_version != ADOPTION_REQUEST_SCHEMA_VERSION {
        bail!(
            "unsupported adoption request schema {:?}",
            request.schema_version
        );
    }
    Uuid::parse_str(&request.review_id).context("adoption request review ID is not a UUID")?;
    if request.reviewer_command.is_empty() {
        bail!("adoption request reviewer command is empty");
    }
    for digest in [
        &request.anchors.result_digest,
        &request.anchors.run_input_digest,
        &request.anchors.base_workspace_snapshot_digest,
        &request.anchors.candidate_workspace_snapshot_digest,
        &request.anchors.current_workspace_snapshot_digest,
        &request.anchors.base_filesystem_digest,
        &request.anchors.candidate_filesystem_digest,
        &request.anchors.portable_delta_digest,
        &request.anchors.raw_delta_digest,
    ] {
        normalize_digest(digest).context("adoption request contains an invalid anchor digest")?;
    }
    for digest in request.input_artifacts.values() {
        normalize_digest(digest)
            .context("adoption request contains an invalid input-artifact digest")?;
    }
    let mut paths = BTreeSet::new();
    for candidate in &request.candidates {
        if !paths.insert(candidate.path.as_str()) {
            bail!("duplicate adoption candidate path {:?}", candidate.path);
        }
        let relative = candidate
            .path
            .strip_prefix('/')
            .context("adoption candidate path must be absolute")?;
        snapshot::validate_relative_path(relative)?;
        let derived_workspace_path =
            workspace_relative(&candidate.path, &request.workspace_guest_path)?;
        match candidate.scope.as_str() {
            "workspace" if candidate.workspace_path == derived_workspace_path => {}
            "environment"
                if candidate.workspace_path.is_none() && derived_workspace_path.is_none() => {}
            _ => bail!(
                "adoption candidate {:?} has inconsistent scope or workspace path",
                candidate.path
            ),
        }
        if !matches!(
            candidate.current_relation.as_str(),
            "unchanged_from_base"
                | "already_matches_candidate"
                | "changed_since_base"
                | "not_applicable"
        ) {
            bail!(
                "adoption candidate {:?} has invalid current relation",
                candidate.path
            );
        }
        if candidate.scope == "environment" && candidate.current_relation != "not_applicable" {
            bail!(
                "environment adoption candidate {:?} must use not_applicable current relation",
                candidate.path
            );
        }
    }
    Ok(())
}

fn write_bundle_inputs(
    store: &Store,
    run_id: &str,
    bundle: &Path,
    base: &Manifest,
    candidate: &Manifest,
    current: &Manifest,
) -> Result<BTreeMap<String, String>> {
    let mut artifacts = BTreeMap::new();
    for (name, relative, filename) in [
        ("run_spec", "spec.json", "spec.json"),
        ("run_result", "result.json", "result.json"),
        (
            "base_rootfs_manifest",
            "base-rootfs.json",
            "base-rootfs.json",
        ),
        (
            "candidate_rootfs_manifest",
            "result-rootfs.json",
            "candidate-rootfs.json",
        ),
    ] {
        let bytes = store.read_run_file(run_id, relative)?;
        fs::write(bundle.join(filename), &bytes)
            .with_context(|| format!("write adoption bundle input {filename:?}"))?;
        artifacts.insert(name.to_owned(), run::sha256_bytes(&bytes));
    }
    for (name, filename, bytes) in [
        (
            "base_workspace_manifest",
            "base-manifest.json",
            store.read_snapshot(&base.digest)?,
        ),
        (
            "candidate_workspace_manifest",
            "candidate-manifest.json",
            store.read_snapshot(&candidate.digest)?,
        ),
        (
            "current_workspace_manifest",
            "current-manifest.json",
            store.read_snapshot(&current.digest)?,
        ),
        (
            "portable_delta",
            "delta.json",
            store.read_run_file(run_id, "delta.json")?,
        ),
        (
            "raw_delta",
            "delta.raw.json",
            store.read_run_file(run_id, "delta.raw.json")?,
        ),
    ] {
        fs::write(bundle.join(filename), &bytes)
            .with_context(|| format!("write adoption bundle input {filename:?}"))?;
        artifacts.insert(name.to_owned(), run::sha256_bytes(&bytes));
    }
    Ok(artifacts)
}

fn verify_bundle_inputs(
    bundle: &Path,
    request: &AdoptionRequest,
    request_bytes: &[u8],
) -> Result<()> {
    let mappings = [
        ("run_spec", "spec.json"),
        ("run_result", "result.json"),
        ("base_rootfs_manifest", "base-rootfs.json"),
        ("candidate_rootfs_manifest", "candidate-rootfs.json"),
        ("base_workspace_manifest", "base-manifest.json"),
        ("candidate_workspace_manifest", "candidate-manifest.json"),
        ("current_workspace_manifest", "current-manifest.json"),
        ("portable_delta", "delta.json"),
        ("raw_delta", "delta.raw.json"),
    ];
    for (name, filename) in mappings {
        let expected = request
            .input_artifacts
            .get(name)
            .with_context(|| format!("adoption request omitted input artifact {name:?}"))?;
        let actual = run::sha256_bytes(
            &fs::read(bundle.join(filename))
                .with_context(|| format!("re-read adoption bundle input {filename:?}"))?,
        );
        if &actual != expected {
            bail!("adoption reviewer mutated bundle input {filename:?}");
        }
    }
    let actual_request = fs::read(bundle.join("request.json"))?;
    if actual_request != request_bytes {
        bail!("adoption reviewer mutated request.json");
    }
    Ok(())
}

fn adoption_candidates(
    raw_delta: &DeltaManifest,
    workspace_guest_path: &str,
    base: &Manifest,
    current: &Manifest,
) -> Result<Vec<AdoptionCandidate>> {
    let mut candidates = Vec::with_capacity(raw_delta.changes.len());
    for change in &raw_delta.changes {
        let workspace_path = workspace_relative(&change.path, workspace_guest_path)?;
        let (scope, current_relation) = match &workspace_path {
            Some(path) => (
                "workspace".to_owned(),
                current_relation(path, change.after.as_ref(), base, current),
            ),
            None => ("environment".to_owned(), "not_applicable".to_owned()),
        };
        candidates.push(AdoptionCandidate {
            path: change.path.clone(),
            change: change.change.clone(),
            scope,
            workspace_path,
            current_relation,
        });
    }
    Ok(candidates)
}

fn current_relation(
    workspace_path: &str,
    candidate_after: Option<&RootFsEntry>,
    base: &Manifest,
    current: &Manifest,
) -> String {
    let base_entry = base
        .entries
        .iter()
        .find(|entry| entry.path == workspace_path);
    let current_entry = current
        .entries
        .iter()
        .find(|entry| entry.path == workspace_path);
    if snapshot_matches_rootfs(current_entry, candidate_after) {
        "already_matches_candidate".to_owned()
    } else if base_entry == current_entry {
        "unchanged_from_base".to_owned()
    } else {
        "changed_since_base".to_owned()
    }
}

fn snapshot_matches_rootfs(snapshot: Option<&Entry>, rootfs: Option<&RootFsEntry>) -> bool {
    match (snapshot, rootfs) {
        (None, None) => true,
        (Some(snapshot), Some(rootfs)) => {
            snapshot.kind == rootfs.kind
                && snapshot.mode == rootfs.mode
                && snapshot.size == rootfs.size
                && snapshot.digest == rootfs.digest
                && snapshot.link_target == rootfs.link_target
        }
        _ => false,
    }
}

fn workspace_relative(path: &str, workspace_guest_path: &str) -> Result<Option<String>> {
    let prefix = normalized_guest_path(workspace_guest_path)?;
    if path == format!("/{prefix}") {
        return Ok(Some(".".to_owned()));
    }
    Ok(path.strip_prefix(&format!("/{prefix}/")).map(str::to_owned))
}

fn materialize_candidate_workspace(
    store: &Store,
    rootfs: &RootFsManifest,
    workspace_guest_path: &str,
    destination: &Path,
) -> Result<()> {
    fs::create_dir_all(destination)?;
    let prefix = normalized_guest_path(workspace_guest_path)?;
    let prefix_with_separator = format!("{prefix}/");
    let entries: Vec<_> = rootfs
        .entries
        .iter()
        .filter_map(|entry| {
            entry
                .path
                .strip_prefix(&prefix_with_separator)
                .map(|relative| (relative, entry))
        })
        .collect();
    for (relative, entry) in &entries {
        if entry.kind == "directory" {
            fs::create_dir_all(snapshot::safe_join(destination, relative)?)?;
        }
    }
    for (relative, entry) in &entries {
        let target = snapshot::safe_join(destination, relative)?;
        match entry.kind.as_str() {
            "directory" => {}
            "file" => {
                let mut source = store.open_blob(&entry.digest)?;
                let mut output: File = create_new_file(&target)?;
                io::copy(&mut source, &mut output)?;
                output.sync_all()?;
                set_mode(&target, entry.mode)?;
            }
            "symlink" => create_symlink(&entry.link_target, &target)?,
            value => bail!("unsupported candidate workspace entry type {value:?}"),
        }
    }
    let mut directories: Vec<_> = entries
        .iter()
        .filter(|(_, entry)| entry.kind == "directory")
        .collect();
    directories.sort_by_key(|(relative, _)| std::cmp::Reverse(relative.matches('/').count()));
    for (relative, entry) in directories {
        set_mode(&snapshot::safe_join(destination, relative)?, entry.mode)?;
    }
    Ok(())
}

fn materialize_machine_changes(
    store: &Store,
    raw_delta: &DeltaManifest,
    destination: &Path,
) -> Result<()> {
    fs::create_dir_all(destination)?;
    let mut after_entries: Vec<_> = raw_delta
        .changes
        .iter()
        .filter_map(|change| change.after.as_ref())
        .collect();
    after_entries.sort_by(|left, right| {
        let left_directory = left.kind == "directory";
        let right_directory = right.kind == "directory";
        right_directory
            .cmp(&left_directory)
            .then_with(|| {
                left.path
                    .matches('/')
                    .count()
                    .cmp(&right.path.matches('/').count())
            })
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut directory_modes = Vec::new();
    for entry in after_entries {
        let relative = entry.path.trim_start_matches('/');
        if relative.is_empty() {
            bail!("cannot materialize a machine change at the filesystem root");
        }
        let target = snapshot::safe_join(destination, relative)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        match entry.kind.as_str() {
            "directory" => {
                fs::create_dir_all(&target)?;
                directory_modes.push((target.clone(), entry.mode));
            }
            "file" => {
                let mut source = store.open_blob(&entry.digest)?;
                let mut output: File = create_new_file(&target)?;
                io::copy(&mut source, &mut output)?;
                output.sync_all()?;
                set_mode(&target, entry.mode)?;
            }
            "symlink" => create_symlink(&entry.link_target, &target)?,
            value => bail!("unsupported machine-change entry type {value:?}"),
        }
    }
    directory_modes.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (path, mode) in directory_modes {
        set_mode(&path, mode)?;
    }
    Ok(())
}

fn normalized_guest_path(path: &str) -> Result<String> {
    if !path.starts_with('/') {
        bail!("workspace guest path must be absolute");
    }
    let value = path.trim_matches('/');
    if value.is_empty()
        || value
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        bail!("workspace guest path is not safely materializable");
    }
    Ok(value.to_owned())
}

fn validate_operation_path(path: &str) -> Result<()> {
    if path.is_empty() || Path::new(path).is_absolute() {
        bail!("workspace operation path must be nonempty and relative");
    }
    if Path::new(path)
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("workspace operation path contains traversal");
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposal_validation_rejects_unanchored_incomplete_and_unsafe_output() {
        let request = fixture_request();
        let proposal = fixture_proposal(&request);
        validate_proposal(&request, &proposal).unwrap();

        let mut wrong_anchor = proposal.clone();
        wrong_anchor.anchors.result_digest = "sha256:wrong".to_owned();
        assert!(
            validate_proposal(&request, &wrong_anchor)
                .unwrap_err()
                .to_string()
                .contains("anchors")
        );

        let mut duplicate = proposal.clone();
        duplicate
            .dispositions
            .push(duplicate.dispositions[0].clone());
        duplicate.counts.proposed += 1;
        assert!(
            validate_proposal(&request, &duplicate)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );

        let mut omitted = proposal.clone();
        omitted.dispositions.pop();
        omitted.counts.unresolved -= 1;
        assert!(
            validate_proposal(&request, &omitted)
                .unwrap_err()
                .to_string()
                .contains("omitted")
        );

        let mut inconsistent = proposal.clone();
        inconsistent.counts.rejected = 1;
        assert!(
            validate_proposal(&request, &inconsistent)
                .unwrap_err()
                .to_string()
                .contains("counts")
        );

        let mut traversal = proposal.clone();
        traversal.dispositions[0]
            .workspace_operation
            .as_mut()
            .unwrap()
            .path = "../safe.txt".to_owned();
        assert!(
            validate_proposal(&request, &traversal)
                .unwrap_err()
                .to_string()
                .contains("traversal")
        );
    }

    fn fixture_request() -> AdoptionRequest {
        AdoptionRequest {
            schema_version: ADOPTION_REQUEST_SCHEMA_VERSION.to_owned(),
            review_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            anchors: fixture_anchors(),
            workspace_guest_path: "/workspace".to_owned(),
            reviewer_command: vec!["reviewer".to_owned()],
            input_artifacts: BTreeMap::new(),
            repositories: AdoptionRepositories {
                base: Vec::new(),
                candidate: Vec::new(),
                current: Vec::new(),
            },
            candidates: vec![
                AdoptionCandidate {
                    path: "/workspace/safe.txt".to_owned(),
                    change: ChangeKind::Added,
                    scope: "workspace".to_owned(),
                    workspace_path: Some("safe.txt".to_owned()),
                    current_relation: "unchanged_from_base".to_owned(),
                },
                AdoptionCandidate {
                    path: "/etc/example.conf".to_owned(),
                    change: ChangeKind::Added,
                    scope: "environment".to_owned(),
                    workspace_path: None,
                    current_relation: "not_applicable".to_owned(),
                },
            ],
        }
    }

    fn fixture_proposal(request: &AdoptionRequest) -> AdoptionProposal {
        AdoptionProposal {
            schema_version: ADOPTION_PROPOSAL_SCHEMA_VERSION.to_owned(),
            review_id: request.review_id.clone(),
            anchors: request.anchors.clone(),
            counts: DispositionCounts {
                proposed: 1,
                rejected: 0,
                conflicted: 0,
                unresolved: 1,
            },
            dispositions: vec![
                AdoptionDisposition {
                    path: "/workspace/safe.txt".to_owned(),
                    disposition: "proposed".to_owned(),
                    reason: "safe candidate".to_owned(),
                    recommendation: None,
                    workspace_operation: Some(WorkspaceOperation {
                        operation: "replace".to_owned(),
                        path: "safe.txt".to_owned(),
                    }),
                },
                AdoptionDisposition {
                    path: "/etc/example.conf".to_owned(),
                    disposition: "unresolved".to_owned(),
                    reason: "requires a declarative environment edit".to_owned(),
                    recommendation: None,
                    workspace_operation: None,
                },
            ],
            summary: None,
        }
    }

    fn fixture_anchors() -> AdoptionAnchors {
        AdoptionAnchors {
            run_id: "00000000-0000-4000-8000-000000000000".to_owned(),
            result_digest: fixture_digest('1'),
            run_input_digest: fixture_digest('2'),
            base_workspace_snapshot_digest: fixture_digest('3'),
            candidate_workspace_snapshot_digest: fixture_digest('4'),
            current_workspace_snapshot_digest: fixture_digest('5'),
            base_filesystem_digest: fixture_digest('6'),
            candidate_filesystem_digest: fixture_digest('7'),
            portable_delta_digest: fixture_digest('8'),
            raw_delta_digest: fixture_digest('9'),
        }
    }

    fn fixture_digest(value: char) -> String {
        format!("sha256:{}", value.to_string().repeat(64))
    }
}
