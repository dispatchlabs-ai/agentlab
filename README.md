# AgentLab

AgentLab is a thin, layout-agnostic primitive for running agentic sessions from content-addressed inputs in scientifically comparable isolation.

![AgentLab overview](docs/images/agent-experiment-lab-bench-v3.png)

The durable model is:

```text
immutable input
    → isolated execution
    → complete observation and filesystem delta
```

Milestones 1 through 7 implement deterministic workspace snapshots, isolated and comparable direct-Docker execution, retained filesystem lifecycle, external evaluation, optional three-state review, explicit receipt-bound application, and accepted-input lineage. AgentLab reconstructs the snapshot in private container storage, runs one opaque command, records a portable whole-root-filesystem delta without mounting the source workspace, can later stop, continue, fork, inspect, or remove unreferenced state, lets arbitrary external commands attach structured observations without turning any score into a universal judgment, asks a trusted command-line reviewer which changes are worth carrying forward, applies only authorized workspace paths, and can explicitly accept the exact workspace/image input proven by a retest.

## Current capabilities

- Snapshots any selected directory without prescribing its layout.
- Includes regular files, directories, hidden and ignored paths, untracked content, Git metadata, empty directories, large files, modes, and symlink targets by default.
- Excludes nothing based on `.gitignore` unless the user deliberately selects `--respect-gitignore`; explicit filtering uses Git's root and nested wildmatch and negation semantics.
- Discovers Git repositories automatically and keeps tracked files even when an ignore rule matches them.
- Prevents machine-global Git excludes from affecting snapshot selection.
- Stores file bodies as reusable SHA-256-addressed blobs outside the source workspace.
- Produces a canonical snapshot identity and a versioned JSON manifest.
- Inspects paths, hashes, sizes, modes, symlink targets, repositories, and ignore-rule identity without printing file contents.
- Verifies manifest and blob integrity.
- Fails precisely on unsupported special files instead of silently dropping them.
- Resolves an OCI image immutably and executes one command in a uniquely named retained Docker container.
- Optionally injects the host's default Pi authentication file from private runtime memory, records only the injection name, and removes it before filesystem export.
- Captures persistent changes across the complete guest root filesystem, including content, modes, types, symlink targets, and deletions.
- Records raw and `.agentlabignore`-filtered deltas, stdout, stderr, nonzero exit status, lifecycle events, Docker evidence, requested captures, and integrity hashes.
- Rejects image volumes and external writable mounts that would escape complete root-filesystem observation.
- Runs directly from either a mutable host directory (snapshotted at invocation) or an already stored snapshot digest.
- Shows elapsed-time progress, streams guest stdout/stderr live, and recaptures a direct source workspace after execution to report whether it remained unchanged.
- Derives a canonical run-input identity from the actual snapshot, resolved image, command, resource/network policy, captures, ignore rules, backend, and AgentLab version.
- Supports concurrent independent runs and verifies exact repetitions or reports which real controlled inputs differ.
- Retains a stable container supervisor so stop/start never reruns the original agent command.
- Supports integrity-checked harness continuation from the exact filesystem, while explicitly reporting that process memory was not restored.
- Creates independent filesystem-level forks and deletes only the selected run's owned container, image tag, and local artifacts.
- Invokes arbitrary external evaluators against integrity-checked results and records their command, output, status, stdout/stderr, and named JSON observations.
- Aligns actual run-input, workspace, image, portable-base identities, and evaluator score names into Markdown or JSON rows without aggregation, ranking, statistics, or causal claims.
- Constructs immutable base/candidate/current review bundles, exposes the original command output, structured evaluator observations, and actual changed-machine content to any trusted command-line reviewer, validates complete proposed/rejected/conflicted/unresolved dispositions and declarative environment recommendations, and records a proposal without applying it.
- Shows elapsed review progress, retains every reviewer invocation as integrity-checked evidence, permits one constrained schema-correction attempt, and keeps rejected output inspectable without treating it as an actionable proposal.
- Applies one review receipt only when the host workspace still exactly matches the reviewed current snapshot, requires explicit acknowledgement of conflicts or unresolved candidates, privately stages the result, retains a complete before snapshot, changes only proposed workspace operations, rolls back path-scoped failures, and verifies the exact after snapshot.
- Records an explicit `agentlab.acceptance/v1` decision for the exact workspace snapshot, immutable OCI image, platform, and guest workspace path tested by a completed run; exit status remains evidence rather than an automatic verdict.
- Runs directly from an accepted input without repeating workspace/image flags, preserves parent review/apply/retest lineage, excludes test-session output from the next base, and protects referenced evidence from ordinary removal.

AgentLab does not attempt to make a transactionally consistent snapshot of a workspace that is being modified concurrently. It detects changes to regular files while reading them and asks the user to retry from a stable source.

## Requirements

- Rust 1.85 or newer, including Cargo.
- Git available in `PATH` when a workspace contains `.gitignore` files or Git repositories.
- Docker Engine for `agentlab run` (Docker Desktop is sufficient on macOS).

Git is used as the repository-discovery and explicit ignore-semantics authority. When `--respect-gitignore` is selected, AgentLab evaluates workspace ignore files through temporary Git metadata outside the workspace and disables machine-global and system Git configuration. Repository discovery uses read-only Git commands with optional locks disabled.

## Quick start

Build the development CLI:

```bash
cargo build --release
./target/release/agentlab --version
./target/release/agentlab --help
```

For a repeatable Linux development install that builds through rootless Docker, installs under `~/.local/bin`, and embeds the exact Git commit, see [Dogfooding development builds](docs/DOGFOODING.md).

Snapshot a workspace of your choosing:

```bash
./target/release/agentlab snapshot --workspace /path/to/workspace
```

Complete capture is the default. The command prints a stable digest and an inclusion/exclusion summary:

```text
Snapshot: sha256:...
Workspace: /absolute/path/to/workspace
Capture: all
Included paths: ...
Excluded paths: 0
Repositories discovered: ...
Logical file bytes: ...
Content blobs: ... new, ... reused
Workspace-ignore rules: sha256:...
```

Use `--respect-gitignore` only when those exclusions are a deliberate experimental input. The earlier `--capture all` spelling remains accepted as a compatibility alias.

Inspect and integrity-check the snapshot without printing captured contents:

```bash
./target/release/agentlab inspect --verify sha256:...
```

Repeat the snapshot without changing the source. The snapshot digest will be identical and previously stored file blobs will be reused.

Snapshot inspection is concise by default. Add `--verbose` to list every discovered repository and captured path, or use `--json` with `snapshot` or `inspect` for the complete machine-readable record.

Run a harmless command against a private reconstruction of the snapshot you just inspected:

```bash
SNAPSHOT=$(./target/release/agentlab snapshot \
  --workspace /path/to/workspace --json | jq -r .digest)

./target/release/agentlab run \
  --snapshot "$SNAPSHOT" \
  --image ubuntu:24.04 \
  --network none \
  -- /bin/sh -c 'printf "guest only\n" > /workspace/agentlab-proof.txt'
```

`run --workspace /path/to/workspace` is the first-run form: it captures every supported workspace path, runs that exact result, streams guest output, and recaptures the source afterward to report whether it remained unchanged. Use `--respect-gitignore` only for a deliberate filtered input. Use `--snapshot DIGEST` when exact repetition matters. Progress and live guest output use stderr when `--json` reserves stdout for the final machine-readable summary. The human summary includes a run ID, canonical run-input digest, command exit code, portable and ignored change counts, source status, retained container, and ready-to-paste follow-up commands.

```bash
./target/release/agentlab inspect --verify RUN_ID
./target/release/agentlab diff RUN_ID
./target/release/agentlab diff --complete RUN_ID
./target/release/agentlab diff --file /workspace/path/to/file RUN_ID
./target/release/agentlab diff --raw RUN_ID
```

AgentLab creates a content-addressed record for every selected changed path.
Text files receive ordinary unified patches; binary files, directories,
symlinks, permissions, and unavailable historical content receive explicit
metadata records rather than misleading text. Without configuration,
`agentlab diff RUN_ID` shows the complete deterministic per-file view.
`--raw` alone preserves the lower-level path inventory for every machine
change, while `--complete --raw` renders content for that unfiltered set.
`--json` without a presentation flag preserves the original deterministic
delta-manifest contract for scripts.

An optional trusted host harness can reduce the complete diff to the parts a
human needs to see. Configuration is global and private, never loaded from the
workspace under test:

```toml
# ~/.agentlab/config.toml
version = 1
default_harness = "pi"

[harnesses.pi]
command = [
  "pi",
  "--no-tools",
  "--no-session",
  "--no-context-files",
  "--no-skills",
  "--no-extensions",
  "--no-prompt-templates",
  "-p",
]
input = "stdin"
timeout_seconds = 600

[diff]
presentation = "important"
show_omitted_count = true
fallback = "complete"
```

The named command receives one complete review request on standard input and
runs from a private temporary directory. Add model, provider, or thinking
flags directly to its argument array when desired. The presenter is an
observer: AgentLab re-verifies the run-result identity, selected delta,
per-file bundle, and prior presentation receipts; records the exact request,
command, output, timing, status, and input digest; and applies nothing. Use
`agentlab inspect --verify RUN_ID` for a full byte-level audit of every large
run artifact. A missing, failed, timed-out, empty, or non-UTF-8 presentation falls
back to the complete deterministic view. CLI choices override the config:

```bash
agentlab diff --important RUN_ID
agentlab diff --important --harness ANOTHER_NAME RUN_ID
agentlab diff --complete RUN_ID
agentlab diff --inventory RUN_ID
```

Enabling agent presentation may send captured file content to the configured
model provider. The original command can copy a runtime credential into a new
file, and such a copied file is ordinary captured evidence; use this feature
only with a trusted harness and provider. The full per-file bundle remains the
authority regardless of what the presenter omits.

Use `--capture /guest/path=NAME` to export a selected path as a tar artifact and `--change-ignore PATH` to override the snapshotted workspace-root `.agentlabignore`. Network access defaults to Docker `bridge` mode so model-backed harnesses work without another flag. Use `--network none` when the run must be offline.

Use `--pi-auth` when the command needs the invoking host's default `~/.pi/agent/auth.json`:

```bash
./target/release/agentlab run \
  --workspace /path/to/workspace \
  --image IMAGE \
  --pi-auth \
  -- pi --help
```

AgentLab validates the JSON, places it in a private tmpfs, links it at the container user's Pi auth path only while the selected command runs, and removes both paths before inspection and root-filesystem export. The run or continuation record stores only the stable name `pi-auth`, never the host path, credential bytes, or a credential digest. This prevents AgentLab itself from persisting the injected file; the opaque command remains trusted with the credential and can still print or copy it.

Use repeatable `--secret-file NAME=HOST_PATH` options for any other regular host
files a command needs. A file named `NAME` exists only at
`/run/agentlab-secrets/NAME` while that command runs:

```bash
./target/release/agentlab run \
  --workspace /path/to/workspace \
  --image IMAGE \
  --secret-file aws-credentials=/path/to/least-privilege-session.credentials \
  -- env \
    AWS_SHARED_CREDENTIALS_FILE=/run/agentlab-secrets/aws-credentials \
    AWS_CONFIG_FILE=/dev/null \
    AWS_DEFAULT_REGION=us-east-1 \
    aws sts get-caller-identity
```

Names use only letters, digits, `.`, `_`, and `-`. All injected files share a
1 MiB in-memory limit, are readable by the container command user, and are
removed before inspection, capture, or root-filesystem export. Records include
only the stable names—not source paths, bytes, or credential digests. Injecting
a file prevents AgentLab itself from persisting it; the opaque command is still
trusted with the credential and can print or copy it. Use least-privilege,
noninteractive credentials appropriate to the experiment.

The container remains running under an inert shell supervisor after the initial `docker exec` command completes. This makes later stop/start truthful: restarting the container starts only the supervisor and never reruns the original command.

Launch independent runs concurrently using ordinary processes. Reuse one snapshot digest for repetitions; create a new snapshot only after making a real treatment change to the host workspace:

```bash
# Host workspace without the skill
A=$(./target/release/agentlab snapshot --workspace /path/to/workspace --json | jq -r .digest)

# Make the real change in the host workspace, then freeze that state too
mkdir -p /path/to/workspace/skills/review
cp ./SKILL.md /path/to/workspace/skills/review/SKILL.md
B=$(./target/release/agentlab snapshot --workspace /path/to/workspace --json | jq -r .digest)

./target/release/agentlab run --snapshot "$A" --image IMAGE -- HARNESS TASK  # A1
./target/release/agentlab run --snapshot "$A" --image IMAGE -- HARNESS TASK  # A2
./target/release/agentlab run --snapshot "$B" --image IMAGE -- HARNESS TASK  # B1
./target/release/agentlab run --snapshot "$B" --image IMAGE -- HARNESS TASK  # B2

./target/release/agentlab compare RUN_A1 RUN_A2
./target/release/agentlab compare RUN_A1 RUN_B1
```

`compare` integrity-checks both results before reporting whether their complete run-input identity, workspace snapshot, immutable image, exported prepared base, and controlled settings agree; whether their retained container identities are distinct; which actual controlled fields differ; and whether their portable outcomes are equal. The first comparison above is a candidate `comparable_repetition`; the second is a `different_inputs` comparison whose workspace difference is factual rather than declared.

The host workspace is the primary mutable thing under development. Stored snapshots are immutable test inputs. An accepted baseline is a small reference to tested or reviewed snapshot/image/result lineage—not a second “golden” host checkout.

If the treatment must change something outside the workspace, prepare that state with the isolation backend and give AgentLab its immutable identity. With Docker today, enter a disposable container, make the change, commit it as a new image, and pass that image tag to `agentlab run`; AgentLab resolves and records the content digest. VM backends can follow the same model with a VM snapshot. Environment construction is deliberately outside the current primitive.

Manage retained filesystem state:

```bash
./target/release/agentlab list
./target/release/agentlab stop RUN_ID
./target/release/agentlab resume RUN_ID
./target/release/agentlab resume RUN_ID -- /path/to/harness --continue
./target/release/agentlab resume --pi-auth RUN_ID -- pi --continue
./target/release/agentlab resume --secret-file credential="$HOME/credential" RUN_ID -- /path/to/harness --continue
./target/release/agentlab fork RUN_ID
./target/release/agentlab rm RUN_OR_FORK_ID
```

`resume RUN` restarts the same stable container when stopped. Supplying a command executes a harness-level continuation in that container, re-exports and normalizes its complete persistent root filesystem, reapplies the preserved change-ignore rules, refreshes every requested capture, and writes `agentlab.continuation/v1`. Add `--pi-auth` or repeatable `--secret-file NAME=HOST_PATH` options before the run ID when that continuation needs current host credentials. AgentLab injects them only for that command, cleans them before capture, and records only their stable names in the continuation. Newly created runs and forks reserve the private in-memory credential mount even when their initial command does not need it; older retained runs without that mount are rejected rather than receiving a credential through persistent storage. Resume reports `filesystem_state_reused: true` and `process_memory_restored: false`—filesystem continuation is real, but the previous process tree and live memory are gone.

`fork` commits the selected retained filesystem, exports it as the fork's portable base, and launches a separately owned stable container. Its `agentlab.fork/v1` record reports `filesystem_state_copied: true` and `process_memory_copied: false`. Forks can themselves be stopped, resumed, continued, inspected, and removed.

Lifecycle operations require runs created by a lifecycle-capable AgentLab build. `list` marks older retained containers as `legacy`, and mutating commands reject them rather than risking rerunning their original command.

Evaluate one or more completed results with an external command:

```bash
./target/release/agentlab evaluate \
  --name result-facts \
  RUN_A1 RUN_A2 RUN_B1 RUN_B2 \
  -- ./examples/evaluators/result-facts.sh
```

AgentLab verifies every immutable run, lifecycle, and prior-evaluation artifact before and after invoking the command. The evaluator inherits the caller's working directory and receives absolute input paths through:

```text
AGENTLAB_RUN_ID
AGENTLAB_RUN_DIR
AGENTLAB_RESULT_PATH
AGENTLAB_SPEC_PATH
AGENTLAB_DELTA_PATH
AGENTLAB_RAW_DELTA_PATH
```

Successful evaluator stdout must be one JSON object:

```json
{
  "scores": {"correctness": 0.9, "tests_passed": true},
  "observations": {"test_suite": "unit"},
  "summary": "optional human-readable summary"
}
```

Score values must be JSON scalars; observations and extension fields may contain arbitrary JSON. Nonzero commands and malformed output are retained as `command_failed` or `invalid_output` records and are never silently treated as successful scores.

Produce a row table from the latest successful matching evaluation for each run:

```bash
./target/release/agentlab report \
  --evaluator result-facts \
  --score exit_zero --score portable_changes \
  RUN_A1 RUN_A2 RUN_B1 RUN_B2
```

The report identifies each row by its real run-input, workspace, resolved-image, and portable-base digests. Use `--json` for machine-readable output. Reporting performs no averaging or statistical interpretation. Agent and external-service behavior may remain nondeterministic even with identical starting inputs, so meaningful experiments should use repeated runs from the same snapshot and interpret variance externally. See [examples/experiments.md](examples/experiments.md).

`agentlab evaluate` runs the selected command directly on the host with the current user's permissions; it is not an evaluator sandbox. Run only evaluator commands you trust. Post-execution integrity verification detects changes to AgentLab's immutable inputs, but it cannot prevent a malicious command from affecting other host resources.

Review a run against the workspace as it exists now, without applying anything:

```bash
agentlab review \
  RUN_ID \
  --workspace /path/to/current-workspace \
  -- ./examples/reviewers/pi-review.sh
```

Run `agentlab review --help` for the complete command contract. A relative reviewer executable such as `./examples/reviewers/pi-review.sh` is resolved from the directory where you invoke AgentLab before the reviewer starts from the private current-workspace copy.

The selected reviewer runs directly on the host with your authority. AgentLab creates private temporary materializations of the immutable base workspace, the candidate workspace captured from the run, and a fresh snapshot of the current host workspace. It also provides the exact run specification and result, exact command stdout and stderr, all structured evaluator records, root-filesystem manifests, portable and raw deltas, and a changed-machine tree containing actual after-content for captured changes outside the workspace. The command runs from the temporary current copy so ordinary `AGENTS.md` and `CLAUDE.md` discovery works without placing the reviewer in the mutable source.

The reviewer receives these environment variables:

```text
AGENTLAB_RUN_ID
AGENTLAB_REVIEW_ID
AGENTLAB_REVIEW_BUNDLE_DIR
AGENTLAB_REVIEW_REQUEST_PATH
AGENTLAB_REVIEW_RUN_SPEC_PATH
AGENTLAB_REVIEW_RUN_RESULT_PATH
AGENTLAB_REVIEW_RUN_STDOUT_PATH
AGENTLAB_REVIEW_RUN_STDERR_PATH
AGENTLAB_REVIEW_EVALUATIONS_PATH
AGENTLAB_REVIEW_BASE_ROOTFS_MANIFEST_PATH
AGENTLAB_REVIEW_CANDIDATE_ROOTFS_MANIFEST_PATH
AGENTLAB_REVIEW_BASE_MANIFEST_PATH
AGENTLAB_REVIEW_CANDIDATE_MANIFEST_PATH
AGENTLAB_REVIEW_CURRENT_MANIFEST_PATH
AGENTLAB_REVIEW_DELTA_PATH
AGENTLAB_REVIEW_RAW_DELTA_PATH
AGENTLAB_REVIEW_BASE_DIR
AGENTLAB_REVIEW_CANDIDATE_DIR
AGENTLAB_REVIEW_CURRENT_DIR
AGENTLAB_REVIEW_MACHINE_CHANGES_DIR
```

Successful stdout must be one `agentlab.review-proposal/v1` JSON object. It must copy the request's review ID and anchors exactly, classify every raw-delta candidate exactly once as `proposed`, `rejected`, `conflicted`, or `unresolved`, provide reconciled counts and reasons, keep workspace operations relative and in scope, and express worthwhile environment changes as declarative recommendations rather than copies from `/etc`, `/usr`, `/var`, or other machine paths. Missing capabilities discovered from the command answer—such as a required package or runtime-only credential—can be returned in the proposal's optional `recommendations` array with `target: "environment"`, a concrete recommendation, and a reason. AgentLab rejects duplicate, missing, extra, traversing, incorrectly anchored, or inconsistently counted output.

AgentLab emits elapsed-time stages and a heartbeat every 15 seconds while a reviewer is running. If a successful reviewer process emits JSON that fails the proposal contract, AgentLab invokes the same adapter once more with `AGENTLAB_REVIEW_REPAIR=1`, `AGENTLAB_REVIEW_PREVIOUS_STDOUT_PATH`, and `AGENTLAB_REVIEW_VALIDATION_ERROR_PATH`. An adapter may use those inputs to make one constrained correction; there is no open-ended retry loop. Every invocation's stdout, stderr, exit status, and validation error is retained in an immutable `agentlab.review-attempt/v1` record whether the final proposal is accepted or rejected.

A rejected review prints its review ID, an integrity-check command, and the private path to the final raw reviewer output:

```bash
agentlab inspect --verify REVIEW_ID
agentlab inspect --verbose REVIEW_ID
```

The normal inspection shows status, timing, failure, and invocation outcomes. `--verbose` adds the retained stdout and stderr paths without printing potentially sensitive captured content.

[examples/reviewers/pi-review.sh](examples/reviewers/pi-review.sh) is a thin Pi adapter using noninteractive, no-session, read-only built-in tools. It explicitly reads the original prompt-bearing run specification, exact agent answer and errors, evaluator observations, filesystem changes, and current workspace. It uses the host's ordinary Pi authentication. Set `AGENTLAB_PI_MODEL` or `AGENTLAB_PI_THINKING` to choose a model or thinking level. The core protocol remains harness-neutral; replace that script with any command that emits the required JSON.

Review mode means AgentLab itself applies no changes. It re-snapshots the selected source after the reviewer and accepts a receipt only when the source identity is unchanged, but an arbitrary host command is still trusted and cannot be sandboxed by this promise.

Apply one accepted review explicitly:

```bash
agentlab apply \
  REVIEW_ID \
  --workspace /path/to/current-workspace
```

The selected path must resolve to the same host workspace recorded by the review, and its contents must still match the exact current snapshot captured there. By default, apply stops if the proposal contains any `conflicted` or `unresolved` candidate. A deliberate partial application acknowledges those untouched candidates explicitly:

```bash
agentlab apply \
  REVIEW_ID \
  --workspace /path/to/current-workspace \
  --acknowledge-conflicts \
  --acknowledge-unresolved
```

Those flags do not apply conflicted or unresolved paths; they authorize AgentLab to continue with only the proposal's `proposed` workspace operations. Rejected paths and all environment paths remain untouched. AgentLab permits one accepted apply per review, privately stages the exact intended result, takes a complete content-addressed before snapshot, serializes concurrent attempts, applies only exact relative paths without recursively deleting unreviewed directory content, verifies the resulting snapshot, and records `agentlab.apply/v1`. If a path-scoped operation fails, AgentLab attempts to restore every authorized path from the before snapshot. An interrupted apply leaves an exclusive lock and backup evidence so a later invocation does not guess whether it is safe to continue.

The current human output deliberately emphasizes disposition, reason, operation, path, and exact snapshot identity. A polished terminal diff remains planned: it should offer an excellent unified or side-by-side view over immutable base/candidate/current and before/after content, with color and binary/type/mode handling. That renderer will be a read-only view; review receipts—not presentation output—remain the authority for apply.

Accept the exact input tested by a completed run:

```bash
agentlab accept INITIAL_TEST_RUN
```

This accepts the run's starting workspace and OCI image—not its result filesystem. It is the small bootstrap form for a known tested input. The human output prints an acceptance ID and the next ready-to-paste shape:

```bash
agentlab run --accepted ACCEPTANCE_ID -- HARNESS TASK
```

`--accepted` supplies and verifies the immutable workspace snapshot, OCI image reference and digest, platform, ignore identity, and guest workspace path. The new command, network policy, limits, captures, and credentials remain ordinary explicit run inputs. A run specification stores the acceptance ID, record digest, and content-based accepted-input digest as provenance; those fields do not replace the canonical run-input identity derived from the actual controlled content.

After a reviewed apply, use the apply summary's exact retest command, then connect the retest to the application:

```bash
agentlab run \
  --snapshot AFTER_WORKSPACE_DIGEST \
  --image IMMUTABLE_IMAGE_REFERENCE \
  -- TEST_COMMAND

agentlab accept RETEST_RUN --from-apply APPLY_ID
```

The second command requires the retest to be a distinct run, its starting workspace to equal the apply receipt's exact after snapshot, and its resolved OCI image, platform, and guest workspace path to equal the candidate run. The resulting `reviewed_application` acceptance links the prior accepted input when present, candidate run/result, review, apply, retest run/result, and new content-addressed input. AgentLab records a nonzero retest exit code but does not silently reject or approve it; acceptance is the explicit decision.

Running from the new acceptance reconstructs the applied workspace snapshot. It does not copy the retest result filesystem, so test logs, caches, and other session changes cannot enter the next base merely because the retest created them. Rejected, conflicted, unresolved, and environment changes remain unapplied; an ignored change is likewise absent unless the review explicitly authorized its workspace path, because ignore rules affect observation views rather than authorization. `agentlab inspect --verify ACCEPTANCE_ID` walks and verifies the complete lineage. Ordinary `agentlab rm` refuses to delete a candidate or test run referenced by an acceptance.

## Local state

AgentLab stores manifests and blobs in a user-private directory:

```text
~/.agentlab/
├── config.toml
├── blobs/sha256/
├── snapshots/sha256/
├── acceptances/ACCEPTANCE_ID.json
└── runs/RUN_ID/
    ├── spec.json
    ├── delta.json
    ├── delta.raw.json
    ├── result.json
    ├── diffs/
    │   ├── file-diffs.json
    │   └── file-diffs.raw.json
    ├── diff-presentations/PRESENTATION_ID/
    ├── lifecycle/
    ├── continuations/
    ├── evaluations/
    ├── reviews/
    ├── artifacts/
    └── evidence/
```

Set `AGENTLAB_STATE_DIR` to use a different state location, which is especially useful for tests and disposable demonstrations. Generated snapshot content is never written into the selected source workspace by default.

Inspection is metadata-only by default. Complete diffs deliberately print
captured text changes, and configured diff presenters receive those changes;
captured file contents therefore remain sensitive.

## Development

Run all tests and static checks:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo test --test milestone2 -- --ignored --nocapture
cargo test --test milestone3 -- --ignored --nocapture
cargo test --test milestone4 -- --ignored --nocapture
cargo test --test milestone5 -- --ignored --nocapture
cargo test --test milestone6 -- --ignored --nocapture
cargo test --test milestone7 -- --ignored --nocapture
```

The ordinary test suite covers deterministic snapshots, proposal validation, exact path application, unreviewed-directory protection, rollback, accepted-input identity, and public command help without requiring Docker. The explicitly invoked Milestone 2 conformance test uses `ubuntu:24.04` and a disposable workspace to prove whole-machine capture, package changes, repository commits, ignore behavior, source immutability, retained-container inspection, nonzero exit preservation, and result integrity. The Milestone 3 Docker test launches overlapping runs from one stored Alpine-backed snapshot, forces conflicting writes, and proves distinct writable layers, equal run-input identities, comparable repetition, different private outcomes, and an unchanged source. The Milestone 4 test proves stable stop/start identity, session continuation and refreshed capture, full-rootfs continuation evidence, filesystem fork divergence, explicit memory disclaimers, and exact deletion while an unrelated control container survives. The Milestone 5 test creates real workspace snapshots without and with a skill directory, repeats each immutable input twice, runs a supplied external evaluator, verifies every evaluation, records invalid output explicitly, produces identity-and-score tables, and preserves source immutability. The Milestone 6 test advances a current Git workspace independently, validates review-only behavior and all four dispositions, blocks unacknowledged conflicts and unresolved paths, rejects stale current state, applies only one authorized workspace file, proves rejected/conflicted/environment paths stayed untouched, materializes the complete backup, detects backup tampering, verifies the apply receipt, and rejects repeated application. The Milestone 7 test bootstraps a tested accepted input, launches two independent runs from it, reviews and applies one candidate, rejects an incorrect retest, accepts the exact successful retest with complete ancestry, launches the improved run from that reference, proves rejected and ignored session changes stayed out of its base, verifies the full lineage, and prevents removal of referenced evidence.

Lifecycle-capable images currently need `/bin/sh`, `sleep`, and `/bin/true`; typical Ubuntu, Alpine, and agent-development images satisfy this. Minimal `scratch`-style images fail explicitly because AgentLab cannot keep a stable supervisor inside them.

See [SPEC.md](SPEC.md) for the contracts, [CONFORMANCE.md](CONFORMANCE.md) for the staged test plan, and [agentlab-plan.md](agentlab-plan.md) for the complete goal-oriented roadmap.

## License

Apache License 2.0. See [LICENSE](LICENSE).
