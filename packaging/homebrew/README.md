# Homebrew distribution

`ctx-mcp` (the binary from `crates/ctx-mcp`) is published through a Homebrew tap.

```bash
brew install z23cc/tap/ctx-mcp
ctx-mcp --version
```

## How it works

- **Tap repo:** [`z23cc/homebrew-tap`](https://github.com/z23cc/homebrew-tap)
  holds `Formula/ctx-mcp.rb`. The shorthand `z23cc/tap` expands to it.
- **macOS (Apple Silicon):** the formula downloads a **prebuilt binary** release
  asset — no compiler, no Rust/LLVM, instant install.
- **Intel macOS / Linux:** falls back to building from source via `cargo install`
  (Homebrew installs a temporary `rust` build dependency).
- **Versioning:** starts at `0.0.1` and increments the patch by `+0.0.1` per
  release. The single source of truth is `version` under `[workspace.package]`
  in the root `Cargo.toml`; every crate inherits it via `version.workspace = true`.

## Cutting a release

Run from the repo root on a clean `main`:

```bash
Scripts/release.sh            # bump +0.0.1, then release (0.0.1 -> 0.0.2 -> ...)
Scripts/release.sh --current  # release the CURRENT version without bumping
```

The script bumps the version, commits, tags `vX.Y.Z`, and publishes a GitHub
Release with:

- a deterministic **source tarball** (`context-engine-rs-X.Y.Z.tar.gz`), and
- when run on an Apple Silicon Mac, a **prebuilt binary**
  (`ctx-mcp-X.Y.Z-aarch64-apple-darwin.tar.gz`).

It then recomputes the `sha256`s, regenerates `Formula/ctx-mcp.rb`, and pushes it
to the tap. It creates the tap repo on the first run. If the script is *not* run
on an Apple Silicon Mac, it emits a source-only formula (everyone builds from
source) — so always release from an arm64 Mac to ship the prebuilt binary.

## Notes / future work

- The prebuilt binary currently covers **macOS arm64 only**. To add Intel-mac or
  Linux binaries, extend `Scripts/release.sh` to cross-compile (or run it on those
  hosts) and add matching `on_intel` / `on_linux` arch blocks with their own
  `url` + `sha256`.
- A GitHub Actions release pipeline could build all platforms automatically, but
  it needs a token with the `workflow` scope plus a cross-repo PAT secret to push
  the tap — so releases run locally for now.
