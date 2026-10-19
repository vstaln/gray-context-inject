//! Ported wholesale from gray-dirty-guard. Entry points renamed: manifest->main_dirty scheme.
//! gray-dirty-guard — tell the model the work tree is dirty before it edits.
//!
//! The sidecar has no UI to block behind, so it answers
//! `prompt/context` with a single line —
//! "Note: the working tree has N uncommitted changes (M modified, U untracked)."
//! — so the model knows the repo state before touching files. Outside a git
//! work tree it returns `{}` and stays silent.
//!
//! `/dirty on|off` toggles, persisted at ~/.gray/dirty-guard/disabled.
//! Default ON (the hook only informs, never blocks).

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

fn manifest_dirty() -> Value {
    json!({
        "name": "dirty-guard",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [],
        "commands": ["/dirty"],
        "hooks": ["prompt/context"],
    })
}

fn state_dir() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("dirty-guard")
}

fn enabled_in(dir: &Path) -> bool {
    !dir.join("disabled").exists()
}

fn enabled() -> bool {
    enabled_in(&state_dir())
}

/// Run `git -C <cwd> <args>`; stdout on success, None on any failure.
fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn in_work_tree(cwd: &Path) -> bool {
    git(cwd, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

/// One-line dirty-tree note, or None when `cwd` isn't a work tree, the
/// status can't be read, or the tree is clean.
fn dirty_note(cwd: &Path) -> Option<String> {
    if !in_work_tree(cwd) {
        return None;
    }
    let status = git(cwd, &["status", "--porcelain"])?;
    let mut modified = 0usize;
    let mut untracked = 0usize;
    for line in status.lines().filter(|l| !l.trim().is_empty()) {
        // `??` marks untracked paths; every other XY code is a tracked
        // change (staged, unstaged, renamed, conflicted).
        if line.starts_with("??") {
            untracked += 1;
        } else {
            modified += 1;
        }
    }
    let total = modified + untracked;
    if total == 0 {
        return None;
    }
    Some(format!(
        "Note: the working tree has {total} uncommitted changes \
         ({modified} modified, {untracked} untracked)."
    ))
}

/// `/dirty …` — `argv` excludes the command name.
fn run_command(argv: &[&str], dir: &Path) -> String {
    match argv.first().copied() {
        Some("off") => match std::fs::create_dir_all(dir)
            .and_then(|_| std::fs::write(dir.join("disabled"), b""))
        {
            Ok(()) => "dirty-guard off".into(),
            Err(e) => format!("couldn't disable: {e}"),
        },
        Some("on") => match std::fs::remove_file(dir.join("disabled")).or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e)
            }
        }) {
            Ok(()) => "dirty-guard on".into(),
            Err(e) => format!("couldn't enable: {e}"),
        },
        _ => format!(
            "gray-dirty-guard {} — {} — injects a one-line uncommitted-changes note \
             at turn start. /dirty on|off",
            env!("CARGO_PKG_VERSION"),
            if enabled_in(dir) { "on" } else { "off" },
        ),
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_dirty(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_dirty(),
        "prompt/context" => {
            if !enabled() {
                json!({})
            } else {
                let cwd = params
                    .get("session")
                    .and_then(|s| s.get("cwd"))
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| PathBuf::from("."));
                match dirty_note(&cwd) {
                    Some(text) => json!({ "text": text }),
                    None => json!({}),
                }
            }
        }
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({ "text": run_command(&argv, &state_dir()) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main_dirty() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_dirty());
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (reply, exit) = handle_dirty(&req);
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
        handle_dirty(&json!({ "id": 1, "method": method, "params": params }))
            .0
            .unwrap()
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gray-dirty-guard-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tmprepo(tag: &str) -> PathBuf {
        let d = tmpdir(tag);
        assert!(git(&d, &["init", "-q"]).is_some());
        d
    }

    #[test]
    fn manifest_claims_prompt_context_hook() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "dirty-guard");
        assert_eq!(m["protocol"], "1.1");
        assert_eq!(m["hooks"], json!(["prompt/context"]));
        assert_eq!(m["commands"], json!(["/dirty"]));
        assert_eq!(m["tools"], json!([]));
    }

    #[test]
    fn note_counts_modified_and_untracked() {
        let repo = tmprepo("note");
        std::fs::write(repo.join("untracked.txt"), "x").unwrap();
        std::fs::write(repo.join("tracked.txt"), "x").unwrap();
        git(&repo, &["add", "tracked.txt"]);
        git(
            &repo,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "i",
            ],
        );
        std::fs::write(repo.join("tracked.txt"), "y").unwrap();
        let note = dirty_note(&repo).unwrap();
        assert_eq!(
            note,
            "Note: the working tree has 2 uncommitted changes (1 modified, 1 untracked)."
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn clean_repo_and_non_repo_give_no_note() {
        let repo = tmprepo("clean");
        assert_eq!(dirty_note(&repo), None);
        let outside = tmpdir("outside");
        assert_eq!(dirty_note(&outside), None);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn prompt_context_returns_text_or_empty() {
        // Whatever the real cwd is, the reply must be {} or {text: String}.
        let r = call(
            "prompt/context",
            json!({"session": {"id": "s", "cwd": "/nonexistent"}}),
        );
        let result = &r["result"];
        assert!(result.get("text").is_none() || result["text"].is_string());
    }

    #[test]
    fn toggle_persists_under_state_dir() {
        let d = tmpdir("toggle");
        assert!(enabled_in(&d));
        assert!(run_command(&["off"], &d).contains("off"));
        assert!(!enabled_in(&d));
        assert!(run_command(&["on"], &d).contains("on"));
        assert!(enabled_in(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn notifications_are_silent_and_shutdown_exits() {
        let (reply, exit) =
            handle_dirty(&json!({"method":"event/notify","params":{"type":"turn_end"}}));
        assert!(reply.is_none() && !exit);
        let (reply, exit) = handle_dirty(&json!({ "id": 2, "method": "plugin/shutdown" }));
        assert!(reply.is_some() && exit);
    }
}
