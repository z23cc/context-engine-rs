import { test } from "bun:test";
import assert from "node:assert/strict";
import { padTo, stringWidth, stripAnsi, truncateToWidth, wrapText } from "../src/ui/ansi.ts";
import { decodeKeys } from "../src/ui/keys.ts";
import { Screen } from "../src/ui/screen.ts";
import { renderFrame, type State } from "../src/ui/app.ts";
import { parseInline, renderMarkdown } from "../src/ui/markdown.ts";
import { matchCommands } from "../src/cli/commands.ts";
import { layout } from "../src/ui/editor.ts";
import {
  blocksToLines,
  extractToolText,
  formatDuration,
  previewLine,
  renderBlock,
} from "../src/ui/transcript.ts";
import { isDiff, renderDiff } from "../src/ui/diff.ts";
import { highlight } from "../src/ui/highlight.ts";
import { fileUri, hyperlink } from "../src/ui/hyperlink.ts";
import { modelInfo } from "../src/ui/models.ts";

const strip = (s: string): string => s.replace(/\x1b\[[0-9;]*m/g, "");

test("hyperlink wraps text in OSC 8; width/strip ignore it", () => {
  const link = hyperlink("Cargo.toml", "file:///x/Cargo.toml");
  assert.ok(link.includes("\x1b]8;;file:///x/Cargo.toml"));
  assert.equal(stringWidth(link), "Cargo.toml".length);
  assert.equal(stripAnsi(link), "Cargo.toml");
});

test("fileUri only links absolute paths", () => {
  assert.equal(fileUri("Cargo.toml"), undefined);
  assert.match(fileUri("/x/y.rs") ?? "", /^file:\/\/\/x\/y\.rs$/);
});

test("truncateToWidth preserves OSC 8 links and stays within width", () => {
  const link = hyperlink("abcdefghij", "file:///x");
  const truncated = truncateToWidth(link, 5);
  assert.ok(stringWidth(truncated) <= 5);
  assert.ok(truncated.includes("\x1b]8;;"));
});

test("stringWidth ignores ANSI and counts wide chars as 2", () => {
  assert.equal(stringWidth("hello"), 5);
  assert.equal(stringWidth("\x1b[31mred\x1b[39m"), 3);
  assert.equal(stringWidth("你好"), 4); // two wide chars
  assert.equal(stringWidth("a你b"), 4);
});

test("wrapText hard-wraps and honors newlines", () => {
  assert.deepEqual(wrapText("one two three", 7), ["one two", "three"]);
  assert.deepEqual(wrapText("a\nb", 10), ["a", "b"]);
  // long word hard-breaks
  const w = wrapText("abcdefghij", 4);
  assert.ok(w.every((line) => stringWidth(line) <= 4));
  assert.equal(w.join(""), "abcdefghij");
});

test("truncateToWidth adds an ellipsis and stays within width", () => {
  const out = truncateToWidth("abcdefghij", 5);
  assert.ok(stringWidth(out) <= 5);
  assert.match(strip(out), /…$/);
  assert.equal(truncateToWidth("abc", 10), "abc");
});

test("padTo fills to the display width", () => {
  assert.equal(stripAnsi(padTo("ab", 5)), "ab   ");
  assert.equal(padTo("abcde", 3), "abcde"); // never truncates
});

test("decodeKeys handles printable, control, and escape sequences", () => {
  assert.deepEqual(decodeKeys("hi"), [
    { type: "char", value: "h" },
    { type: "char", value: "i" },
  ]);
  assert.deepEqual(decodeKeys("\r"), [{ type: "enter" }]);
  assert.deepEqual(decodeKeys("\x7f"), [{ type: "backspace" }]);
  assert.deepEqual(decodeKeys("\x03"), [{ type: "ctrl-c" }]);
  assert.deepEqual(decodeKeys("\x1b[A"), [{ type: "up" }]);
  assert.deepEqual(decodeKeys("\x1b[6~"), [{ type: "pagedown" }]);
  // multi-byte code point stays intact
  assert.deepEqual(decodeKeys("你"), [{ type: "char", value: "你" }]);
});

test("renderBlock styles each kind and wraps to width", () => {
  assert.match(strip(renderBlock({ kind: "user", text: "hello there" }, 40)[0]), /❯ hello there/);
  const tool = renderBlock(
    { kind: "tool", tool: "read_file", args: '{"path":"a.rs"}', status: "ok" },
    40,
  );
  assert.match(strip(tool[0]), /✓ read_file/);
  const failed = renderBlock(
    { kind: "tool", tool: "edit", args: "", status: "error", output: "boom" },
    40,
  );
  assert.match(strip(failed[0]), /✗ edit/);
  assert.ok(failed.some((line) => /boom/.test(strip(line))));
});

test("blocksToLines separates blocks with a blank line", () => {
  const lines = blocksToLines(
    [
      { kind: "user", text: "hi" },
      { kind: "assistant", text: "yo" },
    ],
    40,
  );
  assert.ok(lines.includes(""));
  assert.ok(lines.some((l) => /hi/.test(strip(l))));
  assert.ok(lines.some((l) => /yo/.test(strip(l))));
});

test("previewLine collapses whitespace", () => {
  assert.equal(previewLine("a\n  b\tc"), "a b c");
});

function sampleState(over: Partial<State> = {}): State {
  return {
    name: "nerve",
    tools: 32,
    provider: "xai",
    model: "grok-4-fast",
    blocks: [
      { kind: "user", text: "hello" },
      { kind: "assistant", text: "hi there" },
    ],
    scroll: 0,
    running: false,
    spinner: 0,
    input: "type here",
    cursor: 9,
    mode: "input",
    hint: "",
    expandTools: false,
    paletteIndex: 0,
    history: [],
    historyIndex: -1,
    themeIndex: 0,
    elapsedMs: 0,
    tokensIn: 0,
    tokensOut: 0,
    lastContextTokens: 0,
    costUsd: 0,
    ...over,
  };
}

test("modelInfo returns context window + pricing for known families", () => {
  assert.equal(modelInfo("claude-sonnet-4")?.contextWindow, 200_000);
  assert.equal(modelInfo("grok-4-fast")?.contextWindow, 256_000);
  assert.ok((modelInfo("gpt-5.5")?.contextWindow ?? 0) >= 200_000);
  assert.equal(modelInfo("totally-unknown-model"), undefined);
});

test("statusLine shows context % and running cost when the model is known", () => {
  const { lines } = renderFrame(
    sampleState({
      model: "grok-4-fast",
      tokensIn: 128_000,
      tokensOut: 500,
      lastContextTokens: 128_000,
      costUsd: 0.05,
    }),
    100,
    12,
  );
  const status = strip(lines[10]);
  assert.match(status, /50%/);
  assert.match(status, /\$0\.05/);
});

test("image tool result renders a thumbnail link", () => {
  const lines = renderBlock(
    {
      kind: "tool",
      tool: "openai_image_generate",
      args: "{}",
      status: "ok",
      output: '{"path":"/tmp/out.png"}',
    },
    50,
  );
  const joined = strip(lines.join("\n"));
  assert.match(joined, /\u{1f5bc}/u);
  assert.match(joined, /out\.png/);
});

test("statusLine shows token usage flush-right", () => {
  const { lines } = renderFrame(sampleState({ tokensIn: 12345, tokensOut: 678 }), 100, 12);
  const status = strip(lines[10]);
  assert.match(status, /↑12\.3k/);
  assert.match(status, /↓678/);
});

test("highlight preserves source text and colors known languages", () => {
  const code = 'const x = "hi"; // note';
  const [line] = highlight(code, "ts");
  assert.equal(strip(line), code);
  assert.notEqual(line, code);
});

test("highlight spans block comments and dims unknown languages", () => {
  const lines = highlight("/* a\n b */ code", "rust");
  assert.equal(lines.length, 2);
  assert.equal(strip(lines.join("\n")), "/* a\n b */ code");
  assert.equal(strip(highlight("whatever", "klingon")[0]), "whatever");
});

test("renderMarkdown highlights fenced code by language", () => {
  const lines = renderMarkdown("```ts\nconst x = 1\n```", 40);
  assert.ok(lines.some((l) => strip(l).includes("const x = 1") && l !== strip(l)));
});

test("isDiff detects unified diffs", () => {
  assert.ok(isDiff("@@ -1 +1 @@\n-a\n+b"));
  assert.ok(isDiff("--- a/x\n+++ b/x\n@@ -1 +1 @@"));
  assert.equal(isDiff("hello world"), false);
});

test("renderDiff colors added/removed/hunk/context lines", () => {
  const lines = renderDiff("--- a/x\n+++ b/x\n@@ -1,1 +1,1 @@\n-old line\n+new line\n unchanged");
  assert.ok(lines.some((l) => /@@ -1,1 \+1,1 @@/.test(strip(l))));
  assert.ok(lines.some((l) => /-old line/.test(strip(l))));
  assert.ok(lines.some((l) => /\+new line/.test(strip(l))));
  assert.ok(lines.some((l) => / unchanged/.test(strip(l))));
});

test("extractToolText surfaces diff/content from JSON envelopes", () => {
  assert.deepEqual(extractToolText('{"diff":"@@ -1 +1 @@\\n-a\\n+b"}'), {
    text: "@@ -1 +1 @@\n-a\n+b",
    diff: true,
  });
  assert.deepEqual(extractToolText('{"content":"hello"}'), { text: "hello", diff: false });
  assert.deepEqual(extractToolText("plain text"), { text: "plain text", diff: false });
});

test("tool cell renders a colored diff for edit output", () => {
  const lines = renderBlock(
    {
      kind: "tool",
      tool: "edit",
      args: '{"path":"a.rs"}',
      status: "ok",
      output: JSON.stringify({ diff: "@@ -1 +1 @@\n-foo\n+bar" }),
    },
    50,
  );
  const joined = strip(lines.join("\n"));
  assert.match(joined, /╭─ ✓ edit/);
  assert.match(joined, /-foo/);
  assert.match(joined, /\+bar/);
});

test("formatDuration is human-readable", () => {
  assert.equal(formatDuration(320), "320ms");
  assert.equal(formatDuration(1500), "1.5s");
  assert.equal(formatDuration(65000), "1m 5s");
});

test("tool cell renders a framed block with duration", () => {
  const lines = renderBlock(
    { kind: "tool", tool: "read_file", args: "{}", status: "ok", output: "a\nb\nc\nd", durationMs: 320 },
    40,
  );
  const joined = strip(lines.join("\n"));
  assert.match(joined, /╭─ ✓ read_file/);
  assert.match(joined, /· 320ms/);
  assert.ok(lines.some((l) => /╰─+╯/.test(strip(l))));
});

test("editor.layout maps the cursor to a row/col across newlines", () => {
  const l = layout("ab\ncde", 5);
  assert.deepEqual(l.rows, ["ab", "cde"]);
  assert.equal(l.cursorRow, 1);
  assert.equal(l.cursorCol, 2);
});

test("decodeKeys handles wheel, alt-enter, and bracketed paste", () => {
  assert.deepEqual(decodeKeys("\x1b[<64;1;1M"), [{ type: "wheel-up" }]);
  assert.deepEqual(decodeKeys("\x1b[<65;1;1M"), [{ type: "wheel-down" }]);
  assert.deepEqual(decodeKeys("\x1b\r"), [{ type: "alt-enter" }]);
  assert.deepEqual(decodeKeys("\x1b[200~hi\nyo\x1b[201~"), [{ type: "paste", value: "hi\nyo" }]);
});

test("renderFrame grows the input to multiple rows", () => {
  const { lines, cursor } = renderFrame(sampleState({ input: "line1\nline2", cursor: 11 }), 40, 12);
  assert.equal(lines.length, 12);
  assert.match(strip(lines[10]), /line1/);
  assert.match(strip(lines[11]), /line2/);
  assert.ok(cursor && cursor.row === 11);
});

test("renderMarkdown styles headings, bullets, code fences, and inline spans", () => {
  assert.match(strip(renderMarkdown("# Title", 40)[0]), /Title/);
  assert.ok(renderMarkdown("- one\n- two", 40).some((l) => /• one/.test(strip(l))));
  assert.ok(renderMarkdown("```\ncode line\n```", 40).some((l) => /code line/.test(strip(l))));
  const inline = strip(renderMarkdown("a **bold** b `code`", 40).join(" "));
  assert.match(inline, /bold/);
  assert.match(inline, /code/);
});

test("parseInline splits bold/code runs", () => {
  const runs = parseInline("a **b** `c`");
  assert.ok(runs.some((r) => r.text === "b"));
  assert.ok(runs.some((r) => r.text === "c"));
});

test("tool cell collapses output and expands with opts", () => {
  const collapsed = renderBlock(
    { kind: "tool", tool: "run", args: "", status: "ok", output: "l1\nl2\nl3\nl4\nl5\nl6" },
    40,
  );
  assert.ok(collapsed.some((l) => /\+\d+ more/.test(strip(l))));
  const expanded = renderBlock(
    { kind: "tool", tool: "run", args: "", status: "ok", output: "l1\nl2\nl3\nl4\nl5\nl6" },
    40,
    { expandTools: true },
  );
  assert.ok(expanded.some((l) => /l6/.test(strip(l))));
  assert.ok(!expanded.some((l) => /more/.test(strip(l))));
});

test("matchCommands filters by a bare slash prefix", () => {
  assert.deepEqual(
    matchCommands("/m").map((c) => c.name),
    ["model", "models"],
  );
  assert.equal(matchCommands("/model x").length, 0);
  assert.equal(matchCommands("hi").length, 0);
  assert.ok(matchCommands("/").length >= 5);
});

test("renderFrame shows the command palette for a bare slash prefix", () => {
  const { lines } = renderFrame(sampleState({ input: "/m", cursor: 2 }), 50, 14);
  assert.ok(lines.some((l) => /\/model\b/.test(strip(l))));
  assert.ok(lines.some((l) => /\/models/.test(strip(l))));
});

test("renderFrame lays out header/transcript/status/input and fills the screen", () => {
  const { lines, cursor } = renderFrame(sampleState(), 40, 12);
  assert.equal(lines.length, 12);
  assert.match(strip(lines[0]), /Nerve/);
  assert.match(strip(lines[0]), /xai\/grok-4-fast/);
  assert.match(strip(lines[10]), /ready|working/);
  assert.match(strip(lines[11]), /❯ type here/);
  assert.ok(lines.some((l) => /hello/.test(strip(l))));
  assert.ok(lines.some((l) => /hi there/.test(strip(l))));
  assert.ok(cursor && cursor.row === 11);
});

test("renderFrame shows the approval prompt with no input cursor", () => {
  const { lines, cursor } = renderFrame(
    sampleState({
      mode: "approval",
      approval: { tool: "edit", args: '{"p":1}', requestId: "r", sessionId: "s" },
    }),
    50,
    10,
  );
  assert.match(strip(lines[9]), /allow edit .* \[y\/N\]/);
  assert.equal(cursor, undefined);
});

test("Screen.render only rewrites changed rows after the first paint", () => {
  let out = "";
  const fake = {
    columns: 20,
    rows: 3,
    write: (s: string) => {
      out += s;
    },
    on() {},
    off() {},
  };
  const screen = new Screen(fake as unknown as never);
  screen.start();
  out = "";
  screen.render(["row-a", "row-b", "row-c"]);
  assert.match(out, /row-a/);
  assert.match(out, /row-c/);
  out = "";
  screen.render(["row-a", "CHANGED", "row-c"]);
  assert.match(out, /CHANGED/);
  assert.doesNotMatch(out, /row-a/);
  assert.doesNotMatch(out, /row-c/);
  screen.stop();
});
