const name = "honey", n = 7;
if (`hi ${name} ${n + 1}` !== "hi honey 8") process.exit(1);
const tag = (s, x) => s[0].trim() + "@" + x;
if (tag`cost ${5}` !== "cost@5") process.exit(1);
if (`${null} ${undefined} ${-0} ${1e21}` !== "null undefined 0 1e+21") process.exit(1);
