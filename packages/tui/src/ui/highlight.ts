// Lightweight pure-TS syntax highlighter for fenced code blocks.
//
// oh-my-pi highlights via a native (Rust) tokenizer, which can't be ported into
// TS — so this is a fresh regex/state-machine lexer covering the common
// languages: keywords, strings, numbers, and comments (incl. block comments
// across lines). Unknown languages are dimmed. Invariant: stripping the ANSI
// from the output yields the original source verbatim.

import { style } from "./ansi.ts";

interface LangSpec {
  keywords: Set<string>;
  line: string[]; // line-comment markers
  block: boolean; // supports /* */ block comments
}

const words = (s: string): Set<string> => new Set(s.split(/\s+/).filter(Boolean));

const TS = words(`const let var function return if else for while class import export from default
  async await new type interface extends implements public private protected readonly this null
  undefined true false switch case break continue throw try catch finally typeof instanceof void
  enum as of in do yield static get set namespace declare keyof infer satisfies`);
const RUST = words(`fn let mut const static struct enum impl trait pub use mod match if else for while
  loop return self Self super crate where async await move ref dyn as in break continue type unsafe
  extern true false Some None Ok Err Box Vec String`);
const PY = words(`def class return if elif else for while import from as with try except finally raise
  lambda yield async await pass break continue global nonlocal True False None and or not in is del`);
const BASH = words(`if then else elif fi for while until do done case esac function return local export
  readonly declare in select`);
const GO = words(`func var const type struct interface package import return if else for range switch
  case default go defer chan map select break continue fallthrough nil true false`);

const LANGS: Record<string, LangSpec> = {
  ts: { keywords: TS, line: ["//"], block: true },
  js: { keywords: TS, line: ["//"], block: true },
  rust: { keywords: RUST, line: ["//"], block: true },
  python: { keywords: PY, line: ["#"], block: false },
  bash: { keywords: BASH, line: ["#"], block: false },
  go: { keywords: GO, line: ["//"], block: true },
  json: { keywords: words("true false null"), line: [], block: false },
};

const ALIASES: Record<string, string> = {
  typescript: "ts",
  tsx: "ts",
  javascript: "js",
  jsx: "js",
  rs: "rust",
  py: "python",
  sh: "bash",
  shell: "bash",
  zsh: "bash",
  golang: "go",
};

function specFor(lang?: string): LangSpec | undefined {
  if (!lang) return undefined;
  const key = lang.toLowerCase();
  return LANGS[ALIASES[key] ?? key];
}

const paint = {
  keyword: style.magenta,
  string: style.green,
  number: style.yellow,
  comment: style.gray,
};

function findStringEnd(line: string, start: number, quote: string): number {
  let i = start + 1;
  while (i < line.length) {
    if (line[i] === "\\") {
      i += 2;
      continue;
    }
    if (line[i] === quote) return i + 1;
    i += 1;
  }
  return line.length;
}

function highlightLine(line: string, spec: LangSpec, inBlock: boolean): { text: string; inBlock: boolean } {
  let out = "";
  let i = 0;
  if (inBlock) {
    const end = line.indexOf("*/");
    if (end === -1) return { text: paint.comment(line), inBlock: true };
    out += paint.comment(line.slice(0, end + 2));
    i = end + 2;
    inBlock = false;
  }
  while (i < line.length) {
    const rest = line.slice(i);
    if (spec.block && rest.startsWith("/*")) {
      const end = rest.indexOf("*/", 2);
      if (end === -1) {
        out += paint.comment(rest);
        return { text: out, inBlock: true };
      }
      out += paint.comment(rest.slice(0, end + 2));
      i += end + 2;
      continue;
    }
    const lineComment = spec.line.find((marker) => rest.startsWith(marker));
    if (lineComment) {
      out += paint.comment(rest);
      break;
    }
    const ch = line[i];
    if (ch === undefined) break;
    if (ch === '"' || ch === "'" || ch === "`") {
      const end = findStringEnd(line, i, ch);
      out += paint.string(line.slice(i, end));
      i = end;
      continue;
    }
    if (/\d/.test(ch) && !/[A-Za-z_$]/.test(line[i - 1] ?? "")) {
      const match = rest.match(/^\d[\d_.eExXa-fA-F]*/);
      if (match) {
        out += paint.number(match[0]);
        i += match[0].length;
        continue;
      }
    }
    if (/[A-Za-z_$]/.test(ch)) {
      const match = rest.match(/^[A-Za-z_$][\w$]*/);
      if (match) {
        out += spec.keywords.has(match[0]) ? paint.keyword(match[0]) : match[0];
        i += match[0].length;
        continue;
      }
    }
    out += ch;
    i += 1;
  }
  return { text: out, inBlock };
}

/** Highlight `code` for `lang` into ANSI lines (dimmed when lang is unknown). */
export function highlight(code: string, lang?: string): string[] {
  const spec = specFor(lang);
  const lines = code.split("\n");
  if (!spec) return lines.map((line) => style.dim(line));
  const out: string[] = [];
  let inBlock = false;
  for (const line of lines) {
    const result = highlightLine(line, spec, inBlock);
    out.push(result.text);
    inBlock = result.inBlock;
  }
  return out;
}
