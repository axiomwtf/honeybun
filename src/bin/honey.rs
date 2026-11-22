//! `honey` — the thin launcher.
//!
//! Warm runs (`honey run <file>`) pay a fixed cost for every exec: mapping
//! the 54MB engine binary costs ~150-250µs of pure startup before `main`
//! even runs. The launcher binary ships V8-free (~1MB), so the hot path
//! pays almost nothing: it talks to the daemon over the unix socket,
//! prints the response and exits. Everything else (build, test, install,
//! --version, daemon lifecycle, trace/dev internals, config-hook runs)
//! `execv`s straight into `honey-full`, the full CLI + engine, with the
//! same argv — indistinguishable from a single-fat-binary CLI, except
//! those commands gain one short exec.

use serde::{Deserialize, Serialize};
use std::os::unix::net::UnixStream;

#[derive(Serialize)]
struct RunRequest<'a> {
    cmd: &'a str,
    file: &'a str,
    args: &'a [String],
    cwd: String,
}

#[derive(Deserialize)]
struct RunResponse {
    exit_code: i32,
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    #[serde(default)]
    error: Option<String>,
}

fn socket_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("HONEY_SOCKET") {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return std::path::PathBuf::from(d).join("honey.sock");
        }
    }
    let uid = unsafe { libc::getuid() };
    std::path::PathBuf::from(format!("/tmp/honey-{uid}.sock"))
}

/// execv into the full CLI/engine binary. Never returns on success.
fn exec_full(argv0: &str) -> ! {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from(argv0));
    let full = exe.with_file_name("honey-full");
    use std::os::unix::process::CommandExt as _;
    let err = std::process::Command::new(&full)
        .args(std::env::args_os().skip(1))
        .exec();
    eprintln!("error: honey launcher failed to start its engine ({err}); reinstall honey");
    std::process::exit(127);
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();

    // Cheap version answer without exec'ing the 54MB engine binary.
    if argv.len() == 2 && (argv[1] == "--version" || argv[1] == "-V") {
        println!("honey {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // Anything that is not a plain daemon-backed run goes to the full binary.
    let plain_run = argv.len() >= 3
        && argv[1] == "run"
        && argv[2] != "--watch"
        && argv[2] != "-w";
    if !plain_run {
        exec_full(argv.first().map(String::as_str).unwrap_or("honey"));
    }

    // Trace/dev modes must run inline in the full binary so phase output and
    // fallbacks behave exactly as before (run_via_daemon also refuses them).
    if std::env::var_os("HONEY_TRACE").is_some() || std::env::var_os("HONEY_NO_DAEMON").is_some() {
        exec_full(argv.first().map(String::as_str).unwrap_or("honey"));
    }

    let file = argv[2].clone();
    let args: Vec<String> = argv[3..].to_vec();

    // Config hooks need the full CLI; without a config file the fast path is
    // pure client<->daemon.
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let has_config = ["honey.config.ts", "honey.config.js"]
        .iter()
        .any(|n| std::path::Path::new(&cwd).join(n).is_file());
    if has_config {
        exec_full(argv.first().map(String::as_str).unwrap_or("honey"));
    }

    let stream = UnixStream::connect(socket_path());
    if stream.is_err() {
        // Daemon not up: the full binary auto-starts it and re-enters this
        // path warm; first runs don't care about the extra exec.
        exec_full(argv.first().map(String::as_str).unwrap_or("honey"));
    }
    let mut stream = stream.unwrap();

    let req = RunRequest {
        cmd: "run",
        file: &file,
        args: &args,
        cwd,
    };
    let mut body = serde_json::to_vec(&req).unwrap_or_default();
    let mut msg = Vec::with_capacity(4 + body.len());
    msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
    msg.append(&mut body);

    use std::io::{Read, Write};
    if stream.write_all(&msg).is_err() {
        exec_full(argv.first().map(String::as_str).unwrap_or("honey"));
    }

    // Spin briefly before blocking: a warm request answers in well under
    // 300µs and busy-polling avoids two scheduler round-trips.
    {
        use std::os::unix::io::AsRawFd;
        let fd = stream.as_raw_fd();
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let deadline = std::time::Instant::now() + std::time::Duration::from_micros(500);
        loop {
            let r = unsafe { libc::poll(&mut pfd, 1, 0) };
            if r != 0 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::hint::spin_loop();
        }
    }

    // Length-prefixed frame read.
    fn read_frame(stream: &mut UnixStream) -> Option<Vec<u8>> {
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

    match read_frame(&mut stream).and_then(|b| serde_json::from_slice::<RunResponse>(&b).ok()) {
        Some(resp) => {
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
            std::process::exit(resp.exit_code);
        }
        None => {
            // Malformed/absent response: bail into the full CLI for the
            // engine's inline answer (better a slow right answer than a
            // wrong fast one).
            exec_full(argv.first().map(String::as_str).unwrap_or("honey"));
        }
    }
}
