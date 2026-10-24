//! Text helpers: cleaning agent text for one-line display, width-aware
//! truncation and padding, word wrapping, and filtering injected "noise"
//! messages that agents store as if the user had typed them.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Collapses whitespace and removes control characters so `value` fits on one line.
pub fn one_line(value: &str) -> String {
    let mut out = String::with_capacity(value.len().min(512));
    let mut pending_space = false;
    for ch in value.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
        } else if ch.is_control() || is_bidi_control(ch) {
            continue;
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// Removes control characters (newlines become spaces) but keeps runs of
/// spaces, so text being typed shows exactly as entered.
pub fn one_line_keep_spaces(value: &str) -> String {
    value.chars().map(|c| if c.is_whitespace() { ' ' } else { c }).filter(|c| !is_unsafe_char(*c)).collect()
}

/// The end of `value` that fits in `max` cells, starting with `…` when cut.
pub fn tail(value: &str, max: usize) -> String {
    if value.width() <= max {
        return value.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0;
    for g in value.graphemes(true).rev() {
        let w = g.width();
        if used + w > max - 1 {
            break;
        }
        kept.push(g);
        used += w;
    }
    kept.reverse();
    format!("…{}", kept.concat())
}

/// Removes control characters but keeps newlines and tabs, so a transcript
/// keeps its layout without letting stored text drive the terminal.
pub fn sanitize_multiline(value: &str) -> String {
    value.chars().filter(|&ch| matches!(ch, '\n' | '\t') || !is_unsafe_char(ch)).collect()
}

/// A transcript message as stored, minus control characters, blank lines
/// before it and trailing whitespace. Indentation and spacing inside the
/// text (code!) are kept exactly.
pub fn transcript_text(value: &str) -> String {
    let clean = sanitize_multiline(value);
    let trimmed = clean.trim_end();
    let start = trimmed
        .char_indices()
        .take_while(|(_, c)| c.is_whitespace())
        .filter(|(_, c)| *c == '\n')
        .last()
        .map_or(0, |(i, _)| i + 1);
    trimmed[start..].to_string()
}

fn is_bidi_control(ch: char) -> bool {
    matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Whether printing `ch` could change terminal state or how text reads:
/// control characters (including newline) and bidirectional overrides.
pub fn is_unsafe_char(ch: char) -> bool {
    ch.is_control() || is_bidi_control(ch)
}

/// Like [`one_line`] but also caps the result at `max_chars` characters
/// (ending with `…` when cut), so huge pasted prompts don't bloat the cache.
/// Reads only as much of `value` as the cap needs, and never splits a
/// grapheme.
pub fn summary(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    // Normalize a bounded prefix: enough characters to decide, plus a few so
    // a grapheme at the cut is seen whole.
    let mut line = String::new();
    let mut count = 0;
    let mut pending_space = false;
    for ch in value.chars() {
        if ch.is_whitespace() {
            pending_space = count > 0;
            continue;
        }
        if is_unsafe_char(ch) {
            continue;
        }
        if pending_space {
            line.push(' ');
            count += 1;
            pending_space = false;
        }
        line.push(ch);
        count += 1;
        if count > max_chars + 8 {
            break;
        }
    }
    if count <= max_chars {
        return line;
    }
    let mut out = String::new();
    let mut used = 0;
    for g in line.graphemes(true) {
        let n = g.chars().count();
        if used + n > max_chars - 1 {
            break;
        }
        out.push_str(g);
        used += n;
    }
    out.push('…');
    out
}

/// Display width of `value` in terminal cells.
pub fn width(value: &str) -> usize {
    value.width()
}

/// Display width of one grapheme cluster.
fn grapheme_width(g: &str) -> usize {
    g.width()
}

/// The longest prefix of whole graphemes that fits in `max` cells.
fn head(value: &str, max: usize) -> (String, usize) {
    let mut out = String::new();
    let mut used = 0;
    for g in value.graphemes(true) {
        let w = grapheme_width(g);
        if used + w > max {
            break;
        }
        out.push_str(g);
        used += w;
    }
    (out, used)
}

/// Cuts `value` to at most `max` cells, ending with `…` when it was cut.
/// Graphemes (emoji sequences, combining marks) are never split.
pub fn truncate(value: &str, max: usize) -> String {
    if value.width() <= max {
        return value.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let (mut out, _) = head(value, max - 1);
    out.push('…');
    out
}

/// Cuts `value` to `max` cells keeping both ends, which suits paths.
pub fn truncate_middle(value: &str, max: usize) -> String {
    if value.width() <= max {
        return value.to_string();
    }
    if max <= 1 {
        return truncate(value, max);
    }
    let keep = max - 1;
    let tail_budget = keep / 2 + keep % 2;
    let head_budget = keep - tail_budget;
    let (front, _) = head(value, head_budget);
    let mut back: Vec<&str> = Vec::new();
    let mut used = 0;
    for g in value.graphemes(true).rev() {
        let w = grapheme_width(g);
        if used + w > tail_budget {
            break;
        }
        back.push(g);
        used += w;
    }
    back.reverse();
    format!("{front}…{}", back.concat())
}

/// Truncates or pads `value` with spaces to exactly `cells` cells.
pub fn fit(value: &str, cells: usize) -> String {
    let mut out = truncate(value, cells);
    let w = out.width();
    if w < cells {
        out.extend(std::iter::repeat_n(' ', cells - w));
    }
    out
}

/// Right-aligns `value` in exactly `cells` cells.
pub fn fit_right(value: &str, cells: usize) -> String {
    let cut = truncate(value, cells);
    let w = cut.width();
    format!("{}{cut}", " ".repeat(cells.saturating_sub(w)))
}

/// Word-wraps `value` to `max` cells per line.
///
/// - Existing newlines are kept, and so are runs of spaces inside a line
///   (aligned code stays aligned); spaces at a wrap point are dropped.
/// - A line's indentation is repeated on its continuation lines.
/// - Words longer than a line are split between graphemes; a single
///   grapheme wider than the whole line is shown as `…`.
pub fn wrap(value: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut lines = Vec::new();
    for raw in value.split('\n') {
        if raw.trim().is_empty() {
            lines.push(String::new());
            continue;
        }
        let content = raw.trim_start_matches(' ');
        let indent_w = (raw.len() - content.len()).min(max / 2);
        let indent = " ".repeat(indent_w);
        let mut line = indent.clone();
        let mut line_w = indent_w;
        let mut has_word = false;
        let mut gap = "";
        for token in split_runs(content) {
            if token.starts_with(' ') {
                gap = token;
                continue;
            }
            let word_w = token.width();
            let gap_w = if has_word { gap.len() } else { 0 };
            if line_w + gap_w + word_w <= max {
                if has_word {
                    line.push_str(gap);
                }
                line.push_str(token);
                line_w += gap_w + word_w;
                has_word = true;
                gap = "";
                continue;
            }
            if has_word {
                lines.push(std::mem::replace(&mut line, indent.clone()));
                line_w = indent_w;
                has_word = false;
            }
            gap = "";
            if line_w + word_w <= max {
                line.push_str(token);
                line_w += word_w;
                has_word = true;
                continue;
            }
            // Split a word that cannot fit on a line of its own.
            for g in token.graphemes(true) {
                let (g, w) =
                    if grapheme_width(g) > max - indent_w.min(max - 1) { ("…", 1) } else { (g, grapheme_width(g)) };
                if line_w + w > max && has_word {
                    lines.push(std::mem::replace(&mut line, indent.clone()));
                    line_w = indent_w;
                }
                line.push_str(g);
                line_w += w;
                has_word = true;
            }
        }
        lines.push(line);
    }
    lines
}

/// Splits text into alternating runs of spaces and non-space characters.
fn split_runs(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let space = rest.starts_with(' ');
        let end = rest.find(|c: char| (c == ' ') != space).unwrap_or(rest.len());
        let (run, tail) = rest.split_at(end);
        rest = tail;
        Some(run)
    })
}

/// Whether `text` (already trimmed) is a prompt the user typed, rather than
/// context the agent injected into a user-role message.
pub fn is_user_prompt(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    // Context agents inject as tagged blocks. Only a message that is made
    // of the block (it also ends with a tag) is noise; a prompt that merely
    // starts by quoting a tag is the user's.
    const NOISE_TAGS: &[&str] = &[
        "<environment_context>",
        "<permissions instructions>",
        "<collaboration_mode>",
        "<apps_instructions>",
        "<skills_instructions>",
        "<user_instructions>",
        "<system-reminder>",
        "<local-command-",
        "<command-name>",
        "<command-message>",
        "<turn_aborted>",
        "<user-prompt-submit-hook>",
    ];
    if NOISE_TAGS.iter().any(|tag| text.starts_with(tag)) && text.trim_end().ends_with('>') {
        return false;
    }
    if text.starts_with("[Request interrupted by user") && text.trim_end().ends_with(']') {
        return false;
    }
    const NOISE_PREFIXES: &[&str] = &[
        "# AGENTS.md instructions",
        "Caveat: The messages below were generated by the user while running local commands",
    ];
    if NOISE_PREFIXES.iter().any(|prefix| text.starts_with(prefix)) {
        return false;
    }
    if text.starts_with("# ") {
        let head = &text[..floor_char_boundary(text, 1000)];
        for marker in ["<INSTRUCTIONS>", "<filesystem>", "MEMORY_SUMMARY BEGINS"] {
            if head.contains(marker) {
                return false;
            }
        }
    }
    true
}

/// Largest index `<= index` that falls on a char boundary of `value`.
pub fn floor_char_boundary(value: &str, index: usize) -> usize {
    if index >= value.len() {
        return value.len();
    }
    let mut i = index;
    while !value.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Replaces the home directory prefix with `~`.
pub fn tilde(path: &str, home: &str) -> String {
    if home.is_empty() || home == "/" {
        return path.to_string();
    }
    match path.strip_prefix(home) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_collapses_whitespace_and_controls() {
        assert_eq!(one_line("  hello\n\n  world\t!  "), "hello world !");
        assert_eq!(one_line("a\u{1b}[31mb"), "a[31mb");
        assert_eq!(one_line("x\u{202e}y"), "xy");
        assert_eq!(one_line(""), "");
    }

    #[test]
    fn sanitize_multiline_keeps_newlines() {
        assert_eq!(sanitize_multiline("a\n\tb\u{7}c\r"), "a\n\tbc");
    }

    #[test]
    fn transcript_text_keeps_code_layout() {
        assert_eq!(transcript_text("    let x = 1;\n"), "    let x = 1;");
        assert_eq!(transcript_text("\n\n  def f():\n      return  1  \n\n"), "  def f():\n      return  1");
        assert_eq!(transcript_text("a  =  \"x  y\""), "a  =  \"x  y\"");
        assert_eq!(transcript_text("ok\u{1b}[31m"), "ok[31m");
    }

    #[test]
    fn summary_caps_length() {
        assert_eq!(summary("short", 10), "short");
        assert_eq!(summary("abcdefghij", 5), "abcd…");
        assert_eq!(summary("añbñcñdñ", 4).chars().count(), 4);
        assert_eq!(summary("a", 0), "");
        assert_eq!(summary("ab", 1), "…");
        assert_eq!(summary("a", 1), "a");
        assert_eq!(summary(" \n\t\u{7} ", 5), "");
        assert_eq!(summary("  hello \n  world ", 50), "hello world");
        // Huge input: only a prefix is read.
        let huge = "word ".repeat(2_000_000);
        assert_eq!(summary(&huge, 9), "word wor…");
        // Graphemes are not split.
        let family = "👩\u{200d}👩\u{200d}👧\u{200d}👦";
        assert_eq!(summary(&format!("{family}{family}"), 8), format!("{family}…"));
    }

    #[test]
    fn truncate_respects_cell_width() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("hello", 0), "");
        // CJK characters are two cells wide.
        let cut = truncate("日本語テキスト", 5);
        assert!(width(&cut) <= 5, "{cut}");
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn truncation_never_splits_graphemes() {
        let family = "👩\u{200d}👩\u{200d}👧\u{200d}👦";
        assert_eq!(truncate(&format!("{family}b"), 2), "…");
        assert_eq!(truncate(&format!("{family}bcd"), 4), format!("{family}b…"));
        assert_eq!(truncate("e\u{301}e\u{301}e\u{301}x", 3), "e\u{301}e\u{301}…");
        assert_eq!(truncate("🇯🇵🇫🇷x", 3), "🇯🇵…");
        assert_eq!(truncate_middle(&format!("{family}abcdef{family}"), 7), format!("{family}a…f{family}"));
        for cut in [truncate("日本語", 3), truncate_middle("日本語テキスト", 6)] {
            assert!(width(&cut) <= 6, "{cut}");
        }
    }

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail("short", 10), "short");
        assert_eq!(tail("a long search query", 8), "…h query");
        assert_eq!(width(&tail("日本語のクエリ", 5)), 5);
        assert_eq!(tail("abc", 0), "");
    }

    #[test]
    fn truncate_middle_keeps_both_ends() {
        assert_eq!(truncate_middle("/home/user/projects/app", 11), "/home…s/app");
        assert_eq!(width(&truncate_middle("/home/user/projects/app", 11)), 11);
        assert_eq!(truncate_middle("short", 10), "short");
    }

    #[test]
    fn fit_pads_and_truncates() {
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit_right("ab", 4), "  ab");
        assert_eq!(width(&fit("日本", 5)), 5);
    }

    #[test]
    fn wrap_breaks_on_words() {
        assert_eq!(wrap("the quick brown fox", 9), vec!["the quick", "brown fox"]);
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn wrap_splits_long_words_and_keeps_indent() {
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("    let x = 1;", 40), vec!["    let x = 1;"]);
        for line in wrap("supercalifragilistic expialidocious word", 7) {
            assert!(width(&line) <= 7, "{line:?}");
        }
    }

    #[test]
    fn wrap_keeps_words_apart_on_indented_continuations() {
        assert_eq!(wrap("    aaaa b c d", 8), vec!["    aaaa", "    b c", "    d"]);
        assert_eq!(wrap("  - one two three four", 12), vec!["  - one two", "  three four"]);
        let long = format!("  {}", "x".repeat(20));
        for line in wrap(&long, 8) {
            assert!(width(&line) <= 8 && line.starts_with("  "), "{line:?}");
        }
    }

    #[test]
    fn wrap_keeps_interior_spacing() {
        assert_eq!(wrap("let  x   = 1;  // aligned", 40), vec!["let  x   = 1;  // aligned"]);
        assert_eq!(wrap("a     b", 3), vec!["a", "b"], "spaces at a wrap point are dropped");
    }

    #[test]
    fn wrap_respects_graphemes_and_width() {
        let family = "👩\u{200d}👩\u{200d}👧\u{200d}👦";
        assert_eq!(wrap(&format!("a{family}b"), 2), vec!["a", family, "b"]);
        assert_eq!(wrap("日本", 1), vec!["…", "…"], "a 2-cell glyph never overflows a 1-cell line");
        for text in ["日本語のテキストです", "e\u{301}e\u{301}e\u{301}e\u{301}", "🇯🇵🇫🇷🇩🇪", "\u{17d8}\u{17d8}"]
        {
            for max in 1..6 {
                for line in wrap(text, max) {
                    assert!(width(&line) <= max, "{text:?} at {max}: {line:?}");
                }
            }
        }
    }

    #[test]
    fn noise_messages_are_not_prompts() {
        assert!(is_user_prompt("fix the bug in main.rs"));
        assert!(!is_user_prompt(""));
        assert!(!is_user_prompt("<environment_context>\n<cwd>/x</cwd>"));
        assert!(!is_user_prompt("<command-name>/model</command-name>"));
        assert!(!is_user_prompt("<local-command-stdout>ok</local-command-stdout>"));
        assert!(!is_user_prompt("[Request interrupted by user for tool use]"));
        assert!(!is_user_prompt("# AGENTS.md instructions for /x"));
        assert!(!is_user_prompt("# Notes\n<INSTRUCTIONS>do x</INSTRUCTIONS>"));
        assert!(is_user_prompt("# Heading the user typed"));
        // A user quoting a tag is still a prompt.
        assert!(is_user_prompt("<environment_context> shows up in my logs, why?"));
        assert!(is_user_prompt("<system-reminder> tags: how do they work"));
        assert!(is_user_prompt("[Request interrupted by user] means what?"));
    }

    #[test]
    fn floor_char_boundary_handles_multibyte() {
        let s = "añb";
        assert_eq!(floor_char_boundary(s, 2), 1);
        assert_eq!(floor_char_boundary(s, 100), s.len());
    }

    #[test]
    fn tilde_replaces_home_prefix_only() {
        assert_eq!(tilde("/home/u/x", "/home/u"), "~/x");
        assert_eq!(tilde("/home/u", "/home/u"), "~");
        assert_eq!(tilde("/home/user2/x", "/home/u"), "/home/user2/x");
        assert_eq!(tilde("/etc", ""), "/etc");
    }
}
