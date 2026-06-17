#!/usr/bin/env bash
#
# check-file-size.sh — hard file-size convention check (see docs/CONVENTIONS.md).
#
# Reports Rust source files whose NON-TEST line count exceeds the cap.
# "Non-test" = everything before the first `#[cfg(test)]` (tests live at the
# bottom by convention). Fails by default. Pass --warn for advisory output.
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
  # Lines before the first `#[cfg(test)]` (or whole file if none).
  n="$(awk '/^[[:space:]]*#\[cfg\(test\)\]/{print NR-1; exit} END{print NR}' "$f" | head -1)"
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
