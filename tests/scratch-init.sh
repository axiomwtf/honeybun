#!/bin/bash
HR=/home/void01x/devin/honeybun/target/release/honey
cd /tmp/honey-bench50 || exit 1
t0=$EPOCHREALTIME
for i in $(seq 1 20); do "$HR" --version >/dev/null 2>&1; done
t1=$EPOCHREALTIME
echo "honey --version mean (20x): $(awk -v a=$t0 -v b=$t1 'BEGIN{printf "%.0fus", (b-a)*1e6/20}')"
t0=$EPOCHREALTIME
for i in $(seq 1 20); do /bin/true; done
t1=$EPOCHREALTIME
echo "/bin/true mean (20x): $(awk -v a=$t0 -v b=$t1 'BEGIN{printf "%.0fus", (b-a)*1e6/20}')"
t0=$EPOCHREALTIME
for i in $(seq 1 20); do "$HR" run hello.js >/dev/null 2>&1; done
t1=$EPOCHREALTIME
echo "honey run hello mean (20x): $(awk -v a=$t0 -v b=$t1 'BEGIN{printf "%.0fus", (b-a)*1e6/20}')"
env -i PATH=/usr/bin:/bin HOME=/home/void01x /usr/bin/bun -e 'console.log("hi")' >/dev/null 2>&1
t0=$EPOCHREALTIME
for i in $(seq 1 20); do env -i PATH=/usr/bin:/bin HOME=/home/void01x /usr/bin/bun -e 'console.log("hi")' >/dev/null 2>&1; done
t1=$EPOCHREALTIME
echo "bun -e mean (20x): $(awk -v a=$t0 -v b=$t1 'BEGIN{printf "%.0fus", (b-a)*1e6/20}')"
