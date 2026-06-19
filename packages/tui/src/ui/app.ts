// Full-screen chat application: a pure renderFrame() layout plus an IO layer
// that wires the daemon (NerveClient + session.* protocol) to a differential
// renderer and raw-mode keyboard input. oh-my-pi-inspired, pure TS, no native
// deps — keeps the standalone `bun build --compile` binary self-contained.

import { stderr, stdin, stdout } from "node:process";
import { NerveClient } from "../backend/nerveClient.ts";
import type { AgentEventKind, RuntimeCommand, RuntimeEvent } from "../backend/types.ts";
import { padTo, SPINNER, stringWidth, style, truncateToWidth } from "./ansi.ts";
import { type Block, blocksToLines, formatDuration, previewLine } from "./transcript.ts";
import { decodeKeys, type Key } from "./keys.ts";
import { type CursorPos, Screen } from "./screen.ts";
import { layout } from "./editor.ts";
import { THEMES, themeIndexByName } from "./theme.ts";
import { modelInfo } from "./models.ts";
import {
  type ChatArgs,
  type CommandSpec,
  formatModels,
  HELP_TEXT,
  matchCommands,
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
  approval?: { tool: string; args: string; requestId: string; sessionId: string };
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
  const text = ` ⬡ Nerve  ${style.dim(`${state.provider}/${state.model}`)}  ${style.dim(`· ${state.tools} tools`)}`;
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
  while (view.length < rows) view.unshift(""); // pin content to the bottom
  return view;
}

function inputBlock(
  state: State,
  width: number,
): { lines: string[]; cursorRow: number; cursorCol: number } {
  if (state.mode === "approval" && state.approval) {
    const a = state.approval;
    const prompt = `${style.yellow("allow")} ${style.bold(a.tool)} ${style.dim(previewLine(a.args))}${style.yellow(" ? [y/N]")} `;
    return { lines: [truncateToWidth(prompt, width)], cursorRow: -1, cursorCol: 0 };
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
    const allow = key.type === "char" && /^y$/i.test(key.value);
    void this.#send({
      kind: "session.respond",
      session_id: approval.sessionId,
      request_id: approval.requestId,
      decision: allow ? "allow" : "deny",
    });
    this.#state.mode = "input";
    this.#state.approval = undefined;
    this.#note(allow ? `allowed ${approval.tool}` : `denied ${approval.tool}`);
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
        };
        break;
      case "job_failed":
        this.#state.running = false;
        this.#state.blocks.push({ kind: "notice", tone: "error", text: event.error?.message ?? "job failed" });
        break;
      default:
        return;
    }
    this.#render();
  }

  #onAgentEvent(event: AgentEventKind): void {
    switch (event.kind) {
      case "message":
        this.#appendText("assistant", event.text);
        break;
      case "reasoning":
        this.#appendText("reasoning", event.text);
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

export async function runApp(args: ChatArgs): Promise<void> {
  await new App(args).run();
}
