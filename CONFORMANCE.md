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

The Docker-gated fixture uses `ubuntu:24.04`, a disposable Git workspace, and a command that installs packages, creates a repository commit, exits with status 23, and modifies persistent paths throughout the guest. It proves:

1. The workspace is materialized privately at `/workspace` without a writable source mount.
2. Workspace, persistent home, `/etc`, package-managed `/usr`, and `/var` changes are captured.
3. Additions, content modifications, deletions, rename-as-delete-plus-add, mode-only changes, type changes, and symlinks are normalized.
4. A Git commit and its object/ref changes are captured without mutating the source repository.
5. `.agentlabignore` removes exactly its selected path from the portable delta while the raw delta still reports it.
6. The source snapshot identity is unchanged after execution.
7. The nonzero exit status, lifecycle, stdout, stderr, captures, Docker evidence, observations, and integrity hashes are retained.
8. `inspect --verify` recalculates every declared run artifact and result identity.
9. A file outside `/workspace` can be copied from the retained stopped container for direct inspection.
10. Persistent root changes are distinguished from pseudo-filesystems and unobserved live process memory.

Run the Docker-gated case explicitly:

```bash
cargo test --test milestone2 -- --ignored --nocapture
```

## Milestone 3: isolation and repetition

The Docker-gated fixture launches two commands concurrently from the same disposable workspace and `alpine:3.21` image. The commands overlap for five seconds, use byte-identical argv, and derive conflicting content from their distinct container hostnames. It proves:

1. Both runs record identical workspace snapshot and resolved OCI image identities.
2. Independently prepared exports normalize to the same portable base identity.
3. Retained container IDs and writable layers are distinct.
4. Command lifecycle intervals overlap rather than merely running sequentially.
5. Each result contains exactly one private owner marker, and neither observes the other's marker.
6. Conflicting writes to the same guest path produce distinct content digests.
7. The selected source workspace remains byte-identical and contains no candidate writes.
8. Arbitrary factors, Unicode values, and replicate labels are preserved exactly.
9. Comparison succeeds when `variant` and `replicate` are the expected differences and detects an undeclared replicate difference when only `variant` is expected.
10. Both results pass complete integrity verification before comparison.

Run the case explicitly:

```bash
cargo test --test milestone3 -- --ignored --nocapture
```

## Milestone 4: retained lifecycle

- List, inspect, stop, resume, and delete a retained run.
- Prove filesystem continuation while explicitly disclaiming live-memory continuation.
- Capture requested harness state outside `/workspace` without harness-specific core logic.
- Delete only resources owned by the exact run.

## Milestones 5–9

Later suites cover arbitrary external evaluators, reviewed three-way adoption, the baseline-to-improved-run loop, backend capability equivalence, and the public fresh-host quickstart. Their authoritative outcomes and hands-on checkpoints are maintained in `agentlab-plan.md` until their protocols are implemented.
