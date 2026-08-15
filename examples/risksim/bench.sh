#!/usr/bin/env bash
# Fair wall-clock race: compile once with PyRs, then race the native binary
# against CPython on the same scenario (stdout discarded).
# Run from repo root:  bash examples/risksim/bench.sh [scenario]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SCENARIO="${1:-$ROOT/examples/risksim/data/stress.scenario}"
PYRS="${PYRS:-$ROOT/target/release/pyrs}"
PYTHON="${PYTHON:-python3}"
RUNS="${RUNS:-3}"
BIN="${TMPDIR:-/tmp}/risksim-bench-$$"

cleanup() { rm -f "$BIN" "$BIN.ll"; }
trap cleanup EXIT

if [[ ! -x "$PYRS" ]]; then
  echo "missing $PYRS — build with: cargo build -p pyrs --release" >&2
  exit 1
fi

echo "scenario: $SCENARIO"
echo "compiling with pyrs -O2 …"
"$PYRS" compile -O2 -i "$ROOT/examples/risksim/main.py" -o "$BIN"
echo "runs:     $RUNS (best wall time; native binary vs python3)"
echo

# Portable wall timer: seconds as float via date +%s%N when available.
wall_secs() {
  local start end
  start="$(date +%s%N 2>/dev/null || date +%s)"
  "$@" >/dev/null
  end="$(date +%s%N 2>/dev/null || date +%s)"
  if [[ ${#start} -gt 12 ]]; then
    # nanosecond epoch
    awk -v a="$start" -v b="$end" 'BEGIN{printf "%.3f", (b-a)/1e9}'
  else
    awk -v a="$start" -v b="$end" 'BEGIN{printf "%.3f", b-a}'
  fi
}

best_py="999999"
best_pr="999999"
for i in $(seq 1 "$RUNS"); do
  t_py="$(wall_secs "$PYTHON" "$ROOT/examples/risksim/main.py" "$SCENARIO")"
  t_pr="$(wall_secs "$BIN" "$SCENARIO")"
  echo "  run $i  python3=${t_py}s  pyrs_native=${t_pr}s"
  best_py="$(awk -v a="$t_py" -v b="$best_py" 'BEGIN{print (a+0<b+0)?a:b}')"
  best_pr="$(awk -v a="$t_pr" -v b="$best_pr" 'BEGIN{print (a+0<b+0)?a:b}')"
done

echo
echo "best python3:     ${best_py}s"
echo "best pyrs native: ${best_pr}s"
awk -v a="$best_py" -v b="$best_pr" 'BEGIN{
  if (b+0 <= 0) { print "speedup: n/a"; exit }
  printf "speedup:          %.2fx\n", a/b
}'
