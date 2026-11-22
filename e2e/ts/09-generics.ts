function first<T>(xs: T[]): T | undefined { return xs[0]; }
function pair<K, V>(k: K, v: V): [K, V] { return [k, v]; }
if (first<number>([3, 1]) !== 3) process.exit(1);
const [k, v] = pair<string, number>("n", 5);
if (k !== "n" || v !== 5) process.exit(1);
interface Repo<T> { get(): T }
const r: Repo<string> = { get: () => "ok" };
if (r.get() !== "ok") process.exit(1);
