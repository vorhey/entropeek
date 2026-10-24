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
//! - Every path must resolve inside the agent's directory.

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

        let deleted_at = timefmt::now_ms();
        let (key, dir) = self.new_entry_dir(deleted_at, session)?;
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
        let setup =
            create_dir_private(&items_dir).and_then(|()| write_manifest(&dir, &entry).map_err(io::Error::other));
        if let Err(err) = setup {
            remove_if_empty(&dir);
            return Err(err).context("creating the trash entry");
        }

        let mut moved: Vec<usize> = Vec::new();
        for (n, item) in entry.items.iter().enumerate() {
            // Re-check right before moving, in case a directory was swapped
            // for a link since the plan was made.
            let failure = match fsutil::resolve_inside(&paths[n], allowed_root) {
                Ok(now) if now == item.original => match (self.mover)(&item.original, &items_dir.join(&item.stored)) {
                    Ok(()) => {
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
        Ok(entry)
    }

    /// Undoes a failed delete as far as possible without losing anything.
    fn abandon_put(&self, dir: &Path, mut entry: TrashEntry, moved: &[usize], failure: anyhow::Error) -> anyhow::Error {
        let items_dir = dir.join(ITEMS);
        let mut kept = Vec::new();
        for &n in moved.iter().rev() {
            let item = &entry.items[n];
            if let Err(err) = (self.mover)(&items_dir.join(&item.stored), &item.original) {
                // A completed copy back means the original is whole again;
                // the leftover in the trash is kept, never deleted.
                kept.push((n, err));
            }
        }
        if kept.is_empty() {
            let _ = fs::remove_file(dir.join(MANIFEST));
            remove_if_empty(&items_dir);
            remove_if_empty(dir);
            return failure;
        }
        let keep: Vec<usize> = kept.iter().map(|(n, _)| *n).collect();
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

    fn new_entry_dir(&self, deleted_at: i64, session: &Session) -> Result<(String, PathBuf)> {
        fsutil::create_private_dir(&self.root).with_context(|| format!("creating {}", self.root.display()))?;
        let safe_id: String =
            session.id.chars().take(80).map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
        let base = format!("{deleted_at}-{}-{safe_id}", session.agent.id());
        for attempt in 0..100 {
            let key = if attempt == 0 { base.clone() } else { format!("{base}-{attempt}") };
            let dir = self.root.join(&key);
            match create_dir_private(&dir) {
                Ok(()) => return Ok((key, dir)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
            }
        }
        bail!("could not create a trash entry for {}", session.id)
    }

    /// Every entry, plus directories that are not valid entries.
    pub fn scan(&self) -> Listing {
        let mut listing = Listing::default();
        let Ok(dirs) = fs::read_dir(&self.root) else {
            return listing;
        };
        for dir in dirs.flatten() {
            let key = dir.file_name().to_string_lossy().into_owned();
            match read_entry(&dir.path(), &key) {
                Ok(entry) => listing.entries.push(entry),
                Err(err) => listing.invalid.push(InvalidEntry { key, error: format!("{err:#}") }),
            }
        }
        listing.entries.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at).then_with(|| b.key.cmp(&a.key)));
        listing.invalid.sort_by(|a, b| a.key.cmp(&b.key));
        listing
    }

    /// Valid entries, most recently deleted first.
    pub fn list(&self) -> Vec<TrashEntry> {
        self.scan().entries
    }

    /// Finds an entry by key, or by session id (the most recent deletion wins).
    pub fn find(&self, selector: &str) -> Result<TrashEntry> {
        let selector = selector.trim();
        let entries = self.list();
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
        let _lock = EntryLock::acquire(&dir)?;
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
                Ok(()) => restored.push(job),
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

        // Everything is back. Remove the entry without recursion, so any
        // unexpected leftover stays in the trash instead of being deleted.
        drop(_lock);
        if remove_if_empty(&items_dir) {
            let _ = fs::remove_file(dir.join(MANIFEST));
            remove_if_empty(&dir);
        }
        entry.state = EntryState::Trashed;
        Ok(entry)
    }

    /// Permanently deletes one entry.
    pub fn purge(&self, key: &str) -> Result<()> {
        let dir = self.entry_dir(key)?;
        let _lock = EntryLock::acquire(&dir)?;
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))
    }

    /// Permanently deletes entries deleted before `cutoff_ms`, and empty
    /// leftovers of interrupted deletes. Invalid entries are left alone.
    /// Returns how many entries were deleted.
    pub fn purge_older_than(&self, cutoff_ms: i64) -> Result<usize> {
        let listing = self.scan();
        let mut count = 0;
        for entry in listing.entries.iter().filter(|e| e.deleted_at < cutoff_ms) {
            self.purge(&entry.key)?;
            count += 1;
        }
        for invalid in &listing.invalid {
            let dir = self.root.join(&invalid.key);
            remove_if_empty(&dir.join(ITEMS));
            remove_if_empty(&dir);
        }
        Ok(count)
    }

    fn entry_dir(&self, key: &str) -> Result<PathBuf> {
        if !valid_key(key) {
            bail!("invalid trash key {key:?}");
        }
        let dir = self.root.join(key);
        if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            bail!("no trash entry {key:?}");
        }
        Ok(dir)
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

/// An exclusive lock on one entry, so two entropeek processes never restore
/// or purge the same entry at once. A lock left by a dead process is stolen.
struct EntryLock(PathBuf);

impl EntryLock {
    fn acquire(dir: &Path) -> Result<EntryLock> {
        let path = dir.join(LOCK);
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    use std::io::Write;
                    let _ = writeln!(&file, "{}", std::process::id());
                    return Ok(EntryLock(path));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    bail!("this trash entry is being changed by another entropeek process");
                }
                Err(e) => return Err(e).with_context(|| format!("locking {}", dir.display())),
            }
        }
        bail!("could not lock {}", dir.display())
    }
}

impl Drop for EntryLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn lock_is_stale(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else { return false };
    let Ok(pid) = text.trim().parse::<i32>() else { return true };
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks whether the process exists.
        let alive =
            unsafe { libc::kill(pid, 0) } == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        !alive
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        fs::metadata(path)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().unwrap_or_default() > std::time::Duration::from_secs(600))
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
    for (n, item) in entry.items.iter().enumerate() {
        if item.stored.is_empty() || !item.stored.bytes().all(|b| b.is_ascii_digit()) {
            bail!("invalid stored name {:?}", item.stored);
        }
        let original = &item.original;
        if !original.is_absolute()
            || original.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir))
            || original.file_name().is_none()
        {
            bail!("invalid original path {}", original.display());
        }
        for other in &entry.items[n + 1..] {
            if other.stored == item.stored {
                bail!("stored name {:?} is listed twice", item.stored);
            }
            if other.original.starts_with(original) || original.starts_with(&other.original) {
                bail!("{} and {} overlap", original.display(), other.original.display());
            }
        }
    }
    Ok(())
}

/// Renames `from` to `to` without replacing anything, falling back to a
/// careful copy + delete across filesystems.
pub fn move_path(from: &Path, to: &Path) -> Result<(), MoveError> {
    move_with(from, to, fsutil::rename_noreplace)
}

/// [`move_path`] with the rename step injectable, so tests can force the
/// cross-filesystem path.
#[doc(hidden)]
pub fn move_with(from: &Path, to: &Path, rename: impl Fn(&Path, &Path) -> io::Result<()>) -> Result<(), MoveError> {
    match rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => move_across(from, to),
        Err(e) => Err(e.into()),
    }
}

/// Copy + delete for moves between filesystems.
///
/// The copy is created exclusively (it never replaces anything), keeps
/// symlinks as symlinks, preserves permissions and modification times, and
/// refuses special files. A failed copy is removed and the source is left
/// alone. A file that changes while being copied is not deleted. If the copy
/// is complete but the source cannot be fully removed, the error says
/// `completed`, so the caller knows the destination is the whole item.
fn move_across(from: &Path, to: &Path) -> Result<(), MoveError> {
    let before = fs::symlink_metadata(from)?;
    if let Err(err) = copy_tree(from, to) {
        if fsutil::occupied(to) && err.kind() != io::ErrorKind::AlreadyExists {
            let _ = remove_tree(to);
        }
        return Err(err.into());
    }
    if before.is_file() {
        let after = fs::symlink_metadata(from)?;
        if after.len() != before.len() || after.modified().ok() != before.modified().ok() {
            let _ = remove_tree(to);
            return Err(io::Error::other(format!("{} changed while it was being moved", from.display())).into());
        }
    }
    remove_tree(from).map_err(|error| MoveError { error, completed: true })
}

fn remove_tree(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) }
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    let kind = meta.file_type();
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
        fs::set_permissions(to, meta.permissions())?;
        set_mtime(to, &meta);
        return Ok(());
    }
    if kind.is_file() {
        let mut src = File::open(from)?;
        let mut dst = OpenOptions::new().write(true).create_new(true).open(to)?;
        io::copy(&mut src, &mut dst)?;
        dst.sync_all()?;
        drop(dst);
        fs::set_permissions(to, meta.permissions())?;
        set_mtime(to, &meta);
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{} is not a regular file, directory or link", from.display()),
    ))
}

fn set_mtime(path: &Path, meta: &fs::Metadata) {
    let Ok(modified) = meta.modified() else { return };
    let times = fs::FileTimes::new().set_modified(modified);
    let file = if meta.is_dir() { File::open(path) } else { OpenOptions::new().write(true).open(path) };
    if let Ok(file) = file {
        let _ = file.set_times(times);
    }
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
            let listing = self.trash().scan();
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
        assert_eq!(trash.list(), vec![entry.clone()]);

        let restored = trash.restore(&entry.key, &f.roots()).unwrap();
        assert_eq!(restored.session, f.session);
        assert_eq!(fs::read(&f.paths[0]).unwrap(), b"{\"a\":1}\n");
        assert_eq!(fs::read(f.paths[1].join("subagents/x.jsonl")).unwrap(), b"sub");
        assert_eq!(fs::metadata(&f.paths[0]).unwrap().modified().unwrap(), mtime, "mtime survives");
        assert!(trash.list().is_empty());
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
        assert!(trash.scan().entries.is_empty() && trash.scan().invalid.is_empty());
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
        assert!(trash.list().is_empty());
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
                for entry in f.trash().list() {
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
        let entries = f.trash().list();
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
        assert_eq!(trash.list().len(), 1);
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
        let entries = f.trash().list();
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

        let listed = trash.list();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].is_incomplete());
        trash.restore(&entry.key, &f.roots()).unwrap();
        assert!(f.paths.iter().all(|p| p.exists()));
        assert!(trash.list().is_empty());
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

        assert!(trash.list()[0].is_incomplete());
        assert_eq!(trash.purge_older_than(i64::MAX).unwrap(), 1);
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
        let listing = trash.scan();
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
            let listing = trash.scan();
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
        let listed = trash.list();
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
        assert_eq!(trash.purge_older_than(0).unwrap(), 0);
        assert_eq!(trash.purge_older_than(i64::MAX).unwrap(), 1);
        assert!(trash.list().is_empty());
    }

    #[test]
    fn same_session_can_be_trashed_twice() {
        let f = fixture();
        let trash = f.trash();
        let first = trash.put(&f.session, &f.paths[..1], &f.root).unwrap();
        fs::write(&f.paths[0], b"again").unwrap();
        let second = trash.put(&f.session, &f.paths[..1], &f.root).unwrap();
        assert_ne!(first.key, second.key);
        assert_eq!(trash.list().len(), 2);
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
        assert!(trash.list().is_empty());
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
        let entries = f.trash().list();
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
}
