#![cfg(unix)]

use std::fs;
use std::process::Command;
use std::sync::{Arc, Barrier};

use agentlab::rootfs::RootFsManifest;
use agentlab::run::{self, RunOptions, RunSummary, WorkspaceSource};
use agentlab::snapshot;
use agentlab::store::Store;
use anyhow::{Context, Result, ensure};

struct DockerCleanup {
    container: String,
    image_tag: String,
}

impl DockerCleanup {
    fn for_run(summary: &RunSummary) -> Self {
        let compact = summary.run_id.replace('-', "");
        Self {
            container: summary.retained_container_name.clone(),
            image_tag: format!("agentlab-prepared:{}", &compact[..12]),
        }
    }
}

impl Drop for DockerCleanup {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "--force", &self.container])
            .output();
        let _ = Command::new("docker")
            .args(["image", "rm", &self.image_tag])
            .output();
    }
}

#[test]
#[ignore = "requires a running Docker engine"]
fn concurrent_runs_from_one_snapshot_are_isolated_comparable_repetitions() -> Result<()> {
    ensure!(
        Command::new("docker")
            .arg("info")
            .output()?
            .status
            .success(),
        "Docker is not available"
    );
    let temporary = tempfile::tempdir()?;
    let workspace = temporary.path().join("workspace");
    let state = temporary.path().join("state");
    fs::create_dir(&workspace)?;
    fs::write(workspace.join("source.txt"), "immutable source\n")?;
    let store = Store::open(Some(&state))?;
    let source_before = snapshot::create(&workspace, &store)?.manifest.digest;

    let command = r#"
set -eu
owner="$HOSTNAME"
printf '%s\n' "$owner" > /workspace/conflict.txt
printf '%s\n' "$owner" > /etc/agentlab-owner
touch "/workspace/owner-$owner"
sleep 5
count=$(find /workspace -maxdepth 1 -name 'owner-*' | wc -l | tr -d ' ')
test "$count" = 1
"#;
    let base_options = RunOptions {
        workspace: WorkspaceSource::Snapshot(source_before.clone()),
        image: "alpine:3.21".to_owned(),
        command: vec!["/bin/sh".to_owned(), "-c".to_owned(), command.to_owned()],
        workspace_guest_path: "/workspace".to_owned(),
        network: "none".to_owned(),
        memory: None,
        cpus: None,
        change_ignore: None,
        captures: Vec::new(),
    };
    let left_options = base_options.clone();
    let right_options = base_options;

    let barrier = Arc::new(Barrier::new(2));
    let left_store = store.clone();
    let left_barrier = barrier.clone();
    let left = std::thread::spawn(move || {
        left_barrier.wait();
        run::execute(&left_options, &left_store)
    });
    let right_store = store.clone();
    let right = std::thread::spawn(move || {
        barrier.wait();
        run::execute(&right_options, &right_store)
    });
    let left = left
        .join()
        .map_err(|_| anyhow::anyhow!("left run panicked"))??;
    let _left_cleanup = DockerCleanup::for_run(&left);
    let right = right
        .join()
        .map_err(|_| anyhow::anyhow!("right run panicked"))??;
    let _right_cleanup = DockerCleanup::for_run(&right);

    let source_after = snapshot::create(&workspace, &store)?.manifest.digest;
    ensure!(
        source_before == source_after,
        "source workspace was mutated"
    );
    ensure!(!workspace.join("conflict.txt").exists());

    let comparison = run::compare_runs(&store, &left.run_id, &right.run_id)?;
    ensure!(comparison.same_run_input);
    ensure!(comparison.same_workspace_snapshot);
    ensure!(comparison.same_resolved_image);
    ensure!(comparison.same_portable_base);
    ensure!(comparison.distinct_private_containers);
    ensure!(comparison.controlled_input_differences.is_empty());
    ensure!(comparison.comparison_kind == "comparable_repetition");
    ensure!(comparison.comparable_repetition);
    ensure!(!comparison.portable_outcomes_equal);

    let left_spec = run::load_spec(&store, &left.run_id)?;
    let right_spec = run::load_spec(&store, &right.run_id)?;
    ensure!(left_spec.schema_version == run::RUN_SCHEMA_VERSION);
    ensure!(left_spec.run_input_digest == right_spec.run_input_digest);
    ensure!(left_spec.run_input_digest == left.run_input_digest);
    ensure!(left_spec.legacy_factors.is_empty());
    let persisted_spec = String::from_utf8(store.read_run_file(&left.run_id, "spec.json")?)?;
    ensure!(!persisted_spec.contains("\"factors\""));

    let left_result = run::load_result(&store, &left.run_id)?;
    let right_result = run::load_result(&store, &right.run_id)?;
    ensure!(
        left_result
            .lifecycle
            .iter()
            .any(|event| event.event == "workspace_snapshot_loaded")
    );
    ensure!(
        right_result
            .lifecycle
            .iter()
            .any(|event| event.event == "workspace_snapshot_loaded")
    );
    let command_interval = |result: &run::RunResult| -> Result<_> {
        let started = result
            .lifecycle
            .iter()
            .find(|event| event.event == "command_started")
            .context("missing command_started event")?
            .timestamp;
        let completed = result
            .lifecycle
            .iter()
            .find(|event| event.event == "command_completed")
            .context("missing command_completed event")?
            .timestamp;
        Ok((started, completed))
    };
    let (left_started, left_completed) = command_interval(&left_result)?;
    let (right_started, right_completed) = command_interval(&right_result)?;
    ensure!(
        left_started < right_completed && right_started < left_completed,
        "command execution intervals did not overlap"
    );

    let load_rootfs = |run_id: &str| -> Result<RootFsManifest> {
        Ok(serde_json::from_slice(
            &store.read_run_file(run_id, "result-rootfs.json")?,
        )?)
    };
    let left_rootfs = load_rootfs(&left.run_id)?;
    let right_rootfs = load_rootfs(&right.run_id)?;
    let owners = |manifest: &RootFsManifest| {
        manifest
            .entries
            .iter()
            .filter(|entry| entry.path.starts_with("workspace/owner-"))
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>()
    };
    let left_owners = owners(&left_rootfs);
    let right_owners = owners(&right_rootfs);
    ensure!(left_owners.len() == 1 && right_owners.len() == 1);
    ensure!(
        left_owners != right_owners,
        "runs observed the same private marker"
    );

    let file_digest = |manifest: &RootFsManifest, path: &str| -> Result<String> {
        Ok(manifest
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .with_context(|| format!("missing {path}"))?
            .digest
            .clone())
    };
    ensure!(
        file_digest(&left_rootfs, "workspace/conflict.txt")?
            != file_digest(&right_rootfs, "workspace/conflict.txt")?,
        "conflicting writes did not produce private outcomes"
    );
    Ok(())
}
