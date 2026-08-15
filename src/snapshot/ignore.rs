use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::{Candidate, CandidateKind, IgnoreRule, Repository};
use crate::store::hex_digest;

pub(crate) struct RepositoryDiscovery {
    pub repositories: Vec<Repository>,
    pub metadata_paths: HashSet<String>,
    pub tracked_paths: HashSet<String>,
    pub unknown_repository_roots: HashSet<String>,
    pub warnings: Vec<String>,
}

pub(crate) fn ignored_paths(
    root: &Path,
    candidates: &[Candidate],
    metadata_paths: &HashSet<String>,
) -> Result<HashSet<String>> {
    let has_ignore_file = candidates.iter().any(|candidate| {
        candidate.kind == CandidateKind::File && basename(&candidate.path) == ".gitignore"
    });
    if !has_ignore_file {
        return Ok(HashSet::new());
    }
    let git = find_git()?;
    let temporary = tempfile::tempdir().context("create temporary Git metadata")?;
    let git_directory = temporary.path().join("ignore.git");
    let output = isolated_git(&git)
        .arg("init")
        .arg("--bare")
        .arg("--quiet")
        .arg(&git_directory)
        .output()
        .context("initialize temporary Git metadata")?;
    if !output.status.success() {
        bail!(
            "initialize temporary Git metadata: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let mut command = isolated_git(&git);
    command
        .arg(format!("--git-dir={}", git_directory.display()))
        .arg(format!("--work-tree={}", root.display()))
        .args(["-c", "core.excludesFile=/dev/null"])
        .args(["check-ignore", "--no-index", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("run Git ignore evaluation")?;
    {
        let input = child.stdin.as_mut().context("open Git ignore input")?;
        for candidate in candidates {
            if metadata_paths.contains(&candidate.path) {
                continue;
            }
            input.write_all(candidate.path.as_bytes())?;
            input.write_all(&[0])?;
        }
    }
    let output = child
        .wait_with_output()
        .context("finish Git ignore evaluation")?;
    match output.status.code() {
        Some(0 | 1) => {}
        _ => bail!(
            "evaluate workspace ignore rules: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
    let mut ignored = HashSet::new();
    for path in output.stdout.split(|byte| *byte == 0) {
        if path.is_empty() {
            continue;
        }
        ignored.insert(
            std::str::from_utf8(path)
                .context("Git returned a non-UTF-8 ignored path")?
                .replace(std::path::MAIN_SEPARATOR, "/"),
        );
    }
    Ok(ignored)
}

pub(crate) fn discover_repositories(root: &Path, candidates: &[Candidate]) -> RepositoryDiscovery {
    let mut repositories = Vec::new();
    let mut metadata_paths: HashSet<String> = HashSet::new();
    for candidate in candidates {
        if basename(&candidate.path) != ".git"
            || !matches!(
                candidate.kind,
                CandidateKind::File | CandidateKind::Directory
            )
            || metadata_paths
                .iter()
                .any(|known| is_path_at_or_beneath(&candidate.path, known))
        {
            continue;
        }
        let repository_path = candidate
            .path
            .rsplit_once('/')
            .map_or(".", |(parent, _)| parent);
        repositories.push(Repository {
            path: repository_path.to_string(),
            metadata_path: candidate.path.clone(),
            metadata_kind: if candidate.kind == CandidateKind::Directory {
                "directory".to_string()
            } else {
                "file".to_string()
            },
        });
        metadata_paths.insert(candidate.path.clone());
        if candidate.kind == CandidateKind::Directory {
            for nested in candidates {
                if is_path_at_or_beneath(&nested.path, &candidate.path) {
                    metadata_paths.insert(nested.path.clone());
                }
            }
        }
    }
    repositories.sort_by(|left, right| left.path.cmp(&right.path));

    let mut tracked_paths = HashSet::new();
    let mut unknown_repository_roots = HashSet::new();
    let mut warnings = Vec::new();
    let git = find_git();
    for repository in &repositories {
        let repository_root = if repository.path == "." {
            root.to_path_buf()
        } else {
            root.join(repository.path.split('/').collect::<std::path::PathBuf>())
        };
        let output = match &git {
            Ok(git) => isolated_git(git)
                .arg("-C")
                .arg(&repository_root)
                .args(["-c", "core.excludesFile=/dev/null"])
                .args(["ls-files", "--cached", "-z", "--"])
                .output(),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Git not found",
            )),
        };
        let output = match output {
            Ok(output) if output.status.success() => output,
            _ => {
                unknown_repository_roots.insert(repository.path.clone());
                warnings.push(format!(
                    "repository {} has unavailable or broken Git metadata; all paths beneath it were conservatively included",
                    repository.path
                ));
                continue;
            }
        };
        for path in output.stdout.split(|byte| *byte == 0) {
            if path.is_empty() {
                continue;
            }
            let Ok(path) = std::str::from_utf8(path) else {
                unknown_repository_roots.insert(repository.path.clone());
                warnings.push(format!(
                    "repository {} has non-UTF-8 tracked paths; all paths beneath it were conservatively included",
                    repository.path
                ));
                break;
            };
            let path = path.replace(std::path::MAIN_SEPARATOR, "/");
            tracked_paths.insert(if repository.path == "." {
                path
            } else {
                format!("{}/{path}", repository.path)
            });
        }
    }
    RepositoryDiscovery {
        repositories,
        metadata_paths,
        tracked_paths,
        unknown_repository_roots,
        warnings,
    }
}

pub(crate) fn active_ignore_rules(
    candidates: &[Candidate],
    ignored: &HashSet<String>,
) -> Result<(Vec<IgnoreRule>, String)> {
    let mut rules = Vec::new();
    for candidate in candidates {
        if basename(&candidate.path) != ".gitignore" || candidate.kind != CandidateKind::File {
            continue;
        }
        let parent = candidate.path.rsplit_once('/').map(|(value, _)| value);
        if parent.is_some_and(|parent| ignored.contains(parent)) {
            continue;
        }
        let content = fs::read(&candidate.absolute)
            .with_context(|| format!("read active ignore file {:?}", candidate.path))?;
        rules.push(IgnoreRule {
            path: candidate.path.clone(),
            digest: format!("sha256:{}", hex_digest(&Sha256::digest(content))),
        });
    }
    rules.sort_by(|left, right| left.path.cmp(&right.path));
    let digest = ignore_rules_digest(&rules);
    Ok((rules, digest))
}

pub(crate) fn ignore_rules_digest(rules: &[IgnoreRule]) -> String {
    let mut hasher = Sha256::new();
    for rule in rules {
        hasher.update(rule.path.as_bytes());
        hasher.update([0]);
        hasher.update(rule.digest.as_bytes());
        hasher.update([0]);
    }
    format!("sha256:{}", hex_digest(&hasher.finalize()))
}

fn find_git() -> Result<std::path::PathBuf> {
    let output = Command::new("git").arg("--exec-path").output();
    match output {
        Ok(output) if output.status.success() => Ok(std::path::PathBuf::from("git")),
        _ => bail!("Git is required to evaluate .gitignore files but was not found in PATH"),
    }
}

fn isolated_git(program: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0");
    command
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_path_at_or_beneath(path: &str, parent: &str) -> bool {
    path == parent
        || path
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('/'))
}
