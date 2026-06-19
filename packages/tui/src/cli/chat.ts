// Interactive terminal chat for the Nerve runtime daemon.
//
// A Protocol-v3 client that mirrors the daemon-served `gui.html` over the
// terminal: connect (spawn `nerve daemon --stdio`) -> start a session ->
// stream the agent loop (assistant text, reasoning, tool calls, approvals) ->
// type messages. Slash commands reconfigure the session in place: `/model` and
// `/provider` restart it via session.close + session.start{resume}, so history
// carries over with no protocol change; `/models` lists the provider's models.
// Ctrl-C interrupts the active turn (or exits when idle); `/quit` closes the
// session. Event rendering and command parsing are pure functions so they are
// unit-testable without a daemon.

import { createInterface } from "node:readline";
import { stdin, stdout } from "node:process";
import { NerveClient } from "../backend/nerveClient.ts";
import type { AgentEventKind, RuntimeCommand, RuntimeEvent } from "../backend/types.ts";

const ESC = "\x1b[";
const color = {
  dim: (s: string) => `${ESC}2m${s}${ESC}0m`,
  cyan: (s: string) => `${ESC}36m${s}${ESC}0m`,
  green: (s: string) => `${ESC}32m${s}${ESC}0m`,
  red: (s: string) => `${ESC}31m${s}${ESC}0m`,
  bold: (s: string) => `${ESC}1m${s}${ESC}0m`,
};

/** One-line, whitespace-collapsed preview of a tool's args/output. */
export function preview(value: unknown, max = 80): string {
  let text: string;
  try {
    text = typeof value === "string" ? value : JSON.stringify(value);
  } catch {
    text = String(value);
  }
  text = (text ?? "").replace(/\s+/g, " ").trim();
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

/** Render one agent-loop step as the text to write to the terminal. Pure. */
export function formatAgentEvent(event: AgentEventKind): string {
  switch (event.kind) {
    case "message":
      return event.text;
    case "reasoning":
      return color.dim(event.text);
    case "tool_started":
      return `\n  ${color.cyan(`→ ${event.tool}`)} ${color.dim(preview(event.arguments))}\n`;
    case "tool_finished":
      return event.ok
        ? `  ${color.green("✓")} ${color.dim(event.tool)}\n`
        : `  ${color.red(`✗ ${event.tool}`)} ${color.dim(preview(event.output))}\n`;
    case "interrupted":
      return `\n  ${color.red(`⊘ interrupted: ${event.reason}`)}\n`;
    case "turn_started":
    default:
      return "";
  }
}

interface ChatArgs {
  root: string;
  binary?: string;
  provider: string;
  model: string;
  agent?: string;
}

export function parseArgs(argv: string[]): ChatArgs {
  const out: Partial<ChatArgs> = {};
  for (let i = 0; i < argv.length; i += 1) {
    const value = argv[i];
    if (value === "--root") out.root = argv[++i];
    else if (value === "--binary") out.binary = argv[++i];
    else if (value === "--provider") out.provider = argv[++i];
    else if (value === "--model") out.model = argv[++i];
    else if (value === "--agent") out.agent = argv[++i];
    else if (value === "--help" || value === "-h") {
      stdout.write(
        "usage: nerve chat --provider P --model M [--root PATH] [--binary PATH] [--agent NAME]\n",
      );
      process.exit(0);
    }
  }
  if (!out.provider || !out.model) {
    stdout.write(
      color.red("error: --provider and --model are required") +
        " (e.g. --provider anthropic --model claude-sonnet-4)\n",
    );
    process.exit(2);
  }
  return {
    root: out.root ?? process.cwd(),
    binary: out.binary,
    provider: out.provider,
    model: out.model,
    agent: out.agent,
  };
}

/** A parsed `/command rest...` line. */
export interface SlashCommand {
  cmd: string;
  rest: string;
}

/** Parse a `/command args...` line; null for ordinary messages. Pure. */
export function parseCommand(line: string): SlashCommand | null {
  const trimmed = line.trim();
  if (!trimmed.startsWith("/")) return null;
  const space = trimmed.indexOf(" ");
  if (space === -1) return { cmd: trimmed.slice(1).toLowerCase(), rest: "" };
  return {
    cmd: trimmed.slice(1, space).toLowerCase(),
    rest: trimmed.slice(space + 1).trim(),
  };
}

/** The model-list tool for a provider, if one exists. Pure. */
export function providerModelsTool(provider: string): string | undefined {
  const name = provider.toLowerCase();
  if (name === "xai" || name === "grok") return "xai_models";
  if (name === "chatgpt" || name === "openai" || name === "openai_responses") return "openai_models";
  return undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function extractModelRows(result: unknown): string[] {
  // Tool results arrive as `{ content, structuredContent: { models: [...] } }`;
  // also tolerate a bare array or a top-level `{ models: [...] }`.
  const root =
    isRecord(result) && isRecord(result.structuredContent) ? result.structuredContent : result;
  const list = Array.isArray(root)
    ? root
    : isRecord(root) && Array.isArray(root.models)
      ? root.models
      : [];
  const rows: string[] = [];
  for (const item of list) {
    if (typeof item === "string") {
      rows.push(item);
      continue;
    }
    if (isRecord(item)) {
      const id = item.id ?? item.slug ?? item.name;
      if (typeof id === "string") {
        rows.push(`${id}${item.live === false ? " (curated)" : ""}`);
      }
    }
  }
  return rows;
}

/** Render a model-list tool result into a readable block. Tolerant of shape. Pure. */
export function formatModels(result: unknown): string {
  const rows = extractModelRows(result);
  if (rows.length === 0) return "  (no models returned)";
  return rows.map((row) => `  ${row}`).join("\n");
}

export const HELP_TEXT = [
  "commands:",
  "  /model <id>                switch model (keeps history)",
  "  /provider <name> [model]   switch provider (claude|chatgpt|xai)",
  "  /models                    list the current provider's models",
  "  /new                       start a fresh session (clears history)",
  "  /login [provider]          how to authenticate a provider",
  "  /help                      show this help",
  "  /quit                      close the session and exit",
  "Ctrl-C interrupts the active turn (or exits when idle).",
].join("\n");

async function runChat(args: ChatArgs): Promise<void> {
  const client = new NerveClient({ root: args.root, binary: args.binary });
  stdout.write(color.dim("connecting to the nerve daemon…\n"));
  await client.start();
  const info = await client.info();
  const tools = await client.listTools();
  const server = (info.serverInfo ?? {}) as { name?: string; version?: string };
  stdout.write(
    `${color.bold("Nerve Workstation")} ${color.dim(`· ${server.name ?? "daemon"} · ${tools.length} tools`)}\n`,
  );

  let provider = args.provider;
  let model = args.model;
  let sessionId: string | undefined;
  let turnActive = false;
  let closing = false;

  stdout.write(color.dim(`session: ${provider} / ${model}\n`));
  stdout.write(color.dim("type a message · /help for commands · Ctrl-C interrupts · /quit to exit\n\n"));

  const rl = createInterface({ input: stdin, output: stdout, prompt: color.cyan("› ") });
  const prompt = (): void => {
    if (!closing) rl.prompt();
  };

  const send = async (command: RuntimeCommand): Promise<void> => {
    try {
      await client.startJob(command);
    } catch (err) {
      stdout.write(color.red(`\n  ! ${(err as Error).message}\n`));
    }
  };

  const handleEvent = (event: RuntimeEvent): void => {
    switch (event.type) {
      case "session_started":
        if (!sessionId) {
          sessionId = event.session_id;
          prompt();
        }
        break;
      case "turn_started":
        turnActive = true;
        break;
      case "session_agent":
        stdout.write(formatAgentEvent(event.event));
        break;
      case "session_idle":
        if (turnActive) {
          turnActive = false;
          stdout.write("\n\n");
        }
        prompt();
        break;
      case "approval_requested":
        rl.question(
          `\n  ${color.red("⚠ allow")} ${color.bold(event.tool)} ${color.dim(preview(event.arguments))}${color.red(" ? [y/N] ")}`,
          (answer) => {
            const decision = /^y(es)?$/i.test(answer.trim()) ? "allow" : "deny";
            void send({
              kind: "session.respond",
              session_id: event.session_id,
              request_id: event.request_id,
              decision,
            });
          },
        );
        break;
      case "job_failed":
        stdout.write(color.red(`\n  ! ${event.error?.message ?? "job failed"}\n`));
        turnActive = false;
        prompt();
        break;
      default:
        break;
    }
  };

  const unsubscribe = client.onEvent(handleEvent);

  const shutdown = async (): Promise<void> => {
    if (closing) return;
    closing = true;
    if (sessionId) {
      try {
        await client.startJob({ kind: "session.close", session_id: sessionId });
      } catch {
        // best effort
      }
    }
    unsubscribe();
    await client.stop();
    rl.close();
    stdout.write(color.dim("\nbye.\n"));
    process.exit(0);
  };

  // Switch model/provider on the LIVE session in place (protocol-native
  // `session.set_model`): the daemon keeps history + checkpoint and applies the
  // new model from the next turn. No restart, no resume.
  const switchModel = async (nextProvider: string, nextModel: string): Promise<void> => {
    if (turnActive) {
      stdout.write(color.dim("  finish or interrupt (Ctrl-C) the current turn first\n"));
      prompt();
      return;
    }
    if (!sessionId) {
      stdout.write(color.dim("  (no active session yet)\n"));
      prompt();
      return;
    }
    provider = nextProvider;
    model = nextModel;
    stdout.write(color.dim(`switching to ${provider} / ${model}…\n`));
    await send({ kind: "session.set_model", session_id: sessionId, provider, model });
    prompt();
  };

  // Start a fresh session (clears history) with the current provider/model.
  const newSession = async (): Promise<void> => {
    if (turnActive) {
      stdout.write(color.dim("  finish or interrupt (Ctrl-C) the current turn first\n"));
      prompt();
      return;
    }
    const previous = sessionId;
    sessionId = undefined;
    if (previous) await send({ kind: "session.close", session_id: previous });
    stdout.write(color.dim(`new session: ${provider} / ${model}…\n`));
    await send({ kind: "session.start", provider, model, agent: args.agent ?? null });
  };

  const listModels = async (): Promise<void> => {
    const tool = providerModelsTool(provider);
    if (!tool) {
      stdout.write(color.dim(`  no model list available for ${provider}\n`));
      prompt();
      return;
    }
    stdout.write(color.dim(`  fetching models (${tool})…\n`));
    try {
      const result = await client.runJob({ kind: "tool.call", name: tool, arguments: {} });
      stdout.write(`${formatModels(result)}\n`);
    } catch (err) {
      stdout.write(color.red(`  ! ${(err as Error).message}\n`));
    }
    prompt();
  };

  const handleCommand = (command: SlashCommand): void => {
    switch (command.cmd) {
      case "quit":
      case "exit":
        void shutdown();
        return;
      case "help":
        stdout.write(`${HELP_TEXT}\n`);
        prompt();
        return;
      case "model":
        if (!command.rest) {
          stdout.write(color.dim(`  current: ${provider} / ${model}\n  usage: /model <id>\n`));
          prompt();
          return;
        }
        void switchModel(provider, command.rest);
        return;
      case "provider": {
        if (!command.rest) {
          stdout.write(
            color.dim(`  current: ${provider} / ${model}\n  usage: /provider <name> [model]\n`),
          );
          prompt();
          return;
        }
        const [name, ...rest] = command.rest.split(/\s+/);
        void switchModel(name, rest[0] ?? model);
        return;
      }
      case "models":
        void listModels();
        return;
      case "new":
      case "reset":
        void newSession();
        return;
      case "login":
        stdout.write(
          color.dim(
            `  authenticate with:  nerve agent login --provider ${command.rest || "claude|chatgpt|xai"}\n` +
              "  then /provider <name> to switch.\n",
          ),
        );
        prompt();
        return;
      default:
        stdout.write(color.dim(`  unknown command: /${command.cmd} — try /help\n`));
        prompt();
        return;
    }
  };

  rl.on("line", (line) => {
    const command = parseCommand(line);
    if (command) {
      handleCommand(command);
      return;
    }
    const text = line.trim();
    if (text === "") {
      prompt();
      return;
    }
    if (!sessionId) {
      stdout.write(color.dim("  (session not ready yet)\n"));
      prompt();
      return;
    }
    turnActive = true;
    stdout.write("\n");
    void send({ kind: "session.message", session_id: sessionId, text });
  });

  rl.on("SIGINT", () => {
    if (turnActive && sessionId) {
      stdout.write(color.dim("\n  interrupting…\n"));
      void send({ kind: "session.interrupt", session_id: sessionId });
    } else {
      void shutdown();
    }
  });

  await send({
    kind: "session.start",
    provider,
    model,
    agent: args.agent ?? null,
  });
}

if (import.meta.main) {
  runChat(parseArgs(process.argv.slice(2))).catch((err) => {
    stdout.write(color.red(`fatal: ${(err as Error).message}\n`));
    process.exit(1);
  });
}
