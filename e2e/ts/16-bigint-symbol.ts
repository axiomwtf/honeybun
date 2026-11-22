const sym: symbol = Symbol("s");
const o: Record<symbol, number> = { [sym]: 42 };
if (o[sym] !== 42) process.exit(1);
const big: bigint = 2n ** 63n;
if (big !== 9223372036854775808n) process.exit(1);
