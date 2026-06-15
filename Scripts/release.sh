#!/usr/bin/env bash
#
# release.sh — cut a release and push the Homebrew formula.
#
# Versioning scheme: start at 0.0.1, increment the patch by +0.0.1 each release
# (0.0.1 -> 0.0.2 -> 0.0.3 ...). The workspace version lives in one place:
# the root Cargo.toml [workspace.package] version (all crates inherit it).
#
# Distribution:
#   * macOS arm64 -> a prebuilt binary asset (no compiler needed on install).
#   * everything else (Intel macOS, Linux) -> build from source via cargo.
# When this script runs on an arm64 Mac it builds and attaches the prebuilt
# binary and emits the dual formula; otherwise it emits a source-only formula.
#
# Usage:
#   Scripts/release.sh            # bump +0.0.1, then release
#   Scripts/release.sh --current  # release the CURRENT version, no bump
#
set -euo pipefail

OWNER="z23cc"
REPO="context-engine-rs"
TAP_REPO="homebrew-tap"   # Homebrew requires the "homebrew-" prefix
FORMULA="ctx-mcp"
BIN="ctx-mcp"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

bump=true
[[ "${1:-}" == "--current" ]] && bump=false

# ---- preconditions ----
command -v gh >/dev/null    || { echo "error: gh CLI not found"; exit 1; }
command -v cargo >/dev/null || { echo "error: cargo not found"; exit 1; }
gh auth status >/dev/null 2>&1 || { echo "error: gh not authenticated"; exit 1; }

branch="$(git rev-parse --abbrev-ref HEAD)"
[[ "$branch" == "main" ]] || { echo "error: not on main (on '$branch')"; exit 1; }

if [[ -n "$(git status --porcelain)" ]]; then
  echo "error: working tree not clean — commit or stash first"; exit 1
fi

read_version() {
  awk '
    /^\[workspace\.package\]/ { inpkg=1; next }
    /^\[/                     { inpkg=0 }
    inpkg && /^version[[:space:]]*=/ { gsub(/[^0-9.]/, ""); print; exit }
  ' Cargo.toml
}

CUR="$(read_version)"
[[ -n "$CUR" ]] || { echo "error: could not read [workspace.package] version"; exit 1; }

if $bump; then
  IFS=. read -r MA MI PA <<<"$CUR"
  NEW="$MA.$MI.$((PA + 1))"
else
  NEW="$CUR"
fi
TAG="v$NEW"

echo ">> current version : $CUR"
echo ">> release version : $NEW  (tag $TAG)"

if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  echo "error: tag $TAG already exists locally"; exit 1
fi
if gh release view "$TAG" -R "$OWNER/$REPO" >/dev/null 2>&1; then
  echo "error: release $TAG already exists on GitHub"; exit 1
fi

# ---- bump commit ----
if $bump; then
  awk -v new="$NEW" '
    /^\[workspace\.package\]/ { inpkg=1; print; next }
    /^\[/ && !/^\[workspace\.package\]/ { inpkg=0 }
    inpkg && /^version[[:space:]]*=/ { print "version = \"" new "\""; next }
    { print }
  ' Cargo.toml >Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml
  cargo update --workspace >/dev/null
  git add Cargo.toml Cargo.lock
  git commit -m "release: $TAG"
fi

# ---- tag + push ----
git tag -a "$TAG" -m "$TAG"
git push origin main
git push origin "$TAG"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# ---- source tarball (Linux / Intel fallback) ----
SRC_TARBALL="$TMP/${REPO}-${NEW}.tar.gz"
git archive --format=tar --prefix="${REPO}-${NEW}/" "$TAG" | gzip -n >"$SRC_TARBALL"
SRC_SHA="$(shasum -a 256 "$SRC_TARBALL" | awk '{print $1}')"
SRC_URL="https://github.com/$OWNER/$REPO/releases/download/$TAG/${REPO}-${NEW}.tar.gz"
echo ">> source sha256   : $SRC_SHA"

# ---- prebuilt macOS arm64 binary (only when building on an arm64 Mac) ----
HAS_MAC=0
ASSETS=("$SRC_TARBALL")
if [[ "$(uname -s)" == "Darwin" && "$(uname -m)" == "arm64" ]]; then
  echo ">> building macOS arm64 binary"
  cargo build --release -p ctx-mcp
  STAGE="$TMP/stage"; mkdir -p "$STAGE"
  cp "target/release/$BIN" "$STAGE/$BIN"
  strip "$STAGE/$BIN" 2>/dev/null || true
  MAC_TARBALL="$TMP/${BIN}-${NEW}-aarch64-apple-darwin.tar.gz"
  tar -czf "$MAC_TARBALL" -C "$STAGE" "$BIN"
  MAC_SHA="$(shasum -a 256 "$MAC_TARBALL" | awk '{print $1}')"
  MAC_URL="https://github.com/$OWNER/$REPO/releases/download/$TAG/${BIN}-${NEW}-aarch64-apple-darwin.tar.gz"
  HAS_MAC=1
  ASSETS+=("$MAC_TARBALL")
  echo ">> macos arm sha256: $MAC_SHA"
fi

# ---- github release (source + any prebuilt assets) ----
gh release create "$TAG" "${ASSETS[@]}" \
  -R "$OWNER/$REPO" \
  --title "$TAG" \
  --notes "Automated release $TAG. Install: \`brew install $OWNER/tap/$FORMULA\`"

# ---- formula generator ----
gen_formula() {
  if [[ "$HAS_MAC" == "1" ]]; then
    cat <<EOF
class CtxMcp < Formula
  desc "Minimal snapshot-centered context engine (MCP server over stdio)"
  homepage "https://github.com/$OWNER/$REPO"
  url "$SRC_URL"
  version "$NEW"
  sha256 "$SRC_SHA"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "$MAC_URL"
      sha256 "$MAC_SHA"
    end
    on_intel do
      depends_on "rust" => :build
    end
  end

  on_linux do
    depends_on "rust" => :build
  end

  def install
    if OS.mac? && Hardware::CPU.arm?
      bin.install "$BIN"
    else
      system "cargo", "install", *std_cargo_args(path: "crates/ctx-mcp")
    end
  end

  test do
    assert_match "$BIN $NEW", shell_output("#{bin}/$BIN --version")
  end
end
EOF
  else
    cat <<EOF
class CtxMcp < Formula
  desc "Minimal snapshot-centered context engine (MCP server over stdio)"
  homepage "https://github.com/$OWNER/$REPO"
  url "$SRC_URL"
  version "$NEW"
  sha256 "$SRC_SHA"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/$OWNER/$REPO.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: "crates/ctx-mcp")
  end

  test do
    assert_match "$BIN $NEW", shell_output("#{bin}/$BIN --version")
  end
end
EOF
  fi
}

# ---- ensure tap repo exists ----
if ! gh repo view "$OWNER/$TAP_REPO" >/dev/null 2>&1; then
  echo ">> creating tap repo $OWNER/$TAP_REPO"
  gh repo create "$OWNER/$TAP_REPO" --public \
    --description "Homebrew tap for $REPO ($BIN)"
fi

# ---- clone tap (retry: a freshly created repo may lag a moment) ----
TAPDIR="$TMP/tap"
cloned=false
for _ in 1 2 3 4 5; do
  if git clone -q "git@github.com:$OWNER/$TAP_REPO.git" "$TAPDIR" 2>/dev/null; then
    cloned=true; break
  fi
  sleep 2
done
$cloned || git clone -q "https://github.com/$OWNER/$TAP_REPO.git" "$TAPDIR"

mkdir -p "$TAPDIR/Formula"
gen_formula >"$TAPDIR/Formula/${FORMULA}.rb"

git -C "$TAPDIR" add "Formula/${FORMULA}.rb"
git -C "$TAPDIR" commit -q -m "$FORMULA $NEW"
git -C "$TAPDIR" branch -M main
git -C "$TAPDIR" push -q -u origin main

echo
echo "released $TAG and pushed formula to $OWNER/$TAP_REPO"
echo "install with: brew install $OWNER/tap/$FORMULA"
