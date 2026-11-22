#!/usr/bin/env bash
# E2E Quality Gate (SPEC §10).
#
# Two suites: e2e/js/ (the oracle) and e2e/ts/ (the same tests, typed).
# The gate: the TS pass rate must be >= 97.25% of the JS pass rate.
# Below threshold this exits 1 (CI blocks) and prints the failing test names.
#
# Usage: scripts/e2e-gate.sh          (HONEY env overrides the binary)
set -u

HONEY="${HONEY:-$HOME/.local/bin/honey}"
GATE_TS_VS_JS="97.25"

here="$(cd "$(dirname "$0")" && pwd)"
cd "$here/.." || exit 1

js_total=0; js_pass=0
ts_total=0; ts_pass=0
js_failed=""
ts_failed=""

echo "== e2e/js (oracle) =="
for f in e2e/js/*.js; do
  [ -f "$f" ] || continue
  js_total=$((js_total + 1))
  if "$HONEY" run "$f" >/tmp/e2e-out.txt 2>&1; then
    js_pass=$((js_pass + 1))
  else
    js_failed="$js_failed $f"
    echo "FAIL $f"
    sed 's/^/    /' /tmp/e2e-out.txt | head -5
  fi
done

echo "== e2e/ts (typed twin) =="
for f in e2e/ts/*.ts; do
  [ -f "$f" ] || continue
  ts_total=$((ts_total + 1))
  if "$HONEY" run "$f" >/tmp/e2e-out.txt 2>&1; then
    ts_pass=$((ts_pass + 1))
  else
    ts_failed="$ts_failed $f"
    echo "FAIL $f"
    sed 's/^/    /' /tmp/e2e-out.txt | head -5
  fi
done

if [ "$js_total" -eq 0 ] || [ "$ts_total" -eq 0 ]; then
  echo "e2e gate: empty suite(s), blocking (js=$js_total ts=$ts_total)"
  exit 1
fi

verdict="$(awk -v p="$ts_pass" -v t="$ts_total" -v jp="$js_pass" -v jt="$js_total" -v gate="$GATE_TS_VS_JS" 'BEGIN {
  tsr = p / t; jsr = jp / jt; need = gate / 100 * jsr;
  printf "js %d/%d (%.2f%%)  ts %d/%d (%.2f%%)  ts-vs-js gate %.2f%%  ", jp, jt, jsr*100, p, t, tsr*100, gate;
  if (tsr + 1e-9 >= need) printf "PASS\n"; else printf "FAIL (need %.2f%%)\n", need*100;
}')"
echo "$verdict"

ts_rate_ok="$(awk -v p="$ts_pass" -v t="$ts_total" -v jp="$js_pass" -v jt="$js_total" -v gate="$GATE_TS_VS_JS" 'BEGIN { print (p/t + 1e-9 >= gate/100 * jp/jt) ? "yes" : "no" }')"
if [ "$ts_rate_ok" != "yes" ] || [ -n "$ts_failed" ]; then
  echo "e2e gate: BLOCKED — failing TS tests:$(echo "$ts_failed" | tr ' ' '\n' | sed 's/^/ /')"
  exit 1
fi
echo "e2e gate: OK"
exit 0
