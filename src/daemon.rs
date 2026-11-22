//! Persistent daemon: binds a per-user Unix socket and executes `run`
//! requests in-process, so each invocation skips process spawn, dynamic
//! linking, snapshot decode and CLI setup — the dev-loop 5x claim.
//!
//! Protocol: 4-byte little-endian length prefix + UTF-8 JSON body.
//! Requests:  { "cmd": "run", "file": ..., "args": [...], "cwd": ... }
//!            { "cmd": "run", "file": ..., "preload": "<JS source>" }
//!            { "cmd": "ping" }
//!            { "cmd": "stop" }
//!            { "cmd": "watch", "file": "<main file>" }
//!            { "cmd": "watch", "args": ["<test file>", ...],
//!              "preloads": ["<JS source>", ...] }
//! Responses: { "exit_code": 0, "stdout": "...", "stderr": "...",
//!              "duration_ms": 1.2, "error": "..." (optional),
//!              "file": "..." (test watch frames only, optional) }
//!
//! A `run` request with a `preload` (the test-runner shim wrapper) is served
//! on a detached connection thread that delegates to a one-shot worker
//! subprocess (`honey --internal-test-worker`): the daemon's single warm
//! engine cannot run requests concurrently (the isolate is not `Send` and
//! stdio capture is process-global), but one fresh engine per file does
//! parallelize across cores. Plain `run` requests keep the warm in-process
//! engine exactly as before.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Request {
    pub cmd: String,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: String,
    /// Optional JS wrapper source evaluated in the engine before the main
    /// module (the test runner injects a shim wrapper that defines
    /// expect/describe/it and then loads the test file). Its presence also
    /// selects the parallel worker-subprocess execution path.
    #[serde(default)]
    pub preload: String,
    /// Test watch for a batch of files (cmd "watch", file empty): args
    /// carries the file list and preloads[i] is the wrapper source for
    /// args[i]; an empty preload runs the file without a shim.
    #[serde(default)]
    pub preloads: Vec<String>,
    /// cmd "eval": JS source evaluated as a classic script in the warm
    /// engine (honey.config.ts hooks). Never registers a main module.
    #[serde(default)]
    pub code: String,
    /// cmd "build": bundle request (the daemon runs Cinnamon in-process, so
    /// one-shot builds never pay a second binary start).
    #[serde(default)]
    pub build_entry: Option<String>,
    #[serde(default)]
    pub build_output: Option<String>,
    #[serde(default)]
    pub build_production: bool,
    #[serde(default)]
    pub build_minify: Option<bool>,
    #[serde(default)]
    pub build_sourcemap: Option<String>,
    #[serde(default)]
    pub build_target: Option<String>,
    #[serde(default)]
    pub build_define: Vec<String>,
    /// Test batches: the client leaves every preload empty and lets the
    /// daemon build the shim wrappers (with this HONEY_SNAPSHOT_UPDATE
    /// client flag), saving a double file-read per test on the client.
    #[serde(default)]
    pub snapshot_update: bool,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Response {
    pub exit_code: i32,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub duration_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The file this frame belongs to (test watch frames only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// Reply to cmd "test_batch": one Response per test file, request order.
#[derive(Serialize, Deserialize, Default)]
struct TestBatchResponse {
    #[serde(default)]
    results: Vec<Response>,
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
    let uid = read_uid();
    PathBuf::from(format!("/tmp/honey-{}.sock", uid))
}

fn read_uid() -> u32 {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:") {
                if let Some(first) = rest.split_whitespace().next() {
                    if let Ok(u) = first.parse() {
                        return u;
                    }
                }
            }
        }
    }
    0
}

fn trace_log(msg: &str) {
    if std::env::var("HONEY_DAEMON_LOG").is_err() {
        return;
    }
    let path = std::env::var("HONEY_DAEMON_LOG").unwrap_or_default();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write as _;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let _ = writeln!(f, "[{:?}] {}", now, msg);
    }
}

// ---------- framing ----------

fn write_msg<S: std::io::Write + ?Sized>(stream: &mut S, value: &impl Serialize) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    stream.write_all(&buf).context("failed to write to daemon socket")?;
    stream.flush().ok();
    Ok(())
}

/// Bound how long a socket read may block: the daemon's accept loop serves
/// requests inline, so a client that connects and dies (SIGKILLed run,
/// interrupted bench iteration) must not wedge the whole daemon on a
/// half-open socket. Legit local clients send their full request frame
/// within microseconds of connect; 2s is generous for even a jammed box.
fn set_recv_timeout(stream: &std::os::unix::net::UnixStream, timeout: Duration) {
    use std::os::unix::io::AsRawFd;
    let tv = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: timeout.subsec_micros() as libc::suseconds_t,
    };
    let r = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const libc::timeval as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if r != 0 {
        trace_log("setsockopt SO_RCVTIMEO failed");
    }
}

fn read_msg<S: std::io::Read + ?Sized>(stream: &mut S) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(_) => return None,
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 256 * 1024 * 1024 {
        return None;
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).ok()?;
    Some(body)
}

// ---------- daemon loop ----------

/// Entry point for `honey --internal-daemon`. Never returns while serving.
// ---------- engine threads ----------
// One retained engine per dedicated thread, for the engine's whole lifetime.
// v8 150+ enters the isolate at construction, checks Isolate::GetCurrent()
// at every ContextScope creation and requires reverse-creation-order drops —
// two retained engines sharing the accept thread made any `run` after a
// `test` batch panic ("Context do not belong to the same Isolate") and kill
// the daemon. The accept thread still blocks on each reply, so execution
// stays serialized exactly as before.

/// A one-command job for the run-engine thread. `eval` selects config-hook
/// evaluation (handle_eval) instead of a file run (handle_run); the reply
/// carries the final Response with stdout/stderr already captured.
struct RunJob {
    req: Request,
    eval: bool,
    reply: std::sync::mpsc::Sender<Response>,
}

struct TestJob {
    req: Request,
    reply: std::sync::mpsc::Sender<TestBatchResponse>,
}

/// Send a job to a run-engine worker and wait for its Response. None when
/// the worker died (channel hangup): callers synthesize a failure reply.
fn on_run_engine(
    tx: &std::sync::mpsc::Sender<RunJob>,
    req: Request,
    eval: bool,
) -> Option<Response> {
    let (rtx, rrx) = std::sync::mpsc::channel();
    tx.send(RunJob { req, eval, reply: rtx }).ok()?;
    rrx.recv().ok()
}

fn spawn_run_engine() -> std::sync::mpsc::Sender<RunJob> {
    let (tx, rx) = std::sync::mpsc::channel::<RunJob>();
    let _ = std::thread::Builder::new().name("honey-run-engine".into()).spawn(
        move || {
            let runtime = crate::runtime::Runtime::new_warm();
            while let Ok(job) = rx.recv() {
                let RunJob { req, eval, reply } = job;
                let (out, err, mut response) = if eval {
                    capture_stdio(|| handle_eval(&req, &runtime))
                } else {
                    capture_stdio(|| handle_run(&req, &runtime))
                };
                response.stdout = out;
                response.stderr = err;
                let _ = reply.send(response);
            }
        },
    );
    tx
}

fn spawn_test_engine() -> std::sync::mpsc::Sender<TestJob> {
    let (tx, rx) = std::sync::mpsc::channel::<TestJob>();
    let _ = std::thread::Builder::new().name("honey-test-engine".into()).spawn(
        move || {
            // A dedicated retained engine for test batches: `run` requests
            // load an ESM main into their engine, which makes it un-reusable
            // for the next preload run (deno_core: one main module per
            // engine) — if tests shared that engine, every batch would pay a
            // full engine rebuild on its first file. Its own thread keeps it
            // main-free (CJS-shaped preloads) and reused forever.
            let test_runtime = crate::runtime::Runtime::new_warm();
            while let Ok(job) = rx.recv() {
                let TestJob { req, reply } = job;
                let _ = reply.send(serve_test_batch(req, &test_runtime));
            }
        },
    );
    tx
}

pub fn serve() {
    // A daemon crash used to be invisible (stderr nulled) and surfaced only
    // as a mysterious ~1s client stall + respawn. Log panics to disk.
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".into());
        let msg = format!(
            "[{}] honey daemon panic at {location}: {info}\n{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            // Always carry a captured backtrace: the daemon's stderr may be
            // nulled and RUST_BACKTRACE may be unset in spawned daemons.
            std::backtrace::Backtrace::force_capture()
        );
        if let Some(f) = daemon_stderr_log() {
            if let Some(dir) = f.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::OpenOptions::new().create(true).append(true).open(&f)
                .and_then(|mut fh| std::io::Write::write_all(&mut fh, msg.as_bytes()));
        }
        eprint!("{msg}");
    }));

    let path = socket_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    // Single-instance arbitration by PID file, not by socket probing: the
    // socket file lies (unlinked zombie listeners keep their path in
    // /proc/net/unix, a dead listener leaves the file intact, and stealing
    // the file mid-run made clients land on the wrong daemon). The pid file
    // is the truth: a live pid wins, everything else is cleanable.
    let pf = pid_file_path();
    // Serialize the whole claim (pid check + socket rebind) behind an
    // exclusive lock. Without it, N racing spawns each remove+bind the
    // socket file and become N live daemons — old ones leak as zombie
    // listeners clients can accidentally reach.
    let lock_path = pf.with_extension("lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path);
    let _guard = match lock_file {
        Ok(f) => {
            let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
            if r != 0 { None } else { Some(f) } // keep the fd alive for the lock
        }
        Err(_) => None,
    };
    for _ in 0..50 {
        match std::fs::read(&pf) {
            Ok(b) if !b.is_empty() => {
                let txt = String::from_utf8_lossy(&b);
                match txt.trim().parse::<u32>() {
                    Ok(pid) if pid != std::process::id() && daemon_pid_alive(pid) => {
                        trace_log("serve: pid file shows a live daemon, exiting");
                        return; // another daemon owns the ship
                    }
                    _ => break, // stale pid file: ours to clean up
                }
            }
            _ => {
                // No pid file (yet). Someone may be mid-bind; re-check briefly.
                if !std::path::Path::new(&pf).exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    // From here we are the only live daemon. Any socket file on the path is
    // folklore — a stale file or an unlinked holdover — remove and rebind.
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => {
            trace_log("serve: bound socket");
            let _ = std::fs::write(&pf, std::process::id().to_string());
            l
        }
        Err(_) => return, // lost a genuinely-concurrent bind; the winner serves
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    // Claim complete: socket bound and pid file written. Release the startup
    // lock — it must not be held for the daemon's lifetime or spawning
    // clients would block forever behind it.
    drop(_guard);

    // One warm engine per dedicated thread, for the engine's whole lifetime.
    // v8 150+ enters the isolate at construction, checks Isolate::GetCurrent()
    // at every ContextScope creation and requires reverse-creation-order
    // drops — two retained engines sharing the accept thread made any `run`
    // after a `test` batch panic ("Context do not belong to the same
    // Isolate") and killed the daemon. The accept thread still blocks on
    // each reply, so execution stays serialized exactly as before.
    let run_tx = spawn_run_engine();
    let test_tx = spawn_test_engine();

    // Prewarm the test-worker pool in the background so the first `honey
    // test` after a cold daemon start finds hot engines instead of paying
    // the engine-init spawn per chunk. Knob: HONEY_TEST_PREWARM (count,
    // default 4, "0" disables; always capped by HONEY_TEST_MAX_IDLE).
    if test_pool_enabled() {
        std::thread::Builder::new()
            .name("pool-prewarm".into())
            .spawn(|| {
                let count = std::env::var("HONEY_TEST_PREWARM")
                    .ok()
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(4)
                    .min(test_pool_max_idle());
                let mut spawned = 0usize;
                for _ in 0..count {
                    match TestWorker::spawn() {
                        Ok(w) => {
                            let pushed = TEST_POOL
                                .try_lock()
                                .map(|mut p| {
                                    if p.len() >= test_pool_max_idle() {
                                        false
                                    } else {
                                        p.push(w);
                                        true
                                    }
                                })
                                .unwrap_or(false);
                            if !pushed {
                                break; // w dropped here: killed + reaped
                            }
                            spawned += 1;
                        }
                        Err(_) => break,
                    }
                }
                if spawned > 0 {
                    trace_log(&format!("pool: prewarmed {spawned} workers"));
                }
            })
            .ok();
    }

    for stream in listener.incoming() {
        trace_log("accepted connection");
        let Ok(mut stream) = stream else { continue };
        // A client that connects and dies must not wedge the accept loop on
        // a half-open read. Bound is PER-READ: requests queued in the
        // listener backlog are unaffected (the timer starts when this loop
        // actually reads them), and legit clients fill the frame in µs.
        set_recv_timeout(&stream, Duration::from_millis(250));
        let request = match read_msg(&mut stream) {
            Some(bytes) if bytes.is_empty() => continue,
            Some(bytes) => match serde_json::from_slice::<Request>(&bytes) {
                Ok(r) => r,
                Err(_) => continue,
            },
            None => continue,
        };
        trace_log(&format!("request received cmd={}", request.cmd));

        match request.cmd.as_str() {
            "ping" => {
                let _ = write_msg(&mut stream, &Response {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    duration_ms: 0.0,
                    error: None,
                    file: None,
                });
            }
            "stop" => {
                let _ = write_msg(&mut stream, &Response {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    duration_ms: 0.0,
                    error: None,
                    file: None,
                });
                trace_log("serve: stop requested, exiting");
                break;
            }
            "build" | "build_watch" => {
                // One-shot build served by the daemon: the bundler runs in
                // this already-resident process instead of a fresh CLI.
                // "build_watch" hands the whole watch loop to the daemon:
                // it rebuilds in-process and streams one Response frame per
                // rebuild until the client disconnects.
                let req = request.clone();
                if req.cmd == "build_watch" {
                    build_watch_session(&mut stream, &req);
                    continue;
                }
                let (out, err, result) = capture_stdio(move || -> anyhow::Result<i32> {
                    if !req.cwd.is_empty() {
                        let _ = std::env::set_current_dir(&req.cwd);
                    }
                    let opts = build_opts_from_request(&req);
                    crate::build::cmd_build(req.build_entry.as_deref(), req.build_output.as_deref(), &opts)
                });
                let (exit_code, error) = match result {
                    Ok(code) => (code, None),
                    Err(e) => (1, Some(format!("{e:#}"))),
                };
                let _ = write_msg(&mut stream, &Response {
                    exit_code,
                    stdout: out,
                    stderr: err,
                    error,
                    ..Default::default()
                });
            }
            "test_batch" => {
                // Warm test suite. Default: the whole suite splits into
                // chunks that run CONCURRENTLY in worker subprocesses (one
                // engine init per chunk instead of per file, cores used in
                // parallel); results are merged in request order, so output
                // is bit-identical to the sequential run. HONEY_TEST_
                // SEQUENTIAL=1 keeps the original single-engine sequential
                // path; HONEY_TEST_WORKERS=1 forces one isolated worker per
                // file (pristine per-file globals); small batches stay in
                // the single warm engine.
                let req = request.clone();
                let out_body = (|| {
                    let (rtx, rrx) = std::sync::mpsc::channel();
                    test_tx
                        .send(TestJob { req, reply: rtx })
                        .ok()
                        .and_then(|_| rrx.recv().ok())
                })()
                .unwrap_or(TestBatchResponse {
                    results: vec![Response {
                        exit_code: 1,
                        error: Some("test engine unavailable".into()),
                        ..Default::default()
                    }],
                });
                let _ = write_msg(&mut stream, &out_body);
            }
            "run" if !request.preload.is_empty() => {
                // Parallel-capable run (test suite): serve on a detached
                // connection thread that delegates to a one-shot worker
                // subprocess, so concurrent connections execute concurrently.
                // The warm engine is never touched from this path.
                let req = request.clone();
                let Ok(mut conn) = stream.try_clone() else { continue };
                let spawned = std::thread::Builder::new()
                    .name("honey-test-run".into())
                    .spawn(move || {
                        let response = run_in_worker(&req);
                        let _ = write_msg(&mut conn, &response);
                    });
                if spawned.is_err() {
                    // Nowhere to put the response without a thread: fail the
                    // run explicitly rather than leave the client hanging.
                    let _ = write_msg(&mut stream, &Response {
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: String::new(),
                        duration_ms: 0.0,
                        error: Some("failed to spawn test worker thread".into()),
                        file: None,
                    });
                }
            }
            "run" => {
                let req = request.clone();
                trace_log("run: dispatching to run engine");
                let response = on_run_engine(&run_tx, req, false).unwrap_or(Response {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: String::new(),
                    duration_ms: 0.0,
                    error: Some("run engine unavailable".into()),
                    file: None,
                });
                trace_log("run: engine returned");
                let _ = write_msg(&mut stream, &response);
                trace_log("run: response written");
            }
            "watch" if request.file.is_empty() => {
                // Test-suite watch (args = files, preloads = shims): own
                // session machinery, no warm engine needed (re-runs go to
                // worker subprocesses).
                trace_log("test watch: session opened");
                watch_test_session(&mut stream, &request);
                trace_log("test watch: session closed");
            }
            "watch" => {
                trace_log("watch: session opened");
                watch_session(&mut stream, &request, &run_tx);
                trace_log("watch: session closed");
            }
            "eval" => {
                // Config hook evaluation: pure script in the warm engine.
                let req = request.clone();
                let response = on_run_engine(&run_tx, req, true).unwrap_or(Response {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: String::new(),
                    duration_ms: 0.0,
                    error: Some("run engine unavailable".into()),
                    file: None,
                });
                let _ = write_msg(&mut stream, &response);
            }
            _ => {}
        }
    }

    // Pool workers are session children on our pipes: they would notice our
    // death by EOF, but a clean stop closes them first (kills + reaps).
    if let Ok(mut pool) = TEST_POOL.lock() {
        pool.clear(); // Drop: kill + wait each worker
    }
    trace_log("serve: accept loop ended (clean exit)");
    let _ = std::fs::remove_file(&path);
    // Clear our pid file only if it still names us (never clobber a
    // successor's).
    if std::fs::read_to_string(pid_file_path())
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        == Some(std::process::id())
    {
        let _ = std::fs::remove_file(pid_file_path());
    }
}

/// Rebuild the client's BuildOptions from the wire fields of a build or
/// build_watch request. Values were validated by the CLI before sending.
fn build_opts_from_request(req: &Request) -> crate::build::BuildOptions {
    let sourcemap = match req.build_sourcemap.as_deref() {
        Some("linked") => Some(crate::build::SourceMapMode::Linked),
        Some("external") => Some(crate::build::SourceMapMode::External),
        Some("inline") => Some(crate::build::SourceMapMode::Inline),
        Some("none") => Some(crate::build::SourceMapMode::None),
        _ => None,
    };
    let target = match req.build_target.as_deref() {
        Some("browser") => Some(crate::build::BuildTarget::Browser),
        Some("node") => Some(crate::build::BuildTarget::Node),
        _ => None,
    };
    let define = req
        .build_define
        .iter()
        .filter_map(|d| d.split_once('=').map(|(k, v)| (k.trim().to_string(), v.to_string())))
        .collect();
    crate::build::BuildOptions {
        production: req.build_production,
        minify: req.build_minify,
        sourcemap,
        target,
        define,
    }
}

/// One rebuild step shared by the build_watch session: dispatch to the html
/// or JS builder, swallow and report build errors (the watcher keeps running),
/// and return (exit_code, error).
fn build_watch_step(
    entry_file: &std::path::Path,
    out_path: &std::path::Path,
    out_dir_abs: &std::path::Path,
    html_mode: bool,
    opts: &crate::build::BuildOptions,
    watch_files: &mut Vec<std::path::PathBuf>,
) -> (i32, Option<String>) {
    let ok = if html_mode {
        crate::build::build_html(entry_file, out_dir_abs, opts).map(|_| ())
    } else {
        crate::build::execute_build(entry_file, out_path, opts, watch_files)
    };
    match ok {
        Ok(()) => (0, None),
        Err(e) => {
            eprintln!("error: {e:#}");
            println!("[watch] build failed; waiting for changes");
            // execute_build still fills watch_files on graph-success paths; on
            // total failure keep the entry alone in the set.
            if watch_files.is_empty() {
                watch_files.push(entry_file.to_path_buf());
            }
            (1, None)
        }
    }
}

/// True when the peer socket has closed (or errored) — checked with a
/// zero-timeout poll + MSG_PEEK, so a watch session ends as soon as its
/// client dies instead of only on the next rebuild.
fn peer_gone(stream: &UnixStream) -> bool {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut pfd = [libc::pollfd { fd, events: libc::POLLIN, revents: 0 }];
    let r = unsafe { libc::poll(pfd.as_mut_ptr(), 1, 0) };
    if r <= 0 {
        return false;
    }
    let re = pfd[0].revents;
    if re & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return true;
    }
    if re & libc::POLLIN != 0 {
        // We never read from the client: readable means EOF (or junk). A
        // zero-byte peek is a clean close; anything else, err on the side of
        // ending the session too — the client has nothing to say to us.
        let mut b = [0u8; 1];
        let n = unsafe {
            libc::recv(fd, b.as_mut_ptr().cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT)
        };
        return n <= 0;
    }
    false
}

/// Daemon side of `honey build --watch`: the same stat-poll loop as the CLI's
/// inline watcher, but hosted in the resident daemon. One captured Response
/// frame per rebuild is streamed to the client; the session ends when the
/// client disconnects (write_msg fails). The first frame carries the
/// once-only header lines (captured with the first build), so the merged
/// client output matches the cold path's.
fn build_watch_session(stream: &mut UnixStream, request: &Request) {
    if !request.cwd.is_empty() {
        let _ = std::env::set_current_dir(&request.cwd);
    }
    let opts = build_opts_from_request(request);
    let entry_input = request
        .build_entry
        .clone()
        .unwrap_or_else(|| "src/index.ts".to_string());
    let cwd = std::env::current_dir().unwrap_or_default();
    let entry_file = match crate::build::resolve_entry_input(&entry_input, &cwd) {
        Ok(p) => p,
        Err(e) => {
            let _ = write_msg(stream, &Response {
                exit_code: 1,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: 0.0,
                error: Some(format!("{e:#}")),
                file: None,
            });
            return;
        }
    };
    let out_dir_abs: std::path::PathBuf = match request.build_output.as_deref() {
        Some(dir) => {
            let p = std::path::Path::new(dir);
            let p = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
            // A file-shaped -o is unusual for a directory-of-outputs watch;
            // treat it as its parent directory.
            if p.extension().map(|e| e == "js" || e == "mjs" || e == "cjs").unwrap_or(false) {
                p.parent().map(|p| p.to_path_buf()).unwrap_or(p)
            } else {
                p
            }
        }
        None => cwd.join("dist"),
    };
    if let Err(e) = std::fs::create_dir_all(&out_dir_abs) {
        let _ = write_msg(stream, &Response {
            exit_code: 1,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 0.0,
            error: Some(format!("failed to create output directory: {} ({e})", out_dir_abs.display())),
            file: None,
        });
        return;
    }
    let out_path = out_dir_abs.join(crate::build::output_name(&entry_file));
    let html_mode = entry_file.extension().map(|e| e == "html").unwrap_or(false);

    // Settled watch set: what the last build read, plus the entry in html mode
    // (scripts inside are re-discovered on an html change — html mode keeps
    // only the entry here, matching the inline watcher).
    let mut watch_files: Vec<std::path::PathBuf> = vec![entry_file.clone()];
    let mut stamps: std::collections::HashMap<std::path::PathBuf, (u128, u64)> = Default::default();
    let refresh_stamps = |watch_files: &[std::path::PathBuf],
                          stamps: &mut std::collections::HashMap<std::path::PathBuf, (u128, u64)>| {
        stamps.clear();
        for f in watch_files {
            if let Some(s) = crate::build::stat_stamp(f) {
                stamps.insert(f.clone(), s);
            }
        }
    };
    refresh_stamps(&watch_files, &mut stamps);

    // First frame: header lines + first build, captured together so the
    // client's merged output equals the inline watcher's.
    let (out, err, (exit_code, error)) = capture_stdio(|| {
        println!("honey build --watch");
        println!("  entry: {}", entry_file.display());
        println!("  output dir: {}", out_dir_abs.display());
        println!("  watching {} file(s) — Ctrl-C to stop", 1);
        build_watch_step(&entry_file, &out_path, &out_dir_abs, html_mode, &opts, &mut watch_files)
    });
    refresh_stamps(&watch_files, &mut stamps);
    if write_msg(stream, &Response {
        exit_code,
        stdout: out,
        stderr: err,
        duration_ms: 0.0,
        error,
        file: None,
    })
    .is_err()
    {
        return;
    }

    loop {
        // 10ms polling keeps edit->rebuild latency near the build cost while
        // staying cheap: one stat per source file per tick.
        std::thread::sleep(Duration::from_millis(10));
        if peer_gone(stream) {
            return; // client died (Ctrl-C, closed terminal, killed): free the daemon
        }
        let mut changed: Vec<std::path::PathBuf> = Vec::new();
        for f in &watch_files {
            match crate::build::stat_stamp(f) {
                Some(s) => {
                    if stamps.get(f).map(|old| old != &s).unwrap_or(true) {
                        changed.push(f.clone());
                    }
                }
                None => changed.push(f.clone()), // deleted: rebuild, build reports
            }
        }
        if changed.is_empty() {
            continue;
        }
        let names: Vec<String> = changed
            .iter()
            .map(|c| {
                c.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| c.display().to_string())
            })
            .collect();
        for f in changed.iter() {
            if let Some(s) = crate::build::stat_stamp(f) {
                stamps.insert(f.clone(), s);
            }
        }

        let (out, err, (exit_code, error)) = capture_stdio(|| {
            println!("\n[watch] change in {}", names.join(", "));
            build_watch_step(&entry_file, &out_path, &out_dir_abs, html_mode, &opts, &mut watch_files)
        });
        refresh_stamps(&watch_files, &mut stamps);
        if write_msg(stream, &Response {
            exit_code,
            stdout: out,
            stderr: err,
            duration_ms: 0.0,
            error,
            file: None,
        })
        .is_err()
        {
            return; // client gone: the session ends with it
        }
    }
}

/// Serve one test_batch request. Chooses between the sequential warm-engine
/// path and chunked-parallel worker subprocesses (see the dispatch comment).
fn serve_test_batch(mut req: Request, runtime: &crate::runtime::Runtime) -> TestBatchResponse {
    // Daemon-side wrapper building: empty preloads mean the client skipped
    // the per-file read (the daemon already reads every file to run it).
    // ESM sniff + shim marker come along from lib::runtime.
    if req.args.iter().enumerate().any(|(i, f)| !f.is_empty() && req.preloads.get(i).map(String::is_empty).unwrap_or(true)) {
        let mut preloads: Vec<String> = req.preloads.clone();
        for (i, f) in req.args.iter().enumerate() {
            if f.is_empty() {
                continue;
            }
            let empty = req.preloads.get(i).map(String::is_empty).unwrap_or(true);
            if empty {
                preloads.resize(req.args.len(), String::new());
                preloads[i] = crate::runtime::test_preload_for_snapshot(std::path::Path::new(f), req.snapshot_update).unwrap_or_default();
            }
        }
        req.preloads = preloads;
    }
    let n = req.args.len();
    let sequential = std::env::var("HONEY_TEST_SEQUENTIAL").as_deref() == Ok("1");
    let force_workers = std::env::var("HONEY_TEST_WORKERS")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    // Chunk target balances engine-init overhead (once per chunk) against
    // parallel breadth. Capped so a big suite cannot spawn a worker per core
    // slot on small-memory machines: each worker is a fresh V8 process, and
    // this box (8GB) starts thrashing well above 8 concurrent engines.
    // HONEY_TEST_CHUNKS overrides the computed chunk count (benchmarking).
    let chunk_target = ((n / cores).max(4)).max(1);
    let mut chunk_count = if n > 1 { (n / chunk_target).max(1).min(n) } else { 1 };
    if chunk_count > 8 {
        chunk_count = 8;
    }
    let env_chunks = std::env::var("HONEY_TEST_CHUNKS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok());
    if let Some(c) = env_chunks {
        chunk_count = c.min(n.max(1));
    }

    // Parallel workers only pay off for LARGE suites: the pool round-trip,
    // per-chunk engine init and cross-process contention cost ~8-11ms of
    // wall each chunk round, while the daemon's retained test engine runs
    // a warm file in ~80µs. Benchmarked crossover on the 50-file suite:
    // sequential ~4-7ms vs parallel 11-18ms. Stay in-daemon until the
    // suite is far beyond what one engine eats quickly.
    if sequential || force_workers || n <= 100 || chunk_count <= 1 {
        return serve_test_batch_sequential(req, runtime);
    }
    serve_test_batch_parallel(req, chunk_count)
}

/// Original sequential path: every file runs in the daemon's retained engine,
/// evicting the module first so unchanged files still re-execute.
fn serve_test_batch_sequential(req: Request, runtime: &crate::runtime::Runtime) -> TestBatchResponse {
    let rt = runtime;
    let t_start = std::time::Instant::now();
    let mut results = Vec::with_capacity(req.args.len());
    for (i, file) in req.args.iter().enumerate() {
        let t_file = std::time::Instant::now();
        let preload = req.preloads.get(i).cloned().unwrap_or_default();
        crate::runtime::evict_module(file);
        let run_req = Request {
            cmd: "run".into(),
            file: file.clone(),
            cwd: req.cwd.clone(),
            preload,
            ..Default::default()
        };
        let (out, err, mut response) = capture_stdio(move || handle_run(&run_req, rt));
        response.stdout = out;
        response.stderr = err;
        response.file = Some(file.clone());
        let us = t_file.elapsed().as_micros();
        if us > 2000 {
            trace_log(&format!(
                "test seq: file {i}/{} slow {us}us {}",
                req.args.len(),
                file.rsplit('/').next().unwrap_or(file)
            ));
        }
        results.push(response);
    }
    trace_log(&format!(
        "test seq done: n={} total_us={}",
        req.args.len(),
        t_start.elapsed().as_micros()
    ));
    TestBatchResponse { results }
}

/// Chunked-parallel path: split (files, preloads) into `chunks` contiguous
/// slices, run each slice on a worker (persistent pool first, fresh
/// one-shot worker as fallback), all chunks in parallel, and merge results
/// in chunk order — identical ordering to a sequential run.
fn serve_test_batch_parallel(req: Request, chunks: usize) -> TestBatchResponse {
    let t_start = std::time::Instant::now();
    let n = req.args.len();
    let per = (n + chunks - 1) / chunks;
    trace_log(&format!("test batch parallel: n={n} chunks={chunks}"));
    let mut handles: Vec<(usize, std::thread::JoinHandle<Result<Vec<Response>, String>>)> = Vec::new();
    for c in 0..chunks {
        let lo = c * per;
        if lo >= n {
            break;
        }
        let hi = ((c + 1) * per).min(n);
        let files: Vec<String> = req.args[lo..hi].to_vec();
        let preloads: Vec<String> = req.preloads.get(lo..hi).map(|s| s.to_vec()).unwrap_or_default();
        let cwd = req.cwd.clone();
        let first = files[0].clone();
        let batch_req = Request {
            cmd: "test_batch".into(),
            args: files,
            cwd,
            preloads,
            ..Default::default()
        };
        let h = std::thread::Builder::new()
            .name(format!("honey-batch-{c}"))
            .spawn(move || {
                let t_c = std::time::Instant::now();
                let r = run_batch_on_worker(batch_req);
                trace_log(&format!(
                    "chunk {c} ({}): done in {:.1}ms",
                    first,
                    t_c.elapsed().as_secs_f64() * 1000.0
                ));
                r
            })
            .map_err(|e| e.to_string());
        match h {
            Ok(h) => handles.push((c, h)),
            Err(e) => handles.push((
                c,
                std::thread::spawn(move || Err(format!("failed to spawn batch worker: {e}"))),
            )),
        }
    }
    let mut all: Vec<(usize, Vec<Response>)> = Vec::with_capacity(handles.len());
    for (c, h) in handles {
        let r = match h.join() {
            Ok(Ok(rs)) => rs,
            Ok(Err(e)) => vec![fail_response(e)],
            Err(_) => vec![fail_response("batch worker panicked".into())],
        };
        all.push((c, r));
    }
    all.sort_by_key(|(c, _)| *c);
    trace_log(&format!(
        "test batch parallel: done in {:.1}ms",
        t_start.elapsed().as_secs_f64() * 1000.0
    ));
    TestBatchResponse {
        results: all.into_iter().flat_map(|(_, r)| r).collect(),
    }
}

// ---------- persistent test-worker pool ----------
//
// Engine init (isolate + snapshot + globals) costs ~3.5ms per fresh worker —
// so a fresh subprocess per batch pays it every time a test suite runs. The
// pool keeps idle session workers alive between batches with their engines
// warm: a repeated `honey test` (the dev loop's actual shape) dispatches to
// already-hot engines and pays only the per-file run cost. Workers are
// session-mode children ("--internal-test-worker --session") driven by
// framed messages on their pipes; they die automatically when the daemon
// and its pipe ends die. `capture_stdio`'s dup2-based capture is
// process-global, which is exactly why parallel engines stay in separate
// processes even though the daemon is always alive.
//
// Knobs: HONEY_TEST_POOL (max live workers; "0" disables the pool entirely
// and restores one-shot workers), HONEY_TEST_MAX_IDLE (idle workers retained
// between batches, default 8 to match the chunk cap — a hot pool makes every
// batch after the first spawn-free; idle engines measured ~22MB each),
// HONEY_TEST_PREWARM (workers pre-spawned at daemon start, default 4, "0"
// disables — the daemon starts them in the background so the first
// `honey test` after a cold daemon never pays the engine-init spawn).

static TEST_POOL: std::sync::Mutex<Vec<TestWorker>> = std::sync::Mutex::new(Vec::new());

struct TestWorker {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<std::process::ChildStdout>,
}

impl TestWorker {
    fn spawn() -> Result<TestWorker, String> {
        let exe = std::env::current_exe().map_err(|e| format!("failed to locate honey executable: {e}"))?;
        let mut cmd = std::process::Command::new(exe);
        cmd.args(["--internal-test-worker", "--session"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().map_err(|e| format!("failed to spawn batch worker: {e}"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        Ok(TestWorker { child, stdin, stdout })
    }

    /// Still running? A worker that died while idle (OOM, crash) is reaped
    /// (Drop kills + waits) so the pool never hands out a corpse.
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for TestWorker {
    fn drop(&mut self) {
        // Closing stdin tells a session worker to exit; kill covers a wedged
        // one, and wait() reaps it so no zombie is left behind.
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn test_pool_max_idle() -> usize {
    std::env::var("HONEY_TEST_MAX_IDLE")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(8)
}

fn test_pool_enabled() -> bool {
    std::env::var("HONEY_TEST_POOL").map(|v| !v.is_empty() && v == "0").unwrap_or(false) == false
}

/// Take an idle worker from the pool (skipping dead ones), or return None
/// when the pool is disabled or over its live-worker cap.
fn pool_take(max_live: usize) -> Option<TestWorker> {
    if !test_pool_enabled() || max_live == 0 {
        return None;
    }
    let mut pool = match TEST_POOL.try_lock() {
        Ok(p) => p,
        Err(_) => return None, // contention: prefer spawning over waiting
    };
    while let Some(mut w) = pool.pop() {
        if w.alive() {
            return Some(w);
        }
        // dead worker reaped by Drop
    }
    if pool.len() < max_live {
        TestWorker::spawn().ok()
    } else {
        None
    }
}

/// Return a worker to the idle pool after a successful batch, culling down
/// to HONEY_TEST_MAX_IDLE.
fn pool_retire(w: TestWorker) {
    let Ok(mut pool) = TEST_POOL.try_lock() else {
        return drop(w); // Drop kills+reaps
    };
    pool.push(w);
    let max_idle = test_pool_max_idle();
    while pool.len() > max_idle {
        if pool.pop().is_none() {
            break;
        }
    }
}

/// Run one test-batch chunk on a pool worker (session) with a one-shot
/// worker fallback. The worker returns to the pool on success.
fn run_batch_on_worker(request: Request) -> Result<Vec<Response>, String> {
    let pooled = pool_take(8);
    if pooled.is_some() {
        trace_log("pool: hit");
    } else {
        trace_log("pool: miss (one-shot fallback or fresh spawn)");
    }
    if let Some(mut w) = pooled {
        let mut send = || -> Result<Vec<Response>, String> {
            let stdin = w.stdin.as_mut().ok_or("batch worker: no stdin")?;
            write_msg(stdin, &request).map_err(|e| format!("batch worker: send failed: {e}"))?;
            let stdout = w.stdout.as_mut().ok_or("batch worker: no stdout")?;
            let bytes = read_msg(stdout).ok_or("batch worker: died before response")?;
            serde_json::from_slice::<TestBatchResponse>(&bytes)
                .map(|r| r.results)
                .map_err(|_| "batch worker: unreadable response".into())
        };
        match send() {
            Ok(results) => {
                pool_retire(w);
                return Ok(results);
            }
            Err(_) => {
                drop(w); // reaped; fall through to the one-shot fallback
            }
        }
    }
    run_worker_batch(request)
}

/// Send one Request to a fresh `--internal-test-worker` subprocess and parse
/// the TestBatchResponse (chunk of a parallel test run).
fn run_worker_batch(request: Request) -> Result<Vec<Response>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("failed to locate honey executable: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--internal-test-worker")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("failed to spawn batch worker: {e}"))?;
    {
        use std::io::Write as _;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = serde_json::to_writer(&mut stdin, &request);
            let _ = stdin.flush();
        }
    }
    drop(child.stdin.take());
    match child.wait_with_output() {
        Ok(out) => match serde_json::from_slice::<TestBatchResponse>(&out.stdout) {
            Ok(resp) => Ok(resp.results),
            Err(_) => {
                let hint: String = String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("no output")
                    .chars()
                    .take(160)
                    .collect();
                Err(format!("batch worker failed: {hint}"))
            }
        },
        Err(e) => Err(format!("batch worker failed: {e}")),
    }
}

fn handle_eval(request: &Request, runtime: &crate::runtime::Runtime) -> Response {
    let started = Instant::now();
    if !request.cwd.is_empty() {
        let _ = std::env::set_current_dir(&request.cwd);
    }
    match runtime.eval_code(&request.code) {
        Ok(json) => Response {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: started.elapsed().as_secs_f64() * 1000.0,
            error: None,
            // Eval-only field reuse: response frames never set `file` for
            // eval; carrying the JSON-serialized completion value keeps the
            // captured stdout free for the hook's own console output.
            file: json,
        },
        Err(e) => Response {
            exit_code: 1,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: started.elapsed().as_secs_f64() * 1000.0,
            error: Some(e),
            file: None,
        },
    }
}

fn handle_run(request: &Request, runtime: &crate::runtime::Runtime) -> Response {
    let started = Instant::now();
    trace_log(&format!("handle_run begin file={}", request.file));
    if !request.cwd.is_empty() {
        let _ = std::env::set_current_dir(&request.cwd);
    }

    // A preload (test shim) runs with the main module loaded by the wrapper
    // itself — the wrapper source executes INLINE in the engine (see
    // runtime::run_preload), so nothing transient is ever written to disk.
    let outcome = if request.preload.is_empty() {
        runtime.run_file(&request.file, &request.args)
    } else {
        // V2 test preloads run under the engine-registered harness: make sure
        // the shim is inside whichever engine run_preload lands on.
        if request.preload.starts_with(crate::runtime::TEST_SHIM_MARKER) {
            crate::runtime::request_test_shim();
        }
        runtime.run_preload(&request.file, &request.preload, &request.args)
    };
    let (exit_code, error) = match outcome {
        Ok(code) => (code, None),
        Err(e) => (1, Some(format!("{e:#}"))),
    };
    trace_log(&format!("run done code={exit_code} t={:?}", started.elapsed()));

    Response {
        exit_code,
        stdout: String::new(), // filled by capture wrapper
        stderr: String::new(),
        duration_ms: started.elapsed().as_secs_f64() * 1000.0,
        error,
        file: None,
    }
}

fn fail_response(error: String) -> Response {
    Response {
        exit_code: 1,
        stdout: String::new(),
        stderr: String::new(),
        duration_ms: 0.0,
        error: Some(error),
        file: None,
    }
}

/// Execute one request in a one-shot worker subprocess (fresh engine, own
/// stdio, so concurrent calls don't interfere). Used for preload (test)
/// runs by the daemon's connection threads and by the CLI's cold fallback.
pub fn run_in_worker(request: &Request) -> Response {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return fail_response(format!("failed to locate honey executable: {e:#}")),
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--internal-test-worker")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail_response(format!("failed to spawn test worker: {e:#}")),
    };
    {
        use std::io::Write as _;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = serde_json::to_writer(&mut stdin, request);
            let _ = stdin.flush();
        }
    }
    drop(child.stdin.take());
    match child.wait_with_output() {
        Ok(out) => match serde_json::from_slice::<Response>(&out.stdout) {
            Ok(resp) => resp,
            Err(_) => {
                let hint: String = String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("no output")
                    .chars()
                    .take(160)
                    .collect();
                fail_response(format!("test worker failed: {hint}"))
            }
        },
        Err(e) => fail_response(format!("test worker failed: {e:#}")),
    }
}

/// Entry point for `honey --internal-test-worker`: reads one JSON request
/// from stdin, executes it in a fresh engine, writes the JSON response to
/// stdout and exits with the run's exit code.
pub fn serve_test_worker() {
    use std::io::Read as _;
    // Session mode: serve framed requests on stdin until EOF (the persistent
    // test-worker pool keeps these alive between batches, engines warm).
    if std::env::args().any(|a| a == "--session") {
        serve_test_worker_session();
        return;
    }
    let mut buf = Vec::new();
    if std::io::stdin().read_to_end(&mut buf).is_err() {
        std::process::exit(2);
    }
    let Ok(req) = serde_json::from_slice::<Request>(&buf) else {
        let _ = std::io::stdout().write_all(
            serde_json::to_vec(&fail_response("test worker: unreadable request".into()))
                .unwrap_or_default()
                .as_slice(),
        );
        std::process::exit(2);
    };
    let runtime = crate::runtime::Runtime::new_warm();
    // Batch worker (cmd "test_batch"): run the files sequentially in this
    // worker's single engine and answer with a TestBatchResponse. One engine
    // init per worker instead of per file; the engine is fresh, so nothing
    // from the parent process's module state leaks in.
    if req.cmd == "test_batch" && !req.args.is_empty() {
        let mut results = Vec::with_capacity(req.args.len());
        let mut exit = 0;
        for (i, file) in req.args.iter().enumerate() {
            let preload = req.preloads.get(i).cloned().unwrap_or_default();
            crate::runtime::evict_module(file);
            let run_req = Request {
                cmd: "run".into(),
                file: file.clone(),
                cwd: req.cwd.clone(),
                preload,
                ..Default::default()
            };
            let (out, err, mut response) = capture_stdio(|| handle_run(&run_req, &runtime));
            response.stdout = out;
            response.stderr = err;
            response.file = Some(file.clone());
            if response.exit_code != 0 {
                exit = response.exit_code;
            }
            results.push(response);
        }
        let body = serde_json::to_vec(&TestBatchResponse { results }).unwrap_or_default();
        {
            use std::io::Write as _;
            let mut out = std::io::stdout();
            let _ = out.write_all(&body);
            let _ = out.flush();
        }
        std::process::exit(exit);
    }
    let (out, err, mut response) = capture_stdio(|| handle_run(&req, &runtime));
    response.stdout = out;
    response.stderr = err;
    let body = serde_json::to_vec(&response).unwrap_or_default();
    {
        use std::io::Write as _;
        let mut out = std::io::stdout();
        let _ = out.write_all(&body);
        let _ = out.flush();
    }
    std::process::exit(response.exit_code);
}

/// Session-mode test worker: read one framed Request at a time on stdin,
/// run it in the retained warm engine, write one framed Response (or
/// TestBatchResponse) back, repeat until stdin EOF or an "exit" request.
/// The pool's lifespan is bounded by the daemon's pipes — a dead daemon
/// closes stdin and the worker exits — so no reaping choreography is needed.
fn serve_test_worker_session() {
    let runtime = crate::runtime::Runtime::new_warm();
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    loop {
        let Some(bytes) = read_msg(&mut stdin) else { return };
        let Ok(req) = serde_json::from_slice::<Request>(&bytes) else { return };
        if req.cmd == "exit" {
            return;
        }
        if req.cmd == "test_batch" {
            // Chunk of files: run sequentially in this worker's one engine,
            // answering with a TestBatchResponse (one Response per file,
            // request order).
            let mut results = Vec::with_capacity(req.args.len());
            for (i, file) in req.args.iter().enumerate() {
                let preload = req.preloads.get(i).cloned().unwrap_or_default();
                crate::runtime::evict_module(file);
                let run_req = Request {
                    cmd: "run".into(),
                    file: file.clone(),
                    cwd: req.cwd.clone(),
                    preload,
                    ..Default::default()
                };
                let (out, err, mut response) = capture_stdio(|| handle_run(&run_req, &runtime));
                response.stdout = out;
                response.stderr = err;
                response.file = Some(file.clone());
                results.push(response);
            }
            let _ = write_msg(&mut stdout, &TestBatchResponse { results }).ok();
            continue;
        }
        // Unknown command: a fail response keeps framing in sync.
        let _ = write_msg(&mut stdout, &fail_response("test worker: unknown cmd".into())).ok();
    }
}

/// Run a batch of files in worker subprocesses, one thread per file,
/// concurrent workers capped at num_cpus. Results come back in input order
/// regardless of completion order.
fn run_batch(jobs: &[(String, String)], cwd: &str) -> Vec<Response> {
    let cap = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let results: std::sync::Mutex<Vec<Option<Response>>> =
        std::sync::Mutex::new((0..jobs.len()).map(|_| None).collect());
    let gate = std::sync::Mutex::new(0usize);
    let cv = std::sync::Condvar::new();
    std::thread::scope(|s| {
        for (i, (file, preload)) in jobs.iter().enumerate() {
            let results = &results;
            let gate = &gate;
            let cv = &cv;
            let req = Request {
                cmd: "run".into(),
                file: file.clone(),
                cwd: cwd.to_string(),
                preload: preload.clone(),
                args: vec![],
                preloads: vec![],
                code: String::new(),
                ..Default::default()
            };
            s.spawn(move || {
                // Cap concurrent workers at num_cpus.
                let mut busy = gate.lock().unwrap();
                while *busy >= cap {
                    busy = cv.wait(busy).unwrap();
                }
                *busy += 1;
                drop(busy);
                let resp = run_in_worker(&req);
                let mut busy = gate.lock().unwrap();
                *busy -= 1;
                cv.notify_one();
                drop(busy);
                results.lock().unwrap()[i] = Some(resp);
            });
        }
    });
    let out = results.into_inner().unwrap();
    out.into_iter()
        .map(|r| r.unwrap_or_else(|| fail_response("test worker produced no response".into())))
        .collect()
}

// ---------- test watch mode ----------

/// Long-lived test watch session for `honey test --watch`: run the batch
/// (one worker subprocess per file, up to num_cpus concurrent), stream one
/// response frame per file plus a batch-summary marker frame, then watch the
/// test files (inotify on Linux, kqueue on macOS); an edit re-runs only the
/// affected files. Events that land mid-run are consumed by the post-run
/// drain, so one editor save yields exactly one re-run. Ends when the client
/// disconnects.
fn watch_test_session(stream: &mut UnixStream, request: &Request) {
    use std::os::unix::io::AsRawFd;
    let Some(mut watcher) = crate::watch::FileWatcher::new() else {
        return;
    };
    let cwd = request.cwd.clone();
    let jobs: Vec<(String, String)> = request
        .args
        .iter()
        .enumerate()
        .map(|(i, f)| (f.clone(), request.preloads.get(i).cloned().unwrap_or_default()))
        .collect();
    let file_set: std::collections::HashSet<String> =
        jobs.iter().map(|(f, _)| f.clone()).collect();
    let cfd = stream.as_raw_fd();
    let mut to_run = jobs.clone();
    'sessions: loop {
        let started = Instant::now();
        for (mut resp, (file, _)) in run_batch(&to_run, &cwd).into_iter().zip(to_run.iter()) {
            resp.file = Some(file.clone());
            if write_msg(stream, &resp).is_err() {
                break 'sessions;
            }
        }
        // Batch marker: file=None, duration carries the batch wall time.
        let _ = write_msg(stream, &Response {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: started.elapsed().as_secs_f64() * 1000.0,
            error: None,
            file: None,
        });
        trace_log("test watch: batch done, armed, awaiting events");

        for (file, _) in &jobs {
            watcher.watch_file(file);
        }
        // Events from before this point describe the run we just did.
        watcher.drain();

        // Wait for the next relevant event or the client hanging up.
        loop {
            let mut pfds = [
                libc::pollfd { fd: watcher.fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: cfd, events: client_poll_events(), revents: 0 },
            ];
            let r = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
            if r < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break 'sessions;
            }
            if pfds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                break 'sessions; // client went away (Ctrl+C)
            }
            if pfds[1].revents & libc::POLLIN != 0 {
                break 'sessions; // clients don't send data mid-watch
            }
            if pfds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                // The post-run drain already consumed this event batch; read
                // whatever event payload this single read yields.
                let changed = watcher.poll_changed(&file_set);
                watcher.drain();
                if changed.is_empty() {
                    continue; // unrelated file event (editor temp etc.); keep waiting
                }
                // Re-run only the affected test files.
                to_run = jobs
                    .iter()
                    .filter(|(f, _)| changed.contains(f))
                    .cloned()
                    .collect();
                continue 'sessions;
            }
        }
    }
    watcher.close();
}

// ---------- stdio capture ----------

/// A queued capture pair for the persistent drainer thread.
struct CaptureJob {
    out_fd: libc::c_int,
    err_fd: libc::c_int,
    /// Forwarded chunks; None = both pipes at EOF (job done).
    tx: std::sync::mpsc::Sender<Option<CaptureChunk>>,
}

#[derive(Debug)]
enum CaptureChunk {
    Out(Vec<u8>),
    Err(Vec<u8>),
}

/// Whether this process is the daemon (long-lived, owns the warm engine and
/// the persistent drainer) rather than a one-shot CLI run.
fn is_daemon() -> bool {
    static DAEMON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DAEMON.get_or_init(|| std::env::args().any(|a| a == "--internal-daemon"))
}

/// Lazily started persistent drainer, daemon-only: sleeps in recv() between
/// requests, so a warm run pays no thread spawn/join at all (that cost
/// ~60-80µs of every request when we spun a thread per capture).
fn capture_queue() -> Option<&'static std::sync::mpsc::SyncSender<CaptureJob>> {
    static QUEUE: std::sync::OnceLock<Option<std::sync::mpsc::SyncSender<CaptureJob>>> =
        std::sync::OnceLock::new();
    QUEUE
        .get_or_init(|| {
            if !is_daemon() {
                return None; // one-shot runs spawn a throwaway drainer instead
            }
            let (tx, rx) = std::sync::mpsc::sync_channel::<CaptureJob>(0);
            if std::thread::Builder::new()
                .name("honey-stdio-drain".into())
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        drain_capture_pipes(job);
                    }
                })
                .is_err()
            {
                return None;
            }
            Some(tx)
        })
        .as_ref()
}

/// Drain one capture pair: poll both fds, forward chunks through `tx`, then
/// send the None EOF marker once both read ends are closed.
fn drain_capture_pipes(job: CaptureJob) {
    use std::os::unix::io::FromRawFd;

    let CaptureJob { out_fd, err_fd, tx } = job;
    let mut pfds = [
        libc::pollfd { fd: out_fd, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: err_fd, events: libc::POLLIN, revents: 0 },
    ];
    let mut files = [
        unsafe { std::fs::File::from_raw_fd(out_fd) },
        unsafe { std::fs::File::from_raw_fd(err_fd) },
    ];
    let mut buf = [0u8; 65536];
    loop {
        let r = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
        if r < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        let mut open = 0;
        for i in 0..2 {
            let pfd = &mut pfds[i];
            if pfd.events == 0 {
                continue; // already at EOF
            }
            if pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                match files[i].read(&mut buf) {
                    Ok(0) => {
                        pfd.events = 0;
                        continue;
                    }
                    Ok(n) => {
                        let chunk = if i == 0 {
                            CaptureChunk::Out(buf[..n].to_vec())
                        } else {
                            CaptureChunk::Err(buf[..n].to_vec())
                        };
                        // Unbounded channel: send never blocks, so a large
                        // burst can't deadlock the drainer against a full
                        // pipe while the request thread is still running JS.
                        if tx.send(Some(chunk)).is_err() {
                            return;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        pfd.events = 0;
                        continue;
                    }
                }
            }
            open += 1;
        }
        if open == 0 {
            break; // both pipes closed
        }
    }
    let _ = tx.send(None);
}

/// Wrap a closure with fd 1/2 redirected into pipes, returning what was
/// written. Everything the runtime prints (including v8 callbacks and
/// eprintln) is captured. In the daemon the pipes are drained by one
/// persistent worker thread; one-shot runs fall back to a dedicated thread.
pub fn capture_stdio<T>(f: impl FnOnce() -> T) -> (String, String, T) {
    let mut out_pipe = [0 as libc::c_int; 2];
    let mut err_pipe = [0 as libc::c_int; 2];
    unsafe {
        libc::pipe(out_pipe.as_mut_ptr());
        libc::pipe(err_pipe.as_mut_ptr());
    }

    let saved_out = unsafe { libc::dup(1) };
    let saved_err = unsafe { libc::dup(2) };
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    unsafe {
        libc::dup2(out_pipe[1], 1);
        libc::dup2(err_pipe[1], 2);
    }

    let (tx, rx) = std::sync::mpsc::channel::<Option<CaptureChunk>>();
    let job = CaptureJob { out_fd: out_pipe[0], err_fd: err_pipe[0], tx };
    // Try the persistent worker first (rendezvous handoff: try_send only
    // succeeds when the worker is idle in recv()); on failure or outside
    // the daemon, drain on a dedicated throwaway thread.
    enum Dispatch {
        Persistent,
        Thread(std::thread::JoinHandle<()>),
    }
    let dispatch = match capture_queue() {
        Some(q) => match q.try_send(job) {
            Ok(()) => Dispatch::Persistent,
            Err(
                std::sync::mpsc::TrySendError::Full(job)
                | std::sync::mpsc::TrySendError::Disconnected(job),
            ) => Dispatch::Thread(std::thread::spawn(move || drain_capture_pipes(job))),
        },
        None => Dispatch::Thread(std::thread::spawn(move || drain_capture_pipes(job))),
    };

    let result = f();

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    unsafe {
        libc::dup2(saved_out, 1);
        libc::dup2(saved_err, 2);
        libc::close(saved_out);
        libc::close(saved_err);
        libc::close(out_pipe[1]);
        libc::close(err_pipe[1]);
    }

    // Collect until the EOF marker (both write ends are closed above, so the
    // drainer always finishes). No join and no thread teardown on the daemon
    // hot path.
    let mut out_bytes = Vec::new();
    let mut err_bytes = Vec::new();
    while let Ok(next) = rx.recv() {
        match next {
            Some(CaptureChunk::Out(c)) => out_bytes.extend_from_slice(&c),
            Some(CaptureChunk::Err(c)) => err_bytes.extend_from_slice(&c),
            None => break,
        }
    }
    if let Dispatch::Thread(h) = dispatch {
        let _ = h.join();
    }

    (
        String::from_utf8_lossy(&out_bytes).into_owned(),
        String::from_utf8_lossy(&err_bytes).into_owned(),
        result,
    )
}

// ---------- watch mode ----------

/// What the client socket subscribes to in a watch session. POLLRDHUP is
/// Linux-only; on macOS, POLLHUP/POLLERR in revents carry the hangup.
#[cfg(target_os = "linux")]
fn client_poll_events() -> i16 {
    libc::POLLIN | libc::POLLRDHUP
}

#[cfg(not(target_os = "linux"))]
fn client_poll_events() -> i16 {
    libc::POLLIN
}

fn abs_main_path(request: &Request) -> String {
    if std::path::Path::new(&request.file).is_absolute() {
        return request.file.clone();
    }
    let base = if request.cwd.is_empty() {
        std::env::current_dir().unwrap_or_default()
    } else {
        PathBuf::from(&request.cwd)
    };
    base.join(&request.file).to_string_lossy().into_owned()
}

/// Long-lived watch session: run, stream one response frame, wait for file
/// events via the platform watcher (inotify on Linux, kqueue on macOS; no
/// polling), re-run on the warm engine, repeat. Any events that arrive
/// mid-run are consumed by the post-run drain, so one editor save yields
/// exactly one re-run. Ends when the client disconnects.
fn watch_session(
    stream: &mut UnixStream,
    request: &Request,
    run_tx: &std::sync::mpsc::Sender<RunJob>,
) {
    use std::os::unix::io::AsRawFd;
    let Some(mut watcher) = crate::watch::FileWatcher::new() else {
        return;
    };
    let cfd = stream.as_raw_fd();
    'sessions: loop {
        // Every refresh runs on the dedicated run-engine thread.
        let mut response = on_run_engine(run_tx, request.clone(), false).unwrap_or(Response {
            exit_code: 1,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 0.0,
            error: Some("run engine unavailable".into()),
            file: None,
        });
        if write_msg(stream, &response).is_err() {
            break;
        }
        trace_log("watch: frame written");

        // Rebuild the dep set: main file + everything required last run.
        let mut deps = crate::runtime::recorded_dep_mtimes();
        let main = abs_main_path(request);
        if let Some(m) = crate::runtime::stat_path_mtime(&main) {
            deps.push((main.clone(), m));
        }
        for (path, _) in &deps {
            watcher.watch_path(path);
            if let Some(dir) = std::path::Path::new(path).parent() {
                let dir = dir.to_string_lossy().into_owned();
                watcher.watch_path(&dir);
            }
        }
        // Events from before this point describe the run we just did.
        watcher.drain();
        trace_log("watch: armed, awaiting events");

        // Wait for the next relevant event or the client hanging up.
        loop {
            let mut pfds = [
                libc::pollfd { fd: watcher.fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: cfd, events: client_poll_events(), revents: 0 },
            ];
            let r = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
            if r < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break 'sessions;
            }
            if pfds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                break 'sessions; // client went away (Ctrl+C)
            }
            if pfds[1].revents & libc::POLLIN != 0 {
                break 'sessions; // clients don't send data mid-watch
            }
            if pfds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                trace_log("watch: event, re-running");
                continue 'sessions; // something changed on disk
            }
        }
    }
    watcher.close();
}

// ---------- client ----------

/// Append a timestamped line to $HONEY_CLIENT_LOG when set (client-side
/// timing diagnostics; no effect in normal runs).
pub fn client_trace(msg: &str) {
    let Ok(path) = std::env::var("HONEY_CLIENT_LOG") else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write as _;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let _ = writeln!(f, "[{:?}] {}", now, msg);
    }
}

fn connect() -> Option<UnixStream> {
    UnixStream::connect(socket_path()).ok()
}

/// Path of the daemon pid file: the single source of truth for "is there a
/// live daemon", because the socket file lags (dead listeners leave it) and
/// /proc/net/unix lies (unlinked zombies keep the path).
fn pid_file_path() -> PathBuf {
    socket_path().with_extension("pid")
}

/// Is `pid` a live honey-full daemon? kill(pid,0) says alive-but-maybe-other
/// program, so also confirm the cmdline.
fn daemon_pid_alive(pid: u32) -> bool {
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return false;
    }
    std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
        .map(|c| c.contains("honey-full") && c.contains("internal-daemon"))
        .unwrap_or(false)
}

/// Is a daemon currently running, per the pid file?
fn daemon_process_alive() -> bool {
    let pf = pid_file_path();
    std::fs::read_to_string(&pf)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map(daemon_pid_alive)
        .unwrap_or(false)
}

/// Where spawned-daemon crash output goes (last ~64KB kept useful by size cap
/// at open time not enforced — one line per crash, realistically tiny).
fn daemon_stderr_log() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| std::path::PathBuf::from(h).join(".cache")))
        .map(|d| d.join("honey").join("daemon-crash.log"))
}

fn spawn_daemon() -> Result<()> {
    // First run: install + `enable --now` the daemon service (systemd user
    // unit on Linux, launchd agent on macOS), silently and once per binary.
    // If the service manager is bringing a daemon up, wait for its socket
    // instead of racing it with our own fork (the systemd instance would
    // lose the bind and go inactive).
    if crate::service::ensure_first_run() {
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            if let Some(s) = connect() {
                drop(s);
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        return Ok(()); // service start may still be initializing; don't race it
    }
    if daemon_process_alive() || connect().is_some() {
        return Ok(());
    }
    // Serialize with other spawning clients: under the same lock the daemon
    // claims the socket, re-check the pid file. A burst of concurrent clients
    // must produce ONE spawn, not one per thread.
    let lock_path = pid_file_path().with_extension("lock");
    if let Ok(lock) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
    {
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
            if let Ok(txt) = std::fs::read_to_string(pid_file_path()) {
                if let Ok(pid) = txt.trim().parse::<u32>() {
                    if daemon_pid_alive(pid) {
                        return Ok(()); // someone won the race already
                    }
                }
            }
            // Hold the lock while fork+exec starts: the child re-acquires it
            // itself in serve(), so drop only after the process exists.
            let spawned = (|| -> Result<()> {
                trace_log("client: spawning daemon (connect failed, lock held)");
                spawn_daemon_locked()
            })();
            drop(lock);
            return spawned;
        }
    }
    spawn_daemon_locked()
}

fn spawn_daemon_locked() -> Result<()> {
    let exe = std::env::current_exe().context("failed to locate honey executable")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--internal-daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    // Keep stderr: a daemon that dies (V8 CHECK abort, Rust panic) must leave
    // its last words somewhere readable, or a crash shows up only as a ~1s
    // stall and a respawn. Cost: one O_APPEND fd per daemon lifetime.
    if let Some(f) = daemon_stderr_log() {
        match std::fs::OpenOptions::new().create(true).append(true).open(&f) {
            Ok(file) => {
                cmd.stderr(std::process::Stdio::from(file));
            }
            Err(_) => {
                cmd.stderr(std::process::Stdio::null());
            }
        }
    } else {
        cmd.stderr(std::process::Stdio::null());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let _ = cmd.process_group(0);
    }
    cmd.spawn().context("failed to spawn honey daemon")?;
    Ok(())
}

fn request_raw(req: &Request, mut stream: UnixStream) -> Option<Vec<u8>> {
    write_msg(&mut stream, req).ok()?;
    // Spin briefly before blocking: a warm request answers in well under
    // 300µs, and busy-polling avoids two scheduler round-trips (client
    // descheduling -> daemon wake -> client wake + reschedule) that cost
    // more wall time than the spin itself, especially under load.
    {
        use std::os::unix::io::AsRawFd;
        let fd = stream.as_raw_fd();
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let deadline = std::time::Instant::now() + std::time::Duration::from_micros(500);
        loop {
            let r = unsafe { libc::poll(&mut pfd, 1, 0) };
            if r != 0 {
                break; // data ready (or error; read_msg will surface it)
            }
            if std::time::Instant::now() >= deadline {
                break; // fall through to blocking read
            }
            std::hint::spin_loop();
        }
    }
    read_msg(&mut stream)
}

fn request(req: &Request, stream: UnixStream) -> Option<Response> {
    serde_json::from_slice(&request_raw(req, stream)?).ok()
}

/// Write a request and read one reply frame. Callers that already bounded
/// the connect phase use this for the exchange itself.
fn request_raw_after_connect(req: &Request, stream: &mut UnixStream) -> Option<Vec<u8>> {
    write_msg(stream, req).ok()?;
    read_msg(stream)
}

/// `honey run --watch`: open a watch session with the daemon and stream each
/// run's output as it happens. Returns None when HONEY_TRACE or HONEY_NO_DAEMON
/// is set (trace output must reach the real terminal; the caller falls back to
/// an inline watcher), which the caller handles.
pub fn watch_via_daemon(file: &str, args: &[String]) -> Option<()> {
    if std::env::var("HONEY_TRACE").is_ok() {
        return None;
    }
    if std::env::var("HONEY_NO_DAEMON").map(|v| !v.is_empty() && v != "0").unwrap_or(false) {
        return None;
    }
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let req = Request {
        cmd: "watch".into(),
        file: file.to_string(),
        args: args.to_vec(),
        cwd,
        ..Default::default()
    };
    let mut stream = connect().or_else(|| wait_for_socket(Duration::from_millis(500)))?;
    write_msg(&mut stream, &req).ok()?;
    loop {
        match read_msg(&mut stream) {
            None => return Some(()), // daemon closed the session; done
            Some(bytes) => {
                let resp: Response = match serde_json::from_slice(&bytes) {
                    Ok(r) => r,
                    Err(_) => return Some(()),
                };
                print!("{}", resp.stdout);
                eprint!("{}", resp.stderr);
                if let Some(err) = &resp.error {
                    eprintln!("error: {err}");
                }
                let _ = std::io::stdout().flush();
                let _ = std::io::stderr().flush();
            }
        }
    }
}

fn wait_for_socket(timeout: Duration) -> Option<UnixStream> {
    let deadline = Instant::now() + timeout;
    let mut spawned = false;
    loop {
        if let Some(stream) = connect() {
            return Some(stream);
        }
        // Only spawn when the pid file says nobody's home. If a daemon is
        // alive, the socket must be mid-dance (crash cleanup, restart) —
        // poll, don't pile on spawns.
        if !spawned && !daemon_process_alive() {
            spawned = true;
            let _ = spawn_daemon();
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Send a `run` request (with an optional preload / test shim wrapper) to the
/// daemon, auto-starting it if needed. Returns None if the daemon is
/// unavailable or HONEY_NO_DAEMON/HONEY_TRACE is set, in which case the
/// caller should fall back (worker subprocess or inline execution).
pub fn run_with_preload_via_daemon(file: &str, preload: &str) -> Option<Response> {
    let started = std::time::Instant::now();
    client_trace("run_with_preload_via_daemon entered");
    if std::env::var("HONEY_NO_DAEMON").map(|v| !v.is_empty() && v != "0").unwrap_or(false) {
        return None;
    }
    if std::env::var("HONEY_TRACE").is_ok() {
        return None; // trace output must reach the real terminal
    }

    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let req = Request {
        cmd: "run".into(),
        file: file.to_string(),
        args: vec![],
        cwd,
        preload: preload.to_string(),
        preloads: vec![],
        code: String::new(),
        ..Default::default()
    };

    let stream = match connect() {
        Some(s) => Some(s),
        None => wait_for_socket(Duration::from_millis(500)),
    }?;
    client_trace(&format!("connected (t={}us since entry)", started.elapsed().as_micros()));

    let resp = request(&req, stream);
    client_trace(&format!("response received (t={}us since entry)", started.elapsed().as_micros()));
    resp
}

/// Run one `honey build` in the daemon (cmd "build"). Returns None when the
/// daemon is unavailable/disabled or speaks an older protocol, in which case
/// the caller builds locally.
pub fn build_via_daemon(
    entry: Option<&str>,
    output: Option<&str>,
    opts: &crate::build::BuildOptions,
) -> Option<Response> {
    if std::env::var("HONEY_NO_DAEMON").map(|v| !v.is_empty() && v != "0").unwrap_or(false) {
        return None;
    }
    if std::env::var("HONEY_TRACE").is_ok() {
        return None;
    }
    let sourcemap = opts.sourcemap.map(|m| match m {
        crate::build::SourceMapMode::Linked => "linked",
        crate::build::SourceMapMode::External => "external",
        crate::build::SourceMapMode::Inline => "inline",
        crate::build::SourceMapMode::None => "none",
    });
    let target = opts.target.map(|t| match t {
        crate::build::BuildTarget::Browser => "browser",
        crate::build::BuildTarget::Node => "node",
    });
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let req = Request {
        cmd: "build".into(),
        cwd,
        build_entry: entry.map(str::to_string),
        build_output: output.map(str::to_string),
        build_production: opts.production,
        build_minify: opts.minify,
        build_sourcemap: sourcemap.map(str::to_string),
        build_target: target.map(str::to_string),
        build_define: opts.define.iter().map(|(k, v)| format!("{k}={v}")).collect(),
        ..Default::default()
    };
    let stream = connect().or_else(|| wait_for_socket(Duration::from_millis(500)))?;
    request(&req, stream)
}

/// Client side of `honey build --watch` (cmd "build_watch"): the daemon hosts
/// the watch loop and streams one Response frame per rebuild, which this
/// prints as it arrives. Returns false when the daemon is unavailable or
/// disabled, or when an older daemon ignores the command (no first frame
/// within 1s), so the caller falls back to the inline watcher. The session
/// ends when the daemon sees the client disconnect (Ctrl-C).
pub fn build_watch_via_daemon(
    entry: Option<&str>,
    output: Option<&str>,
    opts: &crate::build::BuildOptions,
) -> bool {
    if std::env::var("HONEY_NO_DAEMON").map(|v| !v.is_empty() && v != "0").unwrap_or(false) {
        return false;
    }
    if std::env::var("HONEY_TRACE").is_ok() {
        return false; // trace output must reach the real terminal
    }
    let sourcemap = opts.sourcemap.map(|m| match m {
        crate::build::SourceMapMode::Linked => "linked",
        crate::build::SourceMapMode::External => "external",
        crate::build::SourceMapMode::Inline => "inline",
        crate::build::SourceMapMode::None => "none",
    });
    let target = opts.target.map(|t| match t {
        crate::build::BuildTarget::Browser => "browser",
        crate::build::BuildTarget::Node => "node",
    });
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let req = Request {
        cmd: "build_watch".into(),
        cwd,
        build_entry: entry.map(str::to_string),
        build_output: output.map(str::to_string),
        build_production: opts.production,
        build_minify: opts.minify,
        build_sourcemap: sourcemap.map(str::to_string),
        build_target: target.map(str::to_string),
        build_define: opts.define.iter().map(|(k, v)| format!("{k}={v}")).collect(),
        ..Default::default()
    };
    let mut stream = match connect().or_else(|| wait_for_socket(Duration::from_millis(500))) {
        Some(s) => s,
        None => return false,
    };
    if write_msg(&mut stream, &req).is_err() {
        return false;
    }
    use std::io::Write as _;
    // A daemon running an older protocol ignores unknown commands: bound the
    // wait for the first frame so we can still fall back inline.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let first = read_msg(&mut stream);
    let _ = stream.set_read_timeout(None);
    let first = match first {
        Some(bytes) => match serde_json::from_slice::<Response>(&bytes) {
            Ok(r) => r,
            Err(_) => return false,
        },
        None => return false,
    };
    let mut print_frame = |r: Response| {
        print!("{}", r.stdout);
        if !r.stderr.is_empty() {
            eprint!("{}", r.stderr);
        }
        if let Some(err) = r.error {
            eprintln!("error: {err}");
        }
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
    };
    print_frame(first);
    loop {
        match read_msg(&mut stream) {
            Some(bytes) => match serde_json::from_slice::<Response>(&bytes) {
                Ok(r) => print_frame(r),
                Err(_) => break,
            },
            None => break,
        }
    }
    true
}

/// Run a whole test suite in the daemon's warm engine (cmd "test_batch").
/// Returns one Response per file in request order, or None when the daemon is
/// unavailable/disabled or speaks an older protocol (caller falls back to the
/// worker-subprocess path).
pub fn test_batch_via_daemon(jobs: &[(String, String)]) -> Option<Vec<Response>> {
    test_batch_via_daemon_with_snapshot(jobs, snapshot_update_enabled_on_client())
}

/// Client env snapshot-update flag, read here so every caller is consistent.
fn snapshot_update_enabled_on_client() -> bool {
    matches!(std::env::var("HONEY_SNAPSHOT_UPDATE").as_deref(), Ok("1"))
}

/// Same as test_batch_via_daemon, but overrides the snapshot-update flag the
/// daemon uses when it builds the shim wrappers (preloads stay empty).
pub fn test_batch_via_daemon_with_snapshot(jobs: &[(String, String)], snapshot_update: bool) -> Option<Vec<Response>> {
    if jobs.is_empty() {
        return None;
    }
    if std::env::var("HONEY_NO_DAEMON").map(|v| !v.is_empty() && v != "0").unwrap_or(false) {
        return None;
    }
    if std::env::var("HONEY_TRACE").is_ok() {
        return None;
    }
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let req = Request {
        cmd: "test_batch".into(),
        args: jobs.iter().map(|(f, _)| f.clone()).collect(),
        cwd,
        preloads: jobs.iter().map(|(_, p)| p.clone()).collect(),
        snapshot_update,
        ..Default::default()
    };
    let mut stream = connect().or_else(|| wait_for_socket(Duration::from_millis(500)))?;
    let body = {
        // A wedged engine used to hang the client forever; a deadline turned
        // that into a fallback storm instead (the daemon serves requests
        // serially, so queued asks can legitimately wait >2s under load).
        // Keep the bounded wait only on the CONNECT phase above; the
        // response wait is unbounded here, and daemon-side dead-client
        // reads are bounded by SO_RCVTIMEO instead.
        request_raw_after_connect(&req, &mut stream)?
    };
    let resp: TestBatchResponse = serde_json::from_slice(&body).ok()?;
    if resp.results.len() == jobs.len() {
        Some(resp.results)
    } else {
        None
    }
}

/// Client side of the test watch session (`honey test --watch`): one request,
/// then response frames as they are produced (file frames, then a batch marker
/// with file=None). Returns None when the daemon is unavailable or disabled.
pub fn test_watch_request(files: &[String], preloads: &[String]) -> Option<TestWatchSession> {
    if std::env::var("HONEY_TRACE").is_ok() {
        return None;
    }
    if std::env::var("HONEY_NO_DAEMON").map(|v| !v.is_empty() && v != "0").unwrap_or(false) {
        return None;
    }
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let req = Request {
        cmd: "watch".into(),
        file: String::new(),
        args: files.to_vec(),
        cwd,
        preload: String::new(),
        preloads: preloads.to_vec(),
        code: String::new(),
        ..Default::default()
    };
    let mut stream = connect().or_else(|| wait_for_socket(Duration::from_millis(500)))?;
    write_msg(&mut stream, &req).ok()?;
    Some(TestWatchSession { stream })
}

/// A live test watch session: yields one Response per completed file run and
/// one marker Response (file = None) per batch.
pub struct TestWatchSession {
    stream: UnixStream,
}

impl Iterator for TestWatchSession {
    type Item = Response;
    fn next(&mut self) -> Option<Response> {
        let bytes = read_msg(&mut self.stream)?;
        serde_json::from_slice(&bytes).ok()
    }
}

/// Send a `run` request to the daemon, auto-starting it if needed.
/// Returns None if the daemon is unavailable or HONEY_NO_DAEMON is set,
/// in which case the caller should execute inline.
pub fn run_via_daemon(file: &str, args: &[String]) -> Option<Response> {
    let started = std::time::Instant::now();
    client_trace("run_via_daemon entered");
    if std::env::var("HONEY_NO_DAEMON").map(|v| !v.is_empty() && v != "0").unwrap_or(false) {
        return None;
    }
    if std::env::var("HONEY_TRACE").is_ok() {
        return None; // trace output must reach the real terminal
    }

    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let req = Request {
        cmd: "run".into(),
        file: file.to_string(),
        args: args.to_vec(),
        cwd,
        ..Default::default()
    };

    let stream = match connect() {
        Some(s) => Some(s),
        None => wait_for_socket(Duration::from_millis(500)),
    }?;
    client_trace(&format!("connected (t={}us since entry)", started.elapsed().as_micros()));

    let resp = request(&req, stream);
    client_trace(&format!("response received (t={}us since entry)", started.elapsed().as_micros()));
    resp
}

/// Evaluate a config-hook script in the daemon's warm engine. Returns None
/// when the daemon is disabled or unreachable (callers fall back in-process).
pub fn eval_via_daemon(code: &str) -> Option<Response> {
    if std::env::var("HONEY_NO_DAEMON").map(|v| !v.is_empty() && v != "0").unwrap_or(false) {
        return None;
    }
    if std::env::var("HONEY_TRACE").is_ok() {
        return None;
    }
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let req = Request {
        cmd: "eval".into(),
        code: code.to_string(),
        cwd,
        ..Default::default()
    };
    let stream = connect()?;
    request(&req, stream)
}

/// `honey start`: spawn the daemon and wait until its socket is live.
/// Returns the child pid and whether a daemon was already running.
pub fn start_daemon() -> Result<(u32, bool)> {
    // If a healthy daemon is already live, don't spawn another.
    if let Some(mut s) = connect() {
        // A jammed daemon must not wedge this process forever.
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
        if request(
            &Request { cmd: "ping".into(), file: String::new(), args: vec![], cwd: String::new(), ..Default::default() },
            s,
        )
        .is_some()
        {
            return Ok((0, true));
        }
    }
    let exe = std::env::current_exe().context("failed to locate honey executable")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--internal-daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    match daemon_stderr_log()
        .and_then(|f| std::fs::OpenOptions::new().create(true).append(true).open(&f).ok())
    {
        Some(file) => {
            cmd.stderr(std::process::Stdio::from(file));
        }
        None => {
            cmd.stderr(std::process::Stdio::null());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let _ = cmd.process_group(0);
    }
    let child = cmd.spawn().context("failed to spawn honey daemon")?;
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(mut s) = connect() {
            if request(
                &Request { cmd: "ping".into(), file: String::new(), args: vec![], cwd: String::new(), ..Default::default() },
                s,
            )
            .is_some()
            {
                return Ok((pid, false));
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!("daemon did not come up within 5s")
}

/// `honey stop`: ask the daemon to exit and remove its socket.
pub fn stop_daemon() -> Result<()> {
    let stream = connect().context("no honey daemon running")?;
    let response = request(
        &Request { cmd: "stop".into(), file: String::new(), args: vec![], cwd: String::new(), ..Default::default() },
        stream,
    )
    .context("daemon did not answer stop")?;
    let _ = response;
    Ok(())
}
