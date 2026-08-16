# Dogfooding development builds

This is the deliberately small development channel for using completed AgentLab milestones on a Linux host before formal release packaging exists.

It provides:

- a clean checkout of `main` as the source of truth;
- a native Linux release build inside the host's selected rootless Docker engine;
- persistent Cargo and target caches in rootless Docker volumes;
- an atomic install at `~/.local/bin/agentlab`;
- one recoverable previous binary at `~/.local/bin/agentlab.previous`; and
- the exact source commit in `agentlab --version` and every new run-input identity.

It does not automatically update a server. The developer explicitly chooses when to take a new build.

## First install

The host needs Git, a working rootless Docker context, and `~/.local/bin` in `PATH`. Rust does not need to be installed on the host.

```bash
git clone git@github.com:dispatchlabs-ai/agentlab.git ~/Development/agentlab
cd ~/Development/agentlab
./scripts/install-dev.sh
agentlab --version
```

The installer refuses a dirty checkout. It mounts the source read-only, builds through a multi-architecture Rust image, verifies the produced executable, preserves the prior executable, and then swaps the new executable into place.

`AGENTLAB_BUILDER_IMAGE` overrides the default `rust:1.92-bookworm` builder. `AGENTLAB_INSTALL_DIR` overrides `~/.local/bin`. A non-rootless Docker engine is rejected unless `AGENTLAB_ALLOW_ROOTFUL_DOCKER=1` is deliberately set.

## Take the latest completed work

The install creates an `agentlab-update` symlink. Run:

```bash
agentlab-update
agentlab --version
```

The updater requires the dogfood checkout to be clean and on `main`, performs only a fast-forward pull from `origin/main`, and invokes the same installer. It never resets local work.

## Roll back the executable

If the newest development build is unusable, the immediately preceding executable remains available:

```bash
~/.local/bin/agentlab.previous --version
```

To make it active again:

```bash
install -m 0755 ~/.local/bin/agentlab.previous ~/.local/bin/agentlab
```

Existing `~/.agentlab` data is not removed by install, update, or executable rollback. Continue to use `agentlab inspect --verify` before relying on older retained results after a development update.
