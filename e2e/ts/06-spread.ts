interface P { x: number; y: number; z?: number }
const a: P = { x: 1, y: 2 };
const b: P = { ...a, y: 3, z: 4 };
if (b.x !== 1 || b.y !== 3 || b.z !== 4) process.exit(1);
function sum(...ns: number[]): number { return ns.reduce((acc: number, v: number) => acc + v, 0); }
if (sum(...[1, 2, 3, 4]) !== 10) process.exit(1);
