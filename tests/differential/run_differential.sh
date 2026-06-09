#!/usr/bin/env bash
# Standalone differential runner (no cargo test harness).
#
# Builds the Rust port emitter, then for each area runs BOTH the original
# (python idare/shadow, node repo-agent-mcp) and the Rust port, and byte-diffs
# the two canonical reports. Prints a PASS/FAIL/SKIP line per area + a summary.
#
# Usage:  bash tests/differential/run_differential.sh
# Exit 0 iff every comparable area matched.
set -u

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"           # cyrus/
fixtures="$here/../fixtures"
py_driver="$here/drivers/emit_python.py"
node_driver="$here/drivers/emit_node.mjs"

echo "== building cyrus-diff-emit =="
( cd "$root" && cargo build -q -p cyrus-differential --bin cyrus-diff-emit )
emit="$root/target/debug/cyrus-diff-emit"
[ -x "$emit" ] || emit="$root/target/debug/cyrus-diff-emit.exe"

# area:program:script
areas=(
  "v1delta:python:$py_driver"
  "sse:python:$py_driver"
  "parse_tool_call:python:$py_driver"
  "relay:python:$py_driver"
  "oauth:node:$node_driver"
)

pass=0; fail=0; skip=0
tmp="${TMPDIR:-/tmp}"

for spec in "${areas[@]}"; do
  area="${spec%%:*}"; rest="${spec#*:}"
  prog="${rest%%:*}"; script="${rest#*:}"
  rustf="$tmp/diff_rust_$area.txt"; origf="$tmp/diff_orig_$area.txt"

  "$emit" "$area" "$fixtures" > "$rustf"

  if ! command -v "$prog" >/dev/null 2>&1; then
    printf 'SKIP   %-16s (%s not found)\n' "$area" "$prog"
    skip=$((skip+1)); continue
  fi

  "$prog" "$script" "$area" "$fixtures" > "$origf"
  if [ $? -eq 86 ]; then
    # Driver sentinel: original source tree not configured (CYRUS_SHADOW_PY_ROOT
    # / CYRUS_OAUTH_TS unset). The originals are not part of this repo.
    printf 'SKIP   %-16s (original not configured; see drivers/)\n' "$area"
    skip=$((skip+1)); continue
  fi

  if cmp -s "$rustf" "$origf"; then
    printf 'PASS   %-16s (%s bytes)\n' "$area" "$(wc -c < "$rustf" | tr -d ' ')"
    pass=$((pass+1))
  else
    printf 'FAIL   %-16s (Rust port != %s original)\n' "$area" "$prog"
    line=$(diff "$rustf" "$origf" | head -4)
    echo "$line" | sed 's/^/         /'
    fail=$((fail+1))
  fi
done

echo
printf 'RESULT: %d passed, %d failed, %d skipped\n' "$pass" "$fail" "$skip"
[ "$fail" -eq 0 ]
