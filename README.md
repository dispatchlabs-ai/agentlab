# AgentLab

AgentLab is a thin, layout-agnostic primitive for running agentic sessions from content-addressed inputs in scientifically comparable isolation.

![AgentLab overview](docs/images/agent-experiment-lab-bench-v3.png)

The durable model is:

```text
immutable input
    → isolated execution
    → complete observation and filesystem delta
```

Milestone 1 implements the first independently useful part of that model: deterministic, content-addressed workspace snapshots. Isolated OCI execution and whole-machine change capture begin in Milestone 2 and are intentionally not represented as working yet.

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

AgentLab does not attempt to make a transactionally consistent snapshot of a workspace that is being modified concurrently. It detects changes to regular files while reading them and asks the user to retry from a stable source.

## Requirements

- Rust 1.85 or newer, including Cargo.
- Git available in `PATH` when a workspace contains `.gitignore` files or Git repositories.

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

## Local state

AgentLab stores manifests and blobs in a user-private directory:

```text
~/.agentlab/
├── blobs/sha256/
└── snapshots/sha256/
```

Set `AGENTLAB_STATE_DIR` to use a different state location, which is especially useful for tests and disposable demonstrations. Generated snapshot content is never written into the selected source workspace by default.

Inspection is metadata-only by default. Captured file contents remain sensitive even though this milestone does not provide a command that prints them.

## Development

Run all tests and static checks:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

The conformance tests build disposable workspaces and prove default inclusion, Git-compatible exclusion, repository discovery, tracked-file inclusion, deterministic identity, source immutability, reconstruction, content reuse, and explicit failure for unsupported special files.

See [SPEC.md](SPEC.md) for the contracts, [CONFORMANCE.md](CONFORMANCE.md) for the staged test plan, and [agentlab-plan.md](agentlab-plan.md) for the complete goal-oriented roadmap.

## License

Apache License 2.0. See [LICENSE](LICENSE).
