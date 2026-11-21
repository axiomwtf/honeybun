#!/bin/bash
# Trace warm batch.js runs: count re-evals, measure in-daemon vs wall.
cd /tmp/honey-bench50 || exit 1
HR=/home/void01x/devin/honeybun/target/release/honey
L=/tmp/honey-batch-$$.log
"$HR" stop >/dev/null 2>&1
sleep 0.2
HONEY_TRACE=1 HONEY_DAEMON_LOG="$L" "$HR" start >/dev/null
sleep 0.3
for n in 1 2 3; do
  t0=$EPOCHREALTIME
  "$HR" run batch.js >/dev/null 2>&1
  t1=$EPOCHREALTIME
  printf 'run%s wall=%.2fms\n' "$n" "$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f", (b-a)*1000}')"
done
echo "total eval lines: $(grep -c eval "$L")"
echo "phase lines:"
grep phase "$L"
echo "in-daemon:"
grep 'run done' "$L"
"$HR" stop >/dev/null 2>&1
