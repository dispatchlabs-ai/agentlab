#![cfg(unix)]

use std::fs;
use std::process::Command;

use agentlab::apply::{self, ApplyRecord};
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
fn review_and_receipt_bound_apply_preserve_authorization_boundaries() -> Result<()> {
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
            accepted_input: None,
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
    ensure!(record.source_workspace == fs::canonicalize(&workspace)?.to_string_lossy());
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
    ensure!(record.request.input_artifacts.len() == 12);
    let attempt = review::find_attempt(&store, &record.review_id)?;
    ensure!(attempt.status == "accepted");
    ensure!(attempt.invocations.len() == 1);
    ensure!(attempt.invocations[0].status == "accepted");
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
    ensure!(records.as_slice() == std::slice::from_ref(&record));

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

    let blocked_conflict = apply_command(&record.review_id, &workspace, &state, &[])?;
    ensure!(!blocked_conflict.status.success());
    ensure!(
        String::from_utf8_lossy(&blocked_conflict.stderr)
            .contains("review contains 1 conflicted candidate(s)")
    );
    ensure!(!workspace.join("accepted.txt").exists());

    let blocked_unresolved = apply_command(
        &record.review_id,
        &workspace,
        &state,
        &["--acknowledge-conflicts"],
    )?;
    ensure!(!blocked_unresolved.status.success());
    ensure!(
        String::from_utf8_lossy(&blocked_unresolved.stderr)
            .contains("review contains 1 unresolved candidate(s)")
    );
    ensure!(!workspace.join("accepted.txt").exists());

    fs::write(workspace.join("current-only.txt"), "stale work\n")?;
    let stale = apply_command(
        &record.review_id,
        &workspace,
        &state,
        &["--acknowledge-conflicts", "--acknowledge-unresolved"],
    )?;
    ensure!(!stale.status.success());
    ensure!(String::from_utf8_lossy(&stale.stderr).contains("current workspace is stale"));
    ensure!(!workspace.join("accepted.txt").exists());
    fs::write(workspace.join("current-only.txt"), "current work\n")?;
    ensure!(snapshot::create(&workspace, &store)?.manifest.digest == current_before);

    let alternate_workspace = temporary.path().join("alternate-workspace");
    let reviewed_current = snapshot::load(&store, &current_before)?;
    snapshot::materialize(&store, &reviewed_current, &alternate_workspace)?;
    let wrong_workspace = apply_command(
        &record.review_id,
        &alternate_workspace,
        &state,
        &["--acknowledge-conflicts", "--acknowledge-unresolved"],
    )?;
    ensure!(!wrong_workspace.status.success());
    ensure!(String::from_utf8_lossy(&wrong_workspace.stderr).contains("was created for workspace"));
    ensure!(!alternate_workspace.join("accepted.txt").exists());

    let lock_relative = format!("reviews/{}/apply.lock", record.review_id);
    store.write_run_file(&summary.run_id, &lock_relative, b"interrupted fixture\n")?;
    let locked = apply_command(
        &record.review_id,
        &workspace,
        &state,
        &["--acknowledge-conflicts", "--acknowledge-unresolved"],
    )?;
    ensure!(!locked.status.success());
    ensure!(
        String::from_utf8_lossy(&locked.stderr).contains("already in progress or was interrupted")
    );
    ensure!(!workspace.join("accepted.txt").exists());
    fs::remove_file(
        state
            .join("runs")
            .join(&summary.run_id)
            .join(&lock_relative),
    )?;

    let applied = apply_command(
        &record.review_id,
        &workspace,
        &state,
        &[
            "--acknowledge-conflicts",
            "--acknowledge-unresolved",
            "--json",
        ],
    )?;
    ensure!(
        applied.status.success(),
        "apply failed: {}",
        String::from_utf8_lossy(&applied.stderr)
    );
    ensure!(
        String::from_utf8_lossy(&applied.stderr)
            .contains("applying only receipt-authorized workspace paths")
    );
    let apply_record: ApplyRecord = serde_json::from_slice(&applied.stdout)?;
    ensure!(apply_record.schema_version == "agentlab.apply/v1");
    ensure!(apply_record.review_id == record.review_id);
    ensure!(apply_record.review_digest == record.digest);
    ensure!(apply_record.before_workspace_snapshot_digest == current_before);
    ensure!(
        apply_record.intended_workspace_snapshot_digest
            == apply_record.after_workspace_snapshot_digest
    );
    ensure!(apply_record.source_workspace_matched_review);
    ensure!(apply_record.result_workspace_verified);
    ensure!(apply_record.acknowledged_conflicts);
    ensure!(apply_record.acknowledged_unresolved);
    ensure!(apply_record.counts.proposed == 1);
    ensure!(apply_record.counts.rejected == 1);
    ensure!(apply_record.counts.conflicted == 1);
    ensure!(apply_record.counts.unresolved == 1);
    ensure!(apply_record.counts.applied == 1);
    ensure!(apply_record.operations.len() == 1);
    ensure!(apply_record.operations[0].operation == "replace");
    ensure!(apply_record.operations[0].path == "accepted.txt");
    ensure!(fs::read_to_string(workspace.join("accepted.txt"))? == "candidate accepted\n");
    ensure!(fs::read_to_string(workspace.join("reject.txt"))? == "base reject\n");
    ensure!(fs::read_to_string(workspace.join("conflict.txt"))? == "current conflict\n");
    ensure!(fs::read_to_string(workspace.join("current-only.txt"))? == "current work\n");
    ensure!(!workspace.join("etc/agentlab-review.conf").exists());
    let applied_snapshot = snapshot::create(&workspace, &store)?.manifest;
    ensure!(applied_snapshot.digest == apply_record.after_workspace_snapshot_digest);

    let backup_bytes = store.read_run_file(&summary.run_id, &apply_record.backup_artifact.path)?;
    let backup: agentlab::snapshot::Manifest = serde_json::from_slice(&backup_bytes)?;
    ensure!(backup.digest == current_before);
    let recovery = temporary.path().join("recovery");
    snapshot::materialize(&store, &backup, &recovery)?;
    ensure!(!recovery.join("accepted.txt").exists());
    ensure!(fs::read_to_string(recovery.join("current-only.txt"))? == "current work\n");

    apply::verify(&store, &apply_record)?;
    let apply_records = apply::list(&store, &summary.run_id)?;
    ensure!(apply_records.as_slice() == std::slice::from_ref(&apply_record));
    store.write_run_file(&summary.run_id, &apply_record.backup_artifact.path, b"{}\n")?;
    ensure!(apply::verify(&store, &apply_record).is_err());
    store.write_run_file(
        &summary.run_id,
        &apply_record.backup_artifact.path,
        &backup_bytes,
    )?;
    apply::verify(&store, &apply_record)?;

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
    ensure!(String::from_utf8_lossy(&inspect.stdout).contains("Applications: 1"));

    let repeated = apply_command(
        &record.review_id,
        &workspace,
        &state,
        &["--acknowledge-conflicts", "--acknowledge-unresolved"],
    )?;
    ensure!(!repeated.status.success());
    ensure!(
        String::from_utf8_lossy(&repeated.stderr).contains("already has an accepted apply record")
    );

    let repaired = Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .args(["review", "--json", &summary.run_id, "--workspace"])
        .arg(&workspace)
        .args(["--", "./examples/reviewers/fixture-reviewer.py"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("AGENTLAB_STATE_DIR", &state)
        .env("AGENTLAB_FIXTURE_INVALID_FIRST", "1")
        .output()?;
    ensure!(
        repaired.status.success(),
        "repairing review failed: {}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    ensure!(String::from_utf8_lossy(&repaired.stderr).contains("requesting one schema correction"));
    let repaired_review: ReviewRecord = serde_json::from_slice(&repaired.stdout)?;
    let repaired_attempt = review::find_attempt(&store, &repaired_review.review_id)?;
    ensure!(repaired_attempt.status == "accepted");
    ensure!(repaired_attempt.invocations.len() == 2);
    ensure!(repaired_attempt.invocations[0].status == "invalid_proposal");
    ensure!(repaired_attempt.invocations[1].status == "accepted");
    let first_response: serde_json::Value = serde_json::from_slice(&store.read_run_file(
        &summary.run_id,
        &repaired_attempt.invocations[0].stdout.path,
    )?)?;
    ensure!(first_response.get("dispositions").is_none());
    review::verify_attempt(&store, &repaired_attempt)?;

    let rejected = Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .args(["review", &summary.run_id, "--workspace"])
        .arg(&workspace)
        .args(["--", "./examples/reviewers/fixture-reviewer.py"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("AGENTLAB_STATE_DIR", &state)
        .env("AGENTLAB_FIXTURE_ALWAYS_INVALID", "1")
        .output()?;
    ensure!(!rejected.status.success());
    let rejected_stderr = String::from_utf8_lossy(&rejected.stderr);
    ensure!(rejected_stderr.contains("still violated the contract after one correction"));
    ensure!(rejected_stderr.contains("Inspect: agentlab inspect --verify"));
    ensure!(rejected_stderr.contains("Reviewer output:"));
    let rejected_id = rejected_stderr
        .split("agentlab: review ")
        .nth(1)
        .and_then(|value| value.split(" was rejected").next())
        .context("rejected review error omitted its review ID")?;
    let rejected_attempt = review::find_attempt(&store, rejected_id)?;
    ensure!(rejected_attempt.status == "rejected");
    ensure!(rejected_attempt.invocations.len() == 2);
    ensure!(
        rejected_attempt
            .invocations
            .iter()
            .all(|invocation| invocation.status == "invalid_proposal")
    );
    let rejected_inspect = Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .args(["inspect", "--verify", rejected_id])
        .env("AGENTLAB_STATE_DIR", &state)
        .output()?;
    ensure!(
        rejected_inspect.status.success(),
        "inspect rejected review attempt: {}",
        String::from_utf8_lossy(&rejected_inspect.stderr)
    );
    let rejected_inspect_text = String::from_utf8(rejected_inspect.stdout)?;
    ensure!(rejected_inspect_text.contains("Status: rejected"));
    ensure!(rejected_inspect_text.contains("Reviewer attempts: 2"));
    ensure!(rejected_inspect_text.contains("Integrity: verified"));
    Ok(())
}

fn apply_command(
    review_id: &str,
    workspace: &std::path::Path,
    state: &std::path::Path,
    options: &[&str],
) -> Result<std::process::Output> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentlab"));
    command.arg("apply");
    command.args(options);
    command.arg(review_id).arg("--workspace").arg(workspace);
    command.env("AGENTLAB_STATE_DIR", state);
    Ok(command.output()?)
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
