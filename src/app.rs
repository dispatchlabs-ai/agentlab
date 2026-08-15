use std::io::Write;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::VERSION;
use crate::snapshot::{self, Repository};
use crate::store::Store;

pub fn run(arguments: Vec<String>, stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    match execute(arguments, stdout) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "agentlab: {error:#}");
            1
        }
    }
}

fn execute(arguments: Vec<String>, stdout: &mut dyn Write) -> Result<()> {
    let Some(command) = arguments.first().map(String::as_str) else {
        print_help(stdout)?;
        return Ok(());
    };
    match command {
        "--help" | "-h" | "help" => print_help(stdout),
        "--version" | "version" => {
            writeln!(stdout, "agentlab {VERSION}")?;
            Ok(())
        }
        "snapshot" => snapshot_command(&arguments[1..], stdout),
        "inspect" => inspect_command(&arguments[1..], stdout),
        _ => bail!("unknown command {command:?}\n\nRun `agentlab --help` for usage."),
    }
}

fn snapshot_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    let mut workspace = PathBuf::from(".");
    let mut json = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--workspace" => {
                index += 1;
                workspace = PathBuf::from(
                    arguments
                        .get(index)
                        .ok_or_else(|| anyhow::anyhow!("--workspace requires PATH"))?,
                );
            }
            "--json" => json = true,
            "--help" | "-h" => {
                writeln!(
                    stdout,
                    "usage: agentlab snapshot [--workspace PATH] [--json]"
                )?;
                return Ok(());
            }
            value => bail!("unexpected snapshot argument {value:?}"),
        }
        index += 1;
    }
    let store = Store::open(None)?;
    let result = snapshot::create(&workspace, &store)?;
    if json {
        #[derive(Serialize)]
        struct Output<'a> {
            digest: &'a str,
            workspace: &'a std::path::Path,
            included_paths: usize,
            excluded_paths: usize,
            logical_bytes: u64,
            new_blobs: usize,
            reused_blobs: usize,
            repositories: &'a [Repository],
            ignore_rules_digest: &'a str,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            warnings: &'a Vec<String>,
        }
        serde_json::to_writer_pretty(
            &mut *stdout,
            &Output {
                digest: &result.manifest.digest,
                workspace: &result.workspace,
                included_paths: result.included_paths,
                excluded_paths: result.excluded_paths,
                logical_bytes: result.logical_bytes,
                new_blobs: result.new_blobs,
                reused_blobs: result.reused_blobs,
                repositories: &result.manifest.repositories,
                ignore_rules_digest: &result.manifest.ignore_rules_digest,
                warnings: &result.warnings,
            },
        )?;
        writeln!(stdout)?;
    } else {
        writeln!(stdout, "Snapshot: {}", result.manifest.digest)?;
        writeln!(stdout, "Workspace: {}", result.workspace.display())?;
        writeln!(stdout, "Included paths: {}", result.included_paths)?;
        writeln!(stdout, "Excluded paths: {}", result.excluded_paths)?;
        writeln!(
            stdout,
            "Repositories discovered: {}",
            result.manifest.repositories.len()
        )?;
        writeln!(stdout, "Logical file bytes: {}", result.logical_bytes)?;
        writeln!(
            stdout,
            "Content blobs: {} new, {} reused",
            result.new_blobs, result.reused_blobs
        )?;
        writeln!(
            stdout,
            "Workspace-ignore rules: {}",
            result.manifest.ignore_rules_digest
        )?;
    }
    Ok(())
}

fn inspect_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    let mut json = false;
    let mut verify = false;
    let mut digest = None;
    for argument in arguments {
        match argument.as_str() {
            "--json" => json = true,
            "--verify" => verify = true,
            "--help" | "-h" => {
                writeln!(
                    stdout,
                    "usage: agentlab inspect [--json] [--verify] SNAPSHOT"
                )?;
                return Ok(());
            }
            value if value.starts_with('-') => bail!("unexpected inspect argument {value:?}"),
            value if digest.is_none() => digest = Some(value),
            value => bail!("unexpected inspect argument {value:?}"),
        }
    }
    let digest = digest.ok_or_else(|| anyhow::anyhow!("inspect requires SNAPSHOT"))?;
    let store = Store::open(None)?;
    let manifest = snapshot::load(&store, digest)?;
    if verify {
        snapshot::verify(&store, &manifest)?;
    }
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &manifest)?;
        writeln!(stdout)?;
        return Ok(());
    }
    writeln!(stdout, "Snapshot: {}", manifest.digest)?;
    writeln!(stdout, "Schema: {}", manifest.schema_version)?;
    writeln!(
        stdout,
        "Workspace-ignore rules: {}",
        manifest.ignore_rules_digest
    )?;
    writeln!(stdout, "Repositories: {}", manifest.repositories.len())?;
    for repository in &manifest.repositories {
        writeln!(
            stdout,
            "  repo  {} ({} metadata at {})",
            repository.path, repository.metadata_kind, repository.metadata_path
        )?;
    }
    writeln!(stdout, "Entries: {}", manifest.entries.len())?;
    for entry in &manifest.entries {
        let detail = match entry.kind.as_str() {
            "file" => format!(" size={} digest={}", entry.size, entry.digest),
            "symlink" => format!(" target={:?}", entry.link_target),
            _ => String::new(),
        };
        writeln!(
            stdout,
            "  {:<9} {:04o} {}{}",
            entry.kind, entry.mode, entry.path, detail
        )?;
    }
    if verify {
        writeln!(stdout, "Integrity: verified")?;
    }
    Ok(())
}

fn print_help(output: &mut dyn Write) -> Result<()> {
    writeln!(
        output,
        "AgentLab {VERSION}\n\nContent-addressed workspace snapshots and isolated agent execution.\n\nUsage:\n  agentlab --version\n  agentlab snapshot [--workspace PATH] [--json]\n  agentlab inspect [--json] [--verify] SNAPSHOT\n\nMilestone 1 commands:\n  snapshot    capture an immutable workspace snapshot\n  inspect     inspect snapshot metadata without printing file contents\n\nRun lifecycle commands are introduced in later milestones."
    )?;
    Ok(())
}
