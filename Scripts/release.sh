#!/usr/bin/env bash
#
# release.sh — cut a release and push the Homebrew formula.
#
# Versioning scheme: start at 0.0.1, increment the patch by +0.0.1 each release
# (0.0.1 -> 0.0.2 -> 0.0.3 ...). The workspace version lives in one place:
# the root Cargo.toml [workspace.package] version (all crates inherit it).
#
# What it does:
#   1. bump [workspace.package] version by +0.0.1 (unless --current)
#   2. commit the bump, tag vX.Y.Z, push main + tag
#   3. build a deterministic source tarball and create a GitHub Release
#   4. compute its sha256, regenerate Formula/ctx-mcp.rb, push it to the tap
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
command -v gh >/dev/null   || { echo "error: gh CLI not found"; exit 1; }
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

# ---- deterministic source tarball ----
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
TARBALL="$TMP/${REPO}-${NEW}.tar.gz"
git archive --format=tar --prefix="${REPO}-${NEW}/" "$TAG" | gzip -n >"$TARBALL"
SHA="$(shasum -a 256 "$TARBALL" | awk '{print $1}')"
URL="https://github.com/$OWNER/$REPO/releases/download/$TAG/${REPO}-${NEW}.tar.gz"
echo ">> sha256          : $SHA"

# ---- github release ----
gh release create "$TAG" "$TARBALL" \
  -R "$OWNER/$REPO" \
  --title "$TAG" \
  --notes "Automated release $TAG. Install: \`brew install $OWNER/tap/$FORMULA\`"

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

# ---- (re)generate formula ----
mkdir -p "$TAPDIR/Formula"
cat >"$TAPDIR/Formula/${FORMULA}.rb" <<EOF
class CtxMcp < Formula
  desc "Minimal snapshot-centered context engine (MCP server over stdio)"
  homepage "https://github.com/$OWNER/$REPO"
  url "$URL"
  version "$NEW"
  sha256 "$SHA"
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

git -C "$TAPDIR" add "Formula/${FORMULA}.rb"
git -C "$TAPDIR" commit -q -m "$FORMULA $NEW"
git -C "$TAPDIR" branch -M main
git -C "$TAPDIR" push -q -u origin main

echo
echo "released $TAG and pushed formula to $OWNER/$TAP_REPO"
echo "install with: brew install $OWNER/tap/$FORMULA"
