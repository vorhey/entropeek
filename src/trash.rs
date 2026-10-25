//! A recoverable trash for deleted sessions.
//!
//! Deleting moves every file that belongs to a session into
//! `<data_dir>/trash/<key>/items/<n>`, and `manifest.json` records where each
//! came from. Restoring moves them back.
//!
//! Safety rules:
//! - The manifest is written *before* anything moves, with a state
//!   (`deleting`, `trashed`, `restoring`). An interrupted operation leaves a
//!   listed, restorable entry instead of invisible data.
//! - Moves never replace anything at the destination, so a file that
//!   reappeared at an original location is never overwritten.
//! - Nothing is removed from the trash unless it is provably no longer
//!   needed: rollbacks that cannot complete keep the files in the trash.
//! - Every path must resolve inside the agent's directory, and moves are
//!   made relative to directories opened without following links, so a
//!   directory swapped for a link after the check is refused.
//! - Every delete, restore and purge holds the entry's lock (an operating
//!   system lock, released if the process dies), so two entropeek
//!   processes never work on the same entry at once.
//! - Across filesystems, items are copied to a hidden staging name, checked
//!   to be unchanged, and only then published under their final name and
//!   removed from their source.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::fsutil;
use crate::model::Session;
use crate::timefmt;

/// The manifest format written by this version.
pub const MANIFEST_VERSION: u32 = 2;
const MANIFEST: &str = "manifest.json";
const LOCK: &str = "lock";
const ITEMS: &str = "items";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashedItem {
    /// Absolute, resolved path the item came from.
    pub original: PathBuf,
    /// File name inside the entry's `items/` directory.
    pub stored: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryState {
    /// Files are being moved into the trash, or a delete was interrupted.
    Deleting,
    /// Every item that could be moved is in the trash.
    Trashed,
    /// Files are being moved back, or a restore was interrupted.
    Restoring,
}

fn version_one() -> u32 {
    1
}

fn trashed() -> EntryState {
    EntryState::Trashed
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashEntry {
    #[serde(default = "version_one")]
    pub version: u32,
    /// Directory name of the entry inside the trash.
    pub key: String,
    pub deleted_at: i64,
    #[serde(default = "trashed")]
    pub state: EntryState,
    /// The agent directory (resolved) the items were deleted from. Restore
    /// accepts it even if the configuration has changed since.
    #[serde(default)]
    pub root: Option<PathBuf>,
    pub session: Session,
    pub items: Vec<TrashedItem>,
}

impl TrashEntry {
    /// Whether a delete or restore of this entry did not finish.
    pub fn is_incomplete(&self) -> bool {
        self.state != EntryState::Trashed
    }
}

/// A trash directory that could not be read as an entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidEntry {
    pub key: String,
    pub error: String,
}

#[derive(Debug, Default)]
pub struct Listing {
    /// Most recently deleted first.
    pub entries: Vec<TrashEntry>,
    pub invalid: Vec<InvalidEntry>,
}

/// What [`Trash::purge_older_than`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Purged {
    /// Entries permanently deleted.
    pub count: usize,
    /// Entries skipped because another entropeek process was using them.
    pub busy: usize,
}

/// The error when another entropeek process is working on an entry.
#[derive(Debug)]
pub struct Busy;

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("this trash entry is being changed by another entropeek process")
    }
}

impl std::error::Error for Busy {}

/// Why a move failed. `completed` means the destination holds a complete
/// copy and only removing the source failed (possible across filesystems).
#[derive(Debug)]
pub struct MoveError {
    pub error: io::Error,
    pub completed: bool,
}

impl From<io::Error> for MoveError {
    fn from(error: io::Error) -> MoveError {
        MoveError { error, completed: false }
    }
}

type Mover = Box<dyn Fn(&Path, &Path) -> Result<(), MoveError> + Send + Sync>;

pub struct Trash {
    root: PathBuf,
    mover: Mover,
}

impl Trash {
    pub fn new(root: PathBuf) -> Trash {
        Trash { root, mover: Box::new(move_path) }
    }

    /// A trash that moves files with `mover`, for failure-injection tests.
    #[doc(hidden)]
    pub fn with_mover(
        root: PathBuf,
        mover: impl Fn(&Path, &Path) -> Result<(), MoveError> + Send + Sync + 'static,
    ) -> Trash {
        Trash { root, mover: Box::new(mover) }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Moves `paths` (which all belong to `session`) into the trash. Every
    /// path must resolve inside `allowed_root`. If a move fails, the moves
    /// already made are undone; anything that cannot be put back stays in the
    /// trash as a restorable entry, and the error says so.
    pub fn put(&self, session: &Session, paths: &[PathBuf], allowed_root: &Path) -> Result<TrashEntry> {
        if paths.is_empty() {
            bail!("nothing to delete");
        }
        let resolved: Vec<PathBuf> =
            paths.iter().map(|p| fsutil::resolve_inside(p, allowed_root)).collect::<Result<_>>()?;
        if !fsutil::occupied(&resolved[0]) {
            bail!("session file {} no longer exists", paths[0].display());
        }

        let root = self.real_root()?;
        let deleted_at = timefmt::now_ms();
        let (key, dir) = new_entry_dir(&root, deleted_at, session)?;
        // Held until the entry is complete, so no other process can restore
        // or purge an entry whose files are still arriving.
        let lock = match EntryLock::acquire(&dir) {
            Ok(lock) => lock,
            Err(err) => {
                discard_entry_dir(&dir);
                return Err(err).context("creating the trash entry");
            }
        };
        let items_dir = dir.join(ITEMS);
        let mut entry = TrashEntry {
            version: MANIFEST_VERSION,
            key,
            deleted_at,
            state: EntryState::Deleting,
            root: fs::canonicalize(allowed_root).ok(),
            session: session.clone(),
            items: resolved
                .iter()
                .enumerate()
                .map(|(n, original)| TrashedItem { original: original.clone(), stored: n.to_string() })
                .collect(),
        };
        // The manifest (and the entry's own name in the trash) must be on
        // disk before anything moves, or a crash could lose track of files.
        let setup = create_dir_private(&items_dir)
            .and_then(|()| write_manifest(&dir, &entry).map_err(io::Error::other))
            .and_then(|()| fsutil::sync_dir_strict(&root));
        if let Err(err) = setup {
            discard_entry_dir(&dir);
            return Err(err).context("creating the trash entry");
        }

        let mut moved: Vec<usize> = Vec::new();
        for (n, item) in entry.items.iter().enumerate() {
            // Re-check right before moving, in case a directory was swapped
            // for a link since the plan was made. The move itself refuses
            // links anywhere in either path, which closes the window left.
            let failure = match fsutil::resolve_inside(&paths[n], allowed_root) {
                Ok(now) if now == item.original => match (self.mover)(&item.original, &items_dir.join(&item.stored)) {
                    Ok(()) => {
                        sync_parent(&item.original);
                        fsutil::sync_dir(&items_dir);
                        moved.push(n);
                        continue;
                    }
                    Err(err) => {
                        if err.completed {
                            moved.push(n);
                        }
                        anyhow::Error::new(err.error)
                    }
                },
                Ok(now) => anyhow!("{} now resolves to {}", paths[n].display(), now.display()),
                Err(err) => err,
            };
            let failure = failure.context(format!("moving {} to the trash", item.original.display()));
            return Err(self.abandon_put(&dir, entry, &moved, failure));
        }

        entry.state = EntryState::Trashed;
        // If this fails, the `deleting` manifest still describes every item,
        // so the entry stays listed and restorable.
        let _ = write_manifest(&dir, &entry);
        drop(lock);
        Ok(entry)
    }

    /// Undoes a failed delete as far as possible without losing anything.
    /// The caller holds the entry's lock.
    fn abandon_put(&self, dir: &Path, mut entry: TrashEntry, moved: &[usize], failure: anyhow::Error) -> anyhow::Error {
        let items_dir = dir.join(ITEMS);
        let mut kept = Vec::new();
        for &n in moved.iter().rev() {
            let item = &entry.items[n];
            match (self.mover)(&items_dir.join(&item.stored), &item.original) {
                Ok(()) => sync_parent(&item.original),
                // A completed copy back means the original is whole again;
                // the leftover in the trash is kept, never deleted.
                Err(err) => kept.push((n, err)),
            }
        }
        if kept.is_empty() {
            discard_entry_dir(dir);
            return failure;
        }
        let keep: HashSet<usize> = kept.iter().map(|(n, _)| *n).collect();
        entry.items =
            entry.items.iter().enumerate().filter(|(n, _)| keep.contains(n)).map(|(_, i)| i.clone()).collect();
        entry.state = EntryState::Trashed;
        let _ = write_manifest(dir, &entry);
        let names: Vec<String> = entry.items.iter().map(|i| i.original.display().to_string()).collect();
        failure.context(format!(
            "could not put back {} (kept safely in the trash; restore with `entropeek trash restore {}`)",
            names.join(", "),
            entry.key
        ))
    }

    /// The trash directory, created if needed, with every link in its path
    /// resolved (moves refuse links on the way to their destination).
    fn real_root(&self) -> Result<PathBuf> {
        fsutil::create_private_dir(&self.root).with_context(|| format!("creating {}", self.root.display()))?;
        fs::canonicalize(&self.root).with_context(|| format!("resolving {}", self.root.display()))
    }

    /// Every entry, plus directories that are not valid entries. A trash
    /// that does not exist yet is empty; one that cannot be read is an error.
    pub fn scan(&self) -> Result<Listing> {
        let mut listing = Listing::default();
        let dirs = match fs::read_dir(&self.root) {
            Ok(dirs) => dirs,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(listing),
            Err(e) => return Err(e).with_context(|| format!("reading the trash at {}", self.root.display())),
        };
        for dir in dirs {
            let dir = dir.with_context(|| format!("reading the trash at {}", self.root.display()))?;
            let key = dir.file_name().to_string_lossy().into_owned();
            match read_entry(&dir.path(), &key) {
                Ok(entry) => listing.entries.push(entry),
                Err(err) => listing.invalid.push(InvalidEntry { key, error: format!("{err:#}") }),
            }
        }
        listing.entries.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at).then_with(|| b.key.cmp(&a.key)));
        listing.invalid.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(listing)
    }

    /// Valid entries, most recently deleted first.
    pub fn list(&self) -> Result<Vec<TrashEntry>> {
        Ok(self.scan()?.entries)
    }

    /// Finds an entry by key, or by session id (the most recent deletion wins).
    pub fn find(&self, selector: &str) -> Result<TrashEntry> {
        let selector = selector.trim();
        let entries = self.list()?;
        entries
            .iter()
            .find(|e| e.key == selector)
            .or_else(|| entries.iter().find(|e| e.session.id == selector))
            .or_else(|| {
                let matches: Vec<&TrashEntry> = entries.iter().filter(|e| e.session.id.starts_with(selector)).collect();
                (selector.len() >= 4 && matches.len() == 1).then(|| matches[0])
            })
            .cloned()
            .with_context(|| format!("nothing in the trash matches {selector:?}"))
    }

    /// Moves an entry's files back to where they were. Every original must
    /// resolve inside one of `allowed_roots`. Refuses (and changes nothing)
    /// if an original location is occupied. Safe to repeat after an
    /// interruption: items already back in place are skipped.
    pub fn restore(&self, key: &str, allowed_roots: &[PathBuf]) -> Result<TrashEntry> {
        let dir = self.entry_dir(key)?;
        let lock = EntryLock::acquire(&dir)?;
        let mut entry = read_entry(&dir, key)?;
        let items_dir = dir.join(ITEMS);

        let recorded = entry.root.iter().filter(|r| r.is_absolute() && r.parent().is_some() && r.is_dir());
        let roots: Vec<&PathBuf> = allowed_roots.iter().chain(recorded).collect();
        let mut pending: Vec<(TrashedItem, PathBuf)> = Vec::new();
        for item in &entry.items {
            let stored = items_dir.join(&item.stored);
            if !fsutil::occupied(&stored) {
                continue; // restored by an earlier, interrupted run
            }
            let target =
                roots.iter().find_map(|root| resolve_for_create(&item.original, root).ok()).with_context(|| {
                    format!("cannot restore {}: it is not inside an agent directory", item.original.display())
                })?;
            if fsutil::occupied(&target) {
                bail!("cannot restore: {} already exists", item.original.display());
            }
            pending.push((item.clone(), target));
        }
        if pending.is_empty() && !entry.is_incomplete() {
            bail!("trash entry {key} holds no files");
        }

        entry.state = EntryState::Restoring;
        write_manifest(&dir, &entry)?;
        let mut restored: Vec<&(TrashedItem, PathBuf)> = Vec::new();
        for job in &pending {
            let (item, target) = job;
            let result = target
                .parent()
                .map_or(Ok(()), fs::create_dir_all)
                .map_err(MoveError::from)
                .and_then(|()| (self.mover)(&items_dir.join(&item.stored), target));
            match result {
                Ok(()) => {
                    sync_parent(target);
                    restored.push(job);
                }
                Err(err) => {
                    if err.completed {
                        restored.push(job);
                    }
                    let mut back_in_trash = true;
                    for (done, target) in restored.iter().map(|j| (&j.0, &j.1)).rev() {
                        back_in_trash &= (self.mover)(target, &items_dir.join(&done.stored)).is_ok();
                    }
                    if back_in_trash {
                        entry.state = EntryState::Trashed;
                    }
                    let _ = write_manifest(&dir, &entry);
                    return Err(anyhow::Error::new(err.error))
                        .with_context(|| format!("restoring {}", item.original.display()));
                }
            }
        }

        // Everything is back. Remove the entry, still holding its lock and
        // without recursion, so any unexpected leftover stays in the trash
        // instead of being deleted.
        discard_entry_dir(&dir);
        drop(lock);
        entry.state = EntryState::Trashed;
        Ok(entry)
    }

    /// Permanently deletes one entry. Fails with [`Busy`] if another
    /// entropeek process is deleting into or restoring from it.
    pub fn purge(&self, key: &str) -> Result<()> {
        let dir = self.entry_dir(key)?;
        let _lock = EntryLock::acquire(&dir)?;
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))
    }

    /// Permanently deletes entries deleted before `cutoff_ms`, and empty
    /// leftovers of interrupted deletes. Invalid entries are left alone, and
    /// so are entries another entropeek process is working on.
    pub fn purge_older_than(&self, cutoff_ms: i64) -> Result<Purged> {
        let listing = self.scan()?;
        let mut purged = Purged::default();
        for entry in listing.entries.iter().filter(|e| e.deleted_at < cutoff_ms) {
            match self.purge(&entry.key) {
                Ok(()) => purged.count += 1,
                Err(err) if err.is::<Busy>() => purged.busy += 1,
                Err(err) => return Err(err),
            }
        }
        for invalid in &listing.invalid {
            let dir = self.root.join(&invalid.key);
            if valid_key(&invalid.key)
                && fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir())
                && let Ok(_lock) = EntryLock::acquire(&dir)
            {
                discard_entry_dir(&dir);
            }
        }
        Ok(purged)
    }

    fn entry_dir(&self, key: &str) -> Result<PathBuf> {
        if !valid_key(key) {
            bail!("invalid trash key {key:?}");
        }
        let root = fs::canonicalize(&self.root).map_err(|_| anyhow!("no trash entry {key:?}"))?;
        let dir = root.join(key);
        if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            bail!("no trash entry {key:?}");
        }
        Ok(dir)
    }
}

fn new_entry_dir(root: &Path, deleted_at: i64, session: &Session) -> Result<(String, PathBuf)> {
    let safe_id: String =
        session.id.chars().take(80).map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    let base = format!("{deleted_at}-{}-{safe_id}", session.agent.id());
    for attempt in 0..100 {
        let key = if attempt == 0 { base.clone() } else { format!("{base}-{attempt}") };
        let dir = root.join(&key);
        match create_dir_private(&dir) {
            Ok(()) => return Ok((key, dir)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
        }
    }
    bail!("could not create a trash entry for {}", session.id)
}

/// Removes an entry directory that holds no items any more: its manifest,
/// its lock, leftovers of interrupted copies, and the directories, never
/// recursively. Anything else keeps the entry. The caller holds the lock.
fn discard_entry_dir(dir: &Path) {
    let items_dir = dir.join(ITEMS);
    remove_partials(&items_dir);
    if remove_if_empty(&items_dir) {
        let _ = fs::remove_file(dir.join(MANIFEST));
        let _ = fs::remove_file(dir.join(LOCK));
        remove_if_empty(dir);
    }
}

/// Best-effort flush of the directory that holds `path`.
fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        fsutil::sync_dir(parent);
    }
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && !key.starts_with('.') && !key.contains(['/', '\\']) && Path::new(key).components().count() == 1
}

fn create_dir_private(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Removes `dir` only if it is empty. Returns whether it is gone.
fn remove_if_empty(dir: &Path) -> bool {
    fs::remove_dir(dir).is_ok() || !fsutil::occupied(dir)
}

/// Like [`fsutil::resolve_inside`] for a path that may not exist yet: the
/// nearest existing ancestor must resolve inside `root`.
fn resolve_for_create(path: &Path, root: &Path) -> Result<PathBuf> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        bail!("{} is not an absolute path", path.display());
    }
    let real_root = fs::canonicalize(root).with_context(|| format!("resolving {}", root.display()))?;
    let mut existing = path.parent().context("no parent")?;
    let mut missing = vec![path.file_name().context("no file name")?];
    while !fsutil::occupied(existing) {
        missing.push(existing.file_name().context("no file name")?);
        existing = existing.parent().context("no parent")?;
    }
    let real = fs::canonicalize(existing)?;
    if !real.starts_with(&real_root) {
        bail!("{} is not inside {}", path.display(), root.display());
    }
    let resolved = missing.iter().rev().fold(real, |acc, part| acc.join(part));
    if resolved == real_root {
        bail!("{} is the agent directory itself", path.display());
    }
    Ok(resolved)
}

/// An exclusive lock on one entry, held for a whole delete, restore or
/// purge, so two entropeek processes never work on the same entry at once.
///
/// The operating system holds it (`flock` on the entry's `lock` file) for as
/// long as the file stays open, and releases it when the process exits, so
/// a crash never leaves a stale lock and a lock is never "stolen" from a
/// live process. The lock file itself is only removed together with its
/// entry, by the holder.
#[cfg(unix)]
struct EntryLock {
    _file: File,
}

#[cfg(unix)]
impl EntryLock {
    fn acquire(dir: &Path) -> Result<EntryLock> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let path = dir.join(LOCK);
        for _ in 0..8 {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)
                .with_context(|| format!("locking {}", dir.display()))?;
            match lock_briefly(&file) {
                Ok(()) => {}
                Err(err) if err.is::<Busy>() => return Err(err),
                Err(err) => return Err(err.context(format!("locking {}", dir.display()))),
            }
            // The holder may have removed the lock file (with its entry)
            // between our open and our lock: then we locked a file nobody
            // else will ever open, and must try again with the current one.
            let held = file.metadata()?;
            match fs::symlink_metadata(&path) {
                Ok(now) if now.dev() == held.dev() && now.ino() == held.ino() => return Ok(EntryLock { _file: file }),
                _ => continue,
            }
        }
        bail!("could not lock {}", dir.display())
    }
}

/// How long a contended lock is retried before the entry counts as busy.
/// A just-started child process can briefly share our open lock file (from
/// `fork` until it runs its program); real operations hold it far longer.
#[cfg(unix)]
const LOCK_PATIENCE: std::time::Duration = std::time::Duration::from_millis(100);

/// Takes the exclusive lock on `file`, or fails with [`Busy`].
#[cfg(unix)]
fn lock_briefly(file: &File) -> Result<()> {
    use std::os::fd::AsRawFd;
    let deadline = std::time::Instant::now() + LOCK_PATIENCE;
    loop {
        // SAFETY: `file` is an open file descriptor for the whole call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::WouldBlock {
            return Err(err.into());
        }
        if std::time::Instant::now() >= deadline {
            return Err(Busy.into());
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Elsewhere: an exclusively created lock file, treated as stale after ten
/// minutes.
#[cfg(not(unix))]
struct EntryLock(PathBuf);

#[cfg(not(unix))]
impl EntryLock {
    fn acquire(dir: &Path) -> Result<EntryLock> {
        let path = dir.join(LOCK);
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(EntryLock(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .is_ok_and(|t| t.elapsed().unwrap_or_default() > std::time::Duration::from_secs(600));
                    if !stale {
                        return Err(Busy.into());
                    }
                    let _ = fs::remove_file(&path);
                }
                Err(e) => return Err(e).with_context(|| format!("locking {}", dir.display())),
            }
        }
        bail!("could not lock {}", dir.display())
    }
}

#[cfg(not(unix))]
impl Drop for EntryLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn write_manifest(dir: &Path, entry: &TrashEntry) -> Result<()> {
    let json = serde_json::to_vec_pretty(entry)?;
    let path = dir.join(MANIFEST);
    fsutil::write_private(&path, &json).with_context(|| format!("writing {}", path.display()))
}

/// Reads and validates an entry's manifest.
fn read_entry(dir: &Path, key: &str) -> Result<TrashEntry> {
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        bail!("not a directory");
    }
    let path = dir.join(MANIFEST);
    match fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() => {}
        Ok(_) => bail!("{MANIFEST} is not a regular file"),
        Err(_) => bail!("no {MANIFEST} (an interrupted delete that moved nothing, or not an entropeek entry)"),
    }
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let entry: TrashEntry = serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    validate(&entry, key)?;
    Ok(entry)
}

/// Checks a manifest. Linear in the number of items (plus a sort), so an
/// entry with thousands of sub-agent files still lists quickly.
fn validate(entry: &TrashEntry, key: &str) -> Result<()> {
    if !(1..=MANIFEST_VERSION).contains(&entry.version) {
        bail!("unsupported manifest version {}", entry.version);
    }
    if entry.key != key {
        bail!("manifest key {:?} does not match its directory {key:?}", entry.key);
    }
    if entry.items.is_empty() {
        bail!("manifest lists no items");
    }
    let mut stored = HashSet::with_capacity(entry.items.len());
    for item in &entry.items {
        if item.stored.is_empty() || !item.stored.bytes().all(|b| b.is_ascii_digit()) {
            bail!("invalid stored name {:?}", item.stored);
        }
        if !stored.insert(item.stored.as_str()) {
            bail!("stored name {:?} is listed twice", item.stored);
        }
        let original = &item.original;
        if !original.is_absolute()
            || original.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir))
            || original.file_name().is_none()
        {
            bail!("invalid original path {}", original.display());
        }
    }
    // Sorted by components, a path's descendants come right after it, so
    // checking neighbours finds every overlap (including duplicates).
    let mut originals: Vec<&Path> = entry.items.iter().map(|i| i.original.as_path()).collect();
    originals.sort();
    for pair in originals.windows(2) {
        if pair[1].starts_with(pair[0]) {
            bail!("{} and {} overlap", pair[0].display(), pair[1].display());
        }
    }
    Ok(())
}

/// Renames `from` to `to` without replacing anything, falling back to a
/// careful copy + delete across filesystems. Both paths must be fully
/// resolved: a link anywhere on the way to either makes the move fail.
pub fn move_path(from: &Path, to: &Path) -> Result<(), MoveError> {
    move_with(from, to, fsutil::rename_noreplace_nofollow)
}

/// [`move_path`] with the rename step injectable, so tests can force the
/// cross-filesystem path.
#[doc(hidden)]
pub fn move_with(from: &Path, to: &Path, rename: impl Fn(&Path, &Path) -> io::Result<()>) -> Result<(), MoveError> {
    match rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => move_across(from, to, &|| {}),
        Err(e) => Err(e.into()),
    }
}

/// Names of in-progress copies start with this, followed by
/// `<pid>-<n>-<final name>`.
const PARTIAL: &str = ".entropeek-partial-";

/// Copy + delete for moves between filesystems.
///
/// - The copy goes to a new, private staging name next to the destination.
///   An interrupted copy therefore never occupies the final name, so a
///   later restore is not blocked by it; leftovers of dead processes are
///   removed.
/// - Files are created private (0600) and only get their real permissions
///   and modification time once complete, through the same handle.
/// - Every file and directory in the source is compared before and after
///   copying (identity, size, times). If anything changed, or the directory
///   holding the source was swapped, the copy is discarded and the source
///   is kept.
/// - The finished copy is flushed to disk and published under the final
///   name without replacing anything; only then is the source removed.
///
/// Writers that keep appending through a file they already have open
/// after the final check can still lose those last bytes when the source
/// is removed; entropeek cannot see them. If the copy is complete but the
/// source cannot be fully removed, the error says `completed`, so the
/// caller knows the destination is the whole item.
fn move_across(from: &Path, to: &Path, after_copy: &dyn Fn()) -> Result<(), MoveError> {
    let from_parent = from.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let to_parent = to.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let to_name = to.file_name().ok_or_else(|| io::Error::other(format!("{} has no file name", to.display())))?;
    #[cfg(unix)]
    let parent_id = fsutil::dir_identity(from_parent)?;
    remove_dead_partials(to_parent, to_name);
    if fsutil::occupied(to) {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already exists", to.display())).into());
    }
    let before = inventory(from)?;
    let staging = staging_path(to_parent, to_name)?;
    if let Err(err) = copy_tree(from, &staging) {
        let _ = remove_tree(&staging);
        return Err(err.into());
    }
    after_copy();
    #[cfg(unix)]
    let same_parent = fsutil::dir_identity(from_parent).is_ok_and(|now| now == parent_id);
    #[cfg(not(unix))]
    let same_parent = true;
    if !same_parent || inventory(from).ok().as_ref() != Some(&before) {
        let _ = remove_tree(&staging);
        return Err(io::Error::other(format!("{} changed while it was being moved", from.display())).into());
    }
    if let Err(err) = fsutil::rename_noreplace_nofollow(&staging, to) {
        let _ = remove_tree(&staging);
        return Err(err.into());
    }
    if let Err(err) = fsutil::sync_dir_strict(to_parent) {
        let _ = remove_tree(to);
        return Err(err.into());
    }
    remove_tree(from).map_err(|error| MoveError { error, completed: true })?;
    fsutil::sync_dir(from_parent);
    Ok(())
}

/// A unique staging name for a copy that will become `dir/name`.
fn staging_path(dir: &Path, name: &std::ffi::OsStr) -> io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut staged = std::ffi::OsString::from(format!("{PARTIAL}{}-{n}-", std::process::id()));
    staged.push(name);
    Ok(dir.join(staged))
}

/// The process that made a staging copy, if `file_name` is one for `name`.
fn partial_owner(file_name: &std::ffi::OsStr, name: Option<&std::ffi::OsStr>) -> Option<u32> {
    let text = file_name.to_str()?;
    let rest = text.strip_prefix(PARTIAL)?;
    let (pid, rest) = rest.split_once('-')?;
    let (_, final_name) = rest.split_once('-')?;
    if let Some(name) = name
        && Some(final_name) != name.to_str()
    {
        return None;
    }
    pid.parse().ok()
}

/// Removes staging copies of `dir/name` left by processes that died.
fn remove_dead_partials(dir: &Path, name: &std::ffi::OsStr) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if let Some(pid) = partial_owner(&entry.file_name(), Some(name))
            && !process_alive(pid)
        {
            let _ = remove_tree(&entry.path());
        }
    }
}

/// Removes every staging copy in an entry's `items` directory. Only the
/// holder of the entry's lock calls this, so none of them is in use.
fn remove_partials(items_dir: &Path) {
    let Ok(entries) = fs::read_dir(items_dir) else { return };
    for entry in entries.flatten() {
        if partial_owner(&entry.file_name(), None).is_some() {
            let _ = remove_tree(&entry.path());
        }
    }
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
        // SAFETY: signal 0 only checks whether the process exists.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// What identifies one version of a file, directory or link.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    kind: u8,
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    id: (u64, u64, i64, i64, u32),
}

impl Snapshot {
    fn of(meta: &fs::Metadata) -> Snapshot {
        let kind = meta.file_type();
        Snapshot {
            kind: if kind.is_symlink() {
                2
            } else if kind.is_dir() {
                1
            } else {
                0
            },
            len: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            id: {
                use std::os::unix::fs::MetadataExt;
                (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec(), meta.mode())
            },
        }
    }
}

/// Every path below `root` (relative), with its snapshot, sorted.
fn inventory(root: &Path) -> io::Result<Vec<(PathBuf, Snapshot)>> {
    fn walk(root: &Path, rel: PathBuf, out: &mut Vec<(PathBuf, Snapshot)>) -> io::Result<()> {
        let path = if rel.as_os_str().is_empty() { root.to_path_buf() } else { root.join(&rel) };
        let meta = fs::symlink_metadata(&path)?;
        let is_dir = meta.is_dir();
        out.push((rel.clone(), Snapshot::of(&meta)));
        if is_dir {
            for child in fs::read_dir(&path)? {
                walk(root, rel.join(child?.file_name()), out)?;
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, PathBuf::new(), &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn remove_tree(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) }
}

/// Copies `from` to the new path `to`, which must not exist. Directories
/// and files are created private and get their permissions and times last,
/// through the handle that created them, so restrictive modes (0444, 0555)
/// cannot get in the way. File data and every directory are flushed.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    let kind = meta.file_type();
    let times = meta.modified().ok().map(|m| fs::FileTimes::new().set_modified(m));
    if kind.is_symlink() {
        let target = fs::read_link(from)?;
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, to);
        #[cfg(not(unix))]
        return Err(io::Error::new(io::ErrorKind::Unsupported, format!("cannot copy link {}", target.display())));
    }
    if kind.is_dir() {
        create_dir_private(to)?;
        for child in fs::read_dir(from)? {
            let child = child?;
            copy_tree(&child.path(), &to.join(child.file_name()))?;
        }
        let dir = File::open(to)?;
        if let Some(times) = times {
            dir.set_times(times)?;
        }
        fsutil::sync_handle(&dir)?;
        dir.set_permissions(meta.permissions())?;
        return Ok(());
    }
    if kind.is_file() {
        let mut src = File::open(from)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut dst = options.open(to)?;
        io::copy(&mut src, &mut dst)?;
        if let Some(times) = times {
            dst.set_times(times)?;
        }
        dst.set_permissions(meta.permissions())?;
        dst.sync_all()?;
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{} is not a regular file, directory or link", from.display()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, sample_session};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        trash_dir: PathBuf,
        session: Session,
        paths: Vec<PathBuf>,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("agent");
        let file = root.join("projects/p/s1.jsonl");
        let extra = root.join("projects/p/s1");
        let env_dir = root.join("session-env/s1");
        fs::create_dir_all(extra.join("subagents")).unwrap();
        fs::create_dir_all(&env_dir).unwrap();
        fs::write(&file, b"{\"a\":1}\n").unwrap();
        fs::write(extra.join("subagents/x.jsonl"), b"sub").unwrap();
        fs::write(env_dir.join("env"), b"env").unwrap();
        let mut session = sample_session(AgentKind::Claude, "s1");
        session.path = file.clone();
        Fixture { root, trash_dir: base.join("trash"), session, paths: vec![file, extra, env_dir], _dir: dir }
    }

    impl Fixture {
        fn trash(&self) -> Trash {
            Trash::new(self.trash_dir.clone())
        }

        fn roots(&self) -> Vec<PathBuf> {
            vec![self.root.clone()]
        }

        /// Every piece of data is either in place or in a listed trash entry.
        fn assert_recoverable(&self) {
            let listing = self.trash().scan().unwrap();
            assert!(listing.invalid.is_empty(), "{:?}", listing.invalid);
            let wanted: [(&Path, &str, &[u8]); 3] = [
                (&self.paths[0], "", b"{\"a\":1}\n"),
                (&self.paths[1], "subagents/x.jsonl", b"sub"),
                (&self.paths[2], "env", b"env"),
            ];
            for (original, inner, bytes) in wanted {
                let at = |base: &Path| if inner.is_empty() { base.to_path_buf() } else { base.join(inner) };
                let in_place = fs::read(at(original)).ok().as_deref() == Some(bytes);
                let in_trash = listing.entries.iter().any(|e| {
                    e.items.iter().any(|i| {
                        i.original == original
                            && fs::read(at(&self.trash_dir.join(&e.key).join(ITEMS).join(&i.stored))).ok().as_deref()
                                == Some(bytes)
                    })
                });
                assert!(in_place || in_trash, "{} was lost", original.display());
            }
        }
    }

    #[test]
    fn put_then_restore_round_trips() {
        let f = fixture();
        let trash = f.trash();
        let mtime = fs::metadata(&f.paths[0]).unwrap().modified().unwrap();
        let entry = trash.put(&f.session, &f.paths, &f.root).unwrap();
        assert!(f.paths.iter().all(|p| !p.exists()), "originals are gone");
        assert_eq!(entry.items.len(), 3);
        assert_eq!(entry.state, EntryState::Trashed);
        assert_eq!(trash.list().unwrap(), vec![entry.clone()]);

        let restored = trash.restore(&entry.key, &f.roots()).unwrap();
        assert_eq!(restored.session, f.session);
        assert_eq!(fs::read(&f.paths[0]).unwrap(), b"{\"a\":1}\n");
        assert_eq!(fs::read(f.paths[1].join("subagents/x.jsonl")).unwrap(), b"sub");
        assert_eq!(fs::metadata(&f.paths[0]).unwrap().modified().unwrap(), mtime, "mtime survives");
        assert!(trash.list().unwrap().is_empty());
        assert!(!f.trash_dir.join(&entry.key).exists());
    }

    #[cfg(unix)]
    #[test]
    fn trash_is_private_even_if_it_existed() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture();
        fs::create_dir_all(&f.trash_dir).unwrap();
        fs::set_permissions(&f.trash_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let entry = f.trash().put(&f.session, &f.paths, &f.root).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&f.trash_dir), 0o700);
        assert_eq!(mode(&f.trash_dir.join(&entry.key)), 0o700);
        assert_eq!(mode(&f.trash_dir.join(&entry.key).join(MANIFEST)), 0o600);
    }

    #[test]
    fn refuses_paths_outside_the_agent_root() {
        let f = fixture();
        let outside = f.root.parent().unwrap().join("elsewhere.txt");
        fs::write(&outside, b"keep me").unwrap();
        let trash = f.trash();
        let err = trash.put(&f.session, &[f.paths[0].clone(), outside.clone()], &f.root).unwrap_err();
        assert!(err.to_string().contains("not inside"), "{err}");
        assert!(outside.exists() && f.paths[0].exists(), "nothing moved");
        let sneaky = f.root.join("projects/../../elsewhere.txt");
        assert!(trash.put(&f.session, &[sneaky], &f.root).is_err());
        assert!(trash.put(&f.session, std::slice::from_ref(&f.root), &f.root).is_err());
        assert!(trash.scan().unwrap().entries.is_empty() && trash.scan().unwrap().invalid.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_files_reached_through_a_link_to_outside() {
        let f = fixture();
        let outside = f.root.parent().unwrap().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("s.jsonl"), b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, f.root.join("projects/link")).unwrap();
        let err = f.trash().put(&f.session, &[f.root.join("projects/link/s.jsonl")], &f.root).unwrap_err();
        assert!(err.to_string().contains("not inside"), "{err}");
        assert_eq!(fs::read(outside.join("s.jsonl")).unwrap(), b"keep");
    }

    #[test]
    fn failed_move_rolls_back() {
        let f = fixture();
        let trash = f.trash();
        let missing = f.root.join("projects/p/not-there");
        let paths = vec![f.paths[0].clone(), missing];
        assert!(trash.put(&f.session, &paths, &f.root).is_err());
        assert!(f.paths[0].exists(), "the moved file was put back");
        assert!(trash.list().unwrap().is_empty());
        assert!(fs::read_dir(&f.trash_dir).unwrap().next().is_none(), "no debris left");
    }

    /// Fails the `fail_at`th move (counting rollback moves too), optionally
    /// after occupying the first original location.
    fn flaky(fail_at: usize, occupy: Option<PathBuf>) -> impl Fn(&Path, &Path) -> Result<(), MoveError> + Send + Sync {
        let calls = Arc::new(AtomicUsize::new(0));
        move |from: &Path, to: &Path| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n == fail_at {
                if let Some(path) = occupy.as_ref().filter(|p| !fsutil::occupied(p)) {
                    fs::create_dir_all(path).unwrap();
                }
                return Err(io::Error::other("injected failure").into());
            }
            move_path(from, to)
        }
    }

    #[test]
    fn injected_failures_never_lose_data() {
        // 3 forward moves + up to 2 rollback moves.
        for fail_at in 0..6 {
            for occupy in [false, true] {
                let f = fixture();
                let trash = Trash::with_mover(f.trash_dir.clone(), flaky(fail_at, occupy.then(|| f.paths[0].clone())));
                let result = trash.put(&f.session, &f.paths, &f.root);
                f.assert_recoverable();
                if result.is_ok() {
                    continue;
                }
                // Whatever is in the trash can be restored once the way is clear.
                if occupy {
                    let _ = fs::remove_dir(&f.paths[0]);
                }
                for entry in f.trash().list().unwrap() {
                    f.trash().restore(&entry.key, &f.roots()).unwrap();
                }
                f.assert_recoverable();
                assert!(f.paths.iter().all(|p| p.exists()), "fail_at={fail_at} occupy={occupy}");
            }
        }
    }

    #[test]
    fn rollback_onto_a_reappeared_original_keeps_the_trash_copy() {
        // The original location is recreated, then a
        // later move fails. Rollback cannot put the session back, so the
        // trash must keep it.
        let f = fixture();
        let trash = Trash::with_mover(f.trash_dir.clone(), flaky(2, Some(f.paths[0].clone())));
        let err = trash.put(&f.session, &f.paths, &f.root).unwrap_err();
        assert!(format!("{err:#}").contains("kept safely in the trash"), "{err:#}");
        let entries = f.trash().list().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].items.len(), 1);
        assert_eq!(entries[0].items[0].original, f.paths[0]);
        f.assert_recoverable();
        assert!(f.paths[1].exists(), "the other item was put back");
    }

    #[test]
    fn restore_refuses_when_original_is_back() {
        let f = fixture();
        let trash = f.trash();
        let entry = trash.put(&f.session, &f.paths, &f.root).unwrap();
        fs::write(&f.paths[0], b"new").unwrap();
        let err = trash.restore(&entry.key, &f.roots()).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(fs::read(&f.paths[0]).unwrap(), b"new");
        assert!(!f.paths[1].exists(), "nothing was half-restored");
        assert_eq!(trash.list().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn restore_treats_a_dangling_link_as_occupied() {
        let f = fixture();
        let trash = f.trash();
        let entry = trash.put(&f.session, &f.paths, &f.root).unwrap();
        std::os::unix::fs::symlink("/nowhere", &f.paths[0]).unwrap();
        assert!(trash.restore(&entry.key, &f.roots()).is_err());
        assert!(fs::symlink_metadata(&f.paths[0]).unwrap().file_type().is_symlink());
        f.assert_recoverable();
    }

    #[test]
    fn restore_never_overwrites_a_file_created_after_the_check() {
        // A writer recreates the session file just before restore moves it.
        let f = fixture();
        let entry = f.trash().put(&f.session, &f.paths, &f.root).unwrap();
        let racer = f.paths[0].clone();
        let trash = Trash::with_mover(f.trash_dir.clone(), move |from, to| {
            if to == racer {
                fs::write(&racer, b"live data").unwrap();
            }
            move_path(from, to)
        });
        let err = trash.restore(&entry.key, &f.roots()).unwrap_err();
        assert!(format!("{err:#}").contains("exists"), "{err:#}");
        assert_eq!(fs::read(&f.paths[0]).unwrap(), b"live data", "the new file survives");
        let entries = f.trash().list().unwrap();
        assert_eq!(entries.len(), 1, "the entry is still there");
        assert_eq!(entries[0].state, EntryState::Trashed, "everything went back into the trash");
    }

    #[test]
    fn conflict_after_some_items_rolls_them_back() {
        let f = fixture();
        let entry = f.trash().put(&f.session, &f.paths, &f.root).unwrap();
        let racer = f.paths[2].clone();
        let trash = Trash::with_mover(f.trash_dir.clone(), move |from, to| {
            if to == racer {
                fs::create_dir_all(&racer).unwrap();
            }
            move_path(from, to)
        });
        assert!(trash.restore(&entry.key, &f.roots()).is_err());
        assert!(!f.paths[0].exists() && !f.paths[1].exists(), "earlier items went back to the trash");
        fs::remove_dir(&f.paths[2]).unwrap();
        f.trash().restore(&entry.key, &f.roots()).unwrap();
        assert!(f.paths.iter().all(|p| p.exists()));
    }

    #[test]
    fn interrupted_operations_are_listed_and_resumable() {
        let f = fixture();
        let trash = f.trash();
        let entry = trash.put(&f.session, &f.paths, &f.root).unwrap();
        // Simulate a crash in the middle of a restore: one item is back, the
        // manifest says `restoring`.
        let dir = f.trash_dir.join(&entry.key);
        let mut manifest = entry.clone();
        manifest.state = EntryState::Restoring;
        write_manifest(&dir, &manifest).unwrap();
        fs::rename(dir.join(ITEMS).join("0"), &f.paths[0]).unwrap();

        let listed = trash.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].is_incomplete());
        trash.restore(&entry.key, &f.roots()).unwrap();
        assert!(f.paths.iter().all(|p| p.exists()));
        assert!(trash.list().unwrap().is_empty());
    }

    #[test]
    fn interrupted_delete_is_visible_and_purgeable() {
        let f = fixture();
        let trash = f.trash();
        let entry = trash.put(&f.session, &f.paths, &f.root).unwrap();
        let dir = f.trash_dir.join(&entry.key);
        let mut manifest = entry.clone();
        manifest.state = EntryState::Deleting;
        write_manifest(&dir, &manifest).unwrap();
        fs::rename(dir.join(ITEMS).join("2"), &f.paths[2]).unwrap(); // never moved

        assert!(trash.list().unwrap()[0].is_incomplete());
        assert_eq!(trash.purge_older_than(i64::MAX).unwrap().count, 1);
        assert!(!dir.exists());
        assert!(f.paths[2].exists(), "items that never moved are untouched");
    }

    #[test]
    fn debris_and_invalid_entries_are_reported_not_hidden() {
        let f = fixture();
        let trash = f.trash();
        fs::create_dir_all(f.trash_dir.join("123-claude-empty/items")).unwrap();
        let bad = f.trash_dir.join("456-claude-bad");
        fs::create_dir_all(bad.join(ITEMS)).unwrap();
        fs::write(bad.join(ITEMS).join("0"), b"data").unwrap();
        fs::write(bad.join(MANIFEST), b"{not json").unwrap();
        let listing = trash.scan().unwrap();
        assert!(listing.entries.is_empty());
        assert_eq!(listing.invalid.len(), 2);
        trash.purge_older_than(i64::MAX).unwrap();
        assert!(!f.trash_dir.join("123-claude-empty").exists(), "empty debris is cleaned up");
        assert!(bad.join(ITEMS).join("0").exists(), "data in an invalid entry is never purged");
    }

    #[test]
    fn manifests_are_validated() {
        let f = fixture();
        let trash = f.trash();
        let entry = trash.put(&f.session, &f.paths, &f.root).unwrap();
        let dir = f.trash_dir.join(&entry.key);
        type Corrupt = Box<dyn Fn(&mut TrashEntry)>;
        let cases: Vec<(&str, Corrupt)> = vec![
            ("traversal stored name", Box::new(|e| e.items[0].stored = "../../x".into())),
            ("absolute stored name", Box::new(|e| e.items[0].stored = "/etc/passwd".into())),
            ("relative original", Box::new(|e| e.items[0].original = "rel/path".into())),
            ("dotdot original", Box::new(|e| e.items[0].original = "/a/../b".into())),
            ("mismatched key", Box::new(|e| e.key = "other".into())),
            ("duplicate stored", Box::new(|e| e.items[1].stored = e.items[0].stored.clone())),
            ("nested originals", Box::new(|e| e.items[1].original = e.items[0].original.join("x"))),
            ("future version", Box::new(|e| e.version = 99)),
            ("no items", Box::new(|e| e.items.clear())),
        ];
        for (label, corrupt) in cases {
            let mut bad = entry.clone();
            corrupt(&mut bad);
            write_manifest(&dir, &bad).unwrap();
            let listing = trash.scan().unwrap();
            assert!(listing.entries.is_empty(), "{label} was accepted");
            assert_eq!(listing.invalid.len(), 1, "{label}");
            assert!(trash.restore(&entry.key, &f.roots()).is_err(), "{label}");
        }
        // Originals outside the agent roots are refused at restore time.
        let mut moved = entry.clone();
        moved.items[0].original = f.root.parent().unwrap().join("elsewhere");
        write_manifest(&dir, &moved).unwrap();
        let err = trash.restore(&entry.key, &f.roots()).unwrap_err();
        assert!(err.to_string().contains("not inside"), "{err}");
    }

    #[test]
    fn old_manifests_are_still_readable() {
        let f = fixture();
        let trash = f.trash();
        let entry = trash.put(&f.session, &f.paths, &f.root).unwrap();
        let dir = f.trash_dir.join(&entry.key);
        let mut json: serde_json::Value = serde_json::from_slice(&fs::read(dir.join(MANIFEST)).unwrap()).unwrap();
        json.as_object_mut().unwrap().remove("version");
        json.as_object_mut().unwrap().remove("state");
        fs::write(dir.join(MANIFEST), serde_json::to_vec(&json).unwrap()).unwrap();
        let listed = trash.list().unwrap();
        assert_eq!((listed[0].version, listed[0].state), (1, EntryState::Trashed));
        trash.restore(&entry.key, &f.roots()).unwrap();
    }

    #[test]
    fn concurrent_restores_are_locked() {
        let f = fixture();
        let trash = f.trash();
        let entry = trash.put(&f.session, &f.paths, &f.root).unwrap();
        let dir = f.trash_dir.join(&entry.key);
        let held = EntryLock::acquire(&dir).unwrap();
        let err = trash.restore(&entry.key, &f.roots()).unwrap_err();
        assert!(err.to_string().contains("another entropeek process"), "{err}");
        drop(held);
        // A lock left behind by a dead process is ignored.
        fs::write(dir.join(LOCK), format!("{}\n", i32::MAX)).unwrap();
        trash.restore(&entry.key, &f.roots()).unwrap();
    }

    #[test]
    fn find_by_key_or_session_id_and_purge() {
        let f = fixture();
        let trash = f.trash();
        let entry = trash.put(&f.session, &f.paths, &f.root).unwrap();
        assert_eq!(trash.find(&entry.key).unwrap(), entry);
        assert_eq!(trash.find(" s1 ").unwrap(), entry);
        assert!(trash.find("nope").is_err());
        assert!(trash.restore("../etc", &f.roots()).is_err());
        assert_eq!(trash.purge_older_than(0).unwrap().count, 0);
        assert_eq!(trash.purge_older_than(i64::MAX).unwrap().count, 1);
        assert!(trash.list().unwrap().is_empty());
    }

    #[test]
    fn same_session_can_be_trashed_twice() {
        let f = fixture();
        let trash = f.trash();
        let first = trash.put(&f.session, &f.paths[..1], &f.root).unwrap();
        fs::write(&f.paths[0], b"again").unwrap();
        let second = trash.put(&f.session, &f.paths[..1], &f.root).unwrap();
        assert_ne!(first.key, second.key);
        assert_eq!(trash.list().unwrap().len(), 2);
    }

    // ----- moves across filesystems (forced through the copy path) -----

    fn across(from: &Path, to: &Path) -> Result<(), MoveError> {
        move_with(from, to, |_, _| Err(io::Error::from(io::ErrorKind::CrossesDevices)))
    }

    #[cfg(unix)]
    #[test]
    fn cross_device_round_trip_keeps_tree_links_modes_and_times() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let f = fixture();
        let extra = &f.paths[1];
        symlink("x.jsonl", extra.join("subagents/link")).unwrap();
        symlink("/definitely/missing", extra.join("broken")).unwrap();
        symlink("..", extra.join("subagents/cycle")).unwrap();
        fs::set_permissions(extra.join("subagents/x.jsonl"), fs::Permissions::from_mode(0o640)).unwrap();
        let mtime = |p: &Path| fs::symlink_metadata(p).unwrap().modified().unwrap();
        let file_time = mtime(&f.paths[0]);
        let sub_time = mtime(&extra.join("subagents"));

        let trash = Trash::with_mover(f.trash_dir.clone(), across);
        let entry = trash.put(&f.session, &f.paths, &f.root).unwrap();
        assert!(f.paths.iter().all(|p| !p.exists()));
        trash.restore(&entry.key, &f.roots()).unwrap();

        f.assert_recoverable();
        assert_eq!(mtime(&f.paths[0]), file_time);
        assert_eq!(mtime(&extra.join("subagents")), sub_time);
        assert_eq!(fs::read_link(extra.join("subagents/link")).unwrap(), Path::new("x.jsonl"));
        assert_eq!(fs::read_link(extra.join("broken")).unwrap(), Path::new("/definitely/missing"));
        assert_eq!(fs::read_link(extra.join("subagents/cycle")).unwrap(), Path::new(".."));
        let mode = fs::metadata(extra.join("subagents/x.jsonl")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn cross_device_refuses_special_files_and_keeps_the_source() {
        let f = fixture();
        let fifo = f.paths[1].join("pipe");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: valid path string; mkfifo only creates a node.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let trash = Trash::with_mover(f.trash_dir.clone(), across);
        let err = trash.put(&f.session, &f.paths, &f.root).unwrap_err();
        assert!(format!("{err:#}").contains("not a regular file"), "{err:#}");
        f.assert_recoverable();
        assert!(f.paths.iter().all(|p| p.exists()), "everything was put back");
        assert!(fifo.exists());
        assert!(trash.list().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn cross_device_partial_source_removal_keeps_the_complete_copy() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture();
        let locked = f.paths[1].join("subagents");
        // Children of a read-only directory cannot be removed.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        let trash = Trash::with_mover(f.trash_dir.clone(), across);
        let result = trash.put(&f.session, &f.paths, &f.root);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
        f.assert_recoverable();
        let entries = f.trash().list().unwrap();
        assert_eq!(entries.len(), 1, "the complete copy stayed in the trash");
        assert!(entries[0].items.iter().any(|i| i.original == f.paths[1]));
    }

    #[cfg(unix)]
    #[test]
    fn cross_device_copy_failure_removes_the_partial_copy() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture();
        let secret = f.paths[1].join("subagents/x.jsonl");
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
        let dest = f.trash_dir.join("partial");
        fs::create_dir_all(&f.trash_dir).unwrap();
        let err = across(&f.paths[1], &dest).unwrap_err();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!err.completed);
        assert!(!dest.exists(), "partial copy removed");
        f.assert_recoverable();
    }

    // ----- locking, link swaps, unreadable trash, large manifests, staged copies -----

    /// The key of the only entry in the trash.
    fn only_key(trash_dir: &Path) -> String {
        let mut keys: Vec<String> =
            fs::read_dir(trash_dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(keys.len(), 1, "{keys:?}");
        keys.pop().unwrap()
    }

    #[test]
    fn an_entry_being_filled_cannot_be_purged_or_restored() {
        // Another process purges and restores while this delete is moving files.
        let f = fixture();
        let trash_dir = f.trash_dir.clone();
        let roots = f.roots();
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = attempts.clone();
        let trash = Trash::with_mover(f.trash_dir.clone(), move |from, to| {
            move_path(from, to)?;
            let other = Trash::new(trash_dir.clone());
            let key = only_key(&trash_dir);
            let purge = other.purge(&key).unwrap_err();
            assert!(purge.is::<Busy>(), "{purge:#}");
            assert_eq!(other.purge_older_than(i64::MAX).unwrap(), Purged { count: 0, busy: 1 });
            assert!(other.restore(&key, &roots).unwrap_err().is::<Busy>());
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let entry = trash.put(&f.session, &f.paths, &f.root).unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        f.assert_recoverable();
        assert!(f.paths.iter().all(|p| !p.exists()));
        // Once the delete is done, the entry is purgeable as usual.
        f.trash().purge(&entry.key).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_without_a_pid_is_not_stolen() {
        // The state between creating a lock file and recording anything in it.
        use std::os::fd::AsRawFd;
        let f = fixture();
        let entry = f.trash().put(&f.session, &f.paths, &f.root).unwrap();
        let dir = f.trash_dir.join(&entry.key);
        let other = OpenOptions::new().write(true).create(true).truncate(true).open(dir.join(LOCK)).unwrap();
        // SAFETY: valid open descriptor.
        assert_eq!(unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        assert!(f.trash().restore(&entry.key, &f.roots()).unwrap_err().is::<Busy>());
        assert!(f.paths.iter().all(|p| !p.exists()), "nothing was restored under someone else's lock");
        drop(other);
        f.trash().restore(&entry.key, &f.roots()).unwrap();
        assert!(!dir.exists(), "the entry, lock file included, is gone after a restore");
    }

    #[cfg(unix)]
    #[test]
    fn a_parent_swapped_for_a_link_before_the_move_is_refused() {
        let f = fixture();
        let outside = f.root.parent().unwrap().join("outside");
        fs::create_dir_all(outside.join("p")).unwrap();
        fs::write(outside.join("p/s1.jsonl"), b"outside").unwrap();
        let projects = f.root.join("projects");
        let renamed = f.root.join("renamed");
        let (p, r, o) = (projects.clone(), renamed.clone(), outside.clone());
        let swapped = Arc::new(AtomicUsize::new(0));
        let once = swapped.clone();
        let trash = Trash::with_mover(f.trash_dir.clone(), move |from, to| {
            if once.fetch_add(1, Ordering::SeqCst) == 0 {
                fs::rename(&p, &r).unwrap();
                std::os::unix::fs::symlink(&o, &p).unwrap();
            }
            move_path(from, to)
        });
        let err = trash.put(&f.session, &f.paths, &f.root).unwrap_err();
        assert!(format!("{err:#}").contains("no longer the directory"), "{err:#}");
        assert_eq!(fs::read(outside.join("p/s1.jsonl")).unwrap(), b"outside", "outside is untouched");
        assert_eq!(fs::read(renamed.join("p/s1.jsonl")).unwrap(), b"{\"a\":1}\n", "the real file stays put");
        assert!(f.trash().list().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_trash_is_an_error_not_empty() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture();
        f.trash().put(&f.session, &f.paths, &f.root).unwrap();
        fs::set_permissions(&f.trash_dir, fs::Permissions::from_mode(0o000)).unwrap();
        let denied = fs::read_dir(&f.trash_dir).is_err(); // root can read anyway
        let result = f.trash().scan();
        let purge = f.trash().purge_older_than(i64::MAX);
        fs::set_permissions(&f.trash_dir, fs::Permissions::from_mode(0o700)).unwrap();
        if denied {
            assert!(result.is_err() && purge.is_err());
        }
        assert_eq!(f.trash().list().unwrap().len(), 1);
        let missing = Trash::new(f.trash_dir.join("never-created"));
        assert!(missing.scan().unwrap().entries.is_empty(), "a trash that does not exist yet is empty");
    }

    #[test]
    fn large_manifests_validate_quickly_and_still_catch_overlaps() {
        let mut entry = TrashEntry {
            version: MANIFEST_VERSION,
            key: "k".into(),
            deleted_at: 0,
            state: EntryState::Trashed,
            root: None,
            session: sample_session(AgentKind::Codex, "s"),
            items: (0..20_000)
                .map(|n| TrashedItem { original: format!("/a/sessions/{n:05}.jsonl").into(), stored: n.to_string() })
                .collect(),
        };
        let started = std::time::Instant::now();
        validate(&entry, "k").unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
        entry.items[7].original = "/a/sessions".into();
        assert!(validate(&entry, "k").unwrap_err().to_string().contains("overlap"));
        entry.items[7].original = "/a/sessions/00003.jsonl".into();
        assert!(validate(&entry, "k").unwrap_err().to_string().contains("overlap"), "duplicates overlap");
        entry.items[7].original = "/a/sessions/00007.jsonl".into();
        entry.items[9].stored = "8".into();
        assert!(validate(&entry, "k").unwrap_err().to_string().contains("twice"));
    }

    #[cfg(unix)]
    #[test]
    fn cross_device_move_keeps_a_directory_that_changed_while_copying() {
        let f = fixture();
        let tree = f.paths[1].clone();
        let child = tree.join("subagents/x.jsonl");
        let dest = f.trash_dir.join("dest");
        fs::create_dir_all(&f.trash_dir).unwrap();
        type Change = Box<dyn Fn(&Path)>;
        let changes: Vec<(&str, Change)> = vec![
            (
                "append",
                Box::new(|c: &Path| {
                    use std::io::Write;
                    OpenOptions::new().append(true).open(c).unwrap().write_all(b" more").unwrap();
                }),
            ),
            ("add", Box::new(|c: &Path| fs::write(c.with_file_name("new.jsonl"), b"new").unwrap())),
            (
                "replace",
                Box::new(|c: &Path| {
                    fs::write(c.with_file_name("tmp"), b"sub").unwrap();
                    fs::rename(c.with_file_name("tmp"), c).unwrap();
                }),
            ),
        ];
        for (label, change) in changes {
            let err = move_across(&tree, &dest, &|| change(&child)).unwrap_err();
            assert!(!err.completed, "{label}");
            assert!(err.error.to_string().contains("changed while"), "{label}: {}", err.error);
            assert!(!dest.exists(), "{label}: no copy published");
            assert!(child.exists(), "{label}: source kept");
            let leftovers: Vec<_> = fs::read_dir(&f.trash_dir).unwrap().flatten().collect();
            assert!(leftovers.is_empty(), "{label}: staging removed");
        }
        // Unchanged, it moves.
        move_across(&tree, &dest, &|| {}).unwrap();
        assert!(!tree.exists() && dest.join("subagents/x.jsonl").exists());
    }

    #[cfg(unix)]
    #[test]
    fn cross_device_moves_keep_read_only_times_and_private_modes() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture();
        let file = f.paths[0].clone();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        File::options().write(true).open(&file).unwrap().set_times(fs::FileTimes::new().set_modified(old)).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
        let tree = f.paths[1].join("subagents");
        fs::set_permissions(&tree, fs::Permissions::from_mode(0o500)).unwrap();
        fs::create_dir_all(&f.trash_dir).unwrap();
        let dest = f.trash_dir.join("file");
        across(&file, &dest).unwrap();
        let meta = fs::metadata(&dest).unwrap();
        assert_eq!(meta.modified().unwrap(), old, "a read-only file keeps its time");
        assert_eq!(meta.permissions().mode() & 0o777, 0o444);
        let copy = f.trash_dir.join("tree");
        let result = move_across(&tree, &copy, &|| {});
        fs::set_permissions(&tree, fs::Permissions::from_mode(0o700)).ok();
        assert!(result.unwrap_err().completed, "copied; the read-only source could not be emptied");
        assert_eq!(fs::metadata(&copy).unwrap().permissions().mode() & 0o777, 0o500);
        fs::set_permissions(&copy, fs::Permissions::from_mode(0o700)).unwrap();
        // Private files stay private (copies are created 0600 before any
        // data is written, and get their final mode last).
        let staging = staging_path(&f.trash_dir, std::ffi::OsStr::new("x")).unwrap();
        let secret = f.root.join("secret");
        fs::write(&secret, b"s").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        copy_tree(&secret, &staging).unwrap();
        assert_eq!(fs::metadata(&staging).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn an_interrupted_cross_device_restore_can_be_finished() {
        let f = fixture();
        let trash = Trash::with_mover(f.trash_dir.clone(), across);
        let entry = trash.put(&f.session, &f.paths, &f.root).unwrap();
        // A restore was killed while copying the session file back: the
        // manifest says `restoring` and a partial staging copy is left over.
        let dir = f.trash_dir.join(&entry.key);
        let mut manifest = entry.clone();
        manifest.state = EntryState::Restoring;
        write_manifest(&dir, &manifest).unwrap();
        let dead = format!("{PARTIAL}{}-0-s1.jsonl", i32::MAX);
        let leftover = f.paths[0].with_file_name(&dead);
        fs::write(&leftover, b"{\"a\"").unwrap();
        // So was a delete, leaving a partial copy inside the entry.
        fs::write(dir.join(ITEMS).join(format!("{PARTIAL}{}-1-9", i32::MAX)), b"x").unwrap();

        assert!(trash.list().unwrap()[0].is_incomplete());
        trash.restore(&entry.key, &f.roots()).unwrap();
        f.assert_recoverable();
        assert_eq!(fs::read(&f.paths[0]).unwrap(), b"{\"a\":1}\n");
        assert!(!leftover.exists(), "a dead process's partial copy is removed");
        assert!(!dir.exists(), "and so is the finished entry");
    }
}
