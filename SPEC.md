# AgentLab Specification

Status: Milestones 1–7 working contract
Snapshot schema: `agentlab.snapshot/v1`
Run schema: `agentlab.run/v3` (`agentlab.run/v1` and `agentlab.run/v2` read compatibility)
Run-input schema: `agentlab.run-input/v1`
Delta schema: `agentlab.delta/v1`
Per-file diff schema: `agentlab.file-diffs/v1`
Diff-selection schema: `agentlab.diff-selection/v1`
Diff-presenter-input schema: `agentlab.diff-presenter-input/v1`
Diff-presentation schema: `agentlab.diff-presentation/v2`
Result schema: `agentlab.result/v1`
Continuation schema: `agentlab.continuation/v1`
Fork schema: `agentlab.fork/v1`
Lifecycle event schema: `agentlab.lifecycle-event/v1`
Evaluation schema: `agentlab.evaluation/v1`
Review request schema: `agentlab.review-request/v1`
Review proposal schema: `agentlab.review-proposal/v1`
Review record schema: `agentlab.review/v1`
Apply record schema: `agentlab.apply/v1`
Accepted-input identity schema: `agentlab.accepted-input/v1`
Acceptance record schema: `agentlab.acceptance/v1`

## 1. Scope

AgentLab's core protocol is:

```text
immutable input
    → isolated execution
    → complete observation and filesystem delta
```

Milestone 1 defines the immutable workspace input. Milestone 2 defines one isolated direct-Docker execution and portable persistent-root-filesystem result. Milestone 3 proves independent repetition and derives comparisons from those existing records. Milestone 4 manages retained filesystem state and harness-level continuation. Milestone 5 records observations from arbitrary external evaluators. Milestone 6 records a trusted external reviewer's anchored proposal without applying it and provides a separate receipt-bound command for explicitly applying its authorized workspace operations. Milestone 7 records an explicit acceptance of the exact workspace/image input tested by a run, optionally binds it to reviewed application lineage, and launches later runs from that reference. None defines a workspace layout, repository registry, harness integration, authoritative evaluator, automatic apply process, daemon, scheduler, cloud control plane, or generalized execution-backend framework.

The selected workspace is opaque user content. Names such as `AGENTS.md`, `MEMORY.md`, `repos/`, `skills/`, and `worktrees/` have no meaning to the snapshot protocol.

## 2. Workspace snapshot contract

Given a selected directory, the snapshotter MUST:

1. Traverse every path beneath the selected root without following symbolic links.
2. Include regular files, directories, hidden paths, empty directories, Git repositories and their in-workspace metadata, untracked paths, large files, modes, and symlink targets by default.
3. Exclude no supported workspace path by default. When the user explicitly selects `--respect-gitignore`, apply root and nested `.gitignore` files using Git wildmatch, directory-relative, ordering, and negation semantics.
4. Discover ordinary Git repositories from `.git` directories or files without repository declarations.
5. In explicit Git-ignore mode, include tracked files inside discovered repositories even when an ignore rule matches them.
6. Exclude machine-global and system Git ignore configuration from snapshot selection in explicit Git-ignore mode.
7. Never follow a workspace symlink to capture content outside the selected tree. A symlink itself is captured with its target text.
8. Never write generated snapshot state into the source workspace by default.
9. Fail with the exact offending path and type when an included filesystem object is unsupported.
10. Produce the same snapshot digest when resolved content, relevant modes, symlink targets, and active ignore rules are unchanged.

If discovered Git metadata is broken or unavailable, AgentLab conservatively includes every path beneath that repository and emits a warning. It does not risk suppressing a possibly tracked path.

Concurrent source mutation is not part of the Milestone 1 consistency guarantee. AgentLab detects a regular file whose size, type, mode, or modification time changes while it is captured and fails rather than claiming a stable snapshot.

## 3. Snapshot identity

Snapshot identity is SHA-256 over a canonical JSON document containing:

- schema version;
- active ignore-rule digest and ordered rule records; and
- path-sorted snapshot entries.

It excludes:

- the source workspace's host path;
- timestamps;
- traversal order;
- state-store location;
- human labels; and
- repository observations derivable from captured paths.

The active ignore-rule identity is empty for the default complete capture. It is included when Git-ignore filtering is selected because those rules then resolve which workspace content belongs to the immutable input. Each rule record contains its workspace-relative path and content digest, not its contents.

## 4. Manifest

The version-one snapshot manifest is JSON with this logical shape:

```json
{
  "schema_version": "agentlab.snapshot/v1",
  "digest": "sha256:...",
  "ignore_rules_digest": "sha256:...",
  "ignore_rules": [
    {"path": ".gitignore", "digest": "sha256:..."}
  ],
  "repositories": [
    {"path": ".", "metadata_path": ".git", "metadata_kind": "directory"}
  ],
  "entries": [
    {"path": "README.md", "type": "file", "mode": 420, "size": 123, "digest": "sha256:..."},
    {"path": "bin", "type": "directory", "mode": 493},
    {"path": "current", "type": "symlink", "mode": 511, "link_target": "releases/v1"}
  ]
}
```

Entry paths use `/` separators and valid UTF-8, are relative to the selected root, are unique, and are sorted bytewise. Absolute paths and `..` traversal are invalid. Milestone 1 fails explicitly on a non-UTF-8 path or symlink target because JSON normalization would otherwise make lossless reconstruction impossible; a future schema may add an encoded byte-path representation.

Supported entry types in `agentlab.snapshot/v1` are:

- `file`, with size and blob digest;
- `directory`; and
- `symlink`, with the uninterpreted target string.

Modes contain portable permission bits plus set-user-ID, set-group-ID, and sticky bits when the host exposes them. Modification times, ownership, ACLs, extended attributes, hard-link relationships, sparse-file layout, and platform-specific flags are not represented in this schema. Unsupported special types such as FIFOs, sockets, and device nodes fail explicitly when included.

## 5. Content store

Regular-file bodies are stored separately from manifests by their SHA-256 digest. The local layout is an implementation detail with the following semantic contract:

- blobs are immutable;
- equal content reuses one blob;
- manifests reference blobs by algorithm-qualified digest;
- manifest verification recalculates snapshot identity and every referenced blob's digest and size;
- state directories and files are private to the current user by default; and
- the source workspace does not contain generated state unless it was independently selected as the state directory by an explicit user override.

These semantics permit a later object-store implementation without changing snapshot or run meaning.

## 6. Inspection and privacy

Default inspection reports paths, types, hashes, sizes, modes, symlink targets, discovered repository locations, and ignore-rule identity. It MUST NOT print regular-file contents.

`agentlab inspect --verify SNAPSHOT` verifies the canonical manifest identity and all referenced blob bytes before reporting success.

Snapshot artifacts may contain credentials or other sensitive content. Local-only storage and metadata-only inspection reduce accidental exposure but do not make an artifact safe to publish.

## 7. Run contract

`agentlab run` combines a workspace snapshot, immutable OCI image resolution, materialization settings, an opaque command, resource and network policy, change-ignore identity, and requested captures. `--workspace PATH` captures the directory at invocation; `--snapshot DIGEST` loads and verifies an existing immutable snapshot; `--accepted ACCEPTANCE_ID` loads the workspace, immutable image, and guest path from an explicit acceptance. These forms are mutually exclusive. The implementation MUST:

The default network policy is Docker `bridge`, making ordinary model-backed harness invocations usable without another option. `--network none` explicitly selects an offline run. The resolved policy is a controlled input and MUST be recorded in the run specification.

1. Reconstruct the workspace from its snapshot in private storage at `/workspace` by default, never through a writable source mount.
2. Establish and export a prepared base root filesystem after materialization and before command execution.
3. start a minimal stable container supervisor and execute the command exactly once through Docker exec;
4. preserve stdout, stderr, the actual exit code including nonzero values, timestamps, and lifecycle events;
5. reject image-declared volumes and any container mount outside the exported root filesystem, except AgentLab's exact runtime-secret tmpfs;
6. retain the running supervisor container for direct inspection and later filesystem continuation; and
7. state explicitly that pseudo-filesystems and live process memory are not portable persistent state.

The version-three run specification contains a canonical run-input digest, optional accepted-input provenance, snapshot digest, requested image, resolved immutable image digest, Docker image evidence, target platform, guest workspace path, argv, working directory, resource limits, network policy, capture declarations, runtime secret-injection names, workspace- and change-ignore identities, backend evidence, and AgentLab version.

Every newly created retained run and fork reserves AgentLab's exact 1 MiB in-memory runtime-secret tmpfs so a later continuation can request credentials without reconstructing the container. When `--pi-auth` is selected for an initial run or command-bearing resume, AgentLab reads the invoking host's default `~/.pi/agent/auth.json`, requires a JSON object no larger than 1 MiB, and places the bytes in that tmpfs. It links the file to the configured container user's Pi auth location only while the selected opaque command runs and removes both paths before inspect, diff, capture, or root-filesystem export. The run specification or continuation records only the stable name `pi-auth`; host paths, credential bytes, and credential-derived hashes MUST NOT be recorded. A continuation does not implicitly receive the credential: it must select `resume --pi-auth`. Retained containers that predate the exact secure mount MUST be rejected for credentialed continuation rather than receive the credential in persistent storage. This protects against persistence by AgentLab, not against the opaque command deliberately printing or copying credentials.

`agentlab.run-input/v1` is SHA-256 over canonical compact JSON containing the actual resolved snapshot and immutable image identities; target platform; materialization and working paths; argv; resource/network policy; captures; secret-injection names when supported; workspace- and change-ignore digests; backend name/version; and the complete AgentLab build version. Development installers append an exact source build ID so runs from different development commits cannot be mistaken for repetitions. It excludes run ID, timestamps, requested image alias, accepted-input provenance, backend-native image/container evidence, outcome, and descriptive labels. Version-two and version-three specifications MUST store this digest and verification MUST recompute it.

Version-one and version-two run specifications remain readable. The version-one legacy `factors` map is retained only during deserialization and is excluded from derived run-input identity, comparison, and reporting. Version-three writers MUST NOT emit it.

The Docker image and container identifiers are evidence, not AgentLab's portable run identity. A prepared Docker image ID may be nondeterministic without changing the meaning of the declared input.

## 8. Persistent root-filesystem observation

AgentLab exports both the prepared base and completed merged root filesystems. It normalizes each path to a sorted manifest containing its absolute UTF-8 path, type, relevant mode, regular-file digest and size, or symlink target. Supported normalized types are regular file, directory, and symlink; archive hard links resolve to the target regular-file identity. Unsupported persistent archive types fail explicitly.

Comparison produces one of: `added`, `modified`, `deleted`, `type_changed`, `mode_changed`, or `symlink_changed`. A rename is portable as an authoritative delete plus add. Result regular-file bodies are inserted into the content-addressed store.

Docker diff output is retained as path evidence and checked against the authoritative comparison. It does not replace the prepared/result exports: export provides merged content but not deletions, while Docker diff lacks complete portable content and metadata. Any normalized path not covered by Docker diff is recorded explicitly as evidence and a warning rather than silently discarded.

Runtime-only pseudo-filesystems are reported as nonportable. Writable host binds, named volumes, cache mounts, and image volumes are unsupported because their contents would fall outside the captured root filesystem. The sole supported run mount is AgentLab's exact private tmpfs for runtime secret injection; it is empty of the injected secret before persistent observation begins.

## 9. Delta contract

`agentlab.delta/v1` contains:

- its canonical SHA-256 identity;
- prepared-base and result-rootfs identities;
- change-ignore source and content identity;
- normalized path changes; and
- explicit ignored-change records.

`delta.raw.json` records every normalized persistent change. `delta.json` applies `.agentlabignore` from the selected workspace root, or an explicit `--change-ignore` file, using Git-compatible patterns. Ignore rules affect only portable selection: ignored paths remain observed in the raw delta and present in the retained container. They are never represented as unobserved.

The delta identity is SHA-256 over compact JSON containing every semantic field except the digest itself. Arrays are deterministically path ordered by rootfs comparison.

### 9.1 Per-file diff and presentation contract

`agentlab.file-diffs/v1` is a deterministic derivative of one selected raw or
portable delta. It records the run and delta identities, selection mode,
ignored changes, and one path-ordered record for every selected change. Every
record preserves before/after rootfs metadata and classifies content as text,
binary, metadata-only, or unavailable. Text additions, modifications, and
deletions contain ordinary unified patches. Binary and metadata changes MUST
not be rendered as invented text. Missing content from a legacy run is explicit
and does not weaken the authoritative path and rootfs metadata.

New runs preserve content-addressed before-content for every changed regular
file in addition to required result and workspace content. A per-file bundle
re-hashes every content blob it reads, has its own canonical identity, and is
retained beneath the run. The deterministic per-file presentation is the
baseline. `--no-agent` explicitly bypasses configured agent curation;
`--file PATH` selects exactly one raw-evidence record regardless of ignore
rules; `--inventory` preserves the concise path view; `--raw` renders every
captured machine change without presentation filtering, structural collapse,
or AI; and `--json` without explicit `--agent` or `--file` preserves the
delta-manifest JSON contract and never invokes a model.

`~/.agentlab/config.toml` is trusted host configuration and MUST NOT be loaded
from the tested workspace. It may define named command-argv harnesses, one
default harness, whether normal diff presentation uses that agent, and an
ordered array of Git-compatible presentation-ignore patterns. These patterns
MUST affect neither raw nor portable delta identity and MUST be applied before
any selected content is sent to a harness. AgentLab MUST also deterministically
collapse an added directory record when an added descendant already accounts
for it in normal presentation. `--raw` and `--file` bypass both behaviors.
`.agentlabignore` remains the separate portable-evidence selection mechanism.
A harness receives a filtered `agentlab.diff-presenter-input/v1` request on
standard input, starts in a private temporary directory, and returns
human-facing UTF-8 text on stdout. The request includes selected per-file
records and aggregate hidden/collapsed counts but MUST NOT include
presentation-ignore patterns, presentation-hidden paths, or their contents.
Those exact details remain in the local selection and receipt. AgentLab
supplies no captured workspace path or runtime secret file to the harness. The
process still runs with the invoking host user's authority and normal
environment; command configuration is therefore a trust decision, not a
sandbox boundary.

`agentlab.diff-selection/v1` is a deterministic projection of one source
`agentlab.file-diffs/v1` bundle. Its identity includes the source run, delta,
and per-file digests; raw/portable mode; ordered ignore patterns and their
digest; stable config-source label; exact presentation-hidden paths; exact
structurally collapsed paths; source/presented counts; presented per-file
records; and evidence-level ignored-change records. AgentLab reports hidden and
collapsed counts in normal human output. No universal presentation-ignore list
is built in.

The presentation prompt identifies every diff body as untrusted evidence and
limits the requested task to relevance-oriented display. AgentLab verifies the
run-result identity, selected delta identity, complete source per-file bundle,
deterministic filtered selection, and prior presentation records before and
after invocation. The explicit
`agentlab inspect --verify` command remains the full byte-level audit of every
large run artifact.
`agentlab.diff-presentation/v2` records the exact harness name and argv, prompt
version, raw/portable selection, run/delta/source-per-file/presented-selection
identities, config source, ordered patterns and pattern digest, exact hidden and
collapsed paths, source/presented counts, timestamps, exit status, exact
selection/request/stdout/stderr artifacts, warnings, and integrity hashes.
Version-one receipts remain verifiable. Command failure, timeout, empty output,
or non-UTF-8 output is recorded and the human command falls back to the
deterministic filtered selection. The presentation is an observation and never
changes, deletes, or supersedes its underlying evidence.

Because a tested command can deliberately copy an injected secret into
persistent storage, per-file evidence may be sensitive. Enabling an external
model presenter can transmit that evidence to its provider. Runtime-secret
cleanup prevents AgentLab from retaining the injected source file; it cannot
declare arbitrary command-created copies safe.

## 10. Result contract and integrity

`agentlab.result/v1` contains the run ID and run-spec digest, timing and lifecycle, exit code, stdout/stderr and requested capture artifacts, rootfs and delta identities, Docker evidence, observation status, warnings, and a path-to-digest integrity map. Its identity is SHA-256 over compact JSON containing those semantic fields except the result digest itself.

Run-local artifacts include the specification, normalized rootfs manifests, raw and portable deltas, complete base/result exports, stdout, stderr, requested/resolved image inspection, preparation/result container inspection and diff evidence, and requested capture archives. `agentlab inspect --verify RUN` recalculates every listed artifact digest and the result identity before reporting success. Default inspection reports metadata and paths without printing captured file content.

Run artifacts and complete rootfs exports may contain credentials or other sensitive information. A retained container and local state directory are private operational artifacts, not safe publication units.

## 11. Comparable repetition

Independent `agentlab run` invocations may execute concurrently against the same stored snapshot and image. Each MUST verify and reconstruct the snapshot independently and receive a distinct private Docker writable layer and retained container. Concurrent snapshot/content-store writes MUST preserve immutable content-addressed semantics. Reusing an explicit snapshot digest is the authoritative way to request byte-identical workspace input; independently reading a mutable host directory twice is not.

`agentlab compare LEFT RIGHT` loads and integrity-verifies both results and specifications. It reports:

- equality of complete run-input, workspace snapshot, and resolved image identities;
- equality of the exported prepared-base rootfs identity;
- distinct retained container IDs and names;
- controlled-input differences across command, workspace materialization, resource/network settings, captures, ignore identities, backend evidence, and AgentLab version;
- equality or difference of portable result-rootfs identities.

A comparison is a `comparable_repetition` only when complete run-input, workspace, resolved image, and prepared-base identities are identical; retained containers are distinct; and no controlled input differs. It is `different_inputs` when actual controlled inputs differ, and `same_inputs_not_independent` when recorded inputs match but the independence/base conditions do not. Image request aliases are not treated as controlled differences when they resolve to the same immutable image; resolved identity is authoritative.

Comparison is derived metadata rather than a new persisted experimental-cell object. Concurrent launch uses ordinary independent CLI processes in this milestone; AgentLab does not introduce a scheduler, daemon, treatment registry, preparation DSL, automatic statistical conclusion, or label registry.

## 12. Retained lifecycle

Lifecycle-capable containers carry exact AgentLab run ownership and lifecycle-version labels. Every mutating operation MUST load the local record, inspect Docker, match the complete expected container ID and run label, reject external mounts, and reject legacy containers without lifecycle semantics. A container name alone is never sufficient authorization.

The stable main process is independent of the opaque agent command. `agentlab stop RUN` stops that supervisor. `agentlab resume RUN` restarts only the supervisor; it MUST NOT rerun the original agent command. The container ID and private writable filesystem remain identical across this stop/start cycle. A lifecycle event record states that filesystem state was preserved and process memory was not restored.

`agentlab resume RUN -- COMMAND` executes a new opaque command in the retained container. `agentlab.continuation/v1` records:

- the immutable initial-result or fork-record anchor;
- exact command, timestamps, stdout, stderr, and exit code;
- stable runtime secret-injection names, when explicitly selected, without host paths, bytes, or credential-derived hashes;
- same retained container ID and current state;
- whether a restart occurred;
- `filesystem_state_reused: true` and `process_memory_restored: false`;
- complete exported result-rootfs identity and raw/portable deltas from the run or fork base;
- refreshed requested capture archives;
- Docker inspect/diff evidence, warnings, and artifact integrity hashes.

Initial runs preserve the exact change-ignore rule bytes as an integrity-checked private artifact. Continuations reapply those preserved rules rather than rereading a mutable source path. The initial `agentlab.result/v1` remains immutable; later continuations are separate immutable records.

`agentlab fork RUN` exports the selected current filesystem, commits it privately, assigns a new AgentLab run ID, and starts a distinct stable container. `agentlab.fork/v1` anchors to the parent record, identifies its exported portable base and Docker evidence, inherits materialization/resource/capture/change-ignore settings, and states `filesystem_state_copied: true` and `process_memory_copied: false`. Fork continuation deltas use that copied filesystem as their base.

`agentlab rm RUN` removes only the exact ownership-verified container, that run's unique image tag, and that run's local artifact directory. It does not delete parents, children, other AgentLab runs, unrelated Docker resources, shared content-addressed blobs, or workspace snapshots. Local run-artifact deletion is irreversible; the explicit `rm` command is the authorization boundary.

`agentlab inspect --verify RUN` verifies the initial result or fork record plus every continuation and lifecycle event. `agentlab list` derives current Docker state and marks pre-lifecycle runs as legacy rather than attempting unsafe restart.

Lifecycle-capable OCI images currently MUST provide `/bin/sh`, `sleep`, and `/bin/true` for the preparation and stable-supervisor processes. Unsupported minimal images fail rather than falling back to semantics that might rerun the agent command.

## 13. External evaluation

`agentlab evaluate [--name NAME] RUN... -- COMMAND` executes the selected host command once per run. Before and after execution AgentLab verifies the immutable initial result, lifecycle/fork/continuation records, prior evaluation records, and their referenced artifacts. Mutation of those inputs is an explicit failure.

The evaluator inherits the caller's working directory and host environment. AgentLab supplies absolute paths in `AGENTLAB_RUN_DIR`, `AGENTLAB_RESULT_PATH`, `AGENTLAB_SPEC_PATH`, `AGENTLAB_DELTA_PATH`, and `AGENTLAB_RAW_DELTA_PATH`, plus `AGENTLAB_RUN_ID`. AgentLab does not prescribe the evaluator language or interpret its domain.

Successful stdout MUST be one JSON object with optional fields:

- `scores`, an object whose nonempty keys map to JSON scalar values;
- `observations`, an object with nonempty keys and arbitrary JSON values;
- `summary`, a string; and
- arbitrary extension fields, preserved unchanged.

`agentlab.evaluation/v1` records the evaluation ID, anchored result digest, evaluator name, exact command argv, timestamps, actual exit code, status, parsed output when valid, stdout/stderr artifacts, warnings, and integrity hashes. Status is `succeeded`, `command_failed`, or `invalid_output`. A failed command or invalid envelope remains inspectable evidence but is not eligible as a successful score source.

Evaluation records are immutable additions beneath the run and do not alter `agentlab.result/v1`. `agentlab inspect --verify RUN` verifies them along with the run lifecycle. Evaluator stdout, stderr, summaries, and observations may themselves be sensitive.

`agentlab report` selects the latest successful evaluation, optionally by evaluator name, for each explicit run ID. It aligns run, run-input, workspace-snapshot, resolved-image, portable-base, evaluator, and requested or discovered scalar score identities into rows. Missing score values remain missing. JSON output is machine-readable; default output is a Markdown table.

Reporting MUST state that scores are evaluator-specific observations, model/external-service execution can be nondeterministic, multiple exact-input repetitions are advisable, and AgentLab performs no aggregation, statistical test, ranking, causal inference, or universal success judgment. Score names remain opaque strings. Rows sharing run-input and portable-base identities are only candidate repetitions until comparison also verifies distinct containers.

External evaluators run directly on the host with the invoking user's authority. This milestone does not sandbox them. Integrity checks detect mutation of AgentLab records but do not constrain other filesystem, process, credential, network, or service access; users MUST run only evaluator commands they trust.

## 14. Review proposals

`agentlab review RUN --workspace CURRENT -- COMMAND` integrity-verifies the selected immutable run, its lifecycle, evaluations, and prior reviews; loads the exact base workspace snapshot and initial result; freshly snapshots the complete current workspace; materializes private base, candidate, and current workspace trees; and invokes one trusted host reviewer command from the current copy. Review currently selects the immutable initial run result, not mutable retained-container state or a continuation. A relative reviewer executable is resolved from AgentLab's invocation directory before the reviewer starts from the private current-workspace copy.

The reviewer receives absolute environment paths for the versioned request; run specification and result; base and candidate root-filesystem manifests; base, candidate, and current workspace manifests; portable and raw deltas; materialized workspace trees; and a changed-machine tree containing after-content for every raw-delta entry that still exists. A deleted path is represented by the raw delta and has no after-content. AgentLab rechecks every manifest/delta bundle file after the command. Workspace-tree writes are disposable reviewer behavior and do not change the anchored snapshots.

`agentlab.review-request/v1` records a unique review ID; reviewer argv; exact run/result/run-input, base/candidate/current workspace, base/candidate filesystem, and portable/raw-delta anchors; byte digests for every supplied manifest; automatically discovered repositories in all three workspace states; and every raw-delta path. Workspace candidates include a safe relative path and one of `unchanged_from_base`, `already_matches_candidate`, or `changed_since_base`; non-workspace candidates use `not_applicable`.

Successful reviewer stdout MUST be exactly one `agentlab.review-proposal/v1` JSON object. It copies the request review ID and anchors exactly and contains one disposition for every candidate path. Each disposition is `proposed`, `rejected`, `conflicted`, or `unresolved`, has a nonempty reason, and contributes to exact reconciled counts. Duplicate, missing, extra, incorrectly anchored, or inconsistently counted dispositions are invalid. An optional workspace operation is allowed only on a proposed workspace candidate, uses its exact safe relative path, and is `delete` for a deletion or `replace` for any other change. Environment paths never receive workspace operations; a proposed environment path requires a declarative recommendation.

After the reviewer exits, AgentLab re-verifies immutable run and review inputs and freshly snapshots CURRENT again. It accepts a receipt only if the source workspace identity is unchanged. `agentlab.review/v1` stores the canonical absolute source-workspace path, request and validated proposal, reviewer timing and exit status, exact stdout/stderr and canonical request/proposal artifacts, source-unchanged and `agentlab_applied_changes: false` declarations, warnings, and integrity hashes. `agentlab inspect --verify RUN` verifies every accepted review.

The reviewer is a trusted host process with the invoking user's full authority and may see sensitive captured material. Review-only means AgentLab does not apply the proposal; it cannot prevent the reviewer from affecting other host resources. The supplied Pi wrapper deliberately disables sessions, extensions, skills, prompt templates, and mutating built-in tools, but that wrapper is convenience rather than a security boundary.

## 15. Receipt-bound application

`agentlab apply REVIEW_ID --workspace CURRENT` is the only operation in Milestone 6 that authorizes AgentLab to mutate the selected host workspace. The review ID resolves to exactly one integrity-verified `agentlab.review/v1` record. A review accepts at most one successful apply. Concurrent or interrupted attempts are serialized by an exclusive per-review lock; an interrupted lock is an explicit recovery condition rather than permission to guess or retry automatically.

Before changing the source, apply MUST:

1. Reject a review with conflicted candidates unless `--acknowledge-conflicts` is explicit.
2. Reject a review with unresolved candidates unless `--acknowledge-unresolved` is explicit.
3. Interpret those flags only as acknowledgement; conflicted, unresolved, rejected, and non-workspace paths remain unapplied.
4. Resolve CURRENT to the same absolute host path recorded by the review, freshly snapshot it, and require exact identity with the review's anchored current-workspace snapshot.
5. Load and verify the anchored candidate-workspace snapshot and ensure every operation still agrees with candidate presence or deletion.
6. Materialize the complete reviewed current snapshot privately, apply only proposed workspace operations there, and snapshot the intended result.
7. Snapshot CURRENT again immediately before the first write and reject any intervening source change.
8. Retain the canonical complete before-workspace manifest and all of its content-addressed blobs as recovery evidence.

Apply performs only exact relative `replace` and `delete` operations authorized by the receipt. It MUST reject traversal and parent-symlink escape, MUST NOT recursively remove directory content absent corresponding reviewed operations, and MUST NOT copy environment paths. Directory creation and mode restoration may support exact authorized child operations, but missing unreviewed parents are not synthesized. If a path-scoped write fails or the resulting snapshot differs from the privately staged identity, AgentLab attempts to restore every authorized path from the before snapshot and reports whether rollback succeeded. A process interruption may require recovery from the retained before snapshot; it must not be silently retried.

`agentlab.apply/v1` records a unique apply ID; exact review, run, and result identities; absolute selected workspace path; timestamps; explicit conflict/unresolved acknowledgements; reconciled proposed/rejected/conflicted/unresolved/applied counts; exact authorized operations; before, privately intended, and actual after snapshot identities; the canonical backup-manifest artifact; required source-match and result-verification declarations; warnings; and integrity hashes. Intended and actual after identities MUST match. `agentlab inspect --verify RUN` verifies all accepted apply records, referenced review receipts, snapshots, and backup bytes. A second apply for the same review is rejected.

Human-readable review currently reports every disposition, reason, recommendation, operation, and path. A future terminal diff MAY add unified or side-by-side, colorized base/candidate/current and before/after content views, including binary, type, and mode changes. Diff rendering is a read-only projection over immutable records and MUST NOT become an alternative authorization channel or change apply semantics.

## 16. Accepted-input lineage

`agentlab accept RUN` explicitly accepts the exact starting input tested by one completed initial run. It MUST verify the run and all of its referenced immutable lifecycle, evaluation, review, apply, and prior-acceptance evidence before recording the decision. It accepts the starting workspace/image input, not the run's result filesystem. This is the bootstrap form for naming an already tested input and MAY point to a prior acceptance when RUN itself started from one.

`agentlab accept RETEST_RUN --from-apply APPLY_ID` adds reviewed application lineage. It MUST resolve and verify exactly one apply receipt; require RETEST_RUN to differ from the apply's candidate run; require the retest starting workspace snapshot to equal the apply receipt's exact after snapshot; and require the retest and candidate to share resolved OCI image digest, target platform, and guest workspace path. The initial workspace result remains the only apply target in `agentlab.acceptance/v1`; environment recommendations are not promoted into an image.

The content-based `agentlab.accepted-input/v1` identity hashes:

- workspace snapshot digest;
- active workspace-ignore identity;
- guest workspace path;
- resolved OCI image digest; and
- target platform.

It excludes acceptance time, acceptance ID, test command, runtime settings, and test output. Those belong to the decision or test lineage rather than the reusable base content.

`agentlab.acceptance/v1` records a unique acceptance ID and record digest; accepted-input digest; timestamp; `tested_input` or `reviewed_application` kind; `explicit` decision; exact workspace snapshot, ignore, and guest-path identities; requested, immutable execution-reference, resolved-digest, Docker-image-ID, and platform image evidence; test run/result/input identities and exit code; optional parent acceptance; optional candidate run/result/input, review, and apply identities; and warnings. Acceptance after a nonzero test exit is allowed because exit status is evidence rather than a universal judgment. The record MUST postdate its test and application evidence.

Acceptance records live independently beneath the private state root. A completed test run receives at most one acceptance decision. The Docker image execution reference SHOULD be a pullable repository digest; a local-only image ID is valid but MUST produce a portability warning. `agentlab inspect --verify ACCEPTANCE_ID` verifies the record and recursively referenced lineage.

`agentlab run --accepted ACCEPTANCE_ID -- COMMAND` MUST verify the selected acceptance before execution, reconstruct its exact workspace snapshot, resolve its immutable OCI execution reference, and require equality of workspace, ignore, guest path, resolved image, and platform identities. The run specification records acceptance ID, record digest, and accepted-input digest as provenance. That reference is deliberately excluded from `agentlab.run-input/v1`: two runs with identical actual controlled inputs remain comparable even if one names the lineage and one supplies the same snapshot/image directly.

New runs start from the accepted workspace snapshot and image, never the retest result filesystem. Retest logs, caches, and other session writes therefore do not enter the new base automatically. Candidate changes enter only through the explicit review/apply path. Ordinary run removal MUST refuse to delete a test or candidate run referenced by an acceptance so accepted lineage remains auditable.

## 17. Current boundary

The host workspace is mutable developer state, not an AgentLab-owned golden copy; an accepted baseline is a reference to tested or reviewed immutable input/result lineage. Workspace treatments are ordinary host changes captured as new snapshots. A treatment outside the workspace is currently prepared through the backend—for example, by committing a changed Docker container as a new image—and supplied by immutable image identity. Apply deliberately leaves environment recommendations unapplied, so the v1 reviewed-application acceptance requires the candidate and retest to use the same OCI image. Retention preserves the private filesystem and container configuration, not the prior process tree or live memory. No protocol field assigns meaning to a harness, model, reasoning level, skill, prompt convention, evaluator score, or workspace layout.
