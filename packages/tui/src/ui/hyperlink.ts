// OSC 8 terminal hyperlinks. Technique ported from oh-my-pi's tui/hyperlink.ts
// (the pure escape-wrapping part): `ESC ] 8 ; ; URI ESC \ TEXT ESC ] 8 ; ; ESC \`.
// Emitted only on terminals known to support OSC 8; otherwise plain text (most
// other terminals ignore the sequence, but a few render artifacts).

import { pathToFileURL } from "node:url";

const OSC = "\x1b]";
const ST = "\x1b\\";

let cachedSupport: boolean | undefined;

export function supportsHyperlinks(): boolean {
  if (cachedSupport !== undefined) return cachedSupport;
  cachedSupport = detectHyperlinkSupport();
  return cachedSupport;
}

function detectHyperlinkSupport(): boolean {
  if (process.env.NERVE_TUI_NO_HYPERLINKS) return false;
  if (process.env.KITTY_WINDOW_ID || process.env.WEZTERM_PANE || process.env.GHOSTTY_RESOURCES_DIR) {
    return true;
  }
  const program = process.env.TERM_PROGRAM ?? "";
  if (/iTerm|WezTerm|vscode|ghostty|Hyper|rio|kitty/i.test(program)) return true;
  const term = process.env.TERM ?? "";
  return /kitty|wezterm/i.test(term);
}

/** Wrap `text` in an OSC 8 hyperlink to `uri`. Pure (no support gating). */
export function hyperlink(text: string, uri: string): string {
  return `${OSC}8;;${uri}${ST}${text}${OSC}8;;${ST}`;
}

/** `file://` URI for an absolute path; undefined for relative/empty paths. */
export function fileUri(path: string): string | undefined {
  if (!path.startsWith("/")) return undefined;
  try {
    return pathToFileURL(path).href;
  } catch {
    return undefined;
  }
}

/** Render a path as an OSC 8 link when absolute + supported, else plain text. */
export function linkPath(path: string): string {
  const uri = supportsHyperlinks() ? fileUri(path) : undefined;
  return uri ? hyperlink(path, uri) : path;
}
