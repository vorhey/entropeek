//! Core data types shared by every part of entropeek.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A coding agent whose sessions entropeek can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AgentKind {
    Claude,
    Codex,
}

impl AgentKind {
    /// Every supported agent, in display order. Digit keys in the TUI follow this order.
    pub const ALL: [AgentKind; 2] = [AgentKind::Claude, AgentKind::Codex];

    /// Stable lowercase id used on the command line and in JSON.
    pub fn id(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            AgentKind::Claude => "Claude",
            AgentKind::Codex => "Codex",
        }
    }

    /// The executable that resumes this agent's sessions.
    pub fn program(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
        }
    }

    pub fn from_id(value: &str) -> Option<AgentKind> {
        AgentKind::ALL.into_iter().find(|agent| agent.id().eq_ignore_ascii_case(value))
    }
}

/// One conversation stored on disk by an agent.
///
/// Timestamps are Unix milliseconds so the struct stays cheap to cache and compare.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub agent: AgentKind,
    pub id: String,
    /// The file that holds the conversation.
    pub path: PathBuf,
    /// The workspace the agent ran in. Empty when unknown.
    pub cwd: String,
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    pub started_at: Option<i64>,
    pub updated_at: i64,
    /// The session file's modification time when it was parsed (ns). With
    /// `size` it identifies the version of the file the metadata describes.
    pub mtime_ns: i64,
    pub git_branch: Option<String>,
    /// Number of prompts the user typed.
    pub user_turns: u32,
    /// Size of the session file in bytes.
    pub size: u64,
}

impl Session {
    /// The best one-line label: the agent's title, else the first prompt.
    pub fn label(&self) -> &str {
        self.title.as_deref().or(self.first_prompt.as_deref()).unwrap_or("(no prompt)")
    }

    /// Lowercased text that search matches against. Built once per session.
    pub fn search_text(&self) -> String {
        let mut text = String::with_capacity(256);
        for part in [
            self.agent.id(),
            self.id.as_str(),
            self.cwd.as_str(),
            self.title.as_deref().unwrap_or(""),
            self.first_prompt.as_deref().unwrap_or(""),
            self.last_prompt.as_deref().unwrap_or(""),
            self.git_branch.as_deref().unwrap_or(""),
        ] {
            text.push_str(part);
            text.push('\n');
        }
        text.to_lowercase()
    }
}

/// Who wrote a turn in a transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    /// A tool call made by the assistant, shown as a one-line summary.
    Tool,
}

/// One message in a conversation transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    pub timestamp: Option<i64>,
}

#[cfg(test)]
pub(crate) fn sample_session(agent: AgentKind, id: &str) -> Session {
    Session {
        agent,
        id: id.to_string(),
        path: PathBuf::from(format!("/tmp/{id}.jsonl")),
        cwd: "/home/user/project".to_string(),
        title: None,
        first_prompt: Some("first prompt".to_string()),
        last_prompt: Some("last prompt".to_string()),
        started_at: Some(1_000),
        updated_at: 2_000,
        mtime_ns: 0,
        git_branch: None,
        user_turns: 1,
        size: 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_ids_round_trip() {
        for agent in AgentKind::ALL {
            assert_eq!(AgentKind::from_id(agent.id()), Some(agent));
        }
        assert_eq!(AgentKind::from_id("CLAUDE"), Some(AgentKind::Claude));
        assert_eq!(AgentKind::from_id("gemini"), None);
    }

    #[test]
    fn label_prefers_title_then_first_prompt() {
        let mut session = sample_session(AgentKind::Claude, "a");
        assert_eq!(session.label(), "first prompt");
        session.title = Some("A title".into());
        assert_eq!(session.label(), "A title");
        session.title = None;
        session.first_prompt = None;
        assert_eq!(session.label(), "(no prompt)");
    }

    #[test]
    fn search_text_is_lowercase_and_covers_fields() {
        let mut session = sample_session(AgentKind::Codex, "ABC-123");
        session.title = Some("Fix The Parser".into());
        session.git_branch = Some("Feature/X".into());
        let text = session.search_text();
        for needle in
            ["codex", "abc-123", "/home/user/project", "fix the parser", "first prompt", "last prompt", "feature/x"]
        {
            assert!(text.contains(needle), "missing {needle:?} in {text:?}");
        }
        assert_eq!(text, text.to_lowercase());
    }
}
