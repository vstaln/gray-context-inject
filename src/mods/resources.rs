//! Ported wholesale from gray-resources. Entry points renamed: manifest->main_resources scheme.
//! gray-resources — pin files as always-on context.
//!
//! A registry at `<gray-home>/resources/list.json` (`{"files":[…]}`) may
//! hold absolute or session-cwd-relative paths. On every `context/build` the
//! sidecar prepends one synthetic user message — `The following files were
//! pinned as resources:` plus `### path` sections — to the outbound
//! messages (24 KiB cap, binaries skipped).
//! `/resources add <path>` · `/resources rm <path>` · `/resources` list.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Total byte cap on the synthetic pinned-resources message.
const CAP: usize = 24 * 1024;
/// Bytes sniffed for NUL when deciding whether a file is binary.
const BINARY_SNIFF: usize = 8 * 1024;

fn manifest_resources() -> Value {
    json!({
        "name": "resources",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [],
        "commands": ["/resources"],
        "hooks": ["context/build"],
    })
}

/// `<gray-home>/resources` — honors `$GRAY_HOME`, falls back to
/// `$HOME/.gray`.
fn state_dir() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from(".gray"))
        .join("resources")
}

fn list_file() -> PathBuf {
    state_dir().join("list.json")
}

fn load_list() -> Vec<String> {
    std::fs::read_to_string(list_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("files")?.as_array().map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
        })
        .unwrap_or_default()
}

fn save_list(files: &[String]) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir())?;
    let body = serde_json::to_string_pretty(&json!({ "files": files }))?;
    std::fs::write(list_file(), body)
}

/// Expand a leading `~` / `~/…` against `$HOME`.
fn expand_tilde(p: &str) -> String {
    if p == "~" || p.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}{}", Path::new(&home).display(), &p[1..]);
        }
    }
    p.to_string()
}

/// Resolve a registry entry to an absolute path (cwd-relative ok), then
/// canonicalize when the file exists — the dedupe key.
fn resolve(entry: &str, cwd: &Path) -> PathBuf {
    let expanded = expand_tilde(entry);
    let p = Path::new(&expanded);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    abs.canonicalize().unwrap_or(abs)
}

fn session_cwd(params: &Value) -> PathBuf {
    params["session"]["cwd"]
        .as_str()
        .or_else(|| params["cwd"].as_str())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Read a pinned file; `None` = binary or unreadable (with a reason).
fn read_resource(path: &Path) -> Result<String, &'static str> {
    let bytes = std::fs::read(path).map_err(|_| "unreadable")?;
    let sniff = &bytes[..bytes.len().min(BINARY_SNIFF)];
    if sniff.contains(&0) {
        return Err("binary");
    }
    String::from_utf8(bytes).map_err(|_| "binary")
}

/// `context/build`: prepend one synthetic user message carrying every pinned
/// file's contents. Empty registry → `{}` (keep the outbound list).
fn context_build(params: &Value) -> Value {
    let files = load_list();
    if files.is_empty() {
        return json!({});
    }
    let cwd = session_cwd(params);
    let mut body = String::from("The following files were pinned as resources:\n\n");
    for entry in &files {
        if body.len() >= CAP {
            break;
        }
        let path = resolve(entry, &cwd);
        body.push_str("### ");
        body.push_str(&path.display().to_string());
        body.push('\n');
        match read_resource(&path) {
            Ok(text) => {
                body.push_str(text.trim_end());
            }
            Err(why) => {
                body.push_str("(skipped: ");
                body.push_str(why);
                body.push(')');
            }
        }
        body.push_str("\n\n");
        // Truncate on a char boundary.
        if body.len() > CAP {
            let mut end = CAP;
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            body.truncate(end);
        }
    }
    let synthetic = json!({ "role": "user", "content": body.trim_end() });
    let mut messages = vec![synthetic];
    if let Some(rest) = params.get("messages").and_then(Value::as_array) {
        messages.extend(rest.iter().cloned());
    }
    json!({ "messages": messages })
}

/// `/resources …` — `argv` excludes the command name.
fn run_command(argv: &[&str], cwd: &Path) -> String {
    let sub = argv.first().copied().unwrap_or("");
    let target = argv.get(1..).map(|a| a.join(" ")).unwrap_or_default();
    match sub {
        "add" => {
            if target.is_empty() {
                return "usage: /resources add <path>".into();
            }
            let entry = expand_tilde(&target);
            let key = resolve(&entry, cwd);
            let mut files = load_list();
            if files.iter().any(|e| resolve(e, cwd) == key) {
                return format!("already pinned: {}", key.display());
            }
            files.push(entry);
            match save_list(&files) {
                Ok(()) => format!("pinned {} ({} total)", key.display(), files.len()),
                Err(e) => format!("couldn't save registry: {e}"),
            }
        }
        "rm" | "remove" => {
            if target.is_empty() {
                return "usage: /resources rm <path>".into();
            }
            let key = resolve(&expand_tilde(&target), cwd);
            let files = load_list();
            let kept: Vec<String> = files
                .iter()
                .filter(|e| resolve(e, cwd) != key && **e != target)
                .cloned()
                .collect();
            if kept.len() == files.len() {
                return format!("not pinned: {target}");
            }
            match save_list(&kept) {
                Ok(()) => format!("unpinned {} ({} left)", key.display(), kept.len()),
                Err(e) => format!("couldn't save registry: {e}"),
            }
        }
        "" | "list" => {
            let files = load_list();
            if files.is_empty() {
                return format!(
                    "no pinned resources — /resources add <path> \
                     (registry: {})",
                    list_file().display()
                );
            }
            let mut lines = vec![format!("{} pinned resource(s):", files.len())];
            for e in &files {
                let p = resolve(e, cwd);
                let status = if p.is_file() { "" } else { "  (missing)" };
                lines.push(format!("  {e} → {}{status}", p.display()));
            }
            lines.join("\n")
        }
        _ => "usage: /resources [add <path> | rm <path> | list]".into(),
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_resources(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_resources(),
        "context/build" => context_build(&params),
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({ "text": run_command(&argv, &session_cwd(&params)) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main_resources() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_resources());
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let (reply, exit) = handle_resources(&req);
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
        handle_resources(&json!({ "id": 1, "method": method, "params": params })).0.unwrap()
    }

    /// A fresh temp dir that removes itself on drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "gray-resources-test-{}-{}",
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
    struct EnvGuard;
    impl EnvGuard {
        fn set_home(dir: &TempDir) -> Self {
            unsafe { std::env::set_var("GRAY_HOME", &dir.0) };
            Self
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("GRAY_HOME") };
        }
    }

    fn build(dir: &TempDir, messages: Value) -> Value {
        call(
            "context/build",
            json!({ "messages": messages, "session": { "id": "s", "cwd": dir.0 } }),
        )
    }

    #[test]
    fn manifest_claims_context_build_and_resources_command() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "resources");
        assert_eq!(m["protocol"], "2.0");
        assert_eq!(m["hooks"], json!(["context/build"]));
        assert_eq!(m["commands"], json!(["/resources"]));
    }

    #[test]
    fn empty_registry_keeps_messages() {
        let _lock = ENV_LOCK.lock().unwrap();
        let home = TempDir::new();
        let _g = EnvGuard::set_home(&home);
        let dir = TempDir::new();
        let r = build(&dir, json!([{ "role": "user", "content": "hi" }]));
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn pinned_files_are_prepended_as_one_user_message() {
        let _lock = ENV_LOCK.lock().unwrap();
        let home = TempDir::new();
        let _g = EnvGuard::set_home(&home);
        let dir = TempDir::new();
        std::fs::write(dir.0.join("notes.md"), "remember this").unwrap();
        let r = call(
            "command/run",
            json!({ "name": "/resources", "argv": ["add", "notes.md"],
                    "session": { "cwd": dir.0 } }),
        );
        assert!(r["result"]["text"].as_str().unwrap().contains("pinned"));
        let r = build(&dir, json!([{ "role": "user", "content": "hi" }]));
        let msgs = r["result"]["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        let content = msgs[0]["content"].as_str().unwrap();
        assert!(content.starts_with("The following files were pinned as resources:"));
        assert!(content.contains("### ") && content.contains("remember this"));
        assert_eq!(msgs[1]["content"], "hi");
    }

    #[test]
    fn binaries_are_skipped_and_missing_marked() {
        let _lock = ENV_LOCK.lock().unwrap();
        let home = TempDir::new();
        let _g = EnvGuard::set_home(&home);
        let dir = TempDir::new();
        std::fs::write(dir.0.join("blob.bin"), b"a\x00b").unwrap();
        save_list(&["blob.bin".into(), "gone.txt".into()]).unwrap();
        let r = build(&dir, json!([]));
        let content = r["result"]["messages"][0]["content"].as_str().unwrap();
        assert!(content.contains("skipped: binary"));
        assert!(content.contains("skipped: unreadable"));
        assert!(!content.contains("a\x00b"));
    }

    #[test]
    fn dedupes_and_removes() {
        let _lock = ENV_LOCK.lock().unwrap();
        let home = TempDir::new();
        let _g = EnvGuard::set_home(&home);
        let dir = TempDir::new();
        std::fs::write(dir.0.join("f.md"), "x").unwrap();
        let add = |arg: &str| {
            call(
                "command/run",
                json!({ "name": "/resources", "argv": ["add", arg],
                        "session": { "cwd": dir.0 } }),
            )["result"]["text"].as_str().unwrap().to_string()
        };
        assert!(add("f.md").contains("1 total"));
        assert!(add("./f.md").contains("already pinned"));
        assert!(add("f.md").contains("already pinned"));
        let r = call(
            "command/run",
            json!({ "name": "/resources", "argv": ["rm", "f.md"],
                    "session": { "cwd": dir.0 } }),
        );
        assert!(r["result"]["text"].as_str().unwrap().contains("unpinned"));
        assert!(load_list().is_empty());
        let r = call(
            "command/run",
            json!({ "name": "/resources", "argv": ["rm", "f.md"],
                    "session": { "cwd": dir.0 } }),
        );
        assert!(r["result"]["text"].as_str().unwrap().contains("not pinned"));
    }

    #[test]
    fn caps_at_24kib() {
        let _lock = ENV_LOCK.lock().unwrap();
        let home = TempDir::new();
        let _g = EnvGuard::set_home(&home);
        let dir = TempDir::new();
        std::fs::write(dir.0.join("big.md"), "x".repeat(CAP + 8192)).unwrap();
        save_list(&["big.md".into()]).unwrap();
        let r = build(&dir, json!([]));
        let content = r["result"]["messages"][0]["content"].as_str().unwrap();
        assert!(content.len() <= CAP);
    }

    #[test]
    fn shutdown_replies_then_exits_and_notifications_are_silent() {
        let (reply, exit) = handle_resources(&json!({ "id": 2, "method": "plugin/shutdown" }));
        assert!(reply.is_some() && exit);
        let (reply, exit) = handle_resources(&json!({ "method": "plugin/shutdown" }));
        assert!(reply.is_none() && exit);
    }

    #[test]
    fn unknown_methods_are_method_not_found() {
        assert_eq!(call("nope", Value::Null)["error"]["code"], -32601);
    }
}
