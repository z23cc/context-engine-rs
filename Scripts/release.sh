#!/usr/bin/env bash
#
# release.sh — cut a release and push the Homebrew formula.
#
# Versioning scheme: start at 0.0.1, increment the patch by +0.0.1 each release.
# The workspace version lives in one place: root Cargo.toml [workspace.package]
# version (all crates inherit it via version.workspace = true).
#
# Modes:
#   Scripts/release.sh                # bump +0.0.1, then release
#   Scripts/release.sh --current      # release the CURRENT version, no bump
#   Scripts/release.sh --bottle-only  # add THIS machine's bottle to the already
#                                      # released current version and MERGE its
#                                      # sha256 into the tap formula's bottle block
#                                      # (does not overwrite other tags' bottles)
#
# Distribution: macOS arm64 (with brew) gets a poured bottle tagged for the
# runner's macOS; everything else builds from source via `cargo install`. Because
# GitHub Actions has no macOS 27 runner, use --bottle-only locally to add an
# arm64_golden_gate bottle to a CI release so this machine also pours.
#
set -euo pipefail

OWNER="z23cc"
REPO="nerve-workstation"
TAP_REPO="homebrew-tap"
FORMULA="nerve-workstation"
BIN="nerve"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

mode="bump"
case "${1:-}" in
  ""|--bump)     mode="bump" ;;
  --current)     mode="current" ;;
  --bottle-only) mode="bottle-only" ;;
  *) echo "usage: release.sh [--current|--bottle-only]"; exit 1 ;;
esac

# ---- preconditions (all modes) ----
command -v gh >/dev/null    || { echo "error: gh CLI not found"; exit 1; }
command -v cargo >/dev/null || { echo "error: cargo not found"; exit 1; }
gh auth status >/dev/null 2>&1 || { echo "error: gh not authenticated"; exit 1; }
branch="$(git rev-parse --abbrev-ref HEAD)"
[[ "$branch" == "main" ]] || { echo "error: not on main (on '$branch')"; exit 1; }
[[ -z "$(git status --porcelain)" ]] || { echo "error: working tree not clean — commit or stash first"; exit 1; }

read_version() {
  awk '
    /^\[workspace\.package\]/ { inpkg=1; next }
    /^\[/                     { inpkg=0 }
    inpkg && /^version[[:space:]]*=/ { gsub(/[^0-9.]/, ""); print; exit }
  ' Cargo.toml
}
CUR="$(read_version)"
[[ -n "$CUR" ]] || { echo "error: could not read [workspace.package] version"; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

bottle_tag() { brew ruby -e 'require "utils/bottles"; puts Utils::Bottles.tag' 2>/dev/null || true; }

# ---- formula generator (arg1=1 includes the bottle block) ----
# Uses globals: OWNER REPO BIN NEW SRC_URL SRC_SHA HAS_BOTTLE BTAG BOTTLE_ROOT BOTTLE_SHA
gen_formula() {
  cat <<EOF
class NerveWorkstation < Formula
  desc "Local AI workstation runtime and MCP adapter"
  homepage "https://github.com/$OWNER/$REPO"
  url "$SRC_URL"
  sha256 "$SRC_SHA"
  license any_of: ["MIT", "Apache-2.0"]
EOF
  if [[ "${1:-0}" == "1" && "${HAS_BOTTLE:-0}" == "1" ]]; then
    cat <<EOF

  bottle do
    root_url "$BOTTLE_ROOT"
    sha256 cellar: :any_skip_relocation, $BTAG: "$BOTTLE_SHA"
  end
EOF
  fi
  cat <<EOF

  depends_on "rust" => :build

  def install
    # Engine + the Rust terminal UI client, so `nerve chat` works out of the box.
    system "cargo", "install", *std_cargo_args(path: "crates/nerve-workstation")
    system "cargo", "install", *std_cargo_args(path: "crates/nerve-tui")
  end

  test do
    assert_match "$BIN $NEW", shell_output("#{bin}/$BIN --version")
  end
end
EOF
}

# ---- bundle the Rust TUI client (nerve-tui) into a keg ----
# The terminal UI is a runtime-protocol client shipped as a SEPARATE executable
# next to the engine, so `brew` users get `nerve chat` out of the box while the
# engine/client boundary stays intact. Built in build_bottle alongside the engine.
bundle_tui_client() {
  local srcdir="$1" keg="$2"
  echo ">> bundling nerve-tui (Rust TUI client)"
  cp "$srcdir/target/release/nerve-tui" "$keg/bin/nerve-tui"
  chmod +x "$keg/bin/nerve-tui"
  "$keg/bin/nerve-tui" --help >/dev/null # smoke: prints usage and exits 0, else abort
}

# ---- bottle builder: build from $1 (a source dir) and package a bottle ----
# Requires BTAG, NEW, TAG, SRC_URL, SRC_SHA. Sets BOTTLE, BOTTLE_SHA, BOTTLE_ROOT.
build_bottle() {
  local srcdir="$1"
  echo ">> building Homebrew bottle ($BTAG)"
  ( cd "$srcdir" && cargo build --release -p nerve-workstation )
  ( cd "$srcdir" && cargo build --release -p nerve-tui ) # Rust TUI client (nerve chat)
  # nerve-wechat is intentionally NOT bundled: it is an experimental client surface
  # (text-only, needs a live iLink bot_type) and stays buildable/tested via the
  # workspace (default-members + CI) until it is verified end-to-end against a live
  # account. Add it here once it ships as a supported surface.
  "$srcdir/target/release/$BIN" --version >/dev/null # smoke test: abort if it can't run
  local keg="$TMP/bottle/$FORMULA/$NEW"
  rm -rf "$TMP/bottle"; mkdir -p "$keg/bin" "$keg/.brew"
  cp "$srcdir/target/release/$BIN" "$keg/bin/$BIN"
  bundle_tui_client "$srcdir" "$keg"
  [[ -f "$srcdir/LICENSE" ]] && cp "$srcdir/LICENSE" "$keg/LICENSE"
  gen_formula 0 >"$keg/.brew/$FORMULA.rb"
  BOTTLE="$TMP/${FORMULA}-${NEW}.${BTAG}.bottle.tar.gz"
  ( cd "$TMP/bottle" && tar -czf "$BOTTLE" "$FORMULA/$NEW" )
  BOTTLE_SHA="$(shasum -a 256 "$BOTTLE" | awk '{print $1}')"
  BOTTLE_ROOT="https://github.com/$OWNER/$REPO/releases/download/$TAG"
}

# ========================================================================
# --bottle-only: add this machine's bottle to an already-released version
# ========================================================================
if [[ "$mode" == "bottle-only" ]]; then
  NEW="$CUR"; TAG="v$NEW"
  [[ "$(uname -s)" == "Darwin" && "$(uname -m)" == "arm64" ]] && command -v brew >/dev/null \
    || { echo "error: --bottle-only must run on arm64 macOS with Homebrew"; exit 1; }
  gh release view "$TAG" -R "$OWNER/$REPO" >/dev/null 2>&1 \
    || { echo "error: release $TAG does not exist — release $NEW first"; exit 1; }
  if ! git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
    git fetch -q origin "refs/tags/$TAG:refs/tags/$TAG" \
      || { echo "error: tag $TAG not found"; exit 1; }
  fi
  BTAG="$(bottle_tag)"; [[ -n "$BTAG" ]] || { echo "error: could not determine Homebrew bottle tag"; exit 1; }

  TAPDIR="$TMP/tap"
  git clone -q "git@github.com:$OWNER/$TAP_REPO.git" "$TAPDIR" 2>/dev/null \
    || git clone -q "https://github.com/$OWNER/$TAP_REPO.git" "$TAPDIR"
  FRM="$TAPDIR/Formula/${FORMULA}.rb"
  [[ -f "$FRM" ]] || { echo "error: $FORMULA.rb not found in tap"; exit 1; }
  fver="$(grep -oE 'releases/download/v[0-9]+\.[0-9]+\.[0-9]+/' "$FRM" | head -1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' || true)"
  [[ "$fver" == "$NEW" ]] || { echo "error: tap formula is at ${fver:-unknown}, not $NEW — release $NEW first"; exit 1; }

  SRC_URL="$(awk -F'"' '/^  url /{print $2; exit}' "$FRM")"
  SRC_SHA="$(awk -F'"' '/^  sha256 "/{print $2; exit}' "$FRM")"
  HAS_BOTTLE=0

  # build from the released tag's exact source (isolated), not the working tree
  SRCDIR="$TMP/src"; mkdir -p "$SRCDIR"
  git archive "$TAG" | tar -x -C "$SRCDIR"
  build_bottle "$SRCDIR"

  echo ">> uploading $BTAG bottle to $TAG"
  gh release upload "$TAG" "$BOTTLE" -R "$OWNER/$REPO" --clobber 2>&1 | tail -1

  # merge the bottle line into the existing formula's bottle block
  if grep -qE '^[[:space:]]*bottle do' "$FRM"; then
    awk -v btag="$BTAG" -v sha="$BOTTLE_SHA" '
      /^[[:space:]]*bottle do[[:space:]]*$/ { inblk=1; print; next }
      inblk && index($0, ", " btag ":") {
        print "    sha256 cellar: :any_skip_relocation, " btag ": \"" sha "\""; replaced=1; next
      }
      inblk && /^[[:space:]]*end[[:space:]]*$/ {
        if (!replaced) print "    sha256 cellar: :any_skip_relocation, " btag ": \"" sha "\""
        inblk=0; print; next
      }
      { print }
    ' "$FRM" >"$FRM.tmp" && mv "$FRM.tmp" "$FRM"
  else
    awk -v btag="$BTAG" -v sha="$BOTTLE_SHA" -v root="$BOTTLE_ROOT" '
      { print }
      /^  license / && !done {
        print ""; print "  bottle do"; print "    root_url \"" root "\""
        print "    sha256 cellar: :any_skip_relocation, " btag ": \"" sha "\""; print "  end"; done=1
      }
    ' "$FRM" >"$FRM.tmp" && mv "$FRM.tmp" "$FRM"
  fi

  brew style --fix "$FRM" >/dev/null 2>&1 || true # auto-align multi-bottle digests
  git -C "$TAPDIR" add "Formula/${FORMULA}.rb"
  if git -C "$TAPDIR" diff --cached --quiet; then
    echo ">> $BTAG bottle already current in the formula; nothing to push"; exit 0
  fi
  git -C "$TAPDIR" commit -q -m "$FORMULA $NEW: add $BTAG bottle"
  git -C "$TAPDIR" push -q
  echo
  echo "added $BTAG bottle to $TAG and merged it into the tap formula"
  echo "upgrade with: brew upgrade $OWNER/tap/$FORMULA"
  exit 0
fi

# ========================================================================
# normal release (bump / current)
# ========================================================================
if [[ "$mode" == "bump" ]]; then
  IFS=. read -r MA MI PA <<<"$CUR"; NEW="$MA.$MI.$((PA + 1))"
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

if [[ "$mode" == "bump" ]]; then
  awk -v new="$NEW" '
    /^\[workspace\.package\]/ { inpkg=1; print; next }
    /^\[/ && !/^\[workspace\.package\]/ { inpkg=0 }
    inpkg && /^version[[:space:]]*=/ { print "version = \"" new "\""; next }
    { print }
  ' Cargo.toml >Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml
  cargo update --workspace >/dev/null

  # Keep the Tauri desktop shell pinned to the same engine version so the two
  # never drift (see Scripts/check-versions.sh). These files live in their OWN
  # cargo workspace under apps/desktop, so the workspace awk above never touches
  # them; rewrite their version strings explicitly and stage them in this commit.
  DESKTOP_FILES=(
    apps/desktop/src-tauri/tauri.conf.json
    apps/desktop/package.json
    apps/desktop/src-tauri/Cargo.toml
  )
  # tauri.conf.json + package.json: rewrite the first top-level "version": "...".
  # The 1,/re/ range bounds the s/// to the FIRST match (portable across BSD and
  # GNU sed; the 0,/re/s//repl/ empty-regex idiom is GNU-only and silently no-ops
  # on macOS BSD sed).
  jver='"version": "[0-9]+\.[0-9]+\.[0-9]+"'
  sed -i.bak -E "1,/$jver/ s/$jver/\"version\": \"$NEW\"/" \
    apps/desktop/src-tauri/tauri.conf.json
  sed -i.bak -E "1,/$jver/ s/$jver/\"version\": \"$NEW\"/" \
    apps/desktop/package.json
  # Cargo.toml: rewrite the [package] version line only (first version = "..." line).
  cver='^version = "[0-9]+\.[0-9]+\.[0-9]+"'
  sed -i.bak -E "1,/$cver/ s/$cver/version = \"$NEW\"/" \
    apps/desktop/src-tauri/Cargo.toml
  rm -f apps/desktop/src-tauri/tauri.conf.json.bak \
        apps/desktop/package.json.bak \
        apps/desktop/src-tauri/Cargo.toml.bak
  # Refresh the desktop Cargo.lock's nerve-desktop pin if it tracks the version.
  if [[ -f apps/desktop/src-tauri/Cargo.lock ]]; then
    awk -v new="$NEW" '
      /^name = "nerve-desktop"$/ { print; getline; print "version = \"" new "\""; next }
      { print }
    ' apps/desktop/src-tauri/Cargo.lock >apps/desktop/src-tauri/Cargo.lock.tmp \
      && mv apps/desktop/src-tauri/Cargo.lock.tmp apps/desktop/src-tauri/Cargo.lock
    DESKTOP_FILES+=(apps/desktop/src-tauri/Cargo.lock)
  fi

  git add Cargo.toml Cargo.lock "${DESKTOP_FILES[@]}"
  git commit -m "release: $TAG"
fi

git tag -a "$TAG" -m "$TAG"
git push origin main
git push origin "$TAG"

# ---- source tarball ----
SRC_TARBALL="$TMP/${REPO}-${NEW}.tar.gz"
git archive --format=tar --prefix="${REPO}-${NEW}/" "$TAG" | gzip -n >"$SRC_TARBALL"
SRC_SHA="$(shasum -a 256 "$SRC_TARBALL" | awk '{print $1}')"
SRC_URL="https://github.com/$OWNER/$REPO/releases/download/$TAG/${REPO}-${NEW}.tar.gz"
echo ">> source sha256   : $SRC_SHA"

# ---- bottle for this machine's macOS ----
HAS_BOTTLE=0
ASSETS=("$SRC_TARBALL")
if [[ "$(uname -s)" == "Darwin" && "$(uname -m)" == "arm64" ]] && command -v brew >/dev/null; then
  BTAG="$(bottle_tag)"
  if [[ -n "$BTAG" ]]; then
    build_bottle "$ROOT"
    HAS_BOTTLE=1
    ASSETS+=("$BOTTLE")
    echo ">> bottle ($BTAG) sha256: $BOTTLE_SHA"
  fi
fi

# ---- github release ----
gh release create "$TAG" "${ASSETS[@]}" \
  -R "$OWNER/$REPO" \
  --title "$TAG" \
  --notes "Automated release $TAG. Install: \`brew install $OWNER/tap/$FORMULA\`"

# ---- ensure tap repo exists ----
if ! gh repo view "$OWNER/$TAP_REPO" >/dev/null 2>&1; then
  echo ">> creating tap repo $OWNER/$TAP_REPO"
  gh repo create "$OWNER/$TAP_REPO" --public \
    --description "Homebrew tap for $REPO ($BIN)"
fi

# ---- clone tap + push formula ----
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
gen_formula 1 >"$TAPDIR/Formula/${FORMULA}.rb"
git -C "$TAPDIR" add "Formula/${FORMULA}.rb"
git -C "$TAPDIR" commit -q -m "$FORMULA $NEW"
git -C "$TAPDIR" branch -M main
git -C "$TAPDIR" push -q -u origin main

echo
echo "released $TAG and pushed formula to $OWNER/$TAP_REPO"
[[ "${HAS_BOTTLE:-0}" == "1" ]] && echo "macOS ($BTAG): brew pours the bottle (no compile)"
echo "install with: brew install $OWNER/tap/$FORMULA"
