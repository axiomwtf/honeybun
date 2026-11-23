//! `honey` — the shipped thin launcher and user CLI.
//!
//! Every command is a request to the resident `honeyd`: the launcher parses
//! the CLI, talks to the daemon over the unix socket, prints the reply and
//! exits. It links no V8 and no engine, so even a cold client costs ~1MB of
//! mappings instead of the 54MB engine binary.
//!
//! `honeyd start` is asked to bring the daemon up on first use. When the
//! daemon is explicitly disabled (`HONEY_NO_DAEMON`) or trace mode is on
//! (`HONEY_TRACE`), the launcher hands the invocation to `honeyd __cli`,
//! which runs the same commands inline for debugging.

#[path = "../cli_shared.rs"]
mod shared;

use clap::Parser as _;

fn env_inline() -> bool {
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

/// Run this invocation in `honeyd`'s inline engine path. Used for trace /
/// no-daemon modes and as a last resort when the daemon cannot be reached.
fn exec_inline() -> ! {
    let exe = shared::daemon_exe();
    use std::os::unix::process::CommandExt as _;
    let err = std::process::Command::new(&exe)
        .arg("__cli")
        .args(std::env::args_os().skip(1))
        .exec();
    eprintln!(
        "error: honey engine unavailable ({}): {err}",
        exe.display()
    );
    std::process::exit(127);
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();

    // Cheap version answer without touching the daemon.
    if argv.len() == 2 && (argv[1] == "--version" || argv[1] == "-V") {
        println!("honey {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if env_inline() {
        exec_inline();
    }

    use shared::Commands;
    let cli = shared::Cli::parse();

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
            shared::fire_hook(
                "onInstall",
                &format!(r#"{{"cmd":"install","production":{production}}}"#),
            );
            match shared::install(production) {
                Some(resp) => shared::exit_with(resp),
                None => exec_inline(),
            }
        }
        Commands::Add { package, dev } => {
            for pkg in &package {
                shared::fire_hook(
                    "onInstall",
                    &format!(r#"{{"cmd":"add","package":"{pkg}","dev":{dev}}}"#),
                );
            }
            match shared::add(&package, dev) {
                Some(resp) => shared::exit_with(resp),
                None => exec_inline(),
            }
        }
        Commands::Remove { package } => {
            shared::fire_hook(
                "onInstall",
                &format!(r#"{{"cmd":"remove","package":"{package}"}}"#),
            );
            match shared::remove(&package) {
                Some(resp) => shared::exit_with(resp),
                None => exec_inline(),
            }
        }
        Commands::Run { file, watch, args } => {
            // Script tasks from honey.config.ts: `honey run dev` with no ./dev
            // file but a `scripts.dev` entry runs that command through the
            // shell. The file wins when both exist. Resolution runs in the
            // daemon (the config is TypeScript).
            if !std::path::Path::new(&file).exists() {
                if let Some(command) = shared::resolve_script(&file, &args) {
                    shared::run_shell_script(&file, &command, &args, watch);
                }
            }
            shared::fire_hook("onRun", &format!(r#"{{"file":"{file}"}}"#));
            if watch {
                if shared::watch_run(&file, &args).is_none() {
                    exec_inline();
                }
                std::process::exit(0);
            }
            match shared::run_file(&file, &args) {
                Some(resp) => shared::exit_with(resp),
                None => exec_inline(),
            }
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
            if let Some(s) = sourcemap.as_deref() {
                if !matches!(s, "linked" | "external" | "inline" | "none") {
                    eprintln!(
                        "error: invalid --sourcemap value '{s}' (linked|external|inline|none)"
                    );
                    std::process::exit(2);
                }
            }
            if let Some(t) = target.as_deref() {
                if !matches!(t, "browser" | "node") {
                    eprintln!("error: invalid --target value '{t}' (browser|node)");
                    std::process::exit(2);
                }
            }
            for d in &define {
                match d.split_once('=') {
                    Some((k, _)) if !k.trim().is_empty() => {}
                    _ => {
                        eprintln!("error: invalid --define value '{d}' (expected key=value)");
                        std::process::exit(2);
                    }
                }
            }

            shared::fire_hook("onBuild", r#"{"cmd":"build"}"#);
            let req = shared::BuildRequest {
                entry: entry.as_deref(),
                output: output.as_deref(),
                production,
                minify,
                sourcemap: sourcemap.as_deref(),
                target: target.as_deref(),
                define: &define,
            };
            if watch {
                if !shared::build_watch(&req) {
                    exec_inline();
                }
                std::process::exit(0);
            }
            match shared::build(&req) {
                Some(resp) => shared::exit_with(resp),
                None => exec_inline(),
            }
        }
        Commands::Test {
            filter,
            slow,
            watch,
        } => {
            shared::fire_hook(
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
        Commands::Migrate { file } => match shared::migrate(file.as_deref()) {
            Some(resp) => shared::exit_with(resp),
            None => exec_inline(),
        },
        Commands::Start => {
            std::process::exit(shared::start_daemon_cli());
        }
        Commands::Stop => {
            std::process::exit(shared::stop_daemon_cli());
        }
        Commands::Service { uninstall, status } => {
            std::process::exit(shared::service_cli(uninstall, status));
        }
    }
}
