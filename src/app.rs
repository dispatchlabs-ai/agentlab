use std::io::Write;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::VERSION;
use crate::run::{self, CaptureSpec, RunOptions};
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
        "run" => run_command(&arguments[1..], stdout),
        "compare" => compare_command(&arguments[1..], stdout),
        "diff" => diff_command(&arguments[1..], stdout),
        "inspect" => inspect_command(&arguments[1..], stdout),
        _ => bail!("unknown command {command:?}\n\nRun `agentlab --help` for usage."),
    }
}

fn required_value<'a>(arguments: &'a [String], index: &mut usize, flag: &str) -> Result<&'a str> {
    *index += 1;
    arguments
        .get(*index)
        .map(String::as_str)
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))
}

fn run_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    let separator = arguments
        .iter()
        .position(|argument| argument == "--")
        .ok_or_else(|| anyhow::anyhow!("run requires `-- COMMAND [ARG ...]`"))?;
    let (options, command_with_separator) = arguments.split_at(separator);
    let command = &command_with_separator[1..];
    if command.is_empty() {
        bail!("run requires a command after `--`");
    }

    let mut parsed = RunOptions {
        workspace: PathBuf::from("."),
        image: String::new(),
        command: command.to_vec(),
        factors: std::collections::BTreeMap::new(),
        workspace_guest_path: "/workspace".to_owned(),
        network: "none".to_owned(),
        memory: None,
        cpus: None,
        change_ignore: None,
        captures: Vec::new(),
    };
    let mut json = false;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--workspace" => {
                parsed.workspace =
                    PathBuf::from(required_value(options, &mut index, "--workspace")?)
            }
            "--image" => parsed.image = required_value(options, &mut index, "--image")?.to_owned(),
            "--workspace-path" => {
                parsed.workspace_guest_path =
                    required_value(options, &mut index, "--workspace-path")?.to_owned()
            }
            "--network" => {
                parsed.network = required_value(options, &mut index, "--network")?.to_owned()
            }
            "--memory" => {
                parsed.memory = Some(required_value(options, &mut index, "--memory")?.to_owned())
            }
            "--cpus" => {
                parsed.cpus = Some(required_value(options, &mut index, "--cpus")?.to_owned())
            }
            "--factor" => {
                let value = required_value(options, &mut index, "--factor")?;
                let (key, value) = value
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--factor requires KEY=VALUE"))?;
                if key.is_empty() {
                    bail!("--factor key cannot be empty");
                }
                if parsed
                    .factors
                    .insert(key.to_owned(), value.to_owned())
                    .is_some()
                {
                    bail!("duplicate --factor key {key:?}");
                }
            }
            "--change-ignore" => {
                parsed.change_ignore = Some(PathBuf::from(required_value(
                    options,
                    &mut index,
                    "--change-ignore",
                )?))
            }
            "--capture" => {
                let value = required_value(options, &mut index, "--capture")?;
                let (guest_path, name) = value
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--capture requires GUEST_PATH=NAME"))?;
                parsed.captures.push(CaptureSpec {
                    guest_path: guest_path.to_owned(),
                    name: name.to_owned(),
                });
            }
            "--json" => json = true,
            "--help" | "-h" => {
                writeln!(
                    stdout,
                    "usage: agentlab run --image IMAGE [--workspace PATH] [--factor KEY=VALUE] [--workspace-path PATH] [--network MODE] [--memory LIMIT] [--cpus COUNT] [--change-ignore GLOB] [--capture GUEST_PATH=NAME] [--json] -- COMMAND [ARG ...]"
                )?;
                return Ok(());
            }
            value => bail!("unexpected run argument {value:?}"),
        }
        index += 1;
    }
    if parsed.image.is_empty() {
        bail!("run requires --image IMAGE");
    }

    let store = Store::open(None)?;
    let result = run::execute(&parsed, &store)?;
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &result)?;
        writeln!(stdout)?;
    } else {
        writeln!(stdout, "Run: {}", result.run_id)?;
        writeln!(stdout, "Exit code: {}", result.exit_code)?;
        writeln!(stdout, "Snapshot: {}", result.workspace_snapshot_digest)?;
        writeln!(stdout, "Portable changes: {}", result.changes)?;
        writeln!(stdout, "Ignored changes: {}", result.ignored_changes)?;
        writeln!(
            stdout,
            "Retained container: {}",
            result.retained_container_name
        )?;
        writeln!(stdout, "Inspect: agentlab inspect {}", result.run_id)?;
        writeln!(stdout, "Diff: agentlab diff {}", result.run_id)?;
    }
    Ok(())
}

fn compare_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    let mut json = false;
    let mut run_ids = Vec::new();
    let mut expected_factors = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--json" => json = true,
            "--expect-factor" => expected_factors
                .push(required_value(arguments, &mut index, "--expect-factor")?.to_owned()),
            "--help" | "-h" => {
                writeln!(
                    stdout,
                    "usage: agentlab compare [--expect-factor KEY]... [--json] LEFT_RUN RIGHT_RUN"
                )?;
                return Ok(());
            }
            value if value.starts_with('-') => bail!("unexpected compare argument {value:?}"),
            value => run_ids.push(value),
        }
        index += 1;
    }
    if run_ids.len() != 2 {
        bail!("compare requires LEFT_RUN and RIGHT_RUN");
    }
    if expected_factors.iter().any(String::is_empty) {
        bail!("--expect-factor key cannot be empty");
    }
    let store = Store::open(None)?;
    let comparison = run::compare_runs(&store, run_ids[0], run_ids[1], &expected_factors)?;
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &comparison)?;
        writeln!(stdout)?;
        return Ok(());
    }
    writeln!(
        stdout,
        "Runs: {} <> {}",
        comparison.left_run_id, comparison.right_run_id
    )?;
    writeln!(
        stdout,
        "Same workspace snapshot: {}",
        comparison.same_workspace_snapshot
    )?;
    writeln!(
        stdout,
        "Same resolved image: {}",
        comparison.same_resolved_image
    )?;
    writeln!(
        stdout,
        "Same portable base: {}",
        comparison.same_portable_base
    )?;
    writeln!(
        stdout,
        "Distinct private containers: {}",
        comparison.distinct_private_containers
    )?;
    writeln!(
        stdout,
        "Controlled-input differences: {}",
        display_names(&comparison.controlled_input_differences)
    )?;
    writeln!(
        stdout,
        "Factor differences: {}",
        display_names(
            &comparison
                .factor_differences
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        )
    )?;
    writeln!(
        stdout,
        "Missing expected factors: {}",
        display_names(&comparison.missing_expected_factor_differences)
    )?;
    writeln!(
        stdout,
        "Unexpected factor differences: {}",
        display_names(&comparison.unexpected_factor_differences)
    )?;
    writeln!(
        stdout,
        "Only expected factors differ: {}",
        comparison.only_expected_factors_differ
    )?;
    writeln!(
        stdout,
        "Comparable repetition: {}",
        comparison.comparable_repetition
    )?;
    writeln!(
        stdout,
        "Portable outcomes equal: {}",
        comparison.portable_outcomes_equal
    )?;
    Ok(())
}

fn display_names(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
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
                    "usage: agentlab inspect [--json] [--verify] SNAPSHOT_OR_RUN"
                )?;
                return Ok(());
            }
            value if value.starts_with('-') => bail!("unexpected inspect argument {value:?}"),
            value if digest.is_none() => digest = Some(value),
            value => bail!("unexpected inspect argument {value:?}"),
        }
    }
    let digest = digest.ok_or_else(|| anyhow::anyhow!("inspect requires SNAPSHOT_OR_RUN"))?;
    let store = Store::open(None)?;
    if !digest.starts_with("sha256:") {
        let result = run::load_result(&store, digest)?;
        if verify {
            run::verify_result(&store, &result)?;
        }
        if json {
            serde_json::to_writer_pretty(&mut *stdout, &result)?;
            writeln!(stdout)?;
            return Ok(());
        }
        writeln!(stdout, "Run: {}", result.run_id)?;
        writeln!(stdout, "Schema: {}", result.schema_version)?;
        writeln!(stdout, "Result: {}", result.digest)?;
        writeln!(stdout, "Exit code: {}", result.exit_code)?;
        writeln!(stdout, "Started: {}", result.started_at)?;
        writeln!(stdout, "Completed: {}", result.completed_at)?;
        writeln!(stdout, "Base filesystem: {}", result.base_filesystem_digest)?;
        writeln!(
            stdout,
            "Result filesystem: {}",
            result.result_filesystem_digest
        )?;
        writeln!(stdout, "Portable delta: {}", result.portable_delta_digest)?;
        writeln!(
            stdout,
            "Retained container: {} ({})",
            result.docker.retained_container_name, result.docker.retained_container_state
        )?;
        for warning in &result.warnings {
            writeln!(stdout, "Warning: {warning}")?;
        }
        if verify {
            writeln!(stdout, "Integrity: verified")?;
        }
        return Ok(());
    }
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

fn diff_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    let mut json = false;
    let mut raw = false;
    let mut run_id = None;
    for argument in arguments {
        match argument.as_str() {
            "--json" => json = true,
            "--raw" => raw = true,
            "--help" | "-h" => {
                writeln!(stdout, "usage: agentlab diff [--raw] [--json] RUN")?;
                return Ok(());
            }
            value if value.starts_with('-') => bail!("unexpected diff argument {value:?}"),
            value if run_id.is_none() => run_id = Some(value),
            value => bail!("unexpected diff argument {value:?}"),
        }
    }
    let run_id = run_id.ok_or_else(|| anyhow::anyhow!("diff requires RUN"))?;
    let store = Store::open(None)?;
    let delta = run::load_delta(&store, run_id, raw)?;
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &delta)?;
        writeln!(stdout)?;
        return Ok(());
    }
    writeln!(stdout, "Delta: {}", delta.digest)?;
    writeln!(stdout, "Base: {}", delta.base_filesystem_digest)?;
    writeln!(stdout, "Result: {}", delta.result_filesystem_digest)?;
    writeln!(stdout, "Changes: {}", delta.changes.len())?;
    for change in &delta.changes {
        writeln!(stdout, "  {:?} {}", change.change, change.path)?;
    }
    if !raw {
        writeln!(stdout, "Ignored changes: {}", delta.ignored_changes.len())?;
        for change in &delta.ignored_changes {
            writeln!(stdout, "  {:?} {}", change.change, change.path)?;
        }
    }
    Ok(())
}

fn print_help(output: &mut dyn Write) -> Result<()> {
    writeln!(
        output,
        "AgentLab {VERSION}\n\nContent-addressed workspace snapshots and isolated agent execution.\n\nUsage:\n  agentlab --version\n  agentlab snapshot [--workspace PATH] [--json]\n  agentlab run --image IMAGE [OPTIONS] -- COMMAND [ARG ...]\n  agentlab compare [--expect-factor KEY]... [--json] LEFT_RUN RIGHT_RUN\n  agentlab inspect [--json] [--verify] SNAPSHOT_OR_RUN\n  agentlab diff [--raw] [--json] RUN\n\nCommands:\n  snapshot    capture an immutable workspace snapshot\n  run         execute once in a private Docker root filesystem\n  compare     verify controlled inputs and expected factor differences\n  inspect     inspect and verify snapshot or run metadata\n  diff        show normalized persistent filesystem changes\n\nRuns retain their stopped container for direct inspection."
    )?;
    Ok(())
}
