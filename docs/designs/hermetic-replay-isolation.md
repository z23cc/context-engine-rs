# Hermetic Replay Isolation — closing the bit-for-bit gap honestly

Status: **proposed (governing)** — the decision record for how Nerve earns, or honestly declines to
claim, the "bit-for-bit replayable" property the trust substrate rests on. Governed by
`docs/designs/trust-substrate.md` (INV-R1..R6) and `docs/designs/architecture-north-star.md`
(§3.1 determinism boundary, the `SandboxLauncher` seam). Supersedes the "Deferred / P-next"
hand-waving in `docs/designs/agent-exec-sandbox.md` §5/§8 with an actionable, tier-honest plan.
Date: 2026-06-26.

---

## 0. The decision (one paragraph)

Today Nerve re-runs the org's checks through the **best-effort `ProcessLauncher`**
(`crates/nerve-workstation/src/sandbox/process.rs`), which scrubs env and forces cwd but enforces
*none* of the `fs_read` / `fs_write` / `net` policy fields it carries — yet the Run and Receipt it
produces are byte-indistinguishable from a hypothetical hermetic one. That is an **overclaim against
INV-R1.** The fix is two-pronged and ordered: **(1) stop lying first** — stamp a signed
`IsolationTier` onto every Run and Receipt so a verifier always knows how trustworthy the replay is,
and pin the determinism-relevant environment; then **(2) earn the strong tier** on Linux (where CI
runs) via `Landlock` FS scoping + a network namespace + `seccomp`, slotted behind the *existing*
`SandboxLauncher` port with **zero caller change** and **zero `nerve-core` change**. macOS stays an
honestly-downgraded best-effort tier; Windows is an explicit non-goal.

---

## 1. The trust gap, precisely

### 1.1 First, disentangle the two things called "replay"

The codebase has **two** distinct re-execution paths, and conflating them is the root of the
confusion:

| Path | File | What it does | Hermeticity-sensitive? |
|---|---|---|---|
| **L0c "replay"** (`replay.start`) | `replay.rs` → `nerve_core::runpin::verify_replay` (`runpin.rs:89`) | Re-folds the **recorded event tape** and checks its content-addressed spine head equals `Run.root_hash`. Pure; matches by construction unless the tape was tampered. | **No.** This is tape *integrity*, not re-execution. Already bit-for-bit and always will be. |
| **L2 "verify"** (`verify.start` / `nerve verify`) | `verify_runner.rs`, `commands/verify.rs` | Re-runs the org's `<root>/.nerve/checks.json` checks via the `SandboxLauncher`, folds exit codes into a `Verdict`, seals a signed `Receipt`. | **Yes — this is the entire gap.** The verdict bottoms out on `ProcessLauncher.launch()`, which inherits the runner's world. |

So "bit-for-bit replayable" is **honest for the L0c tape** (the record of *what the agent did*) and
**not yet earned for the L2 verdict** (the re-run that decides *whether it cleared the bar*). The
Receipt's `replay_manifest` only attests the former (`receipt.rs:111`); nothing in the signed
statement says how the verdict-producing execution was contained. **That missing signed field is the
precise gap.**

A second, quieter gap: the *closure* the tape commits to is thin. `RunInputs.repo_snapshot_hash` is
**never populated** on the delegate capture path (`jobs/delegate/seal.rs:76` →
`resolve_run_inputs`, which hardcodes `repo_snapshot_hash: String::new()` in `toolchain_pin.rs:65`),
`image_digest` is always `None`, and `ToolchainPin.tools` (actual tool versions) is always empty —
only lockfiles are hashed. Even `repo_snapshot_hash`, when computed, is a **path+size proxy, not
per-byte** (`runpin.rs:24`). So the "pinned closure" in `closure_digest_for` (`verify_runner.rs:347`)
is frequently empty or partial.

### 1.2 What "bit-for-bit replayable" requires that non-hermetic execution does not guarantee

Ranked by whether it **perturbs a verdict** (flips pass/fail) versus is merely **cosmetic** (pollutes
the evidence `output_hash` but not the pass/fail):

| Perturbation source | Today's `ProcessLauncher` | Effect on verdict |
|---|---|---|
| **Network** | `NetPolicy::Deny` recorded as **intent only — not enforced** (`sandbox/mod.rs:59-67`) | **Verdict- + integrity-critical** — a check that fetches deps/hits a service flips on net state; also lets the agent pull un-pinned inputs or exfiltrate. |
| **Filesystem outside the root** | Not enforced (`fs_read`/`fs_write` are dead fields, `#![allow(dead_code)]`) | **Verdict-critical** — checks reading `/usr`, `~/.cargo`, global git config, sibling repos depend on host state absent from the closure. |
| **`$PATH` toolchain version** | `PATH` allowlisted + inherited; *which* `rustc`/`node`/`python` resolves is the host's | **Verdict-critical** — rustc 1.95 vs 1.96 changes results; `ToolchainPin.tools` is empty so the version isn't even recorded. |
| **Locale (`LANG`/`LC_*`)** | Allowlisted + inherited verbatim | **Verdict-affecting** — collation, formatting, tests asserting on formatted strings. |
| **Timezone (`TZ`) / wall-clock** | `TZ` allowlisted + inherited | **Verdict-affecting for time-sensitive tests**; cosmetic elsewhere. |
| **Parallelism / scheduling** | Not controlled | **Verdict-affecting, NOT fixable by hermeticity** — racy tests, shared ports/tmp. The residual hermeticity *cannot* remove (see §5). |
| **Secrets in env values** | Name-scrubbed (`is_secret_name`); `user:pass` in a *value* survives | Integrity (exfil) — net-deny tier mitigates regardless. |
| **Username / hostname / TMPDIR / `COLORTERM`** | Inherited | **Cosmetic** — changes log text → the evidence `output_hash`, but **not** the verdict (exit-code + flaky-fold, `verify_runner.rs:394`). Worth pinning so evidence hashes stabilize, but not verdict-load-bearing. |

The load-bearing four are **network, out-of-root FS, toolchain version, and locale/TZ**. Network and
FS need *kernel* enforcement; toolchain and locale need *pinning + recording*.

---

## 2. The hermetic-closure design, per platform

### 2.1 The seam: reuse `SandboxLauncher`, add backends — no caller change

The containment seam **already exists and is the declared entry point** (`sandbox/mod.rs`). Its
policy already carries the exact fields a strong backend needs — `fs_read`, `fs_write`, `net`, `env`,
`cwd`, `env_overrides` (`SandboxPolicy`, `sandbox/mod.rs:151`). The work is to add backends behind the
trait and select them at the composition roots. **No caller, no protocol vocabulary, and no
`nerve-core` change** for the launcher itself (INV-R2): execution is non-deterministic and stays in
`nerve-workstation`.

```
trait SandboxLauncher { fn launch(..) -> Result<Output>; ... }   // unchanged
   ├─ ProcessLauncher    (exists)  → tier: Contained
   ├─ RefuseLauncher     (exists)  → refuses
   ├─ LandlockLauncher   (NEW, Linux)   → tier: Hermetic | Contained (probed)
   └─ SeatbeltLauncher   (NEW, macOS)   → tier: BestEffort
```

The one change to the trait surface: `launch`/`launch_streaming` must report **the tier actually
established** (not requested), so the verify path can stamp the truth — either an `Output.isolation_tier`
field or an `established_tier(&self, policy)` that the launcher computes from a real capability probe.
The probe result is a *fact about this host*, determined impurely in `nerve-workstation`; the *tier
value* is pure data in `nerve-proto` (§3).

### 2.2 Per-platform closure

| Platform | FS scoping | Network deny | Syscall containment | Tier reachable | Status |
|---|---|---|---|---|---|
| **Linux (CI — primary)** | **Landlock** path-beneath rules (read = root + toolchain dirs; write = root + private tmp); kernel ≥5.13, ABI probed | **Network namespace** (`unshare(CLONE_NEWNET)`, no veth ⇒ no routable iface) — true deny, higher-ROI than socket-level seccomp | **seccomp-bpf denylist** of dangerous syscalls (ptrace, mount, bpf, kexec, module load, raw `socket`) — *not* a strict allowlist (would break `cargo`/`rustc`) | **`Hermetic`** when Landlock + netns established AND a closure digest is pinned | **NEW** |
| **macOS (dev box)** | `sandbox-exec` / Seatbelt SBPL (`file-read*`/`file-write*` scoped to root + tmp) | Seatbelt `(deny network*)` | none | **`BestEffort`** | **NEW (low priority)** |
| **Windows** | — | — | — | **`Unconfined`** | **explicit non-goal v1** (CI isn't Windows) |

> **Amended 2026-06-27 (see §7).** "macOS low priority / Windows non-goal" is the *build-order* call,
> not a platform lock: the honest tier already works everywhere, and the cross-platform guarantee is
> carried by the universal tier + a portable pinned Linux image (§7.5), not by native parity. macOS is
> first-class in intent, honestly capped at `BestEffort`; Windows reuses the Linux backend via WSL2.

**The `Hermetic` contract pins:** (1) declared FS roots (read: root + toolchain dirs; write: root +
ephemeral tmp; else denied); (2) no network by default (`NetPolicy::Deny` *enforced*, not intent —
`Allow` is opt-in and downgrades the tier, §5); (3) a pinned toolchain digest (extend
`toolchain_pin.rs` to capture tool **versions** into `ToolchainPin.tools` and an OCI `image_digest`
via the declared-but-unimplemented `EnvironmentPinner` trait; populate `repo_snapshot_hash` on the
delegate path; upgrade it from path+size proxy toward per-byte/Merkle); (4) a determinism-pinned env
(keep the allowlist+secret-scrub, and **force** `LANG=C`, `LC_ALL=C`, `TZ=UTC`, `SOURCE_DATE_EPOCH`
via the existing `env_overrides`).

### 2.3 Where selection happens (composition roots — the load-bearing wiring)

The launcher is chosen at three roots; the strong backend must be selected by `cfg!(target_os)` + a
runtime capability probe, defaulting **downward** on probe failure (fail-closed):

- `commands/verify.rs:43` — **today hardcodes `ProcessLauncher`.** The `nerve verify` / `nerve gate`
  CI path; the single most important line to make tier-aware.
- `daemon/setup.rs:39` — `process_launcher()` vs `refuse_launcher()` by interactivity.
- `commands/flow.rs:144`.

Add `crate::sandbox::strong_launcher_for_host() -> (Arc<dyn SandboxLauncher>, IsolationTier)` that
probes Landlock/Seatbelt support and returns the best available backend **plus the tier it can
honestly claim**. The verify runner threads that tier into the sealed Verdict and Receipt.

---

## 3. Invariant-safety: the honest "non-hermetic tier" mechanism (INV-R1/R2/R5)

**The court-reporter point: the sandbox must never let Nerve *fabricate* trust.** If isolation cannot
be established, the Run/Receipt must be **honestly marked degraded**, never silently claimed
bit-for-bit. This is *more* important than building the sandbox itself.

### 3.1 New pure vocabulary: `IsolationTier` (nerve-proto, additive)

Add to `crates/nerve-proto/src/provenance.rs` (pure serde, `rename_all="snake_case"`, derives `Eq`,
no floats):

```rust
/// How strongly the closure that produced this artifact was contained. A *fact*
/// about the launcher that actually ran (probed), never a request. Downgrade-only:
/// a probe failure yields a LOWER tier, never a higher one (INV-R1).
pub enum IsolationTier {
    /// Kernel-enforced closure (Landlock FS + net namespace [+ seccomp]) AND a
    /// pinned closure digest. The bit-for-bit claim is honest.
    Hermetic,
    /// Process-level containment only (scrubbed+pinned env, forced cwd, group-kill,
    /// net-deny INTENT) — today's `ProcessLauncher`. Replayable *modulo the host*.
    #[default]                       // unknown/legacy ⇒ the weaker honest claim
    Contained,
    /// Best-effort OS profile (macOS Seatbelt) — weaker than kernel-enforced Linux.
    BestEffort,
    /// No containment established (raw spawn / probe failed). Should not gate a pass.
    Unconfined,
}
```

The existing `Attestation` enum (`provenance.rs:229`, `Full`/`Partial`) is **about capture
completeness** (Nerve-recorded vs OTel-reconstructed) — an orthogonal axis; `IsolationTier` is new
and additive, not a rename. `Default = Contained` (not `Hermetic`) is the fail-closed choice: any
pre-existing serialized Run deserializes to the *weaker* honest claim, never a fabricated strong one.

### 3.2 Where the tier is stamped (signed, so a verifier can trust it)

- **Capture side:** `RunInputs.isolation_tier` — the tier the *agent run* was contained by. Additive,
  `skip_serializing_if` default so existing runs' `root_hash` is byte-stable.
- **Receipt side (load-bearing):** `isolation_tier` on `ReceiptProvenance` (`receipt.rs:90`), threaded
  through `issue_receipt_for_run` (`receipt_store.rs:178`) and `build_statement_with_bar`. Because it
  lands **inside the signed `ReceiptStatement`** (DSSE PAE), it is **co-sealed and tamper-evident**
  (INV-R5) — a malicious host cannot strip or upgrade it, exactly as the merge-bar already works. A
  third party reading the receipt offline now learns the verdict *and* how hermetic the re-run was.

### 3.3 The binding rule (extends INV-R1 — propose as **INV-R7**)

> **INV-R7 — Isolation is recorded as a probed fact, never an assumption.** A Run or Receipt may carry
> the `Hermetic` (bit-for-bit) tier **only** if the launcher that produced it established
> kernel-enforced isolation, confirmed by a runtime capability probe. The verify path stamps the tier
> the launcher **actually achieved**, never the one requested. Determination is downgrade-only and
> fail-closed: probe failure, an unsupported kernel, or a net-allowed run yields a *lower* tier. The
> tier is signed into the receipt statement (INV-R5). A verifier/gate may treat a sub-`Hermetic` tier
> as it sees fit, but Nerve must never present `Contained` work as `Hermetic`.

Purity split (INV-R2): the `IsolationTier` *type and serialization* are pure (`nerve-proto`); the
*probe* that selects a value, and the launcher that enforces it, are impure (`nerve-workstation`). The
launcher reports a fact upward; `nerve-core` only canonicalizes/signs it.

### 3.4 Turning honesty into an *optional* enforceable bar (still court-reporter)

Default behavior is **report, don't block** — emit the tier, gate on the org's checks as today. But
give the org the lever: `nerve gate --require-isolation hermetic` (and `NERVE_REQUIRE_ISOLATION`)
refuses to treat a receipt below the named tier as a pass — mapping to the existing **downgrade-only**
gate semantics (`receipt_gate.rs`): a passing verdict on a `Contained` receipt under a `hermetic`
requirement becomes **neutral exit 2** (never a fabricated pass, never an upgrade), with the tier
shortfall appended to the gate summary. This reuses the merge-bar enforcement kernel, so it inherits
its INV-R1 guarantees for free. The CI template's "Hermetic isolation is partial" concession becomes
"the tier is signed into every receipt; require `hermetic` to enforce."

---

## 4. What's enforceable now vs deferred — incremental build order

Ranked by **value × (1/effort)**. **Build (a) first** — it is the invariant fix and is days of
additive, fully-deterministic work that closes the *overclaim* even before any kernel sandbox exists.

| # | Brick | Effort | Value | Earns |
|---|---|---|---|---|
| **(a) ✅ SHIPPED (proto v16)** | **Honest tier + determinism pin** — added `IsolationTier` (proto, additive, `Unconfined < BestEffort < Contained[default] < Hermetic`); each launcher reports its real tier (`ProcessLauncher → Contained`, `RefuseLauncher → Unconfined`); threaded into `RunInputs` + the **signed** `ReceiptProvenance` (co-sealed in the DSSE statement); forced `LANG=C`/`LC_ALL=C`/`TZ=UTC` via `env_overrides`; added the optional downgrade-only `nerve gate --require-isolation` floor (+ `NERVE_REQUIRE_ISOLATION`). Both fields are `skip_serializing_if`-Contained so existing run `root_hash`es + `receipt_id`s are byte-identical (additive-invariance, zero golden churn). **Deferred to bricks (b)–(d):** `SOURCE_DATE_EPOCH` (no deterministic run-time value to borrow yet), populating `repo_snapshot_hash` on the delegate path, and recording tool versions. | **Low** | **Highest** — stops the lie; stabilizes evidence hashes | Pure + additive. Protocol bump (v15→v16), regen `docs/protocol/*`, drift test. **Done.** |
| **(b)** | **Linux Landlock FS closure** — `LandlockLauncher` enforcing `fs_read`/`fs_write`; select it in `commands/verify.rs` + daemon roots via probe. | **Medium** | **High (CI is Linux)** | `Contained →` partial-`Hermetic` |
| **(c)** | **Network namespace (+ seccomp)** — `unshare(CLONE_NEWNET)` for *enforced* net-deny (the biggest perturber + exfil vector); then a seccomp denylist. | **Medium** | **High** | reaches **`Hermetic`** on Linux |
| **(d)** | **Closure depth — `EnvironmentPinner` (OCI image digest) + per-byte repo snapshot.** | **High** | **Medium** | strengthens what `Hermetic` *means* |
| **(e)** | **macOS Seatbelt profile** — `SeatbeltLauncher`. | **Medium** | **Low** | `Unconfined → BestEffort` (dev box) |

**Recommendation:** **(a) is shipped** (proto v16 — the invariant-critical piece is done; the INV-R1
overclaim is closed and the `--require-isolation` lever exists). Next: **(b)+(c)** on Linux to actually
earn `Hermetic` where CI seals receipts. **(d)** deepens the closure *and* is the "Hermetic anywhere"
lever (§7.5); **(e)** is a dev nicety, last. The whole ladder is a **cross-platform** target, not a
Linux lock — see §7 for the per-OS backend kit (permissive primitives, since the cross-platform
`birdcage` crate is GPL-incompatible) and the portable pinned-image route.

---

## 5. Risks & open questions

1. **seccomp vs the org's real build.** Nerve's own tree-sitter C grammars compile at Nerve *build*
   time, not in the sandbox — not the hazard. The hazard is the *org's* checks (`cargo test`, `cc`,
   parallel `rustc`) needing a broad syscall set. A strict allowlist will break legitimate builds.
   **Mitigation:** start with a **denylist** of unambiguously-dangerous syscalls + Landlock for FS;
   net isolation comes from the namespace, not seccomp, so the syscall filter can stay loose.
2. **Strict isolation vs CI tests that legitimately need network/tools.** Network is `NetPolicy`-driven;
   `Allow` is opt-in and **downgrades the tier** (a net-allowed verify is, by definition, not
   bit-for-bit — mark it, never claim `Hermetic`). The honesty mechanism (§3) is exactly what makes
   this safe: we don't forbid network, we *disclose* it.
3. **Landlock kernel-version variance.** Older/self-hosted runners may lack Landlock or its net-rule
   ABI. **This is precisely why the tier must be a probed runtime fact (INV-R7), not a `cfg!`
   assumption** — probe the ABI, downgrade honestly to `Contained` if unavailable.
4. **Parallelism/scheduling nondeterminism is NOT solved by hermeticity — and the doc must say so.**
   Two *hermetic* runs of a racy test can still disagree. The honest answer is the existing
   flaky-detection (`reruns` ⇒ `Flaky` ⇒ `Inconclusive`); full record/replay determinism (rr-grade
   scheduling) is **out of scope** (trust-substrate §10: "we record the nondeterminism, we don't
   eliminate it"). **"bit-for-bit replayable" must be scoped to the record (the L0c tape) and the
   closure (the pinned inputs) — not to a promise that the verdict is identical on every re-run.**
5. **`repo_snapshot_hash` is a path+size proxy** and unpopulated on the delegate path. Earning
   `Hermetic` should include the per-byte/Merkle upgrade (brick d); until then a `Hermetic` claim
   rests partly on a coarse snapshot — arguably gate `Hermetic` on a non-proxy snapshot digest.
6. **macOS dev parity.** A receipt sealed on a macOS dev box is at best `BestEffort`/`Contained`;
   under `--require-isolation hermetic`, dev-local verify won't gate. That is **correct**
   (verification belongs on Linux CI) but must be documented so it's not surprising.
7. **Open API questions.** (i) Tier on `Output` or a separate `established_tier()`? (ii) `IsolationTier`
   in `provenance.rs` or a new `isolation.rs`? (iii) Should the L1 ledger's `RunRecorded`/`Verdict`
   carry the tier for queryability, or is the signed receipt sufficient? (iv) Net-allowed-but-pinned-
   mirror: a distinct tier or just `Contained`?

---

## 6. Files this design touches (for the eventual implementation)

- **New, additive (pure):** `IsolationTier` in `nerve-proto/src/provenance.rs`; new field on
  `RunInputs` (same file) and `ReceiptProvenance` (`receipt.rs`); regen `docs/protocol/runtime-v3.*`
  + drift test.
- **New backends (impure, workstation):** `sandbox/landlock.rs`, `sandbox/seatbelt.rs`; a
  `strong_launcher_for_host()` selector + capability probe in `sandbox/mod.rs`.
- **Wiring:** tier-aware launcher selection at `commands/verify.rs:43`, `daemon/setup.rs:39`,
  `commands/flow.rs:144`; thread the tier through `verify_runner.rs` (`seal_and_attest` /
  `issue_receipt_for_run`) and `receipt_store.rs:178`.
- **Determinism env:** extend `EnvPolicy`/`SandboxPolicy.env_overrides` defaults in `sandbox/mod.rs`;
  populate `repo_snapshot_hash` + tool versions in `toolchain_pin.rs`.
- **Pure seal pipeline:** `nerve_core::receipt::build_statement_with_bar` gains the tier param.
- **Gate lever:** `--require-isolation` in `commands/gate/mod.rs` reusing `nerve_core::receipt_gate`'s
  downgrade-only kernel.
- **Docs:** replace the "Hermetic isolation is partial" concession in
  `docs/integrations/ci-merge-gate.md`.

The whole plan is **additive, seam-respecting, and `nerve-core`-pure**: the strong sandbox lives
entirely in `nerve-workstation` behind the existing `SandboxLauncher` port (INV-R2), the tier is
signed protocol data added per the additive/versioned discipline (INV-R5), and brick (a) closes the
INV-R1 overclaim *before* any kernel work lands — which is the point.

---

## 7. Cross-platform strategy — earning the tier without locking to Linux

Added 2026-06-27. The §2/§4 ordering puts **Linux first** because the gating verify (the run that
seals a *trusted* Receipt) belongs in CI, and the dominant CI is Linux. But "Linux-first" is a build
**priority**, not a platform **lock**. The amendment below makes the cross-platform stance explicit and
records why we do **not** chase native sandbox parity on each OS.

### 7.1 Two layers — only the *strongest tier* is platform-specific

| Layer | Cross-platform today? | Why |
|---|---|---|
| **Honest `IsolationTier` (brick a)** | **Yes — all platforms now.** | It is pure `nerve-proto` data. macOS/Windows already run Nerve and seal honest receipts, stamped `Contained`/`Unconfined`. |
| **The default gate** | **Yes.** | `nerve gate` is **report, don't block** by default — no platform is locked out of using Nerve. |
| **Earning `Hermetic`** | **Per-OS kernel work.** | `Hermetic` requires kernel-enforced FS+net closure; the primitives differ by OS. |

So the only thing "Linux-only" today is the **opt-in** `--require-isolation hermetic` *floor*. Using
Nerve — capturing runs, gating on the org's checks, sealing signed receipts — is fully cross-platform
now. This is the court-reporter design working as intended: we never *forbid* a platform, we *disclose*
its tier.

### 7.2 A unified API does NOT unify the guarantee

The `SandboxLauncher` seam already lets each OS plug in its own backend with zero caller change
(§2.1). A cross-platform sandbox *library* can smooth the **code** across OSes — but it cannot smooth
the **guarantee**: Linux Landlock (a kernel LSM) and macOS Seatbelt (an Apple-**deprecated** policy
engine whose network denial is a filter, not namespace isolation) are not equally strong. Therefore the
probed `IsolationTier` (INV-R7) stays load-bearing **on top of** any cross-platform crate: the same API
still resolves to `Hermetic` on Linux and `BestEffort` on macOS. Claim less where we are less sure.

### 7.3 Cross-platform Rust sandbox crates — surveyed, and the license wall

| Crate | Coverage | Mechanism | License | Verdict |
|---|---|---|---|---|
| **`birdcage`** (phylum-dev) | Linux + macOS | Landlock (Linux) + Seatbelt/`sandbox-exec` (macOS), unified **FS + network** API — exactly our surface | **GPL-3.0-or-later** | **Cannot link.** This workspace is `MIT OR Apache-2.0`; GPL-3.0 copyleft is incompatible as a linked dependency. Excellent **open reference design**, not a dependency. |
| **`gaol`** (servo) | Linux + macOS + Windows | multiprocess broker | MPL-2.0 | Adds Windows, but self-described "lightly reviewed… not battle-tested" and effectively dormant — unfit for a determinism-critical dependency. |

**Conclusion:** there is no permissively-licensed, maintained, all-three-OS sandbox crate to adopt
wholesale. Build thin backends from permissive primitives behind the seam, using `birdcage` as the
design reference (its scope — FS + network only — is precisely ours).

### 7.4 The permissive backend kit (all impure, `nerve-workstation`-only, zero `nerve-core` change)

| OS | FS closure | Network deny | Syscall | Earns | Primitive (license to confirm via `cargo-deny`) |
|---|---|---|---|---|---|
| **Linux** | `rust-landlock` (Apache-2.0/MIT) | `nix`/`rustix` `unshare(CLONE_NEWNET)` | `seccompiler` (Apache-2.0) denylist | **`Hermetic`** | all permissive |
| **macOS** | Seatbelt SBPL via direct `sandbox_init` FFI (a system API — no crate-license issue) | Seatbelt `(deny network*)` | — | **`BestEffort`** (honest cap — deprecated + filter-not-namespace) | FFI, no third-party license |
| **Windows** | defer native (AppContainer / Job Objects) | — | — | — | **pragmatic path: WSL2 reuses the Linux backend** |

A `cargo-deny` license gate (the repo has no `deny.toml` yet) should be added alongside the first
backend so a GPL/again-incompatible transitive dep can never slip into an `MIT OR Apache-2.0` product.

### 7.5 The portable "Hermetic anywhere" route — preferred for the strongest claim

The robust cross-platform answer is **not** to reimplement each OS's weaker native sandbox, but to
**decouple the dev-host from the seal-host** (the Bazel-RBE / Nix pattern): run the gating verify inside
a **content-addressed pinned Linux OCI image** (brick (d)'s `EnvironmentPinner`) via a local container
runtime, so any host — Mac, Windows, Linux — reaches **one verified `Hermetic` path** instead of three
uneven native ones. This also fills the currently-empty `repo_snapshot_hash` / toolchain closure (§1.1),
so the `Hermetic` claim rests on a real per-byte closure rather than a path+size proxy.

### 7.6 Revised stance (amends the §2.2 "macOS low priority / Windows non-goal" framing)

- **macOS is first-class in *intent*** — Mac CI is real (iOS/Swift shops *are* macOS CI). It gets a
  Seatbelt backend so Mac verify is not bottom-tier, but it is **honestly capped at `BestEffort`**
  (INV-R1), never a fabricated `Hermetic`.
- **The cross-platform guarantee is carried by (a) the universal honest tier and (b) the portable
  pinned image** — not by faking native parity on each OS's sandbox.
- **Build order is unchanged** (Linux (b)+(c) first, where the gating CI runs), but the target is
  explicitly a *cross-platform* substrate, with the portable image (d) as the "Hermetic anywhere" lever.
