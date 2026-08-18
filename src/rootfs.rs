use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::EntryType;

use crate::store::{Store, hex_digest};

pub const ROOTFS_SCHEMA_VERSION: &str = "agentlab.rootfs/v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootFsEntry {
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
pub struct RootFsManifest {
    pub schema_version: String,
    pub digest: String,
    pub entries: Vec<RootFsEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobStorageSummary {
    pub required_paths: usize,
    pub unique_blobs: usize,
    pub reused_blobs: usize,
    pub created_blobs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    TypeChanged,
    ModeChanged,
    SymlinkChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootFsChange {
    pub path: String,
    pub change: ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<RootFsEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<RootFsEntry>,
}

#[derive(Serialize)]
struct RootFsIdentity<'a> {
    schema_version: &'a str,
    entries: &'a [RootFsEntry],
}

struct HardLink {
    path: String,
    target: String,
    mode: u32,
}

pub fn scan_export(export: &Path) -> Result<RootFsManifest> {
    let file = File::open(export)
        .with_context(|| format!("open root filesystem export {}", export.display()))?;
    let mut archive = tar::Archive::new(file);
    let mut entries = BTreeMap::new();
    let mut hard_links = Vec::new();
    for item in archive.entries().context("read root filesystem export")? {
        let mut item = item.context("read root filesystem archive entry")?;
        let path = normalize_archive_path(&item.path_bytes())?;
        if path.is_empty() {
            continue;
        }
        let header = item.header();
        let entry_type = header.entry_type();
        let mode = header.mode().context("read archive entry mode")? & 0o7777;
        let entry = if entry_type.is_file() {
            let (digest, size) = hash_reader(&mut item)
                .with_context(|| format!("hash exported rootfs file {path:?}"))?;
            Some(RootFsEntry {
                path: path.clone(),
                kind: "file".to_string(),
                mode,
                size,
                digest,
                link_target: String::new(),
            })
        } else if entry_type.is_dir() {
            Some(RootFsEntry {
                path: path.clone(),
                kind: "directory".to_string(),
                mode,
                size: 0,
                digest: String::new(),
                link_target: String::new(),
            })
        } else if entry_type.is_symlink() {
            let target = item
                .link_name()
                .context("read symlink target")?
                .context("symlink archive entry has no target")?;
            let target = target
                .to_str()
                .with_context(|| format!("symlink target for {path:?} is not valid UTF-8"))?;
            Some(RootFsEntry {
                path: path.clone(),
                kind: "symlink".to_string(),
                mode,
                size: 0,
                digest: String::new(),
                link_target: target.to_string(),
            })
        } else if entry_type == EntryType::Link {
            let target = item
                .link_name()
                .context("read hard-link target")?
                .context("hard-link archive entry has no target")?;
            hard_links.push(HardLink {
                path: path.clone(),
                target: normalize_archive_path(target.as_os_str().as_encoded_bytes())?,
                mode,
            });
            None
        } else if matches!(
            entry_type,
            EntryType::XGlobalHeader
                | EntryType::XHeader
                | EntryType::GNULongName
                | EntryType::GNULongLink
        ) {
            None
        } else {
            bail!(
                "unsupported root filesystem archive type {:?} at /{}",
                entry_type.as_byte() as char,
                path
            );
        };
        if let Some(entry) = entry {
            if entries.insert(path.clone(), entry).is_some() {
                bail!("duplicate path /{path} in root filesystem export");
            }
        }
    }

    while !hard_links.is_empty() {
        let mut unresolved = Vec::new();
        let mut progress = false;
        for link in hard_links {
            if let Some(target) = entries.get(&link.target) {
                if target.kind != "file" {
                    bail!(
                        "hard link /{} points to non-file /{}",
                        link.path,
                        link.target
                    );
                }
                let mut entry = target.clone();
                entry.path = link.path.clone();
                entry.mode = link.mode;
                entries.insert(link.path, entry);
                progress = true;
            } else {
                unresolved.push(link);
            }
        }
        if !progress {
            let paths: Vec<_> = unresolved.iter().map(|link| link.path.as_str()).collect();
            bail!("unresolved hard links in root filesystem export: {paths:?}");
        }
        hard_links = unresolved;
    }

    manifest_from_entries(entries.into_values().collect())
}

/// Build the canonical AgentLab root-filesystem identity from a provider's
/// complete, already-normalized filesystem inventory.
pub fn manifest_from_entries(entries: Vec<RootFsEntry>) -> Result<RootFsManifest> {
    let mut by_path = BTreeMap::new();
    for entry in entries {
        validate_manifest_entry(&entry)?;
        if by_path.insert(entry.path.clone(), entry).is_some() {
            bail!("duplicate path in root filesystem manifest");
        }
    }
    let entries: Vec<_> = by_path.into_values().collect();
    let identity = RootFsIdentity {
        schema_version: ROOTFS_SCHEMA_VERSION,
        entries: &entries,
    };
    let bytes = serde_json::to_vec(&identity).context("encode rootfs identity")?;
    Ok(RootFsManifest {
        schema_version: ROOTFS_SCHEMA_VERSION.to_string(),
        digest: format!("sha256:{}", hex_digest(&Sha256::digest(bytes))),
        entries,
    })
}

fn validate_manifest_entry(entry: &RootFsEntry) -> Result<()> {
    if entry.path.is_empty()
        || entry.path.starts_with('/')
        || entry.path.contains('\0')
        || entry
            .path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("unsafe root filesystem manifest path {:?}", entry.path);
    }
    if entry.mode > 0o7777 {
        bail!("invalid mode for root filesystem path /{}", entry.path);
    }
    let valid_file_digest = entry.digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    match entry.kind.as_str() {
        "file" if valid_file_digest && entry.link_target.is_empty() => {}
        "directory"
            if entry.size == 0 && entry.digest.is_empty() && entry.link_target.is_empty() => {}
        "symlink"
            if entry.size == 0 && entry.digest.is_empty() && !entry.link_target.contains('\0') => {}
        "file" => bail!("invalid file metadata for /{}", entry.path),
        "directory" => bail!("invalid directory metadata for /{}", entry.path),
        "symlink" => bail!("invalid symlink metadata for /{}", entry.path),
        kind => bail!(
            "unsupported root filesystem entry type {kind:?} at /{}",
            entry.path
        ),
    }
    Ok(())
}

pub fn store_required_file_blobs(
    export: &Path,
    manifest: &RootFsManifest,
    required_paths: &BTreeSet<String>,
    store: &Store,
) -> Result<BlobStorageSummary> {
    let entries: BTreeMap<_, _> = manifest
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut required_blobs = BTreeMap::new();
    for path in required_paths {
        let entry = entries
            .get(path.as_str())
            .with_context(|| format!("required root filesystem path /{path} is absent"))?;
        if entry.kind != "file" {
            bail!(
                "required root filesystem path /{path} is {}, not a file",
                entry.kind
            );
        }
        match required_blobs.insert(entry.digest.clone(), entry.size) {
            Some(size) if size != entry.size => {
                bail!(
                    "root filesystem digest {} has inconsistent sizes",
                    entry.digest
                )
            }
            _ => {}
        }
    }

    let unique_blobs = required_blobs.len();
    let mut missing_blobs = BTreeMap::new();
    for (digest, size) in &required_blobs {
        if !store.contains_blob(digest, *size)? {
            missing_blobs.insert(digest.clone(), *size);
        }
    }
    if missing_blobs.is_empty() {
        return Ok(BlobStorageSummary {
            required_paths: required_paths.len(),
            unique_blobs,
            reused_blobs: unique_blobs,
            created_blobs: 0,
        });
    }

    let file = File::open(export)
        .with_context(|| format!("open root filesystem export {}", export.display()))?;
    let mut archive = tar::Archive::new(file);
    let mut created_blobs = 0;
    for item in archive.entries().context("read root filesystem export")? {
        let mut item = item.context("read root filesystem archive entry")?;
        if !item.header().entry_type().is_file() {
            continue;
        }
        let path = normalize_archive_path(&item.path_bytes())?;
        let entry = entries
            .get(path.as_str())
            .with_context(|| format!("root filesystem manifest is missing file /{path}"))?;
        let Some(expected_size) = missing_blobs.get(&entry.digest).copied() else {
            continue;
        };
        let stored = store
            .put_reader(&mut item)
            .with_context(|| format!("store required rootfs file {path:?}"))?;
        if stored.digest != entry.digest || stored.size != expected_size {
            bail!("required root filesystem file /{path} changed while storing its content");
        }
        if stored.created {
            created_blobs += 1;
        }
        missing_blobs.remove(&entry.digest);
        if missing_blobs.is_empty() {
            break;
        }
    }
    if !missing_blobs.is_empty() {
        let digests: Vec<_> = missing_blobs.keys().map(String::as_str).collect();
        bail!("required root filesystem blobs were absent from the export: {digests:?}");
    }

    Ok(BlobStorageSummary {
        required_paths: required_paths.len(),
        unique_blobs,
        reused_blobs: unique_blobs - created_blobs,
        created_blobs,
    })
}

pub fn compare(base: &RootFsManifest, result: &RootFsManifest) -> Vec<RootFsChange> {
    let base: BTreeMap<_, _> = base
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let result: BTreeMap<_, _> = result
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let paths: BTreeSet<_> = base.keys().chain(result.keys()).copied().collect();
    let mut changes = Vec::new();
    for path in paths {
        let before = base.get(path).copied();
        let after = result.get(path).copied();
        let change = match (before, after) {
            (None, Some(_)) => Some(ChangeKind::Added),
            (Some(_), None) => Some(ChangeKind::Deleted),
            (Some(before), Some(after)) if before.kind != after.kind => {
                Some(ChangeKind::TypeChanged)
            }
            (Some(before), Some(after))
                if before.kind == "file" && before.digest != after.digest =>
            {
                Some(ChangeKind::Modified)
            }
            (Some(before), Some(after))
                if before.kind == "symlink" && before.link_target != after.link_target =>
            {
                Some(ChangeKind::SymlinkChanged)
            }
            (Some(before), Some(after)) if before.mode != after.mode => {
                Some(ChangeKind::ModeChanged)
            }
            _ => None,
        };
        if let Some(change) = change {
            changes.push(RootFsChange {
                path: format!("/{path}"),
                change,
                before: before.cloned(),
                after: after.cloned(),
            });
        }
    }
    changes
}

fn normalize_archive_path(path: &[u8]) -> Result<String> {
    let path = std::str::from_utf8(path).context("rootfs archive path is not valid UTF-8")?;
    let mut path = path
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string();
    while path.ends_with('/') {
        path.pop();
    }
    if path.is_empty() {
        return Ok(path);
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("unsafe path {path:?} in root filesystem export");
    }
    Ok(path)
}

fn hash_reader(reader: &mut dyn Read) -> Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let size = std::io::copy(reader, &mut hasher)?;
    Ok((format!("sha256:{}", hex_digest(&hasher.finalize())), size))
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use tar::{Builder, EntryType, Header};

    use super::*;

    #[test]
    fn complete_scan_stores_only_required_file_blobs_and_resolves_hard_links() {
        let temporary = tempfile::tempdir().unwrap();
        let export = temporary.path().join("rootfs.tar");
        let file = File::create(&export).unwrap();
        let mut builder = Builder::new(file);
        append_file(&mut builder, "workspace/kept.txt", b"already stored\n");
        append_file(
            &mut builder,
            "workspace/new.txt",
            b"new workspace content\n",
        );
        append_file(&mut builder, "usr/bin/tool", b"linked tool content\n");
        append_hard_link(&mut builder, "workspace/tool-link", "usr/bin/tool");
        append_file(&mut builder, "etc/unneeded", b"must not be stored\n");
        builder.finish().unwrap();

        let state = temporary.path().join("state");
        let store = Store::open(Some(&state)).unwrap();
        let existing = store.put_bytes(b"already stored\n").unwrap();
        assert!(existing.created);

        let manifest = scan_export(&export).unwrap();
        let required_paths = [
            "workspace/kept.txt",
            "workspace/new.txt",
            "workspace/tool-link",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let summary =
            store_required_file_blobs(&export, &manifest, &required_paths, &store).unwrap();
        assert_eq!(summary.required_paths, 3);
        assert_eq!(summary.unique_blobs, 3);
        assert_eq!(summary.reused_blobs, 1);
        assert_eq!(summary.created_blobs, 2);

        for path in &required_paths {
            let entry = manifest
                .entries
                .iter()
                .find(|entry| &entry.path == path)
                .unwrap();
            assert!(store.contains_blob(&entry.digest, entry.size).unwrap());
        }
        let hard_link = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "workspace/tool-link")
            .unwrap();
        let mut linked_content = String::new();
        store
            .open_blob(&hard_link.digest, hard_link.size)
            .unwrap()
            .read_to_string(&mut linked_content)
            .unwrap();
        assert_eq!(linked_content, "linked tool content\n");

        let unneeded = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "etc/unneeded")
            .unwrap();
        assert!(
            !store
                .contains_blob(&unneeded.digest, unneeded.size)
                .unwrap()
        );
    }

    #[test]
    fn provider_inventories_are_canonicalized_and_strictly_validated() {
        let directory = RootFsEntry {
            path: "workspace".to_owned(),
            kind: "directory".to_owned(),
            mode: 0o755,
            size: 0,
            digest: String::new(),
            link_target: String::new(),
        };
        let file = RootFsEntry {
            path: "workspace/proof.txt".to_owned(),
            kind: "file".to_owned(),
            mode: 0o644,
            size: 5,
            digest: format!("sha256:{}", "a".repeat(64)),
            link_target: String::new(),
        };
        let manifest = manifest_from_entries(vec![file.clone(), directory.clone()]).unwrap();
        assert_eq!(manifest.entries, vec![directory.clone(), file.clone()]);

        assert!(manifest_from_entries(vec![directory.clone(), directory]).is_err());
        let mut unsafe_file = file.clone();
        unsafe_file.path = "../outside".to_owned();
        assert!(manifest_from_entries(vec![unsafe_file]).is_err());
        let mut bad_digest = file;
        bad_digest.digest = "sha256:not-a-digest".to_owned();
        assert!(manifest_from_entries(vec![bad_digest]).is_err());
    }

    fn append_file(builder: &mut Builder<File>, path: &str, contents: &[u8]) {
        let mut header = Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_entry_type(EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(contents.len() as u64);
        header.set_cksum();
        builder.append(&header, contents).unwrap();
    }

    fn append_hard_link(builder: &mut Builder<File>, path: &str, target: &str) {
        let mut header = Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_entry_type(EntryType::Link);
        header.set_mode(0o644);
        header.set_size(0);
        header.set_link_name(target).unwrap();
        header.set_cksum();
        builder.append(&header, std::io::empty()).unwrap();
    }
}
