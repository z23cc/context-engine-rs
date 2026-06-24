# Codex-style Visual Spec & Checklist — `nerve-gui`

Status: **implementation spec for G4 ("final Codex styling")** — see
`crates/nerve-gui/README.md`. Subordinate to `docs/designs/gui-architecture.md`: this
document governs **only pixels and layout**, which the north star is silent on. The wire
seam is frozen — every change here is CSS / Leptos `view!` markup only. **No new
`RuntimeCommand` / `RuntimeEvent` / method.**

Date: 2026-06-21

> **Positioning note (2026-06-24):** governed by `docs/designs/trust-substrate.md` — Nerve's moat is the deterministic flight-recorder + execution-grounded re-verifier (replayable **Run** + signed **Receipt**); the `delegate.*` cockpit is the distribution body. Under that thesis, this doc is the visual spec for the cockpit — the surfaces it styles (structured delegate events, the Review accept/reject gate, and Run-replay/Receipt) are what render that flight-recorder + re-verifier to the human.

## 1. Scope & honesty caveat

Goal: make `crates/nerve-gui` read as a sibling of the **OpenAI Codex desktop app** without
shipping any proprietary asset.

- **No proprietary assets.** No OpenAI/Codex wordmark or logo, no bundled SF Pro (we only
  *reference* the system font via `-apple-system`), no copied icon artwork. Icons are
  inline geometric SVG or an MIT/ISC open set (Lucide/Feather). The "Nerve" brand and the
  `N` spark stay ours. Matching *layout, structure, spacing, and a neutral aesthetic* is
  fair; copying *artwork or marks* is not.
- **Exact Codex hex/metrics are not public** and the local `Codex.app` could not be
  inspected (computer-use safety block). The color tokens below are a **faithful neutral
  approximation** derived from Codex's documented theming model (base theme + accent / bg /
  fg + UI font + code font), its macOS-native design guidance, and public screenshots — not
  official values. They are tuned to the *traits* Codex is known for: near-greyscale
  neutrals, hairline borders, restrained monochrome accent, generous whitespace.

### Sources

- [App – Codex (OpenAI Developers)](https://developers.openai.com/codex/app) — three-pane workspace.
- [Features – Codex app](https://developers.openai.com/codex/app/features) — composer modes, top controls, task sidebar.
- [Settings – Codex app](https://developers.openai.com/codex/app/settings) — theming model: base theme + accent/bg/fg + UI/code fonts.
- [Build a Mac app shell (sidebar/detail/inspector)](https://developers.openai.com/codex/use-cases/macos-sidebar-detail-inspector) — `NavigationSplitView`, neutral system-material sidebar, designed detail/inspector cards.
- [Introducing the Codex app](https://openai.com/index/introducing-the-codex-app/) · [Complete Beginner's Guide (getpushtoprod)](https://getpushtoprod.substack.com/p/complete-beginners-guide-to-openais) · [Inside the Codex App Workspace (codex.danielvaughan.com)](https://codex.danielvaughan.com/2026/04/17/codex-app-workspace-pr-review-task-sidebar-artifact-viewer/) — IDE-like feel, projects→threads, model picker, plan mode, PR-review/task-sidebar/artifact panes.

## 2. The Codex "feel" in one paragraph

An **IDE-shaped, agent-native** workspace: a neutral, near-monochrome surface that gets out
of the way of code and diffs. Three columns — **project sidebar / active thread / review
(task) pane** — with a calm greyscale palette, **hairline** dividers (not boxes), soft
medium corner-radii on cards, a restrained near-black/near-white accent (blue reserved for
focus/links), and a **hero composer** that carries a Local / Worktree / Cloud mode selector.
Density is comfortable, not cramped; typography is the OS UI font with a monospace code font.

## 3. Design tokens

Drop-in replacement for the `:root` / dark block at the top of `styles.css`. Deltas from the
current palette are intentional: brighter white detail surface, warmer/softer neutrals,
hairline borders, larger card radii, a pill radius, and an inspector width.

```css
:root {
  color-scheme: light dark;

  /* surfaces — sidebar neutral, detail bright white (Codex trait) */
  --bg:         #fbfbfa;   /* app background (thread/detail), warm near-white */
  --surface:    #ffffff;   /* cards: composer, tool cards, modal */
  --surface-2:  #f3f3f1;   /* recessed: reasoning, secondary fills */
  --sidebar:    #f4f4f2;   /* neutral source-list column */
  --inspector:  #faf9f7;   /* right review/task pane */
  --hover:      #ececea;
  --active:     #e3e3e0;

  /* hairlines — thinner/softer than before */
  --border:      #e7e7e3;  /* primary divider / card edge */
  --border-soft: #efefec;  /* internal hairline */

  /* ink */
  --fg:    #131312;
  --muted: #5b5b56;
  --faint: #8c8c85;

  /* accent — monochrome by default (Codex restraint); user-themeable */
  --accent:     #0d0d0d;
  --accent-ink: #ffffff;
  --focus:      #2f6fed;   /* blue reserved for focus rings + links only */

  /* semantic */
  --ok:   #1a7f4b;
  --warn: #8a5a00;
  --err:  #b42318;

  /* code */
  --code:        #f5f5f3;
  --code-border: #e7e7e3;

  /* radii — softer cards, full pills, small chips */
  --r-card:    12px;   /* composer, tool cards, modal, empty card */
  --r-control: 8px;    /* buttons, inputs, nav rows */
  --r-chip:    6px;    /* badges, tiny glyph tiles */
  --r-pill:    999px;  /* mode segments, model picker */

  /* metrics */
  --col:            720px;  /* thread reading column (was 680) */
  --sidebar-width:  260px;  /* was 252 */
  --inspector-width:340px;  /* right pane when open */
  --topbar-h:       44px;
  --fs-ui:    14px;
  --fs-label: 11px;   /* uppercase section labels */
}

@media (prefers-color-scheme: dark) {
  :root {
    --bg:         #131312;  /* true-dark, slightly warm */
    --surface:    #1d1d1b;
    --surface-2:  #242422;
    --sidebar:    #161615;
    --inspector:  #161615;
    --hover:      #2a2a27;
    --active:     #343431;
    --border:      #2c2c29;
    --border-soft: #232321;
    --fg:    #f0f0ec;
    --muted: #b3b1aa;
    --faint: #807e77;
    --accent:     #f0f0ec;  /* near-white accent inverts in dark */
    --accent-ink: #131312;
    --focus:      #6fa1ff;
    --ok:   #5fbf87;
    --warn: #d39b38;
    --err:  #ff7b72;
    --code:        #1a1a18;
    --code-border: #2c2c29;
  }
}
```

### Theming hook (matches Codex's "accent / bg / fg + fonts" settings)

Codex theming is exactly `accent`, `background`, `foreground`, UI font, code font, named
base themes. The token layer above already maps 1:1. To expose it later, set overrides on
`:root` (or a `data-theme` attribute) — e.g. a named theme is just a class that re-declares
the six knobs:

```css
:root { --font-ui: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
        --font-code: ui-monospace, "SF Mono", Menlo, Consolas, monospace; }
[data-theme="codex-warm"] { --bg:#fbfaf7; --accent:#0d0d0d; /* ...accent/bg/fg knobs... */ }
```

Default monochrome accent is the correct Codex default — do **not** ship a colored accent.

## 4. Typography

| Role | Family | Size / weight | Notes |
|---|---|---|---|
| UI / transcript body | `var(--font-ui)` | 14px / 400, line-height 1.6 | system SF Pro — already correct |
| Sidebar rows, nav, topbar title | UI | 13px / 500 | |
| Section labels ("Projects", "Threads") | UI | 11px / 600, **uppercase**, `letter-spacing: .04em` | currently lowercase 11px — make uppercase + tracked |
| Empty-state greeting | UI | 26px / 600, `letter-spacing: -.01em` | down-weight from 650 bold feel |
| Code / tool names / diffs | `var(--font-code)` | 12.5px | already correct |

Action: add `letter-spacing: .04em; text-transform: uppercase;` to `.rail-label`; bump
`.empty-title` to 26px and `font-weight: 600`.

## 5. Layout — adopt the three-pane shell

Codex is **sidebar / thread / review-task pane** (`NavigationSplitView`: source list →
detail → inspector). `nerve-gui` is two-pane today. Make the third pane an **optional,
collapsible inspector** so the default stays a clean two-pane chat and the third column
appears only when there is a plan/diff/artifact to show.

`styles.css` — `#nerve-shell`:

```css
#nerve-shell {
  display: grid;
  grid-template-columns: var(--sidebar-width) minmax(0, 1fr);  /* default: 2-pane */
  height: 100vh; min-width: 0;
}
#nerve-shell.with-inspector {
  grid-template-columns: var(--sidebar-width) minmax(0, 1fr) var(--inspector-width);
}
.inspector {
  display: flex; flex-direction: column; min-width: 0;
  background: var(--inspector);
  border-left: 1px solid var(--border);
}
```

`app.rs` — add an `inspector_open: RwSignal<bool>` and an `<aside class="inspector">` sibling
after `<main>`; toggle `class:with-inspector=move || inspector_open.get()` on `#nerve-shell`.
Inspector contents (later): **Plan** (decomposed steps + progress), **Sources** (files/URLs
consulted — fold from `ToolCard` paths), **Artifacts**, **Changes** (diff). For G4 ship the
shell + Plan tab; wire diff/artifacts when those events exist. The right pane is Codex's
signature — even an empty, collapsible version moves fidelity the most.

## 6. Component checklist

Each item: **Codex** → **current `nerve-gui`** → **concrete change** (`app.rs` markup +
`styles.css`).

### 6.1 Sidebar — project-grouped, real icons

- **Codex:** top-level = **Projects** (long-lived); **Threads** nested under the active
  project; plus **Chats** (project-less), **Skills**, **Automations**. Rows are
  `icon + title (+ optional secondary line)`. Background is a neutral system material.
- **Current:** brand `N Nerve`; "New thread"; nav rows `Threads/Automations/Skills` with
  **letter glyphs** (`T`/`A`/`S`); one "Projects" row; flat "Threads" rail; status row.
- **Change:**
  - Replace letter glyphs with inline 16px SVG icons (Lucide: `message-square`, `zap`,
    `sparkles`, `folder`). Keep `.nav-icon` box at 16–17px; set `color: var(--faint)`.
  - Make section labels uppercase + tracked (see §4).
  - Re-order to Codex's hierarchy: **New thread** → nav (**Threads**, **Chats**,
    **Automations**, **Skills**) → `Projects` label + project row(s) → `Threads` label +
    thread rail. Nest the thread rail visually under the active project (indent 8px,
    optional 1px guide on `.rail`).
  - Sidebar row secondary line: show relative time / model under long titles
    (`.rail-row` → two-line variant, `.rail-sub { color: var(--faint); font-size: 11px; }`).
  - Optional macOS material: `.sidebar { background: var(--sidebar); }` plus, behind a
    capability flag, `backdrop-filter: saturate(180%) blur(20px);` with a translucent
    `--sidebar` for vibrancy in the Tauri webview.
  - Quiet the brand: 13px/600 is fine; keep the `N` spark tile but at `--r-chip`.

### 6.2 Top bar — title + actions cluster, picker as a pill

- **Codex:** thread title left; right cluster = **terminal toggle (⌘J)**, **model picker**,
  **pop-out/detach**. Minimal, icon-driven, hairline bottom border.
- **Current:** title left; right = two raw text `<input>`s (`provider` / `model`) + an
  `Apply` button. Functional but reads like a form, not Codex.
- **Change:**
  - Collapse provider+model into a single **model-picker pill** button:
    `claude · opus-4-8 ▾` styled with `--r-pill`, `--surface`, hairline border, 12px muted
    text. Open a small popover/menu for editing (keep the existing text inputs inside the
    popover so behavior is unchanged; `Apply` lives in the popover footer).
  - Add two icon-buttons left of the picker: **terminal** (`square-terminal`) and
    **pop-out** (`picture-in-picture-2` / `external-link`). Wire pop-out/terminal as no-ops
    or to existing toggles for now.
  - `styles.css`:
    ```css
    .topbar { min-height: var(--topbar-h); padding: 0 14px; gap: 8px;
              border-bottom: 1px solid var(--border-soft); }
    .icon-btn { display:flex; align-items:center; justify-content:center;
                width:30px; height:30px; border:0; border-radius: var(--r-control);
                background:transparent; color:var(--muted); }
    .icon-btn:hover { background: var(--hover); color: var(--fg); }
    .model-pill { display:flex; align-items:center; gap:6px; min-height:30px;
                  padding:4px 10px; border:1px solid var(--border);
                  border-radius: var(--r-pill); background: var(--surface);
                  color: var(--muted); font-size:12px; }
    .model-pill:hover { background: var(--hover); color: var(--fg); }
    ```

### 6.3 Empty / home state — composer-as-hero

- **Codex:** opening a new thread is task-first: a centered greeting and the **composer is
  the focal element**, with light suggestion affordances. Not a logo splash.
- **Current:** centered `N` tile + "Let's build" + "nerve-workstation". Logo-splashy.
- **Change:** when `turns.is_empty()`, render a centered column: greeting **"What should we
  work on?"** (26px/600), a one-line subtitle with the project name (`var(--muted)`), and a
  row of **suggestion chips** (`Plan`, `Ask`, `Explain this repo`) that prefill the composer.
  Keep the real composer docked at the bottom (Codex behavior), or, for the empty state only,
  float a composer copy directly under the greeting. Drop the big `N` mark.
  ```css
  .empty { margin:auto; max-width: var(--col); text-align:center; }
  .empty-title { font-size:26px; font-weight:600; letter-spacing:-.01em; color:var(--fg); }
  .empty-sub { margin-top:6px; color:var(--muted); font-size:13px; }
  .suggests { display:flex; gap:8px; justify-content:center; margin-top:18px; flex-wrap:wrap; }
  .chip { padding:6px 12px; border:1px solid var(--border); border-radius:var(--r-pill);
          background:var(--surface); color:var(--muted); font-size:12px; }
  .chip:hover { background:var(--hover); color:var(--fg); }
  ```

### 6.4 Composer — the Codex tell: mode selector + affordances

- **Codex:** a prominent rounded card with a **Local / Worktree / Cloud** segmented mode
  selector, an attach (image/file) control, voice (⌘/Ctrl+M), `/` for commands, the text
  input, and the send button. This segmented mode row is the single most recognizable Codex
  element and is **absent** today.
- **Current:** one rounded box = textarea + circular send (`↑`) / stop (`■`).
- **Change:** restructure `.composer-inner` into **two rows**: a top toolbar row (mode
  segments left; attach + mic right) and the input row (textarea + send). Raise card radius
  to `--r-card`, add a soft shadow.
  - `app.rs` markup sketch:
    ```rust
    <div class="composer-inner">
      <div class="composer-bar">
        <div class="seg" role="tablist">
          <button class="seg-item on">"Local"</button>
          <button class="seg-item">"Worktree"</button>
          <button class="seg-item">"Cloud"</button>
        </div>
        <div class="composer-tools">
          <button class="icon-btn" title="Attach">/* paperclip svg */</button>
          <button class="icon-btn" title="Dictate (⌘M)">/* mic svg */</button>
        </div>
      </div>
      <div class="composer-input-row">
        <textarea class="input" /* …unchanged props… */ placeholder="Describe a task…  /  for commands"></textarea>
        /* send / stop button unchanged */
      </div>
    </div>
    ```
  - `styles.css`:
    ```css
    .composer-inner { display:flex; flex-direction:column; gap:8px; padding:10px 10px 8px 12px;
      background:var(--surface); border:1px solid var(--border); border-radius:var(--r-card);
      box-shadow: 0 1px 2px rgba(0,0,0,.04), 0 8px 24px rgba(0,0,0,.04); }
    .composer-inner:focus-within { border-color: color-mix(in srgb, var(--focus) 60%, var(--border)); }
    .composer-bar { display:flex; align-items:center; justify-content:space-between; }
    .seg { display:inline-flex; padding:2px; gap:2px; background:var(--surface-2);
           border-radius:var(--r-pill); }
    .seg-item { padding:4px 11px; border:0; border-radius:var(--r-pill); background:transparent;
                color:var(--muted); font-size:12px; }
    .seg-item.on { background:var(--surface); color:var(--fg);
                   box-shadow:0 1px 2px rgba(0,0,0,.08); }
    .composer-tools { display:flex; gap:2px; }
    .composer-input-row { display:flex; align-items:flex-end; gap:10px; }
    ```
  - Keep the send button, but consider a slightly smaller 30px circle and `--r-pill`; the
    near-black fill already matches Codex.

### 6.5 Transcript, tool cards, reasoning — calmer cards

- **Codex:** assistant text flush-left in the reading column; tool/diff output as quiet,
  hairline-bordered cards; user turns lightly distinguished.
- **Current:** already close — user bubble right-aligned; assistant `.md`; `.tool` cards;
  `<details class="reasoning">`. Good bones.
- **Change (polish):**
  - Tool cards to `--r-card`, header to a monospace name + a small status **dot** (not a
    word badge): `running` (amber pulse), `ok` (green), `err` (red). Keeps the IDE feel.
  - Reasoning: render as a collapsible "Thought for …" row in `--faint`, matching Codex's
    quiet reasoning treatment; body in `--surface-2` (already so).
  - User turn: keep the bubble but soften to `--surface-2` fill + no border, `--r-card`.
  - Increase inter-turn gap to 28px (`.transcript { gap:28px; }`) for Codex breathing room.

### 6.6 Model picker (detail) & 6.7 Approval modal

- **Model picker:** Codex shows model + reasoning level (low/med/high). The pill popover can
  carry a second segmented control for effort later; for G4, model text is enough.
- **Approval modal:** already well-aligned (scrim + card + tier chip + actions). Polish only:
  card to `--r-card`; relabel actions to Codex vocabulary — **Allow** / **Allow for session**
  / **Deny** / **Always deny** (current: "Always" / "Deny always"); tier chip stays a pill.

## 7. Spacing, radii, density — quick reference

| Element | Value |
|---|---|
| Sidebar width / inspector width | 260px / 340px |
| Top bar height | 44px, hairline bottom |
| Thread reading column (`--col`) | 720px, centered via `calc((100% - var(--col))/2)` (already used) |
| Card radius (composer, tool, modal, chips-as-cards) | 12px |
| Control radius (buttons, inputs, nav rows) | 8px |
| Pill radius (mode segments, model picker, chips) | full |
| Inter-turn gap | 28px |
| Borders | 1px hairline, `--border` / `--border-soft` — favor dividers over boxes |
| Shadows | almost none; only the composer + popovers get a soft shadow |

## 8. Wording / labels to align with Codex

- Nav: **Threads**, **Chats**, **Automations**, **Skills** (add Chats; today: Threads/Automations/Skills).
- **New thread** (keep), **Projects** / **Threads** section labels (uppercase).
- Composer modes: **Local · Worktree · Cloud**. Placeholder: `Describe a task…  /  for commands`.
- Empty state: **"What should we work on?"**
- Approvals: **Allow · Allow for session · Deny · Always deny**.
- Status row: keep `runtime v4` / `running`.

## 9. Implementation notes & guardrails

- **Pixels only.** Everything above is `styles.css` + `app.rs` `view!` markup. Do **not**
  add a `RuntimeCommand`/`RuntimeEvent`/method (gui-architecture §9). Mode selector and
  pop-out/terminal can be local UI state until backed by real protocol events.
- **Rebuild the bundle.** After editing, `cd crates/nerve-gui && trunk build` and commit the
  regenerated `dist/` (committed-artifact drift discipline — README "Drift discipline").
- **Assets.** Inline SVG icons or vendor an MIT/ISC set into the crate; no remote fetches
  (the served artifact must stay self-contained — gui-architecture §4). No OpenAI marks.
- **Accessibility:** keep the existing `:focus-visible` blue ring; ensure segmented control
  and icon-buttons are real `<button>`s with `title`/`aria-label`; honor the existing
  `prefers-reduced-motion` block (dot pulse, cursor).
- **Responsive:** extend the `max-width:760px` block to also drop the inspector
  (`#nerve-shell.with-inspector { grid-template-columns: minmax(0,1fr); }`).

## 10. Prioritized checklist

**P0 — biggest fidelity-per-edit (pure CSS + small markup):**
- [x] Swap in the §3 token block (brighter white surface, hairline borders, larger radii).
- [x] Composer: add the **Local/Worktree/Cloud** segmented bar + attach/mic row; card radius + soft shadow (§6.4).
- [x] Top bar: collapse provider/model inputs into a **model-picker pill** + add terminal/pop-out icon-buttons (§6.2).
- [x] Empty state: greeting **"What should we work on?"** + suggestion chips; drop the `N` splash (§6.3).
- [x] Section labels uppercase+tracked; empty-title 26px/600 (§4).

**P1 — structure & icons:**
- [x] Replace sidebar letter-glyphs with inline SVG icons; add **Chats**; nest threads under the active project (§6.1).
- [x] Tool cards: status **dot** instead of word badge; user bubble → borderless `--surface-2`; inter-turn gap 28px (§6.5).
- [x] Approval modal: relabel to **Allow / Allow for session / Deny / Always deny**; card radius (§6.7).

**P2 — the third pane & theming:**
- [x] Add the collapsible **inspector** (review/task) pane: ship shell + **Plan** tab; wire Sources/Artifacts/Changes as events land (§5).
- [x] Formalize the theme hook (`data-theme` + accent/bg/fg/font knobs) to mirror Codex Settings; keep monochrome accent default (§3).
- [x] Optional macOS vibrancy on the sidebar behind a capability flag (§6.1).
