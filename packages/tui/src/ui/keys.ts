// Decode a raw-mode stdin chunk into discrete key events. Pure + testable.

export type Key =
  | { type: "char"; value: string }
  | { type: "enter" }
  | { type: "backspace" }
  | { type: "tab" }
  | { type: "up" }
  | { type: "down" }
  | { type: "left" }
  | { type: "right" }
  | { type: "home" }
  | { type: "end" }
  | { type: "pageup" }
  | { type: "pagedown" }
  | { type: "ctrl-c" }
  | { type: "ctrl-d" }
  | { type: "ctrl-u" }
  | { type: "ctrl-l" }
  | { type: "ctrl-o" }
  | { type: "ctrl-w" }
  | { type: "alt-enter" }
  | { type: "wheel-up" }
  | { type: "wheel-down" }
  | { type: "paste"; value: string }
  | { type: "esc" }
  | { type: "unknown"; raw: string };

const ESCAPES: Record<string, Key["type"]> = {
  "\x1b[A": "up",
  "\x1b[B": "down",
  "\x1b[C": "right",
  "\x1b[D": "left",
  "\x1b[H": "home",
  "\x1b[F": "end",
  "\x1b[1~": "home",
  "\x1b[4~": "end",
  "\x1b[5~": "pageup",
  "\x1b[6~": "pagedown",
  "\x1bOA": "up",
  "\x1bOB": "down",
  "\x1bOC": "right",
  "\x1bOD": "left",
};

const CONTROL: Record<number, Key["type"]> = {
  3: "ctrl-c",
  4: "ctrl-d",
  15: "ctrl-o",
  21: "ctrl-u",
  12: "ctrl-l",
  23: "ctrl-w",
};

export function decodeKeys(data: string): Key[] {
  const keys: Key[] = [];
  let i = 0;
  while (i < data.length) {
    const ch = data[i];
    if (ch === undefined) break;
    const code = data.charCodeAt(i);
    if (ch === "\x1b") {
      const consumed = decodeEscape(data.slice(i), keys);
      i += consumed;
      continue;
    }
    if (code === 13 || code === 10) {
      keys.push({ type: "enter" });
      i += 1;
    } else if (code === 127 || code === 8) {
      keys.push({ type: "backspace" });
      i += 1;
    } else if (code === 9) {
      keys.push({ type: "tab" });
      i += 1;
    } else if (CONTROL[code]) {
      keys.push({ type: CONTROL[code] } as Key);
      i += 1;
    } else if (code < 32) {
      keys.push({ type: "unknown", raw: ch });
      i += 1;
    } else {
      const cp = data.codePointAt(i) ?? code;
      const value = String.fromCodePoint(cp);
      keys.push({ type: "char", value });
      i += value.length;
    }
  }
  return keys;
}

/** Decode one escape sequence at the start of `rest`; returns bytes consumed. */
function decodeEscape(rest: string, keys: Key[]): number {
  // Alt+Enter -> insert a newline (multiline input).
  if (rest.startsWith("\x1b\r") || rest.startsWith("\x1b\n")) {
    keys.push({ type: "alt-enter" });
    return 2;
  }
  // Bracketed paste: ESC[200~ <content> ESC[201~ -> literal insert (no submit).
  if (rest.startsWith("\x1b[200~")) {
    const end = rest.indexOf("\x1b[201~", 6);
    if (end !== -1) {
      keys.push({ type: "paste", value: rest.slice(6, end) });
      return end + 6;
    }
    keys.push({ type: "paste", value: rest.slice(6) });
    return rest.length;
  }
  // SGR mouse (wheel): ESC[<button;col;row(M|m). 64 = up, 65 = down.
  const mouse = rest.match(/^\x1b\[<(\d+);\d+;\d+[Mm]/);
  if (mouse) {
    const button = Number(mouse[1]);
    if (button === 64) keys.push({ type: "wheel-up" });
    else if (button === 65) keys.push({ type: "wheel-down" });
    else keys.push({ type: "unknown", raw: mouse[0] });
    return mouse[0].length;
  }
  for (const [seq, type] of Object.entries(ESCAPES)) {
    if (rest.startsWith(seq)) {
      keys.push({ type } as Key);
      return seq.length;
    }
  }
  // Unknown CSI/SS3 sequence — consume up to the final byte so it isn't typed.
  const csi = rest.match(/^\x1b[[O][0-9;]*[A-Za-z~]/);
  if (csi) {
    keys.push({ type: "unknown", raw: csi[0] });
    return csi[0].length;
  }
  // Lone ESC.
  keys.push({ type: "esc" });
  return 1;
}
