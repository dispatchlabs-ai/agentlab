#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::process::Command;
use std::sync::{Arc, Barrier};

use agentlab::rootfs::RootFsManifest;
use agentlab::run::{self, RunOptions, RunSummary};
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
fn concurrent_runs_are_isolated_and_comparable() -> Result<()> {
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
        workspace: workspace.clone(),
        image: "alpine:3.21".to_owned(),
        command: vec!["/bin/sh".to_owned(), "-c".to_owned(), command.to_owned()],
        factors: BTreeMap::new(),
        workspace_guest_path: "/workspace".to_owned(),
        network: "none".to_owned(),
        memory: None,
        cpus: None,
        change_ignore: None,
        captures: Vec::new(),
    };
    let mut left_options = base_options.clone();
    left_options.factors = BTreeMap::from([
        ("variant".to_owned(), "A".to_owned()),
        ("replicate".to_owned(), "1".to_owned()),
        ("opaque-label".to_owned(), "α".to_owned()),
    ]);
    let mut right_options = base_options;
    right_options.factors = BTreeMap::from([
        ("variant".to_owned(), "B".to_owned()),
        ("replicate".to_owned(), "2".to_owned()),
        ("opaque-label".to_owned(), "α".to_owned()),
    ]);

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

    let comparison = run::compare_runs(
        &store,
        &left.run_id,
        &right.run_id,
        &["variant".to_owned(), "replicate".to_owned()],
    )?;
    ensure!(comparison.same_workspace_snapshot);
    ensure!(comparison.same_resolved_image);
    ensure!(comparison.same_portable_base);
    ensure!(comparison.distinct_private_containers);
    ensure!(comparison.controlled_input_differences.is_empty());
    ensure!(comparison.only_expected_factors_differ);
    ensure!(comparison.comparable_repetition);
    ensure!(!comparison.portable_outcomes_equal);
    ensure!(
        comparison
            .factor_differences
            .get("variant")
            .is_some_and(|difference| difference.left.as_deref() == Some("A")
                && difference.right.as_deref() == Some("B"))
    );
    let incorrectly_declared =
        run::compare_runs(&store, &left.run_id, &right.run_id, &["variant".to_owned()])?;
    ensure!(!incorrectly_declared.only_expected_factors_differ);
    ensure!(incorrectly_declared.unexpected_factor_differences == ["replicate"]);

    let left_result = run::load_result(&store, &left.run_id)?;
    let right_result = run::load_result(&store, &right.run_id)?;
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
