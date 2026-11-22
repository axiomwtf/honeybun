const a: number = 1 + 2;
const b: string = "x" + "y";
const c: boolean = true && false;
interface Q { r: number }
const { p, q: { r } }: { p: number; q: Q } = { p: 5, q: { r: 6 } };
if (a !== 3 || b !== "xy" || c !== false) process.exit(1);
if (p !== 5 || r !== 6) process.exit(1);
