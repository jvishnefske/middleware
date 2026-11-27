#!/usr/bin/env bash
# Push a local directory tree (default ~/sw) into an Artifactory repository
# with the JFrog CLI (`jf`). Paths are preserved under the target prefix.
#
#   scripts/push-sw-artifactory.sh [--dry-run] [--repo REPO] [--prefix PATH] [--src DIR] [--server-id ID]
#
# Env (instead of flags): SW_SRC, SW_REPO, SW_PREFIX, JF_SERVER_ID.
#
# Auth is whatever `jf config add` stored for the server id. Create it once:
#   jf config add artifactory --url https://<host>/artifactory --access-token <token>
# (or --user/--password). This script never reads or prints credentials.
set -euo pipefail

SRC="${SW_SRC:-$HOME/sw}"
REPO="${SW_REPO:-}"
PREFIX="${SW_PREFIX:-}"
SERVER_ID="${JF_SERVER_ID:-}"
DRY_RUN=()

usage() { sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)   DRY_RUN=(--dry-run=true); shift ;;
    --repo)      REPO="$2"; shift 2 ;;
    --prefix)    PREFIX="$2"; shift 2 ;;
    --src)       SRC="$2"; shift 2 ;;
    --server-id) SERVER_ID="$2"; shift 2 ;;
    -h|--help)   usage 0 ;;
    *) echo "push-sw: unknown argument $1" >&2; usage 2 ;;
  esac
done

die() { echo "push-sw: $*" >&2; exit 1; }

command -v jf >/dev/null || die "jf (JFrog CLI) is not on PATH; install it from https://docs.jfrog-applications.jfrog.io/jfrog-applications/jfrog-cli"
[[ -n "$REPO" ]] || die "no repository: pass --repo NAME (e.g. a generic-local repo on the server)"
[[ -d "$SRC" ]] || die "source $SRC is not a directory"
[[ "$REPO" =~ ^[A-Za-z0-9._-]+$ ]] || die "repository name '$REPO' has unexpected characters"

server_args=()
if [[ -n "$SERVER_ID" ]]; then
  server_args=(--server-id "$SERVER_ID")
fi

# Show what the CLI will see before it sends anything.
files=$(find "$SRC" -type f | wc -l)
bytes=$(du -sh "$SRC" | cut -f1)
echo "push-sw: $files files, $bytes from $SRC -> $REPO/${PREFIX}"
[[ ${#DRY_RUN[@]} -eq 0 ]] || echo "push-sw: dry run, nothing will be uploaded"

# Upload from inside SRC so the stored paths are relative to it.
# --flat=false keeps the directory layout; --recursive=true walks subdirectories.
cd "$SRC"
jf rt upload "**/*" "$REPO/${PREFIX}" \
  --flat=false \
  --recursive=true \
  --retries=3 \
  "${server_args[@]}" \
  "${DRY_RUN[@]}"

echo "push-sw: done"
