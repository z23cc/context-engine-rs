// Pure CLI/command helpers shared by the entry point (chat.ts) and the
// full-screen app (ui/app.ts). No IO beyond parseArgs' usage/error output.

import { stdout } from "node:process";
import type { ApprovalMode } from "../backend/types.ts";

export interface ChatArgs {
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
      "error: --provider and --model are required (e.g. --provider anthropic --model claude-sonnet-4)\n",
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

/**
 * Parse a friendly approval-mode spelling into the protocol `ApprovalMode`.
 * Accepts "always-ask"/"always_ask"/"ask" → "always_ask", "write", "yolo".
 * Returns undefined for anything unrecognized. Pure.
 */
export function parseApprovalMode(value: string): ApprovalMode | undefined {
  switch (value.trim().toLowerCase().replace(/-/g, "_")) {
    case "always_ask":
    case "ask":
      return "always_ask";
    case "write":
      return "write";
    case "yolo":
      return "yolo";
    default:
      return undefined;
  }
}

/** Human-readable label for an approval mode (matches the friendly spelling). Pure. */
export function approvalModeLabel(mode: ApprovalMode): string {
  return mode === "always_ask" ? "always-ask" : mode;
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
  if (rows.length === 0) return "(no models returned)";
  return rows.map((row) => `  ${row}`).join("\n");
}

export interface CommandSpec {
  name: string;
  hint: string;
}

/** Slash commands offered by the autocomplete palette. */
export const COMMANDS: CommandSpec[] = [
  { name: "model", hint: "switch model (keeps history)" },
  { name: "provider", hint: "switch provider" },
  { name: "models", hint: "list provider models" },
  { name: "mode", hint: "approval mode: always-ask|write|yolo" },
  { name: "new", hint: "fresh session (clears history)" },
  { name: "login", hint: "how to authenticate" },
  { name: "theme", hint: "cycle accent color" },
  { name: "help", hint: "show commands" },
  { name: "quit", hint: "close and exit" },
];

/** Commands matching a bare `/word` prefix; empty once the line has a space. */
export function matchCommands(input: string): CommandSpec[] {
  const match = input.match(/^\/(\w*)$/);
  if (!match) return [];
  const query = (match[1] ?? "").toLowerCase();
  return COMMANDS.filter((command) => command.name.startsWith(query));
}

export const HELP_TEXT = [
  "commands:",
  "  /model <id>                switch model (keeps history)",
  "  /provider <name> [model]   switch provider (claude|chatgpt|xai)",
  "  /models                    list the current provider's models",
  "  /mode [always-ask|write|yolo]  set the approval mode (bare = show current)",
  "  /new                       start a fresh session (clears history)",
  "  /login [provider]          how to authenticate a provider",
  "  /theme                     cycle the accent color",
  "  /help                      show this help",
  "  /quit                      close the session and exit",
].join("\n");
