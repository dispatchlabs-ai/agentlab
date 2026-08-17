#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::process::Command;
use std::sync::{Arc, Barrier};

use agentlab::evaluation::{self, EvaluationTable};
use agentlab::run::{self, RunOptions, RunSummary, WorkspaceSource};
use agentlab::snapshot;
use agentlab::store::Store;
use anyhow::{Context, Result, ensure};

struct DockerCleanup {
    store: Store,
    runs: Vec<RunSummary>,
}

impl Drop for DockerCleanup {
    fn drop(&mut self) {
        for run in &self.runs {
            let _ = Command::new("docker")
                .args(["rm", "--force", &run.retained_container_name])
                .output();
            let compact = run.run_id.replace('-', "");
            let _ = Command::new("docker")
                .args([
                    "image",
                    "rm",
                    &format!("agentlab-prepared:{}", &compact[..12]),
                ])
                .output();
            let _ = self.store.remove_run_directory(&run.run_id);
        }
    }
}

#[test]
#[ignore = "requires a running Docker engine and python3 for the supplied evaluator"]
fn external_evaluator_reports_real_input_identities_and_scores() -> Result<()> {
    ensure!(
        Command::new("docker")
            .arg("info")
            .output()?
            .status
            .success(),
        "Docker is not available"
    );
    ensure!(
        Command::new("python3")
            .arg("--version")
            .output()?
            .status
            .success(),
        "python3 is not available"
    );
    let temporary = tempfile::tempdir()?;
    let workspace = temporary.path().join("workspace");
    let state = temporary.path().join("state");
    fs::create_dir(&workspace)?;
    fs::write(workspace.join("source.txt"), "immutable\n")?;
    let store = Store::open(Some(&state))?;
    let without_skill = snapshot::create(&workspace, &store)?.manifest.digest;
    fs::create_dir_all(workspace.join("skills/review"))?;
    fs::write(
        workspace.join("skills/review/SKILL.md"),
        "# Review skill\n\nInspect the change carefully.\n",
    )?;
    let with_skill = snapshot::create(&workspace, &store)?.manifest.digest;
    ensure!(without_skill != with_skill);

    let inputs = [
        without_skill.clone(),
        without_skill.clone(),
        with_skill.clone(),
        with_skill.clone(),
    ];
    let barrier = Arc::new(Barrier::new(inputs.len()));
    let mut threads = Vec::new();
    for workspace_snapshot in inputs {
        let barrier = barrier.clone();
        let store = store.clone();
        threads.push(std::thread::spawn(move || {
            let options = RunOptions {
                workspace: WorkspaceSource::Snapshot(workspace_snapshot),
                workspace_capture_mode: snapshot::CaptureMode::All,
                image: "alpine:3.21".to_owned(),
                command: vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    "if test -f /workspace/skills/review/SKILL.md; then treatment=with-skill; else treatment=without-skill; fi; printf '%s:%s\\n' \"$treatment\" \"$HOSTNAME\" > /workspace/private-result.txt"
                        .to_owned(),
                ],
                workspace_guest_path: "/workspace".to_owned(),
                network: "none".to_owned(),
                memory: None,
                cpus: None,
                pi_auth: None,
                change_ignore: None,
                captures: Vec::new(),
                accepted_input: None,
            };
            barrier.wait();
            run::execute(&options, &store)
        }));
    }
    let mut runs = Vec::new();
    for thread in threads {
        runs.push(
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("experiment run panicked"))??,
        );
    }
    let _cleanup = DockerCleanup {
        store: store.clone(),
        runs: runs.clone(),
    };
    let run_ids: Vec<_> = runs.iter().map(|run| run.run_id.clone()).collect();

    let binary = env!("CARGO_BIN_EXE_agentlab");
    let evaluator = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/evaluators/result-facts.sh");
    let mut evaluate = Command::new(binary);
    evaluate
        .env("AGENTLAB_STATE_DIR", &state)
        .args(["evaluate", "--name", "result-facts"])
        .args(&run_ids)
        .arg("--")
        .arg(&evaluator);
    let output = evaluate.output().context("run public evaluate command")?;
    ensure!(
        output.status.success(),
        "evaluate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for run_id in &run_ids {
        let records = evaluation::list(&store, run_id)?;
        ensure!(records.len() == 1);
        ensure!(records[0].status == "succeeded");
        ensure!(records[0].evaluator_name == "result-facts");
        ensure!(records[0].output.as_ref().is_some_and(|payload| {
            payload
                .scores
                .get("exit_zero")
                .and_then(|value| value.as_i64())
                == Some(1)
                && payload
                    .scores
                    .get("portable_changes")
                    .and_then(|value| value.as_u64())
                    == Some(1)
        }));
        evaluation::verify_all(&store, run_id)?;
    }

    let mut report = Command::new(binary);
    report
        .env("AGENTLAB_STATE_DIR", &state)
        .args([
            "report",
            "--evaluator",
            "result-facts",
            "--score",
            "exit_zero",
            "--score",
            "portable_changes",
        ])
        .args(&run_ids);
    let output = report.output().context("run public report command")?;
    ensure!(output.status.success());
    let markdown = String::from_utf8(output.stdout)?;
    ensure!(markdown.contains("| run | input | workspace | image | base | evaluator |"));
    ensure!(markdown.contains("score:exit_zero"));
    ensure!(markdown.contains("score:portable_changes"));
    ensure!(markdown.matches("| result-facts |").count() == 4);
    ensure!(markdown.contains("no aggregation, statistical test, ranking, or causal inference"));

    let table = evaluation::table(
        &store,
        &run_ids,
        Some("result-facts"),
        &["exit_zero".to_owned(), "portable_changes".to_owned()],
    )?;
    ensure!(table.rows.len() == 4);
    ensure!(table.score_columns == ["exit_zero", "portable_changes"]);
    let mut input_counts = BTreeMap::new();
    let mut workspace_inputs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in &table.rows {
        *input_counts
            .entry(row.run_input_digest.clone())
            .or_insert(0_usize) += 1;
        workspace_inputs
            .entry(row.workspace_snapshot_digest.clone())
            .or_default()
            .insert(row.run_input_digest.clone());
    }
    ensure!(input_counts.len() == 2);
    ensure!(input_counts.values().all(|count| *count == 2));
    ensure!(workspace_inputs.len() == 2);
    ensure!(workspace_inputs.values().all(|inputs| inputs.len() == 1));
    ensure!(workspace_inputs.contains_key(&without_skill));
    ensure!(workspace_inputs.contains_key(&with_skill));

    let without_comparison = run::compare_runs(&store, &run_ids[0], &run_ids[1])?;
    let with_comparison = run::compare_runs(&store, &run_ids[2], &run_ids[3])?;
    let treatment_comparison = run::compare_runs(&store, &run_ids[0], &run_ids[2])?;
    ensure!(without_comparison.comparable_repetition);
    ensure!(with_comparison.comparable_repetition);
    ensure!(treatment_comparison.comparison_kind == "different_inputs");
    ensure!(treatment_comparison.controlled_input_differences == ["workspace_snapshot_digest"]);

    let invalid = evaluation::evaluate(
        &store,
        &run_ids[0],
        "invalid-fixture",
        &[
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "printf 'not json'".to_owned(),
        ],
    )?;
    ensure!(invalid.status == "invalid_output");
    ensure!(invalid.output.is_none());
    evaluation::verify(&store, &invalid)?;
    let failed = evaluation::evaluate(
        &store,
        &run_ids[1],
        "failed-fixture",
        &[
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "printf 'diagnostic' >&2; exit 42".to_owned(),
        ],
    )?;
    ensure!(failed.status == "command_failed");
    ensure!(failed.exit_code == 42);
    ensure!(failed.output.is_none());
    evaluation::verify(&store, &failed)?;
    let latest_success = evaluation::table(&store, &run_ids, Some("result-facts"), &[])?;
    ensure!(latest_success.rows.len() == 4);

    let json = serde_json::to_vec(&table)?;
    let decoded: EvaluationTable = serde_json::from_slice(&json)?;
    ensure!(decoded == table);
    let source_after = snapshot::create(&workspace, &store)?.manifest.digest;
    ensure!(with_skill == source_after, "source workspace was mutated");
    ensure!(!workspace.join("private-result.txt").exists());
    Ok(())
}
