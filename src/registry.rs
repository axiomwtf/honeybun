use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde_json::Value;
use std::collections::HashMap;

use crate::package::{RegistryPackage, RegistryResponse};

pub struct Registry {
    client: Client,
    client_async: reqwest::Client,
    npm_url: String,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
            client_async: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
            npm_url: "https://registry.npmjs.org".to_string(),
        }
    }

    /// Client shared by concurrent downloads (tokio pipeline).
    pub fn async_client(&self) -> reqwest::Client {
        self.client_async.clone()
    }

    pub fn npm_url(&self) -> &str {
        &self.npm_url
    }

    pub fn get_package(&self, name: &str) -> Result<RegistryResponse> {
        let url = format!("{}/{}", self.npm_url, name);

        let resp = self
            .client
            .get(&url)
            .send()
            .with_context(|| format!("failed to fetch package: {}", name))?;

        if !resp.status().is_success() {
            anyhow::bail!("package not found: {} (status {})", name, resp.status());
        }

        let body: Value = resp.json().context("failed to parse registry response")?;

        parse_package_response(name, body)
    }

    pub fn resolve_version(&self, name: &str, version_spec: &str) -> Result<RegistryPackage> {
        let response = self.get_package(name)?;
        pick_version(&response, name, version_spec)
    }
}

/// Fetch and resolve a package version concurrently (used by the parallel
/// install pipeline).
pub async fn resolve_package_async(
    client: &reqwest::Client,
    npm_url: &str,
    name: &str,
    version_spec: &str,
) -> Result<RegistryPackage> {
    let url = format!("{}/{}", npm_url, name);

    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("failed to fetch package: {}", name))?;

    if !resp.status().is_success() {
        anyhow::bail!("package not found: {} (status {})", name, resp.status());
    }

    let body: Value = resp.json().await.context("failed to parse registry response")?;

    let response = parse_package_response(name, body)?;
    pick_version(&response, name, version_spec)
}

fn parse_package_response(name: &str, body: Value) -> Result<RegistryResponse> {
    let dist_tags: HashMap<String, String> = body
        .get("dist-tags")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let versions: HashMap<String, RegistryPackage> = body
        .get("versions")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    if versions.is_empty() {
        anyhow::bail!("package not found or empty: {}", name);
    }

    Ok(RegistryResponse { dist_tags, versions })
}

fn pick_version(response: &RegistryResponse, name: &str, version_spec: &str) -> Result<RegistryPackage> {
    let spec = version_spec.trim();

    let latest = response.dist_tags.get("latest").cloned().or_else(|| {
        response
            .versions
            .keys()
            .max_by(|a, b| version_compare(a, b))
            .cloned()
    });

    let matches_spec = |candidate: &str| -> bool {
        // Handle OR specs by taking the first alternative
        if spec.contains("||") {
            let first = spec.split("||").next().unwrap_or("").trim();
            return spec_matches_single(first, candidate);
        }
        spec_matches_single(spec, candidate)
    };

    let version = if response.versions.contains_key(spec) {
        // Exact version present in the registry
        spec.to_string()
    } else {
        let mut matching: Vec<&String> = response
            .versions
            .keys()
            .filter(|v| matches_spec(v))
            .collect();
        matching.sort_by(|a, b| version_compare(b, a));
        matching
            .first()
            .map(|s| (*s).clone())
            // Fall back to latest if no version matches the spec
            .or(latest)
            .unwrap_or_default()
    };

    response
        .versions
        .get(&version)
        .cloned()
        .with_context(|| format!(
            "version {} not found for package {} (resolved spec: {:?}, available: {:?})",
            version, name, spec, response.versions.keys().collect::<Vec<_>>()
        ))
}

fn spec_matches_single(spec: &str, candidate: &str) -> bool {
    let spec = spec.trim();
    if spec.is_empty() || spec == "*" || spec == "latest" || spec == "x" {
        return true;
    }

    // Range specs like ">= 1.2.0", "> 1.0.0", "< 2.0.0": any candidate at or
    // above the lower bound is acceptable; pick via version_compare.
    for (op, cmp) in [(">=", std::cmp::Ordering::Greater), (">", std::cmp::Ordering::Equal)] {
        if let Some(rest) = spec.strip_prefix(op) {
            let rest = rest.trim();
            return rest.is_empty() || version_compare(candidate, rest) != std::cmp::Ordering::Less;
        }
        let _ = cmp;
    }
    if spec.starts_with('<') {
        let rest = spec[1..].trim();
        if rest.is_empty() {
            return true;
        }
        return version_compare(candidate, rest) == std::cmp::Ordering::Less;
    }

    if let Some(target) = spec.strip_prefix('^') {
        return semver_caret_match(candidate, target);
    }
    if let Some(target) = spec.strip_prefix('~') {
        return semver_tilde_match(candidate, target);
    }

    // Partial or exact versions: "1", "1.2", "1.2.3"
    let c_parts = parse_parts(candidate);
    let s_parts = parse_parts(spec);
    for (i, s) in s_parts.iter().enumerate() {
        match c_parts.get(i) {
            Some(c) if c == s => continue,
            _ => return false,
        }
    }
    true
}

fn parse_parts(v: &str) -> Vec<u32> {
    v.split('.')
        .take_while(|s| !s.is_empty())
        .map(|s| s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect()
}

fn semver_caret_match(v: &str, target: &str) -> bool {
    // ^1.2.3: same major; ^0.x: same minor
    let v_parts = parse_parts(v);
    let t_parts = parse_parts(target);

    if v_parts.len() < 1 || t_parts.len() < 1 {
        return false;
    }

    if t_parts[0] != 0 {
        return v_parts[0] == t_parts[0];
    }
    if t_parts.len() >= 2 {
        return v_parts[0] == 0 && v_parts.get(1) == Some(&t_parts[1]);
    }
    v_parts[0] == 0
}

fn semver_tilde_match(v: &str, target: &str) -> bool {
    // ~1.2.3: same major and minor
    let v_parts = parse_parts(v);
    let t_parts = parse_parts(target);

    if v_parts.len() < 1 || t_parts.len() < 1 {
        return false;
    }

    if v_parts[0] != t_parts[0] {
        return false;
    }
    if t_parts.len() >= 2 {
        return v_parts.get(1) == Some(&t_parts[1]);
    }
    true
}

fn version_compare(a: &str, b: &str) -> std::cmp::Ordering {
    let a_parts: Vec<u32> = a.split('.').filter_map(|s| s.parse().ok()).collect();
    let b_parts: Vec<u32> = b.split('.').filter_map(|s| s.parse().ok()).collect();

    for i in 0..a_parts.len().max(b_parts.len()) {
        let a_part = a_parts.get(i).unwrap_or(&0);
        let b_part = b_parts.get(i).unwrap_or(&0);
        match a_part.cmp(b_part) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}