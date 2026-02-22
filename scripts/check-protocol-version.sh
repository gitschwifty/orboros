#!/usr/bin/env bash
set -euo pipefail

ORBOROS_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HEDDLE_ROOT="/Users/pjtaggart/repos/heddle/main"

if [[ ! -f "$ORBOROS_ROOT/PROTOCOL_VERSION" ]]; then
  echo "Missing PROTOCOL_VERSION in Orboros." >&2
  exit 1
fi

if [[ ! -f "$HEDDLE_ROOT/PROTOCOL_VERSION" ]]; then
  echo "Missing PROTOCOL_VERSION in Heddle." >&2
  exit 1
fi

ORBOROS_VER="$(cat "$ORBOROS_ROOT/PROTOCOL_VERSION" | tr -d ' \t\n\r')"
HEDDLE_VER="$(cat "$HEDDLE_ROOT/PROTOCOL_VERSION" | tr -d ' \t\n\r')"

if [[ -z "$ORBOROS_VER" || -z "$HEDDLE_VER" ]]; then
  echo "Empty PROTOCOL_VERSION file(s)." >&2
  exit 1
fi

ORBOROS_MAJOR="${ORBOROS_VER%%.*}"
HEDDLE_MAJOR="${HEDDLE_VER%%.*}"

if [[ "$ORBOROS_MAJOR" != "$HEDDLE_MAJOR" ]]; then
  echo "Protocol MAJOR mismatch: Orboros=$ORBOROS_VER Heddle=$HEDDLE_VER" >&2
  exit 1
fi

if [[ "$ORBOROS_VER" != "$HEDDLE_VER" ]]; then
  echo "Protocol version differs (minor/patch): Orboros=$ORBOROS_VER Heddle=$HEDDLE_VER" >&2
fi
