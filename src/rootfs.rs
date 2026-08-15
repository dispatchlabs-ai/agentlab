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

pub fn scan_export(export: &Path, store_files: Option<&Store>) -> Result<RootFsManifest> {
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
            let (digest, size) = if let Some(store) = store_files {
                let stored = store
                    .put_reader(&mut item)
                    .with_context(|| format!("store exported rootfs file {path:?}"))?;
                (stored.digest, stored.size)
            } else {
                hash_reader(&mut item)
                    .with_context(|| format!("hash exported rootfs file {path:?}"))?
            };
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

    let entries: Vec<_> = entries.into_values().collect();
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
