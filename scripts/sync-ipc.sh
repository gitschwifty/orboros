#!/usr/bin/env bash
set -euo pipefail

ORBOROS_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HEDDLE_ROOT="/Users/pjtaggart/repos/heddle/main"

if [[ ! -d "$HEDDLE_ROOT" ]]; then
  echo "Heddle repo not found at $HEDDLE_ROOT" >&2
  exit 1
fi

mkdir -p "$HEDDLE_ROOT/test/ipc/fixtures"

rsync -a --delete "$ORBOROS_ROOT/fixtures/ipc/" "$HEDDLE_ROOT/test/ipc/fixtures/"
rsync -a "$ORBOROS_ROOT/compatibility.md" "$HEDDLE_ROOT/compatibility.md"
rsync -a "$ORBOROS_ROOT/PROTOCOL_VERSION" "$HEDDLE_ROOT/PROTOCOL_VERSION"

echo "Synced IPC fixtures and compatibility policy to Heddle."

# Fail only if there are unstaged changes after sync.
if [[ -n "$(git -C "$HEDDLE_ROOT" diff --name-only)" ]]; then
  echo "IPC sync produced unstaged changes in Heddle. Run git add and commit there." >&2
  exit 1
fi
