use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct CinmLayout {
    pub root: PathBuf,
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
            }
        }
    }

    pub fn package_dir(&self, name: &str, version: &str) -> PathBuf {
        let safe_name = name.replace('/', "+");
        self.root.join(format!("{}@{}", safe_name, version))
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
        self.install_bytes(name, version, &bytes)
    }

    /// Write and extract a downloaded tarball into `.cinm/<name>@<version>`,
    /// through a temp sibling dir + rename so a failure never leaves a
    /// half-written package directory behind.
    fn install_bytes(&self, name: &str, version: String, bytes: &[u8]) -> Result<PathBuf> {
        let dir = self.package_dir(name, &version);
        if dir.exists() {
            return Ok(dir);
        }

        let unique = format!(
            ".tmp-{}-{}-{}",
            name.replace('/', "+"),
            version,
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let tmp = self.root.join(&unique);

        let result = (|| -> Result<PathBuf> {
            fs::create_dir_all(&tmp).context("failed to create temp package directory")?;

            let staging = tmp.join("pkg");
            let tarball_path = tmp.join("package.tgz");

            fs::write(&tarball_path, bytes).context("failed to write tarball")?;

            extract_tarball(&tarball_path, &staging).context("failed to extract tarball")?;

            let _ = fs::remove_file(&tarball_path);

            // First task wins; a concurrent task for the same package@version
            // (or a leftover from a previous run) reuses the already-present
            // directory.
            if dir.exists() {
                let _ = fs::remove_dir_all(&tmp);
                return Ok(dir);
            }
            match fs::rename(&staging, &dir) {
                Ok(()) => {}
                Err(_) if dir.exists() => {
                    // Lost a race: keep the winner, drop our temp copy.
                }
                Err(e) => return Err(e).context("failed to move package into .cinm"),
            }
            let _ = fs::remove_dir_all(&tmp);

            Ok(dir)
        })();

        if result.is_err() && tmp.exists() {
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

        Ok(())
    }

    pub fn cleanup(&self) -> Result<()> {
        if self.root.exists() {
            fs::remove_dir_all(&self.root).context("failed to remove .cinm directory")?;
        }
        Ok(())
    }
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