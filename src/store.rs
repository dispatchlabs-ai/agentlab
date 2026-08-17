use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
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
            root.join("acceptances"),
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
        self.put_reader(&mut source)
            .with_context(|| format!("capture source file {}", path.display()))
    }

    pub fn put_bytes(&self, bytes: &[u8]) -> Result<PutResult> {
        self.put_reader(&mut Cursor::new(bytes))
    }

    pub fn put_reader(&self, source: &mut dyn Read) -> Result<PutResult> {
        let incoming_directory = self.root.join("blobs").join("sha256");
        let mut temporary =
            NamedTempFile::new_in(&incoming_directory).context("create temporary content blob")?;
        secure_file(temporary.as_file())?;

        let mut hasher = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = [0_u8; 128 * 1024];
        loop {
            let read = source.read(&mut buffer).context("read content source")?;
            if read == 0 {
                break;
            }
            temporary
                .write_all(&buffer[..read])
                .context("write content blob")?;
            hasher.update(&buffer[..read]);
            size += read as u64;
        }
        let digest = format!("sha256:{}", hex_digest(hasher.finalize().as_slice()));
        let hex = normalize_digest(&digest)?;
        let destination = self.blob_path(&hex);

        if self.contains_blob(&digest, size)? {
            return Ok(PutResult {
                digest,
                size,
                created: false,
            });
        }

        temporary
            .as_file()
            .sync_all()
            .context("sync new content blob")?;
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

    pub fn contains_blob(&self, digest: &str, expected_size: u64) -> Result<bool> {
        let hex = normalize_digest(digest)?;
        match fs::metadata(self.blob_path(&hex)) {
            Ok(metadata) if metadata.len() == expected_size => Ok(true),
            Ok(metadata) => bail!(
                "content store collision for {digest}: stored size {}, expected {expected_size}",
                metadata.len()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).context("inspect existing content blob"),
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

    pub fn create_run_directory(&self, run_id: &str) -> Result<PathBuf> {
        validate_run_id(run_id)?;
        let runs = self.root.join("runs");
        fs::create_dir_all(&runs).context("create AgentLab runs directory")?;
        secure_directory(&runs)?;
        let directory = runs.join(run_id);
        fs::create_dir(&directory).with_context(|| format!("create run directory {run_id}"))?;
        secure_directory(&directory)?;
        for child in [
            "artifacts",
            "evidence",
            "continuations",
            "evaluations",
            "reviews",
            "lifecycle",
        ] {
            let path = directory.join(child);
            fs::create_dir(&path)?;
            secure_directory(&path)?;
        }
        Ok(directory)
    }

    pub fn run_directory(&self, run_id: &str) -> Result<PathBuf> {
        validate_run_id(run_id)?;
        let directory = self.root.join("runs").join(run_id);
        if !directory.is_dir() {
            bail!("run {run_id:?} not found");
        }
        Ok(directory)
    }

    pub fn write_run_file(&self, run_id: &str, relative: &str, data: &[u8]) -> Result<PathBuf> {
        let directory = self.run_directory(run_id)?;
        let destination = safe_run_path(&directory, relative)?;
        let parent = destination.parent().context("run file has no parent")?;
        fs::create_dir_all(parent)?;
        secure_directory(parent)?;
        let mut temporary = NamedTempFile::new_in(parent).context("create temporary run file")?;
        secure_file(temporary.as_file())?;
        temporary.write_all(data)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&destination)
            .map_err(|error| error.error)
            .with_context(|| format!("persist run artifact {relative:?}"))?;
        Ok(destination)
    }

    pub fn read_run_file(&self, run_id: &str, relative: &str) -> Result<Vec<u8>> {
        let directory = self.run_directory(run_id)?;
        let path = safe_run_path(&directory, relative)?;
        fs::read(&path).with_context(|| format!("read run artifact {relative:?}"))
    }

    pub fn run_file_exists(&self, run_id: &str, relative: &str) -> Result<bool> {
        let directory = self.run_directory(run_id)?;
        Ok(safe_run_path(&directory, relative)?.is_file())
    }

    pub(crate) fn run_path(&self, run_id: &str, relative: &str) -> Result<PathBuf> {
        let directory = self.run_directory(run_id)?;
        safe_run_path(&directory, relative)
    }

    pub fn list_run_ids(&self) -> Result<Vec<String>> {
        let runs = self.root.join("runs");
        let mut run_ids = Vec::new();
        let entries = match fs::read_dir(&runs) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(run_ids),
            Err(error) => return Err(error).context("list AgentLab runs"),
        };
        for entry in entries {
            let entry = entry.context("read AgentLab runs directory")?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let run_id = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("run directory name is not valid UTF-8"))?;
            validate_run_id(&run_id)?;
            run_ids.push(run_id);
        }
        run_ids.sort();
        Ok(run_ids)
    }

    pub fn write_acceptance(&self, acceptance_id: &str, data: &[u8]) -> Result<PathBuf> {
        validate_record_id(acceptance_id, "acceptance")?;
        let directory = self.root.join("acceptances");
        fs::create_dir_all(&directory).context("create AgentLab acceptances directory")?;
        secure_directory(&directory)?;
        let destination = directory.join(format!("{acceptance_id}.json"));
        let mut temporary =
            NamedTempFile::new_in(&directory).context("create temporary acceptance record")?;
        secure_file(temporary.as_file())?;
        temporary
            .write_all(data)
            .context("write acceptance record")?;
        temporary
            .as_file()
            .sync_all()
            .context("sync acceptance record")?;
        temporary
            .persist_noclobber(&destination)
            .map_err(|error| error.error)
            .with_context(|| format!("persist acceptance {acceptance_id:?}"))?;
        Ok(destination)
    }

    pub fn read_acceptance(&self, acceptance_id: &str) -> Result<Vec<u8>> {
        validate_record_id(acceptance_id, "acceptance")?;
        let path = self
            .root
            .join("acceptances")
            .join(format!("{acceptance_id}.json"));
        fs::read(&path).with_context(|| format!("acceptance {acceptance_id:?} not found"))
    }

    pub fn list_acceptance_ids(&self) -> Result<Vec<String>> {
        let directory = self.root.join("acceptances");
        let mut ids = Vec::new();
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
            Err(error) => return Err(error).context("list AgentLab acceptances"),
        };
        for entry in entries {
            let entry = entry.context("read AgentLab acceptances directory")?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("acceptance filename is not valid UTF-8"))?;
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            validate_record_id(id, "acceptance")?;
            ids.push(id.to_owned());
        }
        ids.sort();
        Ok(ids)
    }

    pub fn remove_run_directory(&self, run_id: &str) -> Result<()> {
        let directory = self.run_directory(run_id)?;
        fs::remove_dir_all(&directory).with_context(|| format!("remove run directory {run_id}"))
    }

    fn blob_path(&self, hex: &str) -> PathBuf {
        self.root
            .join("blobs")
            .join("sha256")
            .join(&hex[..2])
            .join(&hex[2..])
    }
}

fn validate_run_id(run_id: &str) -> Result<()> {
    if run_id.is_empty()
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!("invalid run ID {run_id:?}");
    }
    Ok(())
}

fn validate_record_id(id: &str, kind: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!("invalid {kind} ID {id:?}");
    }
    Ok(())
}

fn safe_run_path(root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.is_empty()
        || relative.starts_with('/')
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("unsafe run artifact path {relative:?}");
    }
    let mut path = root.to_path_buf();
    for part in relative.split('/') {
        path.push(part);
    }
    Ok(path)
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
