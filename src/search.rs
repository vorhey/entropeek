//! Search and session lookup.

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::model::{AgentKind, Session};
use crate::text;

/// A search query: every word must appear (case-insensitively) somewhere in
/// the session's search text. Predictable, and fast enough to run on every
/// keystroke over tens of thousands of sessions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    words: Vec<String>,
}

impl Query {
    pub fn new(input: &str) -> Query {
        Query { words: input.split_whitespace().map(str::to_lowercase).collect() }
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// `haystack` must already be lowercase (see [`Session::search_text`]).
    pub fn matches(&self, haystack: &str) -> bool {
        self.words.iter().all(|word| haystack.contains(word.as_str()))
    }
}

/// How a command names a session. Parsed once, so policy checks (such as
/// "never delete `latest`") and lookup always agree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selector {
    /// The most recently updated session.
    Latest,
    /// An exact id or a unique id prefix (4+ characters), optionally
    /// limited to one agent with `claude:<id>` / `codex:<id>`.
    Id { agent: Option<AgentKind>, id: String },
    /// The session file itself (any argument containing `/`).
    Path(PathBuf),
}

impl Selector {
    pub fn parse(input: &str) -> Result<Selector> {
        let input = input.trim();
        if input.is_empty() {
            bail!("empty session id");
        }
        if input == "latest" {
            return Ok(Selector::Latest);
        }
        if input.contains('/') {
            let path = PathBuf::from(input);
            return Ok(Selector::Path(std::path::absolute(&path).unwrap_or(path)));
        }
        if let Some((prefix, rest)) = input.split_once(':')
            && let Some(agent) = AgentKind::from_id(prefix)
        {
            let id = rest.trim();
            if id.is_empty() {
                bail!("empty session id after {prefix}:");
            }
            return Ok(Selector::Id { agent: Some(agent), id: id.to_string() });
        }
        Ok(Selector::Id { agent: None, id: input.to_string() })
    }
}

/// Finds one session by `latest`, `[agent:]id`, a unique id prefix of at
/// least 4 characters, or the session file's path. Several sessions with the
/// same id are an error, never a silent pick.
pub fn find_session<'a>(sessions: &'a [Session], selector: &str) -> Result<&'a Session> {
    find(sessions, &Selector::parse(selector)?)
}

pub fn find<'a>(sessions: &'a [Session], selector: &Selector) -> Result<&'a Session> {
    let (agent, id) = match selector {
        Selector::Latest => return sessions.first().ok_or_else(|| anyhow::anyhow!("no sessions found")),
        Selector::Path(path) => {
            let real = std::fs::canonicalize(path).ok();
            return sessions
                .iter()
                .find(|s| &s.path == path || (real.is_some() && std::fs::canonicalize(&s.path).ok() == real))
                .ok_or_else(|| anyhow::anyhow!("no session is stored at {}", path.display()));
        }
        Selector::Id { agent, id } => (*agent, id.as_str()),
    };
    let candidates = || sessions.iter().filter(move |s| agent.is_none_or(|a| s.agent == a));
    let exact: Vec<&Session> = candidates().filter(|s| s.id == id).collect();
    match exact.as_slice() {
        [one] => return Ok(one),
        [] => {}
        many => bail!("{}", ambiguous(id, many)),
    }
    if id.chars().count() < 4 {
        bail!("no session with id {id:?} (prefixes need at least 4 characters)");
    }
    let matches: Vec<&Session> = candidates().filter(|s| s.id.starts_with(id)).collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => bail!("no session with id {id:?}"),
        many => bail!("{}", ambiguous(id, many)),
    }
}

fn ambiguous(id: &str, many: &[&Session]) -> String {
    let shown: Vec<String> = many
        .iter()
        .take(5)
        .map(|s| format!("{}:{} ({})", s.agent.id(), text::one_line(&s.id), text::one_line(&s.path.to_string_lossy())))
        .collect();
    format!(
        "{:?} matches {} sessions: {}; use a longer id, `agent:id`, or the session file path",
        text::one_line(id),
        many.len(),
        shown.join(", ")
    )
}

/// The shortest id prefix length (at least 8) that tells every session in
/// `sessions` apart, so ids printed in tables can be used as selectors.
pub fn unique_prefix_len(sessions: &[Session]) -> usize {
    let mut ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    let longest = ids.iter().map(|id| id.chars().count()).max().unwrap_or(0);
    let needed = ids
        .windows(2)
        .map(|pair| pair[0].chars().zip(pair[1].chars()).take_while(|(a, b)| a == b).count() + 1)
        .max()
        .unwrap_or(0);
    needed.max(8).min(longest.max(8))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, sample_session};

    #[test]
    fn all_words_must_match() {
        let q = Query::new("  Rate   LIMIT ");
        assert!(!q.is_empty());
        assert!(q.matches("add rate limiting to the api"));
        assert!(!q.matches("add rate control"));
        assert!(Query::new("").matches("anything"));
        assert!(Query::new("   ").is_empty());
    }

    #[test]
    fn finds_sessions_by_selector() {
        let sessions = vec![
            sample_session(AgentKind::Claude, "abcd1111"),
            sample_session(AgentKind::Codex, "abcd2222"),
            sample_session(AgentKind::Claude, "ffff0000"),
        ];
        assert_eq!(find_session(&sessions, "latest").unwrap().id, "abcd1111");
        assert_eq!(find_session(&sessions, "ffff0000").unwrap().id, "ffff0000");
        assert_eq!(find_session(&sessions, "abcd2").unwrap().id, "abcd2222");
        let err = find_session(&sessions, "abcd").unwrap_err().to_string();
        assert!(err.contains("matches 2 sessions"), "{err}");
        assert!(find_session(&sessions, "ab").is_err());
        assert!(find_session(&sessions, "zzzzzz").is_err());
        assert!(find_session(&[], "latest").is_err());
        assert_eq!(find_session(&sessions, "codex:abcd").unwrap().id, "abcd2222");
        assert!(find_session(&sessions, "codex:ffff0000").is_err());
    }

    #[test]
    fn selectors_are_normalized_once() {
        assert_eq!(Selector::parse(" latest ").unwrap(), Selector::Latest);
        assert_eq!(Selector::parse("\tlatest\n").unwrap(), Selector::Latest);
        assert_eq!(
            Selector::parse(" claude:abc ").unwrap(),
            Selector::Id { agent: Some(AgentKind::Claude), id: "abc".into() }
        );
        assert_eq!(Selector::parse("abc").unwrap(), Selector::Id { agent: None, id: "abc".into() });
        assert!(matches!(Selector::parse("/x/y.jsonl").unwrap(), Selector::Path(_)));
        assert!(Selector::parse("   ").is_err());
        assert!(Selector::parse("claude:").is_err());
    }

    #[test]
    fn duplicate_exact_ids_are_ambiguous() {
        let mut a = sample_session(AgentKind::Claude, "same-id-1234");
        a.path = "/one/same.jsonl".into();
        let mut b = sample_session(AgentKind::Claude, "same-id-1234");
        b.path = "/two/same.jsonl".into();
        let mut c = sample_session(AgentKind::Codex, "same-id-1234");
        c.path = "/three/same.jsonl".into();
        let sessions = vec![a, b, c];
        let err = find_session(&sessions, "same-id-1234").unwrap_err().to_string();
        assert!(err.contains("matches 3 sessions") && err.contains("/two/same.jsonl"), "{err}");
        assert_eq!(find_session(&sessions, "codex:same-id-1234").unwrap().path, PathBuf::from("/three/same.jsonl"));
        assert!(find_session(&sessions, "claude:same-id-1234").is_err());
        assert_eq!(find_session(&sessions, "/two/same.jsonl").unwrap().path, PathBuf::from("/two/same.jsonl"));
    }

    #[test]
    fn unique_prefixes_tell_sessions_apart() {
        let s = |id: &str| sample_session(AgentKind::Claude, id);
        assert_eq!(unique_prefix_len(&[s("aaaaaaaa-1"), s("bbbbbbbb-2")]), 8);
        assert_eq!(unique_prefix_len(&[s("aaaaaaaa-0001"), s("aaaaaaaa-0002")]), 13);
        assert_eq!(unique_prefix_len(&[s("dup-dup-dup"), s("dup-dup-dup")]), 8);
        assert_eq!(unique_prefix_len(&[]), 8);
    }
}
