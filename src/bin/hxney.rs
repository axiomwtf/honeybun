//! `hxney` — one-off package executor (npx/bunx equivalent), thin launcher.
//!
//! Package resolution is plain filesystem work (scan .cinm, read
//! package.json, pick bin/main); executing is daemon work. The launcher
//! does the fs part itself (~0.2ms), warm-runs the resolved entry through
//! the daemon like `honey run`, and execv's into `hxney-full` for
//! anything unusual: packages that need installing from npm, scoped
//! names (`@scope/pkg`), subpath entries (`pkg/bin`), trace/dev modes —
//! those keep the full-fat path with identical behaviour.

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

/// execv into the full-fat CLI/engine binary. Never returns on success.
fn exec_full() -> ! {
    let exe = std::env::current_exe().unwrap_or_default();
    let full = exe.with_file_name("hxney-full");
    use std::os::unix::process::CommandExt as _;
    let err = std::process::Command::new(&full)
        .args(std::env::args_os().skip(1))
        .exec();
    eprintln!("error: hxney launcher failed to start its engine ({err}); reinstall hxney");
    std::process::exit(127);
}

/// Scan `.cinm` for the newest `name@*` dir and resolve its bin/main entry.
/// Returns None whenever anything is off the fast path.
fn resolve_entry(package: &str) -> Option<std::path::PathBuf> {
    // Only plain, unscoped package names take the fast path.
    if package.is_empty() || package.contains('@') || package.contains('/') {
        return None;
    }
    let root = std::path::Path::new(".cinm");
    let mut found: Option<std::path::PathBuf> = None;
    let prefix = format!("{package}@");
    for entry in fs_read_dir(root)? {
        let name = entry.file_name()?.to_string_lossy().into_owned();
        if name.starts_with(&prefix) && found.is_none() {
            found = Some(entry.as_path().to_owned());
        }
    }
    let pkg_dir = found?;
    let pkg_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(pkg_dir.join("package.json")).ok()?,
    )
    .ok()?;

    let bin_path = match pkg_json.get("bin") {
        Some(serde_json::Value::Object(m)) => m.values().next().and_then(|v| v.as_str()),
        Some(serde_json::Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    let entry = match bin_path {
        Some(rel) => pkg_dir.join(rel),
        None => match pkg_json.get("main").and_then(|v| v.as_str()) {
            Some(rel) => pkg_dir.join(rel),
            None => pkg_dir.join("index.js"),
        },
    };
    let entry = entry.canonicalize().unwrap_or(entry);
    if entry.is_file() {
        Some(entry)
    } else {
        None
    }
}

/// Minimal read_dir iterator that yields entries (or None on any error).
fn fs_read_dir(
    path: &std::path::Path,
) -> Option<impl Iterator<Item = std::path::PathBuf>> {
    let rd = std::fs::read_dir(path).ok()?;
    Some(rd.filter_map(|e| e.ok()).map(|e| e.path()))
}

fn main() {
    let mut argv: Vec<String> = std::env::args().collect();

    if argv.len() == 2 && (argv[1] == "--version" || argv[1] == "-V") {
        println!("hxney {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // `hxney npm` always takes the full path (its npm bootstrap lives in the
    // full binary); never let a stray .cinm/npm@ entry shadow it.
    if argv.len() >= 2 && argv[1] == "npm" {
        exec_full();
    }

    // Package-management and daemon subcommands are `honey` commands, not
    // npx-style package executions: `hxney add ms` must install ms, not exec
    // a package called "add". Forward the whole argv to the honey binary.
    {
        const MGMT: &[&str] =
            &["add", "remove", "install", "migrate", "start", "stop", "service"];
        if argv.len() >= 2 && MGMT.contains(&argv[1].as_str()) {
            let exe = std::env::current_exe().unwrap_or_default();
            let honey = exe.with_file_name("honey");
            use std::os::unix::process::CommandExt as _;
            let err = std::process::Command::new(&honey)
                .args(std::env::args_os().skip(1))
                .exec();
            eprintln!(
                "error: hxney could not exec {} ({err}); is honey installed next to it?",
                honey.display()
            );
            std::process::exit(127);
        }
    }

    // Dev/trace modes and odd invocations keep the full-fat semantics.
    if std::env::var_os("HONEY_TRACE").is_some() || std::env::var_os("HONEY_NO_DAEMON").is_some()
        || argv.len() < 2
    {
        exec_full();
    }
    let package = argv[1].clone();
    let args: Vec<String> = argv[2..].to_vec();

    // Node semantics: argv[0] executable, argv[1] entry path, argv[2..] args.
    // The daemon's run path builds exactly this shape (honey, file, args).
    let Some(entry) = resolve_entry(&package) else {
        exec_full(); // needs install, is scoped, or has no resolvable entry
    };
    let entry_str = entry.to_string_lossy().into_owned();
    let pkg_cwd = entry.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();

    let stream = UnixStream::connect(socket_path());
    if stream.is_err() {
        exec_full(); // daemon down: full binary auto-starts it
    }
    let mut stream = stream.unwrap();

    let req = RunRequest { cmd: "run", file: &entry_str, args: &args, cwd: pkg_cwd };
    let mut body = serde_json::to_vec(&req).unwrap_or_default();
    let mut msg = Vec::with_capacity(4 + body.len());
    msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
    msg.append(&mut body);

    use std::io::{Read, Write};
    if stream.write_all(&msg).is_err() {
        exec_full();
    }

    // Spin briefly before blocking; warm requests answer sub-millisecond.
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
        None => exec_full(),
    }
}
