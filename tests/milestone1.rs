use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::process::Command;

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
    let first = snapshot::create(&workspace, &store).unwrap();
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

    let second = snapshot::create(&workspace, &store).unwrap();
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
    let first = snapshot::create(workspace.path(), &store).unwrap();
    write_file(workspace.path().join(".gitignore"), "ignored.tmp\n", 0o644);
    let second = snapshot::create(workspace.path(), &store).unwrap();
    assert_ne!(first.manifest.digest, second.manifest.digest);
}

#[test]
fn verification_recomputes_ignore_rule_digest() {
    require_git();
    let workspace = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    write_file(workspace.path().join(".gitignore"), "*.tmp\n", 0o644);
    write_file(workspace.path().join("kept.txt"), "same\n", 0o644);
    let store = Store::open(Some(state.path())).unwrap();
    let result = snapshot::create(workspace.path(), &store).unwrap();
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
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        "agentlab 0.1.0-dev\n"
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
