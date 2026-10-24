//! Interactive terminal UI: terminal setup, the event loop, and effects.
//!
//! The loop redraws only after something changed (a key, a resize, or a job
//! result) and never touches the disk itself: scans, transcript loads and
//! wrapping, resume checks, deletes and restores run on [`jobs::Jobs`]
//! workers and report back as [`Msg`]s. Input is handled in bounded batches,
//! so a flood of keys or a long paste never holds up drawing or results.

pub mod app;
pub mod icons;
pub mod jobs;
pub mod terminal;
pub mod theme;
pub mod view;

use std::io::{self, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::{cursor, execute, terminal as tty};

use crate::cache::Cache;
use crate::cli::{Outcome, Runtime};
use crate::model::Session;
use crate::provider;
use crate::timefmt;
use crate::trash::Trash;
use app::{App, Effect, Msg, PreviewKey, SearchEdit};
use jobs::{CacheLoader, Jobs};

/// How long the cursor must rest on a session before its transcript loads.
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(40);
/// How often to check for job results while jobs are running.
const JOB_POLL: Duration = Duration::from_millis(16);
/// Redraw at least this often so relative times stay fresh.
const CLOCK_REFRESH: Duration = Duration::from_secs(30);
/// Longest stretch spent handling queued input before drawing again.
const INPUT_BUDGET: Duration = Duration::from_millis(12);
/// Most queued events handled before drawing again.
const INPUT_BATCH: usize = 512;
/// How long to wait for the terminal to answer the startup query. Most
/// answer within a few milliseconds, and the last question is one every
/// terminal answers, so this is rarely waited out.
const TERMINAL_QUERY_TIMEOUT: Duration = Duration::from_millis(150);

pub fn run(rt: &Runtime) -> Result<Outcome> {
    let providers = rt.providers.clone();
    let for_resume = providers.clone();
    let resume_line = Box::new(move |s: &Session| {
        provider::find(&for_resume, s.agent).map(|p| p.resume_command(s).shell_line()).unwrap_or_default()
    });
    // The list starts empty; the scan worker loads the cache from disk and
    // sends what it knows first, so the first frame never waits on disk.
    let mut app = App::new(Vec::new(), rt.env.home_str(), timefmt::now_ms(), resume_line);
    app.scanning = true;

    let (tx, rx) = mpsc::channel();
    let loader: CacheLoader = if rt.no_cache {
        Box::new(Cache::in_memory)
    } else {
        let file = Cache::default_file(&rt.env.cache_dir);
        Box::new(move || Cache::load(&file))
    };
    let mut jobs = Jobs::new(tx, providers, loader, Trash::new(rt.env.trash_dir()))?;
    jobs.scan();

    let guard = TerminalGuard::new(CrosstermOps)?;
    install_panic_hook();
    // One round trip to the terminal (background color, name and version,
    // kitty's font), before anything else reads input.
    let auto = rt.theme == theme::ThemeChoice::Auto || rt.icons == icons::IconChoice::Auto;
    let reply = if auto { terminal::query(TERMINAL_QUERY_TIMEOUT) } else { terminal::Reply::default() };
    let colorfgbg = std::env::var("COLORFGBG").ok();
    let mode = theme::resolve(rt.theme, || reply.background, colorfgbg.as_deref());
    let detection = rt.icons.resolve(|| {
        let env = |key: &str| std::env::var(key).ok();
        let read = |path: &std::path::Path| std::fs::read_to_string(path).ok();
        icons::detect(&icons::Facts {
            env: &env,
            reply: &reply,
            tmux_client: &icons::tmux_client,
            read: &read,
            home: &rt.env.home,
        })
    });
    app.theme = theme::Theme::for_mode(mode).with_icons(detection.icons);
    app.icons_reason = detection.reason;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = event_loop(&mut terminal, &mut app, &jobs, &rx, &mut Crossterm);
    if jobs.mutating() {
        app.info("finishing pending changes…");
        let _ = terminal.draw(|frame| view::draw(frame, &mut app));
    }
    // Deletes and restores always complete before entropeek exits or execs.
    jobs.finish();
    drop(terminal);
    drop(guard);
    result
}

/// The steps of taking over the terminal and giving it back.
pub trait TerminalOps {
    fn enable_raw(&mut self) -> io::Result<()>;
    fn enter_alternate_screen(&mut self) -> io::Result<()>;
    fn leave_alternate_screen(&mut self) -> io::Result<()>;
    fn disable_raw(&mut self) -> io::Result<()>;
}

/// Owns the terminal setup: everything that was switched on is switched off
/// when the guard drops, including when setup itself fails halfway.
pub struct TerminalGuard<O: TerminalOps> {
    ops: O,
    raw: bool,
    alternate: bool,
}

impl<O: TerminalOps> TerminalGuard<O> {
    pub fn new(ops: O) -> io::Result<TerminalGuard<O>> {
        let mut guard = TerminalGuard { ops, raw: false, alternate: false };
        guard.ops.enable_raw()?;
        guard.raw = true;
        guard.ops.enter_alternate_screen()?;
        guard.alternate = true;
        Ok(guard)
    }
}

impl<O: TerminalOps> Drop for TerminalGuard<O> {
    fn drop(&mut self) {
        if self.alternate {
            let _ = self.ops.leave_alternate_screen();
        }
        if self.raw {
            let _ = self.ops.disable_raw();
        }
    }
}

struct CrosstermOps;

impl TerminalOps for CrosstermOps {
    fn enable_raw(&mut self) -> io::Result<()> {
        tty::enable_raw_mode()
    }

    fn enter_alternate_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), tty::EnterAlternateScreen)
    }

    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), tty::LeaveAlternateScreen, cursor::Show)
    }

    fn disable_raw(&mut self) -> io::Result<()> {
        tty::disable_raw_mode()
    }
}

/// A panic on the UI thread restores the terminal before the message is
/// printed. A panic on a worker is caught there and shown in the status
/// line, so it must not tear down the screen or print over it.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if thread::current().name().is_some_and(|name| name.starts_with(jobs::WORKER_PREFIX)) {
            return;
        }
        let mut ops = CrosstermOps;
        let _ = ops.leave_alternate_screen();
        let _ = ops.disable_raw();
        previous(info);
    }));
}

/// Where terminal events come from. Real runs use crossterm; tests script it.
pub trait EventSource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<Event>;
}

struct Crossterm;

impl EventSource for Crossterm {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
        event::read()
    }
}

fn event_loop<B>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    jobs: &Jobs,
    rx: &mpsc::Receiver<Msg>,
    events: &mut impl EventSource,
) -> Result<Outcome>
where
    B: Backend,
    B::Error: Send + Sync + 'static,
{
    let mut dirty = true;
    let mut last_draw = Instant::now();
    let mut selected = app.selected_session().map(PreviewKey::of);
    let mut selected_since = Instant::now();

    loop {
        // Check for running jobs *before* draining results. A job sends its
        // result before it stops counting as busy, so either we see it busy
        // (and poll again soon) or its result is already in the channel.
        // Checking after draining could miss a result and sleep on it.
        let mut busy = jobs.busy();
        while let Ok(msg) = rx.try_recv() {
            dirty = true;
            if let Some(effect) = app.on_msg(msg)
                && let Some(outcome) = apply(effect, app, jobs)
            {
                return Ok(outcome);
            }
        }

        let now_selected = app.selected_session().map(PreviewKey::of);
        if now_selected != selected {
            selected = now_selected;
            selected_since = Instant::now();
        }
        let rested = selected_since.elapsed() >= PREVIEW_DEBOUNCE;
        if let Some(session) = app.preview_request(rested) {
            jobs.preview(session);
            busy = true;
            dirty = true;
        }

        if dirty || last_draw.elapsed() >= CLOCK_REFRESH {
            app.now = timefmt::now_ms();
            terminal.draw(|frame| view::draw(frame, app))?;
            last_draw = Instant::now();
            dirty = false;
        }
        // Drawing may have found a transcript too big to wrap inline.
        if let Some(request) = app.wrap_wanted.take() {
            jobs.wrap(request);
            busy = true;
        }

        let timeout = if app.preview_pending() && !rested {
            PREVIEW_DEBOUNCE.saturating_sub(selected_since.elapsed())
        } else if busy {
            JOB_POLL
        } else {
            CLOCK_REFRESH.saturating_sub(last_draw.elapsed())
        };
        if !events.poll(timeout)? {
            continue;
        }
        // Handle what is already queued, but get back to drawing and job
        // results within a frame. Consecutive search edits are applied as
        // one, so a paste costs one filter pass.
        let started = Instant::now();
        let mut edits: Vec<SearchEdit> = Vec::new();
        let mut handled = 0;
        loop {
            handled += 1;
            match events.read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    dirty = true;
                    match SearchEdit::from_key(&key).filter(|_| app.searching) {
                        Some(edit) => edits.push(edit),
                        None => {
                            if !edits.is_empty() {
                                app.edit_search(&std::mem::take(&mut edits));
                            }
                            if let Some(effect) = app.handle_key(key)
                                && let Some(outcome) = apply(effect, app, jobs)
                            {
                                return Ok(outcome);
                            }
                        }
                    }
                }
                Event::Paste(text) if app.searching => {
                    dirty = true;
                    edits.extend(text.chars().filter(|c| !c.is_control()).map(SearchEdit::Insert));
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
            if handled >= INPUT_BATCH || started.elapsed() >= INPUT_BUDGET || !events.poll(Duration::ZERO)? {
                break;
            }
        }
        if !edits.is_empty() {
            app.edit_search(&edits);
        }
    }
}

/// Carries out an effect. Returns an outcome when the UI should exit.
fn apply(effect: Effect, app: &mut App, jobs: &Jobs) -> Option<Outcome> {
    match effect {
        Effect::Quit => return Some(Outcome::Done),
        // Checking the workspace and program touches the disk: a worker
        // does it and answers with `Effect::Exec` or an error.
        Effect::Resume(session) => jobs.check_resume(session),
        Effect::Exec(command) => return Some(Outcome::Exec(command)),
        Effect::Delete(session) => jobs.delete(session),
        Effect::Restore(key) => jobs.restore(key),
        Effect::Rescan => {
            app.scanning = true;
            jobs.scan();
        }
        Effect::CopyResume(session) => {
            let line = app.resume_line(&session);
            match copy_to_clipboard(&line) {
                Ok(()) => app.info(format!("copied: {line}")),
                Err(err) => app.error(format!("copy failed: {err}")),
            }
        }
    }
    None
}

/// Copies through the terminal with an OSC 52 escape sequence, which works
/// over SSH and in tmux (with `set-clipboard on`).
fn copy_to_clipboard(text: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Env;
    use crate::index;
    use crate::testkit;
    use jobs::Providers;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::Arc;

    #[test]
    fn base64_matches_known_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
            ("cd ~ && x", "Y2QgfiAmJiB4"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected, "{input:?}");
        }
    }

    /// Records terminal setup steps and fails the chosen one.
    struct FakeOps {
        log: Rc<RefCell<Vec<&'static str>>>,
        fail: Option<&'static str>,
    }

    impl FakeOps {
        fn step(&mut self, name: &'static str) -> io::Result<()> {
            self.log.borrow_mut().push(name);
            if self.fail == Some(name) { Err(io::Error::other("injected")) } else { Ok(()) }
        }
    }

    impl TerminalOps for FakeOps {
        fn enable_raw(&mut self) -> io::Result<()> {
            self.step("raw on")
        }
        fn enter_alternate_screen(&mut self) -> io::Result<()> {
            self.step("alt on")
        }
        fn leave_alternate_screen(&mut self) -> io::Result<()> {
            self.step("alt off")
        }
        fn disable_raw(&mut self) -> io::Result<()> {
            self.step("raw off")
        }
    }

    #[test]
    fn terminal_is_restored_after_every_failure_point() {
        let run = |fail: Option<&'static str>, later_error: bool| {
            let log = Rc::new(RefCell::new(Vec::new()));
            let result = TerminalGuard::new(FakeOps { log: log.clone(), fail });
            if let Ok(guard) = result {
                // An error after setup (e.g. creating the Terminal) drops the guard too.
                assert!(later_error);
                drop(guard);
            }
            log.borrow().clone()
        };
        assert_eq!(run(Some("raw on"), false), vec!["raw on"]);
        assert_eq!(run(Some("alt on"), false), vec!["raw on", "alt on", "raw off"]);
        assert_eq!(run(None, true), vec!["raw on", "alt on", "alt off", "raw off"]);
    }

    /// Plays scripted events like a user who waits for the UI to settle: the
    /// next event only arrives when the loop is idle (willing to wait a
    /// second or more). After the script it sends `q`.
    struct Script {
        events: VecDeque<Event>,
        polls: usize,
        /// Deliver everything at once, like a paste or a flood of keys.
        burst: bool,
        outer_polls: usize,
    }

    impl Script {
        fn new(keys: &str) -> Script {
            Script::events(keys.chars().map(|c| Event::Key(key(c))).collect())
        }

        fn events(events: VecDeque<Event>) -> Script {
            Script { events, polls: 0, burst: false, outer_polls: 0 }
        }
    }

    fn key(c: char) -> KeyEvent {
        match c {
            '\n' => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            '\t' => KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            c => KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
        }
    }

    impl EventSource for Script {
        fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
            self.polls += 1;
            if self.polls > 200_000 {
                panic!("event loop spun without settling");
            }
            if !timeout.is_zero() {
                self.outer_polls += 1;
            }
            if self.burst && !self.events.is_empty() {
                return Ok(true);
            }
            if timeout.is_zero() {
                return Ok(false); // nothing else queued in this burst
            }
            if timeout < Duration::from_secs(1) {
                // The loop is waiting on work; let a little time pass.
                std::thread::sleep(timeout.min(Duration::from_millis(5)));
                return Ok(false);
            }
            if self.events.is_empty() {
                self.events.push_back(Event::Key(key('q')));
            }
            Ok(true)
        }

        fn read(&mut self) -> io::Result<Event> {
            Ok(self.events.pop_front().expect("poll said an event was ready"))
        }
    }

    fn loop_fixture() -> (tempfile::TempDir, Env, App, Jobs, mpsc::Receiver<Msg>) {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::under(dir.path());
        testkit::populate(&env);
        let providers: Providers = Arc::new(provider::all(&env));
        let mut cache = Cache::in_memory();
        let sessions = index::scan(&providers, &mut cache).sessions;
        let app = App::new(sessions, "/h".into(), 0, Box::new(|_| String::new()));
        let (tx, rx) = mpsc::channel();
        let jobs = Jobs::new(tx, providers, Box::new(move || cache), Trash::new(env.trash_dir())).unwrap();
        (dir, env, app, jobs, rx)
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn job_results_are_applied_before_the_loop_sleeps() {
        // Regression: a delete finishing between "drain results" and "any
        // jobs busy?" used to leave the result unread until the next key.
        for _ in 0..10 {
            let (_dir, _env, mut app, jobs, rx) = loop_fixture();
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            let mut script = Script::new("dy");
            let outcome = event_loop(&mut terminal, &mut app, &jobs, &rx, &mut script).unwrap();
            assert!(matches!(outcome, Outcome::Done));
            assert_eq!(app.sessions.len(), 3, "the delete was applied before the loop went idle");
            assert_eq!(app.undo.len(), 1);
        }
    }

    #[test]
    fn loop_loads_previews_and_undoes() {
        let (_dir, _env, mut app, jobs, rx) = loop_fixture();
        let mut terminal = Terminal::new(TestBackend::new(130, 30)).unwrap();
        let mut script = Script::new("jkdyu");
        event_loop(&mut terminal, &mut app, &jobs, &rx, &mut script).unwrap();
        assert_eq!(app.sessions.len(), 4, "undo restored the session");
        assert!(matches!(app.preview, app::PreviewState::Ready(..)), "the preview loaded once the cursor rested");
        assert!(screen(&terminal).contains("restored"), "the final frame shows the restore");
    }

    #[test]
    fn resume_is_checked_off_the_input_thread() {
        let (_dir, _env, mut app, jobs, rx) = loop_fixture();
        let mut terminal = Terminal::new(TestBackend::new(130, 30)).unwrap();
        // The fixture's workspaces do not exist, so the check fails.
        let mut script = Script::new("\n");
        let outcome = event_loop(&mut terminal, &mut app, &jobs, &rx, &mut script).unwrap();
        assert!(matches!(outcome, Outcome::Done));
        assert!(screen(&terminal).contains("no longer exists"), "the error was shown before quitting");
    }

    #[test]
    fn a_flood_of_input_still_draws_and_quits() {
        let (_dir, _env, mut app, jobs, rx) = loop_fixture();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut events: VecDeque<Event> =
            (0..20_000).map(|i| Event::Key(key(if i % 2 == 0 { 'j' } else { 'k' }))).collect();
        events.push_back(Event::Key(key('q')));
        let mut script = Script::events(events);
        script.burst = true;
        let outcome = event_loop(&mut terminal, &mut app, &jobs, &rx, &mut script).unwrap();
        assert!(matches!(outcome, Outcome::Done));
        assert!(script.outer_polls >= 20_000 / INPUT_BATCH, "only {} frames for 20k keys", script.outer_polls);
    }

    #[test]
    fn a_pasted_query_is_applied_in_one_go() {
        let (_dir, _env, mut app, jobs, rx) = loop_fixture();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut events: VecDeque<Event> = VecDeque::new();
        events.push_back(Event::Key(key('/')));
        events.extend("landing page".chars().map(|c| Event::Key(key(c))));
        events.push_back(Event::Paste(" responsive\u{1b}".into()));
        events.push_back(Event::Key(key('\n')));
        events.push_back(Event::Key(key('q')));
        let mut script = Script::events(events);
        script.burst = true;
        event_loop(&mut terminal, &mut app, &jobs, &rx, &mut script).unwrap();
        assert_eq!(app.query, "landing page responsive");
        assert!(!app.searching, "Enter, after the edits, ended the search");
        assert_eq!(app.match_count(), 1);
    }

    /// Delivers a key every few milliseconds whether or not the loop is
    /// busy, like a user holding a key, and records the longest time the
    /// loop spent between two polls (handling input, results and drawing).
    struct Steady {
        keys: VecDeque<Event>,
        next_at: Instant,
        returned: Option<Instant>,
        worst: Duration,
    }

    impl EventSource for Steady {
        fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
            if let Some(at) = self.returned {
                self.worst = self.worst.max(at.elapsed());
            }
            let wait = self.next_at.saturating_duration_since(Instant::now());
            let ready = wait <= timeout;
            std::thread::sleep(wait.min(timeout));
            self.returned = Some(Instant::now());
            Ok(ready)
        }

        fn read(&mut self) -> io::Result<Event> {
            self.next_at = Instant::now() + Duration::from_millis(5);
            Ok(self.keys.pop_front().unwrap_or(Event::Key(key('q'))))
        }
    }

    /// Run with `cargo test --release -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn input_stays_responsive_while_a_huge_transcript_loads() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::under(dir.path());
        let id = "dddddddd-0000-0000-0000-000000000001";
        let paragraph = "lorem ipsum dolor sit amet, consectetur adipiscing elit ".repeat(180);
        let mut lines = vec![testkit::claude::user(id, "/w", "2026-01-01T00:00:00Z", "go")];
        lines.extend((0..2000).map(|_| testkit::claude::assistant_text(id, "2026-01-01T00:00:01Z", &paragraph)));
        let file = testkit::claude::write_session(&env.claude_dir, id, "/w", &lines);
        println!("transcript file: {} MB", std::fs::metadata(&file).unwrap().len() / 1_000_000);
        let providers: Providers = Arc::new(provider::all(&env));
        let mut cache = Cache::in_memory();
        let sessions = index::scan(&providers, &mut cache).sessions;
        let mut app = App::new(sessions, "/h".into(), 0, Box::new(|_| String::new()));
        let (tx, rx) = mpsc::channel();
        let jobs = Jobs::new(tx, providers, Box::new(move || cache), Trash::new(env.trash_dir())).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(160, 50)).unwrap();

        let mut keys: VecDeque<Event> = VecDeque::new();
        keys.push_back(Event::Key(key('z')));
        keys.extend((0..600).map(|i| Event::Key(key(if i % 3 == 2 { 'k' } else { 'j' }))));
        let mut source = Steady { keys, next_at: Instant::now(), returned: None, worst: Duration::ZERO };
        let started = Instant::now();
        event_loop(&mut terminal, &mut app, &jobs, &rx, &mut source).unwrap();
        println!(
            "{:<46} {:>9.2?} over {:.1?}",
            "worst loop iteration while loading 20 MB",
            source.worst,
            started.elapsed()
        );
        assert!(app.wrapped.is_some(), "the transcript finished loading during the test");
        assert!(app.preview_scroll > 0, "keys scrolled it");
        assert!(source.worst < Duration::from_millis(50), "the loop stalled for {:?}", source.worst);
    }
}
