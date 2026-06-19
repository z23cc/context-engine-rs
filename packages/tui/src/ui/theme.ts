// Accent color themes for the UI chrome (header logo, status spinner, input
// prompt, palette). Cycled live with `/theme`. Transcript colors are fixed.

import { style } from "./ansi.ts";

export interface Theme {
  name: string;
  accent: (s: string) => string;
}

export const THEMES: Theme[] = [
  { name: "cyan", accent: style.cyan },
  { name: "green", accent: style.green },
  { name: "magenta", accent: style.magenta },
  { name: "amber", accent: style.yellow },
];

/** Index of a theme by name, defaulting to 0 (cyan). */
export function themeIndexByName(name: string | undefined): number {
  const idx = THEMES.findIndex((theme) => theme.name === name);
  return idx >= 0 ? idx : 0;
}
