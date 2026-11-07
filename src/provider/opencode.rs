//! OpenCode sessions: rows in OpenCode's SQLite database
//! (`~/.local/share/opencode/opencode.db`), not a file each.
//!
//! The tables we read:
//! - `session_v2`: one row per session, with its directory and title.
//!   Sessions with a `parent_id` are sub-agents a session spawned; they are
//!   part of it, not listed on their own. Archived sessions are not listed
//!   either (OpenCode hides them too).
//! - `session_message`: the conversation, ordered by `seq`. `data` is JSON
//!   whose shape depends on `type`: `user` (`text`, attached `files`),
//!   `assistant` (`content`: `text`, `reasoning` and `tool` parts), `shell`
//!   (a command the user ran), and bookkeeping we skip.
//! - `session_part`: assistant parts OpenCode moved out of a message's
//!   `content`; they come before what is still inline.
//!
//! The database is only ever opened read-only. Deleting would mean writing
//! to the database of a program that may be running, which the trash cannot
//! undo, so deletes are refused and pointed at `opencode session delete`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, Statement, params};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::value::RawValue;

use super::content::{Block, Content, tool_summary, with_detail};
use super::{FileStamp, Provider, ResumeCommand, Stored, Unread, push_block, push_turn};
use crate::cache::CacheEntry;
use crate::model::{AgentKind, Role, Session, Turn};
use crate::text;

pub struct OpenCodeProvider {
    db: PathBuf,
}

impl OpenCodeProvider {
    pub fn new(db: PathBuf) -> OpenCodeProvider {
        OpenCodeProvider { db }
    }

    /// A read-only connection, or `None` if there is no database yet.
    fn open(&self) -> Result<Option<Connection>> {
        match fs::metadata(&self.db) {
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {
                return Ok(None);
            }
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", self.db.display())),
        }
        // Even a read-only connection creates the `-wal` and `-shm` files of
        // a WAL database and leaves them behind. Without a `-wal` file
        // (OpenCode is not running) everything is in the main file, which
        // `immutable` reads without creating anything.
        let mut wal = self.db.clone().into_os_string();
        wal.push("-wal");
        let idle = matches!(fs::symlink_metadata(&wal), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
        let open = || -> rusqlite::Result<Connection> {
            let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let conn = if idle {
                Connection::open_with_flags(file_uri(&self.db) + "?immutable=1", flags | OpenFlags::SQLITE_OPEN_URI)?
            } else {
                Connection::open_with_flags(&self.db, flags)?
            };
            // OpenCode writes while we read; wait out its short locks.
            conn.busy_timeout(Duration::from_secs(2))?;
            Ok(conn)
        };
        open().map(Some).with_context(|| format!("cannot open {}", self.db.display()))
    }

    /// The path that stands for a session in the list and the cache. It does
    /// not exist on disk; it only has to be unique and inside [`Provider::root`].
    fn session_path(&self, id: &str) -> PathBuf {
        self.db.join(id)
    }
}

/// `path` as an SQLite `file:` URI, every byte that could be read as URI
/// syntax percent-encoded.
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file:");
    for &byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// What changes whenever a session's listing could: the time of its last
/// change, and the message count and last sequence number, which adding or
/// removing a message changes.
fn stamp(updated_ms: i64, messages: i64, last_seq: i64) -> FileStamp {
    FileStamp { mtime_ns: updated_ms.saturating_mul(1_000_000), size: ((last_seq as u64) << 32) ^ messages as u64 }
}

/// A title as shown. OpenCode 1.x named untitled sessions
/// `New session - <date>`; those fall back to the first prompt.
fn title(value: Option<&str>) -> Option<String> {
    let value = value?;
    let placeholder = ["New session - ", "Child session - "]
        .iter()
        .any(|prefix| value.strip_prefix(prefix).is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit())));
    if placeholder { None } else { super::title(value) }
}

#[derive(Deserialize)]
struct Time {
    #[serde(default)]
    created: Option<i64>,
}

#[derive(Deserialize)]
struct UserData<'a> {
    #[serde(borrow, default)]
    text: Option<Cow<'a, str>>,
    #[serde(default)]
    files: Option<Vec<IgnoredAny>>,
    #[serde(default)]
    time: Option<Time>,
}

/// What the user typed in a `user` message, with a note for attachments,
/// or `None` if it is not a prompt.
fn user_prompt(data: &str) -> Option<(String, Option<i64>)> {
    let data: UserData = serde_json::from_str(data).ok()?;
    let mut blocks: Vec<Block> = data.text.map(|t| Block::Text(t.into_owned())).into_iter().collect();
    blocks.extend(std::iter::repeat_n(Block::Attachment, data.files.map_or(0, |f| f.len())));
    let prompt = Content { blocks }.user_text()?;
    text::is_user_prompt(prompt.trim()).then_some((prompt, data.time.and_then(|t| t.created)))
}

#[derive(Deserialize)]
struct AssistantData<'a> {
    #[serde(borrow, default)]
    content: Vec<&'a RawValue>,
    #[serde(default)]
    time: Option<Time>,
}

#[derive(Deserialize)]
struct Part<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    text: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    name: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    state: Option<ToolState<'a>>,
}

#[derive(Deserialize)]
struct ToolState<'a> {
    #[serde(borrow, default)]
    input: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct ShellData<'a> {
    #[serde(borrow, default)]
    command: Option<Cow<'a, str>>,
    #[serde(default)]
    time: Option<Time>,
}

/// Appends an assistant part (its JSON): text is shown, tool calls are
/// summarized, reasoning is skipped (like Claude's thinking).
fn push_part(turns: &mut Vec<Turn>, json: &str, timestamp: Option<i64>) {
    let Ok(part) = serde_json::from_str::<Part>(json) else { return };
    let block = match part.kind.as_deref() {
        Some("text") => Block::Text(part.text.unwrap_or_default().into_owned()),
        Some("tool") => {
            Block::Tool(tool_summary(part.name.as_deref().unwrap_or("tool"), part.state.and_then(|s| s.input)))
        }
        _ => return,
    };
    push_block(turns, block, timestamp);
}

/// Builds the conversation from a session's messages (`type`, `data`,
/// in order) and the parts moved out of them (by message id, in order).
fn transcript_from(messages: &[(String, String, String)], parts: &HashMap<String, Vec<String>>) -> Vec<Turn> {
    let mut turns = Vec::new();
    for (id, kind, data) in messages {
        match kind.as_str() {
            "user" => {
                if let Some((prompt, timestamp)) = user_prompt(data) {
                    push_turn(&mut turns, Turn { role: Role::User, text: prompt, timestamp });
                }
            }
            "assistant" => {
                let Ok(message) = serde_json::from_str::<AssistantData>(data) else { continue };
                let timestamp = message.time.and_then(|t| t.created);
                for moved in parts.get(id).into_iter().flatten() {
                    push_part(&mut turns, moved, timestamp);
                }
                for raw in message.content {
                    push_part(&mut turns, raw.get(), timestamp);
                }
            }
            "shell" => {
                let Ok(shell) = serde_json::from_str::<ShellData>(data) else { continue };
                let summary = with_detail("Shell", shell.command.as_deref());
                let timestamp = shell.time.and_then(|t| t.created);
                turns.push(Turn { role: Role::Tool, text: text::one_line(&summary), timestamp });
            }
            _ => {}
        }
    }
    turns
}

/// One `session_v2` row, as listed.
struct Row {
    id: String,
    directory: String,
    title: Option<String>,
    created: i64,
    updated: i64,
    stamp: FileStamp,
}

const LIST_SQL: &str = "
    SELECT s.id, s.directory, s.title, s.time_created, s.time_updated,
           (SELECT count(*) FROM session_message m WHERE m.session_id = s.id),
           (SELECT coalesce(max(m.seq), 0) FROM session_message m WHERE m.session_id = s.id),
           (SELECT max(m.time_created) FROM session_message m WHERE m.session_id = s.id)
    FROM session_v2 s
    WHERE s.parent_id IS NULL AND s.time_archived IS NULL";

impl OpenCodeProvider {
    fn rows(conn: &Connection) -> rusqlite::Result<Vec<Row>> {
        let mut stmt = conn.prepare(LIST_SQL)?;
        let rows = stmt.query_map([], |r| {
            // OpenCode does not always move `time_updated` when it adds a
            // message; like the other agents, the last message counts.
            let updated = r.get::<_, i64>(4)?.max(r.get::<_, Option<i64>>(7)?.unwrap_or(i64::MIN));
            Ok(Row {
                id: r.get(0)?,
                directory: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                title: r.get(2)?,
                created: r.get(3)?,
                updated,
                stamp: stamp(updated, r.get(5)?, r.get(6)?),
            })
        })?;
        rows.collect()
    }

    /// Reads one session's prompts (with `users`) and size (with `size`).
    /// `None` if it has no prompt.
    fn parse_row(&self, users: &mut Statement, size: &mut Statement, row: &Row) -> rusqlite::Result<Option<Session>> {
        let mut first_prompt = None;
        let mut last_prompt = None;
        let mut user_turns = 0u32;
        let mut rows = users.query(params![row.id])?;
        while let Some(r) = rows.next()? {
            let data: String = r.get(0)?;
            if let Some((prompt, _)) = user_prompt(&data) {
                user_turns += 1;
                if first_prompt.is_none() {
                    first_prompt = Some(text::summary(&prompt, 300));
                }
                last_prompt = Some(prompt);
            }
        }
        if user_turns == 0 {
            return Ok(None);
        }
        let size: i64 = size.query_row(params![row.id], |r| r.get(0))?;
        Ok(Some(Session {
            agent: AgentKind::OpenCode,
            id: row.id.clone(),
            path: self.session_path(&row.id),
            cwd: row.directory.clone(),
            title: title(row.title.as_deref()),
            first_prompt,
            last_prompt: last_prompt.map(|p| text::summary(&p, 300)),
            started_at: Some(row.created),
            updated_at: row.updated,
            mtime_ns: row.stamp.mtime_ns,
            git_branch: None,
            user_turns,
            size: u64::try_from(size).unwrap_or(0),
        }))
    }
}

impl Provider for OpenCodeProvider {
    fn kind(&self) -> AgentKind {
        AgentKind::OpenCode
    }

    /// The database file: every session lives in it.
    fn root(&self) -> &Path {
        &self.db
    }

    fn discover(&self, _unread: &mut Unread) -> Vec<PathBuf> {
        Vec::new()
    }

    fn parse(&self, _path: &Path, _stamp: FileStamp) -> Result<Option<Session>> {
        Ok(None)
    }

    /// Lists every session. Only sessions whose stamp changed are read
    /// again; a reused one still gets the current title and directory,
    /// which can change without a new message.
    fn read_all(&self, cached: &dyn Fn(&Path, FileStamp) -> Option<CacheEntry>) -> Result<Stored> {
        let Some(conn) = self.open()? else { return Ok(Vec::new()) };
        let context = || format!("reading OpenCode sessions from {}", self.db.display());
        // One snapshot, so each stamp describes exactly what is read.
        let _snapshot = conn.unchecked_transaction().with_context(context)?;
        let rows = Self::rows(&conn).with_context(context)?;
        let mut users = conn
            .prepare("SELECT data FROM session_message WHERE session_id = ?1 AND type = 'user' ORDER BY seq")
            .with_context(context)?;
        let mut size = conn
            .prepare(
                "SELECT (SELECT coalesce(sum(octet_length(data)), 0) FROM session_message WHERE session_id = ?1)
                      + (SELECT coalesce(sum(octet_length(data)), 0) FROM session_part WHERE session_id = ?1)",
            )
            .with_context(context)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            // Every session must be addressable as a path of its own.
            if row.id.is_empty() || row.id.contains(['/', '\\', '\0']) || row.id == "." || row.id == ".." {
                continue;
            }
            let path = self.session_path(&row.id);
            let entry = match cached(&path, row.stamp) {
                Some(mut entry) => {
                    if let Some(session) = &mut entry.session {
                        session.title = title(row.title.as_deref());
                        session.cwd = row.directory;
                    }
                    entry
                }
                None => CacheEntry {
                    stamp: row.stamp,
                    session: self.parse_row(&mut users, &mut size, &row).with_context(context)?,
                },
            };
            out.push((path, entry));
        }
        Ok(out)
    }

    fn transcript(&self, session: &Session) -> Result<Vec<Turn>> {
        let context = || format!("reading OpenCode session {} from {}", session.id, self.db.display());
        let Some(conn) = self.open()? else { bail!("{} no longer exists", self.db.display()) };
        let read = || -> rusqlite::Result<Vec<Turn>> {
            // One snapshot: OpenCode moves parts out of messages as it goes.
            let _snapshot = conn.unchecked_transaction()?;
            let mut stmt =
                conn.prepare("SELECT id, type, data FROM session_message WHERE session_id = ?1 ORDER BY seq")?;
            let messages = stmt
                .query_map(params![session.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<(String, String, String)>>>()?;
            let mut stmt = conn.prepare(
                "SELECT message_id, data FROM session_part WHERE session_id = ?1 ORDER BY message_id, ordinal",
            )?;
            let mut parts: HashMap<String, Vec<String>> = HashMap::new();
            let mut rows = stmt.query(params![session.id])?;
            while let Some(r) = rows.next()? {
                parts.entry(r.get(0)?).or_default().push(r.get(1)?);
            }
            Ok(transcript_from(&messages, &parts))
        };
        read().with_context(context)
    }

    fn resume_command(&self, session: &Session) -> ResumeCommand {
        ResumeCommand {
            program: AgentKind::OpenCode.program().to_string(),
            args: vec!["--session".to_string(), session.id.clone()],
            cwd: PathBuf::from(&session.cwd),
            executable: None,
        }
    }

    /// Always refuses: the session is rows in OpenCode's live database.
    fn related_paths(&self, session: &Session) -> Result<Vec<PathBuf>> {
        bail!("{}", AgentKind::OpenCode.cannot_delete(&session.id).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::opencode as fx;
    use serde_json::json;

    const CWD: &str = "/home/u/proj";

    fn setup() -> (tempfile::TempDir, PathBuf, OpenCodeProvider) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode/opencode.db");
        fx::create(&db);
        let provider = OpenCodeProvider::new(db.clone());
        (dir, db, provider)
    }

    fn list(provider: &OpenCodeProvider) -> Vec<Session> {
        let entries = provider.read_all(&|_, _| None).unwrap();
        entries.into_iter().filter_map(|(_, e)| e.session).collect()
    }

    #[test]
    fn lists_sessions_with_prompts_titles_and_times() {
        let (_dir, db, provider) = setup();
        let conn = fx::connect(&db);
        fx::session(&conn, "ses_a", CWD, Some("Fix the parser"), 1_000, 9_000);
        fx::message(&conn, "ses_a", 1, "user", fx::user(1_000, "first question"));
        fx::message(&conn, "ses_a", 2, "assistant", fx::assistant(1_100, json!([fx::text("an answer")])));
        fx::message(&conn, "ses_a", 3, "idle", json!({"time": {"created": 1_200}, "outcome": "succeeded"}));
        fx::message(&conn, "ses_a", 4, "user", fx::user(2_000, "second\n  question"));
        drop(conn);

        let sessions = list(&provider);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.agent, AgentKind::OpenCode);
        assert_eq!(s.id, "ses_a");
        assert_eq!(s.path, db.join("ses_a"));
        assert!(s.path.starts_with(provider.root()));
        assert_eq!(s.cwd, CWD);
        assert_eq!(s.title.as_deref(), Some("Fix the parser"));
        assert_eq!(s.first_prompt.as_deref(), Some("first question"));
        assert_eq!(s.last_prompt.as_deref(), Some("second question"));
        assert_eq!(s.user_turns, 2);
        assert_eq!((s.started_at, s.updated_at), (Some(1_000), 9_000));
        // A message newer than the session's own update time counts.
        let conn = fx::connect(&db);
        fx::message(&conn, "ses_a", 5, "idle", json!({"time": {"created": 12_000}, "outcome": "failed"}));
        assert_eq!(list(&provider)[0].updated_at, 12_000);
        assert!(s.size > 0);
    }

    #[test]
    fn sub_agents_archived_and_empty_sessions_are_not_listed() {
        let (_dir, db, provider) = setup();
        let conn = fx::connect(&db);
        fx::session(&conn, "ses_root", CWD, None, 1, 5);
        fx::message(&conn, "ses_root", 1, "user", fx::user(1, "q"));
        fx::session(&conn, "ses_child", CWD, None, 2, 6);
        conn.execute("UPDATE session_v2 SET parent_id = 'ses_root' WHERE id = 'ses_child'", []).unwrap();
        fx::message(&conn, "ses_child", 1, "user", fx::user(2, "subagent prompt"));
        fx::session(&conn, "ses_archived", CWD, None, 3, 7);
        conn.execute("UPDATE session_v2 SET time_archived = 8 WHERE id = 'ses_archived'", []).unwrap();
        fx::message(&conn, "ses_archived", 1, "user", fx::user(3, "old"));
        fx::session(&conn, "ses_empty", CWD, None, 4, 8);
        drop(conn);
        let ids: Vec<String> = list(&provider).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["ses_root"]);
    }

    #[test]
    fn untitled_sessions_fall_back_to_the_first_prompt() {
        assert_eq!(title(None), None);
        assert_eq!(title(Some("New session - 2026-01-02T03:04:05.000Z")), None);
        assert_eq!(title(Some("Child session - 2026-01-02T03:04:05.000Z")), None);
        assert_eq!(title(Some("New session - ideas")).as_deref(), Some("New session - ideas"));
        assert_eq!(title(Some(" \u{7} ")), None);
        assert_eq!(title(Some("Dead code\nin repo")).as_deref(), Some("Dead code in repo"));
    }

    #[test]
    fn attachments_count_and_injected_text_does_not() {
        let image = json!({"time": {"created": 1}, "text": "", "files": [{"mime": "image/png", "url": "x"}]});
        assert_eq!(user_prompt(&image.to_string()).unwrap().0, "[1 attachment]");
        let both = json!({"time": {"created": 1}, "text": "look", "files": [{}, {}]});
        assert_eq!(user_prompt(&both.to_string()).unwrap().0, "look\n[2 attachments]");
        let injected = fx::user(1, "<system-reminder>be brief</system-reminder>");
        assert_eq!(user_prompt(&injected.to_string()), None);
        assert_eq!(user_prompt(&fx::user(1, "  ").to_string()), None);
        assert_eq!(user_prompt("not json"), None);
    }

    #[test]
    fn transcript_keeps_order_and_reads_moved_parts() {
        let (_dir, db, provider) = setup();
        let conn = fx::connect(&db);
        fx::session(&conn, "ses_t", CWD, None, 1, 9);
        fx::message(&conn, "ses_t", 1, "user", fx::user(1_000, "hello\n\tthere"));
        let reply = fx::assistant(
            1_100,
            json!([
                {"type": "reasoning", "text": "thinking hard"},
                fx::text("part one"),
                fx::tool("shell", json!({"command": "ls -la"})),
                fx::text("part two"),
            ]),
        );
        fx::message(&conn, "ses_t", 2, "assistant", reply);
        // A message whose first parts were moved to `session_part`.
        let moved =
            fx::message(&conn, "ses_t", 3, "assistant", fx::assistant(1_200, json!([fx::text("still inline")])));
        fx::part(&conn, "ses_t", &moved, 0, fx::tool("read", json!({"path": "/a.rs"})));
        fx::part(&conn, "ses_t", &moved, 1, fx::text("moved text"));
        fx::message(&conn, "ses_t", 4, "compaction", json!({"time": {"created": 1}, "summary": "## Objective"}));
        fx::message(&conn, "ses_t", 5, "synthetic", json!({"time": {"created": 1}, "text": "injected"}));
        fx::message(
            &conn,
            "ses_t",
            6,
            "shell",
            json!({"time": {"created": 1_300}, "command": "cargo test", "status": "exited"}),
        );
        fx::message(&conn, "ses_t", 7, "user", fx::user(1_400, "thanks"));
        drop(conn);

        let session = list(&provider).remove(0);
        let turns = provider.transcript(&session).unwrap();
        let got: Vec<(Role, &str)> = turns.iter().map(|t| (t.role, t.text.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (Role::User, "hello\n\tthere"),
                (Role::Assistant, "part one"),
                (Role::Tool, "shell: ls -la"),
                (Role::Assistant, "part two"),
                (Role::Tool, "read: /a.rs"),
                (Role::Assistant, "moved text\n\nstill inline"),
                (Role::Tool, "Shell: cargo test"),
                (Role::User, "thanks"),
            ]
        );
        assert_eq!(turns[0].timestamp, Some(1_000));
    }

    #[test]
    fn unchanged_sessions_are_reused_and_changes_seen() {
        let (_dir, db, provider) = setup();
        let conn = fx::connect(&db);
        fx::session(&conn, "ses_c", CWD, Some("Old title"), 1, 10);
        fx::message(&conn, "ses_c", 1, "user", fx::user(1, "q1"));
        let first = provider.read_all(&|_, _| None).unwrap();
        let (path, entry) = first[0].clone();

        // Reused while the stamp matches; the title still comes fresh.
        conn.execute("UPDATE session_v2 SET title = 'Renamed' WHERE id = 'ses_c'", []).unwrap();
        let mut stale = entry.clone();
        stale.session.as_mut().unwrap().first_prompt = Some("from the cache".into());
        let reuse = |p: &Path, s: FileStamp| (p == path && s == entry.stamp).then(|| stale.clone());
        let again = provider.read_all(&reuse).unwrap();
        let session = again[0].1.session.as_ref().unwrap();
        assert_eq!(session.first_prompt.as_deref(), Some("from the cache"));
        assert_eq!(session.title.as_deref(), Some("Renamed"));

        // A new message changes the stamp, even without a newer time.
        fx::message(&conn, "ses_c", 2, "user", fx::user(2, "q2"));
        let changed = provider.read_all(&reuse).unwrap();
        assert_ne!(changed[0].1.stamp, entry.stamp);
        assert_eq!(changed[0].1.session.as_ref().unwrap().user_turns, 2);
    }

    #[test]
    fn a_missing_database_is_empty_and_a_foreign_one_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = OpenCodeProvider::new(dir.path().join("nope/opencode.db"));
        assert!(missing.read_all(&|_, _| None).unwrap().is_empty());
        let other = dir.path().join("other.db");
        Connection::open(&other).unwrap().execute_batch("CREATE TABLE t (x)").unwrap();
        let err = OpenCodeProvider::new(other).read_all(&|_, _| None).unwrap_err();
        assert!(format!("{err:#}").contains("session_v2"), "{err:#}");
    }

    #[test]
    fn reading_never_writes_and_sees_what_is_still_in_the_wal() {
        let (_dir, db, provider) = setup();
        let conn = fx::connect(&db);
        fx::session(&conn, "ses_r", CWD, None, 1, 2);
        fx::message(&conn, "ses_r", 1, "user", fx::user(1, "q"));
        drop(conn); // like OpenCode quitting: no `-wal` or `-shm` left
        let files = || {
            let mut names: Vec<_> =
                fs::read_dir(db.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
            names.sort();
            names
        };
        let (names, bytes) = (files(), fs::read(&db).unwrap());
        assert_eq!(names, vec!["opencode.db"]);
        let session = list(&provider).remove(0);
        provider.transcript(&session).unwrap();
        assert_eq!(files(), names, "no files are created next to the database");
        assert_eq!(fs::read(&db).unwrap(), bytes);

        // While OpenCode runs, recent writes are only in its WAL.
        let live = fx::connect(&db);
        fx::session(&live, "ses_live", CWD, None, 3, 4);
        fx::message(&live, "ses_live", 1, "user", fx::user(3, "fresh"));
        assert!(db.with_file_name("opencode.db-wal").exists());
        let ids: Vec<String> = list(&provider).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["ses_r", "ses_live"]);
    }

    #[test]
    fn file_uris_escape_uri_syntax() {
        assert_eq!(file_uri(Path::new("/a b/x?y#z%.db")), "file:/a%20b/x%3Fy%23z%25.db");
        assert_eq!(
            file_uri(Path::new("/h/.local/share/opencode/opencode.db")),
            "file:/h/.local/share/opencode/opencode.db"
        );
    }

    #[test]
    fn resume_and_delete() {
        let provider = OpenCodeProvider::new(PathBuf::from("/x/opencode.db"));
        let mut session = crate::model::sample_session(AgentKind::OpenCode, "ses_abc");
        session.cwd = CWD.into();
        let cmd = provider.resume_command(&session);
        assert_eq!(cmd.shell_line(), "cd /home/u/proj && opencode --session ses_abc");
        let err = provider.related_paths(&session).unwrap_err().to_string();
        assert!(err.contains("opencode session delete ses_abc"), "{err}");
    }
}
