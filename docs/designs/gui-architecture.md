# GUI / Client-Surface Architecture

Status: **governing for the client surface** — read before any change to how a human-facing UI is
built or how it reaches the engine. Subordinate to and consistent with
`docs/designs/architecture-north-star.md` (the prime directive, the single-protocol-authority
invariant §3.3, and the seam table). Where this document and the north star disagree, the north star
wins; this document only *specializes* it for the GUI.

Date: 2026-06-21

> **Positioning note (2026-06-24):** governed by `docs/designs/trust-substrate.md` — Nerve's moat is the deterministic flight-recorder + execution-grounded re-verifier (replayable **Run** + signed **Receipt**); the `delegate.*` cockpit is the distribution body. Under that thesis, this doc specifies the human cockpit surface, which must become a server-authoritative fleet dashboard (`delegate.list`/`get`), a structured per-agent event view, an accept/reject Review gate (not a copy-packet), and Run-replay + Receipt surfaces — all over the same versioned protocol seam.

This is the long-term contract for Nerve Workstation's GUI / client surface — the window a human
opens onto the engine. It exists because "what the UI looks like" and "how the UI is authored" are
easy to conflate with "how the UI talks to the engine," and only the last of those is governed by the
north star. This document draws the line precisely, picks the long-term architecture, and writes down
the **observable triggers** that move us from one stage to the next so the decision is made on
evidence, not vibes (mirroring the north star's "promote on a measured trigger, never speculatively"
discipline — §3.8, §8 memory/persistence).

## 1. The decision in one paragraph

**Stay on Option A — the daemon-served, single-file, no-build `gui.html` — now, and evolve to
Option C — a daemon-served *structured* frontend that the daemon still serves as one self-contained
bundle over `GET /` — when, and only when, a measured trigger fires.** Never adopt Option B (a
dedicated Tauri frontend reaching the engine over Tauri IPC) and never adopt Option D (a full-Rust
native GUI) as the *primary* client surface. A and C are **wire-identical to the daemon** — both serve
one artifact at `GET /` that speaks only `/rpc` + `/events` — so the choice between them is an internal
*authoring* decision the north star is silent on, which is exactly why A→C is cheap and reversible. B
and D each sacrifice one of the two load-bearing properties (single-protocol-authority and
one-UI-everywhere) and are the choices you cannot cleanly undo.

## 2. What is actually load-bearing (and what is not)

Two properties of the current client surface are load-bearing and must be defended across every stage.
Everything else — framework vs. no-build, native vs. web — is negotiable.

1. **Single-protocol-authority (north star §3.3, §3.4, prime directive §2).** Every client reaches the
   engine **only** through the versioned runtime protocol (Protocol v3 → v4), over `POST /rpc` +
   `GET /events`. This is a property of the **transport**, not of how the asset is authored. It is true
   today by construction and CI-guarded:
   - `crates/nerve-workstation/src/daemon/http.rs` is documented as "a *new transport for the existing
     protocol*, never a new protocol … adds **no** `RuntimeCommand` / `RuntimeEvent` / method" (http.rs:1-13).
   - The GUI is embedded via `const GUI_HTML: &str = include_str!("gui.html");` (http.rs:68) and the
     test `gui_html_is_embedded_and_targets_runtime_endpoints` (http.rs:726) pins it to `/rpc`,
     `/events`, `runtime/info`, `runtime/tools/list`.
   - The Tauri shell has **zero** IPC commands (no `#[tauri::command]`, no `invoke_handler`, no
     `generate_handler` anywhere in `apps/desktop/src-tauri/src/`). It only `window.navigate(url)`s to
     the daemon URL (`daemon.rs:95`), and even native OAuth round-trips the stateless `auth.*` protocol
     over `/rpc` (`auth.rs:148-162`) rather than bridging through IPC — exactly the §3.7 topology.

2. **One-UI-everywhere + remote reach.** A *single* `include_str!`'d artifact serves the browser, the
   Tauri desktop webview, and Tailscale/mobile remote — there is exactly one UI to build, test, and
   keep at Codex-class fidelity across all three surfaces. The desktop shell "owns no UI of its own"
   (`apps/desktop/README.md`: it navigates to the daemon URL; "adds no new protocol, no new RPC");
   mobile is **remote-only**, reusing the same served GUI over Tailscale (README "Mobile" section).

The third value the project holds — the **full-Rust ethos** — is real but must be read correctly. The
TS retirement (commit `10aae89`, "make the Rust TUI the default chat client; retire the TS TUI`) killed
a TypeScript *protocol/client* stack (a TS TUI, the TS protocol codegen, `bun.lock` for that). It did
**not** rule that human-facing pixels must be Rust. The wire authority stayed in Rust
(`export-runtime-protocol`), which is the invariant that actually mattered. The one sanctioned web
surface is the GUI; the only JS dependency in the entire repo today is `@tauri-apps/cli` (no framework,
no bundler — verified in `apps/desktop/package.json`), and `apps/desktop` is a **quarantined separate
cargo workspace** (root `Cargo.toml` `exclude = ["fuzz", "apps"]`) so its toolchain cannot regress
engine CI.

> **Corollary that decides the whole question:** A and C preserve *both* load-bearing properties
> identically and respect the (correctly-scoped) ethos. B-via-IPC breaks single-protocol-authority.
> B-via-HTTP and D break one-UI-everywhere. So the long-term answer lives entirely inside {A, C}, and
> the only open question is *when* to pay for a build step.

## 3. The honest weakness of the status quo

A is the right *baseline*, not the right *forever*. The weakness is specific and measurable:

- `gui.html` is **1867 lines** (~1103 of hand-written JS): a real CSS design system, a multi-turn
  `session.*` chat surface with an approval round-trip, a one-shot `agent.run`, jobs/tools drawers, an
  SSE reconnect/replay client, a dependency-free markdown renderer — all imperative DOM mutation with
  no component model, no type checker, and no view-layer tests.
- It is the **one file in a fanatically gated repo with no gate**: `Scripts/check-file-size.sh` globs
  `*.rs` only (line 34), so the ≤600-line file cap, `clippy -D warnings`, and golden tests cannot see
  it. The project's own taste (functions ≤100 lines, files ≤600) says a 1867-line hand-authored file is
  a smell; the gate just can't enforce it here.

This is an **authoring-ergonomics** problem, not an architecture problem — it does not touch any
governed invariant. That is precisely why the fix (A→C) is cheap: it changes *what produces the served
string*, not the seam.

## 4. The decision, staged

### Stage 0 — now: Option A (status quo)

Keep `gui.html` as the single client surface. Spend the saved toolchain budget on the **protocol**
(v4 vocabulary for flows / multi-session / settings) and on **no-build authoring ergonomics** that do
not re-introduce a bundler:

- Modularize authoring by **build-time concatenation** of several source fragments into the one served
  string (a tiny `build.rs` / `include_str!` join), so the *artifact* stays a single self-contained
  string while *authoring* becomes modular.
- Adopt native **Web Components** (`customElements.define`) + `<template>` for reusable widgets — zero
  framework, supported by every system WebView Tauri uses.
- Render any DAG/diagram surface in **SVG/Canvas**.
- Add a **check-only** `tsc --checkJs --noEmit` gate over the JS (JSDoc types) for type safety against
  protocol-field renames — this emits no bundle and re-introduces no build output, only a lint.

These keep A buildless while removing most of the single-file pain, and each is individually
reversible.

### Stage 1 — on a trigger: Option C (daemon-served structured bundle)

When a Stage-1 trigger fires (§5), migrate to a structured frontend that the daemon **still** serves
as **one self-contained bundle** at `GET /`, talking **only** `/rpc` + `/events`. Bind C with four
non-negotiable guardrails so it cannot reopen what the TS retirement closed:

1. **One self-contained artifact.** The build MUST emit a single HTML (inline JS/CSS, no external asset
   fetches) embedded via the existing `include_str!` seam. The daemon's serving model, the
   `__NERVE_DAEMON_TOKEN__` injection in `render_gui` (http.rs:70-73, :199), and the CORS/host-guard
   hardening do not change. The http.rs client-contract test stays the acceptance gate.
2. **Thin client only.** No new `RuntimeCommand` / `RuntimeEvent` / method. All logic stays behind the
   versioned protocol (§3.3). The bundle is a protocol client with zero business logic and no second
   source of truth.
3. **Protocol consumed from the Rust codegen.** Any TS protocol types are generated *from*
   `export-runtime-protocol`'s output — never a hand-kept TS copy. (Re-creating a divergent TS protocol
   source is the exact thing `10aae89` removed.)
4. **Quarantined + gated build.** Ring-fence the JS toolchain exactly as `apps/desktop` already
   quarantines Tauri (own workspace, excluded from engine CI). Wire `dist` generation into the build
   (or commit `dist` and drift-check it like the protocol schema) so the embedded UI can never go
   stale. Add a frontend file-size/lint gate the day the build lands — closing the one un-gated hole
   in the repo.

Migration is mechanical and incremental: today `http.rs` does `include_str!("gui.html")`; point that
(or a `build.rs`-staged copy) at the built `dist/index.html`. Port surface-by-surface — keep the chat
view as-is, build the first rich surface in the framework, mount both. The existing 1867-line file is a
precise functional spec and the http.rs tests are an executable acceptance checklist.

### Never — Options B and D as the primary surface

See §6 and §7. If a genuine native-OS need appears, it goes in the **thin shell** and is exposed to the
one UI via the protocol or a narrow shell capability — never by forking the UI onto IPC, and never by
replacing the served surface with a native binary.

## 5. Decision triggers (observable)

Stage transitions are gated on **observable** signals, not speculation.

**A → C (adopt the build) — fire when ANY holds:**

- **First irreducibly-visual surface lands or is committed on the roadmap** — a flow DAG with
  hit-testing / pan-zoom, or true multi-pane sessions with independent per-pane SSE subscriptions, or a
  multi-pane diff/editor. These cannot be expressed as 100+ more imperative DOM-mutation functions
  without `gui.html` becoming the project's worst file.
- **`gui.html` exceeds a maintainable size after Web-Components factoring** — concretely, it crosses
  ~3000 lines *despite* the Stage-0 ergonomics work, or a single render/handler region repeatedly
  causes merge conflicts.
- **A protocol-field rename ships a UI bug that a type checker would have caught** — i.e. the
  check-only `tsc` gate proves insufficient and component-level typing/tests are needed.
- **A settings / registry-management surface** (provider config, MCP server CRUD, policy editing)
  requires real stateful forms with validation.

**C → (stay)** — C is the destination, not a waypoint. There is no planned Stage 2 past C. A future
native Rust GUI (D-as-shared-view-crate, §7) may be added *alongside* C as an optional client only if
its own trigger fires; it never replaces the served surface.

**Trigger to reconsider B's *shell* powers (not B-the-frontend) — fire when:**

- The **rendering surface itself** (not the supervising shell) needs an OS capability a daemon-served
  web page structurally cannot have — the north star already documents one such ceiling (§3.7: a served
  page cannot bind a socket). Today the shell already owns every native need (window, menu, folder
  picker, signal-reaping, native OAuth capture) while the UI stays on the protocol. When a *new* such
  need appears, add it to the shell and surface it to the one UI; do **not** fork the UI.

## 6. Why B loses (dedicated Tauri frontend / Tauri IPC)

- **B-via-IPC is a direct prime-directive violation.** Tauri `invoke` would be a **second bespoke entry
  point** to the engine — off-protocol, invisible to the drift test, and impossible for browser/mobile
  to use. It is the modern re-run of the `nerve agent run` cautionary tale the north star calls out
  (§2). It bifurcates the protocol (desktop speaks `invoke`, everyone else speaks HTTP/SSE), so every
  protocol change must be implemented twice.
- **B-via-HTTP buys almost nothing.** If B stays HTTP-only to preserve protocol purity, it is barely
  distinguishable from today's shell-with-served-UI — because the shell **already** owns the native
  concerns (verified: `auth.rs` does zero-paste OAuth via a raw `TcpListener` and `/rpc`; `lib.rs` /
  `daemon.rs` own window/menu/supervision/folder-picker) while keeping the UI on the protocol. You'd
  pay for a **second UI that diverges** from `gui.html` (the exact maintenance tax `10aae89` paid to
  eliminate) for no governed gain, and you'd split remote/mobile onto the old served UI.
- B is only ever correct for *presentation* if a real native-UI need appears (§5), and even then engine
  access must stay on the protocol with `invoke` confined to shell-only OS concerns — which is the
  status quo, not a new frontend.

## 7. Why D loses (full-Rust native GUI: egui / Iced / GPUI)

- **Forfeits one-UI-everywhere and zero-install remote reach.** A native binary cannot be opened from a
  phone browser; mobile would need a separately-built native app per platform (GPUI has no mobile
  story; Iced's is immature). The current property — one served artifact reaching browser + desktop +
  Tailscale mobile — collapses. This is a hard product regression on a property the architecture
  explicitly prizes (north star P6 lists GUI/TUI/mobile as protocol clients of one surface).
- **Cannot cheaply meet the Codex-class web-fidelity bar.** Codex.app is itself a web UI; egui looks
  like a tool, Iced is cleaner but not Codex-polished, and only GPUI could plausibly approach it — at
  the cost of a pre-1.0, GPU-driver-sensitive, effectively single-vendor dependency. Betting the
  flagship's look on that is a real maturity gamble.
- **Over-applies the ethos.** The TS retirement was about a TUI / the protocol-client *stack*, not a
  ban on web pixels. D is the most ethos-pure on the literal "Rust everywhere" reading but loses the
  "one client artifact everywhere" reading the architecture cares about more for clients.
- **Highest, least-reversible cost** — a from-scratch UI codebase, and you'd likely keep `gui.html`
  anyway for browser/mobile, i.e. two UIs.

D is **not** wrong as a *future optional* client: a native Rust GUI built on a **shared view crate
extracted from `nerve-tui/src/ui/`** (markdown/highlight/diff/flow already live there) could be added
alongside C and promoted to primary only if/when GPUI (or Iced) reaches Codex fidelity. It is never the
near-term primary surface.

## 8. Risks & anti-goals

- **C re-introduces a JS toolchain for one surface — own it honestly, ring-fence it hard.** The
  guardrails in §4 Stage-1 (single self-contained artifact, thin client, Rust-sourced protocol types,
  quarantined + gated build) are what keep C from becoming the dual-source-of-truth the project
  deliberately removed. If C ever fragments into per-shell bundles, or consumes a hand-kept TS copy of
  the protocol, or skips the staleness check, it silently recreates that debt. **Anti-goal:** a TS
  protocol source of truth; the wire authority stays in `nerve-runtime` codegen, always.
- **Stale-bundle drift.** A built artifact embedded in a Rust binary can fall out of sync with the
  daemon it ships inside. Mitigation: build `dist` in CI and drift-check it (like the protocol schema),
  or build it in the embedding crate's `build.rs`.
- **Supply chain.** This is a code-execution agent; a framework pulls transitive deps into a
  security-sensitive product. Prefer a minimal framework + few deps; audit the lockfile; keep the
  toolchain in the quarantined `apps`/frontend workspace, never the engine workspace.
- **Thin-client erosion.** A framework makes it easy to add client-side state/logic that *should* live
  behind the protocol. Anti-goal: business logic in the bundle. The client renders protocol state and
  sends protocol commands — nothing more.
- **The single most expensive mistake available** is choosing B for short-term "native feel" and
  discovering at year two that every workspace feature must be built twice (desktop-IPC vs.
  browser/mobile-HTTP). Rejecting B outright avoids it.
- **De-facto divergence under A.** If mobile/desktop ever needs a divergent layout, the temptation is
  to fork `gui.html`, quietly breaking one-UI-everywhere from the inside. Anti-goal: a forked UI file —
  layout differences belong inside the one artifact (responsive CSS / capability flags).

## 9. Governance

- **The seam is frozen across all stages:** `GET /` serves one self-contained, token-gated artifact;
  the client speaks **only** `/rpc` + `/events`; no new `RuntimeCommand` / `RuntimeEvent` / method
  enters for a UI feature. The http.rs contract test
  (`gui_html_is_embedded_and_targets_runtime_endpoints`) is the standing guard; extend it (don't
  replace it) when C lands so it asserts the built artifact still targets only the protocol endpoints.
- **No Tauri IPC engine surface, ever.** A test/grep guard that `apps/desktop/src-tauri/src/` contains
  no `#[tauri::command]` reaching engine/tool/session/auth vocabulary keeps B-via-IPC out by
  construction. Shell-only OS commands (tray/notifications/fs/window) are the only allowed `invoke`
  surface, and they carry no engine vocabulary.
- **Close the un-gated hole when C lands.** Add a frontend file-size + lint + (if TS) drift gate the
  day the build is introduced, so the UI rejoins the quality regime the rest of the repo lives under.
- When this document and the code disagree, treat it as a bug in one of them — either a UI change
  skipped the protocol seam (fix the change) or the seam genuinely evolved (update this doc and the
  north star in the same PR).
