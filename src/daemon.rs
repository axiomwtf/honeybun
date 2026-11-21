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
pub fn serve() {
    let path = socket_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Another daemon may already be live: ping it.
            let already_live = UnixStream::connect(&path).ok().and_then(|mut s| {
                // A jammed daemon must not wedge this process forever.
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
                let ping = Request {
                    cmd: "ping".into(),
                    file: String::new(),
                    args: vec![],
                    cwd: String::new(),
                    ..Default::default()
                };
                write_msg(&mut s, &ping).ok()?;
                read_msg(&mut s)
            });
            if already_live.is_some() {
                return; // daemon already running
            }
            let _ = std::fs::remove_file(&path);
            match UnixListener::bind(&path) {
                Ok(l) => l,
                Err(_) => return, // lost the race; the other daemon serves
            }
        }
        Err(_) => return,
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    // One warm engine for the whole daemon lifetime: first request builds
    // the V8 isolate, later runs reuse it (module cache included).
    let runtime = crate::runtime::Runtime::new_warm();

    for stream in listener.incoming() {
        trace_log("accepted connection");
        let Ok(mut stream) = stream else { continue };
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
                let out_body = serve_test_batch(req, &runtime);
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
                let rt = &runtime;
                trace_log("run: entering capture_stdio");
                let (out, err, mut response) = capture_stdio(move || handle_run(&req, rt));
                trace_log("run: capture done");
                response.stdout = out;
                response.stderr = err;
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
                watch_session(&mut stream, &request, &runtime);
                trace_log("watch: session closed");
            }
            "eval" => {
                // Config hook evaluation: pure script in the warm engine.
                let req = request.clone();
                let rt = &runtime;
                let (out, err, mut response) = capture_stdio(move || handle_eval(&req, rt));
                response.stdout = out;
                response.stderr = err;
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
    let _ = std::fs::remove_file(&path);
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
fn serve_test_batch(req: Request, runtime: &crate::runtime::Runtime) -> TestBatchResponse {
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

    if sequential || force_workers || n < 8 || chunk_count <= 1 {
        return serve_test_batch_sequential(req, runtime);
    }
    serve_test_batch_parallel(req, chunk_count)
}

/// Original sequential path: every file runs in the daemon's retained engine,
/// evicting the module first so unchanged files still re-execute.
fn serve_test_batch_sequential(req: Request, runtime: &crate::runtime::Runtime) -> TestBatchResponse {
    let rt = runtime;
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
        let (out, err, mut response) = capture_stdio(move || handle_run(&run_req, rt));
        response.stdout = out;
        response.stderr = err;
        response.file = Some(file.clone());
        results.push(response);
    }
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
// batch after the first spawn-free; idle engines measured ~22MB each).

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

    // A preload (test shim) is materialized as a wrapper module that runs the
    // shim first and then loads the main module itself, so both share one
    // engine. The wrapper lives in the system temp dir (nothing transient is
    // ever written into a user project) and is removed after the run.
    let (file, wrapper) = if request.preload.is_empty() {
        (request.file.clone(), None)
    } else {
        match write_wrapper_file(&request.preload) {
            Ok(p) => (p.to_string_lossy().into_owned(), Some(p)),
            Err(e) => {
                return Response {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: String::new(),
                    duration_ms: started.elapsed().as_secs_f64() * 1000.0,
                    error: Some(format!("failed to write test wrapper: {e:#}")),
                    file: None,
                }
            }
        }
    };

    // V2 test preloads run under the engine-registered harness: make sure
    // the shim is inside whichever engine run_file lands on.
    if !request.preload.is_empty() && request.preload.starts_with(crate::runtime::TEST_SHIM_MARKER) {
        crate::runtime::request_test_shim();
    }

    let mut error = None;
    let exit_code = match runtime.run_file(&file, &request.args) {
        Ok(code) => code,
        Err(e) => {
            error = Some(format!("{e:#}"));
            1
        }
    };
    if let Some(p) = wrapper {
        let _ = std::fs::remove_file(p);
    }
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

/// A unique temp file for one preload wrapper module.
fn write_wrapper_file(source: &str) -> Result<std::path::PathBuf> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        "honey-preload-{}-{}.js",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, source).context("failed to write preload wrapper")?;
    Ok(path)
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

/// Watch one test file itself plus its parent directory (file watches catch
/// in-place writes; dir watches catch atomic saves and new files). Returns
/// the watch descriptor, recorded so inotify events can be mapped back to
/// the affected test file.
fn inotify_watch(
    fd: i32,
    path: &std::path::Path,
    watches: &mut std::collections::HashSet<String>,
    wd_dirs: &mut std::collections::HashMap<i32, String>,
) {
    for p in [path.to_path_buf(), path.parent().map(|d| d.to_path_buf()).unwrap_or_default()] {
        if p.as_os_str().is_empty() {
            continue;
        }
        let dir = p.to_string_lossy().into_owned();
        if !watches.insert(dir.clone()) {
            continue;
        }
        let Some(c) = std::ffi::CString::new(dir.clone()).ok() else { continue };
        let wd = unsafe { libc::inotify_add_watch(fd, c.as_ptr(), WATCH_MASK) };
        if wd >= 0 {
            wd_dirs.insert(wd, dir);
        }
    }
}

/// Read one batch of inotify events (the fd is non-blocking) and return the
/// watched file paths the events refer to.
fn inotify_changed_files(
    fd: i32,
    files: &std::collections::HashSet<String>,
    wd_dirs: &std::collections::HashMap<i32, String>,
) -> Vec<String> {
    let mut buf = [0u8; 8192];
    let n = unsafe {
        libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
    };
    if n <= 0 {
        return vec![];
    }
    let n = n as usize;
    let mut changed = std::collections::BTreeSet::new();
    let mut off = 0usize;
    while off + 16 <= n {
        let wd = i32::from_ne_bytes(buf[off..off + 4].try_into().unwrap());
        let mask = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap());
        let _cookie = u32::from_ne_bytes(buf[off + 8..off + 12].try_into().unwrap());
        let len = u32::from_ne_bytes(buf[off + 12..off + 16].try_into().unwrap()) as usize;
        off += 16;
        let name_end = (off + len).min(n);
        let raw = &buf[off..name_end];
        off += len;
        let name = raw.split(|b| *b == 0).next().unwrap_or(&[]);
        if name.is_empty() && mask & libc::IN_ISDIR != 0 {
            continue; // directory event without a name: not a test file
        }
        let Some(dir) = wd_dirs.get(&wd) else { continue };
        let candidate = if name.is_empty() {
            dir.clone()
        } else {
            format!(
                "{}/{}",
                dir.trim_end_matches('/'),
                String::from_utf8_lossy(name)
            )
        };
        if files.contains(&candidate) {
            changed.insert(candidate);
        }
    }
    changed.into_iter().collect()
}

/// Long-lived test watch session for `honey test --watch`: run the batch
/// (one worker subprocess per file, up to num_cpus concurrent), stream one
/// response frame per file plus a batch-summary marker frame, then watch the
/// test files with inotify; an edit re-runs only the affected files. Events
/// that land mid-run are consumed by the post-run drain, so one editor save
/// yields exactly one re-run. Ends when the client disconnects.
fn watch_test_session(stream: &mut UnixStream, request: &Request) {
    use std::os::unix::io::AsRawFd;
    let ifd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK) };
    if ifd < 0 {
        return;
    }
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

        let mut watches = std::collections::HashSet::new();
        let mut wd_dirs = std::collections::HashMap::new();
        for (file, _) in &jobs {
            inotify_watch(ifd, std::path::Path::new(file), &mut watches, &mut wd_dirs);
        }
        // Events from before this point describe the run we just did.
        drain_inotify(ifd);

        // Wait for the next relevant event or the client hanging up.
        loop {
            let mut pfds = [
                libc::pollfd { fd: ifd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: cfd, events: libc::POLLIN | libc::POLLRDHUP, revents: 0 },
            ];
            let r = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
            if r < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break 'sessions;
            }
            if pfds[1].revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0 {
                break 'sessions; // client went away (Ctrl+C)
            }
            if pfds[1].revents & libc::POLLIN != 0 {
                break 'sessions; // clients don't send data mid-watch
            }
            if pfds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                // The post-run drain already consumed this event batch; read
                // whatever event payload this single read yields.
                let changed = inotify_changed_files(ifd, &file_set, &wd_dirs);
                drain_inotify(ifd);
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
    unsafe { libc::close(ifd); }
}

/// Wrap a closure with fd 1/2 redirected into pipes drained by a single
/// poll() thread, returning what was written. Everything the runtime prints
/// (including v8 callbacks and eprintln) is captured.
pub fn capture_stdio<T>(f: impl FnOnce() -> T) -> (String, String, T) {
    use std::os::unix::io::{FromRawFd, RawFd};

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

    let (out_tx, out_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (err_tx, err_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    // One thread drains both pipes; half the per-request thread-spawn cost.
    let out_fd: RawFd = out_pipe[0];
    let err_fd: RawFd = err_pipe[0];
    let out_tx2 = out_tx.clone();
    let err_tx2 = err_tx.clone();
    let drainer = std::thread::spawn(move || {
        let mut pfds = [
            libc::pollfd { fd: out_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: err_fd, events: libc::POLLIN, revents: 0 },
        ];
        let mut out_file = unsafe { std::fs::File::from_raw_fd(out_fd) };
        let mut err_file = unsafe { std::fs::File::from_raw_fd(err_fd) };
        let mut buf = [0u8; 16384];
        loop {
            // Block until either pipe has data.
            let r = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
            if r < 0 {
                break;
            }
            let mut open = 0;
            for (i, pfd) in pfds.iter_mut().enumerate() {
                if pfd.revents & libc::POLLIN != 0 {
                    let (file, tx) = if i == 0 { (&mut out_file, &out_tx2) } else { (&mut err_file, &err_tx2) };
                    match file.read(&mut buf) {
                        Ok(0) => pfd.events = 0, // EOF; stop polling this fd
                        Ok(n) => { let _ = tx.send(buf[..n].to_vec()); open += 1; }
                        Err(_) => { pfd.events = 0; }
                    }
                }
                if pfd.events != 0 && pfd.revents & (libc::POLLHUP | libc::POLLERR) == 0 {
                    open += 1;
                }
            }
            if open == 0 {
                break; // both pipes closed
            }
        }
    });

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

    // Close our copies of the write ends; the drain thread sees EOF on both
    // pipes and exits, so data isn't lost to a late join.
    drainer.join().ok();

    let mut out_bytes = Vec::new();
    while let Ok(chunk) = out_rx.try_recv() {
        out_bytes.extend_from_slice(&chunk);
    }
    let mut err_bytes = Vec::new();
    while let Ok(chunk) = err_rx.try_recv() {
        err_bytes.extend_from_slice(&chunk);
    }

    (
        String::from_utf8_lossy(&out_bytes).into_owned(),
        String::from_utf8_lossy(&err_bytes).into_owned(),
        result,
    )
}

// ---------- watch mode ----------

/// What we subscribe to: content edits, attribute/mtime bumps, creates,
/// deletes and renames. Directory watches catch atomic saves (delete+rename)
/// and new files; per-file watches catch plain in-place writes.
const WATCH_MASK: u32 = libc::IN_MODIFY | libc::IN_ATTRIB | libc::IN_CREATE
    | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE
    | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;

fn inotify_add_dir(fd: i32, dir: &str, watches: &mut std::collections::HashSet<String>) {
    if !watches.insert(dir.to_string()) {
        return;
    }
    let c = std::ffi::CString::new(dir).ok();
    if let Some(c) = c {
        unsafe { libc::inotify_add_watch(fd, c.as_ptr(), WATCH_MASK) };
    }
}

fn drain_inotify(fd: i32) {
    let mut buf = [0u8; 8192];
    loop {
        let n = unsafe {
            libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        if n <= 0 {
            break; // fully drained (EAGAIN or error; fd is non-blocking)
        }
        if (n as usize) < buf.len() {
            break;
        }
    }
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
/// events via inotify (no polling), re-run on the warm engine, repeat. Any
/// events that arrive mid-run are consumed by the post-run drain, so one
/// editor save yields exactly one re-run. Ends when the client disconnects.
fn watch_session(stream: &mut UnixStream, request: &Request, runtime: &crate::runtime::Runtime) {
    use std::os::unix::io::AsRawFd;
    // Non-blocking: the post-run drain must return immediately when there is
    // nothing pending; a blocking fd would wait for the next real event and
    // swallow it.
    let ifd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK) };
    if ifd < 0 {
        return;
    }
    let mut watches = std::collections::HashSet::new();
    let cfd = stream.as_raw_fd();
    'sessions: loop {
        let (out, err, mut response) = capture_stdio(|| handle_run(request, runtime));
        response.stdout = out;
        response.stderr = err;
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
            inotify_add_dir(ifd, path, &mut watches);
            if let Some(dir) = std::path::Path::new(path).parent() {
                let dir = dir.to_string_lossy().into_owned();
                inotify_add_dir(ifd, &dir, &mut watches);
            }
        }
        // Events from before this point describe the run we just did.
        drain_inotify(ifd);
        trace_log("watch: armed, awaiting events");

        // Wait for the next relevant event or the client hanging up.
        loop {
            let mut pfds = [
                libc::pollfd { fd: ifd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: cfd, events: libc::POLLIN | libc::POLLRDHUP, revents: 0 },
            ];
            let r = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
            if r < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break 'sessions;
            }
            if pfds[1].revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0 {
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
    unsafe { libc::close(ifd); }
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
    if connect().is_some() {
        return Ok(());
    }
    let exe = std::env::current_exe().context("failed to locate honey executable")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--internal-daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
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
    read_msg(&mut stream)
}

fn request(req: &Request, stream: UnixStream) -> Option<Response> {
    serde_json::from_slice(&request_raw(req, stream)?).ok()
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
        if !spawned {
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
        ..Default::default()
    };
    let stream = connect().or_else(|| wait_for_socket(Duration::from_millis(500)))?;
    let body = request_raw(&req, stream)?;
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
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
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
