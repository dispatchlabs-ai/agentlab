# AgentLab

AgentLab is a thin, layout-agnostic primitive for running agentic sessions from content-addressed inputs in scientifically comparable isolation.

![AgentLab overview](docs/images/agent-experiment-lab-bench-v3.png)

The durable model is:

```text
immutable input
    → isolated execution
    → complete observation and filesystem delta
```

Milestones 1 through 4 implement deterministic workspace snapshots, isolated and comparable direct-Docker execution, and retained filesystem lifecycle. AgentLab reconstructs the snapshot in private container storage, runs one opaque command, records a portable whole-root-filesystem delta without mounting the source workspace, and can later stop, continue, fork, inspect, or remove that retained state.

## Current capabilities

- Snapshots any selected directory without prescribing its layout.
- Includes regular files, directories, hidden paths, untracked content, Git metadata, empty directories, large files, modes, and symlink targets by default.
- Uses Git itself to evaluate root and nested `.gitignore` files with wildmatch and negation semantics.
- Discovers Git repositories automatically and keeps tracked files even when an ignore rule matches them.
- Prevents machine-global Git excludes from affecting snapshot selection.
- Stores file bodies as reusable SHA-256-addressed blobs outside the source workspace.
- Produces a canonical snapshot identity and a versioned JSON manifest.
- Inspects paths, hashes, sizes, modes, symlink targets, repositories, and ignore-rule identity without printing file contents.
- Verifies manifest and blob integrity.
- Fails precisely on unsupported special files instead of silently dropping them.
- Resolves an OCI image immutably and executes one command in a uniquely named retained Docker container.
- Captures persistent changes across the complete guest root filesystem, including content, modes, types, symlink targets, and deletions.
- Records raw and `.agentlabignore`-filtered deltas, stdout, stderr, nonzero exit status, lifecycle events, Docker evidence, requested captures, and integrity hashes.
- Rejects image volumes and external writable mounts that would escape complete root-filesystem observation.
- Supports concurrent independent runs and verifies whether two results share the same workspace, resolved image, portable base, and controlled inputs.
- Compares arbitrary factor values against explicitly expected differences, including ordinary `replicate` labels, without assigning semantics to them.
- Retains a stable container supervisor so stop/start never reruns the original agent command.
- Supports integrity-checked harness continuation from the exact filesystem, while explicitly reporting that process memory was not restored.
- Creates independent filesystem-level forks and deletes only the selected run's owned container, image tag, and local artifacts.

AgentLab does not attempt to make a transactionally consistent snapshot of a workspace that is being modified concurrently. It detects changes to regular files while reading them and asks the user to retry from a stable source.

## Requirements

- Rust 1.85 or newer, including Cargo.
- Git available in `PATH` when a workspace contains `.gitignore` files or Git repositories.
- Docker Engine for `agentlab run` (Docker Desktop is sufficient on macOS).

Git is used as the ignore-semantics authority. AgentLab evaluates workspace ignore files through temporary Git metadata outside the workspace, disables machine-global and system Git configuration for that evaluation, and uses read-only Git discovery commands with optional locks disabled.

## Quick start

Build the development CLI:

```bash
cargo build --release
./target/release/agentlab --version
./target/release/agentlab --help
```

Snapshot a workspace of your choosing:

```bash
./target/release/agentlab snapshot --workspace /path/to/workspace
```

The command prints a stable digest and an inclusion/exclusion summary:

```text
Snapshot: sha256:...
Workspace: /absolute/path/to/workspace
Included paths: ...
Excluded paths: ...
Repositories discovered: ...
Logical file bytes: ...
Content blobs: ... new, ... reused
Workspace-ignore rules: sha256:...
```

Inspect and integrity-check the snapshot without printing captured contents:

```bash
./target/release/agentlab inspect --verify sha256:...
```

Repeat the snapshot without changing the source. The snapshot digest will be identical and previously stored file blobs will be reused.

Use `--json` with `snapshot` or `inspect` for machine-readable output.

Run a harmless command against a private reconstruction of a workspace:

```bash
./target/release/agentlab run \
  --workspace /path/to/workspace \
  --image ubuntu:24.04 \
  --network none \
  -- /bin/sh -c 'printf "guest only\n" > /workspace/agentlab-proof.txt'
```

The summary includes a run ID, the command's exit code, the number of portable and ignored changes, and the retained container name. Inspect and verify the result, then view its normalized delta:

```bash
./target/release/agentlab inspect --verify RUN_ID
./target/release/agentlab diff RUN_ID
./target/release/agentlab diff --raw RUN_ID
```

Use `--factor KEY=VALUE` to preserve arbitrary experimental factors, `--capture /guest/path=NAME` to export a selected path as a tar artifact, and `--change-ignore PATH` to override the workspace-root `.agentlabignore`. Network access defaults to `none`; Milestone 2 also accepts `--network bridge`.

The container remains running under an inert shell supervisor after the initial `docker exec` command completes. This makes later stop/start truthful: restarting the container starts only the supervisor and never reruns the original command.

Launch independent runs concurrently using ordinary processes, recording every experimental label as a factor. Then verify that only the intended factors differed:

```bash
./target/release/agentlab compare \
  --expect-factor variant \
  --expect-factor replicate \
  LEFT_RUN_ID RIGHT_RUN_ID
```

`compare` integrity-checks both results before reporting whether their workspace snapshot, immutable image, exported prepared base, and controlled settings agree; whether their retained container identities are distinct; the exact factor differences; and whether their portable outcomes are equal. Factors absent from one side are reported with their missing value rather than normalized away.

Manage retained filesystem state:

```bash
./target/release/agentlab list
./target/release/agentlab stop RUN_ID
./target/release/agentlab resume RUN_ID
./target/release/agentlab resume RUN_ID -- /path/to/harness --continue
./target/release/agentlab fork RUN_ID
./target/release/agentlab rm RUN_OR_FORK_ID
```

`resume RUN` restarts the same stable container when stopped. Supplying a command executes a harness-level continuation in that container, re-exports and normalizes its complete persistent root filesystem, reapplies the preserved change-ignore rules, refreshes every requested capture, and writes `agentlab.continuation/v1`. It reports `filesystem_state_reused: true` and `process_memory_restored: false`—filesystem continuation is real, but the previous process tree and live memory are gone.

`fork` commits the selected retained filesystem, exports it as the fork's portable base, and launches a separately owned stable container. Its `agentlab.fork/v1` record reports `filesystem_state_copied: true` and `process_memory_copied: false`. Forks can themselves be stopped, resumed, continued, inspected, and removed.

Lifecycle operations require runs created by a lifecycle-capable AgentLab build. `list` marks older retained containers as `legacy`, and mutating commands reject them rather than risking rerunning their original command.

## Local state

AgentLab stores manifests and blobs in a user-private directory:

```text
~/.agentlab/
├── blobs/sha256/
├── snapshots/sha256/
└── runs/RUN_ID/
    ├── spec.json
    ├── delta.json
    ├── delta.raw.json
    ├── result.json
    ├── lifecycle/
    ├── continuations/
    ├── artifacts/
    └── evidence/
```

Set `AGENTLAB_STATE_DIR` to use a different state location, which is especially useful for tests and disposable demonstrations. Generated snapshot content is never written into the selected source workspace by default.

Inspection is metadata-only by default. Captured file contents remain sensitive even though this milestone does not provide a command that prints them.

## Development

Run all tests and static checks:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo test --test milestone2 -- --ignored --nocapture
cargo test --test milestone3 -- --ignored --nocapture
cargo test --test milestone4 -- --ignored --nocapture
```

The ordinary test suite covers deterministic snapshots without requiring Docker. The explicitly invoked Milestone 2 conformance test uses `ubuntu:24.04` and a disposable workspace to prove whole-machine capture, package changes, repository commits, ignore behavior, source immutability, retained-container inspection, nonzero exit preservation, and result integrity. The Milestone 3 Docker test launches overlapping runs from an identical Alpine base, forces conflicting writes, and proves distinct writable layers, exact factor preservation, comparable inputs, different private outcomes, and an unchanged source. The Milestone 4 test proves stable stop/start identity, session continuation and refreshed capture, full-rootfs continuation evidence, filesystem fork divergence, explicit memory disclaimers, and exact deletion while an unrelated control container survives.

Lifecycle-capable images currently need `/bin/sh`, `sleep`, and `/bin/true`; typical Ubuntu, Alpine, and agent-development images satisfy this. Minimal `scratch`-style images fail explicitly because AgentLab cannot keep a stable supervisor inside them.

See [SPEC.md](SPEC.md) for the contracts, [CONFORMANCE.md](CONFORMANCE.md) for the staged test plan, and [agentlab-plan.md](agentlab-plan.md) for the complete goal-oriented roadmap.

## License

Apache License 2.0. See [LICENSE](LICENSE).
