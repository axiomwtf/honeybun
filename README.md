<p align="center">
  <img src="./honeybun.svg" alt="honeybun" width="320">
</p>

# honeybun

**Up to 8.4x faster than Bun. One binary. One toolchain.**

honeybun is a single static binary that provides equivalent functionality to npm, npx, esbuild, vite, and vitest in one toolchain, plus TypeScript-to-JavaScript transpilation. It is Node-compatible, TypeScript-native, scriptable, and faster on the dev loop due to a persistent daemon architecture. It is not a tsc replacement: TypeScript is transpiled with the Oxc parser + transformer, no full type-checking is performed (use tsc for that). honeybun is owned by AXIOM.

> **Status:** `0.1.0` (in development)

## Why

- **One binary.** `honey` is a single static binary. No runtime files. No `.so`. No sidecar.
- **No Bun code.** No Zig. Rust + upstream V8, newest, unpinned (`deno_core = "0"` — every build takes the latest crates.io release). Clean room implementation.
- **Daemon-backed speed.** A persistent daemon holds a warm V8 isolate with an mtime-validated module cache. Every `honey run` is an IPC round-trip (~0.2ms) instead of a cold start. Even a fresh engine is cheap: a build-time V8 context snapshot roughly triples engine init speed.
- **Watch mode.** `honey run --watch file.ts` re-runs on dependency changes via inotify in the daemon (event-driven, no polling) — edit→output refresh in ~1ms on the warm engine. `honey build --watch` is hosted by the daemon too: rebuild frames stream over IPC and the client is a thin relay, so watch mode never pays a cold start either.
- **TypeScript-native.** Oxc parser + transformer. TypeScript is transpiled to JavaScript; full type-checking remains tsc's job. `.ts`, `.mts`, `.cts`, `.tsx` and `.jsx` all run directly (`honey run main.tsx`), with extensionless-import resolution and tsconfig `jsx` settings honored.
- **Node-compatible.** Full compat layer built on upstream `deno_core`. Target: 95%+ of npm packages at v0.1.

## Naming

| Name | Role |
|------|------|
| `honey` | CLI binary, daemon, package manager |
| `hxney` | One-off package executor (npx/bunx equivalent) |
| `Cinnamon` | Internal engine: compiler, resolver, bundler, minifier |
| `.cinm` | Module directory (equivalent functionality to `node_modules`) |
| `node_modules` | Symlink to `.cinm` (interop) |

## Architecture

One static binary. No runtime files. No `.so`. No sidecar.

```
honey (CLI, daemon, PM) → Cinnamon (engine) → V8 (upstream, newest) (runtime)
```

| Component | Role |
|-----------|------|
| `honey` | CLI, daemon, package manager |
| `hxney` | One-off package executor (npx/bunx equivalent) |
| `Cinnamon` | Compiler, Resolver, Bundler, Minifier, `.cinm` layout |
| `V8 (upstream)` | Runtime (newest crates.io `deno_core` + its matched `v8`) |

## Daemon

- Auto-spawned by the CLI on first `honey run`; managed with `honey start` / `honey stop`
- Listens on `/run/user/$UID/honey.sock` (chmod 0600, removed on exit)
- Holds one warm V8 isolate across runs, with an mtime-validated module cache (edit a dependency, the next run picks it up)
- IPC round-trip ~0.2ms on top of a ~90µs process floor
- Crash recovery: the CLI falls back to in-process execution if the socket is dead. `honey` always works.
- Rebuilds: a running daemon keeps executing the code it was spawned with — run `honey stop` once after `cargo build` so the next run picks up the new binary.

## Cinnamon (Engine)

| Subsystem | What it does |
|-----------|-------------|
| Compiler | TS to JS via Oxc parser + transformer. Type-stripping only — no type-checking (use tsc). |
| Resolver | `.cinm` traversal, exports/imports maps, ESM/CJS interop |
| Bundler | Graph to output. Based on Rolldown. |
| Minifier | oxc codegen minify + NODE_ENV define / dead-branch elimination at bundle time. Tree-shake tracked, not landed. |

## Usage

```bash
honey install              # install dependencies (npm registry)
honey add <package>        # add a package
honey remove <package>     # remove a package
honey run file.js          # run a JS file
honey run file.ts          # run a TS file (Oxc transpile)
honey build                # bundle with Cinnamon
honey build -p             # production: minify + NODE_ENV=production define (DCE) + external sourcemap
honey build --minify       # minify only
honey build --sourcemap inline   # linked | external | inline | none
honey build --watch        # rebuild the bundle on source changes (300ms stat-poll)
honey test                 # run tests
honey migrate              # JS to TS migration
hxney <package>           # one-off package executor
```

## Package Manager

- Default registry: npm
- Lockfile: `honey.lock` (binary format, fast)
- Can read `package-lock.json` for migration
- Parallel downloads (tokio) — concurrent BFS waves, `Semaphore(8)`, single atomic lockfile write
- Hard-linked cache (shared across projects)

### `.cinm` Layout

The `.cinm` directory provides equivalent functionality to `node_modules` with a flat, hard-linked structure:

```
.cinm/
├── lodash@4.17.21/          # package name + version
│   ├── package.json
│   ├── index.js
│   └── ...
├── typescript@5.6.3/
├── react@19.2.8/
├── @types/                  # scoped packages
│   └── react@19.2.8/
└── ...
```

When the same package version is installed in multiple projects, files are hard-linked:

```
Project A/.cinm/lodash@4.17.21/index.js  -> inode 42
Project B/.cinm/lodash@4.17.21/index.js  -> inode 42 (same file)
```

`node_modules` is a symlink (Unix) or junction (Windows) pointing to `.cinm`, so `require('lodash')` works in any tool that looks for `node_modules/`. Packages installed by plain npm resolve too — standard `node_modules/<pkg>` with `package.json` `exports`/`main` or an `index.*` entry works whether or not the package went through honey; when both stores exist, `.cinm` is searched first per ancestor and `node_modules` fills the gap. Extensionless imports from `.ts`/`.tsx` files fall back through `.ts`, `.tsx`, `.js`, `.mjs`, `.cjs` and directory indexes (bundler-mode rules). Transpiled `.tsx`/`.jsx` output is cached per file mtime plus the nearest tsconfig.json's mtime, so `jsx`/`jsxImportSource` changes re-transpile immediately — no daemon restart.

## honey.config.ts

```typescript
export default defineConfig({
  onInstall(ctx) { ... }
  onResolve(ctx) { ... }
  transform(ctx) { ... }
  onBuild(ctx) { ... }
  onRun(ctx) { ... }
  onTest(ctx) { ... }
})
```

Zero config out of the box. Fully scriptable when you need it. Runs in the daemon's warm V8 isolate (in-process fallback when the daemon is down).

Hooks are advisory-only in v0.1: they observe commands and can log/mutate config state, hook errors are warnings (never fail the command), and `onResolve`/`transform` are accepted but ignored. `honey.config.js` also works.

## Test Runner

```bash
honey test                 # discover + run all (*.test.* / *.spec.*)
honey test --filter "auth" # filter by path substring
honey test --slow          # include *.slow.* tests
honey test --watch         # re-run on test-file changes (inotify)
```

- Parallel execution: files run concurrently through one-shot worker subprocesses (cap = CPU count), results printed in path order
- Snapshots: `expect(x).toEqual(y)` persists failures to `<testfile>.snap`; `HONEY_SNAPSHOT_UPDATE=1` re-records
- Built-in assertions: an `expect`/`describe`/`it` shim is injected as a preload
- Limitation: parallel runs pay fresh engine init per worker (the warm claim applies to hot re-runs, not first parallel pass)

## Benchmarks

Measured on the actual target hardware: an 8GB RAM i5-13420H laptop, at ~75% RAM utilization, under everyday load — browser open, media streaming while scrolling — not a pristine benchmark rig. Benchmarks were run against the standard `cargo build --release` binary (opt-level 3, LTO, strip) with default `release` profile flags — no special PGO, no custom tuning, and no cherry-picked machine state. Nominal on the dev loop (repeated invocations, warm daemon) is ~5x faster vs Bun, and the peak — warm-daemon per-file test spawns — reaches up to 8.4x. The numbers below were captured while multitasking on an already-pressured machine, which is exactly how a dev tool gets used; a dedicated or idle machine only does better. That is what makes these claims defensible rather than marketing.

| Operation | vs Bun |
|-----------|--------|
| Dev loop (repeated invocations, warm daemon) | 7.4x single-run (warm `honey run hello.js` 1.23ms vs `bun hello.js` 9.14ms), 5.2x batched (50-module batch: 1.76 vs 9.14ms), 5.9x on TS (1.47 vs 8.61ms), 3.6x process floor (0.85 vs 3.10ms) — 2026-11-22 battery medians |
| Test/batch runs (vitest-style suites, warm runner) | 1.6x faster (50 files / 150 assertions: honey 11.35 vs Bun 18.15ms median) |
| Per-file spawns (fresh process per file, warm daemon) | 8.1x faster (50 files: honey 57.1ms vs Bun 460.1ms) |
| Watch mode (edit-refresh cycle) | 2.0x faster (edit→output vs `bun --watch`: honey ~7.4ms vs Bun ~15.1ms warm; re-run itself ~1ms on the warm engine, event-driven inotify, no polling) |
| Node startup-bound workloads | up to 23x faster (warm JS run: honey 1.60ms vs Node 35.85ms) |
| Transpile (TS to JS) | 2.0x faster (1.3MB TS: 25.8 vs 50.8ms) |
| Bundle | 2.4x faster dev (10 source files + CSS: 4.41 vs 10.55ms; `honey build <entry> [-o dir]`: inlines relative imports and bare npm packages (exports/main via `.cinm` or standard `node_modules`), extracts CSS imports to a sibling `.css`, emits a browser-safe `process` guard; `node:*` stays external; tree-shaking: unused-export pruning across the module graph, dead modules dropped) |
| Minify | 2.6x faster (10-module bundle: 3.66 vs 9.53ms; oxc codegen minify + NODE_ENV define with dead-branch elimination (`-p`/`--minify`; prod build 3.77ms)) |
| Sourcemaps | 2.2x faster external (3.63 vs 7.81ms; per-module oxc maps merged at bundle line offsets; `--sourcemap linked|external|inline|none` (linked default unminified, external default with `-p`)) |
| Build watch (3 consecutive source touches) | ~ parity (honey 10.56 vs Bun 10.14ms) |
| Install | ~1:1 (network-bound: 295.8 vs 375.5ms, small package) |
| Type-checking | Not supported (use tsc) |
| Custom minifier | Tracking (custom Cinnamon minifier) |

Reference points for the dev-loop row: a two-line `console.log` script runs in ~113µs median end-to-end through the warm daemon (vs ~590µs for Bun on the same machine in the same session, precision-measured with EPOCHREALTIME, 40 rounds); a `require('ms')`-style script with node_modules resolution runs in ~114µs (Bun: ~623µs). A warm `require()` to an already-cached module costs 2-3µs. The batch rows are the same 50 vitest-style test files: Bun pays its per-file runtime startup 50 times; with the warm daemon honey pays it once, and a fully warm batch re-validates 51 modules via a single mtime check per file (~0.6µs each).

## Building from source

```bash
cargo build --release
# optional, for the fastest engine init: generate + embed the V8 context snapshot
honey --internal-mksnapshot && cargo build --release
```

Regenerate the snapshot (and rebuild) whenever the runtime's context setup changes.

The compiler is written in Rust (edition 2024) and lives in [`src/`](./src).

Dependencies (compile-time; everything statically linked except base system libs):

```
clap, tokio, reqwest (rustls-tls), serde, serde_json, flate2, tar, anyhow,
futures, libc, deno_core (newest, unpinned; brings its own matched v8),
deno_error, oxc_parser, oxc_transformer, oxc_semantic,
oxc_codegen, oxc_allocator, oxc_span
```

## Spec

The full specification is in [`SPEC.md`](./SPEC.md).
