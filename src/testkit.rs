//! Builders for realistic agent session files, used by unit tests,
//! integration tests and the synthetic-load benchmark.
//!
//! The record shapes mirror real Claude Code and Codex files (compact JSON,
//! one record per line), trimmed to the fields that matter plus some noise.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Serializes records as compact JSONL, the way the agents write them.
pub fn jsonl_bytes(lines: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(line.to_string().as_bytes());
        out.push(b'\n');
    }
    out
}

/// Like [`jsonl_bytes`] but with a space after `:` and `,`, the way Python's
/// `json.dumps` writes JSON. Parsers must not depend on compact spacing.
/// Formats token by token, so string contents are never altered.
pub fn jsonl_bytes_spaced(lines: &[Value]) -> Vec<u8> {
    let mut out = String::new();
    for line in lines {
        write_spaced(&mut out, line);
        out.push('\n');
    }
    out.into_bytes()
}

fn write_spaced(out: &mut String, value: &Value) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_spaced(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push_str(": ");
                write_spaced(out, item);
            }
            out.push('}');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

pub fn write_jsonl(path: &Path, lines: &[Value]) {
    fs::create_dir_all(path.parent().expect("path has a parent")).expect("create dirs");
    fs::write(path, jsonl_bytes(lines)).expect("write jsonl");
}

pub mod claude {
    use super::*;

    /// Claude's project directory name for a working directory.
    pub fn project_dir_name(cwd: &str) -> String {
        cwd.replace(['/', '.'], "-")
    }

    pub fn session_path(root: &Path, id: &str, cwd: &str) -> PathBuf {
        root.join("projects").join(project_dir_name(cwd)).join(format!("{id}.jsonl"))
    }

    /// Writes `<root>/projects/<encoded cwd>/<id>.jsonl` and returns its path.
    pub fn write_session(root: &Path, id: &str, cwd: &str, lines: &[Value]) -> PathBuf {
        let path = session_path(root, id, cwd);
        write_jsonl(&path, lines);
        path
    }

    fn base_user(id: &str, cwd: &str, ts: &str, content: Value) -> Value {
        json!({
            "parentUuid": null,
            "isSidechain": false,
            "promptId": "p-1",
            "type": "user",
            "message": {"role": "user", "content": content},
            "uuid": format!("u-{ts}"),
            "timestamp": ts,
            "permissionMode": "auto",
            "userType": "external",
            "entrypoint": "cli",
            "cwd": cwd,
            "sessionId": id,
            "version": "2.1.308",
            "gitBranch": "main"
        })
    }

    pub fn user(id: &str, cwd: &str, ts: &str, text: &str) -> Value {
        base_user(id, cwd, ts, json!(text))
    }

    /// A user record with arbitrary `message.content`.
    pub fn user_content(id: &str, cwd: &str, ts: &str, content: Value) -> Value {
        base_user(id, cwd, ts, content)
    }

    /// A user record whose content is a list of blocks (as pasted images produce).
    pub fn user_blocks(id: &str, cwd: &str, ts: &str, text: &str) -> Value {
        base_user(
            id,
            cwd,
            ts,
            json!([{"type": "text", "text": text}, {"type": "image", "source": {"type": "base64", "data": "AAAA"}}]),
        )
    }

    pub fn meta_user(id: &str, cwd: &str, ts: &str, text: &str) -> Value {
        let mut v = user(id, cwd, ts, text);
        v["isMeta"] = json!(true);
        v
    }

    pub fn sidechain_user(id: &str, cwd: &str, ts: &str, text: &str) -> Value {
        let mut v = user(id, cwd, ts, text);
        v["isSidechain"] = json!(true);
        v
    }

    pub fn tool_result(id: &str, cwd: &str, ts: &str, output: &str) -> Value {
        let mut v = base_user(
            id,
            cwd,
            ts,
            json!([{"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": output}]}]),
        );
        v["toolUseResult"] = json!({"stdout": output, "stderr": "", "interrupted": false});
        v
    }

    fn base_assistant(id: &str, ts: &str, content: Value) -> Value {
        json!({
            "parentUuid": "u-1",
            "isSidechain": false,
            "message": {
                "model": "claude-opus-5-5",
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "content": content,
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 2, "output_tokens": 10}
            },
            "type": "assistant",
            "uuid": format!("a-{ts}"),
            "timestamp": ts,
            "sessionId": id
        })
    }

    pub fn assistant_text(id: &str, ts: &str, text: &str) -> Value {
        base_assistant(
            id,
            ts,
            json!([{"type": "thinking", "thinking": "", "signature": "sig"}, {"type": "text", "text": text}]),
        )
    }

    /// An assistant record with arbitrary `message.content`.
    pub fn assistant_content(id: &str, ts: &str, content: Value) -> Value {
        base_assistant(id, ts, content)
    }

    pub fn assistant_tool(id: &str, ts: &str, name: &str, input: Value) -> Value {
        base_assistant(id, ts, json!([{"type": "tool_use", "id": "toolu_1", "name": name, "input": input}]))
    }

    pub fn ai_title(id: &str, title: &str) -> Value {
        json!({"type": "ai-title", "aiTitle": title, "sessionId": id})
    }

    pub fn custom_title(id: &str, title: &str) -> Value {
        json!({"type": "custom-title", "customTitle": title, "sessionId": id})
    }

    pub fn last_prompt(id: &str, text: &str) -> Value {
        json!({"type": "last-prompt", "lastPrompt": text, "leafUuid": "u-9", "sessionId": id})
    }

    /// A record with a nested `timestamp` that must not be mistaken for the record's own.
    pub fn snapshot() -> Value {
        json!({"type": "file-history-snapshot", "messageId": "m1", "snapshot": {"trackedFileBackups": {}, "timestamp": "2000-01-01T00:00:00Z"}, "isSnapshotUpdate": false})
    }
}

pub mod codex {
    use super::*;

    pub fn session_path(root: &Path, id: &str) -> PathBuf {
        root.join("sessions/2026/10/24").join(format!("rollout-2026-10-24T13-38-56-{id}.jsonl"))
    }

    /// Writes a rollout file under `<root>/sessions/YYYY/MM/DD/` and returns its path.
    pub fn write_session(root: &Path, id: &str, lines: &[Value]) -> PathBuf {
        let path = session_path(root, id);
        write_jsonl(&path, lines);
        path
    }

    /// Writes `<root>/session_index.jsonl` with `(id, thread name)` entries.
    pub fn write_index(root: &Path, entries: &[(&str, &str)]) {
        let lines: Vec<Value> = entries
            .iter()
            .map(|(id, name)| json!({"id": id, "thread_name": name, "updated_at": "2026-10-24T16:39:48.159449511Z"}))
            .collect();
        write_jsonl(&root.join("session_index.jsonl"), &lines);
    }

    pub fn meta(id: &str, cwd: &str, ts: &str) -> Value {
        json!({
            "timestamp": ts,
            "ordinal": 0,
            "type": "session_meta",
            "payload": {
                "session_id": id,
                "id": id,
                "timestamp": ts,
                "cwd": cwd,
                "originator": "codex-tui",
                "cli_version": "0.164.0",
                "git": {"commit_hash": "abc123", "branch": "main"},
                "base_instructions": {"text": "You are Codex, an agent based on GPT-6."}
            }
        })
    }

    /// The `session_meta` of a sub-agent thread spawned by `parent`.
    pub fn subagent_meta(id: &str, parent: &str, cwd: &str, ts: &str) -> Value {
        let mut v = meta(id, cwd, ts);
        let payload = v["payload"].as_object_mut().expect("object");
        payload.insert("session_id".into(), json!(parent));
        payload.insert("forked_from_id".into(), json!(parent));
        payload.insert("parent_thread_id".into(), json!(parent));
        payload.insert("thread_source".into(), json!("subagent"));
        payload.insert(
            "source".into(),
            json!({"subagent": {"thread_spawn": {"parent_thread_id": parent, "depth": 1, "agent_nickname": "Leibniz"}}}),
        );
        v
    }

    fn message(ts: &str, role: &str, kind: &str, text: &str) -> Value {
        json!({
            "timestamp": ts,
            "type": "response_item",
            "payload": {"type": "message", "id": "msg_1", "role": role, "content": [{"type": kind, "text": text}]}
        })
    }

    pub fn developer(ts: &str, text: &str) -> Value {
        message(ts, "developer", "input_text", text)
    }

    pub fn env_context(ts: &str, cwd: &str) -> Value {
        message(
            ts,
            "user",
            "input_text",
            &format!("<environment_context>\n  <cwd>{cwd}</cwd>\n  <shell>zsh</shell>\n</environment_context>"),
        )
    }

    pub fn user(ts: &str, text: &str) -> Value {
        message(ts, "user", "input_text", text)
    }

    pub fn assistant(ts: &str, text: &str) -> Value {
        let mut v = message(ts, "assistant", "output_text", text);
        v["payload"]["phase"] = json!("commentary");
        v
    }

    pub fn turn_context(ts: &str, cwd: &str) -> Value {
        json!({"timestamp": ts, "type": "turn_context", "payload": {"turn_id": "t1", "cwd": cwd, "model": "gpt-6", "approval_policy": "on-request"}})
    }

    pub fn function_call(ts: &str, name: &str, arguments: &str) -> Value {
        json!({"timestamp": ts, "type": "response_item", "payload": {"type": "function_call", "name": name, "arguments": arguments, "call_id": "call_1"}})
    }

    /// A free-form tool call (`apply_patch` and friends): the input is text.
    pub fn custom_tool_call(ts: &str, name: &str, input: &str) -> Value {
        json!({"timestamp": ts, "type": "response_item", "payload": {"type": "custom_tool_call", "status": "completed", "call_id": "call_2", "name": name, "input": input}})
    }

    /// A shell call whose command lives in `action`.
    pub fn local_shell_call(ts: &str, command: &[&str]) -> Value {
        json!({"timestamp": ts, "type": "response_item", "payload": {"type": "local_shell_call", "call_id": "call_3", "status": "completed", "action": {"type": "exec", "command": command, "timeout_ms": 10000}}})
    }

    /// A user message with arbitrary content blocks.
    pub fn user_content(ts: &str, content: Value) -> Value {
        json!({"timestamp": ts, "type": "response_item", "payload": {"type": "message", "role": "user", "content": content}})
    }

    pub fn token_count(ts: &str) -> Value {
        json!({"timestamp": ts, "type": "event_msg", "payload": {"type": "token_count", "info": null}})
    }
}

/// A small, varied set of sessions under `env`'s agent directories:
/// two Claude sessions in one workspace, one in another, and one Codex session.
/// Returns the session ids, newest first.
pub fn populate(env: &crate::env::Env) -> Vec<String> {
    let c = &env.claude_dir;
    let ids = [
        "aaaaaaaa-0000-0000-0000-000000000001",
        "aaaaaaaa-0000-0000-0000-000000000002",
        "aaaaaaaa-0000-0000-0000-000000000003",
        "01a16ec7-96b5-7c11-b69f-000000000004",
    ];
    let app = "/work/app";
    let site = "/work/site";
    claude::write_session(
        c,
        ids[0],
        app,
        &[
            claude::user(ids[0], app, "2026-10-01T10:00:00Z", "add rate limiting to the api"),
            claude::assistant_text(ids[0], "2026-10-01T10:00:10Z", "Sure, adding a token bucket."),
            claude::ai_title(ids[0], "Rate limiting for API"),
        ],
    );
    claude::write_session(
        c,
        ids[1],
        app,
        &[
            claude::user(ids[1], app, "2026-10-03T10:00:00Z", "fix the flaky redis test"),
            claude::assistant_text(ids[1], "2026-10-03T10:00:10Z", "Mocking the clock."),
        ],
    );
    claude::write_session(
        c,
        ids[2],
        site,
        &[
            claude::user(ids[2], site, "2026-10-02T10:00:00Z", "make the landing page responsive"),
            claude::assistant_text(ids[2], "2026-10-02T10:00:10Z", "Using CSS grid."),
            claude::custom_title(ids[2], "landing-page"),
        ],
    );
    codex::write_session(
        &env.codex_dir,
        ids[3],
        &[
            codex::meta(ids[3], app, "2026-10-04T09:00:00Z"),
            codex::env_context("2026-10-04T09:00:01Z", app),
            codex::user("2026-10-04T09:00:02Z", "review the rate limiter"),
            codex::assistant("2026-10-04T09:00:30Z", "Looks good overall."),
        ],
    );
    codex::write_index(&env.codex_dir, &[(ids[3], "Review rate limiter")]);
    vec![ids[3].into(), ids[1].into(), ids[2].into(), ids[0].into()]
}

/// Writes `count` synthetic Claude sessions across 20 workspaces, each with
/// `turns` prompt/answer pairs and bulky tool output, for load testing.
pub fn synthetic(env: &crate::env::Env, count: usize, turns: usize) {
    let filler = "lorem ipsum dolor sit amet ".repeat(200);
    for n in 0..count {
        let id = format!("{n:08x}-0000-4000-8000-000000000000");
        let cwd = format!("/work/project-{}", n % 20);
        let mut lines = Vec::with_capacity(turns * 4 + 1);
        for t in 0..turns {
            let ts = format!("2026-{:02}-{:02}T{:02}:{:02}:00Z", 1 + n % 12, 1 + n % 28, t % 24, n % 60);
            lines.push(claude::user(&id, &cwd, &ts, &format!("prompt {t} for session {n}")));
            lines.push(claude::assistant_tool(&id, &ts, "Bash", json!({"command": "cargo test"})));
            lines.push(claude::tool_result(&id, &cwd, &ts, &filler));
            lines.push(claude::assistant_text(&id, &ts, &filler));
        }
        lines.push(claude::ai_title(&id, &format!("Synthetic session {n}")));
        claude::write_session(&env.claude_dir, &id, &cwd, &lines);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spaced_fixtures_keep_string_values() {
        let record = json!({"type": "user", "text": "{\"x\":\"y\"}, \"a\":1", "n": [1, {"k": "v"}]});
        let spaced = jsonl_bytes_spaced(std::slice::from_ref(&record));
        let text = String::from_utf8(spaced.clone()).unwrap();
        assert!(text.contains("\"type\": \"user\""), "{text}");
        let decoded: Value = serde_json::from_slice(&spaced).unwrap();
        assert_eq!(decoded, record, "decoding gives back exactly the same values");
    }
}
