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
    let paths = provider.related_paths(session);
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
    let mut command = provider.resume_command(session);
    command.executable = Some(validate(&command, std::env::var_os("PATH"))?);
    Ok(command)
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

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
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
