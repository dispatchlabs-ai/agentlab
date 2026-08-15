use std::io::Write;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::VERSION;
use crate::evaluation;
use crate::lifecycle;
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
        "evaluate" => evaluate_command(&arguments[1..], stdout),
        "report" => report_command(&arguments[1..], stdout),
        "list" => list_command(&arguments[1..], stdout),
        "stop" => stop_command(&arguments[1..], stdout),
        "resume" => resume_command(&arguments[1..], stdout),
        "fork" => fork_command(&arguments[1..], stdout),
        "rm" => remove_command(&arguments[1..], stdout),
        "compare" => compare_command(&arguments[1..], stdout),
        "diff" => diff_command(&arguments[1..], stdout),
        "inspect" => inspect_command(&arguments[1..], stdout),
        _ => bail!("unknown command {command:?}\n\nRun `agentlab --help` for usage."),
    }
}

fn evaluate_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    let separator = arguments
        .iter()
        .position(|argument| argument == "--")
        .ok_or_else(|| anyhow::anyhow!("evaluate requires `-- COMMAND [ARG ...]`"))?;
    let (options, command_with_separator) = arguments.split_at(separator);
    let command = &command_with_separator[1..];
    if command.is_empty() {
        bail!("evaluate requires a command after --");
    }
    let mut name = None;
    let mut json = false;
    let mut run_ids = Vec::new();
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--name" => name = Some(required_value(options, &mut index, "--name")?.to_owned()),
            "--json" => json = true,
            "--help" | "-h" => {
                writeln!(
                    stdout,
                    "usage: agentlab evaluate [--name NAME] [--json] RUN... -- COMMAND [ARG ...]"
                )?;
                return Ok(());
            }
            value if value.starts_with('-') => bail!("unexpected evaluate argument {value:?}"),
            value => run_ids.push(value.to_owned()),
        }
        index += 1;
    }
    if run_ids.is_empty() {
        bail!("evaluate requires at least one RUN");
    }
    let evaluator_name = name.unwrap_or_else(|| {
        std::path::Path::new(&command[0])
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(&command[0])
            .to_owned()
    });
    let store = Store::open(None)?;
    let mut records = Vec::new();
    for run_id in &run_ids {
        records.push(evaluation::evaluate(
            &store,
            run_id,
            &evaluator_name,
            command,
        )?);
    }
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &records)?;
        writeln!(stdout)?;
    } else {
        for record in &records {
            writeln!(stdout, "Run: {}", record.run_id)?;
            writeln!(stdout, "Evaluation: {}", record.evaluation_id)?;
            writeln!(stdout, "Evaluator: {}", record.evaluator_name)?;
            writeln!(stdout, "Status: {}", record.status)?;
            writeln!(stdout, "Exit code: {}", record.exit_code)?;
            if let Some(output) = &record.output {
                writeln!(
                    stdout,
                    "Scores: {}",
                    if output.scores.is_empty() {
                        "none".to_owned()
                    } else {
                        output.scores.keys().cloned().collect::<Vec<_>>().join(", ")
                    }
                )?;
                if let Some(summary) = &output.summary {
                    writeln!(stdout, "Summary: {summary}")?;
                }
            }
        }
    }
    if records.iter().any(|record| record.status != "succeeded") {
        bail!("one or more evaluator commands failed or emitted invalid output");
    }
    Ok(())
}

fn report_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    let mut evaluator_name = None;
    let mut factors = Vec::new();
    let mut scores = Vec::new();
    let mut run_ids = Vec::new();
    let mut json = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--evaluator" => {
                evaluator_name =
                    Some(required_value(arguments, &mut index, "--evaluator")?.to_owned())
            }
            "--factor" => {
                factors.push(required_value(arguments, &mut index, "--factor")?.to_owned())
            }
            "--score" => scores.push(required_value(arguments, &mut index, "--score")?.to_owned()),
            "--json" => json = true,
            "--help" | "-h" => {
                writeln!(
                    stdout,
                    "usage: agentlab report [--evaluator NAME] [--factor KEY]... [--score KEY]... [--json] RUN..."
                )?;
                return Ok(());
            }
            value if value.starts_with('-') => bail!("unexpected report argument {value:?}"),
            value => run_ids.push(value.to_owned()),
        }
        index += 1;
    }
    let store = Store::open(None)?;
    let table = evaluation::table(
        &store,
        &run_ids,
        evaluator_name.as_deref(),
        &factors,
        &scores,
    )?;
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &table)?;
        writeln!(stdout)?;
    } else {
        write!(stdout, "{}", evaluation::markdown_table(&table))?;
        for warning in &table.warnings {
            writeln!(stdout, "Warning: {warning}")?;
        }
    }
    Ok(())
}

fn list_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    let json = match arguments {
        [] => false,
        [argument] if argument == "--json" => true,
        [argument] if argument == "--help" || argument == "-h" => {
            writeln!(stdout, "usage: agentlab list [--json]")?;
            return Ok(());
        }
        _ => bail!("usage: agentlab list [--json]"),
    };
    let store = Store::open(None)?;
    let runs = lifecycle::list(&store)?;
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &runs)?;
        writeln!(stdout)?;
        return Ok(());
    }
    if runs.is_empty() {
        writeln!(stdout, "No retained runs.")?;
        return Ok(());
    }
    writeln!(
        stdout,
        "RUN ID                                KIND  STATE    CONTINUATIONS  CONTAINER"
    )?;
    for run in runs {
        writeln!(
            stdout,
            "{:<36}  {:<4}  {:<7}  {:<13}  {}{}",
            run.run_id,
            run.kind,
            run.container_state,
            run.continuation_count,
            run.container_name,
            if run.lifecycle_capable {
                ""
            } else {
                " (legacy)"
            }
        )?;
    }
    Ok(())
}

fn stop_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    if arguments
        .iter()
        .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
    {
        writeln!(stdout, "usage: agentlab stop [--json] RUN")?;
        return Ok(());
    }
    let (run_id, json) = lifecycle_run_argument(arguments, "stop")?;
    let store = Store::open(None)?;
    let run = lifecycle::stop(&store, run_id)?;
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &run)?;
        writeln!(stdout)?;
    } else {
        writeln!(stdout, "Run: {}", run.run_id)?;
        writeln!(stdout, "Container: {}", run.container_name)?;
        writeln!(stdout, "State: {}", run.container_state)?;
        writeln!(stdout, "Filesystem state preserved: true")?;
        writeln!(stdout, "Process memory preserved: false")?;
    }
    Ok(())
}

fn resume_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    if arguments
        .iter()
        .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
    {
        writeln!(
            stdout,
            "usage: agentlab resume [--json] RUN [-- COMMAND [ARG ...]]"
        )?;
        return Ok(());
    }
    let separator = arguments.iter().position(|argument| argument == "--");
    let (options, command) = match separator {
        Some(index) => (&arguments[..index], &arguments[index + 1..]),
        None => (arguments, &[][..]),
    };
    let (run_id, json) = lifecycle_run_argument(options, "resume")?;
    let store = Store::open(None)?;
    let result = lifecycle::resume(&store, run_id, command)?;
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &result)?;
        writeln!(stdout)?;
    } else {
        writeln!(stdout, "Run: {}", result.run_id)?;
        writeln!(stdout, "Container: {}", result.container_name)?;
        writeln!(stdout, "State: {}", result.container_state)?;
        writeln!(
            stdout,
            "Container restarted: {}",
            result.container_restarted
        )?;
        writeln!(stdout, "Filesystem state reused: true")?;
        writeln!(stdout, "Process memory restored: false")?;
        if let Some(continuation) = result.continuation {
            writeln!(stdout, "Continuation: {}", continuation.continuation_id)?;
            writeln!(stdout, "Exit code: {}", continuation.exit_code)?;
            writeln!(
                stdout,
                "Result filesystem: {}",
                continuation.result_filesystem_digest
            )?;
            writeln!(
                stdout,
                "Portable delta: {}",
                continuation.portable_delta_digest
            )?;
            writeln!(stdout, "Captures: {}", continuation.captures.len())?;
        }
    }
    Ok(())
}

fn fork_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    if arguments
        .iter()
        .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
    {
        writeln!(stdout, "usage: agentlab fork [--json] RUN")?;
        return Ok(());
    }
    let (run_id, json) = lifecycle_run_argument(arguments, "fork")?;
    let store = Store::open(None)?;
    let fork = lifecycle::fork(&store, run_id)?;
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &fork)?;
        writeln!(stdout)?;
    } else {
        writeln!(stdout, "Fork: {}", fork.run_id)?;
        writeln!(stdout, "Parent: {}", fork.parent_run_id)?;
        writeln!(stdout, "Container: {}", fork.container_name)?;
        writeln!(stdout, "State: running")?;
        writeln!(stdout, "Base filesystem: {}", fork.base_filesystem_digest)?;
        writeln!(stdout, "Filesystem state copied: true")?;
        writeln!(stdout, "Process memory copied: false")?;
    }
    Ok(())
}

fn remove_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    if arguments
        .iter()
        .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
    {
        writeln!(stdout, "usage: agentlab rm [--json] RUN")?;
        return Ok(());
    }
    let (run_id, json) = lifecycle_run_argument(arguments, "rm")?;
    let store = Store::open(None)?;
    let removed = lifecycle::remove(&store, run_id)?;
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &removed)?;
        writeln!(stdout)?;
    } else {
        writeln!(stdout, "Removed run: {}", removed.run_id)?;
        writeln!(stdout, "Removed container: {}", removed.container_name)?;
        writeln!(stdout, "Removed image tag: {}", removed.image_tag)?;
        writeln!(stdout, "Removed local run artifacts: true")?;
    }
    Ok(())
}

fn lifecycle_run_argument<'a>(arguments: &'a [String], command: &str) -> Result<(&'a str, bool)> {
    let mut run_id = None;
    let mut json = false;
    for argument in arguments {
        match argument.as_str() {
            "--json" => json = true,
            value if value.starts_with('-') => bail!("unexpected {command} argument {value:?}"),
            value if run_id.is_none() => run_id = Some(value),
            value => bail!("unexpected {command} argument {value:?}"),
        }
    }
    Ok((
        run_id.ok_or_else(|| anyhow::anyhow!("{command} requires RUN"))?,
        json,
    ))
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
        if store.run_file_exists(digest, "fork.json")? {
            let fork = lifecycle::load_fork(&store, digest)?;
            if verify {
                lifecycle::verify_all(&store, digest)?;
                evaluation::verify_all(&store, digest)?;
            }
            if json {
                serde_json::to_writer_pretty(&mut *stdout, &fork)?;
                writeln!(stdout)?;
                return Ok(());
            }
            let managed = lifecycle::inspect(&store, digest, false)?;
            writeln!(stdout, "Fork: {}", fork.run_id)?;
            writeln!(stdout, "Schema: {}", fork.schema_version)?;
            writeln!(stdout, "Record: {}", fork.digest)?;
            writeln!(stdout, "Parent: {}", fork.parent_run_id)?;
            writeln!(stdout, "Base filesystem: {}", fork.base_filesystem_digest)?;
            writeln!(
                stdout,
                "Container: {} ({})",
                managed.container_name, managed.container_state
            )?;
            writeln!(stdout, "Continuations: {}", managed.continuation_count)?;
            writeln!(
                stdout,
                "Filesystem state copied: {}",
                fork.filesystem_state_copied
            )?;
            writeln!(
                stdout,
                "Process memory copied: {}",
                fork.process_memory_copied
            )?;
            if verify {
                writeln!(stdout, "Integrity: verified")?;
            }
            return Ok(());
        }
        let result = run::load_result(&store, digest)?;
        if verify {
            lifecycle::verify_all(&store, digest)?;
            evaluation::verify_all(&store, digest)?;
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
        let managed = lifecycle::inspect(&store, digest, false)?;
        writeln!(
            stdout,
            "Retained container: {} ({})",
            managed.container_name, managed.container_state
        )?;
        writeln!(stdout, "Lifecycle capable: {}", managed.lifecycle_capable)?;
        writeln!(stdout, "Continuations: {}", managed.continuation_count)?;
        writeln!(
            stdout,
            "Evaluations: {}",
            evaluation::list(&store, digest)?.len()
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
        "AgentLab {VERSION}\n\nContent-addressed workspace snapshots and isolated agent execution.\n\nUsage:\n  agentlab --version\n  agentlab snapshot [--workspace PATH] [--json]\n  agentlab run --image IMAGE [OPTIONS] -- COMMAND [ARG ...]\n  agentlab list [--json]\n  agentlab inspect [--json] [--verify] SNAPSHOT_OR_RUN\n  agentlab diff [--raw] [--json] RUN\n  agentlab compare [--expect-factor KEY]... [--json] LEFT_RUN RIGHT_RUN\n  agentlab evaluate [--name NAME] [--json] RUN... -- COMMAND [ARG ...]\n  agentlab report [--evaluator NAME] [--factor KEY]... [--score KEY]... [--json] RUN...\n  agentlab stop [--json] RUN\n  agentlab resume [--json] RUN [-- COMMAND [ARG ...]]\n  agentlab fork [--json] RUN\n  agentlab rm [--json] RUN\n\nCommands:\n  snapshot    capture an immutable workspace snapshot\n  run         execute once in a private Docker root filesystem\n  list        list locally recorded runs and live container state\n  inspect     inspect and verify snapshot, run, fork, lifecycle, and evaluation metadata\n  diff        show normalized persistent filesystem changes\n  compare     verify controlled inputs and expected factor differences\n  evaluate    invoke an arbitrary external evaluator for one or more results\n  report      align factors and evaluator scores without interpreting them\n  stop        stop the stable retained-container process\n  resume      restart the container and optionally execute a continuation\n  fork        create a private filesystem-level fork\n  rm          delete exactly one run's container, image tag, and local artifacts\n\nFilesystem state survives stop/resume. Process trees and live memory do not. Evaluator scores are observations, not universal judgments."
    )?;
    Ok(())
}
