use std::fs::{self, File};
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{Manifest, safe_join, verify};
use crate::store::{Store, create_new_file};

pub fn materialize(store: &Store, manifest: &Manifest, destination: &Path) -> Result<()> {
    verify(store, manifest)?;
    fs::create_dir_all(destination).context("create materialization destination")?;
    if fs::read_dir(destination)
        .context("inspect materialization destination")?
        .next()
        .is_some()
    {
        bail!(
            "materialization destination {} is not empty",
            destination.display()
        );
    }

    for entry in &manifest.entries {
        if entry.kind != "directory" {
            continue;
        }
        let target = safe_join(destination, &entry.path)?;
        fs::create_dir_all(&target)
            .with_context(|| format!("create directory {:?}", entry.path))?;
    }
    for entry in &manifest.entries {
        let target = safe_join(destination, &entry.path)?;
        match entry.kind.as_str() {
            "directory" => {}
            "symlink" => create_symlink(&entry.link_target, &target)
                .with_context(|| format!("create symlink {:?}", entry.path))?,
            "file" => {
                let mut blob = store.open_blob(&entry.digest)?;
                let mut output: File = create_new_file(&target)?;
                io::copy(&mut blob, &mut output)
                    .with_context(|| format!("materialize file {:?}", entry.path))?;
                output.sync_all()?;
                set_mode(&target, entry.mode)?;
            }
            _ => unreachable!("manifest was verified"),
        }
    }
    let mut directories: Vec<_> = manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == "directory")
        .collect();
    directories.sort_by_key(|entry| std::cmp::Reverse(entry.path.matches('/').count()));
    for entry in directories {
        set_mode(&safe_join(destination, &entry.path)?, entry.mode)?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &str, path: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(windows)]
fn create_symlink(target: &str, path: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, path)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}
