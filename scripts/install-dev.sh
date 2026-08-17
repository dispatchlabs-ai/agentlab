#!/usr/bin/env bash
set -euo pipefail

die() {
  printf 'agentlab install: %s\n' "$*" >&2
  exit 1
}

allow_dirty="${AGENTLAB_ALLOW_DIRTY:-0}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --allow-dirty) allow_dirty=1 ;;
    --help | -h)
      printf 'usage: %s [--allow-dirty]\n' "$0"
      exit 0
      ;;
    *) die "unknown argument $1" ;;
  esac
  shift
done

for command_name in git install mktemp realpath; do
  command -v "$command_name" >/dev/null 2>&1 || die "$command_name is required"
done

script_path="$(realpath "${BASH_SOURCE[0]}")"
repository_dir="$(cd "$(dirname "$script_path")/.." && pwd -P)"
install_dir="${AGENTLAB_INSTALL_DIR:-$HOME/.local/bin}"
builder_image="${AGENTLAB_BUILDER_IMAGE:-rust:1.92-bookworm}"

dirty=0
git -C "$repository_dir" diff --quiet --ignore-submodules -- || dirty=1
git -C "$repository_dir" diff --cached --quiet --ignore-submodules -- || dirty=1
[[ -z "$(git -C "$repository_dir" ls-files --others --exclude-standard)" ]] || dirty=1
if [[ "$dirty" == 1 && "$allow_dirty" != 1 ]]; then
  die "the checkout has local changes; commit or stash them, or install deliberately with --allow-dirty"
fi

commit="$(git -C "$repository_dir" rev-parse --verify HEAD)"
build_id="git.${commit}"
if [[ "$dirty" == 1 ]]; then
  for command_name in shasum; do
    command -v "$command_name" >/dev/null 2>&1 || die "$command_name is required for dirty build identity"
  done
  dirty_digest="$({
    git -C "$repository_dir" diff --binary HEAD --
    while IFS= read -r -d '' untracked_path; do
      printf '\0untracked\0%s\0' "$untracked_path"
      shasum -a 256 "$repository_dir/$untracked_path"
    done < <(git -C "$repository_dir" ls-files --others --exclude-standard -z)
  } | shasum -a 256 | awk '{print substr($1, 1, 12)}')"
  build_id="${build_id}.dirty.${dirty_digest}"
  printf 'Installing a deliberately dirty development build: %s\n' "$build_id" >&2
fi

host_os="$(uname -s)"
case "$host_os" in
  Linux)
    for command_name in docker; do
      command -v "$command_name" >/dev/null 2>&1 || die "$command_name is required"
    done

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
    ;;
  Darwin)
    command -v cargo >/dev/null 2>&1 || die "cargo is required for native macOS builds"
    ;;
  *) die "unsupported development host $host_os" ;;
esac

temporary_dir="$(mktemp -d "${TMPDIR:-/tmp}/agentlab-install.XXXXXXXX")"
cleanup() {
  if [[ -n "${temporary_dir:-}" && -d "$temporary_dir" ]]; then
    rm -f -- "$temporary_dir/agentlab"
    rmdir -- "$temporary_dir" 2>/dev/null || true
  fi
}
trap cleanup EXIT

case "$host_os" in
  Linux)
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
      sh -c 'cargo build --release --locked && install -m 0755 /target/release/agentlab /out/agentlab'
    ;;
  Darwin)
    printf 'Building AgentLab %s natively for macOS...\n' "$build_id"
    (
      cd "$repository_dir"
      AGENTLAB_BUILD_ID="$build_id" cargo build --release --locked
    )
    install -m 0755 "$repository_dir/target/release/agentlab" "$temporary_dir/agentlab"
    ;;
esac

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
