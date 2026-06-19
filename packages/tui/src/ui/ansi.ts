// Pure ANSI / display-width / wrapping helpers + terminal control codes.
// No native dependencies — width and wrapping are computed in plain TS so the
// standalone `bun build --compile` binary stays self-contained.

const ESC = "\x1b[";
const ANSI_RE = /\x1b\[[0-9;]*m/g;
// OSC 8 hyperlink sequences (opener + closer), terminated by ST or BEL.
const OSC8_RE = /\x1b\]8;[^\x07\x1b]*(?:\x07|\x1b\\)/g;
const LINK_CLOSE = "\x1b]8;;\x1b\\";

/** Braille spinner frames, shared by the status bar and running tool cells. */
export const SPINNER = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/** SGR color/style helpers. Each wraps the input and resets. */
export const style = {
  reset: `${ESC}0m`,
  dim: (s: string) => `${ESC}2m${s}${ESC}22m`,
  bold: (s: string) => `${ESC}1m${s}${ESC}22m`,
  italic: (s: string) => `${ESC}3m${s}${ESC}23m`,
  cyan: (s: string) => `${ESC}36m${s}${ESC}39m`,
  green: (s: string) => `${ESC}32m${s}${ESC}39m`,
  red: (s: string) => `${ESC}31m${s}${ESC}39m`,
  yellow: (s: string) => `${ESC}33m${s}${ESC}39m`,
  blue: (s: string) => `${ESC}34m${s}${ESC}39m`,
  magenta: (s: string) => `${ESC}35m${s}${ESC}39m`,
  gray: (s: string) => `${ESC}90m${s}${ESC}39m`,
  invert: (s: string) => `${ESC}7m${s}${ESC}27m`,
};

/** Terminal control sequences (alt-screen, cursor, clearing, positioning). */
export const term = {
  enterAlt: `${ESC}?1049h`,
  leaveAlt: `${ESC}?1049l`,
  hideCursor: `${ESC}?25l`,
  showCursor: `${ESC}?25h`,
  clear: `${ESC}2J`,
  home: `${ESC}H`,
  clearLine: `${ESC}2K`,
  /** 1-based row/col cursor move. */
  moveTo: (row: number, col: number) => `${ESC}${row};${col}H`,
  // SGR mouse reporting (wheel events) + bracketed paste.
  mouseOn: `${ESC}?1000h${ESC}?1006h`,
  mouseOff: `${ESC}?1006l${ESC}?1000l`,
  pasteOn: `${ESC}?2004h`,
  pasteOff: `${ESC}?2004l`,
};

export function stripAnsi(text: string): string {
  return text.replace(ANSI_RE, "").replace(OSC8_RE, "");
}

/**
 * Strip raw control sequences from untrusted content (model/tool output) that
 * would corrupt the differential renderer: tabs -> spaces, drop CR and every
 * escape/control char except newline. (oh-my-pi guards renders the same way.)
 */
export function sanitize(text: string): string {
  return text
    .replace(/\t/g, "  ")
    .replace(/\r/g, "")
    // eslint-disable-next-line no-control-regex
    .replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, "")
    // eslint-disable-next-line no-control-regex
    .replace(/\x1b[@-Z\\-_]/g, "")
    // eslint-disable-next-line no-control-regex
    .replace(/[\x00-\x08\x0b\x0c\x0e-\x1f]/g, "");
}

function isZeroWidth(cp: number): boolean {
  return (
    cp === 0 ||
    (cp >= 0x300 && cp <= 0x36f) || // combining marks
    (cp >= 0x200b && cp <= 0x200f) || // zero-width spaces / marks
    cp === 0xfeff
  );
}

function isWide(cp: number): boolean {
  return (
    (cp >= 0x1100 && cp <= 0x115f) ||
    cp === 0x2329 ||
    cp === 0x232a ||
    (cp >= 0x2e80 && cp <= 0x303e) ||
    (cp >= 0x3041 && cp <= 0x33ff) ||
    (cp >= 0x3400 && cp <= 0x4dbf) ||
    (cp >= 0x4e00 && cp <= 0x9fff) ||
    (cp >= 0xa000 && cp <= 0xa4cf) ||
    (cp >= 0xac00 && cp <= 0xd7a3) ||
    (cp >= 0xf900 && cp <= 0xfaff) ||
    (cp >= 0xfe30 && cp <= 0xfe4f) ||
    (cp >= 0xff00 && cp <= 0xff60) ||
    (cp >= 0xffe0 && cp <= 0xffe6) ||
    (cp >= 0x1f300 && cp <= 0x1faff) ||
    (cp >= 0x20000 && cp <= 0x3fffd)
  );
}

/** Display width of a string, ignoring ANSI and counting wide chars as 2. */
export function stringWidth(text: string): number {
  let width = 0;
  for (const ch of stripAnsi(text)) {
    const cp = ch.codePointAt(0) ?? 0;
    if (isZeroWidth(cp)) continue;
    width += isWide(cp) ? 2 : 1;
  }
  return width;
}

/** Pad a string with spaces up to `width` display columns (no truncation). */
export function padTo(text: string, width: number): string {
  const pad = width - stringWidth(text);
  return pad > 0 ? text + " ".repeat(pad) : text;
}

/**
 * Truncate to `width` display columns, preserving ANSI escapes (they cost no
 * width) and appending an ellipsis when content is dropped. Always resets SGR.
 */
export function truncateToWidth(text: string, width: number): string {
  if (width <= 0) return "";
  if (stringWidth(text) <= width) return text;
  let out = "";
  let used = 0;
  let i = 0;
  let linked = false;
  while (i < text.length) {
    if (text[i] === "\x1b") {
      const sgr = text.slice(i).match(/^\x1b\[[0-9;]*m/);
      if (sgr) {
        out += sgr[0];
        i += sgr[0].length;
        continue;
      }
      const osc = text.slice(i).match(/^\x1b\]8;[^\x07\x1b]*(?:\x07|\x1b\\)/);
      if (osc) {
        out += osc[0];
        i += osc[0].length;
        linked = true;
        continue;
      }
    }
    const cp = text.codePointAt(i) ?? 0;
    const ch = String.fromCodePoint(cp);
    const w = isZeroWidth(cp) ? 0 : isWide(cp) ? 2 : 1;
    if (used + w > width - 1) break;
    out += ch;
    used += w;
    i += ch.length;
  }
  // Close any open hyperlink so it doesn't bleed past the truncation point.
  return `${out}…${linked ? LINK_CLOSE : ""}${style.reset}`;
}

/**
 * Word-wrap plain text to `width` columns, hard-breaking words longer than the
 * width. Honors existing newlines; drops the leading space on continued lines.
 */
export function wrapText(text: string, width: number): string[] {
  if (width <= 0) return [text];
  const out: string[] = [];
  for (const rawLine of text.split("\n")) {
    if (rawLine === "") {
      out.push("");
      continue;
    }
    let line = "";
    let lineWidth = 0;
    for (const word of rawLine.split(/(\s+)/)) {
      if (word === "") continue;
      const wordWidth = stringWidth(word);
      if (lineWidth === 0 && /^\s+$/.test(word)) continue; // no leading space
      if (lineWidth + wordWidth <= width) {
        line += word;
        lineWidth += wordWidth;
        continue;
      }
      if (lineWidth > 0) {
        out.push(line);
        line = "";
        lineWidth = 0;
        if (/^\s+$/.test(word)) continue;
      }
      if (wordWidth <= width) {
        line = word;
        lineWidth = wordWidth;
      } else {
        for (const ch of word) {
          const cw = stringWidth(ch);
          if (lineWidth + cw > width) {
            out.push(line);
            line = "";
            lineWidth = 0;
          }
          line += ch;
          lineWidth += cw;
        }
      }
    }
    out.push(line);
  }
  return out;
}
