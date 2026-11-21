#!/bin/bash
# Watch refresh latency on the 50-file batch fixture, marker-based:
# each round appends `console.log("MARKER n")` to t7; the refresh that re-
# evaluates t7 on the warm engine prints the marker, so detection is exact
# regardless of duplicate inotify events.
H=/home/void01x/devin/honeybun/target/release/honey
B=/usr/bin/bun
TO=/tmp/honey-bench50
OUT=""
ROUNDS=5

bench() { # name rounds
  local name="$1" rounds="$2" i=1
  while [ "$i" -le "$rounds" ]; do
    local t0 seen="" want="MARKER $i"
    t0=$(date +%s%N)
    echo "console.log('$want');" >> "$TO/tests/t7.test.js"
    local deadline=$((t0 + 5000000000))
    while [ "$(date +%s%N)" -lt "$deadline" ]; do
      if grep -q "$want" "$OUT" 2>/dev/null; then
        # let the rest of the frame land, then time it
        sleep 0.002
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
    sleep 0.4
    i=$((i+1))
  done
}

cd "$TO" || exit 1
cp tests/t7.test.js /tmp/t7.bak

echo "=== honey watch (50-file batch) ==="
rm -f /tmp/honey-watch.sock
HONEY_SOCKET=/tmp/honey-watch.sock "$H" start >/dev/null 2>&1
sleep 0.3
HONEY_SOCKET=/tmp/honey-watch.sock "$H" run --watch batch.js > honey50.out 2>&1 &
HPID=$!
sleep 1.0
OUT=honey50.out
bench "honey50" "$ROUNDS"
kill -TERM $HPID 2>/dev/null
sleep 0.3
HONEY_SOCKET=/tmp/honey-watch.sock "$H" stop >/dev/null 2>&1

echo "=== bun watch (50-file batch) ==="
env -i PATH=/usr/bin:/bin HOME=/home/void01x "$B" --watch batch.js > bun50.out 2>&1 &
BPID=$!
sleep 1.0
OUT=bun50.out
bench "bun50" "$ROUNDS"
kill -TERM $BPID 2>/dev/null
sleep 0.2
cp /tmp/t7.bak tests/t7.test.js
echo restored
