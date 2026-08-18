use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::acceptance::{self, AcceptOptions};
use crate::apply::{self, ApplyOptions};
use crate::build_version;
use crate::config::AgentLabConfig;
use crate::diff;
use crate::evaluation;
use crate::lifecycle;
use crate::review::{self, ReviewOptions};
use crate::run::{self, CaptureSpec, RunOptions, SecretFileSpec, WorkspaceSource};
use crate::snapshot::{self, CaptureMode, Repository};
use crate::store::Store;
use crate::terminal;

pub fn run(arguments: Vec<String>, stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    match execute(arguments, stdout, stderr) {
        Ok(()) => 0,
        Err(error) => {
            let rendered = terminal::sanitize_external(&format!("{error:#}"));
            let _ = writeln!(stderr, "agentlab: {rendered}");
            1
        }
    }
}

fn execute(arguments: Vec<String>, stdout: &mut dyn Write, stderr: &mut dyn Write) -> Result<()> {
    let Some(command) = arguments.first().map(String::as_str) else {
        print_help(stdout)?;
        return Ok(());
    };
    match command {
        "--help" | "-h" => print_help(stdout),
        "help" => match arguments.as_slice() {
            [_] => print_help(stdout),
            [_, command] => execute(
                vec![command.to_owned(), "--help".to_owned()],
                stdout,
                stderr,
            ),
            _ => bail!("help accepts at most one COMMAND\n\nRun `agentlab --help` for usage."),
        },
        "--version" | "version" => {
            writeln!(stdout, "agentlab {}", build_version())?;
            Ok(())
        }
        "snapshot" => snapshot_command(&arguments[1..], stdout, stderr),
        "run" => run_command(&arguments[1..], stdout, stderr),
        "evaluate" => evaluate_command(&arguments[1..], stdout),
        "report" => report_command(&arguments[1..], stdout),
        "review" => review_command(&arguments[1..], stdout, stderr),
        "apply" => apply_command(&arguments[1..], stdout, stderr),
        "accept" => accept_command(&arguments[1..], stdout, stderr),
        "list" => list_command(&arguments[1..], stdout),
        "stop" => stop_command(&arguments[1..], stdout),
        "resume" => resume_command(&arguments[1..], stdout),
        "fork" => fork_command(&arguments[1..], stdout),
        "rm" => remove_command(&arguments[1..], stdout),
        "compare" => compare_command(&arguments[1..], stdout),
        "diff" => diff_command(&arguments[1..], stdout, stderr),
        "inspect" => inspect_command(&arguments[1..], stdout),
        _ => bail!("unknown command {command:?}\n\nRun `agentlab --help` for usage."),
    }
}

fn review_command(
    arguments: &[String],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<()> {
    let options_end = arguments
        .iter()
        .position(|argument| argument == "--")
        .unwrap_or(arguments.len());
    if arguments.is_empty()
        || arguments[..options_end]
            .iter()
            .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
    {
        print_review_help(stdout)?;
        return Ok(());
    }
    let separator = arguments
        .iter()
        .position(|argument| argument == "--")
        .ok_or_else(|| anyhow::anyhow!("review requires `-- COMMAND [ARG ...]`"))?;
    let (options, command_with_separator) = arguments.split_at(separator);
    let reviewer_command = &command_with_separator[1..];
    if reviewer_command.is_empty() {
        bail!("review requires a reviewer command after `--`");
    }
    let mut run_id = None;
    let mut workspace = None;
    let mut json = false;
    let mut timeout_seconds = crate::process::DEFAULT_EXTERNAL_TIMEOUT_SECONDS;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--workspace" => {
                workspace = Some(PathBuf::from(required_value(
                    options,
                    &mut index,
                    "--workspace",
                )?))
            }
            "--json" => json = true,
            "--timeout" => {
                timeout_seconds = required_value(options, &mut index, "--timeout")?
                    .parse::<u64>()
                    .context("--timeout requires a whole number of seconds")?;
                if !(1..=86_400).contains(&timeout_seconds) {
                    bail!("--timeout must be between 1 and 86400 seconds");
                }
            }
            value if value.starts_with('-') => bail!("unexpected review argument {value:?}"),
            value if run_id.is_none() => run_id = Some(value.to_owned()),
            value => bail!("unexpected review argument {value:?}"),
        }
        index += 1;
    }
    let run_id = run_id.ok_or_else(|| anyhow::anyhow!("review requires RUN"))?;
    let workspace =
        workspace.ok_or_else(|| anyhow::anyhow!("review requires --workspace CURRENT"))?;
    let store = Store::open(None)?;
    let record = {
        let mut observer = CliReviewObserver {
            stderr,
            started: Instant::now(),
        };
        review::review_with_observer(
            &store,
            &ReviewOptions {
                run_id,
                workspace,
                reviewer_command: reviewer_command.to_vec(),
                timeout_seconds,
            },
            &mut observer,
        )?
    };
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &record)?;
        writeln!(stdout)?;
        return Ok(());
    }
    writeln!(stdout, "Review: {}", record.review_id)?;
    writeln!(stdout, "Run: {}", record.run_id)?;
    writeln!(stdout, "Receipt: {}", record.digest)?;
    writeln!(
        stdout,
        "Workspace path: {}",
        terminal::escape(&record.source_workspace)
    )?;
    writeln!(
        stdout,
        "Base snapshot: {}",
        record.request.anchors.base_workspace_snapshot_digest
    )?;
    writeln!(
        stdout,
        "Candidate snapshot: {}",
        record.request.anchors.candidate_workspace_snapshot_digest
    )?;
    writeln!(
        stdout,
        "Current snapshot: {}",
        record.request.anchors.current_workspace_snapshot_digest
    )?;
    writeln!(stdout, "Candidates: {}", record.request.candidates.len())?;
    let attempt = review::find_attempt(&store, &record.review_id)?;
    writeln!(stdout, "Reviewer attempts: {}", attempt.invocations.len())?;
    writeln!(
        stdout,
        "Dispositions: {} proposed, {} rejected, {} conflicted, {} unresolved",
        record.proposal.counts.proposed,
        record.proposal.counts.rejected,
        record.proposal.counts.conflicted,
        record.proposal.counts.unresolved
    )?;
    for disposition in &record.proposal.dispositions {
        writeln!(
            stdout,
            "  {:<10} {} — {}",
            terminal::escape(&disposition.disposition),
            terminal::escape(&disposition.path),
            terminal::escape(&disposition.reason)
        )?;
        if let Some(recommendation) = &disposition.recommendation {
            writeln!(
                stdout,
                "    recommendation: {}",
                terminal::escape(recommendation)
            )?;
        }
    }
    if !record.proposal.recommendations.is_empty() {
        writeln!(stdout, "Environment recommendations:")?;
        for recommendation in &record.proposal.recommendations {
            writeln!(
                stdout,
                "  {}",
                terminal::escape(&recommendation.recommendation)
            )?;
            writeln!(
                stdout,
                "    reason: {}",
                terminal::escape(&recommendation.reason)
            )?;
        }
    }
    writeln!(
        stdout,
        "Source workspace unchanged: {}",
        record.source_workspace_unchanged
    )?;
    writeln!(
        stdout,
        "AgentLab applied changes: {}",
        record.agentlab_applied_changes
    )?;
    for warning in &record.warnings {
        writeln!(stdout, "Warning: {}", terminal::escape(warning))?;
    }
    writeln!(
        stdout,
        "Inspect review: agentlab inspect --verify {}",
        record.review_id
    )?;
    writeln!(
        stdout,
        "Inspect run: agentlab inspect --verify {}",
        record.run_id
    )?;
    writeln!(
        stdout,
        "Apply (mutates workspace): agentlab apply {} --workspace {}",
        record.review_id,
        shell_word(&record.source_workspace)
    )?;
    Ok(())
}

fn print_review_help(stdout: &mut dyn Write) -> Result<()> {
    writeln!(
        stdout,
        "AgentLab review\n\nAsk a trusted command-line reviewer which changes from a run are worth carrying forward. AgentLab records the proposal and applies nothing.\n\nUsage:\n  agentlab review [--json] [--timeout SECONDS] RUN --workspace CURRENT -- COMMAND [ARG ...]\n\nArguments:\n  RUN                       Completed AgentLab run to review\n  --workspace CURRENT       Current host workspace to compare with the run\n  --timeout SECONDS         Reviewer limit (default: 1800)\n  --json                    Write the complete review receipt as JSON\n  -- COMMAND [ARG ...]      Trusted reviewer command and its arguments\n\nExample:\n  agentlab review RUN_ID --workspace ./project -- ./pi-review.sh\n\nThe reviewer receives private base, candidate, and current workspace copies; the original command output; evaluator observations; and the complete machine delta. It runs on the host with your permissions and may see sensitive captured content. AgentLab shows elapsed progress, retains every invocation, allows one correction for a structurally invalid proposal, rechecks that the source workspace did not change, and applies nothing."
    )?;
    Ok(())
}

fn apply_command(
    arguments: &[String],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<()> {
    if arguments.is_empty()
        || arguments
            .iter()
            .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
    {
        print_apply_help(stdout)?;
        return Ok(());
    }
    let mut review_id = None;
    let mut workspace = None;
    let mut acknowledge_conflicts = false;
    let mut acknowledge_unresolved = false;
    let mut json = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--workspace" => {
                workspace = Some(PathBuf::from(required_value(
                    arguments,
                    &mut index,
                    "--workspace",
                )?))
            }
            "--acknowledge-conflicts" => acknowledge_conflicts = true,
            "--acknowledge-unresolved" => acknowledge_unresolved = true,
            "--json" => json = true,
            value if value.starts_with('-') => bail!("unexpected apply argument {value:?}"),
            value if review_id.is_none() => review_id = Some(value.to_owned()),
            value => bail!("unexpected apply argument {value:?}"),
        }
        index += 1;
    }
    let review_id = review_id.ok_or_else(|| anyhow::anyhow!("apply requires REVIEW_ID"))?;
    let workspace =
        workspace.ok_or_else(|| anyhow::anyhow!("apply requires --workspace CURRENT"))?;
    writeln!(
        stderr,
        "AgentLab: applying only receipt-authorized workspace paths; the current workspace must exactly match the reviewed state, and a complete backup snapshot will be retained."
    )?;
    stderr.flush()?;
    let store = Store::open(None)?;
    let record = apply::apply(
        &store,
        &ApplyOptions {
            review_id,
            workspace,
            acknowledge_conflicts,
            acknowledge_unresolved,
        },
    )?;
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &record)?;
        writeln!(stdout)?;
        return Ok(());
    }
    writeln!(stdout, "Apply: {}", record.apply_id)?;
    writeln!(stdout, "Review: {}", record.review_id)?;
    writeln!(stdout, "Run: {}", record.run_id)?;
    writeln!(stdout, "Receipt: {}", record.digest)?;
    writeln!(
        stdout,
        "Before workspace: {}",
        record.before_workspace_snapshot_digest
    )?;
    writeln!(
        stdout,
        "After workspace: {}",
        record.after_workspace_snapshot_digest
    )?;
    writeln!(
        stdout,
        "Applied workspace operations: {}",
        record.counts.applied
    )?;
    for operation in &record.operations {
        writeln!(
            stdout,
            "  {:<7} {}",
            terminal::escape(&operation.operation),
            terminal::escape(&operation.path)
        )?;
    }
    writeln!(
        stdout,
        "Acknowledged candidates: {} conflicted, {} unresolved",
        record.counts.conflicted, record.counts.unresolved
    )?;
    writeln!(
        stdout,
        "Backup snapshot: {}",
        record.before_workspace_snapshot_digest
    )?;
    writeln!(stdout, "Result workspace verified: true")?;
    let image_reference = run::immutable_image_reference(&store, &record.run_id)?;
    writeln!(
        stdout,
        "Retest exact applied input: agentlab run --snapshot {} --image {} -- COMMAND",
        record.after_workspace_snapshot_digest,
        shell_word(&image_reference)
    )?;
    writeln!(
        stdout,
        "Accept after retest: agentlab accept RETEST_RUN --from-apply {}",
        record.apply_id
    )?;
    writeln!(
        stdout,
        "Inspect: agentlab inspect --verify {}",
        record.run_id
    )?;
    Ok(())
}

fn accept_command(
    arguments: &[String],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<()> {
    if arguments.is_empty()
        || arguments
            .iter()
            .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
    {
        print_accept_help(stdout)?;
        return Ok(());
    }
    let mut tested_by_run_id = None;
    let mut from_apply_id = None;
    let mut json = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--from-apply" => {
                from_apply_id =
                    Some(required_value(arguments, &mut index, "--from-apply")?.to_owned())
            }
            "--json" => json = true,
            value if value.starts_with('-') => bail!("unexpected accept argument {value:?}"),
            value if tested_by_run_id.is_none() => tested_by_run_id = Some(value.to_owned()),
            value => bail!("unexpected accept argument {value:?}"),
        }
        index += 1;
    }
    let tested_by_run_id =
        tested_by_run_id.ok_or_else(|| anyhow::anyhow!("accept requires RUN"))?;
    writeln!(
        stderr,
        "AgentLab: recording an explicit acceptance of the exact workspace and OCI image input tested by run {tested_by_run_id}; test output and session changes are not promoted."
    )?;
    stderr.flush()?;
    let store = Store::open(None)?;
    let record = acceptance::accept(
        &store,
        &AcceptOptions {
            tested_by_run_id,
            from_apply_id,
        },
    )?;
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &record)?;
        writeln!(stdout)?;
        return Ok(());
    }
    writeln!(stdout, "Acceptance: {}", record.acceptance_id)?;
    writeln!(stdout, "Record: {}", record.digest)?;
    writeln!(stdout, "Accepted input: {}", record.accepted_input_digest)?;
    writeln!(stdout, "Kind: {}", record.kind)?;
    writeln!(stdout, "Workspace: {}", record.workspace_snapshot_digest)?;
    writeln!(
        stdout,
        "OCI image: {} ({})",
        record.image.execution_reference, record.image.resolved_digest
    )?;
    writeln!(stdout, "Test run: {}", record.tested_by_run_id)?;
    writeln!(stdout, "Test exit code: {}", record.test_exit_code)?;
    if let Some(lineage) = &record.applied_lineage {
        writeln!(stdout, "Candidate run: {}", lineage.candidate_run_id)?;
        writeln!(stdout, "Review: {}", lineage.review_id)?;
        writeln!(stdout, "Apply: {}", lineage.apply_id)?;
    }
    writeln!(stdout, "Decision: explicit")?;
    writeln!(
        stdout,
        "Run accepted input: agentlab run --accepted {} -- COMMAND",
        record.acceptance_id
    )?;
    writeln!(
        stdout,
        "Inspect: agentlab inspect --verify {}",
        record.acceptance_id
    )?;
    Ok(())
}

fn print_accept_help(stdout: &mut dyn Write) -> Result<()> {
    writeln!(
        stdout,
        "AgentLab accept\n\nExplicitly accept the exact workspace and OCI image input tested by a completed run. This records lineage; it does not promote the run's output filesystem.\n\nUsage:\n  agentlab accept [--json] RUN [--from-apply APPLY_ID]\n\nArguments:\n  RUN                       Completed run that tested the input being accepted\n  --from-apply APPLY_ID     Require the test input to equal this reviewed apply result\n  --json                    Write the immutable acceptance record as JSON\n\nExamples:\n  agentlab accept INITIAL_TEST_RUN\n  agentlab accept RETEST_RUN --from-apply APPLY_ID\n\nWithout --from-apply, this creates or extends a tested-input lineage. With --from-apply, AgentLab requires an independent retest whose workspace is the exact after snapshot and whose OCI image and workspace path match the candidate run. Exit status is recorded but never interpreted as universal correctness. Each test run receives at most one acceptance decision."
    )?;
    Ok(())
}

fn print_apply_help(stdout: &mut dyn Write) -> Result<()> {
    writeln!(
        stdout,
        "AgentLab apply\n\nApply only the workspace operations authorized by one immutable review receipt. This command mutates the selected host workspace.\n\nUsage:\n  agentlab apply [--json] [--acknowledge-conflicts] [--acknowledge-unresolved] REVIEW_ID --workspace CURRENT\n\nArguments:\n  REVIEW_ID                    Review receipt to apply exactly once\n  --workspace CURRENT          Host workspace that was captured by the review\n  --acknowledge-conflicts      Acknowledge conflicted candidates without applying them\n  --acknowledge-unresolved     Acknowledge unresolved candidates without applying them\n  --json                       Write the complete apply receipt as JSON\n\nExample:\n  agentlab apply REVIEW_ID --workspace ./project --acknowledge-conflicts --acknowledge-unresolved\n\nThe current workspace must exactly match the snapshot anchored by the review. AgentLab privately stages the authorized result, retains a complete recoverable before snapshot, changes no rejected/conflicted/unresolved or environment path, verifies the exact after snapshot, and rejects a second apply from the same review."
    )?;
    Ok(())
}

fn evaluate_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    let options_end = arguments
        .iter()
        .position(|argument| argument == "--")
        .unwrap_or(arguments.len());
    if arguments[..options_end]
        .iter()
        .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
    {
        print_evaluate_help(stdout)?;
        return Ok(());
    }
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
    let mut timeout_seconds = crate::process::DEFAULT_EXTERNAL_TIMEOUT_SECONDS;
    let mut run_ids = Vec::new();
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--name" => name = Some(required_value(options, &mut index, "--name")?.to_owned()),
            "--timeout" => {
                timeout_seconds = required_value(options, &mut index, "--timeout")?
                    .parse::<u64>()
                    .context("--timeout requires a whole number of seconds")?;
                if !(1..=86_400).contains(&timeout_seconds) {
                    bail!("--timeout must be between 1 and 86400 seconds");
                }
            }
            "--json" => json = true,
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
        records.push(evaluation::evaluate_with_timeout(
            &store,
            run_id,
            &evaluator_name,
            command,
            timeout_seconds,
        )?);
    }
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &records)?;
        writeln!(stdout)?;
    } else {
        for record in &records {
            writeln!(stdout, "Run: {}", record.run_id)?;
            writeln!(stdout, "Evaluation: {}", record.evaluation_id)?;
            writeln!(
                stdout,
                "Evaluator: {}",
                terminal::escape(&record.evaluator_name)
            )?;
            writeln!(stdout, "Status: {}", record.status)?;
            writeln!(stdout, "Exit code: {}", record.exit_code)?;
            if let Some(output) = &record.output {
                writeln!(
                    stdout,
                    "Scores: {}",
                    if output.scores.is_empty() {
                        "none".to_owned()
                    } else {
                        output
                            .scores
                            .keys()
                            .map(|key| terminal::escape(key))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )?;
                if let Some(summary) = &output.summary {
                    writeln!(stdout, "Summary: {}", terminal::escape(summary))?;
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
            "--factor" => bail!(
                "--factor was removed; reports identify real run-input, workspace, image, and portable-base identities"
            ),
            "--score" => scores.push(required_value(arguments, &mut index, "--score")?.to_owned()),
            "--json" => json = true,
            "--help" | "-h" => {
                writeln!(
                    stdout,
                    "AgentLab report\n\nShow evaluator observations beside the real inputs for explicit runs. AgentLab does not rank or aggregate them.\n\nUsage:\n  agentlab report [--evaluator NAME] [--score KEY]... [--json] RUN...\n\nOptions:\n  --evaluator NAME          Use the latest successful record from this evaluator\n  --score KEY               Include this scalar score; repeat for more columns\n  --json                    Write a machine-readable table instead of Markdown"
                )?;
                return Ok(());
            }
            value if value.starts_with('-') => bail!("unexpected report argument {value:?}"),
            value => run_ids.push(value.to_owned()),
        }
        index += 1;
    }
    let store = Store::open(None)?;
    let table = evaluation::table(&store, &run_ids, evaluator_name.as_deref(), &scores)?;
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &table)?;
        writeln!(stdout)?;
    } else {
        write!(
            stdout,
            "{}",
            terminal::sanitize_external(&evaluation::markdown_table(&table))
        )?;
        for warning in &table.warnings {
            writeln!(stdout, "Warning: {}", terminal::escape(warning))?;
        }
    }
    Ok(())
}

fn list_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    let json = match arguments {
        [] => false,
        [argument] if argument == "--json" => true,
        [argument] if argument == "--help" || argument == "-h" => {
            writeln!(
                stdout,
                "AgentLab list\n\nList retained runs and their current Docker container state.\n\nUsage:\n  agentlab list [--json]\n\nOptions:\n  --json                    Write machine-readable run records"
            )?;
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
        writeln!(
            stdout,
            "AgentLab stop\n\nStop a retained run while preserving its container filesystem. Process memory is not preserved.\n\nUsage:\n  agentlab stop [--json] RUN"
        )?;
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
            "AgentLab resume\n\nRestart a retained container and optionally run a continuation command in its existing filesystem. Process memory is not restored.\n\nUsage:\n  agentlab resume [--json] RUN\n  agentlab resume [--json] [--pi-auth] [--secret-file NAME=HOST_PATH]... RUN -- COMMAND [ARG ...]\n\nOptions:\n  --pi-auth                 Inject ~/.pi/agent/auth.json only for the continuation command\n  --secret-file NAME=PATH   Inject a host file at /run/agentlab-secrets/NAME only for the command\n  --json                    Write the lifecycle or continuation record as JSON"
        )?;
        return Ok(());
    }
    let separator = arguments.iter().position(|argument| argument == "--");
    let (options, command) = match separator {
        Some(index) => (&arguments[..index], &arguments[index + 1..]),
        None => (arguments, &[][..]),
    };
    let mut run_id = None;
    let mut json = false;
    let mut pi_auth = None;
    let mut secret_files = Vec::new();
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--json" => json = true,
            "--pi-auth" => {
                pi_auth = Some(
                    dirs::home_dir()
                        .context("resolve host home directory for --pi-auth")?
                        .join(".pi/agent/auth.json"),
                )
            }
            "--secret-file" => secret_files.push(parse_secret_file(required_value(
                options,
                &mut index,
                "--secret-file",
            )?)?),
            value if value.starts_with('-') => bail!("unexpected resume argument {value:?}"),
            value if run_id.is_none() => run_id = Some(value),
            value => bail!("unexpected resume argument {value:?}"),
        }
        index += 1;
    }
    let run_id = run_id.ok_or_else(|| anyhow::anyhow!("resume requires RUN"))?;
    if (pi_auth.is_some() || !secret_files.is_empty()) && command.is_empty() {
        bail!("resume credential injection requires `-- COMMAND [ARG ...]`");
    }
    let store = Store::open(None)?;
    let result =
        lifecycle::resume_with_secrets(&store, run_id, command, pi_auth.as_deref(), &secret_files)?;
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
        writeln!(
            stdout,
            "AgentLab fork\n\nCreate an independent retained run from another run's current filesystem. Process memory is not copied.\n\nUsage:\n  agentlab fork [--json] RUN"
        )?;
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
        writeln!(
            stdout,
            "AgentLab rm\n\nDelete exactly one unreferenced run's owned container, prepared image tag, and local run artifacts. Runs preserved by accepted lineage are refused.\n\nUsage:\n  agentlab rm [--json] RUN"
        )?;
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

fn parse_secret_file(value: &str) -> Result<SecretFileSpec> {
    let (name, source) = value
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("--secret-file requires NAME=HOST_PATH"))?;
    if name.is_empty() || source.is_empty() {
        bail!("--secret-file requires non-empty NAME and HOST_PATH");
    }
    Ok(SecretFileSpec {
        name: name.to_owned(),
        source: PathBuf::from(source),
    })
}

struct CliRunObserver<'a> {
    stdout: &'a mut dyn Write,
    stderr: &'a mut dyn Write,
    json: bool,
    started: Instant,
    command_stdout_sanitizer: terminal::StreamSanitizer,
    command_stderr_sanitizer: terminal::StreamSanitizer,
}

impl CliRunObserver<'_> {
    fn finish_command_output(&mut self) -> std::io::Result<()> {
        if self.json {
            self.command_stdout_sanitizer.finish(self.stderr)?;
        } else {
            self.command_stdout_sanitizer.finish(self.stdout)?;
        }
        self.command_stderr_sanitizer.finish(self.stderr)
    }
}

struct CliReviewObserver<'a> {
    stderr: &'a mut dyn Write,
    started: Instant,
}

struct CliDiffObserver<'a> {
    stderr: &'a mut dyn Write,
    started: Instant,
}

impl review::ReviewObserver for CliReviewObserver<'_> {
    fn stage(&mut self, message: &str) -> std::io::Result<()> {
        let message = terminal::sanitize_external(message);
        writeln!(
            self.stderr,
            "[{:.1}s] {message}",
            self.started.elapsed().as_secs_f64()
        )?;
        self.stderr.flush()
    }
}

impl diff::DiffObserver for CliDiffObserver<'_> {
    fn stage(&mut self, message: &str) -> std::io::Result<()> {
        let message = terminal::sanitize_external(message);
        writeln!(
            self.stderr,
            "[{:.1}s] {message}",
            self.started.elapsed().as_secs_f64()
        )?;
        self.stderr.flush()
    }
}

impl run::RunObserver for CliRunObserver<'_> {
    fn stage(&mut self, message: &str) -> std::io::Result<()> {
        let message = terminal::sanitize_external(message);
        writeln!(
            self.stderr,
            "[{:.1}s] {message}",
            self.started.elapsed().as_secs_f64()
        )?;
        self.stderr.flush()
    }

    fn command_stdout(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.json {
            self.command_stdout_sanitizer.write(self.stderr, bytes)?;
            self.stderr.flush()
        } else {
            self.command_stdout_sanitizer.write(self.stdout, bytes)?;
            self.stdout.flush()
        }
    }

    fn command_stderr(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.command_stderr_sanitizer.write(self.stderr, bytes)?;
        self.stderr.flush()
    }
}

fn print_run_help(stdout: &mut dyn Write) -> Result<()> {
    writeln!(
        stdout,
        "AgentLab run\n\nRun one opaque command in a private Docker filesystem reconstructed from a complete workspace, exact stored snapshot, or explicitly accepted input.\n\nUsage:\n  agentlab run --workspace PATH --image IMAGE [OPTIONS] -- COMMAND [ARG ...]\n  agentlab run --snapshot DIGEST --image IMAGE [OPTIONS] -- COMMAND [ARG ...]\n  agentlab run --accepted ACCEPTANCE_ID [OPTIONS] -- COMMAND [ARG ...]\n\nWorkspace input:\n  --workspace PATH          Capture every supported path, then run that exact snapshot\n  --snapshot DIGEST         Reuse and verify an existing immutable snapshot\n  --accepted ACCEPTANCE_ID  Reuse an accepted workspace, OCI image, and guest path\n  --respect-gitignore       Deliberately omit paths selected by workspace .gitignore files\n  --workspace-path PATH     Guest path for the private workspace (default: /workspace)\n\nRuntime:\n  --image IMAGE             OCI image tag or digest (required except with --accepted)\n  --network none|bridge     Network policy (default: bridge)\n  --memory LIMIT            Docker memory limit\n  --cpus COUNT              Docker CPU limit\n  --pi-auth                 Inject ~/.pi/agent/auth.json only while the command runs\n  --secret-file NAME=PATH   Inject a host file at /run/agentlab-secrets/NAME only for the command\n\nObservation:\n  --change-ignore PATH      Git-compatible rules for portable result changes\n  --capture PATH=NAME       Export an additional guest path as a retained tar artifact\n  --json                    Write only the final JSON summary to stdout; progress stays on stderr\n\nExamples:\n  agentlab run --workspace ./project --image alpine:3.21 -- /bin/sh -c 'printf \"private\\n\" > /workspace/proof.txt'\n  agentlab run --accepted ACCEPTANCE_ID -- HARNESS TASK\n\nThe source workspace is never mounted into the container. An accepted reference supplies and verifies the exact snapshot, immutable OCI image, and workspace path; command and runtime settings remain explicit experiment inputs. Network access uses Docker bridge mode by default; pass --network none for an offline run. Runtime credentials use private memory and are removed before filesystem capture. A generic secret named NAME is available only at /run/agentlab-secrets/NAME; records retain the name but never its host path, bytes, or digest. The guest command can still reveal or copy any credential it receives. AgentLab streams command output, captures the complete persistent guest filesystem, retains the container, and verifies a direct source workspace again after the run."
    )?;
    Ok(())
}

fn print_evaluate_help(stdout: &mut dyn Write) -> Result<()> {
    writeln!(
        stdout,
        "AgentLab evaluate\n\nInvoke one trusted host command against one or more integrity-checked run results.\n\nUsage:\n  agentlab evaluate [--name NAME] [--timeout SECONDS] [--json] RUN... -- COMMAND [ARG ...]\n\nOptions:\n  --name NAME               Stable evaluator name\n  --timeout SECONDS         Per-run evaluator limit (default: 1800)\n  --json                    Write complete evaluation receipts as JSON\n\nThe evaluator receives absolute AgentLab artifact paths through environment variables and must emit one JSON object. Evaluators run with the invoking user's host permissions; AgentLab does not sandbox them."
    )?;
    Ok(())
}

fn run_command(arguments: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> Result<()> {
    let options_end = arguments
        .iter()
        .position(|argument| argument == "--")
        .unwrap_or(arguments.len());
    if arguments[..options_end]
        .iter()
        .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
    {
        print_run_help(stdout)?;
        return Ok(());
    }
    let separator = arguments
        .iter()
        .position(|argument| argument == "--")
        .ok_or_else(|| anyhow::anyhow!("run requires `-- COMMAND [ARG ...]`"))?;
    let (options, command_with_separator) = arguments.split_at(separator);
    let command = &command_with_separator[1..];
    if command.is_empty() {
        bail!("run requires a command after `--`");
    }

    let mut workspace = None;
    let mut snapshot = None;
    let mut accepted = None;
    let mut workspace_path_set = false;
    let mut parsed = RunOptions {
        workspace: WorkspaceSource::Directory(PathBuf::from(".")),
        workspace_capture_mode: CaptureMode::All,
        image: String::new(),
        command: command.to_vec(),
        workspace_guest_path: "/workspace".to_owned(),
        network: "bridge".to_owned(),
        memory: None,
        cpus: None,
        pi_auth: None,
        secret_files: Vec::new(),
        change_ignore: None,
        captures: Vec::new(),
        accepted_input: None,
    };
    let mut json = false;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--workspace" => {
                if snapshot.is_some() || accepted.is_some() {
                    bail!("--workspace, --snapshot, and --accepted are mutually exclusive");
                }
                workspace = Some(PathBuf::from(required_value(
                    options,
                    &mut index,
                    "--workspace",
                )?));
            }
            "--snapshot" => {
                if workspace.is_some() || accepted.is_some() {
                    bail!("--workspace, --snapshot, and --accepted are mutually exclusive");
                }
                snapshot = Some(required_value(options, &mut index, "--snapshot")?.to_owned());
            }
            "--accepted" => {
                if workspace.is_some() || snapshot.is_some() {
                    bail!("--workspace, --snapshot, and --accepted are mutually exclusive");
                }
                accepted = Some(required_value(options, &mut index, "--accepted")?.to_owned());
            }
            "--image" => parsed.image = required_value(options, &mut index, "--image")?.to_owned(),
            "--workspace-path" => {
                workspace_path_set = true;
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
            "--pi-auth" => {
                parsed.pi_auth = Some(
                    dirs::home_dir()
                        .context("resolve host home directory for --pi-auth")?
                        .join(".pi/agent/auth.json"),
                )
            }
            "--secret-file" => parsed.secret_files.push(parse_secret_file(required_value(
                options,
                &mut index,
                "--secret-file",
            )?)?),
            "--respect-gitignore" => parsed.workspace_capture_mode = CaptureMode::RespectGitignore,
            "--factor" => bail!(
                "--factor was removed; vary a real workspace snapshot, image, command, or runtime input instead"
            ),
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
            value => bail!("unexpected run argument {value:?}"),
        }
        index += 1;
    }
    let store = Store::open(None)?;
    if let Some(acceptance_id) = accepted {
        if !parsed.image.is_empty() {
            bail!("--accepted supplies the OCI image and cannot be combined with --image");
        }
        if workspace_path_set {
            bail!(
                "--accepted supplies the guest workspace path and cannot be combined with --workspace-path"
            );
        }
        if parsed.workspace_capture_mode != CaptureMode::All {
            bail!(
                "--accepted supplies an exact snapshot and cannot be combined with --respect-gitignore"
            );
        }
        let record = acceptance::find(&store, &acceptance_id)?;
        acceptance::verify(&store, &record)?;
        parsed.workspace = WorkspaceSource::Snapshot(record.workspace_snapshot_digest.clone());
        parsed.workspace_guest_path = record.workspace_guest_path.clone();
        parsed.image = record.image.execution_reference.clone();
        parsed.accepted_input = Some(acceptance::reference(&record));
    } else {
        if parsed.image.is_empty() {
            bail!("run requires --image IMAGE unless --accepted ACCEPTANCE_ID is used");
        }
        parsed.workspace = match snapshot {
            Some(digest) => WorkspaceSource::Snapshot(digest),
            None => WorkspaceSource::Directory(workspace.unwrap_or_else(|| PathBuf::from("."))),
        };
    }
    let result = {
        let mut observer = CliRunObserver {
            stdout,
            stderr,
            json,
            started: Instant::now(),
            command_stdout_sanitizer: terminal::StreamSanitizer::default(),
            command_stderr_sanitizer: terminal::StreamSanitizer::default(),
        };
        let result = run::execute_with_observer(&parsed, &store, &mut observer);
        observer.finish_command_output()?;
        result?
    };
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &result)?;
        writeln!(stdout)?;
    } else {
        writeln!(stdout)?;
        writeln!(stdout, "Run: {}", result.run_id)?;
        writeln!(stdout, "Exit code: {}", result.exit_code)?;
        writeln!(stdout, "Run input: {}", result.run_input_digest)?;
        writeln!(stdout, "Snapshot: {}", result.workspace_snapshot_digest)?;
        if let Some(reference) = &result.accepted_input {
            writeln!(stdout, "Acceptance: {}", reference.acceptance_id)?;
            writeln!(
                stdout,
                "Accepted input: {}",
                reference.accepted_input_digest
            )?;
        }
        writeln!(stdout, "Portable changes: {}", result.changes)?;
        writeln!(stdout, "Ignored changes: {}", result.ignored_changes)?;
        writeln!(
            stdout,
            "Source workspace: {}",
            result.source_workspace_status
        )?;
        writeln!(
            stdout,
            "Retained container: {}",
            result.retained_container_name
        )?;
        writeln!(
            stdout,
            "Inspect:  agentlab inspect --verify {}",
            result.run_id
        )?;
        writeln!(stdout, "Changes:  agentlab diff {}", result.run_id)?;
        writeln!(stdout, "Raw:      agentlab diff --raw {}", result.run_id)?;
        writeln!(stdout, "Stop:     agentlab stop {}", result.run_id)?;
        writeln!(
            stdout,
            "Continue: agentlab resume {} -- COMMAND",
            result.run_id
        )?;
        writeln!(stdout, "Fork:     agentlab fork {}", result.run_id)?;
        writeln!(stdout, "Remove:   agentlab rm {}", result.run_id)?;
    }
    Ok(())
}

fn compare_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    let mut json = false;
    let mut run_ids = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--json" => json = true,
            "--expect-factor" => bail!(
                "--expect-factor was removed; compare reports differences in actual resolved inputs"
            ),
            "--help" | "-h" => {
                writeln!(
                    stdout,
                    "AgentLab compare\n\nCompare two runs using their actual resolved inputs, private containers, and portable outcomes.\n\nUsage:\n  agentlab compare [--json] LEFT_RUN RIGHT_RUN"
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
    let store = Store::open(None)?;
    let comparison = run::compare_runs(&store, run_ids[0], run_ids[1])?;
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
    writeln!(stdout, "Comparison: {}", comparison.comparison_kind)?;
    writeln!(stdout, "Same run input: {}", comparison.same_run_input)?;
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
        names
            .iter()
            .map(|name| terminal::escape(name))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/_-.:".contains(&byte))
    {
        value.to_owned()
    } else if terminal::escape(value) != value {
        let mut escaped = String::from("$'");
        for byte in value.as_bytes() {
            match byte {
                b'\\' => escaped.push_str("\\\\"),
                b'\'' => escaped.push_str("\\'"),
                0x20..=0x7e => escaped.push(char::from(*byte)),
                _ => escaped.push_str(&format!("\\x{byte:02x}")),
            }
        }
        escaped.push('\'');
        escaped
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn snapshot_command(
    arguments: &[String],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<()> {
    let mut workspace = PathBuf::from(".");
    let mut capture_mode = CaptureMode::All;
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
            "--respect-gitignore" => capture_mode = CaptureMode::RespectGitignore,
            "--capture" => {
                index += 1;
                capture_mode = match arguments.get(index).map(String::as_str) {
                    Some("all") => CaptureMode::All,
                    Some(value) => bail!("--capture requires `all`, got {value:?}"),
                    None => bail!("--capture requires `all`"),
                };
            }
            "--help" | "-h" => {
                writeln!(
                    stdout,
                    "AgentLab snapshot\n\nCapture a workspace as an immutable, content-addressed snapshot. Every supported path is included by default.\n\nUsage:\n  agentlab snapshot [--workspace PATH] [--respect-gitignore] [--json]\n\nOptions:\n  --workspace PATH          Workspace to capture (default: current directory)\n  --respect-gitignore       Deliberately omit paths selected by workspace .gitignore files\n  --json                    Write the snapshot summary as JSON"
                )?;
                return Ok(());
            }
            value => bail!("unexpected snapshot argument {value:?}"),
        }
        index += 1;
    }
    let store = Store::open(None)?;
    let started = Instant::now();
    write_cli_stage(
        stderr,
        started,
        &format!(
            "Capturing workspace ({}): {}",
            capture_mode.as_str(),
            workspace.display()
        ),
    )?;
    let result = snapshot::create_with_mode(&workspace, &store, capture_mode)?;
    write_cli_stage(
        stderr,
        started,
        &format!(
            "Workspace captured: {} paths, {} bytes, excluded {} ({})",
            result.included_paths,
            result.logical_bytes,
            result.excluded_paths,
            result.manifest.digest
        ),
    )?;
    if json {
        #[derive(Serialize)]
        struct Output<'a> {
            digest: &'a str,
            workspace: &'a std::path::Path,
            capture: &'a str,
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
                capture: capture_mode.as_str(),
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
        writeln!(
            stdout,
            "Workspace: {}",
            terminal::escape(&result.workspace.display().to_string())
        )?;
        writeln!(stdout, "Capture: {}", capture_mode.as_str())?;
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

fn write_cli_stage(stderr: &mut dyn Write, started: Instant, message: &str) -> Result<()> {
    let message = terminal::sanitize_external(message);
    writeln!(
        stderr,
        "[{:.1}s] {message}",
        started.elapsed().as_secs_f64()
    )?;
    stderr.flush()?;
    Ok(())
}

fn inspect_command(arguments: &[String], stdout: &mut dyn Write) -> Result<()> {
    let mut json = false;
    let mut verify = false;
    let mut verbose = false;
    let mut digest = None;
    for argument in arguments {
        match argument.as_str() {
            "--json" => json = true,
            "--verify" => verify = true,
            "--verbose" | "-v" => verbose = true,
            "--help" | "-h" => {
                writeln!(
                    stdout,
                    "AgentLab inspect\n\nInspect a stored snapshot, retained run, diff presentation, review attempt, or accepted input without printing captured file contents.\n\nUsage:\n  agentlab inspect [--json] [--verify] [--verbose] ID_OR_DIGEST\n\nOptions:\n  --verify                  Recompute and verify referenced identities and artifacts\n  --verbose, -v             List repositories, snapshot paths, or reviewer/presenter artifact paths\n  --json                    Write the complete underlying record as JSON"
                )?;
                return Ok(());
            }
            value if value.starts_with('-') => bail!("unexpected inspect argument {value:?}"),
            value if digest.is_none() => digest = Some(value),
            value => bail!("unexpected inspect argument {value:?}"),
        }
    }
    let digest = digest.ok_or_else(|| anyhow::anyhow!("inspect requires ID_OR_DIGEST"))?;
    let store = Store::open(None)?;
    if !digest.starts_with("sha256:") {
        let is_run = store.list_run_ids()?.iter().any(|run_id| run_id == digest);
        if !is_run {
            if let Some(presentation) = diff::find_optional(&store, digest)? {
                if verify {
                    diff::verify(&store, &presentation)?;
                }
                if json {
                    serde_json::to_writer_pretty(&mut *stdout, &presentation)?;
                    writeln!(stdout)?;
                    return Ok(());
                }
                writeln!(
                    stdout,
                    "Diff presentation: {}",
                    presentation.presentation_id
                )?;
                writeln!(stdout, "Schema: {}", presentation.schema_version)?;
                writeln!(stdout, "Record: {}", presentation.digest)?;
                writeln!(stdout, "Status: {}", presentation.status)?;
                writeln!(stdout, "Run: {}", presentation.run_id)?;
                writeln!(stdout, "Delta: {}", presentation.delta_digest)?;
                writeln!(
                    stdout,
                    "Source per-file diff: {}",
                    presentation.file_diff_digest
                )?;
                if let Some(digest) = &presentation.presented_diff_digest {
                    writeln!(stdout, "Presented diff: {digest}")?;
                    writeln!(
                        stdout,
                        "Changes presented: {} of {}",
                        presentation.presented_change_count, presentation.source_change_count
                    )?;
                    writeln!(
                        stdout,
                        "Presentation-hidden changes: {}",
                        presentation.presentation_ignored_paths.len()
                    )?;
                    writeln!(
                        stdout,
                        "Collapsed directory changes: {}",
                        presentation.structurally_collapsed_paths.len()
                    )?;
                    if let Some(source) = &presentation.presentation_ignore_source {
                        writeln!(
                            stdout,
                            "Presentation-ignore source: {}",
                            terminal::escape(source)
                        )?;
                        writeln!(
                            stdout,
                            "Presentation-ignore rules: {}",
                            presentation.presentation_ignore_digest
                        )?;
                    }
                }
                writeln!(stdout, "Raw selection: {}", presentation.raw)?;
                writeln!(
                    stdout,
                    "Harness: {}",
                    terminal::escape(&presentation.harness_name)
                )?;
                writeln!(stdout, "Command: {}", display_names(&presentation.command))?;
                writeln!(stdout, "Prompt: {}", presentation.prompt_version)?;
                writeln!(stdout, "Started: {}", presentation.started_at)?;
                writeln!(stdout, "Completed: {}", presentation.completed_at)?;
                writeln!(stdout, "Exit code: {}", presentation.exit_code)?;
                writeln!(stdout, "AgentLab applied changes: false")?;
                for warning in &presentation.warnings {
                    writeln!(stdout, "Warning: {}", terminal::escape(warning))?;
                }
                if verbose {
                    for (label, artifact) in [
                        ("Request", &presentation.request),
                        ("Stdout", &presentation.stdout),
                        ("Stderr", &presentation.stderr),
                    ] {
                        writeln!(
                            stdout,
                            "{label}: {}",
                            store
                                .run_path(&presentation.run_id, &artifact.path)?
                                .display()
                        )?;
                    }
                    if let Some(artifact) = &presentation.presented_diff {
                        writeln!(
                            stdout,
                            "Presented selection: {}",
                            store
                                .run_path(&presentation.run_id, &artifact.path)?
                                .display()
                        )?;
                    }
                } else {
                    writeln!(
                        stdout,
                        "Artifacts: agentlab inspect --verbose {}",
                        presentation.presentation_id
                    )?;
                }
                if verify {
                    writeln!(stdout, "Integrity: verified")?;
                }
                return Ok(());
            }
            if let Some(attempt) = review::find_attempt_optional(&store, digest)? {
                if verify {
                    review::verify_attempt(&store, &attempt)?;
                    if let Some(record) = review::find_optional(&store, digest)? {
                        review::verify(&store, &record)?;
                    }
                }
                if json {
                    serde_json::to_writer_pretty(&mut *stdout, &attempt)?;
                    writeln!(stdout)?;
                    return Ok(());
                }
                writeln!(stdout, "Review attempt: {}", attempt.review_id)?;
                writeln!(stdout, "Schema: {}", attempt.schema_version)?;
                writeln!(stdout, "Record: {}", attempt.digest)?;
                writeln!(stdout, "Status: {}", attempt.status)?;
                writeln!(stdout, "Run: {}", attempt.run_id)?;
                writeln!(
                    stdout,
                    "Current workspace: {}",
                    terminal::escape(&attempt.source_workspace)
                )?;
                writeln!(stdout, "Started: {}", attempt.started_at)?;
                writeln!(stdout, "Completed: {}", attempt.completed_at)?;
                writeln!(stdout, "Reviewer attempts: {}", attempt.invocations.len())?;
                if let Some(failure) = &attempt.failure {
                    writeln!(stdout, "Failure: {}", terminal::escape(failure))?;
                }
                for invocation in &attempt.invocations {
                    writeln!(
                        stdout,
                        "  Attempt {}: {} (exit {})",
                        invocation.attempt, invocation.status, invocation.exit_code
                    )?;
                    if let Some(error) = &invocation.validation_error {
                        writeln!(stdout, "    Validation: {}", terminal::escape(error))?;
                    }
                    if verbose {
                        writeln!(
                            stdout,
                            "    Stdout: {}",
                            store
                                .run_path(&attempt.run_id, &invocation.stdout.path)?
                                .display()
                        )?;
                        writeln!(
                            stdout,
                            "    Stderr: {}",
                            store
                                .run_path(&attempt.run_id, &invocation.stderr.path)?
                                .display()
                        )?;
                    }
                }
                writeln!(
                    stdout,
                    "Source workspace unchanged: {}",
                    attempt.source_workspace_unchanged
                )?;
                writeln!(
                    stdout,
                    "AgentLab applied changes: {}",
                    attempt.agentlab_applied_changes
                )?;
                if !verbose {
                    writeln!(
                        stdout,
                        "Artifacts: agentlab inspect --verbose {}",
                        attempt.review_id
                    )?;
                }
                if verify {
                    writeln!(stdout, "Integrity: verified")?;
                }
                return Ok(());
            }
            let record = acceptance::find(&store, digest)?;
            if verify {
                acceptance::verify(&store, &record)?;
            }
            if json {
                serde_json::to_writer_pretty(&mut *stdout, &record)?;
                writeln!(stdout)?;
                return Ok(());
            }
            writeln!(stdout, "Acceptance: {}", record.acceptance_id)?;
            writeln!(stdout, "Schema: {}", record.schema_version)?;
            writeln!(stdout, "Record: {}", record.digest)?;
            writeln!(stdout, "Accepted input: {}", record.accepted_input_digest)?;
            writeln!(stdout, "Kind: {}", record.kind)?;
            writeln!(stdout, "Decision: {}", record.decision)?;
            writeln!(stdout, "Accepted at: {}", record.accepted_at)?;
            writeln!(stdout, "Workspace: {}", record.workspace_snapshot_digest)?;
            writeln!(
                stdout,
                "Workspace path: {}",
                terminal::escape(&record.workspace_guest_path)
            )?;
            writeln!(
                stdout,
                "OCI image: {} ({})",
                terminal::escape(&record.image.execution_reference),
                record.image.resolved_digest
            )?;
            writeln!(stdout, "Test run: {}", record.tested_by_run_id)?;
            writeln!(stdout, "Test result: {}", record.test_result_digest)?;
            writeln!(stdout, "Test exit code: {}", record.test_exit_code)?;
            if let Some(parent) = &record.parent_accepted_input {
                writeln!(stdout, "Parent acceptance: {}", parent.acceptance_id)?;
            }
            if let Some(lineage) = &record.applied_lineage {
                writeln!(stdout, "Candidate run: {}", lineage.candidate_run_id)?;
                writeln!(stdout, "Review: {}", lineage.review_id)?;
                writeln!(stdout, "Apply: {}", lineage.apply_id)?;
            }
            for warning in &record.warnings {
                writeln!(stdout, "Warning: {}", terminal::escape(warning))?;
            }
            writeln!(
                stdout,
                "Run: agentlab run --accepted {} -- COMMAND",
                record.acceptance_id
            )?;
            if verify {
                writeln!(stdout, "Integrity: verified")?;
            }
            return Ok(());
        }
        if store.run_file_exists(digest, "fork.json")? {
            let fork = lifecycle::load_fork(&store, digest)?;
            if verify {
                lifecycle::verify_all(&store, digest)?;
                evaluation::verify_all(&store, digest)?;
                review::verify_all(&store, digest)?;
                apply::verify_all(&store, digest)?;
                diff::verify_all(&store, digest)?;
                for record in acceptance::list_for_run(&store, digest)? {
                    acceptance::verify(&store, &record)?;
                }
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
            review::verify_all(&store, digest)?;
            apply::verify_all(&store, digest)?;
            diff::verify_all(&store, digest)?;
            for record in acceptance::list_for_run(&store, digest)? {
                acceptance::verify(&store, &record)?;
            }
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
        writeln!(stdout, "Reviews: {}", review::list(&store, digest)?.len())?;
        writeln!(
            stdout,
            "Diff presentations: {}",
            diff::list(&store, digest)?.len()
        )?;
        writeln!(
            stdout,
            "Review attempts: {}",
            review::list_attempts(&store, digest)?.len()
        )?;
        writeln!(
            stdout,
            "Applications: {}",
            apply::list(&store, digest)?.len()
        )?;
        writeln!(
            stdout,
            "Acceptances: {}",
            acceptance::list_for_run(&store, digest)?.len()
        )?;
        for warning in &result.warnings {
            writeln!(stdout, "Warning: {}", terminal::escape(warning))?;
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
    writeln!(stdout, "Entries: {}", manifest.entries.len())?;
    if verbose {
        for repository in &manifest.repositories {
            writeln!(
                stdout,
                "  repo  {} ({} metadata at {})",
                terminal::escape(&repository.path),
                terminal::escape(&repository.metadata_kind),
                terminal::escape(&repository.metadata_path)
            )?;
        }
        for entry in &manifest.entries {
            let detail = match entry.kind.as_str() {
                "file" => format!(" size={} digest={}", entry.size, entry.digest),
                "symlink" => format!(" target={:?}", terminal::escape(&entry.link_target)),
                _ => String::new(),
            };
            writeln!(
                stdout,
                "  {:<9} {:04o} {}{}",
                terminal::escape(&entry.kind),
                entry.mode,
                terminal::escape(&entry.path),
                detail
            )?;
        }
    }
    if verify {
        writeln!(stdout, "Integrity: verified")?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffView {
    Inventory,
    NoAgent,
    Agent,
}

fn diff_command(
    arguments: &[String],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<()> {
    let mut json = false;
    let mut raw = false;
    let mut requested_view = None;
    let mut harness_name = None;
    let mut selected_path = None;
    let mut run_id = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--json" => json = true,
            "--raw" => raw = true,
            "--inventory" => set_diff_view(&mut requested_view, DiffView::Inventory)?,
            "--no-agent" => set_diff_view(&mut requested_view, DiffView::NoAgent)?,
            "--agent" => set_diff_view(&mut requested_view, DiffView::Agent)?,
            "--complete" => bail!(
                "--complete is no longer needed; the deterministic per-file diff is the baseline. Use --no-agent to bypass configured agent curation"
            ),
            "--important" => {
                bail!("--important was renamed to --agent; use `agentlab diff --agent RUN`")
            }
            "--harness" => {
                harness_name = Some(required_value(arguments, &mut index, "--harness")?.to_owned())
            }
            "--file" => {
                selected_path = Some(required_value(arguments, &mut index, "--file")?.to_owned())
            }
            "--help" | "-h" => {
                writeln!(
                    stdout,
                    "AgentLab diff\n\nShow persistent filesystem changes as deterministic per-file diffs, optionally curated by a configured agent. The default uses presentation-only ignore patterns from ~/.agentlab/config.toml without changing captured evidence.\n\nUsage:\n  agentlab diff [--agent | --no-agent | --inventory] [--harness NAME] [--file PATH] [--raw] [--json] RUN\n\nOptions:\n  --agent                   Force the configured trusted host harness for this view\n  --no-agent                Show the deterministic presentation without agent curation\n  --inventory               Show only the path-level evidence inventory\n  --harness NAME            Select a configured harness; implies --agent\n  --file PATH               Show one exact raw-evidence path regardless of ignore rules\n  --raw                     Show every captured machine change; bypass ignores and agent curation\n  --json                    Write the deterministic delta JSON unless --agent or --file is explicit\n\nConfiguration:\n  ~/.agentlab/config.toml may define default_harness, named [harnesses.NAME] commands, and [diff] use_agent and ignore. Ignore patterns use Git syntax and affect presentation only. Harnesses receive only the filtered diff selection on stdin and run from a private temporary directory. A failed agent presentation falls back to that deterministic selection. Workspace-local harness configuration is never loaded."
                )?;
                return Ok(());
            }
            value if value.starts_with('-') => bail!("unexpected diff argument {value:?}"),
            value if run_id.is_none() => run_id = Some(value.to_owned()),
            value => bail!("unexpected diff argument {value:?}"),
        }
        index += 1;
    }
    let run_id = run_id.ok_or_else(|| anyhow::anyhow!("diff requires RUN"))?;
    if selected_path.is_some() {
        match requested_view {
            Some(DiffView::Agent | DiffView::Inventory) => {
                bail!("--file cannot be combined with --agent or --inventory")
            }
            _ => requested_view = Some(DiffView::NoAgent),
        }
        raw = true;
    }
    if harness_name.is_some() {
        match requested_view {
            Some(DiffView::NoAgent | DiffView::Inventory) => {
                bail!("--harness cannot be combined with --no-agent or --inventory")
            }
            _ => requested_view = Some(DiffView::Agent),
        }
    }
    if raw && requested_view == Some(DiffView::Agent) {
        bail!("--raw cannot be combined with --agent or --harness");
    }

    let store = Store::open(None)?;
    let delta = run::load_delta(&store, &run_id, raw)?;
    if json && requested_view != Some(DiffView::Agent) && selected_path.is_none() {
        serde_json::to_writer_pretty(&mut *stdout, &delta)?;
        writeln!(stdout)?;
        return Ok(());
    }

    let config = if raw || requested_view == Some(DiffView::Inventory) {
        AgentLabConfig::default()
    } else {
        AgentLabConfig::load(&store)?
    };
    let view = requested_view.unwrap_or({
        if raw {
            DiffView::NoAgent
        } else if config.diff.use_agent() {
            DiffView::Agent
        } else {
            DiffView::NoAgent
        }
    });
    if view == DiffView::Inventory {
        if json {
            serde_json::to_writer_pretty(&mut *stdout, &delta)?;
            writeln!(stdout)?;
        } else {
            render_diff_inventory(stdout, &delta, raw)?;
        }
        return Ok(());
    }

    let bundle = diff::ensure_file_diff_bundle(&store, &run_id, raw)?;
    if let Some(path) = selected_path.as_deref() {
        if json {
            let normalized = format!("/{}", path.trim_start_matches('/'));
            let file = bundle
                .files
                .iter()
                .find(|file| file.path == normalized)
                .with_context(|| format!("run has no selected change at {normalized:?}"))?;
            serde_json::to_writer_pretty(&mut *stdout, file)?;
            writeln!(stdout)?;
        } else {
            let rendered = diff::render_complete(&bundle, Some(path))?;
            write!(stdout, "{}", terminal::sanitize_external(&rendered))?;
        }
        return Ok(());
    }

    if raw {
        let rendered = diff::render_complete(&bundle, None)?;
        write!(stdout, "{}", terminal::sanitize_external(&rendered))?;
        return Ok(());
    }

    let selection = diff::select_for_presentation(
        &bundle,
        Some("~/.agentlab/config.toml"),
        &config.diff.ignore,
    )?;
    if view == DiffView::NoAgent {
        let rendered = diff::render_selection(&selection)?;
        write!(stdout, "{}", terminal::sanitize_external(&rendered))?;
        return Ok(());
    }

    let selected_harness = config.selected_harness(harness_name.as_deref())?;
    let Some((selected_harness_name, harness)) = selected_harness else {
        if json {
            bail!(
                "agent diff presentation requires a configured harness in {}",
                crate::config::config_path(&store).display()
            );
        }
        writeln!(
            stderr,
            "AgentLab: agent diff presentation requested, but no harness is configured; showing the deterministic presentation selection."
        )?;
        let rendered = diff::render_selection(&selection)?;
        write!(stdout, "{}", terminal::sanitize_external(&rendered))?;
        return Ok(());
    };
    let record = {
        let mut observer = CliDiffObserver {
            stderr,
            started: Instant::now(),
        };
        diff::present_with_observer(
            &store,
            &bundle,
            &selection,
            selected_harness_name,
            harness,
            config.diff.show_omitted_count,
            &mut observer,
        )?
    };
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &record)?;
        writeln!(stdout)?;
        return Ok(());
    }
    if record.status == "succeeded" {
        let presentation =
            terminal::sanitize_external(&diff::presentation_output(&store, &record)?);
        writeln!(
            stdout,
            "Important changes from {} shown filesystem changes ({} captured)",
            selection.presented_change_count, selection.source_change_count
        )?;
        write_diff_selection_disclosure(stdout, &selection)?;
        writeln!(
            stdout,
            "Reviewed by harness: {}",
            terminal::escape(&record.harness_name)
        )?;
        writeln!(stdout, "Presentation: {}\n", record.presentation_id)?;
        write!(stdout, "{presentation}")?;
        if !presentation.ends_with('\n') {
            writeln!(stdout)?;
        }
        writeln!(stdout, "\nPresentation receipt: {}", record.digest)?;
        return Ok(());
    }
    writeln!(
        stderr,
        "AgentLab: diff harness {} returned {}; showing the deterministic presentation selection.",
        terminal::escape(&record.harness_name),
        terminal::escape(&record.status)
    )?;
    for warning in &record.warnings {
        writeln!(stderr, "AgentLab: {}", terminal::escape(warning))?;
    }
    let rendered = diff::render_selection(&selection)?;
    write!(stdout, "{}", terminal::sanitize_external(&rendered))?;
    Ok(())
}

fn set_diff_view(selected: &mut Option<DiffView>, requested: DiffView) -> Result<()> {
    if let Some(existing) = selected {
        if *existing != requested {
            bail!("choose only one of --agent, --no-agent, or --inventory");
        }
    }
    *selected = Some(requested);
    Ok(())
}

fn write_diff_selection_disclosure(
    stdout: &mut dyn Write,
    selection: &diff::DiffSelection,
) -> Result<()> {
    if !selection.ignored_paths.is_empty() {
        writeln!(
            stdout,
            "{} {} hidden by {}.",
            selection.ignored_paths.len(),
            if selection.ignored_paths.len() == 1 {
                "change"
            } else {
                "changes"
            },
            selection
                .ignore_source
                .as_deref()
                .unwrap_or("the diff presentation configuration")
        )?;
    }
    if !selection.collapsed_paths.is_empty() {
        writeln!(
            stdout,
            "{} implied mode-0755 directory {} collapsed.",
            selection.collapsed_paths.len(),
            if selection.collapsed_paths.len() == 1 {
                "change"
            } else {
                "changes"
            }
        )?;
    }
    if !selection.ignored_paths.is_empty() || !selection.collapsed_paths.is_empty() {
        writeln!(
            stdout,
            "Raw evidence: agentlab diff --raw {}",
            selection.run_id
        )?;
    }
    Ok(())
}

fn render_diff_inventory(
    stdout: &mut dyn Write,
    delta: &run::DeltaManifest,
    raw: bool,
) -> Result<()> {
    writeln!(stdout, "Delta: {}", delta.digest)?;
    writeln!(stdout, "Base: {}", delta.base_filesystem_digest)?;
    writeln!(stdout, "Result: {}", delta.result_filesystem_digest)?;
    writeln!(stdout, "Changes: {}", delta.changes.len())?;
    for change in &delta.changes {
        writeln!(
            stdout,
            "  {:?} {}",
            change.change,
            terminal::escape(&change.path)
        )?;
    }
    if !raw {
        writeln!(stdout, "Ignored changes: {}", delta.ignored_changes.len())?;
        for change in &delta.ignored_changes {
            writeln!(
                stdout,
                "  {:?} {}",
                change.change,
                terminal::escape(&change.path)
            )?;
        }
    }
    Ok(())
}

fn print_help(output: &mut dyn Write) -> Result<()> {
    let version = build_version();
    writeln!(
        output,
        "AgentLab {version}\n\nContent-addressed workspace snapshots and isolated agent execution.\n\nUsage:\n  agentlab --version\n  agentlab snapshot [--workspace PATH] [--respect-gitignore] [--json]\n  agentlab run [--workspace PATH | --snapshot DIGEST] --image IMAGE [OPTIONS] -- COMMAND [ARG ...]\n  agentlab run --accepted ACCEPTANCE_ID [OPTIONS] -- COMMAND [ARG ...]\n  agentlab list [--json]\n  agentlab inspect [--json] [--verify] [--verbose] SNAPSHOT_RUN_OR_ACCEPTANCE\n  agentlab diff [--agent | --no-agent | --inventory] [--harness NAME] [--file PATH] [--raw] [--json] RUN\n  agentlab compare [--json] LEFT_RUN RIGHT_RUN\n  agentlab evaluate [--name NAME] [--timeout SECONDS] [--json] RUN... -- COMMAND [ARG ...]\n  agentlab report [--evaluator NAME] [--score KEY]... [--json] RUN...\n  agentlab review [--json] [--timeout SECONDS] RUN --workspace CURRENT -- COMMAND [ARG ...]\n  agentlab apply [--json] [--acknowledge-conflicts] [--acknowledge-unresolved] REVIEW_ID --workspace CURRENT\n  agentlab accept [--json] RUN [--from-apply APPLY_ID]\n  agentlab stop [--json] RUN\n  agentlab resume [--json] [--pi-auth] [--secret-file NAME=HOST_PATH]... RUN [-- COMMAND [ARG ...]]\n  agentlab fork [--json] RUN\n  agentlab rm [--json] RUN\n\nCommands:\n  snapshot    capture every supported workspace path into an immutable snapshot\n  run         execute once from a captured, stored, or explicitly accepted input\n  list        list locally recorded runs and live container state\n  inspect     inspect and verify snapshots, runs, accepted inputs, and their lineage\n  diff        show deterministic per-file changes with optional agent curation\n  compare     report equality and differences across actual resolved run inputs\n  evaluate    invoke an arbitrary external evaluator for one or more results\n  report      align real run-input identities and evaluator scores without interpreting them\n  review      obtain a validated proposal from a trusted host command without applying it\n  apply       apply exactly one review's authorized workspace operations with a backup\n  accept      explicitly accept the exact workspace and OCI image input tested by a run\n  stop        stop the stable retained-container process\n  resume      restart the container and optionally execute a credentialed continuation\n  fork        create a private filesystem-level fork\n  rm          delete one unreferenced run's container, image tag, and local artifacts\n\nRun `agentlab COMMAND --help` for command-specific usage. Workspace capture includes every supported path by default. Use --respect-gitignore only when exclusions are deliberate. Diff presentation ignores and a trusted host harness may be configured only in ~/.agentlab/config.toml; they never alter evidence, and --raw always shows every captured machine change without AI. Review gives a trusted host command sensitive copies and applies nothing; apply is the separate mutating authorization. Accept records explicit tested lineage without promoting retest session output. Filesystem state survives stop/resume, but process trees and live memory do not. Evaluator scores and exit status are observations, not universal judgments."
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{run, shell_word};

    #[test]
    fn follow_up_workspace_paths_are_shell_safe() {
        assert_eq!(shell_word("/tmp/plain-workspace"), "/tmp/plain-workspace");
        assert_eq!(
            shell_word("/tmp/Chris's workspace"),
            "'/tmp/Chris'\"'\"'s workspace'"
        );
        assert_eq!(shell_word("/tmp/first\nsecond"), "$'/tmp/first\\x0asecond'");
    }

    #[test]
    fn diff_help_exposes_the_small_presentation_interface() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = run(
            vec!["diff".to_owned(), "--help".to_owned()],
            &mut stdout,
            &mut stderr,
        );
        let stdout = String::from_utf8(stdout).unwrap();
        assert_eq!(status, 0);
        assert!(stderr.is_empty());
        assert!(stdout.contains("--agent"));
        assert!(stdout.contains("--no-agent"));
        assert!(stdout.contains("--raw"));
        assert!(!stdout.contains("--complete"));
        assert!(!stdout.contains("--important"));
    }

    #[test]
    fn retired_diff_flags_have_actionable_migration_errors() {
        for (flag, expected) in [
            ("--complete", "deterministic per-file diff is the baseline"),
            ("--important", "was renamed to --agent"),
        ] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let status = run(
                vec!["diff".to_owned(), flag.to_owned(), "run-id".to_owned()],
                &mut stdout,
                &mut stderr,
            );
            assert_eq!(status, 1);
            assert!(stdout.is_empty());
            assert!(
                String::from_utf8(stderr).unwrap().contains(expected),
                "missing migration guidance for {flag}"
            );
        }
    }
}
