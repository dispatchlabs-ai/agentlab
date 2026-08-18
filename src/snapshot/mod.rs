mod ignore;
mod materialize;

pub use materialize::materialize;

use std::collections::HashSet;
#[cfg(any(not(unix), test))]
use std::fs::OpenOptions;
use std::fs::{self, File, FileType, Metadata};
#[cfg(unix)]
use std::os::fd::{AsFd, OwnedFd};
#[cfg(not(unix))]
use std::path::Component;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::{Store, hex_digest, normalize_digest};

pub const SCHEMA_VERSION: &str = "agentlab.snapshot/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    All,
    RespectGitignore,
}

impl CaptureMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::RespectGitignore => "respect-gitignore",
        }
    }
}

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
    #[cfg(any(not(unix), test))]
    pub absolute: PathBuf,
    pub kind: CandidateKind,
    pub mode: u32,
    pub size: u64,
    pub modified: Option<SystemTime>,
    #[cfg(unix)]
    pub device: u64,
    #[cfg(unix)]
    pub inode: u64,
    #[cfg(unix)]
    pub changed_seconds: i64,
    #[cfg(unix)]
    pub changed_nanoseconds: i64,
}

/// A workspace generation pinned for the complete duration of an operation.
///
/// On Unix, every snapshot read and traversal starts from this directory
/// descriptor. The canonical pathname is retained only for display, Git
/// discovery, and a reachability check; it is never used to open captured
/// content.
pub(crate) struct PinnedWorkspace {
    path: PathBuf,
    #[cfg(unix)]
    root: OwnedFd,
}

impl PinnedWorkspace {
    pub(crate) fn open(workspace: &Path) -> Result<Self> {
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
        let path = fs::canonicalize(&absolute)
            .with_context(|| format!("resolve workspace {}", workspace.display()))?;
        path.to_str().context("workspace path is not valid UTF-8")?;

        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags};

            let root = rustix::fs::open(
                &path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .with_context(|| {
                format!(
                    "open workspace root {} without following symlinks",
                    path.display()
                )
            })?;
            let pinned = Self { path, root };
            pinned.verify_path_identity()?;
            Ok(pinned)
        }

        #[cfg(not(unix))]
        {
            if !fs::metadata(&path).context("inspect workspace")?.is_dir() {
                bail!("workspace {:?} is not a directory", workspace);
            }
            Ok(Self { path })
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(unix)]
    pub(crate) fn duplicate_root(&self) -> Result<OwnedFd> {
        rustix::io::dup(&self.root).context("duplicate pinned workspace root")
    }

    pub(crate) fn lock_identity(&self) -> Result<String> {
        #[cfg(unix)]
        {
            let metadata = metadata_for_fd(&self.root)?;
            Ok(format!(
                "unix-device-{}-inode-{}",
                unix_device(&metadata),
                unix_inode(&metadata)
            ))
        }

        #[cfg(not(unix))]
        Ok(format!("path-{}", self.path.display()))
    }

    /// Prove that the selected pathname still names the pinned directory
    /// generation. This deliberately checks identity, not timestamps: apply
    /// is allowed to change the pinned directory's contents.
    pub(crate) fn verify_path_identity(&self) -> Result<()> {
        #[cfg(unix)]
        {
            let open = metadata_for_fd(&self.root)?;
            let visible = fs::symlink_metadata(&self.path)
                .with_context(|| format!("reinspect workspace root {}", self.path.display()))?;
            if !visible.file_type().is_dir()
                || unix_device(&open) != unix_device(&visible)
                || unix_inode(&open) != unix_inode(&visible)
            {
                bail!(
                    "workspace root {} was renamed or replaced during the operation",
                    self.path.display()
                );
            }
        }
        #[cfg(not(unix))]
        if !fs::metadata(&self.path)
            .with_context(|| format!("reinspect workspace root {}", self.path.display()))?
            .is_dir()
        {
            bail!(
                "workspace root {} is no longer a directory",
                self.path.display()
            );
        }
        Ok(())
    }
}

pub fn create(workspace: &Path, store: &Store) -> Result<SnapshotResult> {
    create_with_mode(workspace, store, CaptureMode::All)
}

pub fn create_with_mode(
    workspace: &Path,
    store: &Store,
    capture_mode: CaptureMode,
) -> Result<SnapshotResult> {
    let source = PinnedWorkspace::open(workspace)?;
    create_from_pinned(&source, store, capture_mode)
}

pub(crate) fn create_from_pinned(
    source: &PinnedWorkspace,
    store: &Store,
    capture_mode: CaptureMode,
) -> Result<SnapshotResult> {
    let (root, candidates, root_candidate) = scan(source)?;
    let discovery = ignore::discover_repositories(&root, &candidates);
    let ignored = match capture_mode {
        CaptureMode::RespectGitignore => {
            ignore::ignored_paths(&root, &candidates, &discovery.metadata_paths)?
        }
        CaptureMode::All => HashSet::new(),
    };
    let (ignore_rules, ignore_rules_digest) = match capture_mode {
        CaptureMode::RespectGitignore => {
            ignore::active_ignore_rules(source, &candidates, &ignored)?
        }
        CaptureMode::All => {
            let rules = Vec::new();
            let digest = ignore::ignore_rules_digest(&rules);
            (rules, digest)
        }
    };

    let mut included: HashSet<String> = HashSet::with_capacity(candidates.len());
    for candidate in &candidates {
        let keep = capture_mode == CaptureMode::All
            || !ignored.contains(&candidate.path)
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
                let mut source_file = source.open_stable_candidate(candidate)?;
                let stored = store
                    .put_reader(&mut source_file)
                    .with_context(|| format!("capture file {:?}", candidate.path))?;
                let opened_after = source_file
                    .metadata()
                    .with_context(|| format!("reinspect open file {:?}", candidate.path))?;
                if !file_metadata_unchanged(candidate, &opened_after)
                    || !source.path_entry_unchanged(candidate)?
                {
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
            CandidateKind::Directory => {
                source.verify_directory(candidate)?;
                entry.kind = "directory".to_string();
            }
            CandidateKind::Symlink => {
                entry.kind = "symlink".to_string();
                entry.link_target = source.read_stable_link(candidate)?;
            }
            CandidateKind::Special => bail!(
                "unsupported special file {:?} with mode {:o}; special files are never silently omitted",
                candidate.path,
                candidate.mode
            ),
        }
        entries.push(entry);
    }
    source.verify_directory(&root_candidate)?;
    source.verify_path_identity()?;
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
            .open_blob(&entry.digest, entry.size)
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

fn scan(source: &PinnedWorkspace) -> Result<(PathBuf, Vec<Candidate>, Candidate)> {
    #[cfg(unix)]
    {
        let root_metadata = metadata_for_fd(&source.root)?;
        let root_candidate = candidate_from_open_metadata(
            ".".to_owned(),
            source.path.clone(),
            CandidateKind::Directory,
            &root_metadata,
        );
        let mut candidates = Vec::new();
        scan_directory_descriptor(source, &source.root, "", &mut candidates)?;
        candidates.sort_by(|left, right| left.path.cmp(&right.path));
        Ok((source.path.clone(), candidates, root_candidate))
    }

    #[cfg(not(unix))]
    {
        let mut candidates = Vec::new();
        scan_directory_path(&source.path, &source.path, &mut candidates)?;
        candidates.sort_by(|left, right| left.path.cmp(&right.path));
        let metadata = fs::metadata(&source.path).context("inspect workspace")?;
        let root_candidate = candidate_from_open_metadata(
            ".".to_owned(),
            source.path.clone(),
            CandidateKind::Directory,
            &metadata,
        );
        Ok((source.path.clone(), candidates, root_candidate))
    }
}

#[cfg(unix)]
fn scan_directory_descriptor(
    source: &PinnedWorkspace,
    directory: &impl AsFd,
    prefix: &str,
    candidates: &mut Vec<Candidate>,
) -> Result<()> {
    use rustix::fs::{AtFlags, Dir, FileType as RustixFileType, Mode, OFlags};

    let mut names = Vec::new();
    let entries = Dir::read_from(directory).with_context(|| {
        format!(
            "walk pinned workspace directory {:?}",
            if prefix.is_empty() { "." } else { prefix }
        )
    })?;
    for entry in entries {
        let entry = entry.context("read pinned workspace directory entry")?;
        let bytes = entry.file_name().to_bytes();
        if matches!(bytes, b"." | b"..") {
            continue;
        }
        let name = std::str::from_utf8(bytes).with_context(|| {
            format!(
                "workspace path is not valid UTF-8 and cannot be represented by snapshot schema {SCHEMA_VERSION}"
            )
        })?;
        names.push(name.to_owned());
    }
    names.sort();

    for name in names {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let stat = rustix::fs::statat(directory, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
            .with_context(|| format!("inspect pinned workspace path {path:?}"))?;
        let kind = match RustixFileType::from_raw_mode(stat.st_mode as _) {
            RustixFileType::RegularFile => CandidateKind::File,
            RustixFileType::Directory => CandidateKind::Directory,
            RustixFileType::Symlink => CandidateKind::Symlink,
            _ => CandidateKind::Special,
        };
        let absolute = source.path.join(path.split('/').collect::<PathBuf>());

        match kind {
            CandidateKind::File => {
                let descriptor = rustix::fs::openat(
                    directory,
                    name.as_str(),
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .with_context(|| format!("open pinned workspace file {path:?}"))?;
                let metadata = metadata_for_fd(&descriptor)?;
                let candidate =
                    candidate_from_open_metadata(path, absolute, CandidateKind::File, &metadata);
                if !stat_matches_candidate(&stat, &candidate) {
                    bail!(
                        "workspace changed while scanning {:?}; retry from a stable source",
                        candidate.path
                    );
                }
                candidates.push(candidate);
            }
            CandidateKind::Directory => {
                let descriptor = rustix::fs::openat(
                    directory,
                    name.as_str(),
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .with_context(|| format!("open pinned workspace directory {path:?}"))?;
                let metadata = metadata_for_fd(&descriptor)?;
                let candidate = candidate_from_open_metadata(
                    path.clone(),
                    absolute,
                    CandidateKind::Directory,
                    &metadata,
                );
                if !stat_matches_candidate(&stat, &candidate) {
                    bail!(
                        "workspace changed while scanning {:?}; retry from a stable source",
                        candidate.path
                    );
                }
                candidates.push(candidate.clone());
                scan_directory_descriptor(source, &descriptor, &path, candidates)?;
                let opened_after = metadata_for_fd(&descriptor)?;
                let visible_after =
                    rustix::fs::statat(directory, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
                        .with_context(|| format!("reinspect workspace directory {path:?}"))?;
                if !file_metadata_unchanged(&candidate, &opened_after)
                    || !stat_matches_candidate(&visible_after, &candidate)
                {
                    bail!(
                        "workspace changed while scanning {:?}; retry from a stable source",
                        candidate.path
                    );
                }
            }
            CandidateKind::Symlink | CandidateKind::Special => {
                candidates.push(candidate_from_stat(path, absolute, kind, &stat)?);
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn scan_directory_path(
    root: &Path,
    directory: &Path,
    candidates: &mut Vec<Candidate>,
) -> Result<()> {
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
        candidates.push(candidate_from_open_metadata(
            path,
            absolute.clone(),
            kind,
            &metadata,
        ));
        if kind == CandidateKind::Directory {
            scan_directory_path(root, &absolute, candidates)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
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

fn candidate_from_open_metadata(
    path: String,
    _absolute: PathBuf,
    kind: CandidateKind,
    metadata: &Metadata,
) -> Candidate {
    Candidate {
        path,
        #[cfg(any(not(unix), test))]
        absolute: _absolute,
        kind,
        mode: portable_mode(metadata),
        size: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        device: unix_device(metadata),
        #[cfg(unix)]
        inode: unix_inode(metadata),
        #[cfg(unix)]
        changed_seconds: unix_changed_seconds(metadata),
        #[cfg(unix)]
        changed_nanoseconds: unix_changed_nanoseconds(metadata),
    }
}

#[cfg(unix)]
fn candidate_from_stat(
    path: String,
    _absolute: PathBuf,
    kind: CandidateKind,
    stat: &rustix::fs::Stat,
) -> Result<Candidate> {
    Ok(Candidate {
        path,
        #[cfg(test)]
        absolute: _absolute,
        kind,
        mode: (stat.st_mode as u32) & 0o7777,
        size: u64::try_from(stat.st_size).context("workspace entry has negative size")?,
        modified: None,
        device: stat.st_dev as u64,
        inode: stat.st_ino,
        changed_seconds: 0,
        changed_nanoseconds: 0,
    })
}

#[cfg(unix)]
fn stat_matches_candidate(stat: &rustix::fs::Stat, candidate: &Candidate) -> bool {
    use rustix::fs::FileType as RustixFileType;

    let kind = match RustixFileType::from_raw_mode(stat.st_mode as _) {
        RustixFileType::RegularFile => CandidateKind::File,
        RustixFileType::Directory => CandidateKind::Directory,
        RustixFileType::Symlink => CandidateKind::Symlink,
        _ => CandidateKind::Special,
    };
    kind == candidate.kind
        && ((stat.st_mode as u32) & 0o7777) == candidate.mode
        && u64::try_from(stat.st_size).ok() == Some(candidate.size)
        && stat.st_dev as u64 == candidate.device
        && stat.st_ino == candidate.inode
}

fn file_metadata_unchanged(candidate: &Candidate, after: &Metadata) -> bool {
    let portable = candidate_from_metadata(after) == candidate.kind
        && portable_mode(after) == candidate.mode
        && after.len() == candidate.size
        && after.modified().ok() == candidate.modified;
    #[cfg(unix)]
    {
        portable
            && unix_device(after) == candidate.device
            && unix_inode(after) == candidate.inode
            && unix_changed_seconds(after) == candidate.changed_seconds
            && unix_changed_nanoseconds(after) == candidate.changed_nanoseconds
    }
    #[cfg(not(unix))]
    {
        portable
    }
}

#[cfg(any(not(unix), test))]
fn open_stable_candidate(candidate: &Candidate) -> Result<File> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(&candidate.absolute).with_context(|| {
        format!(
            "open workspace file {:?} without following symlinks",
            candidate.path
        )
    })?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect open workspace file {:?}", candidate.path))?;
    if !file_metadata_unchanged(candidate, &metadata) {
        bail!(
            "workspace changed before snapshotting {:?}; retry from a stable source",
            candidate.path
        );
    }
    Ok(file)
}

#[cfg(unix)]
fn metadata_for_fd(fd: &impl AsFd) -> Result<Metadata> {
    let duplicate = rustix::io::dup(fd).context("duplicate filesystem descriptor")?;
    File::from(duplicate)
        .metadata()
        .context("inspect pinned filesystem descriptor")
}

impl PinnedWorkspace {
    #[cfg(unix)]
    fn open_parent(&self, relative: &str) -> Result<(OwnedFd, String)> {
        use rustix::fs::{Mode, OFlags};

        validate_relative_path(relative)?;
        let mut components: Vec<_> = relative.split('/').collect();
        let name = components
            .pop()
            .context("validated workspace path has no final component")?
            .to_owned();
        let mut directory = self.duplicate_root()?;
        for component in components {
            directory = rustix::fs::openat(
                &directory,
                component,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .with_context(|| {
                format!(
                    "open workspace ancestor {component:?} for {relative:?} without following symlinks"
                )
            })?;
        }
        Ok((directory, name))
    }

    fn open_stable_candidate(&self, candidate: &Candidate) -> Result<File> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags};

            let (parent, name) = self.open_parent(&candidate.path)?;
            let descriptor = rustix::fs::openat(
                &parent,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .with_context(|| {
                format!(
                    "open workspace file {:?} through the pinned root",
                    candidate.path
                )
            })?;
            let file = File::from(descriptor);
            let metadata = file
                .metadata()
                .with_context(|| format!("inspect open workspace file {:?}", candidate.path))?;
            if !file_metadata_unchanged(candidate, &metadata) {
                bail!(
                    "workspace changed before snapshotting {:?}; retry from a stable source",
                    candidate.path
                );
            }
            Ok(file)
        }

        #[cfg(not(unix))]
        open_stable_candidate(candidate)
    }

    fn path_entry_unchanged(&self, candidate: &Candidate) -> Result<bool> {
        #[cfg(unix)]
        {
            use rustix::fs::AtFlags;
            let (parent, name) = self.open_parent(&candidate.path)?;
            let stat = rustix::fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW)
                .with_context(|| format!("reinspect workspace path {:?}", candidate.path))?;
            Ok(stat_matches_candidate(&stat, candidate))
        }

        #[cfg(not(unix))]
        {
            let metadata = fs::symlink_metadata(&candidate.absolute)
                .with_context(|| format!("reinspect workspace path {:?}", candidate.path))?;
            Ok(file_metadata_unchanged(candidate, &metadata))
        }
    }

    fn verify_directory(&self, candidate: &Candidate) -> Result<()> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags};

            let descriptor = if candidate.path == "." {
                self.duplicate_root()?
            } else {
                let (parent, name) = self.open_parent(&candidate.path)?;
                rustix::fs::openat(
                    &parent,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .with_context(|| format!("reopen workspace directory {:?}", candidate.path))?
            };
            let metadata = metadata_for_fd(&descriptor)?;
            if !file_metadata_unchanged(candidate, &metadata)
                || (candidate.path != "." && !self.path_entry_unchanged(candidate)?)
            {
                bail!(
                    "workspace changed while snapshotting {:?}; retry from a stable source",
                    candidate.path
                );
            }
        }

        #[cfg(not(unix))]
        {
            let metadata = fs::metadata(&candidate.absolute)
                .with_context(|| format!("reinspect workspace directory {:?}", candidate.path))?;
            if !file_metadata_unchanged(candidate, &metadata) {
                bail!(
                    "workspace changed while snapshotting {:?}; retry from a stable source",
                    candidate.path
                );
            }
        }
        Ok(())
    }

    fn read_stable_link(&self, candidate: &Candidate) -> Result<String> {
        #[cfg(unix)]
        {
            use rustix::fs::AtFlags;

            let (parent, name) = self.open_parent(&candidate.path)?;
            let before = rustix::fs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW)
                .with_context(|| format!("inspect symlink {:?}", candidate.path))?;
            if !stat_matches_candidate(&before, candidate) {
                bail!(
                    "workspace changed before snapshotting {:?}; retry from a stable source",
                    candidate.path
                );
            }
            let target = rustix::fs::readlinkat(&parent, &name, Vec::new())
                .with_context(|| format!("read symlink {:?}", candidate.path))?;
            let after = rustix::fs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW)
                .with_context(|| format!("reinspect symlink {:?}", candidate.path))?;
            if !stat_matches_candidate(&after, candidate) {
                bail!(
                    "workspace changed while snapshotting {:?}; retry from a stable source",
                    candidate.path
                );
            }
            String::from_utf8(target.into_bytes()).with_context(|| {
                format!(
                    "symlink target for {:?} is not valid UTF-8 and cannot be represented by snapshot schema {SCHEMA_VERSION}",
                    candidate.path
                )
            })
        }

        #[cfg(not(unix))]
        {
            let target = fs::read_link(&candidate.absolute)
                .with_context(|| format!("read symlink {:?}", candidate.path))?;
            target
                .to_str()
                .with_context(|| {
                    format!(
                        "symlink target for {:?} is not valid UTF-8 and cannot be represented by snapshot schema {SCHEMA_VERSION}",
                        candidate.path
                    )
                })
                .map(ToOwned::to_owned)
        }
    }
}

#[cfg(unix)]
fn unix_device(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
}

#[cfg(unix)]
fn unix_inode(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ino()
}

#[cfg(unix)]
fn unix_changed_seconds(metadata: &Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ctime()
}

#[cfg(unix)]
fn unix_changed_nanoseconds(metadata: &Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ctime_nsec()
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
            device: unix_device(&before),
            inode: unix_inode(&before),
            changed_seconds: unix_changed_seconds(&before),
            changed_nanoseconds: unix_changed_nanoseconds(&before),
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let after = fs::symlink_metadata(path).unwrap();
        assert!(!file_metadata_unchanged(&candidate, &after));
    }

    #[cfg(unix)]
    #[test]
    fn same_size_and_restored_mtime_still_detects_content_substitution() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("file");
        fs::write(&path, b"original").unwrap();
        let before = fs::symlink_metadata(&path).unwrap();
        let candidate = Candidate {
            path: "file".to_owned(),
            absolute: path.clone(),
            kind: candidate_from_metadata(&before),
            mode: portable_mode(&before),
            size: before.len(),
            modified: before.modified().ok(),
            device: unix_device(&before),
            inode: unix_inode(&before),
            changed_seconds: unix_changed_seconds(&before),
            changed_nanoseconds: unix_changed_nanoseconds(&before),
        };

        std::thread::sleep(std::time::Duration::from_millis(2));
        fs::write(&path, b"replaced").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(before.modified().unwrap()))
            .unwrap();
        let after = fs::symlink_metadata(&path).unwrap();

        assert_eq!(candidate.size, after.len());
        assert_eq!(candidate.modified, after.modified().ok());
        assert!(!file_metadata_unchanged(&candidate, &after));
    }

    #[cfg(unix)]
    #[test]
    fn candidate_open_never_follows_a_replacement_symlink() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("file");
        let outside = temporary.path().join("outside");
        fs::write(&path, b"inside").unwrap();
        fs::write(&outside, b"outside").unwrap();
        let before = fs::symlink_metadata(&path).unwrap();
        let candidate = Candidate {
            path: "file".to_owned(),
            absolute: path.clone(),
            kind: candidate_from_metadata(&before),
            mode: portable_mode(&before),
            size: before.len(),
            modified: before.modified().ok(),
            device: unix_device(&before),
            inode: unix_inode(&before),
            changed_seconds: unix_changed_seconds(&before),
            changed_nanoseconds: unix_changed_nanoseconds(&before),
        };

        fs::remove_file(&path).unwrap();
        symlink(&outside, &path).unwrap();
        assert!(open_stable_candidate(&candidate).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn pinned_workspace_never_follows_a_replaced_intermediate_directory() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let outside = temporary.path().join("outside");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(workspace.join("parent")).unwrap();
        fs::write(workspace.join("parent/file"), b"inside").unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("file"), b"outside-secret").unwrap();

        let pinned = PinnedWorkspace::open(&workspace).unwrap();
        let (_, candidates, _) = scan(&pinned).unwrap();
        let candidate = candidates
            .iter()
            .find(|candidate| candidate.path == "parent/file")
            .unwrap();

        fs::rename(workspace.join("parent"), workspace.join("original-parent")).unwrap();
        symlink(&outside, workspace.join("parent")).unwrap();

        let error = pinned.open_stable_candidate(candidate).unwrap_err();
        assert!(
            format!("{error:#}").contains("without following symlinks"),
            "unexpected error: {error:#}"
        );
    }
}
