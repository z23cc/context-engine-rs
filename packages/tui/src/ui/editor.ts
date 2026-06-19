// Multiline input layout: map an input value + cursor index to display rows and
// a cursor (row, col). One display row per logical line (newline-separated);
// horizontal/vertical windowing is the renderer's job. Pure + testable.

import { stringWidth } from "./ansi.ts";

export interface EditorLayout {
  rows: string[];
  cursorRow: number;
  cursorCol: number;
}

export function layout(value: string, cursor: number): EditorLayout {
  const rows = value.split("\n");
  const before = value.slice(0, Math.max(0, Math.min(cursor, value.length)));
  const cursorRow = (before.match(/\n/g) ?? []).length;
  const lineStart = before.lastIndexOf("\n") + 1;
  const cursorCol = stringWidth(before.slice(lineStart));
  return { rows, cursorRow, cursorCol };
}
