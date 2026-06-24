# Code conventions

Size/complexity limits, tuned for both human review and AI-agent context cost
(small, single-responsibility units load less context and are easier to edit).

## Functions — hard limit (CI-enforced)

- **≤ 100 lines per function.** Enforced by `clippy::too_many_lines` (denied in
  CI via `-D warnings`; threshold in `clippy.toml`). Split by responsibility.
- Deep nesting is blocked by `clippy::excessive_nesting` (threshold in `clippy.toml`);
  prefer early returns and extracted helpers.
- Genuinely irreducible cases — static lookup tables, generated tool-spec blocks,
  long table-driven test fixtures — may carry an explicit
  `#[allow(clippy::too_many_lines)] // reason: …` rather than being split into
  meaningless fragments.

## Files — soft cap (advisory)

- **Target ≤ 600 non-test lines per file** (lines before the first `#[cfg(test)]`).
  Checked by `Scripts/check-file-size.sh` (hard gate by default; use `--warn` only for local advisory exploration).
- Over the cap → split by responsibility into a module directory
  (`foo/{mod.rs, ...}`), not by arbitrary line count. A cohesive module that
  reads better whole is fine; a god-file that mixes concerns is not.

## Why these numbers (not 50/200)

The aggressive "AI-ideal" of ~200 lines/file, ~50 lines/function over-fragments
idiomatic Rust (related impls belong together). 100/600 keeps units small enough
for cheap context and review while avoiding a maze of tiny files.

## Gates

```bash
cargo clippy --all-targets -- -D warnings            # functions ≤ 100, nesting
./Scripts/check-file-size.sh                          # hard file-size gate
```
