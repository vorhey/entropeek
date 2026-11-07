//! Where entropeek reads agent data and keeps its own files.
//!
//! Everything is resolved once into an [`Env`] and passed down, so tests can
//! point the whole program at a temporary directory.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Env {
    pub home: PathBuf,
    /// Claude Code's data directory (`~/.claude`, or `$CLAUDE_CONFIG_DIR`).
    pub claude_dir: PathBuf,
    /// Codex's data directory (`~/.codex`, or `$CODEX_HOME`).
    pub codex_dir: PathBuf,
    /// OpenCode's database (`opencode.db` in its data directory,
    /// `$XDG_DATA_HOME/opencode` or `~/.local/share/opencode`; or `$OPENCODE_DB`).
    pub opencode_db: PathBuf,
    /// Where the session index cache lives.
    pub cache_dir: PathBuf,
    /// Where deleted sessions are kept until purged.
    pub data_dir: PathBuf,
}

impl Env {
    /// Resolves paths from the real environment.
    pub fn from_system() -> Result<Env> {
        let home = dirs::home_dir().context("cannot determine the home directory")?;
        Ok(Env::from_vars(
            &home,
            dirs::cache_dir().unwrap_or_else(|| home.join(".cache")),
            dirs::data_dir().unwrap_or_else(|| home.join(".local/share")),
            |key| std::env::var_os(key),
        ))
    }

    /// Resolves paths using `var` to read environment variables.
    pub fn from_vars(
        home: &Path,
        system_cache: PathBuf,
        system_data: PathBuf,
        var: impl Fn(&str) -> Option<OsString>,
    ) -> Env {
        // Relative overrides are resolved once, against the directory
        // entropeek was started in, so files are never moved relative to
        // wherever a later command happens to run.
        let dir = |key: &str, default: PathBuf| {
            let path = var(key).filter(|value| !value.is_empty()).map(PathBuf::from).unwrap_or(default);
            std::path::absolute(&path).unwrap_or(path)
        };
        Env {
            home: std::path::absolute(home).unwrap_or_else(|_| home.to_path_buf()),
            claude_dir: dir("CLAUDE_CONFIG_DIR", home.join(".claude")),
            codex_dir: dir("CODEX_HOME", home.join(".codex")),
            opencode_db: opencode_db(dir("XDG_DATA_HOME", home.join(".local/share")).join("opencode"), &var),
            cache_dir: dir("ENTROPEEK_CACHE_DIR", system_cache.join("entropeek")),
            data_dir: dir("ENTROPEEK_DATA_DIR", system_data.join("entropeek")),
        }
    }

    /// Every path under `root`, laid out like a real home directory. For tests.
    pub fn under(root: &Path) -> Env {
        Env::from_vars(root, root.join(".cache"), root.join(".local/share"), |_| None)
    }

    pub fn trash_dir(&self) -> PathBuf {
        self.data_dir.join("trash")
    }

    pub fn home_str(&self) -> String {
        self.home.to_string_lossy().into_owned()
    }
}

/// Where OpenCode keeps its database, found the way OpenCode finds it: in
/// its `data` directory (under the XDG data directory on every platform,
/// macOS included), or `OPENCODE_DB` relative to that directory unless
/// absolute. An in-memory database (`:memory:`) has nothing to read, so
/// the default is used.
fn opencode_db(data: PathBuf, var: &impl Fn(&str) -> Option<OsString>) -> PathBuf {
    let db = var("OPENCODE_DB").filter(|value| !value.is_empty() && value != ":memory:");
    data.join(db.unwrap_or_else(|| "opencode.db".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_live_under_home() {
        let env = Env::under(Path::new("/h"));
        assert_eq!(env.claude_dir, PathBuf::from("/h/.claude"));
        assert_eq!(env.codex_dir, PathBuf::from("/h/.codex"));
        assert_eq!(env.opencode_db, PathBuf::from("/h/.local/share/opencode/opencode.db"));
        assert_eq!(env.cache_dir, PathBuf::from("/h/.cache/entropeek"));
        assert_eq!(env.trash_dir(), PathBuf::from("/h/.local/share/entropeek/trash"));
    }

    #[test]
    fn env_vars_override_defaults() {
        let env = Env::from_vars(Path::new("/h"), "/c".into(), "/d".into(), |key| match key {
            "CLAUDE_CONFIG_DIR" => Some("/claude".into()),
            "CODEX_HOME" => Some("/codex".into()),
            "ENTROPEEK_CACHE_DIR" => Some("".into()), // empty means unset
            "ENTROPEEK_DATA_DIR" => Some("/data".into()),
            _ => None,
        });
        assert_eq!(env.claude_dir, PathBuf::from("/claude"));
        assert_eq!(env.codex_dir, PathBuf::from("/codex"));
        assert_eq!(env.cache_dir, PathBuf::from("/c/entropeek"));
        assert_eq!(env.data_dir, PathBuf::from("/data"));
    }

    #[test]
    fn opencode_db_follows_opencode() {
        let db = |vars: &[(&str, &str)]| {
            let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            Env::from_vars(Path::new("/h"), "/c".into(), "/d".into(), |key| {
                vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.into())
            })
            .opencode_db
        };
        assert_eq!(db(&[("XDG_DATA_HOME", "/xdg")]), PathBuf::from("/xdg/opencode/opencode.db"));
        assert_eq!(db(&[("XDG_DATA_HOME", "")]), PathBuf::from("/h/.local/share/opencode/opencode.db"));
        assert_eq!(db(&[("OPENCODE_DB", "other.db")]), PathBuf::from("/h/.local/share/opencode/other.db"));
        assert_eq!(
            db(&[("OPENCODE_DB", "/elsewhere/oc.db"), ("XDG_DATA_HOME", "/xdg")]),
            PathBuf::from("/elsewhere/oc.db")
        );
        assert_eq!(db(&[("OPENCODE_DB", ":memory:")]), PathBuf::from("/h/.local/share/opencode/opencode.db"));
    }

    #[test]
    fn relative_overrides_become_absolute() {
        let env = Env::from_vars(Path::new("/h"), "/c".into(), "/d".into(), |key| {
            (key == "CLAUDE_CONFIG_DIR").then(|| "rel/claude".into())
        });
        assert!(env.claude_dir.is_absolute());
        assert_eq!(env.claude_dir, std::env::current_dir().unwrap().join("rel/claude"));
    }
}
