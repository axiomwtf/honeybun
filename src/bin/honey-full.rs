use clap::{Parser, Subcommand};
use honey::daemon;
use honey::{PackageManager, Runtime};

#[derive(Parser)]
#[command(name = "honey")]
#[command(bin_name = "honey")]
#[command(author = "AXIOM")]
#[command(version = "0.1.0")]
#[command(about = "Up to 8.4x faster than Bun. Up to 23x faster than Node. One binary. One toolchain.", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Install dependencies
    Install {
        /// Production only
        #[arg(short = 'p')]
        production: bool,
        /// Global install (not supported here — use `hxney npm install -g`)
        #[arg(short = 'g', long = "global")]
        global: bool,
        /// Package names — only valid with `hxney npm install <pkg>`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        packages: Vec<String>,
    },
    /// Add a package
    Add {
        /// Package name(s)
        package: Vec<String>,
        /// Install as dev dependency
        #[arg(short = 'D')]
        dev: bool,
    },
    /// Remove a package
    Remove {
        /// Package name
        package: String,
    },
    /// Run a file
    Run {        /// File to run
        file: String,
        /// Re-run whenever the file or its dependencies change
        #[arg(short = 'w', long)]
        watch: bool,
        /// Arguments to pass to the file
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Bundle with Cinnamon
    Build {
        /// Entry file (default: src/index.ts, falling back to conventional
        /// entry names)
        entry: Option<String>,
        /// Output directory
        #[arg(short = 'o')]
        output: Option<String>,
        /// Production mode: minify, define process.env.NODE_ENV=production
        /// (with static-if elimination), evaluate static branches, and emit
        /// an external sourcemap.
        #[arg(short = 'p', long)]
        production: bool,
        /// Minify (default: on in production, off otherwise)
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        minify: Option<bool>,
        /// Sourcemap mode: linked (map + comment), external (map, no
        /// comment), inline (data URL), none (default: opposite of --minify)
        #[arg(long = "sourcemap")]
        sourcemap: Option<String>,
        /// Emission target: browser (process guard, guarded entry require)
        /// or node (CommonJS output, assumes the host runtime)
        #[arg(long)]
        target: Option<String>,
        /// Compile-time define, `--define key=value` (repeatable); the value
        /// is inserted verbatim as `true`/`false`/`null`/number/quoted string
        #[arg(long = "define")]
        define: Vec<String>,
        /// Re-bundle whenever an input file changes
        #[arg(short = 'w', long)]
        watch: bool,
    },
    /// Run tests
    Test {
        /// Filter by name
        #[arg(short = 'f', long)]
        filter: Option<String>,
        /// Include slow tests
        #[arg(short = 's', long)]
        slow: bool,
        /// Watch mode
        #[arg(short = 'w', long)]
        watch: bool,
    },
    /// JS to TS migration
    Migrate {
        /// Single file migration
        #[arg(short = 'f')]
        file: Option<String>,
    },
    /// Start the persistent daemon
    Start,
    /// Stop the daemon
    Stop,
    /// Manage the daemon service (systemd user unit / launchd agent).
    /// Installs and enables it with --now by default; honey also does this
    /// automatically on its first run.
    Service {
        /// Remove the service
        #[arg(long)]
        uninstall: bool,
        /// Show status only
        #[arg(long)]
        status: bool,
    },
}

/// No-daemon / trace-mode watcher: run on the current thread, stat-poll the
/// dep set the runtime recorded, and re-run on change. Used only when the
/// daemon-backed watch session is unavailable.
fn inline_watch(file: &str, args: &[String]) {
    let runtime = Runtime::new_warm();
    loop {
        match runtime.run_file(file, args) {
            Ok(code) if code != 0 => eprintln!("[watch] exit code {code}"),
            Ok(_) => {}
            Err(e) => eprintln!("error: {e:#}"),
        }
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        let deps = honey::runtime::recorded_dep_mtimes();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if deps
                .iter()
                .any(|(p, m)| honey::runtime::stat_path_mtime(p) != Some(*m))
            {
                break;
            }
        }
    }
}

const TEST_EXTENSIONS: [&str; 2] = [".test.", ".spec."];

/// Internal: create the V8 context startup snapshot consumed by build.rs.
/// Builds a pristine engine (HONEY_MKSNAPSHOT is already set in this process
/// so snapshot_active() is false), runs warmup JS to bake ICs/feedback into
/// the context, serializes it, and writes the blob for the next build.
fn mksnapshot(out: &str) {
    let blob = honey::runtime::make_snapshot_blob();
    if blob.is_empty() {
        eprintln!("error: snapshot serialization produced an empty blob");
        std::process::exit(1);
    }
    if let Some(parent) = std::path::Path::new(out).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(out, &blob) {
        eprintln!("error: cannot write {out}: {e}");
        std::process::exit(1);
    }
    eprintln!("wrote {} ({} bytes)", out, blob.len());
}

/// A `*.test.{js,ts}` / `*.spec.{js,ts}` file, with `.slow.` marking slow tests.
fn is_test_file(name: &str) -> bool {
    if name.ends_with(".d.ts") || !(name.ends_with(".js") || name.ends_with(".ts")) {
        return false;
    }
    TEST_EXTENSIONS.iter().any(|ext| name.contains(ext))
}

fn is_slow_test(name: &str) -> bool {
    name.contains(".slow.")
}

/// Directories never searched for test files.
const EXCLUDED_TEST_DIRS: [&str; 4] = [".cinm", "node_modules", "target", ".git"];

fn collect_test_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if !EXCLUDED_TEST_DIRS.contains(&name.as_ref()) {
                subdirs.push(path);
            }
        } else if is_test_file(&name) {
            out.push(path);
        }
    }
    for d in subdirs {
        collect_test_files(&d, out);
    }
}

/// Collapse an error to a one-line summary: the daemon `error` field if set,
/// else the first non-empty stderr line, else the exit code.
fn error_summary(exit_code: i32, error: Option<&str>, stderr: &str) -> String {
    let raw = error
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            stderr
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(String::from)
        })
        .unwrap_or_else(|| format!("exit code {exit_code}"));
    let truncated: String = raw.chars().take(160).collect();
    if truncated.len() < raw.len() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

const RESULT_PREFIX: &str = "@@HONEY@@";

/// The test shim, evaluated before the test module on the same engine:
/// expect/describe/it/test/beforeEach/afterEach with plain assertions
/// (deep equal, throws, async), plus toMatchSnapshot. Each test and each
/// snapshot is reported as a single machine-readable stdout line so the CLI
/// can attribute pass/fail per file; everything after the module body is
/// flushed by the runtime's event loop, so asynchronous tests are included.

/// Mirrors the runtime's ESM handling for the main module, so the shim
/// wrapper can be generated in the matching flavor. `import type` and other
/// constructs removed by TS transpilation can cause a false positive here;
/// in that case the wrapper re-loads the file through the ESM path, which
/// the runtime also uses for such files (via its package.json check).
fn looks_like_esm_source(source: &str) -> bool {
    for line in source.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
            continue;
        }
        if t.starts_with("export ") || t.starts_with("export{") || t.starts_with("export *") || t.starts_with("export*") {
            return true;
        }
        if (t.starts_with("import ") || t.starts_with("import{") || t.starts_with("import *") || t.starts_with("import*"))
            && !t.starts_with("import(") {
            return true;
        }
    }
    false
}

/// HONEY_SNAPSHOT_UPDATE=1 forces snapshot rewrite on mismatch.
fn snapshot_update_enabled() -> bool {
    matches!(std::env::var("HONEY_SNAPSHOT_UPDATE").as_deref(), Ok("1"))
}

/// Nearest package.json "type": "module" (mirrors the runtime's check).
fn nearest_package_json_is_module(from: &std::path::Path) -> bool {
    honey::runtime::nearest_package_json_is_module(from)
}

/// The ESM/CJS flavor the engine will use for a main module (mirrors the
/// runtime's decision: .mjs/.mts, nearest package.json "type", source sniff).
fn main_module_is_esm(path: &std::path::Path) -> bool {
    honey::runtime::main_module_is_esm(path)
}

/// The JS wrapper loaded instead of a test file: it installs the shim, then
/// loads the actual test module, so shim and tests share one engine. The
/// load is `require(...)` (CJS wrapper) or `await import(...)` (ESM wrapper,
/// `export {}` marks it ESM for the engine's sniff).
fn test_preload_for(path: &std::path::Path) -> Option<String> {
    honey::runtime::test_preload_for(path)
}

/// One test file run through the daemon's preload path (worker subprocess on
/// the daemon side) with a worker-subprocess cold fallback.
fn run_single_test(file: &str, preload: &str) -> daemon::Response {
    if let Some(resp) = daemon::run_with_preload_via_daemon(file, preload) {
        return resp;
    }
    // No daemon (HONEY_NO_DAEMON, trace mode, or dead socket): a fresh worker
    // subprocess per file keeps stdio capture isolated across parallel runs.
    let req = daemon::Request {
        cmd: "run".into(),
        file: file.into(),
        cwd: std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        preload: preload.into(),
        ..Default::default()
    };
    daemon::run_in_worker(&req)
}

/// Human-readable result lines collected for one test file.
struct TestOutcome {
    failed: usize,
    details: Vec<String>,
    snapshots_written: Vec<String>,
}

impl TestOutcome {
    fn ok(&self, resp: &daemon::Response) -> bool {
        self.failed == 0 && resp.exit_code == 0 && resp.error.is_none()
    }
}

/// A parsed shim result line.
fn json_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("?")
        .to_string()
}

/// Snapshots are stored in `<test file>.snap` as human-readable sections:
///
///   # honey snapshots for <test file>
///
///   [<test title> #<n>]
///   <serialized value>
///
/// Header lines start with `[`, end with `]` and contain ` #` (snapshot
/// values are pretty-printed multi-line, so no value line collides).
fn snap_header_line(line: &str) -> Option<String> {
    if line.starts_with('[') && line.ends_with(']') && line.contains(" #") {
        Some(line[1..line.len() - 1].to_string())
    } else {
        None
    }
}

fn snapshot_path_for(test_file: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{test_file}.snap"))
}

fn load_snap_sections(path: &std::path::Path) -> Vec<(String, String)> {
    let mut sections = Vec::new();
    let Ok(content) = std::fs::read_to_string(path) else {
        return sections;
    };
    let mut current: Option<(String, Vec<String>)> = None;
    for line in content.lines() {
        if let Some(key) = snap_header_line(line) {
            if let Some((k, mut body)) = current.take() {
                // Drop the blank separator line(s) after a section value.
                while body.last().map(|l| l.is_empty()).unwrap_or(false) {
                    body.pop();
                }
                sections.push((k, body.join("\n")));
            }
            current = Some((key, Vec::new()));
        } else if let Some((_, body)) = current.as_mut() {
            body.push(line.to_string());
        }
    }
    if let Some((k, mut body)) = current.take() {
        while body.last().map(|l| l.is_empty()).unwrap_or(false) {
            body.pop();
        }
        sections.push((k, body.join("\n")));
    }
    sections
}

fn write_snap_sections(path: &std::path::Path, test_file: &str, sections: &[(String, String)]) {
    let mut out = format!("# honey snapshots for {test_file}\n\n");
    for (key, value) in sections {
        out.push_str(&format!("[{key}]\n{value}\n\n"));
    }
    let _ = std::fs::write(path, out);
}

/// Parse a test file's run response: pass/fail per shim result line, snapshot
/// sections created/updated/compared against `<test file>.snap`.
fn process_test_response(resp: &daemon::Response, test_file: &str, update: bool) -> TestOutcome {
    let mut outcome = TestOutcome { failed: 0, details: Vec::new(), snapshots_written: Vec::new() };
    let snap_path = snapshot_path_for(test_file);
    let mut snap_sections = load_snap_sections(&snap_path);
    let mut snap_dirty = false;

    for line in resp.stdout.lines() {
        let Some(rest) = line.strip_prefix(RESULT_PREFIX) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(rest) else { continue };
        match v.get("s").and_then(|s| s.as_str()) {
            Some("fail") => {
                outcome.failed += 1;
                let title = json_str(&v, "t");
                let msg = json_str(&v, "m");
                outcome.details.push(format!("FAIL {title}\n    {}", msg.replace('\n', "\n    ")));
            }
            Some("snapshot") => {
                let key = json_str(&v, "k");
                let value = json_str(&v, "v");
                match snap_sections.iter().position(|(k, _)| *k == key) {
                    Some(idx) if snap_sections[idx].1 == value => {} // match
                    Some(idx) => {
                        if update {
                            snap_sections[idx].1 = value;
                            snap_dirty = true;
                            outcome.snapshots_written.push(key);
                        } else {
                            outcome.failed += 1;
                            outcome.details.push(format!(
                                "FAIL snapshot mismatch: {key}\n    stored (run with HONEY_SNAPSHOT_UPDATE=1 to overwrite):\n{}\n    received:\n{}",
                                indent_lines(&snap_sections[idx].1, "      "),
                                indent_lines(&value, "      ")
                            ));
                        }
                    }
                    None => {
                        snap_sections.push((key.clone(), value));
                        snap_dirty = true;
                        outcome.snapshots_written.push(key);
                    }
                }
            }
            _ => {}
        }
    }
    if snap_dirty {
        write_snap_sections(&snap_path, test_file, &snap_sections);
    }
    outcome
}

fn indent_lines(text: &str, prefix: &str) -> String {
    text.lines().map(|l| format!("{prefix}{l}")).collect::<Vec<_>>().join("\n")
}

fn rel_display(file: &str, cwd: &std::path::Path) -> String {
    std::path::Path::new(file)
        .strip_prefix(cwd)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| file.to_string())
}

/// Print one file's result block; returns file-level (passed, failed) counts.
fn print_file_result(rel: &str, resp: &daemon::Response, test_file: &str, update: bool) -> (usize, usize) {
    let outcome = process_test_response(resp, test_file, update);
    if outcome.ok(resp) {
        println!("PASS {rel}");
        for note in &outcome.snapshots_written {
            println!("  snapshot recorded: {note}");
        }
        (1, 0)
    } else if !outcome.details.is_empty() {
        let plural = if outcome.failed == 1 { "test" } else { "tests" };
        println!("FAIL {rel} — {} failing {plural}", outcome.failed);
        for d in &outcome.details {
            println!("{}", indent_lines(d, "  "));
        }
        (0, 1)
    } else {
        println!("FAIL {rel} — {}", error_summary(resp.exit_code, resp.error.as_deref(), &resp.stderr));
        if !resp.stderr.trim().is_empty() {
            println!("{}", indent_lines(resp.stderr.trim_end(), "  "));
        }
        (0, 1)
    }
}

/// Discover and run the project's test files. Runs execute in parallel:
/// one tokio task per file (concurrency capped at num_cpus via a semaphore,
/// one worker subprocess each through the daemon preload path), results are
/// buffered and printed deterministically in original path order. Returns
/// the process exit code (0 = all passed, 1 = any failure or none found).
fn run_test_suite(filter: Option<&str>, slow: bool, watch: bool) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    // `tests/` is the primary location; only fall back to a full walk of the
    // project when it yields nothing.
    let tests_dir = cwd.join("tests");
    if tests_dir.is_dir() {
        collect_test_files(&tests_dir, &mut files);
    }
    if files.is_empty() {
        collect_test_files(&cwd, &mut files);
    }
    files.sort();
    files.dedup();

    // Filter and display tests by their cwd-relative path so `--filter b`
    // matches `tests/b.test.js`.
    let display = |p: &std::path::Path| -> String {
        p.strip_prefix(&cwd)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned()
    };
    if let Some(f) = filter {
        files.retain(|p| display(p).contains(f));
    }
    let (to_run, skipped): (Vec<_>, Vec<_>) = files
        .into_iter()
        .partition(|p| slow || !is_slow_test(&p.file_name().unwrap_or_default().to_string_lossy()));

    if to_run.is_empty() && skipped.is_empty() {
        eprintln!("error: no test files found (*.test.js, *.test.ts, *.spec.js, *.spec.ts)");
        return 1;
    }

    println!(
        "running {} test{}...",
        to_run.len(),
        if to_run.len() == 1 { "" } else { "s" }
    );
    for p in &skipped {
        println!("SKIP {} (slow; pass --slow to include)", display(p));
    }

    // (absolute file path, JS preload wrapper) for each test file. Wrappers
    // are built LAZILY by the consumers: the daemon path never needs them
    // (the daemon reads the files anyway — see serve_test_batch), so paying
    // a read + ESM sniff per file here would tax the daemon path for nothing.
    let jobs: Vec<(String, String)> = to_run
        .iter()
        .map(|p| (p.to_string_lossy().into_owned(), String::new()))
        .collect();

    if watch {
        run_test_watch(&jobs, &cwd);
        return 0; // watch mode ends when the user interrupts (Ctrl+C)
    }

    let started = std::time::Instant::now();
    let (passed, failed) = run_test_batch(&jobs);
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    println!(
        "{passed} passed, {failed} failed ({ms:.1} ms total, results printed in path order)"
    );
    i32::from(failed > 0)
}

// Run a test batch: one tokio task per file, concurrency capped at num_cpus
// (semaphore), buffered results printed in original path order.
// `jobs` carries (path, preload-wrapper); an empty wrapper here means "build
// it lazily at use site" — the daemon path never wants wrappers, and the
// local fallbacks only need each one when its file actually runs.
fn run_test_batch(jobs: &[(String, String)]) -> (usize, usize) {
    // Resolve a possibly-lazy wrapper once the file is about to run.
    let preload_of = |p: &str, wrapped: &str| -> String {
        if wrapped.is_empty() {
            test_preload_for(std::path::Path::new(p)).unwrap_or_default()
        } else {
            wrapped.to_string()
        }
    };
    let sequential = std::env::var("HONEY_TEST_SEQUENTIAL").as_deref() == Ok("1");
    let update = snapshot_update_enabled();
    let cap = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);

    // Warm path: the daemon runs the whole suite sequentially in its retained
    // engine (one engine init for the suite instead of one worker process per
    // file). HONEY_TEST_WORKERS=1 forces the isolated worker path for suites
    // that depend on pristine per-file globals.
    let force_workers = std::env::var("HONEY_TEST_WORKERS")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    // Warm daemon path: send only the file list; the daemon builds the shim
    // wrappers itself (it reads the files anyway). Local non-daemon paths
    // (sequential/parallel workers below) need real wrappers, so those keep
    // building them client-side.
    if !force_workers {
        let bare: Vec<(String, String)> = jobs
            .iter()
            .map(|(f, _)| (f.clone(), String::new()))
            .collect();
        if let Some(results) = daemon::test_batch_via_daemon_with_snapshot(&bare, snapshot_update_enabled()) {
            let cwd = std::env::current_dir().unwrap_or_default();
            let mut passed = 0usize;
            let mut failed = 0usize;
            for (r, (file, _)) in results.into_iter().zip(jobs.iter()) {
                let rel = rel_display(file, &cwd);
                let (p, f) = print_file_result(&rel, &r, file, update);
                passed += p;
                failed += f;
            }
            return (passed, failed);
        }
    }

    let run_all = |rt: tokio::runtime::Runtime| -> (usize, usize) {
        rt.block_on(async move {
            let cwd = std::env::current_dir().unwrap_or_default();
            let resp = |e: tokio::task::JoinError| -> daemon::Response {
                daemon::Response {
                    exit_code: 1,
                    error: Some(format!("test task failed: {e}")),
                    ..Default::default()
                }
            };
            let print_result = |passed: &mut usize, failed: &mut usize, file: &str, r: daemon::Response| {
                let rel = rel_display(file, &cwd);
                let (p, f) = print_file_result(&rel, &r, file, update);
                *passed += p;
                *failed += f;
            };
            if sequential {
                // Hidden knob for benchmarking: files run one at a time.
                let mut passed = 0usize;
                let mut failed = 0usize;
                for (file, preload) in jobs {
                    let r = tokio::task::spawn_blocking({
                        let file = file.clone();
                        let preload = preload_of(&file, &preload);
                        move || run_single_test(&file, &preload)
                    })
                    .await
                    .unwrap_or_else(resp);
                    print_result(&mut passed, &mut failed, file, r);
                }
                return (passed, failed);
            }
            // Parallel: one tokio task per file, semaphore at num_cpus.
            let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(cap));
            let mut handles = Vec::new();
            for (file, preload) in jobs {
                let sem = sem.clone();
                let file = file.clone();
                let preload = preload_of(&file, &preload);
                handles.push(tokio::task::spawn(async move {
                    let _permit = sem.acquire().await;
                    tokio::task::spawn_blocking(move || run_single_test(&file, &preload))
                        .await
                        .unwrap_or_else(resp)
                }));
            }
            // Deterministic output: buffer results, print in input order.
            let mut passed = 0usize;
            let mut failed = 0usize;
            for (handle, (file, _)) in handles.into_iter().zip(jobs.iter()) {
                let r = handle
                    .await
                    .unwrap_or_else(|e| daemon::Response {
                        exit_code: 1,
                        error: Some(format!("test task failed: {e}")),
                        ..Default::default()
                    });
                print_result(&mut passed, &mut failed, file, r);
            }
            (passed, failed)
        })
    };

    match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => run_all(rt),
        Err(_) => {
            // No tokio runtime available: run sequentially, same results.
            let mut passed = 0usize;
            let mut failed = 0usize;
            for (file, preload) in jobs {
                let preload = preload_of(file, &preload);
                let resp = run_single_test(file, &preload);
                let rel = rel_display(file, &std::env::current_dir().unwrap_or_default());
                let (p, f) = print_file_result(&rel, &resp, file, update);
                passed += p;
                failed += f;
            }
            (passed, failed)
        }
    }
}

/// `honey test --watch`: daemon-backed watch session for the batch (worker
/// subprocess re-runs for affected files); stat-poll fallback when the daemon
/// is unavailable. Loops until interrupted.
fn run_test_watch(jobs: &[(String, String)], cwd: &std::path::Path) {
    // Watch sessions run the batch on worker subprocesses, which consume real
    // wrapper sources (first batch); daemon-side watch only re-runs affected
    // files after edits. Resolve lazy wrappers here, once, per file.
    let abs_files: Vec<String> = jobs.iter().map(|(f, _)| f.clone()).collect();
    let preloads: Vec<String> = jobs
        .iter()
        .map(|(f, p)| {
            if p.is_empty() {
                test_preload_for(std::path::Path::new(f)).unwrap_or_default()
            } else {
                p.clone()
            }
        })
        .collect();
    let update = snapshot_update_enabled();

    if let Some(session) = daemon::test_watch_request(&abs_files, &preloads) {
        let mut passed = 0usize;
        let mut failed = 0usize;
        for resp in session {
            use std::io::Write as _;
            match resp.file.clone() {
                Some(file) => {
                    let rel = std::path::Path::new(&file)
                        .strip_prefix(cwd)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or(file.clone());
                    let (p, f) = print_file_result(&rel, &resp, &file, update);
                    passed += p;
                    failed += f;
                }
                None => {
                    println!(
                        "{passed} passed, {failed} failed ({:.1} ms total, results printed in path order)",
                        resp.duration_ms
                    );
                    passed = 0;
                    failed = 0;
                }
            }
            let _ = std::io::stdout().flush();
        }
        return;
    }

    // No daemon: stat-poll the test files and re-run only the changed ones.
    eprintln!("[watch] daemon watch unavailable; stat-polling instead");
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut baselines: Vec<(String, Option<(i64, u32)>)> = abs_files
        .iter()
        .map(|f| (f.clone(), honey::runtime::stat_path_mtime(f)))
        .collect();
    loop {
        let mut changed = Vec::new();
        for (file, mtime) in &baselines {
            if honey::runtime::stat_path_mtime(file) != *mtime {
                changed.push(file.clone());
            }
        }
        if !changed.is_empty() {
            for (file, preload) in jobs {
                if !changed.contains(file) {
                    continue;
                }
                let resp = run_single_test(file, preload);
                let rel = std::path::Path::new(file)
                    .strip_prefix(cwd)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| file.clone());
                let (p, f) = print_file_result(&rel, &resp, file, update);
                passed += p;
                failed += f;
            }
            println!(
                "{passed} passed, {failed} failed (results printed in path order)"
            );
            passed = 0;
            failed = 0;
            for entry in baselines.iter_mut() {
                if changed.contains(&entry.0) {
                    entry.1 = honey::runtime::stat_path_mtime(&entry.0);
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }
}

fn main() {
    // Daemon mode is handled before clap: it must start silently and fast.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(|s| s.as_str()) == Some("--internal-daemon") {
        daemon::serve();
        return;
    }
    if argv.get(1).map(|s| s.as_str()) == Some("--internal-test-worker") {
        daemon::serve_test_worker();
        return;
    }
    if argv.get(1).map(|s| s.as_str()) == Some("--internal-mksnapshot") {
        let out = argv
            .get(2)
            .cloned()
            .unwrap_or_else(|| "target/honey_snapshot.bin".to_string());
        mksnapshot(&out);
        return;
    }
    if argv.get(1).map(|s| s.as_str()) == Some("--internal-enginetiming") {
        let n: usize = argv
            .get(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(20);
        honey::runtime::engine_init_timing(n);
        return;
    }

    // Debug: print the ESM->CJS transform of a file (no engine involved).
    if argv.get(1).map(|s| s.as_str()) == Some("--internal-transform-js") {
        let Some(f) = argv.get(2) else {
            eprintln!("usage: honey --internal-transform-js <file>");
            std::process::exit(2);
        };
        match std::fs::read_to_string(f) {
            Ok(src) => print!("{}", honey::runtime::transform_esm_to_cjs_debug(&src)),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // Dev-loop fast path: dispatch `honey run <file>` directly, skipping
    // clap parsing. Every ~0.5 ms counts at this timescale.
    if argv.get(1).map(|s| s.as_str()) == Some("run") && argv.len() >= 3 {
        let t_main = std::time::Instant::now();
        daemon::client_trace("main fast path entered");
        // `honey run --watch <file> [args]` (or -w before the file).
        let raw = &argv[2..];
        let watch = raw[0] == "--watch" || raw[0] == "-w";
        let (file, args): (String, Vec<String>) = if watch {
            if raw.len() < 2 {
                eprintln!("error: --watch requires a file to run");
                std::process::exit(2);
            }
            (raw[1].clone(), raw[2..].to_vec())
        } else {
            (raw[0].clone(), raw[1..].to_vec())
        };
        if watch {
            if let Some(()) = daemon::watch_via_daemon(&file, &args) {
                return;
            }
            inline_watch(&file, &args);
            return;
        }
        // Script tasks from honey.config.ts: `honey run dev` with no ./dev
        // file but a `scripts.dev` entry in the config runs that command
        // through the shell. The file wins when both exist.
        if !std::path::Path::new(&file).exists() {
            if let Some(command) = honey::config::resolve_script(&file, &args) {
                let mut cmd = std::process::Command::new("sh");
                cmd.arg("-c").arg(&format!("exec {command} \"$@\"")).arg(&file);
                for a in &args {
                    cmd.arg(a);
                }
                match cmd.status() {
                    Ok(s) => std::process::exit(s.code().unwrap_or(1)),
                    Err(e) => {
                        eprintln!("honey: failed to run script '{file}': {e}");
                        std::process::exit(127);
                    }
                }
            }
        }
        honey::config::fire("onRun", &format!(r#"{{"file":"{file}"}}"#));
        if let Some(resp) = daemon::run_via_daemon(&file, &args) {
            use std::io::Write as _;
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let _ = lock.write_all(resp.stdout.as_bytes());
            let _ = lock.flush();
            drop(lock);
            eprint!("{}", resp.stderr);
            if let Some(err) = resp.error {
                eprintln!("error: {err}");
            }
            daemon::client_trace(&format!(
                "client exiting ({}us total, exit={})",
                t_main.elapsed().as_micros(),
                resp.exit_code
            ));
            std::process::exit(resp.exit_code);
        }
        let runtime = Runtime::new();
        let code = match runtime.run_file(&file, &args) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("error: {:#}", e);
                1
            }
        };
        std::process::exit(code);
    }

    let cli = Cli::parse();

    match cli.command {
        Commands::Install { production, global, packages } => {
            if global || !packages.is_empty() {
                // Package installs belong to hxney, not the project installer.
                let pkgs = packages.join(" ");
                eprintln!("honey install only installs this project's dependencies.");
                if global {
                    eprintln!("To install a package globally:");
                    eprintln!("  hxney npm install -g {pkgs}");
                } else {
                    eprintln!("To install a package:");
                    eprintln!("  hxney npm install {pkgs}");
                }
                std::process::exit(2);
            }
            honey::config::fire("onInstall", &format!(r#"{{"cmd":"install","production":{production}}}"#));
            let pm = PackageManager::new(std::path::Path::new("."));
            if let Err(e) = pm.install(production) {
                eprintln!("error: {:#}", e);
                std::process::exit(1);
            }
        }
        Commands::Add { package, dev } => {
            let packages: &[String] = &package;
            for pkg in packages {
                honey::config::fire(
                    "onInstall",
                    &format!(r#"{{"cmd":"add","package":"{pkg}","dev":{dev}}}"#),
                );
            }
            let pm = PackageManager::new(std::path::Path::new("."));
            for pkg in packages {
                if let Err(e) = pm.add(pkg, dev) {
                    eprintln!("error: {:#}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Remove { package } => {
            honey::config::fire("onInstall", &format!(r#"{{"cmd":"remove","package":"{package}"}}"#));
            let pm = PackageManager::new(std::path::Path::new("."));
            if let Err(e) = pm.remove(&package) {
                eprintln!("error: {:#}", e);
                std::process::exit(1);
            }
        }
        Commands::Run { file, args, watch } => {
            // Script tasks from honey.config.ts: `honey run dev` with no ./dev
            // file but a `scripts.dev` entry in the config runs that command
            // through the shell. The file wins when both exist.
            if !std::path::Path::new(&file).exists() {
                if let Some(command) = honey::config::resolve_script(&file, &args) {
                    if watch {
                        eprintln!("honey: warning: --watch is not supported for config scripts");
                    }
                    let mut cmd = std::process::Command::new("sh");
                    cmd.arg("-c")
                        .arg(&format!("exec {command} \"$@\""))
                        .arg(&file);
                    for a in &args[..] {
                        cmd.arg(a);
                    }
                    match cmd.status() {
                        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
                        Err(e) => {
                            eprintln!("honey: failed to run script '{file}': {e}");
                            std::process::exit(127);
                        }
                    }
                }
            }
            honey::config::fire("onRun", &format!(r#"{{"file":"{file}"}}"#));
            if watch {
                if let Some(()) = daemon::watch_via_daemon(&file, &args) {
                    // Session ended (daemon reported EOF or fallback not needed).
                    std::process::exit(0);
                }
                inline_watch(&file, &args);
            }
            // Fast path: execute inside the persistent daemon.
            if let Some(resp) = daemon::run_via_daemon(&file, &args) {
                print!("{}", resp.stdout);
                eprint!("{}", resp.stderr);
                if let Some(err) = resp.error {
                    eprintln!("error: {err}");
                }
                std::process::exit(resp.exit_code);
            }
            // Cold path: no daemon available (or HONEY_NO_DAEMON set).
            let runtime = Runtime::new();
            let code = match runtime.run_file(&file, &args) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("error: {:#}", e);
                    1
                }
            };
            std::process::exit(code);
        }
        Commands::Build { entry, output, production, minify, sourcemap, target, define, watch } => {
            honey::config::fire("onBuild", r#"{"cmd":"build"}"#);
            let sourcemap = match sourcemap.as_deref() {
                None => None,
                Some(s) => match s {
                    "linked" => Some(honey::build::SourceMapMode::Linked),
                    "external" => Some(honey::build::SourceMapMode::External),
                    "inline" => Some(honey::build::SourceMapMode::Inline),
                    "none" => Some(honey::build::SourceMapMode::None),
                    other => {
                        eprintln!("error: invalid --sourcemap value '{other}' (linked|external|inline|none)");
                        std::process::exit(2);
                    }
                },
            };
            let target = match target.as_deref() {
                None => None,
                Some("browser") => Some(honey::build::BuildTarget::Browser),
                Some("node") => Some(honey::build::BuildTarget::Node),
                Some(other) => {
                    eprintln!("error: invalid --target value '{other}' (browser|node)");
                    std::process::exit(2);
                }
            };
            let mut defines: Vec<(String, String)> = Vec::new();
            for d in &define {
                match d.split_once('=') {
                    Some((k, v)) if !k.trim().is_empty() => {
                        defines.push((k.trim().to_string(), v.to_string()));
                    }
                    _ => {
                        eprintln!("error: invalid --define value '{d}' (expected key=value)");
                        std::process::exit(2);
                    }
                }
            }
            let opts = honey::build::BuildOptions {
                production,
                minify,
                sourcemap,
                target,
                define: defines,
            };
            if watch {
                // Warm path: the daemon hosts the watch loop and streams each
                // rebuild's output; the client is a thin relay, so watch mode
                // never pays a cold engine/bundler start either.
                if daemon::build_watch_via_daemon(entry.as_deref(), output.as_deref(), &opts) {
                    std::process::exit(0);
                }
                match honey::build::cmd_build_watch(entry.as_deref(), output.as_deref(), &opts) {
                    Ok(code) => std::process::exit(code),
                    Err(e) => {
                        eprintln!("error: {e:#}");
                        std::process::exit(1);
                    }
                }
            }
            // Warm path: the daemon runs Cinnamon in its resident process.
            if let Some(resp) = daemon::build_via_daemon(entry.as_deref(), output.as_deref(), &opts) {
                print!("{}", resp.stdout);
                eprint!("{}", resp.stderr);
                if let Some(err) = resp.error {
                    eprintln!("error: {err}");
                }
                std::process::exit(resp.exit_code);
            }
            match honey::build::cmd_build(entry.as_deref(), output.as_deref(), &opts) {
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("error: {:#}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Test { filter, slow, watch } => {
            honey::config::fire("onTest", &format!(r#"{{"filter":{}}}"#, filter.as_deref().map(|f| format!(r#""{f}""#)).unwrap_or_else(|| "null".into())));
            let code = run_test_suite(filter.as_deref(), slow, watch);
            std::process::exit(code);
        }
        Commands::Migrate { file } => {
            match honey::migrate::migrate(file.as_deref()) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("error: {:#}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Start => {
            match daemon::start_daemon() {
                Ok((0, true)) => println!("honey daemon already running"),
                Ok((pid, _)) => println!("honey daemon started (pid {pid})"),
                Err(e) => {
                    eprintln!("error: {:#}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Stop => {
            match daemon::stop_daemon() {
                Ok(()) => println!("honey daemon stopped"),
                Err(e) => {
                    eprintln!("error: {:#}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Service { uninstall, status } => {
            let code = honey::service::cli(uninstall, status);
            std::process::exit(code);
        }
    }
}