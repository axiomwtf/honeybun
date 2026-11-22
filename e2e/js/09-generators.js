function* fib() { let a = 0, b = 1; while (true) { yield a; [a, b] = [b, a + b]; } }
const it = fib();
const seen = [];
for (const v of it) { seen.push(v); if (seen.length === 7) break; }
if (seen.join(",") !== "0,1,1,2,3,5,8") process.exit(1);
function* two() { yield 1; yield 2; }
const arr = [...two(), 3];
if (arr.join(",") !== "1,2,3") process.exit(1);
