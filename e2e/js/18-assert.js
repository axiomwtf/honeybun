const assert = require("assert");
assert.strictEqual(1 + 1, 2, "sum");
assert.deepStrictEqual({ a: [1] }, { a: [1] });
assert.throws(() => { throw new Error("x"); }, /x/);
assert.ok(true);
