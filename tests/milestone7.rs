#![cfg(unix)]

use std::fs;
use std::process::{Command, Output};

use agentlab::acceptance::{self, AcceptanceRecord};
use agentlab::apply::{self, ApplyRecord};
use agentlab::review::ReviewRecord;
use agentlab::run::{self, RunOptions, RunSummary, WorkspaceSource};
use agentlab::snapshot;
use agentlab::store::Store;
use anyhow::{Context, Result, ensure};

struct Cleanup {
    store: Store,
    runs: Vec<RunSummary>,
}

impl Cleanup {
    fn retain(&mut self, run: RunSummary) -> RunSummary {
        self.runs.push(run.clone());
        run
    }
}

impl Drop for Cleanup {
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
#[ignore = "requires a running Docker engine and python3"]
fn accepted_input_to_reviewed_improvement_preserves_complete_lineage() -> Result<()> {
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
    fs::write(
        workspace.join("AGENTS.md"),
        "Review every candidate and preserve only explicitly accepted changes.\n",
    )?;
    fs::write(workspace.join(".agentlabignore"), "session.tmp\n")?;
    fs::write(workspace.join("conflict.txt"), "base conflict\n")?;
    fs::write(workspace.join("reject.txt"), "base reject\n")?;
    initialize_repository(&workspace)?;

    let store = Store::open(Some(&state))?;
    let initial_snapshot = snapshot::create(&workspace, &store)?.manifest.digest;
    let mut cleanup = Cleanup {
        store: store.clone(),
        runs: Vec::new(),
    };

    let seed = cleanup.retain(run::execute(
        &RunOptions {
            workspace: WorkspaceSource::Snapshot(initial_snapshot),
            workspace_capture_mode: snapshot::CaptureMode::All,
            image: "alpine:3.21".to_owned(),
            command: vec!["/bin/true".to_owned()],
            workspace_guest_path: "/workspace".to_owned(),
            network: "none".to_owned(),
            memory: None,
            cpus: None,
            pi_auth: None,
            secret_files: Vec::new(),
            change_ignore: None,
            captures: Vec::new(),
            accepted_input: None,
        },
        &store,
    )?);
    let initial_accept_output = Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .args(["accept", &seed.run_id])
        .env("AGENTLAB_STATE_DIR", &state)
        .output()?;
    ensure_success(&initial_accept_output, "accept initial tested input")?;
    let initial_accept_text = String::from_utf8(initial_accept_output.stdout)?;
    let initial_acceptance_id = output_value(&initial_accept_text, "Acceptance: ")?;
    ensure!(initial_accept_text.contains(&format!(
        "Run accepted input: agentlab run --accepted {initial_acceptance_id} -- COMMAND"
    )));
    ensure!(initial_accept_text.contains(&format!(
        "Inspect: agentlab inspect --verify {initial_acceptance_id}"
    )));
    let initial_acceptance = acceptance::find(&store, initial_acceptance_id)?;
    ensure!(initial_acceptance.kind == "tested_input");
    ensure!(initial_acceptance.applied_lineage.is_none());
    ensure!(initial_acceptance.tested_by_run_id == seed.run_id);

    let candidate_script = "printf 'candidate accepted\\n' > /workspace/accepted.txt; printf 'candidate conflict\\n' > /workspace/conflict.txt; printf 'candidate reject\\n' > /workspace/reject.txt; printf 'ignored session debris\\n' > /workspace/session.tmp; printf 'environment recommendation\\n' > /etc/agentlab-review.conf";
    let candidate_a = cleanup.retain(run_accepted(
        &state,
        &initial_acceptance.acceptance_id,
        candidate_script,
    )?);
    let candidate_b = cleanup.retain(run_accepted(
        &state,
        &initial_acceptance.acceptance_id,
        candidate_script,
    )?);
    let comparison = run::compare_runs(&store, &candidate_a.run_id, &candidate_b.run_id)?;
    ensure!(comparison.comparable_repetition);
    ensure!(comparison.distinct_private_containers);
    for run_id in [&candidate_a.run_id, &candidate_b.run_id] {
        let spec = run::load_spec(&store, run_id)?;
        ensure!(
            spec.accepted_input.as_ref().is_some_and(
                |reference| reference.acceptance_id == initial_acceptance.acceptance_id
            )
        );
    }

    fs::write(workspace.join("conflict.txt"), "current conflict\n")?;
    fs::write(workspace.join("current-only.txt"), "current work\n")?;
    let reviewer = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/reviewers/fixture-reviewer.py");
    let review_output = Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .args(["review", "--json", &candidate_a.run_id, "--workspace"])
        .arg(&workspace)
        .arg("--")
        .arg(&reviewer)
        .env("AGENTLAB_STATE_DIR", &state)
        .output()?;
    ensure_success(&review_output, "review candidate A")?;
    let review: ReviewRecord = serde_json::from_slice(&review_output.stdout)?;
    ensure!(review.proposal.counts.proposed == 1);
    ensure!(review.proposal.counts.rejected == 1);
    ensure!(review.proposal.counts.conflicted == 1);
    ensure!(review.proposal.counts.unresolved == 2);

    let apply_output = Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .args([
            "apply",
            "--acknowledge-conflicts",
            "--acknowledge-unresolved",
            &review.review_id,
            "--workspace",
        ])
        .arg(&workspace)
        .env("AGENTLAB_STATE_DIR", &state)
        .output()?;
    ensure_success(&apply_output, "apply reviewed candidate")?;
    let apply_text = String::from_utf8(apply_output.stdout)?;
    let applied_id = output_value(&apply_text, "Apply: ")?;
    ensure!(apply_text.contains("Retest exact applied input: agentlab run --snapshot "));
    ensure!(apply_text.contains(&format!(
        "Accept after retest: agentlab accept RETEST_RUN --from-apply {applied_id}"
    )));
    let applied: ApplyRecord = apply::list(&store, &candidate_a.run_id)?
        .into_iter()
        .find(|record| record.apply_id == applied_id)
        .context("human apply output did not name its stored record")?;
    ensure!(fs::read_to_string(workspace.join("accepted.txt"))? == "candidate accepted\n");
    ensure!(fs::read_to_string(workspace.join("reject.txt"))? == "base reject\n");
    ensure!(fs::read_to_string(workspace.join("conflict.txt"))? == "current conflict\n");
    ensure!(!workspace.join("session.tmp").exists());
    ensure!(!workspace.join("etc/agentlab-review.conf").exists());

    let wrong_retest = Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .args([
            "accept",
            &candidate_b.run_id,
            "--from-apply",
            &applied.apply_id,
        ])
        .env("AGENTLAB_STATE_DIR", &state)
        .output()?;
    ensure!(!wrong_retest.status.success());
    ensure!(
        String::from_utf8_lossy(&wrong_retest.stderr).contains("does not match applied workspace")
    );

    let retest = cleanup.retain(run_snapshot(
        &state,
        &applied.after_workspace_snapshot_digest,
        "test \"$(cat /workspace/accepted.txt)\" = 'candidate accepted'; test \"$(cat /workspace/reject.txt)\" = 'base reject'; test ! -e /workspace/session.tmp; test ! -e /etc/agentlab-review.conf",
    )?);
    ensure!(retest.exit_code == 0);
    let improved_acceptance: AcceptanceRecord = json_command(
        &state,
        &[
            "accept",
            "--json",
            &retest.run_id,
            "--from-apply",
            &applied.apply_id,
        ],
    )?;
    ensure!(improved_acceptance.kind == "reviewed_application");
    ensure!(
        improved_acceptance
            .parent_accepted_input
            .as_ref()
            .is_some_and(|reference| reference.acceptance_id == initial_acceptance.acceptance_id)
    );
    let lineage = improved_acceptance
        .applied_lineage
        .as_ref()
        .context("improved acceptance omitted apply lineage")?;
    ensure!(lineage.candidate_run_id == candidate_a.run_id);
    ensure!(lineage.review_id == review.review_id);
    ensure!(lineage.apply_id == applied.apply_id);

    let improved = snapshot::load(&store, &improved_acceptance.workspace_snapshot_digest)?;
    let materialized = temporary.path().join("accepted-input");
    snapshot::materialize(&store, &improved, &materialized)?;
    ensure!(fs::read_to_string(materialized.join("accepted.txt"))? == "candidate accepted\n");
    ensure!(fs::read_to_string(materialized.join("reject.txt"))? == "base reject\n");
    ensure!(fs::read_to_string(materialized.join("conflict.txt"))? == "current conflict\n");
    ensure!(!materialized.join("session.tmp").exists());

    let improved_run = cleanup.retain(run_accepted(
        &state,
        &improved_acceptance.acceptance_id,
        "test \"$(cat /workspace/accepted.txt)\" = 'candidate accepted'; test \"$(cat /workspace/reject.txt)\" = 'base reject'; test ! -e /workspace/session.tmp; test ! -e /etc/agentlab-review.conf",
    )?);
    ensure!(improved_run.exit_code == 0);
    let improved_spec = run::load_spec(&store, &improved_run.run_id)?;
    ensure!(
        improved_spec
            .accepted_input
            .as_ref()
            .is_some_and(|reference| reference.acceptance_id == improved_acceptance.acceptance_id)
    );
    ensure!(improved_spec.workspace_snapshot_digest == applied.after_workspace_snapshot_digest);

    let inspect = Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .args(["inspect", "--verify", &improved_acceptance.acceptance_id])
        .env("AGENTLAB_STATE_DIR", &state)
        .output()?;
    ensure_success(&inspect, "inspect improved acceptance")?;
    let inspect_text = String::from_utf8(inspect.stdout)?;
    ensure!(inspect_text.contains("Kind: reviewed_application"));
    ensure!(inspect_text.contains(&format!("Candidate run: {}", candidate_a.run_id)));
    ensure!(inspect_text.contains(&format!("Review: {}", review.review_id)));
    ensure!(inspect_text.contains(&format!("Apply: {}", applied.apply_id)));
    ensure!(inspect_text.contains("Integrity: verified"));

    let protected = Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .args(["rm", &candidate_a.run_id])
        .env("AGENTLAB_STATE_DIR", &state)
        .output()?;
    ensure!(!protected.status.success());
    ensure!(String::from_utf8_lossy(&protected.stderr).contains("preserved by accepted lineage"));
    Ok(())
}

fn run_accepted(state: &std::path::Path, acceptance_id: &str, script: &str) -> Result<RunSummary> {
    json_command(
        state,
        &[
            "run",
            "--json",
            "--accepted",
            acceptance_id,
            "--network",
            "none",
            "--",
            "/bin/sh",
            "-c",
            script,
        ],
    )
}

fn run_snapshot(state: &std::path::Path, digest: &str, script: &str) -> Result<RunSummary> {
    json_command(
        state,
        &[
            "run",
            "--json",
            "--snapshot",
            digest,
            "--image",
            "alpine:3.21",
            "--network",
            "none",
            "--",
            "/bin/sh",
            "-c",
            script,
        ],
    )
}

fn json_command<T: serde::de::DeserializeOwned>(
    state: &std::path::Path,
    arguments: &[&str],
) -> Result<T> {
    let output = Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .args(arguments)
        .env("AGENTLAB_STATE_DIR", state)
        .output()?;
    ensure_success(
        &output,
        arguments.first().copied().unwrap_or("AgentLab command"),
    )?;
    serde_json::from_slice(&output.stdout).context("decode AgentLab JSON output")
}

fn ensure_success(output: &Output, operation: &str) -> Result<()> {
    ensure!(
        output.status.success(),
        "{operation} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn output_value<'a>(output: &'a str, prefix: &str) -> Result<&'a str> {
    output
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .with_context(|| format!("command output omitted {prefix:?}"))
}

fn initialize_repository(workspace: &std::path::Path) -> Result<()> {
    for arguments in [
        vec!["init", "--quiet", "--initial-branch=main"],
        vec!["config", "user.name", "AgentLab Fixture"],
        vec!["config", "user.email", "fixture@agentlab.invalid"],
        vec!["add", "."],
        vec!["commit", "--quiet", "-m", "base"],
    ] {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(workspace)
            .output()
            .context("initialize fixture repository")?;
        ensure!(
            output.status.success(),
            "initialize fixture repository: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
