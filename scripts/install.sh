#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This installer currently supports macOS only." >&2
  exit 1
fi

install_dir="${HOME}/.local/bin"
if [[ ! -d "${install_dir}" ]]; then
  echo "Install directory does not exist: ${install_dir}" >&2
  echo "Create it first with: mkdir -p ${install_dir}" >&2
  exit 1
fi

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary="${repo_dir}/target/release/orboros"

cargo build --release --bin orboros --manifest-path "${repo_dir}/Cargo.toml"
cp "${binary}" "${install_dir}/orboros"
chmod +x "${install_dir}/orboros"

echo "Installed Orboros to ${install_dir}/orboros"
