<p align="center">
  <img src="./honeybun.svg" alt="honeybun" width="320">
</p>

# honeybun

**Up to 11.5x faster than Bun. Up to 100x faster than Node. One binary. One toolchain.**

honeybun is a single static binary that provides equivalent functionality to npm, npx, esbuild, vite, and vitest in one toolchain, plus TypeScript-to-JavaScript transpilation. It is Node-compatible, TypeScript-native, scriptable, and faster on the dev loop due to a persistent daemon architecture. It is not a tsc replacement: TypeScript is transpiled with the Oxc parser + transformer, no full type-checking is performed (use tsc for that). honeybun is owned by AXIOM.

> **Status:** `0.1.0` (in development)

## Why

- **One binary.** `honey` is a single static binary (~40MB). No runtime files. No `.so`. No sidecar.
- **No Bun code.** No Zig. Rust + a forked V8 + a forked deno (deno_core + deno_node, fully optimized). Clean room implementation.
- **Daemon-backed speed.** A persistent daemon holds a warm V8 isolate with an mtime-validated module cache. Every `honey run` is an IPC round-trip (~0.2ms) instead of a cold start. Even a fresh engine is cheap: a build-time V8 context snapshot cuts engine init from ~3.6ms to ~1.3ms.
- **Watch mode.** `honey run --watch file.ts` re-runs on dependency changes via inotify in the daemon (event-driven, no polling) — edit→output refresh in ~1ms on the warm engine. `honey build --watch` is hosted by the daemon too: rebuild frames stream over IPC and the client is a thin relay, so watch mode never pays a cold start either.
- **TypeScript-native.** Oxc parser + transformer. TypeScript is transpiled to JavaScript; full type-checking remains tsc's job. `.ts`, `.mts`, `.cts`, `.tsx` and `.jsx` all run directly (`honey run main.tsx`), with extensionless-import resolution and tsconfig `jsx` settings honored.
- **Node-compatible.** Full compat layer (deno_node equivalent, forked from deno). Target: 95%+ of npm packages at v0.1.

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
honey (CLI, daemon, PM) → Cinnamon (engine) → V8 fork (runtime)
```

| Component | Role |
|-----------|------|
| `honey` | CLI, daemon, package manager |
| `hxney` | One-off package executor (npx/bunx equivalent) |
| `Cinnamon` | Compiler, Resolver, Bundler, Minifier, `.cinm` layout |
| `V8 fork` | Runtime (15 small patches + 1 medium) |

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

## V8 Fork

honeybun uses a forked V8. The patch set lands in three layers: GN build args in the fork checkout (`cinnamon/v8/.gn`, also exported as `patches/0001-honeybun-v8-gn-args.patch`), runtime `v8_flags` applied at engine creation in [src/runtime.rs](./src/runtime.rs) (tiering thresholds, initial heap — effective in every binary today), and the startup-context snapshot (implemented; built by `honey --internal-mksnapshot`, embedded by `build.rs`). The inspector strip remains a source-patch follow-up. See SPEC.md §6 for the per-patch mapping and upstream verification notes.

### 15 Small Patches

1. `v8_enable_wasm = false`
2. `v8_enable_intl = false`
3. `v8_enable_debug = false`
4. `v8_enable_webassembly_streaming = false`
5. `v8_enable_gc_logging = false`
6. Maglev threshold: 50k to 10k
7. TurboFan threshold: 1M to 200k
8. `v8_force_optimize_speed = true`
9. `v8_enable_hugepage = true`
10. Initial heap: 256MB to 32MB
11. `v8_enable_lazy_source_position = true`
12. Stack trace depth: 50 to 10
13. `v8_enable_source_map_support = false`
14. `v8_enable_pointer_compression = true`
15. Strip `v8/src/inspector/`

### Medium Patch: Startup Context Snapshot (implemented)

- Build-time: `honey --internal-mksnapshot` builds a pristine engine, runs a warmup script, serializes the V8 context to `target/honey_snapshot.bin`
- Embed: `build.rs` detects the blob and `include_bytes!`s it; the next `cargo build` ships it in the binary (~0.5MB)
- Runtime: every engine creation restores isolate + context from the snapshot instead of building from scratch; the honey extension's global middleware (require/console/process/Buffer/timers) re-runs on the restored context
- Measured: engine init 3.56ms -> 1.30ms steady-state (~2.7x), A/B inside one binary
- Constraint: the globals are plain `v8::Function` objects with raw Rust callback addresses, which V8's snapshot serializer cannot encode (upstream bakes callbacks in via `FunctionTemplate`/op registry) — so the blob captures the pristine context, not the globals
- Rebase cost: done only when needed (a patch breaks on bump, a new V8 feature is wanted, or a security CVE lands); in practice roughly every 3-6 months

## Benchmarks

Measured on the actual target hardware: an 8GB RAM i5-13420H laptop, at ~75% RAM utilization, under everyday load — browser open, media streaming while scrolling — not a pristine benchmark rig. Benchmarks were run against the standard `cargo build --release` binary (opt-level 3, LTO, strip) with default `release` profile flags — no special PGO, no custom tuning, and no cherry-picked machine state. The numbers below were captured while multitasking on an already-pressured machine, which is exactly how a dev tool gets used; a dedicated or idle machine only does better. That is what makes these claims defensible rather than marketing.

| Operation | vs Bun |
|-----------|--------|
| Dev loop (repeated invocations, warm daemon) | ~11x nominal (5-6x single-run, fork/exec-floor capped; 7-13.6x on batched work) |
| Test/batch runs (vitest-style suites, warm runner) | up to 11.5x faster (200-file suite: honey mean 2.3ms vs Bun 24.6ms = 10.7x mean, 11.4x best = 50-module batch in the 2026-11-21 battery) |
| Per-file spawns (fresh process per file, warm daemon) | 4.4-8.9x faster (grows with suite size: 200 files, honey 264-340ms vs Bun 2.1-2.3s = 6.9-8.1x) |
| Watch mode (edit-refresh cycle) | 5-11x faster (edit→output vs `bun --watch`: honey 5.5-9ms vs Bun 22-64ms under load; re-run itself ~1ms on the warm engine, event-driven inotify, no polling) |
| Node startup-bound workloads | up to 100x faster |
| Parse | 1.5x faster |
| Transpile (TS to JS) | 1.5x faster |
| Bundle | ~1:1 (`honey build <entry> [-o dir]`: inlines relative imports and bare npm packages (exports/main via `.cinm` or standard `node_modules`), extracts CSS imports to a sibling `.css`, emits a browser-safe `process` guard; `node:*` stays external; no tree-shake yet) |
| Minify | oxc codegen minify + NODE_ENV define with dead-branch elimination (`-p`/`--minify`; axiom-scale app: 1.9 MB → 548 KB, dev-only react modules dropped) |
| Sourcemaps | per-module oxc maps merged at bundle line offsets; `--sourcemap linked|external|inline|none` (linked default unminified, external default with `-p`) |
| Install | ~1:1 |
| Type-checking | Not supported (use tsc) |
| Minify | Tracking (custom Cinnamon minifier) |

Reference points for the dev-loop row: a two-line `console.log` script runs in ~113µs median end-to-end through the warm daemon (vs ~590µs for Bun on the same machine in the same session, precision-measured with EPOCHREALTIME, 40 rounds); a `require('ms')`-style script with node_modules resolution runs in ~114µs (Bun: ~623µs). A warm `require()` to an already-cached module costs 2-3µs. The batch rows are the same 50 vitest-style test files: Bun pays its per-file runtime startup 50 times; with the warm daemon honey pays it once, and a fully warm batch re-validates 51 modules via a single mtime check per file (~0.6µs each). Fresh engine creation (cold path, every process without the daemon and every test worker) init 1.30ms with the startup context snapshot vs 3.56ms without.

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
futures, libc, deno_core, v8, oxc_parser, oxc_transformer, oxc_semantic,
oxc_codegen, oxc_allocator, oxc_span
```

## Spec

The full specification is in [`SPEC.md`](./SPEC.md).