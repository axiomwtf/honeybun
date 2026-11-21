#![allow(dead_code)]

//! `honey build` — the Cinnamon bundler (SPEC.md section 5, "Bundler: Graph to
//! output").
//!
//! Walks the module graph from the entry (ESM import/export-from/dynamic
//! import + CJS require, statically literal specifiers only), transpiles
//! TypeScript with the Oxc parser + transformer (the same pipeline the runtime
//! uses, called directly on the oxc crates — no imports from src/runtime.rs),
//! and emits one self-contained file: a CJS-style module registry keyed by
//! resolved absolute path, with the entry module required last.
//!
//! v1 scoping decisions:
//! - Bare npm specifiers (`require('lodash')`, `import 'lodash'`) are INLINED
//!   when resolvable from disk: honey's `.cinm` store (versioned layout) or a
//!   standard npm `node_modules` tree, via package.json `exports`/`main`.
//!   Unresolvable bare names, `node:` builtins and URL specifiers stay
//!   EXTERNAL: the emitted file keeps the literal specifier for the runtime
//!   to resolve. CSS imports are extracted to a sibling `.css` output.
//! - honey.config.ts is ignored (config hooks are wired later).
//! - No tree-shaking, no minification (later Cinnamon layers).
//! - Cycles are allowed and behave like CommonJS circular requires: a module
//!   mid-evaluation exposes its (partial) exports object.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// CLI entry point
// ---------------------------------------------------------------------------

/// Sourcemap emission mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceMapMode {
    /// No sourcemaps.
    None,
    /// Write `<output>.map` next to the bundle, no link comment (Bun's
    /// `--sourcemap external`).
    External,
    /// Write `<output>.map` and append `//# sourceMappingURL=` (Bun's
    /// `--sourcemap linked`; the default for non-minified builds).
    Linked,
    /// Embed the map as a base64 data-URL comment inside the bundle.
    Inline,
}

/// `honey build` options (production decides defaults; explicit flags win).
#[derive(Clone, Debug, Default)]
pub struct BuildOptions {
    /// Production mode: NODE_ENV define + static-if elimination.
    pub production: bool,
    /// Minify: None → follow `production`.
    pub minify: Option<bool>,
    /// Sourcemap: None → linked when minify is off, external when on.
    pub sourcemap: Option<SourceMapMode>,
    /// Emission target. Browser (the default) keeps the `process` guard and
    /// the module-guarded entry require; node assumes a host with `process`
    /// and assigns `module.exports` unconditionally.
    pub target: Option<BuildTarget>,
    /// User defines (`--define k=v`), replaced with their value verbatim
    /// (quoted strings / true / false / numbers / null).
    pub define: Vec<(String, String)>,
}

/// Bundle emission target (mirrors Bun's `--target`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BuildTarget {
    #[default]
    Browser,
    Node,
}

impl BuildOptions {
    pub fn minify_enabled(&self) -> bool {
        self.minify.unwrap_or(self.production)
    }

    pub fn sourcemap_mode(&self) -> SourceMapMode {
        self.sourcemap.unwrap_or(if self.minify_enabled() {
            SourceMapMode::External
        } else {
            SourceMapMode::Linked
        })
    }

    pub fn target(&self) -> BuildTarget {
        self.target.unwrap_or(BuildTarget::Browser)
    }

    /// Compile-time defines; production pins NODE_ENV to "production".
    /// User defines win over the built-in NODE_ENV define.
    pub fn defines(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if self.production {
            out.push(("process.env.NODE_ENV".to_string(), "\"production\"".to_string()));
        }
        for (k, v) in &self.define {
            out.retain(|(ek, _)| ek != k);
            out.push((k.clone(), v.clone()));
        }
        out
    }
}

/// Classify a file extension as a static asset (copied to the output
/// directory, imported as its URL string).
fn is_static_asset(ext: &str) -> bool {
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "svg" | "ico" | "bmp"
            | "mp4" | "webm" | "mp3" | "wav" | "ogg"
            | "woff" | "woff2" | "ttf" | "otf" | "eot"
            | "wasm" | "txt" | "xml" | "bin"
    )
}

/// FNV-1a 64-bit content hash, lowercase hex (used for asset file names).
fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Output name for a static asset: `<stem>.<hash8>.<ext>` — keyed by content
/// so the name is computable at the import site and at copy time alike, and
/// identical content reuses one file.
fn asset_output_name(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let hash = &format!("{:016x}", fnv1a64(&data))[..8];
    Some(format!("{stem}.{hash}.{ext}"))
}

/// `honey build [entry] [-o dir] [--production] [--minify] [--sourcemap ..]`:
/// bundle the module graph rooted at `entry` (default `src/index.ts`) into
/// one self-contained file under `out_dir` (default `dist/`). Return value
/// is the process exit code (0 = ok).
pub fn cmd_build(entry: Option<&str>, out_dir: Option<&str>, opts: &BuildOptions) -> Result<i32> {
    println!("honey build");

    let cwd = std::env::current_dir().context("failed to get current directory")?;

    let entry_input = entry.unwrap_or("src/index.ts");
    let entry_input_path = if Path::new(entry_input).is_absolute() {
        PathBuf::from(entry_input)
    } else {
        cwd.join(entry_input)
    };
    let entry_file =
        resolve_entry_input(entry.unwrap_or("src/index.ts"), &cwd)?;
    let entry_id = lexically_normalize_path(&entry_file).to_string_lossy().into_owned();

    // HTML entry: bundle every script src, copy stylesheets, rewrite refs.
    if entry_file.extension().map(|e| e == "html").unwrap_or(false) {
        let out_dir_abs = match out_dir {
            Some(dir) => {
                let p = Path::new(dir);
                let p = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
                if p.extension().map(|e| e == "js" || e == "mjs" || e == "cjs" || e == "html").unwrap_or(false) {
                    // file-shaped -o for an html entry: use its parent dir
                    p.parent().map(|p| p.to_path_buf()).unwrap_or(p)
                } else {
                    p
                }
            }
            None => cwd.join("dist"),
        };
        return build_html(&entry_file, &out_dir_abs, opts);
    }

    let out_path = match out_dir {
        Some(dir) => {
            let p = Path::new(dir);
            let p = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
            if p.extension().map(|e| e == "js" || e == "mjs" || e == "cjs").unwrap_or(false) {
                p
            } else {
                p.join(output_name(&entry_file))
            }
        }
        None => cwd.join("dist").join(output_name(&entry_file)),
    };

    execute_build(&entry_file, &out_path, opts, &mut Vec::new())?;
    Ok(0)
}

/// Resolve, bundle and emit a single JS entry to a concrete output file
/// path.  Shared by `cmd_build` (one-shot) and the watch loop (rebuilds;
/// also refreshes `watch_files` with this build's source set).
pub(crate) fn execute_build(
    entry_file: &Path,
    out_path: &Path,
    opts: &BuildOptions,
    watch_files: &mut Vec<PathBuf>,
) -> Result<()> {
    let entry_id = lexically_normalize_path(entry_file).to_string_lossy().into_owned();

    let (bundle, map, module_count, css_files, asset_files, sources) =
        bundle_graph(entry_file, opts)?;
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory: {}", parent.display()))?;
    }

    // Copy static assets next to the output under their content-hash names
    // (matching the URLs baked into the bundle; identical content reuses one
    // file, so no collision handling is needed).
    let out_dir_abs = out_path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
    for a in &asset_files {
        let src = Path::new(a);
        let name = asset_output_name(src)
            .ok_or_else(|| anyhow::anyhow!("cannot read asset: {}", src.display()))?;
        let dest = out_dir_abs.join(&name);
        std::fs::copy(src, &dest)
            .with_context(|| format!("failed to copy asset {} -> {}", src.display(), dest.display()))?;
    }

    // Emit the JS bundle (plus sourcemap per mode: linked appends a
    // sourceMappingURL comment, inline embeds a data URL, external writes
    // the bare map file with no comment).
    let minify = opts.minify_enabled();
    let sm_mode = opts.sourcemap_mode();
    let final_bundle: String = match sm_mode {
        SourceMapMode::None => bundle,
        SourceMapMode::External => bundle,
        SourceMapMode::Linked => {
            let map_file = out_path.with_file_name(format!(
                "{}.map",
                out_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
            ));
            let rel = map_file
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            format!("{bundle}\n//# sourceMappingURL={rel}\n")
        }
        SourceMapMode::Inline => match map.as_ref() {
            Some(m) => format!("{bundle}\n//# sourceMappingURL={}\n", m.to_data_url()),
            None => bundle,
        },
    };
    std::fs::write(&out_path, &final_bundle)
        .with_context(|| format!("failed to write {}", out_path.display()))?;

    // Write the map for Linked/External.
    if matches!(sm_mode, SourceMapMode::Linked | SourceMapMode::External) {
        if let Some(map) = &map {
            let map_file = out_path.with_file_name(format!(
                "{}.map",
                out_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
            ));
            let mut m = map.clone();
            m.set_file(
                out_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .as_str(),
            );
            std::fs::write(&map_file, m.to_json_string())
                .with_context(|| format!("failed to write {}", map_file.display()))?;
        }
    }

    // Stylesheet bundle: one file next to the JS output, same stem, imports
    // concatenated in first-import (discovery) order, deduped. Minified in
    // production (lightweight strip-minify; no oxc CSS crate in-tree).
    let mut css_path = None;
    if !css_files.is_empty() {
        let stem = out_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let p = out_path.with_file_name(format!("{stem}.css"));
        let mut css_out = String::new();
        if minify {
            for f in &css_files {
                let content = std::fs::read_to_string(f)
                    .with_context(|| format!("failed to read stylesheet: {f}"))?;
                css_out.push_str(&minify_css(&content));
                css_out.push('\n');
            }
        } else {
            for f in &css_files {
                let content = std::fs::read_to_string(f)
                    .with_context(|| format!("failed to read stylesheet: {f}"))?;
                css_out.push_str(&content);
                if !content.ends_with('\n') {
                    css_out.push('\n');
                }
                css_out.push('\n');
            }
        }
        std::fs::write(&p, css_out)
            .with_context(|| format!("failed to write {}", p.display()))?;
        css_path = Some(p);
    }

    println!("  entry: {entry_id}");
    println!("  modules: {module_count}");
    if let Some(p) = &css_path {
        println!("  css: {} ({} file{})", p.display(), css_files.len(), if css_files.len() == 1 { "" } else { "s" });
    }
    if !asset_files.is_empty() {
        println!("  assets: {} copied", asset_files.len());
    }
    let mut flags: Vec<&str> = Vec::new();
    if minify {
        flags.push("minify");
    }
    if opts.production {
        flags.push("production");
    }
    match opts.target() {
        BuildTarget::Browser => flags.push("target=browser"),
        BuildTarget::Node => flags.push("target=node"),
    }
    if !matches!(sm_mode, SourceMapMode::None) {
        flags.push(match sm_mode {
            SourceMapMode::External => "sourcemap=external",
            SourceMapMode::Linked => "sourcemap=linked",
            SourceMapMode::Inline => "sourcemap=inline",
            SourceMapMode::None => unreachable!(),
        });
    }
    if !flags.is_empty() {
        println!("  flags: {}", flags.join(", "));
    }
    println!("  output: {}", out_path.display());
    watch_files.clear();
    for s in sources {
        watch_files.push(PathBuf::from(s));
    }
    Ok(())
}

/// Entry module id -> default output file name.
pub(crate) fn output_name(entry: &Path) -> String {
    let stem = entry.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    format!("{stem}.js")
}

/// Settled fingerprint of a file for stat-polling (mtime nanos + size; size
/// catches same-second rewrites that coarse mtimes miss).
pub(crate) fn stat_stamp(path: &Path) -> Option<(u128, u64)> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md.modified().ok()?;
    let nanos = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((nanos, md.len()))
}

/// `honey build --watch`: build once, then stat-poll every source file of the
/// last build (modules + extracted css + copied assets) and rebuild on any
/// change.  Build errors are printed and swallowed (the watcher keeps
/// running; the next save retries).  Ctrl-C ends the loop.  Only files that
/// were part of the last successful build are polled — a brand-new untouched
/// file is picked up when a tracked file that imports it changes.
pub fn cmd_build_watch(entry: Option<&str>, out_dir: Option<&str>, opts: &BuildOptions) -> Result<i32> {
    println!("honey build --watch");

    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let entry_input = entry.unwrap_or("src/index.ts");
    let resolved_err = match resolve_entry_input(entry_input, &cwd) {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    let entry_file = resolved_err; // already extension-resolved

    let out_dir_abs = match out_dir {
        Some(dir) => {
            let p = Path::new(dir);
            let p = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
            // A file-shaped -o is unusual for a directory-of-outputs watch;
            // treat it as its parent directory.
            if p.extension().map(|e| e == "js" || e == "mjs" || e == "cjs").unwrap_or(false) {
                p.parent().map(|p| p.to_path_buf()).unwrap_or(p)
            } else {
                p
            }
        }
        None => cwd.join("dist"),
    };
    std::fs::create_dir_all(&out_dir_abs)
        .with_context(|| format!("failed to create output directory: {}", out_dir_abs.display()))?;

    let out_path = out_dir_abs.join(output_name(&entry_file));
    let html_mode = entry_file.extension().map(|e| e == "html").unwrap_or(false);

    // Settled watch set: what the last build read, plus the html file in
    // html-entry mode (scripts inside are re-discovered on the html change).
    let mut watch_files: Vec<PathBuf> = vec![entry_file.clone()];
    let mut stamps: HashMap<PathBuf, (u128, u64)> = HashMap::new();
    let refresh_stamps = |watch_files: &[PathBuf], stamps: &mut HashMap<PathBuf, (u128, u64)>| {
        stamps.clear();
        for f in watch_files {
            if let Some(s) = stat_stamp(f) {
                stamps.insert(f.clone(), s);
            }
        }
    };
    refresh_stamps(&watch_files, &mut stamps);

    println!("  entry: {}", entry_file.display());
    println!("  output dir: {}", out_dir_abs.display());
    println!("  watching {} file(s) — Ctrl-C to stop", watch_files.len());

    // First build.
    if html_mode {
        if let Err(e) = build_html(&entry_file, &out_dir_abs, opts) {
            eprintln!("error: {e:#}");
            println!("[watch] build failed; waiting for changes");
        }
    } else if let Err(e) = execute_build(&entry_file, &out_path, opts, &mut watch_files) {
        eprintln!("error: {e:#}");
        println!("[watch] build failed; waiting for changes");
        // execute_build still fills watch_files on graph-success paths; on
        // total failure keep the entry alone in the set.
        if watch_files.is_empty() {
            watch_files.push(entry_file.clone());
        }
    }
    refresh_stamps(&watch_files, &mut stamps);

    loop {
        // 10ms polling keeps edit->rebuild latency near the build cost while
        // staying cheap: one stat per source file per tick (a 100-module
        // project is ~10k stats/s, far below a core).
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut changed: Vec<PathBuf> = Vec::new();
        for f in &watch_files {
            match stat_stamp(f) {
                Some(s) => {
                    if stamps.get(f).map(|old| old != &s).unwrap_or(true) {
                        changed.push(f.clone());
                    }
                }
                None => {
                    // File deleted: rebuild (the build will report the error).
                    changed.push(f.clone());
                }
            }
        }
        if changed.is_empty() {
            continue;
        }
        let names: Vec<String> =
            changed.iter().map(|c| c.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| c.display().to_string())).collect();
        println!("\n[watch] change in {}", names.join(", "));
        for f in changed {
            if let Some(s) = stat_stamp(&f) {
                stamps.insert(f, s);
            }
        }
        if html_mode {
            if let Err(e) = build_html(&entry_file, &out_dir_abs, opts) {
                eprintln!("error: {e:#}");
                println!("[watch] build failed; waiting for changes");
            }
        } else if let Err(e) = execute_build(&entry_file, &out_path, opts, &mut watch_files) {
            eprintln!("error: {e:#}");
            println!("[watch] build failed; waiting for changes");
        }
        refresh_stamps(&watch_files, &mut stamps);
    }
}

/// Entry resolution shared by cmd_build and cmd_build_watch: expand relative
/// paths, try explicit file, then conventional default entries.
pub(crate) fn resolve_entry_input(entry_input: &str, cwd: &Path) -> Result<PathBuf> {
    let entry_input_path = if Path::new(entry_input).is_absolute() {
        PathBuf::from(entry_input)
    } else {
        cwd.join(entry_input)
    };
    resolve_with_extensions(&entry_input_path)
        .or_else(|_| {
            const DEFAULTS: [&str; 5] =
                ["src/index.js", "src/index.jsx", "src/index.mjs", "src/index.mts", "index.js"];
            DEFAULTS
                .iter()
                .filter_map(|c| {
                    let p = cwd.join(c);
                    if p.is_file() {
                        Some(Ok(p))
                    } else {
                        None
                    }
                })
                .next()
                .unwrap_or_else(|| {
                    Err(anyhow::anyhow!("module not found: {}", entry_input_path.display()))
                })
        })
        .with_context(|| format!("cannot find build entry: {entry_input}"))
}

// ---------------------------------------------------------------------------
// HTML entry
// ---------------------------------------------------------------------------

/// One located attribute inside the html text: value span (byte offsets of
/// the quoted contents) and the value itself.
struct HtmlAttr {
    value_start: usize,
    value_end: usize,
    value: String,
}

/// Find attribute `name` inside a single tag's source text (region `from` to
/// `to` of `html`).  Only supports plainly quoted values ("...").
fn find_html_attr(html: &str, from: usize, to: usize, name: &str) -> Option<HtmlAttr> {
    let hay = &html.as_bytes()[from..to];
    let pat = format!("{name}=");
    let pos = hay.windows(pat.len()).position(|w| w == pat.as_bytes())?;
    let after = &hay[pos + pat.len()..];
    let q = match after.first() {
        Some(b'"') | Some(b'\'') => *after.first().unwrap(),
        _ => return None,
    };
    let after = &after[1..];
    let end = after.iter().position(|&b| b == q)?;
    let vstart = from + pos + pat.len() + 1;
    Some(HtmlAttr {
        value_start: vstart,
        value_end: vstart + end,
        value: String::from_utf8_lossy(&after[..end]).into_owned(),
    })
}

/// Is this html reference external (not to be bundled/copied)?
fn external_ref(value: &str) -> bool {
    value.starts_with("http:")
        || value.starts_with("https:")
        || value.starts_with("//")
        || value.starts_with("data:")
        || value.starts_with('#')
        || value.starts_with('/')
        || value.is_empty()
}

/// `honey build index.html`: bundle each relative `<script src>` like a JS
/// entry (named after its stem), copy each relative
/// `<link rel="stylesheet" href>`, rewrite both refs to the emitted files,
/// and write the html into the output directory.
pub(crate) fn build_html(html_file: &Path, out_dir: &Path, opts: &BuildOptions) -> Result<i32> {
    let html = std::fs::read_to_string(html_file)
        .with_context(|| format!("failed to read html entry: {}", html_file.display()))?;
    let dir = html_file.parent().unwrap_or(Path::new(".")).to_path_buf();
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("failed to create output directory: {}", out_dir.display()))?;

    let mut replacements: Vec<(usize, usize, String)> = Vec::new();

    // Lowercase scan for tags: iterate occurrences of "<script" and "<link "
    // (case-insensitive by manual check), attribute region = up to the '>'.
    let lowered = html.to_ascii_lowercase();

    // Collect all tag attribute regions for both tag types first.
    let mut script_tags: Vec<(usize, usize)> = Vec::new(); // attr region spans
    let mut link_tags: Vec<(usize, usize)> = Vec::new();
    {
        let mut i = 0;
        let lb = lowered.as_bytes();
        while i < lb.len() {
            if lb[i] == b'<' {
                let rest = &lowered[i..];
                let tag_kind = if rest.starts_with("<script") {
                    Some("script")
                } else if rest.starts_with("<link") {
                    Some("link")
                } else {
                    None
                };
                if let Some(kind) = tag_kind {
                    // attr region ends at the first '>' (no '>' inside attrs
                    // in well-formed tags; good enough for v1).
                    if let Some(close_off) = rest.find('>') {
                        let region = (i, i + close_off);
                        if kind == "script" {
                            script_tags.push(region);
                        } else {
                            link_tags.push(region);
                        }
                    }
                }
            }
            i += 1;
        }
    }

    let mut scripts_built = 0usize;
    let mut css_copied = 0usize;

    for (from, to) in &script_tags {
        let Some(src) = find_html_attr(&html, *from, *to, "src") else { continue };
        if external_ref(&src.value) {
            continue;
        }
        let resolved =
            resolve_with_extensions(&dir.join(&src.value)).with_context(|| {
                format!("cannot resolve html script src: {} (in {})", src.value, html_file.display())
            })?;
        let stem = resolved
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "entry".to_string());
        let out_js = out_dir.join(format!("{stem}.js"));
        execute_build(&resolved, &out_js, opts, &mut Vec::new())?;
        scripts_built += 1;
        replacements.push((src.value_start, src.value_end, stem + ".js"));
    }

    for (from, to) in &link_tags {
        // rel="stylesheet" only.
        let Some(rel) = find_html_attr(&html, *from, *to, "rel") else { continue };
        if rel.value.to_ascii_lowercase() != "stylesheet" {
            continue;
        }
        let Some(href) = find_html_attr(&html, *from, *to, "href") else { continue };
        if external_ref(&href.value) {
            continue;
        }
        let src_path = dir.join(&href.value);
        if !src_path.is_file() {
            bail!(
                "html stylesheet not found: {} (in {})",
                src_path.display(),
                html_file.display()
            );
        }
        let file_name = src_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "style.css".to_string());
        std::fs::copy(&src_path, out_dir.join(&file_name)).with_context(|| {
            format!("failed to copy stylesheet {} -> {}", src_path.display(), out_dir.display())
        })?;
        css_copied += 1;
        replacements.push((href.value_start, href.value_end, file_name));
    }

    // Apply rewrites from the END so byte offsets stay valid.
    replacements.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out_html = html;
    for (s, e, v) in replacements {
        out_html.replace_range(s..e, &v);
    }

    let out_file = out_dir.join(
        html_file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "index.html".to_string()),
    );
    std::fs::write(&out_file, out_html)
        .with_context(|| format!("failed to write {}", out_file.display()))?;

    println!("  entry: {}", html_file.display());
    println!("  scripts: {scripts_built}, stylesheets: {css_copied}");
    println!("  output: {}", out_file.display());
    Ok(0)
}

// ---------------------------------------------------------------------------
// Module graph
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModuleKind {
    Esm,
    Cjs,
    Json,
}

#[derive(Clone)]
struct Module {
    dir: String,
    code: String,
    strict: bool,
    /// Line offset of this module's code within the bundle (filled at
    /// emission; used for sourcemap token shifting).
    bundle_line_offset: usize,
    /// Sourcemap produced by TS transpilation of this module, if any.
    source_map: Option<oxc_sourcemap::SourceMap>,
}

// ---------------------------------------------------------------------------
// Module compile cache (build mode)
// ---------------------------------------------------------------------------

/// One cached processed module: the transformed + scan-rewritten code, the
/// module's transpile sourcemap, and its resolved inline dependencies.
#[derive(Clone)]
struct CachedBuildModule {
    mtime: (u128, u64),
    opts_fp: String,
    module: Module,
    deps: Vec<String>,
}

thread_local! {
    /// Per-process (i.e., per-daemon) build cache. Keyed by absolute module
    /// path; entries carry the file's (mtime nanos, size) stamp and the
    /// options fingerprint, so an edited file or changed flags re-compiles.
    /// process_module results are deterministic for a given
    /// (input, opts, path), so reuse is safe.
    static BUILD_MODULE_CACHE: std::cell::RefCell<HashMap<String, CachedBuildModule>> =
        std::cell::RefCell::new(HashMap::new());
}

fn build_cache_lookup(path: &str, stamp: (u128, u64), fp: &str) -> Option<(Module, Vec<String>)> {
    BUILD_MODULE_CACHE.with(|c| {
        let map = c.borrow();
        let entry = map.get(path);
        match entry {
            Some(m) if m.mtime == stamp && m.opts_fp == fp => Some((m.module.clone(), m.deps.clone())),
            _ => None,
        }
    })
}

fn build_cache_store(path: String, stamp: (u128, u64), fp: String, module: Module, deps: Vec<String>) {
    BUILD_MODULE_CACHE.with(|c| {
        let mut map = c.borrow_mut();
        if map.len() > 4096 {
            map.clear(); // simple pressure valve; most projects stay well below
        }
        map.insert(path, CachedBuildModule { mtime: stamp, opts_fp: fp, module, deps });
    });
}

/// Deterministic fingerprint of the build options that affect module
/// compilation (emission-only flags like the target banner also affect the
/// emitted TEXT of a module through require_expr? — no: require_expr output
/// is identical for both targets, so the fingerprint only covers the
/// per-module transform: minify/production/sourcemap/defines).
fn module_opts_fingerprint(opts: &BuildOptions) -> String {
    let mut defines: Vec<&(String, String)> = opts.define.iter().collect();
    defines.sort();
    format!(
        "m{:?}p{}s{:?}|{}",
        opts.minify,
        opts.production,
        opts.sourcemap,
        defines.iter().map(|(k, v)| format!("{k}={v};")).collect::<String>()
    )
}

struct Bundler {
    /// Discovery order (entry first) used for emission.
    order: Vec<String>,
    modules: HashMap<String, Module>,
    /// CSS files extracted from the graph, in first-import order (deduped).
    css: Vec<String>,
    /// Static assets from the graph, in first-import order (deduped).
    assets: Vec<String>,
    /// Counter for fresh `__honey_m<n>`/`__honey_r<n>` temporaries.
    temp: usize,
}

type GraphResult = (
    String,
    Option<oxc_sourcemap::SourceMap>,
    usize,
    Vec<String>,
    Vec<String>,
    Vec<String>, // every source file that went into the graph (for the watcher)
);

// ---------------------------------------------------------------------------
// Profiling (HONEY_BUILD_PROFILE=1 prints per-stage timings on stderr)
// ---------------------------------------------------------------------------

fn profile_active() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("HONEY_BUILD_PROFILE").map(|v| !v.is_empty() && v != "0").unwrap_or(false))
}

struct ProfileStage(std::time::Instant, &'static str);
impl ProfileStage {
    fn new(name: &'static str) -> Self {
        ProfileStage(std::time::Instant::now(), name)
    }
}
impl Drop for ProfileStage {
    fn drop(&mut self) {
        if profile_active() {
            eprintln!("[profile] {:<26} {:>8.2?} ms", self.1, self.0.elapsed().as_secs_f64() * 1000.0);
        }
    }
}

fn bundle_graph(entry_file: &Path, opts: &BuildOptions) -> Result<GraphResult> {
    let entry_id = lexically_normalize_path(entry_file).to_string_lossy().into_owned();
    let mut bundler = Bundler { order: Vec::new(), modules: HashMap::new(), css: Vec::new(), assets: Vec::new(), temp: 0 };

    let mut queue: VecDeque<String> = VecDeque::new();
    queue.push_back(entry_id.clone());
    let _p_walk = ProfileStage::new("graph: module walk");
    while let Some(id) = queue.pop_front() {
        if bundler.modules.contains_key(&id) || bundler.css.iter().any(|c| c == &id) || bundler.assets.iter().any(|c| c == &id) {
            continue;
        }
        // CSS ids never enter the JS module graph: collect them for the
        // stylesheet bundle in first-import order.
        if id.ends_with(".css") {
            bundler.css.push(id);
            continue;
        }
        // Static assets likewise: collected for copying to the output dir.
        if Path::new(&id)
            .extension()
            .and_then(|e| e.to_str())
            .map(is_static_asset)
            .unwrap_or(false)
        {
            bundler.assets.push(id);
            continue;
        }
        let (module, inline_deps) = process_module(Path::new(&id), &mut bundler.temp, opts)?;
        bundler.modules.insert(id.clone(), module);
        bundler.order.push(id);
        for dep in inline_deps {
            if !bundler.modules.contains_key(&dep) {
                queue.push_back(dep);
            }
        }
    }

    drop(_p_walk);
    let css = bundler.css.clone();
    let assets = bundler.assets.clone();
    let _p_emit = ProfileStage::new("emit_bundle (concat+map)");
    let (bundle, map) = emit_bundle(&mut bundler, &entry_id, opts);
    drop(_p_emit);
    let mut sources = bundler.order.clone();
    sources.extend(css.iter().cloned());
    sources.extend(assets.iter().cloned());
    Ok((bundle, map, bundler.modules.len(), css, assets, sources))
}

fn process_module(id: &Path, temp: &mut usize, opts: &BuildOptions) -> Result<(Module, Vec<String>)> {
    let id_str = id.to_string_lossy().into_owned();
    let fp = module_opts_fingerprint(opts);
    // Warm path: an unchanged module compiled with the same options is served
    // from the per-daemon build cache — repeated builds re-transpile nothing.
    if let Some(stamp) = stat_stamp(id) {
        if let Some((module, deps)) = build_cache_lookup(&id_str, stamp, &fp) {
            return Ok((module, deps));
        }
    }

    let source =
        std::fs::read_to_string(id).with_context(|| format!("failed to read module: {id_str}"))?;

    let mut source_map = None;
    let is_ts = is_typescript_path(&id_str);
    let needs_compile = opts.minify_enabled() || !opts.defines().is_empty();
    let _p_transform = ProfileStage::new(if is_ts { "transform_typescript" } else if needs_compile { "compile_js" } else { "passthrough (no compile)" });
    let text = if is_ts {
        let (t, m) = transform_typescript(&source, id, opts)?;
        source_map = m;
        t
    } else if needs_compile {
        let (t, m) = compile_js(&source, id, opts)?;
        source_map = m;
        t
    } else {
        source
    };
    drop(_p_transform);

    if id.extension().map(|e| e == "json").unwrap_or(false) {
        let dir = module_dir(id);
        return Ok((
            Module { dir, code: format!("module.exports = {text};"), strict: false, bundle_line_offset: 0, source_map: None },
            Vec::new(),
        ));
    }

    if detect_esm(id, &text) {
        let _p = ProfileStage::new("process_esm (scan)");
        let (mut m, deps) = process_esm(&text, id, temp)?;
        m.source_map = source_map;
        if let Some(stamp) = stat_stamp(id) {
            build_cache_store(id_str, stamp, fp, m.clone(), deps.clone());
        }
        Ok((m, deps))
    } else {
        let _p = ProfileStage::new("process_cjs (scan)");
        let (mut m, deps) = process_cjs(&text, id, temp)?;
        m.source_map = source_map;
        if let Some(stamp) = stat_stamp(id) {
            build_cache_store(id_str, stamp, fp, m.clone(), deps.clone());
        }
        Ok((m, deps))
    }
}

/// ESM/CJS classification for the emitted registry, mirroring the runtime's
/// main-file/require-path detection: `.mjs`, nearest package.json
/// "type": "module", or ESM syntax in the (transpiled) source.
fn detect_esm(path: &Path, text: &str) -> bool {
    if path.extension().map(|e| e == "mjs").unwrap_or(false) {
        return true;
    }
    if nearest_package_json_is_module(path) {
        return true;
    }
    looks_like_esm(text)
}

// ---------------------------------------------------------------------------
// Specifier resolution (mirrors the runtime's conventions)
// ---------------------------------------------------------------------------

enum Resolved {
    /// Relative/absolute specifier resolved to a file to inline.
    Inline(PathBuf),
    /// CSS stylesheets: extracted to a sibling `.css` output file instead of
    /// entering the JS module graph (a browser app's `import './x.css'`).
    Css(PathBuf),
    /// Static asset (images, fonts, wasm, …): copied to the output directory,
    /// the import site becomes the URL string.
    Asset(PathBuf),
    /// Bare specifier, `node:` builtin or URL: stays external in v1; the
    /// emitted file keeps the literal specifier for the runtime to resolve.
    External,
}

/// Resolve one literal specifier. `prefer_import` selects the "import"
/// condition over "require" for package.json `exports` (ESM import sites vs
/// `require()`), like the runtime does.
fn resolve_specifier(
    spec: &str,
    from_dir: &Path,
    ts_referrer: bool,
    prefer_import: bool,
) -> Result<Resolved> {
    if spec.starts_with('.') || spec.starts_with('/') {
        if spec.ends_with(".css") {
            let path = if Path::new(spec).is_absolute() {
                PathBuf::from(spec)
            } else {
                from_dir.join(spec)
            };
            let resolved = resolve_with_extensions(&path)
                .with_context(|| format!("cannot resolve import '{spec}'"))?;
            return Ok(Resolved::Css(resolved));
        }
        // Static assets: an explicit asset extension must not go through
        // extension probing (e.g. `fonts/hero.woff2` staying `.woff2`).
        let raw = if Path::new(spec).is_absolute() {
            PathBuf::from(spec)
        } else {
            from_dir.join(spec)
        };
        if let Some(ext) = raw.extension().and_then(|e| e.to_str()) {
            if is_static_asset(ext) {
                if !raw.is_file() {
                    bail!("cannot resolve import '{spec}': asset file not found: {}", raw.display());
                }
                return Ok(Resolved::Asset(raw));
            }
        }
        let file = if prefer_import {
            resolve_relative_esm(spec, from_dir, ts_referrer)?
        } else {
            resolve_relative_cjs(spec, from_dir)?
        };
        return Ok(Resolved::Inline(file));
    }
    // Bare specifiers: inline npm packages resolvable from disk (honey's
    // `.cinm` store or a standard npm `node_modules` tree) so the emitted
    // bundle is self-contained in the browser. `node:` builtins and URL
    // specifiers stay external for the runtime resolver.
    if !spec.starts_with("node:") && !spec.contains("://") {
        if let Some(file) = resolve_bare(spec, from_dir, prefer_import) {
            return Ok(Resolved::Inline(file));
        }
    }
    Ok(Resolved::External)
}

/// Bare-specifier resolution for build-time inlining, mirroring the runtime
/// resolver: per ancestor directory (nearest first), honey's `.cinm` store
/// (versioned `pkg@<ver>` layout, scoped `/` encoded as `+`) is scanned
/// before a standard `node_modules/<pkg>` (npm layout); the package entry is
/// resolved via package.json `exports` (import/require condition), else
/// `main`, else `index.*`.
fn resolve_bare(spec: &str, from_dir: &Path, prefer_import: bool) -> Option<PathBuf> {
    let (mut pkg_name, subpath_guess) = {
        let mut it = spec.splitn(2, '/');
        (it.next()?.to_string(), it.next())
    };
    let mut subpath = subpath_guess;
    if pkg_name.starts_with('@') {
        // "@scope/pkg/…": the package dir is "@scope/pkg", the rest is subpath
        let sub = subpath?;
        let (pkg_base, real_sub) = match sub.split_once('/') {
            Some((base, rest)) => (base, Some(rest)),
            None => (sub, None),
        };
        pkg_name = format!("{pkg_name}/{pkg_base}");
        subpath = real_sub;
    }

    let mut dir = from_dir.to_path_buf();
    loop {
        for store in [".cinm", "node_modules"] {
            let root = dir.join(store);
            if !root.is_dir() {
                continue;
            }
            // honey install layout: `<pkg>@<version>` dirs, scoped `/` -> `+`
            let versioned_prefix = format!("{}@", pkg_name.replace('/', "+"));
            if let Some(pkg_dir) = std::fs::read_dir(&root).ok().and_then(|mut entries| {
                entries.find_map(|entry| {
                    let entry = entry.ok()?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with(&versioned_prefix) {
                        Some(entry.path())
                    } else {
                        None
                    }
                })
            }) {
                if let Some(p) = package_entry(&pkg_dir, subpath, prefer_import) {
                    return Some(p);
                }
            }
            // standard npm layout: the literal package directory
            let std_dir = root.join(&pkg_name);
            if std_dir.is_dir() {
                if let Some(p) = package_entry(&std_dir, subpath, prefer_import) {
                    return Some(p);
                }
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Package entry file: package.json `exports` map (preferring the import or
/// require condition per `prefer_import`, falling back to `default`), else
/// `main`, else `index.js`.
fn package_entry(pkg_dir: &Path, subpath: Option<&str>, prefer_import: bool) -> Option<PathBuf> {
    let subpath = subpath.unwrap_or("");
    let package_json = pkg_dir.join("package.json");
    let mut entry = None;
    if let Ok(content) = std::fs::read_to_string(&package_json) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(exports) = json.get("exports") {
                let key = if subpath.is_empty() { ".".to_string() } else { format!("./{subpath}") };
                entry = exports_entry(exports, &key, prefer_import);
            }
            if entry.is_none() && subpath.is_empty() {
                if let Some(m) = json.get("main").and_then(|v| v.as_str()) {
                    entry = Some(m.to_string());
                }
            }
        }
    }
    let rel = entry
        .unwrap_or_else(|| if subpath.is_empty() { "index.js".to_string() } else { subpath.to_string() });
    let p = pkg_dir.join(rel.trim_start_matches("./"));
    resolve_with_extensions(&p).ok()
}

fn exports_entry(exports: &serde_json::Value, key: &str, prefer_import: bool) -> Option<String> {
    if exports.is_string() {
        if key == "." {
            return Some(exports.as_str().unwrap_or_default().to_string());
        }
        return None;
    }
    let obj = exports.as_object()?;
    if let Some(target) = obj.get(key) {
        return pick_export_target(target, prefer_import);
    }
    if key == "." {
        return pick_export_target(exports, prefer_import);
    }
    let alt = key.trim_start_matches("./");
    if obj.contains_key(alt) {
        return pick_export_target(&obj[alt], prefer_import);
    }
    None
}

fn pick_export_target(target: &serde_json::Value, prefer_import: bool) -> Option<String> {
    if target.is_string() {
        return Some(target.as_str().unwrap_or_default().to_string());
    }
    let obj = target.as_object()?;
    let conditions: [&str; 2] = if prefer_import {
        ["import", "default"]
    } else {
        ["require", "default"]
    };
    for cond in conditions {
        if let Some(t) = obj.get(cond) {
            let picked = pick_export_target(t, prefer_import);
            if picked.is_some() {
                return picked;
            }
        }
    }
    None
}

/// require()-style relative/absolute resolution = the runtime's
/// `resolve_with_extensions`: exact file, then extension probing, then
/// directory `index.*` fallbacks.
fn resolve_relative_cjs(spec: &str, from_dir: &Path) -> Result<PathBuf> {
    let path = if Path::new(spec).is_absolute() {
        PathBuf::from(spec)
    } else {
        from_dir.join(spec)
    };
    resolve_with_extensions(&path)
}

/// ESM import resolution = the runtime's HoneyModuleLoader path: the literal
/// path joined against the referrer's directory; when it does not exist and
/// the referrer is TypeScript, the TS-style fallback candidates are tried
/// (`.js` -> `.ts` sibling, `+.ts`, `+/index.ts`; plus `.tsx` equivalents).
fn resolve_relative_esm(spec: &str, from_dir: &Path, ts_referrer: bool) -> Result<PathBuf> {
    let resolved = if Path::new(spec).is_absolute() {
        PathBuf::from(spec)
    } else {
        from_dir.join(spec)
    };
    if resolved.exists() {
        return Ok(lexically_normalize_path(&resolved));
    }

    if ts_referrer {
        let mut candidates: Vec<String> = Vec::new();
        if let Some(ts_style) = spec.strip_suffix(".js") {
            candidates.push(format!("{ts_style}.ts"));
            candidates.push(format!("{ts_style}.tsx"));
        }
        candidates.push(format!("{spec}.ts"));
        candidates.push(format!("{spec}.tsx"));
        let trimmed = spec.trim_end_matches('/');
        candidates.push(format!("{trimmed}/index.ts"));
        candidates.push(format!("{trimmed}/index.tsx"));
        for cand in candidates {
            let candidate = if Path::new(&cand).is_absolute() {
                PathBuf::from(&cand)
            } else {
                from_dir.join(&cand)
            };
            if candidate.exists() {
                return Ok(lexically_normalize_path(&candidate));
            }
        }
    }

    bail!("module not found: {}", resolved.display())
}

/// Copy of the runtime's `resolve_with_extensions` (same order of extensions
/// and index fallbacks so bundling resolves exactly like a direct run).
fn resolve_with_extensions(path: &Path) -> Result<PathBuf> {
    if path.is_dir() {
        for index in
            ["index.js", "index.mjs", "index.cjs", "index.json", "index.ts", "index.mts", "index.cts"]
        {
            let candidate = path.join(index);
            if candidate.exists() {
                return Ok(lexically_normalize_path(&candidate));
            }
        }
        bail!("cannot resolve directory: {}", path.display());
    }

    if path.exists() {
        return Ok(lexically_normalize_path(path));
    }

    for ext in ["js", "mjs", "cjs", "json", "ts", "mts", "cts"] {
        let candidate = PathBuf::from(format!("{}.{}", path.display(), ext));
        if candidate.exists() {
            return Ok(lexically_normalize_path(&candidate));
        }
    }

    bail!("module not found: {}", path.display())
}

/// Lexically resolve `.` and `..` components. Keeps the path absolute if it
/// already is; otherwise normalizes the relative form. (Same as the runtime.)
fn lexically_normalize_path(p: &Path) -> PathBuf {
    let absolute = p.is_absolute();
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !parts.is_empty() {
                    parts.pop();
                } else if !absolute {
                    parts.push(std::ffi::OsString::from(".."));
                }
            }
            other => parts.push(other.as_os_str().to_os_string()),
        }
    }
    let mut out = PathBuf::new();
    if absolute {
        out.push("/");
    }
    for part in parts {
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

fn module_dir(id: &Path) -> String {
    id.parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Walk up from the file's directory looking for the nearest package.json and
/// report whether it declares `"type": "module"` (default CommonJS).
/// (Same as the runtime's `nearest_package_json_is_module`.)
fn nearest_package_json_is_module(from: &Path) -> bool {
    let mut dir = from.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    loop {
        let pkg = dir.join("package.json");
        if pkg.exists() {
            if let Ok(content) = std::fs::read_to_string(&pkg) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                    return json
                        .get("type")
                        .and_then(|v| v.as_str())
                        .map(|t| t == "module")
                        .unwrap_or(false);
                }
            }
            return false;
        }
        if !dir.pop() {
            return false;
        }
    }
}

/// Conservative ESM sniff, same as the runtime.
fn looks_like_esm(source: &str) -> bool {
    for line in source.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
            continue;
        }
        if t.starts_with("export ") || t.starts_with("export{") || t.starts_with("export *") || t.starts_with("export*") {
            return true;
        }
        // static import declarations; `import(...)` dynamic calls stay CJS-legal
        if (t.starts_with("import ") || t.starts_with("import{") || t.starts_with("import *") || t.starts_with("import*"))
            && !t.starts_with("import(") {
            return true;
        }
    }
    false
}

/// TS entry points honey transpiles: .ts, .mts, .cts. (Same as the runtime.)
fn is_typescript_path(path: &str) -> bool {
    // Includes the JSX flavors: the runtime treats .tsx/.jsx sources as
    // transpilable TS/JS-with-JSX (transform_typescript keys the JSX settings
    // off the path), and the bundler must transpile them the same way.
    path.ends_with(".ts")
        || path.ends_with(".mts")
        || path.ends_with(".cts")
        || path.ends_with(".tsx")
        || path.ends_with(".jsx")
}

// ---------------------------------------------------------------------------
// Module scanning: module record + require/dynamic-import collection
// ---------------------------------------------------------------------------

/// One `import` statement in scanned (syntactic) form.
struct ImportData {
    statement_span: oxc_span::Span,
    source: String,
    side_effect: bool,
    default: Option<String>,
    namespace: Option<String>,
    named: Vec<(String, String)>,
}

impl ImportData {
    fn new(span: oxc_span::Span, source: String) -> Self {
        Self {
            statement_span: span,
            source,
            side_effect: false,
            default: None,
            namespace: None,
            named: Vec::new(),
        }
    }
}

/// Everything the rewriter needs about one ESM module. Spans index into the
/// parsed (transpiled) text; the analysis is derived from oxc's module record
/// and a semantic node pass, reached only through `AstKind` accessors so no
/// type from the (non-dependency) `oxc_ast` crate is named here.
#[derive(Default)]
struct ScannedModule {
    imports: Vec<ImportData>,
    /// Import specifiers keyed by their import statement's span start
    /// (working map, drained by `scan_esm` before returning).
    import_specifiers: HashMap<u32, Vec<SpecifierData>>,
    /// `require('<literal>')` call spans and their specifier values.
    requires: Vec<(oxc_span::Span, String)>,
    /// `import(...)` spans with the string value when the argument is literal.
    dynamic: Vec<(oxc_span::Span, Option<String>)>,
    /// `export <decl>` statements: (statement span, declaration span, bound
    /// names `(local, exported)` from the module record).
    export_decls: Vec<(oxc_span::Span, oxc_span::Span, Vec<(String, String)>)>,
    /// `export { ... } [from 'm']` with non-empty specifiers: (statement
    /// span, `(remote, exported)` or `(local, exported)` pairs, source).
    export_named: Vec<(oxc_span::Span, Vec<(String, String)>, Option<String>)>,
    /// `export * [as ns] from 'm'`: (statement span, source, ns name).
    export_stars: Vec<(oxc_span::Span, String, Option<String>)>,
    /// `export default` keyed by statement span start ->
    /// (child declaration span, bound name for a named function/class child).
    default_children: HashMap<u32, (oxc_span::Span, Option<String>)>,
}

/// One import specifier (kind + names), scanned by node kind.
enum SpecifierData {
    Default(String),
    Namespace(String),
    /// (imported name, local name, type-only)
    Named(String, String, bool),
}

/// Collect literal `require()` calls and `import()` expressions from the
/// semantic node graph. `import()`'s source is a direct child (literal
/// detected via parent pairing); a `require()` argument sits under an
/// `Argument` node, so its literal is matched by span containment — safe
/// because `is_require_call` guarantees a string-literal first argument.
fn scan_calls(semantic: &oxc_semantic::Semantic<'_>) -> (Vec<(oxc_span::Span, String)>, Vec<(oxc_span::Span, Option<String>)>) {
    use oxc_span::GetSpan as _;

    let nodes = semantic.nodes();
    let mut literal_by_parent: HashMap<u32, String> = HashMap::new();
    let mut literals: Vec<(oxc_span::Span, String)> = Vec::new();
    let mut require_spans: Vec<oxc_span::Span> = Vec::new();
    let mut import_spans: Vec<oxc_span::Span> = Vec::new();

    for node in nodes.iter() {
        let kind = node.kind();
        let Some(lit) = kind.as_string_literal() else { continue };
        literals.push((node.span(), lit.value.to_string()));
        if let Some(parent) = nodes.parent_node(node.id()) {
            literal_by_parent
                .entry(parent.span().start)
                .or_insert_with(|| lit.value.to_string());
        }
    }
    literals.sort_by_key(|(span, _)| span.start);

    for node in nodes.iter() {
        let kind = node.kind();
        if let Some(call) = kind.as_call_expression() {
            if call.is_require_call() {
                require_spans.push(node.span());
            }
        } else if kind.as_import_expression().is_some() {
            import_spans.push(node.span());
        }
    }

    let requires = require_spans
        .into_iter()
        .filter_map(|span| {
            let spec = literals
                .iter()
                .find(|(lit, _)| lit.start > span.start && lit.end < span.end)
                .map(|(_, v)| v.clone());
            spec.map(|spec| (span, spec))
        })
        .collect();
    let dynamic = import_spans
        .into_iter()
        .map(|span| (span, literal_by_parent.get(&span.start).cloned()))
        .collect();
    (requires, dynamic)
}

/// Parse + scan one (already transpiled) ESM module.
fn scan_esm(text: &str, path: &Path) -> Result<ScannedModule> {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::GetSpan as _;
    use oxc_span::SourceType;

    let allocator = Allocator::default();
    let _p_parse = ProfileStage::new("scan_esm: parse");
    let ret = Parser::new(&allocator, text, SourceType::mjs()).parse();
    check_parse_diags(text, path, ret.panicked, &ret.errors, |e| {
        e.labels.as_ref().and_then(|l| l.first()).map(|l| l.offset() as u32)
    })?;
    drop(_p_parse);

    let _p_sem = ProfileStage::new("scan_esm: semantic+calls");
    let semantic = SemanticBuilder::new().with_scope_tree_child_ids(false).with_check_syntax_error(false).build(&ret.program).semantic;
    let (requires, dynamic) = scan_calls(&semantic);
    drop(_p_sem);
    let nodes = semantic.nodes();
    let _p_nodes = ProfileStage::new("scan_esm: node walk");

    let mut scan = ScannedModule { requires, dynamic, ..ScannedModule::default() };

    for node in nodes.iter() {
        let kind = node.kind();
        if let Some(d) = kind.as_import_declaration() {
            let mut data = ImportData::new(d.span, d.source.value.to_string());
            if d.specifiers.is_none() {
                data.side_effect = true; // `import './x'`
            }
            scan.imports.push(data);
        } else if let Some(n) = kind.as_export_named_declaration() {
            if n.declaration.is_some() {
                continue; // `export <decl>`: handled via the decl pass below
            }
            let mut pairs: Vec<(String, String)> = Vec::new();
            for spec in n.specifiers.iter() {
                if spec.export_kind.is_type() {
                    continue;
                }
                pairs.push((
                    spec.local.name().to_string(),
                    spec.exported.name().to_string(),
                ));
            }
            if pairs.is_empty() && n.source.is_none() {
                continue; // `export {}`: no names, no request
            }
            let source = n.source.as_ref().map(|s| s.value.to_string());
            scan.export_named.push((n.span, pairs, source));
        } else if let Some(a) = kind.as_export_all_declaration() {
            let ns = a.exported.as_ref().map(|name| name.name().to_string());
            scan.export_stars.push((a.span, a.source.value.to_string(), ns));
        } else if let Some(parent) = nodes.parent_node(node.id()) {
            // Children attach to their parent statement by span.
            let parent_start = parent.span().start;
            if let Some(s) = kind.as_import_default_specifier() {
                scan.import_specifiers
                    .entry(parent_start)
                    .or_default()
                    .push(SpecifierData::Default(s.local.name.to_string()));
            } else if let Some(s) = kind.as_import_namespace_specifier() {
                scan.import_specifiers
                    .entry(parent_start)
                    .or_default()
                    .push(SpecifierData::Namespace(s.local.name.to_string()));
            } else if let Some(s) = kind.as_import_specifier() {
                scan.import_specifiers
                    .entry(parent_start)
                    .or_default()
                    .push(SpecifierData::Named(
                        s.imported.name().to_string(),
                        s.local.name.to_string(),
                        s.import_kind.is_type(),
                    ));
            } else if parent.kind().as_export_default_declaration().is_some() {
                let name = if let Some(f) = kind.as_function() {
                    f.id.as_ref().map(|id| id.name.to_string())
                } else if let Some(c) = kind.as_class() {
                    c.id.as_ref().map(|id| id.name.to_string())
                } else {
                    None // arbitrary default expression
                };
                scan.default_children
                    .entry(parent_start)
                    .or_insert((node.span(), name));
            }
        }
    }

    // Fill import statements with their specifiers and drop type-only ones.
    let mut imports: Vec<ImportData> = Vec::new();
    for mut data in std::mem::take(&mut scan.imports) {
        let start = data.statement_span.start;
        if let Some(specifiers) = scan.import_specifiers.remove(&start) {
            for spec in specifiers {
                match spec {
                    SpecifierData::Default(local) => data.default = Some(local),
                    SpecifierData::Namespace(local) => data.namespace = Some(local),
                    SpecifierData::Named(imported, local, is_type) => {
                        if !is_type {
                            data.named.push((imported, local));
                        }
                    }
                }
            }
        }
        imports.push(data);
    }
    imports.sort_by_key(|d| d.statement_span.start);
    scan.imports = imports;

    // `export <decl>` bound names come from the module record's local export
    // entries, keyed by statement span start: the record walks destructuring
    // patterns for us, and aliases are impossible in declarations, so the
    // local name is both the local binding and the exported name. Type-only
    // and default entries are skipped (defaults are handled separately).
    let mr = &ret.module_record;
    let mut export_decls: Vec<(oxc_span::Span, oxc_span::Span, Vec<(String, String)>)> = Vec::new();
    // statement-span start -> index into export_decls (a linear .find() per
    // entry is quadratic on module-count and dominates big files).
    let mut by_start: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for entry in mr.local_export_entries.iter() {
        if entry.is_type || entry.export_name.is_default() {
            continue;
        }
        let Some(name) = entry.local_name.name() else { continue };
        let start = entry.statement_span.start;
        match by_start.get(&start) {
            Some(&i) => export_decls[i].2.push((name.to_string(), name.to_string())),
            None => {
                by_start.insert(start, export_decls.len());
                export_decls.push((
                    entry.statement_span,
                    entry.statement_span, // replaced below by the real decl span
                    vec![(name.to_string(), name.to_string())],
                ));
            }
        }
    }
    // Attach real declaration spans to `export <decl>` statements (the
    // keyword-stripping edit starts at the declaration, not the record's
    // statement span entries).
    for node in nodes.iter() {
        let Some(n) = node.kind().as_export_named_declaration() else { continue };
        let Some(decl) = n.declaration.as_ref() else { continue };
        let start = node.span().start;
        if let Some(&i) = by_start.get(&start) {
            export_decls[i].1 = decl.span();
        } else {
            // declaration exports no plain names (e.g. `export default`-less
            // TS leftovers stripped earlier): record so `export` is still stripped
            export_decls.push((node.span(), decl.span(), Vec::new()));
        }
    }
    export_decls.sort_by_key(|(span, _, _)| span.start);
    scan.export_decls = export_decls;
    drop(_p_nodes);

    Ok(scan)
}

/// 1-based line/column of a byte offset.
fn span_to_line_col(source: &str, offset: u32) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, ch) in source.char_indices() {
        if i as u32 >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// `file:line:col` for an AST span in the parsed source text.
fn span_loc(text: &str, path: &Path, span: oxc_span::Span) -> String {
    let (line, col) = span_to_line_col(text, span.start);
    format!("{}:{}:{}", path.display(), line, col)
}

/// JSON-escape a string for embedding in the emitted file.
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("{:?}", s))
}

/// Resolve a code location to the require expression to splice in:
/// `__honey_require("<id>")` for inlined modules (the bundled file), plain
/// `require("<spec>")` for externalized ones (bare specifiers, `node:`
/// builtins), so honey's runtime resolves them exactly like a direct run.
fn require_expr(
    spec: &str,
    from_dir: &Path,
    ts_referrer: bool,
    prefer_import: bool,
    text: &str,
    path: &Path,
    span: oxc_span::Span,
) -> Result<(String, Option<String>)> {
    match resolve_specifier(spec, from_dir, ts_referrer, prefer_import) {
        Ok(Resolved::Inline(file)) => {
            let id = lexically_normalize_path(&file).to_string_lossy().into_owned();
            Ok((format!("__honey_require({})", json_str(&id)), Some(id)))
        }
        Ok(Resolved::Css(file)) => {
            // CSS never enters the JS registry: the bundle graph extracts it
            // to the sibling .css output; the JS site of the import becomes a
            // no-op expression (side-effect imports rewrite to `void 0;`).
            let id = lexically_normalize_path(&file).to_string_lossy().into_owned();
            Ok((String::from("void 0"), Some(id)))
        }
        Ok(Resolved::Asset(file)) => {
            // Static asset: copied to `<stem>.<hash8>.<ext>` next to the
            // bundle output at emit time (content-hash name, computable
            // independently at the import site); the import site becomes the
            // relative URL string. No require — the asset never enters the
            // registry graph (the dep id is still returned so the bundler
            // tracks the file for copying).
            let id = lexically_normalize_path(&file).to_string_lossy().into_owned();
            let name = asset_output_name(&file).ok_or_else(|| anyhow::anyhow!("cannot read asset: {}", file.display()))?;
            Ok((format!("({})", json_str(&format!("./{name}"))), Some(id)))
        }
        Ok(Resolved::External) => Ok((format!("require({})", json_str(spec)), None)),
        Err(e) => {
            bail!(
                "{}: cannot resolve import '{}': {e}",
                span_loc(text, path, span),
                spec
            );
        }
    }
}

/// Apply source edits (start, end, replacement) descending by span start;
/// spans must not overlap.
fn apply_edits(text: &str, mut edits: Vec<(u32, u32, String)>) -> String {
    if edits.is_empty() {
        return text.to_string();
    }
    // Single-pass splice: sorting ascending and copying the untouched slices
    // between spans keeps this O(text + replacement bytes).  The previous
    // descending `replace_range` loop shifted the whole tail once per edit,
    // which is quadratic on files with many top-level declarations.
    edits.sort_by_key(|e| (e.0, e.1));
    let mut out = String::with_capacity(text.len() + 64);
    let mut cursor = 0usize;
    for (start, end, replacement) in edits {
        let s = start as usize;
        let e = end as usize;
        if s < cursor || e > text.len() {
            continue; // defensive: never let overlapping/out-of-range spans corrupt output
        }
        out.push_str(&text[cursor..s]);
        out.push_str(&replacement);
        cursor = e;
    }
    out.push_str(&text[cursor..]);
    out
}

// ---------------------------------------------------------------------------
// ESM modules: rewrite into the registry shape
// ---------------------------------------------------------------------------

/// Tree-shaking helper: is the identifier `name` referenced anywhere in
/// `text` OUTSIDE the span `span` (its import statement)?  Word-boundary
/// identifier scan, no regex crate; conservative in the safe direction (any
/// accidental match keeps the binding).  Byte-offset spans in, so multibyte
/// chars elsewhere in the text do not shift anything.
fn binding_used_elsewhere(text: &str, span: oxc_span::Span, name: &str) -> bool {
    let bs = text.as_bytes();
    let nb = name.as_bytes();
    let is_ident_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    for i in 0..bs.len() {
        // Occurrence start: identifier boundary before it.
        if i > 0 && is_ident_char(bs[i - 1]) {
            continue;
        }
        if bs[i..].len() < nb.len() || &bs[i..i + nb.len()] != nb {
            continue;
        }
        if i + nb.len() < bs.len() && is_ident_char(bs[i + nb.len()]) {
            continue;
        }
        // Inside (or touching) the import span itself?  Not a real use.
        if (i as u64) >= span.start as u64 && (i as u64) < span.end as u64 {
            continue;
        }
        return true;
    }
    false
}

fn process_esm(text: &str, path: &Path, temp: &mut usize) -> Result<(Module, Vec<String>)> {
    let scanned = scan_esm(text, path)?;

    let from_dir = PathBuf::from(module_dir(path));
    let ts_referrer = is_typescript_path(&path.to_string_lossy());

    let mut deps: Vec<String> = Vec::new();

    // Dynamic import() found anywhere in the module.
    let mut body_edit_spans: Vec<(u32, u32)> = Vec::new();
    let mut edits: Vec<(u32, u32, String)> = Vec::new();
    // `__honey_require` promise expression per dynamic-import span (reused
    // when the import is also an `export default` child).
    let mut dyn_exprs: HashMap<u32, String> = HashMap::new();

    for (span, spec_opt) in scanned.dynamic {
        let spec = match spec_opt {
            Some(s) => s,
            None => bail!(
                "{}: dynamic import() with a non-literal argument is unsupported in honey build",
                span_loc(text, path, span)
            ),
        };
        let (expr, dep) = require_expr(
            &spec,
            &from_dir,
            ts_referrer,
            true, // dynamic import resolves with import conditions
            text,
            path,
            span,
        )?;
        if let Some(dep) = dep {
            deps.push(dep);
        }
        let expr = format!("Promise.resolve().then(function () {{ return {expr}; }})");
        dyn_exprs.insert(span.start, expr.clone());
        edits.push((span.start, span.end, expr));
    }

    // `require` inside an ESM module has no ESM semantics; leave it to the
    // registry factory's (runtime-anchored) require parameter.
    let mut epilogue: Vec<String> = Vec::new();

    for import in scanned.imports {
        // Tree-shaking v1: drop import bindings that are never referenced in
        // the rest of the module, and — when NO binding of the statement
        // survives (and it isn't a side-effect import) — drop the dependency
        // entirely (its module never enters the graph).  Side-effect-only
        // imports and any surviving binding keep the require/enqueue.
        let died = !import.side_effect
            && import.default.is_none()
            && import.namespace.is_none()
            && !import.named.is_empty()
            && import.named.iter().all(|(_, local)| {
                !binding_used_elsewhere(text, import.statement_span, local)
            });
        let (req, dep) = require_expr(
            &import.source,
            &from_dir,
            ts_referrer,
            true,
            text,
            path,
            import.statement_span,
        )?;
        body_edit_spans.push((import.statement_span.start, import.statement_span.end));
        if died {
            // Fully dead import: emit nothing, do not enqueue the dep.
            edits.push((import.statement_span.start, import.statement_span.end, String::new()));
            if let Some(dep) = dep {
                // Track it as a parse-time dependency for sourcemap/watch
                // bookkeeping, but the module itself is never registered.
                let _ = dep;
            }
            continue;
        }
        if let Some(dep) = dep {
            deps.push(dep);
        }
        let code = if import.side_effect
            || (import.default.is_none() && import.namespace.is_none() && import.named.is_empty())
        {
            // `import './x'` / `import {} from './x'`: side effect only
            format!("{req};")
        } else {
            let n = *temp;
            *temp += 1;
            let mv = format!("__honey_m{n}");
            let mut lines = vec![format!("var {mv} = {req};")];
            if let Some(local) = &import.default {
                lines.push(format!("var {local} = __honey_idefault({mv});"));
            }
            if let Some(local) = &import.namespace {
                lines.push(format!("var {local} = {mv};"));
            }
            for (remote, local) in &import.named {
                if !binding_used_elsewhere(text, import.statement_span, local) {
                    continue;
                }
                if remote == local {
                    lines.push(format!("var {{ {local} }} = {mv};"));
                } else {
                    // `import { a as b }` or `import { "x-y" as b }`
                    lines.push(format!("var {{ {}: {} }} = {mv};", json_str(remote), local));
                }
            }
            lines.join("\n")
        };
        edits.push((import.statement_span.start, import.statement_span.end, code));
    }

    for (stmt_span, decl_span, names) in scanned.export_decls {
        // `export const x = ...` etc: strip the `export` keyword, export the
        // bound names at the end of the module.
        for (local, exported) in &names {
            epilogue.push(format!("exports[{}] = {local};", json_str(exported)));
        }
        body_edit_spans.push((stmt_span.start, stmt_span.end));
        edits.push((stmt_span.start, decl_span.start, String::new()));
    }

    for (stmt_span, pairs, source) in scanned.export_named {
        body_edit_spans.push((stmt_span.start, stmt_span.end));
        match source {
            Some(spec) => {
                if pairs.is_empty() {
                    // `export {} from 'm'`: side effect only
                    let (req, dep) =
                        require_expr(&spec, &from_dir, ts_referrer, true, text, path, stmt_span)?;
                    if let Some(dep) = dep {
                        deps.push(dep);
                    }
                    edits.push((stmt_span.start, stmt_span.end, format!("{req};")));
                } else {
                    let (req, dep) =
                        require_expr(&spec, &from_dir, ts_referrer, true, text, path, stmt_span)?;
                    if let Some(dep) = dep {
                        deps.push(dep);
                    }
                    let n = *temp;
                    *temp += 1;
                    let rv = format!("__honey_r{n}");
                    let mut lines = vec![format!("var {rv} = {req};")];
                    for (remote, exported) in &pairs {
                        lines.push(format!(
                            "exports[{}] = {rv}[{}];",
                            json_str(exported),
                            json_str(remote)
                        ));
                    }
                    edits.push((stmt_span.start, stmt_span.end, lines.join("\n")));
                }
            }
            None => {
                // `export { a, b as c }` (local bindings)
                for (local, exported) in &pairs {
                    epilogue.push(format!("exports[{}] = {local};", json_str(exported)));
                }
                edits.push((stmt_span.start, stmt_span.end, String::new()));
            }
        }
    }

    for (stmt_span, source, ns) in scanned.export_stars {
        let (req, dep) = require_expr(&source, &from_dir, ts_referrer, true, text, path, stmt_span)?;
        if let Some(dep) = dep {
            deps.push(dep);
        }
        body_edit_spans.push((stmt_span.start, stmt_span.end));
        let n = *temp;
        *temp += 1;
        let sv = format!("__honey_a{n}");
        let code = match ns {
            // `export * as ns from 'm'` (namespace object includes default)
            Some(name) => format!(
                "var {sv} = {req};\nexports[{}] = Object.assign({{}}, {sv});",
                json_str(&name)
            ),
            // `export * from 'm'`: copy every binding except `default`
            None => format!(
                "var {sv} = {req};\nObject.keys({sv}).forEach(function (k) {{ if (k !== 'default') exports[k] = {sv}[k]; }});"
            ),
        };
        edits.push((stmt_span.start, stmt_span.end, code));
    }

    // `export default` statements, in source order.
    let mut default_edits: Vec<(u32, (oxc_span::Span, Option<String>))> =
        scanned.default_children.into_iter().collect();
    default_edits.sort_by_key(|(start, _)| *start);

    for (stmt_start, (child_span, bound_name)) in default_edits {
        let stmt_end = child_span.end;
        // A dynamic import as the default expression is folded in with its
        // registry form (otherwise the raw `import` text would resolve
        // relative to the bundle file, not the module's directory).
        let child_code = dyn_exprs.get(&child_span.start).cloned().unwrap_or_else(|| {
            text[child_span.start as usize..child_span.end as usize].to_string()
        });
        body_edit_spans.push((stmt_start, stmt_end));
        let code = match bound_name {
            // `export default function greet() {}`: keep the declaration,
            // export the binding in the epilogue (hoisting-safe order).
            Some(name) => {
                epilogue.push(format!("exports[{}] = {name};", json_str("default")));
                child_code
            }
            // `export default <expr>` / anonymous declaration
            None => format!("exports[{}] = {child_code};", json_str("default")),
        };
        edits.push((stmt_start, stmt_end, code));
    }

    let mut code = apply_edits(text, edits);
    code.push('\n');
    for line in &epilogue {
        code.push_str(line);
        code.push('\n');
    }
    code.push_str("exports.__esModule = true;\n");

    Ok((Module { dir: module_dir(path), code, strict: true, bundle_line_offset: 0, source_map: None }, deps))
}

// ---------------------------------------------------------------------------
// CJS modules: rewrite literal require() calls, keep everything else
// ---------------------------------------------------------------------------

/// Rewrite literal `require('<spec>')` calls inside a CommonJS module:
/// inlined modules become `__honey_require('<id>')`, externalized ones keep
/// the literal specifier (runtime-resolved). Non-literal requires keep the
/// plaintext call and resolve through the registry factory's require.
fn process_cjs(text: &str, path: &Path, _temp: &mut usize) -> Result<(Module, Vec<String>)> {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::SourceType;

    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, text, SourceType::cjs()).parse();
    check_parse_diags(text, path, ret.panicked, &ret.errors, |e| {
        e.labels.as_ref().and_then(|l| l.first()).map(|l| l.offset() as u32)
    })?;

    let semantic = SemanticBuilder::new().with_scope_tree_child_ids(false).with_check_syntax_error(false).build(&ret.program).semantic;
    let (requires, dynamic) = scan_calls(&semantic);

    let from_dir = PathBuf::from(module_dir(path));
    // require() from a CJS module takes the file's own TS-ness into account.
    let ts_referrer = is_typescript_path(&path.to_string_lossy());

    let mut deps: Vec<String> = Vec::new();
    let mut edits: Vec<(u32, u32, String)> = Vec::new();

    for (span, spec) in requires {
        match require_expr(&spec, &from_dir, ts_referrer, false, text, path, span) {
            // Inlined modules become `__honey_require("<id>")`; external
            // specifiers keep the plain `require('spec')` text.
            Ok((expr, Some(dep))) => {
                edits.push((span.start, span.end, expr));
                deps.push(dep);
            }
            Ok((_, None)) => {}
            Err(e) => return Err(e),
        }
    }

    for (span, spec_opt) in dynamic {
        let spec = match spec_opt {
            Some(s) => s,
            None => bail!(
                "{}: dynamic import() with a non-literal argument is unsupported in honey build",
                span_loc(text, path, span)
            ),
        };
        let (expr, dep) = require_expr(&spec, &from_dir, ts_referrer, true, text, path, span)?;
        if let Some(dep) = dep {
            deps.push(dep);
        }
        edits.push((
            span.start,
            span.end,
            format!("Promise.resolve().then(function () {{ return {expr}; }})"),
        ));
    }

    let code = apply_edits(text, edits);
    Ok((Module { dir: module_dir(path), code, strict: false, bundle_line_offset: 0, source_map: None }, deps))
}

// ---------------------------------------------------------------------------
// Check parse errors with runtime-compatible formatting
// ---------------------------------------------------------------------------

/// Fail with `file:line:col: message` lines when a parse produced errors.
/// Generic over the diagnostic item: the diagnostic type and its label
/// extraction stay at the call site (the types belong to transitive oxc
/// crates, not this crate's dependencies).
fn check_parse_diags<D: std::fmt::Display>(
    text: &str,
    path: &Path,
    panicked: bool,
    errors: &[D],
    label_offset: impl Fn(&D) -> Option<u32>,
) -> Result<()> {
    if !panicked && errors.is_empty() {
        return Ok(());
    }
    let messages: Vec<String> = errors
        .iter()
        .map(|e| match label_offset(e) {
            Some(off) => {
                let (line, col) = span_to_line_col(text, off);
                format!("{}:{}:{}: {}", path.display(), line, col, e)
            }
            None => format!("{}: {}", path.display(), e),
        })
        .collect();
    if messages.is_empty() {
        bail!(
            "{}: parser panicked while recovering from a syntax error",
            path.display()
        );
    }
    bail!("{}", messages.join("\n"))
}

// ---------------------------------------------------------------------------
// TypeScript transpilation (direct oxc, same pipeline as the runtime)
// ---------------------------------------------------------------------------

/// oxc SourceType for a transpilable TypeScript path (.ts/.mts are modules,
/// .cts is a script; from_path handles all three, unknown extensions fall
/// back to plain TypeScript).
fn ts_source_type(path: &Path) -> oxc_span::SourceType {
    oxc_span::SourceType::from_path(path).unwrap_or_else(|_| oxc_span::SourceType::ts())
}

/// Transpile a TypeScript source to JavaScript via the Oxc parser +
/// transformer: type/interface/generics/type-alias stripping plus TS-only
/// runtime constructs (enums, namespaces, class fields, etc.). No
/// type-checking (use tsc, see SPEC.md). Parse or transform errors come back
/// as Err with `file:line:col: message`. Any `BuildOptions` defines
/// (`process.env.NODE_ENV` in production) are inlined on the AST with
/// dead-branch elimination applied to a fixpoint. Returns `(js, sourcemap)` —
/// the map only when minify or a sourcemap mode is active.
fn transform_typescript(source: &str, path: &Path, opts: &BuildOptions) -> Result<(String, Option<oxc_sourcemap::SourceMap>)> {
    use oxc_allocator::Allocator;
    use oxc_codegen::{Codegen, CodegenOptions};
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::SourceType;
    use oxc_transformer::{TransformOptions, Transformer};

    let source_type: SourceType = ts_source_type(path);
    let allocator = Allocator::default();
    let _p_parse = ProfileStage::new("transform: parse");
    let ret = Parser::new(&allocator, source, source_type).parse();
    check_parse_diags(source, path, ret.panicked, &ret.errors, |e| {
        e.labels.as_ref().and_then(|l| l.first()).map(|l| l.offset() as u32)
    })?;
    drop(_p_parse);

    let _p_sem = ProfileStage::new("transform: semantic");
    let semantic = SemanticBuilder::new().with_scope_tree_child_ids(false).with_check_syntax_error(false).build(&ret.program);
    let (symbols, scopes) = semantic.semantic.into_symbol_table_and_scope_tree();
    drop(_p_sem);

    let _p_tx = ProfileStage::new("transform: oxc transformer");
    let mut program = ret.program;
    let transform_ret = Transformer::new(&allocator, path, &TransformOptions::default())
        .build_with_symbols_and_scopes(symbols, scopes, &mut program);
    check_parse_diags(source, path, false, &transform_ret.errors, |e| {
        e.labels.as_ref().and_then(|l| l.first()).map(|l| l.offset() as u32)
    })?;
    drop(_p_tx);

    // Production: inline defines (process.env.NODE_ENV → "production") and
    // eliminate statically-known ifs/ternaries to a fixpoint. Done on the AST
    // before the single codegen pass so the sourcemap stays accurate.
    if !opts.defines().is_empty() {
        fold_defines(&allocator, &mut program, &opts.defines());
    }

    let minify = opts.minify_enabled();
    let want_map = minify || opts.sourcemap_mode() != SourceMapMode::None;
    let _p_cg = ProfileStage::new("transform: codegen");
    let cg_ret = Codegen::new()
        .with_options(CodegenOptions {
            minify,
            source_map_path: if want_map { Some(path.to_path_buf()) } else { None },
            ..CodegenOptions::default()
        })
        .build(&program);
    drop(_p_cg);

    Ok((cg_ret.code, cg_ret.map))
}

/// Fold compile-time defines into a parsed program and run dead-branch
/// elimination to a fixpoint (see `DefineFolder`).
fn fold_defines<'a>(allocator: &'a oxc_allocator::Allocator, program: &mut oxc_ast::ast::Program<'a>, defines: &[(String, String)]) {
    use oxc_ast::visit::VisitMut as _;
    for _ in 0..8 {
        let mut folder = DefineFolder { ast: oxc_ast::AstBuilder::new(allocator), defines, changed: false };
        folder.visit_program(program);
        if !folder.changed {
            break;
        }
    }
}

/// A `!`/logical/ternary/if constant folder driven by compile-time define
/// substitution. Matches dotted member chains rooted at an identifier
/// (e.g. `process.env.NODE_ENV`), replaces them with their define value, then
/// folds `!`, `===`/`!==`/`==`/`!=` on string/bool literals, `&&`/`||` short
/// circuits, ternaries and if-statements with statically-known tests.
struct DefineFolder<'a, 'd> {
    ast: oxc_ast::AstBuilder<'a>,
    defines: &'d [(String, String)],
    changed: bool,
}

const SPAN: oxc_span::Span = oxc_span::SPAN;

impl<'a, 'd> DefineFolder<'a, 'd> {
    /// Produce a placeholder expression for `mem::replace` dances.
    fn placeholder(&self) -> oxc_ast::ast::Expression<'a> {
        self.ast.expression_boolean_literal(SPAN, false)
    }

    /// Convert a raw `--define k=v` value into an expression literal:
    /// quoted strings stay strings, `true`/`false` booleans, JSON numbers
    /// numeric literals, `null` null; anything else is inserted as a string.
    fn define_value(&self, value: &str) -> oxc_ast::ast::Expression<'a> {
        let v = value.trim();
        if v == "true" {
            return self.ast.expression_boolean_literal(SPAN, true);
        }
        if v == "false" {
            return self.ast.expression_boolean_literal(SPAN, false);
        }
        if v == "null" {
            return self.ast.expression_null_literal(SPAN);
        }
        if let Ok(n) = v.parse::<f64>() {
            if !v.is_empty() {
                let r = self.ast.atom(v);
                return self.ast.expression_numeric_literal(SPAN, n, Some(r), oxc_ast::ast::NumberBase::Decimal);
            }
        }
        let s = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(v);
        self.ast.expression_string_literal(SPAN, s, None)
    }

    fn empty_statement(&self) -> oxc_ast::ast::Statement<'a> {
        oxc_ast::ast::Statement::EmptyStatement(self.ast.alloc(self.ast.empty_statement(SPAN)))
    }

    /// Full dotted path of a member expression chain rooted at an identifier.
    fn define_path(expr: &oxc_ast::ast::Expression<'a>) -> Option<String> {
        let mut parts: Vec<&str> = Vec::new();
        let mut cur = expr;
        loop {
            match cur {
                oxc_ast::ast::Expression::StaticMemberExpression(m) => {
                    parts.push(m.property.name.as_str());
                    cur = &m.object;
                }
                oxc_ast::ast::Expression::Identifier(id) => {
                    parts.push(id.name.as_str());
                    break;
                }
                _ => return None,
            }
        }
        parts.reverse();
        Some(parts.join("."))
    }

    /// Static boolean value of an expression (post-substitution literals).
    fn const_bool(expr: &oxc_ast::ast::Expression<'a>) -> Option<bool> {
        match expr {
            oxc_ast::ast::Expression::BooleanLiteral(b) => Some(b.value),
            oxc_ast::ast::Expression::StringLiteral(s) => Some(!s.value.is_empty()),
            oxc_ast::ast::Expression::NumericLiteral(n) => Some(n.value != 0.0),
            oxc_ast::ast::Expression::NullLiteral(_) => Some(false),
            _ => None,
        }
    }

    /// Static string value.
    fn const_str(expr: &oxc_ast::ast::Expression<'a>) -> Option<&'a str> {
        match expr {
            oxc_ast::ast::Expression::StringLiteral(s) => Some(s.value.as_str()),
            _ => None,
        }
    }

    /// Equality of two literal expressions under `==`/`===` (and negations).
    fn const_eq(left: &oxc_ast::ast::Expression<'a>, right: &oxc_ast::ast::Expression<'a>) -> Option<bool> {
        if let (Some(l), Some(r)) = (Self::const_str(left), Self::const_str(right)) {
            return Some(l == r);
        }
        if let (Some(l), Some(r)) = (Self::const_bool(left), Self::const_bool(right)) {
            return Some(l == r);
        }
        if let (
            oxc_ast::ast::Expression::NumericLiteral(l),
            oxc_ast::ast::Expression::NumericLiteral(r),
        ) = (left, right)
        {
            return Some(l.value == r.value);
        }
        None
    }
}

impl<'a, 'd> oxc_ast::visit::VisitMut<'a> for DefineFolder<'a, 'd> {
    fn visit_expression(&mut self, expr: &mut oxc_ast::ast::Expression<'a>) {
        // 1. Define substitution: dotted member chain matching a define key.
        if !self.defines.is_empty() {
            if let Some(path) = Self::define_path(expr) {
                if let Some((_, value)) = self.defines.iter().find(|(k, _)| *k == path) {
                    *expr = self.define_value(value);
                    self.changed = true;
                    return;
                }
            }
        }

        // 2. Walk children first (bottom-up folding).
        oxc_ast::visit::walk_mut::walk_expression(self, expr);

        // 3. Fold this node. Inner block computes the replacement, ending the
        // borrow of `*expr` before the reassignment.
        let replacement: Option<oxc_ast::ast::Expression<'a>> = match expr {
            oxc_ast::ast::Expression::UnaryExpression(u) => {
                if u.operator == oxc_ast::ast::UnaryOperator::LogicalNot {
                    Self::const_bool(&u.argument).map(|b| {
                        let _ = std::mem::replace(&mut u.argument, self.placeholder());
                        self.ast.expression_boolean_literal(SPAN, !b)
                    })
                } else {
                    None
                }
            }
            oxc_ast::ast::Expression::BinaryExpression(b) => {
                let is_eq = matches!(
                    b.operator,
                    oxc_ast::ast::BinaryOperator::Equality
                        | oxc_ast::ast::BinaryOperator::StrictEquality
                        | oxc_ast::ast::BinaryOperator::Inequality
                        | oxc_ast::ast::BinaryOperator::StrictInequality
                );
                match (is_eq, Self::const_eq(&b.left, &b.right)) {
                    (true, Some(eq)) => {
                        let val = matches!(
                            b.operator,
                            oxc_ast::ast::BinaryOperator::Equality
                                | oxc_ast::ast::BinaryOperator::StrictEquality
                        ) == eq;
                        let _ = std::mem::replace(&mut b.left, self.placeholder());
                        let _ = std::mem::replace(&mut b.right, self.placeholder());
                        Some(self.ast.expression_boolean_literal(SPAN, val))
                    }
                    _ => None,
                }
            }
            oxc_ast::ast::Expression::LogicalExpression(l) => match (l.operator, Self::const_bool(&l.left)) {
                (oxc_ast::ast::LogicalOperator::And, Some(false))
                | (oxc_ast::ast::LogicalOperator::Or, Some(true)) => Some(std::mem::replace(
                    &mut l.left,
                    self.placeholder(),
                )),
                (oxc_ast::ast::LogicalOperator::And, Some(true))
                | (oxc_ast::ast::LogicalOperator::Or, Some(false)) => Some(std::mem::replace(
                    &mut l.right,
                    self.placeholder(),
                )),
                _ => None,
            },
            oxc_ast::ast::Expression::ConditionalExpression(c) => Self::const_bool(&c.test).map(|b| {
                if b {
                    std::mem::replace(&mut c.consequent, self.placeholder())
                } else {
                    std::mem::replace(&mut c.alternate, self.placeholder())
                }
            }),
            _ => None,
        };
        if let Some(new_expr) = replacement {
            *expr = new_expr;
            self.changed = true;
        }
    }

    fn visit_statement(&mut self, stmt: &mut oxc_ast::ast::Statement<'a>) {
        // `if (static_test) A else B` → take the live branch and re-fold it.
        let taken: Option<oxc_ast::ast::Statement<'a>> =
            if let oxc_ast::ast::Statement::IfStatement(if_stmt) = stmt {
                oxc_ast::visit::walk_mut::walk_if_statement(self, if_stmt);
                // The walk may have folded the test; re-check.
                if let oxc_ast::ast::Statement::IfStatement(if_stmt) = stmt {
                    Self::const_bool(&if_stmt.test).map(|b| {
                        if b {
                            std::mem::replace(&mut if_stmt.consequent, self.empty_statement())
                        } else {
                            match if_stmt.alternate.take() {
                                Some(mut alt) => std::mem::replace(&mut alt, self.empty_statement()),
                                None => self.empty_statement(),
                            }
                        }
                    })
                } else {
                    None
                }
            } else {
                oxc_ast::visit::walk_mut::walk_statement(self, stmt);
                None
            };
        if let Some(s) = taken {
            *stmt = s;
            self.changed = true;
            self.visit_statement(stmt);
        }
    }
}

/// Minify a plain-JS module: parse (module kind inferred from syntax), apply
/// defines + dead-branch elimination, codegen-minify. Returns `(js, map)`.
fn compile_js(source: &str, path: &Path, opts: &BuildOptions) -> Result<(String, Option<oxc_sourcemap::SourceMap>)> {
    use oxc_allocator::Allocator;
    use oxc_codegen::{Codegen, CodegenOptions};
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    let source_type = SourceType::from_path(path)
        .unwrap_or_else(|_| SourceType::unambiguous());
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, source, source_type).parse();
    check_parse_diags(source, path, ret.panicked, &ret.errors, |e| {
        e.labels.as_ref().and_then(|l| l.first()).map(|l| l.offset() as u32)
    })?;

    let mut program = ret.program;
    if !opts.defines().is_empty() {
        fold_defines(&allocator, &mut program, &opts.defines());
    }

    let minify = opts.minify_enabled();
    let want_map = minify || opts.sourcemap_mode() != SourceMapMode::None;
    let cg_ret = Codegen::new()
        .with_options(CodegenOptions {
            minify,
            source_map_path: if want_map { Some(path.to_path_buf()) } else { None },
            ..CodegenOptions::default()
        })
        .build(&program);

    Ok((cg_ret.code, cg_ret.map))
}

/// Lightweight CSS minifier for v1: strip comments and runs of whitespace,
/// collapse `;}`/`;` before `}`, trim trailing/leading whitespace per rule.
/// Not a full CSS parser — good enough for framework stylesheets.
fn minify_css(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let bytes = css.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // comments
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        // strings pass through verbatim
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            out.push(bytes[i] as char);
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    out.push(bytes[i] as char);
                    out.push(bytes[i + 1] as char);
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    break;
                }
                if bytes[i] == b'\n' {
                    break;
                }
                out.push(bytes[i] as char);
                i += 1;
            }
            if i < bytes.len() {
                out.push(bytes[i] as char);
                i += 1;
            }
            continue;
        }
        // whitespace runs → single space (kept only if not adjacent to ;{}:,
        // which self-delimit)
        if (bytes[i] as char).is_ascii_whitespace() {
            while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
                i += 1;
            }
            if !out.is_empty() {
                let last = out.chars().last().unwrap();
                if !matches!(last, ';' | '{' | '}' | ':' | ',' | '>') {
                    out.push(' ');
                }
            }
            continue;
        }
        // `;` directly before `}` is redundant
        if bytes[i] == b';' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'}' {
                i += 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// Bundle emission
// ---------------------------------------------------------------------------

/// One self-contained file: registry of module factories keyed by resolved
/// absolute path, CommonJS semantics (partial exports on cycles), entry
/// module required last.
fn emit_bundle(bundler: &mut Bundler, entry_id: &str, opts: &BuildOptions) -> (String, Option<oxc_sourcemap::SourceMap>) {
    let mut out = String::new();
    out.push_str("// generated by `honey build` (Cinnamon v1 bundler)\n");
    out.push_str("// modules keyed by resolved absolute path; CommonJS registry semantics;\n");
    out.push_str("// entry module required last. node: builtins stay external and resolve\n");
    out.push_str("// through honey's runtime require.\n");
    // Browser guard (browser target only; node provides `process`): code
    // inlined from CJS packages reads process.env (e.g. react-dom's
    // production/development switch). Only injected when a process global is
    // missing, so runs under honey/Node keep theirs.
    if opts.target() == BuildTarget::Browser {
        out.push_str(
            "if (typeof process === 'undefined') {\n  globalThis.process = { env: {}, argv: [], platform: 'browser', version: '' };\n}\n\n",
        );
    }
    out.push_str("var __honey_defs = {\n");
    for id in &bundler.order {
        let m = bundler.modules.get_mut(id).expect("module present");
        out.push_str(&format!(
            "  {}: {{ dir: {}, factory: function (module, exports, require) {{\n",
            json_str(id),
            json_str(&m.dir)
        ));
        if m.strict {
            out.push_str("\"use strict\";\n");
        }
        m.bundle_line_offset = out.matches('\n').count();
        out.push_str(&m.code);
        if !m.code.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("} },\n");
    }
    out.push_str("};\n\n");
    out.push_str(
        r#"function __honey_idefault(m) {
  return (m && typeof m === 'object' && 'default' in m) ? m.default : m;
}

function __honey_dir_require(dir) {
  var r = function (spec) { globalThis.__honeySetRequireDir(dir); return globalThis.require(spec); };
  r.resolve = function (spec) { globalThis.__honeySetRequireDir(dir); return globalThis.require.resolve(spec); };
  return r;
}

function __honey_require(id) {
  var d = __honey_defs[id];
  if (!d) throw new Error('honey bundle: no such module: ' + id);
  if (d.done !== undefined) return d.done;
  // module still evaluating: CommonJS circular require gets its (partial)
  // in-progress exports object
  if (d.partial !== undefined) return d.partial;
  var module = { exports: {} };
  // pre-register the (partial) exports so require cycles get partial exports
  d.partial = module.exports;
  d.factory(module, module.exports, __honey_dir_require(d.dir));
  // the factory may have replaced module.exports entirely
  d.done = module.exports;
  return d.done;
}
"#,
    );
    out.push('\n');
    // The bundle's job in the browser is evaluating the entry's side effects;
    // keep the exports only where a host provides `module` (honey CJS runs).
    // The node target assumes a CommonJS host and assigns unconditionally.
    if opts.target() == BuildTarget::Node {
        out.push_str(&format!("module.exports = __honey_require({});\n", json_str(entry_id)));
    } else {
        out.push_str(&format!(
            "if (typeof module === 'undefined') {{ __honey_require({}); }} else {{ module.exports = __honey_require({}); }}\n",
            json_str(entry_id),
            json_str(entry_id)
        ));
    }

    // Combined sourcemap: per-module maps (minified TS/JS) merged with each
    // module's line offset in the emitted text. Modules without a map (dev
    // transpiles, plain JS) contribute no tokens.
    let maps: Vec<(String, oxc_sourcemap::SourceMap)> = bundler
        .order
        .iter()
        .filter_map(|id| {
            bundler
                .modules
                .get(id)
                .and_then(|m| m.source_map.as_ref().map(|sm| (id.clone(), sm.clone())))
        })
        .collect();
    if maps.is_empty() {
        return (out, None);
    }
    let pairs: Vec<(&oxc_sourcemap::SourceMap, u32)> = maps
        .iter()
        .map(|(id, sm)| {
            let off = bundler.modules[id].bundle_line_offset as u32;
            if std::env::var("HONEY_DEBUG_MAP").is_ok() {
                eprintln!("[map] module {id} line_offset={off} tokens={}", sm.get_tokens().count());
            }
            (sm, off)
        })
        .collect();
    let combined = oxc_sourcemap::ConcatSourceMapBuilder::from_sourcemaps(&pairs).into_sourcemap();
    (out, Some(combined))
}

// ---------------------------------------------------------------------------
// Verification (run with: cargo test --release --lib, or from the wired copy)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod verify {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Mutex;

    const VERIFY_ROOT: &str = "/tmp/honey-build-verify";
    const TEST_SOCKET: &str = "/tmp/honey-build.sock";

    /// The honey binary from the (shared) cargo target dir.
    fn binary_path() -> PathBuf {
        let target = std::env::var("CARGO_TARGET_DIR")
            .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/target").to_string());
        PathBuf::from(target).join("release").join("honey")
    }

    fn run_honey(cwd: &Path, args: &[&str]) -> (String, String, i32) {
        let out = Command::new(binary_path())
            .args(args)
            .current_dir(cwd)
            .env("HONEY_SOCKET", TEST_SOCKET)
            // Hermetic: never route through a daemon (a leftover daemon from a
            // previous test binary or one auto-spawned mid-run would make the
            // fixture runs depend on shared, warm, stale-able engine state).
            .env("HONEY_NO_DAEMON", "1")
            .output()
            .expect("failed to spawn honey binary");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.code().unwrap_or(-1),
        )
    }

    fn write_fixture(dir: &Path, files: &[(&str, &str)]) {
        let _ = std::fs::remove_dir_all(dir);
        for (rel, content) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
    }

    /// Unit tests share one process; serialize the daemon-backed runs.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn esm_bundle_matches_direct_run() {
        let _lock = TEST_LOCK.lock().unwrap();
        let dir = Path::new(VERIFY_ROOT).join("esm");
        write_fixture(
            &dir,
            &[
                (
                    "src/index.ts",
                    r#"import { add, bump, V, bee, tag } from "./a";
import greeting from "./greet";
import { load } from "./dyn";
import type { T } from "./types-helper";
console.log("add:", add(2, 3));
console.log("bump:", bump(5), "V:", V);
console.log("bee:", bee.name, bee.bump(1));
console.log("tag:", tag);
console.log(greeting("world"));
load().then((s) => console.log("dyn:", s));
"#,
                ),
                (
                    "src/a.ts",
                    r#"export const add = (x: number, y: number): number => x + y;
export { bump, V } from "./b";
export { default as bee } from "./b";
export * from "./extra";
"#,
                ),
                (
                    "src/b.ts",
                    r#"export const V = 40;
export function bump(n: number): number { return n + V; }
export default { name: "bee", bump };
"#,
                ),
                ("src/extra.ts", "export const tag = \"extra\";\n"),
                (
                    "src/greet.ts",
                    r#"export default function greet(who: string): string {
  return "hi " + who;
}
"#,
                ),
                (
                    "src/dyn.ts",
                    r#"export async function load(): Promise<string> {
  const m = await import("./late");
  return m.msg;
}
"#,
                ),
                ("src/late.ts", "export const msg = \"late-loaded\";\n"),
                ("src/types-helper.ts", "export interface T { a: number }\n"),
            ],
        );

        let (direct_out, direct_err, direct_code) = run_honey(&dir, &["run", "src/index.ts"]);
        assert_eq!(direct_code, 0, "direct run failed: {direct_err}");

        cmd_build(
            Some(dir.join("src/index.ts").to_str().unwrap()),
            Some(dir.join("dist").to_str().unwrap()),
            &BuildOptions::default(),
        )
        .expect("esm bundle failed");

        let (bundle_out, bundle_err, bundle_code) = run_honey(&dir, &["run", "dist/index.js"]);
        assert_eq!(bundle_code, 0, "bundled run failed: {bundle_err}");
        assert!(!bundle_out.is_empty());
        assert_eq!(
            bundle_out, direct_out,
            "bundled stdout differs from direct run: direct={direct_out:?} bundled={bundle_out:?} stderr={bundle_err:?}"
        );
    }

    #[test]
    fn cjs_bundle_matches_direct_run() {
        let _lock = TEST_LOCK.lock().unwrap();
        let dir = Path::new(VERIFY_ROOT).join("cjs");
        write_fixture(
            &dir,
            &[
                (
                    "src/index.js",
                    r#"const counter = require('./lib/counter');
const path = require('path');
console.log('counter:', counter.increment(), counter.increment(), counter.get());
console.log('builtin:', path.basename('/a/b/c.js'));
const state = require('./lib/state');
console.log('state:', state.count);
console.log('answer:', require('./lib/data.json').answer);
"#,
                ),
                (
                    "src/lib/counter.js",
                    r#"const state = require('./state');
function increment() { state.count += 1; return state.count; }
function get() { return state.count; }
module.exports = { increment, get };
"#,
                ),
                (
                    "src/lib/state.js",
                    r#"const counter = require('./counter');
let count = 10;
if (typeof counter.get === 'function') {
  count = counter.get();
}
module.exports = { count };
"#,
                ),
                ("src/lib/data.json", "{\n  \"answer\": 42\n}\n"),
            ],
        );

        let (direct_out, direct_err, direct_code) = run_honey(&dir, &["run", "src/index.js"]);
        assert_eq!(direct_code, 0, "direct run failed: {direct_err}");

        cmd_build(
            Some(dir.join("src/index.js").to_str().unwrap()),
            Some(dir.join("dist").to_str().unwrap()),
            &BuildOptions::default(),
        )
        .expect("cjs bundle failed");

        let (bundle_out, bundle_err, bundle_code) = run_honey(&dir, &["run", "dist/index.js"]);
        assert_eq!(bundle_code, 0, "bundled run failed: {bundle_err}");
        assert_eq!(
            bundle_out, direct_out,
            "bundled stdout differs from direct run: direct={direct_out:?} bundled={bundle_out:?} stderr={bundle_err:?}"
        );
    }

    #[test]
    fn mixed_ts_entry_with_lodash_package() {
        let _lock = TEST_LOCK.lock().unwrap();
        let dir = Path::new(VERIFY_ROOT).join("mixed");
        write_fixture(
            &dir,
            &[(
                "src/index.ts",
                r#"import _ from "lodash";
const chunks = _([1, 2, 3, 4]).chunk(2).value();
console.log("chunks:", JSON.stringify(chunks));
console.log("pick:", JSON.stringify(_.pick({ a: 1, b: 2 }, ["a"])));
"#,
            )],
        );

        // Install lodash through the package manager.
        let (add_out, add_err, add_code) = run_honey(&dir, &["add", "lodash"]);
        assert_eq!(add_code, 0, "honey add lodash failed: {add_err}{add_out}");

        let (direct_out, direct_err, direct_code) = run_honey(&dir, &["run", "src/index.ts"]);
        assert_eq!(direct_code, 0, "direct run failed: {direct_err}");

        // cmd_build's default entry is `src/index.ts` relative to the process
        // cwd — set it to the fixture dir (tests are serialized by TEST_LOCK)
        // and restore afterwards.
        let prev_dir = std::env::current_dir().expect("current dir");
        std::env::set_current_dir(&dir).expect("chdir to fixture dir");
        let built = cmd_build(None, Some(dir.join("dist").to_str().unwrap()), &BuildOptions::default());
        std::env::set_current_dir(prev_dir).expect("restore cwd");
        built.expect("mixed bundle failed");

        let (bundle_out, bundle_err, bundle_code) = run_honey(&dir, &["run", "dist/index.js"]);
        assert_eq!(bundle_code, 0, "bundled run failed: {bundle_err}");
        assert_eq!(
            bundle_out, direct_out,
            "bundled stdout differs from direct run: direct={direct_out:?} bundled={bundle_out:?} stderr={bundle_err:?}"
        );

        // Bare packages ARE inlined from `.cinm`/node_modules when resolvable
        // (the emitted bundle must be self-contained); the lodash factory is
        // keyed by its resolved `.cinm` id.
        let bundle_source = std::fs::read_to_string(dir.join("dist").join("index.js")).unwrap();
        assert!(
            bundle_source.lines().any(|l| l.contains("lodash@")),
            "bundle must inline the installed lodash package"
        );
    }

    #[test]
    fn dynamic_import_nonliteral_is_build_error() {
        let _lock = TEST_LOCK.lock().unwrap();
        let dir = Path::new(VERIFY_ROOT).join("dynbad");
        write_fixture(
            &dir,
            &[(
                "src/index.ts",
                "const target = \"./late\";\nimport(target);\n",
            )],
        );
        let result = cmd_build(
            Some(dir.join("src/index.ts").to_str().unwrap()),
            Some(dir.join("dist").to_str().unwrap()),
            &BuildOptions::default(),
        );
        let err = result.expect_err("non-literal dynamic import must fail the build");
        let msg = format!("{err:#}");
        assert!(msg.contains("non-literal"), "unexpected error: {msg}");
        assert!(msg.contains("src/index.ts:2:1"), "error must carry file:line:col: {msg}");
    }
}
