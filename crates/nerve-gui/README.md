# nerve-gui

The Leptos (Rust → WASM) **client-side-rendered** frontend for `nerve daemon`.
It is a client of the runtime protocol (Protocol v4), talking **only** over HTTP
`POST /rpc` (JSON-RPC) and `GET /events` (SSE) — never Tauri IPC. It shares the
engine's exact wire types via the [`nerve-proto`](../nerve-proto) crate, so there
is no hand-duplicated protocol vocabulary and no TS/codegen drift.

Status: **G1b spike** — proves the end-to-end pipeline (daemon serves the WASM
bundle at `/app`, the bundle reads the injected token, calls `runtime/info` +
`runtime/tools/list`, renders the result). The real multi-turn chat surface (the
`session.*` command family) is G2; the final Codex styling is G4. The legacy
`gui.html` single-page GUI stays served at `/` unchanged.

## Workspace placement

`nerve-gui` is a workspace **member** (so it shares the lockfile + the
`nerve-proto` path dependency) but is **excluded from `default-members`** in the
root `Cargo.toml`. It depends on `leptos` / `web-sys` / `gloo`, which only
compile for `wasm32-unknown-unknown` — so the engine's ordinary host-target
`cargo build/test --workspace` never tries to build it. Build it explicitly:

## Build (regenerate the embedded bundle)

```bash
# Prereqs (one-time): rustup target add wasm32-unknown-unknown; cargo install trunk
cd crates/nerve-gui
trunk build              # emits dist/{index.html, nerve-gui.js, nerve-gui_bg.wasm, styles.css}
```

`Trunk.toml` sets `filehash = false`, so the asset filenames are **stable**
(`nerve-gui.js`, `nerve-gui_bg.wasm`). The daemon `include_bytes!`/`include_str!`s
these committed `dist/` artifacts (see
`crates/nerve-workstation/src/daemon/app.rs`) so it serves them with no `trunk`
step at engine-build time.

### Drift discipline

The built `dist/` is **committed**, mirroring the runtime-protocol schema drift
discipline (regenerate, fail on stale). After changing any frontend source,
re-run `trunk build` and commit the regenerated `dist/`. Long-term, CI should
rebuild the frontend and drift-check the committed `dist/` against a fresh build.

## End-to-end smoke

```bash
# 1. Build the bundle (above), then the engine binary embeds it:
cargo build -p nerve-workstation --bin nerve
# 2. Start the daemon over HTTP:
./target/debug/nerve daemon --http 127.0.0.1:4173 --root .
# 3. The served Leptos app + its assets (token is embedded on a loopback bind):
curl -s http://127.0.0.1:4173/app | grep __NERVE_DAEMON_TOKEN__   # token injected
curl -sI http://127.0.0.1:4173/app/nerve-gui_bg.wasm | grep -i application/wasm
curl -sI http://127.0.0.1:4173/ | grep -i text/html               # legacy GUI intact
```
