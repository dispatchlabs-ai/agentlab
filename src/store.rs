use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

const STATE_DIRECTORY_ENVIRONMENT: &str = "AGENTLAB_STATE_DIR";

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutResult {
    pub digest: String,
    pub size: u64,
    pub created: bool,
}

impl Store {
    pub fn open(root: Option<&Path>) -> Result<Self> {
        let root = match root {
            Some(path) => path.to_path_buf(),
            None => match std::env::var_os(STATE_DIRECTORY_ENVIRONMENT) {
                Some(path) => PathBuf::from(path),
                None => dirs::home_dir()
                    .context("locate user home for AgentLab state")?
                    .join(".agentlab"),
            },
        };
        let root = absolute_path(&root).context("resolve AgentLab state directory")?;
        for directory in [
            root.clone(),
            root.join("blobs").join("sha256"),
            root.join("snapshots").join("sha256"),
        ] {
            fs::create_dir_all(&directory).with_context(|| {
                format!(
                    "create private AgentLab state directory {}",
                    directory.display()
                )
            })?;
            secure_directory(&directory)?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn put_file(&self, path: &Path) -> Result<PutResult> {
        let mut source =
            File::open(path).with_context(|| format!("open source file {}", path.display()))?;
        let incoming_directory = self.root.join("blobs").join("sha256");
        let mut temporary =
            NamedTempFile::new_in(&incoming_directory).context("create temporary content blob")?;
        secure_file(temporary.as_file())?;

        let mut hasher = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = [0_u8; 128 * 1024];
        loop {
            let read = source
                .read(&mut buffer)
                .with_context(|| format!("read source file {}", path.display()))?;
            if read == 0 {
                break;
            }
            temporary
                .write_all(&buffer[..read])
                .context("write content blob")?;
            hasher.update(&buffer[..read]);
            size += read as u64;
        }
        temporary
            .as_file()
            .sync_all()
            .context("sync content blob")?;
        let digest = format!("sha256:{}", hex_digest(hasher.finalize().as_slice()));
        let hex = normalize_digest(&digest)?;
        let destination = self.blob_path(&hex);

        match fs::metadata(&destination) {
            Ok(metadata) => {
                if metadata.len() != size {
                    bail!(
                        "content store collision for {digest}: stored size {}, incoming size {size}",
                        metadata.len()
                    );
                }
                return Ok(PutResult {
                    digest,
                    size,
                    created: false,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect existing content blob"),
        }

        let parent = destination
            .parent()
            .context("content blob destination has no parent")?;
        fs::create_dir_all(parent).context("create content blob shard")?;
        secure_directory(parent)?;
        match temporary.persist_noclobber(&destination) {
            Ok(_) => Ok(PutResult {
                digest,
                size,
                created: true,
            }),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = fs::metadata(&destination).context("inspect concurrent blob")?;
                if metadata.len() != size {
                    bail!("content store collision for {digest}");
                }
                Ok(PutResult {
                    digest,
                    size,
                    created: false,
                })
            }
            Err(error) => Err(error.error).context("persist content blob"),
        }
    }

    pub fn open_blob(&self, digest: &str) -> Result<File> {
        let hex = normalize_digest(digest)?;
        File::open(self.blob_path(&hex)).with_context(|| format!("open content blob {digest}"))
    }

    pub fn write_snapshot(&self, digest: &str, data: &[u8]) -> Result<()> {
        let hex = normalize_digest(digest)?;
        let destination = self
            .root
            .join("snapshots")
            .join("sha256")
            .join(format!("{hex}.json"));
        match fs::read(&destination) {
            Ok(existing) => {
                if existing == data || semantically_equal_json(&existing, data) {
                    return Ok(());
                }
                bail!("snapshot manifest collision for sha256:{hex}");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("read existing snapshot manifest"),
        }

        let parent = destination
            .parent()
            .context("manifest path has no parent")?;
        let mut temporary = NamedTempFile::new_in(parent).context("create manifest file")?;
        secure_file(temporary.as_file())?;
        temporary
            .write_all(data)
            .context("write snapshot manifest")?;
        temporary
            .as_file()
            .sync_all()
            .context("sync snapshot manifest")?;
        match temporary.persist_noclobber(&destination) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = fs::read(&destination).context("read concurrent manifest")?;
                if existing == data || semantically_equal_json(&existing, data) {
                    Ok(())
                } else {
                    bail!("snapshot manifest collision for sha256:{hex}")
                }
            }
            Err(error) => Err(error.error).context("persist snapshot manifest"),
        }
    }

    pub fn read_snapshot(&self, digest: &str) -> Result<Vec<u8>> {
        let hex = normalize_digest(digest)?;
        let path = self
            .root
            .join("snapshots")
            .join("sha256")
            .join(format!("{hex}.json"));
        fs::read(&path).with_context(|| format!("snapshot sha256:{hex} not found"))
    }

    fn blob_path(&self, hex: &str) -> PathBuf {
        self.root
            .join("blobs")
            .join("sha256")
            .join(&hex[..2])
            .join(&hex[2..])
    }
}

pub fn normalize_digest(digest: &str) -> Result<String> {
    let value = digest
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(digest.trim());
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid SHA-256 digest {digest:?}");
    }
    Ok(value.to_ascii_lowercase())
}

pub fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn semantically_equal_json(left: &[u8], right: &[u8]) -> bool {
    let left: serde_json::Result<serde_json::Value> = serde_json::from_slice(left);
    let right: serde_json::Result<serde_json::Value> = serde_json::from_slice(right);
    matches!((left, right), (Ok(left), Ok(right)) if left == right)
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("secure AgentLab state directory {}", path.display()))
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn secure_file(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .context("secure AgentLab state file")
}

#[cfg(not(unix))]
fn secure_file(_file: &File) -> Result<()> {
    Ok(())
}

pub fn create_new_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))
}
