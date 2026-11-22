use anyhow::{Context, Result};
use clap::Parser;
use honey::runtime::Runtime;
use honey::PackageManager;
use serde_json;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "hxney")]
#[command(bin_name = "hxney")]
#[command(author = "AXIOM")]
#[command(version = "0.1.0")]
#[command(about = "One-off package executor (npx/bunx equivalent)", long_about = None)]
struct Cli {
    /// Package to execute
    package: String,
    /// Arguments to pass to the package
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();

    // Bare `hxney`: drop into the interactive JS shell until `exit`.
    if argv.len() == 1 {
        if let Err(e) = run_shell() {
            eprintln!("error: {:#}", e);
            std::process::exit(1);
        }
        return;
    }

    // `hxney npm <args>`: npm-style package management, backed by a bundled
    // npm living in the shared package store. Handled before Cli::parse so
    // npm's own flags (-g etc.) pass through untouched.
    if argv.len() >= 2 && argv[1] == "npm" {
        let args: Vec<String> = argv[2..].to_vec();
        if let Err(e) = run_npm(&args) {
            eprintln!("error: {:#}", e);
            std::process::exit(1);
        }
        return;
    }

    let cli = Cli::parse();

    if let Err(e) = execute_package(&cli.package, &cli.args) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

/// Bare `hxney`: an interactive JS shell on the warm engine. Completion
/// values are echoed, session state (vars, functions, requires) persists
/// across lines, unbalanced brackets continue on the next line. `exit`,
/// `.exit`, `quit` or Ctrl-D leaves.
fn run_shell() -> Result<()> {
    use std::io::{BufRead as _, Write as _};
    println!("hxney {} \u{1F40D} type JavaScript, `exit` to leave", env!("CARGO_PKG_VERSION"));
    let runtime = honey::runtime::Runtime::new_warm();
    let stdin = std::io::stdin();
    let mut buffer = String::new();
    loop {
        {
            let mut out = std::io::stdout();
            if buffer.is_empty() {
                write!(out, "hxney> ")?;
            } else {
                write!(out, "....> ")?;
            }
            out.flush()?;
        }
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            println!();
            break; // EOF (Ctrl-D)
        }
        let trimmed = line.trim();
        if buffer.is_empty() && matches!(trimmed, "exit" | ".exit" | "quit" | ":q") {
            break;
        }
        buffer.push_str(&line);
        if !brackets_balanced(&buffer) {
            continue;
        }
        let input = std::mem::take(&mut buffer);
        let t = input.trim();
        if t.is_empty() {
            continue;
        }
        // Direct eval keeps const/let/class scoped to the eval itself —
        // invisible to the next line. A leading declaration is rewritten to
        // `var` (function-scoped, lands on the engine global) so REPL state
        // persists; declarations further in are left untouched.
        let runnable = if let Some(rest) = t.strip_prefix("const ") {
            format!("var {rest}")
        } else if let Some(rest) = t.strip_prefix("let ") {
            format!("var {rest}")
        } else if let Some(rest) = t.strip_prefix("class ") {
            let name = rest.split(['{', ' ', '\t', '\n']).next().unwrap_or("").trim().to_string();
            if name.is_empty() {
                t.to_string()
            } else {
                format!("var {name} = {t}")
            }
        } else {
            t.to_string()
        };
        // Direct eval: completion value of the input comes back (last
        // expression), and top-level vars/functions attach to the enclosing
        // script scope — which is the engine global, so they persist.
        match runtime.eval_code(&format!("eval({})", serde_json::to_string(&runnable)?)) {
            Ok(Some(v)) => println!("{v}"),
            Ok(None) => {}
            Err(e) => eprintln!("error: {e}"),
        }
    }
    Ok(())
}

/// Bracket balance for REPL continuation: ignores strings, template
/// literals, escapes and comments. A heuristic, not a parser.
fn brackets_balanced(s: &str) -> bool {
    let mut depth: i64 = 0;
    let mut in_str: Option<char> = None;
    let mut escaped = false;
    let mut in_block = false;
    let mut in_line = false;
    let mut prev = '\0';
    for c in s.chars() {
        if in_line {
            if c == '\n' {
                in_line = false;
            }
            prev = c;
            continue;
        }
        if in_block {
            if prev == '*' && c == '/' {
                in_block = false;
                prev = '\0';
                continue;
            }
            prev = c;
            continue;
        }
        if let Some(q) = in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                in_str = None;
            }
            prev = c;
            continue;
        }
        match c {
            '/' if prev == '/' => in_line = true,
            '*' if prev == '/' => in_block = true,
            '"' | '\'' | '`' => in_str = Some(c),
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth -= 1,
            _ => {}
        }
        prev = c;
    }
    depth <= 0 && in_str.is_none() && !in_block
}

/// npm bootstrap: the npm package itself is installed once into the shared
/// package store (`~/.cache/honey/store/npm@<version>`), same place every
/// other hxney package lives, then executed with the Node-compatible runner
/// on PATH (node, falling back to bun). `hxney npm install -g <pkg>` keeps
/// its global tree under `~/.cache/honey/npm-global` unless the user has
/// already set a prefix, so no sudo and no system writes.
fn run_npm(args: &[String]) -> Result<()> {
    let cli = ensure_npm_cli()?;
    let runner = find_node_runner().with_context(|| {
        "`hxney npm` runs npm on a Node-compatible runner, but neither node nor bun was found on PATH"
    })?;

    let mut cmd = std::process::Command::new(&runner);
    cmd.arg(&cli).args(args);
    let wants_global = args.iter().any(|a| a == "-g" || a == "--global");
    // Our npm-global prefix is hxney-owned but not on PATH, so global
    // commands must be linked into ~/.local/bin after npm finishes.
    let our_prefix: Option<PathBuf> = if wants_global
        && std::env::var_os("npm_config_prefix").is_none()
    {
        honey::cinm::shared_store_root()
            .parent()
            .map(|p| p.join("npm-global"))
    } else {
        None
    };
    if let Some(prefix) = &our_prefix {
        cmd.env("npm_config_prefix", prefix);
    }
    let status = cmd
        .status()
        .with_context(|| format!("failed to run npm with {}", runner.display()))?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    if let Some(prefix) = &our_prefix {
        if let Err(e) = sync_global_bins(prefix) {
            eprintln!(
                "honey: warning: could not link global commands into ~/.local/bin: {:#}",
                e
            );
        }
    }
    Ok(())
}

/// Mirror npm's `<prefix>/bin` shims into `~/.local/bin` so freshly
/// installed global commands are immediately runnable. Never clobbers
/// real files (only replaces our own previous symlinks); stale links for
/// since-removed commands are pruned.
fn sync_global_bins(prefix: &Path) -> Result<()> {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let src = prefix.join("bin");
    if !src.is_dir() {
        return Ok(());
    }
    let home = std::env::var("HOME").context("no $HOME; cannot link global commands")?;
    let dest_dir = PathBuf::from(home).join(".local/bin");
    fs::create_dir_all(&dest_dir).context("failed to create ~/.local/bin")?;

    let mut current: Vec<String> = Vec::new();
    for entry in fs::read_dir(&src)
        .with_context(|| format!("failed to read {}", src.display()))?
        .flatten()
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Only real executables: skip npm's .cmd/.ps1 Windows shims.
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".cmd") || name.ends_with(".ps1") {
            continue;
        }
        if fs::metadata(&path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
        {
            current.push(name.clone());
        }
    }

    // Prune links we own that no longer have a shim (uninstall/rename).
    if let Ok(rd) = fs::read_dir(&dest_dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(target) = fs::read_link(&path) else {
                continue; // not a symlink: never touched
            };
            if !target.starts_with(&src) {
                continue; // somebody else's link
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !current.contains(&name) {
                let _ = fs::remove_file(&path);
            }
        }
    }

    // Link every shim; refuse to clobber anything that isn't ours.
    for name in &current {
        let link = dest_dir.join(name);
        match fs::symlink_metadata(&link) {
            Ok(m) if m.file_type().is_symlink() => {
                if fs::read_link(&link).map(|t| t.starts_with(&src)).unwrap_or(false) {
                    continue; // already linked
                }
                let _ = fs::remove_file(&link);
                symlink(src.join(name), &link).with_context(|| {
                    format!("failed to link {name} into ~/.local/bin")
                })?;
            }
            Ok(_) => {
                eprintln!("honey: not linking {name}: a real file already exists in ~/.local/bin");
            }
            Err(_) => {
                symlink(src.join(name), &link).with_context(|| {
                    format!("failed to link {name} into ~/.local/bin")
                })?;
            }
        }
    }
    Ok(())
}

/// Latest npm, extracted into the shared store. Idempotent and race-safe:
/// extraction lands in a private temp dir that is renamed into place.
fn ensure_npm_cli() -> Result<PathBuf> {
    let store = honey::cinm::shared_store_root();

    // Already bootstrapped? Pick the highest version present.
    let mut found: Option<(String, PathBuf)> = None;
    if let Ok(rd) = fs::read_dir(&store) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("npm@") {
                match &found {
                    Some((v, _)) if *v >= name => {}
                    _ => found = Some((name, entry.path())),
                }
            }
        }
    }
    if let Some((_, dir)) = found {
        let cli = dir.join("bin/npm-cli.js");
        if cli.is_file() {
            return Ok(cli);
        }
    }

    // Fetch the newest npm from the registry and unpack it store-side.
    println!("hxney: installing npm into the package store...");
    let registry = honey::registry::Registry::new();
    let pkg = registry.resolve_version("npm", "latest")?;
    let resp = reqwest::blocking::get(&pkg.dist.tarball)
        .context("failed to download the npm tarball")?;
    if !resp.status().is_success() {
        anyhow::bail!("failed to download npm: status {}", resp.status());
    }
    let bytes = resp.bytes().context("failed to read the npm tarball")?.to_vec();

    let dest = store.join(format!("npm@{}", pkg.version));
    if !dest.exists() {
        fs::create_dir_all(&store).ok();
        let tmp = store.join(format!(".tmp-npm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).context("failed to create the npm staging dir")?;
        let tgz = tmp.join("npm.tgz");
        fs::write(&tgz, &bytes).context("failed to write the npm tarball")?;
        honey::cinm::extract_tarball(&tgz, &tmp.join("npm"))
            .context("failed to extract the npm tarball")?;
        match fs::rename(tmp.join("npm"), &dest) {
            Ok(()) => {}
            Err(_) if dest.exists() => {} // lost a race; the winner is fine
            Err(e) => return Err(e).context("failed to move npm into the package store"),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    let cli = dest.join("bin/npm-cli.js");
    if !cli.is_file() {
        anyhow::bail!(
            "npm bootstrap incomplete: bin/npm-cli.js missing from npm@{}",
            pkg.version
        );
    }
    Ok(cli)
}

/// node first, bun second (bun runs plain npm-cli.js CJS scripts fine).
fn find_node_runner() -> Option<PathBuf> {
    which("node").or_else(|| which("bun"))
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(&candidate)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
            {
                return Some(candidate);
            }
        }
    }
    None
}

fn execute_package(package: &str, args: &[String]) -> Result<()> {
    let pm = PackageManager::new(Path::new("."));

    // Parse package@version or package
    let (name, _version) = if package.contains('@') {
        let parts: Vec<&str> = package.splitn(2, '@').collect();
        (parts[0], Some(parts.get(1).cloned().unwrap_or("latest")))
    } else {
        (package, None)
    };

    // Check if package is already installed by scanning .cinm
    let entries = if pm.layout.root.exists() {
        fs::read_dir(&pm.layout.root)
            .with_context(|| format!("failed to read .cinm directory"))?
    } else {
        // .cinm doesn't exist yet
        fs::create_dir_all(&pm.layout.root).context("failed to create .cinm directory")?;
        fs::read_dir(&pm.layout.root)
            .with_context(|| format!("failed to read .cinm directory"))?
    };

    let mut found_dir: Option<std::path::PathBuf> = None;
    let cwd = std::env::current_dir()?;
    for entry in entries {
        let entry = entry?;
        let entry_name = entry.file_name().to_string_lossy().to_string();
        if entry_name.starts_with(&format!("{}@", name)) {
            // Convert to absolute path
            let abs_path = if entry.path().is_absolute() {
                entry.path()
            } else {
                cwd.join(entry.path())
            };
            found_dir = Some(abs_path);
            break;
        }
    }

    let pkg_dir = if let Some(dir) = found_dir {
        dir.canonicalize().unwrap_or(dir)
    } else {
        // Need to install from npm
        // For now, try to install the package
        match pm.add(package, false) {
            Ok(_) => {
                // Now find it again
                let cwd = std::env::current_dir()?;
                let entries = fs::read_dir(&pm.layout.root)
                    .with_context(|| format!("failed to read .cinm directory"))?;
                let mut new_found_dir: Option<std::path::PathBuf> = None;
                for entry in entries {
                    let entry = entry?;
                    let entry_name = entry.file_name().to_string_lossy().to_string();
                    if entry_name.starts_with(&format!("{}@", name)) {
                        let abs_path = if entry.path().is_absolute() {
                            entry.path()
                        } else {
                            cwd.join(entry.path())
                        };
                        let canonical_path = abs_path.canonicalize().unwrap_or(abs_path);
                        new_found_dir = Some(canonical_path);
                        break;
                    }
                }
                new_found_dir.ok_or_else(|| anyhow::anyhow!("package {} not found in .cinm after install", name))?
            }
            Err(_) => {
                // If install fails, try to use the package as a local path
                // This allows testing with locally set up .cinm
                anyhow::bail!("failed to install package {}. Please run 'honey add {}' first or ensure it's in .cinm", package, package);
            }
        }
    };

    // Read package.json to find bin entry
    let package_json_path = pkg_dir.join("package.json");
    let bin_name = if package.contains('/') {
        // Scoped package or subpath like "package/bin"
        let parts: Vec<&str> = package.split('/').collect();
        if parts.len() > 1 {
            parts[1].to_string()
        } else {
            name.to_string()
        }
    } else {
        name.to_string()
    };

    let bin_path = if package_json_path.exists() {
        if let Ok(package_json_str) = fs::read_to_string(&package_json_path) {
            if let Ok(package_json) = serde_json::from_str::<serde_json::Value>(&package_json_str) {
                if let Some(bin_obj) = package_json.get("bin").and_then(|v| v.as_object()) {
                    // bin is an object mapping command names to paths
                    if let Some(bin_entry) = bin_obj.get(&bin_name) {
                        Some(pkg_dir.join(bin_entry.as_str().unwrap_or("index.js")))
                    } else if let Some(bin_entry) = bin_obj.get(name) {
                        Some(pkg_dir.join(bin_entry.as_str().unwrap_or("index.js")))
                    } else {
                        // Try first key
                        bin_obj.keys().next().map(|k| pkg_dir.join(bin_obj.get(k).unwrap().as_str().unwrap_or("index.js")))
                    }
                } else if let Some(bin_str) = package_json.get("bin").and_then(|v| v.as_str()) {
                    // bin is a string
                    Some(pkg_dir.join(bin_str))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    // Fallback to main or index.js
    let entry_path = bin_path.unwrap_or_else(|| {
        let main_path = if package_json_path.exists() {
            if let Ok(package_json_str) = fs::read_to_string(&package_json_path) {
                if let Ok(package_json) = serde_json::from_str::<serde_json::Value>(&package_json_str) {
                    package_json.get("main").and_then(|v| v.as_str()).map(|m| pkg_dir.join(m)).unwrap_or(pkg_dir.join("index.js"))
                } else {
                    pkg_dir.join("index.js")
                }
            } else {
                pkg_dir.join("index.js")
            }
        } else {
            pkg_dir.join("index.js")
        };
        main_path
    });

    // Check if the entry point exists
    if !entry_path.exists() {
        anyhow::bail!("entry point not found: {}", entry_path.display());
    }

    // Save current directory
    let original_dir = std::env::current_dir()?;

    // Change to the package directory context
    std::env::set_current_dir(&pkg_dir)?;

    // Create a wrapper script that sets process.argv and requires the entry point
    // Node semantics: argv[0] = executable, argv[1] = script path, argv[2..] = args
    let escaped_entry = entry_path.display().to_string().replace('\\', "\\\\").replace('\'', "\\'");
    let args_json: Vec<String> = args
        .iter()
        .map(|a| format!("'{}'", a.replace('\\', "\\\\").replace('\'', "\\'")))
        .collect();
    let wrapper_content = format!(
        "process.argv = ['honey', '{}', {}];\nrequire('{}');\n",
        escaped_entry,
        args_json.join(", "),
        escaped_entry
    );
    
    // Write wrapper to a temp file OUTSIDE the project: Axiom policy counts
    // companion files as bugs, so nothing transient is left in the package dir.
    let wrapper_path = std::env::temp_dir().join(format!("hxney-wrapper-{}.js", std::process::id()));
    fs::write(&wrapper_path, &wrapper_content).context("failed to write wrapper")?;

    // Execute the wrapper
    let runtime = Runtime::new();
    let result = runtime.run_file(wrapper_path.to_str().unwrap_or(""), &[]);

    // Clean up wrapper
    let _ = fs::remove_file(&wrapper_path);

    // Restore original directory
    std::env::set_current_dir(&original_dir)?;

    match result {
        Ok(code) => {
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}