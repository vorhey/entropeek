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
//! their `session_meta` (`thread_source: "subagent"`, `parent_thread_id`,
//! or a `subagent`/`internal` `source`) and starting with a copy of the
//! parent's history. They are part of the parent session, not sessions of
//! their own: they are not listed, and deleting the parent takes them along.
//!
//! A fork can instead *refer* to its base's history (`history_base`: the
//! base thread and how many of its bytes are inherited) rather than copy
//! it. Its transcript is then the base's prefix followed by its own
//! records, and the base cannot be deleted while such a fork remains:
//! entropeek refuses, rather than leave the fork without its beginning.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::value::RawValue;

use super::content::{custom_tool_summary, extract, shell_action_summary, tool_summary};
use super::{FileStamp, Provider, ResumeCommand, Unread, files_recursive, has_extension, push_turn};
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

    /// The thread names. A failed read is reported and not remembered: the
    /// last titles read are used meanwhile, and the next scan tries again.
    fn titles(&self, errors: &mut Vec<String>) -> Arc<HashMap<String, String>> {
        let index = self.root.join(INDEX_FILE);
        let stamp = match FileStamp::try_of(&index) {
            Ok(stamp) => stamp,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Arc::default(),
            Err(e) => {
                errors.push(format!("cannot read {}: {e}", index.display()));
                return self.last_titles();
            }
        };
        let mut memo = self.titles.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((seen, titles)) = memo.as_ref()
            && *seen == stamp
        {
            return titles.clone();
        }
        #[cfg(test)]
        self.title_reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match read_titles(&self.root) {
            Ok(titles) => {
                let titles = Arc::new(titles);
                *memo = Some((stamp, titles.clone()));
                titles
            }
            Err(e) => {
                errors.push(format!("cannot read {}: {e}", index.display()));
                memo.as_ref().map(|(_, titles)| titles.clone()).unwrap_or_default()
            }
        }
    }

    fn last_titles(&self) -> Arc<HashMap<String, String>> {
        let memo = self.titles.lock().unwrap_or_else(|e| e.into_inner());
        memo.as_ref().map(|(_, titles)| titles.clone()).unwrap_or_default()
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
    /// `"cli"`, `"vscode"`, … for sessions people start;
    /// `{"subagent": {"thread_spawn": {"parent_thread_id": ...}}}`,
    /// `{"subagent": "review"}` (and other kinds) or `internal` for threads
    /// Codex starts itself.
    #[serde(borrow, default)]
    source: Option<&'a RawValue>,
    /// For a fork that refers to its base's history instead of copying it.
    #[serde(borrow, default)]
    history_base: Option<HistoryBase<'a>>,
}

#[derive(Deserialize)]
struct HistoryBase<'a> {
    #[serde(borrow, default)]
    thread_id: Option<Cow<'a, str>>,
    /// How many bytes of the base rollout the fork inherits.
    #[serde(default)]
    end_byte_offset: Option<u64>,
}

/// Whether a `source` says Codex started this thread itself (a sub-agent
/// of any kind, or an internal thread), and if so its parent, when known.
fn non_root_source(source: &serde_json::Value) -> Option<Option<String>> {
    use serde_json::Value;
    match source {
        Value::String(kind) if kind == "subagent" || kind == "internal" => Some(None),
        Value::Object(map) => {
            if let Some(subagent) = map.get("subagent") {
                let parent = subagent.pointer("/thread_spawn/parent_thread_id").and_then(Value::as_str);
                return Some(parent.map(str::to_string));
            }
            map.contains_key("internal").then_some(None)
        }
        _ => None,
    }
}

impl Meta<'_> {
    /// For a thread Codex started itself (a sub-agent): its parent's id,
    /// possibly unknown.
    fn subagent_of(&self) -> Option<Option<String>> {
        let spawn = self
            .source
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw.get()).ok())
            .and_then(|source| non_root_source(&source));
        let marked = self.thread_source.as_deref() == Some("subagent");
        match (spawn, &self.parent_thread_id) {
            (Some(parent), own) => Some(parent.or_else(|| own.as_deref().map(str::to_string))),
            (None, Some(parent)) => Some(Some(parent.to_string())),
            (None, None) if marked => Some(None),
            (None, None) => None,
        }
    }
}

/// How one rollout file relates to others.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Thread {
    id: String,
    /// For a sub-agent: its parent's id, possibly unknown.
    subagent_of: Option<Option<String>>,
    /// For a fork that refers to its base's history: the base thread.
    base: Option<String>,
    /// How many of the base's bytes it inherits.
    base_bytes: Option<u64>,
}

/// How a rollout file relates to others, from its own (first)
/// `session_meta`. `Ok(None)` if it has none; an error if it cannot be read.
fn read_thread(path: &Path) -> std::io::Result<Option<Thread>> {
    use std::io::BufRead;
    let file = fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    for line in reader.split(b'\n').take(16) {
        let line = line?;
        let Ok(record) = serde_json::from_slice::<Record>(&line) else { continue };
        if record.kind.as_deref() != Some("session_meta") {
            continue;
        }
        let Some(meta) = parse_payload::<Meta>(&record) else { return Ok(None) };
        let id = meta.id.as_deref().filter(|id| !id.is_empty()).map(str::to_string).or_else(|| id_from_file_name(path));
        let Some(id) = id else { return Ok(None) };
        let base = meta.history_base.as_ref();
        return Ok(Some(Thread {
            subagent_of: meta.subagent_of(),
            base: base.and_then(|b| b.thread_id.as_deref()).filter(|b| !b.is_empty()).map(str::to_string),
            base_bytes: base.and_then(|b| b.end_byte_offset),
            id,
        }));
    }
    Ok(None)
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
    let mut has_base = false;

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
                    has_base = meta.history_base.as_ref().is_some_and(|b| b.thread_id.is_some());
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

    // A fork that refers to its base's history is a session even before
    // anything new is said in it.
    if user_turns == 0 && !has_base {
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

/// Reads `session_index.jsonl` into `id -> thread name`. Later lines win;
/// a name with nothing printable in it is ignored, so the session keeps
/// falling back to its first prompt. A missing index has no titles.
pub fn read_titles(root: &Path) -> std::io::Result<HashMap<String, String>> {
    let bytes = match fs::read(root.join(INDEX_FILE)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e),
    };
    let mut titles = HashMap::new();
    for line in jsonl::lines(&bytes) {
        if let Ok(entry) = serde_json::from_slice::<IndexEntry>(line)
            && let Some(name) = entry.thread_name.map(|n| text::summary(&n, 200)).filter(|n| !n.is_empty())
        {
            titles.insert(entry.id, name);
        }
    }
    Ok(titles)
}

/// Rollout files kept outside `sessions/` that may still refer to others.
const ARCHIVE_DIR: &str = "archived_sessions";

impl Provider for CodexProvider {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn discover(&self, unread: &mut Unread) -> Vec<PathBuf> {
        let mut out = Vec::new();
        files_recursive(&self.root.join("sessions"), 4, &is_rollout, &mut out, unread);
        out
    }

    fn parse(&self, path: &Path, stamp: FileStamp) -> Result<Option<Session>> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(parse_meta(path, &bytes, stamp))
    }

    fn decorate(&self, sessions: &mut [Session], errors: &mut Vec<String>) {
        if sessions.is_empty() {
            return;
        }
        let titles = self.titles(errors);
        for session in sessions {
            session.title = titles.get(&session.id).cloned();
        }
    }

    /// The session's own records, preceded by the history it inherits from
    /// its base when it is a fork that refers to it.
    fn transcript(&self, session: &Session) -> Result<Vec<Turn>> {
        let bytes = fs::read(&session.path).with_context(|| format!("reading {}", session.path.display()))?;
        let own = parse_transcript(&bytes);
        let Some(thread) = read_thread(&session.path).ok().flatten().filter(|t| t.base.is_some()) else {
            return Ok(own);
        };
        let mut turns = self.inherited(&thread, &session.path, 0).unwrap_or_else(|e| {
            vec![Turn {
                role: Role::Tool,
                text: text::one_line(&format!("earlier history unavailable: {e:#}")),
                timestamp: None,
            }]
        });
        for turn in own {
            push_turn(&mut turns, turn);
        }
        Ok(turns)
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
    /// theirs, recursively). Refuses when a rollout file could not be read
    /// (one of them might belong to the session), or when a fork that is
    /// not being deleted refers to the history of one that is.
    fn related_paths(&self, session: &Session) -> Result<Vec<PathBuf>> {
        let threads = self.threads(true)?;
        let mut children: HashMap<&str, Vec<(&str, &Path)>> = HashMap::new();
        for (file, thread) in &threads {
            if *file == session.path {
                continue;
            }
            if let Some(Some(parent)) = &thread.subagent_of {
                children.entry(parent.as_str()).or_default().push((thread.id.as_str(), file.as_path()));
            }
        }
        let mut found: Vec<PathBuf> = Vec::new();
        let mut ids: Vec<&str> = vec![session.id.as_str()];
        let mut parents = vec![session.id.as_str()];
        while let Some(parent) = parents.pop() {
            // `remove` also guards against cycles in broken metadata.
            for (id, file) in children.remove(parent).unwrap_or_default() {
                found.push(file.to_path_buf());
                ids.push(id);
                parents.push(id);
            }
        }
        found.sort();
        let mut paths = vec![session.path.clone()];
        paths.extend(found);

        let deleting: std::collections::HashSet<&Path> = paths.iter().map(PathBuf::as_path).collect();
        for (file, thread) in &threads {
            if deleting.contains(file.as_path()) {
                continue;
            }
            if let Some(base) = thread.base.as_deref().filter(|base| ids.contains(base)) {
                anyhow::bail!(
                    "cannot delete this session: Codex session {} is a fork that refers to the history of {base} \
                     instead of copying it ({}); delete the fork first",
                    text::one_line(&thread.id),
                    file.display()
                );
            }
        }
        Ok(paths)
    }
}

fn is_rollout(p: &Path) -> bool {
    has_extension(p, "jsonl") && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("rollout-"))
}

impl CodexProvider {
    /// Every rollout file (with `archived` also the archived ones) and how
    /// it relates to others. Files that vanish meanwhile are skipped; any
    /// other failure to read is an error, since the answer would be
    /// incomplete.
    fn threads(&self, archived: bool) -> Result<Vec<(PathBuf, Thread)>> {
        let mut unread = Unread::default();
        let mut files = self.discover(&mut unread);
        if archived {
            files_recursive(&self.root.join(ARCHIVE_DIR), 4, &is_rollout, &mut files, &mut unread);
        }
        if let Some(error) = unread.errors.first() {
            anyhow::bail!("could not check every Codex session for related files: {error}");
        }
        let mut threads = Vec::with_capacity(files.len());
        for file in files {
            match read_thread(&file) {
                Ok(Some(thread)) => threads.push((file, thread)),
                Ok(None) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => anyhow::bail!(
                    "could not check every Codex session for related files: cannot read {}: {e}",
                    file.display()
                ),
            }
        }
        Ok(threads)
    }

    /// The turns `thread` inherits from its base, itself resolved the same
    /// way. Depth-limited, so a cycle in broken metadata ends.
    fn inherited(&self, thread: &Thread, own_path: &Path, depth: usize) -> Result<Vec<Turn>> {
        let Some(base) = thread.base.as_deref() else { return Ok(Vec::new()) };
        if depth >= 16 {
            anyhow::bail!("Codex session {} refers to the history of others too deeply (a cycle?)", thread.id);
        }
        let threads = self.threads(true)?;
        let Some((path, base_thread)) = threads.into_iter().find(|(path, t)| t.id == base && path != own_path) else {
            anyhow::bail!(
                "the history this fork continues (session {base}) is missing; only the fork's own part can be shown"
            );
        };
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let end = thread.base_bytes.map_or(bytes.len(), |n| usize::try_from(n).unwrap_or(usize::MAX).min(bytes.len()));
        let mut turns = self.inherited(&base_thread, &path, depth + 1)?;
        for turn in parse_transcript(&bytes[..end]) {
            push_turn(&mut turns, turn);
        }
        Ok(turns)
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
            .discover(&mut Default::default())
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
        assert_eq!(provider.related_paths(parent_session).unwrap(), expected);
        let other_session = sessions.iter().find(|s| s.id == ID).unwrap();
        assert_eq!(provider.related_paths(other_session).unwrap(), vec![other]);
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
        assert_eq!(provider.discover(&mut Default::default()), vec![file.clone()]);
        let mut sessions = vec![provider.parse(&file, FileStamp::of(&file).unwrap()).unwrap().unwrap()];
        provider.decorate(&mut sessions, &mut Vec::new());
        assert_eq!(sessions[0].title.as_deref(), Some("New name"));
        provider.decorate(&mut sessions, &mut Vec::new());
        assert_eq!(provider.title_reads.load(std::sync::atomic::Ordering::SeqCst), 1, "unchanged index is reused");
        fx::write_index(&root, &[(ID, "Newest name, longer")]);
        provider.decorate(&mut sessions, &mut Vec::new());
        assert_eq!(sessions[0].title.as_deref(), Some("Newest name, longer"));
        assert_eq!(provider.related_paths(&sessions[0]).unwrap(), vec![file]);
        let cmd = provider.resume_command(&sessions[0]);
        assert_eq!((cmd.program.as_str(), cmd.args.clone()), ("codex", vec!["resume".to_string(), ID.to_string()]));
    }

    #[test]
    fn every_non_root_source_shape_is_a_subagent() {
        for source in [
            serde_json::json!({"subagent": "review"}),
            serde_json::json!({"subagent": "compact"}),
            serde_json::json!({"subagent": "memory_consolidation"}),
            serde_json::json!({"subagent": {"some_future_kind": {}}}),
            serde_json::json!({"internal": "anything"}),
            serde_json::json!("internal"),
        ] {
            let mut meta = fx::meta(CHILD, "/w", "2026-10-24T16:40:00Z");
            meta["payload"]["source"] = source.clone();
            let data = bytes(&[meta, fx::user("2026-10-24T16:39:01Z", "inherited prompt")]);
            assert_eq!(parse_meta(&path(), &data, stamp()), None, "{source}");
        }
        for source in [serde_json::json!("cli"), serde_json::json!("vscode"), serde_json::json!({"mcp": {}})] {
            let mut meta = fx::meta(ID, "/w", "2026-10-24T16:40:00Z");
            meta["payload"]["source"] = source.clone();
            let data = bytes(&[meta, fx::user("2026-10-24T16:39:01Z", "my prompt")]);
            assert!(parse_meta(&path(), &data, stamp()).is_some(), "{source}");
        }
    }

    #[test]
    fn titles_with_nothing_printable_fall_back_to_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".codex");
        let file = fx::write_session(
            &root,
            ID,
            &[fx::meta(ID, "/h", "2026-10-24T16:38:56Z"), fx::user("2026-10-24T16:39:01Z", "fallback prompt")],
        );
        fx::write_index(&root, &[(ID, "\u{7}\u{202e}")]);
        let provider = CodexProvider::new(root);
        let mut sessions = vec![provider.parse(&file, FileStamp::of(&file).unwrap()).unwrap().unwrap()];
        provider.decorate(&mut sessions, &mut Vec::new());
        assert_eq!(sessions[0].title, None);
        assert_eq!(sessions[0].label(), "fallback prompt");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_title_index_is_reported_and_retried() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".codex");
        let file = fx::write_session(
            &root,
            ID,
            &[fx::meta(ID, "/h", "2026-10-24T16:38:56Z"), fx::user("2026-10-24T16:39:01Z", "q")],
        );
        fx::write_index(&root, &[(ID, "Important title")]);
        let index = root.join(INDEX_FILE);
        let provider = CodexProvider::new(root.clone());
        let mut sessions = vec![provider.parse(&file, FileStamp::of(&file).unwrap()).unwrap().unwrap()];
        fs::set_permissions(&index, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&index).is_ok() {
            return; // running as root
        }
        let mut errors = Vec::new();
        provider.decorate(&mut sessions, &mut errors);
        assert_eq!(sessions[0].title, None);
        assert!(errors[0].contains("session_index"), "{errors:?}");
        fs::set_permissions(&index, fs::Permissions::from_mode(0o600)).unwrap();
        provider.decorate(&mut sessions, &mut Vec::new());
        assert_eq!(sessions[0].title.as_deref(), Some("Important title"), "retried once readable");
        // A failed refresh keeps the titles already known.
        fx::write_index(&root, &[(ID, "A newer, longer title")]);
        fs::set_permissions(&index, fs::Permissions::from_mode(0o000)).unwrap();
        let mut errors = Vec::new();
        provider.decorate(&mut sessions, &mut errors);
        fs::set_permissions(&index, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("Important title"));
        assert_eq!(errors.len(), 1);
        provider.decorate(&mut sessions, &mut Vec::new());
        assert_eq!(sessions[0].title.as_deref(), Some("A newer, longer title"));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_subagent_stops_the_delete() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".codex");
        let parent = fx::write_session(
            &root,
            PARENT,
            &[fx::meta(PARENT, "/w", "2026-10-24T16:38:56Z"), fx::user("2026-10-24T16:39:01Z", "review")],
        );
        let child = fx::write_session(&root, CHILD, &subagent_file(CHILD, PARENT));
        let provider = CodexProvider::new(root.clone());
        let session = provider.parse(&parent, FileStamp::of(&parent).unwrap()).unwrap().unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&child).is_ok() {
            return; // running as root
        }
        let result = provider.related_paths(&session);
        fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
        let err = result.unwrap_err();
        assert!(format!("{err:#}").contains("could not check"), "{err:#}");
        assert_eq!(provider.related_paths(&session).unwrap(), vec![parent, child]);
    }

    /// A base session and a fork that refers to its first two records.
    fn referenced_fork(root: &Path) -> (PathBuf, PathBuf) {
        let base = fx::write_session(
            root,
            PARENT,
            &[fx::meta(PARENT, "/w", "2026-10-24T16:38:56Z"), fx::user("2026-10-24T16:39:01Z", "inherited prompt")],
        );
        let inherited = fs::metadata(&base).unwrap().len();
        // Later records in the base are not part of the fork.
        let mut later = fs::read(&base).unwrap();
        later.extend(bytes(&[fx::user("2026-10-24T16:45:00Z", "only in the base")]));
        fs::write(&base, later).unwrap();
        let mut meta = fx::meta(CHILD, "/w", "2026-10-24T16:40:00Z");
        meta["payload"]["thread_source"] = serde_json::json!("user");
        meta["payload"]["forked_from_id"] = serde_json::json!(PARENT);
        meta["payload"]["history_mode"] = serde_json::json!("paginated");
        meta["payload"]["history_base"] =
            serde_json::json!({"thread_id": PARENT, "end_ordinal_exclusive": 2, "end_byte_offset": inherited});
        let fork = fx::write_session(root, CHILD, &[meta, fx::user("2026-10-24T16:41:00Z", "fork prompt")]);
        (base, fork)
    }

    #[test]
    fn a_base_whose_history_a_fork_refers_to_cannot_be_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".codex");
        let (base, fork) = referenced_fork(&root);
        let provider = CodexProvider::new(root.clone());
        let base_session = provider.parse(&base, FileStamp::of(&base).unwrap()).unwrap().unwrap();
        let fork_session = provider.parse(&fork, FileStamp::of(&fork).unwrap()).unwrap().unwrap();
        let err = provider.related_paths(&base_session).unwrap_err();
        assert!(err.to_string().contains("delete the fork first"), "{err}");
        // The fork shows the history it continues, then its own.
        let turns: Vec<String> = provider.transcript(&fork_session).unwrap().into_iter().map(|t| t.text).collect();
        assert_eq!(turns, vec!["inherited prompt", "fork prompt"]);
        // The fork itself can go; then so can the base.
        assert_eq!(provider.related_paths(&fork_session).unwrap(), vec![fork.clone()]);
        fs::remove_file(&fork).unwrap();
        assert_eq!(provider.related_paths(&base_session).unwrap(), vec![base]);
        // An archived fork protects its base too.
        let archived = root.join(ARCHIVE_DIR).join("rollout-x.jsonl");
        fs::create_dir_all(archived.parent().unwrap()).unwrap();
        let (base, fork) = referenced_fork(&root);
        fs::rename(&fork, &archived).unwrap();
        let base_session = provider.parse(&base, FileStamp::of(&base).unwrap()).unwrap().unwrap();
        assert!(provider.related_paths(&base_session).is_err());
    }

    #[test]
    fn a_fork_with_nothing_new_is_still_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".codex");
        let (_, fork) = referenced_fork(&root);
        let lines: Vec<serde_json::Value> =
            fs::read_to_string(&fork).unwrap().lines().take(1).map(|l| serde_json::from_str(l).unwrap()).collect();
        fs::write(&fork, bytes(&lines)).unwrap();
        let provider = CodexProvider::new(root);
        let session = provider.parse(&fork, FileStamp::of(&fork).unwrap()).unwrap().expect("listed");
        assert_eq!(session.user_turns, 0);
        let turns: Vec<String> = provider.transcript(&session).unwrap().into_iter().map(|t| t.text).collect();
        assert_eq!(turns, vec!["inherited prompt"]);
        // Without its base, the fork's own part is still shown, with a note.
        fs::remove_file(fx::session_path(&provider.root, PARENT)).unwrap();
        let turns = provider.transcript(&session).unwrap();
        assert_eq!(turns.len(), 1);
        assert!(turns[0].text.contains("missing"), "{}", turns[0].text);
    }
}
