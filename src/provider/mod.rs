//! Agent adapters. Each agent implements [`Provider`] in its own file.

pub mod claude;
pub mod codex;
pub mod content;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::env::Env;
use crate::model::{AgentKind, Role, Session, Turn};

/// The modification time and size of a session file. Together they decide
/// whether a cached parse of the file is still valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub mtime_ns: i64,
    pub size: u64,
}

impl FileStamp {
    pub fn of(path: &Path) -> Option<FileStamp> {
        FileStamp::try_of(path).ok()
    }

    /// Like [`FileStamp::of`], keeping the reason it failed.
    pub fn try_of(path: &Path) -> io::Result<FileStamp> {
        let meta = fs::metadata(path)?;
        if !meta.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
        }
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        Ok(FileStamp { mtime_ns, size: meta.len() })
    }

    pub fn mtime_ms(&self) -> i64 {
        self.mtime_ns / 1_000_000
    }
}

/// A command that reopens a session in its agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResumeCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// The resolved program, filled in by [`crate::actions::resume_command`]
    /// (and by [`crate::actions::printable_command`] when the plain name
    /// could mean another program after changing directory).
    #[serde(skip)]
    pub executable: Option<PathBuf>,
}

impl ResumeCommand {
    /// A copy-pasteable shell line, e.g. `cd ~/x && claude --resume abc`.
    /// A resolved [`ResumeCommand::executable`] is used instead of the
    /// program's name, so the line runs the same program.
    pub fn shell_line(&self) -> String {
        let program = match &self.executable {
            Some(path) => path.to_string_lossy().into_owned(),
            None => self.program.clone(),
        };
        let mut parts = vec![shell_quote(&program)];
        parts.extend(self.args.iter().map(|a| shell_quote(a)));
        let command = parts.join(" ");
        if self.cwd.as_os_str().is_empty() {
            command
        } else {
            format!("cd {} && {command}", shell_quote(&self.cwd.to_string_lossy()))
        }
    }
}

/// Quotes `value` for POSIX shells only when needed. Values containing
/// control characters use `$'...'` quoting (bash, zsh, ksh) with the
/// characters escaped, so printing the line can never drive the terminal.
pub fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty() && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"_./:=@%+-,".contains(&b));
    if safe {
        return value.to_string();
    }
    if !value.chars().any(crate::text::is_unsafe_char) {
        return format!("'{}'", value.replace('\'', r"'\''"));
    }
    let mut out = String::from("$'");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x80 && crate::text::is_unsafe_char(c) => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if crate::text::is_unsafe_char(c) => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Everything entropeek needs to support one agent.
pub trait Provider: Send + Sync {
    fn kind(&self) -> AgentKind;

    /// The agent's data directory. Deletes never touch anything outside it.
    fn root(&self) -> &Path;

    /// Candidate session files. Must be cheap: directory walks only.
    /// Directories that exist but cannot be read are reported in `unread`.
    fn discover(&self, unread: &mut Unread) -> Vec<PathBuf>;

    /// Reads one session file into list metadata. `Ok(None)` means the file
    /// is not a usable session (for example, it has no user prompts), which
    /// is safe to remember; `Err` means it could not be read right now.
    fn parse(&self, path: &Path, stamp: FileStamp) -> Result<Option<Session>>;

    /// Fills in data kept outside the session files (such as a title index).
    /// Runs on every scan, after cached and fresh sessions are merged.
    /// What could not be read goes to `errors`.
    fn decorate(&self, _sessions: &mut [Session], _errors: &mut Vec<String>) {}

    /// The full conversation.
    fn transcript(&self, session: &Session) -> Result<Vec<Turn>>;

    fn resume_command(&self, session: &Session) -> ResumeCommand;

    /// Every file or directory that belongs to the session, starting with the
    /// session file. A delete moves all of them to the trash. Fails if the
    /// list cannot be known to be complete (something could not be read),
    /// or if deleting the session would break another one.
    fn related_paths(&self, session: &Session) -> Result<Vec<PathBuf>>;
}

/// What a discovery walk could not read.
#[derive(Debug, Default)]
pub struct Unread {
    /// What went wrong, for the user.
    pub errors: Vec<String>,
    /// Directories (or entries) whose sessions may be missing from the
    /// result. Sessions known from before inside them are kept, not dropped
    /// as deleted.
    pub dirs: Vec<PathBuf>,
}

impl Unread {
    pub fn add(&mut self, dir: &Path, error: &io::Error) {
        self.errors.push(format!("cannot read {}: {error}", dir.display()));
        self.dirs.push(dir.to_path_buf());
    }

    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }
}

/// All supported providers, in [`AgentKind::ALL`] order.
pub fn all(env: &Env) -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(claude::ClaudeProvider::new(env.claude_dir.clone())),
        Box::new(codex::CodexProvider::new(env.codex_dir.clone())),
    ]
}

/// The provider for `agent` within `providers`.
pub fn find(providers: &[Box<dyn Provider>], agent: AgentKind) -> Option<&dyn Provider> {
    providers.iter().find(|p| p.kind() == agent).map(|p| p.as_ref())
}

/// Appends a turn, merging consecutive assistant text into one turn. Agents
/// often store one assistant reply as several records.
pub(crate) fn push_turn(turns: &mut Vec<Turn>, turn: Turn) {
    if turn.text.trim().is_empty() {
        return;
    }
    if turn.role == Role::Assistant
        && let Some(last) = turns.last_mut()
        && last.role == Role::Assistant
    {
        last.text.push_str("\n\n");
        last.text.push_str(&turn.text);
        return;
    }
    turns.push(turn);
}

/// Opens a directory for listing. A missing directory is simply empty;
/// any other failure is recorded in `unread`.
fn read_dir(dir: &Path, unread: &mut Unread) -> Option<fs::ReadDir> {
    match fs::read_dir(dir) {
        Ok(entries) => Some(entries),
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => None,
        Err(e) => {
            unread.add(dir, &e);
            None
        }
    }
}

/// The entries of `dir`; an error part way through is recorded.
fn entries(dir: &Path, unread: &mut Unread) -> Vec<fs::DirEntry> {
    let Some(list) = read_dir(dir, unread) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in list {
        match entry {
            Ok(entry) => out.push(entry),
            Err(e) => {
                unread.add(dir, &e);
                break;
            }
        }
    }
    out
}

/// Session files directly inside each subdirectory of `root`, matching `keep`.
pub(crate) fn files_in_subdirs(root: &Path, keep: impl Fn(&Path) -> bool, unread: &mut Unread) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in entries(root, unread) {
        let path = dir.path();
        // Plain files at this level (and dangling links) are not projects.
        match fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                unread.add(&path, &e);
                continue;
            }
        }
        for entry in entries(&path, unread) {
            let file = entry.path();
            if !keep(&file) {
                continue;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_file() => out.push(file),
                Ok(_) => {}
                Err(e) => unread.add(&file, &e),
            }
        }
    }
    out
}

/// Files below `root` (up to `depth` levels) matching `keep`.
pub(crate) fn files_recursive(
    root: &Path,
    depth: usize,
    keep: &impl Fn(&Path) -> bool,
    out: &mut Vec<PathBuf>,
    unread: &mut Unread,
) {
    for entry in entries(root, unread) {
        let path = entry.path();
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(e) => {
                unread.add(&path, &e);
                continue;
            }
        };
        if kind.is_dir() {
            if depth > 0 {
                files_recursive(&path, depth - 1, keep, out, unread);
            }
        } else if kind.is_file() && keep(&path) {
            out.push(path);
        }
    }
}

pub(crate) fn has_extension(path: &Path, ext: &str) -> bool {
    path.extension().is_some_and(|e| e == ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_only_when_needed() {
        assert_eq!(shell_quote("abc-123"), "abc-123");
        assert_eq!(shell_quote("/home/u/my dir"), "'/home/u/my dir'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("/w/\u{1b}]52;c;x\u{7}"), r"$'/w/\x1b]52;c;x\x07'");
        assert_eq!(shell_quote("a\nb'c\u{202e}"), r"$'a\nb\'c\u202e'");
    }

    #[test]
    fn shell_line_includes_cd() {
        let cmd = ResumeCommand {
            program: "claude".into(),
            args: vec!["--resume".into(), "abc".into()],
            cwd: PathBuf::from("/w/my app"),
            executable: None,
        };
        assert_eq!(cmd.shell_line(), "cd '/w/my app' && claude --resume abc");
    }

    #[test]
    fn push_turn_merges_assistant_text() {
        let t = |role, text: &str| Turn { role, text: text.into(), timestamp: None };
        let mut turns = Vec::new();
        push_turn(&mut turns, t(Role::User, "q"));
        push_turn(&mut turns, t(Role::Assistant, "a1"));
        push_turn(&mut turns, t(Role::Assistant, "a2"));
        push_turn(&mut turns, t(Role::Tool, "Bash"));
        push_turn(&mut turns, t(Role::Assistant, "a3"));
        push_turn(&mut turns, t(Role::User, "   "));
        let texts: Vec<_> = turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["q", "a1\n\na2", "Bash", "a3"]);
    }

    #[test]
    fn file_stamp_of_missing_file_is_none() {
        assert_eq!(FileStamp::of(Path::new("/definitely/not/here")), None);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(FileStamp::of(dir.path()), None, "directories are not session files");
        let file = dir.path().join("f");
        fs::write(&file, b"abc").unwrap();
        assert_eq!(FileStamp::of(&file).unwrap().size, 3);
    }
}
