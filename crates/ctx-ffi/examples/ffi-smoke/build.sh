#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
REPO_ROOT="$(cd "$CRATE_DIR/../.." && pwd)"
CC_BIN="${CC_BIN:-/usr/bin/cc}"

if [[ ! -x "$CC_BIN" ]]; then
  echo "C compiler not executable: $CC_BIN" >&2
  exit 1
fi

VERSION_OUTPUT="$($CC_BIN --version 2>&1 | head -n 1)"
case "$VERSION_OUTPUT" in
  *"Apple clang"*|*"clang"*) ;;
  *)
    echo "Expected Apple clang/clang, got: $VERSION_OUTPUT" >&2
    exit 1
    ;;
esac

echo "cc: $VERSION_OUTPUT"
cargo build --release -p ctx-ffi

mkdir -p "$REPO_ROOT/target/ffi-smoke"
"$CC_BIN" \
  -I"$CRATE_DIR/include" \
  "$SCRIPT_DIR/main.c" \
  -L"$REPO_ROOT/target/release" \
  -lctx_engine \
  -Wl,-rpath,"$REPO_ROOT/target/release" \
  -o "$REPO_ROOT/target/ffi-smoke/ctx-ffi-smoke"

"$REPO_ROOT/target/ffi-smoke/ctx-ffi-smoke" "$REPO_ROOT/crates"
