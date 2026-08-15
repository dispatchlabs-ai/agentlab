# AgentLab Specification

Status: Milestones 1–3 working contract
Snapshot schema: `agentlab.snapshot/v1`
Run schema: `agentlab.run/v1`
Delta schema: `agentlab.delta/v1`
Result schema: `agentlab.result/v1`

## 1. Scope

AgentLab's core protocol is:

```text
immutable input
    → isolated execution
    → complete observation and filesystem delta
```

Milestone 1 defines the immutable workspace input. Milestone 2 defines one isolated direct-Docker execution and portable persistent-root-filesystem result. Milestone 3 proves independent repetition and derives comparisons from those existing records. None defines a workspace layout, repository registry, harness integration, evaluator, adopter, daemon, scheduler, cloud control plane, or generalized execution-backend framework.

The selected workspace is opaque user content. Names such as `AGENTS.md`, `MEMORY.md`, `repos/`, `skills/`, and `worktrees/` have no meaning to the snapshot protocol.

## 2. Workspace snapshot contract

Given a selected directory, the snapshotter MUST:

1. Traverse every path beneath the selected root without following symbolic links.
2. Include regular files, directories, hidden paths, empty directories, Git repositories and their in-workspace metadata, untracked paths, large files, modes, and symlink targets by default.
3. Apply root and nested `.gitignore` files using Git wildmatch, directory-relative, ordering, and negation semantics.
4. Discover ordinary Git repositories from `.git` directories or files without repository declarations.
5. Include tracked files inside discovered repositories even when an ignore rule matches them.
6. Exclude machine-global and system Git ignore configuration from snapshot selection.
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

The active ignore-rule identity is included because ignore rules resolve which workspace content belongs to the immutable input. Each rule record contains its workspace-relative path and content digest, not its contents.

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

`agentlab run` combines a workspace snapshot, immutable OCI image resolution, materialization settings, an opaque command, resource and network policy, arbitrary factors, change-ignore identity, and requested captures. The implementation MUST:

1. Reconstruct the workspace from its snapshot in private storage at `/workspace` by default, never through a writable source mount.
2. Establish and export a prepared base root filesystem after materialization and before command execution.
3. execute the command exactly once in a uniquely named retained container;
4. preserve stdout, stderr, the actual exit code including nonzero values, timestamps, and lifecycle events;
5. reject image-declared volumes and any container mount outside the exported root filesystem;
6. retain the stopped result container for direct inspection; and
7. state explicitly that pseudo-filesystems and live process memory are not portable persistent state.

The run specification contains the snapshot digest, requested image, resolved immutable image digest, Docker image evidence, target platform, guest workspace path, argv, working directory, factors, resource limits, network policy, capture declarations, workspace- and change-ignore identities, backend evidence, and AgentLab version. Factors are recorded verbatim and have no core semantics.

The Docker image and container identifiers are evidence, not AgentLab's portable run identity. A prepared Docker image ID may be nondeterministic without changing the meaning of the declared input.

## 8. Persistent root-filesystem observation

AgentLab exports both the prepared base and completed merged root filesystems. It normalizes each path to a sorted manifest containing its absolute UTF-8 path, type, relevant mode, regular-file digest and size, or symlink target. Supported normalized types are regular file, directory, and symlink; archive hard links resolve to the target regular-file identity. Unsupported persistent archive types fail explicitly.

Comparison produces one of: `added`, `modified`, `deleted`, `type_changed`, `mode_changed`, or `symlink_changed`. A rename is portable as an authoritative delete plus add. Result regular-file bodies are inserted into the content-addressed store.

Docker diff output is retained as path evidence and checked against the authoritative comparison. It does not replace the prepared/result exports: export provides merged content but not deletions, while Docker diff lacks complete portable content and metadata. Any normalized path not covered by Docker diff is recorded explicitly as evidence and a warning rather than silently discarded.

Runtime-only pseudo-filesystems are reported as nonportable. Writable host binds, named volumes, cache mounts, and image volumes are unsupported because their contents would fall outside the captured root filesystem.

## 9. Delta contract

`agentlab.delta/v1` contains:

- its canonical SHA-256 identity;
- prepared-base and result-rootfs identities;
- change-ignore source and content identity;
- normalized path changes; and
- explicit ignored-change records.

`delta.raw.json` records every normalized persistent change. `delta.json` applies `.agentlabignore` from the selected workspace root, or an explicit `--change-ignore` file, using Git-compatible patterns. Ignore rules affect only portable selection: ignored paths remain observed in the raw delta and present in the retained container. They are never represented as unobserved.

The delta identity is SHA-256 over compact JSON containing every semantic field except the digest itself. Arrays are deterministically path ordered by rootfs comparison.

## 10. Result contract and integrity

`agentlab.result/v1` contains the run ID and run-spec digest, timing and lifecycle, exit code, stdout/stderr and requested capture artifacts, rootfs and delta identities, Docker evidence, observation status, warnings, and a path-to-digest integrity map. Its identity is SHA-256 over compact JSON containing those semantic fields except the result digest itself.

Run-local artifacts include the specification, normalized rootfs manifests, raw and portable deltas, complete base/result exports, stdout, stderr, Docker inspection and diff evidence, and requested capture archives. `agentlab inspect --verify RUN` recalculates every listed artifact digest and the result identity before reporting success. Default inspection reports metadata and paths without printing captured file content.

Run artifacts and complete rootfs exports may contain credentials or other sensitive information. A retained container and local state directory are private operational artifacts, not safe publication units.

## 11. Comparable repetition

Independent `agentlab run` invocations may execute concurrently against the same source workspace and image. Each MUST reconstruct its own snapshot and receive a distinct private Docker writable layer and retained container. Concurrent snapshot/content-store writes MUST preserve immutable content-addressed semantics.

Factors are an ordered string-to-string map recorded unchanged in `agentlab.run/v1`. AgentLab does not interpret names such as `variant`, `replicate`, `model`, or `thinking`. Empty keys and duplicate CLI keys are rejected rather than normalized or silently overwritten.

`agentlab compare LEFT RIGHT` loads and integrity-verifies both results and specifications. It reports:

- equality of workspace snapshot and resolved image identities;
- equality of the exported prepared-base rootfs identity;
- distinct retained container IDs and names;
- controlled-input differences across command, workspace materialization, resource/network settings, captures, ignore identities, backend evidence, and AgentLab version;
- exact left/right factor values, including a missing value on either side;
- missing or unexpected differences relative to repeated `--expect-factor KEY` declarations; and
- equality or difference of portable result-rootfs identities.

A comparison is a `comparable_repetition` only when workspace, resolved image, and prepared base are identical; retained containers are distinct; controlled inputs are equal; and the actual factor-difference key set exactly matches the expected set. Image request aliases are not treated as controlled differences when they resolve to the same immutable image; resolved identity is authoritative.

Comparison is derived metadata rather than a new persisted experimental-cell object. Concurrent launch uses ordinary independent CLI processes in this milestone; AgentLab does not introduce a scheduler, daemon, automatic statistical conclusion, or factor registry.

## 12. Current boundary

AgentLab does not yet provide lifecycle management, continuation, fork/adopt operations, evaluation, or another backend. Retention preserves the private filesystem and container configuration, not the prior process tree or live memory. No protocol field assigns meaning to a harness, model, reasoning level, skill, prompt convention, or workspace layout.
