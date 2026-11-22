const m = new Map([["a", 1]]);
m.set("b", 2); m.set("c", 3);
if (m.get("b") !== 2 || m.size !== 3 || [...m.keys()].join("") !== "abc") process.exit(1);
const s = new Set([1, 2, 2, 3, 3, 1]);
if (s.size !== 3 || [...s].reduce((a, b) => a + b, 0) !== 6) process.exit(1);
