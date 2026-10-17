//! gray-context-inject — one sidecar for every "put text into context" plugin.
//!
//! Merges gray-header, gray-context-pin, gray-resources, gray-rules,
//! gray-include, gray-dirty-guard and gray-gh-issues. Each source lives
//! verbatim in `mods/` with its own state files and slash commands — only
//! the wire plumbing is shared.
//!
//! Hook fan-out:
//! - `prompt/context`  → header + pin + include + dirty: `{text}` results are
//!   joined with blank lines (each module contributes or abstains).
//! - `context/build`   → resources (message prepend).
//! - `agent/before_start` → rules (rule-file injection).
//! - `input/submit`    → ghissues (rewrite `#N` mentions with issue titles).
//! - `command/run`     → routed by `params.name` to the owning module.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

#[path = "mods/dirty.rs"]
mod dirty;
#[path = "mods/ghissues.rs"]
mod ghissues;
#[path = "mods/header.rs"]
mod header;
#[path = "mods/include.rs"]
mod include;
#[path = "mods/pin.rs"]
mod pin;
#[path = "mods/resources.rs"]
mod resources;
#[path = "mods/rules.rs"]
mod rules;

fn manifest() -> Value {
    json!({
        "name": "context-inject",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [],
        "commands": [
            "/inject", "/header", "/pin", "/unpin", "/pins",
            "/resources", "/rules", "/include", "/dirty", "/ghissues",
        ],
        "hooks": ["prompt/context", "context/build", "agent/before_start", "input/submit"],
    })
}

/// Commands each module owns, routed by `command/run`'s `params.name`.
fn command_owner(name: &str) -> Option<fn(&Value) -> (Option<Value>, bool)> {
    match name {
        "/header" => Some(header::handle_header),
        "/pin" | "/unpin" | "/pins" => Some(pin::handle_pin),
        "/resources" => Some(resources::handle_resources),
        "/rules" => Some(rules::handle_rules),
        "/include" => Some(include::handle_include),
        "/dirty" => Some(dirty::handle_dirty),
        "/ghissues" => Some(ghissues_shim),
        _ => None,
    }
}

/// ghissues' handler carries plugin state; `command/run` doesn't need it.
fn ghissues_shim(req: &Value) -> (Option<Value>, bool) {
    let mut p = ghissues::GhIssues::new();
    ghissues::handle_ghissues(&mut p, req)
}

/// `prompt/context` fan-out: every contributing module returns `{text}`;
/// join the non-empty ones.
fn prompt_context(req: &Value) -> Value {
    let mut parts: Vec<String> = Vec::new();
    for h in [
        header::handle_header,
        pin::handle_pin,
        include::handle_include,
        dirty::handle_dirty,
    ] {
        if let (Some(reply), _) = h(req) {
            if let Some(t) = reply
                .get("result")
                .and_then(|r| r.get("text"))
                .and_then(Value::as_str)
            {
                let t = t.trim_end();
                if !t.is_empty() {
                    parts.push(t.to_string());
                }
            }
        }
    }
    if parts.is_empty() {
        json!({})
    } else {
        json!({ "text": parts.join("\n\n") })
    }
}

/// `/inject` — merged-plugin overview.
fn inject_command() -> String {
    format!(
        "gray-context-inject {} — one sidecar, seven injection sources:\n\
         \x20 /header     ~/.gray/header.txt banner into every turn\n\
         \x20 /pin,/unpin,/pins  pinned files/notes into every turn\n\
         \x20 /resources  always-on files as a synthetic message\n\
         \x20 /rules      repo rule files at agent start\n\
         \x20 /include    @path expansion in context files\n\
         \x20 /dirty      uncommitted-changes note at turn start\n\
         \x20 /ghissues   #N → GitHub issue titles in prompts",
        env!("CARGO_PKG_VERSION")
    )
}

fn handle(req: &Value, gh: &mut ghissues::GhIssues) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest(),
        "prompt/context" => prompt_context(req),
        "context/build" => {
            let (reply, _) = resources::handle_resources(req);
            return (reply, false);
        }
        "agent/before_start" => {
            let (reply, _) = rules::handle_rules(req);
            return (reply, false);
        }
        "input/submit" => {
            let (reply, _) = ghissues::handle_ghissues(gh, req);
            return (reply, false);
        }
        "command/run" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if name == "/inject" {
                json!({ "text": inject_command() })
            } else if let Some(owner) = command_owner(name) {
                let (reply, _) = owner(req);
                match reply {
                    Some(r) => r.get("result").cloned().unwrap_or(json!({})),
                    None => json!({}),
                }
            } else {
                json!({ "text": format!("unknown command {name} — try /inject") })
            }
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest());
        return Ok(());
    }
    let mut gh = ghissues::GhIssues::new();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (reply, exit) = handle(&req, &mut gh);
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
