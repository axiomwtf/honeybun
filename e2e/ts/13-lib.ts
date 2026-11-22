import { triple, DEFAULTS } from "./lib14.js";
interface Cfg { mode: string; readonly threads: number }
if (triple<number>(4) !== 12 || DEFAULTS.mode !== "fast") process.exit(1);
const cfg: Cfg = { ...DEFAULTS, threads: 4 };
if (cfg.threads !== 4) process.exit(1);
