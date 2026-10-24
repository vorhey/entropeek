//! Extracts text, tool calls and attachments from a message `content` value.
//!
//! Both Claude and Codex store content either as a plain string or as an
//! array of typed blocks. Blocks keep their order, so a reply that says
//! something, calls a tool and says more reads back the same way. Only the
//! fields we display are deserialized; large tool inputs and results are
//! skipped by serde without being copied, and the text inside a tool result
//! is never read (it is output, not something anyone typed).

use serde::Deserialize;
use serde_json::value::RawValue;
use std::borrow::Cow;

use crate::text;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// Text exactly as stored.
    Text(String),
    /// A one-line summary of a tool call.
    Tool(String),
    /// An image or file attached to the message.
    Attachment,
    /// The output of a tool (not something the user typed).
    ToolResult,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Content {
    pub blocks: Vec<Block>,
}

impl Content {
    /// The text blocks, each cleaned for display, joined by blank lines.
    pub fn text(&self) -> String {
        let parts: Vec<String> = self
            .blocks
            .iter()
            .filter_map(|b| if let Block::Text(t) = b { Some(text::transcript_text(t)) } else { None })
            .filter(|t| !t.trim().is_empty())
            .collect();
        parts.join("\n\n")
    }

    pub fn has_tool_result(&self) -> bool {
        self.blocks.contains(&Block::ToolResult)
    }

    pub fn attachments(&self) -> usize {
        self.blocks.iter().filter(|b| **b == Block::Attachment).count()
    }

    /// What a user message says: its text, plus a note for attachments, so
    /// an image-only request still reads as a prompt. `None` when empty.
    pub fn user_text(&self) -> Option<String> {
        let text = self.text();
        let note = match self.attachments() {
            0 => None,
            1 => Some("[1 attachment]".to_string()),
            n => Some(format!("[{n} attachments]")),
        };
        match (text.trim().is_empty(), note) {
            (true, None) => None,
            (true, Some(note)) => Some(note),
            (false, None) => Some(text),
            (false, Some(note)) => Some(format!("{text}\n{note}")),
        }
    }
}

#[derive(Deserialize)]
struct RawBlock<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    text: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    name: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    input: Option<&'a RawValue>,
}

/// Parses a `content` value. Unknown shapes yield empty content.
pub fn extract(raw: &RawValue) -> Content {
    let json = raw.get();
    if json.starts_with('"') {
        let text: String = serde_json::from_str(json).unwrap_or_default();
        return Content { blocks: vec![Block::Text(text)] };
    }
    let Ok(raw_blocks) = serde_json::from_str::<Vec<RawBlock>>(json) else {
        return Content::default();
    };
    let blocks = raw_blocks
        .into_iter()
        .filter_map(|block| match block.kind.as_deref().unwrap_or("") {
            "text" | "input_text" | "output_text" => block.text.map(|t| Block::Text(t.into_owned())),
            "tool_use" | "server_tool_use" => {
                Some(Block::Tool(tool_summary(block.name.as_deref().unwrap_or("tool"), block.input)))
            }
            "image" | "input_image" | "document" | "file" | "input_file" => Some(Block::Attachment),
            "tool_result" => Some(Block::ToolResult),
            _ => None,
        })
        .collect();
    Content { blocks }
}

/// A string, or a list of strings (commands are given either way).
#[derive(Deserialize)]
#[serde(untagged)]
enum Words<'a> {
    #[serde(borrow)]
    One(Cow<'a, str>),
    #[serde(borrow)]
    Many(Vec<Cow<'a, str>>),
}

impl Words<'_> {
    fn text(&self) -> String {
        match self {
            Words::One(s) => s.to_string(),
            Words::Many(parts) => command_text(parts),
        }
    }
}

/// A command given as an argument list. `bash -lc 'script'` shows the
/// script, which is what the agent actually ran.
fn command_text(parts: &[Cow<str>]) -> String {
    match parts {
        [shell, flag, script] if shell.ends_with("sh") && flag.starts_with('-') && flag.ends_with('c') => {
            script.to_string()
        }
        _ => parts.join(" "),
    }
}

#[derive(Deserialize)]
struct ToolInput<'a> {
    #[serde(borrow, default)]
    description: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    command: Option<Words<'a>>,
    /// Codex `exec_command`.
    #[serde(borrow, default)]
    cmd: Option<Words<'a>>,
    #[serde(borrow, default)]
    file_path: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    path: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pattern: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    url: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    query: Option<Cow<'a, str>>,
}

/// `Name: detail`, where detail is the most telling string argument.
pub fn tool_summary(name: &str, input: Option<&RawValue>) -> String {
    let detail = input.and_then(|raw| serde_json::from_str::<ToolInput>(raw.get()).ok()).and_then(|input| {
        let command = input.command.as_ref().or(input.cmd.as_ref()).map(Words::text);
        [input.description.map(Cow::into_owned), command]
            .into_iter()
            .chain(
                [input.file_path, input.path, input.pattern, input.url, input.query]
                    .into_iter()
                    .map(|v| v.map(Cow::into_owned)),
            )
            .flatten()
            .find(|value| !value.trim().is_empty())
    });
    with_detail(name, detail.as_deref())
}

fn with_detail(name: &str, detail: Option<&str>) -> String {
    match detail.map(|d| text::summary(d, 120)).filter(|d| !d.is_empty()) {
        Some(detail) => format!("{name}: {detail}"),
        None => name.to_string(),
    }
}

/// Summary of a Codex free-form tool call (`custom_tool_call`), whose input
/// is plain text. Patches list the files they touch.
pub fn custom_tool_summary(name: &str, input: &str) -> String {
    if input.trim_start().starts_with("*** Begin Patch") {
        let files: Vec<String> = input
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                [("*** Update File: ", "update"), ("*** Add File: ", "add"), ("*** Delete File: ", "delete")]
                    .into_iter()
                    .find_map(|(prefix, verb)| line.strip_prefix(prefix).map(|path| format!("{verb} {path}")))
            })
            .collect();
        if !files.is_empty() {
            return with_detail(name, Some(&files.join(", ")));
        }
    }
    with_detail(name, Some(input))
}

#[derive(Deserialize)]
struct ShellAction<'a> {
    #[serde(borrow, default)]
    command: Option<Words<'a>>,
}

/// Summary of a Codex `local_shell_call`, whose command is in `action`.
pub fn shell_action_summary(name: &str, action: Option<&RawValue>) -> String {
    let command = action
        .and_then(|raw| serde_json::from_str::<ShellAction>(raw.get()).ok())
        .and_then(|a| a.command)
        .map(|c| c.text());
    with_detail(name, command.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(json: &str) -> Box<RawValue> {
        RawValue::from_string(json.to_string()).unwrap()
    }

    #[test]
    fn string_content() {
        let c = extract(&raw(r#""hello\nworld""#));
        assert_eq!(c.text(), "hello\nworld");
        assert_eq!(c.blocks.len(), 1);
    }

    #[test]
    fn blocks_keep_their_order() {
        let c = extract(&raw(r#"[{"type":"thinking","thinking":"hmm"},
                {"type":"text","text":"First"},
                {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la","description":"List files"}},
                {"type":"text","text":"Second\n"}]"#));
        assert_eq!(
            c.blocks,
            vec![Block::Text("First".into()), Block::Tool("Bash: List files".into()), Block::Text("Second\n".into())]
        );
        assert_eq!(c.text(), "First\n\nSecond");
        assert!(!c.has_tool_result());
    }

    #[test]
    fn text_keeps_indentation_and_spacing() {
        let c = extract(&raw(r#"[{"type":"text","text":"    let x  =  1;\n"}]"#));
        assert_eq!(c.text(), "    let x  =  1;");
    }

    #[test]
    fn codex_text_and_image_blocks() {
        let c = extract(&raw(r#"[{"type":"input_text","text":"do it"},{"type":"input_image","image_url":"x"}]"#));
        assert_eq!(c.text(), "do it");
        assert_eq!(c.attachments(), 1);
        assert_eq!(c.user_text().as_deref(), Some("do it\n[1 attachment]"));
        let only = extract(&raw(r#"[{"type":"image","source":{}},{"type":"image","source":{}}]"#));
        assert_eq!(only.user_text().as_deref(), Some("[2 attachments]"));
        assert_eq!(extract(&raw(r#"[{"type":"text","text":"  "}]"#)).user_text(), None);
    }

    #[test]
    fn tool_results_are_flagged_and_their_text_is_never_read() {
        let c =
            extract(&raw(r#"[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"output"}]},
                {"type":"text","text":"please correct that"}]"#));
        assert!(c.has_tool_result());
        assert_eq!(c.text(), "please correct that");
    }

    #[test]
    fn odd_shapes_do_not_panic() {
        assert_eq!(extract(&raw("42")), Content::default());
        assert_eq!(extract(&raw("null")), Content::default());
        assert_eq!(extract(&raw(r#"[1,2]"#)), Content::default());
    }

    #[test]
    fn tool_summaries() {
        assert_eq!(tool_summary("Read", Some(&raw(r#"{"file_path":"/a/b.rs"}"#))), "Read: /a/b.rs");
        assert_eq!(
            tool_summary("shell", Some(&raw(r#"{"command":["bash","-lc","cargo test"]}"#))),
            "shell: cargo test"
        );
        assert_eq!(tool_summary("shell", Some(&raw(r#"{"command":["ls","-la"]}"#))), "shell: ls -la");
        assert_eq!(tool_summary("exec_command", Some(&raw(r#"{"cmd":"rg TODO"}"#))), "exec_command: rg TODO");
        assert_eq!(tool_summary("Weird", Some(&raw(r#"{"command":[1,2]}"#))), "Weird");
        assert_eq!(tool_summary("NoInput", None), "NoInput");
    }

    #[test]
    fn codex_custom_and_shell_tools() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Add File: b.txt\n+hi\n*** End Patch";
        assert_eq!(custom_tool_summary("apply_patch", patch), "apply_patch: update src/a.rs, add b.txt");
        assert_eq!(custom_tool_summary("tool", "some free text"), "tool: some free text");
        assert_eq!(custom_tool_summary("tool", ""), "tool");
        let action = raw(r#"{"type":"exec","command":["bash","-lc","git status"],"timeout_ms":1000}"#);
        assert_eq!(shell_action_summary("local_shell", Some(&action)), "local_shell: git status");
        assert_eq!(shell_action_summary("local_shell", None), "local_shell");
    }
}
