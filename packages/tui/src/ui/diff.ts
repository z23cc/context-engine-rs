// Unified-diff -> colored ANSI lines.
//
// Logic ported from oh-my-pi's modes/components/diff.ts (MIT): prefix coloring
// (added green / removed red / hunk cyan / context dim), intra-line inverse
// highlighting on single-line replacements, and dim indentation glyphs. Adapted
// to standard unified diff and pure TS — no `diff` package (a common
// prefix/suffix word diff stands in for diffWords).

import { sanitize, style } from "./ansi.ts";

const DIM = "\x1b[2m";
const DIM_OFF = "\x1b[22m";

/** Heuristic: does this text look like a unified diff? */
export function isDiff(text: string): boolean {
  if (/(^|\n)@@ /.test(text)) return true;
  return /(^|\n)--- /.test(text) && /(^|\n)\+\+\+ /.test(text);
}

/** Visualize leading whitespace: tabs -> dim →, spaces -> dim ·. */
function visualizeIndent(text: string): string {
  const indent = text.match(/^([ \t]+)/)?.[1];
  if (!indent) return text;
  let visible = "";
  for (const ch of indent) visible += ch === "\t" ? `${DIM}→ ${DIM_OFF}` : `${DIM}·${DIM_OFF}`;
  return visible + text.slice(indent.length);
}

/** Inverse-highlight the differing middle (shared prefix/suffix stays plain). */
function intraLine(oldContent: string, newContent: string): { removed: string; added: string } {
  const min = Math.min(oldContent.length, newContent.length);
  let p = 0;
  while (p < min && oldContent[p] === newContent[p]) p += 1;
  let s = 0;
  while (
    s < min - p &&
    oldContent[oldContent.length - 1 - s] === newContent[newContent.length - 1 - s]
  ) {
    s += 1;
  }
  const prefix = oldContent.slice(0, p);
  const suffix = oldContent.slice(oldContent.length - s);
  const oldMid = oldContent.slice(p, oldContent.length - s);
  const newMid = newContent.slice(p, newContent.length - s);
  return {
    removed: prefix + (oldMid ? style.invert(oldMid) : "") + suffix,
    added: prefix + (newMid ? style.invert(newMid) : "") + suffix,
  };
}

function isAdded(line: string): boolean {
  return line.startsWith("+") && !line.startsWith("+++");
}

function isRemoved(line: string): boolean {
  return line.startsWith("-") && !line.startsWith("---");
}

/** Render a unified-diff string to colored terminal lines. */
export function renderDiff(diffText: string): string[] {
  const lines = sanitize(diffText).split("\n");
  const out: string[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (line === undefined) break;
    if (line.startsWith("@@")) {
      out.push(style.cyan(line));
      i += 1;
    } else if (line.startsWith("+++") || line.startsWith("---")) {
      out.push(style.dim(line));
      i += 1;
    } else if (isRemoved(line)) {
      const removed: string[] = [];
      while (i < lines.length && isRemoved(lines[i]!)) removed.push(lines[i++]!.slice(1));
      const added: string[] = [];
      while (i < lines.length && isAdded(lines[i]!)) added.push(lines[i++]!.slice(1));
      if (removed.length === 1 && added.length === 1) {
        const pair = intraLine(removed[0]!, added[0]!);
        out.push(style.red(`-${visualizeIndent(pair.removed)}`));
        out.push(style.green(`+${visualizeIndent(pair.added)}`));
      } else {
        for (const rm of removed) out.push(style.red(`-${visualizeIndent(rm)}`));
        for (const ad of added) out.push(style.green(`+${visualizeIndent(ad)}`));
      }
    } else if (isAdded(line)) {
      out.push(style.green(`+${visualizeIndent(line.slice(1))}`));
      i += 1;
    } else {
      out.push(line.length === 0 ? "" : style.dim(line));
      i += 1;
    }
  }
  return out;
}
