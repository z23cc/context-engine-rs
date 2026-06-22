# Agent config & model selection (claude/codex-style CLI)

status: proposed
date: 2026-06-19

## Problem
`nerve chat` and `nerve agent run` require `--provider` and `--model` on every
invocation (`chat.ts` exits 2 if missing; `agent.rs` errors unless `--agent`
supplies them). claude/codex-style CLIs instead: launch with no args, keep a
persisted default, switch model in-session (`/model`), and treat login as a
separate one-time step. nerve already has the last one (`nerve agent login` +
credential store); this design adds the rest, in stages.

## Non-goals
- Not changing auth (already OAuth login + credential store).
- Not a GUI picker (this is the CLI/TUI surface).
- S1/S2 add **no** protocol vocabulary; only S3 does (and only through
  `nerve-runtime`, versioned + codegen).

## Layered resolution (the model)
Precedence for `(provider, model)`, highest first:
1. explicit `--provider` / `--model` flag (scripts, one-offs)
2. `--agent NAME` definition (P3 agent-def; already resolved in `run_task`)
3. persisted user default (`config_home()/config.json`)
4. interactive picker on a TTY → writes the choice back to (3)
5. else: actionable error

This mirrors the P4 policy layering and is the concrete first slice of roadmap
**P2 (provider registry / config)**.

## Storage
`config_home()/config.json` — sibling of `agent-auth.json` (`config_home()` =
`$NERVE_HOME` → `$XDG_CONFIG_HOME/nerve` → platform config dir). JSON, not TOML:
dependency-free and consistent with the existing credential file.

```json
{ "default_provider": "claude", "default_model": "grok-4-fast" }
```

User-owned, readable, editable — same transparency principle as the memory files.

## Stage 1 — config default + zero-arg + first-run picker (no protocol change)
- New `runconfig` module (workstation): `load()/save()` for
  `RunConfig { default_provider, default_model }`, a pure `decide()` precedence
  helper, and `resolve(provider, model, interactive) -> (String, String)`.
- `resolve`: merge args over config; if both known → return; else if a TTY and
  `interactive` → run the picker, persist, return; else → error.
- Picker: list **logged-in** providers (query the credential store for
  Anthropic / OpenAI / xAI), let the user choose; prompt for a model id with a
  suggested example; persist both. Provider stored canonically
  (`claude`/`chatgpt`/`xai`, accepted everywhere via `parse_builtin`).
- Wire both surfaces:
  - `nerve agent run`: `resolve(args.provider.or(def), args.model.or(def), true)`.
  - `nerve chat`: parse explicit flags (replacing the arg-passthrough), `resolve`,
    then exec `nerve-tui --binary <engine> --provider P --model M [...]`.
    Bonus: `nerve chat --help` now shows real flags (fixes the clap-passthrough
    wart).
- Safety: picker only on a TTY; non-interactive (daemon, CI, pipe) still fails
  closed with a clear message. No network in the picker.

## Stage 2 — in-session slash commands
TUI gains `/model`, `/provider`, `/models` (list from the xai/openai catalogs),
`/login`, and `/lease` (request brokered OAuth lease metadata with token output redacted). `/login --device`
is a UX affordance over the auth/mobile roadmap's additive `auth.start.flow=device_code` protocol seam and
is intentionally fail-closed until provider device-code endpoints are wired. Clients should read `auth.status`
capabilities to discover that state instead of probing failed starts. `/lease` is a client capability check,
not a way to print copyable bearer or refresh credentials; runtime lease jobs expose metadata only, and
browser clients still send `include_token=false` as a defense-in-depth signal. `/model X` is implemented client-side: `session.close` then
`session.start` with the new model, re-seeding history (the orchestrator already
supports `with_history`). Matches codex `/model` UX without touching the protocol.

## Stage 3 — protocol-native session retargeting (protocol change)
Add `session.set_model` to the runtime protocol (`nerve-runtime` authority,
additive + versioned + codegen + drift-check). The daemon swaps the model on a
live session and persists state; the TUI uses it instead of close/restart. This
is the long-term form and folds the registry into the protocol (roadmap P2).

## North-star alignment
- S1/S2: pure composition-root + client work; no `nerve-core`, no protocol
  change. Config is declarative data read at the binary.
- S3: the only protocol change, through the single protocol authority, versioned.
  Engine/client stay decoupled (`nerve chat` is a thin launcher; the client
  speaks only the protocol).

## Open decisions
1. Picker model entry: free-type with a suggestion (S1, no network) vs. list from
   catalogs (needs network / per-provider catalog; deferred to S2 `/models`).
2. A `nerve config model|provider` set/show subcommand to edit defaults without
   the picker (small; could land with S1 or S2).
3. Per-project override (`.nerve/config.json`) layered over the global default
   (defer until asked).
