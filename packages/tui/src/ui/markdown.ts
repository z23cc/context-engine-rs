// Minimal markdown -> ANSI renderer: inline spans (**bold**, *italic*, `code`),
// fenced code blocks, headings, bullet/numbered lists, and blockquotes. Inline
// spans are wrapped span-aware so styling survives line wrapping. Pure.

import { stringWidth, style, truncateToWidth } from "./ansi.ts";
import { highlight } from "./highlight.ts";

type Run = { text: string; render: (s: string) => string };

const IDENTITY = (s: string) => s;
const CODE = (s: string) => style.yellow(s);

/** Parse inline markdown into styled runs. Unmatched markers stay literal. */
export function parseInline(text: string): Run[] {
  const runs: Run[] = [];
  let plain = "";
  let i = 0;
  const flush = (): void => {
    if (plain) {
      runs.push({ text: plain, render: IDENTITY });
      plain = "";
    }
  };
  while (i < text.length) {
    if (text.startsWith("**", i)) {
      const end = text.indexOf("**", i + 2);
      if (end !== -1) {
        flush();
        runs.push({ text: text.slice(i + 2, end), render: style.bold });
        i = end + 2;
        continue;
      }
    }
    const ch = text[i];
    if (ch === "`") {
      const end = text.indexOf("`", i + 1);
      if (end !== -1) {
        flush();
        runs.push({ text: text.slice(i + 1, end), render: CODE });
        i = end + 1;
        continue;
      }
    }
    const next = text[i + 1];
    if ((ch === "*" || ch === "_") && next !== undefined && !/\s/.test(next)) {
      const end = text.indexOf(ch, i + 1);
      if (end > i + 1) {
        flush();
        runs.push({ text: text.slice(i + 1, end), render: style.italic });
        i = end + 1;
        continue;
      }
    }
    plain += ch;
    i += 1;
  }
  flush();
  return runs;
}

/** Wrap styled runs to `width`, returning ANSI lines (styling per token). */
export function wrapRuns(runs: Run[], width: number): string[] {
  if (width <= 0) return [runs.map((r) => r.render(r.text)).join("")];
  const lines: string[] = [];
  let line = "";
  let lineWidth = 0;
  const breakLine = (): void => {
    lines.push(line);
    line = "";
    lineWidth = 0;
  };
  for (const run of runs) {
    for (const token of run.text.split(/(\s+)/)) {
      if (token === "") continue;
      const tokenWidth = stringWidth(token);
      const isSpace = /^\s+$/.test(token);
      if (lineWidth === 0 && isSpace) continue;
      if (lineWidth + tokenWidth <= width) {
        line += run.render(token);
        lineWidth += tokenWidth;
        continue;
      }
      if (lineWidth > 0) {
        breakLine();
        if (isSpace) continue;
      }
      if (tokenWidth <= width) {
        line = run.render(token);
        lineWidth = tokenWidth;
      } else {
        for (const chr of token) {
          const cw = stringWidth(chr);
          if (lineWidth + cw > width) breakLine();
          line += run.render(chr);
          lineWidth += cw;
        }
      }
    }
  }
  if (line || lines.length === 0) lines.push(line);
  return lines;
}

function renderListItem(prefix: string, body: string, width: number): string[] {
  const wrapped = wrapRuns(parseInline(body), Math.max(1, width - prefix.length));
  const pad = " ".repeat(prefix.length);
  return wrapped.map((line, idx) => (idx === 0 ? style.cyan(prefix) + line : pad + line));
}

/** Render markdown text to colored, wrapped terminal lines. */
function emitCodeFence(lang: string | undefined, lines: string[], width: number): string[] {
  return highlight(lines.join("\n"), lang).map(
    (line) => `  ${truncateToWidth(line, Math.max(1, width - 2))}`,
  );
}

export function renderMarkdown(text: string, width: number): string[] {
  const out: string[] = [];
  let fence: { lang?: string; lines: string[] } | null = null;
  for (const raw of text.split("\n")) {
    const fenceMarker = raw.trimStart().match(/^```+\s*([\w+-]+)?/);
    if (fenceMarker) {
      if (fence === null) fence = { lang: fenceMarker[1], lines: [] };
      else {
        out.push(...emitCodeFence(fence.lang, fence.lines, width));
        fence = null;
      }
      continue;
    }
    if (fence) {
      fence.lines.push(raw);
      continue;
    }
    if (raw.trim() === "") {
      out.push("");
      continue;
    }
    const heading = raw.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      out.push(...wrapRuns(parseInline(heading[2] ?? ""), width).map((l) => style.bold(style.cyan(l))));
      continue;
    }
    const quote = raw.match(/^>\s?(.*)$/);
    if (quote) {
      out.push(
        ...wrapRuns(parseInline(quote[1] ?? ""), Math.max(1, width - 2)).map((l) => style.dim(`│ ${l}`)),
      );
      continue;
    }
    const bullet = raw.match(/^(\s*)[-*+]\s+(.*)$/);
    if (bullet) {
      out.push(...renderListItem(`${bullet[1]}• `, bullet[2] ?? "", width));
      continue;
    }
    const numbered = raw.match(/^(\s*)(\d+)\.\s+(.*)$/);
    if (numbered) {
      out.push(...renderListItem(`${numbered[1]}${numbered[2]}. `, numbered[3] ?? "", width));
      continue;
    }
    out.push(...wrapRuns(parseInline(raw), width));
  }
  if (fence) out.push(...emitCodeFence(fence.lang, fence.lines, width));
  return out;
}
