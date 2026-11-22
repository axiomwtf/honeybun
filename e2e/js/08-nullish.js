const o = { a: null, b: 0, c: { d: undefined } };
if ((o.a ?? 1) !== 1 || (o.b ?? 1) !== 0 || (o.c?.d ?? 2) !== 2) process.exit(1);
if (o.x?.deep?.chain !== undefined) process.exit(1);
let calls = 0;
const lazy = () => { calls++; return 3; };
(o.b ?? lazy()); if (calls !== 0) process.exit(1);
