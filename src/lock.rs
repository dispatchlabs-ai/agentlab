use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// A crash-safe advisory lock. The lock file is durable for diagnostics, but
/// the kernel releases the actual lock whenever the process exits.
pub(crate) struct AdvisoryLock {
    _file: File,
}

impl AdvisoryLock {
    pub(crate) fn acquire(path: &Path, description: &str) -> Result<Self> {
        Self::try_acquire(path, description)?
            .with_context(|| format!("another {description} operation is already in progress"))
    }

    pub(crate) fn try_acquire(path: &Path, description: &str) -> Result<Option<Self>> {
        let parent = path.parent().context("operation lock has no parent")?;
        fs::create_dir_all(parent).context("create operation lock directory")?;
        secure_directory(parent)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open {description} lock {}", path.display()))?;
        secure_file(&file)?;

        #[cfg(unix)]
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(error) if error == rustix::io::Errno::WOULDBLOCK => {
                return Ok(None);
            }
            Err(error) => return Err(error).with_context(|| format!("lock {description}")),
        }

        #[cfg(not(unix))]
        {
            // AgentLab's safe apply implementation currently requires Unix.
            // Keeping the file open still serializes callers within a single
            // process on other platforms; cross-process locking is added with
            // the first non-Unix safe-apply backend.
        }

        file.set_len(0)?;
        writeln!(
            file,
            "pid={}\nstarted_at={}",
            std::process::id(),
            chrono::Utc::now()
        )?;
        file.sync_all()?;
        Ok(Some(Self { _file: file }))
    }
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn secure_file(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_file(_file: &File) -> Result<()> {
    Ok(())
}
