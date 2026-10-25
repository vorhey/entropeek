//! Background work for the TUI, on a fixed set of worker threads.
//!
//! - **Scans** run on one worker; requests made while a scan runs collapse
//!   into a single follow-up scan.
//! - **Transcript loads**, **transcript wrapping** and **resume checks** each
//!   have one worker that only runs the newest request, and skips a request
//!   identical to the one already running.
//! - **Deletes and restores** run in order on one worker, so they never race
//!   each other, and [`Jobs::finish`] waits for them before entropeek exits.
//!
//! Every worker catches panics and reports them as failed results, so the UI
//! never waits forever. Results arrive as [`Msg`]s.

use std::any::Any;
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;

use anyhow::{Context, Result};

use super::app::{Msg, PreviewKey, ScanReport, WrapRequest};
use super::view::transcript_lines;
use crate::actions;
use crate::cache::Cache;
use crate::index;
use crate::model::{Role, Session, Turn};
use crate::provider::{self, Provider};
use crate::trash::Trash;

pub type Providers = Arc<Vec<Box<dyn Provider>>>;

/// Transcripts bigger than this are cut to their newest messages in the
/// preview (`entropeek show` still prints everything).
pub const PREVIEW_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Prefix of worker thread names; the TUI's panic hook stays quiet for them.
pub const WORKER_PREFIX: &str = "entropeek-";

/// A worker thread fed by a one-slot queue (or a FIFO, for mutations).
struct Lane<T> {
    state: Mutex<LaneState<T>>,
    wake: Condvar,
}

struct LaneState<T> {
    queue: VecDeque<T>,
    running: Option<T>,
    closed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Policy {
    /// Keep only the newest request; skip one equal to the running request.
    Latest,
    /// Run every request, in order.
    Fifo,
}

impl<T: Clone + Send + 'static> Lane<T> {
    /// Starts the worker thread for a lane. Fallible: a system out of
    /// threads makes `Jobs::new` fail instead of panicking.
    fn spawn(
        name: &str,
        policy: Policy,
        same: fn(&T, &T) -> bool,
        mut run: impl FnMut(&T) -> Msg + Send + 'static,
        fail: impl Fn(&T, String) -> Msg + Send + 'static,
        tx: mpsc::Sender<Msg>,
    ) -> Result<(Handle<T>, thread::JoinHandle<()>)> {
        let lane = Arc::new(Lane {
            state: Mutex::new(LaneState { queue: VecDeque::new(), running: None, closed: false }),
            wake: Condvar::new(),
        });
        let worker = lane.clone();
        let join = thread::Builder::new()
            .name(format!("{WORKER_PREFIX}{name}"))
            .spawn(move || {
                while let Some(request) = worker.next() {
                    let msg = catch_unwind(AssertUnwindSafe(|| run(&request)))
                        .unwrap_or_else(|panic| fail(&request, format!("internal error: {}", panic_text(&*panic))));
                    // Send before marking idle: whoever sees the lane idle
                    // will also find the result in the channel.
                    let _ = tx.send(msg);
                    worker.done();
                }
            })
            .with_context(|| format!("starting the {name} worker"))?;
        Ok((Handle { lane, policy, same }, join))
    }

    fn next(&self) -> Option<T> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(request) = state.queue.pop_front() {
                state.running = Some(request.clone());
                return Some(request);
            }
            if state.closed {
                return None;
            }
            state = self.wake.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn done(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.running = None;
        self.wake.notify_all();
    }

    fn submit(&self, request: T, policy: Policy, same: fn(&T, &T) -> bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if policy == Policy::Latest {
            state.queue.clear();
            if state.running.as_ref().is_some_and(|running| same(running, &request)) {
                return;
            }
        }
        state.queue.push_back(request);
        self.wake.notify_all();
    }

    fn busy(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.running.is_some() || !state.queue.is_empty()
    }
}

fn panic_text(panic: &(dyn Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a worker panicked".to_string())
}

struct Handle<T> {
    lane: Arc<Lane<T>>,
    policy: Policy,
    same: fn(&T, &T) -> bool,
}

impl<T: Clone + Send + 'static> Handle<T> {
    fn submit(&self, request: T) {
        self.lane.submit(request, self.policy, self.same);
    }
}

#[derive(Clone)]
enum Mutation {
    Delete(Session),
    Restore(String),
}

/// Loads the scan cache (on the scan worker, so startup never waits on disk).
pub type CacheLoader = Box<dyn FnOnce() -> Cache + Send>;

pub struct Jobs {
    providers: Providers,
    scan: Handle<()>,
    preview: Handle<Session>,
    wrap: Handle<WrapRequest>,
    resume: Handle<Session>,
    mutate: Handle<Mutation>,
    mutate_thread: Option<thread::JoinHandle<()>>,
}

impl Jobs {
    pub fn new(tx: mpsc::Sender<Msg>, providers: Providers, load_cache: CacheLoader, trash: Trash) -> Result<Jobs> {
        let generation = Arc::new(AtomicU64::new(0));
        let trash = Arc::new(trash);

        let scan = {
            let (providers, generation, tx2) = (providers.clone(), generation.clone(), tx.clone());
            let mut cache: Option<Cache> = None;
            let mut loader = Some(load_cache);
            Lane::spawn(
                "scan",
                Policy::Latest,
                |_, _| false, // a scan requested during a scan must run again
                move |_: &()| {
                    let cache = cache.get_or_insert_with(|| {
                        let loaded = loader.take().map_or_else(Cache::in_memory, |load| load());
                        let _ = tx2.send(Msg::Cached(index::cached(&providers, &loaded)));
                        loaded
                    });
                    let started = generation.load(Ordering::SeqCst);
                    let scan = index::scan(&providers, cache);
                    let _ = cache.save();
                    Msg::Scanned {
                        generation: started,
                        result: Ok(ScanReport { sessions: scan.sessions, errors: scan.errors }),
                    }
                },
                |_, error| Msg::Scanned { generation: 0, result: Err(error) },
                tx.clone(),
            )?
        };

        let preview = {
            let providers = providers.clone();
            Lane::spawn(
                "preview",
                Policy::Latest,
                |a: &Session, b: &Session| PreviewKey::of(a) == PreviewKey::of(b),
                move |session: &Session| {
                    let result = provider::find(&providers, session.agent)
                        .ok_or_else(|| anyhow::anyhow!("unknown agent"))
                        .and_then(|p| p.transcript(session))
                        .map(|turns| Arc::new(limit_turns(turns, PREVIEW_MAX_BYTES, &session.id)))
                        .map_err(|e| format!("{e:#}"));
                    Msg::Preview(PreviewKey::of(session), result)
                },
                |session, error| Msg::Preview(PreviewKey::of(session), Err(error)),
                tx.clone(),
            )?
        };

        let wrap = Lane::spawn(
            "wrap",
            Policy::Latest,
            |a: &WrapRequest, b: &WrapRequest| a.key == b.key,
            |req: &WrapRequest| {
                let lines = transcript_lines(&req.turns, req.agent, req.key.width as usize, req.theme);
                Msg::Wrapped(req.key.clone(), Ok(Arc::new(lines)))
            },
            |req, error| Msg::Wrapped(req.key.clone(), Err(error)),
            tx.clone(),
        )?;

        let resume = {
            let providers = providers.clone();
            Lane::spawn(
                "resume",
                Policy::Latest,
                |a: &Session, b: &Session| a.path == b.path,
                move |session: &Session| {
                    Msg::ResumeChecked(actions::resume_command(&providers, session).map_err(|e| format!("{e:#}")))
                },
                |_, error| Msg::ResumeChecked(Err(error)),
                tx.clone(),
            )?
        };

        let mutate = {
            let (providers, generation) = (providers.clone(), generation.clone());
            let fail_generation = generation.clone();
            Lane::spawn(
                "mutate",
                Policy::Fifo,
                |_, _| false,
                move |mutation: &Mutation| {
                    let result = match mutation {
                        Mutation::Delete(session) => actions::delete(&providers, &trash, session).map(Some),
                        Mutation::Restore(key) => actions::restore(&providers, &trash, key).map(Some),
                    };
                    // Count the change as done before reporting it, so a
                    // scan that starts now is known to include it.
                    let generation = generation.fetch_add(1, Ordering::SeqCst) + 1;
                    mutation_msg(mutation, generation, result.map_err(|e| format!("{e:#}")))
                },
                move |mutation, error| {
                    let generation = fail_generation.fetch_add(1, Ordering::SeqCst) + 1;
                    mutation_msg(mutation, generation, Err(error))
                },
                tx,
            )?
        };

        let (scan, preview, wrap, resume) = (scan.0, preview.0, wrap.0, resume.0);
        let (mutate, mutate_thread) = mutate;
        Ok(Jobs { providers, scan, preview, wrap, resume, mutate, mutate_thread: Some(mutate_thread) })
    }

    pub fn providers(&self) -> &Providers {
        &self.providers
    }

    /// Whether any work is queued or running.
    pub fn busy(&self) -> bool {
        self.scan.lane.busy()
            || self.preview.lane.busy()
            || self.wrap.lane.busy()
            || self.resume.lane.busy()
            || self.mutating()
    }

    /// Whether a delete or restore is queued or running.
    pub fn mutating(&self) -> bool {
        self.mutate.lane.busy()
    }

    pub fn scan(&self) {
        self.scan.submit(());
    }

    pub fn preview(&self, session: Session) {
        self.preview.submit(session);
    }

    pub fn wrap(&self, request: WrapRequest) {
        self.wrap.submit(request);
    }

    pub fn check_resume(&self, session: Session) {
        self.resume.submit(session);
    }

    pub fn delete(&self, session: Session) {
        self.mutate.submit(Mutation::Delete(session));
    }

    pub fn restore(&self, key: String) {
        self.mutate.submit(Mutation::Restore(key));
    }

    /// Stops accepting work and waits until every queued delete and restore
    /// has finished. Reads (scans, previews) are abandoned; they change
    /// nothing on disk except the cache, which is replaced atomically.
    pub fn finish(&mut self) {
        let lanes: [&dyn Close; 5] =
            [&*self.scan.lane, &*self.preview.lane, &*self.wrap.lane, &*self.resume.lane, &*self.mutate.lane];
        for lane in lanes {
            lane.close();
        }
        if let Some(thread) = self.mutate_thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.finish();
    }
}

trait Close {
    fn close(&self);
}

impl<T> Close for Lane<T> {
    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        self.wake.notify_all();
    }
}

fn mutation_msg(mutation: &Mutation, generation: u64, result: Result<Option<crate::trash::TrashEntry>, String>) -> Msg {
    match mutation {
        Mutation::Delete(session) => Msg::Deleted {
            session: session.clone(),
            generation,
            result: result.and_then(|e| e.ok_or_else(|| "nothing deleted".to_string())),
        },
        Mutation::Restore(key) => Msg::Restored {
            key: key.clone(),
            generation,
            result: result.and_then(|e| e.ok_or_else(|| "nothing restored".to_string())),
        },
    }
}

/// Keeps the newest turns that fit in `max_bytes`, with a note in front
/// saying what was left out. A turn too big to keep whole keeps its end.
pub fn limit_turns(mut turns: Vec<Turn>, max_bytes: usize, id: &str) -> Vec<Turn> {
    let total: usize = turns.iter().map(|t| t.text.len()).sum();
    if total <= max_bytes {
        return turns;
    }
    let mut kept = 0;
    let mut start = turns.len();
    while start > 0 && kept + turns[start - 1].text.len() <= max_bytes {
        start -= 1;
        kept += turns[start].text.len();
    }
    let mut cut_one = false;
    if start > 0 && kept < max_bytes {
        let turn = &mut turns[start - 1];
        let mut cut = turn.text.len() - (max_bytes - kept);
        while !turn.text.is_char_boundary(cut) {
            cut += 1;
        }
        turn.text = format!("…{}", &turn.text[cut..]);
        start -= 1;
        cut_one = true;
    }
    let skipped = start;
    let mut rest = turns.split_off(start);
    let what = match (skipped, cut_one) {
        (0, _) => "the start of a long message is".to_string(),
        (n, false) => format!("{n} earlier message(s) are"),
        (n, true) => format!("{n} earlier message(s) and the start of a long message are"),
    };
    let note = Turn {
        role: Role::Tool,
        text: format!("{what} too large to preview; `entropeek show {}` prints everything", crate::text::one_line(id)),
        timestamp: None,
    };
    rest.insert(0, note);
    rest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Env;
    use crate::model::AgentKind;
    use crate::testkit;
    use crate::tui::app::{App, PreviewState, WrapKey};
    use std::time::Duration;

    fn recv(rx: &mpsc::Receiver<Msg>) -> Msg {
        rx.recv_timeout(Duration::from_secs(10)).expect("job result")
    }

    fn setup() -> (tempfile::TempDir, Env, Jobs, mpsc::Receiver<Msg>) {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::under(dir.path());
        testkit::populate(&env);
        let providers: Providers = Arc::new(provider::all(&env));
        let (tx, rx) = mpsc::channel();
        let file = Cache::default_file(&env.cache_dir);
        let jobs = Jobs::new(tx, providers, Box::new(move || Cache::load(&file)), Trash::new(env.trash_dir())).unwrap();
        (dir, env, jobs, rx)
    }

    #[test]
    fn jobs_drive_the_app_end_to_end() {
        let (_dir, env, mut jobs, rx) = setup();
        let mut app = App::new(vec![], "/h".into(), 0, Box::new(|_| String::new()));

        // The first scan sends the (empty) cached view, then the real list.
        jobs.scan();
        assert!(matches!(recv(&rx), Msg::Cached(ref s) if s.is_empty()));
        app.on_msg(recv(&rx));
        assert_eq!(app.sessions.len(), 4);
        assert!(env.cache_dir.join("index.bin").exists());

        let session = app.preview_request(true).unwrap();
        jobs.preview(session);
        app.on_msg(recv(&rx));
        assert!(matches!(app.preview, PreviewState::Ready(_, ref turns) if turns.len() == 2));

        let target = app.selected_session().unwrap().clone();
        jobs.delete(target.clone());
        app.on_msg(recv(&rx));
        assert_eq!(app.sessions.len(), 3);
        assert!(!target.path.exists());
        jobs.restore(app.undo.last().unwrap().clone());
        app.on_msg(recv(&rx));
        assert_eq!(app.sessions.len(), 4);
        assert!(target.path.exists());

        jobs.scan();
        app.on_msg(recv(&rx));
        assert_eq!(app.sessions.len(), 4);
        jobs.finish();
        // A worker marks itself idle just after sending its result.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while jobs.busy() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!jobs.busy());
    }

    #[test]
    fn rescans_are_coalesced() {
        // Workers are only created in `Jobs::new`; requests just queue.
        let (_dir, _env, jobs, rx) = setup();
        for _ in 0..100 {
            jobs.scan();
        }
        while jobs.busy() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let scans = rx.try_iter().filter(|m| matches!(m, Msg::Scanned { .. })).count();
        assert!((1..=2).contains(&scans), "{scans} scans ran for 100 requests");
    }

    #[test]
    fn duplicate_previews_are_skipped() {
        let (_dir, env, jobs, rx) = setup();
        let providers = provider::all(&env);
        let sessions = index::scan(&providers, &mut Cache::in_memory()).sessions;
        for _ in 0..50 {
            jobs.preview(sessions[0].clone());
        }
        jobs.preview(sessions[1].clone());
        while jobs.busy() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let loads: Vec<PreviewKey> =
            rx.try_iter().filter_map(|m| if let Msg::Preview(k, _) = m { Some(k) } else { None }).collect();
        assert!(loads.len() <= 3, "{} loads", loads.len());
        assert_eq!(loads.last(), Some(&PreviewKey::of(&sessions[1])), "the newest request wins");
    }

    #[test]
    fn worker_panics_are_reported_and_clear_busy() {
        struct Panicky;
        impl Provider for Panicky {
            fn kind(&self) -> AgentKind {
                AgentKind::Claude
            }
            fn root(&self) -> &std::path::Path {
                std::path::Path::new("/nonexistent")
            }
            fn discover(&self, _: &mut crate::provider::Unread) -> Vec<std::path::PathBuf> {
                panic!("simulated worker panic")
            }
            fn parse(&self, _: &std::path::Path, _: crate::provider::FileStamp) -> Result<Option<Session>> {
                unreachable!()
            }
            fn transcript(&self, _: &Session) -> Result<Vec<Turn>> {
                panic!("simulated transcript panic")
            }
            fn resume_command(&self, _: &Session) -> crate::provider::ResumeCommand {
                unreachable!()
            }
            fn related_paths(&self, _: &Session) -> Result<Vec<std::path::PathBuf>> {
                unreachable!()
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let providers: Providers = Arc::new(vec![Box::new(Panicky)]);
        let jobs = Jobs::new(tx, providers, Box::new(Cache::in_memory), Trash::new(dir.path().join("trash"))).unwrap();
        let mut app = App::new(vec![], "/h".into(), 0, Box::new(|_| String::new()));
        app.scanning = true;
        jobs.scan();
        let mut got_scan = false;
        while !got_scan {
            let msg = recv(&rx);
            got_scan = matches!(msg, Msg::Scanned { .. });
            app.on_msg(msg);
        }
        assert!(!app.scanning, "the failed scan finished the scanning state");
        assert!(app.status.as_ref().unwrap().text.contains("simulated worker panic"));
        jobs.preview(crate::model::sample_session(AgentKind::Claude, "x"));
        assert!(matches!(recv(&rx), Msg::Preview(_, Err(e)) if e.contains("simulated transcript panic")));
        while jobs.busy() {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn finish_waits_for_queued_mutations() {
        let (_dir, env, mut jobs, rx) = setup();
        let providers = provider::all(&env);
        let sessions = index::scan(&providers, &mut Cache::in_memory()).sessions;
        for session in &sessions {
            jobs.delete(session.clone());
        }
        jobs.finish();
        assert!(sessions.iter().all(|s| !s.path.exists()), "every delete completed before finish returned");
        let generations: Vec<u64> = rx
            .try_iter()
            .filter_map(|m| if let Msg::Deleted { generation, .. } = m { Some(generation) } else { None })
            .collect();
        assert_eq!(generations, vec![1, 2, 3, 4], "mutations ran in order");
    }

    #[test]
    fn failed_jobs_report_errors() {
        let (_dir, env, jobs, rx) = setup();
        let mut ghost = crate::model::sample_session(AgentKind::Claude, "ghost");
        ghost.path = env.claude_dir.join("projects/x/ghost.jsonl");
        jobs.preview(ghost.clone());
        assert!(matches!(recv(&rx), Msg::Preview(_, Err(_))));
        jobs.delete(ghost.clone());
        assert!(matches!(recv(&rx), Msg::Deleted { result: Err(_), .. }));
        jobs.restore("nope".into());
        assert!(matches!(recv(&rx), Msg::Restored { result: Err(_), .. }));
        jobs.check_resume(ghost);
        assert!(matches!(recv(&rx), Msg::ResumeChecked(Err(_))));
    }

    #[test]
    fn wrapping_happens_off_thread() {
        let (_dir, _env, jobs, rx) = setup();
        let turns = Arc::new(vec![Turn { role: Role::User, text: "word ".repeat(1000), timestamp: None }]);
        let key = WrapKey {
            preview: PreviewKey { path: "/p".into(), mtime_ns: 1, size: 1 },
            width: 40,
            mode: crate::tui::theme::Mode::Dark,
            icons: false,
        };
        jobs.wrap(WrapRequest { key: key.clone(), turns, agent: AgentKind::Claude, theme: Default::default() });
        match recv(&rx) {
            Msg::Wrapped(got, Ok(lines)) => {
                assert_eq!(got, key);
                assert!(lines.len() > 100);
            }
            _ => panic!("expected a wrapped transcript"),
        }
    }

    #[test]
    fn huge_transcripts_keep_the_newest_turns() {
        let turns: Vec<Turn> =
            (0..10).map(|i| Turn { role: Role::User, text: format!("{i}").repeat(100), timestamp: None }).collect();
        let limited = limit_turns(turns.clone(), 350, "abc");
        assert_eq!(limited.len(), 5);
        assert!(
            limited[0].text.contains("6 earlier message(s) and the start of a long message"),
            "{}",
            limited[0].text
        );
        assert_eq!(limited[1].text, format!("…{}", "6".repeat(50)));
        assert_eq!(limited[4].text, turns[9].text);
        assert_eq!(limit_turns(turns.clone(), 10_000, "abc"), turns);
        // One giant message keeps its end instead of disappearing.
        let giant = vec![Turn { role: Role::Assistant, text: "é".repeat(1000), timestamp: None }];
        let limited = limit_turns(giant, 101, "abc");
        assert_eq!(limited.len(), 2);
        assert!(limited[0].text.starts_with("the start of a long message"));
        assert!(limited[1].text.len() <= 104 && limited[1].text.ends_with('é'));
    }
}
