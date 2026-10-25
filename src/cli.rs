//! Command-line interface.

use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::actions;
use crate::cache::Cache;
use crate::env::Env;
use crate::index;
use crate::model::{AgentKind, Role, Session, Turn};
use crate::provider::{self, Provider};
use crate::search::{self, Query, Selector};
use crate::text;
use crate::timefmt;
use crate::trash::{Trash, TrashEntry};

#[derive(Parser, Debug)]
#[command(name = "entropeek", version, about = "Browse, preview, resume and delete your coding-agent sessions")]
pub struct Cli {
    /// Ignore the session cache and re-read every file.
    #[arg(long, global = true)]
    pub no_cache: bool,

    /// Color theme for the browser. `auto` matches the terminal's
    /// background. Also settable with ENTROPEEK_THEME.
    #[arg(long, global = true, value_enum)]
    pub theme: Option<crate::tui::theme::ThemeChoice>,

    /// Nerd Font icons in the browser. `auto` turns them on when a Nerd
    /// Font is installed. Also settable with ENTROPEEK_ICONS.
    #[arg(long, global = true, value_enum)]
    pub icons: Option<crate::tui::icons::IconChoice>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Open the interactive browser (the default in a terminal).
    Tui,
    /// Print sessions, newest first.
    List(ListArgs),
    /// Print a session's conversation.
    Show {
        /// Session id, unique id prefix, or `latest`.
        id: String,
        #[arg(long)]
        json: bool,
        /// Only print the last N turns.
        #[arg(long)]
        last: Option<usize>,
    },
    /// Reopen a session in the agent that created it.
    Resume {
        /// Session id, unique id prefix, or `latest`.
        id: String,
        /// Print the command instead of running it.
        #[arg(long)]
        print: bool,
    },
    /// Move a session to the entropeek trash (restore it with `trash restore`).
    Delete {
        /// Session id or unique id prefix.
        id: String,
        /// Don't ask for confirmation.
        #[arg(long, short)]
        yes: bool,
    },
    /// Manage deleted sessions.
    Trash {
        #[command(subcommand)]
        action: Option<TrashAction>,
    },
}

#[derive(Args, Debug, Default)]
pub struct ListArgs {
    #[arg(long)]
    pub json: bool,
    /// Only this agent (claude, codex).
    #[arg(long)]
    pub agent: Option<String>,
    /// Only sessions matching all these words.
    #[arg(long, short)]
    pub query: Option<String>,
    #[arg(long, short)]
    pub limit: Option<usize>,
}

#[derive(Subcommand, Debug)]
pub enum TrashAction {
    /// List deleted sessions.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Put a deleted session back.
    Restore {
        /// Trash key or session id.
        key: String,
    },
    /// Permanently delete trashed sessions.
    Purge {
        /// Only entries deleted more than this many days ago.
        #[arg(long)]
        older_than_days: Option<u32>,
        /// Purge everything.
        #[arg(long)]
        all: bool,
    },
}

/// What `main` should do after the CLI finished.
pub enum Outcome {
    Done,
    /// Leave and replace the process with this command.
    Exec(crate::provider::ResumeCommand),
}

pub struct Runtime<'a> {
    pub env: &'a Env,
    pub providers: std::sync::Arc<Vec<Box<dyn Provider>>>,
    pub no_cache: bool,
    pub theme: crate::tui::theme::ThemeChoice,
    pub icons: crate::tui::icons::IconChoice,
}

impl Runtime<'_> {
    pub fn cache(&self) -> Cache {
        if self.no_cache { Cache::in_memory() } else { Cache::load(&Cache::default_file(&self.env.cache_dir)) }
    }

    /// Scans all sessions and saves the refreshed cache.
    pub fn sessions(&self) -> Vec<Session> {
        let mut cache = self.cache();
        let scan = index::scan(&self.providers, &mut cache);
        if let Err(err) = cache.save() {
            eprintln!("entropeek: warning: could not save cache: {err:#}");
        }
        for error in scan.errors.iter().take(5) {
            eprintln!("entropeek: warning: {}", text::one_line(error));
        }
        if scan.errors.len() > 5 {
            eprintln!("entropeek: warning: … and {} more unreadable files", scan.errors.len() - 5);
        }
        scan.sessions
    }

    pub fn trash(&self) -> Trash {
        Trash::new(self.env.trash_dir())
    }
}

/// Runs a non-interactive command, writing results to `out`.
pub fn run(command: Command, ctx: &Runtime, out: &mut dyn Write) -> Result<Outcome> {
    match command {
        Command::Tui => bail!("the TUI is started by main"),
        Command::List(args) => list(ctx, &args, out)?,
        Command::Show { id, json, last } => {
            let selector = Selector::parse(&id)?;
            let sessions = ctx.sessions();
            let session = search::find(&sessions, &selector)?;
            let provider = provider::find(&ctx.providers, session.agent).context("unknown agent")?;
            let mut turns = provider.transcript(session)?;
            if let Some(last) = last {
                turns.drain(..turns.len().saturating_sub(last));
            }
            if json {
                serde_json::to_writer_pretty(
                    &mut *out,
                    &JsonTranscript { session: JsonSession::new(session, ctx), turns: &turns },
                )?;
                writeln!(out)?;
            } else {
                write_transcript(out, session, &turns, &ctx.env.home_str())?;
            }
        }
        Command::Resume { id, print } => {
            let selector = Selector::parse(&id)?;
            let sessions = ctx.sessions();
            let session = search::find(&sessions, &selector)?;
            if print {
                let provider = provider::find(&ctx.providers, session.agent).context("unknown agent")?;
                writeln!(out, "{}", provider.resume_command(session).shell_line())?;
            } else {
                return Ok(Outcome::Exec(actions::resume_command(&ctx.providers, session)?));
            }
        }
        Command::Delete { id, yes } => {
            let selector = Selector::parse(&id)?;
            if selector == Selector::Latest {
                bail!("refusing to delete `latest`; pass an explicit session id");
            }
            let sessions = ctx.sessions();
            let session = search::find(&sessions, &selector)?;
            if !yes {
                if !io::stdin().is_terminal() {
                    bail!("refusing to delete without confirmation; pass --yes");
                }
                eprint!(
                    "Move {} session {} ({}) to the trash? [y/N] ",
                    session.agent.display_name(),
                    text::one_line(&session.id),
                    text::truncate(&text::one_line(session.label()), 60)
                );
                let mut answer = String::new();
                io::stdin().lock().read_line(&mut answer)?;
                if !matches!(answer.trim(), "y" | "Y" | "yes") {
                    writeln!(out, "kept")?;
                    return Ok(Outcome::Done);
                }
            }
            let entry = actions::delete(&ctx.providers, &ctx.trash(), session)?;
            writeln!(
                out,
                "moved {} session {} to the trash ({} item(s)); undo with: entropeek trash restore {}",
                session.agent.id(),
                text::one_line(&session.id),
                entry.items.len(),
                entry.key
            )?;
        }
        Command::Trash { action } => trash(ctx, action.unwrap_or(TrashAction::List { json: false }), out)?,
    }
    Ok(Outcome::Done)
}

fn list(ctx: &Runtime, args: &ListArgs, out: &mut dyn Write) -> Result<()> {
    let agent = match &args.agent {
        Some(name) => Some(
            AgentKind::from_id(name).with_context(|| format!("unknown agent {name:?} (expected: claude, codex)"))?,
        ),
        None => None,
    };
    let query = Query::new(args.query.as_deref().unwrap_or(""));
    let all = ctx.sessions();
    let id_width = search::unique_prefix_len(&all);
    let sessions: Vec<Session> = all
        .into_iter()
        .filter(|s| agent.is_none_or(|a| s.agent == a))
        .filter(|s| query.matches(&s.search_text()))
        .take(args.limit.unwrap_or(usize::MAX))
        .collect();
    if args.json {
        let rows: Vec<JsonSession> = sessions.iter().map(|s| JsonSession::new(s, ctx)).collect();
        serde_json::to_writer_pretty(&mut *out, &rows)?;
        writeln!(out)?;
    } else {
        write_table_with(out, &sessions, &ctx.env.home_str(), timefmt::now_ms(), id_width)?;
    }
    Ok(())
}

fn trash(ctx: &Runtime, action: TrashAction, out: &mut dyn Write) -> Result<()> {
    let trash = ctx.trash();
    match action {
        TrashAction::List { json } => {
            let listing = trash.scan()?;
            for invalid in &listing.invalid {
                eprintln!(
                    "entropeek: warning: {} is not a usable trash entry: {}",
                    text::one_line(&trash.root().join(&invalid.key).to_string_lossy()),
                    text::one_line(&invalid.error)
                );
            }
            let entries = listing.entries;
            if json {
                serde_json::to_writer_pretty(&mut *out, &entries)?;
                writeln!(out)?;
            } else if entries.is_empty() {
                writeln!(out, "the trash is empty")?;
            } else {
                write_trash_table(out, &entries, timefmt::now_ms())?;
            }
        }
        TrashAction::Restore { key } => {
            let entry = trash.find(&key)?;
            let restored = actions::restore(&ctx.providers, &trash, &entry.key)?;
            writeln!(out, "restored {} session {}", restored.session.agent.id(), text::one_line(&restored.session.id))?;
        }
        TrashAction::Purge { older_than_days, all } => {
            let cutoff = match (older_than_days, all) {
                (Some(days), false) => timefmt::now_ms() - i64::from(days) * 86_400_000,
                (None, true) => i64::MAX,
                _ => bail!("pass exactly one of --older-than-days N or --all"),
            };
            let purged = trash.purge_older_than(cutoff)?;
            writeln!(out, "permanently deleted {} session(s)", purged.count)?;
            if purged.busy > 0 {
                writeln!(out, "skipped {} being changed by another entropeek process", purged.busy)?;
            }
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct JsonSession<'a> {
    id: &'a str,
    agent: &'static str,
    cwd: &'a str,
    title: Option<&'a str>,
    first_prompt: Option<&'a str>,
    last_prompt: Option<&'a str>,
    started_at: Option<String>,
    updated_at: String,
    git_branch: Option<&'a str>,
    user_turns: u32,
    size: u64,
    path: String,
    resume_command: Option<String>,
}

impl<'a> JsonSession<'a> {
    fn new(s: &'a Session, ctx: &Runtime) -> JsonSession<'a> {
        JsonSession {
            id: &s.id,
            agent: s.agent.id(),
            cwd: &s.cwd,
            title: s.title.as_deref(),
            first_prompt: s.first_prompt.as_deref(),
            last_prompt: s.last_prompt.as_deref(),
            started_at: s.started_at.map(timefmt::rfc3339),
            updated_at: timefmt::rfc3339(s.updated_at),
            git_branch: s.git_branch.as_deref(),
            user_turns: s.user_turns,
            size: s.size,
            path: s.path.to_string_lossy().into_owned(),
            resume_command: provider::find(&ctx.providers, s.agent).map(|p| p.resume_command(s).shell_line()),
        }
    }
}

#[derive(Serialize)]
struct JsonTranscript<'a> {
    session: JsonSession<'a>,
    turns: &'a [Turn],
}

/// Plain table: agent, age, short id, workspace, label.
pub fn write_table(out: &mut dyn Write, sessions: &[Session], home: &str, now: i64) -> io::Result<()> {
    write_table_with(out, sessions, home, now, search::unique_prefix_len(sessions))
}

/// [`write_table`] with ids shown to `id_width` characters. Every value
/// comes from session files, so each is cleaned of control characters.
fn write_table_with(
    out: &mut dyn Write,
    sessions: &[Session],
    home: &str,
    now: i64,
    id_width: usize,
) -> io::Result<()> {
    if sessions.is_empty() {
        return writeln!(out, "no sessions found");
    }
    writeln!(
        out,
        "{}  {}  {}  {}  TITLE",
        text::fit("AGENT", 6),
        text::fit_right("AGE", 4),
        text::fit("ID", id_width),
        text::fit("WORKSPACE", 28)
    )?;
    for s in sessions {
        let id: String = text::one_line(&s.id).chars().take(id_width).collect();
        let workspace = text::truncate_middle(&text::one_line(&text::tilde(&s.cwd, home)), 28);
        writeln!(
            out,
            "{}  {}  {}  {}  {}",
            text::fit(s.agent.id(), 6),
            text::fit_right(&timefmt::relative(s.updated_at, now), 4),
            text::fit(&id, id_width),
            text::fit(&workspace, 28),
            text::one_line(s.label())
        )?;
    }
    Ok(())
}

/// Entries are most recent first. `trash restore <session id>` picks the
/// most recent deletion of a session, so older deletions of the same
/// session show the key that selects them.
fn write_trash_table(out: &mut dyn Write, entries: &[TrashEntry], now: i64) -> io::Result<()> {
    writeln!(out, "{}  {}  {}  TITLE", text::fit("DELETED", 7), text::fit("AGENT", 6), text::fit("SESSION", 36))?;
    let mut seen = std::collections::HashSet::new();
    for e in entries {
        let mut note = String::new();
        if e.is_incomplete() {
            note.push_str("  (interrupted; restore to finish)");
        }
        if !seen.insert(e.session.id.as_str()) {
            note.push_str(&format!("  (older deletion; restore with key {})", text::one_line(&e.key)));
        }
        writeln!(
            out,
            "{}  {}  {}  {}{note}",
            text::fit_right(&timefmt::relative(e.deleted_at, now), 7),
            text::fit(e.session.agent.id(), 6),
            text::fit(&text::one_line(&e.session.id), 36),
            text::one_line(e.session.label())
        )?;
    }
    Ok(())
}

fn write_transcript(out: &mut dyn Write, s: &Session, turns: &[Turn], home: &str) -> io::Result<()> {
    writeln!(out, "{} · {}", s.agent.display_name(), text::one_line(s.label()))?;
    writeln!(out, "id:        {}", text::one_line(&s.id))?;
    writeln!(out, "workspace: {}", text::one_line(&text::tilde(&s.cwd, home)))?;
    writeln!(out, "updated:   {}", timefmt::local(s.updated_at))?;
    for turn in turns {
        let who = match turn.role {
            Role::User => "you",
            Role::Assistant => "assistant",
            Role::Tool => {
                writeln!(out, "  ⚙ {}", text::one_line(&turn.text))?;
                continue;
            }
        };
        writeln!(out, "\n── {who} ──")?;
        writeln!(out, "{}", text::sanitize_multiline(&turn.text))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::sample_session;

    #[test]
    fn table_has_header_and_one_row_per_session() {
        let mut a = sample_session(AgentKind::Claude, "abcdef0123456789");
        a.title = Some("Rate limiting".into());
        a.cwd = "/home/u/work/app".into();
        let b = sample_session(AgentKind::Codex, "0000");
        let mut out = Vec::new();
        write_table(&mut out, &[a, b], "/home/u", 2_000 + 3_600_000).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("AGENT"));
        assert!(lines[1].contains("claude") && lines[1].contains("1h") && lines[1].contains("abcdef01"));
        assert!(lines[1].contains("~/work/app") && lines[1].ends_with("Rate limiting"));
        assert!(lines[2].contains("codex") && lines[2].ends_with("first prompt"));
    }

    #[test]
    fn table_output_cannot_carry_terminal_controls() {
        let mut s = sample_session(AgentKind::Claude, "id\u{1b}[2Jx\nFAKE ROW");
        s.cwd = "/home/u/\u{1b}]52;c;aGk=\u{7}evil\nforged".into();
        s.title = Some("t\u{202e}itle".into());
        let mut out = Vec::new();
        write_table(&mut out, &[s.clone()], "/home/u", 0).unwrap();
        let mut transcript = Vec::new();
        let turns = [Turn { role: Role::User, text: "hi\u{1b}[31m".into(), timestamp: None }];
        write_transcript(&mut transcript, &s, &turns, "/home/u").unwrap();
        for bytes in [out, transcript] {
            let text = String::from_utf8(bytes).unwrap();
            assert!(!text.chars().any(|c| c != '\n' && text::is_unsafe_char(c)), "{text:?}");
        }
        let mut out = Vec::new();
        write_table(&mut out, &[s], "/home/u", 0).unwrap();
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 2, "no forged rows");
    }

    #[test]
    fn trash_table_gives_older_deletions_a_selector() {
        let entry = |key: &str, at: i64| TrashEntry {
            version: crate::trash::MANIFEST_VERSION,
            key: key.into(),
            deleted_at: at,
            state: crate::trash::EntryState::Trashed,
            root: None,
            session: sample_session(AgentKind::Claude, "same-id"),
            items: vec![],
        };
        let mut out = Vec::new();
        write_trash_table(&mut out, &[entry("200-claude-same-id", 200), entry("100-claude-same-id", 100)], 300)
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(!lines[1].contains("restore with key"), "the latest is selected by its id");
        assert!(lines[2].ends_with("(older deletion; restore with key 100-claude-same-id)"), "{}", lines[2]);
    }

    #[test]
    fn empty_table_says_so() {
        let mut out = Vec::new();
        write_table(&mut out, &[], "/h", 0).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "no sessions found\n");
    }

    #[test]
    fn cli_parses_subcommands() {
        let cli = Cli::try_parse_from(["entropeek", "list", "--json", "--agent", "codex", "-l", "5"]).unwrap();
        match cli.command {
            Some(Command::List(args)) => {
                assert!(args.json);
                assert_eq!(args.agent.as_deref(), Some("codex"));
                assert_eq!(args.limit, Some(5));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(Cli::try_parse_from(["entropeek"]).unwrap().command.is_none());
        assert!(Cli::try_parse_from(["entropeek", "--no-cache", "trash", "purge", "--all"]).unwrap().no_cache);
        assert!(Cli::try_parse_from(["entropeek", "bogus"]).is_err());
        let themed = Cli::try_parse_from(["entropeek", "--theme", "light"]).unwrap();
        assert_eq!(themed.theme, Some(crate::tui::theme::ThemeChoice::Light));
        assert!(Cli::try_parse_from(["entropeek", "--theme", "blue"]).is_err());
        let iconic = Cli::try_parse_from(["entropeek", "--icons", "on"]).unwrap();
        assert_eq!(iconic.icons, Some(crate::tui::icons::IconChoice::On));
    }
}
