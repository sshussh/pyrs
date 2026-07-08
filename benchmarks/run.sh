#!/usr/bin/env bash
# Benchmark PyRs-compiled binaries against CPython.
#
#   ./run.sh            # all benchmarks, best of 3 runs each
#   RUNS=5 ./run.sh     # more runs per benchmark
#   ./run.sh fib sort   # a subset
#
# Each benchmark's output must be byte-identical between python3 and the
# PyRs binary before it is timed — a mismatch aborts the suite.
set -euo pipefail
cd "$(dirname "$0")"

PYRS=${PYRS:-../target/release/pyrs}
RUNS=${RUNS:-3}

command -v python3 > /dev/null || { echo "python3 not found" >&2; exit 1; }
[ -x "$PYRS" ] || { echo "PyRs not built; run: cargo build --release" >&2; exit 1; }

# best (minimum) wall time in nanoseconds over $RUNS runs
best_ns() {
    local best=""
    for _ in $(seq "$RUNS"); do
        local t0 t1 dt
        t0=$(date +%s%N)
        "$@" > /dev/null
        t1=$(date +%s%N)
        dt=$((t1 - t0))
        if [ -z "$best" ] || [ "$dt" -lt "$best" ]; then best=$dt; fi
    done
    echo "$best"
}

secs() { awk -v n="$1" 'BEGIN{printf "%.3f", n / 1e9}'; }

targets=("$@")
if [ ${#targets[@]} -eq 0 ]; then
    for f in *.py; do targets+=("${f%.py}"); done
fi

tmp=$(mktemp -d -t pyrs-bench-XXXXXX)
trap 'rm -rf "$tmp"' EXIT

printf '%-12s %12s %12s %10s\n' benchmark python3 PyRs speedup
printf '%-12s %12s %12s %10s\n' --------- ------- ---- -------

total_py=0
total_rs=0
for name in "${targets[@]}"; do
    src="$name.py"
    bin="$tmp/$name"
    "$PYRS" compile -O2 -i "$src" -o "$bin"

    # correctness first: outputs must match byte-for-byte
    if ! diff <(python3 "$src") <("$bin") > /dev/null; then
        echo "$name: OUTPUT MISMATCH between python3 and PyRs" >&2
        exit 1
    fi

    t_py=$(best_ns python3 "$src")
    t_rs=$(best_ns "$bin")
    total_py=$((total_py + t_py))
    total_rs=$((total_rs + t_rs))
    printf '%-12s %11ss %11ss %10s\n' "$name" "$(secs "$t_py")" "$(secs "$t_rs")" \
        "$(awk -v a="$t_py" -v b="$t_rs" 'BEGIN{printf "%.1fx", a / b}')"
done

printf '%-12s %12s %12s %10s\n' --------- ------- ---- -------
printf '%-12s %11ss %11ss %10s\n' total "$(secs "$total_py")" "$(secs "$total_rs")" \
    "$(awk -v a="$total_py" -v b="$total_rs" 'BEGIN{printf "%.1fx", a / b}')"
