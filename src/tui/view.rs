//! Drawing, styled after showagent: a title line with agent chips, a column
//! header, rows with agent badges, workspace groups, a session box and a
//! keys box. Only rows that fit on screen are turned into widgets. A
//! transcript is wrapped once per width and cached; big ones are wrapped on
//! a worker thread, so drawing never stalls on a long conversation.

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Padding, Paragraph};

use super::app::{App, Focus, PreviewKey, PreviewState, Row, StatusKind, WrapKey, WrapRequest, turns_bytes};
use super::icons::{self, glyph};
use super::theme::Theme;
use crate::model::{AgentKind, Role, Session, Turn};
use crate::text;
use crate::timefmt;

/// Width at which the preview moves beside the list instead of below it.
const SIDE_BY_SIDE_MIN_WIDTH: u16 = 120;
/// Body height below which a stacked preview is hidden.
const STACKED_MIN_BODY: u16 = 14;
/// Transcripts up to this size are wrapped while drawing; bigger ones on a
/// worker (wrapping is roughly 10 ms per MB).
pub const INLINE_WRAP_BYTES: usize = 256 * 1024;
/// The keys box: a border, group titles and four rows of keys.
const KEYS_BOX_HEIGHT: u16 = 7;
/// Rows the list keeps before the keys box gives way to a one-line hint bar.
const MIN_LIST_ROWS: u16 = 8;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < 24 || area.height < 5 {
        frame.render_widget(Paragraph::new("entropeek: terminal too small"), area);
        return;
    }
    let search_visible = app.searching || !app.query.is_empty();
    let fixed = 1 + u16::from(search_visible);
    app.keys_fit = area.width >= 80 && area.height >= fixed + 1 + KEYS_BOX_HEIGHT + MIN_LIST_ROWS;
    let keys_box = app.keys_fit && app.show_keys;
    // With the keys box, a status message gets its own line above it;
    // otherwise it takes the place of the hint bar.
    let status_line = keys_box && app.status.is_some();
    let [header, search, body, status, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(u16::from(search_visible)),
        Constraint::Min(1),
        Constraint::Length(u16::from(status_line)),
        Constraint::Length(if keys_box { KEYS_BOX_HEIGHT } else { 1 }),
    ])
    .areas(area);

    draw_header(frame, header, app);
    if search_visible {
        draw_search(frame, search, app);
    }
    let side_by_side = area.width >= SIDE_BY_SIDE_MIN_WIDTH;
    let fits = side_by_side || body.height >= STACKED_MIN_BODY;
    app.set_preview_visible(app.zoom || (app.show_preview && fits));
    if app.zoom {
        draw_preview(frame, body, app);
    } else if !app.show_preview || !fits {
        draw_list(frame, body, app);
    } else if side_by_side {
        let [list, preview] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);
        draw_list(frame, list, app);
        draw_preview(frame, preview, app);
    } else {
        let [list, preview] = Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body);
        draw_list(frame, list, app);
        draw_preview(frame, preview, app);
    }
    if keys_box {
        if status_line {
            draw_status(frame, status, app);
        }
        draw_keys(frame, bottom, app);
    } else if app.status.is_some() {
        draw_status(frame, bottom, app);
    } else {
        draw_hints(frame, bottom, app);
    }
    if app.show_help {
        draw_help(frame, area, app);
    }
}

/// `entropeek  N sessions  [1 claude 21] [2 codex 3]  · tagline`, dropping
/// parts from the end when the terminal is narrow.
fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let mut parts: Vec<Vec<Span<'static>>> = Vec::new();
    let total = app.sessions.len();
    let shown = app.match_count();
    let count = if shown == total { format!("{total} sessions") } else { format!("{shown} of {total} sessions") };
    parts.push(vec![Span::styled(count, t.dim())]);
    let mut chips = Vec::new();
    for (agent, count, hidden) in app.agent_counts() {
        let digit = AgentKind::ALL.iter().position(|a| *a == agent).unwrap_or(0) + 1;
        let style = if hidden { t.dim().add_modifier(Modifier::CROSSED_OUT) } else { t.badge(agent) };
        if !chips.is_empty() {
            chips.push(Span::raw(" "));
        }
        chips.push(Span::styled(format!(" {digit} {} {count} ", agent.id()), style));
    }
    if !chips.is_empty() {
        parts.push(chips);
    }
    if app.scanning {
        parts.push(vec![Span::styled("· scanning…", t.dim())]);
    }

    let title = Span::styled(format!("{}entropeek", t.icon(glyph::LOGO, "")), t.bold(t.brand));
    let width = area.width as usize;
    let title_width = text::width(&title.content);
    while !parts.is_empty() {
        let used = title_width
            + parts.iter().map(|p| 2 + p.iter().map(|s| text::width(&s.content)).sum::<usize>()).sum::<usize>();
        if used <= width {
            break;
        }
        parts.pop();
    }
    let mut spans = vec![title];
    for part in parts {
        spans.push(Span::raw("  "));
        spans.extend(part);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_search(frame: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let count = app.match_count();
    let count_text = format!("   {count} match{}", if count == 1 { "" } else { "es" });
    let cursor = if app.searching { "▏" } else { "" };
    // Room for the query after the prompt, the cursor and the match count.
    // Editing happens at the end, so a long query shows its tail.
    let room = (area.width as usize).saturating_sub(3 + text::width(cursor) + text::width(&count_text));
    let query = text::one_line_keep_spaces(&app.query);
    let shown = text::tail(&query, room);
    let spans = vec![
        Span::styled(if t.icons { format!(" {} ", glyph::SEARCH) } else { " / ".to_string() }, t.bold(t.accent)),
        Span::styled(shown, t.text()),
        Span::styled(cursor, t.accent()),
        Span::styled(count_text, t.dim()),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Column widths of the session table, like showagent's: a 2-cell gutter
/// (`❯ ` on the selected row), then agent, updated, the workspace (flat
/// list only) and the title, separated by single spaces.
#[derive(Debug, PartialEq, Eq)]
struct Columns {
    agent: usize,
    updated: usize,
    workspace: usize,
    title: usize,
}

const GUTTER: usize = 2;

fn columns(width: usize, with_workspace: bool, icons: bool) -> Columns {
    let inner = width.saturating_sub(GUTTER);
    // ` CLAUDE `, or ` 󰚩 CLAUDE ` with icons.
    let (mut agent, mut updated) = (if icons { 10 } else { 8 }, 8);
    if inner < agent + updated + 2 + 10 {
        agent = agent.min((inner / 5).max(3));
        updated = updated.min((inner / 4).max(5));
    }
    let rest = inner.saturating_sub(agent + updated + 2).max(2);
    // The title keeps priority: the workspace column (and its separator)
    // only appears when there is room for both.
    let workspace = if with_workspace && rest >= 30 { (inner / 3).clamp(10, 46).min((rest - 1) / 2) } else { 0 };
    let title = rest - if workspace > 0 { workspace + 1 } else { 0 };
    Columns { agent, updated, workspace, title: title.max(1) }
}

fn draw_list(frame: &mut Frame, area: Rect, app: &mut App) {
    let t = app.theme;
    // The focused pane has a bright border; the other one is dimmed.
    let focused = app.focus == Focus::List;
    let (edge, title) = if focused { (t.accent, t.accent) } else { (t.edge, t.muted) };
    let block = titled_box(&format!("{}Sessions", t.icon(glyph::LIST, "")), Style::new().fg(edge), t.bold(title));
    let area = {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    };
    let width = area.width as usize;
    let cols = columns(width, !app.showing_groups(), t.icons);
    let [head, rows_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(Paragraph::new(Line::from(Span::styled(column_header(&cols, width), t.header()))), head);

    let height = rows_area.height as usize;
    app.ensure_visible(height);
    if app.rows.is_empty() {
        let message = if app.sessions.is_empty() && app.scanning {
            "scanning sessions…".to_string()
        } else if app.sessions.is_empty() {
            "No sessions found. entropeek reads ~/.claude/projects and ~/.codex/sessions.".to_string()
        } else if !app.query.trim().is_empty() {
            format!("No sessions match \"{}\"", app.query.trim())
        } else {
            "All agents are hidden. Press 1-9 to show them.".to_string()
        };
        frame.render_widget(Paragraph::new(Line::from(Span::styled(format!("  {message}"), t.dim()))), rows_area);
        return;
    }
    let end = (app.offset + height).min(app.rows.len());
    let lines: Vec<Line> = (app.offset..end)
        .map(|index| {
            let selected = index == app.cursor;
            match &app.rows[index] {
                Row::Group { cwd, count, collapsed } => group_line(app, cwd, *count, *collapsed, selected, width),
                Row::Session(i) => session_line(app, &app.sessions[*i], &cols, selected, width),
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rows_area);
}

fn column_header(cols: &Columns, width: usize) -> String {
    let mut line =
        format!("{}{} {}", " ".repeat(GUTTER), text::fit("AGENT", cols.agent), text::fit("UPDATED", cols.updated));
    if cols.workspace > 0 {
        line.push(' ');
        line.push_str(&text::fit("WORKSPACE", cols.workspace));
    }
    line.push(' ');
    line.push_str("TITLE");
    text::fit(&line, width)
}

fn workspace_label(cwd: &str, home: &str) -> String {
    if cwd.is_empty() { "(unknown workspace)".to_string() } else { text::one_line(&text::tilde(cwd, home)) }
}

/// `▾ ~/repos/app (3)` in the group color, or highlighted when selected.
fn group_line(app: &App, cwd: &str, count: usize, collapsed: bool, selected: bool, width: usize) -> Line<'static> {
    let t = app.theme;
    let count_text = format!(" ({count})");
    let marker = match (t.icons, collapsed) {
        (true, true) => glyph::FOLDER,
        (true, false) => glyph::FOLDER_OPEN,
        (false, true) => "▸",
        (false, false) => "▾",
    };
    let label = format!("{marker} {}", workspace_label(cwd, &app.home));
    let label = text::truncate_middle(&label, width.saturating_sub(text::width(&count_text)).max(1));
    let line = text::fit(&format!("{label}{count_text}"), width);
    let style = if selected { t.selected(app.focus == Focus::List) } else { t.bold(t.group) };
    Line::from(Span::styled(line, style))
}

/// The agent's name, centered in `cells`: ` CLAUDE `, or ` CL ` when
/// narrow; with icons ` 󰚩 CLAUDE `, or just the icon when narrow.
fn agent_label(agent: AgentKind, cells: usize, icons: bool) -> String {
    let name = agent.id().to_uppercase();
    let (name, short) = if icons {
        let icon = icons::agent(agent);
        (format!("{icon} {name}"), icon.to_string())
    } else {
        let short = match agent {
            AgentKind::Claude => "CL",
            AgentKind::Codex => "CX",
        };
        (name, short.to_string())
    };
    let label = if cells >= text::width(&name) + 2 {
        format!(" {name} ")
    } else if cells >= text::width(&short) + 2 {
        format!(" {short} ")
    } else {
        short
    };
    let label = text::truncate(&label, cells);
    let left = cells.saturating_sub(text::width(&label)) / 2;
    text::fit(&format!("{}{label}", " ".repeat(left)), cells)
}

fn age_label(app: &App, session: &Session, cells: usize) -> String {
    let full = timefmt::updated(session.updated_at, app.now);
    let label = if text::width(&full) <= cells { full } else { timefmt::relative(session.updated_at, app.now) };
    text::fit(&label, cells)
}

fn session_line(app: &App, session: &Session, cols: &Columns, selected: bool, width: usize) -> Line<'static> {
    let t = app.theme;
    let armed = app.delete_armed.as_ref() == Some(&session.path);
    let agent = agent_label(session.agent, cols.agent, t.icons);
    let age = age_label(app, session, cols.updated);
    let workspace = workspace_label(&session.cwd, &app.home);
    let title = text::one_line(session.label());

    // The selected (or armed) row is one uniform highlight, like showagent.
    if selected || armed {
        let mut line = format!("❯ {agent} {age}");
        if cols.workspace > 0 {
            line.push(' ');
            line.push_str(&text::fit(&text::truncate_middle(&workspace, cols.workspace), cols.workspace));
        }
        line.push(' ');
        line.push_str(&text::truncate(&title, cols.title));
        let style = if armed { t.danger() } else { t.selected(app.focus == Focus::List) };
        return Line::from(Span::styled(text::fit(&line, width), style));
    }

    let mut spans = vec![
        Span::raw(" ".repeat(GUTTER)),
        Span::styled(agent, t.badge(session.agent)),
        Span::raw(" "),
        Span::styled(age, t.dim()),
    ];
    if cols.workspace > 0 {
        spans.push(Span::raw(" "));
        spans.extend(workspace_cell(&t, &workspace, cols.workspace));
    }
    spans.push(Span::raw(" "));
    spans.push(Span::styled(text::fit(&title, cols.title), t.text()));
    Line::from(spans)
}

/// A workspace path with its last part emphasized.
fn workspace_cell(t: &Theme, workspace: &str, cells: usize) -> Vec<Span<'static>> {
    let value = text::fit(&text::truncate_middle(workspace, cells), cells);
    let base = workspace.rsplit('/').find(|p| !p.is_empty()).unwrap_or("");
    match value.rfind(base).filter(|&i| i > 0 && !base.is_empty()) {
        Some(index) => vec![
            Span::styled(value[..index].to_string(), t.dim()),
            Span::styled(value[index..].to_string(), t.bold(t.strong)),
        ],
        None => vec![Span::styled(value, t.dim())],
    }
}

fn human_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

/// The session's details as labeled lines, like showagent's session box.
fn meta_lines(app: &App, session: &Session, width: usize) -> Vec<Line<'static>> {
    let t = app.theme;
    let label = |name: &str| Span::styled(format!("{name:<10} "), t.bold(t.value));
    let workspace = format!("{}{}", t.icon(glyph::FOLDER_OPEN, ""), workspace_label(&session.cwd, &app.home));
    let mut place = vec![label("workspace"), Span::styled(workspace, t.text())];
    if let Some(branch) = &session.git_branch {
        place.push(Span::styled(format!(" · {}{}", t.icon(glyph::BRANCH, "⎇ "), text::one_line(branch)), t.dim()));
    }
    let prompts =
        if session.user_turns == 1 { "1 prompt".to_string() } else { format!("{} prompts", session.user_turns) };
    let fit = |spans: Vec<Span<'static>>| {
        let mut used = 0;
        let mut out = Vec::new();
        for span in spans {
            let room = width.saturating_sub(used);
            if room == 0 {
                break;
            }
            let content = text::truncate(&span.content, room);
            used += text::width(&content);
            out.push(Span::styled(content, span.style));
        }
        Line::from(out)
    };
    vec![
        fit(vec![Span::styled(text::one_line(session.label()), t.bold(t.text))]),
        fit(place),
        fit(vec![
            label("updated"),
            Span::styled(format!("{}{}", t.icon(glyph::CLOCK, ""), timefmt::local(session.updated_at)), t.text()),
            Span::styled(format!(" ({})", timefmt::ago(session.updated_at, app.now)), t.dim()),
            Span::styled(format!(" · {prompts} · {}", human_size(session.size)), t.dim()),
        ]),
        fit(vec![
            label("session"),
            Span::styled(agent_label(session.agent, if t.icons { 10 } else { 8 }, t.icons), t.badge(session.agent)),
            Span::styled(format!(" {}", text::one_line(&session.id)), t.text()),
        ]),
        fit(vec![
            label("command"),
            Span::styled(format!("{}{}", t.icon(glyph::TERMINAL, ""), app.resume_line(session)), t.text()),
        ]),
        Line::from(Span::styled("─".repeat(width), Style::new().fg(t.edge))),
    ]
}

/// Turns a transcript into display lines wrapped to `width`. Pure, so it
/// can run on a worker thread.
pub fn transcript_lines(turns: &[Turn], agent: AgentKind, width: usize, t: Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let body_width = width.saturating_sub(2).max(1);
    if turns.is_empty() {
        lines.push(Line::from(Span::styled("(no messages)", t.dim())));
    }
    for turn in turns {
        let (who, color) = match turn.role {
            Role::User => (format!("{}You", t.icon(glyph::USER, "")), t.accent),
            Role::Assistant => (format!("{}{}", t.icon(icons::agent(agent), ""), agent.display_name()), t.agent(agent)),
            Role::Tool => {
                let icon = t.icon(glyph::WRENCH, "⚙ ");
                lines.push(Line::from(Span::styled(
                    format!("  {icon}{}", text::truncate(&turn.text, width.saturating_sub(4))),
                    t.dim(),
                )));
                continue;
            }
        };
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        let mut heading = vec![Span::styled(format!("▍{who}"), t.bold(color))];
        if let Some(ts) = turn.timestamp {
            heading.push(Span::styled(format!("  {}", timefmt::local(ts)), t.dim()));
        }
        lines.push(Line::from(heading));
        let body = if turn.text.contains('\t') { turn.text.replace('\t', "    ") } else { turn.text.clone() };
        for line in text::wrap(&body, body_width) {
            lines.push(Line::from(Span::styled(format!("  {line}"), t.text())));
        }
    }
    lines
}

/// The wrapped lines for a transcript at `width`. Small transcripts are
/// wrapped right here; big ones are handed to a worker, and until it
/// answers the previous wrap of the same transcript (at its old width) or a
/// placeholder is shown, so a resize or a new selection never blocks input.
fn wrapped_transcript(
    app: &mut App,
    key: &PreviewKey,
    turns: &Arc<Vec<Turn>>,
    agent: AgentKind,
    width: u16,
) -> Arc<Vec<Line<'static>>> {
    let t = app.theme;
    let wrap_key = WrapKey { preview: key.clone(), width, mode: t.mode, icons: t.icons };
    if let Some(lines) = app.cached_wrap(&wrap_key) {
        return lines;
    }
    if turns_bytes(turns) <= INLINE_WRAP_BYTES {
        let lines = Arc::new(transcript_lines(turns, agent, width as usize, t));
        app.store_wrapped(wrap_key, lines.clone());
        return lines;
    }
    app.want_wrap(WrapRequest { key: wrap_key, turns: turns.clone(), agent, theme: t });
    if let Some(previous) = &app.wrapped
        && &previous.key.preview == key
    {
        return previous.lines.clone();
    }
    Arc::new(vec![Line::from(Span::styled("formatting…", t.dim()))])
}

/// A rounded box with a bold title, like showagent's.
fn titled_box(title: &str, edge: Style, title_style: Style) -> Block<'static> {
    Block::bordered()
        .border_set(border::ROUNDED)
        .border_style(edge)
        .title(Line::from(vec![Span::styled("─", edge), Span::styled(format!(" {title} "), title_style)]))
}

fn draw_preview(frame: &mut Frame, area: Rect, app: &mut App) {
    let t = app.theme;
    let focused = app.focus == Focus::Preview || app.zoom;
    // The conversation box is green; it turns to the accent color when focused.
    let color = if focused { t.accent } else { t.detail };
    let edge = Style::new().fg(color);
    let title = format!("{}Conversation", t.icon(glyph::COMMENTS, ""));
    let block = titled_box(&title, edge, t.bold(color)).padding(Padding::horizontal(1));
    let inner = block.inner(area);
    let width = inner.width as usize;

    let Some(session) = app.selected_session().cloned() else {
        let lines = match app.selected_row() {
            Some(Row::Group { cwd, count, collapsed }) => vec![
                Line::from(Span::styled(workspace_label(cwd, &app.home), t.bold(t.group))),
                Line::from(Span::styled(format!("{count} session{}", if *count == 1 { "" } else { "s" }), t.text())),
                Line::from(Span::styled(
                    if *collapsed { "space or enter to expand" } else { "space or enter to collapse" },
                    t.dim(),
                )),
            ],
            _ => vec![Line::from(Span::styled("no session selected", t.dim()))],
        };
        frame.render_widget(Paragraph::new(lines).block(block), area);
        return;
    };

    let header = meta_lines(app, &session, width);
    let body = match app.preview.clone() {
        PreviewState::Ready(key, turns) => wrapped_transcript(app, &key, &turns, session.agent, inner.width),
        PreviewState::Failed(_, error) => Arc::new(vec![Line::from(Span::styled(
            format!("could not read the session: {error}"),
            Style::new().fg(t.error),
        ))]),
        PreviewState::Loading(_) | PreviewState::Empty => Arc::new(vec![Line::from(Span::styled("loading…", t.dim()))]),
    };

    let header_height = (header.len() as u16).min(inner.height);
    let body_height = inner.height - header_height;
    app.preview_height = body_height as usize;
    let max_scroll = body.len().saturating_sub(body_height as usize);
    app.preview_scroll = app.preview_scroll.min(max_scroll);
    let start = app.preview_scroll;
    let end = (start + body_height as usize).min(body.len());

    let mut block = block;
    if body.len() > body_height as usize {
        let percent = (start * 100).checked_div(max_scroll).unwrap_or(100);
        block = block.title_bottom(Line::from(Span::styled(format!(" {percent}% "), edge)).right_aligned());
    }
    frame.render_widget(block, area);
    let [head_area, body_area] = Layout::vertical([Constraint::Length(header_height), Constraint::Min(0)]).areas(inner);
    frame.render_widget(Paragraph::new(header), head_area);
    frame.render_widget(Paragraph::new(body[start..end].to_vec()), body_area);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let Some(status) = &app.status else { return };
    let style = match status.kind {
        StatusKind::Info => t.accent(),
        StatusKind::Warn => t.bold(t.warn),
        StatusKind::Error => t.bold(t.error),
    };
    let text = text::truncate(&format!(" {}", status.text), area.width as usize);
    frame.render_widget(Paragraph::new(Span::styled(text, style)), area);
}

/// One key in the keys box: the key, what it does, and a live value.
struct KeyHint {
    key: &'static str,
    desc: &'static str,
    value: Option<&'static str>,
}

const fn hint(key: &'static str, desc: &'static str) -> KeyHint {
    KeyHint { key, desc, value: None }
}

const KEY_GROUPS: [&str; 4] = ["SESSION", "VIEW", "FIND", "MORE"];

/// Every key in four groups, with the current state where it has one.
fn key_groups(app: &App) -> [Vec<KeyHint>; 4] {
    let on_off = |on: bool| Some(if on { "on" } else { "off" });
    let resume = if matches!(app.selected_row(), Some(Row::Group { .. })) {
        hint("enter", "open/close")
    } else {
        hint("enter", "resume")
    };
    let view = if app.focus == Focus::Preview || app.zoom {
        vec![hint("j/k", "scroll"), hint("g/G", "top / end"), hint("esc", "back to list"), hint("z", "full screen")]
    } else {
        vec![
            hint("tab", "focus preview"),
            hint("z", "full screen"),
            KeyHint { key: "p", desc: "preview", value: on_off(app.show_preview) },
            KeyHint { key: "w", desc: "groups", value: on_off(app.grouped) },
        ]
    };
    let agents = if app.agent_counts().len() > 1 { "1-2" } else { "1" };
    let theme = Some(match app.theme.mode {
        super::theme::Mode::Dark => "dark",
        super::theme::Mode::Light => "light",
    });
    [
        vec![resume, hint("d", "delete"), hint("u", "undo"), hint("y", "copy command")],
        view,
        vec![hint("/", "search"), hint(agents, "agents"), hint("space", "collapse"), hint("r", "rescan")],
        vec![
            KeyHint { key: "t", desc: "theme", value: theme },
            hint("?", "hide keys"),
            hint("q", "quit"),
            KeyHint { key: "i", desc: "icons", value: Some(if app.theme.icons { "on" } else { "off" }) },
        ],
    ]
}

/// The keys box at the bottom, laid out on a fixed grid like showagent's so
/// columns do not move as values change.
fn draw_keys(frame: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let title = format!("{}Keys", t.icon(glyph::KEYBOARD, ""));
    let block = titled_box(&title, Style::new().fg(t.edge), t.bold(t.accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };

    const GAP: usize = 3;
    let groups = key_groups(app);
    let cells: Vec<Vec<Vec<Span<'static>>>> = groups
        .iter()
        .enumerate()
        .map(|(index, group)| {
            let key_width = group.iter().map(|h| text::width(h.key)).max().unwrap_or(0);
            let mut column = vec![vec![Span::styled(KEY_GROUPS[index], t.bold(t.group))]];
            for h in group {
                let mut cell = vec![
                    Span::styled(text::fit(h.key, key_width), t.bold(t.accent)),
                    Span::raw(" "),
                    Span::styled(h.desc, t.text()),
                ];
                if let Some(value) = h.value {
                    cell.push(Span::raw(" "));
                    cell.push(Span::styled(value, t.bold(t.value)));
                }
                column.push(cell);
            }
            column
        })
        .collect();
    let width_of = |cell: &[Span]| cell.iter().map(|s| text::width(&s.content)).sum::<usize>();
    let mut widths: Vec<usize> = cells.iter().map(|c| c.iter().map(|cell| width_of(cell)).max().unwrap_or(0)).collect();
    let room = inner.width as usize;
    let slot = (room.saturating_sub(GAP * 3) / 4).min(30);
    let grid: Vec<usize> = widths.iter().map(|w| (*w).max(slot)).collect();
    if grid.iter().sum::<usize>() + GAP * 3 <= room {
        widths = grid;
    }
    while widths.iter().sum::<usize>() + GAP * 3 > room {
        let widest = (0..widths.len()).max_by_key(|&i| widths[i]).unwrap_or(0);
        if widths[widest] <= 8 {
            break;
        }
        widths[widest] -= 1;
    }

    let lines: Vec<Line> = (0..5)
        .map(|row| {
            let mut spans = Vec::new();
            for (index, column) in cells.iter().enumerate() {
                let cell = column.get(row).cloned().unwrap_or_default();
                let mut used = 0;
                for span in cell {
                    let content = text::truncate(&span.content, widths[index].saturating_sub(used));
                    used += text::width(&content);
                    spans.push(Span::styled(content, span.style));
                }
                let pad = widths[index].saturating_sub(used) + if index + 1 < cells.len() { GAP } else { 0 };
                spans.push(Span::raw(" ".repeat(pad)));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn hints(app: &App) -> &'static [(&'static str, &'static str)] {
    if app.searching {
        &[("type", "filter"), ("↑↓", "move"), ("enter", "keep"), ("esc", "clear")]
    } else if app.focus == Focus::Preview || app.zoom {
        &[("j/k", "scroll"), ("g/G", "top/end"), ("↵", "resume"), ("esc", "back"), ("q", "quit")]
    } else {
        &[
            ("↵", "resume"),
            ("/", "search"),
            ("d", "delete"),
            ("u", "undo"),
            ("space", "fold"),
            ("tab", "preview"),
            ("z", "zoom"),
            ("y", "copy cmd"),
            ("?", "keys"),
            ("q", "quit"),
        ]
    }
}

/// The one-line hint bar, used when the keys box is hidden or doesn't fit.
fn draw_hints(frame: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let width = area.width as usize;
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    for (key, label) in hints(app) {
        let cost = text::width(key) + text::width(label) + 3;
        if used + cost > width {
            break;
        }
        spans.push(Span::styled(*key, t.bold(t.accent)));
        spans.push(Span::styled(format!(" {label}  "), t.dim()));
        used += cost;
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

const HELP: &[(&str, &str)] = &[
    ("j/k ↑/↓", "move"),
    ("g/G", "first / last"),
    ("PgUp/PgDn ^u/^d", "page / half page"),
    ("enter", "resume session (or fold a workspace)"),
    ("/", "search titles, prompts, paths, ids"),
    ("esc", "clear search / cancel / back"),
    ("d then y", "move session to the trash"),
    ("u", "undo the last delete"),
    ("space h/l", "fold / unfold workspace"),
    ("w", "group by workspace on/off"),
    ("1-9", "show / hide an agent"),
    ("tab l", "focus the preview (j/k scroll)"),
    ("z", "full-screen transcript"),
    ("p", "preview pane on/off"),
    ("y", "copy resume command to clipboard"),
    ("t", "light / dark theme"),
    ("i", "Nerd Font icons on/off"),
    ("r", "rescan session files"),
    ("?", "keys box on/off (this help when it doesn't fit)"),
    ("q ^c", "quit"),
];

fn draw_help(frame: &mut Frame, area: Rect, app: &mut App) {
    let t = app.theme;
    let width = 64.min(area.width.saturating_sub(4));
    let height = (HELP.len() as u16 + 2).min(area.height.saturating_sub(2));
    if width < 4 || height < 3 {
        return;
    }
    let rect = Rect { x: area.x + (area.width - width) / 2, y: area.y + (area.height - height) / 2, width, height };
    let rows = (height - 2) as usize;
    let max_scroll = HELP.len().saturating_sub(rows);
    app.help_scroll = app.help_scroll.min(max_scroll);
    let key_width = 18.min((width as usize).saturating_sub(2) / 2);
    let lines: Vec<Line> = HELP
        .iter()
        .skip(app.help_scroll)
        .take(rows)
        .map(|(key, what)| {
            Line::from(vec![Span::styled(text::fit(key, key_width), t.bold(t.accent)), Span::styled(*what, t.text())])
        })
        .collect();
    let title = if max_scroll > 0 { "Keys · j/k scroll · esc closes" } else { "Keys · esc closes" };
    let mut block = titled_box(title, Style::new().fg(t.edge), t.bold(t.accent));
    if max_scroll > 0 {
        let more = if app.help_scroll < max_scroll { " ↓ more " } else { " end " };
        block = block.title_bottom(Line::from(Span::styled(more, t.dim())).right_aligned());
    }
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::sample_session;
    use crate::tui::app::Msg;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::Color;

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn session(agent: AgentKind, id: &str, cwd: &str, updated: i64, title: &str) -> Session {
        let mut s = sample_session(agent, id);
        s.cwd = cwd.into();
        s.updated_at = updated;
        s.title = Some(title.into());
        s.git_branch = Some("main".into());
        s
    }

    const NOW: i64 = 10 * 86_400_000;

    fn app() -> App {
        App::new(
            vec![
                session(AgentKind::Claude, "a1", "/home/u/app", NOW - 3_600_000, "Rate limiting for API"),
                session(AgentKind::Codex, "x1", "/home/u/app", NOW - 2 * 86_400_000, "Review rate limiter"),
                session(AgentKind::Claude, "s1", "/home/u/site", NOW - 3 * 86_400_000, "Landing page"),
            ],
            "/home/u".into(),
            NOW,
            Box::new(|s| format!("claude --resume {}", s.id)),
        )
    }

    fn render(app: &mut App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn lines(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        lines(&render(app, width, height)).join("\n")
    }

    fn row_of(buffer: &Buffer, needle: &str) -> u16 {
        lines(buffer).iter().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle:?} not on screen"))
            as u16
    }

    /// The cell column where `needle` starts on row `y` (wide characters
    /// before it take one string char but one cell each in the buffer).
    fn col_of(buffer: &Buffer, y: u16, needle: &str) -> u16 {
        let line = &lines(buffer)[y as usize];
        let byte = line.find(needle).unwrap_or_else(|| panic!("{needle:?} not on row {y}"));
        line[..byte].chars().count() as u16
    }

    #[test]
    fn title_line_column_header_groups_and_rows() {
        let mut app = app();
        let screen = screen(&mut app, 100, 30);
        assert!(screen.lines().nth(1).unwrap().starts_with("╭─ Sessions ─"), "{screen}");
        let first = screen.lines().next().unwrap();
        assert_eq!(first.trim_end(), "entropeek  3 sessions   1 claude 2   2 codex 1", "no description");
        let buffer = render(&mut app, 100, 30);
        assert_eq!(buffer[(0, 0)].fg, Theme::dark().brand, "the name is dark green");
        assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        app.theme = Theme::light().with_icons(true);
        let buffer = render(&mut app, 100, 30);
        assert!(
            lines(&buffer)[0].starts_with(&format!("{} entropeek  3 sessions", glyph::LOGO)),
            "{}",
            lines(&buffer)[0]
        );
        assert_eq!(buffer[(0, 0)].fg, Theme::light().brand, "the logo too");
        assert!(screen.contains("  AGENT    UPDATED  TITLE"), "column header");
        assert!(screen.contains("▾ ~/app (2)"));
        assert!(screen.contains("▾ ~/site (1)"));
        assert!(screen.contains("❯  CLAUDE  1h ago   Rate limiting for API"), "{screen}");
        assert!(screen.contains("   CODEX   2d ago   Review rate limiter"));
        assert!(screen.contains("   CLAUDE  3d ago   Landing page"));
    }

    #[test]
    fn column_header_uses_the_header_colors() {
        let mut app = app();
        let buffer = render(&mut app, 100, 30);
        let y = row_of(&buffer, "AGENT");
        for x in [1, 3, 50, 98] {
            assert_eq!(buffer[(x, y)].bg, Theme::dark().header_bg, "the whole row inside the box, x={x}");
        }
        assert_eq!(buffer[(2, y)].fg, Theme::dark().header_fg);
        assert!(buffer[(2, y)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn nerd_font_icons_when_enabled() {
        let mut app = app();
        app.theme = Theme::dark().with_icons(true);
        let session = app.preview_request(true).unwrap();
        let turns = vec![
            Turn { role: Role::User, text: "hi".into(), timestamp: None },
            Turn { role: Role::Assistant, text: "hello".into(), timestamp: None },
            Turn { role: Role::Tool, text: "Bash: ls".into(), timestamp: None },
        ];
        app.on_msg(Msg::Preview(PreviewKey::of(&session), Ok(Arc::new(turns))));
        let screen = screen(&mut app, 130, 30);
        let has = |text: String| assert!(screen.contains(&text), "{text:?} missing\n{screen}");
        has(format!("{} Sessions", glyph::LIST));
        has(format!("{} Conversation", glyph::COMMENTS));
        has(format!("{} Keys", glyph::KEYBOARD));
        has(format!("{} ~/app (2)", glyph::FOLDER_OPEN));
        has(format!(" {} CLAUDE ", glyph::ROBOT));
        has(format!(" {} CODEX ", glyph::CODE));
        has(format!("▍{} You", glyph::USER));
        has(format!("▍{} Claude", glyph::ROBOT));
        has(format!("{} Bash: ls", glyph::WRENCH));
        has(format!("{} main", glyph::BRANCH));
        has("i icons on".to_string());
        // Rows stay aligned with the column header.
        assert!(screen.contains("  AGENT      UPDATED  TITLE"), "{screen}");
        // Narrow badges become just the icon.
        let narrow = self::screen(&mut app, 24, 12);
        assert!(narrow.contains(&format!(" {} ", glyph::CODE)), "{narrow}");
        // A collapsed workspace shows a closed folder.
        app.handle_key(ch(' '));
        assert!(self::screen(&mut app, 130, 30).contains(&format!("{} ~/app (2)", glyph::FOLDER)));
    }

    #[test]
    fn i_switches_icons_and_reformats_the_transcript() {
        let mut app = app();
        let session = app.preview_request(true).unwrap();
        let turns = vec![Turn { role: Role::User, text: "hello".into(), timestamp: None }];
        app.on_msg(Msg::Preview(PreviewKey::of(&session), Ok(Arc::new(turns))));
        assert!(screen(&mut app, 130, 30).contains("▍You"));
        app.handle_key(ch('i'));
        assert!(app.theme.icons);
        assert!(app.status.as_ref().unwrap().text.contains("icons on"));
        assert!(screen(&mut app, 130, 30).contains(&format!("▍{} You", glyph::USER)), "not the cached plain lines");
        app.handle_key(ch('t'));
        assert!(app.theme.icons, "switching the theme keeps icons");
        app.handle_key(ch('i'));
        assert!(screen(&mut app, 130, 30).contains("▍You"));
    }

    #[test]
    fn the_focused_pane_has_the_bright_border() {
        let mut app = app();
        let t = Theme::dark();
        let edge_color = |app: &mut App, title: &str| {
            let buffer = render(app, 130, 30);
            let y = row_of(&buffer, &format!("╭─ {title}"));
            buffer[(col_of(&buffer, y, &format!("╭─ {title}")), y)].fg
        };
        assert_eq!(edge_color(&mut app, "Sessions"), t.accent, "the list starts focused");
        assert_eq!(edge_color(&mut app, "Conversation"), t.detail);
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(edge_color(&mut app, "Sessions"), t.edge, "dimmed when the preview has focus");
        assert_eq!(edge_color(&mut app, "Conversation"), t.accent);
        // The list's bottom border closes the box.
        let buffer = render(&mut app, 130, 30);
        assert!(lines(&buffer).iter().any(|l| l.starts_with("╰─")));
    }

    #[test]
    fn badges_use_showagent_colors() {
        let mut app = app();
        let buffer = render(&mut app, 100, 30);
        for (title, agent) in [("Review rate limiter", AgentKind::Codex), ("Landing page", AgentKind::Claude)] {
            let y = row_of(&buffer, title);
            let name = agent.id().to_uppercase();
            let x = col_of(&buffer, y, &name);
            let badge = Theme::dark().badge(agent);
            assert_eq!(buffer[(x, y)].bg, badge.bg.unwrap(), "{title}");
            assert_eq!(buffer[(x, y)].fg, badge.fg.unwrap());
            assert_eq!(buffer[(x - 1, y)].bg, badge.bg.unwrap(), "padded on the left");
        }
        assert_eq!(Theme::dark().badge(AgentKind::Claude).bg, Some(Color::Rgb(0xD2, 0xA8, 0xFF)));
        // Narrow terminals use short badges that still tell the agents apart.
        let narrow = screen(&mut app, 24, 12);
        assert!(narrow.contains(" CX ") && narrow.contains(" CL "), "{narrow}");
    }

    #[test]
    fn selected_row_is_one_highlight() {
        let mut app = app();
        let buffer = render(&mut app, 100, 30);
        let t = Theme::dark();
        let y = row_of(&buffer, "Rate limiting for API");
        for x in [1, 3, 20, 98] {
            assert_eq!(buffer[(x, y)].bg, t.selection, "x={x}");
        }
        assert_eq!(buffer[(20, y)].fg, Color::Rgb(255, 255, 255));
        let other = row_of(&buffer, "Landing page");
        assert_ne!(buffer[(20, other)].bg, t.selection);
        // While the preview has focus, the list selection is quieter.
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let buffer = render(&mut app, 130, 30);
        let y = row_of(&buffer, "1h ago");
        assert_eq!(buffer[(20, y)].bg, t.chip);
    }

    #[test]
    fn armed_delete_is_a_danger_row_with_a_warning() {
        let mut app = app();
        app.handle_key(ch('d'));
        let buffer = render(&mut app, 100, 30);
        let y = row_of(&buffer, "Rate limiting for API");
        assert_eq!(buffer[(4, y)].bg, Theme::dark().danger);
        assert_eq!(buffer[(20, y)].bg, Theme::dark().danger);
        let status = row_of(&buffer, "y to confirm");
        assert_eq!(buffer[(2, status)].fg, Theme::dark().warn);
    }

    #[test]
    fn light_theme_uses_its_own_colors() {
        let mut app = app();
        app.theme = Theme::light();
        let buffer = render(&mut app, 100, 30);
        let t = Theme::light();
        assert_eq!(buffer[(0, 0)].fg, t.brand, "title");
        assert_eq!(buffer[(2, row_of(&buffer, "AGENT"))].bg, t.header_bg);
        let y = row_of(&buffer, "Landing page");
        assert_eq!(buffer[(col_of(&buffer, y, "3d ago"), y)].fg, t.muted, "dates");
        assert_eq!(buffer[(col_of(&buffer, y, "Landing"), y)].fg, t.text, "titles");
        assert_eq!(buffer[(2, row_of(&buffer, "▾ ~/site"))].fg, t.group, "group headers");
        assert_eq!(buffer[(3, y)].bg, t.badge(AgentKind::Claude).bg.unwrap());
        assert_ne!(t.badge(AgentKind::Claude), Theme::dark().badge(AgentKind::Claude));
    }

    #[test]
    fn switching_themes_recolors_the_transcript() {
        let mut app = app();
        let session = app.preview_request(true).unwrap();
        let turns = vec![Turn { role: Role::User, text: "hello".into(), timestamp: Some(0) }];
        app.on_msg(Msg::Preview(PreviewKey::of(&session), Ok(Arc::new(turns))));
        let color_of_heading = |app: &mut App| {
            let buffer = render(app, 130, 30);
            let y = row_of(&buffer, "▍You");
            buffer[(col_of(&buffer, y, "▍You"), y)].fg
        };
        assert_eq!(color_of_heading(&mut app), Theme::dark().accent);
        app.handle_key(ch('t'));
        assert_eq!(app.theme.mode, crate::tui::theme::Mode::Light);
        assert!(app.status.as_ref().unwrap().text.contains("light theme"));
        assert_eq!(color_of_heading(&mut app), Theme::light().accent, "not the cached dark lines");
        app.handle_key(ch('t'));
        assert_eq!(color_of_heading(&mut app), Theme::dark().accent);
    }

    #[test]
    fn keys_box_lists_every_group_with_live_values() {
        let mut app = app();
        let screen = screen(&mut app, 120, 30);
        for text in ["Keys", "SESSION", "VIEW", "FIND", "MORE", "enter resume", "d     delete", "p   preview on"] {
            assert!(screen.contains(text), "{text:?}\n{screen}");
        }
        assert!(screen.contains("t theme dark") && screen.contains("? hide keys") && screen.contains("q quit"));
        app.handle_key(ch('p'));
        assert!(self::screen(&mut app, 120, 30).contains("p   preview off"), "values follow the state");
        // While the preview has focus, the VIEW group shows scrolling keys.
        app.handle_key(ch('p'));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(self::screen(&mut app, 120, 30).contains("j/k scroll"));
    }

    #[test]
    fn keys_box_colors() {
        let mut app = app();
        let buffer = render(&mut app, 120, 30);
        let t = Theme::dark();
        let y = row_of(&buffer, "SESSION");
        assert_eq!(buffer[(col_of(&buffer, y, "SESSION"), y)].fg, t.group);
        let y = row_of(&buffer, "enter resume");
        assert_eq!(buffer[(col_of(&buffer, y, "enter"), y)].fg, t.accent);
        assert_eq!(buffer[(col_of(&buffer, y, "resume"), y)].fg, t.text);
        let y = row_of(&buffer, "preview on");
        assert_eq!(buffer[(col_of(&buffer, y, " on") + 1, y)].fg, t.value);
        let top = row_of(&buffer, "╭─ Keys");
        assert_eq!(buffer[(0, top)].fg, t.edge);
    }

    #[test]
    fn question_mark_hides_the_keys_box_and_small_terminals_get_a_hint_bar() {
        let mut app = app();
        render(&mut app, 100, 30);
        assert!(app.keys_fit);
        app.handle_key(ch('?'));
        let hidden = screen(&mut app, 100, 30);
        assert!(!hidden.contains("SESSION"), "{hidden}");
        assert!(hidden.lines().last().unwrap().contains("? keys"));
        app.handle_key(ch('?'));
        assert!(screen(&mut app, 100, 30).contains("SESSION"));
        // No room: a one-line hint bar.
        let small = screen(&mut app, 100, 16);
        assert!(!small.contains("SESSION"));
        assert!(small.lines().last().unwrap().contains("↵ resume"));
        assert!(!screen(&mut app, 70, 30).contains("SESSION"), "too narrow for four columns");
    }

    #[test]
    fn search_line_and_flat_results() {
        let mut app = app();
        app.handle_key(ch('/'));
        for c in "rate".chars() {
            app.handle_key(ch(c));
        }
        let screen = screen(&mut app, 100, 30);
        assert!(screen.contains(" / rate▏   2 matches"), "{screen}");
        assert!(screen.contains("2 of 3 sessions"), "the title line counts matches");
        assert!(!screen.contains("▾ ~/app"), "results are flat while searching");
        assert!(screen.contains("WORKSPACE"), "flat lists get a workspace column");
        assert!(screen.contains("~/app"));
        assert!(!screen.contains("Landing page"));
    }

    #[test]
    fn session_box_shows_details_and_transcript() {
        let mut app = app();
        let session = app.preview_request(true).unwrap();
        let turns = vec![
            Turn { role: Role::User, text: "please add rate limiting".into(), timestamp: None },
            Turn { role: Role::Assistant, text: "Adding a token bucket.".into(), timestamp: None },
            Turn { role: Role::Tool, text: "Bash: cargo test".into(), timestamp: None },
        ];
        app.on_msg(Msg::Preview(PreviewKey::of(&session), Ok(Arc::new(turns))));
        let buffer = render(&mut app, 130, 30);
        let screen = lines(&buffer).join("\n");
        assert!(screen.contains("╭─ Conversation ─"), "{screen}");
        assert!(screen.contains("workspace  ~/app · ⎇ main"), "{screen}");
        assert!(screen.contains("session     CLAUDE  a1"));
        assert!(screen.contains("command    claude --resume a1"));
        assert!(screen.contains("▍You"));
        assert!(screen.contains("please add rate limiting"));
        assert!(screen.contains("▍Claude"));
        assert!(screen.contains("⚙ Bash: cargo test"));
        let t = Theme::dark();
        let y = row_of(&buffer, "workspace  ~/app");
        assert_eq!(buffer[(col_of(&buffer, y, "workspace"), y)].fg, t.value, "labels");
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let buffer = render(&mut app, 130, 30);
        let top = row_of(&buffer, "╭─ Conversation ─");
        assert_eq!(
            buffer[(col_of(&buffer, top, "╭─ Conversation ─"), top)].fg,
            t.detail,
            "green border when unfocused"
        );
    }

    #[test]
    fn preview_is_beside_on_wide_and_below_on_narrow_terminals() {
        let mut app = app();
        let wide = render(&mut app, 130, 30);
        let y = row_of(&wide, "Rate limiting for API");
        assert!(lines(&wide)[y as usize].contains("│"), "the session box is on the same rows");
        let narrow = render(&mut app, 80, 30);
        let list_row = row_of(&narrow, "Landing page");
        let preview_row = row_of(&narrow, "╭─ Conversation ─");
        assert!(preview_row > list_row, "the session box is stacked below the list");
        let short = screen(&mut app, 80, 12);
        assert!(!short.contains("╭─ Conversation ─"), "no room for a stacked preview");
    }

    #[test]
    fn long_transcripts_scroll() {
        let mut app = app();
        let session = app.preview_request(true).unwrap();
        let turns: Vec<Turn> =
            (0..100).map(|i| Turn { role: Role::User, text: format!("message number {i}"), timestamp: None }).collect();
        app.on_msg(Msg::Preview(PreviewKey::of(&session), Ok(Arc::new(turns))));
        let first = screen(&mut app, 130, 30);
        assert!(first.contains("message number 0") && first.contains(" 0% "), "{first}");
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_key(ch('G'));
        let last = screen(&mut app, 130, 30);
        assert!(last.contains("message number 99"), "{last}");
        assert!(last.contains(" 100% "));
        assert!(!last.contains("message number 0 "));
    }

    #[test]
    fn zoom_shows_only_the_transcript() {
        let mut app = app();
        app.handle_key(ch('z'));
        let screen = screen(&mut app, 100, 30);
        assert!(screen.contains("╭─ Conversation ─"));
        assert!(!screen.contains("▾ ~/site"), "the list is hidden");
        assert!(screen.contains("j/k scroll"));
    }

    #[test]
    fn group_selection_shows_a_summary() {
        let mut app = app();
        app.handle_key(ch('g'));
        let screen = screen(&mut app, 130, 30);
        assert!(screen.contains("2 sessions"), "{screen}");
        assert!(screen.contains("space or enter to collapse"));
        assert!(screen.contains("enter open/close"), "the keys box says what enter does here");
    }

    #[test]
    fn empty_states() {
        let mut empty = App::new(vec![], "/h".into(), 0, Box::new(|_| String::new()));
        empty.scanning = true;
        assert!(screen(&mut empty, 80, 20).contains("scanning sessions…"));
        empty.scanning = false;
        assert!(screen(&mut empty, 80, 20).contains("No sessions found"));

        let mut app = app();
        app.handle_key(ch('/'));
        app.handle_key(ch('#'));
        assert!(screen(&mut app, 80, 20).contains("No sessions match \"#\""));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_key(ch('1'));
        app.handle_key(ch('2'));
        assert!(screen(&mut app, 80, 20).contains("All agents are hidden"));
    }

    #[test]
    fn help_overlay() {
        let mut app = app();
        app.handle_key(ch('?'));
        let screen = screen(&mut app, 100, 30);
        assert!(screen.contains("Keys · esc closes"));
        assert!(screen.contains("move session to the trash"));
    }

    #[test]
    fn scanning_indicator_and_status() {
        let mut app = app();
        app.scanning = true;
        app.error("delete failed: boom");
        let buffer = render(&mut app, 100, 30);
        assert!(lines(&buffer)[0].contains("· scanning…"));
        let y = row_of(&buffer, "delete failed: boom");
        assert_eq!(buffer[(2, y)].fg, Theme::dark().error);
        assert!(lines(&buffer)[(y + 1) as usize].contains("╭─ Keys"), "status sits above the keys box");
        // Without the keys box, the status replaces the hint bar.
        assert!(lines(&render(&mut app, 100, 16)).last().unwrap().contains("delete failed: boom"));
    }

    #[test]
    fn tiny_and_odd_sizes_do_not_panic() {
        let mut app = app();
        for (w, h) in [(1, 1), (10, 3), (23, 4), (24, 5), (30, 6), (200, 60), (119, 19), (120, 20), (80, 17), (80, 16)]
        {
            render(&mut app, w, h);
        }
        app.handle_key(ch('z'));
        render(&mut app, 24, 5);
        app.handle_key(ch('?'));
        render(&mut app, 24, 5);
    }

    #[test]
    fn list_scrolls_to_keep_cursor_visible() {
        let many: Vec<Session> = (0..200)
            .map(|i| session(AgentKind::Claude, &format!("id{i:03}"), "/w", NOW - i, &format!("session number {i}")))
            .collect();
        let mut app = App::new(many, "/h".into(), NOW, Box::new(|_| String::new()));
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        let screen = screen(&mut app, 80, 12);
        assert!(screen.contains("session number 199"), "{screen}");
        assert!(!screen.contains("session number 0 "));
    }

    #[test]
    fn transcript_lines_wrap_to_width() {
        let turns = vec![Turn { role: Role::Assistant, text: "word ".repeat(40), timestamp: Some(0) }];
        let lines = transcript_lines(&turns, AgentKind::Codex, 30, Theme::dark());
        assert!(lines.len() > 5);
        for line in &lines {
            assert!(line.width() <= 30, "{line:?}");
        }
        assert_eq!(transcript_lines(&[], AgentKind::Claude, 30, Theme::dark()).len(), 1);
    }

    #[test]
    fn big_transcripts_are_wrapped_off_the_draw_path() {
        let mut app = app();
        let session = app.preview_request(true).unwrap();
        let turns: Vec<Turn> = (0..400)
            .map(|i| Turn { role: Role::Assistant, text: format!("{i} {}", "word ".repeat(400)), timestamp: None })
            .collect();
        assert!(turns_bytes(&turns) > INLINE_WRAP_BYTES);
        app.on_msg(Msg::Preview(PreviewKey::of(&session), Ok(Arc::new(turns.clone()))));
        let first = screen(&mut app, 130, 30);
        assert!(first.contains("formatting…"), "{first}");
        let request = app.wrap_wanted.take().expect("a background wrap was requested");
        assert_eq!(request.key.width, 61, "box width minus borders and padding");
        // What the worker would send back:
        let lines = transcript_lines(&request.turns, request.agent, request.key.width as usize, request.theme);
        app.on_msg(Msg::Wrapped(request.key.clone(), Ok(Arc::new(lines))));
        let ready = screen(&mut app, 130, 30);
        assert!(ready.contains("0 word word"), "{ready}");
        // A resize shows the old wrap while the new width is prepared.
        let resized = screen(&mut app, 140, 30);
        assert!(resized.contains("0 word word"));
        assert_eq!(app.wrap_wanted.take().unwrap().key.width, 66);
        assert!(screen(&mut app, 130, 30).contains("0 word word"), "back to a cached width");
        assert!(app.wrap_wanted.is_none());
    }

    #[test]
    fn hidden_preview_is_reported_to_the_app() {
        let mut app = app();
        render(&mut app, 80, 12);
        assert!(!app.preview_visible);
        render(&mut app, 80, 30);
        assert!(app.preview_visible);
        render(&mut app, 130, 12);
        assert!(app.preview_visible, "side by side fits at any height");
    }

    #[test]
    fn narrow_layouts_keep_titles_visible() {
        for width in [24, 30, 33, 40, 60] {
            for (label, keys) in [("grouped", ""), ("flat", "w"), ("search", "/rate")] {
                let mut app = app();
                for c in keys.chars() {
                    app.handle_key(ch(c));
                }
                let screen = screen(&mut app, width, 12);
                assert!(
                    screen.contains("Rate li") || screen.contains("Review"),
                    "{label} at {width}: no title visible\n{screen}"
                );
            }
        }
    }

    #[test]
    fn columns_follow_showagent() {
        assert_eq!(columns(100, false, false), Columns { agent: 8, updated: 8, workspace: 0, title: 80 });
        assert_eq!(columns(100, true, false), Columns { agent: 8, updated: 8, workspace: 32, title: 47 });
        assert_eq!(columns(24, true, false), Columns { agent: 4, updated: 5, workspace: 0, title: 11 });
        for width in 24..220 {
            for with_workspace in [false, true] {
                let c = columns(width, with_workspace, width % 2 == 0);
                let used =
                    GUTTER + c.agent + 1 + c.updated + 1 + c.title + if c.workspace > 0 { c.workspace + 1 } else { 0 };
                assert_eq!(used, width, "{width} {with_workspace}: {c:?}");
                assert!(c.title >= 10, "{width} {with_workspace}: {c:?}");
            }
        }
    }

    #[test]
    fn every_help_row_is_reachable_on_short_terminals() {
        for (w, h) in [(80, 12), (80, 20), (40, 8)] {
            let mut app = app();
            app.handle_key(ch('?'));
            let first = screen(&mut app, w, h);
            assert!(first.contains("Keys · j/k scroll"), "{w}x{h}: {first}");
            app.handle_key(ch('G'));
            let last = screen(&mut app, w, h);
            assert!(last.contains("quit"), "{w}x{h}: last row reachable\n{last}");
            assert!(app.show_help);
        }
        let mut tall = app();
        tall.handle_key(ch('?'));
        assert!(!screen(&mut tall, 100, 30).contains("Keys · j/k scroll"), "no scrolling needed");
    }

    #[test]
    fn long_queries_show_their_end_and_the_count() {
        let mut app = app();
        app.handle_key(ch('/'));
        for c in "a very long query that is wider than the screen rate".chars() {
            app.handle_key(ch(c));
        }
        let screen = screen(&mut app, 40, 12);
        let line = screen.lines().nth(1).unwrap().to_string();
        assert!(line.contains("rate▏"), "the edit position is visible: {line:?}");
        assert!(line.contains("0 matches"), "{line:?}");
        let mut cjk = self::app();
        cjk.handle_key(ch('/'));
        for c in "日本語のとても長い検索クエリです".chars() {
            cjk.handle_key(ch(c));
        }
        let buffer = render(&mut cjk, 30, 12);
        assert!(lines(&buffer)[1].replace(' ', "").contains("です▏"), "{:?}", lines(&buffer)[1]);
    }

    #[test]
    fn widths_are_measured_in_cells_not_bytes() {
        let mut app = app();
        app.handle_key(ch('g')); // a group header
        let buffer = render(&mut app, 80, 30);
        let y = row_of(&buffer, "▾ ~/app");
        let highlighted = (0..80).filter(|&x| buffer[(x, y)].bg == Theme::dark().selection).count();
        assert_eq!(highlighted, 78, "the whole header row inside the box is highlighted");
        // Hints with multi-byte keys use their display width.
        app.handle_key(ch('/'));
        let footer = lines(&render(&mut app, 50, 12)).last().unwrap().clone();
        assert!(footer.contains("↑↓ move"), "{footer:?}");
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(10), "10 B");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(human_size(3 * 1024 * 1024 / 2), "1.5 MB");
    }
}
