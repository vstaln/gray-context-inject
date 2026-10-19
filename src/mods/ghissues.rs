//! Ported wholesale from gray-gh-issues. Entry points renamed: manifest->main_ghissues scheme.
//! gray-gh-issues — resolve `#N` references into issue titles on submit.
//!
//! Gray has no autocomplete wire, so this resolves references instead of
//! suggesting them. Claims
//! `input/submit` (protocol 2.0): when the submitted text contains `#<num>`
//! and the session cwd is a checkout with a GitHub remote, each issue title
//! is fetched with `gh issue view` (5s timeout, cached in-memory for the
//! process) and the input is rewritten with an appended context block:
//!
//!     Referencing issues: #12 'Fix crash (OPEN)' · #9 'Add login (MERGED)'
//!
//! `gh` missing, non-GitHub repo, or every lookup failing → pass through
//! unchanged (fail open: a broken rewriter must never eat input).
//! `/ghissues on|off` toggles.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Bound on issues resolved per submission.
const MAX_REFS: usize = 10;

fn manifest_ghissues() -> Value {
    json!({
        "name": "gh-issues",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [],
        "commands": ["/ghissues"],
        "hooks": ["input/submit"],
    })
}

fn state_dir() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("gh-issues")
}

fn enabled() -> bool {
    !state_dir().join("disabled").exists()
}

/// Ordered unique `#<num>` references in `text` (`#` inside a word like
/// `abc#12` still counts — it is how people write it).
fn find_refs(text: &str) -> Vec<u64> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' {
            let start = i + 1;
            let mut j = start;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > start
                && let Ok(n) = text[start..j].parse::<u64>()
                && !out.contains(&n)
            {
                out.push(n);
                if out.len() >= MAX_REFS {
                    break;
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// `prog args` (optionally in `cwd`), bounded by a wall-clock deadline —
/// the `timeout(1)` wrapper ported to spawn+try_wait so it also works on
/// Windows/macOS, where GNU `timeout` doesn't exist. None on spawn
/// failure or timeout (the child is killed).
fn run_with_timeout(
    prog: &str,
    args: &[&str],
    cwd: Option<&str>,
    secs: u64,
) -> Option<std::process::Output> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().ok()?;
    // Drain both pipes on threads so a chatty child can't deadlock.
    let drain = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = r.read_to_end(&mut b);
            b
        })
    };
    let out_t = child.stdout.take().map(|s| drain(Box::new(s)));
    let err_t = child.stderr.take().map(|s| drain(Box::new(s)));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(_) => return None,
        }
    };
    let stdout = out_t.and_then(|t| t.join().ok()).unwrap_or_default();
    let stderr = err_t.and_then(|t| t.join().ok()).unwrap_or_default();
    Some(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// `true` when `cwd` is a git checkout whose remotes include github.com.
fn real_github_repo(cwd: &str) -> bool {
    if !Path::new(cwd).is_dir() {
        return false;
    }
    let Some(out) = run_with_timeout("git", &["-C", cwd, "remote", "-v"], None, 5) else {
        return false;
    };
    out.status.success() && String::from_utf8_lossy(&out.stdout).contains("github.com")
}

/// `gh issue view <n>` → `"Title (STATE)"`, 5s timeout, run in `cwd`.
fn real_fetch_issue(cwd: &str, n: u64) -> Option<String> {
    let out = run_with_timeout(
        "gh",
        &[
            "issue",
            "view",
            &n.to_string(),
            "--json",
            "title,state",
            "-q",
            ".title + \" (\" + .state + \")\"",
        ],
        Some(cwd),
        5,
    )?;
    if !out.status.success() {
        return None;
    }
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

/// Resolver with per-process caches; lookups are injectable for tests.
pub struct GhIssues {
    repo: HashMap<String, bool>,
    issues: HashMap<(String, u64), Option<String>>,
    is_repo: fn(&str) -> bool,
    fetch: fn(&str, u64) -> Option<String>,
}

impl GhIssues {
    pub fn new() -> Self {
        Self {
            repo: HashMap::new(),
            issues: HashMap::new(),
            is_repo: real_github_repo,
            fetch: real_fetch_issue,
        }
    }

    fn in_github_repo(&mut self, cwd: &str) -> bool {
        *self
            .repo
            .entry(cwd.to_string())
            .or_insert_with(|| (self.is_repo)(cwd))
    }

    fn issue(&mut self, cwd: &str, n: u64) -> Option<String> {
        self.issues
            .entry((cwd.to_string(), n))
            .or_insert_with(|| (self.fetch)(cwd, n))
            .clone()
    }
}

fn on_submit(plugin: &mut GhIssues, params: &Value) -> Value {
    if !enabled() {
        return json!({});
    }
    let text = params.get("text").and_then(Value::as_str).unwrap_or("");
    let refs = find_refs(text);
    if refs.is_empty() {
        return json!({});
    }
    let Some(cwd) = params
        .get("session")
        .and_then(|s| s.get("cwd"))
        .and_then(Value::as_str)
    else {
        return json!({});
    };
    if !plugin.in_github_repo(cwd) {
        return json!({});
    }
    let resolved: Vec<String> = refs
        .iter()
        .filter_map(|n| plugin.issue(cwd, *n).map(|t| format!("#{n} '{t}'")))
        .collect();
    if resolved.is_empty() {
        return json!({});
    }
    json!({
        "text": format!("{text}\n\nReferencing issues: {}", resolved.join(" · "))
    })
}

/// `/ghissues …` — `argv` excludes the command name.
fn run_command(argv: &[&str]) -> String {
    match argv.first().copied() {
        Some("on") | Some("off") => {
            let dir = state_dir();
            let flag = dir.join("disabled");
            let res = if argv[0] == "off" {
                std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&flag, b""))
            } else {
                std::fs::remove_file(&flag).or_else(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(e)
                    }
                })
            };
            match res {
                Ok(()) => format!("gh-issues {}", if argv[0] == "off" { "off" } else { "on" }),
                Err(e) => format!("couldn't flip state: {e}"),
            }
        }
        _ => format!(
            "gray-gh-issues {} — {} · appends `gh issue view` titles when input \
             mentions #N in a GitHub checkout. /ghissues on|off",
            env!("CARGO_PKG_VERSION"),
            if enabled() { "enabled" } else { "disabled" },
        ),
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_ghissues(plugin: &mut GhIssues, req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_ghissues(),
        "input/submit" => on_submit(plugin, &params),
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

fn main_ghissues() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_ghissues());
        return Ok(());
    }
    let mut plugin = GhIssues::new();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (reply, exit) = handle_ghissues(&mut plugin, &req);
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

    /// Stubbed resolver: `/repo` is a GitHub checkout, `#1`/`#2` exist.
    fn stubbed() -> GhIssues {
        GhIssues {
            repo: HashMap::new(),
            issues: HashMap::new(),
            is_repo: |cwd| cwd == "/repo",
            fetch: |_, n| match n {
                1 => Some("Fix crash (OPEN)".into()),
                2 => Some("Add login (MERGED)".into()),
                _ => None,
            },
        }
    }

    fn call(p: &mut GhIssues, method: &str, params: Value) -> Value {
        handle_ghissues(p, &json!({ "id": 1, "method": method, "params": params }))
            .0
            .unwrap()
    }

    fn submit(p: &mut GhIssues, text: &str, cwd: &str) -> Value {
        call(
            p,
            "input/submit",
            json!({"text": text, "session": {"cwd": cwd}}),
        )
    }

    #[test]
    fn manifest_claims_input_submit_at_2_0() {
        let mut p = stubbed();
        let m = call(&mut p, "plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "gh-issues");
        assert_eq!(m["protocol"], "2.0");
        assert_eq!(m["hooks"], json!(["input/submit"]));
        assert_eq!(m["commands"], json!(["/ghissues"]));
    }

    #[test]
    fn refs_extract_dedupe_order() {
        assert_eq!(find_refs("fix #12 and #9, see #12"), vec![12, 9]);
        assert_eq!(find_refs("no refs here"), Vec::<u64>::new());
        assert_eq!(find_refs("#abc #1"), vec![1]);
    }

    #[test]
    fn rewrites_with_issue_block() {
        let mut p = stubbed();
        let r = submit(&mut p, "please fix #1", "/repo");
        let t = r["result"]["text"].as_str().unwrap();
        assert!(t.starts_with("please fix #1"));
        assert!(t.contains("Referencing issues: #1 'Fix crash (OPEN)'"));
    }

    #[test]
    fn multiple_refs_joined() {
        let mut p = stubbed();
        let r = submit(&mut p, "#1 and #2", "/repo");
        let t = r["result"]["text"].as_str().unwrap();
        assert!(t.contains("#1 'Fix crash (OPEN)' · #2 'Add login (MERGED)'"));
    }

    #[test]
    fn non_github_repo_passes() {
        let mut p = stubbed();
        let r = submit(&mut p, "fix #1", "/elsewhere");
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn unknown_issue_passes() {
        let mut p = stubbed();
        let r = submit(&mut p, "fix #404", "/repo");
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn mixed_known_unknown_keeps_known() {
        let mut p = stubbed();
        let r = submit(&mut p, "#1 vs #404", "/repo");
        let t = r["result"]["text"].as_str().unwrap();
        let block = t.split("Referencing issues: ").nth(1).unwrap();
        assert!(block.contains("#1 'Fix crash (OPEN)'") && !block.contains("#404"));
    }

    #[test]
    fn issue_lookups_are_cached() {
        let mut p = stubbed();
        p.issues
            .insert(("/repo".into(), 1), Some("cached title (OPEN)".into()));
        p.fetch = |_, _| panic!("must not be called");
        let r = submit(&mut p, "fix #1", "/repo");
        assert!(
            r["result"]["text"]
                .as_str()
                .unwrap()
                .contains("cached title")
        );
    }

    #[test]
    fn notifications_are_silent() {
        let mut p = stubbed();
        let (reply, exit) = handle_ghissues(
            &mut p,
            &json!({"method": "event/notify", "params": {"type": "turn_end"}}),
        );
        assert!(reply.is_none() && !exit);
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let mut p = stubbed();
        let (reply, exit) =
            handle_ghissues(&mut p, &json!({ "id": 2, "method": "plugin/shutdown" }));
        assert!(reply.is_some() && exit);
    }
}
