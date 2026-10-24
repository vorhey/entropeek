//! Load tests. Ignored by default; run with:
//!
//!     cargo test --release -- --ignored --nocapture
//!
//! Each operation is timed individually and the worst case is checked, not
//! just the average: a UI that is fast on average but stalls once in a while
//! still feels slow. Limits are generous so slow CI machines pass; the
//! printed numbers are what to compare. The event-loop latency test (real
//! loop, real workers, a 20 MB transcript) lives in `src/tui/mod.rs`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use entropeek::cache::Cache;
use entropeek::env::Env;
use entropeek::model::{AgentKind, Role, Session, Turn};
use entropeek::tui::app::{App, Msg, PreviewKey};
use entropeek::tui::view;
use entropeek::{index, provider, testkit};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn sessions(count: usize) -> Vec<Session> {
    (0..count)
        .map(|i| Session {
            agent: if i % 5 == 0 { AgentKind::Codex } else { AgentKind::Claude },
            id: format!("{i:08x}-0000-4000-8000-000000000000"),
            path: format!("/sessions/{i}.jsonl").into(),
            cwd: format!("/home/u/project-{}", i % 50),
            title: Some(format!("Session {i} about refactoring the parser and adding tests")),
            first_prompt: Some(format!("please help with task number {i}")),
            last_prompt: Some("thanks, looks good".into()),
            started_at: Some(0),
            updated_at: 10_000_000 - i as i64,
            mtime_ns: 0,
            git_branch: Some("main".into()),
            user_turns: 5,
            size: 100_000,
        })
        .collect()
}

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

struct Timing {
    median: Duration,
    p99: Duration,
    max: Duration,
}

/// Times every call of `f` separately.
fn time<F: FnMut()>(label: &str, iterations: usize, mut f: F) -> Timing {
    let mut samples: Vec<Duration> = (0..iterations)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .collect();
    samples.sort();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    let timing = Timing { median: at(0.5), p99: at(0.99), max: *samples.last().unwrap() };
    println!("{label:<46} median {:>9.2?}  p99 {:>9.2?}  max {:>9.2?}", timing.median, timing.p99, timing.max);
    timing
}

/// One frame at 60 Hz for nearly every call; a few hiccups are tolerated.
fn assert_interactive(label: &str, t: &Timing) {
    assert!(t.p99 < Duration::from_millis(16), "{label}: p99 {:?}", t.p99);
    assert!(t.max < Duration::from_millis(100), "{label}: max {:?}", t.max);
}

#[test]
#[ignore]
fn tui_stays_fast_with_10k_sessions() {
    let mut app =
        App::new(sessions(10_000), "/home/u".into(), 10_000_000, Box::new(|s| format!("claude --resume {}", s.id)));
    let mut terminal = Terminal::new(TestBackend::new(160, 50)).unwrap();

    let frame = time("draw frame (10k sessions, grouped)", 300, || {
        terminal.draw(|f| view::draw(f, &mut app)).unwrap();
    });
    let step = time("key j + draw", 500, || {
        app.handle_key(key('j'));
        terminal.draw(|f| view::draw(f, &mut app)).unwrap();
    });
    app.handle_key(key('/'));
    let mut typed = 0;
    let search = time("search keystroke (filter 10k) + draw", 300, || {
        // Alternate typing and deleting so every iteration filters all sessions.
        if typed % 2 == 0 {
            app.handle_key(key('7'));
        } else {
            app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        }
        typed += 1;
        terminal.draw(|f| view::draw(f, &mut app)).unwrap();
    });
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    for (label, t) in [("frame", &frame), ("step", &step), ("search", &search)] {
        assert_interactive(label, t);
    }
}

#[test]
#[ignore]
fn big_transcripts_never_block_drawing() {
    let mut app = App::new(sessions(100), "/home/u".into(), 10_000_000, Box::new(|_| String::new()));
    let mut terminal = Terminal::new(TestBackend::new(160, 50)).unwrap();
    // 2000 turns of ~2 KB: 4 MB of text.
    let session = app.preview_request(true).unwrap();
    let turns: Vec<Turn> = (0..2000)
        .map(|i| Turn {
            role: if i % 2 == 0 { Role::User } else { Role::Assistant },
            text: "word ".repeat(400),
            timestamp: None,
        })
        .collect();
    app.on_msg(Msg::Preview(PreviewKey::of(&session), Ok(Arc::new(turns))));

    // While the worker would be wrapping, frames (including resizes) stay fast.
    let mut width = 160;
    let pending = time("draw / resize while a 4 MB wrap is pending", 100, || {
        width = if width == 160 { 150 } else { 160 };
        terminal.resize(ratatui::layout::Rect::new(0, 0, width, 50)).unwrap();
        terminal.draw(|f| view::draw(f, &mut app)).unwrap();
    });
    assert_interactive("pending wrap", &pending);

    // The worker's share, off the input thread.
    let request = app.wrap_wanted.take().expect("a background wrap was requested");
    let started = Instant::now();
    let lines = view::transcript_lines(&request.turns, request.agent, request.key.width as usize, request.theme);
    let wrap_time = started.elapsed();
    println!("{:<46} {wrap_time:>9.2?} ({} lines, on a worker)", "wrap 4 MB transcript", lines.len());
    assert!(wrap_time < Duration::from_secs(2), "wrapping took {wrap_time:?}");

    app.on_msg(Msg::Wrapped(request.key, Ok(Arc::new(lines))));
    app.focus = entropeek::tui::app::Focus::Preview;
    let scroll = time("scroll preview + draw (wrapped)", 500, || {
        app.handle_key(key('j'));
        terminal.draw(|f| view::draw(f, &mut app)).unwrap();
    });
    assert_interactive("scroll", &scroll);
}

#[test]
#[ignore]
fn scan_2000_session_files() {
    let dir = tempfile::tempdir().unwrap();
    let env = Env::under(dir.path());
    testkit::synthetic(&env, 2000, 8);
    let providers = provider::all(&env);
    let mut cache = Cache::in_memory();
    // "Cold" means no metadata cache; the files were just written, so the
    // OS page cache is warm. Disk-cold numbers depend on the machine.
    let cold = time("scan 2000 files, no metadata cache", 1, || {
        let scan = index::scan(&providers, &mut cache);
        assert_eq!(scan.sessions.len(), 2000);
    });
    let warm = time("scan 2000 files, warm metadata cache", 10, || {
        let scan = index::scan(&providers, &mut cache);
        assert_eq!(scan.stats.parsed, 0);
    });
    assert!(cold.max < Duration::from_secs(5), "cold scan took {:?}", cold.max);
    assert!(warm.max < Duration::from_millis(250), "warm scan took {:?}", warm.max);
    assert!(warm.median < cold.median);
}
