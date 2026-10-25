//! Builds the session list from every provider, reusing the cache.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::cache::{Cache, CacheEntry};
use crate::model::Session;
use crate::provider::{FileStamp, Provider, Unread};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Candidate files found.
    pub files: usize,
    /// Files parsed because they were new or changed.
    pub parsed: usize,
    /// Files whose cached result was reused.
    pub reused: usize,
    /// Files or directories that could not be read.
    pub failed: usize,
    pub elapsed: Duration,
}

pub struct Scan {
    /// Sessions, newest first.
    pub sessions: Vec<Session>,
    pub stats: ScanStats,
    /// What could not be read. Unreadable files are not cached as
    /// "not a session", so they are retried on the next scan.
    pub errors: Vec<String>,
}

enum Outcome {
    Reused(CacheEntry),
    Parsed(CacheEntry),
    /// Could not be read now; keeps the last good entry, if any.
    Failed(String, Option<CacheEntry>),
    /// Deleted between discovery and reading.
    Gone,
}

/// Discovers and parses every session, parsing only files that changed since
/// they were cached. Updates `cache` in place (the caller decides when to save).
pub fn scan(providers: &[Box<dyn Provider>], cache: &mut Cache) -> Scan {
    let started = Instant::now();
    // Per provider: (provider index, file) pairs and what could not be read.
    type Found = (Vec<(usize, PathBuf)>, Unread);
    let discovered: Vec<Found> = providers
        .par_iter()
        .enumerate()
        .map(|(index, provider)| {
            let mut unread = Unread::default();
            let files = provider.discover(&mut unread).into_iter().map(|path| (index, path)).collect();
            (files, unread)
        })
        .collect();
    let mut errors = Vec::new();
    let mut found = Vec::new();
    let mut unread_dirs = Vec::new();
    for (files, unread) in discovered {
        found.extend(files);
        errors.extend(unread.errors);
        unread_dirs.extend(unread.dirs);
    }

    let cache_ref = &*cache;
    let results: Vec<(PathBuf, Outcome)> = found
        .into_par_iter()
        .map(|(index, path)| {
            let previous = || cache_ref.entry(&path).cloned();
            let outcome = match FileStamp::try_of(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Outcome::Gone,
                Err(e) => Outcome::Failed(format!("cannot read {}: {e}", path.display()), previous()),
                Ok(stamp) => match cache_ref.get(&path, stamp) {
                    Some(entry) => Outcome::Reused(entry.clone()),
                    None => match providers[index].parse(&path, stamp) {
                        Ok(session) => Outcome::Parsed(CacheEntry { stamp, session }),
                        Err(e) => Outcome::Failed(format!("{e:#}"), previous()),
                    },
                },
            };
            (path, outcome)
        })
        .collect();

    let mut stats = ScanStats { files: results.len(), failed: errors.len(), ..ScanStats::default() };
    let mut entries = HashMap::with_capacity(results.len());
    let mut sessions = Vec::with_capacity(results.len());
    for (path, outcome) in results {
        let entry = match outcome {
            Outcome::Reused(entry) => {
                stats.reused += 1;
                entry
            }
            Outcome::Parsed(entry) => {
                stats.parsed += 1;
                entry
            }
            Outcome::Failed(error, previous) => {
                stats.failed += 1;
                errors.push(error);
                match previous {
                    Some(entry) => entry,
                    None => continue,
                }
            }
            Outcome::Gone => continue,
        };
        if let Some(session) = &entry.session {
            sessions.push(session.clone());
        }
        entries.insert(path, entry);
    }
    // A directory that could not be listed says nothing about whether its
    // sessions still exist: keep what was known about them until it can be
    // read again.
    if !unread_dirs.is_empty() {
        for (path, entry) in cache.iter() {
            if !entries.contains_key(path) && unread_dirs.iter().any(|dir| path.starts_with(dir)) {
                if let Some(session) = &entry.session {
                    sessions.push(session.clone());
                }
                entries.insert(path.to_path_buf(), entry.clone());
            }
        }
    }
    cache.replace(entries);
    let sessions = finalize(providers, sessions, &mut errors);
    stats.elapsed = started.elapsed();
    Scan { sessions, stats, errors }
}

/// The sessions currently in the cache, decorated and sorted, without
/// touching session files. Lets the UI show a list before the scan finishes.
/// Only sessions inside the currently configured agent directories count,
/// so switching `CLAUDE_CONFIG_DIR` never shows another profile's history.
pub fn cached(providers: &[Box<dyn Provider>], cache: &Cache) -> Vec<Session> {
    let in_scope = |s: &Session| {
        providers.iter().any(|p| p.kind() == s.agent && s.path.starts_with(p.root()) && s.path != p.root())
    };
    finalize(providers, cache.sessions().into_iter().filter(in_scope).collect(), &mut Vec::new())
}

/// Applies provider decorations and sorts newest first.
fn finalize(providers: &[Box<dyn Provider>], mut sessions: Vec<Session>, errors: &mut Vec<String>) -> Vec<Session> {
    sessions.sort_by_key(|s| s.agent);
    let mut rest = sessions.as_mut_slice();
    while let Some(first) = rest.first() {
        let agent = first.agent;
        let len = rest.iter().take_while(|s| s.agent == agent).count();
        let (group, tail) = rest.split_at_mut(len);
        if let Some(provider) = providers.iter().find(|p| p.kind() == agent) {
            provider.decorate(group, errors);
        }
        rest = tail;
    }
    sort_newest_first(&mut sessions);
    sessions
}

/// Newest first; ties broken by id so the order is stable.
pub fn sort_newest_first(sessions: &mut [Session]) {
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Env;
    use crate::model::AgentKind;
    use crate::provider;
    use crate::testkit;
    use std::fs;

    fn setup() -> (tempfile::TempDir, Env, Vec<Box<dyn Provider>>) {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::under(dir.path());
        let providers = provider::all(&env);
        (dir, env, providers)
    }

    #[test]
    fn scan_lists_all_agents_newest_first() {
        let (_dir, env, providers) = setup();
        let expected = testkit::populate(&env);
        let scan = scan(&providers, &mut Cache::in_memory());
        let ids: Vec<&str> = scan.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, expected);
        assert_eq!(scan.sessions[0].agent, AgentKind::Codex);
        assert_eq!(scan.sessions[0].title.as_deref(), Some("Review rate limiter"));
        assert_eq!(scan.stats.files, 4);
        assert_eq!(scan.stats.parsed, 4);
    }

    #[test]
    fn second_scan_reuses_the_cache() {
        let (_dir, env, providers) = setup();
        testkit::populate(&env);
        let mut cache = Cache::in_memory();
        let first = scan(&providers, &mut cache);
        let second = scan(&providers, &mut cache);
        assert_eq!(second.stats.parsed, 0);
        assert_eq!(second.stats.reused, 4);
        assert_eq!(first.sessions, second.sessions);
    }

    #[test]
    fn changed_files_are_reparsed_and_deleted_files_dropped() {
        let (_dir, env, providers) = setup();
        let ids = testkit::populate(&env);
        let mut cache = Cache::in_memory();
        let first = scan(&providers, &mut cache);

        // Append a prompt to one session; delete another.
        let changed = first.sessions.iter().find(|s| s.id == ids[3]).unwrap();
        let mut bytes = fs::read(&changed.path).unwrap();
        bytes.extend(testkit::jsonl_bytes(&[testkit::claude::user(
            &ids[3],
            "/work/app",
            "2026-10-09T00:00:00Z",
            "one more thing",
        )]));
        fs::write(&changed.path, bytes).unwrap();
        let removed = first.sessions.iter().find(|s| s.id == ids[2]).unwrap();
        fs::remove_file(&removed.path).unwrap();

        let second = scan(&providers, &mut cache);
        assert_eq!(second.stats.parsed, 1);
        assert_eq!(second.stats.reused, 2);
        assert_eq!(cache.len(), 3);
        assert_eq!(second.sessions[0].id, ids[3], "the updated session is now the newest");
        assert_eq!(second.sessions[0].last_prompt.as_deref(), Some("one more thing"));
        assert!(second.sessions.iter().all(|s| s.id != ids[2]));
    }

    #[test]
    fn unusable_files_are_cached_too() {
        let (_dir, env, providers) = setup();
        let empty = env.claude_dir.join("projects/-x/empty.jsonl");
        testkit::write_jsonl(&empty, &[testkit::claude::snapshot()]);
        let mut cache = Cache::in_memory();
        assert_eq!(scan(&providers, &mut cache).stats.parsed, 1);
        let again = scan(&providers, &mut cache);
        assert_eq!((again.stats.parsed, again.stats.reused), (0, 1));
        assert!(again.sessions.is_empty());
    }

    #[test]
    fn cached_view_is_decorated_with_fresh_titles() {
        let (_dir, env, providers) = setup();
        let ids = testkit::populate(&env);
        let mut cache = Cache::in_memory();
        scan(&providers, &mut cache);
        // Renaming a Codex thread only touches the index file, not the session file.
        testkit::codex::write_index(&env.codex_dir, &[(&ids[0], "Renamed thread")]);
        let sessions = cached(&providers, &cache);
        assert_eq!(sessions.len(), 4);
        assert_eq!(sessions[0].title.as_deref(), Some("Renamed thread"));
        let rescanned = scan(&providers, &mut cache);
        assert_eq!(rescanned.sessions[0].title.as_deref(), Some("Renamed thread"));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_files_are_retried_not_cached_as_empty() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, env, providers) = setup();
        let ids = testkit::populate(&env);
        let path = testkit::claude::session_path(&env.claude_dir, &ids[1], "/work/app");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let mut cache = Cache::in_memory();
        let denied = scan(&providers, &mut cache);
        assert_eq!(denied.sessions.len(), 3);
        assert_eq!(denied.stats.failed, 1);
        assert!(denied.errors[0].contains(&ids[1]), "{:?}", denied.errors);
        // Readable again, with the same size and mtime.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let again = scan(&providers, &mut cache);
        assert_eq!(again.sessions.len(), 4, "the session is back on a warm rescan");
        assert!(again.errors.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_read_keeps_the_last_good_entry() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, env, providers) = setup();
        let ids = testkit::populate(&env);
        let mut cache = Cache::in_memory();
        scan(&providers, &mut cache);
        let path = testkit::claude::session_path(&env.claude_dir, &ids[1], "/work/app");
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend(b"\n");
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let failed = scan(&providers, &mut cache);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(failed.sessions.len(), 4, "the stale entry is still shown");
        assert_eq!(failed.stats.failed, 1);
        assert_eq!(scan(&providers, &mut cache).stats.parsed, 1, "and retried next time");
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directories_are_reported() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, env, providers) = setup();
        testkit::populate(&env);
        let projects = env.claude_dir.join("projects");
        let project = fs::read_dir(&projects).unwrap().next().unwrap().unwrap().path();
        fs::set_permissions(&project, fs::Permissions::from_mode(0o000)).unwrap();
        let result = scan(&providers, &mut Cache::in_memory());
        fs::set_permissions(&project, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.errors.iter().any(|e| e.contains("cannot read")), "{:?}", result.errors);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_keeps_its_known_sessions() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, env, providers) = setup();
        testkit::populate(&env);
        let mut cache = Cache::in_memory();
        let before = scan(&providers, &mut cache).sessions;
        let project = before.iter().find(|s| s.agent == AgentKind::Claude).unwrap().path.parent().unwrap().to_owned();
        fs::set_permissions(&project, fs::Permissions::from_mode(0o000)).unwrap();
        let denied = fs::read_dir(&project).is_err(); // root reads anyway
        let during = scan(&providers, &mut cache);
        fs::set_permissions(&project, fs::Permissions::from_mode(0o755)).unwrap();
        if denied {
            assert!(!during.errors.is_empty());
        }
        assert_eq!(during.sessions, before, "nothing disappears because it could not be listed");
        assert_eq!(cache.len(), before.len());
        // Sessions really deleted meanwhile do go, once the directory is readable.
        let gone = before.iter().find(|s| s.path.starts_with(&project)).unwrap();
        fs::remove_file(&gone.path).unwrap();
        assert_eq!(scan(&providers, &mut cache).sessions.len(), before.len() - 1);
    }

    #[test]
    fn cached_sessions_are_scoped_to_the_current_roots() {
        let (_dir, env, providers) = setup();
        testkit::populate(&env);
        let mut cache = Cache::in_memory();
        scan(&providers, &mut cache);
        assert_eq!(cached(&providers, &cache).len(), 4);
        // Same cache, another profile.
        let other = tempfile::tempdir().unwrap();
        let other_providers = provider::all(&Env::under(other.path()));
        assert!(cached(&other_providers, &cache).is_empty());
        let only_codex: Vec<Box<dyn Provider>> =
            provider::all(&env).into_iter().filter(|p| p.kind() == AgentKind::Codex).collect();
        assert_eq!(cached(&only_codex, &cache).len(), 1);
    }

    #[test]
    fn missing_agent_directories_are_fine() {
        let (_dir, _env, providers) = setup();
        let scan = scan(&providers, &mut Cache::in_memory());
        assert!(scan.sessions.is_empty());
        assert_eq!(scan.stats.files, 0);
        assert!(scan.errors.is_empty(), "missing directories are not errors");
    }
}
