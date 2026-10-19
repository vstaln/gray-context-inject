//! Ported wholesale from gray-include. Entry points renamed: manifest->main_include scheme.
//! gray-include — `@path` includes in AGENTS.md / CLAUDE.md.
//!
//! On every `prompt/context`
//! request the sidecar reads AGENTS.md and CLAUDE.md in the session's cwd,
//! finds `@path/to/file` tokens (line-level and inline), reads the files,
//! recursively expands includes inside included files (depth cap 8, cycle
//! set on canonical paths), and returns `{text}` with an
//! "## Included context" section. Binary or missing targets become inline
//! notes instead of errors; anything unexpected yields `{}` — fail open.
//!
//! `/include lint` lists every `@token` found and whether it resolved.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

const MAX_DEPTH: u32 = 8;
const TOTAL_CAP: usize = 40_000;

fn manifest_include() -> Value {
    json!({
        "name": "include",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [],
        "commands": ["/include"],
        "hooks": ["prompt/context"],
    })
}

// ── token extraction ──────────────────────────────────────────

/// Chars that can appear inside a `@path` token.
fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~' | '+' | '@')
}

/// Does the captured token look like a file path (not an email fragment)?
fn plausible_path(tok: &str) -> bool {
    tok.contains('/')
        || tok.starts_with('~')
        || Path::new(tok).extension().is_some_and(|e| !e.is_empty())
}

/// Extract `@path` tokens in order, deduplicated. Tokens may appear anywhere
/// a word boundary allows (line start, after whitespace/punctuation) but not
/// inside fenced code blocks or glued to a word char (emails).
fn extract_tokens(content: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let mut seen = HashSet::new();
    let mut fenced = false;
    for line in content.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let bytes: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != '@' {
                i += 1;
                continue;
            }
            let ok_prev = i == 0
                || bytes[i - 1].is_whitespace()
                || matches!(bytes[i - 1], '(' | '[' | '{' | '"' | '\'' | '`' | '<');
            i += 1;
            let start = i;
            while i < bytes.len() && is_path_char(bytes[i]) && bytes[i] != '@' {
                i += 1;
            }
            let tok: String = bytes[start..i].iter().collect();
            let tok = tok.trim_end_matches(['.', '/']);
            if ok_prev && plausible_path(tok) && !tok.is_empty() && seen.insert(tok.to_string()) {
                refs.push(tok.to_string());
            }
        }
    }
    refs
}

// ── resolution ────────────────────────────────────────────────

fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

fn resolve(base: &Path, token: &str) -> PathBuf {
    if let Some(rest) = token.strip_prefix("~/") {
        if let Some(h) = home_dir() {
            return h.join(rest);
        }
    }
    let p = Path::new(token);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// Short display path: relative to the session cwd when possible.
fn display(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .map(|r| r.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

enum Block {
    /// `### @disp` + body
    File { disp: String, body: String },
    /// `### @disp` + italic note (missing/binary/depth)
    Note { disp: String, note: String },
}

struct Scan {
    cwd: PathBuf,
    visited: HashSet<PathBuf>,
    blocks: Vec<Block>,
}

impl Scan {
    fn walk(&mut self, token: &str, base: &Path, depth: u32) {
        let resolved = resolve(base, token);
        let disp = display(&self.cwd, &resolved);
        if depth > MAX_DEPTH {
            self.blocks.push(Block::Note {
                disp,
                note: "depth cap reached".into(),
            });
            return;
        }
        let key = resolved.canonicalize().unwrap_or_else(|_| resolved.clone());
        if !self.visited.insert(key) {
            return; // cycle or duplicate — silent
        }
        if depth == 0 && resolved.is_dir() {
            // top-level context files are dirs-checked by caller; ignore
        }
        let bytes = match std::fs::read(&resolved) {
            Ok(b) => b,
            Err(_) => {
                self.blocks.push(Block::Note {
                    disp,
                    note: "not found".into(),
                });
                return;
            }
        };
        if bytes.contains(&0) {
            self.blocks.push(Block::Note {
                disp,
                note: "binary file".into(),
            });
            return;
        }
        let body = String::from_utf8_lossy(&bytes).into_owned();
        let nested = extract_tokens(&body);
        self.blocks.push(Block::File { disp, body });
        let parent = resolved
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| base.to_path_buf());
        for tok in nested {
            self.walk(&tok, &parent, depth + 1);
        }
    }
}

/// Read AGENTS.md and CLAUDE.md in `cwd` (both, when both exist) and expand
/// every `@path` token. Returns the rendered context text, or `None` when
/// nothing was found.
fn build_context(cwd: &Path) -> Option<String> {
    let mut scan = Scan {
        cwd: cwd.to_path_buf(),
        visited: HashSet::new(),
        blocks: Vec::new(),
    };
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let f = cwd.join(name);
        let Ok(bytes) = std::fs::read(&f) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        // Register the context file itself so a self-reference is a no-op.
        if let Ok(k) = f.canonicalize() {
            scan.visited.insert(k);
        }
        let content = String::from_utf8_lossy(&bytes).into_owned();
        for tok in extract_tokens(&content) {
            scan.walk(&tok, cwd, 1);
        }
    }
    if scan.blocks.is_empty() {
        return None;
    }
    Some(render(&scan.blocks))
}

fn render(blocks: &[Block]) -> String {
    let mut out = String::from("## Included context\n");
    let mut truncated = false;
    for b in blocks {
        if truncated {
            break;
        }
        let (disp, chunk) = match b {
            Block::File { disp, body } => {
                (disp.clone(), format!("````\n{}\n````", body.trim_end()))
            }
            Block::Note { disp, note } => (disp.clone(), format!("_{note}_")),
        };
        let piece = format!("\n### @{disp}\n{chunk}\n");
        if out.len() + piece.len() > TOTAL_CAP {
            let remaining = TOTAL_CAP.saturating_sub(out.len());
            out.push_str(&piece[..remaining.min(piece.len())]);
            out.push_str("\n[… included context truncated at 40k chars …]\n");
            truncated = true;
        } else {
            out.push_str(&piece);
        }
    }
    out
}

// ── /include lint ─────────────────────────────────────────────

fn lint(cwd: &Path) -> String {
    let mut lines = Vec::new();
    let mut found_any_file = false;
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let f = cwd.join(name);
        let Ok(bytes) = std::fs::read(&f) else {
            continue;
        };
        found_any_file = true;
        let content = String::from_utf8_lossy(&bytes).into_owned();
        let tokens = extract_tokens(&content);
        lines.push(format!("{name}: {} @token(s)", tokens.len()));
        for tok in tokens {
            let resolved = resolve(cwd, &tok);
            let status = if resolved.is_file() {
                match std::fs::read(&resolved) {
                    Ok(b) if b.contains(&0) => "binary (skipped)".to_string(),
                    Ok(_) => format!("ok → {}", display(cwd, &resolved)),
                    Err(e) => format!("unreadable: {e}"),
                }
            } else {
                "missing".to_string()
            };
            lines.push(format!("  @{tok} — {status}"));
        }
    }
    if !found_any_file {
        return "no AGENTS.md or CLAUDE.md in the session directory".into();
    }
    lines.join("\n")
}

fn run_command(argv: &[&str], params: &Value) -> String {
    let cwd = params
        .get("session")
        .and_then(|s| s.get("cwd"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    match argv.first().copied() {
        Some("lint") => lint(&cwd),
        Some(other) => format!("unknown subcommand: {other} (try /include lint)"),
        None => format!(
            "gray-include {} — expands @path tokens in AGENTS.md/CLAUDE.md into prompt context. \
             /include lint lists tokens and resolution status.",
            env!("CARGO_PKG_VERSION")
        ),
    }
}

// ── wire ──────────────────────────────────────────────────────

pub fn handle_include(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_include(),
        "prompt/context" => {
            // Fail open: any surprise → keep going with no injected context.
            let cwd = params
                .get("session")
                .and_then(|s| s.get("cwd"))
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            match std::panic::catch_unwind(|| build_context(&cwd)) {
                Ok(Some(text)) => json!({ "text": text }),
                _ => json!({}),
            }
        }
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({ "text": run_command(&argv, &params) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main_include() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_include());
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (reply, exit) = handle_include(&req);
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
        if exit {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> Value {
        handle_include(&json!({ "id": 1, "method": method, "params": params }))
            .0
            .unwrap()
    }

    #[test]
    fn manifest_claims_prompt_context_hook() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "include");
        assert_eq!(m["hooks"], json!(["prompt/context"]));
    }

    #[test]
    fn tokens_line_level_and_inline() {
        let c = "# Rules\n@docs/a.md\n- @b.md\nsee also @dir/c.md here\nmail me at admin@x.md\n```\n@hidden.md\n```\n";
        assert_eq!(extract_tokens(c), vec!["docs/a.md", "b.md", "dir/c.md"]);
    }

    #[test]
    fn tokens_dedupe_and_skip_fences() {
        assert_eq!(
            extract_tokens("@a.md\n@a.md\n~~~\n@z.md\n~~~"),
            vec!["a.md"]
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("grayinc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn context_expands_includes_recursively() {
        let d = scratch("rec");
        std::fs::write(d.join("AGENTS.md"), "rules\n@sub/more.md\n").unwrap();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/more.md"), "inner\n@leaf.md\n").unwrap();
        std::fs::write(d.join("sub/leaf.md"), "leaf body\n").unwrap();
        let text = build_context(&d).unwrap();
        assert!(text.contains("## Included context"));
        assert!(text.contains("### @sub/more.md") && text.contains("inner"));
        assert!(text.contains("### @sub/leaf.md") && text.contains("leaf body"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cycles_terminate_and_missing_become_notes() {
        let d = scratch("cyc");
        std::fs::write(d.join("AGENTS.md"), "@a.md @nope.md\n").unwrap();
        std::fs::write(d.join("a.md"), "@b.md\n").unwrap();
        std::fs::write(d.join("b.md"), "@a.md\n").unwrap();
        let text = build_context(&d).unwrap();
        assert!(text.contains("_not found_"));
        assert!(text.matches("### @a.md").count() == 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn empty_session_dir_returns_no_text() {
        let d = scratch("empty");
        let r = call("prompt/context", json!({ "session": { "cwd": &d } }));
        assert_eq!(r["result"], json!({}));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn lint_lists_tokens_and_status() {
        let d = scratch("lint");
        std::fs::write(d.join("AGENTS.md"), "@ok.md @gone.md\n").unwrap();
        std::fs::write(d.join("ok.md"), "x").unwrap();
        let r = call(
            "command/run",
            json!({ "name": "/include", "argv": ["lint"], "session": { "cwd": &d } }),
        );
        let t = r["result"]["text"].as_str().unwrap();
        assert!(t.contains("@ok.md — ok") && t.contains("@gone.md — missing"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) = handle_include(&json!({ "id": 2, "method": "plugin/shutdown" }));
        assert!(reply.is_some() && exit);
    }
}
