# AgentLab: Goal-Oriented Implementation Plan

## Purpose of this document

This is the implementation plan for a new, standalone open-source project named **AgentLab**. It is intended to be handed to an implementation agent in a fresh repository outside the Daily Log workspace.

Recommended repository location:

```text
~/Development/agentlab
```

Do not implement AgentLab inside `daily-log`, `daily-log-infra`, or another workspace-specific repository. Those projects may later consume AgentLab and may provide useful reference material, but AgentLab must have no dependency on their layouts, policies, memories, secrets, or tooling.

The plan is goal-oriented. Implementation choices are subordinate to the observable behaviors and invariants defined here. Prefer the smallest design that satisfies them.

## Current status

- Milestone 0 is complete: AgentLab exists as an independent Rust project with a CLI, specification, conformance outline, and permissive license.
- Milestone 1 is complete: deterministic, content-addressed workspace snapshots are implemented and covered by passing unit and conformance tests plus strict Clippy validation.
- The `agentlab.snapshot/v1` verification gaps are closed: permission-only source mutations are detected during capture, and verification independently recomputes `ignore_rules_digest` from the recorded rule set.
- Milestone 2 is complete: one direct-Docker command is materialized privately, executed once, retained, and normalized into documented run, delta, and result artifacts with integrity verification. New runs use `agentlab.run/v2` and a derived `agentlab.run-input/v1` identity; legacy version-one runs remain readable.
- The Docker-gated whole-machine conformance fixture covers repository commits, package installation, workspace and system paths, content/mode/type/symlink/delete changes, ignore selection, nonzero exit, captures, evidence, and source immutability.
- The Pi-workspace hands-on checkpoint passed against `ubuntu:24.04`: writes under `/workspace`, `/etc`, and `/root` appeared only in the retained container and portable delta, while the source snapshot digest remained unchanged.
- Milestone 3 is complete: `run --snapshot DIGEST` reuses one exact workspace input, concurrent runs receive distinct writable layers, and comparison derives exact repetition or real controlled-input differences from recorded identities without factor labels.
- The Pi-workspace repetition checkpoint established simultaneous isolation with identical snapshot, image, command, and portable base plus distinct retained containers and private outcomes. The current conformance case additionally proves equal version-two run-input digests from one stored snapshot and an unchanged source workspace.
- Milestone 4 is complete: stable retained containers support ownership-checked list, inspect, stop, restart, harness continuation, filesystem fork, and exact removal while explicitly disclaiming live-memory restoration.
- The Pi-workspace lifecycle checkpoint passed: one container ID survived stop/start, session value `1` continued to `2`, the requested capture refreshed, the fork inherited the exact continued base, deleting the fork preserved its parent, and the source snapshot remained unchanged.
- Milestone 5 is complete: arbitrary host evaluator commands produce integrity-checked structured observations, and reports align actual run-input, workspace, image, portable-base, and scalar-score identities without built-in ranking, statistics, or causal claims.
- The current four-run conformance case snapshots a real workspace without and with a skill directory, repeats each exact input twice, verifies within-input repetition and the cross-input workspace difference, and produces Markdown and JSON reports while preserving the host workspace after the deliberate edit.
- The next implementation milestone is optional reviewed adoption (Milestone 6). Keep review-only as the default and require explicit authorization before changing a current workspace.

## North-star goal

Create a thin, layout-agnostic primitive for running agentic sessions in scientifically comparable isolation.

Given:

- any host directory as a workspace;
- any OCI environment image;
- any command as an agent harness;
- any task or prompt passed to that command; and
- any real treatment expressed as workspace content, image content, command arguments, or recorded runtime settings;

AgentLab must:

1. Capture an immutable, content-addressed snapshot of the workspace.
2. Materialize it inside a private machine without a writable mount of the source workspace.
3. Run the command with the private workspace as its working context.
4. Allow the command to modify the entire private machine as it sees fit.
5. Capture every persistent filesystem change across the entire machine by default, not only changes beneath the workspace.
6. Record the resolved inputs, derived input identity, lifecycle, outputs, observations, and resulting filesystem delta.
7. Allow independent runs from the same base to execute without affecting one another.
8. Retain, inspect, stop, resume, fork when supported, or discard a run while distinguishing filesystem continuation from live-memory resume.
9. Make results available to external evaluators.
10. Optionally support AI-assisted adoption of selected changes into a current host workspace or environment definition.

AgentLab should make experiments such as these practical:

- Does a skill improve or harm agent performance?
- Does a different workspace layout affect results?
- What is the lowest reasoning level that preserves effectiveness?
- Which harness performs best against this workspace and task?
- Does a tool-rich environment outperform a minimal environment?
- Does a proposed instruction, memory, or policy improve future sessions?
- Which changes produced by a successful run should become part of the next workspace or environment?

## Product boundary

The core primitive is:

```text
immutable input
    → isolated execution
    → complete observation and filesystem delta
```

The optional adoption convenience is:

```text
base + candidate + current
    → AI judgment
    → selectively improved workspace/environment
```

Keep these layers separate. A user who only wants controlled experiments must not need to configure adoption.

## Non-negotiable principles

### Default inclusion

Everything beneath the selected workspace directory is included by default:

- regular files;
- directories;
- hidden files and directories;
- symbolic links;
- Git repositories and their metadata;
- untracked files;
- large files; and
- any workspace layout selected by the user.

AgentLab must not require an inclusion allowlist.

### Gitignore-compatible workspace exclusion

`.gitignore` is the workspace snapshot exclusion standard, including when the selected workspace root is not itself a Git repository.

Required semantics:

- A `.gitignore` at the selected workspace root applies from that root.
- Nested `.gitignore` files apply relative to their directories.
- Negation and Git wildmatch behavior are supported.
- Git repositories are discovered automatically.
- Within a discovered Git repository, tracked files remain included even if they match an ignore pattern; ignore rules govern untracked paths as Git users expect.
- Outside discovered repositories, `.gitignore` patterns apply directly to the filesystem walk.
- Machine-global Git excludes must not silently influence a snapshot because they would make results host-dependent. Support them only through an explicit option if a real use case appears.

Do not require repository declarations. Do not prescribe where repositories live.

### Complete machine-change capture

Every persistent filesystem change made inside the private machine is captured by default, including changes beneath paths such as:

```text
/workspace
/home
/root
/etc
/usr
/opt
/var
```

Examples include:

- workspace edits and deletions;
- repository commits and `.git` changes;
- installed or removed packages;
- shell configuration;
- harness state and sessions;
- nested container-runtime images, layers, volumes, and daemon configuration when those live in the captured guest root filesystem;
- downloaded tools;
- service configuration;
- logs, caches, build products, and temporary persistent files; and
- accidental or undesirable changes.

AgentLab records facts. It must not decide that a path is unimportant merely because it looks temporary.

Virtual and runtime-mounted filesystems such as `/proc`, `/sys`, `/dev`, and runtime-only mount state are not persistent root-filesystem changes. Record the mount topology and relevant runtime observations, but do not misrepresent pseudo-filesystem contents as portable filesystem changes.

The authoritative capture boundary is the prepared guest root filesystem. Backend control-plane storage, engine caches, host runtime metadata, and ephemeral mounts outside that guest rootfs are not guest changes. Record their relevant identities and observations separately. “Complete machine-change capture” means every persistent path visible in the captured guest rootfs, not every byte used internally by the execution provider.

### Gitignore-compatible change exclusion

Users may optionally omit selected machine changes from the portable result by providing `.agentlabignore` at the workspace root or an explicit `--change-ignore PATH` file.

The ignore language must use Git wildmatch and negation semantics. Unlike `.gitignore`, these patterns are evaluated against paths rooted at the guest filesystem root.

Examples:

```gitignore
/tmp/**
/var/cache/**
/var/lib/docker/**
/home/agent/.codex/auth.json
/home/agent/.pi/agent/auth.json
**/*.log
```

Required behavior:

- If no change-ignore file exists, no persistent filesystem changes are suppressed.
- Ignored changes remain present in a retained sandbox or native checkpoint; they are omitted only from the exported portable delta.
- The result records which ignore rules were active and the count and paths of ignored changes, without reading or printing ignored sensitive contents unnecessarily.
- Change-ignore behavior must not alter the running agent's machine.

### No prescribed workspace layout

AgentLab must not require or assign meaning to any of these names:

```text
AGENTS.md
MEMORY.md
memory/
repos/
skills/
projects/
worktrees/
```

If they exist, they are ordinary snapshot content. Their effects can be measured experimentally.

The selected workspace appears at `/workspace` by default. Allow a different guest path when requested, but impose no structure inside it.

### Opaque harness

The harness is an arbitrary command and argument vector. AgentLab must not need built-in knowledge of Pi, Codex, Claude Code, OpenCode, or another agent.

Harness-specific wrappers may be supplied as examples. They are not protocol requirements.

### Existing environment standard

Use an OCI image digest as the resolved environment identity. Do not invent a new environment-definition language.

Users may produce images with Dockerfiles, Nix, Buildpacks, CI, or another tool. A convenience option may build a Dockerfile, but each run records the resolved immutable image digest.

### Real experimental inputs

Anything AgentLab claims to compare must be present in the actual controlled input. A descriptive label is not evidence that a skill, model, prompt, layout, or tool was used.

The primitive preparation workflow is:

1. Arrange the mutable host workspace exactly as desired for input A and snapshot it once.
2. Launch every A repetition from that stored snapshot digest.
3. Make the actual treatment change in the host workspace and snapshot input B once.
4. Launch every B repetition from the B digest.
5. Let comparison derive whether inputs are identical and which recorded fields differ.

A model, reasoning level, harness, or task belongs in the opaque command or in snapshotted harness configuration. A workspace skill is an ordinary directory and files: A's snapshot omits it and B's snapshot contains it. A tool or system package outside the workspace belongs in the OCI image. With Docker today, a user may enter a disposable container, make the change, commit a new layer, and give AgentLab that image; a VM backend may later accept an equivalent VM snapshot.

AgentLab does not need a factor map, treatment registry, preparation DSL, or special knowledge of any of these concepts. Human-friendly experiment names may live in an external notebook or orchestration script, but they never substitute for content-addressed evidence.

### Content-addressed identity

The meaningful identity of a base is derived from resolved content, not a mutable name:

```text
base identity = workspace snapshot digest + OCI image digest + materialization settings
```

Human-friendly names such as `baseline`, `g7`, or `skill-on` are optional labels.

The host workspace is the primary mutable development state, not a golden copy. Immutable snapshots and resolved images are the states used for tests. A later accepted baseline is a reference to reviewed immutable input/result lineage, not another host workspace managed behind the user's back.

### Source workspace safety

The source workspace must never be mounted writable into a run.

The default implementation must snapshot and copy the workspace into private storage before execution. Reject writable host bind mounts in the first version. Later unsafe escape hatches, if any, must be explicit and visibly recorded in the run result.

## Minimal protocol

Keep each versioned wire model as small as possible. New run specifications use `agentlab.run/v2`; version-one run specifications remain readable only for migration.

### Run specification

The resolved run specification contains only the information required to reproduce or interpret the controlled input:

- schema version;
- run ID;
- canonical run-input digest;
- workspace snapshot digest;
- resolved OCI image digest;
- target platform/architecture;
- workspace guest path;
- command and arguments;
- working directory;
- hashes of supplied stdin, prompt, or task files when applicable;
- declared resource limits;
- declared network policy when supported;
- declared captures and secret injections;
- active workspace-ignore and change-ignore rule digests;
- backend name and version;
- underlying runtime version when applicable; and
- AgentLab version.

Do not add first-class fields for specific harnesses, models, skills, or workspace conventions.

### Run result

The run result contains:

- exact resolved run specification;
- start and completion timestamps;
- lifecycle events;
- exit status;
- stdout and stderr artifacts;
- resource and network observations available from the backend;
- base and result filesystem identities;
- complete persistent root-filesystem delta, subject only to declared change-ignore rules;
- portable content blobs or references for added and modified files;
- deletion and whiteout records;
- file modes and symlink targets;
- automatically discovered Git observations as derived metadata;
- requested captures from runtime locations;
- retained backend state or native checkpoint reference;
- backend-native identifiers and digests as evidence rather than canonical AgentLab identities;
- warnings, unsupported observations, and collection failures; and
- integrity hashes for all result artifacts.

The result must clearly distinguish:

- observed and captured;
- observed but deliberately ignored;
- requested but unavailable;
- runtime-only and nonportable; and
- not observed by the backend.

Never imply that an incomplete observation is complete.

### Content store

Store file content separately from manifests using content-addressed blobs. This allows unchanged and large files to be reused without repeatedly copying them.

The first implementation may use a local filesystem store. Its on-disk contract must permit a later S3-compatible implementation without changing run semantics.

AgentLab owns the portable identities for snapshots, bases, deltas, and results. Docker image and container IDs are recorded as execution evidence, not substituted for the portable result identity.

Do not put generated run content into the user's source workspace by default.

## Minimal CLI surface

Prioritize a small, composable CLI. The first coherent interface should be no larger than:

```text
agentlab snapshot
agentlab run
agentlab list
agentlab inspect
agentlab diff
agentlab stop
agentlab resume
agentlab fork
agentlab rm
```

Optional adoption may add:

```text
agentlab adopt
```

`agentlab snapshot` is part of the first coherent interface so deterministic snapshotting is independently usable and inspectable before isolated execution exists. `agentlab run` still snapshots automatically when given a workspace.

In the direct-Docker implementation, lifecycle commands manage retained named containers. `resume` restarts or re-enters the exact retained persistent filesystem and container configuration. It does not imply restoration of process memory. Unsupported operations such as memory resume must fail explicitly rather than being simulated.

Representative use:

```bash
SNAPSHOT=$(agentlab snapshot --workspace . --json | jq -r .digest)

agentlab run \
  --snapshot "$SNAPSHOT" \
  --image ubuntu-agent@sha256:... \
  --capture /home/agent/.codex/sessions=codex-sessions \
  -- \
  pi --provider openai-codex --model gpt-5.6-sol --thinking medium "Perform the task"
```

The agent inside the run should need only its ordinary command-line tools. It should not need to know AgentLab exists.

## Reference backend

Build Milestones 2 through 4 directly against Docker. It is already available in the target development environment, the retained-container experiment has proven the essential lifecycle, and it is the shortest path to validating AgentLab's actual protocol.

AgentLab, not Docker, owns the portable workspace snapshot, base identity, run specification, filesystem delta, result manifest, and content store. Docker image IDs, container IDs, inspection records, and diff output are execution evidence.

### Thin execution seam

Keep Docker-specific mechanics in one internal package with only the operations the first implementation needs:

```text
prepare -> run -> inspect -> export -> stop/resume -> delete
```

Do not design a public generalized backend framework yet. Do not add capability negotiation, abstract checkpoint hierarchies, or lifecycle concepts that have no second implementation. The versioned AgentLab manifests should remain portable enough that a later backend can consume and produce them, but internal interfaces may be refactored when real evidence from that backend exists.

### Direct-Docker workflow

The Docker implementation must:

1. Load and verify the AgentLab workspace snapshot.
2. Resolve the requested OCI image to an immutable digest and record the Docker Engine version.
3. Materialize the snapshot into private temporary storage using AgentLab's manifest and content store.
4. Create a preparation container without a writable source-workspace mount, copy the workspace into `/workspace`, and apply the requested ownership, modes, working directory, user, environment, and materialization settings.
5. Commit or build that complete prepared state as a private temporary base image before agent execution. The AgentLab base identity remains the workspace snapshot digest, resolved OCI image digest, and canonical materialization settings; a nondeterministic Docker image ID does not replace it.
6. Launch a uniquely named retained agent container from that prepared base with the requested resource and network settings and a minimal stable container process that permits later `exec`, stop, and start operations.
7. Execute the opaque command once through the Docker exec API with cwd at `/workspace` by default, and capture stdout, stderr, exit status, timestamps, lifecycle events, inspection data, and available resource observations even when it exits nonzero.
8. Capture path-level additions, modifications, and deletions with Docker's container diff facilities and export the complete merged result root filesystem for authoritative changed content, modes, types, and symlink targets.
9. Normalize those observations into `agentlab.delta/v1`, using authoritative delete records and representing a rename as delete-plus-add when necessary.
10. Apply `.agentlabignore` only while producing the portable delta; ignored changes remain in the retained container.
11. Store changed regular-file content in the AgentLab content store and write an integrity-checked `agentlab.result/v1` manifest.
12. Retain the named container when requested so its exact persistent filesystem and container configuration can be stopped, restarted, inspected, resumed at the harness level, or used to create a new image for a filesystem-level fork.
13. Delete only the selected run's container, temporary prepared image, and explicitly owned temporary resources when removal is requested.

Docker diff is path evidence, not the entire portable result. Docker export supplies the merged result content but excludes volume contents and does not itself express deletions. AgentLab must combine both sources and verify their consistency. The first version must reject agent-writable host bind mounts, named volumes, cache mounts, and other persistent mount paths that would fall outside the exported root filesystem.

Docker stop/start preserves the container's private filesystem state but does not restore the process tree or live memory. Record this honestly. Harness-level continuation from session files is still useful and is the required first-version meaning of resume.

### Later alternatives

After Milestones 2 through 4 pass, run a time-boxed Dagger spike against the same conformance fixture. Adopt Dagger only if measured implementation size, correctness, portability, or operational simplicity is materially better than direct Docker. Dagger is not a required AgentLab dependency or planned production backend merely because it is being evaluated.

Add a Firecracker/E2B-compatible VM backend only when stronger isolation, remote placement, VM snapshots, memory checkpoints, or cloud scale becomes a concrete requirement. Preserve the portable AgentLab run and result formats, but allow the internal execution seam to evolve from evidence rather than prediction.

Do not build a daemon, scheduler, database, billing layer, or cloud control plane in the first version.

## Derived Git observations

Git support is discovery and interpretation, not workspace configuration.

After a run, discover repositories anywhere in the base and result filesystems. For each repository, report when determinable:

- path;
- base and result `HEAD`;
- branches;
- clean or dirty status;
- new commits;
- changed tracked files;
- untracked files; and
- broken or externally referenced Git metadata.

The filesystem delta remains authoritative. Git observations exist to improve evaluation and optional adoption.

## Evaluation layer

AgentLab creates controlled and inspectable experimental runs; it does not define universal success.

Allow an evaluator to be any command that consumes a run directory or result manifest and emits structured scores or observations.

Evaluation examples include:

- test success;
- task-specific correctness;
- human or model preference;
- policy adherence;
- workspace damage;
- promotion acceptance rate;
- number of rejected changes;
- wall time;
- token use and model cost when exposed by the harness;
- CPU, memory, disk, and network use; and
- result variance across repeated runs from an exact input.

Scientific comparisons should use repeated executions because model execution is nondeterministic. AgentLab guarantees identical content-addressed starting inputs when the same snapshot and other resolved inputs are reused; it does not claim deterministic model or external-service behavior.

The initial version may leave matrix orchestration, statistics, and visualization to external scripts. Preserve enough structured data for those tools to work.

## Optional adoption layer

Adoption is convenience porcelain over the core result. It is not required to run or evaluate experiments.

The adopter receives three states:

```text
base       = immutable workspace/image input used by the run
candidate  = immutable run result and complete machine delta
current    = freshly snapshotted mutable host workspace and environment source
```

The current host workspace is the developer's primary working state. It is not a golden copy. The immutable base exists so candidate changes can be reviewed against both what the run saw and what the developer has changed since. A later accepted baseline records reviewed lineage; adoption does not silently redefine one.

Default mode is proposal-only. Applying changes requires a separate explicit operation against the exact review receipt. The smallest harness-neutral interface invokes any command-line reviewer and supplies a materialized review bundle plus paths through environment variables. The command emits a versioned JSON proposal that classifies each candidate path as proposed, rejected, conflicted, or unresolved and may include workspace patch operations. AgentLab validates schema, path scope, base/candidate/current anchors, and disposition counts before accepting the proposal.

The reviewer may be Pi, Codex, another agent, a deterministic program, or a human-authored command wrapper. AgentLab must not understand its prompt protocol. A host reviewer executes with the invoking user's authority and may see sensitive candidate material; it is a trusted process, not an AgentLab sandbox. Proposal-only means AgentLab itself does not apply the proposal. It cannot make a malicious host command non-mutating.

The adoption agent should:

- receive faithful materializations of base, candidate workspace, and current plus the full machine-delta manifest;
- run from a review copy whose current tree contains the applicable workspace instructions;
- discover repositories automatically and use Git where appropriate;
- use three-way filesystem merging for nonrepository paths;
- recognize when the host workspace advanced after the run began;
- review every captured machine change, not only `/workspace` changes;
- translate worthwhile OS or tool changes into declarative OCI image source changes rather than copying `/usr`, `/etc`, or `/var` blindly;
- treat container-runtime state, caches, logs, downloads, and temporary files as evidence to judge rather than predetermined garbage;
- never print or casually copy credentials;
- report accepted, rejected, conflicted, and unresolved changes precisely; and
- emit an immutable adoption receipt.

Initial automatic apply should be limited to reviewed workspace changes that can be expressed and checked safely. Valuable changes outside `/workspace` produce explicit environment recommendations or declarative image-source edits; unresolved machine changes must not be copied blindly into the host or silently committed into an accepted image. An explicit allow-unresolved override, if ever provided, must remain visible in the receipt.

Before apply, AgentLab snapshots current again and rejects a stale proposal unless the recorded three-way preconditions still hold. Apply writes only the paths authorized by the receipt, preserves a recoverable backup or patch artifact, verifies the resulting snapshot, and records exactly what changed. Review and apply are separate trust and authorization boundaries.

## Security and privacy goals

Capturing all changes creates an intentionally sensitive artifact. Design for that fact rather than silently dropping information.

Required defaults:

- Run state and captured content remain local unless the user explicitly exports or uploads them.
- Local state directories are private to the current user.
- Result inspection shows paths, hashes, sizes, and modes by default; file contents require an explicit request.
- Authentication material is never baked into an environment image.
- Support injecting secrets through ephemeral mechanisms such as environment variables, tmpfs, or backend-native secret facilities.
- If a process writes a secret into persistent storage, it is a captured change unless the user ignored it; warn clearly without printing the value.
- Record whether secret scanning ran and whether possible sensitive paths or content were detected.
- Never claim that ignore rules, redaction, or scanning make an untrusted result safe to publish.
- Destructive host operations and adoption remain explicit.

## Goal-oriented milestones

Each milestone must end with a documented, user-visible hands-on checkpoint that can be run on a developer machine. Starting with Milestone 1, the checkpoint must accept a workspace chosen by the user rather than working only with internal fixtures. Disposable fixtures remain mandatory for exhaustive and destructive conformance tests; real-workspace checkpoints must be read-only with respect to the source unless the user explicitly invokes adoption with apply authorization.

### Milestone 0: Establish the independent project

**Status:** Complete.

**Goal:** Create a clean, standalone AgentLab repository whose contracts are not coupled to Daily Log or any particular harness.

**Outcomes:**

- A new repository exists outside `daily-log`.
- The repository has a permissive open-source license selected before public release.
- `README.md` states the north-star goal and a minimal example.
- `SPEC.md` records the invariants and minimal run/result model.
- A conformance-test outline exists before substantial implementation.
- The development CLI can be installed or invoked locally and reports its version and command help, even though experimental behavior is not implemented yet.
- No Daily Log path, repository name, workspace policy, model, or credential is embedded in core behavior.

**Acceptance:** A reviewer can describe AgentLab without mentioning Daily Log, Pi, Codex, `repos/`, memories, or skills.

**Hands-on checkpoint:** From a fresh checkout, install or invoke the development CLI and run `agentlab --version` and `agentlab --help`. This checkpoint proves packaging and entry-point viability only; it does not claim that isolation or observation works yet.

### Milestone 1: Produce deterministic workspace snapshots

**Status:** Complete, including the two schema-freeze verification corrections listed below.

**Goal:** Turn any directory into a content-addressed immutable snapshot with correct default-inclusion and `.gitignore` behavior.

**Outcomes:**

- Hidden, untracked, large, and repository files are included by default.
- Nested `.gitignore` behavior is tested.
- Tracked files inside discovered repositories remain included.
- The source directory is unchanged.
- Repeating a snapshot without source changes produces the same digest.
- Special files are either supported explicitly or cause a precise failure; they are never silently lost.
- A permission-only mutation that occurs while a regular file is captured causes a precise consistency failure.
- A manifest can reconstruct the snapshot byte-for-byte with relevant modes and symlinks.
- Manifest verification independently proves that `ignore_rules_digest` matches the recorded ignore rules.
- `agentlab snapshot --workspace PATH` exposes snapshotting through the public CLI and reports the snapshot digest plus a concise inclusion/exclusion summary.
- `agentlab inspect SNAPSHOT` shows the manifest's paths, hashes, sizes, modes, symlink targets, discovered repositories, and active ignore-rule identity without printing file contents by default.

**Acceptance:** Two structurally different fixture workspaces snapshot correctly without repository declarations or workspace configuration. In addition, a user can run the public CLI against an arbitrary local workspace, receive a stable snapshot digest, inspect included and excluded path metadata without revealing contents by default, reconstruct or verify the snapshot, and confirm that the source workspace remains unchanged.

**Hands-on checkpoint:** Run `agentlab snapshot --workspace /path/to/chosen/workspace`, inspect the returned digest, repeat the snapshot to demonstrate stable identity, and verify that the chosen source workspace is byte-identical afterward.

### Milestone 2: Run one isolated Docker command and capture the whole machine

**Status:** Complete.

**Goal:** Prove the portable run/result protocol by executing an arbitrary command in a private Docker container and exporting every persistent filesystem change across the guest.

**Outcomes:**

- `agentlab.run/v2`, `agentlab.run-input/v1`, `agentlab.delta/v1`, and `agentlab.result/v1` are documented before Docker details leak into them; version-one run reads remain compatible.
- Docker mechanics remain behind the thin internal execution seam.
- The workspace is reconstructed from the AgentLab snapshot and copied privately to `/workspace` by default.
- The source workspace has no writable mount in the container.
- A private prepared base image is established after environment resolution and workspace materialization but before command execution.
- The agent command executes once in a uniquely named retained container created from that prepared base.
- The command can modify `/workspace`, its home, `/etc`, `/usr`, `/opt`, and `/var` subject to guest permissions.
- Added, modified, deleted, type-changed, and renamed or authoritative delete-plus-add paths are captured across the persistent root filesystem.
- Docker diff and a complete merged-rootfs export are combined and consistency-checked rather than treating either one as sufficient alone.
- Modes, regular-file content, symlink targets, deletions, and unsupported types are normalized through AgentLab.
- `.agentlabignore` can omit selected exported changes using Git-compatible patterns.
- Ignored changes remain inside the retained container.
- stdout, stderr, nonzero exit status, lifecycle, Docker identifiers and inspection evidence, and integrity hashes are recorded.
- The run can be inspected without printing changed file contents.
- Agent-writable persistent mounts outside the exported rootfs are rejected.

**Acceptance:** A fixture command edits workspace files, commits in a discovered repository, installs a package, changes `/etc`, writes home-directory state, creates logs and caches, chmods and replaces files and symlinks, deletes a file, and exits with a known status. The result accounts for every persistent change except exactly those ignored by the fixture's declared rules.

**Hands-on checkpoint:** Run a harmless command through direct Docker against a chosen workspace and resolved `ubuntu:24.04` image that writes distinct files beneath `/workspace`, `/etc`, and a persistent home directory. Inspect the portable full-machine delta and Docker evidence, retain the container, and confirm that none of those writes reached the source workspace or host filesystem.

### Milestone 3: Prove isolation and comparable repetition

**Status:** Complete.

**Goal:** Launch multiple runs from the same resolved base and prove they cannot affect each other or the source workspace.

**Outcomes:**

- `agentlab run --snapshot DIGEST` loads and verifies an existing snapshot instead of rereading a mutable source directory.
- Two runs resolve to the same canonical run-input, workspace, image, and environment digests.
- Each receives a distinct private writable layer.
- Concurrent writes to identical paths do not cross between runs.
- Version-two run specifications contain a recomputable input digest and do not contain descriptive factor maps.
- Repeated runs reuse the same exact input digest; comparisons derive actual controlled-input differences from the specifications.
- Legacy version-one specifications remain readable, but their old factor maps do not affect input identity or reporting.

**Acceptance:** A repetition conformance test launches two commands concurrently from one stored snapshot, verifies identical run-input and portable-base identities, produces conflicting private changes, derives `comparable_repetition`, and confirms both the source and the other run remain untouched.

**Hands-on checkpoint:** Snapshot a chosen workspace once, launch two runs from that exact digest and image with conflicting writes to the same guest path, then inspect and compare them to show identical input/base identities, distinct private outcomes, and an unchanged source workspace.

### Milestone 4: Retain and manage session lifecycle

**Status:** Complete.

**Goal:** Preserve a Docker run long enough for inspection, harness continuation, filesystem-level fork, and later adoption.

**Outcomes:**

- Runs can be listed, inspected, stopped, restarted, resumed at the harness level, and deleted.
- Docker container identity and exact persistent filesystem state survive stop/start.
- Requested capture paths can export harness state outside `/workspace`.
- Backend-native state is clearly distinguished from the portable filesystem delta.
- A filesystem-level fork may commit/export the retained container and launch another container from that state.
- Live-process and memory resume are explicitly unsupported rather than simulated.
- Cleanup is explicit and recoverable where practical.

**Acceptance:** A Pi-, Codex-, or fixture-generated session file outside the workspace survives Docker stop/start and supports harness-level continuation without requiring core harness awareness. The result explicitly reports that process memory was not restored.

**Hands-on checkpoint:** Against a chosen workspace, retain a run that creates session-like state outside `/workspace`; list and inspect it, stop and restart it, demonstrate harness continuation from the same persistent state, create a filesystem-level fork, export the requested capture, and delete only the selected run's resources.

### Milestone 5: Support external evaluation

**Status:** Complete.

**Goal:** Make AgentLab runs useful for controlled agentic-environment experiments.

**Outcomes:**

- Run results are machine-readable and integrity-checked.
- An arbitrary evaluator command can consume a result.
- Example experiments demonstrate skill on/off and workspace-layout A/B through real snapshots; model or reasoning changes are real command/configuration inputs.
- Examples use repeated runs from each exact input and state the limits of nondeterministic model comparison.
- No evaluator is treated as universally authoritative.

**Acceptance:** A supplied example produces a table of actual run-input/workspace/image/base identities and evaluator scores without AgentLab understanding the semantic meaning of the treatment.

**Hands-on checkpoint:** Snapshot a chosen workspace, make one real file or directory treatment and snapshot again, run each exact input at least twice, then invoke the supplied external evaluator to produce an identity-and-score table and compare within-input repetition against the cross-treatment difference.

### Milestone 6: Add optional AI-assisted adoption

**Status:** Next implementation milestone.

**Goal:** Allow selected successful changes to improve a current workspace or future environment without weakening the core isolation model.

**Outcomes:**

- `agentlab adopt review RUN --workspace CURRENT -- COMMAND...` constructs an integrity-checked base/candidate/current review bundle and invokes an arbitrary reviewer adapter command.
- The adapter contract is harness-neutral: it receives request, manifest, delta, and materialized-tree paths through environment variables and emits one versioned JSON proposal. A thin wrapper can adapt any AI harness with a command-line invocation, including Pi, without making that harness part of AgentLab core.
- The host reviewer is explicitly trusted: it runs with the invoking user's authority and may receive sensitive captured content. Review mode promises only that AgentLab does not apply the proposal; it does not pretend to sandbox or make an arbitrary command non-mutating.
- The proposal anchors the exact run result, base workspace/image, freshly snapshotted current workspace, reviewer command, and every input artifact digest.
- Every candidate delta entry is accounted for exactly once as proposed, rejected, conflicted, or unresolved; semantic validation rejects traversal, duplicate dispositions, inconsistent counts, invalid base references, and operations outside the selected workspace.
- Repositories are discovered, not declared.
- Nonrepository changes use base/candidate/current comparison.
- Environment changes are evaluated across the entire machine delta, but initial automatic apply is limited to safely expressible workspace changes. Valuable non-workspace changes become explicit recommendations or proposed declarative image-source edits; they are never copied blindly from `/usr`, `/etc`, `/var`, caches, or credentials.
- `agentlab adopt apply REVIEW_ID --workspace CURRENT` is a separate explicit authorization. It resnapshots current, rejects stale proposals, blocks conflicts or unresolved entries by default, applies only receipt-authorized paths, preserves a recoverable patch/backup artifact, and verifies the resulting snapshot.
- Review and apply emit immutable records with reconciled proposed/rejected/conflicted/unresolved/applied counts and exact before/after identities.
- Adoption does not silently change the accepted baseline. Acceptance/promotion is a later explicit lineage decision.

**Acceptance:** From a run based on an older snapshot, advance a disposable current workspace independently, invoke a fixture reviewer through the public command adapter, validate a complete three-way proposal, prove review itself causes no AgentLab apply, reject a stale or unresolved apply, then explicitly apply an authorized nonconflicting workspace subset while leaving rejected and non-workspace changes untouched.

**Hands-on checkpoint:** Use a disposable copy of a chosen workspace to generate candidate workspace and machine changes, advance the copy independently, and run `agentlab adopt review` with a chosen CLI reviewer (for example a Pi adapter). Inspect the anchored proposed, rejected, conflicted, and unresolved dispositions. Apply the exact review ID only after explicit authorization, then verify the current workspace's before/after snapshots and the untouched non-workspace recommendations.

### Milestone 7: Prove the self-improvement loop

**Goal:** Launch a new isolated run from an improved workspace/environment produced through reviewed adoption.

**Outcomes:**

- One accepted input reference produces independent runs A and B.
- Selected A changes are adopted.
- The resulting workspace/image input is tested, reviewed, and explicitly accepted as a new content-addressed baseline reference.
- Run C begins from that improved base.
- C contains accepted improvements and excludes rejected or ignored session debris.
- All prior run evidence remains auditable.

**Acceptance:** A documented short sequence executes the complete accepted-input → isolated run → result → reviewed adoption → retest → explicit acceptance → improved run loop against a disposable fixture workspace, preserving its lineage.

**Hands-on checkpoint:** Run the documented accepted-input → candidate run → reviewed adoption → new snapshot → retest → explicit acceptance → improved run sequence against a disposable copy of a chosen workspace, and verify that the new run contains accepted improvements but not rejected or ignored debris. The developer's mutable host workspace remains the working tree throughout; “accepted” is a recorded immutable reference, not a hidden golden checkout.

### Milestone 8: Evaluate whether a second backend earns its complexity

**Goal:** Use evidence from the completed Docker implementation to decide whether Dagger or a VM backend materially improves AgentLab.

**Outcomes:**

- A time-boxed Dagger spike runs the existing conformance fixture without changing target workspace conventions or portable AgentLab manifests.
- The spike compares implementation size, correctness, performance, retained-session behavior, portability, operational dependencies, and failure modes with direct Docker.
- Dagger is adopted only if that comparison demonstrates material value; otherwise the rejection and evidence are documented and no production adapter is added.
- A Firecracker/E2B-compatible backend is considered separately when strong isolation, remote placement, memory snapshots, or cloud scale is a concrete requirement.
- Any later backend consumes the same workspace snapshot and emits the same portable delta and result semantics, while its native snapshot or checkpoint remains opaque backend evidence.

**Acceptance:** A decision record grounded in the working Docker implementation either justifies a second backend with measured advantages or explicitly defers it without changing AgentLab's portable protocol.

**Hands-on checkpoint:** Run the same safe fixture through direct Docker and the time-boxed Dagger spike if viable, compare their portable results and operational complexity, and record the keep-or-reject decision. Do not build a VM backend as part of this checkpoint unless a concrete strong-isolation or cloud requirement already exists.

### Milestone 9: Open-source readiness

**Goal:** Make AgentLab understandable, installable, safe to evaluate, and useful outside its originating workspace.

**Outcomes:**

- A 15-minute quickstart works on a fresh supported host.
- Security and privacy boundaries are documented.
- Ignore semantics have conformance fixtures.
- Result formats are versioned.
- Examples use generic fixture workspaces.
- CI runs unit and end-to-end direct-Docker tests.
- Conformance tests keep portable result assertions separate from Docker-specific evidence checks so a later implementation can reuse them.
- A second real workspace integrates without core modification.
- Release notes clearly distinguish stable protocol behavior from experimental backend features.

**Acceptance:** An unfamiliar user can run an isolated arbitrary command against an arbitrary directory, inspect every persistent guest change, repeat the experiment, and understand how to evaluate or optionally adopt the result.

**Hands-on checkpoint:** On a fresh supported host or clean test environment, follow only the public 15-minute quickstart against a workspace chosen by the tester, inspect the complete result, repeat the run, and exercise review-only adoption without project-specific setup.

## First end-to-end conformance scenario

Build one disposable fixture that deliberately contains:

- a non-Git workspace root;
- hidden files;
- a root `.gitignore`;
- nested `.gitignore` files;
- at least two Git repositories in unrelated paths;
- tracked, untracked, ignored, and explicitly unignored files;
- an ordinary large file;
- a symlink;
- a small testable task; and
- no real credentials.

The test command must:

1. Read the workspace from `/workspace`.
2. Modify, add, delete, and rename workspace paths.
3. Commit a change in one repository.
4. Leave another repository dirty.
5. Install or remove an OS package.
6. Modify an `/etc` file.
7. Create persistent home-directory state.
8. Create a large cache or log path.
9. Write a requested capture outside `/workspace`.
10. Replace regular-file content while preserving its size and modification time.
11. Change a file's permission mode.
12. Exit with a known status.

The test must prove:

- the source workspace is byte-identical afterward;
- two concurrent runs cannot observe one another's changes;
- workspace snapshot exclusions match `.gitignore` semantics;
- every persistent guest change is represented in the raw result;
- only declared `.agentlabignore` paths are omitted from the portable delta;
- Git observations require no declarations;
- run manifests have stable integrity hashes;
- retained Docker filesystem state survives stop/start and supports harness-level continuation;
- results distinguish filesystem resume from unsupported process or memory resume; and
- deletion removes only the exact run resources.

## Explicit non-goals for the first version

Do not build these until the minimal protocol is proven and a concrete need exists:

- cloud scheduler;
- PostgreSQL or Redis control plane;
- billing or quota system;
- web dashboard;
- hosted service;
- Kubernetes integration;
- automatic statistical conclusions;
- a universal workspace layout;
- a repository registry;
- a skill registry;
- a model or harness registry;
- a custom environment-definition language;
- cross-architecture live-memory migration;
- transparent secret management for every provider;
- automatic adoption without review; or
- infinite-scale infrastructure.

## Design pressure tests

Before accepting a new core concept, ask:

1. Is this a real controlled input, or only a label claiming what happened?
2. Can this be ordinary workspace/image preparation rather than a new AgentLab object or DSL?
3. Can this be an opaque command rather than a plugin?
4. Can this use an existing standard such as OCI, `.gitignore`, Git, tar, or JSON?
5. Can this be derived by discovery or content identity rather than declared in configuration?
6. Is this required for isolated execution and observation, or is it optional adoption/evaluation convenience?
7. Does this impose one person's preferred workspace layout?
8. Does this make the Docker proof unnecessarily resemble the future cloud system or prematurely generalize one backend's lifecycle?
9. Can the result remain truthful when a backend lacks the capability?

If a feature fails these tests, keep it out of the core until evidence requires it.

## Final definition of done

AgentLab's first coherent release is complete when a user can run:

```bash
cd /path/to/any/workspace

SNAPSHOT=$(agentlab snapshot --workspace . --json | jq -r .digest)

agentlab run \
  --snapshot "$SNAPSHOT" \
  --image ubuntu:24.04 \
  -- \
  bash -lc 'perform-the-task'
```

and rely on all of the following:

1. Every workspace path is included unless excluded through Git-compatible ignore rules.
2. No workspace layout or repository location is prescribed.
3. The source workspace is never writable from the run.
4. The run receives a private machine and workspace.
5. Every persistent machine change is captured by default.
6. Only explicit Git-compatible change-ignore rules suppress portable changes.
7. Independent runs from the same base cannot affect one another.
8. The workspace and environment inputs are content-addressed.
9. The harness command is arbitrary, while every compared treatment is represented by a real recorded input.
10. Results are complete, structured, integrity-checked, and inspectable without revealing contents by default.
11. Retained runs survive Docker stop/start and can be managed without confusing filesystem continuation with live-memory resume.
12. External evaluators can score repeated executions and reports expose their actual input identities.
13. Optional adoption can use any command-line reviewer to evaluate base, candidate, and current state, then selectively improve future runs through a separate explicit apply.
14. A structurally different workspace works without modifying AgentLab core code.

That is AgentLab's durable primitive:

> Content-addressed inputs, isolated agent execution, complete machine-change capture, scientifically comparable results, and optional intelligent adoption.
