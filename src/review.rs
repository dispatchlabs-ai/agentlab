use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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

pub const REVIEW_REQUEST_SCHEMA_VERSION: &str = "agentlab.review-request/v1";
pub const REVIEW_PROPOSAL_SCHEMA_VERSION: &str = "agentlab.review-proposal/v1";
pub const REVIEW_SCHEMA_VERSION: &str = "agentlab.review/v1";
pub const REVIEW_ATTEMPT_SCHEMA_VERSION: &str = "agentlab.review-attempt/v1";

const REVIEWER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct ReviewOptions {
    pub run_id: String,
    pub workspace: PathBuf,
    pub reviewer_command: Vec<String>,
}

pub trait ReviewObserver {
    fn stage(&mut self, message: &str) -> io::Result<()>;
}

struct SilentReviewObserver;

impl ReviewObserver for SilentReviewObserver {
    fn stage(&mut self, _message: &str) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewAnchors {
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
pub struct ReviewRepositories {
    pub base: Vec<Repository>,
    pub candidate: Vec<Repository>,
    pub current: Vec<Repository>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewCandidate {
    pub path: String,
    pub change: ChangeKind,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    pub current_relation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewRequest {
    pub schema_version: String,
    pub review_id: String,
    pub anchors: ReviewAnchors,
    pub workspace_guest_path: String,
    pub reviewer_command: Vec<String>,
    pub input_artifacts: BTreeMap<String, String>,
    pub repositories: ReviewRepositories,
    pub candidates: Vec<ReviewCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceOperation {
    pub operation: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewDisposition {
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
pub struct ReviewProposal {
    pub schema_version: String,
    pub review_id: String,
    pub anchors: ReviewAnchors,
    pub counts: DispositionCounts,
    pub dispositions: Vec<ReviewDisposition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommendations: Vec<DeclarativeRecommendation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeclarativeRecommendation {
    pub target: String,
    pub recommendation: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewRecord {
    pub schema_version: String,
    pub digest: String,
    pub review_id: String,
    pub run_id: String,
    pub result_digest: String,
    pub source_workspace: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub reviewer_exit_code: i64,
    pub request: ReviewRequest,
    pub proposal: ReviewProposal,
    pub request_artifact: Artifact,
    pub proposal_artifact: Artifact,
    pub stdout: Artifact,
    pub stderr: Artifact,
    pub source_workspace_unchanged: bool,
    pub agentlab_applied_changes: bool,
    pub warnings: Vec<String>,
    pub integrity: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewerInvocation {
    pub attempt: usize,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub exit_code: i64,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_error: Option<String>,
    pub stdout: Artifact,
    pub stderr: Artifact,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewAttemptRecord {
    pub schema_version: String,
    pub digest: String,
    pub review_id: String,
    pub run_id: String,
    pub result_digest: String,
    pub source_workspace: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    pub request: ReviewRequest,
    pub request_artifact: Artifact,
    pub invocations: Vec<ReviewerInvocation>,
    pub source_workspace_unchanged: bool,
    pub agentlab_applied_changes: bool,
    pub warnings: Vec<String>,
    pub integrity: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct ReviewIdentity<'a> {
    schema_version: &'a str,
    review_id: &'a str,
    run_id: &'a str,
    result_digest: &'a str,
    source_workspace: &'a str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    reviewer_exit_code: i64,
    request: &'a ReviewRequest,
    proposal: &'a ReviewProposal,
    request_artifact: &'a Artifact,
    proposal_artifact: &'a Artifact,
    stdout: &'a Artifact,
    stderr: &'a Artifact,
    source_workspace_unchanged: bool,
    agentlab_applied_changes: bool,
    warnings: &'a [String],
    integrity: &'a BTreeMap<String, String>,
}

#[derive(Serialize)]
struct ReviewAttemptIdentity<'a> {
    schema_version: &'a str,
    review_id: &'a str,
    run_id: &'a str,
    result_digest: &'a str,
    source_workspace: &'a str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    status: &'a str,
    failure: &'a Option<String>,
    request: &'a ReviewRequest,
    request_artifact: &'a Artifact,
    invocations: &'a [ReviewerInvocation],
    source_workspace_unchanged: bool,
    agentlab_applied_changes: bool,
    warnings: &'a [String],
    integrity: &'a BTreeMap<String, String>,
}

pub fn review(store: &Store, options: &ReviewOptions) -> Result<ReviewRecord> {
    review_with_observer(store, options, &mut SilentReviewObserver)
}

pub fn review_with_observer(
    store: &Store,
    options: &ReviewOptions,
    observer: &mut dyn ReviewObserver,
) -> Result<ReviewRecord> {
    if options.reviewer_command.is_empty() {
        bail!("review requires a reviewer command after `--`");
    }
    report_stage(
        observer,
        "Preparing trusted host reviewer; verifying immutable run and prior review evidence",
    )?;
    let reviewer_command = resolve_reviewer_command(&options.reviewer_command)?;
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

    report_stage(observer, "Capturing the current host workspace")?;
    let current_capture = snapshot::create(&options.workspace, store)?;
    let source_workspace = current_capture
        .workspace
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("source workspace path is not valid UTF-8"))?;
    let current_before = current_capture.manifest;
    let review_id = Uuid::new_v4().to_string();
    let bundle = tempfile::tempdir().context("create private review bundle")?;
    let base_directory = bundle.path().join("base");
    let candidate_directory = bundle.path().join("candidate");
    let current_directory = bundle.path().join("current");
    let machine_changes_directory = bundle.path().join("machine-changes");
    report_stage(
        observer,
        "Materializing private base, candidate, and current workspace copies",
    )?;
    snapshot::materialize(store, &base_workspace, &base_directory)?;
    materialize_candidate_workspace(
        store,
        &candidate_rootfs,
        &spec.workspace_guest_path,
        &candidate_directory,
    )?;
    let candidate_workspace = snapshot::create(&candidate_directory, store)?.manifest;
    snapshot::materialize(store, &current_before, &current_directory)?;

    let anchors = ReviewAnchors {
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
        &result,
        &base_workspace,
        &candidate_workspace,
        &current_before,
    )?;
    let candidates = review_candidates(
        &raw_delta,
        &spec.workspace_guest_path,
        &base_workspace,
        &current_before,
    )?;
    materialize_machine_changes(store, &raw_delta, &machine_changes_directory)?;
    let request = ReviewRequest {
        schema_version: REVIEW_REQUEST_SCHEMA_VERSION.to_owned(),
        review_id: review_id.clone(),
        anchors: anchors.clone(),
        workspace_guest_path: spec.workspace_guest_path.clone(),
        reviewer_command: reviewer_command.clone(),
        input_artifacts,
        repositories: ReviewRepositories {
            base: base_workspace.repositories.clone(),
            candidate: candidate_workspace.repositories.clone(),
            current: current_before.repositories.clone(),
        },
        candidates,
    };
    let request_bytes = run::pretty_json(&request)?;
    fs::write(bundle.path().join("request.json"), &request_bytes)
        .context("write review request into review bundle")?;

    let started_at = Utc::now();
    let mut captures = Vec::new();
    let mut proposal = None;
    let mut repair = None;
    let mut failure = None;
    for attempt in 1..=2 {
        let output = execute_reviewer(
            &reviewer_command,
            &options.run_id,
            &review_id,
            bundle.path(),
            &base_directory,
            &candidate_directory,
            &current_directory,
            &machine_changes_directory,
            repair.as_ref(),
            attempt,
            observer,
        )?;
        verify_reviewer_postconditions(
            store,
            &options.run_id,
            bundle.path(),
            &request,
            &request_bytes,
            &base_workspace,
            &candidate_workspace,
            &current_before,
            &options.workspace,
        )?;

        let exit_code = output.status.code().map(i64::from).unwrap_or(-1);
        if !output.status.success() {
            let message = format!(
                "reviewer command exited with status {exit_code}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            captures.push(InvocationCapture {
                attempt,
                started_at: output.started_at,
                completed_at: output.completed_at,
                exit_code,
                status: "command_failed".to_owned(),
                validation_error: Some(message.clone()),
                stdout: output.stdout,
                stderr: output.stderr,
            });
            failure = Some(message);
            break;
        }

        match decode_proposal(&request, &output.stdout) {
            Ok(valid) => {
                captures.push(InvocationCapture {
                    attempt,
                    started_at: output.started_at,
                    completed_at: output.completed_at,
                    exit_code,
                    status: "accepted".to_owned(),
                    validation_error: None,
                    stdout: output.stdout,
                    stderr: output.stderr,
                });
                proposal = Some(valid);
                break;
            }
            Err(error) => {
                let message = format!("{error:#}");
                let previous_stdout = output.stdout.clone();
                captures.push(InvocationCapture {
                    attempt,
                    started_at: output.started_at,
                    completed_at: output.completed_at,
                    exit_code,
                    status: "invalid_proposal".to_owned(),
                    validation_error: Some(message.clone()),
                    stdout: output.stdout,
                    stderr: output.stderr,
                });
                if attempt == 1 {
                    report_stage(
                        observer,
                        "Reviewer attempt 1 returned an invalid proposal; requesting one schema correction",
                    )?;
                    let stdout_path = bundle.path().join("previous-reviewer-stdout.bin");
                    let error_path = bundle.path().join("proposal-validation-error.txt");
                    fs::write(&stdout_path, previous_stdout)
                        .context("write previous reviewer output for correction")?;
                    fs::write(&error_path, message.as_bytes())
                        .context("write proposal validation error for correction")?;
                    repair = Some(RepairInput {
                        previous_stdout_path: stdout_path,
                        validation_error_path: error_path,
                    });
                } else {
                    failure = Some(format!(
                        "reviewer proposal still violated the contract after one correction: {message}"
                    ));
                }
            }
        }
    }
    let completed_at = Utc::now();
    let attempt_status = if proposal.is_some() {
        "accepted"
    } else {
        "rejected"
    };
    let attempt_record = persist_attempt_record(
        store,
        &options.run_id,
        &review_id,
        &result.digest,
        &source_workspace,
        started_at,
        completed_at,
        attempt_status,
        failure.clone(),
        &request,
        &request_bytes,
        &captures,
    )?;
    if let Some(failure) = failure {
        report_stage(observer, &format!("Rejected review recorded: {review_id}"))?;
        let last = attempt_record
            .invocations
            .last()
            .context("rejected review recorded no reviewer invocation")?;
        let output_path = store.run_path(&options.run_id, &last.stdout.path)?;
        bail!(
            "review {review_id} was rejected: {failure}\nInspect: agentlab inspect --verify {review_id}\nReviewer output: {}",
            output_path.display()
        );
    }
    let proposal = proposal.context("review ended without a proposal or recorded failure")?;
    report_stage(
        observer,
        "Proposal contract validated; recording immutable review",
    )?;

    let prefix = format!("reviews/{review_id}");
    let request_artifact = attempt_record.request_artifact.clone();
    let proposal_bytes = run::pretty_json(&proposal)?;
    let proposal_artifact = write_artifact(
        store,
        &options.run_id,
        &format!("{prefix}/proposal.json"),
        &proposal_bytes,
    )?;
    let final_capture = captures
        .last()
        .context("accepted review recorded no reviewer invocation")?;
    let stdout = write_artifact(
        store,
        &options.run_id,
        &format!("{prefix}/artifacts/stdout.bin"),
        &final_capture.stdout,
    )?;
    let stderr = write_artifact(
        store,
        &options.run_id,
        &format!("{prefix}/artifacts/stderr.bin"),
        &final_capture.stderr,
    )?;
    let mut integrity = BTreeMap::new();
    for artifact in [&request_artifact, &proposal_artifact, &stdout, &stderr] {
        integrity.insert(artifact.path.clone(), artifact.digest.clone());
    }
    let mut warnings = vec![
        "the reviewer ran as a trusted host process with the invoking user's authority".to_owned(),
        "review mode recorded a proposal only; AgentLab applied no changes".to_owned(),
        "captured workspaces, machine deltas, reviewer output, and receipts may contain sensitive information"
            .to_owned(),
    ];
    if captures.len() > 1 {
        warnings.push(
            "the first reviewer response violated the proposal contract; a second constrained correction was validated and retained"
                .to_owned(),
        );
    }
    let identity = ReviewIdentity {
        schema_version: REVIEW_SCHEMA_VERSION,
        review_id: &review_id,
        run_id: &options.run_id,
        result_digest: &result.digest,
        source_workspace: &source_workspace,
        started_at,
        completed_at,
        reviewer_exit_code: final_capture.exit_code,
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
    let record = ReviewRecord {
        schema_version: REVIEW_SCHEMA_VERSION.to_owned(),
        digest: run::sha256_bytes(&serde_json::to_vec(&identity)?),
        review_id,
        run_id: options.run_id.clone(),
        result_digest: result.digest,
        source_workspace,
        started_at,
        completed_at,
        reviewer_exit_code: final_capture.exit_code,
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
    report_stage(observer, &format!("Review accepted: {}", record.review_id))?;
    Ok(record)
}

#[derive(Debug)]
struct ReviewerOutput {
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug)]
struct InvocationCapture {
    attempt: usize,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    exit_code: i64,
    status: String,
    validation_error: Option<String>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug)]
struct RepairInput {
    previous_stdout_path: PathBuf,
    validation_error_path: PathBuf,
}

#[allow(clippy::too_many_arguments)]
fn execute_reviewer(
    reviewer_command: &[String],
    run_id: &str,
    review_id: &str,
    bundle: &Path,
    base_directory: &Path,
    candidate_directory: &Path,
    current_directory: &Path,
    machine_changes_directory: &Path,
    repair: Option<&RepairInput>,
    attempt: usize,
    observer: &mut dyn ReviewObserver,
) -> Result<ReviewerOutput> {
    report_stage(observer, &format!("Reviewer attempt {attempt} started"))?;
    let started_at = Utc::now();
    let mut command = Command::new(&reviewer_command[0]);
    command
        .args(&reviewer_command[1..])
        .current_dir(current_directory)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("AGENTLAB_RUN_ID", run_id)
        .env("AGENTLAB_REVIEW_ID", review_id)
        .env("AGENTLAB_REVIEW_BUNDLE_DIR", bundle)
        .env("AGENTLAB_REVIEW_REQUEST_PATH", bundle.join("request.json"))
        .env(
            "AGENTLAB_REVIEW_BASE_MANIFEST_PATH",
            bundle.join("base-manifest.json"),
        )
        .env(
            "AGENTLAB_REVIEW_CANDIDATE_MANIFEST_PATH",
            bundle.join("candidate-manifest.json"),
        )
        .env(
            "AGENTLAB_REVIEW_CURRENT_MANIFEST_PATH",
            bundle.join("current-manifest.json"),
        )
        .env("AGENTLAB_REVIEW_DELTA_PATH", bundle.join("delta.json"))
        .env(
            "AGENTLAB_REVIEW_RAW_DELTA_PATH",
            bundle.join("delta.raw.json"),
        )
        .env("AGENTLAB_REVIEW_RUN_SPEC_PATH", bundle.join("spec.json"))
        .env(
            "AGENTLAB_REVIEW_RUN_RESULT_PATH",
            bundle.join("result.json"),
        )
        .env(
            "AGENTLAB_REVIEW_RUN_STDOUT_PATH",
            bundle.join("run-stdout.bin"),
        )
        .env(
            "AGENTLAB_REVIEW_RUN_STDERR_PATH",
            bundle.join("run-stderr.bin"),
        )
        .env(
            "AGENTLAB_REVIEW_EVALUATIONS_PATH",
            bundle.join("evaluations.json"),
        )
        .env(
            "AGENTLAB_REVIEW_BASE_ROOTFS_MANIFEST_PATH",
            bundle.join("base-rootfs.json"),
        )
        .env(
            "AGENTLAB_REVIEW_CANDIDATE_ROOTFS_MANIFEST_PATH",
            bundle.join("candidate-rootfs.json"),
        )
        .env("AGENTLAB_REVIEW_BASE_DIR", base_directory)
        .env("AGENTLAB_REVIEW_CANDIDATE_DIR", candidate_directory)
        .env("AGENTLAB_REVIEW_CURRENT_DIR", current_directory)
        .env(
            "AGENTLAB_REVIEW_MACHINE_CHANGES_DIR",
            machine_changes_directory,
        );
    if let Some(repair) = repair {
        command
            .env("AGENTLAB_REVIEW_REPAIR", "1")
            .env(
                "AGENTLAB_REVIEW_PREVIOUS_STDOUT_PATH",
                &repair.previous_stdout_path,
            )
            .env(
                "AGENTLAB_REVIEW_VALIDATION_ERROR_PATH",
                &repair.validation_error_path,
            );
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("execute reviewer command {:?}", reviewer_command[0]))?;
    let stdout = child
        .stdout
        .take()
        .context("capture reviewer stdout pipe")?;
    let stderr = child
        .stderr
        .take()
        .context("capture reviewer stderr pipe")?;
    let stdout_reader = thread::spawn(move || read_pipe(stdout));
    let stderr_reader = thread::spawn(move || read_pipe(stderr));
    let wait_started = Instant::now();
    let mut next_heartbeat = REVIEWER_HEARTBEAT_INTERVAL;
    let status = loop {
        if let Some(status) = child.try_wait().context("wait for reviewer command")? {
            break status;
        }
        if wait_started.elapsed() >= next_heartbeat {
            if let Err(error) = observer.stage(&format!(
                "Reviewer attempt {attempt} still running ({:.0}s)",
                wait_started.elapsed().as_secs_f64()
            )) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("report review progress");
            }
            next_heartbeat += REVIEWER_HEARTBEAT_INTERVAL;
        }
        thread::sleep(Duration::from_millis(200));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("reviewer stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("reviewer stderr reader panicked"))??;
    let completed_at = Utc::now();
    report_stage(
        observer,
        &format!(
            "Reviewer attempt {attempt} completed with exit code {}",
            status.code().map(i64::from).unwrap_or(-1)
        ),
    )?;
    Ok(ReviewerOutput {
        started_at,
        completed_at,
        status,
        stdout,
        stderr,
    })
}

fn read_pipe(mut source: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    source.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn report_stage(observer: &mut dyn ReviewObserver, message: &str) -> Result<()> {
    observer.stage(message).context("report review progress")
}

#[allow(clippy::too_many_arguments)]
fn verify_reviewer_postconditions(
    store: &Store,
    run_id: &str,
    bundle: &Path,
    request: &ReviewRequest,
    request_bytes: &[u8],
    base_workspace: &Manifest,
    candidate_workspace: &Manifest,
    current_before: &Manifest,
    source_workspace: &Path,
) -> Result<()> {
    verify_bundle_inputs(bundle, request, request_bytes)?;
    lifecycle::verify_all(store, run_id)
        .context("reviewer mutated immutable run or lifecycle artifacts")?;
    evaluation::verify_all(store, run_id).context("reviewer mutated evaluation artifacts")?;
    verify_all(store, run_id).context("reviewer mutated prior review records")?;
    snapshot::verify(store, base_workspace)
        .context("reviewer mutated base workspace snapshot content")?;
    snapshot::verify(store, candidate_workspace)
        .context("reviewer mutated candidate workspace snapshot content")?;
    snapshot::verify(store, current_before)
        .context("reviewer mutated current workspace snapshot content")?;
    let current_after = snapshot::create(source_workspace, store)?.manifest;
    if current_after.digest != current_before.digest {
        bail!("reviewer changed the selected current workspace; review receipt was not accepted");
    }
    Ok(())
}

fn decode_proposal(request: &ReviewRequest, stdout: &[u8]) -> Result<ReviewProposal> {
    let value: serde_json::Value =
        serde_json::from_slice(stdout).context("reviewer stdout was not valid JSON")?;
    let proposal: ReviewProposal = serde_json::from_value(value)
        .context("reviewer JSON did not match agentlab.review-proposal/v1")?;
    validate_proposal(request, &proposal)
        .context("reviewer proposal violated agentlab.review-proposal/v1")?;
    Ok(proposal)
}

#[allow(clippy::too_many_arguments)]
fn persist_attempt_record(
    store: &Store,
    run_id: &str,
    review_id: &str,
    result_digest: &str,
    source_workspace: &str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    status: &str,
    failure: Option<String>,
    request: &ReviewRequest,
    request_bytes: &[u8],
    captures: &[InvocationCapture],
) -> Result<ReviewAttemptRecord> {
    let prefix = format!("reviews/{review_id}");
    let request_artifact = write_artifact(
        store,
        run_id,
        &format!("{prefix}/request.json"),
        request_bytes,
    )?;
    let mut invocations = Vec::with_capacity(captures.len());
    let mut integrity = BTreeMap::new();
    integrity.insert(
        request_artifact.path.clone(),
        request_artifact.digest.clone(),
    );
    for capture in captures {
        let stdout = write_artifact(
            store,
            run_id,
            &format!("{prefix}/artifacts/attempt-{}-stdout.bin", capture.attempt),
            &capture.stdout,
        )?;
        let stderr = write_artifact(
            store,
            run_id,
            &format!("{prefix}/artifacts/attempt-{}-stderr.bin", capture.attempt),
            &capture.stderr,
        )?;
        integrity.insert(stdout.path.clone(), stdout.digest.clone());
        integrity.insert(stderr.path.clone(), stderr.digest.clone());
        invocations.push(ReviewerInvocation {
            attempt: capture.attempt,
            started_at: capture.started_at,
            completed_at: capture.completed_at,
            exit_code: capture.exit_code,
            status: capture.status.clone(),
            validation_error: capture.validation_error.clone(),
            stdout,
            stderr,
        });
    }
    let warnings = vec![
        "the reviewer ran as a trusted host process with the invoking user's authority".to_owned(),
        "review attempts retain raw reviewer stdout and stderr, which may contain sensitive information"
            .to_owned(),
        "AgentLab applied no changes during review".to_owned(),
    ];
    let identity = ReviewAttemptIdentity {
        schema_version: REVIEW_ATTEMPT_SCHEMA_VERSION,
        review_id,
        run_id,
        result_digest,
        source_workspace,
        started_at,
        completed_at,
        status,
        failure: &failure,
        request,
        request_artifact: &request_artifact,
        invocations: &invocations,
        source_workspace_unchanged: true,
        agentlab_applied_changes: false,
        warnings: &warnings,
        integrity: &integrity,
    };
    let record = ReviewAttemptRecord {
        schema_version: REVIEW_ATTEMPT_SCHEMA_VERSION.to_owned(),
        digest: run::sha256_bytes(&serde_json::to_vec(&identity)?),
        review_id: review_id.to_owned(),
        run_id: run_id.to_owned(),
        result_digest: result_digest.to_owned(),
        source_workspace: source_workspace.to_owned(),
        started_at,
        completed_at,
        status: status.to_owned(),
        failure,
        request: request.clone(),
        request_artifact,
        invocations,
        source_workspace_unchanged: true,
        agentlab_applied_changes: false,
        warnings,
        integrity,
    };
    store.write_run_file(
        run_id,
        &format!("{prefix}/review-attempt.json"),
        &run::pretty_json(&record)?,
    )?;
    verify_attempt(store, &record)?;
    Ok(record)
}

fn resolve_reviewer_command(command: &[String]) -> Result<Vec<String>> {
    let mut resolved = command.to_vec();
    let executable = Path::new(&resolved[0]);
    if !executable.is_absolute() && executable.components().count() > 1 {
        let invocation_directory = std::env::current_dir().context("resolve current directory")?;
        let absolute = invocation_directory
            .join(executable)
            .canonicalize()
            .with_context(|| {
                format!(
                    "resolve reviewer executable {:?} from {}",
                    resolved[0],
                    invocation_directory.display()
                )
            })?;
        resolved[0] = absolute
            .into_os_string()
            .into_string()
            .map_err(|_| anyhow::anyhow!("reviewer executable path is not valid UTF-8"))?;
    }
    Ok(resolved)
}

pub fn list(store: &Store, run_id: &str) -> Result<Vec<ReviewRecord>> {
    let directory = store.run_path(run_id, "reviews")?;
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut records: Vec<ReviewRecord> = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("review ID is not valid UTF-8"))?;
        let relative = format!("reviews/{id}/review.json");
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

pub fn find(store: &Store, review_id: &str) -> Result<ReviewRecord> {
    find_optional(store, review_id)?.with_context(|| format!("review {review_id:?} not found"))
}

pub fn find_optional(store: &Store, review_id: &str) -> Result<Option<ReviewRecord>> {
    Uuid::parse_str(review_id).context("review ID is not a UUID")?;
    let mut found = None;
    let relative = format!("reviews/{review_id}/review.json");
    for run_id in store.list_run_ids()? {
        if !store.run_file_exists(&run_id, &relative)? {
            continue;
        }
        let record: ReviewRecord =
            serde_json::from_slice(&store.read_run_file(&run_id, &relative)?)?;
        if record.review_id != review_id {
            bail!("review path and record ID do not agree");
        }
        if found.is_some() {
            bail!("review ID {review_id:?} is not unique");
        }
        found = Some(record);
    }
    Ok(found)
}

pub fn list_attempts(store: &Store, run_id: &str) -> Result<Vec<ReviewAttemptRecord>> {
    let directory = store.run_path(run_id, "reviews")?;
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("review ID is not valid UTF-8"))?;
        let relative = format!("reviews/{id}/review-attempt.json");
        if store.run_file_exists(run_id, &relative)? {
            records.push(serde_json::from_slice(
                &store.read_run_file(run_id, &relative)?,
            )?);
        }
    }
    records.sort_by(|left: &ReviewAttemptRecord, right: &ReviewAttemptRecord| {
        left.completed_at
            .cmp(&right.completed_at)
            .then_with(|| left.review_id.cmp(&right.review_id))
    });
    Ok(records)
}

pub fn find_attempt(store: &Store, review_id: &str) -> Result<ReviewAttemptRecord> {
    find_attempt_optional(store, review_id)?
        .with_context(|| format!("review attempt {review_id:?} not found"))
}

pub fn find_attempt_optional(
    store: &Store,
    review_id: &str,
) -> Result<Option<ReviewAttemptRecord>> {
    Uuid::parse_str(review_id).context("review ID is not a UUID")?;
    let mut found = None;
    let relative = format!("reviews/{review_id}/review-attempt.json");
    for run_id in store.list_run_ids()? {
        if !store.run_file_exists(&run_id, &relative)? {
            continue;
        }
        let record: ReviewAttemptRecord =
            serde_json::from_slice(&store.read_run_file(&run_id, &relative)?)?;
        if record.review_id != review_id {
            bail!("review-attempt path and record ID do not agree");
        }
        if found.is_some() {
            bail!("review attempt ID {review_id:?} is not unique");
        }
        found = Some(record);
    }
    Ok(found)
}

pub fn verify_all(store: &Store, run_id: &str) -> Result<()> {
    for record in list_attempts(store, run_id)? {
        verify_attempt(store, &record)?;
    }
    for record in list(store, run_id)? {
        verify(store, &record)?;
    }
    Ok(())
}

pub fn verify_attempt(store: &Store, record: &ReviewAttemptRecord) -> Result<()> {
    if record.schema_version != REVIEW_ATTEMPT_SCHEMA_VERSION {
        bail!(
            "unsupported review-attempt schema {:?}",
            record.schema_version
        );
    }
    if record.review_id != record.request.review_id
        || record.run_id != record.request.anchors.run_id
        || record.result_digest != record.request.anchors.result_digest
    {
        bail!("review-attempt record fields do not agree with request anchors");
    }
    if !Path::new(&record.source_workspace).is_absolute() {
        bail!("review-attempt source workspace path is not absolute");
    }
    if !record.source_workspace_unchanged || record.agentlab_applied_changes {
        bail!("review-attempt safety fields are inconsistent");
    }
    if record.invocations.is_empty() || record.invocations.len() > 2 {
        bail!("review attempt must retain one or two reviewer invocations");
    }
    for (index, invocation) in record.invocations.iter().enumerate() {
        if invocation.attempt != index + 1 {
            bail!("reviewer invocation sequence is inconsistent");
        }
        if !matches!(
            invocation.status.as_str(),
            "accepted" | "invalid_proposal" | "command_failed"
        ) {
            bail!("invalid reviewer invocation status {:?}", invocation.status);
        }
        if invocation.status == "accepted" && invocation.validation_error.is_some() {
            bail!("accepted reviewer invocation contains a validation error");
        }
        if invocation.status != "accepted" && invocation.validation_error.is_none() {
            bail!("failed reviewer invocation omitted its validation error");
        }
    }
    let last = record
        .invocations
        .last()
        .context("review attempt omitted reviewer invocations")?;
    match record.status.as_str() {
        "accepted" if record.failure.is_none() && last.status == "accepted" => {}
        "rejected" if record.failure.is_some() && last.status != "accepted" => {}
        "accepted" | "rejected" => bail!("review-attempt outcome fields are inconsistent"),
        value => bail!("invalid review-attempt status {value:?}"),
    }
    if record.invocations.len() == 2 && record.invocations[0].status != "invalid_proposal" {
        bail!("only an invalid proposal may trigger a second reviewer invocation");
    }
    for (relative, expected) in &record.integrity {
        let actual = run::sha256_bytes(&store.read_run_file(&record.run_id, relative)?);
        if &actual != expected {
            bail!("review-attempt artifact integrity mismatch for {relative:?}");
        }
    }
    let stored_request: ReviewRequest = serde_json::from_slice(
        &store.read_run_file(&record.run_id, &record.request_artifact.path)?,
    )?;
    if stored_request != record.request {
        bail!("review-attempt record and stored request do not agree");
    }
    let identity = ReviewAttemptIdentity {
        schema_version: REVIEW_ATTEMPT_SCHEMA_VERSION,
        review_id: &record.review_id,
        run_id: &record.run_id,
        result_digest: &record.result_digest,
        source_workspace: &record.source_workspace,
        started_at: record.started_at,
        completed_at: record.completed_at,
        status: &record.status,
        failure: &record.failure,
        request: &record.request,
        request_artifact: &record.request_artifact,
        invocations: &record.invocations,
        source_workspace_unchanged: record.source_workspace_unchanged,
        agentlab_applied_changes: record.agentlab_applied_changes,
        warnings: &record.warnings,
        integrity: &record.integrity,
    };
    if run::sha256_bytes(&serde_json::to_vec(&identity)?) != record.digest {
        bail!("review-attempt record integrity mismatch");
    }
    Ok(())
}

pub fn verify(store: &Store, record: &ReviewRecord) -> Result<()> {
    if record.schema_version != REVIEW_SCHEMA_VERSION {
        bail!("unsupported review schema {:?}", record.schema_version);
    }
    if record.review_id != record.request.review_id
        || record.run_id != record.request.anchors.run_id
        || record.result_digest != record.request.anchors.result_digest
        || record.proposal.review_id != record.review_id
    {
        bail!("review record fields do not agree with request/proposal anchors");
    }
    if !Path::new(&record.source_workspace).is_absolute() {
        bail!("review record source workspace path is not absolute");
    }
    for (relative, expected) in &record.integrity {
        let actual = run::sha256_bytes(&store.read_run_file(&record.run_id, relative)?);
        if &actual != expected {
            bail!("review artifact integrity mismatch for {relative:?}");
        }
    }
    let stored_request: ReviewRequest = serde_json::from_slice(
        &store.read_run_file(&record.run_id, &record.request_artifact.path)?,
    )?;
    let stored_proposal: ReviewProposal = serde_json::from_slice(
        &store.read_run_file(&record.run_id, &record.proposal_artifact.path)?,
    )?;
    if stored_request != record.request || stored_proposal != record.proposal {
        bail!("review record and stored request/proposal do not agree");
    }
    validate_proposal(&record.request, &record.proposal)?;
    let identity = ReviewIdentity {
        schema_version: REVIEW_SCHEMA_VERSION,
        review_id: &record.review_id,
        run_id: &record.run_id,
        result_digest: &record.result_digest,
        source_workspace: &record.source_workspace,
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
        bail!("review record integrity mismatch");
    }
    Ok(())
}

fn validate_proposal(request: &ReviewRequest, proposal: &ReviewProposal) -> Result<()> {
    validate_request(request)?;
    if proposal.schema_version != REVIEW_PROPOSAL_SCHEMA_VERSION {
        bail!(
            "unsupported review proposal schema {:?}",
            proposal.schema_version
        );
    }
    if proposal.review_id != request.review_id {
        bail!("review proposal review ID does not match request");
    }
    if proposal.anchors != request.anchors {
        bail!("review proposal anchors do not match request");
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
            bail!("duplicate review disposition for {:?}", disposition.path);
        }
        let candidate = candidates.get(disposition.path.as_str()).with_context(|| {
            format!(
                "review disposition references non-candidate path {:?}",
                disposition.path
            )
        })?;
        if disposition.reason.trim().is_empty() {
            bail!(
                "review disposition {:?} requires a reason",
                disposition.path
            );
        }
        match disposition.disposition.as_str() {
            "proposed" => counts.proposed += 1,
            "rejected" => counts.rejected += 1,
            "conflicted" => counts.conflicted += 1,
            "unresolved" => counts.unresolved += 1,
            value => bail!("invalid review disposition {value:?}"),
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
        bail!("review proposal omitted candidate paths: {missing:?}");
    }
    if proposal.counts != counts {
        bail!("review proposal disposition counts are inconsistent");
    }
    for recommendation in &proposal.recommendations {
        if recommendation.target != "environment" {
            bail!(
                "review recommendation target must be \"environment\", got {:?}",
                recommendation.target
            );
        }
        if recommendation.recommendation.trim().is_empty()
            || recommendation.reason.trim().is_empty()
        {
            bail!("review recommendations require nonempty recommendation and reason");
        }
    }
    Ok(())
}

fn validate_request(request: &ReviewRequest) -> Result<()> {
    if request.schema_version != REVIEW_REQUEST_SCHEMA_VERSION {
        bail!(
            "unsupported review request schema {:?}",
            request.schema_version
        );
    }
    Uuid::parse_str(&request.review_id).context("review request review ID is not a UUID")?;
    if request.reviewer_command.is_empty() {
        bail!("review request reviewer command is empty");
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
        normalize_digest(digest).context("review request contains an invalid anchor digest")?;
    }
    for digest in request.input_artifacts.values() {
        normalize_digest(digest)
            .context("review request contains an invalid input-artifact digest")?;
    }
    let mut paths = BTreeSet::new();
    for candidate in &request.candidates {
        if !paths.insert(candidate.path.as_str()) {
            bail!("duplicate review candidate path {:?}", candidate.path);
        }
        let relative = candidate
            .path
            .strip_prefix('/')
            .context("review candidate path must be absolute")?;
        snapshot::validate_relative_path(relative)?;
        let derived_workspace_path =
            workspace_relative(&candidate.path, &request.workspace_guest_path)?;
        match candidate.scope.as_str() {
            "workspace" if candidate.workspace_path == derived_workspace_path => {}
            "environment"
                if candidate.workspace_path.is_none() && derived_workspace_path.is_none() => {}
            _ => bail!(
                "review candidate {:?} has inconsistent scope or workspace path",
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
                "review candidate {:?} has invalid current relation",
                candidate.path
            );
        }
        if candidate.scope == "environment" && candidate.current_relation != "not_applicable" {
            bail!(
                "environment candidate {:?} must use not_applicable current relation",
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
    result: &run::RunResult,
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
            .with_context(|| format!("write review bundle input {filename:?}"))?;
        artifacts.insert(name.to_owned(), run::sha256_bytes(&bytes));
    }
    for (name, filename, bytes) in [
        (
            "run_stdout",
            "run-stdout.bin",
            store.read_run_file(run_id, &result.stdout.path)?,
        ),
        (
            "run_stderr",
            "run-stderr.bin",
            store.read_run_file(run_id, &result.stderr.path)?,
        ),
        (
            "evaluations",
            "evaluations.json",
            run::pretty_json(&evaluation::list(store, run_id)?)?,
        ),
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
            .with_context(|| format!("write review bundle input {filename:?}"))?;
        artifacts.insert(name.to_owned(), run::sha256_bytes(&bytes));
    }
    Ok(artifacts)
}

fn verify_bundle_inputs(
    bundle: &Path,
    request: &ReviewRequest,
    request_bytes: &[u8],
) -> Result<()> {
    let mappings = [
        ("run_spec", "spec.json"),
        ("run_result", "result.json"),
        ("run_stdout", "run-stdout.bin"),
        ("run_stderr", "run-stderr.bin"),
        ("evaluations", "evaluations.json"),
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
            .with_context(|| format!("review request omitted input artifact {name:?}"))?;
        let actual = run::sha256_bytes(
            &fs::read(bundle.join(filename))
                .with_context(|| format!("re-read review bundle input {filename:?}"))?,
        );
        if &actual != expected {
            bail!("reviewer mutated bundle input {filename:?}");
        }
    }
    let actual_request = fs::read(bundle.join("request.json"))?;
    if actual_request != request_bytes {
        bail!("reviewer mutated request.json");
    }
    Ok(())
}

fn review_candidates(
    raw_delta: &DeltaManifest,
    workspace_guest_path: &str,
    base: &Manifest,
    current: &Manifest,
) -> Result<Vec<ReviewCandidate>> {
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
        candidates.push(ReviewCandidate {
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

        let mut recommendation = proposal.clone();
        recommendation
            .recommendations
            .push(DeclarativeRecommendation {
                target: "environment".to_owned(),
                recommendation: "Install the AWS CLI in the Containerfile".to_owned(),
                reason: "The run could not inspect the requested AWS state".to_owned(),
            });
        validate_proposal(&request, &recommendation).unwrap();

        recommendation.recommendations[0].target = "host".to_owned();
        assert!(
            validate_proposal(&request, &recommendation)
                .unwrap_err()
                .to_string()
                .contains("target")
        );
    }

    #[test]
    fn invalid_json_shape_is_reported_as_a_proposal_contract_error() {
        let request = fixture_request();
        let mut value = serde_json::to_value(fixture_proposal(&request)).unwrap();
        value.as_object_mut().unwrap().remove("dispositions");
        let bytes = serde_json::to_vec(&value).unwrap();
        let error = decode_proposal(&request, &bytes).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("reviewer JSON did not match agentlab.review-proposal/v1")
        );
        assert!(format!("{error:#}").contains("dispositions"));
    }

    #[test]
    fn rejected_review_attempt_retains_and_verifies_raw_output() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(Some(temporary.path())).unwrap();
        let request = fixture_request();
        store.create_run_directory(&request.anchors.run_id).unwrap();
        let request_bytes = run::pretty_json(&request).unwrap();
        let now = Utc::now();
        let captures = vec![InvocationCapture {
            attempt: 1,
            started_at: now,
            completed_at: now,
            exit_code: 0,
            status: "invalid_proposal".to_owned(),
            validation_error: Some("missing field dispositions".to_owned()),
            stdout: br#"{"summary":"useful but malformed"}"#.to_vec(),
            stderr: Vec::new(),
        }];
        let record = persist_attempt_record(
            &store,
            &request.anchors.run_id,
            &request.review_id,
            &request.anchors.result_digest,
            "/tmp/workspace",
            now,
            now,
            "rejected",
            Some("proposal contract failed".to_owned()),
            &request,
            &request_bytes,
            &captures,
        )
        .unwrap();

        verify_attempt(&store, &record).unwrap();
        let found = find_attempt(&store, &request.review_id).unwrap();
        assert_eq!(found, record);
        assert_eq!(
            store
                .read_run_file(&record.run_id, &record.invocations[0].stdout.path)
                .unwrap(),
            captures[0].stdout
        );

        store
            .write_run_file(
                &record.run_id,
                &record.invocations[0].stdout.path,
                b"tampered",
            )
            .unwrap();
        assert!(
            verify_attempt(&store, &record)
                .unwrap_err()
                .to_string()
                .contains("integrity")
        );
    }

    fn fixture_request() -> ReviewRequest {
        ReviewRequest {
            schema_version: REVIEW_REQUEST_SCHEMA_VERSION.to_owned(),
            review_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            anchors: fixture_anchors(),
            workspace_guest_path: "/workspace".to_owned(),
            reviewer_command: vec!["reviewer".to_owned()],
            input_artifacts: BTreeMap::new(),
            repositories: ReviewRepositories {
                base: Vec::new(),
                candidate: Vec::new(),
                current: Vec::new(),
            },
            candidates: vec![
                ReviewCandidate {
                    path: "/workspace/safe.txt".to_owned(),
                    change: ChangeKind::Added,
                    scope: "workspace".to_owned(),
                    workspace_path: Some("safe.txt".to_owned()),
                    current_relation: "unchanged_from_base".to_owned(),
                },
                ReviewCandidate {
                    path: "/etc/example.conf".to_owned(),
                    change: ChangeKind::Added,
                    scope: "environment".to_owned(),
                    workspace_path: None,
                    current_relation: "not_applicable".to_owned(),
                },
            ],
        }
    }

    fn fixture_proposal(request: &ReviewRequest) -> ReviewProposal {
        ReviewProposal {
            schema_version: REVIEW_PROPOSAL_SCHEMA_VERSION.to_owned(),
            review_id: request.review_id.clone(),
            anchors: request.anchors.clone(),
            counts: DispositionCounts {
                proposed: 1,
                rejected: 0,
                conflicted: 0,
                unresolved: 1,
            },
            dispositions: vec![
                ReviewDisposition {
                    path: "/workspace/safe.txt".to_owned(),
                    disposition: "proposed".to_owned(),
                    reason: "safe candidate".to_owned(),
                    recommendation: None,
                    workspace_operation: Some(WorkspaceOperation {
                        operation: "replace".to_owned(),
                        path: "safe.txt".to_owned(),
                    }),
                },
                ReviewDisposition {
                    path: "/etc/example.conf".to_owned(),
                    disposition: "unresolved".to_owned(),
                    reason: "requires a declarative environment edit".to_owned(),
                    recommendation: None,
                    workspace_operation: None,
                },
            ],
            recommendations: Vec::new(),
            summary: None,
        }
    }

    fn fixture_anchors() -> ReviewAnchors {
        ReviewAnchors {
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
