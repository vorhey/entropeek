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
    sync_dir_strict(dir)
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
    let _ = sync_dir_strict(dir);
}

/// Flushes a directory's entries to disk, reporting real failures (such as
/// an I/O error). Filesystems that cannot sync directories at all are not
/// an error: there is nothing more that can be done on them.
pub fn sync_dir_strict(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return sync_handle(&File::open(dir)?);
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Flushes an open file or directory, like [`sync_dir_strict`].
pub fn sync_handle(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        let unsupported = |e: &io::Error| {
            e.raw_os_error().is_some_and(|c| c == libc::EINVAL || c == libc::ENOTSUP || c == libc::EOPNOTSUPP)
        };
        match file.sync_all() {
            Err(e) if unsupported(&e) => Ok(()),
            other => other,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(())
    }
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
/// Atomic on Linux and macOS. Where the filesystem has no atomic
/// no-replace rename, a fallback claims the destination atomically first
/// (a hard link, an empty directory or a new symlink), and refuses rather
/// than risk replacing anything when it cannot.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    rename_at(from, to, Follow::Yes, Native::Try)
}

/// [`rename_noreplace`] for paths that are already fully resolved: no
/// directory on the way to either path may be a symlink. Both parent
/// directories are opened first and the rename is made relative to those
/// handles, so a directory swapped for a link after the paths were checked
/// makes the move fail instead of reaching somewhere else.
pub fn rename_noreplace_nofollow(from: &Path, to: &Path) -> io::Result<()> {
    rename_at(from, to, Follow::No, Native::Try)
}

/// The fallback used where no atomic no-replace rename exists.
#[cfg(test)]
pub(crate) fn fallback_rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    rename_at(from, to, Follow::Yes, Native::Skip)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Follow {
    Yes,
    No,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Native {
    Try,
    #[cfg_attr(not(test), allow(dead_code))]
    Skip,
}

/// The parent directory and file name of `path`.
fn split(path: &Path) -> io::Result<(&Path, &std::ffi::OsStr)> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("{} has no file name", path.display())))?;
    Ok((parent_dir(path), name))
}

/// The device and inode of the directory at `path`, reached without
/// following any symlink. Comparing it before and after a long operation
/// shows whether the directory was swapped in between.
#[cfg(unix)]
pub fn dir_identity(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let dir = File::from(at::open_dir(path, Follow::No)?);
    let meta = dir.metadata()?;
    Ok((meta.dev(), meta.ino()))
}

#[cfg(unix)]
fn rename_at(from: &Path, to: &Path, follow: Follow, native: Native) -> io::Result<()> {
    let (from_dir, from_name) = split(from)?;
    let (to_dir, to_name) = split(to)?;
    let from_fd = at::open_dir(from_dir, follow)?;
    let to_fd = at::open_dir(to_dir, follow)?;
    let from = at::Name::new(from_name)?;
    let to = at::Name::new(to_name)?;
    if native == Native::Try
        && let Some(result) = at::rename_native(&from_fd, &from, &to_fd, &to)
    {
        return result;
    }
    at::rename_claiming(&from_fd, &from, &to_fd, &to)
}

/// Without `*at` system calls: hard link then unlink for files (the link
/// never replaces anything); other kinds cannot be moved without a window
/// in which a new destination could be replaced, so they are refused.
#[cfg(not(unix))]
fn rename_at(from: &Path, to: &Path, _follow: Follow, _native: Native) -> io::Result<()> {
    if occupied(to) {
        return Err(already_exists(to));
    }
    if !fs::symlink_metadata(from)?.file_type().is_file() {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "cannot move this without risking an overwrite here"));
    }
    fs::hard_link(from, to)?;
    if let Err(err) = fs::remove_file(from) {
        let _ = fs::remove_file(to);
        return Err(err);
    }
    Ok(())
}

/// Directory-handle based operations.
#[cfg(unix)]
mod at {
    use std::ffi::{CString, OsStr};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::Follow;

    fn cstr(bytes: &[u8]) -> io::Result<CString> {
        CString::new(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
    }

    fn check(rc: libc::c_int) -> io::Result<()> {
        if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned by the kernel and is owned by nobody else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn link_in_path(path: &Path) -> io::Error {
        io::Error::other(format!(
            "{} is no longer the directory that was checked (a link is in its path)",
            path.display()
        ))
    }

    /// A file name inside a directory handle.
    pub(super) struct Name(CString);

    impl Name {
        pub(super) fn new(name: &OsStr) -> io::Result<Name> {
            Ok(Name(cstr(name.as_bytes())?))
        }
    }

    #[cfg(target_os = "linux")]
    const DIR_FLAGS: libc::c_int = libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    const DIR_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;

    /// Opens a directory. With `Follow::No`, no component of `path` may be
    /// a symlink.
    pub(super) fn open_dir(path: &Path, follow: Follow) -> io::Result<OwnedFd> {
        let c = cstr(path.as_os_str().as_bytes())?;
        if follow == Follow::Yes {
            // SAFETY: valid NUL-terminated path; the result is checked.
            return owned(unsafe { libc::open(c.as_ptr(), DIR_FLAGS) });
        }
        open_dir_nofollow(path, &c)
    }

    #[cfg(target_os = "linux")]
    fn open_dir_nofollow(path: &Path, c: &CString) -> io::Result<OwnedFd> {
        // SAFETY: `open_how` is plain integers; all zero is a valid value.
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = DIR_FLAGS as u64;
        how.resolve = libc::RESOLVE_NO_SYMLINKS;
        // SAFETY: valid path and `open_how`, with its exact size.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                libc::AT_FDCWD,
                c.as_ptr(),
                &how as *const libc::open_how,
                std::mem::size_of::<libc::open_how>(),
            )
        };
        if fd >= 0 {
            return owned(fd as libc::c_int);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ELOOP) => Err(link_in_path(path)),
            // Kernels before 5.6, or a sandbox that blocks openat2.
            Some(libc::ENOSYS | libc::EPERM | libc::E2BIG | libc::EINVAL) => walk_nofollow(path),
            _ => Err(err),
        }
    }

    #[cfg(target_os = "macos")]
    fn open_dir_nofollow(path: &Path, c: &CString) -> io::Result<OwnedFd> {
        // SAFETY: valid NUL-terminated path; the result is checked.
        let fd = unsafe { libc::open(c.as_ptr(), DIR_FLAGS | libc::O_NOFOLLOW_ANY) };
        match owned(fd) {
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => Err(link_in_path(path)),
            other => other,
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn open_dir_nofollow(path: &Path, _c: &CString) -> io::Result<OwnedFd> {
        walk_nofollow(path)
    }

    /// Opens `path` one component at a time, refusing symlinks.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    fn walk_nofollow(path: &Path) -> io::Result<OwnedFd> {
        use std::path::Component;
        let start: &[u8] = if path.is_absolute() { b"/" } else { b"." };
        let start = cstr(start)?;
        // SAFETY: valid NUL-terminated path; the result is checked.
        let mut dir = owned(unsafe { libc::open(start.as_ptr(), DIR_FLAGS) })?;
        for part in path.components() {
            let name = match part {
                Component::RootDir | Component::CurDir | Component::Prefix(_) => continue,
                Component::ParentDir => cstr(b"..")?,
                Component::Normal(name) => cstr(name.as_bytes())?,
            };
            // SAFETY: `dir` is an open directory and `name` a valid C string.
            let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), DIR_FLAGS | libc::O_NOFOLLOW) };
            dir = owned(fd).map_err(|e| match e.raw_os_error() {
                Some(libc::ELOOP | libc::ENOTDIR) => link_in_path(path),
                _ => e,
            })?;
        }
        Ok(dir)
    }

    /// The atomic no-replace rename, or `None` where it is unsupported.
    #[cfg(target_os = "linux")]
    pub(super) fn rename_native(
        from_dir: &OwnedFd,
        from: &Name,
        to_dir: &OwnedFd,
        to: &Name,
    ) -> Option<io::Result<()>> {
        const RENAME_NOREPLACE: libc::c_uint = 1;
        // SAFETY: open directory handles and valid C strings.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                from_dir.as_raw_fd(),
                from.0.as_ptr(),
                to_dir.as_raw_fd(),
                to.0.as_ptr(),
                RENAME_NOREPLACE,
            )
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
    pub(super) fn rename_native(
        from_dir: &OwnedFd,
        from: &Name,
        to_dir: &OwnedFd,
        to: &Name,
    ) -> Option<io::Result<()>> {
        // SAFETY: open directory handles and valid C strings.
        let rc = unsafe {
            libc::renameatx_np(
                from_dir.as_raw_fd(),
                from.0.as_ptr(),
                to_dir.as_raw_fd(),
                to.0.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
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
    pub(super) fn rename_native(_: &OwnedFd, _: &Name, _: &OwnedFd, _: &Name) -> Option<io::Result<()>> {
        None
    }

    fn already_exists() -> io::Error {
        io::Error::new(io::ErrorKind::AlreadyExists, "the destination already exists")
    }

    fn exists_error(err: io::Error) -> io::Error {
        match err.raw_os_error() {
            Some(libc::EEXIST | libc::ENOTEMPTY) => already_exists(),
            _ => err,
        }
    }

    /// Moves without native support by first claiming the destination with
    /// an operation that itself never replaces anything:
    /// - a file gets a hard link at the destination, then the old name is
    ///   removed (if that fails, the new link is removed again);
    /// - a directory gets an empty directory there, which `rename` then
    ///   replaces (it only ever replaces an *empty* directory, so whatever
    ///   might take its place in between is never lost);
    /// - a symlink is recreated at the destination, then removed.
    ///
    /// Anything else, or a file on a filesystem without hard links, is
    /// refused rather than moved with a check-then-rename window.
    pub(super) fn rename_claiming(from_dir: &OwnedFd, from: &Name, to_dir: &OwnedFd, to: &Name) -> io::Result<()> {
        let (fd, td) = (from_dir.as_raw_fd(), to_dir.as_raw_fd());
        // SAFETY (all calls below): open directory handles, valid C strings,
        // and buffers of the stated sizes.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        check(unsafe { libc::fstatat(fd, from.0.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) })?;
        let kind = stat.st_mode & libc::S_IFMT;
        if kind == libc::S_IFREG {
            if let Err(err) = check(unsafe { libc::linkat(fd, from.0.as_ptr(), td, to.0.as_ptr(), 0) }) {
                match err.raw_os_error() {
                    Some(libc::EEXIST) => return Err(already_exists()),
                    // The caller copies across filesystems instead.
                    Some(libc::EXDEV) => return Err(err),
                    _ => {}
                }
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("this filesystem can neither rename without replacing nor hard-link ({err})"),
                ));
            }
            if let Err(err) = check(unsafe { libc::unlinkat(fd, from.0.as_ptr(), 0) }) {
                let _ = check(unsafe { libc::unlinkat(td, to.0.as_ptr(), 0) });
                return Err(err);
            }
            return Ok(());
        }
        if kind == libc::S_IFDIR {
            check(unsafe { libc::mkdirat(td, to.0.as_ptr(), 0o700) }).map_err(exists_error)?;
            if let Err(err) = check(unsafe { libc::renameat(fd, from.0.as_ptr(), td, to.0.as_ptr()) }) {
                // Only removes the placeholder if it is still an empty directory.
                let _ = check(unsafe { libc::unlinkat(td, to.0.as_ptr(), libc::AT_REMOVEDIR) });
                return Err(match err.raw_os_error() {
                    Some(libc::EEXIST | libc::ENOTEMPTY | libc::ENOTDIR | libc::EISDIR) => already_exists(),
                    _ => err,
                });
            }
            return Ok(());
        }
        if kind == libc::S_IFLNK {
            let mut buf = vec![0u8; (stat.st_size as usize).max(255) + 1];
            let n = unsafe { libc::readlinkat(fd, from.0.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n as usize >= buf.len() {
                return Err(io::Error::other("the link changed while it was being moved"));
            }
            let target = cstr(&buf[..n as usize])?;
            check(unsafe { libc::symlinkat(target.as_ptr(), td, to.0.as_ptr()) }).map_err(exists_error)?;
            if let Err(err) = check(unsafe { libc::unlinkat(fd, from.0.as_ptr(), 0) }) {
                let _ = check(unsafe { libc::unlinkat(td, to.0.as_ptr(), 0) });
                return Err(err);
            }
            return Ok(());
        }
        Err(io::Error::new(io::ErrorKind::Unsupported, "not a regular file, directory or link"))
    }
}

/// Whether anything exists at `path`, including a dangling symlink.
pub fn occupied(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

#[cfg(not(unix))]
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
    fn fallback_claims_the_destination_for_every_kind() {
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n);
        fs::create_dir(p("tree")).unwrap();
        fs::write(p("tree/inner"), b"x").unwrap();
        symlink("tree/inner", p("link")).unwrap();
        fs::write(p("taken"), b"keep").unwrap();
        // Directories and links cannot replace anything either.
        for from in ["tree", "link"] {
            assert_eq!(
                fallback_rename_noreplace(&p(from), &p("taken")).unwrap_err().kind(),
                io::ErrorKind::AlreadyExists
            );
            assert_eq!(fs::read(p("taken")).unwrap(), b"keep");
        }
        fs::create_dir(p("empty")).unwrap();
        assert_eq!(
            fallback_rename_noreplace(&p("tree"), &p("empty")).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(p("empty").is_dir() && p("tree/inner").exists(), "nothing moved");
        fallback_rename_noreplace(&p("tree"), &p("moved")).unwrap();
        assert_eq!(fs::read(p("moved/inner")).unwrap(), b"x");
        fallback_rename_noreplace(&p("link"), &p("link2")).unwrap();
        assert_eq!(fs::read_link(p("link2")).unwrap(), Path::new("tree/inner"));
        assert!(!occupied(&p("link")));
    }

    #[test]
    fn nofollow_rename_refuses_a_directory_swapped_for_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let inside = base.join("agent/projects");
        let outside = base.join("outside");
        fs::create_dir_all(&inside).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(inside.join("s.jsonl"), b"inside").unwrap();
        fs::write(outside.join("s.jsonl"), b"outside").unwrap();
        let checked = inside.join("s.jsonl");
        let before = dir_identity(&inside).unwrap();
        // After the check: the parent is renamed and a link takes its place.
        fs::rename(&inside, base.join("agent/renamed")).unwrap();
        symlink(&outside, &inside).unwrap();
        assert!(dir_identity(&inside).is_err(), "a link in the path is noticed");
        let err = rename_noreplace_nofollow(&checked, &base.join("moved")).unwrap_err();
        assert!(err.to_string().contains("no longer the directory"), "{err}");
        assert_eq!(fs::read(outside.join("s.jsonl")).unwrap(), b"outside", "outside is untouched");
        assert_eq!(dir_identity(&base.join("agent/renamed")).unwrap(), before);
        // With nothing swapped it moves as usual.
        rename_noreplace_nofollow(&base.join("agent/renamed/s.jsonl"), &base.join("moved")).unwrap();
        assert_eq!(fs::read(base.join("moved")).unwrap(), b"inside");
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
