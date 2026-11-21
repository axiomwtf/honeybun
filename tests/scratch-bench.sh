#!/bin/bash
# Precision benchmark: EPOCHREALTIME (no forks inside the timed region).
HR=/home/void01x/devin/honeybun/target/release/honey
B=/usr/bin/bun
CASE=/tmp/honey-daemon-test
cd "$CASE" || exit 1

export HONEY_SOCKET=/tmp/honey-bench.sock   # isolate from any stale daemon
rm -f "$HONEY_SOCKET"

run_bench() {
  local name="$1"; shift
  local n="$1"; shift
  local discard="$1"; shift
  local vals=()
  for ((i=0; i<n; i++)); do
    local t0=$EPOCHREALTIME
    "$@" >/dev/null 2>&1
    local t1=$EPOCHREALTIME
    vals+=("$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.0f", (b-a)*100000}')")
  done
  # stats in microseconds
  local sorted=($(printf '%s\n' "${vals[@]:$discard}" | sort -n))
  local m=${#sorted[@]}
  local median=$(( sorted[m/2] ))
  local min=${sorted[0]}
  local p95=${sorted[ m*95/100 >= m ? m-1 : m*95/100 ]}
  local sum=0; for v in "${sorted[@]}"; do sum=$((sum+v)); done
  printf '%-28s median=%6dus min=%6dus p95=%6dus mean=%6dus\n' "$name" "$median" "$min" "$p95" "$((sum/m))"
}

echo "=== floor"
run_bench "exec /bin/true" 30 3 env -i PATH=/bin /bin/true
echo "=== hello.js"
run_bench "honey(daemon)" 40 5 "$HR" run hello.js
run_bench "bun" 40 5 env -i PATH=/usr/bin:/bin HOME=/home/void01x "$B" hello.js
echo "=== overhead probes"
run_bench "honey --version (clap)" 40 5 "$HR" --version
run_bench "honey run, no daemon" 25 3 env HONEY_NO_DAEMON=1 HONEY_SOCKET=/tmp/honey-bench.sock "$HR" run hello.js
echo "=== ms-require (node_modules)"
run_bench "honey(daemon) ms" 40 5 "$HR" run msrequire.js
run_bench "bun ms" 40 5 env -i PATH=/usr/bin:/bin HOME=/home/void01x "$B" msrequire.js

# Leave no bench daemon behind (it would count as an orphan in lifecycle checks).
"$HR" stop >/dev/null 2>&1
