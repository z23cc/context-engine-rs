# Homebrew distribution

`ctx-mcp` (the binary from `crates/ctx-mcp`) is published through a Homebrew tap.

```bash
brew install z23cc/tap/ctx-mcp
ctx-mcp --version
```

## How it works

- **Tap repo:** [`z23cc/homebrew-tap`](https://github.com/z23cc/homebrew-tap)
  holds `Formula/ctx-mcp.rb`. The shorthand `z23cc/tap` expands to it.
- **Formula:** builds from source via `cargo install` (Homebrew installs a
  temporary `rust` build dependency). One formula covers macOS arm64/x86 and
  Linux — no per-platform binaries to maintain.
- **Versioning:** starts at `0.0.1` and increments the patch by `+0.0.1` per
  release. The single source of truth is `version` under `[workspace.package]`
  in the root `Cargo.toml`; every crate inherits it via `version.workspace = true`.

## Cutting a release

Run from the repo root on a clean `main`:

```bash
Scripts/release.sh            # bump +0.0.1, then release (0.0.1 -> 0.0.2 -> ...)
Scripts/release.sh --current  # release the CURRENT version without bumping
```

The script bumps the version, commits, tags `vX.Y.Z`, pushes a GitHub Release
with a deterministic source tarball, recomputes the `sha256`, regenerates the
formula, and pushes it to the tap. It creates the tap repo on the first run.

## Notes / future work

- The formula compiles on install (~1–2 min, pulls Homebrew's `rust`). If that
  becomes annoying, add a CI job to ship pre-built bottles / binary releases and
  point the formula at those assets instead.
- A GitHub Actions release pipeline would need a token with the `workflow` scope
  (and a cross-repo PAT to push the tap), so releases run locally for now.
