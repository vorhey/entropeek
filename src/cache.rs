//! On-disk cache of parsed session metadata.
//!
//! Keyed by file path and validated by [`FileStamp`] (mtime + size), so a
//! warm start only re-parses files that changed since the last run. Files that
//! are not usable sessions are cached too (`session: None`), so they are not
//! re-read either.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::Session;
use crate::provider::FileStamp;

/// Bump whenever parsing output changes, so old caches are discarded.
pub const CACHE_FORMAT: u32 = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub stamp: FileStamp,
    pub session: Option<Session>,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    format: u32,
    entries: HashMap<PathBuf, CacheEntry>,
}

#[derive(Default)]
pub struct Cache {
    entries: HashMap<PathBuf, CacheEntry>,
    file: Option<PathBuf>,
}

impl Cache {
    /// A cache that is never written to disk.
    pub fn in_memory() -> Cache {
        Cache::default()
    }

    /// Loads the cache from `file`. A missing, corrupt or outdated file gives
    /// an empty cache; it will be rewritten on the next save.
    pub fn load(file: &Path) -> Cache {
        let entries = fs::read(file)
            .ok()
            .and_then(|bytes| postcard::from_bytes::<CacheFile>(&bytes).ok())
            .filter(|cache| cache.format == CACHE_FORMAT)
            .map(|cache| cache.entries)
            .unwrap_or_default();
        Cache { entries, file: Some(file.to_path_buf()) }
    }

    /// The default cache file inside `cache_dir`.
    pub fn default_file(cache_dir: &Path) -> PathBuf {
        cache_dir.join("index.bin")
    }

    /// The cached entry for `path` if it is still valid for `stamp`.
    pub fn get(&self, path: &Path, stamp: FileStamp) -> Option<&CacheEntry> {
        self.entries.get(path).filter(|entry| entry.stamp == stamp)
    }

    /// The cached entry for `path`, however old.
    pub fn entry(&self, path: &Path) -> Option<&CacheEntry> {
        self.entries.get(path)
    }

    /// Every cached session, without checking the files. Used to draw the
    /// first frame instantly while a real scan runs.
    pub fn sessions(&self) -> Vec<Session> {
        self.entries.values().filter_map(|e| e.session.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Replaces all entries; paths not present any more are dropped.
    pub fn replace(&mut self, entries: HashMap<PathBuf, CacheEntry>) {
        self.entries = entries;
    }

    /// Forgets one file, e.g. after it was deleted.
    pub fn remove(&mut self, path: &Path) {
        self.entries.remove(path);
    }

    /// Writes the cache atomically to a private file. In-memory caches do nothing.
    pub fn save(&self) -> Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let dir = file.parent().context("cache file has no parent directory")?;
        crate::fsutil::create_private_dir(dir).with_context(|| format!("creating {}", dir.display()))?;
        let bytes = postcard::to_stdvec(&CacheFileRef { format: CACHE_FORMAT, entries: &self.entries })?;
        crate::fsutil::write_private(file, &bytes).with_context(|| format!("writing {}", file.display()))
    }
}

/// Borrowing twin of [`CacheFile`] so saving does not clone every entry.
#[derive(Serialize)]
struct CacheFileRef<'a> {
    format: u32,
    entries: &'a HashMap<PathBuf, CacheEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, sample_session};

    fn entry(id: &str, size: u64) -> (PathBuf, CacheEntry) {
        let session = sample_session(AgentKind::Claude, id);
        (session.path.clone(), CacheEntry { stamp: FileStamp { mtime_ns: 1, size }, session: Some(session) })
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let file = Cache::default_file(dir.path());
        let mut cache = Cache::load(&file);
        assert!(cache.is_empty());
        cache.replace(HashMap::from([entry("a", 1), entry("b", 2)]));
        cache.save().unwrap();

        let loaded = Cache::load(&file);
        assert_eq!(loaded.len(), 2);
        let (path, e) = entry("a", 1);
        assert_eq!(loaded.get(&path, e.stamp), Some(&e));
        assert_eq!(loaded.sessions().len(), 2);
    }

    #[test]
    fn stale_stamp_misses() {
        let mut cache = Cache::in_memory();
        cache.replace(HashMap::from([entry("a", 1)]));
        let (path, _) = entry("a", 1);
        assert!(cache.get(&path, FileStamp { mtime_ns: 1, size: 1 }).is_some());
        assert!(cache.get(&path, FileStamp { mtime_ns: 2, size: 1 }).is_none());
        assert!(cache.get(&path, FileStamp { mtime_ns: 1, size: 9 }).is_none());
    }

    #[test]
    fn corrupt_or_outdated_files_load_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index.bin");
        fs::write(&file, b"\xff\xff garbage").unwrap();
        assert!(Cache::load(&file).is_empty());

        let old = postcard::to_stdvec(&CacheFile { format: CACHE_FORMAT + 1, entries: HashMap::from([entry("a", 1)]) })
            .unwrap();
        fs::write(&file, old).unwrap();
        assert!(Cache::load(&file).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn cache_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = Cache::default_file(dir.path());
        Cache::load(&file).save().unwrap();
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn in_memory_save_is_a_no_op() {
        Cache::in_memory().save().unwrap();
    }
}
