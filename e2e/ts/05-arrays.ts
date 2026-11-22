const xs: number[] = [1, 2, 3, 4];
if (xs.map((x: number) => x * 2).filter((x: number) => x > 2).reduce((a: number, b: number) => a + b, 0) !== 18) process.exit(1);
if (xs.includes(3) !== true || xs.indexOf(9) !== -1) process.exit(1);
xs.sort((a: number, b: number) => b - a);
if (xs[0] !== 4) process.exit(1);
const flat: number[] = [1, [2, [3]]].flat(2);
if (flat.join(",") !== "1,2,3") process.exit(1);
