// Transcript model + rendering: turn conversation blocks into colored, wrapped
// terminal lines. Assistant text renders as markdown; tool calls render as a
// status-colored framed block (oh-my-pi output-block style) with duration and a
// per-tool spinner. Untrusted text is sanitized. Pure + testable (no IO).

import { padTo, sanitize, SPINNER, stringWidth, style, truncateToWidth, wrapText } from "./ansi.ts";
import { isDiff, renderDiff } from "./diff.ts";
import { linkPath } from "./hyperlink.ts";
import { renderMarkdown } from "./markdown.ts";

export type Block =
  | { kind: "user"; text: string }
  | { kind: "assistant"; text: string }
  | { kind: "reasoning"; text: string }
  | {
      kind: "tool";
      tool: string;
      args: string;
      status: "running" | "ok" | "error";
      output?: string;
      startedAt?: number;
      durationMs?: number;
    }
  | { kind: "notice"; tone?: "info" | "warn" | "error"; text: string };

export interface RenderOptions {
  /** Expand tool cells to show full (capped) output instead of a preview. */
  expandTools?: boolean;
  /** Spinner frame index for animating the running tool's marker. */
  spinner?: number;
}

const COLLAPSED_OUTPUT_LINES = 3;
const EXPANDED_OUTPUT_LINES = 40;
// Absolute path to an image file in a tool result (image tools save to disk).
const IMAGE_RE = /(\/[^\s"']+\.(?:png|jpe?g|gif|webp|bmp))/i;

/** Collapse whitespace to a single-line preview. */
export function previewLine(value: string): string {
  return sanitize(value).replace(/\s+/g, " ").trim();
}

/** Human-readable elapsed duration. */
export function formatDuration(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds.toFixed(1)}s`;
  return `${Math.floor(seconds / 60)}m ${Math.round(seconds % 60)}s`;
}

/** Wrap `header` + `body` in a status-colored rounded box exactly `width` wide. */
function frame(header: string, body: string[], width: number, color: (s: string) => string): string[] {
  if (width < 6) return [header, ...body];
  const head = truncateToWidth(header, Math.max(0, width - 6));
  const dashes = Math.max(0, width - 5 - stringWidth(head));
  const inner = Math.max(1, width - 4);
  const lines = [color("╭─ ") + head + color(` ${"─".repeat(dashes)}╮`)];
  for (const line of body) {
    lines.push(color("│ ") + padTo(truncateToWidth(line, inner), inner) + color(" │"));
  }
  lines.push(color(`╰${"─".repeat(width - 2)}╯`));
  return lines;
}

function renderUser(text: string, width: number): string[] {
  return wrapText(sanitize(text), Math.max(1, width - 2)).map((line, idx) =>
    idx === 0 ? style.cyan("❯ ") + style.bold(line) : style.cyan("  ") + style.bold(line),
  );
}

function renderReasoning(text: string, width: number): string[] {
  return wrapText(sanitize(text), Math.max(1, width - 2)).map((line) => style.dim(`· ${line}`));
}

function toolColor(status: "running" | "ok" | "error"): (s: string) => string {
  if (status === "running") return style.yellow;
  return status === "ok" ? style.green : style.red;
}

/** Prefer a clean (clickable, when absolute) path over raw JSON args. */
function toolArgsDisplay(args: string): string {
  try {
    const parsed = JSON.parse(args);
    if (isRecord(parsed) && typeof parsed.path === "string") return linkPath(parsed.path);
  } catch {
    // not JSON — fall back to the raw preview
  }
  return previewLine(args);
}

function toolHeader(block: Extract<Block, { kind: "tool" }>, spinner: number): string {
  const marker =
    block.status === "running"
      ? style.yellow(SPINNER[spinner % SPINNER.length] ?? "")
      : block.status === "ok"
        ? style.green("✓")
        : style.red("✗");
  const duration = block.durationMs != null ? style.dim(` · ${formatDuration(block.durationMs)}`) : "";
  // Duration before args so it survives header truncation on long argument lines.
  return `${marker} ${style.cyan(block.tool)}${duration} ${style.dim(toolArgsDisplay(block.args))}`;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

/**
 * Pull the human-readable payload out of a tool result. nerve tools return JSON
 * envelopes (e.g. `{diff}` for edit, `{content}` for read), so surface that
 * text instead of raw JSON, and flag diffs for colored rendering.
 */
export function extractToolText(output: string): { text: string; diff: boolean } {
  let value: unknown;
  try {
    value = JSON.parse(output);
  } catch {
    return { text: output, diff: isDiff(output) };
  }
  if (typeof value === "string") return { text: value, diff: isDiff(value) };
  if (isRecord(value)) {
    if (typeof value.diff === "string") return { text: value.diff, diff: true };
    if (typeof value.content === "string") {
      return { text: value.content, diff: isDiff(value.content) };
    }
    if (Array.isArray(value.content)) {
      const joined = value.content
        .map((part) => (isRecord(part) && typeof part.text === "string" ? part.text : ""))
        .filter(Boolean)
        .join("\n");
      if (joined) return { text: joined, diff: isDiff(joined) };
    }
    for (const key of ["view", "output", "message", "stdout", "text", "result"]) {
      const field = value[key];
      if (typeof field === "string") return { text: field, diff: isDiff(field) };
    }
  }
  return { text: output, diff: false };
}

function renderTool(
  block: Extract<Block, { kind: "tool" }>,
  width: number,
  opts: RenderOptions,
): string[] {
  const header = toolHeader(block, opts.spinner ?? 0);
  if (!block.output || block.status === "running") return [truncateToWidth(header, width)];
  // Image tools save a file; surface it as a clickable thumbnail link rather than
  // dumping JSON. (Inline pixel rendering — kitty/sixel — is a separate item.)
  const image = block.output.match(IMAGE_RE);
  if (image) {
    return frame(header, [style.dim(`\u{1f5bc}  ${linkPath(image[1] ?? "")}`)], width, toolColor(block.status));
  }
  const { text, diff } = extractToolText(block.output);
  if (!text.trim()) return [truncateToWidth(header, width)];
  const inner = Math.max(1, width - 4);
  const outColor = block.status === "error" ? style.red : style.dim;
  const allLines = diff
    ? renderDiff(text)
    : sanitize(text)
        .split("\n")
        .flatMap((line) => wrapText(line, inner))
        .map((line) => outColor(line));
  const limit = opts.expandTools ? EXPANDED_OUTPUT_LINES : COLLAPSED_OUTPUT_LINES;
  const body = allLines.slice(0, limit);
  const hidden = allLines.length - body.length;
  if (hidden > 0) body.push(style.dim(`… +${hidden} more line${hidden > 1 ? "s" : ""} (Ctrl-O)`));
  return frame(header, body, width, toolColor(block.status));
}

function renderNotice(block: Extract<Block, { kind: "notice" }>, width: number): string[] {
  const color =
    block.tone === "error" ? style.red : block.tone === "warn" ? style.yellow : style.dim;
  return wrapText(sanitize(block.text), width).map((line) => color(line));
}

/** Render a single block to colored, wrapped lines. */
export function renderBlock(block: Block, width: number, opts: RenderOptions = {}): string[] {
  switch (block.kind) {
    case "user":
      return renderUser(block.text, width);
    case "assistant":
      return renderMarkdown(sanitize(block.text), width);
    case "reasoning":
      return renderReasoning(block.text, width);
    case "tool":
      return renderTool(block, width, opts);
    case "notice":
      return renderNotice(block, width);
  }
}

/** Flatten all blocks into transcript lines, one blank line between blocks. */
export function blocksToLines(blocks: Block[], width: number, opts: RenderOptions = {}): string[] {
  const lines: string[] = [];
  blocks.forEach((block, idx) => {
    if (idx > 0) lines.push("");
    lines.push(...renderBlock(block, width, opts));
  });
  return lines;
}
