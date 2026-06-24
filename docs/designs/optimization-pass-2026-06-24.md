# Optimization pass — 2026-06-24

> **Positioning note (2026-06-24):** governed by `docs/designs/trust-substrate.md` — Nerve's moat is the deterministic flight-recorder + execution-grounded re-verifier (replayable **Run** + signed **Receipt**); the `delegate.*` cockpit is the distribution body. Under that thesis, this doc is a dated engineering pass whose findings stand on their own; only its directional framing of the cockpit as "the headline" is realigned below.

A multi-dimensional audit (deps · code quality · functionality/architecture · CI/tooling)
followed by green, reversible changesets. Every item below was verified against the
real code before acting — two audit findings turned out to be **misreads** and were
*not* "fixed" (see Verified non-issues).

## Landed on `main` (all green: clippy -D · fmt · file-size · `test --workspace` · drift)

- **Dependencies → latest.** `cargo update` (semver refresh: rustls/time/quinn/syn/
  log/bytes/getrandom/webpki-roots…) + **criterion 0.5 → 0.8** + **schemars 0.8 → 1.2**
  (schema moves to JSON Schema draft 2020-12 / `$defs`; verified **no in-repo TS
  consumer** of `runtime-v3.schema.json`, so nothing breaks — the "codegen'd to TS"
  pipeline is not yet implemented). All other workspace pins already at latest major.
- **Security.** `cargo-audit` run: **0 vulnerabilities** (490 deps vs 1138 advisories).
  Two informational *unmaintained* warnings (`paste`, `proc-macro-error2` — transitive
  build-time proc-macro deps, non-vulnerable) kept visible (not suppressed).
- **CI hardening.** New `.github/workflows/ci.yml` runs every documented gate on every
  push/PR (was Windows/release-only): fmt, clippy `-D warnings`, file-size, full
  `test --workspace`, protocol-drift — plus a separate **RUSTSEC `cargo audit`** job.
  Fixed `windows.yml` to `cargo test --workspace` (the new crate's tests never ran).
- **Dependency right-sizing.** `nerve-wechat` now depends on `nerve-proto` (protocol
  vocabulary) instead of `nerve-runtime` (which pulled `nerve-core` + tree-sitter).
- **Test coverage.** Added `PolicyToolBox` enforcement test for `ApprovalMode::Write`
  (auto-allow Edit-tier without prompting; still gate Exec-tier).
- **`nerve-wechat` production polish.** On-disk session persistence (0o600, skip QR on
  restart); graceful acknowledgement of an owner's media-only message (vs silent drop);
  run-loop resilience (retry transient transport blips, bail on fatal).
- **Docs.** Fixed broken/ambiguous intra-doc links surfaced by `cargo doc -D warnings`
  (incl. 2 introduced this session); regenerated the runtime-v3 schema afterward
  (RuntimeCommand doc comments serialize into schema `description` fields).

## Verified non-issues (audit was wrong — no change made)

- **Flow strategies "unimplemented".** FALSE: all seven `Strategy` variants are wired to
  real interpreters (`flow/engine.rs:100-123`, asserted by
  `every_strategy_variant_is_wired_into_the_dispatcher`). The `other =>` arm is
  intentional `#[non_exhaustive]` defense.
- **device-code OAuth mismatch.** Already clean: reserved vocabulary, a clear actionable
  error, capabilities advertise `device_code.supported=false` with a reason, and tests
  pin it. Full per-provider implementation needs live provider endpoints.

## Deliberate deferrals (documented, NOT autonomously done — by design)

| Item | Why deferred |
|---|---|
| ~~schemars 0.8 → 1.x~~ | **DONE** (commit `c968ebc`) after verifying there is no in-repo TS consumer of the schema — see Landed. |
| **build_context double-walk** | Verified real (repo-map's query-dependent `analyze_files` vs `shared_indexed_files`), but the expensive parse is already `(mtime,size)`-cached so the cost is byte re-reads; unifying risks repo-map ranking/golden regressions and was deliberately deferred by the designer. |
| **Full `cargo doc` cleanup + gate** | nerve-agent/nerve-tui carry a long tail of public-mod-doc → private-submodule links (a deliberate internal-nav style). Mass backticking is opinionated churn; a surgical `broken_intra_doc_links`-only gate is the right deliberate follow-up. |
| **wechat media (image/file)** | AES-128-ECB + CDN flow unverified against Tencent docs; needs a live account + `bot_type`. |
| **P7 multi-agent cockpit · P6 Tauri GUI** | Large product/UX features needing direction, not autonomous completion. |

## Leptos GUI hardening (nerve-gui, same pass)

Audited the ~8.8k-line Leptos CSR frontend (resilience / state / UX / build-test) and
landed the high-value, verifiable fixes (host tests + wasm build + wasm clippy):

- **Security:** markdown URL-scheme sanitization — pulldown_cmark passed
  `javascript:`/`data:`/`vbscript:` link/image URLs straight to `inner_html` (raw
  `<script>` was already escaped). Now neutralized at the event level (+4 tests).
- **Correctness:** route `RuntimeEvent::Agent` (own-engine job-scoped path was
  swallowed → no transcript); `close_chat` now picks `session.close`/`delegate.close`
  by backend (was leaking session-backend sessions).
- **Resilience:** SSE `onerror`/`onopen` → a "Reconnecting…" banner (was a silent
  freeze on daemon outage); the discarded `open_events` error is now surfaced.
- **Tests:** event-folding logic covered (nerve-gui 14 → 25 host tests).
- **CI:** `cargo clippy -p nerve-gui --target wasm32-unknown-unknown` now gates the
  frontend (wasm-only breakage was invisible).
- **Build:** rebuilt the committed `dist/` so the fixes actually ship.

Remaining GUI backlog (documented, not yet done): bounded chat/turn history vs
localStorage quota (policy choice); SettingsModal missing `runtime_provider`/
`runtime_model` props (session backend unconfigurable via UI); inspector load-error
states (silent "—"); `start_job_await` per-fetch timeout + unmount cancellation;
keyed `<For>` transcript to avoid O(n) re-render per delta; a `dist` drift gate (or
build.rs rebuild) in CI; a `wasm-bindgen-test` harness for DOM/reactive paths.

## Recommended next steps (your call)

1. Review `main` (green, releasable) — cut **v0.0.69** if you want the deps/CI/wechat
   work shipped.
2. Greenlight schemars 1.x as a standalone reviewed PR (protocol-schema diff to inspect).
3. Pick a big rock: P7 cockpit (the distribution body, not the moat — the moat is the flight-recorder + execution-grounded re-verifier) (2026-06-24), device-code (provide provider specifics),
   or wechat media (provide a live `bot_type`).
