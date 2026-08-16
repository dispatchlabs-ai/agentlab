#!/usr/bin/env bash
set -euo pipefail

die() {
  printf 'agentlab install: %s\n' "$*" >&2
  exit 1
}

for command_name in docker git install mktemp realpath; do
  command -v "$command_name" >/dev/null 2>&1 || die "$command_name is required"
done

[[ "$(uname -s)" == "Linux" ]] || die "this development installer currently supports Linux hosts"

script_path="$(realpath "${BASH_SOURCE[0]}")"
repository_dir="$(cd "$(dirname "$script_path")/.." && pwd -P)"
install_dir="${AGENTLAB_INSTALL_DIR:-$HOME/.local/bin}"
builder_image="${AGENTLAB_BUILDER_IMAGE:-rust:1.92-bookworm}"

git -C "$repository_dir" diff --quiet --ignore-submodules -- \
  || die "the checkout has unstaged changes; commit or stash them before installing"
git -C "$repository_dir" diff --cached --quiet --ignore-submodules -- \
  || die "the checkout has staged changes; commit or stash them before installing"
[[ -z "$(git -C "$repository_dir" ls-files --others --exclude-standard)" ]] \
  || die "the checkout has untracked files; commit, ignore, or remove them before installing"

commit="$(git -C "$repository_dir" rev-parse --verify HEAD)"
build_id="git.${commit}"

docker_platform="$(docker info --format '{{.OSType}}/{{.Architecture}}' 2>/dev/null)" \
  || die "Docker is not reachable; start the rootless user service and select its context"
[[ "$docker_platform" == linux/* ]] || die "the Docker engine must build Linux containers"

security_options="$(docker info --format '{{json .SecurityOptions}}')"
if [[ "$security_options" != *rootless* && "${AGENTLAB_ALLOW_ROOTFUL_DOCKER:-0}" != "1" ]]; then
  die "the selected Docker engine is not rootless (set AGENTLAB_ALLOW_ROOTFUL_DOCKER=1 to override deliberately)"
fi

case "${docker_platform#linux/}" in
  arm64 | aarch64) cache_arch="arm64" ;;
  amd64 | x86_64) cache_arch="amd64" ;;
  *) die "unsupported Docker architecture ${docker_platform#linux/}" ;;
esac

temporary_dir="$(mktemp -d "${TMPDIR:-/tmp}/agentlab-install.XXXXXXXX")"
cleanup() {
  if [[ -n "${temporary_dir:-}" && -d "$temporary_dir" ]]; then
    rm -f -- "$temporary_dir/agentlab"
    rmdir -- "$temporary_dir" 2>/dev/null || true
  fi
}
trap cleanup EXIT

printf 'Building AgentLab %s for %s with %s...\n' "$build_id" "$docker_platform" "$builder_image"
docker run --rm --pull=missing \
  --env "AGENTLAB_BUILD_ID=$build_id" \
  --env CARGO_HOME=/cargo \
  --env CARGO_TARGET_DIR=/target \
  --volume "$repository_dir:/src:ro" \
  --volume "agentlab-dev-cargo-registry:/cargo/registry" \
  --volume "agentlab-dev-cargo-git:/cargo/git" \
  --volume "agentlab-dev-target-$cache_arch:/target" \
  --volume "$temporary_dir:/out" \
  --workdir /src \
  "$builder_image" \
  sh -lc 'cargo build --release --locked && install -m 0755 /target/release/agentlab /out/agentlab'

"$temporary_dir/agentlab" --version

mkdir -p "$install_dir"
destination="$install_dir/agentlab"
pending="$install_dir/.agentlab.new.$$"
install -m 0755 "$temporary_dir/agentlab" "$pending"
if [[ -f "$destination" ]]; then
  install -m 0755 "$destination" "$install_dir/agentlab.previous"
fi
mv -f "$pending" "$destination"

update_link="$install_dir/agentlab-update"
if [[ ! -e "$update_link" || -L "$update_link" ]]; then
  ln -sfn "$repository_dir/scripts/update-dev.sh" "$update_link"
else
  printf 'Not replacing existing non-symlink %s\n' "$update_link" >&2
fi

printf 'Installed %s\n' "$destination"
printf 'Update later with %s\n' "$update_link"
if [[ ":$PATH:" != *":$install_dir:"* ]]; then
  printf 'Add %s to PATH to invoke agentlab directly.\n' "$install_dir"
fi
