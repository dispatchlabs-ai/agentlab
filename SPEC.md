# AgentLab Specification

Status: Milestone 1 working contract
Snapshot schema: `agentlab.snapshot/v1`

## 1. Scope

AgentLab's core protocol is:

```text
immutable input
    → isolated execution
    → complete observation and filesystem delta
```

Milestone 1 defines the immutable workspace-input portion. It deliberately does not define a workspace layout, repository registry, harness integration, environment-definition language, evaluator, adopter, daemon, scheduler, or cloud control plane.

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

## 7. Future protocol boundary

Milestone 2 will combine a workspace snapshot with a resolved OCI image digest and materialization settings, copy that state into private Docker storage, execute an opaque command, and capture the persistent guest-root delta. No Milestone 1 field assigns meaning to a harness, model, reasoning level, skill, prompt convention, or workspace layout.
