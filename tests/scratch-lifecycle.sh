#!/bin/bash
# Daemon lifecycle: start/stop cycles, double-spawn race, orphan check.
HR=/home/void01x/devin/honeybun/target/release/honey
count_daemons() { ps -eo cmd | awk '$1=="/home/void01x/devin/honeybun/target/release/honey" && $2=="--internal-daemon"' | wc -l; }

for i in 1 2 3; do
  "$HR" start >/dev/null 2>&1; sleep 0.2
  "$HR" stop >/dev/null 2>&1
done
sleep 0.3
echo "orphan daemons after 3 start/stop cycles: $(count_daemons) (want 0)"

"$HR" start >/dev/null 2>&1; sleep 0.2
"$HR" start >/dev/null 2>&1; sleep 1.0
echo "daemons after double start: $(count_daemons) (want 1)"
"$HR" stop >/dev/null 2>&1
sleep 0.3
echo "daemons after stop: $(count_daemons) (want 0)"
