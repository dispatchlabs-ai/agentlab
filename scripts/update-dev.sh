#!/usr/bin/env bash
set -euo pipefail

die() {
  printf 'agentlab update: %s\n' "$*" >&2
  exit 1
}

command -v git >/dev/null 2>&1 || die "git is required"
command -v realpath >/dev/null 2>&1 || die "realpath is required"

script_path="$(realpath "${BASH_SOURCE[0]}")"
repository_dir="$(cd "$(dirname "$script_path")/.." && pwd -P)"

[[ "$(git -C "$repository_dir" branch --show-current)" == "main" ]] \
  || die "the dogfood checkout must be on main"
git -C "$repository_dir" diff --quiet --ignore-submodules -- \
  || die "the checkout has unstaged changes"
git -C "$repository_dir" diff --cached --quiet --ignore-submodules -- \
  || die "the checkout has staged changes"
[[ -z "$(git -C "$repository_dir" ls-files --others --exclude-standard)" ]] \
  || die "the checkout has untracked files"

printf 'Updating %s from origin/main...\n' "$repository_dir"
git -C "$repository_dir" pull --ff-only origin main
exec "$repository_dir/scripts/install-dev.sh"
