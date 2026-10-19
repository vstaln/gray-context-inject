//! Ported wholesale from gray-rules. Entry points renamed: manifest->main_rules scheme.
//! gray-rules — inject rule files into the session before the first request.
//!
//! On `agent/before_start` the sidecar reads, in order,
//! `~/.claude/CLAUDE.md`, `<session.cwd>/CLAUDE.md`, and
//! `<session.cwd>/AGENTS.md`, and returns their concatenated contents
//! (capped at 16 KiB) as the pre-turn user message. `/rules` shows which
//! files would be injected for the session.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Total byte cap across all injected rule files.
const CAP: usize = 16 * 1024;

fn manifest_rules() -> Value {
    json!({
        "name": "rules",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [],
        "commands": ["/rules"],
        "hooks": ["agent/before_start"],
    })
}

/// The candidate rule files, in injection order.
fn candidates(cwd: &Path) -> Vec<PathBuf> {
    let mut v = Vec::with_capacity(3);
    if let Some(home) = std::env::home_dir() {
        v.push(home.join(".claude").join("CLAUDE.md"));
    }
    v.push(cwd.join("CLAUDE.md"));
    v.push(cwd.join("AGENTS.md"));
    v
}

/// Existing rule files, deduped by canonical path (covers the edge case of
/// a session rooted at `~/.claude` itself).
fn existing(cwd: &Path) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    candidates(cwd)
        .into_iter()
        .filter(|p| p.is_file())
        .filter(|p| {
            let key = p.canonicalize().unwrap_or_else(|_| p.clone());
            seen.insert(key)
        })
        .collect()
}

/// Truncate at `max` bytes on a char boundary.
fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// The concatenated rule text to inject, or `None` when nothing exists.
/// Per-file `## path` headers keep multi-file output attributable.
fn inject_text(cwd: &Path) -> Option<String> {
    let mut out = String::new();
    for file in existing(cwd) {
        if out.len() >= CAP {
            break;
        }
        let Ok(body) = std::fs::read_to_string(&file) else {
            continue;
        };
        if body.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("## ");
        out.push_str(&file.display().to_string());
        out.push_str("\n\n");
        out.push_str(body.trim_end());
        truncate_utf8(&mut out, CAP);
    }
    if out.is_empty() { None } else { Some(out) }
}

fn session_cwd(params: &Value) -> PathBuf {
    params["session"]["cwd"]
        .as_str()
        .or_else(|| params["cwd"].as_str())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `/rules` — `argv` excludes the command name.
fn run_command(cwd: &Path) -> String {
    let found = existing(cwd);
    if found.is_empty() {
        return format!(
            "no rule files found for {}\n\
             looked for: {}",
            cwd.display(),
            candidates(cwd)
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let mut lines = vec![format!("rules injected for {}:", cwd.display())];
    for f in &found {
        let size = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
        lines.push(format!("  {} ({size} B)", f.display()));
    }
    let injected: u64 = found
        .iter()
        .map(|f| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0))
        .sum();
    if injected as usize > CAP {
        lines.push(format!("  (truncated to {CAP} B total)"));
    }
    lines.join("\n")
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_rules(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_rules(),
        "agent/before_start" => match inject_text(&session_cwd(&params)) {
            Some(text) => json!({ "text": text }),
            None => json!({}),
        },
        "command/run" => json!({ "text": run_command(&session_cwd(&params)) }),
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main_rules() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_rules());
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (reply, exit) = handle_rules(&req);
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
        handle_rules(&json!({ "id": 1, "method": method, "params": params }))
            .0
            .unwrap()
    }

    /// A fresh temp dir that removes itself on drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "gray-rules-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn manifest_claims_before_start_and_rules_command() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "rules");
        assert_eq!(m["protocol"], "2.0");
        assert_eq!(m["hooks"], json!(["agent/before_start"]));
        assert_eq!(m["commands"], json!(["/rules"]));
    }

    #[test]
    fn before_start_is_empty_when_no_files() {
        let dir = TempDir::new();
        let r = call(
            "agent/before_start",
            json!({ "session": { "id": "s", "cwd": dir.0 } }),
        );
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn before_start_concatenates_existing_files() {
        let dir = TempDir::new();
        std::fs::write(dir.0.join("CLAUDE.md"), "claude rules").unwrap();
        std::fs::write(dir.0.join("AGENTS.md"), "agents rules").unwrap();
        let r = call(
            "agent/before_start",
            json!({ "session": { "id": "s", "cwd": dir.0 } }),
        );
        let text = r["result"]["text"].as_str().unwrap();
        assert!(text.contains("claude rules") && text.contains("agents rules"));
        assert!(text.find("claude rules").unwrap() < text.find("agents rules").unwrap());
    }

    #[test]
    fn before_start_caps_at_16kib() {
        let dir = TempDir::new();
        std::fs::write(dir.0.join("CLAUDE.md"), "x".repeat(CAP + 4096)).unwrap();
        let r = call(
            "agent/before_start",
            json!({ "session": { "id": "s", "cwd": dir.0 } }),
        );
        let text = r["result"]["text"].as_str().unwrap();
        assert!(text.len() <= CAP);
    }

    #[test]
    fn rules_command_lists_found_and_missing() {
        let dir = TempDir::new();
        let r = call(
            "command/run",
            json!({ "name": "/rules", "argv": [], "session": { "cwd": dir.0 } }),
        );
        assert!(
            r["result"]["text"]
                .as_str()
                .unwrap()
                .contains("no rule files")
        );
        std::fs::write(dir.0.join("AGENTS.md"), "rules!").unwrap();
        let r = call(
            "command/run",
            json!({ "name": "/rules", "argv": [], "session": { "cwd": dir.0 } }),
        );
        assert!(r["result"]["text"].as_str().unwrap().contains("AGENTS.md"));
    }

    #[test]
    fn shutdown_replies_then_exits_and_notifications_are_silent() {
        let (reply, exit) = handle_rules(&json!({ "id": 2, "method": "plugin/shutdown" }));
        assert!(reply.is_some() && exit);
        let (reply, exit) = handle_rules(&json!({ "method": "plugin/shutdown" }));
        assert!(reply.is_none() && exit);
    }

    #[test]
    fn unknown_methods_are_method_not_found() {
        assert_eq!(call("nope", Value::Null)["error"]["code"], -32601);
    }
}
