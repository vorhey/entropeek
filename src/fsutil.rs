//! Filesystem helpers with the safety properties the rest of entropeek relies on.
//!
//! - entropeek's own files hold snippets of your conversations, so they are
//!   written atomically to fresh private files, and its directories are kept
//!   private even if they already existed with looser permissions.
//! - Moves never replace something that appeared at the destination
//!   ([`rename_noreplace`]).
//! - Deletes only touch paths whose real location is inside the agent's
//!   directory ([`resolve_inside`]), even when symlinks are involved.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Atomically replaces `path` with `bytes`. The data goes to a new,
/// exclusively created private temporary file in the same directory, is
/// flushed to disk, and is then renamed over `path`. A planted file or link
/// at a predictable temporary name cannot redirect the write, and a failed
/// write leaves the old file untouched.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = parent_dir(path);
    // tempfile creates the file with O_EXCL and mode 0600.
    let mut tmp = tempfile::Builder::new().prefix(".entropeek-").suffix(".tmp").tempfile_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    sync_dir(dir);
    Ok(())
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// Flushes a directory entry change (create/rename/remove) to disk. Best
/// effort: not every platform or filesystem supports syncing directories.
pub fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Creates `path` and its parents, and makes sure the last directory is a
/// real directory owned by the current user with no group/other access.
/// An existing directory with looser permissions is tightened; one that is a
/// symlink or belongs to someone else is rejected.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    ensure_private_dir(path)
}

/// See [`create_private_dir`]; checks an existing directory.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_dir() {
        return Err(io::Error::other(format!("{} is not a directory", path.display())));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        if meta.uid() != me {
            return Err(io::Error::other(format!("{} belongs to another user", path.display())));
        }
        if meta.mode() & 0o077 != 0 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// Renames `from` to `to`, failing with [`io::ErrorKind::AlreadyExists`]
/// if anything (a file, a directory, even a dangling symlink) is at `to`.
/// Atomic on Linux and macOS; elsewhere, and on filesystems without
/// support, a careful fallback is used.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    match native_rename_noreplace(from, to) {
        Some(result) => result,
        None => fallback_rename_noreplace(from, to),
    }
}

/// `None` when the platform or filesystem has no atomic no-replace rename.
#[cfg(target_os = "linux")]
fn native_rename_noreplace(from: &Path, to: &Path) -> Option<io::Result<()>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    const RENAME_NOREPLACE: libc::c_uint = 1;
    let from = CString::new(from.as_os_str().as_bytes()).ok()?;
    let to = CString::new(to.as_os_str().as_bytes()).ok()?;
    // SAFETY: both pointers are valid NUL-terminated strings for the call.
    let rc = unsafe {
        libc::syscall(libc::SYS_renameat2, libc::AT_FDCWD, from.as_ptr(), libc::AT_FDCWD, to.as_ptr(), RENAME_NOREPLACE)
    };
    if rc == 0 {
        return Some(Ok(()));
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP) => None,
        _ => Some(Err(err)),
    }
}

#[cfg(target_os = "macos")]
fn native_rename_noreplace(from: &Path, to: &Path) -> Option<io::Result<()>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let from = CString::new(from.as_os_str().as_bytes()).ok()?;
    let to = CString::new(to.as_os_str().as_bytes()).ok()?;
    // SAFETY: both pointers are valid NUL-terminated strings for the call.
    let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if rc == 0 {
        return Some(Ok(()));
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EINVAL | libc::ENOTSUP) => None,
        _ => Some(Err(err)),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn native_rename_noreplace(_from: &Path, _to: &Path) -> Option<io::Result<()>> {
    None
}

/// Files: hard link (which never replaces) then unlink, so the claim on the
/// destination is atomic. Directories and filesystems without hard links:
/// check, then rename; a small window remains there.
pub(crate) fn fallback_rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    if occupied(to) {
        return Err(already_exists(to));
    }
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_file() {
        match fs::hard_link(from, to) {
            Ok(()) => return fs::remove_file(from),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(e),
            Err(_) => {} // no hard links here; fall through
        }
    }
    if occupied(to) {
        return Err(already_exists(to));
    }
    fs::rename(from, to)
}

/// Whether anything exists at `path`, including a dangling symlink.
pub fn occupied(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn already_exists(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already exists", path.display()))
}

/// Checks that `path` really lives inside `root`, following every symlink in
/// its parent directories, and returns the resolved path (real parent +
/// file name). The last component is not followed: moving a symlink moves
/// the link itself. A linked agent root is fine; a linked project directory
/// that points outside the root is refused.
pub fn resolve_inside(path: &Path, root: &Path) -> Result<PathBuf> {
    let refuse = || format!("refusing to touch {}: it is not inside {}", path.display(), root.display());
    if path.components().any(|c| matches!(c, Component::ParentDir)) || !path.starts_with(root) || path == root {
        bail!(refuse());
    }
    let name = path.file_name().with_context(refuse)?;
    let real_root = fs::canonicalize(root).with_context(|| format!("resolving {}", root.display()))?;
    let parent = path.parent().with_context(refuse)?;
    let real_parent = fs::canonicalize(parent).with_context(|| format!("resolving {}", parent.display()))?;
    if !real_parent.starts_with(&real_root) {
        bail!("{} (it resolves to {})", refuse(), real_parent.join(name).display());
    }
    Ok(real_parent.join(name))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn files_and_dirs_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        write_private(&file, b"secret").unwrap();
        assert_eq!(mode(&file), 0o600);
        assert_eq!(fs::read(&file).unwrap(), b"secret");
        write_private(&file, b"x").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"x", "overwrites");

        let nested = dir.path().join("a/b/trash");
        create_private_dir(&nested).unwrap();
        assert_eq!(mode(&nested), 0o700);
        create_private_dir(&nested).unwrap(); // idempotent
    }

    #[test]
    fn existing_permissive_objects_are_made_private() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        fs::write(&file, b"old").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&file, b"new").unwrap();
        assert_eq!(mode(&file), 0o600, "a replaced file is private");

        let open = dir.path().join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&open).unwrap();
        assert_eq!(mode(&open), 0o700, "an existing directory is tightened");
    }

    #[test]
    fn symlinked_private_dir_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        symlink(&real, &link).unwrap();
        assert!(create_private_dir(&link).is_err());
    }

    #[test]
    fn planted_temporary_files_cannot_redirect_writes() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"keep").unwrap();
        // Plant links at every name the old implementation would have used.
        for name in ["index.tmp", "index.bin.tmp", &format!("index.tmp-{}", std::process::id())] {
            symlink(&victim, dir.path().join(name)).unwrap();
        }
        let target = dir.path().join("index.bin");
        write_private(&target, b"cache").unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert!(!fs::symlink_metadata(&target).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"cache");
        let leftovers = fs::read_dir(dir.path()).unwrap().flatten().filter(|e| {
            let name = e.file_name();
            name.to_string_lossy().starts_with(".entropeek-")
        });
        assert_eq!(leftovers.count(), 0, "no temporary files left behind");
    }

    #[test]
    fn rename_noreplace_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n);
        fs::write(p("a"), b"a").unwrap();
        fs::write(p("b"), b"b").unwrap();
        fs::create_dir(p("empty")).unwrap();
        symlink(p("nowhere"), p("dangling")).unwrap();
        for rename in [rename_noreplace as fn(&Path, &Path) -> io::Result<()>, fallback_rename_noreplace] {
            for target in ["b", "empty", "dangling"] {
                let err = rename(&p("a"), &p(target)).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{target}");
            }
            assert_eq!(fs::read(p("b")).unwrap(), b"b");
            assert!(fs::symlink_metadata(p("dangling")).unwrap().file_type().is_symlink());
        }
        rename_noreplace(&p("a"), &p("c")).unwrap();
        assert_eq!(fs::read(p("c")).unwrap(), b"a");
        fallback_rename_noreplace(&p("c"), &p("d")).unwrap();
        assert_eq!(fs::read(p("d")).unwrap(), b"a");
        fallback_rename_noreplace(&p("empty"), &p("moved")).unwrap();
        assert!(p("moved").is_dir());
    }

    #[test]
    fn resolve_inside_follows_parent_links() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("agent");
        let outside = dir.path().join("outside");
        fs::create_dir_all(root.join("projects/real")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("s.jsonl"), b"x").unwrap();
        fs::write(root.join("projects/real/s.jsonl"), b"x").unwrap();
        symlink(&outside, root.join("projects/escape")).unwrap();
        symlink(root.join("projects/real"), root.join("projects/alias")).unwrap();

        let inside = resolve_inside(&root.join("projects/real/s.jsonl"), &root).unwrap();
        assert_eq!(inside, fs::canonicalize(root.join("projects/real/s.jsonl")).unwrap());
        let alias = resolve_inside(&root.join("projects/alias/s.jsonl"), &root).unwrap();
        assert_eq!(alias, inside, "a link that stays inside resolves to the real path");
        let err = resolve_inside(&root.join("projects/escape/s.jsonl"), &root).unwrap_err();
        assert!(err.to_string().contains("not inside"), "{err}");
        assert!(resolve_inside(&root.join("projects/../../outside/s.jsonl"), &root).is_err());
        assert!(resolve_inside(&outside.join("s.jsonl"), &root).is_err());
        assert!(resolve_inside(&root, &root).is_err());
        symlink(dir.path().join("gone"), root.join("projects/dangling")).unwrap();
        assert!(resolve_inside(&root.join("projects/dangling/s.jsonl"), &root).is_err());

        // A linked agent root is supported.
        let linked_root = dir.path().join("agent-link");
        symlink(&root, &linked_root).unwrap();
        assert!(resolve_inside(&linked_root.join("projects/real/s.jsonl"), &linked_root).is_ok());
    }
}
