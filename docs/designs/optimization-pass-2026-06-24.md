# Optimization pass — 2026-06-24

A multi-dimensional audit (deps · code quality · functionality/architecture · CI/tooling)
followed by green, reversible changesets. Every item below was verified against the
real code before acting — two audit findings turned out to be **misreads** and were
*not* "fixed" (see Verified non-issues).

## Landed on `main` (all green: clippy -D · fmt · file-size · `test --workspace` · drift)

- **Dependencies → latest.** `cargo update` (semver refresh: rustls/time/quinn/syn/
  log/bytes/getrandom/webpki-roots…) + **criterion 0.5 → 0.8** (latest major, dev/bench).
  All other workspace pins are already at their latest major.
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
| **schemars 0.8 → 1.x** | Tractable (derive-only) BUT changes the emitted protocol schema draft-07 → 2020-12 — a *consumer-facing* artifact (codegen'd to TS, drift-checked). The drift test only checks self-consistency, not downstream TS-codegen acceptance. Needs a deliberate, reviewed PR. |
| **build_context double-walk** | Verified real (repo-map's query-dependent `analyze_files` vs `shared_indexed_files`), but the expensive parse is already `(mtime,size)`-cached so the cost is byte re-reads; unifying risks repo-map ranking/golden regressions and was deliberately deferred by the designer. |
| **Full `cargo doc` cleanup + gate** | nerve-agent/nerve-tui carry a long tail of public-mod-doc → private-submodule links (a deliberate internal-nav style). Mass backticking is opinionated churn; a surgical `broken_intra_doc_links`-only gate is the right deliberate follow-up. |
| **wechat media (image/file)** | AES-128-ECB + CDN flow unverified against Tencent docs; needs a live account + `bot_type`. |
| **P7 multi-agent cockpit · P6 Tauri GUI** | Large product/UX features needing direction, not autonomous completion. |

## Recommended next steps (your call)

1. Review `main` (green, releasable) — cut **v0.0.69** if you want the deps/CI/wechat
   work shipped.
2. Greenlight schemars 1.x as a standalone reviewed PR (protocol-schema diff to inspect).
3. Pick a big rock: P7 cockpit (headline), device-code (provide provider specifics),
   or wechat media (provide a live `bot_type`).
