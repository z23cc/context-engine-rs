// Differential full-screen renderer over the alternate screen buffer.
//
// The app produces a full frame (one string per terminal row, each already
// truncated to the terminal width). render() diffs it against the previous
// frame and rewrites only the rows that changed, batched into a single write
// to avoid flicker. No native deps.

import { term } from "./ansi.ts";

export interface Size {
  width: number;
  height: number;
}

/** 0-based cursor position to park the hardware cursor (for the input line). */
export interface CursorPos {
  row: number;
  col: number;
}

interface OutStream {
  columns?: number;
  rows?: number;
  write(data: string): unknown;
  on?(event: string, cb: () => void): unknown;
  off?(event: string, cb: () => void): unknown;
}

export class Screen {
  #out: OutStream;
  #prev: string[] = [];
  #started = false;
  #resizeHandler?: () => void;

  constructor(out: OutStream = process.stdout as unknown as OutStream) {
    this.#out = out;
  }

  size(): Size {
    return { width: this.#out.columns ?? 80, height: this.#out.rows ?? 24 };
  }

  start(): void {
    if (this.#started) return;
    this.#started = true;
    this.#out.write(
      term.enterAlt + term.hideCursor + term.clear + term.home + term.mouseOn + term.pasteOn,
    );
    this.#prev = [];
  }

  onResize(cb: () => void): void {
    this.#resizeHandler = cb;
    this.#out.on?.("resize", cb);
  }

  /**
   * Paint `lines` (one per row). Rows equal to the previous frame are skipped;
   * a row-count change (resize/first paint) forces a full repaint.
   */
  render(lines: string[], cursor?: CursorPos): void {
    if (!this.#started) return;
    const fullRepaint = this.#prev.length !== lines.length;
    let buf = fullRepaint ? term.clear : "";
    for (let row = 0; row < lines.length; row += 1) {
      if (!fullRepaint && this.#prev[row] === lines[row]) continue;
      buf += term.moveTo(row + 1, 1) + term.clearLine + lines[row];
    }
    this.#prev = lines.slice();
    buf += cursor
      ? term.moveTo(cursor.row + 1, cursor.col + 1) + term.showCursor
      : term.hideCursor;
    this.#out.write(buf);
  }

  stop(): void {
    if (!this.#started) return;
    this.#started = false;
    if (this.#resizeHandler) this.#out.off?.("resize", this.#resizeHandler);
    this.#out.write(term.pasteOff + term.mouseOff + term.showCursor + term.leaveAlt);
    this.#prev = [];
  }
}
