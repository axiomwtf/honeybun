interface Style { color: string; size?: number }
const s = { color: "red", size: 4 } satisfies Style;
if (s.color !== "red" || s.size !== 4) process.exit(1);
type Game = { move: (d: number) => number };
const g: Game = { move: (d) => d * 2 };
if (g.move(3) !== 6) process.exit(1);
