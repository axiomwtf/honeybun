#!/bin/bash
# 50-file vitest-style batch benchmark: per-spawn vs single-runner, honey vs bun.
HR=/home/void01x/devin/honeybun/target/release/honey
B=/usr/bin/bun
CASE=/tmp/honey-bench50
cd "$CASE" || exit 1

$HR stop >/dev/null 2>&1
sleep 0.2

run_honey() {
  local mode="$1"
  local t0=$EPOCHREALTIME
  if [ "$mode" = "per-spawn" ]; then
    for i in $(seq 1 50); do "$HR" run "tests/t$i.test.js" >/dev/null 2>&1; done
  else
    "$HR" run batch.js >/dev/null 2>&1
  fi
  awk -v a="$t0" -v b="$EPOCHREALTIME" 'BEGIN{printf "%.1f", (b-a)*1000}'
}

run_bun() {
  local mode="$1"
  local t0=$EPOCHREALTIME
  if [ "$mode" = "per-spawn" ]; then
    for i in $(seq 1 50); do env -i PATH=/usr/bin:/bin HOME=/home/void01x "$B" "tests/t$i.test.js" >/dev/null 2>&1; done
  else
    env -i PATH=/usr/bin:/bin HOME=/home/void01x "$B" batch.js >/dev/null 2>&1
  fi
  awk -v a="$t0" -v b="$EPOCHREALTIME" 'BEGIN{printf "%.1f", (b-a)*1000}'
}

for round in 1 2 3; do
  h_ps=$(run_honey per-spawn)
  b_ps=$(run_bun per-spawn)
  h_r=$(run_honey runner)
  b_r=$(run_bun runner)
  awk -v hp="$h_ps" -v bp="$b_ps" -v hr="$h_r" -v br="$b_r" -v r="$round" 'BEGIN{
    printf "round %d: per-spawn honey=%sms bun=%sms (%.1fx) | runner honey=%sms bun=%sms (%.1fx)\n",
      r, hp, bp, bp/hp, hr, br, br/hr}'
done
$HR stop >/dev/null 2>&1
