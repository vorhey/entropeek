//! Actions on sessions: delete (to the trash), restore, and resume.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::model::Session;
use crate::provider::{self, Provider, ResumeCommand};
use crate::trash::{Trash, TrashEntry};

/// Moves a session and everything that belongs to it into the trash.
pub fn delete(providers: &[Box<dyn Provider>], trash: &Trash, session: &Session) -> Result<TrashEntry> {
    let provider =
        provider::find(providers, session.agent).with_context(|| format!("no provider for {}", session.agent.id()))?;
    let paths = provider.related_paths(session)?;
    trash.put(session, &paths, provider.root())
}

/// Puts a trashed session back. Its files must belong inside an agent directory.
pub fn restore(providers: &[Box<dyn Provider>], trash: &Trash, key: &str) -> Result<TrashEntry> {
    let roots: Vec<PathBuf> = providers.iter().map(|p| p.root().to_path_buf()).collect();
    trash.restore(key, &roots)
}

/// The command that resumes `session`, checked so it can actually run. The
/// program is resolved to an absolute path now, so changing into the
/// workspace cannot make a relative `PATH` entry pick a different program.
pub fn resume_command(providers: &[Box<dyn Provider>], session: &Session) -> Result<ResumeCommand> {
    let provider =
        provider::find(providers, session.agent).with_context(|| format!("no provider for {}", session.agent.id()))?;
    check_id(&session.id)?;
    let mut command = provider.resume_command(session);
    command.executable = Some(validate(&command, std::env::var_os("PATH"))?);
    Ok(command)
}

/// The command to print or copy for resuming `session`: what
/// [`ResumeCommand::shell_line`] shows does what a direct resume would.
/// The program keeps its plain name unless `PATH` finds it through a
/// relative (or empty) entry; such an entry would mean a different
/// directory after the `cd` into the workspace, so the program found now
/// is shown by its absolute path instead.
pub fn printable_command(providers: &[Box<dyn Provider>], session: &Session) -> Result<ResumeCommand> {
    let provider =
        provider::find(providers, session.agent).with_context(|| format!("no provider for {}", session.agent.id()))?;
    check_id(&session.id)?;
    let mut command = provider.resume_command(session);
    command.executable = exported_program(&command.program);
    Ok(command)
}

/// The shell line for [`printable_command`], or an empty string if the
/// session cannot be resumed safely.
pub fn printable_line(providers: &[Box<dyn Provider>], session: &Session) -> String {
    printable_command(providers, session).map(|c| c.shell_line()).unwrap_or_default()
}

/// Session ids are passed to the agent as arguments. One that looks like an
/// option (`--last`) would be read as one and resume a different session,
/// so ids from malformed or hand-edited files are refused.
fn check_id(id: &str) -> Result<()> {
    if id.is_empty() || id.starts_with('-') || id.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("this session's id {:?} cannot be passed to the agent safely", crate::text::one_line(id));
    }
    Ok(())
}

/// See [`printable_command`]. Looked up once per program: `PATH` does not
/// change while entropeek runs.
fn exported_program(name: &str) -> Option<PathBuf> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static FOUND: OnceLock<Mutex<HashMap<String, Option<PathBuf>>>> = OnceLock::new();
    let mut found = FOUND.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    found.entry(name.to_string()).or_insert_with(|| export_lookup(name, std::env::var_os("PATH"))).clone()
}

/// `Some(absolute program)` when `PATH` finds `name` through a relative or
/// empty entry; `None` when the plain name already means the same program
/// everywhere (or is not found at all).
pub fn export_lookup(name: &str, path_var: Option<OsString>) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return (!path.is_absolute()).then(|| std::path::absolute(&path).unwrap_or(path));
    }
    for dir in std::env::split_paths(&path_var?) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return (!dir.is_absolute()).then(|| std::path::absolute(&candidate).unwrap_or(candidate));
        }
    }
    None
}

/// Checks that the workspace exists and the agent's program is installed,
/// and returns the program's absolute path. Agents look sessions up by
/// working directory, so resuming elsewhere fails.
pub fn validate(command: &ResumeCommand, path_var: Option<OsString>) -> Result<PathBuf> {
    if command.cwd.as_os_str().is_empty() {
        bail!("this session has no recorded workspace, so it cannot be resumed");
    }
    if !command.cwd.is_dir() {
        bail!("workspace {} no longer exists", command.cwd.display());
    }
    find_program(&command.program, path_var)
        .with_context(|| format!("`{}` is not installed or not on PATH", command.program))
}

/// Looks up `name` in a `PATH`-style list of directories. The result is
/// absolute: relative and empty entries are resolved against the current
/// directory at lookup time.
pub fn find_program(name: &str, path_var: Option<OsString>) -> Option<PathBuf> {
    let absolute = |p: PathBuf| std::path::absolute(&p).unwrap_or(p);
    if name.contains('/') {
        let path = absolute(PathBuf::from(name));
        return is_executable(&path).then_some(path);
    }
    std::env::split_paths(&path_var?).map(|dir| absolute(dir.join(name))).find(|candidate| is_executable(candidate))
}

/// Whether this process may run `path`: a regular file that the kernel's
/// own permission check (with the effective user and groups, and any ACLs)
/// lets us execute. An execute bit for someone else is not enough.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if !path.metadata().is_ok_and(|m| m.is_file()) {
            return false;
        }
        let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
        // SAFETY: a valid NUL-terminated path; faccessat only checks access.
        unsafe { libc::faccessat(libc::AT_FDCWD, c.as_ptr(), libc::X_OK, libc::AT_EACCESS) == 0 }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Replaces this process with the agent (Unix), or runs it and exits with its
/// status elsewhere. Only returns on failure.
pub fn exec(command: &ResumeCommand) -> anyhow::Error {
    let program = command.executable.clone().unwrap_or_else(|| PathBuf::from(&command.program));
    let mut cmd = std::process::Command::new(program);
    cmd.args(&command.args).current_dir(&command.cwd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        anyhow::Error::new(err).context(format!("running {}", command.program))
    }
    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(err) => anyhow::Error::new(err).context(format!("running {}", command.program)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Env;
    use crate::index;
    use crate::testkit;

    fn fake_program(dir: &Path, name: &str) {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn finds_programs_on_path() {
        let dir = tempfile::tempdir().unwrap();
        fake_program(dir.path(), "claude");
        std::fs::write(dir.path().join("not-exec"), "x").unwrap();
        let path = std::env::join_paths([Path::new("/nonexistent"), dir.path()]).unwrap();
        assert_eq!(find_program("claude", Some(path.clone())), Some(dir.path().join("claude")));
        assert_eq!(find_program("codex", Some(path.clone())), None);
        #[cfg(unix)]
        assert_eq!(find_program("not-exec", Some(path)), None);
        assert_eq!(find_program("claude", None), None);
    }

    #[test]
    fn relative_path_entries_resolve_to_absolute_programs() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = std::env::current_dir().unwrap();
        let rel = pathdiff(dir.path(), &cwd);
        fake_program(dir.path(), "claude");
        let found = find_program("claude", Some(rel.into_os_string())).unwrap();
        assert!(found.is_absolute(), "{}", found.display());
        assert_eq!(found.canonicalize().unwrap(), dir.path().join("claude").canonicalize().unwrap());
    }

    /// `target` relative to `base`, using `..` as needed.
    fn pathdiff(target: &Path, base: &Path) -> PathBuf {
        let target: Vec<_> = target.components().collect();
        let base: Vec<_> = base.components().collect();
        let common = target.iter().zip(&base).take_while(|(a, b)| a == b).count();
        let mut out = PathBuf::new();
        for _ in common..base.len() {
            out.push("..");
        }
        for part in &target[common..] {
            out.push(part);
        }
        out
    }

    #[cfg(unix)]
    #[test]
    fn path_lookup_skips_programs_this_user_cannot_run() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (bad, good) = (dir.path().join("bad"), dir.path().join("good"));
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::create_dir_all(&good).unwrap();
        fake_program(&good, "claude");
        std::fs::write(bad.join("claude"), "#!/bin/sh\n").unwrap();
        // Executable by others, not by its owner.
        std::fs::set_permissions(bad.join("claude"), std::fs::Permissions::from_mode(0o641)).unwrap();
        let path = std::env::join_paths([&bad, &good]).unwrap();
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(find_program("claude", Some(path)), Some(good.join("claude")));
        }
    }

    #[test]
    fn exported_commands_only_pin_programs_found_through_relative_entries() {
        let dir = tempfile::tempdir().unwrap();
        fake_program(dir.path(), "claude");
        let absolute = Some(dir.path().as_os_str().to_owned());
        assert_eq!(export_lookup("claude", absolute), None, "an absolute entry means the same after `cd`");
        let cwd = std::env::current_dir().unwrap();
        let relative = pathdiff(dir.path(), &cwd).into_os_string();
        let pinned = export_lookup("claude", Some(relative)).unwrap();
        assert!(pinned.is_absolute());
        assert_eq!(pinned.canonicalize().unwrap(), dir.path().join("claude").canonicalize().unwrap());
        let cmd = ResumeCommand {
            program: "claude".into(),
            args: vec!["--resume".into(), "abc".into()],
            cwd: "/w".into(),
            executable: Some("/opt/bin/claude".into()),
        };
        assert_eq!(cmd.shell_line(), "cd /w && /opt/bin/claude --resume abc");
    }

    #[test]
    fn ids_that_look_like_options_are_refused() {
        assert!(check_id("01a16ec7-96b5-7c11-b69f-156e8bb6ba12").is_ok());
        for bad in ["--last", "-x", "", "a b", "a\u{7}"] {
            assert!(check_id(bad).is_err(), "{bad:?}");
        }
        let dir = tempfile::tempdir().unwrap();
        let providers = provider::all(&Env::under(dir.path()));
        let mut session = crate::model::sample_session(crate::model::AgentKind::Codex, "--last");
        session.cwd = dir.path().to_string_lossy().into_owned();
        assert!(resume_command(&providers, &session).is_err());
        assert!(printable_command(&providers, &session).is_err());
        assert_eq!(printable_line(&providers, &session), "");
    }

    #[test]
    fn validate_reports_missing_workspace_and_program() {
        let dir = tempfile::tempdir().unwrap();
        fake_program(dir.path(), "claude");
        let path = Some(dir.path().as_os_str().to_owned());
        let mut cmd =
            ResumeCommand { program: "claude".into(), args: vec![], cwd: dir.path().to_path_buf(), executable: None };
        assert_eq!(validate(&cmd, path.clone()).unwrap(), dir.path().join("claude"));

        cmd.cwd = dir.path().join("gone");
        assert!(validate(&cmd, path.clone()).unwrap_err().to_string().contains("no longer exists"));
        cmd.cwd = PathBuf::new();
        assert!(validate(&cmd, path.clone()).unwrap_err().to_string().contains("no recorded workspace"));
        cmd.cwd = dir.path().to_path_buf();
        cmd.program = "codex".into();
        assert!(validate(&cmd, path).unwrap_err().to_string().contains("not installed"));
    }

    #[test]
    fn delete_moves_all_related_files_and_restore_brings_them_back() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::under(dir.path());
        testkit::populate(&env);
        let providers = provider::all(&env);
        let mut cache = crate::cache::Cache::in_memory();
        let sessions = index::scan(&providers, &mut cache).sessions;
        let target = sessions.iter().find(|s| s.agent == crate::model::AgentKind::Claude).unwrap().clone();
        let env_dir = env.claude_dir.join("session-env").join(&target.id);
        std::fs::create_dir_all(&env_dir).unwrap();

        let trash = Trash::new(env.trash_dir());
        let entry = delete(&providers, &trash, &target).unwrap();
        assert_eq!(entry.items.len(), 2);
        assert!(!target.path.exists() && !env_dir.exists());
        let after = index::scan(&providers, &mut cache).sessions;
        assert_eq!(after.len(), sessions.len() - 1);

        restore(&providers, &trash, &entry.key).unwrap();
        let restored = index::scan(&providers, &mut cache);
        assert_eq!(restored.sessions, sessions);
        assert!(env_dir.exists());
    }
}
