mod ignore;
mod materialize;

pub use materialize::materialize;

use std::collections::HashSet;
use std::fs::{self, FileType, Metadata};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::{Store, hex_digest, normalize_digest};

pub const SCHEMA_VERSION: &str = "agentlab.snapshot/v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub mode: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub size: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub link_target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IgnoreRule {
    pub path: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Repository {
    pub path: String,
    pub metadata_path: String,
    pub metadata_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub schema_version: String,
    pub digest: String,
    pub ignore_rules_digest: String,
    pub ignore_rules: Vec<IgnoreRule>,
    pub repositories: Vec<Repository>,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotResult {
    #[serde(skip)]
    pub manifest: Manifest,
    pub workspace: PathBuf,
    pub included_paths: usize,
    pub excluded_paths: usize,
    pub logical_bytes: u64,
    pub new_blobs: usize,
    pub reused_blobs: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Serialize)]
struct IdentityDocument<'a> {
    schema_version: &'a str,
    ignore_rules_digest: &'a str,
    ignore_rules: &'a [IgnoreRule],
    entries: &'a [Entry],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateKind {
    File,
    Directory,
    Symlink,
    Special,
}

#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub path: String,
    pub absolute: PathBuf,
    pub kind: CandidateKind,
    pub mode: u32,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

pub fn create(workspace: &Path, store: &Store) -> Result<SnapshotResult> {
    let (root, candidates) = scan(workspace)?;
    let discovery = ignore::discover_repositories(&root, &candidates);
    let ignored = ignore::ignored_paths(&root, &candidates, &discovery.metadata_paths)?;
    let (ignore_rules, ignore_rules_digest) = ignore::active_ignore_rules(&candidates, &ignored)?;

    let mut included: HashSet<String> = HashSet::with_capacity(candidates.len());
    for candidate in &candidates {
        let keep = !ignored.contains(&candidate.path)
            || discovery.tracked_paths.contains(&candidate.path)
            || discovery.metadata_paths.contains(&candidate.path)
            || beneath_unknown_repository(&candidate.path, &discovery.unknown_repository_roots);
        if keep {
            included.insert(candidate.path.clone());
        }
    }
    let included_snapshot: Vec<String> = included.iter().cloned().collect();
    for path in included_snapshot {
        let mut current = path.as_str();
        while let Some((parent, _)) = current.rsplit_once('/') {
            if parent.is_empty() {
                break;
            }
            included.insert(parent.to_string());
            current = parent;
        }
    }

    let mut result = SnapshotResult {
        manifest: Manifest {
            schema_version: String::new(),
            digest: String::new(),
            ignore_rules_digest: String::new(),
            ignore_rules: Vec::new(),
            repositories: Vec::new(),
            entries: Vec::new(),
        },
        workspace: root,
        included_paths: 0,
        excluded_paths: 0,
        logical_bytes: 0,
        new_blobs: 0,
        reused_blobs: 0,
        warnings: discovery.warnings,
    };
    let mut entries = Vec::with_capacity(included.len());
    for candidate in &candidates {
        if !included.contains(&candidate.path) {
            result.excluded_paths += 1;
            continue;
        }
        let mut entry = Entry {
            path: candidate.path.clone(),
            kind: String::new(),
            mode: candidate.mode,
            size: 0,
            digest: String::new(),
            link_target: String::new(),
        };
        match candidate.kind {
            CandidateKind::File => {
                entry.kind = "file".to_string();
                let stored = store
                    .put_file(&candidate.absolute)
                    .with_context(|| format!("capture file {:?}", candidate.path))?;
                let after = fs::symlink_metadata(&candidate.absolute)
                    .with_context(|| format!("reinspect file {:?}", candidate.path))?;
                if !file_metadata_unchanged(candidate, &after) {
                    bail!(
                        "workspace changed while snapshotting {:?}; retry from a stable source",
                        candidate.path
                    );
                }
                entry.digest = stored.digest;
                entry.size = stored.size;
                result.logical_bytes += stored.size;
                if stored.created {
                    result.new_blobs += 1;
                } else {
                    result.reused_blobs += 1;
                }
            }
            CandidateKind::Directory => entry.kind = "directory".to_string(),
            CandidateKind::Symlink => {
                entry.kind = "symlink".to_string();
                let target = fs::read_link(&candidate.absolute)
                    .with_context(|| format!("read symlink {:?}", candidate.path))?;
                entry.link_target = target
                    .to_str()
                    .with_context(|| {
                        format!(
                            "symlink target for {:?} is not valid UTF-8 and cannot be represented by snapshot schema {SCHEMA_VERSION}",
                            candidate.path
                        )
                    })?
                    .to_string();
            }
            CandidateKind::Special => bail!(
                "unsupported special file {:?} with mode {:o}; special files are never silently omitted",
                candidate.path,
                candidate.mode
            ),
        }
        entries.push(entry);
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));

    let identity = IdentityDocument {
        schema_version: SCHEMA_VERSION,
        ignore_rules_digest: &ignore_rules_digest,
        ignore_rules: &ignore_rules,
        entries: &entries,
    };
    let identity_bytes = serde_json::to_vec(&identity).context("encode snapshot identity")?;
    let digest = format!("sha256:{}", hex_digest(&Sha256::digest(identity_bytes)));
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION.to_string(),
        digest,
        ignore_rules_digest,
        ignore_rules,
        repositories: discovery.repositories,
        entries,
    };
    let mut manifest_bytes =
        serde_json::to_vec_pretty(&manifest).context("encode snapshot manifest")?;
    manifest_bytes.push(b'\n');
    store
        .write_snapshot(&manifest.digest, &manifest_bytes)
        .context("store snapshot manifest")?;
    result.included_paths = manifest.entries.len();
    result.manifest = manifest;
    Ok(result)
}

pub fn load(store: &Store, digest: &str) -> Result<Manifest> {
    let bytes = store.read_snapshot(digest)?;
    let manifest: Manifest = serde_json::from_slice(&bytes).context("decode snapshot manifest")?;
    if manifest.schema_version != SCHEMA_VERSION {
        bail!("unsupported snapshot schema {:?}", manifest.schema_version);
    }
    Ok(manifest)
}

pub fn verify(store: &Store, manifest: &Manifest) -> Result<()> {
    validate_manifest(manifest)?;
    let computed_ignore_rules_digest = ignore::ignore_rules_digest(&manifest.ignore_rules);
    if manifest.ignore_rules_digest != computed_ignore_rules_digest {
        bail!(
            "workspace-ignore rule digest mismatch: recorded {}, computed {computed_ignore_rules_digest}",
            manifest.ignore_rules_digest
        );
    }
    let identity = IdentityDocument {
        schema_version: &manifest.schema_version,
        ignore_rules_digest: &manifest.ignore_rules_digest,
        ignore_rules: &manifest.ignore_rules,
        entries: &manifest.entries,
    };
    let identity_bytes = serde_json::to_vec(&identity).context("encode snapshot identity")?;
    let expected = format!("sha256:{}", hex_digest(&Sha256::digest(identity_bytes)));
    if manifest.digest != expected {
        bail!(
            "snapshot manifest digest mismatch: recorded {}, computed {expected}",
            manifest.digest
        );
    }
    for entry in &manifest.entries {
        if entry.kind != "file" {
            continue;
        }
        let mut blob = store
            .open_blob(&entry.digest)
            .with_context(|| format!("open blob for {:?}", entry.path))?;
        let mut hasher = Sha256::new();
        let size = std::io::copy(&mut blob, &mut hasher)
            .with_context(|| format!("verify blob for {:?}", entry.path))?;
        let actual = format!("sha256:{}", hex_digest(&hasher.finalize()));
        if actual != entry.digest || size != entry.size {
            bail!("blob integrity mismatch for {:?}", entry.path);
        }
    }
    Ok(())
}

fn scan(workspace: &Path) -> Result<(PathBuf, Vec<Candidate>)> {
    let workspace = if workspace.as_os_str().is_empty() {
        Path::new(".")
    } else {
        workspace
    };
    let absolute = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolve current directory")?
            .join(workspace)
    };
    let root = fs::canonicalize(&absolute)
        .with_context(|| format!("resolve workspace {}", workspace.display()))?;
    root.to_str().context("workspace path is not valid UTF-8")?;
    if !fs::metadata(&root).context("inspect workspace")?.is_dir() {
        bail!("workspace {:?} is not a directory", workspace);
    }
    let mut candidates = Vec::new();
    scan_directory(&root, &root, &mut candidates)?;
    candidates.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((root, candidates))
}

fn scan_directory(root: &Path, directory: &Path, candidates: &mut Vec<Candidate>) -> Result<()> {
    let mut children: Vec<_> = fs::read_dir(directory)
        .with_context(|| format!("walk workspace directory {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let absolute = child.path();
        let metadata = fs::symlink_metadata(&absolute)
            .with_context(|| format!("inspect workspace path {}", absolute.display()))?;
        let relative = absolute
            .strip_prefix(root)
            .context("compute workspace-relative path")?;
        let path = slash_path(relative)?;
        let kind = candidate_from_metadata(&metadata);
        candidates.push(Candidate {
            path,
            absolute: absolute.clone(),
            kind,
            mode: portable_mode(&metadata),
            size: metadata.len(),
            modified: metadata.modified().ok(),
        });
        if kind == CandidateKind::Directory {
            scan_directory(root, &absolute, candidates)?;
        }
    }
    Ok(())
}

fn slash_path(path: &Path) -> Result<String> {
    let mut pieces = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => pieces.push(
                value
                    .to_str()
                    .with_context(|| {
                        format!(
                            "workspace path is not valid UTF-8 and cannot be represented by snapshot schema {SCHEMA_VERSION}: {path:?}"
                        )
                    })?
                    .to_string(),
            ),
            _ => bail!("unsafe workspace-relative path {path:?}"),
        }
    }
    Ok(pieces.join("/"))
}

fn candidate_from_metadata(metadata: &Metadata) -> CandidateKind {
    let kind: FileType = metadata.file_type();
    if kind.is_file() {
        CandidateKind::File
    } else if kind.is_dir() {
        CandidateKind::Directory
    } else if kind.is_symlink() {
        CandidateKind::Symlink
    } else {
        CandidateKind::Special
    }
}

fn file_metadata_unchanged(candidate: &Candidate, after: &Metadata) -> bool {
    candidate_from_metadata(after) == candidate.kind
        && portable_mode(after) == candidate.mode
        && after.len() == candidate.size
        && after.modified().ok() == candidate.modified
}

#[cfg(unix)]
pub(crate) fn portable_mode(metadata: &Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.mode() & 0o7777
}

#[cfg(not(unix))]
pub(crate) fn portable_mode(metadata: &Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

fn beneath_unknown_repository(path: &str, roots: &HashSet<String>) -> bool {
    roots.iter().any(|root| {
        root == "."
            || path == root
            || path
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn validate_manifest(manifest: &Manifest) -> Result<()> {
    let mut previous_rule: Option<&str> = None;
    for rule in &manifest.ignore_rules {
        validate_relative_path(&rule.path)?;
        if previous_rule.is_some_and(|value| rule.path.as_str() <= value) {
            bail!("snapshot ignore rules are not uniquely sorted");
        }
        previous_rule = Some(&rule.path);
        normalize_digest(&rule.digest)
            .with_context(|| format!("invalid ignore-rule digest for {:?}", rule.path))?;
    }
    let mut previous: Option<&str> = None;
    for entry in &manifest.entries {
        validate_relative_path(&entry.path)?;
        if previous.is_some_and(|value| entry.path.as_str() <= value) {
            bail!("snapshot entries are not uniquely sorted");
        }
        previous = Some(&entry.path);
        match entry.kind.as_str() {
            "file" => {
                normalize_digest(&entry.digest)
                    .with_context(|| format!("invalid blob digest for {:?}", entry.path))?;
            }
            "directory" => {}
            "symlink" => {
                if entry.link_target.contains('\0') {
                    bail!("symlink target for {:?} contains NUL", entry.path);
                }
            }
            value => bail!("unsupported manifest entry type {value:?}"),
        }
    }
    Ok(())
}

pub(crate) fn validate_relative_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path == "."
        || path.starts_with('/')
        || path
            .split('/')
            .any(|piece| piece.is_empty() || piece == "." || piece == "..")
    {
        bail!("unsafe snapshot path {path:?}");
    }
    Ok(())
}

pub(crate) fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
    validate_relative_path(relative)?;
    let mut path = root.to_path_buf();
    for component in relative.split('/') {
        path.push(component);
    }
    Ok(path)
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn permission_only_change_is_detected() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("file");
        fs::write(&path, b"unchanged bytes").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let before = fs::symlink_metadata(&path).unwrap();
        let candidate = Candidate {
            path: "file".to_string(),
            absolute: path.clone(),
            kind: candidate_from_metadata(&before),
            mode: portable_mode(&before),
            size: before.len(),
            modified: before.modified().ok(),
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let after = fs::symlink_metadata(path).unwrap();
        assert!(!file_metadata_unchanged(&candidate, &after));
    }
}
