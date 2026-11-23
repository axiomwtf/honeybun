//! `honeyd` — the honey engine and daemon binary.
//!
//! Its command surface is deliberately about the daemon, not about running
//! projects (that is `honey`'s job):
//!
//! ```text
//! honeyd [serve]            run the resident daemon (default)
//! honeyd start              install/enable the service and start the daemon
//! honeyd stop               ask the running daemon to exit
//! honeyd status             show daemon state
//! honeyd worker             internal test-worker protocol on stdin/stdout
//! honeyd mksnapshot [path]  build the V8 startup snapshot
//! honeyd engine-timing [n]  engine-init timing benchmark
//! honeyd transform-js <f>   print the ESM->CJS transform of a file
//! honeyd service [--uninstall|--status]
//! ```
//!
//! The hidden `__cli` subcommand is the inline fallback `honey` uses for
//! `HONEY_NO_DAEMON` / `HONEY_TRACE` and when the daemon cannot be reached.
//! It parses the user CLI exactly like `honey` but executes locally.

#[path = "../cli_shared.rs"]
mod shared;

use clap::Parser as _;
use honey::daemon;
use honey::{PackageManager, Runtime};

/// No-daemon / trace-mode watcher: run on the current thread, stat-poll the
/// dep set the runtime recorded, and re-run on change.
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

/// Internal: create the V8 context startup snapshot consumed by build.rs.
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

fn usage() -> ! {
    eprintln!(
        "honeyd — honey engine/daemon\n\
         \n\
         usage: honeyd [serve]\n\
         \x20      honeyd start\n\
         \x20      honeyd stop\n\
         \x20      honeyd status\n\
         \x20      honeyd worker\n\
         \x20      honeyd mksnapshot [path]\n\
         \x20      honeyd engine-timing [n]\n\
         \x20      honeyd transform-js <file>\n\
         \x20      honeyd service [--uninstall|--status]"
    );
    std::process::exit(2);
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    match argv.get(1).map(String::as_str) {
        None | Some("serve") => daemon::serve(),
        Some("start") => match daemon::start_daemon() {
            Ok((0, true)) => println!("honey daemon already running"),
            Ok((pid, _)) => println!("honey daemon started (pid {pid})"),
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        },
        Some("stop") => match daemon::stop_daemon() {
            Ok(()) => println!("honey daemon stopped"),
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        },
        Some("status") => {
            println!("honeyd status");
            println!("  socket: {}", daemon::socket_path().display());
            match daemon::running_pid() {
                Some(pid) => println!("  daemon: running (pid {pid})"),
                None => println!("  daemon: not running"),
            }
        }
        Some("worker") => daemon::serve_test_worker(),
        Some("mksnapshot") => {
            let out = argv
                .get(2)
                .cloned()
                .unwrap_or_else(|| "target/honey_snapshot.bin".to_string());
            mksnapshot(&out);
        }
        Some("engine-timing") => {
            let n: usize = argv.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
            honey::runtime::engine_init_timing(n);
        }
        Some("transform-js") => {
            let Some(f) = argv.get(2) else {
                eprintln!("usage: honeyd transform-js <file>");
                std::process::exit(2);
            };
            match std::fs::read_to_string(f) {
                Ok(src) => print!("{}", honey::runtime::transform_esm_to_cjs_debug(&src)),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("service") => {
            let uninstall = argv.iter().any(|a| a == "--uninstall");
            let status = argv.iter().any(|a| a == "--status");
            std::process::exit(honey::service::cli(uninstall, status));
        }
        // Hidden inline fallback for `honey` (HONEY_NO_DAEMON / HONEY_TRACE,
        // or a daemon that could not be reached). Not part of the daemon CLI.
        Some("__cli") => run_user_cli(&argv[2..]),
        Some(_) => usage(),
    }
}

/// Inline execution of the user-facing CLI (hidden `__cli` path).
fn run_user_cli(args: &[String]) {
    use shared::Commands;
    let cli = shared::Cli::parse_from(
        std::iter::once("honey".to_string()).chain(args.iter().cloned()),
    );

    match cli.command {
        Commands::Install {
            production,
            global,
            packages,
        } => {
            if global || !packages.is_empty() {
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
            honey::config::fire(
                "onInstall",
                &format!(r#"{{"cmd":"install","production":{production}}}"#),
            );
            let pm = PackageManager::new(std::path::Path::new("."));
            if let Err(e) = pm.install(production) {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        }
        Commands::Add { package, dev } => {
            for pkg in &package {
                honey::config::fire(
                    "onInstall",
                    &format!(r#"{{"cmd":"add","package":"{pkg}","dev":{dev}}}"#),
                );
            }
            let pm = PackageManager::new(std::path::Path::new("."));
            for pkg in &package {
                if let Err(e) = pm.add(pkg, dev) {
                    eprintln!("error: {e:#}");
                    std::process::exit(1);
                }
            }
        }
        Commands::Remove { package } => {
            honey::config::fire(
                "onInstall",
                &format!(r#"{{"cmd":"remove","package":"{package}"}}"#),
            );
            let pm = PackageManager::new(std::path::Path::new("."));
            if let Err(e) = pm.remove(&package) {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        }
        Commands::Run { file, watch, args } => {
            // Script tasks from honey.config.ts: `honey run dev` with no ./dev
            // file but a `scripts.dev` entry runs that command through the
            // shell. The file wins when both exist.
            if !std::path::Path::new(&file).exists() {
                if let Some(command) = honey::config::resolve_script(&file, &args) {
                    shared::run_shell_script(&file, &command, &args, watch);
                }
            }
            honey::config::fire("onRun", &format!(r#"{{"file":"{file}"}}"#));
            if watch {
                if daemon::watch_via_daemon(&file, &args).is_some() {
                    std::process::exit(0);
                }
                inline_watch(&file, &args);
            }
            if let Some(resp) = daemon::run_via_daemon(&file, &args) {
                print!("{}", resp.stdout);
                eprint!("{}", resp.stderr);
                if let Some(err) = resp.error {
                    eprintln!("error: {err}");
                }
                std::process::exit(resp.exit_code);
            }
            let runtime = Runtime::new();
            let code = match runtime.run_file(&file, &args) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("error: {e:#}");
                    1
                }
            };
            std::process::exit(code);
        }
        Commands::Build {
            entry,
            output,
            production,
            minify,
            sourcemap,
            target,
            define,
            watch,
        } => {
            honey::config::fire("onBuild", r#"{"cmd":"build"}"#);
            let sourcemap = match sourcemap.as_deref() {
                None => None,
                Some(s) => match s {
                    "linked" => Some(honey::build::SourceMapMode::Linked),
                    "external" => Some(honey::build::SourceMapMode::External),
                    "inline" => Some(honey::build::SourceMapMode::Inline),
                    "none" => Some(honey::build::SourceMapMode::None),
                    other => {
                        eprintln!(
                            "error: invalid --sourcemap value '{other}' (linked|external|inline|none)"
                        );
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
            if let Some(resp) = daemon::build_via_daemon(entry.as_deref(), output.as_deref(), &opts)
            {
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
                    eprintln!("error: {e:#}");
                    std::process::exit(1);
                }
            }
        }
        Commands::Test {
            filter,
            slow,
            watch,
        } => {
            honey::config::fire(
                "onTest",
                &format!(
                    r#"{{"filter":{}}}"#,
                    filter
                        .as_deref()
                        .map(|f| format!(r#""{f}""#))
                        .unwrap_or_else(|| "null".into())
                ),
            );
            std::process::exit(shared::run_test_suite(filter.as_deref(), slow, watch));
        }
        Commands::Migrate { file } => match honey::migrate::migrate(file.as_deref()) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        },
        Commands::Start => match daemon::start_daemon() {
            Ok((0, true)) => println!("honey daemon already running"),
            Ok((pid, _)) => println!("honey daemon started (pid {pid})"),
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        },
        Commands::Stop => match daemon::stop_daemon() {
            Ok(()) => println!("honey daemon stopped"),
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        },
        Commands::Service { uninstall, status } => {
            std::process::exit(honey::service::cli(uninstall, status));
        }
    }
}
