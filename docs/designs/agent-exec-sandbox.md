# Agent command execution + containment (`run_command` behind a `SandboxLauncher`)

Status: **Phase 1 (MVP) implemented** — `run_command` (argv) behind `--allow-exec`
(default off), P4 `Ask` per call, `SandboxPolicy` + `ProcessLauncher` (cwd / env
scrub / wall-clock group-kill / output cap / net-deny intent) for the CLI, and a
`RefuseLauncher` for the daemon / session / subagent paths. Deferred (future
seams, intentionally NOT built): Linux Landlock/seccomp + cgroups, macOS
App-Sandbox, microVM/WASM, shell-mode (pipes/redirects), and the daemon approval
round-trip. See §5/§8 below.
Date: 2026-06-19
Related: `docs/designs/architecture-north-star.md` (P4 permission engine, seams),
the prior sandbox analysis (cross-platform isolation asymmetry).

## 1. Problem

nerve's agent can search / read / navigate / **edit** code, but it has **no way to
execute anything** — verified: the only `std::process::Command` uses are the read-only
`git` tool, MCP-server spawn, OAuth browser-open, and CLI install/doctor. There is no
`exec` / `shell` / `run_command` tool. So for a *code* workstation the agent **edits
blind**: it cannot run `cargo test`, `npm run build`, a linter, or a script to verify its
own change. This is now the dominant capability gap.

Execution is also the **highest-risk** capability and the trigger for the long-deferred
**sandbox** decision: adding exec forces the containment story. Binding constraints (from
the project owner, earlier): **default-off; behind a `Sandbox` port; in the binary, never
`nerve-core`.**

## 2. Goals / non-goals

**Goals**
- A `run_command` tool so the agent can build / test / lint / run scripts and read results.
- **Safe by construction**: default-off, permission-gated (P4), bounded (cwd / timeout /
  output / env / net), behind a **`SandboxLauncher` port** so containment can strengthen
  without touching callers.
- Zero `nerve-core` change (execution is non-deterministic → workstation only).

**Non-goals (deferred / separate seams)**
- Strong kernel isolation on day one (see §5 — it is OS-asymmetric).
- microVM (firecracker/microsandbox) and WASM (wasmtime) sandboxes — these are **separate
  future seams**, *not* backends of this port (avoid premature over-abstraction).
- Arbitrary shell strings with pipes/redirection (MVP is argv, no shell — see §4.1).

## 3. Two-gate safety model + the containment seam

Execution passes **two independent gates**, then runs **contained**:
1. **Capability gate (is exec even available?)** — the `run_command` tool is **not
   registered** unless explicitly enabled (`--allow-exec` / config). Off by default, so a
   default agent simply cannot execute.
2. **Authorization gate (may I run *this* call?)** — reuses the **P4 `ToolGate`**:
   `run_command` is an Ask-class tool (like writes), so each call prompts unless
   `--allow-all`. P4 answers *"are you allowed?"*.
3. **Containment (what can the process touch?)** — the **`SandboxLauncher` port** answers
   *"what can it reach?"*: cwd, fs, env, net, time, output size. Sandbox ≠ authorization;
   they compose.

## 4. Design

### 4.1 The `run_command` tool (`nerve-workstation`, ToolBox seam)
A `ToolBox` decorator (the shipped `CheckpointToolBox`/`MemoryToolBox` pattern), registered
only when exec is enabled. Args: `{ command: string, args: [string], cwd?: string }` —
**argv, no shell interpretation** (no injection surface; the agent calls it N times instead
of chaining with `&&`). Returns `{ exit_code, stdout, stderr, timed_out }`, each stream
capped (e.g. 32 KiB, head+tail) like tool-output capping elsewhere.

### 4.2 The `SandboxLauncher` port (`nerve-workstation`) — the new seam
```
trait SandboxLauncher {
    fn launch(&self, spec: &CommandSpec, policy: &SandboxPolicy) -> Result<Output>;
}
struct SandboxPolicy {
    cwd: PathBuf,            // defaults to the workspace root
    fs_read: Vec<PathBuf>,   // workspace (+ toolchain dirs)
    fs_write: Vec<PathBuf>,  // workspace only
    net: NetPolicy,          // Deny by default
    env: EnvPolicy,          // allowlist (PATH, HOME, toolchain) — scrub secrets
    timeout: Duration,       // hard wall-clock kill
    max_output: usize,
}
```
Policy is **data**, derived from the workspace root + config — composing with P4 the same
way (P4 = authorization, this = containment). Matches nerve's port culture
(`CatalogProvider`/`LlmProvider`/`MemoryStore`).

### 4.3 Backends (the port's whole point)
- **MVP — `ProcessLauncher` (trusted-local):** spawn argv with the policy's **cwd, env
  allowlist (secrets scrubbed), wall-clock timeout, output cap**. This bounds blast radius
  but is **not** strong isolation (a determined process can still escape on a dev box). Safe
  for the *trusted local developer* who already runs these commands by hand — and that is
  the honest default audience.
- **Linux — `LandlockLauncher` (strong):** Landlock fs scoping + seccomp syscall filter
  (+ optional cgroups/namespaces). This is where real isolation lives (cloud / CI / shared).
- **macOS — best-effort:** `sandbox-exec`/Seatbelt is deprecated and App Sandbox needs a
  bundle, so macOS stays best-effort/trusted (documented, not pretended).

### 4.4 Determinism / placement
Execution is non-deterministic → the tool, the port, and all backends live in
`nerve-workstation`. `nerve-core` stays pure (its only subprocess is the read-only `git`
tool). No protocol change (it is just another ToolBox tool + a hidden capability flag).

## 5. The load-bearing reality: isolation is OS-asymmetric

Strong containment is **Linux-strong, macOS-weak** (Landlock/seccomp/microVM vs deprecated
Seatbelt). So the realistic long-term shape is **"strong isolation on Linux (cloud/CI),
macOS dev = best-effort/trusted"** — which is exactly why this is a **port**: the MVP ships
the trusted-local backend everywhere, and Linux gets the strong backend without changing the
tool or any caller. Do **not** block the MVP on solving macOS isolation (it is a platform
gap, not a design gap).

## 6. Anti-footgun checklist
default-off capability · P4 Ask per call · argv (no shell) · cwd = workspace · fs-write =
workspace · **net Deny by default** · env allowlist (scrub `*_TOKEN`/`*_KEY`/`*_SECRET`) ·
hard timeout · output cap · every run emits a `ToolStarted/Finished` event (auditable).

## 7. Architecture fit (north-star)
- New **`SandboxLauncher` seam** (workstation) — the declared entry point for containment,
  added to the seam scorecard. Tool enters via the existing `ToolBox` seam; authorization via
  P4; composition only in the binary.
- `nerve-core`/`nerve-runtime`/protocol untouched. microVM + WASM remain **separate future
  seams**, not backends here (per the prior analysis — avoid premature abstraction).
- **The permission gate must be the OUTERMOST toolbox decorator** so `run_command` (and every
  tool) is gated by P4 — north-star invariant 9. The stack currently has the gate inner, so
  this reorder (the `AgentAssembly` refactor) is a **prerequisite** that lands before exec.
- **Execution capability is bound to the trust context, not just `--allow-exec`.** `run_at_depth`
  is shared by the CLI and the daemon/session paths: a local CLI run may use the best-effort
  `ProcessLauncher`; a daemon-served / remote run must require a strong-isolation backend or
  refuse exec. Encode the trust assumption in launcher selection, not in user discipline.

## 8. Phasing
- **MVP** (✅ implemented, this): `run_command` (argv) + `--allow-exec` default-off + P4 Ask +
  `SandboxPolicy` + `ProcessLauncher` (cwd/env/timeout/output/net-deny) + tests. Trusted-local.
  Containment notes: the wall-clock timeout kills the whole **process group** (so forked build
  tools die and release the pipes); output capping keeps **head+tail**; the env scrub is
  name-based (value-embedded `user:pass` in e.g. index URLs is **not** sanitized — credential-prone
  prefixes like `NPM_CONFIG`/`PIP_` are simply not allowlisted). `ProcessLauncher` does not enforce
  the `fs_read`/`fs_write`/`net` policy fields — those are the contract for the strong backend below.
- **P-next**: `LandlockLauncher` (Linux strong) selected at composition by target/config.
- **Later**: shell-mode opt-in; macOS App-Sandbox bundle; microVM/WASM as their own seams.

## 9. Open decisions
1. Tool name: `run_command` (recommended, argv) vs `exec`/`shell`.
2. Net default: **Deny** (recommended) vs allow (builds that fetch deps need allow — maybe a
   policy preset `--exec-net` for dependency fetches).
3. Capability surface: a dedicated `--allow-exec` flag (recommended) vs folding into a
   broader capability/permission config.
4. Subagent exec: inherit the parent's exec capability, or always-off for subagents?
5. Timeout/output defaults (e.g. 120 s / 32 KiB).

## 10. References
- **nerve** — P4 `ToolGate` (`crates/nerve-workstation/src/policy.rs`), ToolBox-decorator
  pattern (`checkpoint.rs`/`memory.rs`), read-only `git` exec (`nerve-core/dispatch/git.rs`),
  MCP subprocess (`mcp/client.rs` — today's only untrusted spawn). North-star P4 + invariants.
- **Prior analysis** — cross-platform isolation asymmetry; microVM (firecracker/microsandbox)
  & WASM (wasmtime) as separate seams; "confined subprocess execution", not a universal sandbox.
