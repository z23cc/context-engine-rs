#!/usr/bin/env bash
#
# build-all.sh — the canonical, reproducible multi-surface build for Nerve.
#
# WHY THE ORDER MATTERS (this is the load-bearing part):
#
#   The `nerve` binary's daemon serves the web GUI from bytes baked into the
#   executable at compile time: `crates/nerve-workstation/src/daemon/app.rs`
#   `include_bytes!`'s `crates/nerve-gui/dist/nerve-gui_bg.wasm` (and the rest of
#   `dist/`). Those `dist/` artifacts are ONLY produced by `trunk build` in
#   `crates/nerve-gui`. So the cargo build picks up whatever `dist/` currently
#   exists on disk — there is no cargo-level dependency that forces a fresh trunk
#   build. The coupling is invisible at the cargo layer and easy to get wrong.
#
#   Practical consequence: to ship a GUI change you must `trunk build` FIRST so
#   the committed `dist/` is fresh, THEN `cargo build` so the engine bakes in the
#   updated bytes. This script also re-runs cargo before staging the desktop
#   sidecar so the Tauri bundle embeds the freshly built engine, not a stale one.
#
#   Phase order:
#     1. cargo build --release (engine + TUI)        — baseline engine binary
#     2. trunk build (wasm GUI -> committed dist/)    — produces dist/ bytes
#     3. cargo build --release (engine) again         — bakes fresh dist/ into nerve
#     4. (optional) stage sidecar + tauri build       — desktop bundle
#
#   Steps 1 and 3 are the same `cargo build`; step 1 makes a binary available
#   even if trunk is unavailable, step 3 guarantees the engine embeds the GUI we
#   just built. cargo's incremental cache makes the second invocation cheap when
#   nothing but dist/ changed.
#
# The desktop (Tauri) phase is OPTIONAL and gated on tooling being present; it is
# never fatal when tauri/bun are absent.
#
# Usage:  bash Scripts/build-all.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

banner() {
  printf '\n=== [build-all] %s ===\n' "$1"
}

# ----------------------------------------------------------------------------
# Phase 1: engine + TUI (release)
# ----------------------------------------------------------------------------
banner "Phase 1/4: cargo build --release (engine + TUI)"
cargo build --release -p nerve-workstation -p nerve-tui

# ----------------------------------------------------------------------------
# Phase 2: wasm GUI -> committed dist/ (trunk)
# ----------------------------------------------------------------------------
banner "Phase 2/4: trunk build (wasm GUI -> crates/nerve-gui/dist/)"
if ! command -v trunk >/dev/null 2>&1; then
  echo "error: 'trunk' is required to build the wasm GUI (cargo install trunk)." >&2
  echo "       The nerve binary include_bytes!'s crates/nerve-gui/dist/, which only" >&2
  echo "       trunk produces. Aborting so we never ship a stale GUI." >&2
  exit 1
fi
(cd crates/nerve-gui && trunk build)

# ----------------------------------------------------------------------------
# Phase 3: re-bake the fresh dist/ into the engine
# ----------------------------------------------------------------------------
banner "Phase 3/4: cargo build --release (engine) — bake fresh dist/ into nerve"
cargo build --release -p nerve-workstation

# ----------------------------------------------------------------------------
# Phase 4: desktop bundle (OPTIONAL — gated on tauri/bun, never fatal)
# ----------------------------------------------------------------------------
banner "Phase 4/4: desktop bundle (optional)"

# Detect the host target triple from rustc (e.g. aarch64-apple-darwin).
TARGET_TRIPLE="$(rustc -vV | awk -F': ' '/^host:/ {print $2}')"
DESKTOP_DIR="$REPO_ROOT/apps/desktop"
SIDECAR_DIR="$DESKTOP_DIR/src-tauri/binaries"
SIDECAR_DST="$SIDECAR_DIR/nerve-${TARGET_TRIPLE}"
ENGINE_BIN="$REPO_ROOT/target/release/nerve"

# Pick a tauri driver if one is available.
TAURI_CMD=""
if command -v cargo-tauri >/dev/null 2>&1; then
  TAURI_CMD="cargo-tauri"
elif command -v bunx >/dev/null 2>&1; then
  TAURI_CMD="bunx tauri"
elif command -v npx >/dev/null 2>&1; then
  TAURI_CMD="npx tauri"
fi

if [[ -z "$TAURI_CMD" || ! -d "$DESKTOP_DIR" ]]; then
  echo "Skipping desktop bundle (non-fatal)."
  if [[ ! -d "$DESKTOP_DIR" ]]; then
    echo "  reason: apps/desktop is not present."
  else
    echo "  reason: no tauri driver found (cargo-tauri / bunx / npx tauri)."
  fi
  echo "  To build the desktop bundle manually:"
  echo "    1. Install tooling: 'cargo install tauri-cli --version ^2' (or 'bun install' in apps/desktop)."
  echo "    2. Stage the engine sidecar:"
  echo "         cp '$ENGINE_BIN' '$SIDECAR_DST'"
  echo "    3. Build the bundle (cwd apps/desktop/src-tauri):"
  echo "         (cd '$DESKTOP_DIR/src-tauri' && cargo tauri build)"
  echo "Done (engine + GUI built; desktop skipped)."
  exit 0
fi

# Tooling is present: stage the freshly built engine as the Tauri sidecar.
# Tauri's externalBin = "binaries/nerve" resolves per-target to nerve-<triple>.
echo "Staging engine sidecar: $ENGINE_BIN -> $SIDECAR_DST"
mkdir -p "$SIDECAR_DIR"
cp "$ENGINE_BIN" "$SIDECAR_DST"
chmod +x "$SIDECAR_DST"

echo "Running tauri build (driver: $TAURI_CMD)"
(cd "$DESKTOP_DIR/src-tauri" && $TAURI_CMD build)

banner "build-all complete (engine + GUI + desktop)"
