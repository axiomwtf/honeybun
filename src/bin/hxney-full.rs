use anyhow::{Context, Result};
use clap::Parser;
use honey::runtime::Runtime;
use honey::PackageManager;
use serde_json;
use std::fs;
use std::path::Path;

#[derive(Parser)]
#[command(name = "hxney")]
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
    let cli = Cli::parse();

    if let Err(e) = execute_package(&cli.package, &cli.args) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
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