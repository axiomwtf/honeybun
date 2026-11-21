#!/bin/bash
# Full honey-vs-bun benchmark, every category that works today.
# Real target hardware: i5-13420H, 8GB, under normal desktop load.
#
#   bash scripts/bench/run.sh                       # defaults
#   HONEY=... BUN=... ROOT=/tmp/honeybench scripts/bench/run.sh
#
# Run scripts/bench/setup.sh first (or once, fixtures are deterministic).
set -u
BREPO=$(cd "$(dirname "$0")/../.." && pwd)
H=${HONEY:-$BREPO/target/release/honey}
B=${BUN:-/usr/bin/bun}
N=${NODE:-/usr/bin/node}
ROOT=${ROOT:-/tmp/honeybench}
BENCH50=$ROOT/bench50
SUITE=$ROOT/suite
[ -x "$H" ] || { echo "honey binary not found at $H (build with: cargo build --release)"; exit 1; }
cd "$ROOT" || { echo "fixtures missing at $ROOT — run scripts/bench/setup.sh"; exit 1; }

bunrun() { env -i PATH=/usr/bin:/bin HOME="${HOME:-/root}" "$B" "$@"; }
nodex()  { env -i PATH=/usr/bin:/bin HOME="${HOME:-/root}" "$N" "$@"; }

median() { sort -n | awk '{a[NR]=$1} END{if(NR%2) print a[(NR+1)/2]; else printf "%.3f\n",(a[NR/2]+a[NR/2+1])/2}'; }
mean()   { awk '{s+=$1} END{printf "%.3f", s/NR}'; }
pmin()   { sort -n | head -1; }

bench() { # label rounds cmd...
  local label=$1 rounds=$2; shift 2
  local f; f=$(mktemp)
  local i t0 t1
  for i in $(seq 1 "$rounds"); do
    t0=$EPOCHREALTIME
    "$@" >/dev/null 2>&1
    t1=$EPOCHREALTIME
    awk -v a="$t0" -v b="$t1" 'BEGIN{print (b-a)*1000}' >> "$f"
  done
  local med mn me lo
  med=$(median < "$f"); me=$(mean < "$f"); lo=$(pmin < "$f")
  printf '%-46s median=%9.2fms  mean=%9.2fms  min=%9.2fms\n' "$label" "$med" "$me" "$lo"
  rm -f "$f"
}

echo "=============================================================="
echo "honey vs bun benchmark  $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "=============================================================="
echo "honey: $("$H" --version | head -1)"
echo "bun:   $(bunrun --version)"
echo "node:  $(nodex --version)"
echo "cpu:   $(lscpu | awk -F: '/Model name/{gsub(/^ +/,"",$2);print $2}')"
echo "mem:   $(free -m | awk '/Mem:/{print $3"MB used / "$2"MB total"}')"
echo "load:  $(uptime | sed 's/.*load average: //')"
echo

# ---------------------------------------------------------------- daemon up
"$H" stop >/dev/null 2>&1
sleep 0.2
"$H" start >/dev/null 2>&1
sleep 0.3
"$H" run hello.js >/dev/null 2>&1   # warm module cache / engine

echo "--- process floor ---"
bench "honey --version"            20 "$H" --version
bench "bun --version"              20 bunrun --version
bench "node --version"             20 nodex --version
echo

echo "--- simple JS script (honey warm daemon / bun cold per-invocation) ---"
bench "honey warm (daemon) hello.js"    20 "$H" run hello.js
bench "bun hello.js"                    20 bunrun hello.js
bench "node hello.js"                   20 nodex hello.js
echo

echo "--- TypeScript run (honey warm / bun cold) ---"
bench "honey warm hello.ts"        20 "$H" run hello.ts
bench "bun hello.ts"               20 bunrun hello.ts
echo

echo "--- node_modules resolution (honey warm / bun cold) (require('ms')) ---"
bench "honey warm ms.js"           20 "$H" run ms.js
bench "bun ms.js"                  20 bunrun ms.js
echo

echo "--- batch runner: 50 modules, one process (honey warm / bun cold) ---"
bench "honey warm batch.js"        10 "$H" run "$BENCH50/batch.js"
bench "bun batch.js"               10 bunrun "$BENCH50/batch.js"
echo

echo "--- per-file spawns: 50 processes (honey warm daemon / bun cold) ---"
bench "honey warm 50x run"          3 bash -c "for i in \$(seq 1 50); do $H run $BENCH50/tests/t\$i.test.js >/dev/null 2>&1; done"
bench "bun 50x spawn"               3 bash -c "for i in \$(seq 1 50); do env -i PATH=/usr/bin:/bin HOME=${HOME:-/root} $B $BENCH50/tests/t\$i.test.js >/dev/null 2>&1; done"
echo

echo "--- test runner: 50 files / 150 assertions (both one-shot) ---"
bench "honey test"                  5 bash -c "cd $SUITE && $H test"
bench "bun test"                    5 bash -c "cd $SUITE && env -i PATH=/usr/bin:/bin HOME=${HOME:-/root} $B test"
echo

echo "--- transpile: one 1.3MB TS file (one-shot both) ---"
bench "honey build big.ts"          5 "$H" build big.ts -o "$ROOT/big.honey.js"
bench "bun build --no-bundle big.ts" 5 bunrun build big.ts --no-bundle --outfile "$ROOT/big.bun.js"
echo

echo "--- bundle: 100 modules + CSS, dev (one-shot both) ---"
bench "honey build 100 modules"     5 "$H" build bundle/src/index.ts -o "$ROOT/bundle.honey.js"
bench "bun build 100 modules"       5 bunrun build bundle/src/index.ts --outdir "$ROOT/bunout" --target=browser
echo

echo "--- minify: 100-module bundle (one-shot both) ---"
bench "honey build --minify"        5 "$H" build bundle/src/index.ts -o "$ROOT/min.honey.js" --minify
bench "bun build --minify"          5 bunrun build bundle/src/index.ts --outdir "$ROOT/bunoutmin" --target=browser --minify
bench "honey build -p (prod)"       5 "$H" build bundle/src/index.ts -o "$ROOT/prod.honey.js" -p
echo

echo "--- sourcemaps: 100-module bundle, external (one-shot both) ---"
bench "honey build --sourcemap external" 5 "$H" build bundle/src/index.ts -o "$ROOT/sm.honey.js" --sourcemap external
bench "bun build --sourcemap=external"   5 bunrun build bundle/src/index.ts --outdir "$ROOT/bunoutsm" --target=browser --sourcemap=external
echo

# ---------------------------------------------------------------- watch run
watch_run() { # name cmd...
  local name=$1; shift
  local out="$ROOT/watch/$name.out"
  : > "$out"
  "$@" > "$out" 2>&1 &
  local pid=$!
  sleep 1.0
  local k seen t0 t1
  for k in 1 2 3 4 5; do
    t0=$(date +%s%N)
    printf 'module.exports = %s;\n' "$k" > "$ROOT/watch/dep.js"
    seen=""
    local deadline=$((t0 + 3000000000))
    while [ "$(date +%s%N)" -lt "$deadline" ]; do
      if grep -q "REFRESH $k" "$out"; then
        sleep 0.002
        seen=$(date +%s%N)
        break
      fi
      sleep 0.001
    done
    if [ -n "$seen" ]; then
      awk -v a="$t0" -v b="$seen" -v n="$name" -v k="$k" 'BEGIN{printf "%-10s refresh %d: %7.2fms\n", n, k, (b-a)/1e6}'
    else
      echo "$name refresh $k: TIMEOUT"
    fi
    sleep 0.35
  done
  kill -TERM "$pid" 2>/dev/null
  wait "$pid" 2>/dev/null
}

echo "--- watch mode: edit -> re-run output (both warm/watch) ---"
printf 'module.exports = 0;\n' > "$ROOT/watch/dep.js"
watch_run honey "$H" run --watch "$ROOT/watch/main.js"
sleep 0.3
watch_run bun bunrun --watch "$ROOT/watch/main.js"
echo

# ---------------------------------------------------------------- build watch
build_watch() { # name outfile cmd...
  local name=$1 outfile=$2; shift 2
  local log="$ROOT/buildwatch/$name.log"
  : > "$log"
  "$@" > "$log" 2>&1 &
  local pid=$!
  sleep 1.2
  local k seen t0 t1
  for k in 1 2 3; do
    t0=$(date +%s%N)
    printf 'export function used(x: string): string { return `BW-%s-${x}`; }\n' "$k" > "$ROOT/buildwatch/widget.ts"
    seen=""
    local deadline=$((t0 + 3000000000))
    while [ "$(date +%s%N)" -lt "$deadline" ]; do
      if grep -q "BW-$k-" "$outfile" 2>/dev/null; then
        seen=$(date +%s%N)
        break
      fi
      sleep 0.005
    done
    if [ -n "$seen" ]; then
      awk -v a="$t0" -v b="$seen" -v n="$name" -v k="$k" 'BEGIN{printf "%-10s rebuild %d: %7.2fms\n", n, k, (b-a)/1e6}'
    else
      echo "$name rebuild $k: TIMEOUT"
    fi
    sleep 0.5
  done
  kill -TERM "$pid" 2>/dev/null
  wait "$pid" 2>/dev/null
}

echo "--- build watch: edit -> bundle update (both warm/watch) ---"
rm -rf "$ROOT/buildwatch/dist" "$ROOT/buildwatch/distbun"
build_watch honey "$ROOT/buildwatch/dist/widget.js" "$H" build --watch "$ROOT/buildwatch/widget.ts" -o "$ROOT/buildwatch/dist"
sleep 0.3
build_watch bun "$ROOT/buildwatch/distbun/widget.js" bunrun build --watch "$ROOT/buildwatch/widget.ts" --outfile "$ROOT/buildwatch/distbun/widget.js"
echo

# ---------------------------------------------------------------- install
echo "--- install: 3 tiny packages (no warm path; network-bound, informational) ---"
inst() { # name dir cmd...
  local name=$1 dir=$2; shift 2
  rm -rf "$dir"; mkdir -p "$dir"
  printf '{"name":"bench","version":"1.0.0","dependencies":{"ms":"2.1.3","is-number":"7.0.0","left-pad":"1.3.0"}}\n' > "$dir/package.json"
  local t0 t1
  t0=$EPOCHREALTIME
  ( cd "$dir" && "$@" ) >/dev/null 2>&1
  t1=$EPOCHREALTIME
  awk -v a="$t0" -v b="$t1" -v n="$name" 'BEGIN{printf "%-10s install: %8.1fms\n", n, (b-a)*1000}'
}
inst honey "$ROOT/inst-honey" "$H" install
inst bun   "$ROOT/inst-bun"   bunrun install --cache-dir="$ROOT/bun-cache-$$"
echo

"$H" stop >/dev/null 2>&1
echo "done $(date -u +%H:%M:%SZ)"
