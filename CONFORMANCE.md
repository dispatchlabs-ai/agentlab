# AgentLab Conformance Plan

This document defines observable tests for the durable AgentLab protocol. Tests should favor disposable fixtures and public behavior over implementation-specific assertions.

## Milestone 0: independent project

- A fresh checkout builds with the documented Rust version.
- `agentlab --version` succeeds.
- `agentlab --help` describes only implemented commands as available.
- The README states the north-star goal and a minimal working example.
- The specification contains no dependency on a particular workspace layout, harness, model, credential, or unrelated repository.

## Milestone 1: deterministic workspace snapshots

The automated fixture uses a non-Git root containing:

- hidden content;
- root and nested `.gitignore` files;
- negated patterns;
- an ignore file below an ignored directory that must not become active;
- two unrelated Git repositories;
- tracked files matching ignore patterns;
- ignored and included untracked files;
- a machine-global Git exclusion that must have no effect;
- an empty directory;
- executable and nonexecutable modes;
- a large regular file;
- a symlink; and
- no real credentials.

The test proves:

1. Included and excluded paths exactly match the contract.
2. Git repositories require no declarations.
3. Tracked files remain included when ignore rules match.
4. In-workspace Git metadata is captured.
5. Repeating a stable snapshot returns the same digest.
6. Repeated file content reuses stored blobs.
7. The source tree's paths, types, modes, contents, and symlink targets are unchanged.
8. Manifest and blob integrity verification succeeds.
9. Materialization reconstructs every manifest entry with the declared bytes, modes, and symlink targets.
10. An included FIFO or other unsupported special file produces a precise failure and is never silently omitted.
11. CLI JSON is machine-readable.
12. Default inspection exposes metadata but not file contents.

The hands-on checkpoint additionally runs the public CLI against a workspace chosen by the user, repeats the snapshot, inspects and verifies the digest, and independently confirms the source is unchanged.

## Milestone 2: isolated execution and whole-machine delta

- Materialize the workspace privately at `/workspace` without a writable source mount.
- Modify `/workspace`, a persistent home, `/etc`, `/usr`, `/opt`, and `/var` where guest permissions permit.
- Capture additions, modifications, deletions, renames or their authoritative add/delete representation, modes, symlinks, and whiteouts.
- Prove `.agentlabignore` affects only portable export, not retained guest state.
- Record lifecycle, stdout, stderr, exit status, warnings, and integrity hashes.
- Distinguish persistent root changes from pseudo-filesystems and runtime-only mounts.

## Milestone 3: isolation and repetition

- Resolve two runs to the same workspace and OCI image identities.
- Launch them concurrently with separate writable layers.
- Produce conflicting writes and prove neither run, nor the source workspace, observes the other run's changes.
- Preserve arbitrary factors exactly.

## Milestone 4: retained lifecycle

- List, inspect, stop, resume, and delete a retained run.
- Prove filesystem continuation while explicitly disclaiming live-memory continuation.
- Capture requested harness state outside `/workspace` without harness-specific core logic.
- Delete only resources owned by the exact run.

## Milestones 5–9

Later suites cover arbitrary external evaluators, reviewed three-way adoption, the baseline-to-improved-run loop, backend capability equivalence, and the public fresh-host quickstart. Their authoritative outcomes and hands-on checkpoints are maintained in `agentlab-plan.md` until their protocols are implemented.
