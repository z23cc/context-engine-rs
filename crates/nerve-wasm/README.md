# nerve-wasm (legacy, non-workspace)

This directory holds a **legacy generated WASM artifact** (`pkg/ctx_wasm*`), built from
an older MCP/tool-call WASM surface (`ctx_wasm`). It is intentionally:

- **Not a Cargo workspace member** — it is not listed in the root `Cargo.toml`
  `members`/`default-members`, so `cargo build/test --workspace` never touches it.
- **Not the runtime protocol.** The single protocol authority is `crates/nerve-proto`
  (re-exported by `nerve-runtime`, drift-checked against `docs/protocol/runtime-v3.*.json`).
  The committed `pkg/*.d.ts` here describe the old MCP/tool-call surface, **not** the
  `RuntimeCommand` / `RuntimeEvent` vocabulary. Do not mistake it for a parallel
  protocol authority.
- **The current WASM frontend** is `crates/nerve-gui` (Leptos CSR), which depends on
  `nerve-proto` for the exact engine protocol types — see `CLAUDE.md`.

Kept (not deleted) only as a historical reference artifact. Do not wire new code to it.
