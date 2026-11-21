# honeybun — eslint runs end to end (2026-07 session)

## Status
`hbun` / `hxney` (Rust JS runtime, deno_core 0.300 + v8 0.101) now runs eslint 10.15.0 fully:
config discovery/loading, rule loading, linting, stylish formatter output, exit codes.

## Verified
- `cd /tmp/honeybun-eslint && hbun run .cinm/eslint@10.15.0/bin/eslint.js lintme.js` -> 1 error, exit 1; clean file -> warning only, exit 0.
- `hxney eslint ../../lintme.js` (or abs path; hxney cd's into pkg dir) -> same, exit 1.
- Regressions green: /tmp/hbun-test2 {simple,multi,fs_test,test_cjs,main}.js, hxney test-pkg args, `hbun add ms` + require ("ms" is ESM-only 4.0 nightly; require(esm) interop added — use `msns.parse("2d")`, no default export in that pkg).

## Fixes landed in src/runtime.rs this session
1. path.parse / path.format callbacks (find-up needs them).
2. fs callback-style shim (NODE_FS_CB_SHIM_JS): nodeify fs.promises (stat/access/...) + accessSync polyfill — locate-path/path-exists use util.promisify(fs.stat).
3. util.promisify implemented for real (was identity stub returning the fn).
4. URL.searchParams completed (append/delete/has/forEach; updates search+href); pathToFileURL now returns real URL instance via globalThis.URL.
5. ModuleLoader.load strips ?query/#fragment from file URLs (eslint cache-buster `?mtime=`).
6. Per-module `require`: CJS wrapper now builds a scoped require that re-anchors the MODULE_DIR thread-local via globalThis.__hbunSetRequireDir before each call (fixed deferred/lazy requires using stale dir, e.g. eslint's LazyLoadingRuleMap).
7. CJS detection for main script in run_script (.mjs / nearest package.json "type": "module" / looks_like_esm sniff); CJS mains go through require machinery, ESM through load_main_es_module; both honor process.exitCode at exit (std::process::exit when != 0).
8. structuredClone global (recursive, handles Date/RegExp/Map/Set/ArrayBuffer/views).
9. AbortController/AbortSignal minimal polyfill (throwIfAborted on the PROTOTYPE).
10. looks_like_esm rewritten: scans whole file for import/export statement lines (first-line-only sniff broke humanfs hfs.js).
11. ModuleLoader.load bridges CJS files (dynamic import of CJS) via `export default globalThis.require('path')` — replaced broken in-ESM CJS wrapper (wrap_commonjs_if_needed).
12. path.resolve fixed: was joining cwd into first relative comp + break; now plain right-to-left concat, cwd only if never absolute.
13. exports map conditions fixed: require -> [require, default] (never "import" — was loading index.mjs into CJS pipeline, regexpp broke).
14. util.styleText + util.stripVTControlCharacters (+ ESM export list).
15. transform_esm_to_cjs: require() of ESM files — handles static imports (single-line), export default (named decls + `expr;` form), export const/let/var/function/class, `export {a as b}` blocks & re-exports, `export *`. Names re-exported at end of body; default-only modules unwrap to the default value; adds __esModule. Unsupported: multi-line anonymous default.
16. ARGS set BEFORE JsRuntime::new in run_script (context + process.argv built at construction; was empty before).
17. main.rs Run args: allow_hyphen_values = true (flags like --fix now pass through to scripts).
18. module.createRequire anchors MODULE_DIR from its filename arg.

## Ops notes
- test fixture /tmp/honeybun-eslint/eslint.config.js (flat config with globals) is test scaffolding, not runtime.
- CLI-DEBUG line removed from cli.js; probe_t1.js deleted.
- Build: `cargo build` in /home/void01x/devin/honeybun. About ~10 min of warnings remain (unused vars).
