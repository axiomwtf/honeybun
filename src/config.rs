//! honey.config.ts hooks (SPEC section 8).
//!
//! Zero config out of the box: everything here is inert unless a
//! `honey.config.ts` (or `.js`) exists in the project root. The config file is
//! transpiled with the same oxc pipeline the runtime uses, converted to CJS,
//! and the requested hook is invoked as one self-contained classic script in
//! the daemon's warm engine — no extra engine, no sidecar files.
//!
//! v1 semantics: hooks are *advisory* observation points. A hook's return
//! value is reported but never alters control flow; a broken config or a
//! failing hook prints a warning and the command proceeds. This keeps the
//! zero-config promise honest: a bad config file can silently degrade to
//! no hooks, never to a broken toolchain.

use std::path::PathBuf;

/// Locate honey.config.ts / honey.config.js in the process cwd (not the
/// ancestors — a nested project must not pick up a parent's config).
pub fn find_config() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    for name in ["honey.config.ts", "honey.config.js"] {
        let p = cwd.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Build the self-contained hook-eval script:
/// load config (CJS closure) -> pick the hook fn -> call it with ctx.
fn hook_script(transpiled: &str, hook: &str, ctx_json: &str) -> String {
    format!(
        r#"(function(){{
var module = {{ exports: {{}} }}; var exports = module.exports;
var defineConfig = function (config) {{ return config; }};
{transpiled}
var cfg = (module.exports && typeof module.exports === "object" && module.exports.default !== undefined) ? module.exports.default : (module.exports && typeof module.exports === "object" ? module.exports : null);
var fn = cfg && cfg["{hook}"];
if (typeof fn !== "function") return null;
return fn({ctx});
}})()"#,
        transpiled = transpiled,
        hook = hook,
        ctx = ctx_json
    )
}

/// Load the config file and return its transpiled CJS-ready source.
fn load_transpiled(path: &PathBuf) -> Option<String> {
    let source = std::fs::read_to_string(path).ok()?;
    let path_str = path.to_string_lossy().into_owned();
    // TS configs go through the shared oxc pipeline; .js passes through
    // untouched (transpile is a no-op for .js by extension check).
    let js = crate::runtime::transpile_if_typescript(&path_str, &source)
        .map_err(|e| eprintln!("honey: config transpile failed: {e}"))
        .ok()?;
    Some(cjs_config(&js))
}

/// Minimal ESM -> CJS conversion for config files. The shared `runtime::
/// transform_esm_to_cjs` is line-based and mishandles the canonical
/// multi-line `export default defineConfig({ ... })` shape, so configs get
/// a purpose-built one: `export default <expr>` (an expression spanning any
/// number of lines, since only the prefix is rewritten), other `export`
/// prefixes are stripped, `import` is rejected with a clear error.
fn cjs_config(source: &str) -> String {
    let mut out = String::with_capacity(source.len() + 32);
    for line in source.lines() {
        let t = line.trim_start();
        if t.starts_with("import ") || t.starts_with("import{") || t.starts_with("import *") {
            eprintln!("honey: warning: config files do not support imports in v1");
            continue;
        }
        if t.starts_with("export default ") || t == "export default" {
            out.push_str(&line.replacen("export default", "module.exports.default =", 1));
            out.push('\n');
            continue;
        }
        if t.starts_with("export ") || t.starts_with("export{") {
            out.push_str(line.replacen("export", "", 1).as_str());
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Fire a hook. Returns the hook's JSON-serialized return value when present.
/// Never fails the caller: hook errors are warnings on stderr.
pub fn fire(hook: &str, ctx_json: &str) -> Option<String> {
    if hook.is_empty() {
        return None;
    }
    let path = find_config()?;
    let transpiled = load_transpiled(&path)?;
    let code = hook_script(&transpiled, hook, ctx_json);

    let outcome: Option<Result<Option<String>, String>> = match crate::daemon::eval_via_daemon(&code) {
        Some(resp) if resp.error.is_none() => {
            // Hook console output is advisory context: surface it on stderr
            // so command stdout stays parseable.
            if !resp.stdout.is_empty() {
                eprint!("{}", resp.stdout);
            }
            if !resp.stderr.is_empty() {
                eprint!("{}", resp.stderr);
            }
            Some(Ok(resp.file))
        }
        Some(resp) => {
            eprint!("{}", resp.stdout);
            eprintln!("honey: config hook `{hook}` failed: {}", resp.error.unwrap_or_default());
            None
        }
        None => {
            let rt = crate::runtime::Runtime::new();
            match rt.eval_code(&code) {
                Ok(json) => Some(Ok(json)),
                Err(e) => {
                    eprintln!("honey: config hook `{hook}` failed: {e}");
                    None
                }
            }
        }
    };
    match outcome {
        // Advisory result: surface it on stderr so command stdout stays
        // parseable while the hook is still visible.
        Some(Ok(Some(value))) => {
            eprintln!("honey: {hook} hook: {value}");
            Some(value)
        }
        r => r.and_then(|o| o.ok().flatten()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_wraps_and_calls() {
        let js = hook_script(
            "module.exports.default = { onBuild(ctx) { return ctx.where + '!' } };",
            "onBuild",
            r#"{"where":"built"}"#,
        );
        let rt = crate::runtime::Runtime::new();
        let out = rt.eval_code(&js).ok().flatten();
        assert_eq!(out.as_deref(), Some("\"built!\""));
    }

    #[test]
    fn documented_define_config_shape_works() {
        let js = cjs_config("export default defineConfig({ onBuild(ctx) { return ctx.where + '?'; } });");
        let js = hook_script(&js, "onBuild", r#"{"where":"built"}"#);
        let rt = crate::runtime::Runtime::new();
        let out = rt.eval_code(&js).ok().flatten();
        assert_eq!(out.as_deref(), Some("\"built?\""));
    }

    #[test]
    fn script_handles_missing_hook() {
        let js = hook_script("module.exports.default = {};", "onBuild", "{}");
        let rt = crate::runtime::Runtime::new();
        let out = rt.eval_code(&js).ok().flatten();
        assert_eq!(out.as_deref(), None);
    }
}
