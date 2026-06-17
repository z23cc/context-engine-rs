# Homebrew distribution

`nerve-workstation` publishes the `nerve` binary through a Homebrew tap.

```bash
brew install z23cc/tap/nerve-workstation
nerve --version
```

## How it works

- **Tap repo:** [`z23cc/homebrew-tap`](https://github.com/z23cc/homebrew-tap)
  holds `Formula/nerve-workstation.rb`. The shorthand `z23cc/tap` expands to it.
- **Bottles (poured, no compile):** the formula ships real Homebrew **bottles**.
  A bottle is poured, not built, so installs are instant and **skip Homebrew's
  build-from-source Xcode-version gate** (which blocks installs on pre-release
  macOS). Each bottle is tagged for the exact macOS it was built on.
- **Source fallback:** any platform without a matching bottle builds from source
  via `cargo install` (Homebrew installs a temporary `rust`).
- **Versioning:** starts at `0.0.1`, `+0.0.1` per release. Single source of truth:
  `version` under `[workspace.package]` in the root `Cargo.toml` (crates inherit it).
- The release binary is stripped at link time (`[profile.release] strip = true`),
  keeping a valid ad-hoc code signature — do **not** run an external `strip` on the
  macOS binary or it is SIGKILLed on Apple Silicon even after re-signing.

## Release paths

There are two ways to release; both bump/produce the same version and push the tap.

### 1. GitHub Actions (automated, mainstream macOS)

`.github/workflows/release.yml` (manual `workflow_dispatch`) bumps `+0.0.1`,
builds an **arm64 bottle on the runner's macOS** (currently `macos-15` →
`arm64_sequoia`), publishes the GitHub Release, and pushes the regenerated formula
to the tap (cross-repo push via the `TAP_DEPLOY_KEY` SSH deploy key).

```bash
gh workflow run release.yml -R z23cc/nerve-workstation   # or the Actions "Run workflow" button
```

GitHub has **no macOS 27 runner**, so CI cannot produce an `arm64_golden_gate`
(macOS 27) bottle — see `--bottle-only` below to add one.

### 2. Local `Scripts/release.sh`

Run from a clean `main` on an Apple Silicon Mac:

```bash
Scripts/release.sh            # bump +0.0.1, then release (builds this Mac's bottle)
Scripts/release.sh --current  # release the CURRENT version without bumping
```

### Adding this machine's bottle to an existing release — `--bottle-only`

After a CI release (which only has e.g. an `arm64_sequoia` bottle), run this on a
macOS 27 machine to add an `arm64_golden_gate` bottle so this machine pours too:

```bash
git checkout main && git pull          # get the released version into Cargo.toml
Scripts/release.sh --bottle-only
brew upgrade z23cc/tap/nerve-workstation         # now pours the golden_gate bottle
```

It builds the binary from the **released tag's exact source** (isolated), uploads
the bottle to that release, and **merges** its `sha256` line into the formula's
existing `bottle do` block (it does not overwrite other tags' bottles, and
re-running it just updates this tag's checksum).

## Notes / future work

- To cover more macOS versions / Intel / Linux, build bottles on those hosts (or
  add runners to the CI matrix) and merge their `sha256 ... <tag>:` lines.
- `brew test` spins up a build environment, so on pre-release macOS without a
  current Xcode it hits the Xcode gate; the actual install (pour) does not.
