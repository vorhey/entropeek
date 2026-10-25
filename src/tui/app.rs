//! TUI state and behavior, independent of the terminal.
//!
//! Keys go in through [`App::handle_key`], results from background jobs
//! through [`App::on_msg`]. Anything with side effects (resume, delete,
//! rescan) is returned as an [`Effect`] for the event loop to carry out.
//! This keeps the whole UI testable without a terminal.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::path::PathBuf;
use std::sync::Arc;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Line;

use super::theme::{Mode, Theme};
use crate::index::sort_newest_first;
use crate::model::{AgentKind, Session, Turn};
use crate::provider::ResumeCommand;
use crate::search::Query;
use crate::trash::TrashEntry;

/// Identifies one version of a session's transcript: the file and its
/// modification time and size when it was indexed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PreviewKey {
    pub path: PathBuf,
    pub mtime_ns: i64,
    pub size: u64,
}

impl PreviewKey {
    pub fn of(session: &Session) -> PreviewKey {
        PreviewKey { path: session.path.clone(), mtime_ns: session.mtime_ns, size: session.size }
    }
}

/// One transcript version wrapped to one width.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WrapKey {
    pub preview: PreviewKey,
    pub width: u16,
    /// Lines carry colors and icons, so each look has its own wrap.
    pub mode: Mode,
    pub icons: bool,
}

/// A transcript that should be wrapped in the background.
#[derive(Clone, Debug)]
pub struct WrapRequest {
    pub key: WrapKey,
    pub turns: Arc<Vec<Turn>>,
    pub agent: AgentKind,
    pub theme: Theme,
}

/// One line of the session list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// A workspace header.
    Group { cwd: String, count: usize, collapsed: bool },
    /// Index into [`App::sessions`].
    Session(usize),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum RowId {
    Group(String),
    Session(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    List,
    Preview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub text: String,
    pub kind: StatusKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewState {
    Empty,
    Loading(PreviewKey),
    Ready(PreviewKey, Arc<Vec<Turn>>),
    Failed(PreviewKey, String),
}

/// Something the event loop must do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Quit,
    /// Check that the session can be resumed (off the input thread).
    Resume(Session),
    /// Leave the TUI and run this command.
    Exec(ResumeCommand),
    Delete(Session),
    /// Restore the trash entry with this key.
    Restore(String),
    Rescan,
    CopyResume(Session),
}

#[derive(Debug)]
pub struct ScanReport {
    pub sessions: Vec<Session>,
    pub errors: Vec<String>,
}

/// Results from background jobs.
///
/// `generation` counts completed deletes and restores. A scan carries the
/// count from when it *started*, so the app can tell which of its own
/// changes the scan may have missed.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Msg {
    /// What the cache knew at startup, shown until the first scan finishes.
    Cached(Vec<Session>),
    Scanned {
        generation: u64,
        result: Result<ScanReport, String>,
    },
    Preview(PreviewKey, Result<Arc<Vec<Turn>>, String>),
    Wrapped(WrapKey, Result<Arc<Vec<Line<'static>>>, String>),
    Deleted {
        session: Session,
        generation: u64,
        result: Result<TrashEntry, String>,
    },
    Restored {
        key: String,
        generation: u64,
        result: Result<TrashEntry, String>,
    },
    ResumeChecked(Result<ResumeCommand, String>),
    /// Whether to show icons, decided off the input thread at startup.
    Icons(super::icons::Detection),
}

/// Raw transcripts kept for instant back-and-forth (bytes of text).
const PREVIEW_CACHE_BYTES: usize = 64 * 1024 * 1024;
/// Wrapped transcripts kept (estimated bytes of lines and spans).
const WRAP_CACHE_BYTES: usize = 96 * 1024 * 1024;

/// A least-recently-used cache bounded by an estimate of bytes held. The
/// newest entry is always kept, even if it alone exceeds the budget.
struct ByteLru<K, V> {
    map: HashMap<K, (V, usize)>,
    order: VecDeque<K>,
    bytes: usize,
    budget: usize,
    max_entries: usize,
}

impl<K: Clone + Eq + Hash, V: Clone> ByteLru<K, V> {
    fn new(budget: usize, max_entries: usize) -> ByteLru<K, V> {
        ByteLru { map: HashMap::new(), order: VecDeque::new(), bytes: 0, budget, max_entries }
    }

    fn get(&mut self, key: &K) -> Option<V> {
        let value = self.map.get(key)?.0.clone();
        self.touch(key);
        Some(value)
    }

    fn insert(&mut self, key: K, value: V, size: usize) {
        self.remove(&key);
        self.map.insert(key.clone(), (value, size));
        self.order.push_back(key);
        self.bytes += size;
        while self.order.len() > 1 && (self.bytes > self.budget || self.order.len() > self.max_entries) {
            if let Some(old) = self.order.pop_front()
                && let Some((_, size)) = self.map.remove(&old)
            {
                self.bytes -= size;
            }
        }
    }

    fn remove(&mut self, key: &K) {
        if let Some((_, size)) = self.map.remove(key) {
            self.bytes -= size;
            self.order.retain(|k| k != key);
        }
    }

    fn retain(&mut self, keep: impl Fn(&K) -> bool) {
        let gone: Vec<K> = self.order.iter().filter(|k| !keep(k)).cloned().collect();
        for key in gone {
            self.remove(&key);
        }
    }

    fn touch(&mut self, key: &K) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            let key = self.order.remove(pos).expect("position is valid");
            self.order.push_back(key);
        }
    }

    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.bytes
    }
}

pub fn turns_bytes(turns: &[Turn]) -> usize {
    turns.iter().map(|t| t.text.len() + std::mem::size_of::<Turn>()).sum()
}

fn lines_bytes(lines: &[Line]) -> usize {
    lines
        .iter()
        .map(|l| {
            std::mem::size_of::<Line>()
                + l.spans.iter().map(|s| s.content.len() + std::mem::size_of::<ratatui::text::Span>()).sum::<usize>()
        })
        .sum()
}

/// Rendered preview lines for one transcript at one width.
pub struct WrappedPreview {
    pub key: WrapKey,
    pub lines: Arc<Vec<Line<'static>>>,
}

/// A change this app made, kept until a scan is known to include it.
#[derive(Clone, Debug)]
enum Change {
    Deleted,
    Restored(Box<Session>),
}

/// An edit to the search box. Bursts of typing are applied together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchEdit {
    Insert(char),
    Backspace,
}

impl SearchEdit {
    /// The edit a key makes while searching, if it is a plain edit.
    pub fn from_key(key: &KeyEvent) -> Option<SearchEdit> {
        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return None;
        }
        match key.code {
            KeyCode::Char(c) => Some(SearchEdit::Insert(c)),
            KeyCode::Backspace => Some(SearchEdit::Backspace),
            _ => None,
        }
    }
}

pub type ResumeLine = Box<dyn Fn(&Session) -> String>;

/// Whether `key` (with its modifiers) does something that must happen once
/// per press: opening, toggling, deleting, confirming, quitting. Such keys
/// ignore auto-repeat; moving around does not.
fn acts_once(key: &KeyEvent, ctrl: bool) -> bool {
    if ctrl {
        // Ctrl-D / Ctrl-U / Ctrl-N / Ctrl-P move; the rest are commands.
        return !matches!(key.code, KeyCode::Char('d' | 'u' | 'n' | 'p' | 'f' | 'b'));
    }
    matches!(
        key.code,
        KeyCode::Char('d' | 'y' | 'u' | 'r' | 't' | 'i' | 'q' | 'w' | 'p' | 'z' | 'v' | '?' | ' ')
            | KeyCode::Enter
            | KeyCode::Delete
            | KeyCode::Tab
    )
}

pub struct App {
    pub sessions: Vec<Session>,
    haystacks: Vec<String>,
    agent_totals: Vec<(AgentKind, usize)>,
    pub rows: Vec<Row>,
    matches: usize,
    pub cursor: usize,
    /// First list row on screen.
    pub offset: usize,
    /// Rows the list showed in the last frame; used for page moves.
    pub list_height: usize,
    pub query: String,
    /// Whether keys go to the search box.
    pub searching: bool,
    pub hidden_agents: BTreeSet<AgentKind>,
    pub grouped: bool,
    collapsed: HashSet<String>,
    pub delete_armed: Option<PathBuf>,
    pub status: Option<Status>,
    pub focus: Focus,
    /// The user's preference for the preview pane.
    pub show_preview: bool,
    /// Whether the last frame actually had room for the preview pane.
    pub preview_visible: bool,
    pub zoom: bool,
    pub show_help: bool,
    pub help_scroll: usize,
    /// The user's preference for the keys box at the bottom.
    pub show_keys: bool,
    /// Whether the last frame had room for the keys box.
    pub keys_fit: bool,
    pub preview: PreviewState,
    pub preview_scroll: usize,
    /// Rows the preview showed in the last frame.
    pub preview_height: usize,
    /// The lines currently shown for the selected transcript (possibly at
    /// an old width while a new wrap is prepared).
    pub wrapped: Option<WrappedPreview>,
    wrap_cache: ByteLru<WrapKey, Arc<Vec<Line<'static>>>>,
    /// A wrap the view asked for; the event loop hands it to a worker.
    pub wrap_wanted: Option<WrapRequest>,
    wrap_requested: Option<WrapKey>,
    preview_cache: ByteLru<PreviewKey, Arc<Vec<Turn>>>,
    /// Trash keys of this run's deletions, newest last.
    pub undo: Vec<String>,
    /// This app's completed changes, by path, with their generation.
    changes: HashMap<PathBuf, (u64, Change)>,
    pending_deletes: HashSet<PathBuf>,
    pending_restores: HashSet<String>,
    pub scanning: bool,
    pub now: i64,
    pub home: String,
    pub theme: Theme,
    /// Why icons started on or off (shown when switching them).
    pub icons_reason: String,
    /// Startup icon detection has not answered yet (and `i` was not pressed).
    pub icons_pending: bool,
    resume_line: ResumeLine,
}

impl App {
    pub fn new(sessions: Vec<Session>, home: String, now: i64, resume_line: ResumeLine) -> App {
        let mut app = App {
            sessions: Vec::new(),
            haystacks: Vec::new(),
            agent_totals: Vec::new(),
            rows: Vec::new(),
            matches: 0,
            cursor: 0,
            offset: 0,
            list_height: 20,
            query: String::new(),
            searching: false,
            hidden_agents: BTreeSet::new(),
            grouped: true,
            collapsed: HashSet::new(),
            delete_armed: None,
            status: None,
            focus: Focus::List,
            show_preview: true,
            preview_visible: true,
            zoom: false,
            show_help: false,
            help_scroll: 0,
            show_keys: true,
            keys_fit: false,
            preview: PreviewState::Empty,
            preview_scroll: 0,
            preview_height: 20,
            wrapped: None,
            wrap_cache: ByteLru::new(WRAP_CACHE_BYTES, 8),
            wrap_wanted: None,
            wrap_requested: None,
            preview_cache: ByteLru::new(PREVIEW_CACHE_BYTES, 32),
            undo: Vec::new(),
            changes: HashMap::new(),
            pending_deletes: HashSet::new(),
            pending_restores: HashSet::new(),
            scanning: false,
            now,
            home,
            theme: Theme::default(),
            icons_reason: String::new(),
            icons_pending: false,
            resume_line,
        };
        app.set_sessions(sessions);
        app
    }

    pub fn resume_line(&self, session: &Session) -> String {
        (self.resume_line)(session)
    }

    // ----- queries -------------------------------------------------------

    pub fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.cursor)
    }

    pub fn selected_session(&self) -> Option<&Session> {
        match self.selected_row()? {
            Row::Session(index) => self.sessions.get(*index),
            Row::Group { .. } => None,
        }
    }

    /// Number of sessions that pass the agent filter and search.
    pub fn match_count(&self) -> usize {
        self.matches
    }

    /// `(agent, total sessions, hidden)` for agents that have sessions.
    pub fn agent_counts(&self) -> Vec<(AgentKind, usize, bool)> {
        self.agent_totals.iter().map(|&(agent, count)| (agent, count, self.hidden_agents.contains(&agent))).collect()
    }

    /// Whether workspace headers are shown right now.
    pub fn showing_groups(&self) -> bool {
        self.grouped && self.query.trim().is_empty()
    }

    /// Whether any delete or restore is still running.
    pub fn mutating(&self) -> bool {
        !self.pending_deletes.is_empty() || !self.pending_restores.is_empty()
    }

    fn selection_id(&self) -> Option<RowId> {
        match self.selected_row()? {
            Row::Group { cwd, .. } => Some(RowId::Group(cwd.clone())),
            Row::Session(index) => Some(RowId::Session(self.sessions[*index].path.clone())),
        }
    }

    fn find_row(&self, id: &RowId) -> Option<usize> {
        self.rows.iter().position(|row| match (row, id) {
            (Row::Group { cwd, .. }, RowId::Group(want)) => cwd == want,
            (Row::Session(index), RowId::Session(path)) => &self.sessions[*index].path == path,
            _ => false,
        })
    }

    // ----- list building -------------------------------------------------

    /// Replaces all sessions (newest first), keeping the selection if possible.
    pub fn set_sessions(&mut self, sessions: Vec<Session>) {
        let keep = self.selection_id();
        self.replace_sessions(sessions, keep);
    }

    /// Like [`App::set_sessions`], with the row to keep chosen by the caller.
    /// The selection must be captured before `self.sessions` changes, because
    /// rows index into it.
    fn replace_sessions(&mut self, sessions: Vec<Session>, keep: Option<RowId>) {
        self.sessions = sessions;
        self.haystacks = self.sessions.iter().map(Session::search_text).collect();
        self.agent_totals = AgentKind::ALL
            .into_iter()
            .map(|agent| (agent, self.sessions.iter().filter(|s| s.agent == agent).count()))
            .filter(|(_, count)| *count > 0)
            .collect();
        self.rebuild(keep);
    }

    /// Recomputes rows after a filter change. Tries to keep `keep` selected;
    /// otherwise stays at the same position, or picks the first session.
    fn rebuild(&mut self, keep: Option<RowId>) {
        let query = Query::new(&self.query);
        let visible = (0..self.sessions.len())
            .filter(|&i| !self.hidden_agents.contains(&self.sessions[i].agent) && query.matches(&self.haystacks[i]));

        self.rows.clear();
        let mut matches = 0;
        if self.showing_groups() {
            let mut order: Vec<&str> = Vec::new();
            let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
            for i in visible {
                let cwd = self.sessions[i].cwd.as_str();
                groups
                    .entry(cwd)
                    .or_insert_with(|| {
                        order.push(cwd);
                        Vec::new()
                    })
                    .push(i);
            }
            for cwd in order {
                let members = &groups[cwd];
                matches += members.len();
                let collapsed = self.collapsed.contains(cwd);
                self.rows.push(Row::Group { cwd: cwd.to_string(), count: members.len(), collapsed });
                if !collapsed {
                    self.rows.extend(members.iter().map(|&i| Row::Session(i)));
                }
            }
        } else {
            self.rows.extend(visible.map(Row::Session));
            matches = self.rows.len();
        }
        self.matches = matches;

        let previous = self.cursor;
        match keep.and_then(|id| self.find_row(&id)) {
            Some(index) => self.cursor = index,
            None if self.rows.is_empty() => self.cursor = 0,
            None => {
                self.cursor = previous.min(self.rows.len() - 1);
                // Prefer a session over a header when the old row vanished.
                if matches!(self.rows[self.cursor], Row::Group { .. })
                    && let Some(Row::Session(_)) = self.rows.get(self.cursor + 1)
                {
                    self.cursor += 1;
                }
            }
        }
        self.on_selection_changed();
    }

    fn rebuild_keeping_selection(&mut self) {
        let keep = self.selection_id();
        self.rebuild(keep);
    }

    fn on_selection_changed(&mut self) {
        let key = self.selected_session().map(PreviewKey::of);
        let showing = match &self.preview {
            PreviewState::Empty => None,
            PreviewState::Loading(k) | PreviewState::Ready(k, _) | PreviewState::Failed(k, _) => Some(k),
        };
        if key.as_ref() != showing {
            self.preview_scroll = 0;
            self.preview = match key {
                Some(key) => match self.preview_cache.get(&key) {
                    Some(turns) => PreviewState::Ready(key, turns),
                    // Leave it to `preview_request` to start loading.
                    None => PreviewState::Empty,
                },
                None => PreviewState::Empty,
            };
        }
    }

    // ----- cursor ----------------------------------------------------------

    fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
        self.on_selection_changed();
    }

    fn move_to(&mut self, index: usize) {
        if !self.rows.is_empty() {
            self.cursor = index.min(self.rows.len() - 1);
            self.on_selection_changed();
        }
    }

    /// Scrolls the list so the cursor is within `height` visible rows.
    pub fn ensure_visible(&mut self, height: usize) {
        self.list_height = height.max(1);
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + self.list_height {
            self.offset = self.cursor + 1 - self.list_height;
        }
        let max_offset = self.rows.len().saturating_sub(self.list_height);
        self.offset = self.offset.min(max_offset);
    }

    /// Index of the header of the group containing the cursor.
    fn current_group(&self) -> Option<usize> {
        if !self.showing_groups() {
            return None;
        }
        (0..=self.cursor.min(self.rows.len().checked_sub(1)?))
            .rev()
            .find(|&i| matches!(self.rows[i], Row::Group { .. }))
    }

    fn set_collapsed(&mut self, collapse: Option<bool>) {
        let Some(header) = self.current_group() else { return };
        let Row::Group { cwd, collapsed, .. } = &self.rows[header] else { return };
        let cwd = cwd.clone();
        let target = collapse.unwrap_or(!*collapsed);
        if target {
            self.collapsed.insert(cwd.clone());
        } else {
            self.collapsed.remove(&cwd);
        }
        self.rebuild(Some(RowId::Group(cwd)));
    }

    // ----- layout feedback from the view ----------------------------------

    /// Records whether the preview pane fit on screen. A hidden pane cannot
    /// keep keyboard focus.
    pub fn set_preview_visible(&mut self, visible: bool) {
        self.preview_visible = visible;
        if !visible && !self.zoom && self.focus == Focus::Preview {
            self.focus = Focus::List;
        }
    }

    /// Focuses the preview pane, or opens the full-screen transcript when
    /// there is no room for the pane.
    fn focus_preview(&mut self) {
        if self.selected_session().is_none() {
            return;
        }
        if self.show_preview && self.preview_visible {
            self.focus = Focus::Preview;
        } else {
            self.zoom = true;
            self.focus = Focus::Preview;
        }
    }

    // ----- status ----------------------------------------------------------

    pub fn info(&mut self, text: impl Into<String>) {
        self.status = Some(Status { text: text.into(), kind: StatusKind::Info });
    }

    pub fn warn(&mut self, text: impl Into<String>) {
        self.status = Some(Status { text: text.into(), kind: StatusKind::Warn });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.status = Some(Status { text: text.into(), kind: StatusKind::Error });
    }

    // ----- keys ------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return Some(Effect::Quit);
        }
        if key.kind == KeyEventKind::Release {
            return None;
        }
        // Holding down a key that acts once must not act again, nor cancel
        // a pending confirmation. Navigation (including Ctrl-D/Ctrl-U page
        // moves) repeats.
        if key.kind == KeyEventKind::Repeat && !self.searching && acts_once(&key, ctrl) {
            return None;
        }
        self.status = None;
        // Any key other than the confirmation cancels a pending delete.
        let armed = self.delete_armed.take();

        if self.show_help {
            self.handle_help_key(key);
            return None;
        }
        if self.searching {
            return self.handle_search_key(key, ctrl);
        }
        if (self.focus == Focus::Preview || self.zoom)
            && let Some(effect) = self.handle_preview_key(key, ctrl)
        {
            return effect;
        }
        self.handle_list_key(key, ctrl, armed)
    }

    fn handle_help_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.help_scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => self.help_scroll = self.help_scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.help_scroll += 10,
            KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(10),
            KeyCode::Char('g') | KeyCode::Home => self.help_scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.help_scroll = usize::MAX / 2,
            _ => {
                self.show_help = false;
                self.help_scroll = 0;
            }
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent, ctrl: bool) -> Option<Effect> {
        if let Some(edit) = SearchEdit::from_key(&key) {
            self.edit_search(&[edit]);
            return None;
        }
        match key.code {
            KeyCode::Esc => {
                self.searching = false;
                self.set_query(String::new());
            }
            KeyCode::Enter => self.searching = false,
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char('n') if ctrl => self.move_cursor(1),
            KeyCode::Char('p') if ctrl => self.move_cursor(-1),
            KeyCode::Char('u') if ctrl => self.set_query(String::new()),
            KeyCode::Char('w') if ctrl => {
                // Words are split the way the query is: on any whitespace.
                let trimmed = self.query.trim_end();
                let cut = trimmed
                    .rfind(char::is_whitespace)
                    .map_or(0, |i| i + trimmed[i..].chars().next().map_or(1, char::len_utf8));
                self.set_query(trimmed[..cut].to_string());
            }
            _ => {}
        }
        None
    }

    /// Applies several search edits with one filter pass, so a paste or a
    /// burst of typing costs one rebuild instead of one per character.
    /// Backspace removes one visible character (a whole grapheme, such as
    /// an emoji sequence or a letter with its accents).
    pub fn edit_search(&mut self, edits: &[SearchEdit]) {
        use unicode_segmentation::UnicodeSegmentation;
        self.status = None;
        let mut query = self.query.clone();
        for edit in edits {
            match edit {
                SearchEdit::Insert(c) if !c.is_control() => query.push(*c),
                SearchEdit::Insert(_) => {}
                SearchEdit::Backspace => {
                    let keep = query.grapheme_indices(true).next_back().map_or(0, |(at, _)| at);
                    query.truncate(keep);
                }
            }
        }
        self.set_query(query);
    }

    fn set_query(&mut self, query: String) {
        if query == self.query {
            return;
        }
        // Keep the selected session selected across search changes.
        let keep = self.selected_session().map(|s| s.path.clone());
        self.query = query;
        self.offset = 0;
        self.rebuild(keep.clone().map(RowId::Session));
        // If the selected session was filtered out, jump to the best match.
        if self.selected_session().map(|s| &s.path) != keep.as_ref()
            && let Some(first) = self.rows.iter().position(|r| matches!(r, Row::Session(_)))
        {
            self.move_to(first);
        }
    }

    /// Keys while the preview has focus. `Some(effect)` means handled.
    fn handle_preview_key(&mut self, key: KeyEvent, ctrl: bool) -> Option<Option<Effect>> {
        let page = self.preview_height.max(2);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.scroll_preview(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_preview(-1),
            KeyCode::Char('d') if ctrl => self.scroll_preview(page as isize / 2),
            KeyCode::Char('u') if ctrl => self.scroll_preview(-(page as isize) / 2),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_preview(page as isize - 1),
            KeyCode::PageUp => self.scroll_preview(-(page as isize - 1)),
            KeyCode::Char('g') | KeyCode::Home => self.preview_scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.preview_scroll = usize::MAX / 2,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc => {
                self.focus = Focus::List;
                self.zoom = false;
            }
            _ => return None,
        }
        Some(None)
    }

    fn scroll_preview(&mut self, delta: isize) {
        let total = self.wrapped.as_ref().map_or(0, |w| w.lines.len());
        let max = total.saturating_sub(self.preview_height.max(1));
        let current = self.preview_scroll.min(max) as isize;
        self.preview_scroll = (current + delta).clamp(0, max as isize) as usize;
    }

    fn handle_list_key(&mut self, key: KeyEvent, ctrl: bool, armed: Option<PathBuf>) -> Option<Effect> {
        let page = self.list_height.max(2) as isize;
        match key.code {
            KeyCode::Char('q') => return Some(Effect::Quit),
            KeyCode::Esc => {
                if armed.is_some() {
                    self.info("delete cancelled");
                } else if !self.query.is_empty() {
                    self.set_query(String::new());
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char('d') if ctrl => self.move_cursor(page / 2),
            KeyCode::Char('u') if ctrl => self.move_cursor(-page / 2),
            KeyCode::PageDown => self.move_cursor(page - 1),
            KeyCode::PageUp => self.move_cursor(-(page - 1)),
            KeyCode::Char('g') | KeyCode::Home => self.move_to(0),
            KeyCode::Char('G') | KeyCode::End => self.move_to(usize::MAX),
            KeyCode::Enter => match self.selected_row() {
                Some(Row::Group { .. }) => self.set_collapsed(None),
                Some(Row::Session(_)) => return self.selected_session().cloned().map(Effect::Resume),
                None => {}
            },
            KeyCode::Char(' ') => self.set_collapsed(None),
            KeyCode::Char('h') | KeyCode::Left => self.set_collapsed(Some(true)),
            KeyCode::Char('l') | KeyCode::Right => {
                if matches!(self.selected_row(), Some(Row::Group { collapsed: true, .. })) {
                    self.set_collapsed(Some(false));
                } else {
                    self.focus_preview();
                }
            }
            KeyCode::Char('/') => {
                self.searching = true;
                self.focus = Focus::List;
            }
            KeyCode::Char('d') | KeyCode::Delete => self.arm_delete(),
            KeyCode::Char('y') if armed.is_some() => return self.confirm_delete(armed),
            KeyCode::Char('u') => match self.undo.last() {
                Some(key) if self.pending_restores.contains(key) => self.info("already restoring…"),
                Some(key) => {
                    let key = key.clone();
                    self.pending_restores.insert(key.clone());
                    return Some(Effect::Restore(key));
                }
                None => self.info("nothing to undo"),
            },
            KeyCode::Char('y') => {
                if let Some(session) = self.selected_session() {
                    return Some(Effect::CopyResume(session.clone()));
                }
            }
            KeyCode::Char(c @ '1'..='9') => {
                let index = c as usize - '1' as usize;
                if let Some(&agent) = AgentKind::ALL.get(index) {
                    if !self.hidden_agents.remove(&agent) {
                        self.hidden_agents.insert(agent);
                    }
                    self.rebuild_keeping_selection();
                }
            }
            KeyCode::Char('w') => {
                self.grouped = !self.grouped;
                self.rebuild_keeping_selection();
            }
            KeyCode::Char('p') => {
                self.show_preview = !self.show_preview;
                if !self.show_preview {
                    self.focus = Focus::List;
                }
            }
            KeyCode::Tab => self.focus_preview(),
            KeyCode::Char('z') | KeyCode::Char('v') => {
                if self.selected_session().is_some() {
                    self.zoom = true;
                    self.focus = Focus::Preview;
                }
            }
            KeyCode::Char('r') => return Some(self.rescan()),
            KeyCode::Char('i') => {
                // The user's choice wins over a detection still running.
                self.icons_pending = false;
                self.theme = self.theme.with_icons(!self.theme.icons);
                self.wrapped = None;
                let state = if self.theme.icons { "icons on (needs a Nerd Font)" } else { "icons off" };
                if self.icons_reason.is_empty() {
                    self.info(state);
                } else {
                    self.info(format!("{state} · at startup: {}", self.icons_reason));
                }
            }
            KeyCode::Char('t') => {
                self.theme = self.theme.toggled();
                self.wrapped = None;
                self.info(match self.theme.mode {
                    Mode::Dark => "dark theme",
                    Mode::Light => "light theme",
                });
            }
            // The keys box shows every key; `?` hides or shows it. Where it
            // does not fit, `?` opens the full help instead.
            KeyCode::Char('?') if self.keys_fit => self.show_keys = !self.show_keys,
            KeyCode::Char('?') => {
                self.show_help = true;
                self.help_scroll = 0;
            }
            _ => {}
        }
        None
    }

    fn rescan(&mut self) -> Effect {
        // A transcript that failed to load is retried after a rescan.
        if matches!(self.preview, PreviewState::Failed(..)) {
            self.preview = PreviewState::Empty;
        }
        self.scanning = true;
        Effect::Rescan
    }

    /// `d`: asks for confirmation. A second key, `y`, confirms, so holding
    /// `d` down (or a terminal repeating it) can never delete anything.
    fn arm_delete(&mut self) {
        let Some(session) = self.selected_session().cloned() else { return };
        if self.pending_deletes.contains(&session.path) {
            self.info("already moving this session to the trash…");
            return;
        }
        self.delete_armed = Some(session.path.clone());
        self.warn(format!(
            "move \"{}\" to the trash? y to confirm · any other key cancels",
            crate::text::truncate(&crate::text::one_line(session.label()), 50)
        ));
    }

    fn confirm_delete(&mut self, armed: Option<PathBuf>) -> Option<Effect> {
        let session = self.selected_session()?.clone();
        if armed.as_ref() != Some(&session.path) || self.pending_deletes.contains(&session.path) {
            return None;
        }
        self.pending_deletes.insert(session.path.clone());
        Some(Effect::Delete(session))
    }

    // ----- background results ---------------------------------------------

    /// Applies a job result. Returns an effect when the result asks for one
    /// (a checked resume command to run).
    pub fn on_msg(&mut self, msg: Msg) -> Option<Effect> {
        match msg {
            Msg::Cached(sessions) => {
                // Only useful before the first scan result.
                if self.sessions.is_empty() {
                    self.set_sessions(sessions);
                }
            }
            Msg::Scanned { generation, result: Ok(report) } => {
                self.scanning = false;
                let sessions = self.reconcile(generation, report.sessions);
                self.set_sessions(sessions);
                // A rescan retries a transcript that failed to load.
                if matches!(self.preview, PreviewState::Failed(..)) {
                    self.preview = PreviewState::Empty;
                }
                if !report.errors.is_empty() && self.status.is_none() {
                    let count = report.errors.len();
                    let files = if count == 1 { "1 file".to_string() } else { format!("{count} files") };
                    self.warn(format!(
                        "{files} could not be read: {} · r retries",
                        crate::text::one_line(&report.errors[0])
                    ));
                }
            }
            Msg::Scanned { result: Err(error), .. } => {
                self.scanning = false;
                self.error(format!("scan failed: {error}"));
            }
            Msg::Preview(key, result) => {
                let current = self.selected_session().map(PreviewKey::of);
                match result {
                    Ok(turns) => {
                        // Older versions of this file are superseded.
                        self.preview_cache.retain(|k| k.path != key.path || *k == key);
                        self.preview_cache.insert(key.clone(), turns.clone(), turns_bytes(&turns));
                        if current.as_ref() == Some(&key) {
                            self.preview = PreviewState::Ready(key, turns);
                        }
                    }
                    Err(error) => {
                        if current.as_ref() == Some(&key) {
                            self.preview = PreviewState::Failed(key, error);
                        }
                    }
                }
            }
            Msg::Wrapped(key, result) => {
                if self.wrap_requested.as_ref() == Some(&key) {
                    self.wrap_requested = None;
                }
                match result {
                    Ok(lines) => self.store_wrapped(key, lines),
                    Err(error) => self.error(format!("could not format the transcript: {error}")),
                }
            }
            Msg::Deleted { session, generation, result } => {
                self.pending_deletes.remove(&session.path);
                match result {
                    Ok(entry) => {
                        self.changes.insert(session.path.clone(), (generation, Change::Deleted));
                        self.undo.push(entry.key);
                        let keep = self.selection_id();
                        let sessions: Vec<Session> =
                            self.sessions.iter().filter(|s| s.path != session.path).cloned().collect();
                        self.replace_sessions(sessions, keep);
                        self.info(format!(
                            "moved \"{}\" to the trash · u to undo",
                            crate::text::truncate(&crate::text::one_line(session.label()), 50)
                        ));
                    }
                    Err(error) => self.error(format!("delete failed: {error}")),
                }
            }
            Msg::Restored { key, generation, result } => {
                self.pending_restores.remove(&key);
                match result {
                    Ok(entry) => {
                        self.undo.retain(|k| k != &entry.key);
                        let restored = entry.session;
                        let path = restored.path.clone();
                        self.changes.insert(path.clone(), (generation, Change::Restored(Box::new(restored.clone()))));
                        let keep = self.selection_id();
                        let mut sessions = self.sessions.clone();
                        if !sessions.iter().any(|s| s.path == path) {
                            sessions.push(restored.clone());
                            sort_newest_first(&mut sessions);
                        }
                        self.collapsed.remove(&restored.cwd);
                        self.replace_sessions(sessions, keep);
                        if let Some(index) = self.find_row(&RowId::Session(path)) {
                            self.move_to(index);
                        }
                        self.info(format!(
                            "restored \"{}\"",
                            crate::text::truncate(&crate::text::one_line(restored.label()), 50)
                        ));
                    }
                    Err(error) => self.error(format!("restore failed: {error}")),
                }
            }
            Msg::ResumeChecked(Ok(command)) => return Some(Effect::Exec(command)),
            Msg::ResumeChecked(Err(error)) => self.error(error),
            Msg::Icons(detection) => {
                if self.icons_pending {
                    self.icons_pending = false;
                    if detection.icons != self.theme.icons {
                        self.theme = self.theme.with_icons(detection.icons);
                        self.wrapped = None;
                    }
                    self.icons_reason = detection.reason;
                }
            }
        }
        None
    }

    /// Applies this app's own changes that a scan may have missed: those
    /// completed after the scan started (a higher generation). Changes the
    /// scan is known to include are forgotten, so later changes made by
    /// anyone else (a CLI restore, the agent itself) show up normally.
    fn reconcile(&mut self, scan_generation: u64, mut sessions: Vec<Session>) -> Vec<Session> {
        self.changes.retain(|_, (generation, _)| *generation > scan_generation);
        let mut added = false;
        for (path, (_, change)) in &self.changes {
            match change {
                Change::Deleted => sessions.retain(|s| &s.path != path),
                Change::Restored(session) => {
                    if !sessions.iter().any(|s| &s.path == path) {
                        sessions.push((**session).clone());
                        added = true;
                    }
                }
            }
        }
        if added {
            sort_newest_first(&mut sessions);
        }
        sessions
    }

    // ----- preview loading -------------------------------------------------

    /// Whether the selected session's transcript still has to be requested.
    /// Only when the transcript would actually be on screen.
    pub fn preview_pending(&self) -> bool {
        (self.zoom || (self.show_preview && self.preview_visible))
            && self.selected_session().is_some()
            && self.preview == PreviewState::Empty
    }

    /// The session whose transcript should be loaded now, if any. Marks it as
    /// loading. `allow_load` is false while the cursor is still moving fast.
    pub fn preview_request(&mut self, allow_load: bool) -> Option<Session> {
        if !allow_load || !self.preview_pending() {
            return None;
        }
        let session = self.selected_session()?.clone();
        self.preview = PreviewState::Loading(PreviewKey::of(&session));
        Some(session)
    }

    /// Wrapped lines for `key`, if already prepared.
    pub fn cached_wrap(&mut self, key: &WrapKey) -> Option<Arc<Vec<Line<'static>>>> {
        let lines = self.wrap_cache.get(key)?;
        self.wrapped = Some(WrappedPreview { key: key.clone(), lines: lines.clone() });
        Some(lines)
    }

    /// Remembers wrapped lines, and shows them if they belong to the
    /// selected transcript.
    pub fn store_wrapped(&mut self, key: WrapKey, lines: Arc<Vec<Line<'static>>>) {
        self.wrap_cache.retain(|k| k.preview.path != key.preview.path || k.preview == key.preview);
        self.wrap_cache.insert(key.clone(), lines.clone(), lines_bytes(&lines));
        if key.mode == self.theme.mode
            && key.icons == self.theme.icons
            && self.selected_session().map(PreviewKey::of).as_ref() == Some(&key.preview)
        {
            self.wrapped = Some(WrappedPreview { key, lines });
        }
    }

    /// Asks for `request` to be wrapped in the background, once.
    pub fn want_wrap(&mut self, request: WrapRequest) {
        if self.wrap_requested.as_ref() != Some(&request.key) {
            self.wrap_requested = Some(request.key.clone());
            self.wrap_wanted = Some(request);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Role, sample_session};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn repeat(c: char) -> KeyEvent {
        KeyEvent::new_with_kind(KeyCode::Char(c), KeyModifiers::NONE, KeyEventKind::Repeat)
    }

    fn session(agent: AgentKind, id: &str, cwd: &str, updated: i64, title: &str) -> Session {
        let mut s = sample_session(agent, id);
        s.cwd = cwd.into();
        s.path = PathBuf::from(format!("/sessions/{id}.jsonl"));
        s.updated_at = updated;
        s.title = Some(title.into());
        s
    }

    /// Newest first: a1 (/a), b1 (/b), a2 (/a), x1 codex (/b).
    fn sessions() -> Vec<Session> {
        vec![
            session(AgentKind::Claude, "a1", "/a", 400, "alpha one"),
            session(AgentKind::Claude, "b1", "/b", 300, "beta one"),
            session(AgentKind::Claude, "a2", "/a", 200, "alpha two"),
            session(AgentKind::Codex, "x1", "/b", 100, "codex beta"),
        ]
    }

    fn app() -> App {
        App::new(sessions(), "/home/u".into(), 1_000, Box::new(|s| format!("resume {}", s.id)))
    }

    fn selected_id(app: &App) -> Option<&str> {
        app.selected_session().map(|s| s.id.as_str())
    }

    fn ids(app: &App) -> Vec<&str> {
        let mut ids: Vec<&str> = app.sessions.iter().map(|s| s.id.as_str()).collect();
        ids.sort();
        ids
    }

    fn row_labels(app: &App) -> Vec<String> {
        app.rows
            .iter()
            .map(|row| match row {
                Row::Group { cwd, count, collapsed } => {
                    format!("[{cwd} {count}{}]", if *collapsed { " +" } else { "" })
                }
                Row::Session(i) => app.sessions[*i].id.clone(),
            })
            .collect()
    }

    fn scanned(generation: u64, sessions: Vec<Session>) -> Msg {
        Msg::Scanned { generation, result: Ok(ScanReport { sessions, errors: vec![] }) }
    }

    fn entry_for(session: &Session, key: &str) -> TrashEntry {
        TrashEntry {
            version: crate::trash::MANIFEST_VERSION,
            key: key.into(),
            deleted_at: 0,
            state: crate::trash::EntryState::Trashed,
            root: None,
            session: session.clone(),
            items: vec![],
        }
    }

    fn deleted(session: &Session, key: &str, generation: u64) -> Msg {
        Msg::Deleted { session: session.clone(), generation, result: Ok(entry_for(session, key)) }
    }

    fn restored(session: &Session, key: &str, generation: u64) -> Msg {
        Msg::Restored { key: key.into(), generation, result: Ok(entry_for(session, key)) }
    }

    #[test]
    fn groups_by_workspace_in_recency_order() {
        let app = app();
        assert_eq!(row_labels(&app), vec!["[/a 2]", "a1", "a2", "[/b 2]", "b1", "x1"]);
        assert_eq!(selected_id(&app), Some("a1"), "starts on the first session, not a header");
        assert_eq!(app.match_count(), 4);
    }

    #[test]
    fn flat_mode_and_toggle() {
        let mut app = app();
        app.handle_key(ch('w'));
        assert_eq!(row_labels(&app), vec!["a1", "b1", "a2", "x1"]);
        assert_eq!(selected_id(&app), Some("a1"));
        app.handle_key(ch('w'));
        assert_eq!(row_labels(&app).len(), 6);
    }

    #[test]
    fn navigation_is_clamped() {
        let mut app = app();
        app.handle_key(ch('k'));
        app.handle_key(ch('k'));
        assert_eq!(app.cursor, 0);
        app.handle_key(ch('G'));
        assert_eq!(selected_id(&app), Some("x1"));
        app.handle_key(ch('j'));
        assert_eq!(selected_id(&app), Some("x1"));
        app.handle_key(ch('g'));
        assert_eq!(app.cursor, 0);
        app.list_height = 4;
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.cursor, 3);
        app.handle_key(ctrl('u'));
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn navigation_keys_repeat() {
        let mut app = app();
        app.handle_key(KeyEvent::new_with_kind(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Repeat));
        assert_eq!(selected_id(&app), Some("a2"));
        app.handle_key(KeyEvent::new_with_kind(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Release));
        assert_eq!(selected_id(&app), Some("a2"), "releases are ignored");
    }

    #[test]
    fn held_ctrl_page_moves_repeat_but_commands_do_not() {
        let many: Vec<Session> =
            (0..100).map(|i| session(AgentKind::Claude, &format!("s{i:03}"), "/w", 1_000 - i, "t")).collect();
        let mut app = App::new(many, "/h".into(), 2_000, Box::new(|_| String::new()));
        app.grouped = false;
        app.rebuild(None);
        app.list_height = 10;
        let held = |c| KeyEvent::new_with_kind(KeyCode::Char(c), KeyModifiers::CONTROL, KeyEventKind::Repeat);
        let start = app.cursor;
        app.handle_key(held('d'));
        assert!(app.cursor > start, "a held Ctrl-D keeps paging down");
        let down = app.cursor;
        app.handle_key(held('u'));
        assert!(app.cursor < down, "a held Ctrl-U keeps paging up");
        // A held `d` still arms a delete only once.
        app.handle_key(KeyEvent::new_with_kind(KeyCode::Char('d'), KeyModifiers::NONE, KeyEventKind::Repeat));
        assert!(app.delete_armed.is_none());
    }

    #[test]
    fn backspace_removes_one_visible_character() {
        let mut app = app();
        app.handle_key(ch('/'));
        for query in ["ab\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}", "ae\u{301}", "a\u{1f1fa}\u{1f1f8}"] {
            app.set_query(String::new());
            let edits: Vec<SearchEdit> = query.chars().map(SearchEdit::Insert).collect();
            app.edit_search(&edits);
            app.edit_search(&[SearchEdit::Backspace]);
            assert_eq!(app.query.chars().next(), Some('a'));
            assert!(app.query.len() <= 2, "{query:?} left {:?}", app.query);
        }
        app.set_query("one\ttwo".into());
        app.handle_key(ctrl('w'));
        assert_eq!(app.query, "one\t", "Ctrl-W splits on any whitespace, like the query");
    }

    #[test]
    fn collapse_and_expand_groups() {
        let mut app = app();
        app.handle_key(ch(' ')); // on a1: collapses /a, cursor to its header
        assert_eq!(row_labels(&app), vec!["[/a 2 +]", "[/b 2]", "b1", "x1"]);
        assert_eq!(app.cursor, 0);
        assert_eq!(app.match_count(), 4, "collapsed sessions still count as matches");
        app.handle_key(key(KeyCode::Enter)); // Enter on a header toggles it
        assert_eq!(row_labels(&app).len(), 6);
        app.handle_key(ch('h'));
        assert_eq!(row_labels(&app)[0], "[/a 2 +]");
        app.handle_key(ch('l'));
        assert_eq!(row_labels(&app)[0], "[/a 2]");
    }

    #[test]
    fn enter_on_a_session_resumes_it() {
        let mut app = app();
        app.handle_key(ch('j'));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Some(Effect::Resume(app.sessions[2].clone())));
        assert_eq!(app.handle_key(ch('q')), Some(Effect::Quit));
        assert_eq!(app.handle_key(ch('r')), Some(Effect::Rescan));
        assert_eq!(app.handle_key(ch('y')), Some(Effect::CopyResume(app.sessions[2].clone())));
        // The checked command comes back from a worker.
        let command = ResumeCommand { program: "claude".into(), args: vec![], cwd: "/a".into(), executable: None };
        assert_eq!(app.on_msg(Msg::ResumeChecked(Ok(command.clone()))), Some(Effect::Exec(command)));
        assert_eq!(app.on_msg(Msg::ResumeChecked(Err("workspace gone".into()))), None);
        assert_eq!(app.status.as_ref().unwrap().kind, StatusKind::Error);
    }

    #[test]
    fn search_filters_flat_and_restores_groups() {
        let mut app = app();
        app.handle_key(ch('/'));
        assert!(app.searching);
        for c in "BETA".chars() {
            app.handle_key(ch(c));
        }
        assert_eq!(app.query, "BETA");
        assert_eq!(row_labels(&app), vec!["b1", "x1"]);
        assert_eq!(app.match_count(), 2);
        assert_eq!(selected_id(&app), Some("b1"));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(selected_id(&app), Some("x1"));
        // `q` is text while searching, not quit.
        assert_eq!(app.handle_key(ch('q')), None);
        assert!(app.rows.is_empty());
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(row_labels(&app), vec!["b1", "x1"]);
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.searching);
        assert_eq!(app.query, "BETA", "Enter keeps the filter");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.query, "");
        assert_eq!(row_labels(&app).len(), 6);
        assert_eq!(selected_id(&app), Some("b1"), "selection survives clearing the search");
    }

    #[test]
    fn batched_search_edits_match_one_by_one_typing() {
        let mut one = app();
        let mut batch = app();
        one.handle_key(ch('/'));
        batch.handle_key(ch('/'));
        let text = "alphx\u{8}a two";
        let mut edits = Vec::new();
        for c in text.chars() {
            let k = if c == '\u{8}' { key(KeyCode::Backspace) } else { ch(c) };
            one.handle_key(k);
            edits.push(SearchEdit::from_key(&k).unwrap());
        }
        batch.edit_search(&edits);
        assert_eq!(one.query, "alpha two");
        assert_eq!(batch.query, one.query);
        assert_eq!(row_labels(&batch), row_labels(&one));
        assert_eq!(SearchEdit::from_key(&ctrl('w')), None);
    }

    #[test]
    fn search_editing_shortcuts() {
        let mut app = app();
        app.handle_key(ch('/'));
        for c in "alpha two".chars() {
            app.handle_key(ch(c));
        }
        app.handle_key(ctrl('w'));
        assert_eq!(app.query, "alpha ");
        app.handle_key(ctrl('u'));
        assert_eq!(app.query, "");
        app.handle_key(ch('z'));
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.searching);
        assert_eq!(app.query, "");
        assert_eq!(app.handle_key(ctrl('c')), Some(Effect::Quit));
    }

    #[test]
    fn digit_keys_toggle_agents() {
        let mut app = app();
        app.handle_key(ch('2'));
        assert!(app.hidden_agents.contains(&AgentKind::Codex));
        assert_eq!(row_labels(&app), vec!["[/a 2]", "a1", "a2", "[/b 1]", "b1"]);
        assert_eq!(app.agent_counts(), vec![(AgentKind::Claude, 3, false), (AgentKind::Codex, 1, true)]);
        assert_eq!(app.match_count(), 3);
        app.handle_key(ch('9')); // no such agent
        app.handle_key(ch('2'));
        assert_eq!(row_labels(&app).len(), 6);
    }

    #[test]
    fn delete_needs_confirmation_and_can_be_undone() {
        let mut app = app();
        app.handle_key(ch('j')); // a2
        assert_eq!(app.handle_key(ch('d')), None);
        assert!(app.delete_armed.is_some());
        assert_eq!(app.status.as_ref().unwrap().kind, StatusKind::Warn);
        assert!(app.status.as_ref().unwrap().text.contains("y to confirm"));
        // Moving disarms.
        app.handle_key(ch('j'));
        app.handle_key(ch('k'));
        assert!(app.delete_armed.is_none());
        // A second d only re-arms.
        app.handle_key(ch('d'));
        assert_eq!(app.handle_key(ch('d')), None);
        let target = app.selected_session().unwrap().clone();
        assert_eq!(app.handle_key(ch('y')), Some(Effect::Delete(target.clone())));
        assert!(app.mutating());

        app.on_msg(deleted(&target, "k1", 1));
        assert!(!app.mutating());
        assert_eq!(row_labels(&app), vec!["[/a 1]", "a1", "[/b 2]", "b1", "x1"]);
        assert_eq!(selected_id(&app), Some("b1"), "cursor stays in place");
        assert!(app.status.as_ref().unwrap().text.contains("u to undo"));

        // A scan that started before the delete must not bring it back.
        app.on_msg(scanned(0, sessions()));
        assert_eq!(app.sessions.len(), 3);

        assert_eq!(app.handle_key(ch('u')), Some(Effect::Restore("k1".into())));
        assert_eq!(app.handle_key(ch('u')), None, "a restore already running is not repeated");
        app.on_msg(restored(&target, "k1", 2));
        assert_eq!(app.sessions.len(), 4);
        assert_eq!(selected_id(&app), Some("a2"), "the restored session is selected");
        assert!(app.undo.is_empty());
        app.handle_key(ch('u'));
        assert_eq!(app.status.as_ref().unwrap().text, "nothing to undo");
    }

    #[test]
    fn repeated_keys_cannot_confirm_a_delete() {
        let mut app = app();
        app.handle_key(ch('d'));
        // A held d (reported as Repeat, or as more Press events) never deletes.
        assert_eq!(app.handle_key(repeat('d')), None);
        assert!(app.delete_armed.is_some(), "a repeat neither confirms nor cancels");
        for _ in 0..5 {
            assert_eq!(app.handle_key(ch('d')), None);
        }
        assert_eq!(app.handle_key(repeat('y')), None, "a repeated y does not confirm");
        assert!(matches!(app.handle_key(ch('y')), Some(Effect::Delete(_))));
        // While that delete runs, arming and confirming again does nothing.
        app.handle_key(ch('d'));
        assert!(app.delete_armed.is_none());
        assert_eq!(app.handle_key(ch('y')), Some(Effect::CopyResume(app.sessions[0].clone())));
    }

    #[test]
    fn scan_racing_a_restore_keeps_the_restored_session() {
        // Delete A completes; a scan starts and sees A
        // gone; restore A completes; then the older scan result arrives.
        let mut app = app();
        let a = app.sessions[0].clone();
        app.on_msg(deleted(&a, "k1", 1));
        app.on_msg(restored(&a, "k1", 2));
        let without_a: Vec<Session> = sessions().into_iter().filter(|s| s.path != a.path).collect();
        app.on_msg(scanned(1, without_a));
        assert!(app.sessions.iter().any(|s| s.path == a.path), "A is on disk, so it stays listed");
    }

    #[test]
    fn deletes_hide_stale_scans_but_not_later_recreations() {
        let mut app = app();
        let a = app.sessions[0].clone();
        app.on_msg(deleted(&a, "k1", 1));
        app.on_msg(scanned(0, sessions()));
        assert!(!app.sessions.iter().any(|s| s.path == a.path), "a scan from before the delete is corrected");
        // Recreated later by someone else (CLI restore, the agent): a scan
        // that started after the delete is trusted.
        app.on_msg(scanned(1, sessions()));
        assert!(app.sessions.iter().any(|s| s.path == a.path), "the recreated session shows up");
        app.on_msg(scanned(1, sessions().into_iter().skip(1).collect()));
        assert_eq!(ids(&app), vec!["a2", "b1", "x1"]);
    }

    #[test]
    fn esc_cancels_an_armed_delete() {
        let mut app = app();
        app.handle_key(ch('d'));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.delete_armed.is_none());
        assert_eq!(app.status.as_ref().unwrap().text, "delete cancelled");
        // d on a header does nothing.
        app.handle_key(ch('g'));
        assert_eq!(app.handle_key(ch('d')), None);
        assert!(app.delete_armed.is_none());
    }

    #[test]
    fn failed_actions_show_errors() {
        let mut app = app();
        let s = app.sessions[0].clone();
        app.on_msg(Msg::Deleted { session: s, generation: 1, result: Err("boom".into()) });
        assert_eq!(app.status.as_ref().unwrap().kind, StatusKind::Error);
        assert_eq!(app.sessions.len(), 4);
        app.on_msg(Msg::Restored { key: "k".into(), generation: 2, result: Err("nope".into()) });
        assert!(app.status.as_ref().unwrap().text.contains("nope"));
        app.scanning = true;
        app.on_msg(Msg::Scanned { generation: 0, result: Err("internal error: x".into()) });
        assert!(!app.scanning);
        assert!(app.status.as_ref().unwrap().text.contains("scan failed"));
    }

    #[test]
    fn scan_errors_are_reported() {
        let mut app = app();
        app.on_msg(Msg::Scanned {
            generation: 0,
            result: Ok(ScanReport { sessions: sessions(), errors: vec!["cannot read /x: denied".into()] }),
        });
        let status = app.status.as_ref().unwrap();
        assert_eq!(status.kind, StatusKind::Warn);
        assert!(status.text.contains("1 file could not be read") && status.text.contains("/x"), "{}", status.text);
    }

    #[test]
    fn scans_keep_the_selected_session() {
        let mut app = app();
        app.handle_key(ch('G')); // x1
        let mut fresh = sessions();
        fresh.insert(0, session(AgentKind::Claude, "n1", "/n", 999, "new"));
        app.scanning = true;
        app.on_msg(scanned(0, fresh));
        assert!(!app.scanning);
        assert_eq!(selected_id(&app), Some("x1"));
        assert_eq!(app.rows.len(), 8);
        assert_eq!(app.agent_counts(), vec![(AgentKind::Claude, 4, false), (AgentKind::Codex, 1, false)]);
    }

    #[test]
    fn cached_sessions_only_fill_an_empty_list() {
        let mut app = App::new(vec![], "/h".into(), 0, Box::new(|_| String::new()));
        app.on_msg(Msg::Cached(sessions()));
        assert_eq!(app.sessions.len(), 4);
        app.on_msg(Msg::Cached(vec![]));
        assert_eq!(app.sessions.len(), 4);
    }

    #[test]
    fn preview_loads_after_debounce_and_is_cached() {
        let mut app = app();
        assert!(app.preview_pending());
        assert_eq!(app.preview_request(false), None, "no load while the cursor is moving");
        let requested = app.preview_request(true).expect("request");
        assert_eq!(requested.id, "a1");
        assert!(matches!(app.preview, PreviewState::Loading(_)));
        assert!(!app.preview_pending());
        assert_eq!(app.preview_request(true), None, "only one request");

        let turns = Arc::new(vec![Turn { role: Role::User, text: "hi".into(), timestamp: None }]);
        app.on_msg(Msg::Preview(PreviewKey::of(&requested), Ok(turns.clone())));
        assert_eq!(app.preview, PreviewState::Ready(PreviewKey::of(&requested), turns.clone()));

        app.handle_key(ch('j'));
        assert_eq!(app.preview, PreviewState::Empty);
        app.handle_key(ch('k'));
        assert!(matches!(app.preview, PreviewState::Ready(..)), "cache hit is instant");
    }

    #[test]
    fn a_rewritten_file_is_reloaded_even_with_the_same_size_and_time() {
        let mut app = app();
        let first = app.preview_request(true).unwrap();
        let old = Arc::new(vec![Turn { role: Role::User, text: "old".into(), timestamp: None }]);
        app.on_msg(Msg::Preview(PreviewKey::of(&first), Ok(old)));
        // Same size and last record time, new modification time.
        let mut fresh = sessions();
        fresh[0].mtime_ns += 1;
        app.on_msg(scanned(0, fresh));
        assert_eq!(app.preview, PreviewState::Empty, "the old transcript is not shown for the new version");
        assert!(app.preview_request(true).is_some());
    }

    #[test]
    fn rescan_retries_a_failed_preview() {
        let mut app = app();
        let first = app.preview_request(true).unwrap();
        app.on_msg(Msg::Preview(PreviewKey::of(&first), Err("permission denied".into())));
        assert!(matches!(app.preview, PreviewState::Failed(..)));
        app.on_msg(scanned(0, sessions()));
        assert!(app.preview_request(true).is_some(), "a scan result retries the load");
        app.on_msg(Msg::Preview(PreviewKey::of(&first), Err("still denied".into())));
        assert_eq!(app.handle_key(ch('r')), Some(Effect::Rescan));
        assert!(app.preview_request(true).is_some(), "so does pressing r");
    }

    #[test]
    fn stale_preview_results_are_cached_but_not_shown() {
        let mut app = app();
        let first = app.preview_request(true).unwrap();
        app.handle_key(ch('j'));
        let turns = Arc::new(vec![]);
        app.on_msg(Msg::Preview(PreviewKey::of(&first), Ok(turns)));
        assert_eq!(app.preview, PreviewState::Empty);
        app.on_msg(Msg::Preview(PreviewKey::of(app.selected_session().unwrap()), Err("bad".into())));
        assert!(matches!(app.preview, PreviewState::Failed(..)));
        app.handle_key(ch('k'));
        assert!(matches!(app.preview, PreviewState::Ready(..)));
    }

    #[test]
    fn no_preview_requests_when_hidden() {
        let mut app = app();
        app.handle_key(ch('p'));
        assert!(!app.show_preview);
        assert_eq!(app.preview_request(true), None);
        app.handle_key(ch('p'));
        app.set_preview_visible(false);
        assert_eq!(app.preview_request(true), None, "no loads for a pane that does not fit");
    }

    #[test]
    fn a_hidden_preview_cannot_capture_navigation() {
        let mut app = app();
        app.set_preview_visible(false); // e.g. an 80x12 terminal
        app.handle_key(key(KeyCode::Tab));
        assert!(app.zoom, "Tab opens the full-screen transcript instead");
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.zoom);
        assert_eq!(app.focus, Focus::List);
        app.handle_key(ch('j'));
        assert_eq!(selected_id(&app), Some("a2"), "j moves the list");

        // Shrinking the terminal while the preview is focused gives focus back.
        app.set_preview_visible(true);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Preview);
        app.set_preview_visible(false);
        assert_eq!(app.focus, Focus::List);
    }

    #[test]
    fn preview_focus_scrolls_the_preview() {
        let mut app = app();
        app.wrapped = Some(WrappedPreview {
            key: WrapKey {
                preview: PreviewKey::of(&app.sessions[0]),
                width: 40,
                mode: crate::tui::theme::Mode::Dark,
                icons: false,
            },
            lines: Arc::new((0..50).map(|i| Line::from(i.to_string())).collect()),
        });
        app.preview_height = 10;
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Preview);
        app.handle_key(ch('j'));
        app.handle_key(ch('j'));
        assert_eq!(app.preview_scroll, 2);
        assert_eq!(selected_id(&app), Some("a1"), "list did not move");
        app.handle_key(ch('G'));
        app.handle_key(ch('k'));
        assert_eq!(app.preview_scroll, 39);
        app.handle_key(ch('g'));
        assert_eq!(app.preview_scroll, 0);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.focus, Focus::List);
        // Enter from the preview resumes.
        app.handle_key(key(KeyCode::Tab));
        assert!(matches!(app.handle_key(key(KeyCode::Enter)), Some(Effect::Resume(_))));
    }

    #[test]
    fn zoom_and_help() {
        let mut app = app();
        app.handle_key(ch('z'));
        assert!(app.zoom);
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.zoom);
        app.handle_key(ch('?'));
        assert!(app.show_help);
        // Navigation keys scroll the help instead of closing it.
        app.handle_key(ch('j'));
        app.handle_key(ch('j'));
        assert!(app.show_help);
        assert_eq!(app.help_scroll, 2);
        assert_eq!(app.cursor, 1, "and do not move the list");
        // q closes help rather than quitting.
        assert_eq!(app.handle_key(ch('q')), None);
        assert!(!app.show_help);
        app.handle_key(ch('?'));
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.show_help);
    }

    #[test]
    fn question_mark_toggles_the_keys_box_when_it_fits() {
        let mut app = app();
        app.keys_fit = true;
        app.handle_key(ch('?'));
        assert!(!app.show_keys && !app.show_help);
        app.handle_key(ch('?'));
        assert!(app.show_keys);
        app.keys_fit = false;
        app.handle_key(ch('?'));
        assert!(app.show_help, "no room for the box: the help overlay opens");
    }

    #[test]
    fn ensure_visible_scrolls_the_list() {
        let mut app = app();
        app.ensure_visible(3);
        assert_eq!(app.offset, 0);
        app.handle_key(ch('G'));
        app.ensure_visible(3);
        assert_eq!(app.offset, 3);
        app.handle_key(ch('g'));
        app.ensure_visible(3);
        assert_eq!(app.offset, 0);
    }

    #[test]
    fn empty_app_is_safe() {
        let mut app = App::new(vec![], "/h".into(), 0, Box::new(|_| String::new()));
        for k in [
            ch('j'),
            ch('k'),
            ch('G'),
            ch('d'),
            ch('y'),
            ch(' '),
            key(KeyCode::Enter),
            ch('z'),
            key(KeyCode::Tab),
            ch('h'),
        ] {
            assert_eq!(app.handle_key(k), None);
        }
        assert_eq!(app.preview_request(true), None);
        assert_eq!(app.match_count(), 0);
    }

    #[test]
    fn byte_lru_respects_its_budget_and_recency() {
        let mut cache: ByteLru<i64, i64> = ByteLru::new(100, 10);
        cache.insert(1, 1, 40);
        cache.insert(2, 2, 40);
        assert!(cache.get(&1).is_some());
        cache.insert(3, 3, 40); // over budget: evicts 2, the least recent
        assert!(cache.get(&2).is_none());
        assert!(cache.get(&1).is_some() && cache.get(&3).is_some());
        assert_eq!(cache.bytes(), 80);
        cache.insert(4, 4, 500); // a single huge entry is still kept
        assert!(cache.get(&4).is_some());
        assert_eq!(cache.bytes(), 500);
        let mut few: ByteLru<i64, i64> = ByteLru::new(usize::MAX, 2);
        for i in 0..5 {
            few.insert(i, i, 1);
        }
        assert!(few.get(&3).is_some() && few.get(&4).is_some() && few.get(&2).is_none());
    }

    #[test]
    fn preview_memory_is_bounded_by_bytes() {
        let mut app = app();
        let big = |n: usize| Arc::new(vec![Turn { role: Role::User, text: "x".repeat(n), timestamp: None }]);
        for i in 0..40 {
            let key = PreviewKey { path: PathBuf::from(format!("/p{i}")), mtime_ns: 0, size: 0 };
            app.on_msg(Msg::Preview(key, Ok(big(5 * 1024 * 1024))));
        }
        assert!(app.preview_cache.bytes() <= PREVIEW_CACHE_BYTES, "{}", app.preview_cache.bytes());
        // Old versions of one file are dropped when a new one arrives.
        let v = |m: i64| PreviewKey { path: PathBuf::from("/same"), mtime_ns: m, size: 0 };
        app.on_msg(Msg::Preview(v(1), Ok(big(10))));
        app.on_msg(Msg::Preview(v(2), Ok(big(10))));
        assert!(app.preview_cache.get(&v(1)).is_none());
        assert!(app.preview_cache.get(&v(2)).is_some());
    }

    #[test]
    fn wrap_requests_are_made_once_per_key() {
        let mut app = app();
        let request = WrapRequest {
            key: WrapKey {
                preview: PreviewKey::of(&app.sessions[0]),
                width: 40,
                mode: crate::tui::theme::Mode::Dark,
                icons: false,
            },
            turns: Arc::new(vec![]),
            agent: AgentKind::Claude,
            theme: Theme::default(),
        };
        app.want_wrap(request.clone());
        assert!(app.wrap_wanted.take().is_some());
        app.want_wrap(request.clone());
        assert!(app.wrap_wanted.is_none(), "already requested");
        let lines = Arc::new(vec![Line::from("x")]);
        app.on_msg(Msg::Wrapped(request.key.clone(), Ok(lines)));
        assert!(app.wrapped.is_some(), "the selected transcript's lines are shown");
        assert!(app.cached_wrap(&request.key).is_some());
    }
}
