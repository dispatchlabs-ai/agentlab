# AgentLab Conformance Plan

This document defines observable tests for the durable AgentLab protocol. Tests should favor disposable fixtures and public behavior over implementation-specific assertions.

## Milestone 0: independent project

- A fresh checkout builds with the documented Rust version.
- `agentlab --version` succeeds.
- A development build may append its exact source build ID, and that complete version identity is recorded as a controlled run input.
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

The test proves both default complete capture and explicit Git-ignore filtering:

1. Default capture includes every supported path with zero exclusions; explicit Git-ignore mode includes and excludes exactly the paths selected by the contract.
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
13. Direct `run` and `evaluate` help succeeds without a command separator, and run help declares `bridge` as the default network policy.

The hands-on checkpoint additionally runs the public CLI against a workspace chosen by the user, repeats the snapshot, inspects and verifies the digest, and independently confirms the source is unchanged.

## Milestone 2: isolated execution and whole-machine delta

The Docker-gated fixture uses `ubuntu:24.04`, a disposable Git workspace, and a command that installs packages, creates a repository commit, exits with status 23, and modifies persistent paths throughout the guest. It proves:

1. The workspace is materialized privately at `/workspace` without a writable source mount.
2. Workspace, persistent home, `/etc`, package-managed `/usr`, and `/var` changes are captured.
3. Additions, content modifications, deletions, rename-as-delete-plus-add, mode-only changes, type changes, and symlinks are normalized.
4. A Git commit and its object/ref changes are captured without mutating the source repository.
5. `.agentlabignore` removes exactly its selected path from the portable delta while the raw delta still reports it.
6. The source snapshot identity is unchanged after execution and direct-run source verification reports `unchanged`.
7. The nonzero exit status, lifecycle, live-streamed and retained stdout/stderr, progress stages, captures, Docker evidence, observations, and integrity hashes are retained.
8. `inspect --verify` recalculates every declared run artifact and result identity.
9. A file outside `/workspace` can be copied from the retained stopped container for direct inspection.
10. Persistent root changes are distinguished from pseudo-filesystems and unobserved live process memory.
11. A fixture Pi auth JSON is readable only while the command runs, only `pi-auth` appears in the run specification, and neither the auth path nor secret tmpfs appears in the persistent raw delta.

Run the Docker-gated case explicitly:

```bash
cargo test --test milestone2 -- --ignored --nocapture
```

## Milestone 3: isolation and repetition

The Docker-gated fixture takes one immutable snapshot, then launches two commands concurrently from that exact digest and the same `alpine:3.21` image. The commands overlap for five seconds, use byte-identical argv, and derive conflicting content from their distinct container hostnames. It proves:

1. Both runs record identical workspace snapshot and resolved OCI image identities.
2. Independently prepared exports normalize to the same portable base identity.
3. Retained container IDs and writable layers are distinct.
4. Command lifecycle intervals overlap rather than merely running sequentially.
5. Each result contains exactly one private owner marker, and neither observes the other's marker.
6. Conflicting writes to the same guest path produce distinct content digests.
7. The selected source workspace remains byte-identical and contains no candidate writes.
8. Both version-two specifications contain the same recomputable run-input digest and do not write a factor map.
9. Comparison derives `comparable_repetition` without user-declared labels or an expected-difference list.
10. Both results pass complete integrity verification before comparison.

Run the case explicitly:

```bash
cargo test --test milestone3 -- --ignored --nocapture
```

## Milestone 4: retained lifecycle

The Docker-gated fixture creates session-like state under `/root`, requests it as a capture, and retains a stable Alpine container. It proves:

1. The initial opaque command executes through Docker exec, preserves a known nonzero status, and leaves the supervisor running.
2. `list` discovers the run, live Docker state, lifecycle capability, and continuation count.
3. `stop` and `resume` preserve the complete container ID and filesystem while reporting that process memory was not preserved or restored.
4. A continuation after another stop reads prior session state, receives a fixture Pi credential from the host-only source, updates session and workspace state, and preserves its own known nonzero exit.
5. `agentlab.continuation/v1` records only the stable `pi-auth` injection name, captures the complete current rootfs and deltas, Docker evidence, stdout/stderr, and an updated requested session archive; the credential paths are absent after execution and from the raw persistent delta.
6. Initial, lifecycle, and continuation integrity verification succeeds.
7. A filesystem fork's portable base equals the parent's continued result-rootfs identity.
8. The fork reads inherited session value `2`, changes its private copy to `3`, and leaves the parent value at `2`.
9. Fork records and fork continuations explicitly report that filesystem state, but not process memory, was copied or restored.
10. Deleting the fork leaves its parent and an unrelated control container untouched; deleting the parent still leaves the control container untouched.
11. Selected run directories and unique image tags are removed while the shared content store and source workspace remain intact.

Run the case explicitly:

```bash
cargo test --test milestone4 -- --ignored --nocapture
```

## Milestone 5: external evaluation

The Docker-gated fixture creates one workspace snapshot without a skill directory and one after adding that real directory, launches two exact repetitions from each snapshot, then uses the public evaluation/reporting CLI and supplied evaluator. It proves:

1. Every result is machine-readable and passes integrity verification before evaluation.
2. One arbitrary external command receives absolute result/spec/delta paths through the documented environment contract.
3. The supplied evaluator's JSON scores, observations, summary, stdout, stderr, command, timestamps, and exit status are preserved in `agentlab.evaluation/v1`.
4. Every evaluation record and artifact passes independent integrity verification.
5. The public report contains exactly four rows with actual run-input, workspace, resolved-image, portable-base, evaluator, and requested score columns in deterministic order.
6. JSON report output round-trips through the public data model.
7. Malformed evaluator stdout and an intentional exit `42` are retained as `invalid_output` and `command_failed`, never promoted to scores, and do not hide earlier successful evaluations.
8. Reports explicitly disclaim universal judgment, deterministic behavior, aggregation, statistics, ranking, and causal inference.
9. Each actual workspace/run-input identity occurs exactly twice, within-cell comparison reports comparable repetition, and cross-treatment comparison reports the workspace snapshot as the real controlled difference.
10. The source workspace remains byte-identical after its deliberate skill addition and contains no run output.

Run the case explicitly:

```bash
cargo test --test milestone5 -- --ignored --nocapture
```

## Milestone 6: review and receipt-bound apply

The Docker-gated fixture runs from an immutable Git workspace, creates three candidate workspace changes plus one `/etc` change, and then independently advances the current host workspace. Through the public CLI it invokes a deterministic reviewer command using the same adapter contract available to Pi or any other command-line harness. It proves:

1. Base, candidate, and current workspace snapshots have distinct, exact anchors and automatically discovered repository records.
2. The reviewer runs from the private current copy with applicable `AGENTS.md` instructions and receives every documented manifest, delta, workspace tree, and changed-machine path.
3. Actual after-content for the `/etc` candidate is materialized for inspection without copying it into the host workspace.
4. Every raw candidate is accounted exactly once as one proposed workspace addition, one rejected workspace change, one three-way conflict, or one unresolved environment change.
5. Review ID, result/input, workspace, filesystem, and delta anchors plus all nine input-artifact byte digests are immutable.
6. Duplicate, omitted, traversing, incorrectly anchored, and inconsistently counted proposals fail semantic validation.
7. The canonical request, proposal, exact reviewer stdout/stderr, receipt identity, and all artifacts pass independent verification and `agentlab inspect --verify`.
8. The current source snapshot is identical before and after review; current-only work remains, the proposed file is absent, the rejected file is unchanged, and AgentLab explicitly records that it applied nothing.
9. Apply without `--acknowledge-conflicts` is rejected, and acknowledging conflicts alone still rejects the unresolved environment candidate; neither attempt changes the workspace.
10. Advancing the current workspace after review causes stale-current rejection even with both acknowledgements, and restoring the exact reviewed content restores eligibility.
11. A byte-identical copy at another path remains unauthorized because it is not the host workspace selected during review.
12. An existing per-review apply lock is treated as an active or interrupted operation and blocks mutation until explicitly recovered.
13. The successful apply changes only the one proposed workspace file. Rejected and conflicted files, independent current-only work, and the non-workspace recommendation remain untouched.
14. The apply receipt reconciles all four review dispositions with exactly one applied workspace operation, anchors equal privately intended and actual after snapshots, and verifies through `agentlab inspect --verify`.
15. The canonical complete before-workspace manifest materializes as a usable recovery copy; intentional backup tampering is detected, and restored bytes verify again.
16. A second apply using the same review receipt is rejected.
17. Unit coverage additionally proves exact replacement/deletion while preserving unauthorized paths and successful rollback when a later path would require recursively deleting unreviewed directory content.

Run the case explicitly:

```bash
cargo test --test milestone6 -- --ignored --nocapture
```

## Milestone 7: accepted input to improved input

The Docker-gated fixture executes the public accepted-input → independent candidates → review → apply → retest → acceptance → improved-run journey. It proves:

1. One completed seed run can explicitly accept its exact starting workspace/image input without promoting the seed result filesystem.
2. The accepted-input digest is derived from workspace snapshot, ignore identity, guest path, resolved OCI image, and platform, while the decision receives a separate acceptance ID and record digest.
3. Two public `run --accepted` invocations reconstruct the same actual input in distinct retained containers and qualify as a comparable repetition.
4. Both candidate specifications preserve the exact initial acceptance reference without adding labels to canonical run-input identity.
5. Candidate A creates one proposed improvement, rejected and conflicting workspace changes, ignored session debris, and an environment recommendation; review accounts for the complete raw delta.
6. Apply changes only the proposed file in the independently advanced host workspace. The rejected file remains at base content, the conflict remains current content, current-only work survives, and session/environment candidates remain absent.
7. Candidate B is rejected as a retest for the application because it began from the old accepted workspace rather than the apply after snapshot.
8. A distinct run tests the exact apply after snapshot with the candidate's immutable OCI image, platform, and guest path.
9. `accept RETEST --from-apply APPLY` creates `agentlab.acceptance/v1` with parent acceptance, candidate run/result/input, review, apply, retest run/result/input, exit status, and new accepted-input identity.
10. The improved accepted workspace materializes with the authorized improvement, base/current versions of unapplied files, and no ignored session debris.
11. Run C starts through `run --accepted` from the improved snapshot/image, verifies the expected content, and stores the new acceptance reference in `agentlab.run/v3`.
12. `inspect --verify ACCEPTANCE` recursively checks complete lineage, while ordinary `rm` refuses to delete the referenced candidate evidence.

Run the case explicitly:

```bash
cargo test --test milestone7 -- --ignored --nocapture
```

Later suites cover backend capability equivalence and the public fresh-host quickstart. Their authoritative outcomes and hands-on checkpoints remain in `agentlab-plan.md` until implemented.
