use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageJson {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub dependencies: HashMap<String, String>,
    #[serde(default)]
    pub dev_dependencies: HashMap<String, String>,
    #[serde(rename = "optionalDependencies", default)]
    pub optional_dependencies: HashMap<String, String>,
    #[serde(rename = "peerDependencies", default)]
    pub peer_dependencies: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockEntry {
    pub version: String,
    pub resolved: String,
    pub integrity: Option<String>,
    #[serde(default)]
    pub dependencies: HashMap<String, String>,
    #[serde(default)]
    pub dev: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lockfile {
    #[serde(default)]
    pub packages: HashMap<String, LockEntry>,
}

impl Lockfile {
    pub fn new() -> Self {
        Self {
            packages: HashMap::new(),
        }
    }

    pub fn to_binary(&self) -> Vec<u8> {
        let json = serde_json::to_vec(self).unwrap_or_default();
        let mut compressed = Vec::new();
        {
            let mut encoder = flate2::write::GzEncoder::new(&mut compressed, flate2::Compression::best());
            let _ = std::io::Write::write_all(&mut encoder, &json);
        }
        compressed
    }

    pub fn from_binary(data: &[u8]) -> Option<Self> {
        let mut decoder = flate2::read::GzDecoder::new(data);
        let mut json = Vec::new();
        if std::io::Read::read_to_end(&mut decoder, &mut json).is_ok() {
            serde_json::from_slice(&json).ok()
        } else {
            serde_json::from_slice(data).ok()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryPackage {
    pub name: String,
    pub version: String,
    #[serde(rename = "dist")]
    pub dist: RegistryDist,
    #[serde(default)]
    pub dependencies: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryDist {
    pub tarball: String,
    #[serde(default)]
    pub integrity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryResponse {
    #[serde(rename = "dist-tags")]
    pub dist_tags: HashMap<String, String>,
    pub versions: HashMap<String, RegistryPackage>,
}