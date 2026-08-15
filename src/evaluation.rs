use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::run::{self, Artifact, RunSpec};
use crate::store::Store;

pub const EVALUATION_SCHEMA_VERSION: &str = "agentlab.evaluation/v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluatorPayload {
    #[serde(default)]
    pub scores: BTreeMap<String, Value>,
    #[serde(default)]
    pub observations: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationRecord {
    pub schema_version: String,
    pub digest: String,
    pub evaluation_id: String,
    pub run_id: String,
    pub result_digest: String,
    pub evaluator_name: String,
    pub command: Vec<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub exit_code: i64,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<EvaluatorPayload>,
    pub stdout: Artifact,
    pub stderr: Artifact,
    pub warnings: Vec<String>,
    pub integrity: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationTable {
    pub factor_columns: Vec<String>,
    pub score_columns: Vec<String>,
    pub rows: Vec<EvaluationTableRow>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationTableRow {
    pub run_id: String,
    pub evaluation_id: String,
    pub evaluator_name: String,
    pub result_digest: String,
    pub factors: BTreeMap<String, String>,
    pub scores: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Serialize)]
struct EvaluationIdentity<'a> {
    schema_version: &'a str,
    evaluation_id: &'a str,
    run_id: &'a str,
    result_digest: &'a str,
    evaluator_name: &'a str,
    command: &'a [String],
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    exit_code: i64,
    status: &'a str,
    output: &'a Option<EvaluatorPayload>,
    stdout: &'a Artifact,
    stderr: &'a Artifact,
    warnings: &'a [String],
    integrity: &'a BTreeMap<String, String>,
}

pub fn evaluate(
    store: &Store,
    run_id: &str,
    evaluator_name: &str,
    command: &[String],
) -> Result<EvaluationRecord> {
    if evaluator_name.trim().is_empty() {
        bail!("evaluator name cannot be empty");
    }
    if command.is_empty() {
        bail!("evaluate requires a command after --");
    }
    let result = run::load_result(store, run_id)?;
    crate::lifecycle::verify_all(store, run_id)?;
    verify_all(store, run_id)?;
    let run_directory = store.run_directory(run_id)?;
    let evaluation_id = Uuid::new_v4().to_string();
    let prefix = format!("evaluations/{evaluation_id}");

    let started_at = Utc::now();
    let output = Command::new(&command[0])
        .args(&command[1..])
        .env("AGENTLAB_RUN_ID", run_id)
        .env("AGENTLAB_RUN_DIR", &run_directory)
        .env("AGENTLAB_RESULT_PATH", run_directory.join("result.json"))
        .env("AGENTLAB_SPEC_PATH", run_directory.join("spec.json"))
        .env("AGENTLAB_DELTA_PATH", run_directory.join("delta.json"))
        .env(
            "AGENTLAB_RAW_DELTA_PATH",
            run_directory.join("delta.raw.json"),
        )
        .output()
        .with_context(|| format!("execute evaluator command {:?}", command[0]))?;
    let completed_at = Utc::now();
    fs::create_dir_all(store.run_path(run_id, &format!("{prefix}/artifacts"))?)?;
    let exit_code = output.status.code().map(i64::from).unwrap_or(-1);
    let stdout = write_artifact(
        store,
        run_id,
        &format!("{prefix}/artifacts/stdout.bin"),
        &output.stdout,
    )?;
    let stderr = write_artifact(
        store,
        run_id,
        &format!("{prefix}/artifacts/stderr.bin"),
        &output.stderr,
    )?;

    let mut warnings = Vec::new();
    let (status, payload) = if output.status.success() {
        match serde_json::from_slice::<EvaluatorPayload>(&output.stdout) {
            Ok(payload) => match validate_payload(&payload) {
                Ok(()) => ("succeeded".to_owned(), Some(payload)),
                Err(error) => {
                    warnings.push(format!(
                        "evaluator output violated the JSON contract: {error}"
                    ));
                    ("invalid_output".to_owned(), None)
                }
            },
            Err(error) => {
                warnings.push(format!(
                    "evaluator stdout was not a valid JSON object: {error}"
                ));
                ("invalid_output".to_owned(), None)
            }
        }
    } else {
        warnings.push(format!("evaluator command exited with status {exit_code}"));
        ("command_failed".to_owned(), None)
    };

    // Evaluators receive direct paths for efficient local analysis, but they are observers.
    // Re-verification makes mutation of the immutable input an explicit failure.
    crate::lifecycle::verify_all(store, run_id)
        .context("evaluator mutated immutable run or lifecycle artifacts")?;
    verify_all(store, run_id).context("evaluator mutated prior evaluation artifacts")?;

    let mut integrity = BTreeMap::new();
    integrity.insert(stdout.path.clone(), stdout.digest.clone());
    integrity.insert(stderr.path.clone(), stderr.digest.clone());
    let identity = EvaluationIdentity {
        schema_version: EVALUATION_SCHEMA_VERSION,
        evaluation_id: &evaluation_id,
        run_id,
        result_digest: &result.digest,
        evaluator_name,
        command,
        started_at,
        completed_at,
        exit_code,
        status: &status,
        output: &payload,
        stdout: &stdout,
        stderr: &stderr,
        warnings: &warnings,
        integrity: &integrity,
    };
    let record = EvaluationRecord {
        schema_version: EVALUATION_SCHEMA_VERSION.to_owned(),
        digest: run::sha256_bytes(&serde_json::to_vec(&identity)?),
        evaluation_id: evaluation_id.clone(),
        run_id: run_id.to_owned(),
        result_digest: result.digest,
        evaluator_name: evaluator_name.to_owned(),
        command: command.to_vec(),
        started_at,
        completed_at,
        exit_code,
        status,
        output: payload,
        stdout,
        stderr,
        warnings,
        integrity,
    };
    store.write_run_file(
        run_id,
        &format!("{prefix}/evaluation.json"),
        &run::pretty_json(&record)?,
    )?;
    Ok(record)
}

pub fn list(store: &Store, run_id: &str) -> Result<Vec<EvaluationRecord>> {
    let directory = store.run_path(run_id, "evaluations")?;
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
            .map_err(|_| anyhow::anyhow!("evaluation ID is not valid UTF-8"))?;
        let path = format!("evaluations/{id}/evaluation.json");
        if store.run_file_exists(run_id, &path)? {
            records.push(serde_json::from_slice(
                &store.read_run_file(run_id, &path)?,
            )?);
        }
    }
    records.sort_by(|left: &EvaluationRecord, right: &EvaluationRecord| {
        left.completed_at
            .cmp(&right.completed_at)
            .then_with(|| left.evaluation_id.cmp(&right.evaluation_id))
    });
    Ok(records)
}

pub fn verify_all(store: &Store, run_id: &str) -> Result<()> {
    for record in list(store, run_id)? {
        verify(store, &record)?;
    }
    Ok(())
}

pub fn verify(store: &Store, record: &EvaluationRecord) -> Result<()> {
    if record.schema_version != EVALUATION_SCHEMA_VERSION {
        bail!("unsupported evaluation schema {:?}", record.schema_version);
    }
    for (relative, expected) in &record.integrity {
        let actual = run::sha256_bytes(&store.read_run_file(&record.run_id, relative)?);
        if &actual != expected {
            bail!("evaluation artifact integrity mismatch for {relative:?}");
        }
    }
    let identity = EvaluationIdentity {
        schema_version: EVALUATION_SCHEMA_VERSION,
        evaluation_id: &record.evaluation_id,
        run_id: &record.run_id,
        result_digest: &record.result_digest,
        evaluator_name: &record.evaluator_name,
        command: &record.command,
        started_at: record.started_at,
        completed_at: record.completed_at,
        exit_code: record.exit_code,
        status: &record.status,
        output: &record.output,
        stdout: &record.stdout,
        stderr: &record.stderr,
        warnings: &record.warnings,
        integrity: &record.integrity,
    };
    if run::sha256_bytes(&serde_json::to_vec(&identity)?) != record.digest {
        bail!("evaluation record integrity mismatch");
    }
    Ok(())
}

pub fn table(
    store: &Store,
    run_ids: &[String],
    evaluator_name: Option<&str>,
    requested_factors: &[String],
    requested_scores: &[String],
) -> Result<EvaluationTable> {
    if run_ids.is_empty() {
        bail!("report requires at least one RUN");
    }
    reject_duplicates("--factor", requested_factors)?;
    reject_duplicates("--score", requested_scores)?;
    let mut selected = Vec::new();
    let mut all_factors = BTreeSet::new();
    let mut all_scores = BTreeSet::new();
    for run_id in run_ids {
        let spec: RunSpec = run::load_spec(store, run_id)?;
        let result = run::load_result(store, run_id)?;
        run::verify_result(store, &result)?;
        let record = list(store, run_id)?
            .into_iter()
            .rfind(|record| {
                record.status == "succeeded"
                    && evaluator_name.is_none_or(|name| record.evaluator_name == name)
            })
            .with_context(|| {
                format!(
                    "run {run_id} has no successful evaluation{}",
                    evaluator_name
                        .map(|name| format!(" named {name:?}"))
                        .unwrap_or_default()
                )
            })?;
        verify(store, &record)?;
        let payload = record
            .output
            .as_ref()
            .context("successful evaluation omitted structured output")?;
        all_factors.extend(spec.factors.keys().cloned());
        all_scores.extend(payload.scores.keys().cloned());
        selected.push((spec, record));
    }
    let factor_columns = if requested_factors.is_empty() {
        all_factors.into_iter().collect()
    } else {
        requested_factors.to_vec()
    };
    let score_columns = if requested_scores.is_empty() {
        all_scores.into_iter().collect()
    } else {
        requested_scores.to_vec()
    };
    let mut rows = Vec::new();
    for (spec, record) in selected {
        let payload = record.output.as_ref().expect("validated payload");
        rows.push(EvaluationTableRow {
            run_id: record.run_id.clone(),
            evaluation_id: record.evaluation_id.clone(),
            evaluator_name: record.evaluator_name.clone(),
            result_digest: record.result_digest.clone(),
            factors: factor_columns
                .iter()
                .filter_map(|key| {
                    spec.factors
                        .get(key)
                        .map(|value| (key.clone(), value.clone()))
                })
                .collect(),
            scores: score_columns
                .iter()
                .filter_map(|key| {
                    payload
                        .scores
                        .get(key)
                        .map(|value| (key.clone(), value.clone()))
                })
                .collect(),
            summary: payload.summary.clone(),
        });
    }
    rows.sort_by(|left, right| {
        factor_columns
            .iter()
            .map(|key| left.factors.get(key))
            .cmp(factor_columns.iter().map(|key| right.factors.get(key)))
            .then_with(|| left.run_id.cmp(&right.run_id))
    });
    Ok(EvaluationTable {
        factor_columns,
        score_columns,
        rows,
        warnings: vec![
            "scores are evaluator-specific observations, not universal AgentLab judgments"
                .to_owned(),
            "agent and external-service behavior may be nondeterministic; use multiple replicates and interpret variance externally"
                .to_owned(),
            "this report aligns rows only; it performs no aggregation, statistical test, ranking, or causal inference"
                .to_owned(),
        ],
    })
}

pub fn markdown_table(table: &EvaluationTable) -> String {
    let mut headers = vec!["run".to_owned(), "evaluator".to_owned()];
    headers.extend(
        table
            .factor_columns
            .iter()
            .map(|column| format!("factor:{column}")),
    );
    headers.extend(
        table
            .score_columns
            .iter()
            .map(|column| format!("score:{column}")),
    );
    let mut output = String::new();
    output.push_str("| ");
    output.push_str(&headers.join(" | "));
    output.push_str(" |\n|");
    for _ in &headers {
        output.push_str(" --- |");
    }
    output.push('\n');
    for row in &table.rows {
        let mut values = vec![row.run_id.clone(), row.evaluator_name.clone()];
        values.extend(
            table
                .factor_columns
                .iter()
                .map(|key| row.factors.get(key).cloned().unwrap_or_default()),
        );
        values.extend(
            table
                .score_columns
                .iter()
                .map(|key| row.scores.get(key).map(render_value).unwrap_or_default()),
        );
        output.push_str("| ");
        output.push_str(
            &values
                .into_iter()
                .map(|value| value.replace('|', "\\|"))
                .collect::<Vec<_>>()
                .join(" | "),
        );
        output.push_str(" |\n");
    }
    output
}

fn validate_payload(payload: &EvaluatorPayload) -> Result<()> {
    for (key, value) in &payload.scores {
        if key.is_empty() {
            bail!("evaluator score keys cannot be empty");
        }
        if value.is_array() || value.is_object() {
            bail!("evaluator score {key:?} must be a JSON scalar");
        }
    }
    if payload.observations.keys().any(String::is_empty) {
        bail!("evaluator observation keys cannot be empty");
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

fn reject_duplicates(flag: &str, values: &[String]) -> Result<()> {
    let unique: BTreeSet<_> = values.iter().collect();
    if unique.len() != values.len() {
        bail!("duplicate {flag} value");
    }
    Ok(())
}

fn render_value(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}
