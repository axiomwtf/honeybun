const big = 2n ** 63n;
if (big !== 9223372036854775808n) process.exit(1);
const sym = Symbol("s");
if (String(sym) !== "Symbol(s)") process.exit(1);
const o = { [sym]: 42 };
if (o[sym] !== 42) process.exit(1);
