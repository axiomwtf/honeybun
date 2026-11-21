#!/bin/bash
# Full-correlation timing of one warm batch run: wall vs client phases vs daemon phases.
cd /tmp/honey-bench50 || exit 1
HR=/home/void01x/devin/honeybun/target/release/honey
CL=/tmp/honey-x-client.log
L=/tmp/honey-x-d.log
rm -f "$CL"
"$HR" stop >/dev/null 2>&1
sleep 0.2
HONEY_DAEMON_LOG="$L" "$HR" start >/dev/null
sleep 0.3
export HONEY_CLIENT_LOG="$CL"

t0=$EPOCHREALTIME
"$HR" run batch.js >/dev/null 2>&1
t1=$EPOCHREALTIME
printf 'warm wall: %.3fms  (t0=%s t1=%s)\n' "$(awk -v a=$t0 -v b=$t1 'BEGIN{printf "%.3f", (b-a)*1000}')" "$t0" "$t1"

t0=$EPOCHREALTIME
"$HR" run batch.js >/dev/null 2>&1
t1=$EPOCHREALTIME
printf 'warm wall2: %.3fms (t0=%s t1=%s)\n' "$(awk -v a=$t0 -v b=$t1 'BEGIN{printf "%.3f", (b-a)*1000}')" "$t0" "$t1"

echo '--- client log run2:'
tail -5 "$CL"
echo '--- daemon log run2:'
grep -E 'request received|handle_run begin|run done|response written' "$L" | tail -5
"$HR" stop >/dev/null 2>&1
