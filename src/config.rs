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

/// Build the lookup script for a named script task: pick `cfg.scripts[name]`.
/// A string is the command as-is; a function receives ctx {args} and returns
/// the command string (so tasks can shape shell invocations with arguments).
fn script_script(transpiled: &str, name: &str, ctx_json: &str) -> String {
    format!(
        r#"(function(){{
var module = {{ exports: {{}} }}; var exports = module.exports;
var defineConfig = function (config) {{ return config; }};
{transpiled}
var cfg = (module.exports && typeof module.exports === "object" && module.exports.default !== undefined) ? module.exports.default : (module.exports && typeof module.exports === "object" ? module.exports : null);
var map = cfg && cfg["scripts"];
if (!map || typeof map !== "object") return null;
var v = map["{name}"];
if (typeof v === "function") return v({{ args: {ctx} }});
if (typeof v === "string") return v;
return null;
}})()"#,
        transpiled = transpiled,
        name = name,
        ctx = ctx_json
    )
}

/// Build the self-contained script that invokes a config hook. Returns None
/// when there is no config file or it cannot be transpiled. This is the
/// daemon-side half of `fire()`: the thin `honey` client asks the daemon to
/// build and evaluate it so it never links oxc or the engine.
pub fn hook_code(hook: &str, ctx_json: &str) -> Option<String> {
    if hook.is_empty() {
        return None;
    }
    let path = find_config()?;
    let transpiled = load_transpiled(&path)?;
    Some(hook_script(&transpiled, hook, ctx_json))
}

/// Build the self-contained script that resolves a `scripts.<name>` task.
/// Task names are restricted to path-friendly characters so they can never
/// inject into the generated script.
pub fn script_code(name: &str, args: &[String]) -> Option<String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.:@/".contains(c))
    {
        return None;
    }
    let path = find_config()?;
    let transpiled = load_transpiled(&path)?;
    let args_json = serde_json::to_string(args).ok()?;
    Some(script_script(&transpiled, name, &args_json))
}

/// Resolve a named script task (`honey run <task>`) from the config: a
/// `scripts` map whose values are shell commands (strings) or functions of
/// ctx {args} returning a shell command. Returns the command to execute, or
/// None when no config, no such task, or the shape is unusable.
pub fn resolve_script(name: &str, args: &[String]) -> Option<String> {
    let code = script_code(name, args)?;
    let json = eval_quiet(&code)?;
    // A function that returned a non-string (number, object) is not a
    // runnable command: treat it as no script rather than guessing.
    serde_json::from_str::<String>(&json).ok()
}

/// Evaluate in the daemon's warm engine when available, else a fresh engine.
/// Returns the raw JSON-encoded result. Unlike fire(), nothing is printed:
/// script resolution is plumbing, not advisory output.
fn eval_quiet(code: &str) -> Option<String> {
    match crate::daemon::eval_via_daemon(code) {
        Some(resp) if resp.error.is_none() => resp.file,
        Some(_) => None,
        None => crate::runtime::Runtime::new().eval_code(code).ok().flatten(),
    }
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

    #[test]
    fn script_task_string_resolves() {
        let cfg = cjs_config(
            "export default defineConfig({\n  scripts: {\n    dev: \"bun scripts/run.ts dev\",\n  },\n});",
        );
        let js = script_script(&cfg, "dev", "[]");
        let rt = crate::runtime::Runtime::new();
        let out = rt.eval_code(&js).ok().flatten();
        assert_eq!(out.as_deref(), Some("\"bun scripts/run.ts dev\""));
    }

    #[test]
    fn script_task_function_gets_args() {
        let cfg = cjs_config(
            "export default defineConfig({ scripts: { hello(ctx) { return 'echo ' + ctx.args.length; } } });",
        );
        let js = script_script(&cfg, "hello", r#"["a","b"]"#);
        let rt = crate::runtime::Runtime::new();
        let out = rt.eval_code(&js).ok().flatten();
        assert_eq!(out.as_deref(), Some("\"echo 2\""));
    }

    #[test]
    fn script_task_missing_name_is_null() {
        let cfg = cjs_config("export default defineConfig({ scripts: { dev: \"true\" } });");
        let js = script_script(&cfg, "nope", "[]");
        let rt = crate::runtime::Runtime::new();
        let out = rt.eval_code(&js).ok().flatten();
        assert_eq!(out.as_deref(), None);
    }

    #[test]
    fn resolve_script_rejects_injection_names() {
        assert!(resolve_script("dev; rm -rf /", &[]).is_none());
        assert!(resolve_script("\"dev\"", &[]).is_none());
    }
}

#[cfg(test)]
mod script_tests {
    use super::*;

    #[test]
    fn resolve_script_end_to_end() {
        let tmp = std::env::temp_dir().join(format!("honey_conf_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&tmp.join("honey.config.ts"), "export default defineConfig({ scripts: { dev: \"echo hi\" } });").unwrap();
        std::env::set_current_dir(&tmp).unwrap();
        let got = resolve_script("dev", &[]);
        std::env::set_current_dir("/").unwrap();
        std::fs::remove_dir_all(&tmp).unwrap();
        assert_eq!(got.as_deref(), Some("echo hi"));
    }
}
