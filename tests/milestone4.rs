#![cfg(unix)]

use std::fs;
use std::io::Read;
use std::process::Command;

use agentlab::lifecycle;
use agentlab::run::{self, CaptureSpec, RunOptions, WorkspaceSource};
use agentlab::snapshot;
use agentlab::store::Store;
use anyhow::{Context, Result, ensure};

struct Cleanup {
    store: Store,
    run_ids: Vec<String>,
    containers: Vec<String>,
    image_tags: Vec<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for container in &self.containers {
            let _ = Command::new("docker")
                .args(["rm", "--force", container])
                .output();
        }
        for tag in &self.image_tags {
            let _ = Command::new("docker").args(["image", "rm", tag]).output();
        }
        for run_id in &self.run_ids {
            let _ = self.store.remove_run_directory(run_id);
        }
    }
}

#[test]
#[ignore = "requires a running Docker engine"]
fn retained_lifecycle_continuation_fork_and_exact_removal() -> Result<()> {
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
    fs::write(workspace.join("source.txt"), "immutable\n")?;
    let store = Store::open(Some(&state))?;
    let source_before = snapshot::create(&workspace, &store)?.manifest.digest;
    let pi_auth = temporary.path().join("pi-auth.json");
    fs::write(&pi_auth, b"{\"fixture\":\"continuation-only\"}\n")?;

    let unrelated_name = format!(
        "agentlab-unrelated-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let unrelated_id = docker_success(
        Command::new("docker").args([
            "create",
            "--name",
            &unrelated_name,
            "alpine:3.21",
            "/bin/true",
        ]),
        "create unrelated control container",
    )?;
    let mut cleanup = Cleanup {
        store: store.clone(),
        run_ids: Vec::new(),
        containers: vec![unrelated_name.clone()],
        image_tags: Vec::new(),
    };

    let summary = run::execute(
        &RunOptions {
            workspace: WorkspaceSource::Directory(workspace.clone()),
            workspace_capture_mode: snapshot::CaptureMode::All,
            image: "alpine:3.21".to_owned(),
            command: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "printf '1\\n' > /root/session.txt; printf 'initial\\n' > /workspace/initial.txt; exit 19"
                    .to_owned(),
            ],
            workspace_guest_path: "/workspace".to_owned(),
            network: "none".to_owned(),
            memory: None,
            cpus: None,
            pi_auth: None,
            change_ignore: None,
            captures: vec![CaptureSpec {
                guest_path: "/root/session.txt".to_owned(),
                name: "session".to_owned(),
            }],
            accepted_input: None,
        },
        &store,
    )?;
    let compact = summary.run_id.replace('-', "");
    cleanup.run_ids.push(summary.run_id.clone());
    cleanup
        .containers
        .push(summary.retained_container_name.clone());
    cleanup
        .image_tags
        .push(format!("agentlab-prepared:{}", &compact[..12]));

    ensure!(summary.exit_code == 19);
    let result = run::load_result(&store, &summary.run_id)?;
    ensure!(result.docker.retained_container_state == "running");
    lifecycle::verify_all(&store, &summary.run_id)?;
    let listed = lifecycle::list(&store)?;
    ensure!(listed.iter().any(|run| {
        run.run_id == summary.run_id && run.container_state == "running" && run.lifecycle_capable
    }));

    let stopped = lifecycle::stop(&store, &summary.run_id)?;
    ensure!(stopped.container_state == "exited");
    ensure!(stopped.container_id == summary.retained_container_id);
    let restarted = lifecycle::resume(&store, &summary.run_id, &[])?;
    ensure!(restarted.container_restarted);
    ensure!(restarted.container_state == "running");
    ensure!(restarted.container_id == summary.retained_container_id);
    ensure!(restarted.filesystem_state_reused);
    ensure!(!restarted.process_memory_restored);

    lifecycle::stop(&store, &summary.run_id)?;
    let continuation = lifecycle::resume_with_pi_auth(
        &store,
        &summary.run_id,
        &[
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "test \"$(cat /root/session.txt)\" = 1; test \"$(cat /root/.pi/agent/auth.json)\" = '{\"fixture\":\"continuation-only\"}'; printf '2\\n' > /root/session.txt; printf 'continued\\n' > /workspace/continued.txt; exit 29"
                .to_owned(),
        ],
        Some(&pi_auth),
    )?;
    ensure!(continuation.container_restarted);
    ensure!(!continuation.process_memory_restored);
    let continuation = continuation
        .continuation
        .context("missing continuation result")?;
    ensure!(continuation.exit_code == 29);
    ensure!(continuation.secret_injections == ["pi-auth"]);
    ensure!(continuation.filesystem_state_reused);
    ensure!(!continuation.process_memory_restored);
    ensure!(continuation.container_id == summary.retained_container_id);
    ensure!(continuation.captures.len() == 1);
    let auth_cleanup = Command::new("docker")
        .args([
            "exec",
            &summary.retained_container_name,
            "/bin/sh",
            "-c",
            "test ! -e /root/.pi/agent/auth.json; test ! -e /run/agentlab-secrets/pi-auth.json",
        ])
        .output()?;
    ensure!(
        auth_cleanup.status.success(),
        "continuation Pi auth was not removed: {}",
        String::from_utf8_lossy(&auth_cleanup.stderr)
    );
    ensure!(
        read_only_regular_file_from_tar(
            &store
                .root()
                .join("runs")
                .join(&summary.run_id)
                .join(&continuation.captures[0].path),
        )? == b"2\n"
    );
    lifecycle::verify_all(&store, &summary.run_id)?;
    let continuation_raw: run::DeltaManifest = serde_json::from_slice(&store.read_run_file(
        &summary.run_id,
        &format!(
            "continuations/{}/delta.raw.json",
            continuation.continuation_id
        ),
    )?)?;
    ensure!(
        !continuation_raw.changes.iter().any(|change| {
            change.path == "/root/.pi/agent/auth.json"
                || change.path.starts_with("/run/agentlab-secrets")
        }),
        "continuation Pi auth leaked into persistent filesystem changes"
    );

    let fork = lifecycle::fork(&store, &summary.run_id)?;
    cleanup.run_ids.push(fork.run_id.clone());
    cleanup.containers.push(fork.container_name.clone());
    cleanup.image_tags.push(fork.image_tag.clone());
    ensure!(fork.filesystem_state_copied);
    ensure!(!fork.process_memory_copied);
    ensure!(fork.base_filesystem_digest == continuation.result_filesystem_digest);
    lifecycle::verify_fork(&store, &fork)?;
    let fork_continuation = lifecycle::resume(
        &store,
        &fork.run_id,
        &[
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "test \"$(cat /root/session.txt)\" = 2; printf '3\\n' > /root/session.txt; printf 'fork-only\\n' > /workspace/fork.txt"
                .to_owned(),
        ],
    )?
    .continuation
    .context("missing fork continuation")?;
    ensure!(fork_continuation.exit_code == 0);
    ensure!(
        read_only_regular_file_from_tar(
            &store
                .root()
                .join("runs")
                .join(&fork.run_id)
                .join(&fork_continuation.captures[0].path),
        )? == b"3\n"
    );
    lifecycle::verify_all(&store, &fork.run_id)?;

    let parent_session = temporary.path().join("parent-session.txt");
    docker_status(
        Command::new("docker")
            .args([
                "cp",
                &format!("{}:/root/session.txt", summary.retained_container_name),
            ])
            .arg(&parent_session),
        "copy parent session",
    )?;
    ensure!(fs::read(&parent_session)? == b"2\n");

    let removed_fork = lifecycle::remove(&store, &fork.run_id)?;
    ensure!(removed_fork.run_directory_removed);
    ensure!(!store.root().join("runs").join(&fork.run_id).exists());
    ensure!(docker_exists(&summary.retained_container_name));
    ensure!(docker_exists(&unrelated_name));
    ensure!(docker_id(&unrelated_name)? == unrelated_id);

    let source_after = snapshot::create(&workspace, &store)?.manifest.digest;
    ensure!(
        source_before == source_after,
        "source workspace was mutated"
    );
    ensure!(!workspace.join("initial.txt").exists());
    ensure!(!workspace.join("continued.txt").exists());
    ensure!(!workspace.join("fork.txt").exists());

    let removed_parent = lifecycle::remove(&store, &summary.run_id)?;
    ensure!(removed_parent.run_directory_removed);
    ensure!(!store.root().join("runs").join(&summary.run_id).exists());
    ensure!(docker_exists(&unrelated_name));
    ensure!(docker_id(&unrelated_name)? == unrelated_id);
    Ok(())
}

fn read_only_regular_file_from_tar(path: &std::path::Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut archive = tar::Archive::new(file);
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_file() {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            return Ok(bytes);
        }
    }
    anyhow::bail!("capture archive has no regular file")
}

fn docker_exists(name: &str) -> bool {
    Command::new("docker")
        .args(["inspect", name])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn docker_id(name: &str) -> Result<String> {
    docker_success(
        Command::new("docker").args(["inspect", "--format", "{{.Id}}", name]),
        "inspect Docker container ID",
    )
}

fn docker_success(command: &mut Command, context: &str) -> Result<String> {
    let output = command.output().with_context(|| context.to_owned())?;
    ensure!(
        output.status.success(),
        "{context}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn docker_status(command: &mut Command, context: &str) -> Result<()> {
    docker_success(command, context).map(|_| ())
}
