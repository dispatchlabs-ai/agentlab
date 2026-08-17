use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::process::Command;

use agentlab::run;
use agentlab::snapshot::{self, Manifest};
use agentlab::store::{Store, hex_digest};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[test]
fn snapshot_conformance() {
    require_git();
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    let state = temporary.path().join("state");
    mkdir(&workspace, 0o755);

    write_file(
        workspace.join(".gitignore"),
        "*.tmp\nignored-dir/\nglobal-excluded.txt\n!global-excluded.txt\n",
        0o644,
    );
    write_file(workspace.join(".hidden"), "hidden\n", 0o600);
    write_file(workspace.join("ordinary.txt"), "ordinary\n", 0o644);
    write_file(workspace.join("script.sh"), "#!/bin/sh\nexit 0\n", 0o755);
    write_file(workspace.join("drop.tmp"), "excluded\n", 0o644);
    write_file(
        workspace.join("global-excluded.txt"),
        "explicitly unignored\n",
        0o644,
    );
    mkdir(&workspace.join("empty"), 0o750);
    mkdir(&workspace.join("nested"), 0o755);
    write_file(
        workspace.join("nested/.gitignore"),
        "*.cache\n!keep.cache\n",
        0o644,
    );
    write_file(workspace.join("nested/drop.cache"), "excluded\n", 0o644);
    write_file(workspace.join("nested/keep.cache"), "included\n", 0o644);
    mkdir(&workspace.join("ignored-dir"), 0o755);
    write_file(
        workspace.join("ignored-dir/.gitignore"),
        "!revive.txt\n",
        0o644,
    );
    write_file(
        workspace.join("ignored-dir/revive.txt"),
        "must stay excluded\n",
        0o644,
    );
    write_bytes(
        workspace.join("large.bin"),
        &vec![b'L'; 2 * 1024 * 1024],
        0o644,
    );
    create_symlink("ordinary.txt", &workspace.join("ordinary-link"));

    let repository_a = workspace.join("projects/alpha");
    let repository_b = workspace.join("unrelated/beta");
    initialize_repository(&repository_a);
    initialize_repository(&repository_b);
    write_file(
        repository_a.join(".gitignore"),
        "*.generated\nignored-tracked.txt\n",
        0o644,
    );
    write_file(
        repository_a.join("ignored-tracked.txt"),
        "tracked wins\n",
        0o644,
    );
    write_file(
        repository_a.join("ignored-untracked.generated"),
        "ignored\n",
        0o644,
    );
    write_file(repository_a.join("untracked.txt"), "included\n", 0o644);
    run_git(
        &repository_a,
        &["add", "-f", ".gitignore", "ignored-tracked.txt"],
    );
    write_file(repository_b.join(".gitignore"), "build/\n", 0o644);
    write_file(repository_b.join("tracked.txt"), "tracked\n", 0o644);
    write_file(repository_b.join("loose.txt"), "loose\n", 0o644);
    write_file(repository_b.join("build/artifact"), "ignored\n", 0o644);
    run_git(&repository_b, &["add", ".gitignore", "tracked.txt"]);

    let before = tree_fingerprint(&workspace);
    let store = Store::open(Some(&state)).unwrap();
    let first =
        snapshot::create_with_mode(&workspace, &store, snapshot::CaptureMode::RespectGitignore)
            .unwrap();
    let after = tree_fingerprint(&workspace);
    assert_eq!(before, after, "source workspace changed during snapshot");

    let paths: std::collections::HashSet<_> = first
        .manifest
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    for expected in [
        ".gitignore",
        ".hidden",
        "empty",
        "global-excluded.txt",
        "large.bin",
        "nested/.gitignore",
        "nested/keep.cache",
        "ordinary-link",
        "projects/alpha/.git",
        "projects/alpha/.git/index",
        "projects/alpha/ignored-tracked.txt",
        "projects/alpha/untracked.txt",
        "script.sh",
        "unrelated/beta/loose.txt",
    ] {
        assert!(
            paths.contains(expected),
            "expected included path {expected:?}"
        );
    }
    for excluded in [
        "drop.tmp",
        "ignored-dir/.gitignore",
        "ignored-dir/revive.txt",
        "nested/drop.cache",
        "projects/alpha/ignored-untracked.generated",
        "unrelated/beta/build/artifact",
    ] {
        assert!(
            !paths.contains(excluded),
            "expected excluded path {excluded:?}"
        );
    }
    assert_eq!(first.manifest.repositories.len(), 2);
    assert_eq!(first.manifest.ignore_rules.len(), 4);

    let second =
        snapshot::create_with_mode(&workspace, &store, snapshot::CaptureMode::RespectGitignore)
            .unwrap();
    assert_eq!(first.manifest.digest, second.manifest.digest);
    assert_eq!(second.new_blobs, 0);
    snapshot::verify(&store, &first.manifest).unwrap();

    let destination = temporary.path().join("materialized");
    snapshot::materialize(&store, &first.manifest, &destination).unwrap();
    assert_materialized(&destination, &first.manifest);
    assert_eq!(
        fs::read_to_string(destination.join("projects/alpha/ignored-tracked.txt")).unwrap(),
        "tracked wins\n"
    );
}

#[test]
fn active_ignore_change_changes_identity() {
    require_git();
    let workspace = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    write_file(workspace.path().join(".gitignore"), "*.tmp\n", 0o644);
    write_file(workspace.path().join("kept.txt"), "same\n", 0o644);
    write_file(workspace.path().join("ignored.tmp"), "ignored\n", 0o644);
    let store = Store::open(Some(state.path())).unwrap();
    let first = snapshot::create_with_mode(
        workspace.path(),
        &store,
        snapshot::CaptureMode::RespectGitignore,
    )
    .unwrap();
    write_file(workspace.path().join(".gitignore"), "ignored.tmp\n", 0o644);
    let second = snapshot::create_with_mode(
        workspace.path(),
        &store,
        snapshot::CaptureMode::RespectGitignore,
    )
    .unwrap();
    assert_ne!(first.manifest.digest, second.manifest.digest);
}

#[test]
fn capture_all_includes_ignored_paths_without_ignore_rules() {
    require_git();
    let workspace = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    write_file(
        workspace.path().join(".gitignore"),
        "ignored/\n*.tmp\n",
        0o644,
    );
    mkdir(&workspace.path().join("ignored"), 0o755);
    write_file(
        workspace.path().join("ignored/artifact.txt"),
        "included by capture all\n",
        0o644,
    );
    write_file(workspace.path().join("root.tmp"), "included\n", 0o644);
    let store = Store::open(Some(state.path())).unwrap();

    let result =
        snapshot::create_with_mode(workspace.path(), &store, snapshot::CaptureMode::All).unwrap();
    let paths: std::collections::HashSet<_> = result
        .manifest
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();

    assert!(paths.contains("ignored/artifact.txt"));
    assert!(paths.contains("root.tmp"));
    assert_eq!(result.excluded_paths, 0);
    assert!(result.manifest.ignore_rules.is_empty());
    snapshot::verify(&store, &result.manifest).unwrap();
}

#[test]
fn complete_capture_is_default_and_gitignore_filtering_is_explicit() {
    require_git();
    let workspace = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    write_file(workspace.path().join(".gitignore"), "*.tmp\n", 0o644);
    write_file(workspace.path().join("included.tmp"), "complete\n", 0o644);
    let store = Store::open(Some(state.path())).unwrap();

    let complete = snapshot::create(workspace.path(), &store).unwrap();
    assert!(
        complete
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "included.tmp")
    );
    assert_eq!(complete.excluded_paths, 0);
    assert!(complete.manifest.ignore_rules.is_empty());

    let filtered = snapshot::create_with_mode(
        workspace.path(),
        &store,
        snapshot::CaptureMode::RespectGitignore,
    )
    .unwrap();
    assert!(
        !filtered
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "included.tmp")
    );
    assert_eq!(filtered.excluded_paths, 1);
    assert_eq!(filtered.manifest.ignore_rules.len(), 1);
}

#[test]
fn every_public_command_has_useful_help_without_side_effects() {
    let binary = env!("CARGO_BIN_EXE_agentlab");
    for (command, expected) in [
        ("snapshot", "agentlab snapshot"),
        ("run", "Capture every supported path"),
        ("list", "agentlab list"),
        ("inspect", "agentlab inspect"),
        ("diff", "agentlab diff"),
        ("compare", "agentlab compare"),
        ("evaluate", "trusted host command"),
        ("report", "agentlab report"),
        ("review", "applies nothing"),
        ("apply", "mutates the selected host workspace"),
        ("stop", "agentlab stop"),
        ("resume", "--pi-auth"),
        ("fork", "agentlab fork"),
        ("rm", "agentlab rm"),
    ] {
        for arguments in [[command, "--help"], ["help", command]] {
            let output = Command::new(binary).args(arguments).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains(expected),
                "unexpected help for {command}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(
                output.stderr.is_empty(),
                "help for {command} wrote to stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if command == "run" {
                assert!(
                    String::from_utf8_lossy(&output.stdout)
                        .contains("Network policy (default: bridge)"),
                    "run help did not declare bridge as the default: {}",
                    String::from_utf8_lossy(&output.stdout)
                );
            }
        }
    }
}

#[test]
fn review_errors_explain_the_missing_part_of_the_command() {
    let binary = env!("CARGO_BIN_EXE_agentlab");
    for (arguments, expected) in [
        (
            vec!["review", "fixture-run", "--workspace", "."],
            "review requires `-- COMMAND [ARG ...]`",
        ),
        (
            vec!["review", "fixture-run", "--workspace", ".", "--"],
            "review requires a reviewer command after `--`",
        ),
        (vec!["review", "--", "/bin/true"], "review requires RUN"),
        (
            vec!["review", "fixture-run", "--", "/bin/true"],
            "review requires --workspace CURRENT",
        ),
    ] {
        let output = Command::new(binary).args(arguments).output().unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "unexpected review error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    for (arguments, expected) in [
        (
            vec!["apply", "--workspace", "."],
            "apply requires REVIEW_ID",
        ),
        (
            vec!["apply", "00000000-0000-4000-8000-000000000000"],
            "apply requires --workspace CURRENT",
        ),
    ] {
        let output = Command::new(binary).args(arguments).output().unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "unexpected apply error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn resume_pi_auth_requires_a_continuation_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentlab"))
        .args(["resume", "--pi-auth", "fixture-run"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("resume --pi-auth requires `-- COMMAND [ARG ...]`")
    );
}

#[test]
fn verification_recomputes_ignore_rule_digest() {
    require_git();
    let workspace = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    write_file(workspace.path().join(".gitignore"), "*.tmp\n", 0o644);
    write_file(workspace.path().join("kept.txt"), "same\n", 0o644);
    let store = Store::open(Some(state.path())).unwrap();
    let result = snapshot::create_with_mode(
        workspace.path(),
        &store,
        snapshot::CaptureMode::RespectGitignore,
    )
    .unwrap();
    let mut manifest = result.manifest;
    manifest.ignore_rules_digest = format!("sha256:{}", "0".repeat(64));
    let error = snapshot::verify(&store, &manifest).unwrap_err();
    assert!(
        format!("{error:#}").contains("workspace-ignore rule digest mismatch"),
        "unexpected error: {error:#}"
    );
}

#[cfg(unix)]
#[test]
fn unsupported_special_file_fails_precisely() {
    let workspace = tempfile::tempdir().unwrap();
    let status = Command::new("mkfifo")
        .arg(workspace.path().join("pipe"))
        .status()
        .unwrap();
    assert!(status.success());
    let state = tempfile::tempdir().unwrap();
    let store = Store::open(Some(state.path())).unwrap();
    let error = snapshot::create(workspace.path(), &store).unwrap_err();
    assert!(
        format!("{error:#}").contains("unsupported special file \"pipe\""),
        "unexpected error: {error:#}"
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_path_fails_instead_of_normalizing() {
    use std::os::unix::ffi::OsStringExt;
    let workspace = tempfile::tempdir().unwrap();
    let invalid = OsString::from_vec(vec![b'b', b'a', b'd', b'-', 0xff]);
    if fs::write(workspace.path().join(invalid), b"content").is_err() {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let store = Store::open(Some(state.path())).unwrap();
    let error = snapshot::create(workspace.path(), &store).unwrap_err();
    assert!(format!("{error:#}").contains("not valid UTF-8"));
}

#[test]
fn cli_and_machine_global_excludes() {
    require_git();
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    let state = temporary.path().join("state");
    mkdir(&workspace, 0o755);
    write_file(
        workspace.join(".gitignore"),
        "# activate Git engine\n",
        0o644,
    );
    write_file(
        workspace.join("machine-global.txt"),
        "must be included\n",
        0o644,
    );
    let global_ignore = temporary.path().join("global-ignore");
    write_file(&global_ignore, "machine-global.txt\n", 0o600);
    let global_config = temporary.path().join("global-gitconfig");
    write_file(
        &global_config,
        &format!("[core]\n\texcludesFile = {}\n", global_ignore.display()),
        0o600,
    );
    let binary = env!("CARGO_BIN_EXE_agentlab");
    let version = Command::new(binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    let version = String::from_utf8(version.stdout).unwrap();
    assert!(
        version.starts_with("agentlab 0.1.0-dev"),
        "unexpected version output: {version:?}"
    );
    let help = Command::new(binary).arg("--help").output().unwrap();
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("agentlab snapshot")
    );

    let output = Command::new(binary)
        .args(["snapshot", "--workspace"])
        .arg(&workspace)
        .arg("--json")
        .env("AGENTLAB_STATE_DIR", &state)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    let digest = summary["digest"].as_str().unwrap();
    let inspect = Command::new(binary)
        .args(["inspect", "--verify", "--json", digest])
        .env("AGENTLAB_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(
        inspect.status.success(),
        "{}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    let manifest: Manifest = serde_json::from_slice(&inspect.stdout).unwrap();
    assert!(
        manifest
            .entries
            .iter()
            .any(|entry| entry.path == "machine-global.txt")
    );
    assert!(!String::from_utf8_lossy(&inspect.stdout).contains("must be included"));
}

#[test]
fn removed_factor_flags_fail_with_real_input_guidance() {
    let binary = env!("CARGO_BIN_EXE_agentlab");
    let cases = [
        (
            vec![
                "run",
                "--image",
                "unused",
                "--factor",
                "skill=on",
                "--",
                "/bin/true",
            ],
            "vary a real workspace snapshot, image, command, or runtime input",
        ),
        (
            vec!["compare", "--expect-factor", "skill", "left", "right"],
            "differences in actual resolved inputs",
        ),
        (
            vec!["report", "--factor", "skill", "run"],
            "real run-input, workspace, image, and portable-base identities",
        ),
    ];
    for (arguments, guidance) in cases {
        let output = Command::new(binary).args(arguments).output().unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(guidance), "unexpected error: {stderr}");
    }
}

#[test]
fn legacy_v1_specs_remain_readable_but_factors_do_not_define_input_identity() {
    let state = tempfile::tempdir().unwrap();
    let store = Store::open(Some(state.path())).unwrap();
    let run_id = "00000000-0000-4000-8000-000000000001";
    store.create_run_directory(run_id).unwrap();
    let spec = serde_json::json!({
        "schema_version": "agentlab.run/v1",
        "run_id": run_id,
        "workspace_snapshot_digest": format!("sha256:{}", "1".repeat(64)),
        "image_requested": "alpine:3.21",
        "image_resolved_digest": "alpine@sha256:fixture",
        "docker_image_id": "sha256:fixture",
        "target_platform": "linux/arm64",
        "workspace_guest_path": "/workspace",
        "command": ["/bin/true"],
        "working_directory": "/workspace",
        "factors": {"skill": "on", "replicate": "1"},
        "resource_limits": {"memory": null, "cpus": null},
        "network_policy": "none",
        "captures": [],
        "secret_injections": [],
        "workspace_ignore_digest": format!("sha256:{}", "2".repeat(64)),
        "change_ignore": {
            "source": null,
            "digest": format!("sha256:{}", "3".repeat(64))
        },
        "backend_name": "docker-cli",
        "backend_version": "fixture",
        "agentlab_version": "0.1.0-dev"
    });
    store
        .write_run_file(
            run_id,
            "spec.json",
            &serde_json::to_vec_pretty(&spec).unwrap(),
        )
        .unwrap();

    let loaded = run::load_spec(&store, run_id).unwrap();
    assert_eq!(
        loaded.legacy_factors.get("skill").map(String::as_str),
        Some("on")
    );
    let identity_with_legacy_labels = run::compute_run_input_digest(&loaded).unwrap();
    let mut without_legacy_labels = loaded;
    without_legacy_labels.legacy_factors.clear();
    assert_eq!(
        identity_with_legacy_labels,
        run::compute_run_input_digest(&without_legacy_labels).unwrap()
    );
}

fn assert_materialized(root: &Path, manifest: &Manifest) {
    for entry in &manifest.entries {
        let path = join_snapshot_path(root, &entry.path);
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!(mode(&metadata), entry.mode, "mode for {:?}", entry.path);
        match entry.kind.as_str() {
            "file" => {
                let content = fs::read(&path).unwrap();
                let digest = format!("sha256:{}", hex_digest(&Sha256::digest(content)));
                assert_eq!(digest, entry.digest, "content for {:?}", entry.path);
            }
            "directory" => assert!(metadata.is_dir()),
            "symlink" => assert_eq!(
                fs::read_link(path).unwrap(),
                PathBuf::from(&entry.link_target)
            ),
            _ => panic!("unexpected entry type"),
        }
    }
}

fn tree_fingerprint(root: &Path) -> String {
    let mut records = BTreeMap::new();
    fingerprint_directory(root, root, &mut records);
    let mut hasher = Sha256::new();
    for (path, record) in records {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(record);
        hasher.update([0]);
    }
    hex_digest(&hasher.finalize())
}

fn fingerprint_directory(root: &Path, directory: &Path, records: &mut BTreeMap<String, Vec<u8>>) {
    let mut children: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let path = child.path();
        let metadata = fs::symlink_metadata(&path).unwrap();
        let relative = path.strip_prefix(root).unwrap();
        let relative = relative
            .to_str()
            .unwrap()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let mut record =
            format!("{:o}:{}", mode(&metadata), metadata.file_type().is_dir()).into_bytes();
        if metadata.is_file() {
            record.extend_from_slice(&Sha256::digest(fs::read(&path).unwrap()));
        } else if metadata.file_type().is_symlink() {
            record.extend_from_slice(fs::read_link(&path).unwrap().as_os_str().as_encoded_bytes());
        }
        records.insert(relative, record);
        if metadata.is_dir() {
            fingerprint_directory(root, &path, records);
        }
    }
}

fn initialize_repository(path: &Path) {
    mkdir(path, 0o755);
    let output = Command::new("git")
        .args(["init", "--quiet", "--initial-branch=main"])
        .arg(path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success());
}

fn run_git(root: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?}: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn require_git() {
    assert!(Command::new("git").arg("--version").output().is_ok());
}

fn write_file(path: impl AsRef<Path>, content: &str, mode: u32) {
    write_bytes(path, content.as_bytes(), mode);
}

fn write_bytes(path: impl AsRef<Path>, content: &[u8], mode_value: u32) {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
    set_mode(path, mode_value);
}

fn mkdir(path: &Path, mode_value: u32) {
    fs::create_dir_all(path).unwrap();
    set_mode(path, mode_value);
}

fn join_snapshot_path(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(root.to_path_buf(), |mut path, part| {
            path.push(part);
            path
        })
}

#[cfg(unix)]
fn create_symlink(target: &str, path: &Path) {
    std::os::unix::fs::symlink(target, path).unwrap();
}

#[cfg(windows)]
fn create_symlink(target: &str, path: &Path) {
    std::os::windows::fs::symlink_file(target, path).unwrap();
}

#[cfg(unix)]
fn mode(metadata: &Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.mode() & 0o7777
}

#[cfg(not(unix))]
fn mode(metadata: &Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}
