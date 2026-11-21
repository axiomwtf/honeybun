# honeybun Specification
**Version:** 0.1.0
**Ownership:** honeybun is owned by AXIOM
---

## 1. Overview

honeybun is a single static binary that provides equivalent functionality to npm, npx, esbuild, vite, and vitest in one toolchain, plus TypeScript-to-JavaScript transpilation. It is Node-compatible, TypeScript-native, scriptable, and faster on the dev loop due to a persistent daemon architecture. It is not a tsc replacement: TypeScript is transpiled with the Oxc parser + transformer, no full type-checking is performed (use tsc for that).

**The output is always exactly one file: `honey` by default.** No JARs. No DLLs. No sidecars. No "requires runtime" asterisks. Every command compiles down to a single binary. If `honey` cannot produce one binary, it fails with an error. It does not produce partial output.

No Bun code. No Zig. Rust + V8 fork.

---

## 2. Naming

| Name | Role |
|------|------|
| `honey` | CLI binary, daemon, package manager |
| `hxney` | One-off package executor (npx/bunx equivalent) |
| `Cinnamon` | Internal engine: compiler, resolver, bundler, minifier |
| `.cinm` | Module directory (equivalent functionality to node_modules) |
| `node_modules` | Symlink to `.cinm` (interop) |

---

## 3. Architecture

One static binary. No runtime files. No `.so`. No sidecar.

`honey` (CLI, daemon, PM) → `Cinnamon` (engine) → `V8 fork` (runtime)

### Components

| Component | Role |
|-----------|------|
| `honey` | CLI, daemon, package manager |
| `hxney` | One-off package executor (npx/bunx equivalent) |
| `Cinnamon` | Compiler, Resolver, Bundler, Minifier, `.cinm` layout |
| `V8 fork` | Runtime (15 small patches + 1 medium) |

---

## 4. Daemon

- Spawned by `honey start`, or auto-spawned by the CLI on first `honey run`
- Listens on `/run/user/$UID/honey.sock` (chmod 0600, removed on exit)
- Holds one warm V8 isolate across runs; IPC round-trip ~0.2ms on top of a ~90µs process floor
- Crash recovery: the CLI falls back to in-process execution if the socket is dead. `honey stop` kills the daemon.
- Rebuilds: a running daemon keeps executing the code it was spawned with. After `cargo build`, run `honey stop` once so the next `honey run` auto-spawns the new binary.
- Env knobs: `HONEY_NO_DAEMON=1` (disable), `HONEY_SOCKET` (path override), `HONEY_TRACE=1` (trace, forces cold), `HONEY_DAEMON_LOG` (daemon trace file)

### Protocol

Length-prefixed JSON over the Unix socket:

```
Client -> Server: [4-byte LE length][JSON request]
Server -> Client: [4-byte LE length][JSON response]
```

Request shape:
```json
{ "cmd": "run", "file": "app.ts", "args": [], "cwd": "/home/user/project" }
```

Commands: `run`, `watch`, `test_batch`, `eval`, `build`, `build_watch`, `ping`, `stop`.

The `watch` command opens a long-lived session: the daemon runs the file, streams one response frame, then registers inotify watches (non-blocking fd) on the main file plus every resolved dependency file and its directory from the last run's resolution records. Any event re-runs on the warm engine and streams a new frame; events that land mid-run are consumed by the post-run drain, so one editor save yields exactly one re-run. Detection is event-driven (30-60µs from edit to re-run start), no polling. The session ends when the client disconnects; `Ctrl+C` on the CLI kills the client and the daemon returns to its accept loop.

`test_batch` runs a whole suite sequentially in the warm engine (one Response per test file, engine init once per suite instead of one worker subprocess per file). `eval` evaluates a config-hook script (`honey.config.ts` hooks) as a classic script in the warm engine. `build` runs the bundler in the resident process, so one-shot builds never pay a second binary start. `build_watch` hosts the `honey build --watch` loop entirely in the daemon and streams one captured Response frame per rebuild (the once-only header lines ride in the first frame, so the client's output matches the cold watcher's); the session ends when the client disconnects — the daemon zero-timeout-polls the client socket each 10ms tick, so a killed or Ctrl-C'd client frees the daemon immediately.

Response shape:
```json
{ "exit_code": 0, "stdout": "...", "stderr": "...", "duration_ms": 0.4, "error": null }
```

### Fallback

If the daemon isn't running (crashed, not yet started), the CLI auto-spawns it in the background and waits up to 500ms for the socket; if it never appears, the CLI falls back to in-process mode. `honey` always works.

### Warm State

The daemon keeps one V8 isolate alive across successful runs (exit 0). A failed or crashed run drops the engine and its caches.

- The module cache is keyed by absolute resolved path; every cached specifier also keeps a resolution record (specifier + module dir -> resolved path + mtime snapshot). Warm runs re-served from cache are validated by one `stat` per record (~0.6µs each); a changed file drops both its cache entry and its record, and the run re-resolves and re-evaluates from disk.
- The main module always re-executes in a warm engine.
- ESM main modules are an exception: deno_core permits one registered main module per engine, so after an ESM main the engine is rebuilt from scratch on the next run (CJS mains keep full warm reuse).
- System/`node:` builtins are kept in the engine between runs unconditionally.

---

## 5. Cinnamon (Engine)

### Subsystems

| Subsystem | What it does |
|-----------|-------------|
| Compiler | TS to JS via Oxc parser + transformer. Type-stripping only — no type-checking (use tsc). |
| Resolver | `.cinm` traversal, exports/imports maps, ESM/CJS interop; standard `node_modules/<pkg>` fallback (exports/main/index) for npm-installed trees |
| Bundler | Graph to output. Based on Rolldown. |
| Minifier | oxc codegen minify + compile-time defines (`process.env.NODE_ENV`) with static-if/dead-branch elimination to a fixpoint; sourcemaps via oxc_sourcemap concat. Tree-shake tracked, not landed. |

### `.cinm` Layout

Optimized flat/hard-linked layout (no nested `node_modules`):

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

### Hard-Linking

When the same package version is installed in multiple projects, the files are hard-linked, not copied:

```
Project A/.cinm/lodash@4.17.21/index.js  -> inode 42
Project B/.cinm/lodash@4.17.21/index.js  -> inode 42 (same file)
```

- Disk usage is near-zero for shared dependencies
- Updates are isolated (editing one project's copy doesn't affect another)
- The filesystem handles deduplication transparently

### Interop

`node_modules` is a symlink (Unix) or junction (Windows) pointing to `.cinm`:

```
node_modules -> .cinm
```

This means `require('lodash')` works in any tool that looks for `node_modules/`, but the actual files live in the optimized layout. Conversely, a package installed by plain npm (standard `node_modules/<pkg>` with `package.json` `exports`/`main`, or an `index.*` entry) resolves without ever being installed through honey; when both stores exist, `.cinm` is searched first per ancestor directory and `node_modules` fills the gap. Extensionless imports from `.ts`/`.tsx` files fall back through `.ts`, `.tsx`, `.js`, `.mjs`, `.cjs` and directory-index candidates (bundler-mode rules). Transpiled `.tsx`/`.jsx` output is cached per file mtime *plus* the nearest tsconfig.json's mtime, so flipping `jsx`/`jsxImportSource` settings re-transpiles on the next run even mid-daemon-session.

`.gitignore`: `.cinm/` and `node_modules`

### JS to TS Migration

```bash
honey migrate              # whole project
honey migrate --file x.js  # single file
```

- Infers types from usage patterns
- Adds interface/type declarations for repeated shapes
- Flags inferred types with `// @honey: inferred`
- Incremental support

---

## 6. V8 Fork

### Patch application status

Patches land in three places, by what V8 actually supports:

1. **GN args** (`cinnamon/v8/.gn`, exported as `patches/0001-honeybun-v8-gn-args.patch`): WebAssembly off (`v8_enable_webassembly` — renamed from `v8_enable_wasm` in modern V8), Intl/ICU off (`v8_enable_i18n_support`), debug features off (`v8_enable_debugging_features`), huge pages for the code range (`v8_enable_hugepage`), pointer compression (`v8_enable_pointer_compression`; ABI must match rusty_v8's build.rs args.gn). Effective when the fork is built from source; GN ignores unknown names harmlessly.
2. **Runtime v8_flags** (applied in `src/runtime.rs` `apply_v8_flags()` before the first isolate; effective in every shipped binary today): Maglev tier threshold 400→100 invocations, TurboFan tier threshold 3000→750 invocations, initial heap → 32MB, stack trace limit = 10 and lazy source positions = on (both already V8 defaults, kept explicit).
3. **Source patches** (future, applied to the V8 submodule when it is cloned): the medium code-cache patch (serialize `node:*` builtin bytecode at build time, embed as `HONEY_SNAPSHOT`), and the true inspector strip (drop the inspector protocol targets from the build graph — no GN toggle exists in V8 15.8).

Not real, dropped from earlier drafts of the patch list: `v8_enable_webassembly_streaming` (streaming has no toggle; it dies with WebAssembly itself), GC logging (runtime tracing flags, already off), `v8_force_optimize_speed` (covered by `v8_optimized_debug = true`), source-map support (embedder-driven, no build toggle), stack-trace/threshold/heap GN names (they are v8_flags, not build args).

Path-dependency wiring for the fork source build: `patches/0002-honeybun-deno-core-path-deps.patch` (deno_core → the local v8 fork) and `patches/0003-honeybun-deno-path-deps.patch` (deno ext/node → the local deno_core). Apply in order with `git apply patches/*.patch` inside each checkout.

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

### 1 Medium Patch: Startup Context Snapshot (implemented)

- **Build-time:** `honey --internal-mksnapshot` builds a pristine engine (no honey extension), runs a short warmup script (JSON, Date, closures, promises) to bake IC state into the context, and serializes it to `target/honey_snapshot.bin`
- **Embed:** `build.rs` detects the blob, emits `include_bytes!` glue, the next `cargo build` ships it in-binary (~0.5MB); tracked via `rerun-if-changed` so adding/removing the blob triggers a rebuild
- **Runtime:** `build_runtime()` passes `RuntimeOptions::startup_snapshot`; deno_core restores isolate + context from the blob (`Context::from_snapshot`) and re-runs the honey extension's global middleware (require/console/process/Buffer/timers) on the restored global — middleware runs in both init paths upstream
- **Measured:** steady-state engine init 3.56ms -> 1.30ms (~2.7x), A/B inside one binary via `honey --internal-enginetiming` with HONEY_MKSNAPSHOT toggling the path
- **Constraint / divergence from the original design:** the original spec called for serializing `node:*` builtin bytecode via `ScriptCompiler::Compile`. Honey's node-compat globals are plain `v8::Function` objects holding raw Rust callback addresses, which V8's snapshot serializer refuses to encode (upstream bakes callbacks in via `FunctionTemplate`/op registry, honey does not) — the serializer aborts with "Unknown external reference". The implemented blob therefore captures the pristine context (isolate creation, builtins, deno_core internals) and the extension middleware re-injects globals at engine creation, which is where the remaining ~1.3ms goes. A full globals-in-snapshot design needs the builtin surface moved to `FunctionTemplate`s + ops.
- **Rebase cost:** done only when needed (a patch breaks on bump, a new V8 feature is wanted, or a security CVE lands); in practice roughly every 3-6 months

### Patching Strategy

The fork is upstream V8 with a small set of patches applied at build time.

**Patch application** — A `patches/` directory holds diff files applied in order via `git apply` or `patch -p1`. Each patch is a one-line change (build flag) or a small targeted modification.

**Build configuration** — The patches modify `v8.gni` and `BUILD.gn` files:
```
v8_enable_wasm = false
v8_enable_intl = false
v8_enable_debug = false
v8_force_optimize_speed = true
v8_enable_hugepage = true
```

**Rebase process** — Rebases are event-driven, not scheduled: a patch breaks on a bump, a new V8 feature is wanted, or a security CVE lands (roughly every 3-6 months):
1. Update the V8 checkout
2. Run `git apply patches/*.patch` in order
3. If any patch fails, resolve conflicts (usually trivial - the flags haven't changed in years)
4. Rebuild, run the test suite
5. Commit the new version

**The medium patch (startup context snapshot)** — Implemented as described above; the code sketch below shows the original code-cache intent for reference. It touches source/build wiring, not just build flags. At build time a script serializes the V8 context; the blob is embedded in the binary as a static array.

```rust
// Build step
cargo build -> build.rs detects target/honey_snapshot.bin -> include_bytes! glue
honey --internal-mksnapshot -> pristine JsRuntimeForSnapshot + warmup -> blob

// Runtime
let runtime = JsRuntime::new(RuntimeOptions {
    startup_snapshot: Some(HONEY_SNAPSHOT),  // only when the blob was built in
    ...
})
```

### Node Compatibility

- Full compat layer (`deno_node` equivalent, forked from deno)
- Target: 95%+ of npm packages at v0.1
- Native addons (`.node`): v0.2
- No Bun code. Forked deno (deno_core + deno_node, fully optimized). Clean room.

---

## 7. Package Manager

- Default registry: npm
  ```bash
  honey install              # npm
  ```
- Lockfile: `honey.lock` (binary format, fast)
- Can read `package-lock.json` for migration
- Parallel downloads (tokio) — implemented: concurrent BFS waves over a `Semaphore(8)`, shared HTTP client, shared-store dedupe, atomic single-write lockfile
- Hard-linked cache (shared across projects)

### `.cinm` Hard-Link Layout

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

Hard-linking — When the same package version is installed in multiple projects, the files are hard-linked, not copied:

```
Project A/.cinm/lodash@4.17.21/index.js  -> inode 42
Project B/.cinm/lodash@4.17.21/index.js  -> inode 42 (same file)
```

This means:
- Disk usage is near-zero for shared dependencies
- Updates are isolated (editing one project's copy doesn't affect another)
- The filesystem handles deduplication transparently

Interop — `node_modules` is a symlink (Unix) or junction (Windows) pointing to `.cinm`:

```
node_modules -> .cinm
```

This means `require('lodash')` works in any tool that looks for `node_modules/`, but the actual files live in the optimized layout.

Why this matters — pnpm's approach (real `node_modules` made of symlinks into a store) is more complex. This is simpler: one real directory, one symlink. The resolver walks `.cinm` directly, which is faster than traversing nested symlinks.

---

## 8. honey.config.ts

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

- Zero config out of the box
- Fully scriptable when you need it
- Runs in the daemon's warm V8 isolate (no extra engine)

**v0.1 status (implemented):** `onInstall`, `onBuild`, `onRun`, `onTest` fire around the corresponding commands; `honey.config.ts` (or `.js`) is discovered in the cwd, ESM→CJS-transformed and evaluated, `defineConfig` unwrapped, and the hook called with a JSON ctx object. Hooks are advisory-only in v0.1: they can log or mutate config-visible state, hook errors print a warning and never fail the command, `onResolve`/`transform` are accepted but ignored, and the config file cannot yet `require`/import other modules or npm packages.

---

## 9. Test Runner

```bash
honey test                 # discover + run all
honey test --filter "auth" # filter by name
honey test --slow          # include slow tests
honey test --watch         # watch mode
```

- Parallel execution (tokio)
- Snapshots
- Built-in assertions
- Runs on Cinnamon runtime (no separate test framework needed)

**v0.1 status (implemented):** discovery walks `tests/` (falling back to a project-wide walk) for `*.test.js/.ts/.jsx/.tsx` and `*.spec.*` (`.d.ts` excluded, `.slow.` marked); `--filter` is a path substring; parallel execution dispatches files to one-shot worker subprocesses (`honey --internal-test-worker`, stdin JSON protocol, semaphore capped at CPU count) with results printed in path order; built-in assertions via an injected `expect`/`describe`/`it` preload; snapshots persist to `<testfile>.snap` (`HONEY_SNAPSHOT_UPDATE=1` to re-record); `--watch` re-runs on test-file changes via inotify. Limitation: parallel first-pass runs pay fresh engine init per worker — the warm-isolate claim applies to the daemon hot path and sequential re-runs, not to the parallel first pass.

---

## 10. E2E Quality Gate

- Two suites: `e2e/js/` (oracle) and `e2e/ts/` (same tests, typed)
- Gate: TS pass rate must be >= 97.25% of JS pass rate
- Below threshold: CI blocks, prints failing test names
- Track gap over time; raise threshold as it shrinks

---

## 11. Benchmarks (claimed)

### Measurement conditions

Benchmarks are taken on the real target class of hardware — an 8GB RAM i5-13420H laptop — at ~75% RAM utilization, under everyday load: browser open with media streaming while scrolling. Not on an idle benchmark rig. All measurements use the standard `cargo build --release` binary with default profile flags (opt-level 3, LTO, strip) — no PGO, no custom tuning. A quiet dedicated machine only improves the ratios; a laptop under real-world memory and multitask pressure is where the dev loop actually lives, so that is where the ~11x nominal and up-to-11.5x peak are claimed from.

| Operation | vs Bun |
|-----------|--------|
| Dev loop (repeated invocations, warm daemon) | ~11x nominal (5-6x single-run, fork/exec-floor capped; 7-13.6x on batched work) |
| Test/batch runs (vitest-style suites, warm runner) | up to 11.5x faster (200-file suite: honey mean 2.3ms vs Bun 24.6ms = 10.7x mean, 11.4x best = 50-module batch in the 2026-11-21 battery) |
| Per-file spawns (fresh process per file, warm daemon) | 4.4-8.9x faster (grows with suite size: 200 files, honey 264-340ms vs Bun 2.1-2.3s = 6.9-8.1x) |
| Watch mode (edit-refresh cycle) | 5-11x faster (edit→output vs `bun --watch`: honey 5.5-9ms vs Bun 22-64ms under load; re-run itself ~1ms on the warm engine, event-driven inotify, no polling) |
| Node startup-bound workloads | up to 100x faster |
| Parse | 1.5x faster |
| Transpile (TS to JS) | 1.5x faster |
| Bundle | ~1:1 (`honey build <entry> [-o dir]`: inlines relative imports and — since the standard `node_modules` fallback — bare npm packages too, resolved via package.json `exports`/`main` from `.cinm` or `node_modules`; CSS imports are extracted to a sibling `.css` (minified in production); a `process` guard is emitted for browser bundles; `node:` builtins and URL specs stay external; no tree-shake yet) |
| Install | ~1:1 |
| Type-checking | Not supported (use tsc) |
| Minify | ~parity-class (oxc codegen minify + `process.env.NODE_ENV` define with static-if/dead-branch elimination to a fixpoint; a real axiom-scale app bundles 1.9 MB dev → 548 KB `--production`, with dev-only react modules dropped from the graph). Tree-shaking (used-export analysis) is tracked, not landed. |
| Sourcemaps | oxc per-module maps concatenated at bundle line offsets — `--sourcemap linked` (default unminified), `external` (default with `--minify`/`-p`), `inline`, or `none`; tokens map back to original TS/JS lines/columns |

Reference points behind the dev-loop row: a two-line `console.log` script runs in ~113µs median end-to-end through the warm daemon (Bun: ~590µs on the same machine, same session); a `require('ms')`-style node_modules-resolution script runs in ~114µs (Bun: ~623µs); a warm `require()` to an already-cached module costs 2-3µs. Behind the batch row: 50 vitest-style test files where Bun pays its per-file runtime startup 50 times and honey's warm daemon pays it once; warm staleness re-validation is one mtime check per cached file (~0.6µs each). Fresh engine creation init is 1.30ms with the startup context snapshot vs 3.56ms without (§6).

---

## 12. Dependencies (compile-time, linked statically)

```
clap, tokio, reqwest (rustls-tls), serde, serde_json, flate2, tar, anyhow, futures,
deno_core 0.300, v8 0.101, oxc_parser/transformer/semantic/codegen/allocator/ast/span 0.40,
oxc_sourcemap 1.0, libc
```

---

## 13. Build

```bash
cargo build --release
```

Only the base system libraries remain dynamic (libgcc, libm, libc plus the loader); everything else is statically linked. No external files, no `node_modules`, no `.so` shims. One file.

Binaries link non-PIE automatically via `build.rs` (`rustc-link-arg-bins=-no-pie`, Linux only) — plain `cargo build --release` is enough; a non-PIE exec saves ~0.4ms of startup on every invocation.

Optional, for the fastest possible engine init (§6): after the first build, run `honey --internal-mksnapshot` once, then rebuild — the blob is picked up automatically and embedded from then on. Regenerate the blob (and rebuild) whenever the runtime's context setup changes.

---

## 14. Notes

- No Bun code. No Zig. Forked deno (deno_core + deno_node, fully optimized). Clean room.
- The compiler is written in Rust (edition 2024) and lives in `src/`
- The daemon is the single biggest architectural decision in the whole project
- Every `honey run` in the dev loop is an IPC round-trip (~0.2ms on top of a ~90µs process floor) instead of a cold runtime start. That's the up-to-11.5x claim on batched and watch workloads.
- Axiom philosophy ("no sidecars: companion files and helper processes count as bugs"): the daemon is the one permitted helper process, justified by the up-to-11.5x dev-loop claim. Its socket lives in `$XDG_RUNTIME_DIR` (user tmpfs), is chmod 0600, and is removed on daemon exit. Nothing transient is ever written into a user project: only project data (`.cinm/`, `honey.lock`), like Cargo.lock. Transient helper files (hxney's exec wrapper) go to the system temp dir.