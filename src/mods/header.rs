//! Ported wholesale from gray-header. Entry points renamed: manifest->main_header scheme.
//! gray-header — inject `~/.gray/header.txt` into every turn.
//!
//! On `prompt/context` the sidecar returns the contents of
//! `<gray-home>/header.txt` — a
//! user-managed banner/preferences file ("always be terse") — or `{}` when
//! the file is missing or empty. `/header show` prints the current header;
//! `/header edit-hint` prints the file path to edit.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde_json::{Value, json};

fn manifest_header() -> Value {
    json!({
        "name": "header",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [],
        "commands": ["/header"],
        "hooks": ["prompt/context"],
    })
}

/// `<gray-home>/header.txt` — honors `$GRAY_HOME`, falls back to
/// `$HOME/.gray`.
fn header_file() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".gray")))
        .unwrap_or_else(|| PathBuf::from(".gray"))
        .join("header.txt")
}

/// The header text to inject, or `None` when the file is missing or blank.
fn header_text() -> Option<String> {
    let body = std::fs::read_to_string(header_file()).ok()?;
    let trimmed = body.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// `/header …` — `argv` excludes the command name.
fn run_command(argv: &[&str]) -> String {
    match argv.first().copied() {
        Some("show") => match header_text() {
            Some(text) => format!("{}\n\n(from {})", text, header_file().display()),
            None => format!("no header set — {}", header_file().display()),
        },
        Some("edit-hint") => format!("edit {}", header_file().display()),
        _ => format!(
            "gray-header {} — injects {} into every turn. /header show · /header edit-hint",
            env!("CARGO_PKG_VERSION"),
            header_file().display()
        ),
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_header(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_header(),
        "prompt/context" => match header_text() {
            Some(text) => json!({ "text": text }),
            None => json!({}),
        },
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({ "text": run_command(&argv) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main_header() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_header());
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (reply, exit) = handle_header(&req);
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
        handle_header(&json!({ "id": 1, "method": method, "params": params }))
            .0
            .unwrap()
    }

    /// A fresh temp dir that removes itself on drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "gray-header-test-{}-{}",
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

    /// Env vars are process-global: serialize the tests that set GRAY_HOME.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Point GRAY_HOME at `dir` for the duration of a test; restores on drop.
    struct EnvGuard;
    impl EnvGuard {
        fn set_home(dir: &TempDir) -> Self {
            // SAFETY: tests run in one process; serialize via the env var
            // itself being per-test unique values.
            unsafe { std::env::set_var("GRAY_HOME", &dir.0) };
            Self
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("GRAY_HOME") };
        }
    }

    #[test]
    fn manifest_claims_prompt_context_and_header_command() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "header");
        assert_eq!(m["hooks"], json!(["prompt/context"]));
        assert_eq!(m["commands"], json!(["/header"]));
    }

    #[test]
    fn prompt_context_is_empty_when_file_missing() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new();
        let _g = EnvGuard::set_home(&dir);
        let r = call("prompt/context", json!({ "session": {} }));
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn prompt_context_returns_file_contents() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new();
        let _g = EnvGuard::set_home(&dir);
        std::fs::write(dir.0.join("header.txt"), "always be terse\n").unwrap();
        let r = call("prompt/context", json!({ "session": {} }));
        assert_eq!(r["result"]["text"], "always be terse");
    }

    #[test]
    fn prompt_context_is_empty_when_file_blank() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new();
        let _g = EnvGuard::set_home(&dir);
        std::fs::write(dir.0.join("header.txt"), "  \n\n").unwrap();
        let r = call("prompt/context", json!({ "session": {} }));
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn header_show_and_edit_hint() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new();
        let _g = EnvGuard::set_home(&dir);
        let r = call(
            "command/run",
            json!({ "name": "/header", "argv": ["show"] }),
        );
        assert!(
            r["result"]["text"]
                .as_str()
                .unwrap()
                .contains("no header set")
        );
        std::fs::write(dir.0.join("header.txt"), "banner!").unwrap();
        let r = call(
            "command/run",
            json!({ "name": "/header", "argv": ["show"] }),
        );
        assert!(r["result"]["text"].as_str().unwrap().contains("banner!"));
        let r = call(
            "command/run",
            json!({ "name": "/header", "argv": ["edit-hint"] }),
        );
        assert!(r["result"]["text"].as_str().unwrap().contains("header.txt"));
    }

    #[test]
    fn shutdown_replies_then_exits_and_notifications_are_silent() {
        let (reply, exit) = handle_header(&json!({ "id": 2, "method": "plugin/shutdown" }));
        assert!(reply.is_some() && exit);
        let (reply, exit) = handle_header(&json!({ "method": "plugin/shutdown" }));
        assert!(reply.is_none() && exit);
    }

    #[test]
    fn unknown_methods_are_method_not_found() {
        assert_eq!(call("nope", Value::Null)["error"]["code"], -32601);
    }
}
