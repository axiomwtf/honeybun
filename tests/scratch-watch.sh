#!/bin/bash
# Watch-mode edit-refresh latency: time from editing dep.js to the re-run's
# REFRESH line landing in the watcher's output. Honey (daemon watch, inotify)
# vs bun --watch. The poll granularity of the measure loop is 1ms.
H=/home/void01x/devin/honeybun/target/release/honey
B=/usr/bin/bun
TO=/tmp/watch-bench
ROUNDS=6

bench() { # name rounds outfile
  local name="$1" rounds="$2" out="$3"
  : > "$out"
  local i=1
  while [ "$i" -le "$rounds" ]; do
    local t0 seen=""
    t0=$(date +%s%N)
    echo "module.exports = $i" > "$TO/dep.js"
    local deadline=$((t0 + 3000000000))
    while [ "$(date +%s%N)" -lt "$deadline" ]; do
      if [ "$(grep -c REFRESH "$out")" -ge "$i" ]; then
        sleep 0.002 # let the line land
        seen=$(date +%s%N)
        break
      fi
      sleep 0.001
    done
    if [ -z "$seen" ]; then
      echo "$name refresh $i: TIMEOUT"
    else
      awk -v a="$t0" -v b="$seen" -v n="$name" -v i="$i" 'BEGIN{printf "%s refresh %d: %.2fms\n", n, i, (b-a)/1e6}'
    fi
    sleep 0.3
    i=$((i+1))
  done
}

cd "$TO" || exit 1

echo "=== honey watch ==="
rm -f /tmp/honey-watch.sock
HONEY_SOCKET=/tmp/honey-watch.sock "$H" start >/dev/null 2>&1
sleep 0.3
HONEY_SOCKET=/tmp/honey-watch.sock "$H" run --watch main.js > honey.out 2>&1 &
HPID=$!
sleep 1.0
bench "honey" "$ROUNDS" "$TO/honey.out"
kill -TERM $HPID 2>/dev/null
sleep 0.3
HONEY_SOCKET=/tmp/honey-watch.sock "$H" stop >/dev/null 2>&1

echo "=== bun watch ==="
env -i PATH=/usr/bin:/bin HOME=/home/void01x "$B" --watch main.js > bun.out 2>&1 &
BPID=$!
sleep 1.0
bench "bun" "$ROUNDS" "$TO/bun.out"
kill -TERM $BPID 2>/dev/null
sleep 0.2
echo "=== summary: $(head -1 /tmp/watch-bench/honey.out >/dev/null; true)"
