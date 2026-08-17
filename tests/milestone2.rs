#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::process::Command;

use agentlab::rootfs::ChangeKind;
use agentlab::run::{self, CaptureSpec, RunOptions, WorkspaceSource};
use agentlab::snapshot;
use agentlab::store::Store;
use anyhow::{Context, Result, ensure};

struct DockerCleanup {
    container: String,
    image_tag: String,
}

#[derive(Default)]
struct RecordingObserver {
    stages: Vec<String>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl run::RunObserver for RecordingObserver {
    fn stage(&mut self, message: &str) -> std::io::Result<()> {
        self.stages.push(message.to_owned());
        Ok(())
    }

    fn command_stdout(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.stdout.extend_from_slice(bytes);
        Ok(())
    }

    fn command_stderr(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.stderr.extend_from_slice(bytes);
        Ok(())
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
#[ignore = "requires a running Docker engine and network access"]
fn direct_docker_whole_machine_conformance() -> Result<()> {
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
    fs::write(workspace.join("modify.txt"), "before\n")?;
    fs::write(workspace.join("delete.txt"), "delete me\n")?;
    fs::write(workspace.join("mode.txt"), "same bytes\n")?;
    fs::write(workspace.join("rename-before.txt"), "renamed\n")?;
    symlink("modify.txt", workspace.join("replace-me"))?;
    let mut permissions = fs::metadata(workspace.join("mode.txt"))?.permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(workspace.join("mode.txt"), permissions)?;
    fs::write(
        workspace.join(".agentlabignore"),
        "/var/log/agentlab-noise.log\n",
    )?;
    for arguments in [
        vec!["init", "--quiet", "--initial-branch=main"],
        vec!["config", "user.name", "AgentLab Fixture"],
        vec!["config", "user.email", "fixture@agentlab.invalid"],
        vec!["add", "."],
        vec!["commit", "--quiet", "-m", "base"],
    ] {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(&workspace)
            .output()?;
        ensure!(
            output.status.success(),
            "create fixture repository: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let store = Store::open(Some(&state))?;
    let source_before = snapshot::create(&workspace, &store)?.manifest.digest;
    let pi_auth = temporary.path().join("pi-auth.json");
    fs::write(&pi_auth, b"{\"fixture\":\"not-a-real-secret\"}\n")?;
    let command = r#"
set -eu
test -r /root/.pi/agent/auth.json
test "$(cat /root/.pi/agent/auth.json)" = '{"fixture":"not-a-real-secret"}'
printf 'after\n' > /workspace/modify.txt
rm /workspace/delete.txt
chmod 755 /workspace/mode.txt
mv /workspace/rename-before.txt /workspace/rename-after.txt
rm /workspace/replace-me
printf 'now regular\n' > /workspace/replace-me
printf 'new\n' > /workspace/new.txt
ln -s new.txt /workspace/latest
printf 'system\n' > /etc/agentlab.conf
printf 'session\n' > /root/session.txt
mkdir -p /opt/agentlab
printf 'optional\n' > /opt/agentlab/proof.txt
printf 'ignored but observed\n' > /var/log/agentlab-noise.log
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq git jq
cd /workspace
git config --global --add safe.directory /workspace
git config user.name 'AgentLab Fixture'
git config user.email fixture@agentlab.invalid
git add -A
git commit -q -m agent-run
printf 'streamed stdout\n'
printf 'streamed stderr\n' >&2
exit 23
"#;
    let mut observer = RecordingObserver::default();
    let summary = run::execute_with_observer(
        &RunOptions {
            workspace: WorkspaceSource::Directory(workspace.clone()),
            workspace_capture_mode: snapshot::CaptureMode::All,
            image: "ubuntu:24.04".to_owned(),
            command: vec!["/bin/sh".to_owned(), "-c".to_owned(), command.to_owned()],
            workspace_guest_path: "/workspace".to_owned(),
            network: "bridge".to_owned(),
            memory: Some("1g".to_owned()),
            cpus: Some("2".to_owned()),
            pi_auth: Some(pi_auth),
            change_ignore: None,
            captures: vec![CaptureSpec {
                guest_path: "/root/session.txt".to_owned(),
                name: "session".to_owned(),
            }],
            accepted_input: None,
        },
        &store,
        &mut observer,
    )?;
    let compact = summary.run_id.replace('-', "");
    let _cleanup = DockerCleanup {
        container: summary.retained_container_name.clone(),
        image_tag: format!("agentlab-prepared:{}", &compact[..12]),
    };

    ensure!(
        summary.exit_code == 23,
        "nonzero exit status was not preserved"
    );
    ensure!(summary.source_workspace_status == "unchanged");
    ensure!(observer.stdout.ends_with(b"streamed stdout\n"));
    ensure!(observer.stderr.ends_with(b"streamed stderr\n"));
    ensure!(
        observer
            .stages
            .iter()
            .any(|stage| stage == "Source workspace unchanged")
    );
    let source_after = snapshot::create(&workspace, &store)?.manifest.digest;
    ensure!(
        source_before == source_after,
        "source workspace was mutated"
    );

    let result = run::load_result(&store, &summary.run_id)?;
    run::verify_result(&store, &result)?;
    let spec = run::load_spec(&store, &summary.run_id)?;
    ensure!(spec.secret_injections == ["pi-auth"]);
    ensure!(result.docker.retained_container_state == "running");
    ensure!(
        result
            .captures
            .iter()
            .any(|capture| capture.path == "artifacts/capture-session.tar")
    );

    let portable = run::load_delta(&store, &summary.run_id, false)?;
    let raw = run::load_delta(&store, &summary.run_id, true)?;
    let change = |path: &str, kind: ChangeKind| {
        portable
            .changes
            .iter()
            .any(|candidate| candidate.path == path && candidate.change == kind)
    };
    ensure!(change("/workspace/modify.txt", ChangeKind::Modified));
    ensure!(change("/workspace/delete.txt", ChangeKind::Deleted));
    ensure!(change("/workspace/mode.txt", ChangeKind::ModeChanged));
    ensure!(change("/workspace/new.txt", ChangeKind::Added));
    ensure!(change("/workspace/latest", ChangeKind::Added));
    ensure!(change("/workspace/replace-me", ChangeKind::TypeChanged));
    ensure!(change("/workspace/rename-before.txt", ChangeKind::Deleted));
    ensure!(change("/workspace/rename-after.txt", ChangeKind::Added));
    ensure!(change("/etc/agentlab.conf", ChangeKind::Added));
    ensure!(change("/root/session.txt", ChangeKind::Added));
    ensure!(change("/opt/agentlab/proof.txt", ChangeKind::Added));
    ensure!(
        !raw.changes
            .iter()
            .any(|candidate| candidate.path == "/root/.pi/agent/auth.json"
                || candidate.path.starts_with("/run/agentlab-secrets")),
        "runtime Pi authentication leaked into persistent filesystem changes"
    );
    ensure!(
        portable
            .changes
            .iter()
            .any(|candidate| candidate.path == "/usr/bin/jq"),
        "installed package files were not captured"
    );
    ensure!(
        portable
            .changes
            .iter()
            .any(|candidate| { candidate.path.starts_with("/workspace/.git/objects/") }),
        "the repository commit was not captured"
    );
    ensure!(
        raw.changes
            .iter()
            .any(|candidate| candidate.path == "/var/log/agentlab-noise.log"),
        "ignored path was absent from the raw observation"
    );
    ensure!(
        portable
            .ignored_changes
            .iter()
            .any(|candidate| candidate.path == "/var/log/agentlab-noise.log"),
        "ignored path was not classified explicitly"
    );

    let copied = temporary.path().join("retained-system-file");
    let output = Command::new("docker")
        .args([
            "cp",
            &format!("{}:/etc/agentlab.conf", summary.retained_container_name),
        ])
        .arg(&copied)
        .output()
        .context("inspect retained stopped container")?;
    ensure!(
        output.status.success(),
        "docker cp failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(fs::read_to_string(copied)? == "system\n");
    Ok(())
}
