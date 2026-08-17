#![cfg(unix)]

use std::fs;
use std::process::Command;

use agentlab::review::{self, ReviewRecord};
use agentlab::run::{self, RunOptions, WorkspaceSource};
use agentlab::snapshot;
use agentlab::store::Store;
use anyhow::{Context, Result, ensure};

struct Cleanup {
    store: Store,
    run_id: String,
    container: String,
    image_tag: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "--force", &self.container])
            .output();
        let _ = Command::new("docker")
            .args(["image", "rm", &self.image_tag])
            .output();
        let _ = self.store.remove_run_directory(&self.run_id);
    }
}

#[test]
#[ignore = "requires a running Docker engine and python3"]
fn review_anchors_three_states_and_applies_nothing() -> Result<()> {
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
        "Review every candidate and do not mutate the current workspace.\n",
    )?;
    fs::write(workspace.join("conflict.txt"), "base conflict\n")?;
    fs::write(workspace.join("reject.txt"), "base reject\n")?;
    initialize_repository(&workspace)?;

    let store = Store::open(Some(&state))?;
    let summary = run::execute(
        &RunOptions {
            workspace: WorkspaceSource::Directory(workspace.clone()),
            workspace_capture_mode: snapshot::CaptureMode::All,
            image: "alpine:3.21".to_owned(),
            command: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "printf 'candidate accepted\\n' > /workspace/accepted.txt; printf 'candidate conflict\\n' > /workspace/conflict.txt; printf 'candidate reject\\n' > /workspace/reject.txt; printf 'environment recommendation\\n' > /etc/agentlab-review.conf"
                    .to_owned(),
            ],
            workspace_guest_path: "/workspace".to_owned(),
            network: "none".to_owned(),
            memory: None,
            cpus: None,
            pi_auth: None,
            change_ignore: None,
            captures: Vec::new(),
        },
        &store,
    )?;
    let compact = summary.run_id.replace('-', "");
    let _cleanup = Cleanup {
        store: store.clone(),
        run_id: summary.run_id.clone(),
        container: summary.retained_container_name.clone(),
        image_tag: format!("agentlab-prepared:{}", &compact[..12]),
    };

    fs::write(workspace.join("conflict.txt"), "current conflict\n")?;
    fs::write(workspace.join("current-only.txt"), "current work\n")?;
    let current_before = snapshot::create(&workspace, &store)?.manifest.digest;
    let output = Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .args(["review", "--json", &summary.run_id, "--workspace"])
        .arg(&workspace)
        .args(["--", "./examples/reviewers/fixture-reviewer.py"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("AGENTLAB_STATE_DIR", &state)
        .output()?;
    ensure!(
        output.status.success(),
        "review failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        String::from_utf8_lossy(&output.stderr).contains("trusted host reviewer"),
        "CLI omitted reviewer trust warning"
    );
    let record: ReviewRecord = serde_json::from_slice(&output.stdout)?;
    ensure!(record.schema_version == "agentlab.review/v1");
    ensure!(record.request.schema_version == "agentlab.review-request/v1");
    ensure!(record.proposal.schema_version == "agentlab.review-proposal/v1");
    ensure!(record.run_id == summary.run_id);
    ensure!(record.source_workspace_unchanged);
    ensure!(!record.agentlab_applied_changes);
    ensure!(record.request.anchors.current_workspace_snapshot_digest == current_before);
    ensure!(record.proposal.counts.proposed == 1);
    ensure!(record.proposal.counts.rejected == 1);
    ensure!(record.proposal.counts.conflicted == 1);
    ensure!(record.proposal.counts.unresolved == 1);
    ensure!(record.request.repositories.base.len() == 1);
    ensure!(record.request.repositories.candidate.len() == 1);
    ensure!(record.request.repositories.current.len() == 1);
    ensure!(record.request.input_artifacts.len() == 9);
    ensure!(std::path::Path::new(&record.request.reviewer_command[0]).is_absolute());
    ensure!(record.request.reviewer_command[0].ends_with("fixture-reviewer.py"));
    ensure!(record.proposal.dispositions.iter().any(|item| {
        item.path == "/workspace/accepted.txt"
            && item.disposition == "proposed"
            && item.workspace_operation.as_ref().is_some_and(|operation| {
                operation.operation == "replace" && operation.path == "accepted.txt"
            })
    }));
    ensure!(record.proposal.dispositions.iter().any(|item| {
        item.path == "/etc/agentlab-review.conf" && item.disposition == "unresolved"
    }));

    let current_after = snapshot::create(&workspace, &store)?.manifest.digest;
    ensure!(current_after == current_before);
    ensure!(!workspace.join("accepted.txt").exists());
    ensure!(fs::read_to_string(workspace.join("reject.txt"))? == "base reject\n");
    ensure!(fs::read_to_string(workspace.join("conflict.txt"))? == "current conflict\n");
    review::verify(&store, &record)?;
    let records = review::list(&store, &summary.run_id)?;
    ensure!(records == [record]);

    let inspect = Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .args(["inspect", "--verify", &summary.run_id])
        .env("AGENTLAB_STATE_DIR", &state)
        .output()?;
    ensure!(
        inspect.status.success(),
        "inspect --verify failed: {}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    ensure!(String::from_utf8_lossy(&inspect.stdout).contains("Reviews: 1"));

    let mutating_reviewer = Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .args(["review", &summary.run_id, "--workspace"])
        .arg(&workspace)
        .args([
            "--",
            "/bin/sh",
            "-c",
            "printf 'mutation' >> \"$AGENTLAB_REVIEW_RAW_DELTA_PATH\"; printf '{}\\n'",
        ])
        .env("AGENTLAB_STATE_DIR", &state)
        .output()?;
    ensure!(!mutating_reviewer.status.success());
    ensure!(
        String::from_utf8_lossy(&mutating_reviewer.stderr)
            .contains("mutated bundle input \"delta.raw.json\"")
    );
    ensure!(review::list(&store, &summary.run_id)?.len() == 1);
    Ok(())
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
