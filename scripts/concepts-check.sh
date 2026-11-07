#!/usr/bin/env bash
# `bash scripts/concepts-check.sh` (local only: concepts/ and specs/ are gitignored).
# 1. Every roadmap item (### `#anchor`) in concepts/ROADMAP.md names one spec folder on a
#    **Specified as:** line, and every folder under specs/ (outside archive/) is named once.
# 2. Nothing published cites a path into concepts/, specs/ or .specify/: its reader cannot open it.
set -euo pipefail
fail=0; roadmap=concepts/ROADMAP.md
[ -f "$roadmap" ] && [ -d specs ] || { echo "concepts-check needs concepts/ROADMAP.md and specs/ (not in a clone)" >&2; exit 1; }
items=$(grep -cE '^### `#[a-z0-9-]+`' "$roadmap" || true)
named=$(grep -oE '^\*\*Specified as:\*\* `[0-9]{3}-[a-z0-9-]+`' "$roadmap" | sed -E 's/.*`([^`]+)`/\1/' | sort)
n=$(printf '%s\n' "$named" | grep -c . || true)
[ "$items" -eq "$n" ] || { echo "✗ $roadmap: $items items but $n **Specified as:** lines" >&2; fail=1; }
dupes=$(printf '%s\n' "$named" | uniq -d)
[ -z "$dupes" ] || { echo "✗ named by more than one item: $dupes" >&2; fail=1; }
for s in $named; do [ -f "specs/$s/spec.md" ] || { echo "✗ $roadmap names $s, but specs/$s/spec.md does not exist" >&2; fail=1; }; done
for d in specs/[0-9][0-9][0-9]-*/; do
    [ -d "$d" ] || continue; s=$(basename "$d")
    printf '%s\n' "$named" | grep -qx "$s" || { echo "✗ specs/$s is named by no roadmap item" >&2; fail=1; }
done
leaks=$(grep -rnE '(^|[^A-Za-z0-9_-])(concepts/|specs/|\.specify/)' src README.md site/content tests examples benches 2>/dev/null || true)
[ -z "$leaks" ] || { echo "✗ the published tree cites a path its reader does not have:" >&2; echo "$leaks" | sed 's/^/  /' >&2; fail=1; }
[ "$fail" -eq 0 ] || exit 1
echo "✓ $items roadmap items, each with its specification; no published path into concepts/, specs/ or .specify/"
