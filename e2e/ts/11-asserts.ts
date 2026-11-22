const val: unknown = JSON.parse('{"n": 5}');
const typed = val as { n: number };
if (typed.n !== 5) process.exit(1);
function assertTrue(b: boolean): asserts b {}
function len(x: string | string[]): number {
  if (typeof x === "string") return x.length;
  return x.length;
}
if (len("abcd") !== 4 || len([1, 2]) !== 2) process.exit(1);
