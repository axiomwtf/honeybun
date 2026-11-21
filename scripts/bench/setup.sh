#!/bin/bash
# Generate the honey benchmark fixtures. Safe to re-run: regenerates
# everything deterministically into $ROOT (default /tmp/honeybench).
#
#   bash scripts/bench/setup.sh            # -> /tmp/honeybench
#   ROOT=/tmp/hb2 bash scripts/bench/setup.sh
#
# Fixtures:
#   hello.js/hello.ts     trivial scripts for run benchmarks
#   ms.js + node_modules/ms  require('ms') resolution benchmark
#   big.ts                ~1.3MB TypeScript (14k functions) for transpile
#   bundle/               100 modules + styles.css for bundle/minify/sourcemap
#   suite/                50 test files x 3 assertions (150) for the test runner
#   bench50/              50 modules + batch.js for batch-process + spawn benches
#   watch/, buildwatch/   seeds for watch-mode benchmarks
#   scale-*.ts            200/2k/20k-function transpile-scale fixtures
set -u
ROOT=${1:-${ROOT:-/tmp/honeybench}}
mkdir -p "$ROOT"
cd "$ROOT" || exit 1
rm -rf bundle suite bench50 watch buildwatch node_modules
mkdir -p bundle/src suite/tests bench50/tests watch buildwatch

# ---- hello fixtures -------------------------------------------------------
cat > hello.js <<'EOF'
console.log("hello", 1 + 2);
EOF
cat > hello.ts <<'EOF'
const greeting: string = "hello";
const n: number = 41;
console.log(greeting, n + 1);
EOF

# ---- require('ms') --------------------------------------------------------
cat > ms.js <<'EOF'
const ms = require("ms");
console.log(ms("2 days"));
EOF
mkdir -p node_modules/ms
cat > node_modules/ms/package.json <<'EOF'
{"name":"ms","version":"2.1.3","main":"index.js"}
EOF
cat > node_modules/ms/index.js <<'EOF'
const s = 1000, m = s * 60, h = m * 60, d = h * 24, w = d * 7, y = d * 365.25;
module.exports = function (str) {
  const num = parseFloat(str);
  if (/\d*s/.test(str)) return num * s;
  if (/\d*m/.test(str)) return num * m;
  if (/\d*h/.test(str)) return num * h;
  if (/\d*d/.test(str)) return num * d;
  if (/\d*w/.test(str)) return num * w;
  if (/\d*y/.test(str)) return num * y;
  return num;
};
EOF

# ---- big.ts (~1.3MB, 14000 functions) --------------------------------------
awk 'BEGIN{
  for (i = 1; i <= 14000; i++)
    printf "function f%d(x: number): number { const a%d = x * %d + %d; return a%d * 2 - %d; }\n",
      i, i, i % 97 + 1, i % 13, i, i % 7;
  printf "console.log(f1(3), f14000(2));\n";
}' > big.ts

# ---- bundle: 100 modules + styles.css --------------------------------------
{
  for i in $(seq 1 100); do
    n=$(printf '%03d' "$i")
    printf 'import { m%s } from "./mod%s";\n' "$n" "$n"
  done
  printf 'import "./styles.css";\n'
  printf 'let total = 0;\n'
  for i in $(seq 1 100); do
    n=$(printf '%03d' "$i")
    printf 'total += m%s(%d);\n' "$n" "$i"
  done
  printf 'console.log("bundle total", total);\n'
} > bundle/src/index.ts
for i in $(seq 1 100); do
  n=$(printf '%03d' "$i")
  printf 'export function m%s(x: number): number { return x * %d + %d; }\n' \
    "$n" "$(( i % 97 + 1 ))" "$i" > "bundle/src/mod$n.ts"
done
cat > bundle/src/styles.css <<'EOF'
:root { --honey: #ffb400; }
.bundle { color: var(--honey); padding: 4px 8px; }
.bundle > .row { display: flex; gap: 8px; }
EOF

# ---- suite: 50 test files x 3 assertions ------------------------------------
for i in $(seq 1 50); do
  {
    printf 'it("adds %d", () => { expect(1 + 1).toBe(2); });\n' "$i"
    printf 'it("strings %d", () => { expect("a" + "%d").toBe("a%d"); });\n' "$i" "$i" "$i"
    printf 'it("arrays %d", () => { expect([1, 2, 3].length).toBe(3); });\n' "$i"
  } > "suite/tests/t$i.test.ts"
done
printf '{"name":"bench-suite"}\n' > suite/package.json

# ---- bench50: 50 modules + batch.js -----------------------------------------
printf '{"name":"bench50","type":"module"}\n' > bench50/package.json
for i in $(seq 1 50); do
  printf 'export const value = %d;\nconsole.log("t%d");\n' "$i" "$i" > "bench50/tests/t$i.test.js"
done
{
  printf 'let total = 0;\n'
  for i in $(seq 1 50); do
    printf 'import { value as v%d } from "./tests/t%d.test.js";\ntotal += v%d;\n' "$i" "$i" "$i"
  done
  printf 'console.log("batch total", total);\n'
} > bench50/batch.js

# ---- watch seeds -------------------------------------------------------------
printf 'module.exports = 0;\n' > watch/dep.js
printf 'const v = require("./dep.js");\nconsole.log("REFRESH", v);\n' > watch/main.js
printf 'export function used(x: string): string { return `BW-0-${x}`; }\n' > buildwatch/widget.ts

# ---- transpile-scale fixtures -------------------------------------------------
for scale in 200 2000 20000; do
  awk -v n="$scale" 'BEGIN{
    for (i = 1; i <= n; i++)
      printf "export function s%d_%d(x: number): number { return x * %d + %d; }\n",
        n, i, i % 89 + 1, i % 11;
    printf "console.log(s%d_1(1));\n", n;
  }' > "scale-$scale.ts"
done

ls -l big.ts scale-*.ts | awk '{print $5, $9}'
echo "fixtures ready in $ROOT"
