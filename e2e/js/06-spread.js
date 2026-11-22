const a = { x: 1, y: 2 }, b = { ...a, y: 3, z: 4 };
if (b.x !== 1 || b.y !== 3 || b.z !== 4) process.exit(1);
const arr = [1, ...[2, 3], ...(function*(){ yield 4; })()];
if (arr.length !== 4 || arr[3] !== 4) process.exit(1);
function sum(...ns) { return ns.reduce((a, b) => a + b, 0); }
if (sum(...[1, 2, 3, 4]) !== 10) process.exit(1);
