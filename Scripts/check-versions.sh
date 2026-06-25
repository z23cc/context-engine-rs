#!/usr/bin/env bash
#
# check-versions.sh — fail-closed guard against desktop-vs-engine version drift.
#
# The engine workspace version is the single source of truth: root Cargo.toml
# [workspace.package] version. The Tauri desktop shell (apps/desktop) lives in
# its OWN cargo workspace, so it does NOT inherit that version and must be kept
# in lockstep by hand (Scripts/release.sh does this on bump). This script reads
# the engine version plus the three desktop version strings and exits non-zero
# with a clear diff if any of the four disagree.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Engine: [workspace.package] version in root Cargo.toml.
read_workspace_version() {
  awk '
    /^\[workspace\.package\]/ { inpkg=1; next }
    /^\[/                     { inpkg=0 }
    inpkg && /^version[[:space:]]*=/ { gsub(/[^0-9.]/, ""); print; exit }
  ' Cargo.toml
}

# First top-level "version": "..." in a JSON file (tauri.conf.json / package.json).
read_json_version() {
  awk -F'"' '/"version"[[:space:]]*:/ { print $4; exit }' "$1"
}

# [package] version in apps/desktop/src-tauri/Cargo.toml (first version = "..." line).
read_cargo_pkg_version() {
  awk '
    /^version[[:space:]]*=/ { gsub(/[^0-9.]/, ""); print; exit }
  ' "$1"
}

ENGINE="$(read_workspace_version)"
TAURI="$(read_json_version apps/desktop/src-tauri/tauri.conf.json)"
PKGJSON="$(read_json_version apps/desktop/package.json)"
CARGO="$(read_cargo_pkg_version apps/desktop/src-tauri/Cargo.toml)"

fail=0
for pair in \
  "engine (Cargo.toml [workspace.package])=$ENGINE" \
  "tauri.conf.json=$TAURI" \
  "package.json=$PKGJSON" \
  "desktop Cargo.toml [package]=$CARGO"; do
  label="${pair%%=*}"; value="${pair#*=}"
  [[ -n "$value" ]] || { echo "error: could not read version from $label"; fail=1; }
done
[[ "$fail" -eq 0 ]] || exit 1

if [[ "$ENGINE" == "$TAURI" && "$ENGINE" == "$PKGJSON" && "$ENGINE" == "$CARGO" ]]; then
  echo "OK: all versions agree at $ENGINE"
  exit 0
fi

echo "error: version drift detected — the four versions disagree:"
echo "  engine  (Cargo.toml [workspace.package]) : $ENGINE"
echo "  desktop tauri.conf.json                  : $TAURI"
echo "  desktop package.json                     : $PKGJSON"
echo "  desktop src-tauri/Cargo.toml [package]   : $CARGO"
echo
echo "fix: set all four to the engine version ($ENGINE), e.g. via Scripts/release.sh on bump."
exit 1
