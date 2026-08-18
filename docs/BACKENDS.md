# Execution backends

AgentLab's public primitive is independent of a particular isolation provider:

```text
content-addressed workspace + immutable environment + opaque command
    -> isolated execution
    -> portable result and complete persistent-filesystem delta
```

Local Docker and remote E2B/Firecracker are implemented execution backends.
Docker remains the built-in default. A developer selects E2B with one ordinary
run option; the workspace and command do not need provider-specific logic.

## Developer experience

Run locally through Docker by omitting `--backend`:

```bash
agentlab run \
  --workspace ~/Development/agentlab-workspaces/daily-log \
  --image agentlab-daily-log:dev \
  --pi-auth \
  -- pi -p 'Describe this workspace in five concise bullets.'
```

Run the same input in a Dell Firecracker microVM through E2B:

```bash
agentlab run \
  --backend e2b-dell \
  --workspace ~/Development/agentlab-workspaces/daily-log \
  --image agentlab-daily-log:dev \
  --pi-auth \
  -- pi -p 'Describe this workspace in five concise bullets.'
```

That is the complete per-run E2B ceremony. AgentLab captures the workspace,
selects and verifies the configured immutable E2B build, reaches the Dell over
SSH, creates the Firecracker sandbox through E2B, transfers the private input,
injects command-scoped credentials, streams the command, revokes credentials,
captures exact base and result filesystems, retains immutable snapshots, and
returns normal AgentLab follow-up commands.

The SSH alias, E2B SDK paths, credentials, template mapping, and Firecracker
expectation are host configuration. They do not appear in every command. No raw
server address or user-authored SDK script is required.

## Backend configuration

Backend definitions live in private host configuration at
`~/.agentlab/config.toml`, never in the workspace under test:

```toml
version = 1

[backends.e2b-dell]
driver = "e2b"
transport = "ssh"
ssh_alias = "e2b-dell"
sdk_directory = "/home/chris/src/e2b-infra/packages/shared/scripts"
orchestrator_directory = "/home/chris/src/e2b-infra/packages/orchestrator"
remote_root = "/home/chris/.agentlab-e2b"
expected_isolation = "firecracker"

templates = { "agentlab-daily-log:dev" = "agentlab-daily-log:pi-0.86.9-example-disk4096" }
template_builds = { "agentlab-daily-log:dev" = "00000000-0000-4000-8000-000000000000" }
runtime_environments = { "agentlab-daily-log:dev" = { LANG = "C.UTF-8", LC_ALL = "C.UTF-8", TZ = "Etc/UTC", HOME = "/root", WORKSPACE_ROOT = "/workspace", PI_CODING_AGENT_SESSION_DIR = "/workspace/.pi/sessions", PI_SESSION_LOCK_PROTOCOL = "kernel-v1-drained", PATH = "/workspace/bin:/root/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" } }
```

The profile name is opaque. AgentLab never infers a driver from a name,
hostname, or SSH alias. `driver = "e2b"` selects the E2B SDK and Firecracker
path; a Docker daemon on the same server would be a separate Docker profile.
Omitting `default_backend` keeps local Docker as the default. Set
`default_backend = "e2b-dell"` only when every ordinary run on that machine
should use E2B.

Every requested image must have an explicit `templates` mapping. A
`template_builds` entry pins the mutable provider tag to one E2B build UUID;
AgentLab resolves the tag before every run and refuses to proceed if it no
longer names that build. The run records both the E2B template/build evidence
and its own content-derived environment identity.

`runtime_environments` contains non-secret runtime metadata required by a
compiled template. It is image-specific, included in the canonical run-input
identity, and retained in environment evidence. Never put passwords, API keys,
tokens, or other credentials there. Use `--pi-auth` or `--secret-file` for
secrets.

The current first E2B slice invokes the opaque guest command as `root`, because
E2B's filesystem-only checkpoint does not preserve Dockerfile `USER` metadata.
Its mapped template must therefore be root-ready and install the requested
harness on root's `PATH`. This is an explicit current limitation, not a claim
that every future E2B template must run as root.

The explicit runtime map is currently necessary because an E2B template is a
compiled filesystem snapshot: Dockerfile `ENV`, `ENTRYPOINT`, and other OCI
runtime metadata are not automatically restored by E2B after a
filesystem-only pause/resume. A future template-install command can derive and
write this mapping automatically. The run command will not change.

## OCI environment definition

The workspace root Dockerfile or Containerfile remains the portable source of
truth for its surrounding OS and tools. It should define a pinned base image,
packages, runtimes, agent harness, non-secret environment, and ordinary startup
contract. It must not copy the workspace or credentials into the image.
AgentLab places the separately snapshotted workspace into `/workspace` at run
time.

Docker consumes the OCI image directly. E2B compiles the same definition into
a provider-native template and records the immutable E2B build ID. The native
template is a cache/artifact, not a competing workspace definition.

Private build dependencies require a secret-safe template build process. The
validated Daily Log template was built from local private package tarballs;
the temporary registry credential was never copied into the build context,
template, sandbox, or AgentLab result. An OCI or template build must never bake
registry tokens, Pi OAuth, or runtime credentials into an immutable layer.

## What the E2B backend does

For each run, AgentLab:

1. Captures or verifies the complete local AgentLab workspace snapshot.
2. Resolves the configured E2B template tag and verifies its pinned build UUID.
3. Creates private mode-`0700` staging on the configured SSH host and deploys
   version-matched helper code.
4. Uses the Dell's existing E2B SDK credentials in its mode-`0600`
   `.env.local`; those credentials never move to the Mac.
5. Creates the sandbox through E2B with Firecracker isolation and the selected
   network policy.
6. Uploads the workspace through bounded 16 MiB chunks, verifies every chunk,
   and extracts it into the guest without mounting the host workspace.
7. Makes a filesystem-only E2B checkpoint and names an immutable base
   snapshot.
8. Injects Pi or named credential files under `/run`, runs the opaque command,
   captures bounded stdout/stderr, and removes all credential and runner paths.
9. Makes and names an immutable result checkpoint, then terminates the live
   sandbox.
10. Mounts both immutable builds read-only on the Dell, scans every supported
    rootfs path with no-follow descriptor traversal, and retains the exact
    content needed for portable changes and requested captures.
11. Verifies the source workspace remained unchanged and writes the normal
    integrity-protected AgentLab result.

Large inputs are transferred privately and with bounded memory; they are not
excluded merely because of size. Unsupported special files still fail
explicitly rather than disappearing from the experiment.

The filesystem-only checkpoint is deliberate. An ordinary full-memory
Firecracker snapshot can contain writes only in a live overlay that are not yet
visible in its mountable rootfs artifact. AgentLab first forces E2B's
filesystem-only boundary, reconnects, and then creates the named retained
snapshot. The bytes scanned for evidence therefore match the recorded base and
result boundaries.

## Networking, resources, and time limits

`--network bridge` is the default for both backends. For E2B this requests
internet/egress access through E2B; `--network none` disables it. AgentLab does
not expose the Dell's Firecracker socket, E2B API port, or sandbox proxy to the
developer.

E2B CPU, memory, and disk currently belong to the mapped template. The backend
rejects `--cpus` and `--memory` instead of pretending to translate Docker
limits. Choose or build a template with the required resources. The validated
Daily Log build has a 4 GiB data disk because the default sub-gigabyte image did
not have enough space for a 76.7 MB workspace plus Pi and observation data.

The current Dell service limits a sandbox to one hour. AgentLab therefore
limits the E2B guest command to 58 minutes, leaving time for credential cleanup
and snapshot boundaries. Local Docker commands retain AgentLab's 24-hour
fail-safe limit.

## Credentials and interruption

`--pi-auth` reads the invoking Mac's default `~/.pi/agent/auth.json`, validates
it, and transfers it only as a private command input. Inside the microVM it is
placed on `/run` and linked at Pi's expected home path while the command runs.
Named `--secret-file NAME=PATH` inputs use the same `/run/agentlab-secrets`
lease. Records contain stable injection names, not source paths, bytes, or
credential digests.

Before result capture, AgentLab removes the home link, credential files,
command request, runner, and output staging from the guest. The retained base
and result manifests are checked from immutable snapshots. An interrupted
remote helper handles `SIGINT`, `SIGTERM`, and SSH loss by killing the active
sandbox and deleting incomplete snapshots; the local failed run directory and
remote staging are also cleaned. The opaque guest command is still trusted with
any credential it receives and can deliberately print or copy it.

## Portable evidence and native evidence

AgentLab owns the portable workspace, run-input, base/result filesystem,
delta, review, apply, and acceptance identities. Provider-native evidence is
separate:

- Docker records OCI image, engine, container, layer, and diff evidence.
- E2B records SDK version, profile, template, build, sandbox, named base/result
  snapshots, and the asserted `firecracker` isolation boundary.

The backend driver and version are controlled run inputs because Docker and a
Firecracker microVM are materially different environments. Provider IDs never
replace AgentLab content identities.

Firecracker cold boots legitimately change system paths such as the random
seed, journal, `wtmp`, and systemd-private directories. AgentLab keeps these in
raw evidence. Presentation ignores and the optional diff agent can hide or
summarize routine churn for a human without deleting evidence.

## Supported lifecycle today

The following work for both Docker and E2B results:

```bash
agentlab list
agentlab inspect --verify RUN_ID
agentlab diff RUN_ID
agentlab compare RUN_A RUN_B
agentlab evaluate RUN_ID -- EVALUATOR
agentlab review RUN_ID --workspace CURRENT -- REVIEWER
agentlab apply REVIEW_ID --workspace CURRENT
agentlab accept RUN_ID
agentlab rm RUN_ID
```

An acceptance remembers the backend profile of its protected test run, so
`agentlab run --accepted ACCEPTANCE_ID -- COMMAND` selects that profile when
`--backend` is omitted and then re-verifies the exact resolved environment.
The recorded test run remains protected while the acceptance exists, which is
what makes that profile lookup auditable rather than ambient.

For E2B, `run` terminates the live microVM after retaining immutable base and
result snapshots. `list` reports the retained snapshot, `inspect --verify`
checks all portable and provider evidence, and `rm` verifies build binding
before deleting both remote snapshots and local artifacts.

`stop`, `resume`, and `fork` are currently Docker-only. The E2B CLI does not
advertise them after a run and rejects them explicitly if requested. This is a
truthful capability boundary, not an implication that process memory or a live
microVM remains available. Filesystem continuation from the retained result
snapshot is the next lifecycle slice.

## Dell prerequisites

The configured Dell profile assumes:

- `ssh e2b-dell` succeeds with key-only, noninteractive authentication;
- E2B API, client proxy, orchestrator, template manager, and Firecracker are
  healthy on the Dell;
- `sdk_directory/.env.local` contains the Dell's E2B development credentials
  and can be restricted to mode `0600`;
- the SSH user has narrowly configured noninteractive access required to mount
  immutable E2B build root filesystems for read-only evidence;
- the configured immutable template exists and its tag resolves to the pinned
  build UUID; and
- the template has sufficient CPU, memory, and disk for the selected workspace.

Ordinary AgentLab runs use the SSH alias and the remote SDK. They do not need a
local API tunnel or a copy of E2B credentials on the Mac. Server hardening
should keep E2B control ports off the general LAN and expose only the intended
key-authenticated SSH path.

## Validated Daily Log run

On 2026-08-18, the implemented backend—not a standalone SDK script—ran the
curated Daily Log workspace from the Mac through Dell E2B 2.38.0 in an x86-64
Firecracker microVM:

- 1,019 paths and 76,707,637 logical bytes were captured with zero exclusions;
- the immutable workspace digest was
  `sha256:9fe98c6aae8b1c5d8889a9e92eef9b8a6a09f71eb2f1a1b67cb7ced51a708bc0`;
- the private workspace reached the microVM through the bounded upload path;
- Pi 0.86.9 received command-scoped OAuth and answered the prompt successfully;
- credential and runtime-control paths were absent from both retained rootfs
  manifests;
- the source workspace was unchanged;
- `inspect --verify`, raw diff, and agent-reduced diff passed; and
- the complete fidelity-corrected run finalized in 49.8 seconds with immutable
  base and result E2B snapshots.

This proves the first useful E2B backend slice. It does not claim that E2B
stop/resume/fork or automatic Dockerfile-to-template installation is complete.
