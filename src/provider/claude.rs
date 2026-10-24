//! Claude Code sessions: `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`.
//!
//! Each line is a record with a top-level `type`. The ones we read:
//! - `user`: a user message. Many are tool results or injected context, not prompts.
//! - `assistant`: a reply, often split across several records.
//! - `ai-title` / `custom-title`: the generated title and the one set with `/rename`.

use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::value::RawValue;

use super::content::{Block, extract};
use super::{FileStamp, Provider, ResumeCommand, files_in_subdirs, has_extension, push_turn};
use crate::jsonl::{self, Marker};
use crate::model::{AgentKind, Role, Session, Turn};
use crate::text;
use crate::timefmt::parse_rfc3339_ms;

pub struct ClaudeProvider {
    root: PathBuf,
}

impl ClaudeProvider {
    pub fn new(root: PathBuf) -> ClaudeProvider {
        ClaudeProvider { root }
    }
}

#[derive(Deserialize)]
struct Record<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'a, str>>,
    #[serde(rename = "isMeta", default)]
    is_meta: Option<bool>,
    #[serde(rename = "isSidechain", default)]
    is_sidechain: Option<bool>,
    #[serde(borrow, default)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    cwd: Option<Cow<'a, str>>,
    #[serde(rename = "gitBranch", borrow, default)]
    git_branch: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    message: Option<Message<'a>>,
    #[serde(rename = "aiTitle", borrow, default)]
    ai_title: Option<Cow<'a, str>>,
    #[serde(rename = "customTitle", borrow, default)]
    custom_title: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow, default)]
    role: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    content: Option<&'a RawValue>,
}

impl Record<'_> {
    fn is(&self, kind: &str) -> bool {
        self.kind.as_deref() == Some(kind)
    }

    fn is_main_thread(&self) -> bool {
        !self.is_sidechain.unwrap_or(false)
    }

    /// What the user typed in this record, if anything. Text sent along
    /// with tool results counts; the results themselves never do. Layout is
    /// kept (see [`text::transcript_text`]).
    fn user_prompt(&self) -> Option<String> {
        if !self.is("user") || self.is_meta.unwrap_or(false) || !self.is_main_thread() {
            return None;
        }
        let message = self.message.as_ref()?;
        if message.role.as_deref() != Some("user") {
            return None;
        }
        let prompt = extract(message.content?).user_text()?;
        text::is_user_prompt(prompt.trim()).then_some(prompt)
    }
}

struct Markers {
    user: Marker,
    ai_title: Marker,
    custom_title: Marker,
    tool_result: Marker,
    text: Marker,
}

impl Markers {
    fn new() -> Markers {
        Markers {
            user: Marker::token("user"),
            ai_title: Marker::token("ai-title"),
            custom_title: Marker::token("custom-title"),
            tool_result: Marker::token("tool_result"),
            text: Marker::token("text"),
        }
    }
}

/// A title as shown, or `None` if nothing printable is left.
fn title(value: &str) -> Option<String> {
    Some(text::summary(value, 200)).filter(|t| !t.is_empty())
}

/// Parses session metadata from the raw bytes of a Claude session file.
pub fn parse_meta(path: &Path, bytes: &[u8], stamp: FileStamp) -> Option<Session> {
    let markers = Markers::new();
    let mut first_prompt = None;
    let mut last_prompt = None;
    let mut user_turns = 0u32;
    let mut cwd = String::new();
    let mut git_branch = None;
    let mut started_at = None;
    let mut ai_title = None;
    let mut custom_title = None;

    for line in jsonl::lines(bytes) {
        let is_user = markers.user.in_line(line);
        let is_title = markers.ai_title.in_line(line) || markers.custom_title.in_line(line);
        // Tool results are most of the user records and can be huge. One
        // without any text block cannot carry a prompt, so skip it unparsed.
        let result_only = is_user && markers.tool_result.in_line(line) && !markers.text.in_line(line);
        if !(is_user || is_title) || result_only {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Record>(line) else {
            continue;
        };
        if let Some(value) = record.ai_title.as_deref().filter(|_| record.is("ai-title")).and_then(title) {
            ai_title = Some(value);
        }
        if let Some(value) = record.custom_title.as_deref().filter(|_| record.is("custom-title")).and_then(title) {
            custom_title = Some(value);
        }
        if !record.is("user") || !record.is_main_thread() {
            continue;
        }
        if started_at.is_none() {
            started_at = record.timestamp.as_deref().and_then(parse_rfc3339_ms);
        }
        if let Some(value) = record.cwd.as_deref().filter(|v| !v.is_empty()) {
            cwd = value.to_string();
        }
        // Present but `HEAD` (detached) or empty means "no branch now".
        if let Some(branch) = record.git_branch.as_deref() {
            git_branch = (!branch.is_empty() && branch != "HEAD").then(|| branch.to_string());
        }
        if let Some(prompt) = record.user_prompt() {
            user_turns += 1;
            if first_prompt.is_none() {
                first_prompt = Some(text::summary(&prompt, 300));
            }
            last_prompt = Some(prompt);
        }
    }

    if user_turns == 0 {
        return None;
    }
    let id = path.file_stem()?.to_string_lossy().into_owned();
    Some(Session {
        agent: AgentKind::Claude,
        id,
        path: path.to_path_buf(),
        cwd,
        title: custom_title.or(ai_title),
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

/// Parses the full conversation from a Claude session file.
pub fn parse_transcript(bytes: &[u8]) -> Vec<Turn> {
    let user = Marker::token("user");
    let assistant = Marker::token("assistant");
    let mut turns = Vec::new();
    for line in jsonl::lines(bytes) {
        if !user.in_line(line) && !assistant.in_line(line) {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Record>(line) else {
            continue;
        };
        if !record.is_main_thread() {
            continue;
        }
        let timestamp = record.timestamp.as_deref().and_then(parse_rfc3339_ms);
        if let Some(prompt) = record.user_prompt() {
            push_turn(&mut turns, Turn { role: Role::User, text: prompt, timestamp });
            continue;
        }
        if !record.is("assistant") {
            continue;
        }
        let Some(content) = record.message.as_ref().and_then(|m| m.content) else {
            continue;
        };
        // In the order written: text, a tool call, more text…
        for block in extract(content).blocks {
            match block {
                Block::Text(t) => {
                    push_turn(&mut turns, Turn { role: Role::Assistant, text: text::transcript_text(&t), timestamp })
                }
                Block::Tool(tool) => turns.push(Turn { role: Role::Tool, text: text::one_line(&tool), timestamp }),
                Block::Attachment | Block::ToolResult => {}
            }
        }
    }
    turns
}

impl Provider for ClaudeProvider {
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn discover(&self, errors: &mut Vec<String>) -> Vec<PathBuf> {
        // Only `projects/<dir>/<id>.jsonl`; subagent transcripts live deeper
        // (`<id>/subagents/*.jsonl`) and are not sessions of their own.
        files_in_subdirs(&self.root.join("projects"), |p| has_extension(p, "jsonl"), errors)
    }

    fn parse(&self, path: &Path, stamp: FileStamp) -> Result<Option<Session>> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(parse_meta(path, &bytes, stamp))
    }

    fn transcript(&self, session: &Session) -> Result<Vec<Turn>> {
        let bytes = fs::read(&session.path).with_context(|| format!("reading {}", session.path.display()))?;
        Ok(parse_transcript(&bytes))
    }

    fn resume_command(&self, session: &Session) -> ResumeCommand {
        ResumeCommand {
            program: AgentKind::Claude.program().to_string(),
            args: vec!["--resume".to_string(), session.id.clone()],
            cwd: PathBuf::from(&session.cwd),
            executable: None,
        }
    }

    fn related_paths(&self, session: &Session) -> Vec<PathBuf> {
        let id = &session.id;
        let mut paths = vec![session.path.clone()];
        // `<id>/` next to the transcript holds subagent logs and tool output.
        let candidates = [
            session.path.with_extension(""),
            self.root.join("session-env").join(id),
            self.root.join("file-history").join(id),
        ];
        paths.extend(candidates.into_iter().filter(|p| p.exists()));
        if let Ok(entries) = fs::read_dir(self.root.join("todos")) {
            let prefix = format!("{id}-");
            let mut todos: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                .map(|e| e.path())
                .collect();
            todos.sort();
            paths.extend(todos);
        }
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::claude as fx;

    const ID: &str = "11111111-2222-3333-4444-555555555555";
    const CWD: &str = "/home/u/proj";

    fn stamp() -> FileStamp {
        FileStamp { mtime_ns: 9_000_000_000, size: 123 }
    }

    fn bytes(lines: &[serde_json::Value]) -> Vec<u8> {
        crate::testkit::jsonl_bytes(lines)
    }

    fn path() -> PathBuf {
        PathBuf::from(format!("/x/projects/-home-u-proj/{ID}.jsonl"))
    }

    #[test]
    fn parses_prompts_titles_and_times() {
        let data = bytes(&[
            fx::snapshot(),
            fx::user(ID, CWD, "2026-01-01T10:00:00Z", "first question"),
            fx::assistant_text(ID, "2026-01-01T10:00:05Z", "answer"),
            fx::assistant_tool(ID, "2026-01-01T10:00:06Z", "Bash", serde_json::json!({"command": "ls"})),
            fx::tool_result(ID, CWD, "2026-01-01T10:00:07Z", "file.txt"),
            fx::ai_title(ID, "Generated title"),
            fx::user(ID, CWD, "2026-01-01T10:01:00Z", "second\n  question"),
            fx::assistant_text(ID, "2026-01-01T10:02:00Z", "done"),
            fx::last_prompt(ID, "second question"),
        ]);
        let s = parse_meta(&path(), &data, stamp()).expect("session");
        assert_eq!(s.agent, AgentKind::Claude);
        assert_eq!(s.id, ID);
        assert_eq!(s.cwd, CWD);
        assert_eq!(s.title.as_deref(), Some("Generated title"));
        assert_eq!(s.first_prompt.as_deref(), Some("first question"));
        assert_eq!(s.last_prompt.as_deref(), Some("second question"));
        assert_eq!(s.user_turns, 2);
        assert_eq!(s.git_branch.as_deref(), Some("main"));
        assert_eq!(s.started_at, parse_rfc3339_ms("2026-01-01T10:00:00Z"));
        assert_eq!(s.updated_at, parse_rfc3339_ms("2026-01-01T10:02:00Z").unwrap());
        assert_eq!(s.size, 123);
    }

    #[test]
    fn spaced_json_parses_the_same() {
        let lines = [
            fx::user(ID, CWD, "2026-01-01T10:00:00Z", "first question"),
            fx::tool_result(ID, CWD, "2026-01-01T10:00:07Z", "file.txt"),
            fx::ai_title(ID, "Generated title"),
            fx::assistant_text(ID, "2026-01-01T10:02:00Z", "done"),
        ];
        let spaced = crate::testkit::jsonl_bytes_spaced(&lines);
        assert!(std::str::from_utf8(&spaced).unwrap().contains(r#""type": "user""#));
        assert_eq!(parse_meta(&path(), &spaced, stamp()), parse_meta(&path(), &bytes(&lines), stamp()));
        assert!(parse_meta(&path(), &spaced, stamp()).is_some());
        assert_eq!(parse_transcript(&spaced), parse_transcript(&bytes(&lines)));
    }

    #[test]
    fn custom_title_beats_ai_title_regardless_of_order() {
        let data = bytes(&[
            fx::user(ID, CWD, "2026-01-01T10:00:00Z", "q"),
            fx::custom_title(ID, "my-name"),
            fx::ai_title(ID, "Later AI title"),
        ]);
        let s = parse_meta(&path(), &data, stamp()).unwrap();
        assert_eq!(s.title.as_deref(), Some("my-name"));
    }

    #[test]
    fn injected_and_sidechain_messages_are_not_prompts() {
        let data = bytes(&[
            fx::meta_user(ID, CWD, "2026-01-01T10:00:00Z", "Caveat: injected"),
            fx::user(ID, CWD, "2026-01-01T10:00:01Z", "<command-name>/model</command-name>"),
            fx::sidechain_user(ID, CWD, "2026-01-01T10:00:02Z", "subagent prompt"),
            fx::user(ID, CWD, "2026-01-01T10:00:03Z", "real prompt"),
        ]);
        let s = parse_meta(&path(), &data, stamp()).unwrap();
        assert_eq!(s.user_turns, 1);
        assert_eq!(s.first_prompt.as_deref(), Some("real prompt"));
    }

    #[test]
    fn sessions_without_prompts_are_skipped() {
        let data = bytes(&[
            fx::snapshot(),
            fx::user(ID, CWD, "2026-01-01T10:00:00Z", "<local-command-stdout>x</local-command-stdout>"),
        ]);
        assert_eq!(parse_meta(&path(), &data, stamp()), None);
        assert_eq!(parse_meta(&path(), b"", stamp()), None);
        assert_eq!(parse_meta(&path(), b"garbage\n{not json", stamp()), None);
    }

    #[test]
    fn updated_at_falls_back_to_mtime() {
        let mut record = fx::user(ID, CWD, "x", "q");
        record.as_object_mut().unwrap().remove("timestamp");
        let s = parse_meta(&path(), &bytes(&[record]), stamp()).unwrap();
        assert_eq!(s.updated_at, 9_000);
    }

    #[test]
    fn head_branch_is_ignored() {
        let mut record = fx::user(ID, CWD, "2026-01-01T10:00:00Z", "q");
        record["gitBranch"] = "HEAD".into();
        let s = parse_meta(&path(), &bytes(&[record]), stamp()).unwrap();
        assert_eq!(s.git_branch, None);
    }

    #[test]
    fn detaching_head_clears_an_earlier_branch() {
        let named = fx::user(ID, CWD, "2026-01-01T10:00:00Z", "q");
        let mut detached = fx::user(ID, CWD, "2026-01-01T10:01:00Z", "q2");
        detached["gitBranch"] = "HEAD".into();
        let mut other = fx::user(ID, CWD, "2026-01-01T10:02:00Z", "q3");
        other["gitBranch"] = "feature".into();
        let mut absent = fx::user(ID, CWD, "2026-01-01T10:03:00Z", "q4");
        absent.as_object_mut().unwrap().remove("gitBranch");
        let at = |records: &[serde_json::Value]| parse_meta(&path(), &bytes(records), stamp()).unwrap().git_branch;
        assert_eq!(at(&[named.clone(), detached.clone()]), None);
        assert_eq!(at(&[named.clone(), detached, other.clone()]).as_deref(), Some("feature"));
        assert_eq!(at(&[other, absent]).as_deref(), Some("feature"), "a record without the field changes nothing");
    }

    #[test]
    fn empty_titles_fall_back() {
        let data = bytes(&[
            fx::user(ID, CWD, "2026-01-01T10:00:00Z", "the first prompt"),
            fx::ai_title(ID, "Generated"),
            fx::custom_title(ID, " \u{7}\n "),
        ]);
        let s = parse_meta(&path(), &data, stamp()).unwrap();
        assert_eq!(s.title.as_deref(), Some("Generated"));
        let data = bytes(&[fx::user(ID, CWD, "2026-01-01T10:00:00Z", "the first prompt"), fx::ai_title(ID, "")]);
        assert_eq!(parse_meta(&path(), &data, stamp()).unwrap().label(), "the first prompt");
    }

    #[test]
    fn text_sent_with_tool_results_is_a_prompt() {
        let mixed = fx::user_content(
            ID,
            CWD,
            "2026-01-01T10:00:05Z",
            serde_json::json!([
                {"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "tool output"}]},
                {"type": "text", "text": "please correct that"}
            ]),
        );
        let s = parse_meta(&path(), &bytes(std::slice::from_ref(&mixed)), stamp()).expect("the session exists");
        assert_eq!(s.first_prompt.as_deref(), Some("please correct that"));
        let after = bytes(&[fx::user(ID, CWD, "2026-01-01T10:00:00Z", "first"), mixed.clone()]);
        assert_eq!(parse_meta(&path(), &after, stamp()).unwrap().user_turns, 2);
        let turns = parse_transcript(&after);
        assert_eq!(turns.last().unwrap().text, "please correct that");
        assert!(turns.iter().all(|t| !t.text.contains("tool output")), "result text is never a prompt");
        // A plain tool result is still not a prompt.
        let plain = bytes(&[fx::tool_result(ID, CWD, "2026-01-01T10:00:07Z", "file.txt")]);
        assert_eq!(parse_meta(&path(), &plain, stamp()), None);
    }

    #[test]
    fn image_only_requests_count() {
        let image = fx::user_content(
            ID,
            CWD,
            "2026-01-01T10:00:00Z",
            serde_json::json!([{"type": "image", "source": {"type": "base64", "data": "AAAA"}}]),
        );
        let data = bytes(&[image, fx::assistant_text(ID, "2026-01-01T10:00:05Z", "nice screenshot")]);
        let s = parse_meta(&path(), &data, stamp()).expect("an image-only request is a session");
        assert_eq!(s.first_prompt.as_deref(), Some("[1 attachment]"));
        assert_eq!(parse_transcript(&data)[0].text, "[1 attachment]");
    }

    #[test]
    fn quoting_a_tag_is_still_a_prompt() {
        let data = bytes(&[fx::user(ID, CWD, "2026-01-01T10:00:00Z", "<environment_context> keeps appearing, why?")]);
        assert!(parse_meta(&path(), &data, stamp()).is_some());
    }

    #[test]
    fn assistant_text_and_tools_keep_their_order() {
        let reply = fx::assistant_content(
            ID,
            "2026-01-01T10:00:01Z",
            serde_json::json!([
                {"type": "text", "text": "before"},
                {"type": "tool_use", "id": "t", "name": "Read", "input": {"file_path": "a"}},
                {"type": "text", "text": "after"}
            ]),
        );
        let tool_first = fx::assistant_content(
            ID,
            "2026-01-01T10:00:02Z",
            serde_json::json!([
                {"type": "tool_use", "id": "t", "name": "Bash", "input": {"command": "ls"}},
                {"type": "tool_use", "id": "u", "name": "Bash", "input": {"command": "pwd"}},
                {"type": "text", "text": "done"}
            ]),
        );
        let turns = parse_transcript(&bytes(&[fx::user(ID, CWD, "2026-01-01T10:00:00Z", "go"), reply, tool_first]));
        let got: Vec<(Role, &str)> = turns.iter().map(|t| (t.role, t.text.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (Role::User, "go"),
                (Role::Assistant, "before"),
                (Role::Tool, "Read: a"),
                (Role::Assistant, "after"),
                (Role::Tool, "Bash: ls"),
                (Role::Tool, "Bash: pwd"),
                (Role::Assistant, "done"),
            ]
        );
    }

    #[test]
    fn code_keeps_its_indentation() {
        let reply = fx::assistant_text(ID, "2026-01-01T10:00:01Z", "    let x  =  1;\n");
        let turns = parse_transcript(&bytes(&[fx::user(ID, CWD, "2026-01-01T10:00:00Z", "go"), reply]));
        assert_eq!(turns[1].text, "    let x  =  1;");
    }

    #[test]
    fn any_json_spacing_parses() {
        let compact = fx::user(ID, CWD, "2026-01-01T10:00:00Z", "q");
        let odd = String::from_utf8(bytes(std::slice::from_ref(&compact)))
            .unwrap()
            .replace("\"type\":\"user\"", "\"type\" :\t\"user\"")
            .replace("\"role\":\"user\"", "\"role\"  :  \"user\"");
        assert!(odd.contains("\"type\" :\t\"user\""));
        assert_eq!(parse_meta(&path(), odd.as_bytes(), stamp()), parse_meta(&path(), &bytes(&[compact]), stamp()));
        let escaped = br#"{"type":"\u0075ser","isSidechain":false,"message":{"role":"user","content":"q"},"cwd":"/x","timestamp":"2026-01-01T10:00:00Z"}"#;
        assert!(parse_meta(&path(), escaped, stamp()).is_some());
        let crlf =
            String::from_utf8(bytes(&[fx::user(ID, CWD, "2026-01-01T10:00:00Z", "q")])).unwrap().replace('\n', "\r\n");
        assert!(parse_meta(&path(), crlf.as_bytes(), stamp()).is_some());
    }

    #[test]
    fn transcript_keeps_conversation_order() {
        let data = bytes(&[
            fx::user(ID, CWD, "2026-01-01T10:00:00Z", "hello\n\tthere"),
            fx::assistant_text(ID, "2026-01-01T10:00:01Z", "part one"),
            fx::assistant_text(ID, "2026-01-01T10:00:02Z", "part two"),
            fx::assistant_tool(ID, "2026-01-01T10:00:03Z", "Read", serde_json::json!({"file_path": "/a.rs"})),
            fx::tool_result(ID, CWD, "2026-01-01T10:00:04Z", "contents"),
            fx::sidechain_user(ID, CWD, "2026-01-01T10:00:05Z", "hidden"),
            fx::user(ID, CWD, "2026-01-01T10:00:06Z", "thanks"),
        ]);
        let turns = parse_transcript(&data);
        let got: Vec<(Role, &str)> = turns.iter().map(|t| (t.role, t.text.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (Role::User, "hello\n\tthere"),
                (Role::Assistant, "part one\n\npart two"),
                (Role::Tool, "Read: /a.rs"),
                (Role::User, "thanks"),
            ]
        );
        assert_eq!(turns[0].timestamp, parse_rfc3339_ms("2026-01-01T10:00:00Z"));
    }

    #[test]
    fn provider_discovers_and_lists_related_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".claude");
        let file = fx::write_session(&root, ID, CWD, &[fx::user(ID, CWD, "2026-01-01T10:00:00Z", "q")]);
        // A subagent transcript must not be listed as a session.
        let sub = file.with_extension("").join("subagents");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("agent-1.jsonl"), b"{}").unwrap();
        fs::create_dir_all(root.join("session-env").join(ID)).unwrap();
        fs::create_dir_all(root.join("file-history").join(ID)).unwrap();
        fs::create_dir_all(root.join("todos")).unwrap();
        fs::write(root.join("todos").join(format!("{ID}-agent-{ID}.json")), b"[]").unwrap();
        fs::write(root.join("todos").join("other-agent.json"), b"[]").unwrap();

        let provider = ClaudeProvider::new(root.clone());
        assert_eq!(provider.discover(&mut Vec::new()), vec![file.clone()]);

        let session = provider.parse(&file, FileStamp::of(&file).unwrap()).unwrap().unwrap();
        let related = provider.related_paths(&session);
        assert_eq!(
            related,
            vec![
                file.clone(),
                file.with_extension(""),
                root.join("session-env").join(ID),
                root.join("file-history").join(ID),
                root.join("todos").join(format!("{ID}-agent-{ID}.json")),
            ]
        );

        let cmd = provider.resume_command(&session);
        assert_eq!(cmd.program, "claude");
        assert_eq!(cmd.args, vec!["--resume", ID]);
        assert_eq!(cmd.cwd, PathBuf::from(CWD));
    }
}
