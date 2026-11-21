pub mod build;
pub mod cinm;
pub mod config;
pub mod daemon;
pub mod package;
pub mod registry;
pub mod runtime;
pub mod service;

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::cinm::CinmLayout;
use crate::package::{Lockfile, PackageJson, RegistryPackage};
use crate::registry::Registry;

/// Maximum simultaneous registry fetches / tarball downloads.
const MAX_CONCURRENT_DOWNLOADS: usize = 8;

pub struct PackageManager {
    pub registry: Arc<Registry>,
    pub layout: Arc<CinmLayout>,
}

/// One successfully installed (or deduped) package.
struct PackageOutcome {
    name: String,
    pkg: RegistryPackage,
}

impl PackageManager {
    pub fn new(project_root: &Path) -> Self {
        Self {
            registry: Arc::new(Registry::new()),
            layout: Arc::new(CinmLayout::new(project_root)),
        }
    }

    pub fn install(&self, production: bool) -> Result<()> {
        println!("honey install");

        self.layout.init().context("failed to initialize .cinm")?;

        let package_json_path = Path::new("package.json");
        if !package_json_path.exists() {
            println!("no package.json found, creating empty install");
            return Ok(());
        }

        let package_json: PackageJson = serde_json::from_str(
            &std::fs::read_to_string(package_json_path).context("failed to read package.json")?,
        )
        .context("failed to parse package.json")?;

        let mut all_deps = package_json.dependencies.clone();
        if !production {
            all_deps.extend(package_json.dev_dependencies.clone());
        }

        if all_deps.is_empty() {
            println!("no dependencies to install");
            return Ok(());
        }

        println!("installing {} packages...", all_deps.len());

        self.layout.cleanup_temp_dirs();

        let existing_lock = Self::read_lockfile();
        let lockfile = self.resolve_and_download(&all_deps, existing_lock)?;
        self.write_lockfile(&lockfile)?;

        self.layout.create_node_modules_symlink(Path::new("."))?;

        println!("done");
        Ok(())
    }

    fn read_lockfile() -> Option<Lockfile> {
        let data = std::fs::read("honey.lock").ok()?;
        Lockfile::from_binary(&data)
    }

    fn resolve_and_download(&self, deps: &HashMap<String, String>, existing_lock: Option<Lockfile>) -> Result<Lockfile> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to start tokio runtime")?;
        rt.block_on(self.resolve_and_download_concurrent(deps, existing_lock))
    }

    async fn resolve_and_download_concurrent(&self, deps: &HashMap<String, String>, existing_lock: Option<Lockfile>) -> Result<Lockfile> {
        let start = std::time::Instant::now();

        let existing_lock: Arc<Lockfile> = Arc::new(existing_lock.unwrap_or_else(Lockfile::new));
        let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_DOWNLOADS));
        let client = self.registry.async_client();
        let npm_url = self.registry.npm_url().to_string();
        // Packages already being installed or done (first task wins, the
        // second reuses the result).
        let claimed: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

        let mut lockfile = Lockfile::new();
        let mut queue: Vec<(String, String)> = deps
            .iter()
            .map(|(name, version)| (name.clone(), version.clone()))
            .collect();
        let mut queued: HashSet<String> = HashSet::new();
        let mut installed_count = 0usize;
        let mut failures: Vec<String> = Vec::new();

        // Breadth-first waves: each wave resolves + downloads its whole
        // frontier concurrently, then the next wave handles the dependency
        // frontier discovered along the way.
        while !queue.is_empty() {
            let frontier = std::mem::take(&mut queue);

            let mut tasks = Vec::new();
            for (name, version_spec) in frontier {
                let task_key = format!("{}@{}", name, version_spec);
                if !queued.insert(task_key) {
                    continue;
                }

                let semaphore = semaphore.clone();
                let client = client.clone();
                let npm_url = npm_url.clone();
                let claimed = claimed.clone();
                let existing_lock = existing_lock.clone();
                let layout = self.layout.clone();

                tasks.push(tokio::spawn(async move {
                    let label = format!("{}@{}", name, version_spec);
                    let outcome: Result<PackageOutcome> = async {
                        let _permit = semaphore.acquire().await.expect("semaphore closed");

                        // A package already present in honey.lock with its
                        // .cinm directory intact needs no network at all.
                        let pkg = match existing_lock.packages.get(&name) {
                            Some(entry) if layout.package_dir(&name, &entry.version).exists() => {
                                RegistryPackage {
                                    name: name.clone(),
                                    version: entry.version.clone(),
                                    dist: crate::package::RegistryDist {
                                        tarball: entry.resolved.clone(),
                                        integrity: entry.integrity.clone(),
                                    },
                                    dependencies: entry.dependencies.clone(),
                                }
                            }
                            _ => {
                                registry::resolve_package_async(&client, &npm_url, &name, &version_spec)
                                    .await
                                    .with_context(|| format!("failed to resolve {}@{}", name, version_spec))?
                            }
                        };

                        // Same package+version handled by two specs: first install
                        // wins, the second reuses the directory.
                        let install_key = format!("{}@{}", name, pkg.version);
                        let must_install = claimed.lock().unwrap().insert(install_key);

                        if must_install {
                            layout
                                .install_package_async(&client, &name, pkg.version.clone(), &pkg.dist.tarball, pkg.dist.integrity.as_deref())
                                .await
                                .with_context(|| format!("failed to install {}", name))?;
                            println!("  {}@{}", name, pkg.version);
                        }

                        Ok(PackageOutcome { name, pkg })
                    }
                    .await;
                    outcome.map_err(|err| (label, err))
                }));
            }

            for task in tasks {
                match task.await {
                    Ok(Ok(outcome)) => {
                        installed_count += 1;
                        lockfile.packages.insert(
                            outcome.name.clone(),
                            crate::package::LockEntry {
                                version: outcome.pkg.version.clone(),
                                resolved: outcome.pkg.dist.tarball.clone(),
                                integrity: outcome.pkg.dist.integrity.clone(),
                                dependencies: outcome.pkg.dependencies.clone(),
                                dev: false,
                            },
                        );
                        for (dep_name, dep_version) in &outcome.pkg.dependencies {
                            queue.push((dep_name.clone(), dep_version.clone()));
                        }
                    }
                    Ok(Err((label, err))) => failures.push(format!("{}: {}", label, err)),
                    Err(err) => failures.push(format!("task failed: {}", err)),
                }
            }
        }

        if !failures.is_empty() {
            anyhow::bail!(
                "failed to install {} package(s):\n  {}",
                failures.len(),
                failures.join("\n  ")
            );
        }

        println!(
            "installed {} packages in {:.2}s",
            installed_count,
            start.elapsed().as_secs_f64()
        );

        Ok(lockfile)
    }

    fn write_lockfile(&self, lockfile: &Lockfile) -> Result<()> {
        let data = lockfile.to_binary();
        // Atomic single-writer: write a temp sibling, then rename over the
        // final lockfile.
        let tmp = Path::new(".honey.lock.tmp");
        std::fs::write(tmp, data).context("failed to write honey.lock")?;
        std::fs::rename(tmp, "honey.lock").context("failed to finalize honey.lock")?;
        Ok(())
    }

    pub fn add(&self, package: &str, dev: bool) -> Result<()> {
        println!("honey add {}", package);

        let parts: Vec<&str> = package.splitn(2, '@').collect();
        let (name, version_spec) = if package.starts_with('@') {
            let at_pos = package[1..].find('@').map(|p| p + 1);
            if let Some(pos) = at_pos {
                (&package[..pos], &package[pos + 1..])
            } else {
                (package, "latest")
            }
        } else if parts.len() == 2 {
            (parts[0], parts[1])
        } else {
            (package, "latest")
        };

        let pkg = self.registry.resolve_version(name, version_spec)?;
        println!("  resolved {}@{}", name, pkg.version);

        let _dir = self.layout.install_package(name, pkg.version.clone(), &pkg.dist.tarball, pkg.dist.integrity.as_deref())?;

        self.update_package_json(name, &pkg.version, dev)?;
        self.install(false)?;

        println!("added {}@{}", name, pkg.version);
        Ok(())
    }

    fn update_package_json(&self, name: &str, version: &str, dev: bool) -> Result<()> {
        let path = Path::new("package.json");
        let mut pkg: serde_json::Value = if path.exists() {
            serde_json::from_str(&std::fs::read_to_string(path)?)?
        } else {
            serde_json::json!({
                "name": "project",
                "version": "1.0.0"
            })
        };

        let dep_key = if dev { "devDependencies" } else { "dependencies" };
        if pkg.get(dep_key).is_none() {
            pkg[dep_key] = serde_json::json!({});
        }

        pkg[dep_key][name] = serde_json::json!(format!("^{}", version));

        let content = serde_json::to_string_pretty(&pkg)?;
        std::fs::write(path, content).context("failed to write package.json")?;

        Ok(())
    }

    pub fn remove(&self, package: &str) -> Result<()> {
        println!("honey remove {}", package);

        let path = Path::new("package.json");
        if !path.exists() {
            anyhow::bail!("no package.json found");
        }

        let mut pkg: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;

        for key in &["dependencies", "devDependencies", "optionalDependencies", "peerDependencies"] {
            if let Some(obj) = pkg.get_mut(*key) {
                if let Some(map) = obj.as_object_mut() {
                    map.remove(package);
                }
            }
        }

        let content = serde_json::to_string_pretty(&pkg)?;
        std::fs::write(path, content).context("failed to write package.json")?;

        let dir = self.layout.package_dir(package, "*");
        if dir.exists() {
            let _ = std::fs::remove_dir_all(&dir);
        }

        self.install(false)?;

        println!("removed {}", package);
        Ok(())
    }
}

pub use runtime::Runtime;