// Full-screen chat application: a pure renderFrame() layout plus an IO layer
// that wires the daemon (NerveClient + session.* protocol) to a differential
// renderer and raw-mode keyboard input. oh-my-pi-inspired, pure TS, no native
// deps — keeps the standalone `bun build --compile` binary self-contained.

import { stderr, stdin, stdout } from "node:process";
import { NerveClient } from "../backend/nerveClient.ts";
import type {
  AgentEventKind,
  ApprovalMode,
  RiskTier,
  RuntimeCommand,
  RuntimeEvent,
  SessionApprovalDecision,
} from "../backend/types.ts";
import { padTo, SPINNER, stringWidth, style, truncateToWidth, wrapText } from "./ansi.ts";
import { type Block, blocksToLines, formatDuration, previewLine } from "./transcript.ts";
import { decodeKeys, type Key } from "./keys.ts";
import { type CursorPos, Screen } from "./screen.ts";
import { layout } from "./editor.ts";
import { THEMES, themeIndexByName } from "./theme.ts";
import { modelInfo } from "./models.ts";
import {
  approvalModeLabel,
  type ChatArgs,
  type CommandSpec,
  formatModels,
  HELP_TEXT,
  matchCommands,
  parseApprovalMode,
  parseCommand,
  providerModelsTool,
} from "../cli/commands.ts";

const MAX_INPUT_ROWS = 6;

export interface State {
  name: string;
  tools: number;
  provider: string;
  model: string;
  blocks: Block[];
  scroll: number; // rows scrolled up from the bottom (0 = pinned to bottom)
  running: boolean;
  spinner: number;
  input: string;
  cursor: number; // index into `input`
  mode: "input" | "approval";
  approval?: {
    tool: string;
    args: string;
    requestId: string;
    sessionId: string;
    tier: RiskTier;
    preview: string;
  };
  approvalMode: ApprovalMode;
  hint: string;
  expandTools: boolean;
  paletteIndex: number;
  history: string[];
  historyIndex: number;
  themeIndex: number;
  turnStartedAt?: number;
  elapsedMs: number;
  tokensIn: number;
  tokensOut: number;
  lastContextTokens: number;
  costUsd: number;
}

export interface Frame {
  lines: string[];
  cursor?: CursorPos;
}

function headerLine(state: State, width: number): string {
  const mode = approvalModeLabel(state.approvalMode);
  const text = ` ⬡ Nerve  ${style.dim(`${state.provider}/${state.model}`)}  ${style.dim(`· ${state.tools} tools`)}  ${style.dim(`· mode: ${mode}`)}`;
  return style.invert(padTo(truncateToWidth(text, width), width));
}

function formatTokens(n: number): string {
  return n >= 1000 ? `${(n / 1000).toFixed(1)}k` : `${n}`;
}

function statusLine(state: State, width: number): string {
  const body = state.hint
    ? style.yellow(state.hint)
    : state.running
      ? `${SPINNER[state.spinner % SPINNER.length]} working… ${formatDuration(state.elapsedMs)}  ${style.dim("Ctrl-C interrupt")}`
      : `${style.green("●")} ready  ${style.dim("/help · ↑↓ history · ⌥↵ newline · Ctrl-C quit")}`;
  const left = ` ${body}`;
  const info = modelInfo(state.model);
  const ctx =
    info && state.lastContextTokens
      ? ` · ${Math.round((state.lastContextTokens / info.contextWindow) * 100)}%`
      : "";
  const cost =
    state.costUsd >= 0.0005 ? ` · $${state.costUsd.toFixed(state.costUsd < 1 ? 3 : 2)}` : "";
  const tokens =
    state.tokensIn || state.tokensOut
      ? style.dim(`↑${formatTokens(state.tokensIn)} ↓${formatTokens(state.tokensOut)}${ctx}${cost} `)
      : "";
  const leftWidth = stringWidth(left);
  const tokensWidth = stringWidth(tokens);
  const line =
    tokens && leftWidth + tokensWidth < width
      ? left + " ".repeat(width - leftWidth - tokensWidth) + tokens
      : left;
  return style.invert(padTo(truncateToWidth(line, width), width));
}

function transcriptViewport(state: State, width: number, rows: number): string[] {
  const all = blocksToLines(state.blocks, width, {
    expandTools: state.expandTools,
    spinner: state.spinner,
  });
  const maxScroll = Math.max(0, all.length - rows);
  const scroll = Math.min(Math.max(0, state.scroll), maxScroll);
  const end = all.length - scroll;
  const start = Math.max(0, end - rows);
  const view = all.slice(start, end);
  // Top-anchor: pad below the content so a short transcript fills from the top
  // instead of leaving a large blank band above it. Once the transcript is
  // taller than the viewport, the under-full branch no longer runs and normal
  // bottom-pinned scrolling takes over.
  while (view.length < rows) view.push("");
  return view;
}

/** Color + label a risk tier (exec=red, edit=yellow, read-only=dim). Pure. */
function tierBadge(tier: RiskTier): string {
  const label = tier === "read_only" ? "read-only" : tier;
  const color = tier === "exec" ? style.red : tier === "edit" ? style.yellow : style.dim;
  return color(`[${label}]`);
}

const APPROVAL_PREVIEW_ROWS = 8;

/**
 * Render the multi-option approval prompt as a left-bordered block:
 *   ⚠ allow  <tool>  [<tier>]
 *   | <preview line 1>
 *   | …
 *   [a]llow once · [A]lways · [d]eny · [D]eny always · Esc cancel
 * Falls back to a compact args view when the event carried no preview.
 */
export function approvalLines(
  approval: NonNullable<State["approval"]>,
  width: number,
): string[] {
  const header = `${style.yellow("⚠ allow")}  ${style.bold(approval.tool)}  ${tierBadge(approval.tier)}`;
  const inner = Math.max(1, width - 2);
  const bodySource = approval.preview.trim() || previewLine(approval.args);
  const allRows = wrapText(bodySource, inner);
  const wrapped = allRows.slice(0, APPROVAL_PREVIEW_ROWS);
  const hidden = allRows.length - wrapped.length;
  const body = wrapped.map((line) => style.dim("│ ") + style.dim(line));
  if (hidden > 0) body.push(style.dim(`│ … +${hidden} more line${hidden > 1 ? "s" : ""}`));
  const options =
    `${style.bold("[a]")}llow once · ${style.bold("[A]")}lways · ` +
    `${style.bold("[d]")}eny · ${style.bold("[D]")}eny always · ${style.dim("Esc cancel")}`;
  return [
    truncateToWidth(header, width),
    ...body.map((line) => truncateToWidth(line, width)),
    truncateToWidth(options, width),
  ];
}

function inputBlock(
  state: State,
  width: number,
): { lines: string[]; cursorRow: number; cursorCol: number } {
  if (state.mode === "approval" && state.approval) {
    return { lines: approvalLines(state.approval, width), cursorRow: -1, cursorCol: 0 };
  }
  const { rows, cursorRow, cursorCol } = layout(state.input, state.cursor);
  const avail = Math.max(1, width - 2);
  const visible = Math.min(rows.length, MAX_INPUT_ROWS);
  const top =
    rows.length > MAX_INPUT_ROWS
      ? Math.min(Math.max(0, cursorRow - (MAX_INPUT_ROWS - 1)), rows.length - MAX_INPUT_ROWS)
      : 0;
  const accent = THEMES[state.themeIndex % THEMES.length]!.accent;
  const lines: string[] = [];
  for (let i = 0; i < visible; i += 1) {
    const globalRow = top + i;
    let text = rows[globalRow] ?? "";
    while (stringWidth(text) > avail) text = text.slice(1);
    lines.push((globalRow === 0 ? accent("❯ ") : "  ") + text);
  }
  return { lines, cursorRow: cursorRow - top, cursorCol: Math.min(cursorCol, avail - 1) };
}

function paletteLines(specs: CommandSpec[], selected: number, width: number): string[] {
  return specs.map((spec, idx) => {
    const padded = padTo(truncateToWidth(` /${spec.name}  ${style.dim(spec.hint)}`, width), width);
    return idx === selected ? style.invert(padded) : padded;
  });
}

/** Compose a full frame (one string per row) plus the input cursor. Pure. */
export function renderFrame(state: State, width: number, height: number): Frame {
  const palette = state.mode === "input" ? matchCommands(state.input) : [];
  const paletteHeight = Math.min(palette.length, 6);
  const input = inputBlock(state, width);
  const inputHeight = input.lines.length;
  const rows = Math.max(1, height - 2 - paletteHeight - inputHeight);
  const lines = [headerLine(state, width)];
  lines.push(...transcriptViewport(state, width, rows));
  if (paletteHeight > 0) {
    const selected = Math.min(state.paletteIndex % palette.length, paletteHeight - 1);
    lines.push(...paletteLines(palette.slice(0, paletteHeight), selected, width));
  }
  lines.push(statusLine(state, width));
  lines.push(...input.lines);
  const frame = lines.slice(0, height);
  const cursor: CursorPos | undefined =
    input.cursorRow >= 0
      ? { row: height - inputHeight + input.cursorRow, col: 2 + input.cursorCol }
      : undefined;
  return { lines: frame, cursor };
}

export class App {
  #client: NerveClient;
  #screen: Screen;
  #args: ChatArgs;
  #state: State;
  #sessionId?: string;
  #assistant: number | null = null;
  #reasoning: number | null = null;
  #spinnerTimer?: ReturnType<typeof setInterval>;
  #unsub?: () => void;
  #closing = false;
  #crashed = false;

  constructor(args: ChatArgs) {
    this.#args = args;
    this.#client = new NerveClient({ root: args.root, binary: args.binary });
    this.#screen = new Screen();
    this.#state = {
      name: "nerve",
      tools: 0,
      provider: args.provider,
      model: args.model,
      blocks: [],
      scroll: 0,
      running: false,
      spinner: 0,
      input: "",
      cursor: 0,
      mode: "input",
      approvalMode: "yolo",
      hint: "",
      expandTools: false,
      paletteIndex: 0,
      history: [],
      historyIndex: -1,
      themeIndex: themeIndexByName(process.env.NERVE_TUI_THEME),
      elapsedMs: 0,
      tokensIn: 0,
      tokensOut: 0,
      lastContextTokens: 0,
      costUsd: 0,
    };
  }

  async run(): Promise<void> {
    await this.#client.start();
    const info = await this.#client.info();
    const tools = await this.#client.listTools();
    this.#state.tools = tools.length;
    this.#state.name = ((info.serverInfo ?? {}) as { name?: string }).name ?? "nerve";
    this.#note(`connected · ${this.#state.tools} tools · type a message · /help for commands`);
    this.#screen.start();
    this.#screen.onResize(() => this.#render());
    // Restore the terminal no matter how we exit. A throw inside the stdin/key
    // path (or any stray rejection) must never leave it in raw + alt-screen mode.
    process.on("exit", this.#onExit);
    process.on("uncaughtException", this.#onFatal);
    process.on("unhandledRejection", this.#onFatal);
    this.#unsub = this.#client.onEvent((event) => this.#onEvent(event));
    this.#setupStdin();
    this.#spinnerTimer = setInterval(() => {
      if (this.#state.running) {
        this.#state.spinner += 1;
        this.#state.elapsedMs = Date.now() - (this.#state.turnStartedAt ?? Date.now());
        this.#render();
      }
    }, 90);
    await this.#send({
      kind: "session.start",
      provider: this.#state.provider,
      model: this.#state.model,
      agent: this.#args.agent ?? null,
    });
    this.#render();
    await new Promise<void>(() => {}); // run until shutdown() exits the process
  }

  #setupStdin(): void {
    if (stdin.isTTY) stdin.setRawMode(true);
    stdin.resume();
    stdin.setEncoding("utf8");
    stdin.on("data", (chunk: string) => {
      try {
        for (const key of decodeKeys(chunk)) this.#onKey(key);
      } catch (error) {
        this.#crash(error);
      }
    });
  }

  #render(): void {
    const { width, height } = this.#screen.size();
    const frame = renderFrame(this.#state, width, height);
    this.#screen.render(frame.lines, frame.cursor);
  }

  #onKey(key: Key): void {
    if (this.#state.mode === "approval") return this.#onApprovalKey(key);
    if (key.type === "ctrl-o") {
      this.#state.expandTools = !this.#state.expandTools;
      this.#render();
      return;
    }
    const palette = matchCommands(this.#state.input);
    if (palette.length > 0 && this.#handlePaletteKey(key, palette)) {
      this.#render();
      return;
    }
    this.#clearHint();
    switch (key.type) {
      case "ctrl-c":
        if (this.#state.running && this.#sessionId) {
          void this.#send({ kind: "session.interrupt", session_id: this.#sessionId });
          this.#state.hint = "interrupting…";
        } else void this.#shutdown();
        break;
      case "enter":
        this.#onSubmit();
        break;
      case "backspace":
        if (this.#state.cursor > 0) {
          this.#state.input =
            this.#state.input.slice(0, this.#state.cursor - 1) + this.#state.input.slice(this.#state.cursor);
          this.#state.cursor -= 1;
        }
        this.#state.paletteIndex = 0;
        this.#state.historyIndex = -1;
        break;
      case "char":
        this.#insert(key.value);
        break;
      case "paste":
        this.#insert(key.value);
        break;
      case "alt-enter":
        this.#insert("\n");
        break;
      case "left":
        this.#state.cursor = Math.max(0, this.#state.cursor - 1);
        break;
      case "right":
        this.#state.cursor = Math.min(this.#state.input.length, this.#state.cursor + 1);
        break;
      case "home":
        this.#state.cursor = 0;
        break;
      case "end":
        this.#state.cursor = this.#state.input.length;
        break;
      case "ctrl-u":
        this.#state.input = "";
        this.#state.cursor = 0;
        this.#state.paletteIndex = 0;
        this.#state.historyIndex = -1;
        break;
      case "up":
        this.#historyPrev();
        break;
      case "down":
        this.#historyNext();
        break;
      case "wheel-up":
        this.#state.scroll += 3;
        break;
      case "wheel-down":
        this.#state.scroll = Math.max(0, this.#state.scroll - 3);
        break;
      case "pageup":
        this.#state.scroll += Math.max(1, this.#screen.size().height - 4);
        break;
      case "pagedown":
        this.#state.scroll = Math.max(0, this.#state.scroll - (this.#screen.size().height - 4));
        break;
      default:
        return;
    }
    this.#render();
  }

  #onApprovalKey(key: Key): void {
    const approval = this.#state.approval;
    if (!approval) return;
    const decision = approvalDecisionForKey(key);
    if (!decision) {
      // Ignore keys that don't map to a decision (e.g. arrows) — keep the prompt.
      return;
    }
    void this.#send({
      kind: "session.respond",
      session_id: approval.sessionId,
      request_id: approval.requestId,
      decision,
    });
    this.#state.mode = "input";
    this.#state.approval = undefined;
    this.#note(`${decisionVerb(decision)} ${approval.tool}`);
    this.#render();
  }

  /** Handle palette navigation/completion; returns true if the key was used. */
  #handlePaletteKey(key: Key, palette: CommandSpec[]): boolean {
    const len = palette.length;
    switch (key.type) {
      case "up":
        this.#state.paletteIndex = (this.#state.paletteIndex - 1 + len) % len;
        return true;
      case "down":
        this.#state.paletteIndex = (this.#state.paletteIndex + 1) % len;
        return true;
      case "tab":
        this.#completePalette(palette);
        return true;
      case "enter": {
        const sel = palette[this.#state.paletteIndex % len];
        if (!sel) return false;
        if (this.#state.input !== `/${sel.name}`) {
          this.#completePalette(palette);
          return true;
        }
        return false; // exact command — let submit handle it
      }
      default:
        return false;
    }
  }

  #completePalette(palette: CommandSpec[]): void {
    const sel = palette[this.#state.paletteIndex % palette.length];
    if (!sel) return;
    this.#state.input = `/${sel.name} `;
    this.#state.cursor = this.#state.input.length;
    this.#state.paletteIndex = 0;
  }

  #onSubmit(): void {
    const text = this.#state.input.trim();
    this.#state.input = "";
    this.#state.cursor = 0;
    if (text === "") return;
    this.#pushHistory(text);
    const command = parseCommand(text);
    if (command) {
      void this.#onCommand(command.cmd, command.rest);
      return;
    }
    if (!this.#sessionId) {
      this.#state.hint = "session not ready yet";
      return;
    }
    if (this.#state.running) {
      // A turn is already in flight; the backend would reject a second
      // session.message with "already running". Hint instead of sending.
      // (#onKey renders after this returns, like the sibling guards above.)
      this.#state.hint = "still working — Ctrl-C to interrupt";
      return;
    }
    this.#state.blocks.push({ kind: "user", text });
    this.#state.running = true;
    this.#state.scroll = 0;
    this.#resetStreaming();
    void this.#send({ kind: "session.message", session_id: this.#sessionId, text });
  }

  async #onCommand(cmd: string, rest: string): Promise<void> {
    switch (cmd) {
      case "quit":
      case "exit":
        await this.#shutdown();
        return;
      case "help":
        this.#state.blocks.push({ kind: "notice", text: HELP_TEXT });
        break;
      case "model":
        if (rest) this.#switchModel(this.#state.provider, rest);
        else this.#state.hint = `current: ${this.#state.provider}/${this.#state.model} — usage: /model <id>`;
        break;
      case "provider": {
        if (!rest) {
          this.#state.hint = "usage: /provider <name> [model]";
          break;
        }
        const [name = "", ...parts] = rest.split(/\s+/);
        this.#switchModel(name, parts[0] ?? this.#state.model);
        break;
      }
      case "models":
        await this.#listModels();
        break;
      case "mode":
        this.#onModeCommand(rest);
        break;
      case "yolo":
        this.#setMode("yolo");
        break;
      case "write":
        this.#setMode("write");
        break;
      case "ask":
        this.#setMode("always_ask");
        break;
      case "new":
      case "reset":
        await this.#newSession();
        break;
      case "login":
        this.#state.blocks.push({
          kind: "notice",
          text: `authenticate with:  nerve agent login --provider ${rest || "claude|chatgpt|xai"}`,
        });
        break;
      case "theme":
        this.#state.themeIndex = (this.#state.themeIndex + 1) % THEMES.length;
        this.#state.hint = `theme: ${THEMES[this.#state.themeIndex]!.name}`;
        break;
      default:
        this.#state.hint = `unknown command: /${cmd} — try /help`;
    }
    this.#render();
  }

  #switchModel(provider: string, model: string): void {
    if (!this.#sessionId) {
      this.#state.hint = "no active session yet";
      return;
    }
    this.#state.provider = provider;
    this.#state.model = model;
    void this.#send({ kind: "session.set_model", session_id: this.#sessionId, provider, model });
    this.#note(`switched to ${provider}/${model}`);
  }

  /** `/mode` — bare shows the current mode; an argument sets it. */
  #onModeCommand(rest: string): void {
    if (!rest) {
      this.#state.hint = `mode: ${approvalModeLabel(this.#state.approvalMode)} — usage: /mode always-ask|write|yolo`;
      return;
    }
    const mode = parseApprovalMode(rest);
    if (!mode) {
      this.#state.hint = `unknown mode: ${rest} — try always-ask|write|yolo`;
      return;
    }
    this.#setMode(mode);
  }

  /** Set the approval mode locally and push it to the session (mirrors set_model). */
  #setMode(mode: ApprovalMode): void {
    this.#state.approvalMode = mode;
    const label = approvalModeLabel(mode);
    if (this.#sessionId) {
      void this.#send({ kind: "session.set_mode", session_id: this.#sessionId, mode });
    }
    this.#state.hint = `approval mode: ${label}`;
  }

  async #newSession(): Promise<void> {
    const previous = this.#sessionId;
    this.#sessionId = undefined;
    this.#state.blocks = [];
    this.#state.tokensIn = 0;
    this.#state.tokensOut = 0;
    this.#state.lastContextTokens = 0;
    this.#state.costUsd = 0;
    this.#resetStreaming();
    if (previous) await this.#send({ kind: "session.close", session_id: previous });
    this.#note(`new session · ${this.#state.provider}/${this.#state.model}`);
    await this.#send({
      kind: "session.start",
      provider: this.#state.provider,
      model: this.#state.model,
      agent: this.#args.agent ?? null,
    });
  }

  async #listModels(): Promise<void> {
    const tool = providerModelsTool(this.#state.provider);
    if (!tool) {
      this.#state.hint = `no model list for ${this.#state.provider}`;
      return;
    }
    this.#note(`fetching models (${tool})…`);
    this.#render();
    try {
      const result = await this.#client.runJob({ kind: "tool.call", name: tool, arguments: {} });
      this.#state.blocks.push({ kind: "notice", tone: "info", text: `models:\n${formatModels(result)}` });
      this.#clearHint();
    } catch (err) {
      this.#state.blocks.push({ kind: "notice", tone: "error", text: (err as Error).message });
    }
  }

  #onEvent(event: RuntimeEvent): void {
    switch (event.type) {
      case "session_started":
        this.#sessionId = event.session_id;
        break;
      case "turn_started":
        this.#state.running = true;
        this.#state.turnStartedAt = Date.now();
        this.#state.elapsedMs = 0;
        break;
      case "session_agent":
        this.#onAgentEvent(event.event);
        break;
      case "session_idle":
        this.#state.running = false;
        this.#resetStreaming();
        break;
      case "approval_requested":
        this.#state.mode = "approval";
        this.#state.approval = {
          tool: event.tool,
          args: safeJson(event.arguments),
          requestId: event.request_id,
          sessionId: event.session_id,
          // Older emitters omit `tier`; the protocol defaults that to the most
          // restricted tier (exec). `preview` defaults to empty (compact args).
          tier: event.tier ?? "exec",
          preview: event.preview ?? "",
        };
        break;
      case "job_failed": {
        const message = event.error?.message ?? "job failed";
        if (/is already running/.test(message)) {
          // A second message raced an in-flight turn. The genuine turn is still
          // live, so don't clear `running` or dump a red transcript line — hint.
          this.#state.hint = "still working — Ctrl-C to interrupt";
          break;
        }
        this.#state.running = false;
        this.#state.blocks.push({ kind: "notice", tone: "error", text: message });
        break;
      }
      default:
        return;
    }
    this.#render();
  }

  #onAgentEvent(event: AgentEventKind): void {
    switch (event.kind) {
      case "message":
        // Skip empty deltas: providers often emit a trailing empty chunk, which
        // would otherwise push an empty block (a blank line / stray "·" gutter).
        if (event.text) this.#appendText("assistant", event.text);
        break;
      case "reasoning":
        if (event.text) this.#appendText("reasoning", event.text);
        break;
      case "tool_started":
        this.#resetStreaming();
        this.#state.blocks.push({
          kind: "tool",
          tool: event.tool,
          args: safeJson(event.arguments),
          status: "running",
          startedAt: Date.now(),
        });
        break;
      case "tool_finished":
        this.#finishTool(event.tool, event.ok, event.output);
        break;
      case "interrupted":
        this.#state.blocks.push({ kind: "notice", tone: "warn", text: `interrupted: ${event.reason}` });
        break;
      case "usage": {
        this.#state.tokensIn += event.input_tokens;
        this.#state.tokensOut += event.output_tokens;
        this.#state.lastContextTokens = event.input_tokens;
        const info = modelInfo(this.#state.model);
        if (info) {
          this.#state.costUsd +=
            (event.input_tokens / 1e6) * info.inputPerMTok +
            (event.output_tokens / 1e6) * info.outputPerMTok;
        }
        break;
      }
      default:
        break;
    }
  }

  #appendText(kind: "assistant" | "reasoning", text: string): void {
    const current = kind === "assistant" ? this.#assistant : this.#reasoning;
    const block = current === null ? undefined : this.#state.blocks[current];
    if (block && block.kind === kind) {
      block.text += text;
      return;
    }
    this.#state.blocks.push(kind === "assistant" ? { kind: "assistant", text } : { kind: "reasoning", text });
    if (kind === "assistant") {
      this.#assistant = this.#state.blocks.length - 1;
      this.#reasoning = null;
    } else {
      this.#reasoning = this.#state.blocks.length - 1;
      this.#assistant = null;
    }
  }

  #finishTool(tool: string, ok: boolean, output: string): void {
    for (let i = this.#state.blocks.length - 1; i >= 0; i -= 1) {
      const block = this.#state.blocks[i];
      if (!block || block.kind !== "tool") continue;
      if (block.status !== "running" || block.tool !== tool) continue;
      block.status = ok ? "ok" : "error";
      block.output = output;
      block.durationMs = Date.now() - (block.startedAt ?? Date.now());
      return;
    }
  }

  #resetStreaming(): void {
    this.#assistant = null;
    this.#reasoning = null;
  }

  #note(text: string): void {
    this.#state.blocks.push({ kind: "notice", text });
  }

  #clearHint(): void {
    this.#state.hint = "";
  }

  #insert(text: string): void {
    this.#state.input =
      this.#state.input.slice(0, this.#state.cursor) + text + this.#state.input.slice(this.#state.cursor);
    this.#state.cursor += text.length;
    this.#state.paletteIndex = 0;
    this.#state.historyIndex = -1;
  }

  #pushHistory(text: string): void {
    if (this.#state.history[this.#state.history.length - 1] !== text) this.#state.history.push(text);
    this.#state.historyIndex = -1;
  }

  #historyPrev(): void {
    const history = this.#state.history;
    if (history.length === 0) return;
    this.#state.historyIndex =
      this.#state.historyIndex === -1
        ? history.length - 1
        : Math.max(0, this.#state.historyIndex - 1);
    this.#state.input = history[this.#state.historyIndex] ?? "";
    this.#state.cursor = this.#state.input.length;
  }

  #historyNext(): void {
    if (this.#state.historyIndex === -1) return;
    this.#state.historyIndex += 1;
    if (this.#state.historyIndex >= this.#state.history.length) {
      this.#state.historyIndex = -1;
      this.#state.input = "";
    } else {
      this.#state.input = this.#state.history[this.#state.historyIndex] ?? "";
    }
    this.#state.cursor = this.#state.input.length;
  }

  async #send(command: RuntimeCommand): Promise<void> {
    try {
      await this.#client.startJob(command);
    } catch (err) {
      this.#state.blocks.push({ kind: "notice", tone: "error", text: (err as Error).message });
      this.#render();
    }
  }

  // Bound so process.off() can detach them in #shutdown().
  #onExit = (): void => {
    this.#screen.stop();
  };

  #onFatal = (error: unknown): void => {
    this.#crash(error);
  };

  /**
   * Synchronously restore the terminal, print the error after the restore, and
   * exit non-zero. Re-entrancy-safe so it can run from the uncaughtException /
   * unhandledRejection backstop and from a throw in the stdin/key path.
   */
  #crash(error: unknown): never {
    if (!this.#crashed) {
      this.#crashed = true;
      this.#restoreTerminal();
      const detail = error instanceof Error ? (error.stack ?? error.message) : String(error);
      stderr.write(`\nnerve chat: fatal error\n${detail}\n`);
    }
    process.exit(1);
  }

  /** Best-effort terminal restore: stop the spinner, leave raw mode, and leave
   *  the alt-screen + mouse + bracketed-paste modes with the cursor shown. */
  #restoreTerminal(): void {
    if (this.#spinnerTimer) clearInterval(this.#spinnerTimer);
    try {
      if (stdin.isTTY) stdin.setRawMode(false);
      stdin.pause();
    } catch {
      // Restoring the screen/cursor below matters more than stdin state.
    }
    this.#screen.stop();
  }

  async #shutdown(): Promise<void> {
    if (this.#closing) return;
    this.#closing = true;
    process.off("uncaughtException", this.#onFatal);
    process.off("unhandledRejection", this.#onFatal);
    process.off("exit", this.#onExit);
    if (this.#spinnerTimer) clearInterval(this.#spinnerTimer);
    if (this.#sessionId) {
      try {
        await this.#client.startJob({ kind: "session.close", session_id: this.#sessionId });
      } catch {
        // best effort
      }
    }
    this.#unsub?.();
    if (stdin.isTTY) stdin.setRawMode(false);
    stdin.pause();
    this.#screen.stop();
    await this.#client.stop();
    stdout.write("\n");
    process.exit(0);
  }
}

function safeJson(value: unknown): string {
  try {
    return typeof value === "string" ? value : JSON.stringify(value);
  } catch {
    return String(value);
  }
}

/**
 * Map an approval keypress to a `SessionApprovalDecision`, or undefined if the
 * key isn't a decision (so the prompt stays up). Case matters: lowercase = once,
 * uppercase = "always". `Esc` cancels (deny once). Pure.
 *   a/y → allow · A → allow_always · d/n → deny · D → deny_always · Esc → deny
 */
export function approvalDecisionForKey(key: Key): SessionApprovalDecision | undefined {
  if (key.type === "esc") return "deny";
  if (key.type !== "char") return undefined;
  switch (key.value) {
    case "a":
    case "y":
      return "allow";
    case "A":
      return "allow_always";
    case "d":
    case "n":
      return "deny";
    case "D":
      return "deny_always";
    default:
      return undefined;
  }
}

/** Past-tense verb for a decision notice ("allowed"/"denied"). Pure. */
function decisionVerb(decision: SessionApprovalDecision): string {
  switch (decision) {
    case "allow":
      return "allowed";
    case "allow_always":
      return "always-allowed";
    case "deny":
      return "denied";
    case "deny_always":
      return "always-denied";
  }
}

export async function runApp(args: ChatArgs): Promise<void> {
  await new App(args).run();
}
