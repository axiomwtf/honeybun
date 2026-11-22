function counter() { let n = 0; return () => ++n; }
const c = counter();
if (c() !== 1 || c() !== 2 || c() !== 3) process.exit(1);
const add = (a) => (b) => a + b;
if (add(2)(40) !== 42) process.exit(1);
