const xs = [1, 2, 3, 4];
if (xs.map(x => x * 2).filter(x => x > 2).reduce((a, b) => a + b, 0) !== 18) process.exit(1);
if (xs.includes(3) !== true || xs.indexOf(9) !== -1) process.exit(1);
xs.sort((a, b) => b - a);
if (xs[0] !== 4) process.exit(1);
const flat = [1, [2, [3]]].flat(2);
if (flat.join(",") !== "1,2,3") process.exit(1);
