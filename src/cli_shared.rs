//! Shared user-facing CLI surface and daemon wire client.
//!
//! This file is included by both binaries with
//! `#[path = "../cli_shared.rs"] mod shared;`:
//!
//! * `honey` — the shipped thin launcher. It parses the user CLI and turns
//!   every command into a request to the resident `honeyd`.
//! * `honeyd` — the engine/daemon. Its hidden `__cli` path reuses the same
//!   parser and test runner to execute commands inline when the daemon is
//!   explicitly disabled (`HONEY_NO_DAEMON` / `HONEY_TRACE`).
//!
//! Nothing here links V8 or the engine: the launcher stays small, and the
//! daemon owns all engine work. The test runner is plain std + JSON.

#![allow(dead_code)]

use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

// ---------------------------------------------------------------------------
// CLI surface
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "honey")]
#[command(bin_name = "honey")]
#[command(author = "AXIOM")]
#[command(version = "0.1.0")]
#[command(
    about = "Up to 8.4x faster than Bun. Up to 23x faster than Node. One binary. One toolchain.",
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
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
    Run {
        /// File to run
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

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

/// One reply frame. Field-compatible with the daemon's `Response` (unknown
/// fields, like `results`, are ignored here).
#[derive(Deserialize, Default, Clone)]
pub struct Resp {
    #[serde(default)]
    pub exit_code: i32,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub duration_ms: f64,
    #[serde(default)]
    pub error: Option<String>,
    /// Test watch: the file this frame belongs to (None = batch marker).
    /// Eval/script replies reuse it for their JSON result.
    #[serde(default)]
    pub file: Option<String>,
}

#[derive(Deserialize, Default)]
struct BatchResp {
    #[serde(default)]
    results: Vec<Resp>,
}

/// Set HONEY_SOCKET to override (tests); otherwise $XDG_RUNTIME_DIR or /tmp.
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("HONEY_SOCKET") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d).join("honey.sock");
        }
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/honey-{uid}.sock"))
}

/// The daemon/engine binary that sits next to this executable.
pub fn daemon_exe() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("honeyd"));
    let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if name == "honeyd" || name.starts_with("honeyd-") {
        exe
    } else {
        exe.with_file_name("honeyd")
    }
}

fn env_disabled() -> bool {
    for k in ["HONEY_NO_DAEMON", "HONEY_TRACE"] {
        if std::env::var(k)
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn cwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn connect() -> Option<UnixStream> {
    UnixStream::connect(socket_path()).ok()
}

pub fn write_msg<S: Write>(stream: &mut S, value: &serde_json::Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(value).unwrap_or_default();
    stream.write_all(&(body.len() as u32).to_le_bytes())?;
    stream.write_all(&body)
}

pub fn read_msg<S: Read>(stream: &mut S) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 256 * 1024 * 1024 {
        return None;
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).ok()?;
    Some(body)
}

/// Connect to the daemon, asking `honeyd start` to bring it up on the first
/// use. Returns None when the daemon is explicitly disabled or cannot start.
pub fn connect_or_start() -> Option<UnixStream> {
    if env_disabled() {
        return None;
    }
    if let Some(s) = connect() {
        return Some(s);
    }
    let _ = std::process::Command::new(daemon_exe())
        .arg("start")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(s) = connect() {
            return Some(s);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// One request/response exchange with the daemon.
pub fn request(value: &serde_json::Value) -> Option<Resp> {
    let mut stream = connect_or_start()?;
    write_msg(&mut stream, value).ok()?;
    let body = read_msg(&mut stream)?;
    serde_json::from_slice(&body).ok()
}

pub fn print_response(resp: &Resp) {
    use std::io::Write as _;
    let stdout = std::io::stdout();
    {
        let mut lock = stdout.lock();
        let _ = lock.write_all(resp.stdout.as_bytes());
        let _ = lock.flush();
    }
    eprint!("{}", resp.stderr);
    if let Some(err) = &resp.error {
        eprintln!("error: {err}");
    }
}

pub fn exit_with(resp: Resp) -> ! {
    print_response(&resp);
    std::process::exit(resp.exit_code);
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

pub fn run_file(file: &str, args: &[String]) -> Option<Resp> {
    request(&serde_json::json!({
        "cmd": "run",
        "file": file,
        "args": args,
        "cwd": cwd(),
    }))
}

/// `honey run --watch`: stream each run's output until the daemon closes the
/// session (Ctrl+C closes the client, which the daemon notices).
pub fn watch_run(file: &str, args: &[String]) -> Option<()> {
    let mut stream = connect_or_start()?;
    write_msg(
        &mut stream,
        &serde_json::json!({
            "cmd": "watch",
            "file": file,
            "args": args,
            "cwd": cwd(),
        }),
    )
    .ok()?;
    loop {
        match read_msg(&mut stream) {
            None => return Some(()),
            Some(bytes) => match serde_json::from_slice::<Resp>(&bytes) {
                Ok(resp) => print_response(&resp),
                Err(_) => return Some(()),
            },
        }
    }
}

pub struct BuildRequest<'a> {
    pub entry: Option<&'a str>,
    pub output: Option<&'a str>,
    pub production: bool,
    pub minify: Option<bool>,
    pub sourcemap: Option<&'a str>,
    pub target: Option<&'a str>,
    pub define: &'a [String],
}

fn build_json(cmd: &str, b: &BuildRequest<'_>) -> serde_json::Value {
    serde_json::json!({
        "cmd": cmd,
        "cwd": cwd(),
        "build_entry": b.entry,
        "build_output": b.output,
        "build_production": b.production,
        "build_minify": b.minify,
        "build_sourcemap": b.sourcemap,
        "build_target": b.target,
        "build_define": b.define,
    })
}

pub fn build(b: &BuildRequest<'_>) -> Option<Resp> {
    request(&build_json("build", b))
}

/// `honey build --watch`: the daemon hosts the watch loop and streams one
/// frame per rebuild. Returns false when the daemon cannot serve it.
pub fn build_watch(b: &BuildRequest<'_>) -> bool {
    let mut stream = match connect_or_start() {
        Some(s) => s,
        None => return false,
    };
    if write_msg(&mut stream, &build_json("build_watch", b)).is_err() {
        return false;
    }
    // Bound the wait for the first frame: an older/ignoring daemon must not
    // wedge the client forever.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let first = read_msg(&mut stream);
    let _ = stream.set_read_timeout(None);
    let first = match first {
        Some(bytes) => match serde_json::from_slice::<Resp>(&bytes) {
            Ok(r) => r,
            Err(_) => return false,
        },
        None => return false,
    };
    print_response(&first);
    loop {
        match read_msg(&mut stream) {
            Some(bytes) => match serde_json::from_slice::<Resp>(&bytes) {
                Ok(r) => print_response(&r),
                Err(_) => break,
            },
            None => break,
        }
    }
    true
}

/// Evaluate a config hook in the daemon's warm engine. Advisory only: a
/// missing config, disabled daemon, or failing hook never fails the command.
pub fn fire_hook(hook: &str, ctx: &str) {
    let Some(resp) = request(&serde_json::json!({
        "cmd": "hook",
        "hook": hook,
        "hook_ctx": ctx,
        "cwd": cwd(),
    })) else {
        return;
    };
    if !resp.stdout.is_empty() {
        eprint!("{}", resp.stdout);
    }
    if !resp.stderr.is_empty() {
        eprint!("{}", resp.stderr);
    }
    if let Some(err) = resp.error {
        eprintln!("honey: config hook `{hook}` failed: {err}");
        return;
    }
    if let Some(value) = resp.file {
        if !value.is_empty() && value != "null" {
            eprintln!("honey: {hook} hook: {value}");
        }
    }
}

/// Resolve a `scripts.<name>` task from honey.config.ts through the daemon.
pub fn resolve_script(name: &str, args: &[String]) -> Option<String> {
    let resp = request(&serde_json::json!({
        "cmd": "script",
        "file": name,
        "args": args,
        "cwd": cwd(),
    }))?;
    serde_json::from_str::<String>(resp.file.as_deref()?).ok()
}

pub fn install(production: bool) -> Option<Resp> {
    request(&serde_json::json!({
        "cmd": "install",
        "production": production,
        "cwd": cwd(),
    }))
}

pub fn add(packages: &[String], dev: bool) -> Option<Resp> {
    request(&serde_json::json!({
        "cmd": "add",
        "args": packages,
        "dev": dev,
        "cwd": cwd(),
    }))
}

pub fn remove(package: &str) -> Option<Resp> {
    request(&serde_json::json!({
        "cmd": "remove",
        "file": package,
        "cwd": cwd(),
    }))
}

pub fn migrate(file: Option<&str>) -> Option<Resp> {
    request(&serde_json::json!({
        "cmd": "migrate",
        "file": file.unwrap_or(""),
        "cwd": cwd(),
    }))
}

/// `honey start`: `honeyd start` owns service installation and spawning.
pub fn start_daemon_cli() -> i32 {
    match std::process::Command::new(daemon_exe())
        .arg("start")
        .status()
    {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("error: failed to start honeyd: {e}");
            1
        }
    }
}

/// `honey stop`: ask the resident daemon to exit. Never auto-starts one.
pub fn stop_daemon_cli() -> i32 {
    if let Some(mut stream) = connect() {
        if write_msg(&mut stream, &serde_json::json!({"cmd": "stop"})).is_ok() {
            let _ = read_msg(&mut stream);
            println!("honey daemon stopped");
            return 0;
        }
    }
    eprintln!("error: no honey daemon running");
    1
}

/// `honey service`: the unit/agent must point at `honeyd`, so let the daemon
/// binary render and install it.
pub fn service_cli(uninstall: bool, status: bool) -> i32 {
    let mut cmd = std::process::Command::new(daemon_exe());
    cmd.arg("service");
    if uninstall {
        cmd.arg("--uninstall");
    }
    if status {
        cmd.arg("--status");
    }
    match cmd.status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("error: failed to run honeyd service: {e}");
            1
        }
    }
}

/// Execute a `scripts.<name>` shell task with the caller's terminal.
pub fn run_shell_script(name: &str, command: &str, args: &[String], watch: bool) -> ! {
    if watch {
        eprintln!("honey: warning: --watch is not supported for config scripts");
    }
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(format!("exec {command} \"$@\""))
        .arg(name);
    for a in args {
        cmd.arg(a);
    }
    match cmd.status() {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("honey: failed to run script '{name}': {e}");
            std::process::exit(127);
        }
    }
}

// ---------------------------------------------------------------------------
// Test runner (std-only; the daemon runs the files)
// ---------------------------------------------------------------------------

const TEST_EXTENSIONS: [&str; 2] = [".test.", ".spec."];

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

fn collect_test_files(dir: &Path, out: &mut Vec<PathBuf>) {
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

struct Suite {
    cwd: PathBuf,
    to_run: Vec<PathBuf>,
    skipped: Vec<PathBuf>,
}

fn discover_tests(filter: Option<&str>, slow: bool) -> Suite {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut files: Vec<PathBuf> = Vec::new();
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
    let display = |p: &Path| -> String {
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
    Suite { cwd, to_run, skipped }
}

fn snapshot_update_enabled() -> bool {
    matches!(std::env::var("HONEY_SNAPSHOT_UPDATE").as_deref(), Ok("1"))
}

const RESULT_PREFIX: &str = "@@HONEY@@";

/// Human-readable result lines collected for one test file.
struct TestOutcome {
    failed: usize,
    details: Vec<String>,
    snapshots_written: Vec<String>,
}

impl TestOutcome {
    fn ok(&self, resp: &Resp) -> bool {
        self.failed == 0 && resp.exit_code == 0 && resp.error.is_none()
    }
}

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

fn snapshot_path_for(test_file: &str) -> PathBuf {
    PathBuf::from(format!("{test_file}.snap"))
}

fn load_snap_sections(path: &Path) -> Vec<(String, String)> {
    let mut sections = Vec::new();
    let Ok(content) = std::fs::read_to_string(path) else {
        return sections;
    };
    let mut current: Option<(String, Vec<String>)> = None;
    for line in content.lines() {
        if let Some(key) = snap_header_line(line) {
            if let Some((k, mut body)) = current.take() {
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

fn write_snap_sections(path: &Path, test_file: &str, sections: &[(String, String)]) {
    let mut out = format!("# honey snapshots for {test_file}\n\n");
    for (key, value) in sections {
        out.push_str(&format!("[{key}]\n{value}\n\n"));
    }
    let _ = std::fs::write(path, out);
}

/// Parse a test file's run response: pass/fail per shim result line, snapshot
/// sections created/updated/compared against `<test file>.snap`.
fn process_test_response(resp: &Resp, test_file: &str, update: bool) -> TestOutcome {
    let mut outcome = TestOutcome {
        failed: 0,
        details: Vec::new(),
        snapshots_written: Vec::new(),
    };
    let snap_path = snapshot_path_for(test_file);
    let mut snap_sections = load_snap_sections(&snap_path);
    let mut snap_dirty = false;

    for line in resp.stdout.lines() {
        let Some(rest) = line.strip_prefix(RESULT_PREFIX) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(rest) else {
            continue;
        };
        match v.get("s").and_then(|s| s.as_str()) {
            Some("fail") => {
                outcome.failed += 1;
                let title = json_str(&v, "t");
                let msg = json_str(&v, "m");
                outcome
                    .details
                    .push(format!("FAIL {title}\n    {}", msg.replace('\n', "\n    ")));
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
    text.lines()
        .map(|l| format!("{prefix}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn rel_display(file: &str, cwd: &Path) -> String {
    Path::new(file)
        .strip_prefix(cwd)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| file.to_string())
}

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

/// Print one file's result block; returns file-level (passed, failed) counts.
fn print_file_result(rel: &str, resp: &Resp, test_file: &str, update: bool) -> (usize, usize) {
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
        println!(
            "FAIL {rel} — {}",
            error_summary(resp.exit_code, resp.error.as_deref(), &resp.stderr)
        );
        if !resp.stderr.trim().is_empty() {
            println!("{}", indent_lines(resp.stderr.trim_end(), "  "));
        }
        (0, 1)
    }
}

/// Run a whole suite in the daemon's retained test engine. Empty preloads are
/// intentional: the daemon builds the shim wrappers itself.
fn test_batch(files: &[String], snapshot_update: bool) -> Option<Vec<Resp>> {
    let mut stream = connect_or_start()?;
    write_msg(
        &mut stream,
        &serde_json::json!({
            "cmd": "test_batch",
            "args": files,
            "cwd": cwd(),
            "preloads": [],
            "snapshot_update": snapshot_update,
        }),
    )
    .ok()?;
    let body = read_msg(&mut stream)?;
    let batch: BatchResp = serde_json::from_slice(&body).ok()?;
    if batch.results.len() == files.len() {
        Some(batch.results)
    } else {
        None
    }
}

/// No-daemon fallback: one `honeyd worker` subprocess runs the batch in a
/// single fresh engine (the worker builds the shim wrappers).
fn worker_batch(files: &[String], snapshot_update: bool) -> Option<Vec<Resp>> {
    let req = serde_json::json!({
        "cmd": "test_batch",
        "args": files,
        "cwd": cwd(),
        "preloads": [],
        "snapshot_update": snapshot_update,
    });
    let out = run_worker(&req)?;
    let batch: BatchResp = serde_json::from_slice(&out).ok()?;
    if batch.results.len() == files.len() {
        Some(batch.results)
    } else {
        None
    }
}

fn run_worker(req: &serde_json::Value) -> Option<Vec<u8>> {
    let mut child = std::process::Command::new(daemon_exe())
        .arg("worker")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    {
        let mut stdin = child.stdin.take()?;
        let body = serde_json::to_vec(req).ok()?;
        stdin.write_all(&body).ok()?;
    }
    let out = child.wait_with_output().ok()?;
    if out.stdout.is_empty() {
        let hint: String = String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no output")
            .chars()
            .take(160)
            .collect();
        eprintln!("error: test worker failed: {hint}");
        return None;
    }
    Some(out.stdout)
}

/// Live `honey test --watch` session: one frame per completed file, then a
/// batch marker (file = None).
pub struct TestWatchSession {
    stream: UnixStream,
    first: Option<Vec<u8>>,
}

fn test_watch(files: &[String], snapshot_update: bool) -> Option<TestWatchSession> {
    let mut stream = connect_or_start()?;
    write_msg(
        &mut stream,
        &serde_json::json!({
            "cmd": "watch",
            "file": "",
            "args": files,
            "cwd": cwd(),
            "snapshot_update": snapshot_update,
        }),
    )
    .ok()?;
    // The first batch may take a moment; an ignoring/older daemon must not
    // wedge the client, so bound only this first read.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let first = read_msg(&mut stream);
    let _ = stream.set_read_timeout(None);
    let first = first?;
    serde_json::from_slice::<Resp>(&first).ok()?;
    Some(TestWatchSession {
        stream,
        first: Some(first),
    })
}

impl Iterator for TestWatchSession {
    type Item = Resp;
    fn next(&mut self) -> Option<Resp> {
        let bytes = match self.first.take() {
            Some(b) => b,
            None => read_msg(&mut self.stream)?,
        };
        serde_json::from_slice(&bytes).ok()
    }
}

fn print_watch(session: TestWatchSession, cwd: &Path, update: bool) {
    let mut passed = 0usize;
    let mut failed = 0usize;
    for resp in session {
        use std::io::Write as _;
        match resp.file.clone() {
            Some(file) => {
                let rel = rel_display(&file, cwd);
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
}

fn mtime(path: &str) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Daemon-less watch fallback: stat-poll the files and re-run the changed
/// ones through `honeyd worker`.
fn watch_stat_poll(files: &[String], cwd: &Path, update: bool) {
    let mut stamps: Vec<Option<SystemTime>> = files.iter().map(|f| mtime(f)).collect();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let mut changed: Vec<String> = Vec::new();
        for (i, file) in files.iter().enumerate() {
            let now = mtime(file);
            if now != stamps[i] {
                stamps[i] = now;
                changed.push(file.clone());
            }
        }
        if changed.is_empty() {
            continue;
        }
        let Some(results) = worker_batch(&changed, update) else {
            continue;
        };
        let mut passed = 0usize;
        let mut failed = 0usize;
        for (resp, file) in results.into_iter().zip(changed.iter()) {
            let rel = rel_display(file, cwd);
            let (p, f) = print_file_result(&rel, &resp, file, update);
            passed += p;
            failed += f;
        }
        println!("{passed} passed, {failed} failed (results printed in path order)");
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }
}

/// Discover and run the project's test files. Returns the process exit code
/// (0 = all passed, 1 = any failure or none found).
pub fn run_test_suite(filter: Option<&str>, slow: bool, watch: bool) -> i32 {
    let suite = discover_tests(filter, slow);

    if suite.to_run.is_empty() && suite.skipped.is_empty() {
        eprintln!("error: no test files found (*.test.js, *.test.ts, *.spec.js, *.spec.ts)");
        return 1;
    }

    println!(
        "running {} test{}...",
        suite.to_run.len(),
        if suite.to_run.len() == 1 { "" } else { "s" }
    );
    for p in &suite.skipped {
        println!(
            "SKIP {} (slow; pass --slow to include)",
            rel_display(&p.to_string_lossy(), &suite.cwd)
        );
    }

    let files: Vec<String> = suite
        .to_run
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let update = snapshot_update_enabled();

    if watch {
        if let Some(session) = test_watch(&files, update) {
            print_watch(session, &suite.cwd, update);
        } else {
            eprintln!("[watch] daemon watch unavailable; stat-polling instead");
            watch_stat_poll(&files, &suite.cwd, update);
        }
        return 0; // watch mode ends when the user interrupts (Ctrl+C)
    }

    let started = Instant::now();
    let results = test_batch(&files, update).or_else(|| worker_batch(&files, update));
    let Some(results) = results else {
        eprintln!("error: test engine unavailable");
        return 1;
    };

    let mut passed = 0usize;
    let mut failed = 0usize;
    for (resp, file) in results.into_iter().zip(files.iter()) {
        let rel = rel_display(file, &suite.cwd);
        let (p, f) = print_file_result(&rel, &resp, file, update);
        passed += p;
        failed += f;
    }
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    println!("{passed} passed, {failed} failed ({ms:.1} ms total, results printed in path order)");
    i32::from(failed > 0)
}
