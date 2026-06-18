# Nerve Workstation — Desktop shell (Tauri 2)

A thin native wrapper around the Nerve daemon GUI. It owns **no UI of its own**:
desktop builds are local-first and spawn the engine's HTTP daemon as a managed
child, while mobile builds are remote-only and point the window at an already
running `nerve daemon --http` URL.

```
launch ─▶ resolve root ─▶ spawn `nerve daemon --http 127.0.0.1:<port> --root <dir>`
       ─▶ wait for HTTP ─▶ window.navigate("http://127.0.0.1:<port>/")  ─▶ kill on exit
```

Remote mode skips local spawning and navigates directly:

```
launch ─▶ read NERVE_REMOTE_URL / persisted remote_url ─▶ window.navigate("<remote daemon URL>/")
```

This is a **client of the versioned runtime protocol over the existing HTTP
transport** (architecture north-star §6 / §8 P6) — it adds no new protocol, no
new RPC, and does not touch the engine crates.

## Isolation from the engine workspace

This app is deliberately quarantined so the engine's CI gates are unaffected:

- `apps/desktop/src-tauri/Cargo.toml` declares its **own `[workspace]`**, making
  it a standalone cargo workspace; the repo-root manifest also lists `apps` under
  `exclude`. No Tauri/GUI dependency is added to any `nerve-*` crate or the root
  `Cargo.toml`.
- Its build output lives in `apps/desktop/src-tauri/target/` (git-ignored) and it
  keeps its own `Cargo.lock`.
- `cargo {clippy,test,fmt} --workspace` and `./Scripts/check-file-size.sh` at the
  engine root never see this crate.

## Prerequisites

- The `nerve` binary built in the engine workspace:
  ```bash
  cargo build -p nerve-workstation --bin nerve   # run from the repo root
  ```
  In dev the app auto-locates `target/{debug,release}/nerve`. Override with
  `NERVE_BIN=/abs/path/to/nerve` if needed.
- [Bun](https://bun.sh) (`bun@1.3.x`) and a C toolchain + the platform webview
  (WebKitGTK on Linux; WebView2 on Windows; system WebKit on macOS).

## Install

```bash
cd apps/desktop
bun install            # fetches @tauri-apps/cli
bun run icon           # one-time: generate the app icon set into src-tauri/icons/
```

## Dev

```bash
cd apps/desktop
bun run tauri dev      # builds the shell, launches a native window with the nerve GUI
```

Skip the folder picker by pre-selecting a root:

```bash
NERVE_ROOT="$(cd ../.. && pwd)" bun run tauri dev
```

Use a remote daemon instead of spawning a local one:

```bash
nerve daemon --http 0.0.0.0:4732 --root /path/to/workspace   # on desktop/server
NERVE_REMOTE_URL="http://desktop.tailnet.ts.net:4732/" bun run tauri dev
```

## Build (release bundle)

```bash
cd apps/desktop
bun run tauri build    # macOS .app/.dmg; pass --target for Windows/Linux
```

> Shipping note: bundle `nerve` as a Tauri **sidecar** (`bundle.externalBin`) so
> it lands next to the app executable; `resolve_binary()` already prefers a
> sidecar/`Resources` copy before falling back to the dev build output.

## Environment variables

| Var | Effect |
|---|---|
| `NERVE_ROOT` | Use this directory as the workspace root (skips picker + persistence lookup). |
| `NERVE_BIN`  | Absolute path to the `nerve` binary (overrides auto-discovery). |
| `NERVE_REMOTE_URL` | Use an already-running remote daemon URL and skip local daemon spawning. Required on mobile. |

The last chosen root is persisted to the app config dir (`nerve-desktop.json`)
and reused on the next launch. You may also opt into remote mode by adding a
persisted URL:

```json
{
  "last_root": "/path/to/workspace",
  "remote_url": "http://desktop.tailnet.ts.net:4732/"
}
```

Empty or absent `remote_url` keeps desktop in local-spawn mode. Mobile builds
cannot spawn `nerve`; they require `NERVE_REMOTE_URL` or persisted `remote_url`.

## Mobile (iOS / Android readiness)

Mobile reuses the daemon-served HTTP GUI. It does **not** embed or spawn the
engine daemon; run the daemon on a desktop/server and connect over the network
(Tailscale is the recommended private path).

1. On the desktop/server:
   ```bash
   cargo build -p nerve-workstation --bin nerve
   ./target/debug/nerve daemon --http 0.0.0.0:4732 --root /path/to/workspace
   ```
2. Join the desktop/server and phone to the same Tailscale tailnet.
3. Configure the mobile app with the daemon URL, for example:
   ```bash
   export NERVE_REMOTE_URL="http://desktop.tailnet.ts.net:4732/"
   ```
   This works for `tauri ios dev` / simulator runs and Xcode schemes where you
   can set environment variables. For a packaged app, seed the app config file
   in Tauri's app config dir with the same value as `remote_url`.
4. Initialize platform projects on a machine with the native toolchains:
   ```bash
   cd apps/desktop
   bun install
   bun run tauri ios init
   bun run tauri android init
   ```
5. Build/run with signing configured:
   ```bash
   bun run tauri ios build      # requires Xcode, Apple signing team/profile
   bun run tauri android build  # requires Android Studio/SDK/NDK signing setup
   ```

Notes:

- This repository includes Tauri mobile config (`bundle.iOS`,
  `bundle.android`) and an iOS `Info.ios.plist` merge file that allows plain
  HTTP remote daemon URLs such as Tailscale hosts.
- T2 does not add a separate mobile setup wizard. The selectable inputs are
  `NERVE_REMOTE_URL` and the persisted `remote_url` config field; a user-facing
  settings screen can be layered later without changing the daemon protocol.
- Device builds require Xcode/iOS signing or Android Studio/SDK/NDK on the
  build machine. This repo does not commit generated `src-tauri/gen/apple` or
  `src-tauri/gen/android` projects; generate them with the init commands above.

## Layout

```
apps/desktop/
├── package.json            # own bun project (@tauri-apps/cli)
├── scripts/gen-icon.mjs    # zero-dep 1024² PNG generator for `tauri icon`
├── ui/index.html           # splash shown until the daemon URL loads
└── src-tauri/
    ├── Cargo.toml          # standalone [workspace] + package
    ├── Info.ios.plist      # iOS ATS/local-network merge for remote daemon HTTP
    ├── tauri.conf.json     # one window → splash, navigated to the daemon URL
    ├── capabilities/       # core permissions only
    └── src/
        ├── lib.rs          # Tauri builder + mobile entry point + exit cleanup
        ├── main.rs         # desktop binary entry point
        ├── daemon.rs       # local spawn or remote URL attach + cleanup
        └── config.rs       # root/remote URL resolution + persistence
```
