interface C { d?: number }
const o: { a: null; b: number; c: C; x?: { deep?: { chain?: number } } } = { a: null, b: 0, c: { d: undefined } };
if ((o.a ?? 1) !== 1 || (o.b ?? 1) !== 0 || (o.c?.d ?? 2) !== 2) process.exit(1);
let calls: number = 0;
const lazy = (): number => { calls++; return 3; };
(o.b ?? lazy()); if (calls !== 0) process.exit(1);
