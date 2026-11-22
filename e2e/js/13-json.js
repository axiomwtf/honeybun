const obj = { a: [1, "two", { b: null }], c: true };
if (JSON.stringify(JSON.parse(JSON.stringify(obj))) !== JSON.stringify(obj)) process.exit(1);
if (JSON.parse('{"x":[1,2]}').x.length !== 2) process.exit(1);
