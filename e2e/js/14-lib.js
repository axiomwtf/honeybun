import { triple, DEFAULTS } from "./lib14.js";
if (triple(4) !== 12 || DEFAULTS.mode !== "fast") process.exit(1);
const counter = { hits: 0 }; counter.hits += 1;
if (counter.hits !== 1) process.exit(1);
