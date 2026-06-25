#!/usr/bin/env bash
#
# check-file-size.sh — hard file-size convention check (see docs/CONVENTIONS.md).
#
# Reports Rust source files whose NON-TEST line count exceeds the cap.
# "Non-test" = everything before the bottom-of-file test *module* (tests live at
# the bottom by convention). Fails by default. Pass --warn for advisory output.
#
set -euo pipefail

CAP="${FILE_SIZE_CAP:-600}"
WARN=0
case "${1:-}" in
  ""|--strict) ;;
  --warn) WARN=1 ;;
  *)
    echo "usage: $0 [--strict|--warn]" >&2
    exit 2
    ;;
esac

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

over=0
while IFS= read -r f; do
  [[ -f "$f" ]] || continue
  # Lines before the bottom-of-file test *module* (or the whole file if none).
  #
  # The counted (production) region ends ONLY at a `#[cfg(test)]` attribute that
  # introduces a module — i.e. the attribute whose next non-blank line is `mod ...`
  # (the conventional bottom `#[cfg(test)] mod tests`), the inline
  # `#[cfg(test)] mod ...` form, or a bare `mod tests`/`mod tests {`. Recognizes both
  # `#[cfg(test)]` and the platform-gated `#[cfg(all(test, ...))]` form (e.g. unix-only
  # tests).
  #
  # Blind-spot this closes: the old gate exited at the FIRST `#[cfg(test)]` of ANY
  # kind, so a `#[cfg(test)]` on a NON-module item (a test-only `fn`/`impl`/`const`/
  # `use`/`struct` placed mid-file) truncated the production count and HID every line
  # below it. Those non-module test attrs must NOT end the region (conservatively,
  # their lines count as production), so only a real test *module* ends counting.
  n="$(awk '
    function is_test_attr(line) {
      return (line ~ /^[[:space:]]*#\[cfg\(test\)\]/ \
           || line ~ /^[[:space:]]*#\[cfg\(all\(test[,)]/)
    }
    function is_mod_decl(line) {
      return (line ~ /^[[:space:]]*(pub(\([^)]*\))?[[:space:]]+)?mod[[:space:]]/)
    }
    BEGIN { attr_line = 0 }
    # Inline `#[cfg(test)] mod ...` on one line: ends the region at this attribute.
    is_test_attr($0) && is_mod_decl($0) { print NR-1; exit }
    # A `mod` decl whose immediately preceding non-blank line was a test attribute:
    # this is the conventional bottom `#[cfg(test)]\nmod tests`. End at the attribute.
    is_mod_decl($0) && attr_line > 0 { print attr_line-1; exit }
    # Bare `mod tests`/`mod tests {` with no preceding cfg attr (defensive).
    /^[[:space:]]*mod[[:space:]]+tests([[:space:]]*\{|[[:space:]]*;)?[[:space:]]*$/ { print NR-1; exit }
    # Track the most recent test attribute across blank lines so the `mod` decl above
    # can see it. A non-blank, non-attr line (a test-only fn/impl/const/use/struct
    # gated by #[cfg(test)]) clears it — so such items do NOT end the region; their
    # lines conservatively count as production. This is the blind-spot fix.
    {
      if (is_test_attr($0)) attr_line = NR
      else if ($0 !~ /^[[:space:]]*$/) attr_line = 0
    }
    END { print NR }
  ' "$f" | head -1)"
  if (( n > CAP )); then
    printf '  %5d  %s\n' "$n" "$f"
    over=$((over + 1))
  fi
done < <({ git ls-files '*.rs'; git ls-files --others --exclude-standard '*.rs'; } | sort -u | grep -vE '/(tests|benches|examples)/')

if (( over == 0 )); then
  echo "file-size: all source files within ${CAP} non-test lines"
  exit 0
fi
echo "file-size: ${over} file(s) over the ${CAP}-line cap (split by responsibility)"
(( WARN == 1 )) && exit 0 || exit 1
