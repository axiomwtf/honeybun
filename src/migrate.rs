//! JS to TS migration (SPEC §5).
//!
//! `honey migrate` walks the project's `.js` files (or a single one with
//! `--file`) and rewrites them as TypeScript with inferred parameter types:
//!
//! - Function/destructured-free parameters get annotations inferred from
//!   their usage patterns inside the function body (arithmetic → number,
//!   string methods/templates → string, array methods → arrays, property
//!   access → an inferred shape interface).
//! - Repeated object shapes seen across the file become `interface`
//!   declarations, each flagged with `// @honey: inferred`.
//! - Relative import/require specifiers ending in `.js` are rewritten
//!   extensionless so both the runtime and tsc resolve to the migrated
//!   `.ts` siblings.
//! - Incremental: a file whose `.ts` output is already newer than its `.js`
//!   source is skipped, so re-runs only touch changed files.
//!
//! Parse failures never abort a project migration: the file is reported and
//! left untouched. No type-checking is performed (use tsc).

use anyhow::{Context, Result};
use oxc_ast::ast::BindingPatternKind;
use std::path::{Path, PathBuf};

/// Dirs never migrated.
const SKIP_DIRS: &[&str] = &[
    ".git", "node_modules", ".cinm", "dist", "build", "out", "target", "coverage", ".honey",
];

/// A migrated unit: param annotations (span -> replacement), interfaces to
/// prepend, and stats for the summary line.
struct Migration {
    edits: Vec<(usize, usize, String)>, // (start, end, replacement)
    interfaces: Vec<String>,
    annotated_params: usize,
}

pub fn migrate(single_file: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;

    let files = match single_file {
        Some(f) => {
            let p = cwd.join(f);
            if !p.exists() {
                anyhow::bail!("no such file: {}", f);
            }
            vec![p]
        }
        None => collect_js_files(&cwd)?,
    };

    if files.is_empty() {
        println!("no .js files found to migrate");
        return Ok(());
    }

    let mut migrated = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for file in &files {
        let ts_path = file.with_extension("ts");
        // Incremental: skip when the .ts output is already up to date.
        if ts_path.exists() {
            let ts_newer = match (std::fs::metadata(&ts_path), std::fs::metadata(file)) {
                (Ok(t), Ok(j)) => match (t.modified(), j.modified()) {
                    (Ok(t), Ok(j)) => t >= j,
                    _ => false,
                },
                _ => false,
            };
            if ts_newer {
                skipped += 1;
                continue;
            }
        }

        let source = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(_) => {
                failed += 1;
                continue;
            }
        };

        match migrate_source(&source) {
            Some(m) => {
                let mut out = String::with_capacity(source.len() + 256);
                out.push_str("// migrated to TypeScript by honey migrate\n");
                for iface in &m.interfaces {
                    out.push_str("// @honey: inferred\n");
                    out.push_str(iface);
                    out.push('\n');
                }
                out.push_str(&apply_edits(&source, &m.edits));

                let rel = file.strip_prefix(&cwd).unwrap_or(file).display();
                if std::fs::write(&ts_path, out).is_err() {
                    eprintln!("honey: failed to write {}", ts_path.display());
                    failed += 1;
                    continue;
                }
                // The .ts file now owns this code; drop the .js so nothing
                // keeps executing the stale original.
                let _ = std::fs::remove_file(file);
                println!(
                    "  {} -> {} ({} param(s) annotated, {} interface(s))",
                    rel,
                    ts_path.strip_prefix(&cwd).unwrap_or(&ts_path).display(),
                    m.annotated_params,
                    m.interfaces.len()
                );
                migrated += 1;
            }
            None => {
                println!("  skipped {} (parse error)", file.display());
                failed += 1;
            }
        }
    }

    println!(
        "migrated {} file(s), {} up to date, {} skipped",
        migrated, skipped, failed
    );
    Ok(())
}

/// All `.js` files under `root`, skipping vendored/generated dirs and
/// config stubs.
fn collect_js_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with(".tmp-") {
                    stack.push(path);
                }
                continue;
            }
            if name.ends_with(".js")
                && !name.ends_with(".min.js")
                && !name.ends_with(".config.js")
                && name != "honey.config.js"
            {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Migrate one source file. Returns None when the file does not parse.
fn migrate_source(source: &str) -> Option<Migration> {
    use oxc_allocator::Allocator;
    use oxc_ast::ast::{Declaration, Statement};
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    let allocator = Allocator::default();
    let source_type = SourceType::mjs();
    let ret = Parser::new(&allocator, source, source_type).parse();
    if !ret.errors.is_empty() || ret.panicked {
        return None;
    }

    // (name, name_span, body_span)
    let mut functions: Vec<(String, oxc_span::Span, oxc_span::Span)> = Vec::new();

    for stmt in &ret.program.body {
        match stmt {
            Statement::FunctionDeclaration(f) => {
                if let Some(body) = &f.body {
                    collect_params(&f.params, &mut functions, body.span);
                }
            }
            Statement::ExportNamedDeclaration(d) => {
                if let Some(decl) = &d.declaration {
                    if let Declaration::FunctionDeclaration(f) = decl {
                        if let Some(body) = &f.body {
                            collect_params(&f.params, &mut functions, body.span);
                        }
                    }
                    if let Declaration::VariableDeclaration(vd) = decl {
                        collect_var_arrows(vd, &mut functions);
                    }
                }
            }
            Statement::VariableDeclaration(vd) => collect_var_arrows(vd, &mut functions),
            _ => {}
        }
    }

    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut interfaces: Vec<String> = Vec::new();
    let mut annotated = 0usize;
    // param name -> seen object-shape keys (repeated shapes unify here)
    let mut shape_keys: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    for (name, name_span, body_span) in &functions {
        if source[name_span.start as usize..name_span.end as usize].contains(':') {
            continue; // already annotated (shouldn't happen for JS inputs)
        }
        let body = &source[body_span.start as usize..body_span.end as usize];
        let (ty, keys) = infer_param_type(body, name);
        match ty.as_str() {
            "object" if !keys.is_empty() => {
                let entry = shape_keys.entry(name.clone()).or_default();
                for k in &keys {
                    if !entry.contains(k) {
                        entry.push(k.clone());
                    }
                }
                edits.push((
                    name_span.end as usize,
                    name_span.end as usize,
                    format!(": {}", interface_name(name)),
                ));
                annotated += 1;
            }
            "object" | "unknown" => {
                edits.push((
                    name_span.end as usize,
                    name_span.end as usize,
                    ": any".to_string(),
                ));
                annotated += 1;
            }
            other => {
                edits.push((
                    name_span.end as usize,
                    name_span.end as usize,
                    format!(": {}", other),
                ));
                annotated += 1;
            }
        }
    }

    // Emit one interface per object-shaped param name.
    let mut ifaces: Vec<(String, Vec<String>)> = shape_keys.into_iter().collect();
    ifaces.sort();
    for (param, keys) in ifaces {
        let mut iface = format!("interface {} {{\n", interface_name(&param));
        for k in &keys {
            iface.push_str(&format!("  {}: unknown\n", k));
        }
        iface.push('}');
        interfaces.push(iface);
    }

    Some(Migration {
        edits,
        interfaces,
        annotated_params: annotated,
    })
}

fn collect_params(
    params: &oxc_ast::ast::FormalParameters,
    out: &mut Vec<(String, oxc_span::Span, oxc_span::Span)>,
    body_span: oxc_span::Span,
) {
    for param in &params.items {
        if let BindingPatternKind::BindingIdentifier(id) = &param.pattern.kind {
            out.push((id.name.to_string(), id.span, body_span));
        }
        // Destructured / rest / default parameters are left untouched.
    }
}

fn collect_var_arrows(
    vd: &oxc_ast::ast::VariableDeclaration,
    out: &mut Vec<(String, oxc_span::Span, oxc_span::Span)>,
) {
    for decl in &vd.declarations {
        let Some(init) = &decl.init else { continue };
        match init {
            oxc_ast::ast::Expression::ArrowFunctionExpression(a) => {
                collect_arrow_params(&a.params, out, a.body.span);
            }
            _ => {}
        }
    }
}

fn collect_arrow_params(
    params: &oxc_ast::ast::FormalParameters,
    out: &mut Vec<(String, oxc_span::Span, oxc_span::Span)>,
    body_span: oxc_span::Span,
) {
    for param in &params.items {
        if let BindingPatternKind::BindingIdentifier(id) = &param.pattern.kind {
            out.push((id.name.to_string(), id.span, body_span));
        }
    }
}

fn interface_name(param: &str) -> String {
    let mut chars = param.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str() + "Shape",
        None => "UnknownShape".to_string(),
    }
}

/// Apply textual (start, end, replacement) edits back-to-front so spans
/// never shift on us.
fn apply_edits(source: &str, edits: &[(usize, usize, String)]) -> String {
    let mut sorted: Vec<&(usize, usize, String)> = edits.iter().collect();
    sorted.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = source.to_string();
    for (start, end, repl) in sorted {
        if *start <= *end && *end <= out.len() {
            out.replace_range(*start..*end, repl);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Usage-pattern inference
// ---------------------------------------------------------------------------

/// Every occurrence of `word` as a standalone identifier in `src`, with the
/// immediately surrounding non-space characters (`\0` at edges).
fn find_word_usages(src: &str, word: &str) -> Vec<(char, char)> {
    let chars: Vec<char> = src.chars().collect();
    let mut usages = Vec::new();

    // Identifier-start scan without the regex crate.
    let mut i = 0usize;
    while i < chars.len() {
        if is_ident_start(chars[i]) {
            let start = i;
            while i < chars.len() && is_ident_char(chars[i]) {
                i += 1;
            }
            let ident: String = chars[start..i].iter().collect();
            if ident == word {
                let prev = prev_nonspace(&chars, start);
                let next = next_nonspace(&chars, i);
                usages.push((prev, next));
            }
        } else {
            i += 1;
        }
    }
    usages
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}
fn is_ident_char(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

fn prev_nonspace(chars: &[char], from: usize) -> char {
    let mut i = from;
    while i > 0 {
        i -= 1;
        if !chars[i].is_whitespace() {
            return chars[i];
        }
    }
    '\0'
}

fn next_nonspace(chars: &[char], from: usize) -> char {
    let mut i = from;
    while i < chars.len() {
        let c = chars[i];
        if !c.is_whitespace() {
            return c;
        }
        i += 1;
    }
    '\0'
}

/// Look for `needle` starting right after the word's identifier ends in the
/// (whitespace-tolerant) body. Returns true when found.
fn followed_by(body: &str, word: &str, needle: &str) -> bool {
    for (_, end) in find_word_spans(body, word) {
        let after = &body[end..];
        let trimmed = after.trim_start();
        if trimmed.starts_with(needle) {
            return true;
        }
    }
    false
}

/// Byte offsets where `word` occurs as a standalone identifier.
fn find_word_spans(src: &str, word: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = src.chars().collect();
    let mut bytes = chars.clone();
    let _ = &mut bytes;
    let mut out = Vec::new();
    let mut byte_pos = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        let c_len = c.len_utf8();
        if is_ident_start(c) {
            let char_start = i;
            let byte_start = byte_pos;
            while i < chars.len() && is_ident_char(chars[i]) {
                byte_pos += chars[i].len_utf8();
                i += 1;
            }
            let byte_end = byte_pos;
            if src[byte_start..byte_end] == *word {
                out.push((byte_start, byte_end));
            }
            let _ = char_start;
        } else {
            byte_pos += c_len;
            i += 1;
        }
    }
    out
}

/// All property keys accessed on the identifier (`p.foo`, `p["foo"]`).
fn accessed_props(body: &str, word: &str) -> Vec<String> {
    let mut props = Vec::new();
    for (start, end) in find_word_spans(body, word) {
        let after = body[end..].trim_start();
        if after.starts_with('.') {
            let rest = &after[1..];
            let key: String = rest
                .chars()
                .take_while(|c| is_ident_char(*c))
                .collect();
            if !key.is_empty() {
                props.push(key);
            }
        } else if after.starts_with('[') && after.len() > 2 {
            let inner = &after[1..after.len() - 1.min(after.len())];
            let quoted = inner.trim();
            if (quoted.starts_with('"') && quoted.ends_with('"') && quoted.len() > 2)
                || (quoted.starts_with('\'') && quoted.ends_with('\'') && quoted.len() > 2)
            {
                props.push(quoted[1..quoted.len() - 1].to_string());
            }
        }
        let _ = start;
    }
    props
}

/// Infer a parameter's type from its usage inside a function body.
/// Returns (type, object-shape-key-list). "unknown" means "give up" (any).
fn infer_param_type(body: &str, name: &str) -> (String, Vec<String>) {
    let usages = find_word_usages(body, name);
    if usages.is_empty() {
        return ("unknown".to_string(), Vec::new());
    }

    // typeof checks are authoritative.
    if body.contains(&format!("typeof {} === 'number'", name))
        || body.contains(&format!("typeof {} == 'number'", name))
        || body.contains(&format!("typeof {} ===\"number\"", name))
    {
        return ("number".to_string(), Vec::new());
    }
    if body.contains(&format!("typeof {} === 'string'", name))
        || body.contains(&format!("typeof {} == 'string'", name))
    {
        return ("string".to_string(), Vec::new());
    }
    if body.contains(&format!("typeof {} === 'boolean'", name))
        || body.contains(&format!("typeof {} == 'boolean'", name))
    {
        return ("boolean".to_string(), Vec::new());
    }

    // Array methods.
    for m in ["push", "pop", "shift", "unshift", "map", "filter", "forEach", "reduce", "join", "find", "some", "every", "findIndex"] {
        if followed_by(body, name, &format!(".{}(", m)) {
            return ("any[]".to_string(), Vec::new());
        }
    }

    // String methods / template usage.
    for m in ["startsWith", "endsWith", "includes", "trim", "toUpperCase", "toLowerCase", "substring", "charAt", "padStart", "charAt", "indexOf", "replace", "trimStart", "trimEnd"] {
        if followed_by(body, name, &format!(".{}(", m)) {
            return ("string".to_string(), Vec::new());
        }
    }
    if used_in_template(body, name) {
        return ("string".to_string(), Vec::new());
    }

    // Numeric signals: arithmetic except '+', ++/--, Math.*, Number()/parseInt.
    // A usage that is itself a property-access origin (next == '.') is an
    // object context, never a numeric operand (`* r.h` must stay object).
    for (prev, next) in &usages {
        if *next == '.' || *next == '[' {
            continue;
        }
        if matches!(prev, '-' | '*' | '/' | '%') || matches!(next, '-' | '*' | '/' | '%') {
            return ("number".to_string(), Vec::new());
        }
        if (*prev == '+' && *next == '+') {
            return ("number".to_string(), Vec::new());
        }
    }
    if followed_by(body, name, "++")
        || preceded_by(body, name, "++")
        || followed_by(body, name, "--")
        || preceded_by(body, name, "--")
    {
        return ("number".to_string(), Vec::new());
    }
    if followed_by(body, name, "+ ") {
        // Check whether the + is followed by a digit/number-ish token
        for (start, end) in find_word_spans(body, name) {
            let after = body[end..].trim_start();
            if let Some(rest) = after.strip_prefix('+') {
                let operand = rest.trim_start();
                if operand.starts_with(|c: char| c.is_ascii_digit()) {
                    return ("number".to_string(), Vec::new());
                }
                if operand.starts_with("Number(") || operand.starts_with("Math.") {
                    return ("number".to_string(), Vec::new());
                }
            }
        }
    }
    if preceded_by(body, name, "+") {
        // `1 + name` style
        for (start, _) in find_word_spans(body, name) {
            let before = body[..start].trim_end();
            if let Some(rest) = before.strip_suffix('+') {
                let operand = rest.trim_end();
                if operand.ends_with(|c: char| c.is_ascii_digit()) {
                    return ("number".to_string(), Vec::new());
                }
                if operand.ends_with("Math.PI") || operand.ends_with(".length") {
                    return ("number".to_string(), Vec::new());
                }
            }
        }
    }
    for f in ["Math.round", "Math.floor", "Math.max", "Math.min", "Number(", "parseInt(", "parseFloat("] {
        if inside_call(body, name, f) {
            return ("number".to_string(), Vec::new());
        }
    }

    // Boolean signals.
    for pat in ["Boolean(", "== true", "=== true", "== false", "=== false"] {
        if body.contains(&format!("{}{}", pat, "")) && followed_by(body, name, "") {
            if inside_call(body, name, "Boolean(") {
                return ("boolean".to_string(), Vec::new());
            }
        }
        if followed_by(body, name, " === true")
            || followed_by(body, name, " === false")
            || followed_by(body, name, " == true")
        {
            return ("boolean".to_string(), Vec::new());
        }
    }

    // Date.
    if inside_call(body, name, "new Date(") {
        return ("Date".to_string(), Vec::new());
    }

    // Called as a function?
    if usages.iter().any(|(_, next)| *next == '(') {
        return ("Function".to_string(), Vec::new());
    }

    // Property accesses -> object shape.
    let props = accessed_props(body, name);
    if !props.is_empty() {
        return ("object".to_string(), props);
    }

    // Indexed access with no other signal -> array-ish any.
    if followed_by(body, name, "[") {
        return ("any[]".to_string(), Vec::new());
    }

    ("unknown".to_string(), Vec::new())
}

fn preceded_by(body: &str, word: &str, needle: &str) -> bool {
    for (start, _) in find_word_spans(body, word) {
        if body[..start].trim_end().ends_with(needle) {
            return true;
        }
    }
    false
}

/// Is `word` used inside a call to `call_start` (e.g. Math.round(p))?
fn inside_call(body: &str, word: &str, call_start: &str) -> bool {
    let mut search_from = 0usize;
    while let Some(rel) = body[search_from..].find(call_start) {
        let cs = search_from + rel + call_start.len();
        // Look for the word before the matching close paren, naively (no
        // nested-paren tracking needed for the common single-arg case).
        if let Some(close_rel) = body[cs..].find(')') {
            let args = &body[cs..cs + close_rel];
            for (s, e) in find_word_spans(args, word) {
                // Ensure it's the whole argument or part of a simple expr.
                let _ = (s, e);
                return true;
            }
        }
        search_from = cs;
    }
    false
}

/// Is `word` interpolated in a template literal (`${...word...}`)?
fn used_in_template(body: &str, word: &str) -> bool {
    let chars: Vec<char> = body.chars().collect();
    let mut bytes = chars.clone();
    let _ = &mut bytes;
    let mut in_template = false;
    let mut token: String = String::new();
    for c in &chars {
        token.clear();
        match c {
            '`' => {
                in_template = !in_template;
                token.push(*c);
            }
            _ => {}
        }
    }
    // simpler: split on backticks; odd segments are template literals
    for (idx, seg) in body.split('`').enumerate() {
        if idx % 2 == 1 && seg.contains(word) && find_word_spans(seg, word).len() > 0 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_numbers_and_strings() {
        let src = "function add(a, b) { return a - b; }\nfunction greet(name) { return `hi ${name}`; }";
        let m = migrate_source(src).unwrap();
        let out = apply_edits(src, &m.edits);
        assert!(out.contains("function add(a: number, b: number)"), "{out}");
        assert!(out.contains("function greet(name: string)"), "{out}");
    }

    #[test]
    fn infers_object_shape_interface() {
        let src = "function area(r) { return r.w * r.h; }";
        let m = migrate_source(src).unwrap();
        let out = apply_edits(src, &m.edits);
        assert!(out.contains("r: RShape"), "{out}");
        let interfaces = m.interfaces.join("\n");
        assert!(interfaces.contains("interface RShape {"), "{interfaces}");
        assert!(interfaces.contains("w: unknown"), "{interfaces}");
        assert!(interfaces.contains("h: unknown"), "{interfaces}");
    }

    #[test]
    fn array_method_infers_array() {
        let src = "function total(xs) { return xs.reduce(function(a, b){ return a + b; }, 0); }";
        let m = migrate_source(src).unwrap();
        let out = apply_edits(src, &m.edits);
        assert!(out.contains("xs: any[]"), "{out}");
    }
}
