const a = 1 + 2, b = "x" + "y", c = true && false;
if (a !== 3 || b !== "xy" || c !== false) process.exit(1);
if (typeof null !== "object" || typeof undefined !== "undefined") process.exit(1);
const [d, , e] = [1, 2, 3];
if (d !== 1 || e !== 3) process.exit(1);
const { p, q: { r } } = { p: 5, q: { r: 6 } };
if (p !== 5 || r !== 6) process.exit(1);
