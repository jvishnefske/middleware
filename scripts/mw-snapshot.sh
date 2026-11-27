#!/usr/bin/env bash
# Copies this working tree (tracked and untracked files that git does not
# ignore, minus build output) to <dir> and prints <dir>: a clean source for a
# `path:` flake input. The tree itself cannot be one: `target/` directories and
# other build output would be copied into the Nix store.
#
# Usage: scripts/mw-snapshot.sh <dir>
set -euo pipefail
out=${1:?usage: mw-snapshot.sh <dir>}
cd "$(dirname "$0")/.."
rm -rf "$out"
mkdir -p "$out"
git ls-files -co --exclude-standard -z \
  | grep -zvE '(^|/)(target|__pycache__|node_modules|result[^/]*)(/|$)|^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+/' \
  | while IFS= read -r -d '' f; do [ -e "$f" ] && printf '%s\0' "$f"; done \
  | rsync -a --from0 --files-from=- ./ "$out"/
echo "$out"
