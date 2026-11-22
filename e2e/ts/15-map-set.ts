const m: Map<string, number> = new Map([["a", 1]]);
m.set("b", 2);
if (m.get("b") !== 2 || m.size !== 2) process.exit(1);
const s: Set<number> = new Set([1, 2, 2, 3]);
if (s.size !== 3) process.exit(1);
