use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct CinmLayout {
    pub root: PathBuf,
}

/// Package-directory name inside `.cinm` for a package name + version.
/// Scoped packages (`@scope/pkg`) live nested: `.cinm/@scope/pkg@1.2.3/`
/// (exactly what the spec documents); the flat legacy encoding
/// (`.cinm/@scope+pkg@1.2.3/`) is still resolved for older stores.
pub fn package_relative_dir(name: &str, version: &str) -> PathBuf {
    match name.split_once('/') {
        Some((scope, pkg)) => PathBuf::from(scope).join(format!("{}@{}", pkg, version)),
        None => PathBuf::from(format!("{}@{}", name, version)),
    }
}

/// Flat store/cache encoding: `/` -> `+` (single level, no nested dirs in
/// the shared store).
fn flat_store_name(name: &str, version: &str) -> String {
    format!("{}@{}", name.replace('/', "+"), version)
}

/// Shared cross-project store root: `$XDG_CACHE_HOME/honey/store`
/// (defaulting to `~/.cache/honey/store`). Every package tarball is
/// extracted here exactly once on the whole machine; project `.cinm`
/// directories hard-link the files so shared dependencies cost near-zero
/// extra disk.
pub fn shared_store_root() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".cache"))
        })
        .unwrap_or_else(|| std::env::temp_dir());
    base.join("honey").join("store")
}

impl CinmLayout {
    pub fn new(project_root: &Path) -> Self {
        Self {
            root: project_root.join(".cinm"),
        }
    }

    pub fn init(&self) -> Result<()> {
        fs::create_dir_all(&self.root).context("failed to create .cinm directory")?;
        Ok(())
    }

    /// Remove temp staging dirs left behind by an interrupted install.
    pub fn cleanup_temp_dirs(&self) {
        if let Ok(entries) = fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(".tmp-") {
                    let _ = fs::remove_dir_all(entry.path());
                }
                // Nested scoped dirs also can hold temp staging dirs.
                if name.starts_with('@') {
                    if let Ok(scoped) = fs::read_dir(entry.path()) {
                        for sub in scoped.flatten() {
                            let sub_name = sub.file_name().to_string_lossy().to_string();
                            if sub_name.starts_with(".tmp-") {
                                let _ = fs::remove_dir_all(sub.path());
                            }
                        }
                    }
                }
            }
        }
    }

    /// Full versioned package dir: `.cinm/@scope/pkg@<version>` (scoped) or
    /// `.cinm/pkg@<version>`.
    pub fn package_dir(&self, name: &str, version: &str) -> PathBuf {
        self.root.join(package_relative_dir(name, version))
    }

    /// Every installed version dir for a package, any version (for remove).
    pub fn all_package_dirs(&self, name: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        match name.split_once('/') {
            Some((scope, pkg)) => {
                let prefix = format!("{}@", pkg);
                if let Ok(entries) = fs::read_dir(self.root.join(scope)) {
                    for entry in entries.flatten() {
                        let entry_name = entry.file_name().to_string_lossy().to_string();
                        if entry_name.starts_with(&prefix) {
                            out.push(entry.path());
                        }
                    }
                }
                // legacy flat layout
                let flat_prefix = format!("{}+{}@", scope, pkg);
                if let Ok(entries) = fs::read_dir(&self.root) {
                    for entry in entries.flatten() {
                        let entry_name = entry.file_name().to_string_lossy().to_string();
                        if entry_name.starts_with(&flat_prefix) {
                            out.push(entry.path());
                        }
                    }
                }
            }
            None => {
                let prefix = format!("{}@", name);
                if let Ok(entries) = fs::read_dir(&self.root) {
                    for entry in entries.flatten() {
                        let entry_name = entry.file_name().to_string_lossy().to_string();
                        if entry_name.starts_with(&prefix) {
                            out.push(entry.path());
                        }
                    }
                }
            }
        }
        out
    }

    pub fn install_package(&self, name: &str, version: String, tarball_url: &str, _integrity: Option<&str>) -> Result<PathBuf> {
        let resp = reqwest::blocking::get(tarball_url).context("failed to download tarball")?;
        if !resp.status().is_success() {
            anyhow::bail!("failed to download tarball: {} (status {})", tarball_url, resp.status());
        }
        let bytes = resp.bytes().context("failed to read tarball bytes")?.to_vec();
        self.install_bytes(name, version, &bytes)
    }

    /// Concurrent variant used by the parallel install pipeline: downloads
    /// through the shared async client, extracting into a private temp sibling
    /// directory that is renamed into place only once fully written.
    pub async fn install_package_async(&self, client: &reqwest::Client, name: &str, version: String, tarball_url: &str, _integrity: Option<&str>) -> Result<PathBuf> {
        let resp = client
            .get(tarball_url)
            .send()
            .await
            .context("failed to download tarball")?;
        if !resp.status().is_success() {
            anyhow::bail!("failed to download tarball: {} (status {})", tarball_url, resp.status());
        }
        let bytes = resp.bytes().await.context("failed to read tarball bytes")?;
        Self::install_bytes_blocking(name, version, &bytes)
    }

    pub fn install_bytes(&self, name: &str, version: String, bytes: &[u8]) -> Result<PathBuf> {
        Self::install_bytes_blocking(name, version, bytes)
    }

    /// Write and extract a downloaded tarball into the shared store
    /// (`~/.cache/honey/store/`), then materialize the project copy by
    /// HARD-LINKING each file from the store into
    /// `.cinm/<name>@<version>`. Both projects and the store share the same
    /// inodes, so installing the same package version in N projects costs
    /// one extraction total. When linking is impossible (cross-device mount,
    /// filesystem without hard links) the project copy silently falls back
    /// to copying — same layout, same behavior.
    fn install_bytes_blocking(name: &str, version: String, bytes: &[u8]) -> Result<PathBuf> {
        let layout = CinmLayout::new(Path::new("."));
        let dir = layout.package_dir(name, &version);
        if dir.exists() {
            return Ok(dir);
        }

        let store = shared_store_root();
        let store_pkg = store.join(flat_store_name(name, &version));

        // Materialize the store copy once (atomic temp + rename).
        if !store_pkg.exists() {
            fs::create_dir_all(&store).ok();
            // Scoped store names nest ("@scope/name@x.y.z"): create parents.
            if let Some(parent) = store_pkg.parent() {
                fs::create_dir_all(parent).ok();
            }
            let unique = format!(
                ".tmp-{}-{}",
                flat_store_name(name, &version),
                TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let tmp = store.join(&unique);
            let result = (|| -> Result<()> {
                fs::create_dir_all(&tmp).context("failed to create temp store dir")?;
                let staging = tmp.join("pkg");
                let tarball_path = tmp.join("package.tgz");
                fs::write(&tarball_path, bytes).context("failed to write tarball")?;
                extract_tarball(&tarball_path, &staging).context("failed to extract tarball")?;
                let _ = fs::remove_file(&tarball_path);
                if store_pkg.exists() {
                    return Ok(()); // lost a race; the winner is fine
                }
                match fs::rename(&staging, &store_pkg) {
                    Ok(()) => {}
                    Err(_) if store_pkg.exists() => {}
                    Err(e) => return Err(e).context("failed to move package into shared store"),
                }
                Ok(())
            })();
            if result.is_err() {
                let _ = fs::remove_dir_all(&tmp);
                // Fall through: maybe another process won the race after all.
                if !store_pkg.exists() {
                    result.context("failed to materialize shared store entry")?;
                }
            } else {
                let _ = fs::remove_dir_all(&tmp);
            }
        }

        if !store_pkg.exists() {
            anyhow::bail!("shared store entry for {}@{} missing after extraction", name, version);
        }

        // Link (or copy, as fallback) the store files into the project's
        // .cinm via an atomic staging dir + rename.
        let unique = format!(
            ".tmp-{}-{}",
            flat_store_name(name, &version),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let tmp = layout.root.join(&unique);

        let result = (|| -> Result<PathBuf> {
            fs::create_dir_all(&tmp).context("failed to create temp package directory")?;
            let staging = tmp.join("pkg");
            create_dir_all(&staging).context("failed to create staging dir")?;
            hard_link_tree(&store_pkg, &staging, true).context("failed to link store package into project")?;

            if dir.exists() {
                return Ok(dir);
            }
            // Scoped packages live in a nested dir (".cinm/@scope/name@x.y.z"):
            // the "@scope" parent must exist before the rename.
            if let Some(parent) = dir.parent() {
                fs::create_dir_all(parent)
                    .context("failed to create parent dir for scoped package")?;
            }
            match fs::rename(&staging, &dir) {
                Ok(()) => {}
                Err(_) if dir.exists() => {
                    // Lost a race: keep the winner, drop our temp copy.
                }
                Err(e) => return Err(e).context("failed to move package into .cinm"),
            }
            Ok(dir)
        })();

        if result.is_err() && tmp.exists() {
            let _ = fs::remove_dir_all(&tmp);
        } else {
            let _ = fs::remove_dir_all(&tmp);
        }
        result
    }

    pub fn create_node_modules_symlink(&self, project_root: &Path) -> Result<()> {
        let node_modules = project_root.join("node_modules");
        if node_modules.exists() {
            let _ = fs::remove_dir_all(&node_modules);
            let _ = fs::remove_file(&node_modules);
        }

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&self.root, &node_modules)
                .context("failed to create node_modules symlink")?;
        }

        #[cfg(windows)]
        {
            // Prefer a real symlink; fall back to a junction, which needs no
            // developer-mode/privilege on Windows.
            if std::os::windows::fs::symlink_dir(&self.root, &node_modules).is_err() {
                std::os::windows::fs::symlink_junction(&self.root, &node_modules)
                    .context("failed to create node_modules junction")?;
            }
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = project_root;
            anyhow::bail!("node_modules interop is unsupported on this platform");
        }

        Ok(())
    }

    pub fn cleanup(&self) -> Result<()> {
        if self.root.exists() {
            fs::remove_dir_all(&self.root).context("failed to remove .cinm directory")?;
        }
        Ok(())
    }
}

fn create_dir_all(p: &Path) -> Result<()> {
    fs::create_dir_all(p).with_context(|| format!("failed to create dir {}", p.display()))
}

/// Recursively hard-link every file under `src` into `dst`, creating the
/// directory structure. `fallback_copy` copies when linking fails (cross
/// device, unsupported fs). Returns whether every file was hard-linked.
fn hard_link_tree(src: &Path, dst: &Path, fallback_copy: bool) -> Result<bool> {
    for entry in fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            fs::create_dir_all(&to)?;
            if !hard_link_tree(&entry.path(), &to, fallback_copy)? {
                if !fallback_copy {
                    return Ok(false);
                }
            }
        } else if ty.is_file() {
            if fs::hard_link(entry.path(), &to).is_err() {
                if !fallback_copy {
                    return Ok(false);
                }
                fs::copy(entry.path(), &to)
                    .with_context(|| format!("failed to copy {}", entry.path().display()))?;
            }
        } else if ty.is_symlink() {
            // Preserve store-internal symlinks as copies of their targets
            // (npm packages rarely symlink inside their own tree; bins do).
            let target = fs::read_link(entry.path()).ok();
            let _ = target;
            if let Ok(data) = fs::read(entry.path()) {
                fs::write(&to, data).ok();
            }
        }
    }
    Ok(true)
}

fn extract_tarball(tarball: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(tarball).context("failed to open tarball")?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);

    let prefix = "package";
    for entry in archive.entries().context("failed to read tar entries")? {
        let mut entry = entry.context("failed to read tar entry")?;
        let path = entry.path().context("failed to get entry path")?;
        let path_str = path.to_string_lossy();

        if let Some(stripped) = path_str.strip_prefix(&format!("{}/", prefix)) {
            let dest_path = dest.join(stripped);

            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent).context("failed to create parent directory")?;
            }

            entry.unpack(&dest_path).context("failed to unpack entry")?;
        }
    }

    Ok(())
}
