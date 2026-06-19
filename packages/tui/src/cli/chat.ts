// Interactive terminal chat for the Nerve runtime daemon.
//
// A Protocol-v3 client that mirrors the daemon-served `gui.html` over the
// terminal: connect (spawn `nerve daemon --stdio`) -> start a session ->
// stream the agent loop (assistant text, reasoning, tool calls, approvals) ->
// type messages. Ctrl-C interrupts the active turn (or exits when idle);
// `/quit` closes the session. The event -> text rendering is a pure function
// (`formatAgentEvent`) so it is unit-testable without a daemon.

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
  stdout.write(color.dim(`session: ${args.provider} / ${args.model}\n`));
  stdout.write(color.dim("type a message · Ctrl-C interrupts a turn · /quit to exit\n\n"));

  let sessionId: string | undefined;
  let turnActive = false;
  let closing = false;

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

  rl.on("line", (line) => {
    const text = line.trim();
    if (text === "/quit" || text === "/exit") {
      void shutdown();
      return;
    }
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
    provider: args.provider,
    model: args.model,
    agent: args.agent ?? null,
  });
}

if (import.meta.main) {
  runChat(parseArgs(process.argv.slice(2))).catch((err) => {
    stdout.write(color.red(`fatal: ${(err as Error).message}\n`));
    process.exit(1);
  });
}
