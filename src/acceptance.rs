use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::apply::{self, ApplyRecord};
use crate::evaluation;
use crate::lifecycle;
use crate::review;
use crate::run::{self, AcceptedInputReference, RunSpec};
use crate::snapshot;
use crate::store::Store;

pub const ACCEPTANCE_SCHEMA_VERSION: &str = "agentlab.acceptance/v1";
pub const ACCEPTED_INPUT_SCHEMA_VERSION: &str = "agentlab.accepted-input/v1";

#[derive(Debug, Clone)]
pub struct AcceptOptions {
    pub tested_by_run_id: String,
    pub from_apply_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceptedImage {
    pub requested: String,
    pub execution_reference: String,
    pub resolved_digest: String,
    pub docker_image_id: String,
    pub target_platform: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppliedLineage {
    pub candidate_run_id: String,
    pub candidate_result_digest: String,
    pub candidate_run_input_digest: String,
    pub review_id: String,
    pub review_digest: String,
    pub apply_id: String,
    pub apply_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceptanceRecord {
    pub schema_version: String,
    pub digest: String,
    pub acceptance_id: String,
    pub accepted_input_digest: String,
    pub accepted_at: DateTime<Utc>,
    pub kind: String,
    pub decision: String,
    pub workspace_snapshot_digest: String,
    pub workspace_ignore_digest: String,
    pub workspace_guest_path: String,
    pub image: AcceptedImage,
    pub tested_by_run_id: String,
    pub test_result_digest: String,
    pub test_run_input_digest: String,
    pub test_exit_code: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_accepted_input: Option<AcceptedInputReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_lineage: Option<AppliedLineage>,
    pub warnings: Vec<String>,
}

#[derive(Serialize)]
struct AcceptedInputIdentity<'a> {
    schema_version: &'static str,
    workspace_snapshot_digest: &'a str,
    workspace_ignore_digest: &'a str,
    workspace_guest_path: &'a str,
    image_resolved_digest: &'a str,
    target_platform: &'a str,
}

#[derive(Serialize)]
struct AcceptanceIdentity<'a> {
    schema_version: &'static str,
    acceptance_id: &'a str,
    accepted_input_digest: &'a str,
    accepted_at: DateTime<Utc>,
    kind: &'a str,
    decision: &'a str,
    workspace_snapshot_digest: &'a str,
    workspace_ignore_digest: &'a str,
    workspace_guest_path: &'a str,
    image: &'a AcceptedImage,
    tested_by_run_id: &'a str,
    test_result_digest: &'a str,
    test_run_input_digest: &'a str,
    test_exit_code: i64,
    parent_accepted_input: &'a Option<AcceptedInputReference>,
    applied_lineage: &'a Option<AppliedLineage>,
    warnings: &'a [String],
}

pub fn accept(store: &Store, options: &AcceptOptions) -> Result<AcceptanceRecord> {
    Uuid::parse_str(&options.tested_by_run_id).context("test run ID is not a UUID")?;
    let _lock = AcceptanceLock::acquire(store, &options.tested_by_run_id)?;
    if list(store)?
        .iter()
        .any(|record| record.tested_by_run_id == options.tested_by_run_id)
    {
        bail!(
            "run {:?} already has an explicit acceptance decision",
            options.tested_by_run_id
        );
    }

    verify_run_evidence(store, &options.tested_by_run_id)?;
    let test_result = run::load_result(store, &options.tested_by_run_id)?;
    let test_spec = run::load_spec(store, &options.tested_by_run_id)?;
    let workspace = snapshot::load(store, &test_spec.workspace_snapshot_digest)?;
    snapshot::verify(store, &workspace)?;
    let image = accepted_image(store, &test_spec)?;

    let (kind, parent_accepted_input, applied_lineage) = match options.from_apply_id.as_deref() {
        Some(apply_id) => {
            let applied = apply::find(store, apply_id)?;
            apply::verify(store, &applied)?;
            if applied.run_id == options.tested_by_run_id {
                bail!("the retest must be independent from the candidate run that was applied");
            }
            verify_run_evidence(store, &applied.run_id)?;
            let candidate_result = run::load_result(store, &applied.run_id)?;
            let candidate_spec = run::load_spec(store, &applied.run_id)?;
            verify_applied_test_input(&applied, &candidate_spec, &test_spec)?;
            (
                "reviewed_application".to_owned(),
                candidate_spec.accepted_input.clone(),
                Some(AppliedLineage {
                    candidate_run_id: applied.run_id.clone(),
                    candidate_result_digest: candidate_result.digest,
                    candidate_run_input_digest: candidate_spec.run_input_digest,
                    review_id: applied.review_id.clone(),
                    review_digest: applied.review_digest.clone(),
                    apply_id: applied.apply_id,
                    apply_digest: applied.digest,
                }),
            )
        }
        None => (
            "tested_input".to_owned(),
            test_spec.accepted_input.clone(),
            None,
        ),
    };

    let accepted_input_digest = accepted_input_digest(
        &workspace.digest,
        &workspace.ignore_rules_digest,
        &test_spec.workspace_guest_path,
        &image.resolved_digest,
        &image.target_platform,
    )?;
    let acceptance_id = Uuid::new_v4().to_string();
    let accepted_at = Utc::now();
    let decision = "explicit".to_owned();
    let mut warnings = vec![
        "acceptance records an explicit lineage decision, not a universal correctness judgment"
            .to_owned(),
        "new runs reconstruct the accepted workspace snapshot; retest filesystem debris is not promoted"
            .to_owned(),
    ];
    if test_result.exit_code != 0 {
        warnings.push(format!(
            "the explicitly accepted test run exited with status {}",
            test_result.exit_code
        ));
    }
    if image.execution_reference == image.docker_image_id {
        warnings.push(
            "the accepted OCI image has no repository digest and is reusable only while that local image ID remains available"
                .to_owned(),
        );
    }

    let mut record = AcceptanceRecord {
        schema_version: ACCEPTANCE_SCHEMA_VERSION.to_owned(),
        digest: String::new(),
        acceptance_id,
        accepted_input_digest,
        accepted_at,
        kind,
        decision,
        workspace_snapshot_digest: workspace.digest,
        workspace_ignore_digest: workspace.ignore_rules_digest,
        workspace_guest_path: test_spec.workspace_guest_path,
        image,
        tested_by_run_id: options.tested_by_run_id.clone(),
        test_result_digest: test_result.digest,
        test_run_input_digest: test_spec.run_input_digest,
        test_exit_code: test_result.exit_code,
        parent_accepted_input,
        applied_lineage,
        warnings,
    };
    record.digest = record_digest(&record)?;
    store.write_acceptance(&record.acceptance_id, &run::pretty_json(&record)?)?;
    verify(store, &record)?;
    Ok(record)
}

struct AcceptanceLock {
    path: PathBuf,
    _file: File,
}

impl AcceptanceLock {
    fn acquire(store: &Store, tested_by_run_id: &str) -> Result<Self> {
        let path = store
            .root()
            .join("acceptances")
            .join(format!(".{tested_by_run_id}.lock"));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => bail!(
                "acceptance for test run {tested_by_run_id:?} is already in progress or was interrupted; inspect {} before retrying",
                path.display()
            ),
            Err(error) => return Err(error).context("create exclusive acceptance lock"),
        };
        writeln!(
            file,
            "tested_by_run_id={tested_by_run_id}\npid={}\nstarted_at={}",
            std::process::id(),
            Utc::now()
        )?;
        file.sync_all()?;
        Ok(Self { path, _file: file })
    }
}

impl Drop for AcceptanceLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn find(store: &Store, acceptance_id: &str) -> Result<AcceptanceRecord> {
    Uuid::parse_str(acceptance_id).context("acceptance ID is not a UUID")?;
    let record: AcceptanceRecord = serde_json::from_slice(&store.read_acceptance(acceptance_id)?)
        .context("decode acceptance record")?;
    if record.acceptance_id != acceptance_id {
        bail!("acceptance path and record ID do not agree");
    }
    Ok(record)
}

pub fn list(store: &Store) -> Result<Vec<AcceptanceRecord>> {
    let mut records = Vec::new();
    for id in store.list_acceptance_ids()? {
        records.push(find(store, &id)?);
    }
    records.sort_by(|left, right| {
        left.accepted_at
            .cmp(&right.accepted_at)
            .then_with(|| left.acceptance_id.cmp(&right.acceptance_id))
    });
    Ok(records)
}

pub fn list_for_run(store: &Store, run_id: &str) -> Result<Vec<AcceptanceRecord>> {
    Ok(list(store)?
        .into_iter()
        .filter(|record| {
            record.tested_by_run_id == run_id
                || record
                    .applied_lineage
                    .as_ref()
                    .is_some_and(|lineage| lineage.candidate_run_id == run_id)
        })
        .collect())
}

pub fn referencing_run(store: &Store, run_id: &str) -> Result<Vec<String>> {
    Ok(list_for_run(store, run_id)?
        .into_iter()
        .map(|record| record.acceptance_id)
        .collect())
}

pub fn verify_all(store: &Store) -> Result<()> {
    for record in list(store)? {
        verify(store, &record)?;
    }
    Ok(())
}

pub fn verify(store: &Store, record: &AcceptanceRecord) -> Result<()> {
    if record.schema_version != ACCEPTANCE_SCHEMA_VERSION {
        bail!("unsupported acceptance schema {:?}", record.schema_version);
    }
    Uuid::parse_str(&record.acceptance_id).context("acceptance ID is not a UUID")?;
    if record.decision != "explicit" {
        bail!("acceptance decision is not explicit");
    }
    if !matches!(
        record.kind.as_str(),
        "tested_input" | "reviewed_application"
    ) {
        bail!("unsupported acceptance kind {:?}", record.kind);
    }

    verify_run_evidence(store, &record.tested_by_run_id)?;
    let test_result = run::load_result(store, &record.tested_by_run_id)?;
    let test_spec = run::load_spec(store, &record.tested_by_run_id)?;
    if record.accepted_at < test_result.completed_at {
        bail!("acceptance predates completion of its test run");
    }
    if record.test_result_digest != test_result.digest
        || record.test_run_input_digest != test_spec.run_input_digest
        || record.test_exit_code != test_result.exit_code
        || record.workspace_snapshot_digest != test_spec.workspace_snapshot_digest
        || record.workspace_ignore_digest != test_spec.workspace_ignore_digest
        || record.workspace_guest_path != test_spec.workspace_guest_path
    {
        bail!("acceptance record does not agree with its test run input and result");
    }
    let workspace = snapshot::load(store, &record.workspace_snapshot_digest)?;
    snapshot::verify(store, &workspace)?;
    if workspace.ignore_rules_digest != record.workspace_ignore_digest {
        bail!("accepted workspace ignore identity is inconsistent");
    }
    if record.image != accepted_image(store, &test_spec)? {
        bail!("acceptance image does not agree with its test run");
    }
    let expected_input_digest = accepted_input_digest(
        &record.workspace_snapshot_digest,
        &record.workspace_ignore_digest,
        &record.workspace_guest_path,
        &record.image.resolved_digest,
        &record.image.target_platform,
    )?;
    if record.accepted_input_digest != expected_input_digest {
        bail!("accepted input identity mismatch");
    }

    match &record.applied_lineage {
        Some(lineage) => {
            if record.kind != "reviewed_application" {
                bail!("tested-input acceptance unexpectedly contains apply lineage");
            }
            let applied = apply::find(store, &lineage.apply_id)?;
            apply::verify(store, &applied)?;
            if applied.digest != lineage.apply_digest
                || applied.run_id != lineage.candidate_run_id
                || applied.review_id != lineage.review_id
                || applied.review_digest != lineage.review_digest
            {
                bail!("acceptance apply lineage does not agree with the apply receipt");
            }
            if record.accepted_at < applied.completed_at {
                bail!("acceptance predates its reviewed application");
            }
            verify_run_evidence(store, &lineage.candidate_run_id)?;
            let candidate_result = run::load_result(store, &lineage.candidate_run_id)?;
            let candidate_spec = run::load_spec(store, &lineage.candidate_run_id)?;
            if lineage.candidate_result_digest != candidate_result.digest
                || lineage.candidate_run_input_digest != candidate_spec.run_input_digest
                || record.parent_accepted_input != candidate_spec.accepted_input
            {
                bail!("acceptance candidate lineage does not agree with the candidate run");
            }
            verify_applied_test_input(&applied, &candidate_spec, &test_spec)?;
        }
        None => {
            if record.kind != "tested_input" {
                bail!("reviewed-application acceptance omitted apply lineage");
            }
            if record.parent_accepted_input != test_spec.accepted_input {
                bail!("acceptance parent does not agree with the tested run");
            }
        }
    }

    if record_digest(record)? != record.digest {
        bail!("acceptance record integrity mismatch");
    }
    Ok(())
}

pub fn verify_run_input(
    store: &Store,
    reference: &AcceptedInputReference,
    workspace_snapshot_digest: &str,
    image_resolved_digest: &str,
    target_platform: &str,
    workspace_guest_path: &str,
    workspace_ignore_digest: &str,
) -> Result<()> {
    let record = find(store, &reference.acceptance_id)?;
    verify(store, &record)?;
    if reference.acceptance_digest != record.digest
        || reference.accepted_input_digest != record.accepted_input_digest
    {
        bail!("run acceptance reference does not match its immutable record");
    }
    if workspace_snapshot_digest != record.workspace_snapshot_digest
        || image_resolved_digest != record.image.resolved_digest
        || target_platform != record.image.target_platform
        || workspace_guest_path != record.workspace_guest_path
        || workspace_ignore_digest != record.workspace_ignore_digest
    {
        bail!("run input does not match the selected accepted input");
    }
    Ok(())
}

pub fn reference(record: &AcceptanceRecord) -> AcceptedInputReference {
    AcceptedInputReference {
        acceptance_id: record.acceptance_id.clone(),
        acceptance_digest: record.digest.clone(),
        accepted_input_digest: record.accepted_input_digest.clone(),
    }
}

fn verify_run_evidence(store: &Store, run_id: &str) -> Result<()> {
    lifecycle::verify_all(store, run_id)?;
    evaluation::verify_all(store, run_id)?;
    review::verify_all(store, run_id)?;
    apply::verify_all(store, run_id)?;
    Ok(())
}

fn verify_applied_test_input(
    applied: &ApplyRecord,
    candidate: &RunSpec,
    test: &RunSpec,
) -> Result<()> {
    if applied.after_workspace_snapshot_digest != test.workspace_snapshot_digest {
        bail!(
            "test run workspace {} does not match applied workspace {}; retest the exact applied snapshot",
            test.workspace_snapshot_digest,
            applied.after_workspace_snapshot_digest
        );
    }
    if candidate.image_resolved_digest != test.image_resolved_digest
        || candidate.target_platform != test.target_platform
    {
        bail!("test run did not use the candidate run's exact resolved environment and platform");
    }
    if candidate.workspace_guest_path != test.workspace_guest_path {
        bail!("test run changed the accepted workspace materialization path");
    }
    Ok(())
}

fn accepted_image(store: &Store, spec: &RunSpec) -> Result<AcceptedImage> {
    Ok(AcceptedImage {
        requested: spec.image_requested.clone(),
        execution_reference: run::immutable_image_reference(store, &spec.run_id)?,
        resolved_digest: spec.image_resolved_digest.clone(),
        docker_image_id: spec.docker_image_id.clone(),
        target_platform: spec.target_platform.clone(),
    })
}

fn accepted_input_digest(
    workspace_snapshot_digest: &str,
    workspace_ignore_digest: &str,
    workspace_guest_path: &str,
    image_resolved_digest: &str,
    target_platform: &str,
) -> Result<String> {
    let identity = AcceptedInputIdentity {
        schema_version: ACCEPTED_INPUT_SCHEMA_VERSION,
        workspace_snapshot_digest,
        workspace_ignore_digest,
        workspace_guest_path,
        image_resolved_digest,
        target_platform,
    };
    Ok(run::sha256_bytes(&serde_json::to_vec(&identity)?))
}

fn record_digest(record: &AcceptanceRecord) -> Result<String> {
    let identity = AcceptanceIdentity {
        schema_version: ACCEPTANCE_SCHEMA_VERSION,
        acceptance_id: &record.acceptance_id,
        accepted_input_digest: &record.accepted_input_digest,
        accepted_at: record.accepted_at,
        kind: &record.kind,
        decision: &record.decision,
        workspace_snapshot_digest: &record.workspace_snapshot_digest,
        workspace_ignore_digest: &record.workspace_ignore_digest,
        workspace_guest_path: &record.workspace_guest_path,
        image: &record.image,
        tested_by_run_id: &record.tested_by_run_id,
        test_result_digest: &record.test_result_digest,
        test_run_input_digest: &record.test_run_input_digest,
        test_exit_code: record.test_exit_code,
        parent_accepted_input: &record.parent_accepted_input,
        applied_lineage: &record.applied_lineage,
        warnings: &record.warnings,
    };
    Ok(run::sha256_bytes(&serde_json::to_vec(&identity)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_input_identity_is_content_based() {
        let first = accepted_input_digest(
            "sha256:workspace",
            "sha256:ignore",
            "/workspace",
            "sha256:image",
            "linux/arm64",
        )
        .unwrap();
        let second = accepted_input_digest(
            "sha256:workspace",
            "sha256:ignore",
            "/workspace",
            "sha256:image",
            "linux/arm64",
        )
        .unwrap();
        let changed = accepted_input_digest(
            "sha256:improved",
            "sha256:ignore",
            "/workspace",
            "sha256:image",
            "linux/arm64",
        )
        .unwrap();
        assert_eq!(first, second);
        assert_ne!(first, changed);
    }
}
