#![cfg(unix)]

use std::fs;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use agentlab::lifecycle;
use agentlab::run::{self, CaptureSpec, RunOptions, SecretFileSpec, WorkspaceSource};
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
    let runtime_secret = temporary.path().join("aws-credentials");
    fs::write(&runtime_secret, b"runtime-only\n")?;

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
            backend: None,
            workspace: WorkspaceSource::Directory(workspace.clone()),
            workspace_capture_mode: snapshot::CaptureMode::All,
            image: "alpine:3.21".to_owned(),
            command: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "(i=0; while :; do i=$((i + 1)); printf '%s\\n' \"$i\" > /root/background-writer.txt; sleep 0.02; done) >/dev/null 2>&1 & printf '1\\n' > /root/session.txt; printf 'initial\\n' > /workspace/initial.txt; exit 19"
                    .to_owned(),
            ],
            workspace_guest_path: "/workspace".to_owned(),
            network: "none".to_owned(),
            memory: None,
            cpus: None,
            pi_auth: None,
            secret_files: Vec::new(),
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
    ensure!(
        result
            .docker
            .as_ref()
            .context("Docker result omitted Docker evidence")?
            .retained_container_state
            == "running"
    );
    let background_after_capture = docker_success(
        Command::new("docker").args([
            "exec",
            &summary.retained_container_name,
            "cat",
            "/root/background-writer.txt",
        ]),
        "read quiesced background fixture",
    )?;
    thread::sleep(Duration::from_millis(150));
    ensure!(
        background_after_capture
            == docker_success(
                Command::new("docker").args([
                    "exec",
                    &summary.retained_container_name,
                    "cat",
                    "/root/background-writer.txt",
                ]),
                "re-read quiesced background fixture",
            )?,
        "background guest writer survived immutable result capture"
    );
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
    let continuation = lifecycle::resume_with_secrets(
        &store,
        &summary.run_id,
        &[
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "test \"$(cat /root/session.txt)\" = 1; test \"$(cat /root/.pi/agent/auth.json)\" = '{\"fixture\":\"continuation-only\"}'; test \"$(cat /run/agentlab-secrets/aws-credentials)\" = runtime-only; printf '2\\n' > /root/session.txt; printf 'continued\\n' > /workspace/continued.txt; exit 29"
                .to_owned(),
        ],
        Some(&pi_auth),
        &[SecretFileSpec {
            name: "aws-credentials".to_owned(),
            source: runtime_secret,
        }],
    )?;
    ensure!(continuation.container_restarted);
    ensure!(!continuation.process_memory_restored);
    let continuation = continuation
        .continuation
        .context("missing continuation result")?;
    ensure!(continuation.exit_code == 29);
    ensure!(continuation.secret_injections == ["aws-credentials", "pi-auth"]);
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
            "test ! -e /root/.pi/agent/auth.json; test ! -e /run/agentlab-secrets/pi-auth.json; test ! -e /run/agentlab-secrets/aws-credentials",
        ])
        .output()?;
    ensure!(
        auth_cleanup.status.success(),
        "continuation credentials were not removed: {}",
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

    let continuation_directory = store
        .root()
        .join("runs")
        .join(&summary.run_id)
        .join("continuations");
    let completed_continuation_count = fs::read_dir(&continuation_directory)?.count();
    let interrupt_secret = temporary.path().join("interrupt-secret");
    fs::write(&interrupt_secret, b"interrupt-only\n")?;
    let mut interrupted = spawn_credentialed_resume(
        &store,
        &summary.run_id,
        "interrupt-secret",
        &interrupt_secret,
    )?;
    wait_for_container_file(
        &mut interrupted,
        &summary.retained_container_name,
        "/run/agentlab-secrets/interrupt-secret",
    )?;

    let concurrent_fork = lifecycle::fork(&store, &summary.run_id)
        .err()
        .context("concurrent fork unexpectedly succeeded")?;
    ensure!(
        concurrent_fork.to_string().contains("already in progress"),
        "concurrent lifecycle operation failed unclearly: {concurrent_fork:#}"
    );
    signal_child(&interrupted, rustix::process::Signal::INT)?;
    let interrupted_output = interrupted.wait_with_output()?;
    ensure!(
        interrupted_output.status.code() == Some(130),
        "Ctrl-C continuation returned {:?}: {}",
        interrupted_output.status.code(),
        String::from_utf8_lossy(&interrupted_output.stderr)
    );
    ensure!(
        !store.run_file_exists(&summary.run_id, "runtime-secret-lease.json")?,
        "Ctrl-C left an active credential lease"
    );
    ensure!(
        fs::read_dir(&continuation_directory)?.count() == completed_continuation_count,
        "Ctrl-C left an incomplete continuation"
    );
    assert_container_file_absent(
        &summary.retained_container_name,
        "/run/agentlab-secrets/interrupt-secret",
    )?;

    let crash_secret = temporary.path().join("crash-secret");
    fs::write(&crash_secret, b"crash-only\n")?;
    let mut crashed =
        spawn_credentialed_resume(&store, &summary.run_id, "crash-secret", &crash_secret)?;
    wait_for_container_file(
        &mut crashed,
        &summary.retained_container_name,
        "/run/agentlab-secrets/crash-secret",
    )?;
    signal_child(&crashed, rustix::process::Signal::KILL)?;
    let crashed_output = crashed.wait_with_output()?;
    ensure!(!crashed_output.status.success());
    ensure!(
        store.run_file_exists(&summary.run_id, "runtime-secret-lease.json")?,
        "forced crash did not preserve a recoverable credential lease"
    );

    let recovered_stop = lifecycle::stop(&store, &summary.run_id)?;
    ensure!(recovered_stop.container_state == "exited");
    ensure!(
        !store.run_file_exists(&summary.run_id, "runtime-secret-lease.json")?,
        "next lifecycle operation did not clear the crashed credential lease"
    );
    ensure!(
        fs::read_dir(&continuation_directory)?.count() == completed_continuation_count,
        "credential recovery left an incomplete continuation"
    );
    lifecycle::resume(&store, &summary.run_id, &[])?;
    assert_container_file_absent(
        &summary.retained_container_name,
        "/run/agentlab-secrets/crash-secret",
    )?;
    lifecycle::verify_all(&store, &summary.run_id)?;

    let initial_crash_secret = temporary.path().join("initial-crash-secret");
    fs::write(&initial_crash_secret, b"initial-crash-only\n")?;
    let mut crashed_initial = spawn_credentialed_initial_run(
        &store,
        &workspace,
        "initial-crash-secret",
        &initial_crash_secret,
    )?;
    let (orphan_run_id, orphan_container) =
        wait_for_new_runtime_lease(&mut crashed_initial, &store, &summary.run_id)?;
    cleanup.run_ids.push(orphan_run_id.clone());
    cleanup.containers.push(orphan_container.clone());
    let orphan_compact = orphan_run_id.replace('-', "");
    let orphan_image_tag = format!("agentlab-prepared:{}", &orphan_compact[..12]);
    cleanup.image_tags.push(orphan_image_tag.clone());
    wait_for_container_file(
        &mut crashed_initial,
        &orphan_container,
        "/run/agentlab-secrets/initial-crash-secret",
    )?;
    signal_child(&crashed_initial, rustix::process::Signal::KILL)?;
    ensure!(!crashed_initial.wait_with_output()?.status.success());
    ensure!(store.run_file_exists(&orphan_run_id, "runtime-secret-lease.json")?);

    let recovery_trigger = temporary.path().join("recovery-trigger");
    fs::write(&recovery_trigger, b"trigger\n")?;
    let recovery_continuation = lifecycle::resume_with_secrets(
        &store,
        &summary.run_id,
        &["/bin/true".to_owned()],
        None,
        &[SecretFileSpec {
            name: "recovery-trigger".to_owned(),
            source: recovery_trigger,
        }],
    )?;
    ensure!(
        recovery_continuation
            .continuation
            .context("missing recovery-trigger continuation")?
            .exit_code
            == 0
    );
    ensure!(
        !store.root().join("runs").join(&orphan_run_id).exists(),
        "a new credential lease did not remove the crashed initial run state"
    );
    ensure!(
        !docker_exists(&orphan_container),
        "a new credential lease did not remove the crashed credentialed container"
    );
    ensure!(
        !docker_image_exists(&orphan_image_tag),
        "a new credential lease did not remove the crashed run image tag"
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

    let continuation_count = fs::read_dir(&continuation_directory)?.count();
    let failed_continuation = lifecycle::resume(
        &store,
        &summary.run_id,
        &[
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "rm -f /root/session.txt".to_owned(),
        ],
    );
    ensure!(
        failed_continuation.is_err(),
        "continuation unexpectedly succeeded without its requested capture"
    );
    ensure!(
        format!("{:#}", failed_continuation.unwrap_err())
            .contains("export continuation capture /root/session.txt"),
        "continuation failed for an unexpected reason"
    );
    ensure!(
        fs::read_dir(&continuation_directory)?.count() == continuation_count,
        "failed continuation left an incomplete record directory"
    );
    lifecycle::verify_all(&store, &summary.run_id)?;

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

fn spawn_credentialed_resume(
    store: &Store,
    run_id: &str,
    secret_name: &str,
    secret_path: &std::path::Path,
) -> Result<Child> {
    Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .env("AGENTLAB_STATE_DIR", store.root())
        .arg("resume")
        .arg("--secret-file")
        .arg(format!("{secret_name}={}", secret_path.display()))
        .arg(run_id)
        .args([
            "--",
            "/bin/sh",
            "-c",
            "test -f \"$1\"; while :; do sleep 1; done",
            "agentlab-interruption-fixture",
            &format!("/run/agentlab-secrets/{secret_name}"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start credentialed continuation fixture")
}

fn spawn_credentialed_initial_run(
    store: &Store,
    workspace: &std::path::Path,
    secret_name: &str,
    secret_path: &std::path::Path,
) -> Result<Child> {
    Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .env("AGENTLAB_STATE_DIR", store.root())
        .arg("run")
        .arg("--workspace")
        .arg(workspace)
        .args(["--image", "alpine:3.21", "--network", "none"])
        .arg("--secret-file")
        .arg(format!("{secret_name}={}", secret_path.display()))
        .args([
            "--",
            "/bin/sh",
            "-c",
            "test -f \"$1\"; while :; do sleep 1; done",
            "agentlab-initial-crash-fixture",
            &format!("/run/agentlab-secrets/{secret_name}"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start credentialed initial-run fixture")
}

fn wait_for_new_runtime_lease(
    child: &mut Child,
    store: &Store,
    excluded_run_id: &str,
) -> Result<(String, String)> {
    for _ in 0..800 {
        ensure!(
            child.try_wait()?.is_none(),
            "credentialed initial run exited before opening its lease"
        );
        for run_id in store.list_run_ids()? {
            if run_id == excluded_run_id
                || !store.run_file_exists(&run_id, "runtime-secret-lease.json")?
            {
                continue;
            }
            let lease: serde_json::Value = serde_json::from_slice(
                &store.read_run_file(&run_id, "runtime-secret-lease.json")?,
            )?;
            let container = lease["container_name"]
                .as_str()
                .context("runtime lease omitted its container")?;
            return Ok((run_id, container.to_owned()));
        }
        thread::sleep(Duration::from_millis(25));
    }
    anyhow::bail!("timed out waiting for initial runtime credential lease")
}

fn wait_for_container_file(child: &mut Child, container: &str, path: &str) -> Result<()> {
    for _ in 0..200 {
        ensure!(
            child.try_wait()?.is_none(),
            "credentialed continuation exited before its secret was observable"
        );
        if Command::new("docker")
            .args(["exec", container, "test", "-f", path])
            .output()?
            .status
            .success()
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    anyhow::bail!("timed out waiting for credential lease fixture")
}

fn signal_child(child: &Child, signal: rustix::process::Signal) -> Result<()> {
    let raw = i32::try_from(child.id()).context("fixture process ID overflow")?;
    let pid = rustix::process::Pid::from_raw(raw).context("fixture process ID was zero")?;
    rustix::process::kill_process(pid, signal).context("signal credential lease fixture")?;
    Ok(())
}

fn assert_container_file_absent(container: &str, path: &str) -> Result<()> {
    let output = Command::new("docker")
        .args(["exec", container, "test", "!", "-e", path])
        .output()?;
    ensure!(
        output.status.success(),
        "runtime credential remained at {path}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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

fn docker_image_exists(name: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", name])
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
