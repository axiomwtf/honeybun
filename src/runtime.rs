use anyhow::{Context, Result};
use deno_core::{JsRuntime, RuntimeOptions, ModuleSpecifier, ModuleSourceCode, ModuleCodeString, ModuleType, ModuleLoader, ModuleLoadResponse, ModuleLoadReferrer, ModuleLoadOptions, ModuleSource, ResolutionKind, Extension, v8};
use deno_core::error::AnyError;
use serde_json;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::cell::RefCell;
use std::collections::HashMap;
use futures::executor::block_on;

thread_local! {
    static MODULE_CACHE: RefCell<HashMap<String, deno_core::v8::Global<deno_core::v8::Value>>> = RefCell::new(HashMap::new());
    static MODULE_DIR: RefCell<std::path::PathBuf> = RefCell::new(std::path::PathBuf::new());
    static ARGS: RefCell<Vec<String>> = RefCell::new(Vec::new());
    static EXIT_REQUEST: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
    static UNCAUGHT_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Monotonic counter for synthetic inline-preload script/module names (one
/// per executed preload; uniqueness only aids debugging traces).
static PRELOAD_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record an uncaught main-level error so the run ends with exit code 1.
/// `process.exit()` sentinel exceptions are ignored (handled as exit codes).
fn record_uncaught(text: &str) {
    if text.contains("__honey_exit__") {
        return;
    }
    UNCAUGHT_ERROR.with(|u| {
        let mut slot = u.borrow_mut();
        if slot.is_none() {
            *slot = Some(text.to_string());
        }
    });
}

/// Take a pending `process.exit(code)` request, if any. Consumed by the
/// run loop; nonzero codes propagate as the process exit code instead of
/// terminating the current process (so a daemon can keep serving requests).
pub fn take_exit_request() -> Option<i32> {
    EXIT_REQUEST.with(|e| e.take())
}

pub fn phase_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("HONEY_TRACE").as_deref() == Ok("1"))
}

pub fn phase_trace(label: &str, d: std::time::Duration) {
    if !phase_trace_enabled() {
        return;
    }
    eprintln!("[phase] {label} t={d:?}");
    // Also append to the daemon log when running inside the daemon.
    if let Ok(path) = std::env::var("HONEY_DAEMON_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            use std::io::Write as _;
            let _ = writeln!(f, "[phase] {label} t={d:?}");
        }
    }
}

pub struct Runtime {
    /// Persisted engine for warm reuse across runs (daemon mode). Cold mode
    /// never populates it; the engine is created and dropped per run.
    warm: std::cell::Cell<Option<JsRuntime>>,
    keep_alive: bool,
    /// True once THIS engine registered an ESM main module with deno_core;
    /// such an engine can never load another main module. Must live on the
    /// Runtime (not a thread-local): the daemon hands the warm engine from
    /// thread to thread, so the flag has to travel with it.
    engine_has_main: std::cell::Cell<bool>,
}

impl Runtime {
    pub fn new() -> Self {
        Runtime { warm: std::cell::Cell::new(None), keep_alive: false, engine_has_main: std::cell::Cell::new(false) }
    }

    /// Warm mode: a successful run hands the engine back for the next run,
    /// skipping V8 isolate creation, snapshot decode and global setup.
    pub fn new_warm() -> Self {
        Runtime { warm: std::cell::Cell::new(None), keep_alive: true, engine_has_main: std::cell::Cell::new(false) }
    }

    /// Run a JS/TS file, returning its exit code (1 on a host-level error).
    /// Never calls std::process::exit; exit codes are returned to the caller.
    pub fn run_file(&self, file: &str, args: &[String]) -> Result<i32> {
        let path = Path::new(file);
        if !path.exists() {
            anyhow::bail!("file not found: {}", file);
        }

        self.run_script(file, args)
    }

    pub fn run_script(&self, name: &str, _args: &[String]) -> Result<i32> {
        // Set args so process.argv is populated when globals are built
        ARGS.with(|a| {
            let mut v = a.borrow_mut();
            v.clear();
            v.push("honey".to_string());
            v.push(name.to_string());
            v.extend(_args.iter().cloned());
        });

        let (runtime, result) = match self.warm.take() {
            Some(rt) => {
                // Keep the warm engine for BOTH CJS and ESM mains: CJS goes
                // through warm_prepare + the CJS machinery; a second ESM main
                // evaluates as a side module (see execute_script). Only a
                // genuinely cold start builds a fresh engine here.
                let mut rt = rt;
                {
                    let cwd = std::env::current_dir()
                        .context("failed to get current directory")?;
                    let full_path = cwd.join(name);
                    let t0 = std::time::Instant::now();
                    warm_prepare(&mut rt, &full_path);
                    phase_trace("warm_prepare", t0.elapsed());
                }
                self.execute_script(name, _args, rt)
            }
            None => {
                // Truly cold.
                self.engine_has_main.set(false);
                let rt = build_runtime();
                self.execute_script(name, _args, rt)
            }
        };

        // Hand the engine back for the next run only when the script
        // completed cleanly; a crashed isolate is dropped with its cache.
        if self.keep_alive && matches!(&result, Ok(0)) {
            self.warm.set(Some(runtime));
        } else {
            drop(runtime);
            MODULE_CACHE.with(|c| c.borrow_mut().clear());
            MODULE_RESOLUTIONS.with(|c| c.borrow_mut().clear());
        }
        result
    }

    /// Run one test file with the preload shim wrapper executed INLINE — no
    /// temp wrapper file is ever written to disk.
    pub fn run_preload(&self, main_file: &str, preload: &str, args: &[String]) -> Result<i32> {
        let path = Path::new(main_file);
        if !path.exists() {
            anyhow::bail!("file not found: {}", main_file);
        }
        // Set args so process.argv points at the REAL main module, exactly
        // like the wrapper-file path did.
        ARGS.with(|a| {
            let mut v = a.borrow_mut();
            v.clear();
            v.push("honey".to_string());
            v.push(main_file.to_string());
            v.extend(args.iter().cloned());
        });

        let full_path = std::env::current_dir()
            .context("failed to get current directory")?
            .join(main_file);
        let is_esm_wrapper = preload.contains("export {};");

        let (runtime, result) = match self.warm.take() {
            Some(rt) if !self.engine_has_main.get() => {
                let mut rt = rt;
                warm_prepare(&mut rt, &full_path);
                self.execute_preload(main_file, preload, is_esm_wrapper, rt)
            }
            warm_or_none => {
                drop(warm_or_none);
                let rt = build_runtime();
                self.execute_preload(main_file, preload, is_esm_wrapper, rt)
            }
        };

        if self.keep_alive && matches!(&result, Ok(0)) {
            self.warm.set(Some(runtime));
        } else {
            drop(runtime);
            MODULE_CACHE.with(|c| c.borrow_mut().clear());
            MODULE_RESOLUTIONS.with(|c| c.borrow_mut().clear());
        }
        result
    }

    fn execute_preload(
        &self,
        main_file: &str,
        preload: &str,
        esm_wrapper: bool,
        mut runtime: JsRuntime,
    ) -> (JsRuntime, Result<i32>) {
        // Register the test harness in THIS engine before the wrapper runs
        // (same rule as execute_script; the pending request was set by the
        // daemon when the V2-marked preload was seen).
        execute_pending_test_shim(&mut runtime);
        let full_path = std::env::current_dir()
            .map(|c| c.join(main_file))
            .unwrap_or_else(|_| std::env::temp_dir().join(main_file));
        // Anchor relative requires inside the wrapper to the main file's
        // directory (the wrapper itself only uses absolute specifiers).
        MODULE_DIR.with(|d| {
            *d.borrow_mut() = full_path.parent().map(|p| p.to_path_buf()).unwrap_or_default()
        });

        let outcome: Result<(), anyhow::Error> = if esm_wrapper {
            // ESM wrapper: a synthetic specifier main module (nothing on
            // disk). Relative imports from the wrapper would resolve against
            // this scheme, but its loader step uses the absolute main path.
            let seq = PRELOAD_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let specifier = match ModuleSpecifier::parse(&format!("honey:preload/{seq}")) {
                Ok(s) => s,
                Err(e) => return (runtime, Err(anyhow::anyhow!("bad preload specifier: {e}"))),
            };
            self.engine_has_main.set(true);
            (|| -> anyhow::Result<()> {
                let id = block_on(runtime.load_main_es_module_from_code(&specifier, preload.to_owned()))
                    .map_err(|e| anyhow::anyhow!("failed to load test wrapper: {e}"))?;
                let _ = runtime.mod_evaluate(id);
                if let Err(e) = block_on(runtime.run_event_loop(Default::default())) {
                    let msg = format!("{e}");
                    if msg.contains("__honey_exit__") {
                        if let Some(code) = take_exit_request() {
                            use std::io::Write as _;
                            let _ = std::io::stdout().flush();
                            let _ = std::io::stderr().flush();
                            return Ok(()); // exit_code handled below
                        }
                    }
                    anyhow::bail!("event loop failed: {e:#}");
                }
                Ok(())
            })()
        } else {
            // CJS wrapper: evaluate the wrapper source as a plain script in
            // the warm engine. It references the global `require` (absolute
            // main path) and leaves nothing behind but the harness state.
            (|| -> anyhow::Result<()> {
                runtime
                    .execute_script("honey:preload", preload.to_owned())
                    .map_err(|e| anyhow::anyhow!("{e:#}"))?;
                if let Err(e) = block_on(runtime.run_event_loop(Default::default())) {
                    anyhow::bail!("event loop failed: {e:#}");
                }
                Ok(())
            })()
        };

        // Shared tail: forced exits, uncaught errors, process.exitCode.
        if let Some(code) = take_exit_request() {
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            return (runtime, Ok(code));
        }
        if let Err(e) = outcome {
            return (runtime, Err(e));
        }
        if let Some(_uncaught) = UNCAUGHT_ERROR.with(|u| u.borrow_mut().take()) {
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            return (runtime, Ok(1));
        }
        let exit_code = {
            deno_core::scope!(scope, runtime);
            let context = scope.get_current_context();
            let global = context.global(scope);
            let pk = v8::String::new(scope, "process").unwrap();
            global
                .get(scope, pk.into())
                .and_then(|v| v.to_object(scope))
                .and_then(|p| {
                    let ek = v8::String::new(scope, "exitCode").unwrap();
                    p.get(scope, ek.into())
                })
                .and_then(|v| v.int32_value(scope))
                .unwrap_or(0)
        };
        (runtime, Ok(exit_code))
    }

    /// Evaluate a JS string in the retained warm engine (creating one if
    /// necessary). Pure script scope: the engine records no main module, so
    /// later `run`s keep working against the same isolate. Returns the
    /// JSON-serialized completion value (None for undefined/null). A failing
    /// evaluation drops the engine, mirroring the failed-run warm-state rule.
    pub fn eval_code(&self, code: &str) -> Result<Option<String>, String> {
        let (mut runtime, _) = match self.warm.take() {
            Some(rt) => (rt, true),
            None => (build_runtime(), false),
        };
        let result = (|| -> Result<Option<String>, String> {
            let global = runtime
                .execute_script("honey:eval", code.to_owned())
                .map_err(|e| format!("{e:#}"))?;
            let serialized = {
                deno_core::scope!(scope, runtime);
                let local = v8::Local::new(scope, &global);
                if local.is_undefined() || local.is_null() {
                    None
                } else {
                    v8::json::stringify(scope, local).map(|s| s.to_rust_string_lossy(scope))
                }
            };
            // Settle any promise ops the hook left pending; script hooks must
            // be sync, this only prevents leaked microtasks.
            let _ = block_on(runtime.run_event_loop(Default::default()));
            Ok(serialized)
        })();
        match result {
            Ok(v) => {
                if self.keep_alive {
                    self.warm.set(Some(runtime));
                }
                Ok(v)
            }
            Err(e) => {
                drop(runtime);
                MODULE_CACHE.with(|c| c.borrow_mut().clear());
                MODULE_RESOLUTIONS.with(|c| c.borrow_mut().clear());
                Err(e)
            }
        }
    }

    fn execute_script(&self, name: &str, _args: &[String], mut runtime: JsRuntime) -> (JsRuntime, Result<i32>) {
        // A pending test-preload run registers the harness in THIS engine
        // before the wrapper executes (run_script may have just rebuilt it).
        execute_pending_test_shim(&mut runtime);
        let cwd = std::env::current_dir();
        let (cwd, ret) = match cwd {
            Ok(c) => (c, None),
            Err(e) => (std::env::temp_dir(), Some(Err(anyhow::Error::new(e).context("failed to get current directory")))),
        };
        if let Some(r) = ret {
            return (runtime, r);
        }
        let full_path = cwd.join(name);
        let module_specifier = match ModuleSpecifier::from_file_path(&full_path) {
            Ok(s) => s,
            Err(_) => return (runtime, Err(anyhow::anyhow!("failed to parse module specifier: {}", name))),
        };

        // Set MODULE_DIR to the directory of the main script
        let main_dir = full_path.parent().unwrap_or(&cwd).to_path_buf();
        MODULE_DIR.with(|d| *d.borrow_mut() = main_dir);

        // Load the main module source
        let source = match std::fs::read_to_string(&full_path) {
            Ok(s) => s,
            Err(e) => return (runtime, Err(e).context(format!("failed to read {}", name))),
        };

        // TypeScript mains are transpiled to JS before mode detection, so
        // ESM/CJS sniffing below operates on plain JavaScript.
        let source = match transpile_if_typescript(&full_path.to_string_lossy(), &source) {
            Ok(s) => s,
            Err(e) => return (runtime, Err(anyhow::anyhow!("{}", e))),
        };

        // Decide ESM vs CJS for the main script: .mjs extension, nearest
        // package.json "type": "module", or ESM syntax in the source.
        let t_detect = std::time::Instant::now();
        let is_mjs = full_path.extension().map(|e| e == "mjs" || e == "mts").unwrap_or(false);
        let pkg_type_module = nearest_package_json_is_module(&full_path);
        let esm = is_mjs || pkg_type_module || looks_like_esm(&source);
        phase_trace("esm_detect", t_detect.elapsed());

        if esm {
            // Warm-engine ESM replays: deno_core refuses a second *main*
            // module per engine, but a TLA-free side module can be compiled,
            // instantiated and evaluated into the same warm isolate as many
            // times as we like via `lazy_load_es_module_with_code` (JIT
            // state, snapshot context, builtin caches intact). The URL gets
            // a per-run query so re-running the same path re-executes
            // instead of dedup-hitting the module map; HoneyModuleLoader
            // strips the query before disk IO. Real `main` semantics (first
            // run, and full cold runs) are unchanged.
            let had_main = self.engine_has_main.get();
            if phase_trace_enabled() {
                phase_trace(&format!("esm_replay had_main={had_main}"), std::time::Duration::ZERO);
            }
            let mut esm_warm_done = false;
            if had_main {
                static SIDE_SEQ: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(1);
                let seq = SIDE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let unique = format!("{}?honeyrun={}", module_specifier.as_str(), seq);
                let t_eval = std::time::Instant::now();
                // lazy_load_es_module_with_code registers (with a fresh
                // specifier), compiles, instantiates, evaluates and drains
                // microtasks — all synchronously, with no main-module
                // bookkeeping in the way. It panics on TLA sources and/
                // or errors when the module's imports aren't already in
                // the module map (a different main ran in this engine
                // since their last main load), so the attempt is wrapped:
                // ANY failure falls back to a fresh cold engine and the
                // ordinary main path below.
                let lazy_rc = {
                    let prev_hook = std::panic::take_hook();
                    std::panic::set_hook(Box::new(|_| {}));
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        runtime
                            .lazy_load_es_module_with_code(unique, source.clone())
                    }));
                    std::panic::set_hook(prev_hook);
                    r
                };
                match lazy_rc {
                    Ok(Ok(_ns)) => {}
                    _ => {
                        // Engine spent: discard it entirely and rebuild.
                        // esm_warm_done stays false, so the ordinary main
                        // path below runs on the fresh engine.
                        // v8 150+ requires isolates to drop in reverse
                        // creation order, so the spent isolate must exit
                        // BEFORE the fresh one enters.
                        let t_re = std::time::Instant::now();
                        drop(runtime);
                        let fresh = build_runtime();
                        self.engine_has_main.set(false);
                        runtime = fresh;
                        if phase_trace_enabled() {
                            phase_trace("esm_replay_fallback_rebuild", t_re.elapsed());
                        }
                    }
                }
                // Pump the event loop for timers/promises the module left
                // behind (lazy_load only drains microtasks).
                match block_on(runtime.run_event_loop(Default::default())) {
                    Ok(()) => {}
                    Err(e) => {
                        let msg = format!("{}", e);
                        if msg.contains("__honey_exit__") {
                            if let Some(code) = take_exit_request() {
                                use std::io::Write as _;
                                let _ = std::io::stdout().flush();
                                let _ = std::io::stderr().flush();
                                return (runtime, Ok(code));
                            }
                        }
                        return (runtime, Err(e).context("event loop failed"));
                    }
                }
                if phase_trace_enabled() {
                    phase_trace("esm_eval+loop", t_eval.elapsed());
                }
                esm_warm_done = true;
            }
            if !esm_warm_done {
                // Use load_main_es_module_from_code to avoid double loading
                self.engine_has_main.set(true);
                let t_eval = std::time::Instant::now();
                let id = match block_on(runtime.load_main_es_module_from_code(&module_specifier, source)) {
                    Ok(id) => id,
                    Err(e) => return (runtime, Err(e).context("failed to load module")),
                };

                // Evaluate the module - run mod_evaluate and event loop like deno_core tests
                let _ = runtime.mod_evaluate(id);
                match block_on(runtime.run_event_loop(Default::default())) {
                    Ok(()) => {}
                    Err(e) => {
                        let msg = format!("{}", e);
                        if msg.contains("__honey_exit__") {
                            if let Some(code) = take_exit_request() {
                                use std::io::Write as _;
                                let _ = std::io::stdout().flush();
                                let _ = std::io::stderr().flush();
                                return (runtime, Ok(code));
                            }
                        }
                        return (runtime, Err(e).context("event loop failed"));
                    }
                }
                if phase_trace_enabled() {
                    phase_trace("esm_eval+loop", t_eval.elapsed());
                }
            }
        } else {
            // CommonJS main script: evaluate through the require machinery with a
            // correctly anchored per-module require.
            let t_cjs = std::time::Instant::now();
            let cjs_outcome: Result<Option<i32>> = (|| {
                deno_core::scope!(scope, runtime);
                let context = scope.get_current_context();
                let global = context.global(scope);
                let require_key = v8::String::new(scope, "require").unwrap();
                let require_val = global
                    .get(scope, require_key.into())
                    .ok_or_else(|| anyhow::anyhow!("global require missing"))?;
                let require_fn = v8::Local::<v8::Function>::try_from(require_val)
                    .map_err(|_| anyhow::anyhow!("global require is not a function"))?;
                let arg = v8::String::new(scope, &full_path.to_string_lossy()).unwrap();
                let undefined = v8::undefined(scope);
                v8::tc_scope!(let tc, scope);
                let result = require_fn.call(tc, undefined.into(), &[arg.into()]);
                let forced_exit = take_exit_request();
                if let Some(code) = forced_exit {
                    use std::io::Write as _;
                    let _ = std::io::stdout().flush();
                    let _ = std::io::stderr().flush();
                    return Ok(Some(code));
                }
                if result.is_none() {
                    let msg_parts = tc
                        .exception()
                        .and_then(|exc| {
                            let exc_local = v8::Local::new(tc, exc);
                            if let Ok(o) = v8::Local::<v8::Object>::try_from(exc_local) {
                                let stack_key = v8::String::new(tc, "stack").unwrap();
                                o.get(tc, stack_key.into())
                                    .and_then(|v| get_str(tc, v))
                            } else {
                                get_str(tc, exc_local)
                            }
                        })
                        .unwrap_or_else(|| "unknown error".to_string());
                    record_uncaught(&msg_parts);
                    eprintln!("honey: {} failed: {}", name, msg_parts);
                }
                Ok(None)
            })();
            phase_trace("cjs_eval", t_cjs.elapsed());
            match cjs_outcome {
                Err(e) => return (runtime, Err(e)),
                Ok(Some(code)) => return (runtime, Ok(code)),
                Ok(None) => {}
            }
            // Dynamic imports and deferred work still need the event loop.
            let t_loop = std::time::Instant::now();
            match block_on(runtime.run_event_loop(Default::default())) {
                Ok(()) => {}
                Err(e) => return (runtime, Err(e).context("event loop failed")),
            }
            phase_trace("cjs_event_loop", t_loop.elapsed());
        }

        // Honor process.exitCode set by scripts (e.g. eslint's exit code),
        // plus any pending process.exit() request.
        if let Some(code) = take_exit_request() {
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            return (runtime, Ok(code));
        }
        if let Some(_uncaught) = UNCAUGHT_ERROR.with(|u| u.borrow_mut().take()) {
            // The error was already reported with its stack; exit nonzero.
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            return (runtime, Ok(1));
        }
        let exit_code = {
            deno_core::scope!(scope, runtime);
            let context = scope.get_current_context();
            let global = context.global(scope);
            let pk = v8::String::new(scope, "process").unwrap();
            global
                .get(scope, pk.into())
                .and_then(|v| v.to_object(scope))
                .and_then(|p| {
                    let ek = v8::String::new(scope, "exitCode").unwrap();
                    p.get(scope, ek.into())
                })
                .and_then(|v| v.int32_value(scope))
                .unwrap_or(0)
        };
        (runtime, Ok(exit_code))
    }
}

// Startup-snapshot blob, produced by build.rs (empty when absent).
mod honey_snapshot_gen {
    include!(concat!(env!("OUT_DIR"), "/honey_snapshot_gen.rs"));
}

/// True when the engine should start from the compiled-in V8 context
/// snapshot. The snapshot generator sets HONEY_MKSNAPSHOT so it seeds from a
/// pristine context even once a blob exists (chicken-and-egg guard).
pub fn snapshot_active() -> bool {
    !honey_snapshot_gen::BLOB.is_empty() && std::env::var_os("HONEY_MKSNAPSHOT").is_none()
}

/// Internal benchmarking: create N engines with the runtime's normal init
/// path (snapshot on unless HONEY_MKSNAPSHOT is set) and print per-engine
/// init times. Used to A/B the startup snapshot within one binary.
pub fn engine_init_timing(n: usize) {
    for i in 0..n {
        // Each engine lives in its own block: v8 150+ requires isolates to
        // drop in reverse creation order, so engine i must be GONE before
        // engine i+1's isolate is constructed.
        let t0 = std::time::Instant::now();
        let elapsed = {
            let mut rt = build_runtime();
            // Load a trivial script so lazy context portions materialize.
            let _ = rt.execute_script("honey:timing", "1+1".to_owned()).ok();
            t0.elapsed()
        };
        if i == 0 || n <= 10 {
            eprintln!("engine init {}: {:?}", i, elapsed);
        }
    }
}

/// Build the startup-snapshot blob: a JsRuntimeForSnapshot with the exact
/// production RuntimeOptions EXCEPT the honey extension. The injected globals
/// are plain v8::Function objects holding raw callback addresses, which the
/// V8 snapshot serializer cannot encode, so the blob captures the pristine
/// context (isolate, deno_core internals, builtins); the honey extension's
/// global middleware is re-run on the restored context at engine creation
/// (deno_core runs it in both init paths, restoring require/console/etc).
/// Returns an empty Vec on failure; callers must treat that as "no snapshot".
pub fn make_snapshot_blob() -> Vec<u8> {
    use deno_core::{JsRuntimeForSnapshot, RuntimeOptions};

    apply_v8_flags();
    let mut runtime = JsRuntimeForSnapshot::new(RuntimeOptions {
        module_loader: Some(Rc::new(HoneyModuleLoader)),
        ..Default::default()
    });

    // Warmup: exercise V8 builtins on the hot paths user scripts hit first
    // so their bytecode / IC state is part of the serialized context.
    const WARMUP: &str = r#"
        const __o = { a: 1, b: "x", c: [1, 2, 3] };
        JSON.parse(JSON.stringify(__o));
        const __d = new Date();
        typeof __o.a === "number";
        const __r = (x) => x * 2;
        for (let i = 0; i < 32; i++) __r(i);
        const __p = Promise.resolve(1);
        "#;
    if let Err(e) = runtime.execute_script("honey:warmpup", WARMUP.to_owned()) {
        eprintln!("warning: snapshot warmup script failed: {e}");
    }

    let boxed = runtime.snapshot();
    boxed.into_vec()
}

// deno_core captures the ambient tokio handle when a JsRuntime is created
// and spawns every delayed V8 task (timers, GC background tasks) onto it.
// Building an engine with no tokio context — e.g. on a daemon's plain std
// thread — makes deno_core abort() the whole process the first time V8
// posts a delayed task. Entering a process-wide tokio runtime here fixes
// that for every engine built off a plain thread (daemon, test workers).
// The EnterGuard only needs to outlive JsRuntime::new: deno_core clones
// the handle and keeps it in the isolate entry.
static GLOBAL_TOKIO_RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

fn enter_tokio_ctx() -> Option<tokio::runtime::EnterGuard<'static>> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return None;
    }
    let rt = GLOBAL_TOKIO_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build global tokio runtime for engine context")
    });
    Some(rt.enter())
}

/// Build a fresh JsRuntime with the honey core extension.
fn build_runtime() -> JsRuntime {
    let _tokio_ctx = enter_tokio_ctx();
    apply_v8_flags();
    let extension = Extension {
        name: "honey:core",
        deps: &[],
        ops: Cow::Owned(vec![]),
        global_object_middleware: Some(global_object_middleware),
        ..Default::default()
    };
    let snapshot: Option<&'static [u8]> = if snapshot_active() {
        Some(honey_snapshot_gen::BLOB)
    } else {
        None
    };
    let startup_snapshot = snapshot;
    let engine_gen = ENGINE_GEN.with(|g| g.get() + 1);
    ENGINE_GEN.with(|g| g.set(engine_gen));
    JsRuntime::new(RuntimeOptions {
        module_loader: Some(Rc::new(HoneyModuleLoader)),
        extensions: vec![extension],
        startup_snapshot,
        ..Default::default()
    })
}

/// V8 v8_flags equivalents of the fork's runtime-level patches. These are the
/// items that cannot be GN args (tiering thresholds, heap sizing). Unknown
/// flags make set_flags_from_string return Err; ignored, defaults stay.
fn apply_v8_flags() {
    // One string; V8 parses flags separated by spaces/newlines. An unknown
    // flag is reported by V8 on stderr (and honored as default) unless
    // --abort-on-contradictory-flags is set, so a stale flag name can never
    // break the build or the run.
    deno_core::v8::V8::set_flags_from_string(concat!(
        // Patch 6: tier up to Maglev sooner (V8 default 400).
        "--invocation-count-for-maglev=100 ",
        // Patch 7: tier up to TurboFan sooner (V8 default 3000).
        "--invocation-count-for-turbofan=750 ",
        // Patch 10: 32MB initial heap instead of V8's auto sizing.
        "--initial-heap-size=32 ",
        // Patches 11/12: already V8 defaults; set explicitly as documentation.
        "--enable-lazy-source-positions ",
        "--stack-trace-limit=10",
    ));
}

thread_local! {
    /// (specifier \0 requiring-dir) -> (resolved file path, mtime at record
    /// time), so warm runs skip resolution and re-evaluation for unchanged
    /// files. Bare names, relative and absolute specifiers all participate.
    static MODULE_RESOLUTIONS: RefCell<HashMap<String, (String, (i64, u32))>> = RefCell::new(HashMap::new());
}

fn stat_mtime(path: &str) -> Option<(i64, u32)> {
    let d = std::fs::metadata(path).ok()?.modified().ok()?
        .duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((d.as_secs() as i64, d.subsec_nanos()))
}

/// mtime of an arbitrary path, for daemon-side watch loops.
pub fn stat_path_mtime(path: &str) -> Option<(i64, u32)> {
    stat_mtime(path)
}

/// Resolved absolute paths + recorded mtimes of every module required since
/// the last cache clear (deduped). The daemon watch session uses this as the
/// dependency set of the last run.
pub fn recorded_dep_mtimes() -> Vec<(String, (i64, u32))> {
    let mut seen = std::collections::HashSet::new();
    MODULE_RESOLUTIONS.with(|m| {
        m.borrow()
            .values()
            .filter(|r| seen.insert(r.0.clone()))
            .cloned()
            .collect()
    })
}

/// Insert a file-backed module into the warm module cache.
fn cache_module_with_mtime(key: &str, value: v8::Global<v8::Value>) {
    MODULE_CACHE.with(|c| c.borrow_mut().insert(key.to_string(), value));
}

/// Drop one file from the warm engine's module cache so the next `require()`
/// re-evaluates it even when its mtime is unchanged. Used by the warm test
/// batch: every test file must execute on every suite run.
pub fn evict_module(path: &str) {
    MODULE_CACHE.with(|c| c.borrow_mut().remove(path));
    MODULE_RESOLUTIONS.with(|m| {
        m.borrow_mut().retain(|_, (p, _)| p != path);
    });
}

/// Prepare a warm engine for a new run:
/// - drop bare/relative specifier keys (they encode the previous run's dir;
///   in practice nothing is stored under these keys anymore)
/// - always drop the main module so it re-executes
/// - reset process.exitCode and rebuild process.argv for this run
///
/// Staleness is NOT checked here: file-backed entries are validated through
/// their resolution records (specifier \0 MODULE_DIR -> path, mtime) in the
/// require fast path, which stats the target exactly when it is needed.
fn warm_prepare(runtime: &mut JsRuntime, main_path: &Path) {
    {
        let keys = MODULE_CACHE.with(|c| c.borrow().keys().cloned().collect::<Vec<String>>());
        for key in keys {
            match key.strip_prefix("node:") {
                // node: builtins live in the engine, not on disk; keep them.
                Some(_) => continue,
                None => {}
            }
            if !key.starts_with('/') {
                // Non-path keys (legacy relative specifiers, bare names) can
                // outlive the directory they were evaluated in; drop them.
                MODULE_CACHE.with(|c| c.borrow_mut().remove(&key));
            }
        }
    }

    // The main module always re-executes in a warm engine.
    let main_key = main_path.to_string_lossy().to_string();
    MODULE_CACHE.with(|c| c.borrow_mut().remove(&main_key));

    // 2. Reset per-run globals inside the retained isolate.
    deno_core::scope!(scope, runtime);
    let context = scope.get_current_context();
    let global = context.global(scope);
    let pk = v8::String::new(scope, "process").unwrap();
    if let Some(p) = global.get(scope, pk.into()).and_then(|v| v.to_object(scope)) {
        // argv
        let args = ARGS.with(|a| a.borrow().clone());
        let argv = v8::Array::new(scope, args.len() as i32);
        for (i, a) in args.iter().enumerate() {
            let s = v8::String::new(scope, a).unwrap();
            let idx = v8::Number::new(scope, i as f64);
            argv.set(scope, idx.into(), s.into());
        }
        let argv_key = v8::String::new(scope, "argv").unwrap();
        p.set(scope, argv_key.into(), argv.into());
        // exitCode back to 0
        let eck = v8::String::new(scope, "exitCode").unwrap();
        let zero = v8::Number::new(scope, 0.0);
        p.set(scope, eck.into(), zero.into());
    }
}

use std::borrow::Cow;

struct HoneyModuleLoader;

impl HoneyModuleLoader {
    fn resolve_anyhow(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, AnyError> {
        // Bare npm package specifiers (e.g. dynamic import("@humanfs/node"))
        // resolve through our CJS/npm resolver instead of URL joins.
        let is_url = specifier.contains("://") || specifier.starts_with("data:");
        let is_pathish = specifier.starts_with("./")
            || specifier.starts_with("../")
            || specifier.starts_with('/')
            || specifier.starts_with("file:");
        if !is_url && !is_pathish && !specifier.starts_with("node:") {
            match resolve_module_path_prefer(specifier, true) {
                Ok(path) => {
                    let url = ModuleSpecifier::from_file_path(PathBuf::from(&path))
                        .map_err(|_| anyhow::anyhow!("failed to convert to file URL: {}", path))?;
                    return Ok(url);
                }
                Err(e) => {
                    return Err(AnyError::msg(format!("import failed: {}", e)));
                }
            }
        }

        let referrer_url = if referrer == "." {
            let cwd = std::env::current_dir()
                .context("failed to get current directory")?;
            ModuleSpecifier::from_file_path(&cwd).unwrap()
        } else {
            ModuleSpecifier::parse(referrer)
                .or_else(|_| {
                    ModuleSpecifier::from_file_path(PathBuf::from(referrer))
                        .map_err(|_| anyhow::anyhow!("failed to parse referrer: {}", referrer))
                })
                .with_context(|| format!("failed to parse referrer: {}", referrer))?
        };

        let resolved = referrer_url.join(specifier)
            .with_context(|| format!("failed to join specifier {} with referrer {}", specifier, referrer))?;

        // TypeScript-style extension fallback for imports from TS files: an
        // extensionless './x' or a './x.js' convention specifier may point at
        // a '.ts' sibling on disk. Only tried when the literal path is absent,
        // so existing JS behavior is untouched. .tsx referrers behave like
        // TypeScript and also allow '.tsx' siblings.
        if is_typescript_path(referrer) || referrer.ends_with(".tsx") {
            if let Ok(joined_path) = resolved.to_file_path() {
                // Also engage for directory targets ("./pkg" -> "./pkg/index.*")
                if !joined_path.is_file() {
                    let is_tsx = referrer.ends_with(".tsx");
                    let mut candidate_specs: Vec<String> = Vec::new();
                    if let Some(ts_style) = specifier.strip_suffix(".js") {
                        candidate_specs.push(format!("{}.ts", ts_style));
                        if is_tsx {
                            candidate_specs.push(format!("{}.tsx", ts_style));
                        }
                    }
                    // TS source siblings first (TS-style resolution), then
                    // plain JS siblings so an extensionless import from a
                    // .ts/.tsx file resolves to a compiled-adjacent
                    // .js/.mjs/.cjs neighbour or a directory index, per
                    // bundler-mode rules.
                    candidate_specs.push(format!("{}.ts", specifier));
                    if is_tsx {
                        candidate_specs.push(format!("{}.tsx", specifier));
                    }
                    candidate_specs.push(format!("{}.js", specifier));
                    candidate_specs.push(format!("{}.mjs", specifier));
                    candidate_specs.push(format!("{}.cjs", specifier));
                    let stem = specifier.trim_end_matches('/');
                    candidate_specs.push(format!("{}/index.ts", stem));
                    if is_tsx {
                        candidate_specs.push(format!("{}/index.tsx", stem));
                    }
                    candidate_specs.push(format!("{}/index.js", stem));
                    candidate_specs.push(format!("{}/index.mjs", stem));
                    candidate_specs.push(format!("{}/index.cjs", stem));
                    for cand in candidate_specs {
                        if let Ok(u) = referrer_url.join(&cand) {
                            if u.to_file_path().map(|p| p.is_file()).unwrap_or(false) {
                                return Ok(u);
                            }
                        }
                    }
                }
            }
        }

        Ok(resolved)
    }
}

impl ModuleLoader for HoneyModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, deno_error::JsErrorBox> {
        self.resolve_anyhow(specifier, referrer, kind)
            .map_err(|e| deno_error::JsErrorBox::generic(format!("{e:#}")))
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let spec_str = module_specifier.as_str();

        // node:* and bare builtin names: bridge ESM imports to our CJS builtins
        if spec_str.starts_with("node:") || is_builtin_name(spec_str) {
            let name = spec_str.strip_prefix("node:").unwrap_or(spec_str);
            let mut code = format!(
                "const m = globalThis.require('{}');\nexport default m;\n",
                name
            );
            for export_name in builtin_export_list(name) {
                code.push_str(&format!("export const {} = m.{};\n", export_name, export_name));
            }
            let module_source = ModuleSource::new(
                ModuleType::JavaScript,
                ModuleSourceCode::String(ModuleCodeString::from(code)),
                module_specifier,
                None,
            );
            return ModuleLoadResponse::Sync(Ok(module_source));
        }

        // Strip any query/fragment (e.g. the ?mtime= cache-buster eslint appends
        // to config-file imports) before converting to a filesystem path.
        let file_spec = ModuleSpecifier::parse(spec_str)
            .or_else(|_| module_specifier.join("").map(|_| module_specifier.clone()))
            .unwrap_or_else(|_| module_specifier.clone());
        let stripped = if spec_str.contains('?') || spec_str.contains('#') {
            let no_q = spec_str.split('?').next().unwrap_or(spec_str);
            let no_f = no_q.split('#').next().unwrap_or(no_q);
            ModuleSpecifier::parse(no_f).unwrap_or(module_specifier.clone())
        } else {
            file_spec
        };
        let path = match stripped.to_file_path() {
            Ok(p) => p,
            Err(_) => {
                return ModuleLoadResponse::Sync(Err(deno_error::JsErrorBox::generic(format!(
                    "failed to convert module specifier to path: {}",
                    spec_str
                ))));
            }
        };
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                return ModuleLoadResponse::Sync(Err(deno_error::JsErrorBox::generic(format!(
                    "failed to read module {}: {}",
                    path.display(),
                    e
                ))));
            }
        };

        // TypeScript modules imported from ESM code are transpiled to JS
        // before ESM/CJS wrapping.
        let source = match transpile_if_typescript(&path.to_string_lossy(), &source) {
            Ok(s) => s,
            Err(e) => {
                return ModuleLoadResponse::Sync(Err(deno_error::JsErrorBox::generic(format!(
                    "{e:#}"
                ))))
            }
        };

        let wrapped_source = wrap_commonjs_if_needed(&strip_shebang(&source), &path);

        let module_code = ModuleCodeString::from(wrapped_source);
        
        let module_source = ModuleSource::new(
            ModuleType::JavaScript,
            ModuleSourceCode::String(module_code),
            module_specifier,
            None,
        );
        
        ModuleLoadResponse::Sync(Ok(module_source))
    }
}

fn strip_shebang(source: &str) -> String {
    if let Some(rest) = source.strip_prefix("#!") {
        match rest.find('\n') {
            Some(idx) => rest[idx..].to_string(),
            None => String::new(),
        }
    } else {
        source.to_string()
    }
}

fn wrap_commonjs_if_needed(source: &str, path: &Path) -> String {
    // Genuinely ESM files load as-is.
    let is_mjs = path.extension().map(|e| e == "mjs").unwrap_or(false);
    if is_mjs || looks_like_esm(source) {
        return source.to_string();
    }

    // CommonJS: evaluate through the require machinery (which provides
    // module/exports and a correctly-anchored require) and expose both the
    // module.exports as default and, where resolvable, named exports.
    let file_path = path.to_string_lossy().replace('\\', "\\\\").replace('\'', "\\'");
    format!(
        "const __honey_cjs = globalThis.require('{}');\nexport default __honey_cjs;\n",
        file_path
    )
}

fn global_object_middleware(
    scope: &mut v8::PinScope,
    global: v8::Local<v8::Object>,
) {
    // Inject require function (with .resolve)
    let require_fn = v8::Function::new(scope, require_callback).unwrap();
    let resolve_fn = v8::Function::new(scope, require_resolve_callback).unwrap();
    let resolve_key = v8::String::new(scope, "resolve").unwrap();
    require_fn.set(scope, resolve_key.into(), resolve_fn.into());
    let require_key = v8::String::new(scope, "require").unwrap();
    global.set(scope, require_key.into(), require_fn.into());

    // Hook for per-module requires to re-anchor relative resolution to their
    // own directory (deferred requires would otherwise use a stale thread-local).
    let set_dir_fn = v8::Function::new(scope, honey_set_require_dir_callback).unwrap();
    let set_dir_key = v8::String::new(scope, "__honeySetRequireDir").unwrap();
    global.set(scope, set_dir_key.into(), set_dir_fn.into());

    // Inject/override console. Newer deno_core ships a built-in console on
    // the snapshot context whose console.error prints to *stdout* (its own
    // fd quirk), and the old is-absent guard never fired. Always re-point
    // the fd-relevant methods at honey's fns: log/info -> fd 1,
    // warn/error -> fd 2 (node semantics), so capture_stdio tagging and
    // stderr-interleaving behave like node/bun.
    let console_key = v8::String::new(scope, "console").unwrap();
    let console = if let Some(existing) = global.get(scope, console_key.into()) {
        existing.to_object(scope).unwrap_or_else(|| v8::Object::new(scope))
    } else {
        let c = v8::Object::new(scope);
        global.set(scope, console_key.into(), c.into());
        c
    };
    let log_fn = v8::Function::new(scope, console_log).unwrap();
    for name in ["log", "info"] {
        let k = v8::String::new(scope, name).unwrap();
        console.set(scope, k.into(), log_fn.into());
    }
    let error_fn = v8::Function::new(scope, console_error).unwrap();
    for name in ["warn", "error"] {
        let k = v8::String::new(scope, name).unwrap();
        console.set(scope, k.into(), error_fn.into());
    }

    // Inject process
    let process = v8::Object::new(scope);
    setup_process_object(scope, &process);
    let process_key = v8::String::new(scope, "process").unwrap();
    global.set(scope, process_key.into(), process.into());

    // Inject Buffer, timers, misc globals
    let buffer_fn = v8::Function::new(scope, buffer_from_callback).unwrap();
    let from_key = v8::String::new(scope, "from").unwrap();
    let buffer_from_inner = v8::Function::new(scope, buffer_from_callback).unwrap();
    buffer_fn.set(scope, from_key.into(), buffer_from_inner.into());
    let is_buffer_fn = v8::Function::new(scope, buffer_is_buffer_callback).unwrap();
    let is_buffer_key = v8::String::new(scope, "isBuffer").unwrap();
    buffer_fn.set(scope, is_buffer_key.into(), is_buffer_fn.into());
    let buffer_key = v8::String::new(scope, "Buffer").unwrap();
    global.set(scope, buffer_key.into(), buffer_fn.into());

    for name in ["setTimeout", "setInterval"] {
        let f = v8::Function::new(scope, set_timer_callback).unwrap();
        let k = v8::String::new(scope, name).unwrap();
        global.set(scope, k.into(), f.into());
    }
    for name in ["clearTimeout", "clearInterval"] {
        let f = v8::Function::new(scope, noop_callback).unwrap();
        let k = v8::String::new(scope, name).unwrap();
        global.set(scope, k.into(), f.into());
    }
    let qm = v8::Function::new(scope, sync_call_callback).unwrap();
    let qm_key = v8::String::new(scope, "queueMicrotask").unwrap();
    global.set(scope, qm_key.into(), qm.into());

    // TextEncoder / TextDecoder
    let decoder_ctor = v8::Function::new(scope, text_decoder_ctor_callback).unwrap();
    let decoder_key = v8::String::new(scope, "TextDecoder").unwrap();
    global.set(scope, decoder_key.into(), decoder_ctor.into());
    let encoder_ctor = v8::Function::new(scope, text_encoder_ctor_callback).unwrap();
    let encoder_key = v8::String::new(scope, "TextEncoder").unwrap();
    global.set(scope, encoder_key.into(), encoder_ctor.into());

    // URL (minimal WHATWG-compatible facade)
    if let Some(url_ctor) = eval_js_source(scope, MINIMAL_URL_JS) {
        let url_key = v8::String::new(scope, "URL").unwrap();
        global.set(scope, url_key.into(), url_ctor);
    }

    // `global` alias (legacy Node/webpack convention)
    let global_key = v8::String::new(scope, "global").unwrap();
    global.set(scope, global_key.into(), global.into());

    // structuredClone (used by @eslint/config-array validation and others).
    if let Some(clone_ctor) = eval_js_source(scope, STRUCTURED_CLONE_JS) {
        let key = v8::String::new(scope, "structuredClone").unwrap();
        global.set(scope, key.into(), clone_ctor);
    }

    // AbortController / AbortSignal (minimal)
    let _ = eval_js_source(scope, ABORT_CONTROLLER_JS);
}

const ABORT_CONTROLLER_JS: &str = r#"
(function(){
  function AbortSignal(){
    this.aborted = false;
    this.reason = undefined;
    this._listeners = {};
  }
  AbortSignal.prototype.addEventListener = function(type, fn){
    (this._listeners[type] = this._listeners[type] || []).push(fn);
  };
  AbortSignal.prototype.removeEventListener = function(type, fn){
    var a = this._listeners[type];
    if (a) {
      var i = a.indexOf(fn);
      if (i >= 0) a.splice(i, 1);
    }
  };
  AbortSignal.prototype.dispatchEvent = function(type){
    var a = (this._listeners[type] || []).slice();
    for (var i = 0; i < a.length; i++) {
      try { a[i]({ type: type, target: this }); } catch (e) {}
    }
  };
  AbortSignal.prototype.throwIfAborted = function(){
    if (this.aborted) { throw this.reason || new Error('AbortError'); }
  };
  function AbortController(){
    this.signal = new AbortSignal();
  }
  AbortController.prototype.abort = function(reason){
    if (this.signal.aborted) return;
    this.signal.aborted = true;
    this.signal.reason = reason || new Error('This operation was aborted');
    this.signal.dispatchEvent('abort');
  };
  globalThis.AbortController = AbortController;
  globalThis.AbortSignal = AbortSignal;
  return AbortController;
})()
"#;

const STRUCTURED_CLONE_JS: &str = r#"
(function(){
  function structuredClone(value, options){
    if (options && options.transferees) {
      // transfer lists unsupported; ignore
    }
    if (value === null || typeof value !== 'object') {
      return value;
    }
    if (typeof value === 'function') {
      throw new TypeError('structuredClone: functions cannot be cloned');
    }
    var seen = new Map();
    function clone(v){
      if (v === null || typeof v !== 'object') return v;
      if (v instanceof Date) return new Date(v.getTime());
      if (v instanceof RegExp) return new RegExp(v.source, v.flags);
      if (v instanceof Map) {
        if (seen.has(v)) return seen.get(v);
        var m = new Map();
        seen.set(v, m);
        v.forEach(function(val, key){ m.set(clone(key), clone(val)); });
        return m;
      }
      if (v instanceof Set) {
        if (seen.has(v)) return seen.get(v);
        var s = new Set();
        seen.set(v, s);
        v.forEach(function(item){ s.add(clone(item)); });
        return s;
      }
      if (ArrayBuffer.isView(v)) {
        return new v.constructor(v);
      }
      if (v instanceof ArrayBuffer) {
        return v.slice(0);
      }
      if (seen.has(v)) return seen.get(v);
      if (Array.isArray(v)) {
        var a = new Array(v.length);
        seen.set(v, a);
        for (var i = 0; i < v.length; i++) a[i] = clone(v[i]);
        return a;
      }
      var o = Object.create(Object.getPrototypeOf(v));
      seen.set(v, o);
      var keys = Object.keys(v);
      for (var j = 0; j < keys.length; j++) {
        o[keys[j]] = clone(v[keys[j]]);
      }
      return o;
    }
    return clone(value);
  }
  return structuredClone;
})()
"#;

fn setup_process_object<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    process: &v8::Local<'s, v8::Object>,
) {
    // argv: [honey, script, ...user args]
    let args = ARGS.with(|a| a.borrow().clone());
    let argv = v8::Array::new(scope, args.len() as i32);
    for (i, a) in args.iter().enumerate() {
        let s = v8::String::new(scope, a).unwrap();
        let idx = v8::Number::new(scope, i as f64);
        argv.set(scope, idx.into(), s.into());
    }
    let argv_key = v8::String::new(scope, "argv").unwrap();
    process.set(scope, argv_key.into(), argv.into());

    // env: real environment variables
    let env = v8::Object::new(scope);
    for (k, v) in std::env::vars() {
        let ks = v8::String::new(scope, &k).unwrap();
        let vs = v8::String::new(scope, &v).unwrap();
        env.set(scope, ks.into(), vs.into());
    }
    let env_key = v8::String::new(scope, "env").unwrap();
    process.set(scope, env_key.into(), env.into());

    // platform (string property, like Node)
    let platform = v8::String::new(scope, std::env::consts::OS).unwrap();
    let platform_key = v8::String::new(scope, "platform").unwrap();
    process.set(scope, platform_key.into(), platform.into());

    // versions
    let versions = v8::Object::new(scope);
    let node_v = v8::String::new(scope, "22.11.0").unwrap();
    let node_key = v8::String::new(scope, "node").unwrap();
    versions.set(scope, node_key.into(), node_v.into());
    let versions_key = v8::String::new(scope, "versions").unwrap();
    process.set(scope, versions_key.into(), versions.into());

    // methods
    set_fn(scope, process, "cwd", process_cwd_callback);
    set_fn(scope, process, "exit", process_exit_callback);
    set_fn(scope, process, "on", noop_callback);
    set_fn(scope, process, "once", noop_callback);
    set_fn(scope, process, "emit", noop_callback);
    set_fn(scope, process, "removeListener", noop_callback);
    let hrtime_fn = v8::Function::new(scope, process_hrtime_callback).unwrap();
    let bigint_fn = v8::Function::new(scope, process_hrtime_bigint_callback).unwrap();
    let bigint_key = v8::String::new(scope, "bigint").unwrap();
    hrtime_fn.set(scope, bigint_key.into(), bigint_fn.into());
    let hrtime_key = v8::String::new(scope, "hrtime").unwrap();
    process.set(scope, hrtime_key.into(), hrtime_fn.into());
    set_fn(scope, process, "memoryUsage", noop_callback);
    set_fn(scope, process, "umask", noop_callback);
    set_fn(scope, process, "cmd", noop_callback);

    // stdout/stderr
    let stdout = v8::Object::new(scope);
    set_fn(scope, &stdout, "write", stdout_write_callback);
    set_fn(scope, stdout, "end", noop_callback);
    let is_tty = v8::Boolean::new(scope, false);
    let is_tty_key = v8::String::new(scope, "isTTY").unwrap();
    stdout.set(scope, is_tty_key.into(), is_tty.into());
    let stdout_key = v8::String::new(scope, "stdout").unwrap();
    process.set(scope, stdout_key.into(), stdout.into());

    let stderr = v8::Object::new(scope);
    set_fn(scope, &stderr, "write", stderr_write_callback);
    set_fn(scope, stderr, "end", noop_callback);
    let is_tty_err = v8::Boolean::new(scope, false);
    stderr.set(scope, is_tty_key.into(), is_tty_err.into());
    let stderr_key = v8::String::new(scope, "stderr").unwrap();
    process.set(scope, stderr_key.into(), stderr.into());

    // exitCode (settable number)
    let exit_code = v8::Number::new(scope, 0.0);
    let exit_code_key = v8::String::new(scope, "exitCode").unwrap();
    process.set(scope, exit_code_key.into(), exit_code.into());

    // pid, arch
    let arch = v8::String::new(scope, std::env::consts::ARCH).unwrap();
    let arch_key = v8::String::new(scope, "arch").unwrap();
    process.set(scope, arch_key.into(), arch.into());
}

fn get_str(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> Option<String> {
    if v.is_undefined() || v.is_null() {
        return None;
    }
    v.to_string(scope).map(|s| s.to_rust_string_lossy(scope))
}

trait AsObjectLocal<'s> {
    fn as_obj(&self) -> v8::Local<'s, v8::Object>;
}

impl<'s> AsObjectLocal<'s> for v8::Local<'s, v8::Object> {
    fn as_obj(&self) -> v8::Local<'s, v8::Object> {
        *self
    }
}

impl<'s> AsObjectLocal<'s> for &v8::Local<'s, v8::Object> {
    fn as_obj(&self) -> v8::Local<'s, v8::Object> {
        **self
    }
}

fn set_fn<'s, O>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: O,
    name: &str,
    callback: impl v8::MapFnTo<v8::FunctionCallback>,
) where
    O: AsObjectLocal<'s>,
{
    set_function(scope, &obj.as_obj(), name, callback);
}

fn set_function(
    scope: &mut v8::PinScope,
    obj: &v8::Local<v8::Object>,
    name: &str,
    callback: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let key = v8::String::new(scope, name).unwrap();
    let func = v8::Function::new(scope, callback).unwrap();
    obj.set(scope, key.into(), func.into());
}

fn honey_set_require_dir_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut _rv: v8::ReturnValue,
) {
    if let Some(dir) = get_str(scope, args.get(0)) {
        MODULE_DIR.with(|d| *d.borrow_mut() = Path::new(&dir).to_path_buf());
    }
}

fn require_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let module_name_val = args.get(0);
    let module_name_str = module_name_val.to_string(scope).unwrap();
    let module_name = module_name_str.to_rust_string_lossy(scope);

    require_module_internal(scope, &module_name, &mut rv);
}

/// Client-side test wrapper builder (used by `honey --watch` and, on the
/// daemon side, by every test batch). Is ROOTED here so the daemon can build
/// wrappers itself — clients no longer read every test file twice per run.
pub fn snapshot_update_enabled() -> bool {
    matches!(std::env::var("HONEY_SNAPSHOT_UPDATE").as_deref(), Ok("1"))
}

/// Nearest package.json "type": "module" (mirrors the runtime's own check).
pub fn nearest_package_json_is_module(from: &Path) -> bool {
    let mut dir = from.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    loop {
        let pkg = dir.join("package.json");
        if pkg.exists() {
            if let Ok(content) = std::fs::read_to_string(&pkg) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                    return json
                        .get("type")
                        .and_then(|v| v.as_str())
                        .map(|t| t == "module")
                        .unwrap_or(false);
                }
            }
            return false;
        }
        if !dir.pop() {
            return false;
        }
    }
}

thread_local! {
    /// (path, mtime) -> ESM? cache for the test wrapper builder. Content
    /// changes bump mtime, so a static map keyed on both is safe.
    static TEST_ESM_CACHE: RefCell<HashMap<(std::path::PathBuf, Option<(i64, u32)>), bool>> = RefCell::new(HashMap::new());
}

/// The ESM/CJS flavor the engine will use for a main module (mirrors the
/// runtime's decision: .mjs/.mts, nearest package.json "type", source sniff).
pub fn main_module_is_esm(path: &std::path::Path) -> bool {
    if let Some(ext) = path.extension() {
        if ext == "mjs" || ext == "mts" {
            return true;
        }
    }
    if nearest_package_json_is_module(path) {
        return true;
    }
    let mtime = stat_mtime(&path.to_string_lossy());
    let key = (path.to_path_buf(), mtime);
    if let Some(hit) = TEST_ESM_CACHE.with(|c| c.borrow().get(&key).copied()) {
        return hit;
    }
    let esm = match std::fs::read_to_string(path) {
        Ok(source) => looks_like_esm(&source),
        Err(_) => false,
    };
    TEST_ESM_CACHE.with(|c| c.borrow_mut().insert(key, esm));
    esm
}

/// The JS wrapper loaded instead of a test file: it installs the shim, then
/// loads the actual test module, so shim and tests share one engine. The
/// load is `require(...)` (CJS wrapper) or `await import(...)` (ESM wrapper,
/// `export {}` marks it ESM for the engine's sniff).
pub fn test_preload_for(path: &std::path::Path) -> Option<String> {
    test_preload_for_with_snapshot(path, snapshot_update_enabled())
}

/// Daemon variant: the client's snapshot-update flag rides the request, since
/// the daemon's own environment doesn't carry HONEY_SNAPSHOT_UPDATE.
pub fn test_preload_for_snapshot(path: &std::path::Path, snapshot_update: bool) -> Option<String> {
    test_preload_for_with_snapshot(path, snapshot_update)
}

fn test_preload_for_with_snapshot(path: &std::path::Path, snapshot_update: bool) -> Option<String> {
    let path_str = path.to_string_lossy().into_owned();
    let esm = main_module_is_esm(path);
    let prologue = format!(
        "globalThis.__HONEY_TEST_FILE = {};\nglobalThis.__HONEY_SNAPSHOT_UPDATE = {};\n",
        serde_json::to_string(&path_str).ok()?,
        snapshot_update
    );
    let load = if esm {
        format!("await import({});\n", serde_json::to_string(&path_str).ok()?)
    } else {
        format!("require({});\n", serde_json::to_string(&path_str).ok()?)
    };
    let esm_marker = if esm { "export {};\n" } else { "" };
    // V2: the shim itself is registered once per engine (see runtime.rs's
    // TEST_SHIM / execute_pending_test_shim); the wrapper only asks it for a
    // fresh per-file state, keeping per-file parse+exec tiny.
    Some(format!(
        "{}{}\n__honeyTestBegin();\n{}{}",
        TEST_SHIM_MARKER,
        esm_marker,
        prologue,
        load
    ))
}

/// Conservative ESM sniff: look for real `import`/`export` statements anywhere
/// in the file (not strings/comments at line start mistaken; CJS files rarely
/// have lines beginning with these words).
fn looks_like_esm(source: &str) -> bool {
    for line in source.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
            continue;
        }
        if t.starts_with("export ") || t.starts_with("export{") || t.starts_with("export{") || t.starts_with("export *") || t.starts_with("export*") {
            return true;
        }
        // static import declarations; `import(...)` dynamic calls stay CJS-legal
        if (t.starts_with("import ") || t.starts_with("import{") || t.starts_with("import{") || t.starts_with("import *") || t.starts_with("import*"))
            && !t.starts_with("import(") {
            return true;
        }
    }
    false
}

/// Best-effort ESM -> CJS transform for require() of ESM files. Handles the
/// shapes produced by typical bundlers: static imports (single-line),
/// `export default X;`, `export const/let/var/function/class Name`,
/// and `export { A, B as C };` blocks. Names are re-exported at the end of
/// the module body so multi-line declarations still resolve.
pub(crate) fn transform_esm_to_cjs(source: &str) -> String {
    let mut names: Vec<(String, String)> = Vec::new(); // (exported name, local binding)
    let mut default_binding: Option<String> = None;
    let mut default_only = !source.contains("module.exports");

    let mut out: Vec<String> = Vec::new();
    for line in source.lines() {
        let t = line.trim_start();
        if t.starts_with("import ") || t.starts_with("import{") || t.starts_with("import *") || t.starts_with("import*") {
            if t.starts_with("import(") {
                out.push(line.to_string());
                continue;
            }
            // find the closing quotes/source part
            if let Some(from_pos) = t.rfind(" from ") {
                let src_part = t[from_pos + 6..].trim().trim_end_matches(';').to_string();
                let src_part = src_part.trim_matches('"').trim_matches('\'').to_string();
                let clause = t[..from_pos].trim();
                let clause = clause
                    .strip_prefix("import")
                    .unwrap_or(clause)
                    .trim();
                let mut has_default = false;
                let mut bindings: Vec<(String, String)> = Vec::new(); // (local, imported)
                if let Some(rest) = clause.strip_prefix('*') {
                    // import * as N
                    let n = rest.trim().trim_start_matches("as").trim().to_string();
                    out.push(format!("const {} = require('{}');", n, src_part));
                } else {
                    let mut named_part = clause;
                    // default binding before a comma?
                    if let Some(idx) = clause.find(',') {
                        let d = clause[..idx].trim();
                        if !d.is_empty() {
                            has_default = true;
                            out.push(format!(
                                "const {d} = (() => {{ const __m = require('{p}'); return (__m && typeof __m === 'object' && 'default' in __m) ? __m.default : __m; }})();",
                                d = d, p = src_part
                            ));
                        }
                        named_part = clause[idx + 1..].trim();
                    }
                    if let Some(inner) = named_part.strip_prefix('{') {
                        let inner = inner.trim_end_matches('}').trim();
                        if !inner.is_empty() {
                            let mut destructured = String::new();
                            for (i, pair) in inner.split(',').enumerate() {
                                let pair = pair.trim();
                                if pair.is_empty() { continue; }
                                if i > 0 { destructured.push_str(", "); }
                                destructured.push_str(pair);
                                let (local, imported) = match pair.split_once(" as ") {
                                    Some((imp, loc)) => (loc.trim().to_string(), imp.trim().to_string()),
                                    None => (pair.to_string(), pair.to_string()),
                                };
                                bindings.push((local, imported));
                            }
                            if !destructured.is_empty() {
                                out.push(format!("const {{ {} }} = require('{}');", destructured, src_part));
                            }
                        }
                    } else if !has_default && !clause.is_empty() {
                        // import X from (single default)
                        out.push(format!(
                            "const {d} = (() => {{ const __m = require('{p}'); return (__m && typeof __m === 'object' && 'default' in __m) ? __m.default : __m; }})();",
                            d = clause, p = src_part
                        ));
                    }
                }
                continue;
            }
            // side-effect import
            let src = t.strip_prefix("import").unwrap_or("").trim().trim_matches('"').trim_matches('\'');
            out.push(format!("require('{}');", src.trim_end_matches(';')));
            continue;
        }
        if t.starts_with("export default ") {
            let rest = t["export default ".len()..].trim();
            let decl_named = ["async function", "function", "class"].iter().find_map(|kw| {
                let candidate = rest.strip_prefix(kw)?.trim_start();
                let name: String = candidate.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect();
                if name.is_empty() { None } else { Some(name) }
            });
            if let Some(n) = decl_named {
                out.push(line.strip_prefix("export").unwrap_or(line).to_string());
                default_binding = Some(n);
                default_only = false;
            } else if rest.ends_with(';') || rest.is_empty() {
                out.push(line.replacen("export default ", "module.exports.default = ", 1));
                default_binding = None;
                default_only = false;
            } else {
                out.push("throw new Error(\"honey: require() of ESM: unsupported multi-line export default\");".to_string());
            }
            continue;
        }
        if t.starts_with("export {") && !t.contains(" from ") {
            // export block on one line
            if let Some(inner) = t.find('{').map(|i| &t[i + 1..]) {
                let inner = inner.split('}').next().unwrap_or("");
                out.push(format!("// esm->cjs: {}", t));
                for pair in inner.split(',') {
                    let pair = pair.trim();
                    if pair.is_empty() { continue; }
                    let (binding, name) = match pair.split_once(" as ") {
                        Some((b, n)) => (b.trim().to_string(), n.trim().to_string()),
                        None => (pair.to_string(), pair.to_string()),
                    };
                    names.push((name, binding));
                }
                default_only = false;
            }
            continue;
        }
        if t.starts_with("export {") && t.contains(" from ") {
            // re-exports: export { a, b as c } from './x'
            let from_pos = t.rfind(" from ").unwrap_or(0);
            let src_part = t[from_pos + 6..].trim().trim_end_matches(';').trim_matches('"').trim_matches('\'').to_string();
            let inner = t[t.find('{').unwrap_or(0) + 1..].split('}').next().unwrap_or("");
            for pair in inner.split(',') {
                let pair = pair.trim();
                if pair.is_empty() { continue; }
                let (imported, name) = match pair.split_once(" as ") {
                    Some((imp, n)) => (imp.trim().to_string(), n.trim().to_string()),
                    None => (pair.to_string(), pair.to_string()),
                };
                out.push(format!("module.exports[\"{}\"] = require('{}').{};", name, src_part, imported));
            }
            default_only = false;
            continue;
        }
        if t.starts_with("export * from ") {
            let src_part = t["export * from ".len()..].trim().trim_end_matches(';').trim_matches('"').trim_matches('\'').to_string();
            out.push(format!(
                "(() => {{ const __m = require('{}'); for (const k of Object.keys(__m)) {{ if (k !== 'default') module.exports[k] = __m[k]; }} }})();",
                src_part
            ));
            default_only = false;
            continue;
        }
        let stripped = if t.starts_with("export ")
            || t.starts_with("export{")
            || t.starts_with("export{")
            || t.starts_with("export *")
            || t.starts_with("export*")
        {
            if let Some(rest) = t.strip_prefix("export ") {
                let kw = ["const", "let", "var", "function", "class", "async function"].iter().find_map(|kw| {
                    rest.strip_prefix(kw).map(|r| (*kw, r))
                });
                match kw {
                    Some((kw_str, r)) => {
                        let name: String = r.trim_start().chars()
                            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                            .collect();
                        if !name.is_empty() {
                            names.push((name.clone(), name.clone()));
                            out.push(format!("{} {}", kw_str, r));
                            default_only = false;
                            continue;
                        }
                        out.push(rest.to_string());
                        continue;
                    }
                    None => {
                        out.push(rest.to_string());
                        continue;
                    }
                }
            }
            line.to_string()
        } else {
            line.to_string()
        };
        out.push(stripped);
    }

    if default_only && default_binding.is_none() && names.is_empty() {
        // Nothing to export; keep as-is so errors surface downstream
        out.push(String::new());
        return out.join("\n");
    }
    if default_only {
        // Exports are only via module.exports.default; unwrap it for CJS callers
        out.push("module.exports = module.exports.default;".to_string());
        out.push(String::new());
        return out.join("\n");
    }
    for (name, binding) in &names {
        out.push(format!("module.exports[\"{}\"] = {};", name, binding));
    }
    if let Some(d) = &default_binding {
        out.push(format!("module.exports[\"default\"] = {};", d));
    }
    out.push("module.exports.__esModule = true;".to_string());
    out.push(String::new());
    out.join("\n")
}

/// TypeScript entry points honey transpiles: .ts, .mts, .cts.
fn is_typescript_path(path: &str) -> bool {
    path.ends_with(".ts") || path.ends_with(".mts") || path.ends_with(".cts")
}

/// All entry points that go through the oxc transform: the TypeScript paths
/// above plus the JSX flavors (.tsx = TypeScript + JSX, .jsx = JavaScript +
/// JSX). JSX opts in by extension only; plain .js is never JSX-transformed.
fn is_ts_or_jsx_path(path: &str) -> bool {
    is_typescript_path(path) || path.ends_with(".tsx") || path.ends_with(".jsx")
}

thread_local! {
    /// Transpiled TS/TSX/JSX sources: absolute path -> (file mtime snapshot
    /// at transpile time, tsconfig staleness stamp, JS output). Every
    /// .ts/.mts/.cts/.tsx/.jsx file keys separately by path. Validated by
    /// mtime like the module cache, so an edited file is re-transpiled on
    /// the next evaluation — and the tsconfig stamp catches edits to the
    /// nearest tsconfig.json, which flips JSX settings without touching any
    /// source file's mtime. The stamp is the (path, mtime) of the nearest
    /// tsconfig.json, or None when there is none (creation/removal/move of
    /// one changes the stamp, so it invalidates too).
    static TS_TRANSPILE_CACHE: RefCell<HashMap<String, ((i64, u32), Option<(String, (i64, u32))>, String)>> = RefCell::new(HashMap::new());
}

/// Current tsconfig staleness stamp for a source path: the (path, mtime) of
/// the nearest tsconfig.json, or None if there is none in any ancestor.
fn tsconfig_cache_stamp(path_str: &str) -> Option<(String, (i64, u32))> {
    nearest_tsconfig_path(Path::new(path_str)).and_then(|cfg| {
        let p = cfg.to_string_lossy().to_string();
        stat_mtime(&p).map(|m| (p, m))
    })
}

/// oxc SourceType for a transpilable path. TypeScript extensions: .ts/.mts
/// are modules, .cts is a script; .tsx is a TS module with JSX enabled and
/// .jsx a JS module with JSX enabled (from_path sets LanguageVariant::Jsx for
/// both, JSX opt-in by extension). Unknown extensions fall back to plain
/// TypeScript.
fn ts_source_type(path: &Path) -> oxc_span::SourceType {
    oxc_span::SourceType::from_path(path).unwrap_or_else(|_| oxc_span::SourceType::ts())
}

/// JSX settings from the nearest tsconfig.json, for .tsx/.jsx sources:
/// (classic runtime?, jsxImportSource override). "jsx": "react" selects the
/// classic runtime (React.createElement / React.Fragment); anything else
/// keeps the automatic runtime, whose import source "jsxImportSource"
/// overrides (default "react"). Stops at the first tsconfig.json found, like
/// tsc; no tsconfig means the defaults.
fn nearest_tsconfig_jsx_config(from: &Path) -> (bool, Option<String>) {
    if let Some(cfg) = nearest_tsconfig_path(from) {
        if let Ok(content) = std::fs::read_to_string(&cfg) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                let compiler_options = json.get("compilerOptions");
                let classic = compiler_options
                    .and_then(|o| o.get("jsx"))
                    .and_then(|v| v.as_str())
                    .map(|j| j == "react")
                    .unwrap_or(false);
                let import_source = if classic {
                    None
                } else {
                    compiler_options
                        .and_then(|o| o.get("jsxImportSource"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                };
                return (classic, import_source);
            }
        }
        // Unreadable/unparsable tsconfig: degrade to the defaults, like tsc
        // degrades to its own defaults on malformed config.
        return (false, None);
    }
    (false, None)
}

/// Nearest tsconfig.json walking up from a source path (tsc's search:
/// stops at the first one found per directory, then climbs). None = no
/// tsconfig in any ancestor.
fn nearest_tsconfig_path(from: &Path) -> Option<PathBuf> {
    let mut dir = from.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    loop {
        let cfg = dir.join("tsconfig.json");
        if cfg.exists() {
            return Some(cfg);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// 1-based line/column of a byte offset.
fn span_to_line_col(source: &str, offset: u32) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, ch) in source.char_indices() {
        if i as u32 >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Format a pre-extracted oxc diagnostic message as `file:line:col: msg`
/// (when a labelled span offset is known) or `file: msg`.
fn format_oxc_diag(source: &str, path: &Path, msg: String, offset: Option<usize>) -> String {
    match offset {
        Some(off) => {
            let (line, col) = span_to_line_col(source, off as u32);
            format!("{}:{}:{}: {}", path.display(), line, col, msg)
        }
        None => format!("{}: {}", path.display(), msg),
    }
}

/// Transpile a TypeScript source to JavaScript via the Oxc parser +
/// transformer: type/interface/generics/type-alias stripping plus TS-only
/// runtime constructs (enums, namespaces, class fields, etc.). No
/// type-checking (use tsc, see SPEC.md). Parse or transform errors come back
/// as Err with `file:line:col: message`.
fn transform_typescript(source: &str, path: &Path) -> std::result::Result<String, String> {
    use oxc_allocator::Allocator;
    use oxc_codegen::{Codegen, CodegenOptions};
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::SourceType;
    use oxc_transformer::{JsxRuntime, TransformOptions, Transformer};

    let source_type: SourceType = ts_source_type(path);
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, source, source_type).parse();
    if !ret.errors.is_empty() {
        return Err(ret
            .errors
            .iter()
            .map(|e| {
                let off = e.labels.as_ref().and_then(|l| l.first()).map(|l| l.offset());
                format_oxc_diag(source, path, e.to_string(), off)
            })
            .collect::<Vec<_>>()
            .join("\n"));
    }
    if ret.panicked {
        return Err(format!(
            "{}: parser panicked while recovering from a syntax error; first error: {}",
            path.display(),
            ret.errors
                .first()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        ));
    }

    let semantic = SemanticBuilder::new().build(&ret.program);
    let (symbols, scopes) = semantic.semantic.into_symbol_table_and_scope_tree();

    let mut program = ret.program;
    // Only JSX paths (.tsx/.jsx) read tsconfig for JSX settings; .ts/.mts/.cts
    // keep the default options (and their behavior) untouched.
    let mut options = TransformOptions::default();
    if source_type.is_jsx() {
        let (classic, import_source) = nearest_tsconfig_jsx_config(path);
        if classic {
            options.jsx.runtime = JsxRuntime::Classic;
        } else if let Some(src) = import_source {
            options.jsx.import_source = Some(src);
        }
    }
    let transform_ret = Transformer::new(&allocator, path, &options)
        .build_with_symbols_and_scopes(symbols, scopes, &mut program);
    if !transform_ret.errors.is_empty() {
        return Err(transform_ret
            .errors
            .iter()
            .map(|e| {
                let off = e.labels.as_ref().and_then(|l| l.first()).map(|l| l.offset());
                format_oxc_diag(source, path, e.to_string(), off)
            })
            .collect::<Vec<_>>()
            .join("\n"));
    }

    Ok(Codegen::new()
        .with_options(CodegenOptions::default())
        .build(&program)
        .code)
}

/// Transpile a TS/TSX/JSX source if the path opts in (.ts/.mts/.cts plus
/// .tsx/.jsx), using (and updating) the path+mtime transpile cache. Other
/// paths pass through unchanged. Errors are pre-formatted
/// `file:line:col: message` strings.
pub(crate) fn transpile_if_typescript(path_str: &str, source: &str) -> std::result::Result<String, String> {
    if !is_ts_or_jsx_path(path_str) {
        return Ok(source.to_string());
    }
    let mtime = stat_mtime(path_str)
        .ok_or_else(|| format!("failed to stat: {}", path_str))?;
    let cfg_stamp = tsconfig_cache_stamp(path_str);
    if let Some((recorded, recorded_cfg, out)) = TS_TRANSPILE_CACHE.with(|c| c.borrow().get(path_str).cloned()) {
        if recorded == mtime && recorded_cfg == cfg_stamp {
            return Ok(out.clone());
        }
        TS_TRANSPILE_CACHE.with(|c| c.borrow_mut().remove(path_str));
    }
    let out = transform_typescript(source, Path::new(path_str))?;
    TS_TRANSPILE_CACHE.with(|c| c.borrow_mut().insert(path_str.to_string(), (mtime, cfg_stamp, out.clone())));
    Ok(out)
}

fn require_resolve_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let module_name_val = args.get(0);
    let module_name_str = module_name_val.to_string(scope).unwrap();
    let module_name = module_name_str.to_rust_string_lossy(scope);

    let normalized = module_name.strip_prefix("node:").unwrap_or(&module_name);
    if is_builtin_name(normalized) {
        let s = v8::String::new(scope, &module_name).unwrap();
        rv.set(s.into());
        return;
    }

    match resolve_module_path(&module_name) {
        Ok(path) => {
            let s = v8::String::new(scope, &path).unwrap();
            rv.set(s.into());
        }
        Err(e) => {
            let error_msg = format!("Cannot find module '{}': {}", module_name, e);
            let msg = v8::String::new(scope, &error_msg).unwrap();
            let exception = v8::Exception::error(scope, msg);
            scope.throw_exception(exception);
        }
    }
}

const BUILTIN_NAMES: &[&str] = &[
    "path", "fs", "fs/promises", "os", "util", "url", "module", "events", "assert",
    "worker_threads", "stream", "stream/promises", "child_process", "tty", "string_decoder",
    "crypto", "zlib", "http", "https", "net", "dns", "tls", "vm", "repl",
    "constants", "trace_events", "async_hooks", "buffer",
];

fn is_builtin_name(name: &str) -> bool {
    BUILTIN_NAMES.contains(&name)
}

/// Named ESM exports for each builtin (used by the node:* import bridge).
fn builtin_export_list(name: &str) -> &'static [&'static str] {
    match name {
        "path" => &[
            "join", "resolve", "dirname", "basename", "extname", "relative",
            "isAbsolute", "normalize", "sep", "delimiter", "posix",
        ],
        "fs" => &[
            "readFileSync", "writeFileSync", "existsSync", "statSync", "lstatSync",
            "readdirSync", "mkdirSync", "realpathSync", "copyFileSync", "unlinkSync",
            "appendFileSync", "promises", "constants",
        ],
        "fs/promises" => &[
            "readFile", "writeFile", "stat", "lstat", "readdir", "mkdir", "access",
            "rm", "unlink", "copyFile", "realpath", "appendFile",
        ],
        "os" => &["platform", "arch", "homedir", "tmpdir", "hostname", "type", "release", "EOL"],
        "util" => &["inspect", "format", "inherits", "promisify", "deprecate", "debuglog", "types", "styleText", "stripVTControlCharacters"],
        "url" => &["pathToFileURL", "fileURLToPath"],
        "module" => &["enableCompileCache", "disableCompileCache", "createRequire", "_resolveFilename"],
        "events" => &["EventEmitter", "once"],
        "worker_threads" => &["isMainThread", "threadId", "parentPort"],
        "assert" => &[
            "ok", "strict", "default", "equal", "deepEqual", "strictEqual", "notEqual",
            "notDeepEqual", "ifError", "fail", "throws", "doesNotThrow", "AssertionError",
        ],
        "child_process" => &["spawnSync", "execSync", "spawn", "exec", "execFile"],
        "crypto" => &["createHash", "randomBytes", "randomUUID"],
        "tty" => &["isatty"],
        "stream" | "stream/promises" | "zlib" | "http" | "https" | "net" | "dns" | "tls"
        | "vm" | "repl" | "string_decoder" | "trace_events" | "async_hooks" => &[
            "Readable", "Writable", "Duplex", "Transform", "PassThrough", "pipeline",
            "finished",
        ],
        "buffer" => &["from", "isBuffer"],
        "constants" => &[],
        _ => &[],
    }
}

// ---------- warm CJS code cache ----------
//
// V8's serialized bytecode for CJS module bodies. In the daemon the same
// file is often evaluated many times (watch loops, benchmark batteries,
// CLI dev cycles); skipping the full parse+bytecode-compile saves
// ~30-80µs per warm run. Key is a hash of the wrapped source (path +
// contents + require dir), so an edited file gets a fresh entry for
// free. Entries are plain bytes and only valid for the current V8
// version + flags (checked via CachedDataVersionTag).

static CJS_CODE_CACHE: std::sync::OnceLock<
    std::sync::Mutex<HashMap<u64, (u32, Vec<u8>)>>,
> = std::sync::OnceLock::new();
const CJS_CODE_CACHE_MAX: usize = 2048;

fn cjs_code_cache() -> &'static std::sync::Mutex<HashMap<u64, (u32, Vec<u8>)>> {
    CJS_CODE_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn cjs_cache_key(wrapped: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    wrapped.hash(&mut h);
    h.finish()
}

fn cjs_cache_get(key: u64) -> Option<Vec<u8>> {
    let map = cjs_code_cache().lock().ok()?;
    let (tag, bytes) = map.get(&key)?;
    if *tag != v8::script_compiler::cached_data_version_tag() {
        return None;
    }
    Some(bytes.clone())
}

fn cjs_cache_put(key: u64, bytes: Vec<u8>) {
    let tag = v8::script_compiler::cached_data_version_tag();
    if let Ok(mut map) = cjs_code_cache().lock() {
        if map.len() >= CJS_CODE_CACHE_MAX {
            map.clear(); // simple bound; keys are content-hashed so re-creating is safe
        }
        map.insert(key, (tag, bytes));
    }
}

fn require_module_internal(
    scope: &mut v8::PinScope,
    module_name: &str,
    rv: &mut v8::ReturnValue,
) {
    if phase_trace_enabled() {
        eprintln!(
            "[trace] require '{}' (MODULE_DIR={:?})",
            module_name,
            MODULE_DIR.with(|d| d.borrow().display().to_string())
        );
    }
    // Normalize node: prefix for builtins
    let normalized = module_name.strip_prefix("node:").unwrap_or(module_name);

    // Warm fast path: a bare package name or a relative/absolute specifier
    // resolved in this directory during an earlier run, whose target file
    // hasn't changed, skips re-resolution (and re-evaluation if cached).
    let is_builtin_req = module_name.starts_with("node:")
        || is_builtin_name(module_name.strip_prefix("node:").unwrap_or(module_name));
    if !is_builtin_req {
        let key = format!(
            "{}\u{0}{}",
            module_name,
            MODULE_DIR.with(|d| d.borrow().display().to_string())
        );
        let recorded = MODULE_RESOLUTIONS.with(|m| m.borrow().get(&key).cloned());
        if let Some((path, recorded_mtime)) = recorded {
            if stat_mtime(&path) == Some(recorded_mtime) {
                if let Some(cached_global) = MODULE_CACHE.with(|c| c.borrow().get(&path).cloned()) {
                    rv.set(v8::Local::new(scope, &cached_global));
                    return;
                }
            } else {
                // The file changed since the last run: drop both the stale
                // cached module and the outdated resolution record so the
                // fall-through re-resolves and re-evaluates from disk.
                MODULE_CACHE.with(|c| c.borrow_mut().remove(&path));
                MODULE_RESOLUTIONS.with(|m| m.borrow_mut().remove(&key));
            }
        }
    }

    // Check cache (keyed by the specifier as given)
    if let Some(cached_global) = MODULE_CACHE.with(|cache| cache.borrow().get(module_name).cloned()) {
        rv.set(v8::Local::new(scope, &cached_global));
        return;
    }

    // Builtins
    if module_name.starts_with("node:") || is_builtin_name(normalized) {
        match get_builtin_module(scope, normalized) {
            Some(builtin) => {
                MODULE_CACHE.with(|cache| {
                    cache.borrow_mut().insert(module_name.to_string(), v8::Global::new(scope, builtin));
                });
                rv.set(builtin);
            }
            None => {
                let error_msg = format!("builtin module not implemented: {}", module_name);
                let msg = v8::String::new(scope, &error_msg).unwrap();
                let exception = v8::Exception::error(scope, msg);
                scope.throw_exception(exception);
            }
        }
        return;
    }

    // Resolve path
    let resolved = match resolve_module_path(module_name) {
        Ok(path) => path,
        Err(e) => {
            let error_msg = format!("require failed: {}", e);
            let msg = v8::String::new(scope, &error_msg).unwrap();
            let exception = v8::Exception::error(scope, msg);
            scope.throw_exception(exception);
            return;
        }
    };

    // Record the specifier -> file mapping so warm runs can validate and
    // reuse this cache entry (see warm_prepare). Bare names, relative and
    // absolute specifiers all participate; the key includes MODULE_DIR.
    if !module_name.contains("://") && !is_builtin_name(module_name) {
        let key = format!(
            "{}\u{0}{}",
            module_name,
            MODULE_DIR.with(|d| d.borrow().display().to_string())
        );
        MODULE_RESOLUTIONS.with(|m| {
            if let Some(mt) = stat_mtime(&resolved) {
                m.borrow_mut().insert(key, (resolved.clone(), mt));
            }
        });
    }

    // Cache check keyed by resolved path (cross-specifier dedup)
    if let Some(cached_global) = MODULE_CACHE.with(|cache| cache.borrow().get(&resolved).cloned()) {
        rv.set(v8::Local::new(scope, &cached_global));
        return;
    }

    // JSON modules
    if resolved.ends_with(".json") {
        let source = match std::fs::read_to_string(&resolved) {
            Ok(s) => s,
            Err(e) => {
                let error_msg = format!("failed to read module: {}", e);
                let msg = v8::String::new(scope, &error_msg).unwrap();
                let exception = v8::Exception::error(scope, msg);
                scope.throw_exception(exception);
                return;
            }
        };
        let json_str = match v8::String::new(scope, &source) {
            Some(s) => s,
            None => return,
        };
        match v8::json::parse(scope, json_str) {
            Some(value) => {
                let g = v8::Global::new(scope, value);
                cache_module_with_mtime(&resolved, g);
                rv.set(value);
            }
            None => {
                let msg = v8::String::new(scope, "failed to parse JSON module").unwrap();
                let exception = v8::Exception::error(scope, msg);
                scope.throw_exception(exception);
            }
        }
        return;
    }

    let source = match std::fs::read_to_string(&resolved) {
        Ok(s) => s,
        Err(e) => {
            let error_msg = format!("failed to read module: {}", e);
            let msg = v8::String::new(scope, &error_msg).unwrap();
            let exception = v8::Exception::error(scope, msg);
            scope.throw_exception(exception);
            return;
        }
    };

    // TS/TSX/JSX modules are transpiled to JS before ESM/CJS handling, so a
    // required .ts/.mts/.cts/.tsx/.jsx evaluates like its JavaScript
    // equivalent.
    let t_transpile = std::time::Instant::now();
    let source = match transpile_if_typescript(&resolved, &source) {
        Ok(s) => s,
        Err(e) => {
            if phase_trace_enabled() {
                phase_trace("cjs_transpile", t_transpile.elapsed());
            }
            let error_msg = format!("failed to compile module: {}", e);
            let msg = v8::String::new(scope, &error_msg).unwrap();
            let exception = v8::Exception::error(scope, msg);
            scope.throw_exception(exception);
            return;
        }
    };
    if phase_trace_enabled() {
        phase_trace("cjs_transpile", t_transpile.elapsed());
    }

    // If the file is an ES module, transform it to CJS so require() also works
    // on ESM-only packages (mirrors Node's require(esm) interop).
    let source = if resolved.ends_with(".mjs")
        || nearest_package_json_is_module(Path::new(&resolved))
        || looks_like_esm(&source)
    {
        transform_esm_to_cjs(&source)
    } else {
        source
    };

    // Set up module context
    let module_dir = Path::new(&resolved)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();

    let previous_dir = MODULE_DIR.with(|d| d.replace(module_dir.clone()));
    if phase_trace_enabled() {
        eprintln!("[trace]   eval '{}' MODULE_DIR {:?} -> {:?}", resolved, previous_dir.display().to_string(), module_dir.display().to_string());
    }

    // Create module and exports objects
    let exports = v8::Object::new(scope);
    let module = v8::Object::new(scope);
    let exports_key = v8::String::new(scope, "exports").unwrap();
    module.set(scope, exports_key.into(), exports.into());

    let filename_key = v8::String::new(scope, "filename").unwrap();
    let file_str = v8::String::new(scope, &resolved).unwrap();
    module.set(scope, filename_key.into(), file_str.into());

    let context = scope.get_current_context();
    let global = context.global(scope);

    let module_key = v8::String::new(scope, "module").unwrap();
    let undefined = v8::undefined(scope);
    // Save previous module and exports from global
    let prev_module_val = global.get(scope, module_key.into());
    
    global.set(scope, module_key.into(), module.into());

    // Wrap in CommonJS function wrapper
    let file_path = resolved.clone();
    let dir_path = module_dir.to_string_lossy().to_string();
    let source = strip_shebang(&source);
    let wrapped = format!(r#"
(function(gRequire, setDir) {{
var require = (function(g, dir) {{
  var r = function(spec) {{ setDir(dir); return g(spec); }};
  r.resolve = function(spec) {{ setDir(dir); return g.resolve(spec); }};
  r.cache = g.cache;
  return r;
}})(gRequire, "{dir_path}");
return (function(exports, require, module, __filename, __dirname) {{
{source}
}})(module.exports, require, module, "{file_path}", "{dir_path}");
}})(require, globalThis.__honeySetRequireDir);
"#);

    // Pre-register the (partial) exports object to break require cycles
    {
        let exports_value: v8::Local<v8::Value> = exports.into();
        let exports_global = v8::Global::new(scope, exports_value);
        cache_module_with_mtime(&resolved, exports_global);
    }

    let t_cjs_compile = std::time::Instant::now();
    let code = match v8::String::new(scope, &wrapped) {
        Some(c) => c,
        None => {
            let msg = v8::String::new(scope, "failed to create source").unwrap();
            let exception = v8::Exception::error(scope, msg);
            scope.throw_exception(exception);
            global.set(scope, module_key.into(), prev_module_val.unwrap_or(undefined.into()));
            MODULE_DIR.with(|d| d.replace(previous_dir));
            return;
        }
    };
    
    let name_str = v8::String::new(scope, &resolved).unwrap();
    let origin = v8::ScriptOrigin::new(scope, name_str.into(), 0, 0, false, 0, None, false, false, false, None);
    let script = {
        // A compile (parse) failure must reach the enclosing TryCatch: without
        // an inner TryCatch here the exception is never observed by the
        // require machinery and the module silently "succeeds".
        v8::tc_scope!(let tc_compile, scope);
        // Warm path: reuse V8's serialized bytecode for this exact module
        // body (same text hash) when we have it; cold path: compile fresh
        // and store the produced cache for next time.
        let cache_key = cjs_cache_key(&wrapped);
        let cached = cjs_cache_get(cache_key);
        let mut script: Option<v8::Local<v8::Script>> = if let Some(bytes) = &cached {
            let mut source = v8::script_compiler::Source::new_with_cached_data(
                code,
                Some(&origin),
                v8::script_compiler::CachedData::new(bytes),
            );
            v8::script_compiler::compile(
                tc_compile,
                &mut source,
                v8::script_compiler::CompileOptions::ConsumeCodeCache,
                v8::script_compiler::NoCacheReason::NoReason,
            )
        } else {
            let mut source = v8::script_compiler::Source::new(code, Some(&origin));
            v8::script_compiler::compile(
                tc_compile,
                &mut source,
                v8::script_compiler::CompileOptions::NoCompileOptions,
                v8::script_compiler::NoCacheReason::NoReason,
            )
        };
        if script.is_none() && cached.is_some() {
            // Stale/rejected serialized data: fall back to a clean compile.
            let mut fresh = v8::script_compiler::Source::new(code, Some(&origin));
            script = v8::script_compiler::compile(
                tc_compile,
                &mut fresh,
                v8::script_compiler::CompileOptions::NoCompileOptions,
                v8::script_compiler::NoCacheReason::NoReason,
            );
        }
        match script {
            Some(s) => {
                if cached.is_none() {
                    // Produce the bytecode blob once, for every future warm
                    // run of this module body in this daemon.
                    let ub = s.get_unbound_script(tc_compile);
                    if let Some(cd) = ub.create_code_cache() {
                        cjs_cache_put(cache_key, cd.to_vec());
                    }
                }
                s
            }
            None => {
                global.set(tc_compile, module_key.into(), prev_module_val.unwrap_or(undefined.into()));
                MODULE_DIR.with(|d| d.replace(previous_dir.clone()));
                tc_compile.rethrow();
                return;
            }
        }
    };
    
    let run_failure: Option<v8::Global<v8::Value>> = {
        v8::tc_scope!(let tc, scope);
        let r = script.run(tc);
        if phase_trace_enabled() {
            phase_trace("cjs_compile+run", t_cjs_compile.elapsed());
        }
        if r.is_none() {
            // Report the real root-cause exception to stderr with its stack
            let mut parts: Vec<String> = Vec::new();
            let mut exc_global: Option<v8::Global<v8::Value>> = None;
            if let Some(exc) = tc.exception() {
                if let Ok(obj) = v8::Local::<v8::Object>::try_from(exc) {
                    let stack_key = v8::String::new(tc, "stack").unwrap();
                    if let Some(stack) = obj.get(tc, stack_key.into()) {
                        if let Some(s) = get_str(tc, stack) {
                            parts.push(s);
                        }
                    }
                }
                if parts.is_empty() {
                    if let Some(s) = get_str(tc, exc) {
                        parts.push(s);
                    }
                }
                record_uncaught(&parts.join("\n"));
                if parts.iter().all(|p| !p.contains("__honey_exit__")) {
                    eprintln!("honey: module failed: {} -> {}", resolved, parts.join("\n"));
                }
                exc_global = Some(v8::Global::new(tc, exc));
            }
            exc_global
        } else {
            None
        }
    };
    if let Some(exc_global) = run_failure {
        let exc = v8::Local::new(scope, exc_global);
        scope.throw_exception(exc);
        global.set(scope, module_key.into(), prev_module_val.unwrap_or(undefined.into()));
        MODULE_DIR.with(|d| d.replace(previous_dir));
        return;
    }

    // Get exports
    let module_val = global.get(scope, module_key.into()).unwrap();
    let module_obj: v8::Local<v8::Object> = module_val.try_into().unwrap();

    let exports_key = v8::String::new(scope, "exports").unwrap();
    let exports_val = module_obj.get(scope, exports_key.into()).unwrap();

    cache_module_with_mtime(&resolved, v8::Global::new(scope, exports_val));

    // Restore previous module
    global.set(scope, module_key.into(), prev_module_val.unwrap_or(undefined.into()));
    MODULE_DIR.with(|d| d.replace(previous_dir));

    rv.set(exports_val);
}

fn resolve_module_path(module_name: &str) -> Result<String> {
    resolve_module_path_prefer(module_name, false)
}

fn resolve_module_path_prefer(module_name: &str, prefer_import: bool) -> Result<String> {
    let cwd = std::env::current_dir()
        .context("failed to get current directory")?;

    if module_name.starts_with('.') || module_name.starts_with('/') {
        let path = Path::new(module_name);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            let base_dir = MODULE_DIR.with(|d| d.borrow().clone());
            if base_dir.as_os_str().is_empty() {
                cwd.join(module_name)
            } else {
                base_dir.join(module_name)
            }
        };
        return resolve_with_extensions(&resolved);
    }

    // npm package: find the nearest .cinm / node_modules walking up from the
    // current module's directory (falls back to cwd). Both stores are
    // consulted per ancestor: a package absent from the honey store (.cinm,
    // versioned layout) still resolves from a plain npm node_modules tree.
    let roots = module_root_candidates(&cwd);
    let parts: Vec<&str> = module_name.splitn(2, '/').collect();
    let (pkg_name, subpath) = if parts.len() > 1 { (parts[0], parts[1]) } else { (module_name, "") };

    let (dir_prefix, subpath) = if pkg_name.starts_with('@') {
        // Scoped: "@scope/pkg" -> dir prefix "@scope+pkg@" (cinm encodes / as +)
        if parts.len() < 2 || parts[1].is_empty() {
            anyhow::bail!("invalid scoped package specifier: {}", module_name);
        }
        // pkg_name is "@scope"; subpath so far begins with "pkg"
        match subpath.split_once('/') {
            Some((pkg_base, real_sub)) => (
                format!("{}+{}@", pkg_name, pkg_base),
                real_sub.to_string(),
            ),
            None => (
                format!("{}+{}@", pkg_name, subpath),
                String::new(),
            ),
        }
    } else {
        (format!("{}@", pkg_name), subpath.to_string())
    };

    let mut last_root = roots.last().cloned().unwrap_or_else(|| cwd.to_path_buf());
    for root in &roots {
        last_root = root.clone();
        // honey install layout: versioned dirs (`pkg@<version>`, scoped
        // encoded with '+') — matched by the `dir_prefix` scan. Fall back to
        // the standard `node_modules/<pkg>` (or `node_modules/@scope/<pkg>`)
        // layout written by npm so stock package trees resolve through the
        // same package.json exports / main / index handling below.
        let pkg_dir = match std::fs::read_dir(root).ok().and_then(|entries| {
            let mut found = None;
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(&dir_prefix) {
                    found = Some(entry.path());
                    break;
                }
            }
            found
        }) {
            Some(p) => p,
            None => {
                // Standard layout: the bare package's literal directory name.
                let standard_dir = if pkg_name.starts_with('@') {
                    // "@scope/pkg/…" -> node_modules/@scope/<pkg> (the second
                    // component of the original specifier; subpath was already
                    // remapped past it above)
                    let pkg_base = parts[1].split('/').next().unwrap_or("");
                    if pkg_base.is_empty() {
                        anyhow::bail!("invalid scoped package specifier: {}", module_name);
                    }
                    root.join(pkg_name).join(pkg_base)
                } else {
                    root.join(pkg_name)
                };
                if standard_dir.is_dir() {
                    standard_dir
                } else {
                    continue;
                }
            }
        };

        let exports_entry = {
            let package_json = pkg_dir.join("package.json");
            let mut entry = None;
            if package_json.exists() {
                if let Ok(content) = std::fs::read_to_string(&package_json) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(exports) = json.get("exports") {
                            let key = if subpath.is_empty() {
                                ".".to_string()
                            } else {
                                format!("./{}", subpath)
                            };
                            entry = resolve_exports_entry(exports, &key, prefer_import);
                        }
                    }
                }
            }
            entry
        };

        let module_path = match exports_entry {
            Some(rel) => pkg_dir.join(rel.trim_start_matches("./")),
            None if subpath.is_empty() => {
                // Fall back to package.json "main", then index.js
                let package_json = pkg_dir.join("package.json");
                let mut main = None;
                if package_json.exists() {
                    if let Ok(content) = std::fs::read_to_string(&package_json) {
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                            if let Some(m) = json.get("main").and_then(|v| v.as_str()) {
                                main = Some(m.to_string());
                            }
                        }
                    }
                }
                match main {
                    Some(m) => pkg_dir.join(m),
                    None => pkg_dir.join("index.js"),
                }
            }
            None => pkg_dir.join(&subpath),
        };

        if let Ok(resolved) = resolve_with_extensions(&module_path) {
            return Ok(resolved);
        }
        // This store had the package but no resolvable entry (e.g. an
        // exports/subpath combination only present elsewhere); keep looking.
    }

    anyhow::bail!(
        "package not found: {} (searched {})",
        pkg_name,
        last_root.display()
    )
}

/// Look up `key` in a package.json `exports` map, preferring the "require"
/// condition over "default". Returns the relative target path.
fn resolve_exports_entry(
    exports: &serde_json::Value,
    key: &str,
    prefer_import: bool,
) -> Option<String> {
    if exports.is_string() {
        // Simple form: exports -> "./entry.js" (only valid for ".")
        if key == "." {
            return Some(exports.as_str().unwrap_or_default().to_string());
        }
        return None;
    }
    let obj = exports.as_object()?;
    if let Some(target) = obj.get(key) {
        return pick_export_target(target, prefer_import);
    }
    if key == "." {
        // Bare conditions form: exports is { require: ..., import: ... }
        return pick_export_target(exports, prefer_import);
    }
    // Some packages list subpaths without the "./" prefix
    let alt_key = key.trim_start_matches("./");
    if obj.contains_key(alt_key) {
        return pick_export_target(&obj[alt_key], prefer_import);
    }
    None
}

fn pick_export_target(target: &serde_json::Value, prefer_import: bool) -> Option<String> {
    if target.is_string() {
        return Some(target.as_str().unwrap_or_default().to_string());
    }
    let obj = target.as_object()?;
    // require() must never use the "import" condition (that would hand the
    // ESM entry to the CJS pipeline); import() tolerates "default" CJS.
    let conditions: [&str; 2] = if prefer_import {
        ["import", "default"]
    } else {
        ["require", "default"]
    };
    for cond in conditions {
        if let Some(t) = obj.get(cond) {
            let picked = pick_export_target(t, prefer_import);
            if picked.is_some() {
                return picked;
            }
        }
    }
    None
}

fn find_modules_root(cwd: &Path) -> PathBuf {
    let start = modules_root_start(cwd);
    for ancestor in start.ancestors() {
        let cinm = ancestor.join(".cinm");
        if cinm.is_dir() {
            return cinm;
        }
        let nm = ancestor.join("node_modules");
        if nm.exists() {
            return nm;
        }
        // Stop at filesystem root
        if ancestor.parent().is_none() {
            break;
        }
    }
    start.join(".cinm")
}

/// All package-root candidates to search for bare specifiers, nearest first:
/// every ancestor dir's `.cinm` (honey install store) and `node_modules`
/// (npm's layout), innermost-out. A project can mix both stores — a package
/// missing from one must still resolve from the other.
fn module_root_candidates(cwd: &Path) -> Vec<PathBuf> {
    let start = modules_root_start(cwd);
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for ancestor in start.ancestors() {
        for store in [".cinm", "node_modules"] {
            let dir = ancestor.join(store);
            if dir.is_dir() && seen.insert(dir.clone()) {
                out.push(dir);
            }
        }
        if ancestor.parent().is_none() {
            break;
        }
    }
    if out.is_empty() {
        out.push(start.join(".cinm"));
    }
    out
}

fn modules_root_start(cwd: &Path) -> PathBuf {
    let base = MODULE_DIR.with(|d| d.borrow().clone());
    if base.as_os_str().is_empty() { cwd.to_path_buf() } else { base }
}

/// Lexically resolve `.` and `..` components. Keeps the path absolute if it
/// already is; otherwise normalizes the relative form.
fn lexically_normalize_path(p: &Path) -> PathBuf {
    let absolute = p.is_absolute();
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !parts.is_empty() {
                    parts.pop();
                } else if !absolute {
                    parts.push(std::ffi::OsString::from(".."));
                }
            }
            other => parts.push(other.as_os_str().to_os_string()),
        }
    }
    let mut out = PathBuf::new();
    if absolute {
        out.push("/");
    }
    for part in parts {
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

fn resolve_with_extensions(path: &Path) -> Result<String> {
    if path.is_dir() {
        for index in ["index.js", "index.mjs", "index.cjs", "index.json", "index.ts", "index.mts", "index.cts", "index.tsx", "index.jsx"] {
            let candidate = path.join(index);
            if candidate.exists() {
                let normalized = lexically_normalize_path(&candidate);
                return Ok(normalized.to_string_lossy().to_string());
            }
        }
        anyhow::bail!("cannot resolve directory: {}", path.display());
    }

    if path.exists() {
        let normalized = lexically_normalize_path(path);
        return Ok(normalized.to_string_lossy().to_string());
    }

    for ext in ["js", "mjs", "cjs", "json", "ts", "mts", "cts", "tsx", "jsx"] {
        let candidate = PathBuf::from(format!("{}.{}", path.display(), ext));
        if candidate.exists() {
            let normalized = lexically_normalize_path(&candidate);
            return Ok(normalized.to_string_lossy().to_string());
        }
    }

    anyhow::bail!("module not found: {}", path.display());
}

fn get_builtin_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let module = v8::Object::new(scope);

    match name {
        "path" => {
            set_function(scope, &module, "join", path_join_callback);
            set_function(scope, &module, "resolve", path_resolve_callback);
            set_function(scope, &module, "dirname", path_dirname_callback);
            set_function(scope, &module, "basename", path_basename_callback);
            set_function(scope, &module, "extname", path_extname_callback);
            set_function(scope, &module, "relative", path_relative_callback);
            set_function(scope, &module, "isAbsolute", path_is_absolute_callback);
            set_function(scope, &module, "normalize", path_normalize_callback);
            set_function(scope, &module, "parse", path_parse_callback);
            set_function(scope, &module, "format", path_format_callback);
            set_function(scope, &module, "toNamespacedPath", path_normalize_callback);
            let sep = v8::String::new(scope, "/").unwrap();
            let sep_key = v8::String::new(scope, "sep").unwrap();
            module.set(scope, sep_key.into(), sep.into());
            let delim = v8::String::new(scope, ":").unwrap();
            let delim_key = v8::String::new(scope, "delimiter").unwrap();
            module.set(scope, delim_key.into(), delim.into());
            // posix / win32 aliases
            module.set(scope, sep_key.into(), sep.into());
            let posix_key = v8::String::new(scope, "posix").unwrap();
            module.set(scope, posix_key.into(), module.into());
            Some(module.into())
        }
        "fs" => {
            set_function(scope, &module, "readFileSync", fs_read_file_sync_callback);
            set_function(scope, &module, "writeFileSync", fs_write_file_sync_callback);
            set_function(scope, &module, "existsSync", fs_exists_sync_callback);
            set_function(scope, &module, "statSync", fs_stat_sync_callback);
            set_function(scope, &module, "lstatSync", fs_stat_sync_callback);
            set_function(scope, &module, "readdirSync", fs_readdir_sync_callback);
            set_function(scope, &module, "mkdirSync", fs_mkdir_sync_callback);
            set_function(scope, &module, "realpathSync", fs_realpath_sync_callback);
            set_function(scope, &module, "copyFileSync", fs_copy_file_sync_callback);
            set_function(scope, &module, "unlinkSync", fs_unlink_sync_callback);
            set_function(scope, &module, "appendFileSync", fs_write_file_sync_callback);
            let fs_promises = make_fs_promises_module(scope);
            let promises_key = v8::String::new(scope, "promises").unwrap();
            module.set(scope, promises_key.into(), fs_promises.into());
            if let Some(shim_val) = eval_js_source(scope, NODE_FS_CB_SHIM_JS) {
                if let Ok(shim_fn) = v8::Local::<v8::Function>::try_from(shim_val) {
                    let undef = v8::undefined(scope);
                    shim_fn.call(scope, undef.into(), &[module.into()]);
                }
            }
            let constants = v8::Object::new(scope);
            let c0 = v8::Number::new(scope, 0.0);
            for cname in ["F_OK", "R_OK", "W_OK"] {
                let ck = v8::String::new(scope, cname).unwrap();
                constants.set(scope, ck.into(), c0.into());
            }
            let constants_key = v8::String::new(scope, "constants").unwrap();
            module.set(scope, constants_key.into(), constants.into());
            Some(module.into())
        }
        "os" => {
            set_function(scope, &module, "platform", os_platform_callback);
            set_function(scope, &module, "homedir", os_homedir_callback);
            set_function(scope, &module, "tmpdir", os_tmpdir_callback);
            set_function(scope, &module, "hostname", os_hostname_callback);
            set_function(scope, &module, "type", os_platform_callback);
            set_function(scope, &module, "arch", os_arch_callback);
            set_function(scope, &module, "release", os_release_callback);
            let eol = v8::String::new(scope, "\n").unwrap();
            let eol_key = v8::String::new(scope, "EOL").unwrap();
            module.set(scope, eol_key.into(), eol.into());
            Some(module.into())
        }
        "util" => {
            set_function(scope, &module, "inspect", util_inspect_callback);
            set_function(scope, &module, "format", util_format_callback);
            set_function(scope, &module, "inherits", util_inherits_callback);
            set_function(scope, &module, "promisify", util_promisify_callback);
            set_function(scope, &module, "deprecate", util_deprecate_callback);
            set_function(scope, &module, "debuglog", util_debuglog_callback);
            let types = v8::Object::new(scope);
            set_function(scope, &types, "isPromise", util_is_thenable_callback);
            set_function(scope, &types, "isAsyncFunction", util_is_thenable_callback);
            let types_key = v8::String::new(scope, "types").unwrap();
            module.set(scope, types_key.into(), types.into());
            set_function(scope, &module, "styleText", util_style_text_callback);
            set_function(scope, &module, "stripVTControlCharacters", util_strip_vt_callback);
            Some(module.into())
        }
        "url" => {
            set_function(scope, &module, "pathToFileURL", url_path_to_file_url_callback);
            set_function(scope, &module, "fileURLToPath", url_file_to_path_callback);
            Some(module.into())
        }
        "module" => {
            set_function(scope, &module, "enableCompileCache", module_enable_compile_cache_callback);
            set_function(scope, &module, "disableCompileCache", noop_callback);
            set_function(scope, &module, "createRequire", module_create_require_callback);
            set_function(scope, &module, "_resolveFilename", require_resolve_callback);
            Some(module.into())
        }
        "events" => {
            let ctor = eval_js_source(scope, EVENTS_EMITTER_JS)?;
            let ctor_fn: v8::Local<v8::Function> = v8::Local::<v8::Function>::try_from(ctor).ok()?;
            let ee_key = v8::String::new(scope, "EventEmitter").unwrap();
            module.set(scope, ee_key.into(), ctor_fn.into());
            let default_key = v8::String::new(scope, "default").unwrap();
            module.set(scope, default_key.into(), ctor_fn.into());
            set_function(scope, &module, "once", noop_callback);
            Some(module.into())
        }
        "worker_threads" => {
            let is_main = v8::Boolean::new(scope, true);
            let im_key = v8::String::new(scope, "isMainThread").unwrap();
            module.set(scope, im_key.into(), is_main.into());
            let tid = v8::Number::new(scope, 0.0);
            let tid_key = v8::String::new(scope, "threadId").unwrap();
            module.set(scope, tid_key.into(), tid.into());
            let pp = v8::null(scope);
            let pp_key = v8::String::new(scope, "parentPort").unwrap();
            module.set(scope, pp_key.into(), pp.into());
            set_function(scope, &module, "getEnvironmentData", noop_callback);
            set_function(scope, &module, "setEnvironmentData", noop_callback);
            Some(module.into())
        }
        "assert" => {
            set_function(scope, &module, "ok", assert_ok_callback);
            set_function(scope, &module, "strict", assert_ok_callback);
            set_function(scope, &module, "default", assert_ok_callback);
            set_function(scope, &module, "equal", assert_equal_callback);
            set_function(scope, &module, "deepEqual", assert_equal_callback);
            set_function(scope, &module, "strictEqual", assert_equal_callback);
            set_function(scope, &module, "notEqual", noop_callback);
            set_function(scope, &module, "notDeepEqual", noop_callback);
            set_function(scope, &module, "ifError", assert_if_error_callback);
            set_function(scope, &module, "fail", assert_fail_callback);
            set_function(scope, &module, "throws", assert_throws_callback);
            set_function(scope, &module, "doesNotThrow", call_first_arg_callback);
            set_function(scope, &module, "AssertionError", noop_callback);
            Some(module.into())
        }
        "child_process" => {
            set_function(scope, &module, "spawnSync", cp_spawn_sync_callback);
            set_function(scope, &module, "execSync", cp_exec_sync_callback);
            set_function(scope, &module, "spawn", cp_spawn_unsupported_callback);
            set_function(scope, &module, "exec", cp_spawn_unsupported_callback);
            set_function(scope, &module, "execFile", cp_spawn_unsupported_callback);
            Some(module.into())
        }
        "tty" => {
            let tty_obj = v8::Object::new(scope);
            set_function(scope, &tty_obj, "isatty", tty_isatty_callback);
            set_function(scope, &module, "isatty", tty_isatty_callback);
            set_function(scope, &module, "ReadStream", noop_callback);
            set_function(scope, &module, "WriteStream", noop_callback);
            let m = module;
            let _ = tty_obj;
            let tty_key = v8::String::new(scope, "tty").unwrap();
            let _ = tty_key;
            Some(m.into())
        }
        "crypto" => {
            set_function(scope, &module, "createHash", crypto_hash_callback);
            set_function(scope, &module, "randomBytes", crypto_random_bytes_callback);
            set_function(scope, &module, "randomUUID", crypto_random_uuid_callback);
            Some(module.into())
        }
        "stream" | "stream/promises" | "zlib" | "http" | "https" | "net" | "dns" | "tls"
        | "vm" | "repl" | "string_decoder" | "trace_events" | "async_hooks" => {
            // Minimal stream constructor stubs so `class Foo extends Readable`
            // style usage surfaces a clear error at construct-time instead of import-time
            set_function(scope, &module, "Readable", stream_stub_callback);
            set_function(scope, &module, "Writable", stream_stub_callback);
            set_function(scope, &module, "Duplex", stream_stub_callback);
            set_function(scope, &module, "Transform", stream_stub_callback);
            set_function(scope, &module, "PassThrough", stream_stub_callback);
            set_function(scope, &module, "pipeline", noop_callback);
            set_function(scope, &module, "finished", noop_callback);
            Some(module.into())
        }
        "fs/promises" => Some(make_fs_promises_module(scope).into()),
        "buffer" | "constants" => Some(module.into()),
        _ => None,
    }
}

fn make_fs_promises_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Object> {
    let promises = v8::Object::new(scope);
    set_function(scope, &promises, "readFile", fs_promise_read_callback);
    set_function(scope, &promises, "writeFile", fs_promise_write_callback);
    set_function(scope, &promises, "stat", fs_promise_stat_callback);
    set_function(scope, &promises, "lstat", fs_promise_stat_callback);
    set_function(scope, &promises, "readdir", fs_promise_readdir_callback);
    set_function(scope, &promises, "mkdir", fs_promise_mkdir_callback);
    set_function(scope, &promises, "access", fs_promise_access_callback);
    set_function(scope, &promises, "rm", fs_promise_unlink_callback);
    set_function(scope, &promises, "unlink", fs_promise_unlink_callback);
    set_function(scope, &promises, "copyFile", fs_promise_access_callback);
    set_function(scope, &promises, "realpath", fs_promise_access_callback);
    set_function(scope, &promises, "open", fs_promise_unsupported_callback);
    set_function(scope, &promises, "appendFile", fs_promise_write_callback);
    promises
}

fn fs_promise_unsupported_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    let error_msg = "honey: this fs.promises API is not implemented yet";
    let msg = v8::String::new(scope, error_msg).unwrap();
    let exception = v8::Exception::error(scope, msg);
    scope.throw_exception(exception);
}

const EVENTS_EMITTER_JS: &str = r#"
(function(){
  function E(){ this._events = {}; }
  E.prototype.on = function(t, f){ (this._events[t] = this._events[t] || []).push(f); return this; };
  E.prototype.addListener = E.prototype.on;
  E.prototype.prependListener = E.prototype.on;
  E.prototype.once = function(t, f){
    var self = this;
    function g(){ self.removeListener(t, g); f.apply(self, arguments); }
    g._listener = f;
    return this.on(t, g);
  };
  E.prototype.removeListener = function(t, f){
    var a = this._events[t];
    if (a) {
      var i = a.indexOf(f);
      if (i >= 0) a.splice(i, 1);
    }
    return this;
  };
  E.prototype.off = E.prototype.removeListener;
  E.prototype.removeAllListeners = function(t){ if (t) delete this._events[t]; else this._events = {}; return this; };
  E.prototype.emit = function(t){
    var a = this._events[t];
    if (!a || !a.length) return false;
    a = a.slice();
    var xs = Array.prototype.slice.call(arguments, 1);
    for (var i = 0; i < a.length; i++) a[i].apply(this, xs);
    return true;
  };
  E.prototype.listenerCount = function(t){ var a = this._events[t]; return a ? a.length : 0; };
  E.EventEmitter = E;
  E.default = E;
  return E;
})()
"#;

fn eval_js_source<'s>(scope: &mut v8::PinScope<'s, '_>, src: &str) -> Option<v8::Local<'s, v8::Value>> {
    let code = v8::String::new(scope, src)?;
    let script = v8::Script::compile(scope, code, None)?;
    script.run(scope)
}

// ---------- console/URL JS sources ----------

// Wraps fs.promises functions into Node-style (err, callback) functions, so
// util.promisify(fs.stat) and friends keep working for CJS packages.
const NODE_FS_CB_SHIM_JS: &str = r#"
(function(fs){
  if (!fs || !fs.promises) return fs;
  function nodeify(promiseFn){
    return function(){
      var args = Array.prototype.slice.call(arguments);
      var cb = args.pop();
      if (typeof cb !== 'function') throw new TypeError('Callback must be a function');
      Promise.resolve().then(function(){ return promiseFn.apply(fs.promises, args); }).then(
        function(r){ cb(null, r); },
        function(e){ cb(e); }
      );
    };
  }
  ['stat','lstat','access','readFile','writeFile','readdir','mkdir','realpath','copyFile','unlink','rm','appendFile'].forEach(function(name){
    if (typeof fs.promises[name] === 'function' && typeof fs[name] !== 'function') {
      fs[name] = nodeify(fs.promises[name]);
    }
  });
  if (typeof fs.accessSync !== 'function') {
    fs.accessSync = function(p){
      if (!fs.existsSync(p)) {
        throw new Error("ENOENT: no such file or directory, access '" + p + "'");
      }
    };
  }
  return fs;
})
"#;

const MINIMAL_URL_JS: &str = r#"
(function(){
  function URL(u, base) {
    u = String(u);
    if (base !== undefined && !/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(u)) {
      var b = String(base);
      // Resolve relative against base
      if (/^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//.test(b)) {
        var bm = b.match(/^([a-zA-Z][a-zA-Z0-9+.-]*:)(\/\/[^/?#]*)?([^?#]*)(\?[^#]*)?(#.*)?$/);
        if (bm[3] && bm[4] === undefined && bm[5] === undefined && !u.startsWith('/')) {
          var dir = bm[3].replace(/\/[^/]*$/, '/');
          u = bm[1] + (bm[2] || '') + dir + u;
        } else {
          u = bm[1] + (bm[2] || '') + (u.startsWith('/') ? u : (bm[3] || '/').replace(/\/[^/]*$/, '/') + u);
        }
      }
    }
    var m = u.match(/^([a-zA-Z][a-zA-Z0-9+.-]*:)(\/\/[^/?#]*)?([^?#]*)(\?[^#]*)?(#.*)?$/);
    if (!m) throw new TypeError('Invalid URL: ' + u);
    this.protocol = m[1];
    this.host = (m[2] || '').replace(/^\/\//, '');
    this.hostname = this.host.split(':')[0] || '';
    this.port = this.host.includes(':') ? this.host.split(':')[1] : '';
    this.pathname = m[3] || '/';
    this.search = m[4] || '';
    this.hash = m[5] || '';
    this.href = (m[2] || this.protocol === 'file:' ? '' : '') && false ? '' : u;
    var self = this;
    this.origin = this.protocol + '//' + this.host;
    function rebuildHref(){ self.href = self.protocol + '//' + self.host + self.pathname + self.search + self.hash; }
    function queryPairs(){
      var s = self.search.slice(1);
      if (!s) return [];
      return s.split('&').map(function(pair){
        var i = pair.indexOf('=');
        var k = i === -1 ? pair : pair.slice(0, i);
        var v = i === -1 ? '' : pair.slice(i + 1);
        return [decodeURIComponent(k), decodeURIComponent(v)];
      });
    }
    function writePairs(pairs){
      var s = pairs.map(function(p){ return encodeURIComponent(p[0]) + '=' + encodeURIComponent(p[1]); }).join('&');
      self.search = s ? '?' + s : '';
      rebuildHref();
    }
    this.searchParams = {
      get: function(k) {
        var pairs = queryPairs();
        for (var i = 0; i < pairs.length; i++) {
          if (pairs[i][0] === k) return pairs[i][1];
        }
        return null;
      },
      set: function(k, v) {
        var pairs = queryPairs().filter(function(p){ return p[0] !== k; });
        pairs.push([String(k), String(v)]);
        writePairs(pairs);
      },
      append: function(k, v) {
        var pairs = queryPairs();
        pairs.push([String(k), String(v)]);
        writePairs(pairs);
      },
      has: function(k) {
        return this.get(k) !== null;
      },
      delete: function(k) {
        writePairs(queryPairs().filter(function(p){ return p[0] !== k; }));
      },
      forEach: function(cb, thisArg) {
        queryPairs().forEach(function(p){ cb.call(thisArg, p[1], p[0]); });
      },
      toString: function() { return self.search.slice(1); }
    };
  }
  URL.prototype.toString = function() { return this.href; };
  URL.prototype.toJSON = function() { return this.href; };
  URL.canParse = function(u, base) {
    try { new URL(u, base); return true; } catch (e) { return false; }
  };
  URL.createObjectURL = function() { return ''; };
  URL.revokeObjectURL = function() {};
  return URL;
})()
"#;

// ---------- path helpers ----------

fn normalize_path_str(p: &str) -> String {
    let abs = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if !parts.is_empty() && parts.last() != Some(&"..") {
                    parts.pop();
                } else if !abs {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let joined = parts.join("/");
    if abs {
        format!("/{}", joined)
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

fn join_paths(base: &str, next: &str) -> String {
    // Node's path.join concatenates and normalizes: a later segment starting
    // with '/' or '..' never resets the path (only path.resolve anchors).
    if base.is_empty() {
        return normalize_path_str(next);
    }
    normalize_path_str(&format!("{}/{}", base.trim_end_matches('/'), next))
}

fn path_join_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let mut parts: Vec<String> = Vec::new();
    for i in 0..args.length() {
        if let Some(s) = get_str(scope, args.get(i)) {
            parts.push(s);
        }
    }
    let mut result = String::new();
    for p in parts {
        result = join_paths(&result, &p);
        if result == "." { result = String::new(); }
    }
    let out = v8::String::new(scope, &result).unwrap();
    rv.set(out.into());
}

fn path_resolve_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let mut parts: Vec<String> = Vec::new();
    for i in 0..args.length() {
        if let Some(s) = get_str(scope, args.get(i)) {
            parts.push(s);
        }
    }
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let mut resolved = String::new();
    // Build right-to-left; an absolute part (or the cwd at the end) anchors it.
    for p in parts.iter().rev() {
        if resolved.is_empty() {
            resolved = normalize_path_str(p);
        } else if resolved.starts_with('/') {
            // Already anchored: a right-to-left join must not touch it
            // (path.resolve anchors on the rightmost absolute part).
        } else {
            resolved = join_paths(p, &resolved);
        }
    }
    if !resolved.starts_with('/') {
        resolved = join_paths(&cwd, &resolved);
    }
    let out = v8::String::new(scope, &resolved).unwrap();
    rv.set(out.into());
}

fn path_dirname_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    let dir = Path::new(&p).parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_else(|| ".".to_string());
    let out = v8::String::new(scope, &dir).unwrap();
    rv.set(out.into());
}

fn path_basename_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    let base = Path::new(&p).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| p.clone());
    let out = v8::String::new(scope, &base).unwrap();
    rv.set(out.into());
}

fn path_extname_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    let base = Path::new(&p).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    let ext = match base.rfind('.') {
        Some(idx) if idx > 0 && idx < base.len() - 1 => base[idx..].to_string(),
        _ => String::new(),
    };
    let out = v8::String::new(scope, &ext).unwrap();
    rv.set(out.into());
}

fn path_relative_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let from = get_str(scope, args.get(0)).unwrap_or_default();
    let to = get_str(scope, args.get(1)).unwrap_or_default();
    let from_parts: Vec<&str> = from.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    let to_parts: Vec<&str> = to.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    let mut common = 0;
    while common < from_parts.len() && common < to_parts.len() && from_parts[common] == to_parts[common] {
        common += 1;
    }
    let mut result: Vec<String> = Vec::new();
    for _ in common..from_parts.len() {
        result.push("..".to_string());
    }
    for p in &to_parts[common..] {
        result.push(p.to_string());
    }
    let out = result.join("/");
    let out_s = v8::String::new(scope, if out.is_empty() { "." } else { &out }).unwrap();
    rv.set(out_s.into());
}

fn path_is_absolute_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    rv.set(v8::Boolean::new(scope, p.starts_with('/')).into());
}

fn path_normalize_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    let out = v8::String::new(scope, &normalize_path_str(&p)).unwrap();
    rv.set(out.into());
}

fn path_parse_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    let pp = Path::new(&p);
    let file_name = pp.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    let dir = pp.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
    let ext = match file_name.rfind('.') {
        Some(idx) if idx > 0 && idx < file_name.len() - 1 => file_name[idx..].to_string(),
        _ => String::new(),
    };
    let name = if ext.is_empty() {
        file_name.clone()
    } else {
        file_name[..file_name.len().saturating_sub(ext.len())].to_string()
    };
    let root = if p.starts_with('/') { "/".to_string() } else { String::new() };
    let base = file_name;

    let parsed = v8::Object::new(scope);
    for (key, val) in [
        ("root", root),
        ("dir", dir),
        ("base", base),
        ("ext", ext),
        ("name", name),
    ] {
        let k = v8::String::new(scope, key).unwrap();
        let v = v8::String::new(scope, &val).unwrap();
        parsed.set(scope, k.into(), v.into());
    }
    rv.set(parsed.into());
}

fn path_format_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let obj = match v8::Local::<v8::Object>::try_from(args.get(0)) {
        Ok(o) => o,
        Err(_) => {
            let out = v8::String::new(scope, "").unwrap();
            rv.set(out.into());
            return;
        }
    };
    let get_prop = |scope: &mut v8::PinScope, name: &str| -> String {
        let k = v8::String::new(scope, name).unwrap();
        obj.get(scope, k.into())
            .and_then(|v| get_str(scope, v))
            .unwrap_or_default()
    };
    let root = get_prop(scope, "root");
    let dir = get_prop(scope, "dir");
    let base = get_prop(scope, "base");
    let ext = get_prop(scope, "ext");
    let name = get_prop(scope, "name");
    let result = if !dir.is_empty() {
        if !base.is_empty() {
            join_paths(&dir, &base)
        } else {
            join_paths(&dir, &format!("{}{}", name, ext))
        }
    } else {
        if !base.is_empty() { base } else { format!("{}{}", name, ext) }
    };
    let final_path = if root == "/" && !result.starts_with('/') {
        format!("/{}", result)
    } else {
        result
    };
    let out = v8::String::new(scope, &final_path).unwrap();
    rv.set(out.into());
}

// ---------- fs helpers ----------

fn fs_read_file_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let path = match get_str(scope, args.get(0)) {
        Some(p) => p,
        None => {
            let msg = v8::String::new(scope, "fs.readFileSync: path required").unwrap();
            let exception = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exception);
            return;
        }
    };

    // Determine encoding from second arg (string or {encoding: ...})
    let mut encoding: Option<String> = None;
    if args.length() > 1 {
        let opts = args.get(1);
        if opts.is_string() {
            encoding = Some(opts.to_string(scope).unwrap().to_rust_string_lossy(scope));
        } else if let Ok(obj) = v8::Local::<v8::Object>::try_from(opts) {
            let enc_key = v8::String::new(scope, "encoding").unwrap();
            if let Some(enc_val) = obj.get(scope, enc_key.into()) {
                encoding = get_str(scope, enc_val);
            }
        }
    }

    match std::fs::read(&path) {
        Ok(bytes) => {
            let is_utf8 = matches!(
                encoding.as_deref(),
                Some(e) if e.starts_with("utf") || e == "ascii" || e == "latin1"
            );
            if encoding.is_none() {
                // Return Uint8Array with a toString method for Buffer compat
                let backing = v8::ArrayBuffer::new_backing_store_from_vec(bytes);
                let ab = v8::ArrayBuffer::with_backing_store(scope, &backing.make_shared());
                let arr = v8::Uint8Array::new(scope, ab, 0, ab.byte_length()).unwrap();
                let mut arr_val: v8::Local<v8::Value> = arr.into();
                if let Ok(arr_obj) = v8::Local::<v8::Object>::try_from(arr_val) {
                    let ts_fn = v8::Function::new(scope, typed_array_to_string_callback).unwrap();
                    let ts_key = v8::String::new(scope, "toString").unwrap();
                    arr_obj.set(scope, ts_key.into(), ts_fn.into());
                    let sl_fn = v8::Function::new(scope, typed_array_slice_callback).unwrap();
                    let sl_key = v8::String::new(scope, "slice").unwrap();
                    arr_obj.set(scope, sl_key.into(), sl_fn.into());
                }
                err_val_to_val(&mut arr_val);
                rv.set(arr_val);
            } else if is_utf8 {
                let s = String::from_utf8_lossy(&bytes);
                let out = v8::String::new(scope, &s).unwrap();
                rv.set(out.into());
            } else if encoding.as_deref() == Some("hex") {
                let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
                let out = v8::String::new(scope, &hex).unwrap();
                rv.set(out.into());
            } else if encoding.as_deref() == Some("base64") {
                let out = v8::String::new(scope, &base64_encode(&bytes)).unwrap();
                rv.set(out.into());
            } else {
                let s = String::from_utf8_lossy(&bytes);
                let out = v8::String::new(scope, &s).unwrap();
                rv.set(out.into());
            }
        }
        Err(e) => {
            let error_msg = format!("ENOENT: no such file or directory, open '{}': {}", path, e);
            let msg = v8::String::new(scope, &error_msg).unwrap();
            let exception = v8::Exception::error(scope, msg);
            scope.throw_exception(exception);
        }
    }
}

fn err_val_to_val(_v: &mut v8::Local<v8::Value>) {}

fn typed_array_to_string_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    match typed_array_bytes(scope, this.into()) {
        Some(bytes) => {
            let s = String::from_utf8_lossy(&bytes);
            let out = v8::String::new(scope, &s).unwrap();
            rv.set(out.into());
        }
        None => {
            let out = v8::String::new(scope, "").unwrap();
            rv.set(out.into());
        }
    }
}

fn typed_array_slice_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    let bytes = typed_array_bytes(scope, this.into()).unwrap_or_default();
    let start = get_str(scope, args.get(0))
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    let end = get_str(scope, args.get(1))
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(bytes.len());
    let slice: Vec<u8> = bytes.into_iter().skip(start).take(end.saturating_sub(start)).collect();
    let backing = v8::ArrayBuffer::new_backing_store_from_vec(slice);
    let ab = v8::ArrayBuffer::with_backing_store(scope, &backing.make_shared());
    let arr = v8::Uint8Array::new(scope, ab, 0, ab.byte_length()).unwrap();
    rv.set(arr.into());
}

fn typed_array_bytes(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> Option<Vec<u8>> {
    if let Ok(ta) = v8::Local::<v8::Uint8Array>::try_from(v) {
        let len = ta.byte_length();
        let mut buf = vec![0u8; len];
        let copied = ta.copy_contents(&mut buf);
        buf.truncate(copied);
        return Some(buf);
    }
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(v) {
        let len = ab.byte_length();
        if let Some(data) = ab.data() {
            unsafe {
                return Some(std::slice::from_raw_parts(data.as_ptr() as *const u8, len).to_vec());
            }
        }
    }
    None
}

// Minimal base64 for Buffer compat (not constant-time, dev runtime)
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(TABLE[(n >> 18 & 63) as usize] as char);
        out.push(TABLE[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn fs_write_file_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let path = get_str(scope, args.get(0)).unwrap_or_default();
    let data_val = args.get(1);
    let result: std::io::Result<()> = if let Some(bytes) = typed_array_bytes(scope, data_val) {
        std::fs::write(&path, bytes)
    } else if let Some(s) = get_str(scope, data_val) {
        std::fs::write(&path, s)
    } else {
        Ok(())
    };
    if let Err(e) = result {
        let error_msg = format!("fs.writeFileSync failed for '{}': {}", path, e);
        let msg = v8::String::new(scope, &error_msg).unwrap();
        let exception = v8::Exception::error(scope, msg);
        scope.throw_exception(exception);
    }
    let _ = &mut rv;
}

fn fs_exists_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    rv.set(v8::Boolean::new(scope, std::path::Path::new(&p).exists()).into());
}

fn fs_stat_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    match std::fs::metadata(&p) {
        Ok(meta) => {
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as f64);
            if let Some(stat) = make_stat_object(
                scope,
                meta.is_file(),
                meta.is_dir(),
                meta.file_type().is_symlink(),
                meta.len(),
                mtime_ms,
            ) {
                rv.set(stat);
            } else {
                let msg = v8::String::new(scope, "fs.statSync: could not build stat object").unwrap();
                let exception = v8::Exception::error(scope, msg);
                scope.throw_exception(exception);
            }
        }
        Err(e) => {
            let error_msg = format!("ENOENT: stat failed for '{}': {}", p, e);
            let msg = v8::String::new(scope, &error_msg).unwrap();
            let exception = v8::Exception::error(scope, msg);
            scope.throw_exception(exception);
        }
    }
}

fn make_stat_object<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    is_file: bool,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
    mtime_ms: Option<f64>,
) -> Option<v8::Local<'s, v8::Value>> {
    let mt = mtime_ms.unwrap_or(0.0);
    let src = format!(
        "(function(){{ return {{ isFile: function(){{ return {}; }}, \
         isDirectory: function(){{ return {}; }}, isSymbolicLink: function(){{ return {}; }}, \
         size: {}, mtimeMs: {}, mtime: {{ getTime: function(){{ return {}; }} }} }}; }})()",
        is_file, is_dir, is_symlink, size, mt, mt
    );
    eval_js_source(scope, &src)
}

fn fs_readdir_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    match std::fs::read_dir(&p) {
        Ok(entries) => {
            let names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            let arr = v8::Array::new(scope, names.len() as i32);
            for (i, name) in names.iter().enumerate() {
                let s = v8::String::new(scope, name).unwrap();
                let idx = v8::Number::new(scope, i as f64);
                arr.set(scope, idx.into(), s.into());
            }
            rv.set(arr.into());
        }
        Err(e) => {
            let error_msg = format!("ENOENT: readdir failed for '{}': {}", p, e);
            let msg = v8::String::new(scope, &error_msg).unwrap();
            let exception = v8::Exception::error(scope, msg);
            scope.throw_exception(exception);
        }
    }
}

fn fs_mkdir_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    if let Err(e) = std::fs::create_dir_all(&p) {
        let error_msg = format!("fs.mkdirSync failed for '{}': {}", p, e);
        let msg = v8::String::new(scope, &error_msg).unwrap();
        let exception = v8::Exception::error(scope, msg);
        scope.throw_exception(exception);
    }
}

fn fs_realpath_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    match std::fs::canonicalize(&p) {
        Ok(real) => {
            let out = v8::String::new(scope, &real.to_string_lossy()).unwrap();
            rv.set(out.into());
        }
        Err(e) => {
            let error_msg = format!("fs.realpathSync failed for '{}': {}", p, e);
            let msg = v8::String::new(scope, &error_msg).unwrap();
            let exception = v8::Exception::error(scope, msg);
            scope.throw_exception(exception);
        }
    }
}

fn fs_copy_file_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    let from = get_str(scope, args.get(0)).unwrap_or_default();
    let to = get_str(scope, args.get(1)).unwrap_or_default();
    if let Err(e) = std::fs::copy(&from, &to) {
        let error_msg = format!("fs.copyFileSync failed: {}", e);
        let msg = v8::String::new(scope, &error_msg).unwrap();
        let exception = v8::Exception::error(scope, msg);
        scope.throw_exception(exception);
    }
}

fn fs_unlink_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    if let Err(e) = std::fs::remove_file(&p) {
        let error_msg = format!("fs.unlinkSync failed for '{}': {}", p, e);
        let msg = v8::String::new(scope, &error_msg).unwrap();
        let exception = v8::Exception::error(scope, msg);
        scope.throw_exception(exception);
    }
}

// ---------- fs promises (sync work wrapped in resolved promises) ----------

fn resolve_promise_with_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: Option<v8::Local<v8::Value>>,
    error_msg: Option<String>,
) -> v8::Local<'s, v8::Value> {
    if let Some(resolver) = v8::PromiseResolver::new(scope) {
        let promise = resolver.get_promise(scope);
        match value {
            Some(v) => {
                resolver.resolve(scope, v);
            }
            None => {
                let msg_str = error_msg.unwrap_or_else(|| "unknown fs error".to_string());
                let msg = v8::String::new(scope, &msg_str).unwrap();
                let exception = v8::Exception::error(scope, msg);
                resolver.reject(scope, exception);
            }
        }
        promise.into()
    } else {
        scope.get_current_context().global(scope).into()
    }
}

fn fs_promise_read_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let path = get_str(scope, args.get(0)).unwrap_or_default();
    let mut encoding: Option<String> = None;
    if args.length() > 1 {
        let opts = args.get(1);
        if opts.is_string() {
            encoding = Some(opts.to_string(scope).unwrap().to_rust_string_lossy(scope));
        } else if let Ok(obj) = v8::Local::<v8::Object>::try_from(opts) {
            let enc_key = v8::String::new(scope, "encoding").unwrap();
            if let Some(enc_val) = obj.get(scope, enc_key.into()) {
                encoding = get_str(scope, enc_val);
            }
        }
    }

    match std::fs::read(&path) {
        Ok(bytes) => {
            let utf8 = encoding.as_deref().map(|e| e.starts_with("utf") || e == "ascii").unwrap_or(true);
            if utf8 {
                let s = String::from_utf8_lossy(&bytes);
                let out = v8::String::new(scope, &s).unwrap();
                rv.set(resolve_promise_with_value(scope, Some(out.into()), None));
            } else {
                let backing = v8::ArrayBuffer::new_backing_store_from_vec(bytes);
                let ab = v8::ArrayBuffer::with_backing_store(scope, &backing.make_shared());
                let arr = v8::Uint8Array::new(scope, ab, 0, ab.byte_length()).unwrap();
                rv.set(resolve_promise_with_value(scope, Some(arr.into()), None));
            }
        }
        Err(e) => {
            rv.set(resolve_promise_with_value(
                scope,
                None,
                Some(format!("ENOENT: readFile failed for '{}': {}", path, e)),
            ));
        }
    }
}

fn fs_promise_write_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let path = get_str(scope, args.get(0)).unwrap_or_default();
    let data_val = args.get(1);
    let result: std::io::Result<()> = if let Some(bytes) = typed_array_bytes(scope, data_val) {
        std::fs::write(&path, bytes)
    } else if let Some(s) = get_str(scope, data_val) {
        std::fs::write(&path, s)
    } else {
        Ok(())
    };
    match result {
        Ok(_) => {
            let undefined = v8::undefined(scope);
            rv.set(resolve_promise_with_value(scope, Some(undefined.into()), None));
        }
        Err(e) => {
            rv.set(resolve_promise_with_value(
                scope,
                None,
                Some(format!("writeFile failed for '{}': {}", path, e)),
            ));
        }
    }
}

fn fs_promise_stat_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let path = get_str(scope, args.get(0)).unwrap_or_default();
    match std::fs::metadata(&path) {
        Ok(meta) => {
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as f64);
            if let Some(stat) = make_stat_object(
                scope,
                meta.is_file(),
                meta.is_dir(),
                meta.file_type().is_symlink(),
                meta.len(),
                mtime_ms,
            ) {
                rv.set(resolve_promise_with_value(scope, Some(stat), None));
            } else {
                rv.set(resolve_promise_with_value(
                    scope,
                    None,
                    Some(format!("stat failed for '{}'", path)),
                ));
            }
        }
        Err(e) => {
            rv.set(resolve_promise_with_value(
                scope,
                None,
                Some(format!("stat failed for '{}': {}", path, e)),
            ));
        }
    }
}

fn fs_promise_readdir_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    // Dirent mode: readdir(dir, { withFileTypes: true })
    let with_dirents = if args.length() > 1 {
        v8::Local::<v8::Object>::try_from(args.get(1))
            .ok()
            .and_then(|opts| {
                let key = v8::String::new(scope, "withFileTypes").unwrap();
                let val = opts.get(scope, key.into())?;
                Some(val.boolean_value(scope))
            })
            .unwrap_or(false)
    } else {
        false
    };
    match std::fs::read_dir(&p) {
        Ok(entries) => {
            let collected: Vec<std::fs::DirEntry> = entries.filter_map(|e| e.ok()).collect();
            let arr = v8::Array::new(scope, collected.len() as i32);
            for (i, entry) in collected.iter().enumerate() {
                let idx = v8::Number::new(scope, i as f64);
                let val: v8::Local<v8::Value> = if with_dirents {
                    make_dirent_object(
                        scope,
                        &entry.file_name().to_string_lossy(),
                        entry.file_type().map(|t| t.is_dir()).unwrap_or(false),
                        entry.file_type().map(|t| t.is_file()).unwrap_or(false),
                        entry.file_type().map(|t| t.is_symlink()).unwrap_or(false),
                    )
                } else {
                    v8::String::new(scope, &entry.file_name().to_string_lossy())
                        .map(|s| s.into())
                        .unwrap_or(v8::undefined(scope).into())
                };
                arr.set(scope, idx.into(), val);
            }
            rv.set(resolve_promise_with_value(scope, Some(arr.into()), None));
        }
        Err(e) => {
            rv.set(resolve_promise_with_value(
                scope,
                None,
                Some(format!("readdir failed for '{}': {}", p, e)),
            ));
        }
    }
}

fn make_dirent_object<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
) -> v8::Local<'s, v8::Value> {
    let src = format!(
        "(function(){{ return {{ name: {:?}, isDirectory: function(){{ return {}; }}, \
         isFile: function(){{ return {}; }}, isSymbolicLink: function(){{ return {}; }} }}; }})()",
        name, is_dir, is_file, is_symlink
    );
    eval_js_source(scope, &src).unwrap_or(scope.get_current_context().global(scope).into())
}

fn fs_promise_mkdir_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    match std::fs::create_dir_all(&p) {
        Ok(_) => {
            let undefined = v8::undefined(scope);
            rv.set(resolve_promise_with_value(scope, Some(undefined.into()), None));
        }
        Err(e) => {
            rv.set(resolve_promise_with_value(
                scope,
                None,
                Some(format!("mkdir failed for '{}': {}", p, e)),
            ));
        }
    }
}

fn fs_promise_access_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    if std::path::Path::new(&p).exists() {
        let undefined = v8::undefined(scope);
        rv.set(resolve_promise_with_value(scope, Some(undefined.into()), None));
    } else {
        rv.set(resolve_promise_with_value(
            scope,
            None,
            Some(format!("ENOENT: access failed for '{}'", p)),
        ));
    }
}

fn fs_promise_unlink_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    let _ = std::fs::remove_file(&p);
    let undefined = v8::undefined(scope);
    rv.set(resolve_promise_with_value(scope, Some(undefined.into()), None));
}

// ---------- os ----------

fn os_platform_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let out = v8::String::new(scope, std::env::consts::OS).unwrap();
    rv.set(out.into());
}

fn os_arch_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let out = v8::String::new(scope, std::env::consts::ARCH).unwrap();
    rv.set(out.into());
}

fn os_homedir_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let out = v8::String::new(scope, &home).unwrap();
    rv.set(out.into());
}

fn os_tmpdir_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    let out = v8::String::new(scope, &tmp).unwrap();
    rv.set(out.into());
}

fn os_hostname_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let out = v8::String::new(scope, "localhost").unwrap();
    rv.set(out.into());
}

fn os_release_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let out = v8::String::new(scope, "0.0.0").unwrap();
    rv.set(out.into());
}

// ---------- util ----------

fn util_inspect_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let val = args.get(0);
    if val.is_string() {
        rv.set(val);
    } else if let Some(json) = v8::json::stringify(scope, val) {
        rv.set(json.into());
    } else {
        let out = v8::String::new(scope, "[unserializable]").unwrap();
        rv.set(out.into());
    }
}

fn util_format_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let len = args.length() as i32;
    let mut next_arg = 1i32;
    let mut out = String::new();

    if len > 0 {
        let fmt = value_to_display_string(scope, args.get(0));
        let chars: Vec<char> = fmt.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '%' && i + 1 < chars.len() {
                let spec = chars[i + 1];
                let available = next_arg < len;
                let substitution = match spec {
                    '%' => Some("%".to_string()),
                    's' if available => {
                        next_arg += 1;
                        Some(value_to_display_string(scope, args.get(next_arg - 1)))
                    }
                    'd' | 'i' | 'f' if available => {
                        next_arg += 1;
                        Some(number_to_string(scope, args.get(next_arg - 1)))
                    }
                    'j' | 'o' | 'O' if available => {
                        next_arg += 1;
                        let v = args.get(next_arg - 1);
                        Some(
                            v8::json::stringify(scope, v)
                                .map(|x| x.to_rust_string_lossy(scope))
                                .unwrap_or_else(|| "undefined".to_string()),
                        )
                    }
                    _ => None,
                };
                if let Some(rep) = substitution {
                    out.push_str(&rep);
                    i += 2;
                    continue;
                }
            }
            out.push(chars[i]);
            i += 1;
        }
    }

    // Append remaining args not consumed by specifiers, space-separated
    while next_arg < len {
        out.push(' ');
        out.push_str(&value_to_display_string(scope, args.get(next_arg)));
        next_arg += 1;
    }

    let out_str = v8::String::new(scope, &out).unwrap();
    rv.set(out_str.into());
}

fn number_to_string(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> String {
    if let Some(n) = v.number_value(scope) {
        format!("{}", n)
    } else {
        "NaN".to_string()
    }
}

fn value_to_display_string(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> String {
    if val.is_string() {
        val.to_string(scope).unwrap().to_rust_string_lossy(scope)
    } else if let Some(json) = v8::json::stringify(scope, val) {
        json.to_rust_string_lossy(scope)
    } else {
        String::new()
    }
}

fn util_inherits_callback(
    _scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    // Best-effort: real prototype wiring done via the embedded emitter; inheritance
    // of plain constructor functions usually still works with explicit assignment.
}

fn util_promisify_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let f = args.get(0);
    let src = r#"
(function(fn){
  if (typeof fn !== 'function') {
    throw new TypeError('The "original" argument must be of type Function');
  }
  if (fn.__honey_already_promisified) {
    return fn;
  }
  var sym = Symbol.for('nodejs.util.promisify.custom');
  var target = fn[sym] || fn;
  var g = function(){
    var args = Array.prototype.slice.call(arguments);
    var self = this;
    return new Promise(function(resolve, reject){
      args.push(function(err, val){
        if (err) { reject(err); } else { resolve(val); }
      });
      target.apply(self, args);
    });
  };
  g.__honey_already_promisified = true;
  return g;
})
"#;
    let undef = v8::undefined(scope);
    if let Some(srcv) = v8::String::new(scope, src) {
        if let Some(script) = v8::Script::compile(scope, srcv, None) {
            if let Some(res) = script.run(scope) {
                if let Ok(outer) = v8::Local::<v8::Function>::try_from(res) {
                    if let Some(ret) = outer.call(scope, undef.into(), &[f]) {
                        rv.set(ret);
                        return;
                    }
                }
            }
        }
    }
    rv.set(f);
}

fn util_deprecate_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let f = args.get(0);
    rv.set(f);
}

fn util_debuglog_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let noop = v8::Function::new(scope, noop_callback).unwrap();
    rv.set(noop.into());
}

fn util_is_thenable_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let v = args.get(0);
    rv.set(v8::Boolean::new(scope, v.is_promise()).into());
}

fn util_style_text_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let format = get_str(scope, args.get(0)).unwrap_or_default();
    let text = get_str(scope, args.get(1)).unwrap_or_default();
    // Respect a validateStream option: when truthy and stdout isn't a TTY,
    // Node skips styling. We never treat streams as TTY.
    let validate_stream = args
        .get(2)
        .to_object(scope)
        .map(|opts| {
            let key = v8::String::new(scope, "validateStream").unwrap();
            let v = opts.get(scope, key.into()).unwrap();
            v.is_true() || v.to_boolean(scope).is_true()
        })
        .unwrap_or(false);
    let styled = if validate_stream {
        text
    } else {
        let code = match format.as_str() {
            "red" => "31", "green" => "32", "yellow" => "33", "blue" => "34",
            "magenta" => "35", "cyan" => "36", "gray" | "grey" => "90",
            "bold" => "1", "dim" => "2", "italic" => "3", "underline" => "4",
            "strikethrough" => "9", _ => return,
        };
        format!("\u{1b}[{}m{}\u{1b}[0m", code, text)
    };
    let out = v8::String::new(scope, &styled).unwrap();
    rv.set(out.into());
}

fn util_strip_vt_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let text = get_str(scope, args.get(0)).unwrap_or_default();
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // swallow ESC[ ... final byte
            if let Some(next) = chars.clone().next() {
                if next == '[' {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if c2.is_ascii_alphabetic() || c2 == '@' || c2 == '~' {
                            break;
                        }
                    }
                    continue;
                }
            }
            continue;
        }
        if c == '\u{9b}' {
            continue;
        }
        out.push(c);
    }
    let ostr = v8::String::new(scope, &out).unwrap();
    rv.set(ostr.into());
}

// ---------- url ----------

fn url_path_to_file_url_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let p = get_str(scope, args.get(0)).unwrap_or_default();
    // Percent-encode characters that would break the file URL components.
    let mut encoded = String::with_capacity(p.len());
    for b in p.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=' | b':' | b'@' => encoded.push(b as char),
            _ => encoded.push_str(&format!("%{:02X}", b)),
        }
    }
    let href = format!("file://{}", encoded);
    let ctor_src = r#"
(function(href){
  return new globalThis.URL(href);
})
"#;
    let undef = v8::undefined(scope);
    if let Some(srcv) = v8::String::new(scope, ctor_src) {
        if let Some(script) = v8::Script::compile(scope, srcv, None) {
            if let Some(res) = script.run(scope) {
                if let Ok(ctor) = v8::Local::<v8::Function>::try_from(res) {
                    if let Some(href_s) = v8::String::new(scope, &href) {
                        if let Some(obj) = ctor.call(scope, undef.into(), &[href_s.into()]) {
                            rv.set(obj);
                            return;
                        }
                    }
                }
            }
        }
    }
    rv.set(v8::undefined(scope).into());
}

fn url_to_string_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    let href_key = v8::String::new(scope, "href").unwrap();
    if let Some(href) = this.get(scope, href_key.into()) {
        if let Some(s) = get_str(scope, href) {
            let out = v8::String::new(scope, &s).unwrap();
            rv.set(out.into());
            return;
        }
    }
    let out = v8::String::new(scope, "").unwrap();
    rv.set(out.into());
}

fn url_file_to_path_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let raw = if let Ok(obj) = v8::Local::<v8::Object>::try_from(args.get(0)) {
        let href_key = v8::String::new(scope, "href").unwrap();
        obj.get(scope, href_key.into())
            .and_then(|h| get_str(scope, h))
            .unwrap_or_default()
    } else {
        get_str(scope, args.get(0)).unwrap_or_default()
    };
    let replaced = raw.replace("%20", " ");
    let path = replaced.strip_prefix("file://").unwrap_or(&replaced).to_string();
    let out = v8::String::new(scope, &path).unwrap();
    rv.set(out.into());
}

// ---------- module ----------

fn module_enable_compile_cache_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let out = v8::String::new(scope, "").unwrap();
    // Node returns a status object; undefined is fine for optional chaining
    let _ = out;
    rv.set_undefined();
}

fn module_create_require_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if let Some(base) = get_str(scope, args.get(0)) {
        let base_path = Path::new(&base);
        let dir = if base_path.extension().is_some() {
            base_path.parent().map(|p| p.to_path_buf())
        } else {
            Some(base_path.to_path_buf())
        };
        if let Some(d) = dir {
            MODULE_DIR.with(|cur| *cur.borrow_mut() = d);
        }
    }
    let require_fn = v8::Function::new(scope, require_callback).unwrap();
    let resolve_fn = v8::Function::new(scope, require_resolve_callback).unwrap();
    let resolve_key = v8::String::new(scope, "resolve").unwrap();
    require_fn.set(scope, resolve_key.into(), resolve_fn.into());
    rv.set(require_fn.into());
}

// ---------- assert ----------

fn throw_assertion_error(scope: &mut v8::PinScope, message: &str) {
    let error_msg = format!("AssertionError: {}", message);
    let msg = v8::String::new(scope, &error_msg).unwrap();
    let exception = v8::Exception::error(scope, msg);
    scope.throw_exception(exception);
}

fn assert_ok_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    let val = args.get(0);
    let truthy = if val.is_undefined() || val.is_null() || val.is_false() {
        false
    } else if let Some(n) = val.number_value(scope) {
        n != 0.0
    } else {
        true
    };
    if !truthy {
        let msg = get_str(scope, args.get(1)).unwrap_or_else(|| "assertion failed".to_string());
        throw_assertion_error(scope, &msg);
    }
}

fn assert_equal_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    let a = args
        .get(0)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();
    let b = args
        .get(1)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();
    let num_match = match (args.get(0).number_value(scope), args.get(1).number_value(scope)) {
        (Some(x), Some(y)) => Some(x == y),
        _ => None,
    };
    let equal = num_match.unwrap_or(a == b);
    if !equal {
        let msg = get_str(scope, args.get(2))
            .map(|m| format!("{}: {} != {}", m, a, b))
            .unwrap_or_else(|| format!("{} != {}", a, b));
        throw_assertion_error(scope, &msg);
    }
}

fn assert_if_error_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    let val = args.get(0);
    if !val.is_null() && !val.is_undefined() && !val.is_false() {
        // Treat as error: throw it
        scope.throw_exception(val);
    }
}

fn assert_fail_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    let msg = get_str(scope, args.get(0)).unwrap_or_else(|| "assert.fail".to_string());
    throw_assertion_error(scope, &msg);
}

fn assert_throws_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    if args.length() > 0 {
        let f = args.get(0);
        if let Ok(func) = v8::Local::<v8::Function>::try_from(f) {
            let undefined = v8::undefined(scope);
            let caught = {
                v8::tc_scope!(let tc, scope);
                let _result = func.call(tc, undefined.into(), &[]);
                tc.has_caught()
            };
            if !caught {
                let msg = get_str(scope, args.get(1))
                    .unwrap_or_else(|| "Function expected to throw".to_string());
                throw_assertion_error(scope, &msg);
            }
        }
    }
}

fn call_first_arg_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    if args.length() > 0 {
        let f = args.get(0);
        if let Ok(func) = v8::Local::<v8::Function>::try_from(f) {
            let undefined = v8::undefined(scope);
            func.call(scope, undefined.into(), &[]);
        }
    }
}

// ---------- child_process ----------

fn cp_spawn_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let cmd = get_str(scope, args.get(0)).unwrap_or_default();
    let mut cmd_args: Vec<String> = Vec::new();
    if args.length() > 1 {
        let arr_val = args.get(1);
        if let Ok(arr) = v8::Local::<v8::Array>::try_from(arr_val) {
            for i in 0..arr.length() {
                let idx = v8::Number::new(scope, i as f64);
                if let Some(item) = arr.get(scope, idx.into()) {
                    if let Some(s) = get_str(scope, item) {
                        cmd_args.push(s);
                    }
                }
            }
        }
    }

    let mut shell_cmd = format!("{} {}", cmd, cmd_args.join(" "));

    let mut command = std::process::Command::new("sh");
    command.arg("-c");

    if args.length() > 2 {
        if let Ok(opts) = v8::Local::<v8::Object>::try_from(args.get(2)) {
            let cwd_key = v8::String::new(scope, "cwd").unwrap();
            if let Some(cwd_val) = opts.get(scope, cwd_key.into()) {
                if let Some(cwd) = get_str(scope, cwd_val) {
                    command.current_dir(&cwd);
                }
            }
            let shell_key = v8::String::new(scope, "shell").unwrap();
            if let Some(shell_val) = opts.get(scope, shell_key.into()) {
                if let Some(shell) = get_str(scope, shell_val) {
                    command = std::process::Command::new(&shell);
                    command.arg(&shell_cmd);
                    run_cmd_and_set_result(scope, command, &mut rv);
                    return;
                }
                if shell_val.is_boolean() && shell_val.is_false() {
                    let mut c = std::process::Command::new(&cmd);
                    c.args(&cmd_args);
                    run_cmd_and_set_result(scope, c, &mut rv);
                    return;
                }
            }
        }
    }

    command.arg(&shell_cmd);
    run_cmd_and_set_result(scope, command, &mut rv);
}

fn run_cmd_and_set_result(
    scope: &mut v8::PinScope,
    mut command: std::process::Command,
    rv: &mut v8::ReturnValue,
) {
    let mut result_obj = v8::Object::new(scope);
    match command.output() {
        Ok(output) => {
            let status_num = output.status.code().unwrap_or(-1);
            let status = v8::Number::new(scope, status_num as f64);
            let status_key = v8::String::new(scope, "status").unwrap();
            result_obj.set(scope, status_key.into(), status.into());
            let stdout_s = String::from_utf8_lossy(&output.stdout);
            let stdout_val = v8::String::new(scope, &stdout_s).unwrap();
            let stdout_key = v8::String::new(scope, "stdout").unwrap();
            result_obj.set(scope, stdout_key.into(), stdout_val.into());
            let stderr_s = String::from_utf8_lossy(&output.stderr);
            let stderr_val = v8::String::new(scope, &stderr_s).unwrap();
            let stderr_key = v8::String::new(scope, "stderr").unwrap();
            result_obj.set(scope, stderr_key.into(), stderr_val.into());
            let signal = v8::null(scope);
            let signal_key = v8::String::new(scope, "signal").unwrap();
            result_obj.set(scope, signal_key.into(), signal.into());
            rv.set(result_obj.into());
        }
        Err(e) => {
            let err_obj = v8::Object::new(scope);
            let error = v8::Number::new(scope, -1.0);
            let status_key = v8::String::new(scope, "status").unwrap();
            err_obj.set(scope, status_key.into(), error.into());
            let err_msg = v8::String::new(scope, &e.to_string()).unwrap();
            let error_key = v8::String::new(scope, "error").unwrap();
            let err_v = v8::Exception::error(scope, err_msg);
            err_obj.set(scope, error_key.into(), err_v.into());
            result_obj = err_obj;
            rv.set(result_obj.into());
        }
    }
}

fn cp_exec_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let cmd = get_str(scope, args.get(0)).unwrap_or_default();
    let mut command = std::process::Command::new("sh");
    command.arg("-c").arg(&cmd);
    match command.output() {
        Ok(output) => {
            let stdout_s = String::from_utf8_lossy(&output.stdout);
            let out = v8::String::new(scope, &stdout_s).unwrap();
            rv.set(out.into());
        }
        Err(e) => {
            let error_msg = format!("execSync failed: {}", e);
            let msg = v8::String::new(scope, &error_msg).unwrap();
            let exception = v8::Exception::error(scope, msg);
            scope.throw_exception(exception);
        }
    }
}

fn cp_spawn_unsupported_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    let error_msg = "honey: async child_process.spawn is not supported yet (use spawnSync)";
    let msg = v8::String::new(scope, error_msg).unwrap();
    let exception = v8::Exception::error(scope, msg);
    scope.throw_exception(exception);
}

// ---------- tty ----------

fn tty_isatty_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    rv.set(v8::Boolean::new(scope, false).into());
}

// ---------- crypto ----------

fn crypto_hash_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // Hash init with FNV-1a basis (not secure, dev runtime only)
    let algorithm = get_str(scope, _args.get(0)).unwrap_or_default();
    let _ = algorithm;
    let hash_obj = v8::Object::new(scope);
    set_fn(scope, &hash_obj, "update", crypto_hash_update_callback);
    set_fn(scope, &hash_obj, "digest", crypto_hash_digest_callback);
    CRYPTO_HASH_STATE.with(|s| *s.borrow_mut() = 0xcbf29ce484222325u64);
    rv.set(hash_obj.into());
}

thread_local! {
    static CRYPTO_HASH_STATE: RefCell<u64> = RefCell::new(0xcbf29ce484222325u64);
}

fn crypto_hash_update_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    let _ = &args;
}

fn crypto_hash_digest_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state = CRYPTO_HASH_STATE.with(|s| *s.borrow());
    let hex = format!("{:016x}", state);
    let out = v8::String::new(scope, &hex).unwrap();
    rv.set(out.into());
}

fn crypto_random_bytes_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let n = args.get(0).uint32_value(scope).unwrap_or(16) as usize;
    let mut data: Vec<u8> = Vec::with_capacity(n);
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs() << 17))
        .unwrap_or(42);
    for _ in 0..n {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        data.push((seed >> 33) as u8);
    }
    let backing = v8::ArrayBuffer::new_backing_store_from_vec(data);
    let ab = v8::ArrayBuffer::with_backing_store(scope, &backing.make_shared());
    let arr = v8::Uint8Array::new(scope, ab, 0, ab.byte_length()).unwrap();
    rv.set(arr.into());
}

fn crypto_random_uuid_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs() << 17))
        .unwrap_or(7);
    let mut hex = String::new();
    for i in 0..32 {
        if i == 8 || i == 12 || i == 16 || i == 20 {
            hex.push('-');
        }
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        hex.push_str(&format!("{:x}", (seed >> 33) & 0xf));
    }
    let out = v8::String::new(scope, &hex).unwrap();
    rv.set(out.into());
}

// ---------- stream stubs ----------

fn stream_stub_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // Construct a plain object with emit-style methods for `extends`-based use
    let obj = v8::Object::new(scope);
    set_fn(scope, &obj, "on", noop_callback);
    set_fn(scope, &obj, "once", noop_callback);
    set_fn(scope, &obj, "emit", noop_callback);
    set_fn(scope, &obj, "pipe", noop_callback);
    set_fn(scope, &obj, "write", noop_callback);
    set_fn(scope, &obj, "end", noop_callback);
    set_fn(scope, &obj, "push", noop_callback);
    set_fn(scope, &obj, "destroy", noop_callback);
    rv.set(obj.into());
}

// ---------- process helpers ----------

fn process_cwd_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let out = v8::String::new(scope, &cwd).unwrap();
    rv.set(out.into());
}

fn process_exit_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    let code = if args.length() > 0 && args.get(0).is_number() {
        args.get(0).number_value(scope).unwrap_or(0.0) as i32
    } else {
        0
    };
    // Request termination: record the exit code and unwind JS execution by
    // throwing a sentinel. The run loop converts it into an exit code; this
    // keeps std::process::exit out of library code so a daemon can survive.
    EXIT_REQUEST.with(|e| e.set(Some(code)));
    let msg = v8::String::new(scope, "__honey_exit__").unwrap();
    let exc = v8::Exception::error(scope, msg);
    scope.throw_exception(exc);
}

fn process_hrtime_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let arr = v8::Array::new(scope, 2);
    let secs = v8::Number::new(scope, now.as_secs() as f64);
    let nanos = v8::Number::new(scope, now.subsec_nanos() as f64);
    let i0 = v8::Number::new(scope, 0.0);
    let i1 = v8::Number::new(scope, 1.0);
    arr.set(scope, i0.into(), secs.into());
    arr.set(scope, i1.into(), nanos.into());
    rv.set(arr.into());
}

fn process_hrtime_bigint_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    rv.set(v8::BigInt::new_from_u64(scope, nanos).into());
}

fn stdout_write_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    for i in 0..args.length() {
        if let Some(s) = get_str(scope, args.get(i)) {
            print!("{}", s);
        }
    }
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
}

fn stderr_write_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    for i in 0..args.length() {
        if let Some(s) = get_str(scope, args.get(i)) {
            eprint!("{}", s);
        }
    }
    use std::io::Write as _;
    let _ = std::io::stderr().flush();
}

// ---------- TextEncoder / TextDecoder ----------

fn text_decoder_ctor_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _mut_rv: v8::ReturnValue,
) {
    let this = args.this();
    let encoding = get_str(scope, args.get(0))
        .map(|e| e.to_lowercase())
        .unwrap_or_else(|| "utf-8".to_string());
    let enc_s = v8::String::new(scope, &encoding).unwrap();
    let enc_key = v8::String::new(scope, "encoding").unwrap();
    this.set(scope, enc_key.into(), enc_s.into());
    set_fn(scope, &this, "decode", text_decoder_decode_callback);
}

fn text_decoder_decode_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let data_val = args.get(0);
    if let Some(bytes) = typed_array_bytes(scope, data_val) {
        let s = String::from_utf8_lossy(&bytes);
        let out = v8::String::new(scope, &s).unwrap();
        rv.set(out.into());
    } else if let Some(s) = get_str(scope, data_val) {
        let out = v8::String::new(scope, &s).unwrap();
        rv.set(out.into());
    } else {
        let out = v8::String::new(scope, "").unwrap();
        rv.set(out.into());
    }
}

fn text_encoder_ctor_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _mut_rv: v8::ReturnValue,
) {
    let this = args.this();
    set_fn(scope, &this, "encode", text_encoder_encode_callback);
    set_fn(scope, &this, "encodeInto", text_encoder_encode_into_callback);
    let enc = v8::String::new(scope, "utf-8").unwrap();
    let enc_key = v8::String::new(scope, "encoding").unwrap();
    this.set(scope, enc_key.into(), enc.into());
}

fn text_encoder_encode_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let input = args.get(0);
    let fallback = v8::String::new(scope, "");
    let text_val: v8::Local<v8::Value> = if input.is_undefined() && fallback.is_some() {
        fallback.unwrap().into()
    } else {
        input
    };
    let text = get_str(scope, text_val).unwrap_or_default();
    let bytes = text.into_bytes();
    let backing = v8::ArrayBuffer::new_backing_store_from_vec(bytes);
    let ab = v8::ArrayBuffer::with_backing_store(scope, &backing.make_shared());
    let arr = v8::Uint8Array::new(scope, ab, 0, ab.byte_length()).unwrap();
    rv.set(arr.into());
}

fn text_encoder_encode_into_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let source = get_str(scope, args.get(0)).unwrap_or_default();
    let dest = args.get(1);
    let bytes = source.as_bytes().to_vec();
    let written = typed_array_bytes(scope, dest)
        .map(|mut buf| {
            let n = buf.len().min(bytes.len());
            if let Ok(ta) = v8::Local::<v8::Uint8Array>::try_from(dest) {
                let copied = ta.copy_contents(&mut buf);
                let _ = copied;
            }
            n
        })
        .unwrap_or(0);
    let result = v8::Object::new(scope);
    let read_val = v8::Number::new(scope, bytes.len() as f64);
    let read_key = v8::String::new(scope, "read").unwrap();
    result.set(scope, read_key.into(), read_val.into());
    let write_val = v8::Number::new(scope, written as f64);
    let write_key = v8::String::new(scope, "written").unwrap();
    result.set(scope, write_key.into(), write_val.into());
    rv.set(result.into());
}

// ---------- Buffer / timers / misc ----------

fn buffer_from_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let data_val = args.get(0);
    if let Some(bytes) = typed_array_bytes(scope, data_val) {
        let backing = v8::ArrayBuffer::new_backing_store_from_vec(bytes);
        let ab = v8::ArrayBuffer::with_backing_store(scope, &backing.make_shared());
        let arr = v8::Uint8Array::new(scope, ab, 0, ab.byte_length()).unwrap();
        rv.set(arr.into());
    } else if let Some(s) = get_str(scope, data_val) {
        let bytes = s.into_bytes();
        let backing = v8::ArrayBuffer::new_backing_store_from_vec(bytes);
        let ab = v8::ArrayBuffer::with_backing_store(scope, &backing.make_shared());
        let arr = v8::Uint8Array::new(scope, ab, 0, ab.byte_length()).unwrap();
        rv.set(arr.into());
    } else {
        let backing = v8::ArrayBuffer::new_backing_store_from_vec(vec![0u8]);
        let ab = v8::ArrayBuffer::with_backing_store(scope, &backing.make_shared());
        let zero = v8::Uint8Array::new(scope, ab, 0, 1).unwrap();
        rv.set(zero.into());
    }
}

fn buffer_is_buffer_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let v = args.get(0);
    rv.set(v8::Boolean::new(scope, v.is_uint8_array()).into());
}

fn set_timer_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // Timers are not scheduled yet; return a dummy handle with ref/unref
    let handle = v8::Object::new(scope);
    set_fn(scope, &handle, "ref", returning_this_callback);
    set_fn(scope, &handle, "unref", returning_this_callback);
    rv.set(handle.into());
}

fn returning_this_callback(
    _scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    rv.set(args.this().into());
}

fn noop_callback(
    _scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
}

fn sync_call_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _mut_rv: v8::ReturnValue,
) {
    if args.length() > 0 {
        let f = args.get(0);
        if let Ok(func) = v8::Local::<v8::Function>::try_from(f) {
            let undefined = v8::undefined(scope);
            func.call(scope, undefined.into(), &[]);
        }
    }
}

fn console_log(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    for i in 0..args.length() {
        let arg = args.get(i);
        let str = arg.to_string(scope).unwrap();
        let str = str.to_rust_string_lossy(scope);
        if i > 0 { print!(" "); }
        print!("{}", str);
    }
    println!();
}

fn console_error(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    for i in 0..args.length() {
        let arg = args.get(i);
        let str = arg.to_string(scope).unwrap();
        let str = str.to_rust_string_lossy(scope);
        if i > 0 { eprint!(" "); }
        eprint!("{}", str);
    }
    eprintln!();
}
// ---------- honey test shim ----------
//
// The vitest-style harness (it/describe/expect + event-loop reporting) used
// to be re-embedded in every test file's preload wrapper, so every single
// test file paid a full parse+exec of the same ~7KB. It now lives here and
// registers once per engine as `globalThis.__honeyTestBegin` (whose call
// recreates ALL closure state, so per-file isolation is identical to the
// old per-file shim). Wrappers shrink to a few lines; the daemon/worker
// requests registration before evaluating a marker-carrying preload.

/// The test harness as a fresh-closure factory. `globalThis.__honeyTestBegin ??= (f)`
/// registers it once per engine; every wrapper then calls `__honeyTestBegin()`
/// to get per-file clean it/expect/describe state, same as the old per-file shim.
pub const TEST_SHIM: &str = r##"function () {
  "use strict";
  var _stack = [{ name: "", befores: [], afters: [] }];
  var _snapSeq = 0;
  var _current = "";

  function emit(o) {
    try { console.log("@@HONEY@@" + JSON.stringify(o)); } catch (e) { /* reporting must never throw */ }
  }
  function isThenable(v) { return !!v && typeof v.then === "function"; }

  function fullTitle(name) {
    var parts = [];
    for (var i = 0; i < _stack.length; i++) if (_stack[i].name) parts.push(_stack[i].name);
    if (name) parts.push(String(name));
    return parts.join(" > ");
  }

  // Deterministic human-readable serialization (object keys sorted).
  function stableValue(v, pad) {
    if (v === undefined) return "undefined";
    if (typeof v === "bigint") return String(v) + "n";
    if (typeof v === "number" || typeof v === "boolean") return String(v);
    if (typeof v === "symbol") return v.toString();
    if (typeof v === "function") return "[Function" + (v.name ? ": " + v.name : "") + "]";
    if (v === null) return "null";
    if (typeof v === "string") return JSON.stringify(v);
    if (v instanceof Date) return "Date(" + JSON.stringify(v.toISOString()) + ")";
    var inner = pad + "  ";
    if (Array.isArray(v)) {
      if (v.length === 0) return "[]";
      var items = v.map(function (x) { return stableValue(x, inner); });
      return "[\n" + inner + items.join(",\n" + inner) + "\n" + pad + "]";
    }
    if (v instanceof Map) return "Map(" + stableValue(Array.from(v.entries()), pad) + ")";
    if (v instanceof Set) return "Set(" + stableValue(Array.from(v.values()), pad) + ")";
    var keys = Object.keys(v).sort();
    if (keys.length === 0) return "{}";
    var body = keys.map(function (k) {
      return JSON.stringify(k) + ": " + (v[k] === undefined ? "undefined" : stableValue(v[k], inner));
    });
    return "{\n" + inner + body.join(",\n" + inner) + "\n" + pad + "}";
  }

  function deepEqual(a, b) {
    if (a === b) return true;
    if (typeof a === "number" && typeof b === "number") return a !== a && b !== b;
    if (a === null || b === null || typeof a !== "object" || typeof b !== "object") return false;
    if (Array.isArray(a) !== Array.isArray(b)) return false;
    if (a instanceof Date && b instanceof Date) return a.getTime() === b.getTime();
    if (a instanceof Map && b instanceof Map) {
      if (a.size !== b.size) return false;
      var ok1 = true;
      a.forEach(function (val, key) { if (ok1 && !(b.has(key) && deepEqual(val, b.get(key)))) ok1 = false; });
      return ok1;
    }
    if (a instanceof Set && b instanceof Set) {
      if (a.size !== b.size) return false;
      var ok2 = true;
      a.forEach(function (x) { if (ok2 && !b.has(x)) ok2 = false; });
      return ok2;
    }
    if (Array.isArray(a)) {
      if (a.length !== b.length) return false;
      for (var i = 0; i < a.length; i++) if (!deepEqual(a[i], b[i])) return false;
      return true;
    }
    var ka = Object.keys(a), kb = Object.keys(b);
    if (ka.length !== kb.length) return false;
    for (var j = 0; j < ka.length; j++) {
      var k = ka[j];
      if (!Object.prototype.hasOwnProperty.call(b, k) || !deepEqual(a[k], b[k])) return false;
    }
    return true;
  }

  function Assertion(actual, negated) { this.actual = actual; this.negated = !!negated; }
  Assertion.prototype._check = function (cond, messages) {
    var ok = this.negated ? !cond : !!cond;
    if (!ok) {
      var msg = (this.negated && messages.not) ? messages.not : messages.pos;
      var err = new Error(msg);
      err.__honeyAssertion = true;
      throw err;
    }
    return this;
  };
  Object.defineProperty(Assertion.prototype, "not", {
    get: function () { return new Assertion(this.actual, !this.negated); }
  });

  function expect(actual) { return new Assertion(actual); }

  Assertion.prototype.toBe = function (exp) {
    return this._check(Object.is(this.actual, exp), {
      pos: "expect(received).toBe(expected)\n  received: " + stableValue(this.actual, "  ") + "\n  expected: " + stableValue(exp, "  "),
      not: "expect(received).not.toBe(expected)\n  received (must differ from expected): " + stableValue(this.actual, "  ")
    });
  };
  Assertion.prototype.toEqual = function (exp) {
    return this._check(deepEqual(this.actual, exp), {
      pos: "expect(received).toEqual(expected)\n  received: " + stableValue(this.actual, "  ") + "\n  expected: " + stableValue(exp, "  "),
      not: "expect(received).not.toEqual(expected)\n  both deep-equal: " + stableValue(this.actual, "  ")
    });
  };
  Assertion.prototype.toBeTruthy = function () {
    return this._check(!!this.actual, {
      pos: "expect(received).toBeTruthy()\n  received: " + stableValue(this.actual, "  "),
      not: "expect(received).not.toBeTruthy()\n  received: " + stableValue(this.actual, "  ")
    });
  };
  Assertion.prototype.toBeFalsy = function () {
    return this._check(!this.actual, {
      pos: "expect(received).toBeFalsy()\n  received: " + stableValue(this.actual, "  "),
      not: "expect(received).not.toBeFalsy()\n  received: " + stableValue(this.actual, "  ")
    });
  };
  Assertion.prototype.toBeNull = function () {
    return this._check(this.actual === null, { pos: "expect(received).toBeNull()\n  received: " + stableValue(this.actual, "  ") });
  };
  Assertion.prototype.toBeUndefined = function () {
    return this._check(this.actual === undefined, { pos: "expect(received).toBeUndefined()\n  received: " + stableValue(this.actual, "  ") });
  };
  Assertion.prototype.toBeDefined = function () {
    return this._check(this.actual !== undefined, { pos: "expect(received).toBeDefined()\n  received: " + stableValue(this.actual, "  ") });
  };
  Assertion.prototype.toContain = function (exp) {
    var found = false;
    if (typeof this.actual === "string") found = exp !== undefined && this.actual.indexOf(exp) !== -1;
    else if (Array.isArray(this.actual)) {
      for (var i = 0; i < this.actual.length; i++) if (deepEqual(this.actual[i], exp)) { found = true; break; }
    }
    return this._check(found, {
      pos: "expect(received).toContain(expected)\n  received: " + stableValue(this.actual, "  ") + "\n  expected to contain: " + stableValue(exp, "  ")
    });
  };
  Assertion.prototype.toHaveLength = function (n) {
    var len = this.actual == null ? undefined : this.actual.length;
    return this._check(len === n, {
      pos: "expect(received).toHaveLength(" + String(n) + ")\n  received: " + stableValue(this.actual, "  ") + " (length " + String(len) + ")"
    });
  };
  Assertion.prototype.toMatch = function (exp) {
    var actual = typeof this.actual === "string" ? this.actual : String(this.actual);
    var found = typeof exp === "string" ? actual.indexOf(exp) !== -1 : new RegExp(exp).test(actual);
    return this._check(found, {
      pos: "expect(received).toMatch(expected)\n  received: " + stableValue(actual, "  ") + "\n  expected to match: " + stableValue(exp, "  ")
    });
  };
  Assertion.prototype.toThrow = function (msgPart) {
    if (typeof this.actual !== "function") throw new Error("expect(...).toThrow(): received must be a function");
    var threw = false, message = "";
    try { this.actual(); } catch (e) { threw = true; message = (e && e.message) || String(e); }
    if (this.negated) {
      if (threw) throw new Error("expect(received).not.toThrow()\n  function threw: " + message);
      return this;
    }
    if (!threw) throw new Error("expect(received).toThrow()\n  function did not throw");
    if (msgPart !== undefined && message.indexOf(String(msgPart)) === -1) {
      throw new Error("expect(received).toThrow(" + JSON.stringify(String(msgPart)) + ")\n  thrown message: " + message);
    }
    return this;
  };
  Assertion.prototype.toMatchSnapshot = function () {
    _snapSeq++;
    var key = (_current || "(file)") + " #" + _snapSeq;
    emit({ s: "snapshot", t: _current, k: key, v: stableValue(this.actual, "") });
    return this;
  };

  function describe(name, fn) {
    if (typeof fn !== "function") throw new Error("describe(name, fn): fn must be a function");
    _stack.push({ name: String(name), befores: [], afters: [] });
    try { fn(); } finally { _stack.pop(); }
  }

  function beforeEach(fn) { _stack[_stack.length - 1].befores.push(fn); }
  function afterEach(fn) { _stack[_stack.length - 1].afters.push(fn); }

  function it(name, fn) {
    if (typeof fn !== "function") throw new Error("it(name, fn): fn must be a function");
    var title = fullTitle(name);
    var befores = [], afters = [];
    for (var i = 0; i < _stack.length; i++) {
      befores = befores.concat(_stack[i].befores);
      afters = _stack[i].afters.concat(afters);
    }
    _current = title;
    (async function () {
      var err = null;
      try {
        for (var i = 0; i < befores.length; i++) {
          var b = befores[i]();
          if (isThenable(b)) await b;
        }
        var r = fn();
        if (isThenable(r)) await r;
      } catch (e) {
        err = e;
      }
      var afterErr = null;
      try {
        for (var j = 0; j < afters.length; j++) {
          var x = afters[j]();
          if (isThenable(x)) await x;
        }
      } catch (e2) {
        afterErr = e2;
      }
      if (err !== null || afterErr !== null) {
        var e = err || afterErr;
        emit({ s: "fail", t: title, m: String((e && e.message) || e) });
      } else {
        emit({ s: "pass", t: title });
      }
    })();
  }

  globalThis.expect = expect;
  globalThis.describe = describe;
  globalThis.it = it;
  globalThis.test = it;
  globalThis.beforeEach = beforeEach;
  globalThis.afterEach = afterEach;
};"##;

/// The wrapper marker the daemon/worker look for to know a preload runs
/// under the (now engine-registered) test shim.
pub const TEST_SHIM_MARKER: &str = "/*HONEY_TEST_SHIM_V2*/";

thread_local! {
    /// Bumped on every engine build so shim registration follows whichever
    /// isolate is current on this thread.
    static ENGINE_GEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static SHIM_GEN: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
    static TEST_SHIM_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A test-preload run is pending: the next script execution registers the
/// shim inside whichever engine it lands in (run_script may rebuild engines,
/// so registration happens right before the wrapper runs).
pub fn request_test_shim() {
    TEST_SHIM_PENDING.with(|p| p.set(true));
}

/// Late registration hook called from run_script right before the main
/// script executes. Idempotent per engine generation.
pub fn execute_pending_test_shim(runtime: &mut JsRuntime) {
    if !TEST_SHIM_PENDING.with(std::cell::Cell::take) {
        return;
    }
    let genid = ENGINE_GEN.with(|g| g.get());
    if SHIM_GEN.with(|g| g.get()) == genid {
        return;
    }
    SHIM_GEN.with(|g| g.set(genid));
    let code = format!("globalThis.__honeyTestBegin ??= {};", TEST_SHIM);
    let _ = runtime.execute_script("honey:testshim", code);
}
