//! Tool-call arguments in the shapes models actually send, rewritten to the
//! canonical schema before a call is stored or run. Only rewrites whose
//! intent is unambiguous — a flat `{file}` read is a one-item `reads`, a
//! bare `{command}` is a one-step call — so a malformed call costs nothing
//! instead of a turn, and the model's own history shows it the right shape.

use serde_json::{Map, Value, json};

use crate::api::ToolCall;

const FILE_ALIASES: &[&str] = &["path", "filename", "file_path", "filepath"];

/// Idempotent; leaves anything it doesn't recognize alone for the tool's
/// own error.
pub fn canonicalize(call: &mut ToolCall) {
    if call.malformed.is_some() {
        return;
    }
    // A JSON object sent as a string.
    if let Value::String(s) = &call.arguments
        && let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(s)
    {
        call.arguments = v;
    }
    let Value::Object(args) = &mut call.arguments else { return };
    match call.name.as_str() {
        "read" => read(args),
        "edit" => edit(args),
        "write" => {
            rename(args, FILE_ALIASES, "file");
            rename(args, &["text", "contents", "data", "body"], "content");
        }
        "execute_command" => exec(args),
        "search" => search(args),
        "rizin" => {
            rename(args, &["command", "cmd", "cmds"], "commands");
            if let Some(Value::String(c)) = args.get("commands").cloned() {
                args.insert("commands".into(), json!([c]));
            }
        }
        "agent" => {
            rename(args, &["task", "prompt", "instructions"], "brief");
            rename(args, &["write", "files", "paths"], "writes");
            if let Some(Value::String(w)) = args.get("writes").cloned() {
                args.insert("writes".into(), json!([w]));
            }
        }
        _ => {}
    }
}

fn rename(m: &mut Map<String, Value>, aliases: &[&str], canonical: &str) {
    if m.contains_key(canonical) {
        return;
    }
    for alias in aliases {
        if let Some(v) = m.remove(*alias) {
            m.insert(canonical.into(), v);
            return;
        }
    }
}

/// `[x]` stays, a lone object or string becomes `[it]`.
fn as_list(v: Value) -> Vec<Value> {
    match v {
        Value::Array(items) => items,
        other => vec![other],
    }
}

fn number(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.trim().parse().ok())
}

fn read_item(v: Value) -> Vec<Value> {
    let mut m = match v {
        Value::String(s) => return vec![json!({"file": s})],
        Value::Object(m) => m,
        other => return vec![other],
    };
    if let Some(Value::Array(inner)) = m.remove("reads") {
        return inner.into_iter().flat_map(read_item).collect();
    }
    rename(&mut m, FILE_ALIASES, "file");
    rename(&mut m, &["start_line", "start", "line", "from", "line_start"], "offset");
    // "10-40" (or 10..40) as a line range.
    for key in ["lines", "range", "line_range"] {
        if let Some(Value::String(range)) = m.get(key).cloned()
            && let Some((a, b)) = range.split_once('-').or_else(|| range.split_once(".."))
            && let (Ok(a), Ok(b)) = (a.trim().parse::<u64>(), b.trim().parse::<u64>())
            && b >= a
        {
            m.remove(key);
            m.insert("offset".into(), json!(a));
            m.insert("limit".into(), json!(b - a + 1));
        }
    }
    if !m.contains_key("limit")
        && let Some(end) = ["end_line", "end", "to", "line_end"].iter().find_map(|k| m.remove(*k)).as_ref().and_then(number)
    {
        let start = m.get("offset").and_then(number).unwrap_or(1);
        if end >= start {
            m.insert("limit".into(), json!(end - start + 1));
        }
    }
    rename(&mut m, &["lines", "num_lines", "count", "max_lines", "length"], "limit");
    for key in ["offset", "limit"] {
        if let Some(n) = m.get(key).and_then(number) {
            m.insert(key.into(), json!(n));
        }
    }
    vec![Value::Object(m)]
}

fn read(args: &mut Map<String, Value>) {
    let items = match args.remove("reads") {
        Some(v @ (Value::Array(_) | Value::Object(_) | Value::String(_))) => as_list(v),
        Some(other) => {
            args.insert("reads".into(), other);
            return;
        }
        None => {
            if let Some(files) = args.remove("files").or_else(|| args.remove("paths")) {
                as_list(files)
            } else if args.contains_key("file") || FILE_ALIASES.iter().any(|k| args.contains_key(*k)) {
                vec![Value::Object(std::mem::take(args))]
            } else {
                return;
            }
        }
    };
    args.insert("reads".into(), Value::Array(items.into_iter().flat_map(read_item).collect()));
}

fn hunk(v: Value, file: Option<&Value>) -> Vec<Value> {
    let Value::Object(mut m) = v else { return vec![v] };
    rename(&mut m, FILE_ALIASES, "file");
    // A hunk wrapping more hunks: flatten, passing its file down.
    if let Some(inner) = m.remove("edits") {
        let file = m.get("file").cloned().or_else(|| file.cloned());
        return as_list(inner).into_iter().flat_map(|h| hunk(h, file.as_ref())).collect();
    }
    rename(&mut m, &["old_text", "old", "search", "find", "original", "from"], "old_string");
    rename(&mut m, &["new_text", "new", "replace", "replacement", "updated", "to"], "new_string");
    if !m.contains_key("file")
        && let Some(f) = file
    {
        m.insert("file".into(), f.clone());
    }
    vec![Value::Object(m)]
}

fn edit(args: &mut Map<String, Value>) {
    let top_file = args.get("file").or_else(|| FILE_ALIASES.iter().find_map(|k| args.get(*k))).cloned();
    let hunks = match args.remove("edits") {
        Some(v @ (Value::Array(_) | Value::Object(_))) => as_list(v),
        Some(other) => {
            args.insert("edits".into(), other);
            return;
        }
        None if args.keys().any(|k| matches!(k.as_str(), "old_string" | "new_string" | "old_text" | "new_text" | "old" | "new")) => {
            vec![Value::Object(std::mem::take(args))]
        }
        None => return,
    };
    let flat: Vec<Value> = hunks.into_iter().flat_map(|h| hunk(h, top_file.as_ref())).collect();
    args.remove("file");
    for k in FILE_ALIASES {
        args.remove(*k);
    }
    args.insert("edits".into(), Value::Array(flat));
}

/// Step-level fields a model may put on the call instead.
const STEP_KEYS: &[&str] = &["cwd", "env", "streams", "timeout_seconds", "tail_lines", "network"];

fn step(v: Value) -> Value {
    match v {
        Value::String(s) => json!({"command": s}),
        Value::Object(mut m) => {
            rename(&mut m, &["cmd", "script", "run"], "command");
            Value::Object(m)
        }
        other => other,
    }
}

fn exec(args: &mut Map<String, Value>) {
    let steps = match args.remove("steps") {
        Some(v @ (Value::Array(_) | Value::Object(_) | Value::String(_))) => as_list(v),
        Some(other) => {
            args.insert("steps".into(), other);
            return;
        }
        None => {
            if let Some(commands) = args.remove("commands") {
                as_list(commands)
            } else if let Some(command) = ["command", "cmd", "script"].iter().find_map(|k| args.remove(*k)) {
                vec![json!({"command": command})]
            } else {
                return;
            }
        }
    };
    let mut steps: Vec<Value> = steps.into_iter().map(step).collect();
    // Step fields given once for the call apply to every step lacking them.
    for key in STEP_KEYS {
        if let Some(v) = args.remove(*key) {
            for s in steps.iter_mut() {
                if let Value::Object(m) = s {
                    m.entry(key.to_string()).or_insert_with(|| v.clone());
                }
            }
        }
    }
    args.insert("steps".into(), Value::Array(steps));
}

fn search(args: &mut Map<String, Value>) {
    rename(args, &["query", "regex", "text"], "pattern");
    rename(args, &["dir", "directory", "root"], "path");
    let Some(pattern) = args.get("pattern").and_then(Value::as_str).map(str::trim).map(str::to_string) else { return };
    // "Every file" written as a pattern is a file listing.
    if matches!(pattern.as_str(), "*" | "**" | "**/*" | ".*") {
        args.remove("pattern");
        args.entry("glob").or_insert(json!("*"));
    } else if pattern == "." && args.contains_key("glob") {
        args.remove("pattern");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(name: &str, args: Value) -> Value {
        let mut call = ToolCall { id: "c".into(), name: name.into(), arguments: args, malformed: None };
        canonicalize(&mut call);
        let once = call.arguments.clone();
        canonicalize(&mut call);
        assert_eq!(call.arguments, once, "canonicalize must be idempotent");
        once
    }

    #[test]
    fn flat_and_odd_reads_become_a_reads_list() {
        assert_eq!(fix("read", json!({"file": "src/cpu.rs"})), json!({"reads": [{"file": "src/cpu.rs"}]}));
        assert_eq!(
            fix("read", json!({"path": "a.rs", "start_line": 10, "end_line": 39})),
            json!({"reads": [{"file": "a.rs", "offset": 10, "limit": 30}]})
        );
        assert_eq!(fix("read", json!({"files": ["a.rs", "b.rs"]})), json!({"reads": [{"file": "a.rs"}, {"file": "b.rs"}]}));
        assert_eq!(fix("read", json!({"reads": {"file": "a.rs", "lines": "5-9"}})), json!({"reads": [{"file": "a.rs", "offset": 5, "limit": 5}]}));
        assert_eq!(fix("read", json!({"reads": [{"file": "a.rs", "offset": "3"}]})), json!({"reads": [{"file": "a.rs", "offset": 3}]}));
        let canonical = json!({"reads": [{"file": "a.rs", "offset": 1, "limit": 20}]});
        assert_eq!(fix("read", canonical.clone()), canonical);
    }

    #[test]
    fn flat_and_nested_edits_become_one_hunk_list() {
        assert_eq!(
            fix("edit", json!({"file": "src/mem.rs", "old_string": "a", "new_string": "b"})),
            json!({"edits": [{"file": "src/mem.rs", "old_string": "a", "new_string": "b"}]})
        );
        // Seen live: an edits list wrapped in another.
        assert_eq!(
            fix("edit", json!({"edits": [{"edits": [{"file": "src/cpu.rs", "old_string": "x", "new_string": "y"}]}]})),
            json!({"edits": [{"file": "src/cpu.rs", "old_string": "x", "new_string": "y"}]})
        );
        assert_eq!(
            fix("edit", json!({"file": "a.rs", "edits": [{"old_text": "x", "new_text": "y"}, {"search": "p", "replace": "q", "replace_all": true}]})),
            json!({"edits": [{"file": "a.rs", "old_string": "x", "new_string": "y"}, {"file": "a.rs", "old_string": "p", "new_string": "q", "replace_all": true}]})
        );
    }

    #[test]
    fn a_bare_command_becomes_one_step() {
        assert_eq!(
            fix("execute_command", json!({"command": "ls -la && git status", "cwd": "src", "background": true})),
            json!({"steps": [{"command": "ls -la && git status", "cwd": "src"}], "background": true})
        );
        assert_eq!(
            fix("execute_command", json!({"commands": ["cargo build", {"cmd": "cargo test"}], "timeout_seconds": 300})),
            json!({"steps": [{"command": "cargo build", "timeout_seconds": 300}, {"command": "cargo test", "timeout_seconds": 300}]})
        );
        let canonical = json!({"steps": [{"command": "ls"}], "on_error": "continue"});
        assert_eq!(fix("execute_command", canonical.clone()), canonical);
    }

    #[test]
    fn search_and_rizin_and_agent_aliases() {
        assert_eq!(fix("search", json!({"pattern": "*", "path": "src"})), json!({"glob": "*", "path": "src"}));
        assert_eq!(fix("search", json!({"glob": "**/*.ld", "pattern": "."})), json!({"glob": "**/*.ld"}));
        assert_eq!(fix("search", json!({"query": "fn main", "dir": "src"})), json!({"pattern": "fn main", "path": "src"}));
        assert_eq!(fix("rizin", json!({"command": "afl"})), json!({"commands": ["afl"]}));
        assert_eq!(fix("agent", json!({"task": "find X"})), json!({"brief": "find X"}));
        assert_eq!(fix("agent", json!({"files": "src/a.rs", "task": "fix X"})), json!({"writes": ["src/a.rs"], "brief": "fix X"}));
    }

    #[test]
    fn a_stringified_object_is_parsed() {
        assert_eq!(fix("read", json!("{\"file\": \"a.rs\"}")), json!({"reads": [{"file": "a.rs"}]}));
    }
}
