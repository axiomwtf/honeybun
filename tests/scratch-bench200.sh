#!/bin/bash
# 200-file suite: honey per-spawn vs bun per-spawn, warm runner vs warm runner.
H=/home/void01x/devin/honeybun/target/release/honey
B=/usr/bin/bun
export HONEY_SOCKET=/tmp/honey-bench200.sock
rm -f "$HONEY_SOCKET"
env HONEY_TRACE= HONEY_DAEMON_LOG= "$H" start >/dev/null 2>&1
sleep 0.3
cd /tmp/honey-bench200 || exit 1

timeit() { # name rounds cmd...
  local name="$1" rounds="$2"; shift 2
  local best=999999 total=0 n=0 r=0
  while [ "$r" -lt "$rounds" ]; do
    local t0=$EPOCHREALTIME
    "$@" >/dev/null 2>&1
    local t1=$EPOCHREALTIME
    local ms
    ms=$(awk -v a="$t0" -v b="$t1" 'BEGIN{print (b-a)*1000}')
    total=$(awk -v x="$total" -v y="$ms" 'BEGIN{print x+y}')
    n=$((n+1))
    best=$(awk -v b="$best" -v y="$ms" 'BEGIN{print (y<b)?y:b}')
    r=$((r+1))
  done
  awk -v n="$name" -v mean="$(awk -v t="$total" -v n="$n" 'BEGIN{printf "%.1f", t/n}')" -v best="$best" 'BEGIN{printf "%-28s mean=%sms best=%sms\n", n, mean, best}'
}

# warm the daemon plugin caches once
"$H" run batch.js >/dev/null 2>&1

timeit "honey 200 per-spawn" 3 bash -c 'H=/home/void01x/devin/honeybun/target/release/honey; for i in $(seq 1 200); do "$H" run tests/t$i.test.js >/dev/null 2>&1; done'
timeit "bun 200 per-spawn"   3 bash -c 'B=/usr/bin/bun; for i in $(seq 1 200); do env -i PATH=/usr/bin:/bin HOME=/home/void01x "$B" tests/t$i.test.js >/dev/null 2>&1; done'
timeit "honey warm runner"   5 "$H" run batch.js
timeit "bun warm runner"     5 env -i PATH=/usr/bin:/bin HOME=/home/void01x "$B" batch.js
"$H" stop >/dev/null 2>&1
