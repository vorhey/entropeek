//! Codex sessions: `~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`.
//!
//! Each line is `{"timestamp", "type", "payload"}`. The ones we read:
//! - `session_meta`: the session id, start time and working directory.
//! - `turn_context`: the working directory for each turn.
//! - `response_item` with a `message` payload: user and assistant messages.
//!
//! Thread names (titles) live in `~/.codex/session_index.jsonl`.
//!
//! Sub-agents a session spawns get rollout files of their own, marked in
//! their `session_meta` (`thread_source: "subagent"`, `parent_thread_id`)
//! and starting with a copy of the parent's history. They are part of the
//! parent session, not sessions of their own: they are not listed, and
//! deleting the parent takes them along.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::value::RawValue;

use super::content::{custom_tool_summary, extract, shell_action_summary, tool_summary};
use super::{FileStamp, Provider, ResumeCommand, files_recursive, has_extension, push_turn};
use crate::jsonl::{self, Marker};
use crate::model::{AgentKind, Role, Session, Turn};
use crate::text;
use crate::timefmt::parse_rfc3339_ms;

pub struct CodexProvider {
    root: PathBuf,
    /// The parsed title index and the file version it came from, so the
    /// startup view and every rescan don't each re-read an unchanged index.
    titles: Mutex<Option<TitleMemo>>,
    #[cfg(test)]
    title_reads: std::sync::atomic::AtomicUsize,
}

impl CodexProvider {
    pub fn new(root: PathBuf) -> CodexProvider {
        CodexProvider {
            root,
            titles: Mutex::new(None),
            #[cfg(test)]
            title_reads: Default::default(),
        }
    }

    fn titles(&self) -> Arc<HashMap<String, String>> {
        let index = self.root.join(INDEX_FILE);
        let Some(stamp) = FileStamp::of(&index) else {
            return Arc::default();
        };
        let mut memo = self.titles.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((seen, titles)) = memo.as_ref()
            && *seen == stamp
        {
            return titles.clone();
        }
        #[cfg(test)]
        self.title_reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let titles = Arc::new(read_titles(&self.root));
        *memo = Some((stamp, titles.clone()));
        titles
    }
}

const INDEX_FILE: &str = "session_index.jsonl";

type TitleMemo = (FileStamp, Arc<HashMap<String, String>>);

#[derive(Deserialize)]
struct Record<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    payload: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Meta<'a> {
    #[serde(borrow, default)]
    id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    cwd: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    git: Option<Git<'a>>,
    /// `"user"` for sessions people start, `"subagent"` for spawned ones.
    #[serde(borrow, default)]
    thread_source: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    parent_thread_id: Option<Cow<'a, str>>,
    /// `{"subagent": {"thread_spawn": {"parent_thread_id": ...}}}` for
    /// spawned threads; other shapes for normal sessions.
    #[serde(borrow, default)]
    source: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Source<'a> {
    #[serde(borrow, default)]
    subagent: Option<Subagent<'a>>,
}

#[derive(Deserialize)]
struct Subagent<'a> {
    #[serde(borrow, default)]
    thread_spawn: Option<ThreadSpawn<'a>>,
}

#[derive(Deserialize)]
struct ThreadSpawn<'a> {
    #[serde(borrow, default)]
    parent_thread_id: Option<Cow<'a, str>>,
}

impl Meta<'_> {
    /// For a spawned sub-agent thread: its parent's id (possibly unknown).
    fn subagent_of(&self) -> Option<Option<String>> {
        let spawn = self
            .source
            .and_then(|raw| serde_json::from_str::<Source>(raw.get()).ok())
            .and_then(|s| s.subagent)
            .map(|s| s.thread_spawn.and_then(|t| t.parent_thread_id).map(Cow::into_owned));
        let marked = self.thread_source.as_deref() == Some("subagent");
        match (spawn, &self.parent_thread_id) {
            (Some(parent), own) => Some(parent.or_else(|| own.as_deref().map(str::to_string))),
            (None, Some(parent)) => Some(Some(parent.to_string())),
            (None, None) if marked => Some(None),
            (None, None) => None,
        }
    }
}

/// The id and, for a sub-agent, the parent id of the thread a rollout file
/// holds. Reads only up to the file's own (first) `session_meta`.
fn read_thread(path: &Path) -> Option<(String, Option<Option<String>>)> {
    use std::io::BufRead;
    let file = fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.split(b'\n').take(16) {
        let line = line.ok()?;
        let Ok(record) = serde_json::from_slice::<Record>(&line) else { continue };
        if record.kind.as_deref() != Some("session_meta") {
            continue;
        }
        let meta = parse_payload::<Meta>(&record)?;
        let id =
            meta.id.as_deref().filter(|id| !id.is_empty()).map(str::to_string).or_else(|| id_from_file_name(path))?;
        return Some((id, meta.subagent_of()));
    }
    None
}

#[derive(Deserialize)]
struct Git<'a> {
    #[serde(borrow, default)]
    branch: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct TurnContext<'a> {
    #[serde(borrow, default)]
    cwd: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct Item<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    role: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    content: Option<&'a RawValue>,
    #[serde(borrow, default)]
    name: Option<Cow<'a, str>>,
    /// `function_call`: JSON arguments, usually encoded as a string.
    #[serde(borrow, default)]
    arguments: Option<&'a RawValue>,
    /// `custom_tool_call`: free-form text input (e.g. a patch).
    #[serde(borrow, default)]
    input: Option<&'a RawValue>,
    /// `local_shell_call`: `{"type": "exec", "command": [...]}`.
    #[serde(borrow, default)]
    action: Option<&'a RawValue>,
}

impl Item<'_> {
    fn is_message(&self, role: &str) -> bool {
        self.kind.as_deref() == Some("message") && self.role.as_deref() == Some(role)
    }

    fn user_prompt(&self) -> Option<String> {
        if !self.is_message("user") {
            return None;
        }
        let prompt = extract(self.content?).user_text()?;
        text::is_user_prompt(prompt.trim()).then_some(prompt)
    }

    /// A one-line summary if this item is a tool call.
    fn tool_call(&self) -> Option<String> {
        let kind = self.kind.as_deref()?;
        let name = self.name.as_deref();
        let summary = match kind {
            "function_call" => {
                let args = self.arguments.and_then(|raw| match serde_json::from_str::<String>(raw.get()) {
                    Ok(encoded) => RawValue::from_string(encoded).ok(),
                    Err(_) => RawValue::from_string(raw.get().to_string()).ok(),
                });
                tool_summary(name.unwrap_or("tool"), args.as_deref())
            }
            "custom_tool_call" => {
                let input =
                    self.input.and_then(|raw| serde_json::from_str::<String>(raw.get()).ok()).unwrap_or_default();
                custom_tool_summary(name.unwrap_or("tool"), &input)
            }
            "local_shell_call" => shell_action_summary(name.unwrap_or("shell"), self.action),
            _ => return None,
        };
        Some(summary)
    }
}

fn parse_payload<'a, T: Deserialize<'a>>(record: &Record<'a>) -> Option<T> {
    serde_json::from_str(record.payload?.get()).ok()
}

/// The session id embedded at the end of a rollout file name.
fn id_from_file_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let tail = stem.get(stem.len().checked_sub(36)?..)?;
    let looks_like_uuid = tail.len() == 36
        && tail.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    looks_like_uuid.then(|| tail.to_string())
}

/// Parses session metadata from the raw bytes of a Codex rollout file.
pub fn parse_meta(path: &Path, bytes: &[u8], stamp: FileStamp) -> Option<Session> {
    let meta_marker = Marker::token("session_meta");
    let context_marker = Marker::token("turn_context");
    let item_marker = Marker::token("response_item");
    let user_marker = Marker::token("user");

    let mut id = None;
    let mut cwd = String::new();
    let mut started_at = None;
    let mut git_branch = None;
    let mut first_prompt = None;
    let mut last_prompt = None;
    let mut user_turns = 0u32;

    for line in jsonl::lines(bytes) {
        let wanted = meta_marker.in_line(line)
            || context_marker.in_line(line)
            || (item_marker.in_line(line) && user_marker.in_line(line));
        if !wanted {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Record>(line) else { continue };
        match record.kind.as_deref() {
            Some("session_meta") if id.is_none() => {
                if let Some(meta) = parse_payload::<Meta>(&record) {
                    // A sub-agent's file repeats its parent's history; it is
                    // shown as part of the parent, not on its own.
                    if meta.subagent_of().is_some() {
                        return None;
                    }
                    id = meta.id.map(Cow::into_owned);
                    if let Some(value) = meta.cwd.filter(|v| !v.is_empty()) {
                        cwd = value.into_owned();
                    }
                    started_at = meta.timestamp.or(record.timestamp).as_deref().and_then(parse_rfc3339_ms);
                    git_branch = meta.git.and_then(|g| g.branch).map(Cow::into_owned);
                }
            }
            Some("turn_context") => {
                if let Some(value) = parse_payload::<TurnContext>(&record).and_then(|c| c.cwd).filter(|v| !v.is_empty())
                {
                    cwd = value.into_owned();
                }
            }
            Some("response_item") => {
                if let Some(prompt) = parse_payload::<Item>(&record).and_then(|item| item.user_prompt()) {
                    user_turns += 1;
                    if first_prompt.is_none() {
                        first_prompt = Some(text::summary(&prompt, 300));
                    }
                    last_prompt = Some(prompt);
                }
            }
            _ => {}
        }
    }

    if user_turns == 0 {
        return None;
    }
    let id = id.filter(|id| !id.is_empty()).or_else(|| id_from_file_name(path))?;
    Some(Session {
        agent: AgentKind::Codex,
        id,
        path: path.to_path_buf(),
        cwd,
        title: None,
        first_prompt,
        last_prompt: last_prompt.map(|p| text::summary(&p, 300)),
        started_at,
        updated_at: jsonl::last_timestamp(bytes).unwrap_or_else(|| stamp.mtime_ms()),
        mtime_ns: stamp.mtime_ns,
        git_branch,
        user_turns,
        size: stamp.size,
    })
}

/// Parses the full conversation from a Codex rollout file.
pub fn parse_transcript(bytes: &[u8]) -> Vec<Turn> {
    let item_marker = Marker::token("response_item");
    let mut turns = Vec::new();
    for line in jsonl::lines(bytes) {
        if !item_marker.in_line(line) {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Record>(line) else { continue };
        if record.kind.as_deref() != Some("response_item") {
            continue;
        }
        let Some(item) = parse_payload::<Item>(&record) else { continue };
        let timestamp = record.timestamp.as_deref().and_then(parse_rfc3339_ms);
        if let Some(prompt) = item.user_prompt() {
            push_turn(&mut turns, Turn { role: Role::User, text: prompt, timestamp });
        } else if item.is_message("assistant") {
            if let Some(content) = item.content {
                push_turn(&mut turns, Turn { role: Role::Assistant, text: extract(content).text(), timestamp });
            }
        } else if let Some(summary) = item.tool_call() {
            turns.push(Turn { role: Role::Tool, text: text::one_line(&summary), timestamp });
        }
    }
    turns
}

#[derive(Deserialize)]
struct IndexEntry {
    id: String,
    #[serde(default)]
    thread_name: Option<String>,
}

/// Reads `session_index.jsonl` into `id -> thread name`. Later lines win.
pub fn read_titles(root: &Path) -> HashMap<String, String> {
    let Ok(bytes) = fs::read(root.join(INDEX_FILE)) else {
        return HashMap::new();
    };
    let mut titles = HashMap::new();
    for line in jsonl::lines(&bytes) {
        if let Ok(entry) = serde_json::from_slice::<IndexEntry>(line)
            && let Some(name) = entry.thread_name.filter(|n| !n.trim().is_empty())
        {
            titles.insert(entry.id, text::summary(&name, 200));
        }
    }
    titles
}

impl Provider for CodexProvider {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn discover(&self, errors: &mut Vec<String>) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let keep = |p: &Path| {
            has_extension(p, "jsonl") && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("rollout-"))
        };
        files_recursive(&self.root.join("sessions"), 4, &keep, &mut out, errors);
        out
    }

    fn parse(&self, path: &Path, stamp: FileStamp) -> Result<Option<Session>> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(parse_meta(path, &bytes, stamp))
    }

    fn decorate(&self, sessions: &mut [Session]) {
        if sessions.is_empty() {
            return;
        }
        let titles = self.titles();
        for session in sessions {
            session.title = titles.get(&session.id).cloned();
        }
    }

    fn transcript(&self, session: &Session) -> Result<Vec<Turn>> {
        let bytes = fs::read(&session.path).with_context(|| format!("reading {}", session.path.display()))?;
        Ok(parse_transcript(&bytes))
    }

    fn resume_command(&self, session: &Session) -> ResumeCommand {
        ResumeCommand {
            program: AgentKind::Codex.program().to_string(),
            args: vec!["resume".to_string(), session.id.clone()],
            cwd: PathBuf::from(&session.cwd),
            executable: None,
        }
    }

    /// The rollout file plus the files of every sub-agent it spawned (and
    /// theirs, recursively).
    fn related_paths(&self, session: &Session) -> Vec<PathBuf> {
        let mut children: HashMap<String, Vec<(String, PathBuf)>> = HashMap::new();
        for file in self.discover(&mut Vec::new()) {
            if file == session.path {
                continue;
            }
            if let Some((id, Some(Some(parent)))) = read_thread(&file) {
                children.entry(parent).or_default().push((id, file));
            }
        }
        let mut found = Vec::new();
        let mut parents = vec![session.id.clone()];
        while let Some(parent) = parents.pop() {
            // `remove` also guards against cycles in broken metadata.
            for (id, file) in children.remove(&parent).unwrap_or_default() {
                found.push(file);
                parents.push(id);
            }
        }
        found.sort();
        let mut paths = vec![session.path.clone()];
        paths.extend(found);
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::codex as fx;

    const ID: &str = "01a16ec7-96b5-7c11-b69f-156e8bb6ba12";

    fn stamp() -> FileStamp {
        FileStamp { mtime_ns: 7_000_000_000, size: 55 }
    }

    fn path() -> PathBuf {
        PathBuf::from(format!("/x/sessions/2026/10/24/rollout-2026-10-24T13-38-56-{ID}.jsonl"))
    }

    fn bytes(lines: &[serde_json::Value]) -> Vec<u8> {
        crate::testkit::jsonl_bytes(lines)
    }

    #[test]
    fn parses_meta_prompts_and_cwd() {
        let data = bytes(&[
            fx::meta(ID, "/home/u", "2026-10-24T16:38:56Z"),
            fx::developer("2026-10-24T16:39:00Z", "You are Codex"),
            fx::env_context("2026-10-24T16:39:00Z", "/home/u"),
            fx::turn_context("2026-10-24T16:39:00Z", "/home/u/app"),
            fx::user("2026-10-24T16:39:01Z", "optimize ubuntu"),
            fx::assistant("2026-10-24T16:39:30Z", "checking"),
            fx::function_call("2026-10-24T16:39:31Z", "shell", r#"{"command":"uname -a"}"#),
            fx::user("2026-10-24T16:40:00Z", "thanks"),
            fx::token_count("2026-10-24T16:41:00Z"),
        ]);
        let s = parse_meta(&path(), &data, stamp()).expect("session");
        assert_eq!(s.agent, AgentKind::Codex);
        assert_eq!(s.id, ID);
        assert_eq!(s.cwd, "/home/u/app");
        assert_eq!(s.first_prompt.as_deref(), Some("optimize ubuntu"));
        assert_eq!(s.last_prompt.as_deref(), Some("thanks"));
        assert_eq!(s.user_turns, 2);
        assert_eq!(s.git_branch.as_deref(), Some("main"));
        assert_eq!(s.started_at, parse_rfc3339_ms("2026-10-24T16:38:56Z"));
        assert_eq!(s.updated_at, parse_rfc3339_ms("2026-10-24T16:41:00Z").unwrap());
        assert_eq!(s.title, None);
    }

    #[test]
    fn spaced_json_parses_the_same() {
        let lines = [
            fx::meta(ID, "/home/u", "2026-10-24T16:38:56Z"),
            fx::turn_context("2026-10-24T16:39:00Z", "/home/u/app"),
            fx::user("2026-10-24T16:39:01Z", "optimize ubuntu"),
            fx::assistant("2026-10-24T16:39:30Z", "checking"),
        ];
        let spaced = crate::testkit::jsonl_bytes_spaced(&lines);
        assert!(parse_meta(&path(), &spaced, stamp()).is_some());
        assert_eq!(parse_meta(&path(), &spaced, stamp()), parse_meta(&path(), &bytes(&lines), stamp()));
        assert_eq!(parse_transcript(&spaced), parse_transcript(&bytes(&lines)));
    }

    #[test]
    fn id_falls_back_to_file_name() {
        let data = bytes(&[fx::user("2026-10-24T16:39:01Z", "q")]);
        assert_eq!(parse_meta(&path(), &data, stamp()).unwrap().id, ID);
        assert_eq!(id_from_file_name(Path::new("rollout-short.jsonl")), None);
    }

    #[test]
    fn sessions_without_prompts_are_skipped() {
        let data = bytes(&[fx::meta(ID, "/h", "2026-10-24T16:38:56Z"), fx::env_context("2026-10-24T16:39:00Z", "/h")]);
        assert_eq!(parse_meta(&path(), &data, stamp()), None);
    }

    #[test]
    fn transcript_has_users_assistants_and_tools() {
        let data = bytes(&[
            fx::meta(ID, "/h", "2026-10-24T16:38:56Z"),
            fx::env_context("2026-10-24T16:39:00Z", "/h"),
            fx::user("2026-10-24T16:39:01Z", "hello"),
            fx::assistant("2026-10-24T16:39:02Z", "a"),
            fx::assistant("2026-10-24T16:39:03Z", "b"),
            fx::function_call("2026-10-24T16:39:04Z", "shell", r#"{"command":"ls"}"#),
            fx::assistant("2026-10-24T16:39:05Z", "c"),
        ]);
        let got: Vec<(Role, String)> = parse_transcript(&data).into_iter().map(|t| (t.role, t.text)).collect();
        assert_eq!(
            got,
            vec![
                (Role::User, "hello".to_string()),
                (Role::Assistant, "a\n\nb".to_string()),
                (Role::Tool, "shell: ls".to_string()),
                (Role::Assistant, "c".to_string()),
            ]
        );
    }

    #[test]
    fn tool_calls_show_their_details() {
        let patch = "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-a\n+b\n*** End Patch\n";
        let data = bytes(&[
            fx::user("2026-10-24T16:39:01Z", "go"),
            fx::custom_tool_call("2026-10-24T16:39:02Z", "apply_patch", patch),
            fx::local_shell_call("2026-10-24T16:39:03Z", &["bash", "-lc", "cargo test"]),
            fx::function_call("2026-10-24T16:39:04Z", "exec_command", r#"{"cmd":"rg TODO src"}"#),
            fx::function_call("2026-10-24T16:39:05Z", "shell", r#"{"command":["bash","-lc","ls -la"]}"#),
            fx::function_call("2026-10-24T16:39:06Z", "broken", "{not json"),
        ]);
        let tools: Vec<String> =
            parse_transcript(&data).into_iter().filter(|t| t.role == Role::Tool).map(|t| t.text).collect();
        assert_eq!(
            tools,
            vec![
                "apply_patch: update src/main.rs",
                "shell: cargo test",
                "exec_command: rg TODO src",
                "shell: ls -la",
                "broken"
            ]
        );
    }

    #[test]
    fn image_prompts_and_quoted_tags_count() {
        let image = fx::user_content(
            "2026-10-24T16:39:01Z",
            serde_json::json!([{"type": "input_image", "image_url": "data:"}]),
        );
        let quoted = fx::user("2026-10-24T16:39:02Z", "<environment_context> is in my prompt, is that bad?");
        let s = parse_meta(&path(), &bytes(&[image, quoted]), stamp()).unwrap();
        assert_eq!(s.user_turns, 2);
        assert_eq!(s.first_prompt.as_deref(), Some("[1 attachment]"));
    }

    #[test]
    fn any_json_spacing_parses() {
        let lines = [fx::meta(ID, "/home/u", "2026-10-24T16:38:56Z"), fx::user("2026-10-24T16:39:01Z", "q")];
        let odd = String::from_utf8(bytes(&lines))
            .unwrap()
            .replace("\"type\":\"response_item\"", "\"type\" : \"response_item\"")
            .replace("\"role\":\"user\"", "\"role\":\t\"user\"");
        assert_eq!(parse_meta(&path(), odd.as_bytes(), stamp()), parse_meta(&path(), &bytes(&lines), stamp()));
    }

    const PARENT: &str = "01a16f50-a307-7f00-9a19-9549dc24d174";
    const CHILD: &str = "01a16f5e-e564-7780-be78-5227d75ea51b";
    const GRANDCHILD: &str = "01a16f5f-0afe-7cf2-a6e3-bedf731a9dc9";

    /// A sub-agent rollout as Codex writes it: its own meta, then a copy of
    /// the parent's meta and history, then its own work.
    fn subagent_file(id: &str, parent: &str) -> Vec<serde_json::Value> {
        vec![
            fx::subagent_meta(id, parent, "/w", "2026-10-24T16:40:00Z"),
            fx::meta(PARENT, "/w", "2026-10-24T16:38:56Z"),
            fx::user("2026-10-24T16:39:01Z", "audit this project"),
            fx::assistant("2026-10-24T16:40:05Z", "looking at the storage layer"),
        ]
    }

    #[test]
    fn subagent_threads_are_not_sessions() {
        let data = bytes(&subagent_file(CHILD, PARENT));
        assert_eq!(parse_meta(&path(), &data, stamp()), None);
        // Older shape: only `source` says it was spawned.
        let mut old = fx::subagent_meta(CHILD, PARENT, "/w", "2026-10-24T16:40:00Z");
        let payload = old["payload"].as_object_mut().unwrap();
        payload.remove("thread_source");
        payload.remove("parent_thread_id");
        let data = bytes(&[old, fx::user("2026-10-24T16:39:01Z", "audit this project")]);
        assert_eq!(parse_meta(&path(), &data, stamp()), None);
        // The parent itself is a session.
        let parent =
            bytes(&[fx::meta(PARENT, "/w", "2026-10-24T16:38:56Z"), fx::user("2026-10-24T16:39:01Z", "audit")]);
        assert!(parse_meta(&path(), &parent, stamp()).is_some());
    }

    #[test]
    fn deleting_a_session_takes_its_subagents_along() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".codex");
        let parent = fx::write_session(
            &root,
            PARENT,
            &[fx::meta(PARENT, "/w", "2026-10-24T16:38:56Z"), fx::user("2026-10-24T16:39:01Z", "audit this project")],
        );
        let child = fx::write_session(&root, CHILD, &subagent_file(CHILD, PARENT));
        let grandchild = fx::write_session(&root, GRANDCHILD, &subagent_file(GRANDCHILD, CHILD));
        let other = fx::write_session(
            &root,
            ID,
            &[fx::meta(ID, "/w", "2026-10-24T16:38:56Z"), fx::user("2026-10-24T16:39:01Z", "unrelated")],
        );

        let provider = CodexProvider::new(root.clone());
        let sessions: Vec<Session> = provider
            .discover(&mut Vec::new())
            .iter()
            .filter_map(|f| provider.parse(f, FileStamp::of(f).unwrap()).unwrap())
            .collect();
        let mut ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec![ID, PARENT], "only the two real sessions are listed");

        let parent_session = sessions.iter().find(|s| s.id == PARENT).unwrap();
        let mut expected = vec![child, grandchild];
        expected.sort();
        expected.insert(0, parent);
        assert_eq!(provider.related_paths(parent_session), expected);
        let other_session = sessions.iter().find(|s| s.id == ID).unwrap();
        assert_eq!(provider.related_paths(other_session), vec![other]);
    }

    #[test]
    fn decorate_applies_latest_thread_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".codex");
        let file = fx::write_session(
            &root,
            ID,
            &[fx::meta(ID, "/h", "2026-10-24T16:38:56Z"), fx::user("2026-10-24T16:39:01Z", "q")],
        );
        fx::write_index(&root, &[(ID, "Old name"), ("other", "Other"), (ID, "New name")]);

        let provider = CodexProvider::new(root.clone());
        assert_eq!(provider.discover(&mut Vec::new()), vec![file.clone()]);
        let mut sessions = vec![provider.parse(&file, FileStamp::of(&file).unwrap()).unwrap().unwrap()];
        provider.decorate(&mut sessions);
        assert_eq!(sessions[0].title.as_deref(), Some("New name"));
        provider.decorate(&mut sessions);
        assert_eq!(provider.title_reads.load(std::sync::atomic::Ordering::SeqCst), 1, "unchanged index is reused");
        fx::write_index(&root, &[(ID, "Newest name, longer")]);
        provider.decorate(&mut sessions);
        assert_eq!(sessions[0].title.as_deref(), Some("Newest name, longer"));
        assert_eq!(provider.related_paths(&sessions[0]), vec![file]);
        let cmd = provider.resume_command(&sessions[0]);
        assert_eq!((cmd.program.as_str(), cmd.args.clone()), ("codex", vec!["resume".to_string(), ID.to_string()]));
    }
}
