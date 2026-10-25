//! Nerd Font icons, and deciding whether the terminal can show them.
//!
//! Nerd Font glyphs live in Unicode's private use area: with a Nerd Font
//! they are icons, otherwise empty boxes. No escape sequence or variable
//! reports a terminal's font, so — like other tools that do this, e.g.
//! `has-nerd-font` — entropeek infers it, cheapest evidence first:
//!
//! 1. `--icons` / `ENTROPEEK_ICONS`, then the common `NERD_FONT=1|0`;
//! 2. which terminal this is: environment variables, the terminal's
//!    XTVERSION answer (works over SSH), and inside tmux, tmux's client;
//! 3. terminals that ship Nerd Font symbols built in (kitty 0.36+,
//!    WezTerm, Ghostty) can always show them;
//! 4. otherwise the font: kitty reports it (XTGETTCAP); for Alacritty,
//!    foot, Konsole and VS Code it is read from their configuration;
//! 5. when still unsure, no icons: a missing icon beats a broken glyph.
//!
//! `i` switches icons while running.

use std::path::{Path, PathBuf};

use super::terminal::Reply;
use crate::model::AgentKind;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum IconChoice {
    /// On when the terminal is known to show Nerd Font glyphs.
    #[default]
    Auto,
    On,
    Off,
}

impl IconChoice {
    /// Reads `ENTROPEEK_ICONS` (`auto`, `on`/`nerd`/`1`, `off`/`none`/`0`).
    pub fn from_env(value: Option<&str>) -> Option<IconChoice> {
        match value?.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(IconChoice::Auto),
            "on" | "nerd" | "1" | "true" | "yes" => Some(IconChoice::On),
            "off" | "none" | "0" | "false" | "no" => Some(IconChoice::Off),
            _ => None,
        }
    }

    /// Whether to show icons, and why. Detection only runs for `auto`.
    pub fn resolve(self, detect: impl FnOnce() -> Detection) -> Detection {
        match self {
            IconChoice::On => Detection::yes("--icons on / ENTROPEEK_ICONS"),
            IconChoice::Off => Detection::no("--icons off / ENTROPEEK_ICONS"),
            IconChoice::Auto => detect(),
        }
    }
}

/// The answer, with the evidence it rests on (shown when toggling icons).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detection {
    pub icons: bool,
    pub reason: String,
}

impl Detection {
    fn yes(reason: impl Into<String>) -> Detection {
        Detection { icons: true, reason: reason.into() }
    }

    fn no(reason: impl Into<String>) -> Detection {
        Detection { icons: false, reason: reason.into() }
    }
}

/// The glyphs entropeek uses (Nerd Fonts v3 code points).
pub mod glyph {
    pub const FOLDER: &str = "\u{f07b}"; // nf-fa-folder
    pub const FOLDER_OPEN: &str = "\u{f07c}"; // nf-fa-folder_open
    pub const BRANCH: &str = "\u{e725}"; // nf-dev-git_branch
    pub const USER: &str = "\u{f007}"; // nf-fa-user
    pub const ROBOT: &str = "\u{f06a9}"; // nf-md-robot
    pub const CODE: &str = "\u{f121}"; // nf-fa-code
    pub const WRENCH: &str = "\u{f0ad}"; // nf-fa-wrench
    pub const SEARCH: &str = "\u{f002}"; // nf-fa-search
    pub const LIST: &str = "\u{f03a}"; // nf-fa-list
    pub const COMMENTS: &str = "\u{f086}"; // nf-fa-comments
    pub const KEYBOARD: &str = "\u{f11c}"; // nf-fa-keyboard
    pub const CLOCK: &str = "\u{f017}"; // nf-fa-clock
    pub const TERMINAL: &str = "\u{f120}"; // nf-fa-terminal
    pub const LOGO: &str = "\u{f0405}"; // nf-md-tree, entropeek's logo
}

/// An agent's icon.
pub fn agent(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Claude => glyph::ROBOT,
        AgentKind::Codex => glyph::CODE,
    }
}

/// Everything detection looks at, gathered by the caller so detection
/// itself is a pure function.
pub struct Facts<'a> {
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// The terminal's answers to the startup query.
    pub reply: &'a Reply,
    /// Inside tmux: the outer terminal, as tmux knows it.
    pub tmux_client: &'a dyn Fn() -> Option<String>,
    /// Reads a configuration file.
    pub read: &'a dyn Fn(&Path) -> Option<String>,
    pub home: &'a Path,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Emulator {
    /// With its version, when known.
    Kitty(Option<(u32, u32)>),
    WezTerm,
    Ghostty,
    Alacritty,
    Foot,
    Konsole,
    VsCode,
}

impl Emulator {
    fn name(&self) -> &'static str {
        match self {
            Emulator::Kitty(_) => "kitty",
            Emulator::WezTerm => "WezTerm",
            Emulator::Ghostty => "Ghostty",
            Emulator::Alacritty => "Alacritty",
            Emulator::Foot => "foot",
            Emulator::Konsole => "Konsole",
            Emulator::VsCode => "VS Code",
        }
    }
}

/// kitty ships Nerd Font symbols since 0.36.0 (2024).
const KITTY_BUILTIN_NERD: (u32, u32) = (0, 36);

pub fn detect(facts: &Facts) -> Detection {
    let env = |key: &str| (facts.env)(key).filter(|v| !v.is_empty());
    match env("NERD_FONT").map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("1" | "true" | "yes") => return Detection::yes("NERD_FONT=1"),
        Some("0" | "false" | "no") => return Detection::no("NERD_FONT=0"),
        _ => {}
    }
    let Some((term, via)) = identify(facts) else {
        let seen = env("TERM_PROGRAM").or_else(|| env("TERM")).unwrap_or_else(|| "this terminal".into());
        return Detection::no(format!(
            "{seen} is not one entropeek can check (set NERD_FONT=1 if it uses a Nerd Font)"
        ));
    };
    let name = format!("{}{via}", term.name());
    match term {
        Emulator::WezTerm | Emulator::Ghostty => {
            return Detection::yes(format!("{name} has Nerd Font symbols built in"));
        }
        Emulator::Kitty(version) if version.is_none_or(|v| v >= KITTY_BUILTIN_NERD) => {
            return Detection::yes(format!("{name} has Nerd Font symbols built in"));
        }
        _ => {}
    }
    // The font has to tell. kitty says which it uses; others are read
    // from their configuration, which is only here on this machine.
    if let (Emulator::Kitty(_), Some(font)) = (&term, &facts.reply.font) {
        return judge(&name, font, "reported by kitty");
    }
    if env("SSH_CONNECTION").is_some() || env("SSH_TTY").is_some() {
        return Detection::no(format!("{name}'s settings are on the machine you connected from"));
    }
    match configured_font(&term, facts) {
        Some((font, path)) => judge(&name, &font, &format!("from {}", path.display())),
        None => Detection::no(format!("could not find {name}'s font setting")),
    }
}

fn judge(name: &str, font: &str, source: &str) -> Detection {
    if looks_like_nerd_font(font) {
        Detection::yes(format!("{name} uses {font} ({source})"))
    } else {
        Detection::no(format!("{name} uses {font}, not a Nerd Font ({source})"))
    }
}

/// Which terminal this is, and how we know when it is not obvious.
fn identify(facts: &Facts) -> Option<(Emulator, &'static str)> {
    let env = |key: &str| (facts.env)(key).filter(|v| !v.is_empty());
    let term = env("TERM").unwrap_or_default();
    let program = env("TERM_PROGRAM").unwrap_or_default();
    let reply_version = facts.reply.version.as_deref();
    if env("KITTY_WINDOW_ID").is_some() || term == "xterm-kitty" {
        let version = reply_version.and_then(kitty_version);
        return Some((Emulator::Kitty(version), ""));
    }
    let by_env = if program == "WezTerm" || env("WEZTERM_PANE").is_some() {
        Some(Emulator::WezTerm)
    } else if program == "ghostty" || term == "xterm-ghostty" || env("GHOSTTY_RESOURCES_DIR").is_some() {
        Some(Emulator::Ghostty)
    } else if program == "vscode" {
        Some(Emulator::VsCode)
    } else if env("ALACRITTY_WINDOW_ID").is_some() || term == "alacritty" {
        Some(Emulator::Alacritty)
    } else if term.starts_with("foot") {
        Some(Emulator::Foot)
    } else if env("KONSOLE_VERSION").is_some() {
        Some(Emulator::Konsole)
    } else {
        None
    };
    if let Some(found) = by_env {
        return Some((found, ""));
    }
    if let Some(found) = reply_version.and_then(from_name) {
        return Some((found, ""));
    }
    if env("TMUX").is_some() {
        return (facts.tmux_client)().and_then(|client| from_name(&client)).map(|t| (t, " (outside tmux)"));
    }
    None
}

/// A terminal from an XTVERSION answer or a terminal type name.
fn from_name(name: &str) -> Option<Emulator> {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("kitty") || lower == "xterm-kitty" {
        Some(Emulator::Kitty(kitty_version(name)))
    } else if lower.starts_with("wezterm") {
        Some(Emulator::WezTerm)
    } else if lower.starts_with("ghostty") || lower == "xterm-ghostty" {
        Some(Emulator::Ghostty)
    } else if lower.starts_with("foot") {
        Some(Emulator::Foot)
    } else if lower.starts_with("alacritty") {
        Some(Emulator::Alacritty)
    } else if lower.starts_with("konsole") {
        Some(Emulator::Konsole)
    } else {
        None
    }
}

/// `kitty(0.45.0)` -> `(0, 45)`.
fn kitty_version(text: &str) -> Option<(u32, u32)> {
    let inner = text.strip_prefix("kitty")?.trim_start_matches(['(', ' ']);
    let mut parts = inner.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty());
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// The configured font of a terminal that has no built-in symbols, and
/// the file it came from.
fn configured_font(term: &Emulator, facts: &Facts) -> Option<(String, PathBuf)> {
    let config = (facts.env)("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| facts.home.join(".config"));
    let first = |paths: Vec<PathBuf>, find: &dyn Fn(&str) -> Option<String>| {
        paths.into_iter().find_map(|path| (facts.read)(&path).and_then(|text| find(&text)).map(|font| (font, path)))
    };
    match term {
        Emulator::Kitty(_) => first(vec![config.join("kitty/kitty.conf")], &|text| setting(text, "font_family", ' ')),
        Emulator::Alacritty => first(
            vec![
                config.join("alacritty/alacritty.toml"),
                config.join("alacritty/alacritty.yml"),
                facts.home.join(".alacritty.toml"),
            ],
            &|text| alacritty_family(text),
        ),
        Emulator::Foot => first(vec![config.join("foot/foot.ini")], &|text| {
            setting(text, "font", '=').map(|v| v.split([':', ',']).next().unwrap_or("").trim().to_string())
        }),
        Emulator::Konsole => {
            let rc = (facts.read)(&config.join("konsolerc"))?;
            let profile = setting(&rc, "DefaultProfile", '=')?;
            let path = facts.home.join(".local/share/konsole").join(profile);
            let font = setting(&(facts.read)(&path)?, "Font", '=')?;
            Some((font.split(',').next().unwrap_or("").to_string(), path))
        }
        Emulator::VsCode => {
            let paths: Vec<PathBuf> = ["Code", "Code - OSS", "VSCodium"]
                .iter()
                .flat_map(|app| {
                    [
                        config.join(app).join("User/settings.json"),
                        facts.home.join("Library/Application Support").join(app).join("User/settings.json"),
                    ]
                })
                .collect();
            first(paths, &|text| {
                json_string(text, "terminal.integrated.fontFamily").or_else(|| json_string(text, "editor.fontFamily"))
            })
        }
        Emulator::WezTerm | Emulator::Ghostty => None,
    }
}

/// The value of `key <sep> value` on a line of a simple config file,
/// ignoring comments and quotes. The last setting wins.
fn setting(text: &str, key: &str, sep: char) -> Option<String> {
    // The last setting wins, so search from the end.
    text.lines().rev().find_map(|line| {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            return None;
        }
        let rest = line.strip_prefix(key)?;
        let rest = if sep == ' ' {
            rest.starts_with(char::is_whitespace).then_some(rest)?
        } else {
            rest.trim_start().strip_prefix(sep)?
        };
        let value = rest.trim().trim_matches(['"', '\'']).trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// `"key": "value"` in a JSON-with-comments settings file. Commented-out
/// settings are ignored, and the last setting wins (as in VS Code).
fn json_string(text: &str, key: &str) -> Option<String> {
    let text = strip_json_comments(text);
    let needle = format!("\"{key}\"");
    let mut found = None;
    let mut from = 0;
    while let Some(at) = text[from..].find(&needle) {
        let after = from + at + needle.len();
        from = after;
        let Some(rest) = text[after..].trim_start().strip_prefix(':') else { continue };
        let Some(rest) = rest.trim_start().strip_prefix('"') else { continue };
        if let Some(end) = rest.find('"') {
            found = Some(rest[..end].to_string()).filter(|v| !v.trim().is_empty());
        }
    }
    found
}

/// `text` without `//` and `/* */` comments (outside strings).
fn strip_json_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Alacritty's normal (not bold or italic) font family, from
/// `alacritty.toml` (`[font.normal] family = …`, `[font] normal = { family
/// = … }`, `font.normal.family = …`) or the older `alacritty.yml`.
fn alacritty_family(text: &str) -> Option<String> {
    let unquote = |v: &str| {
        let v = v.trim();
        let v = v.split_once(" #").map_or(v, |(v, _)| v).trim();
        let v = v.trim_matches(['"', '\'']).trim();
        (!v.is_empty()).then(|| v.to_string())
    };
    // TOML
    let mut section = String::new();
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            section = header.trim_end_matches(']').replace([' ', '"'], "");
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim().replace([' ', '"'], "");
        let full = if section.is_empty() { key } else { format!("{section}.{key}") };
        match full.as_str() {
            "font.normal.family" => found = unquote(value).or(found),
            "font.normal" => {
                if let Some((_, rest)) = value.split_once("family") {
                    let rest = rest.trim_start().strip_prefix('=').unwrap_or("");
                    let rest = rest.trim_start();
                    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
                    if let Some(quote) = quote
                        && let Some(end) = rest[1..].find(quote)
                    {
                        found = unquote(&rest[1..1 + end]).or(found);
                    }
                }
            }
            _ => {}
        }
    }
    if found.is_some() {
        return found;
    }
    // YAML: the `family` key under `font:` / `normal:`.
    let mut path: Vec<(usize, &str)> = Vec::new();
    for line in text.lines() {
        let content = line.trim_start();
        if content.starts_with('#') || content.is_empty() {
            continue;
        }
        let indent = line.len() - content.len();
        let Some((key, value)) = content.split_once(':') else { continue };
        while path.last().is_some_and(|(at, _)| *at >= indent) {
            path.pop();
        }
        let key = key.trim().trim_matches(['"', '\'']);
        if key == "family" && path.iter().map(|(_, k)| *k).eq(["font", "normal"]) {
            found = unquote(value).or(found);
        }
        path.push((indent, key));
    }
    found
}

/// Nerd Font family and PostScript names: `JetBrainsMono Nerd Font`,
/// `MesloLGS NF`, `CaskaydiaCove NFM`, `JetBrainsMonoNF-Regular`.
pub fn looks_like_nerd_font(font: &str) -> bool {
    if font.to_ascii_lowercase().contains("nerd") {
        return true;
    }
    font.split([' ', ',', '-', '\'', '"']).any(|word| {
        ["NFM", "NFP", "NF"].iter().any(|suffix| {
            word == *suffix
                || word
                    .strip_suffix(suffix)
                    .and_then(|stem| stem.chars().last())
                    .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
    })
}

/// The most of a configuration file read: real ones are far smaller.
const CONFIG_LIMIT: u64 = 1024 * 1024;

/// Reads a terminal configuration file, or its first megabyte.
pub fn read_config(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(path).ok()?.take(CONFIG_LIMIT).read_to_string(&mut text).ok()?;
    Some(text)
}

/// How long tmux gets to answer before it is stopped.
const TMUX_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Inside tmux: the outer terminal, as `kitty(0.45.0)` (tmux 3.3+) or a
/// terminal type like `xterm-kitty`. Gives up (and stops tmux) after
/// [`TMUX_TIMEOUT`].
pub fn tmux_client() -> Option<String> {
    tmux_client_with("tmux", TMUX_TIMEOUT)
}

fn tmux_client_with(program: &str, timeout: std::time::Duration) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(program)
        .args(["display-message", "-p", "#{client_termtype}\t#{client_termname}"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut stdout = Vec::new();
    child.stdout.take()?.take(4096).read_to_end(&mut stdout).ok()?;
    let text = String::from_utf8(stdout).ok()?;
    let mut parts = text.trim().split('\t');
    let termtype = parts.next().unwrap_or("").trim();
    let termname = parts.next().unwrap_or("").trim();
    [termtype, termname].into_iter().find(|v| !v.is_empty() && from_name(v).is_some()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct World {
        env: HashMap<&'static str, &'static str>,
        reply: Reply,
        tmux: Option<&'static str>,
        files: HashMap<PathBuf, &'static str>,
    }

    impl World {
        fn new(env: &[(&'static str, &'static str)]) -> World {
            World { env: env.iter().copied().collect(), reply: Reply::default(), tmux: None, files: HashMap::new() }
        }

        fn file(mut self, path: &str, text: &'static str) -> World {
            self.files.insert(PathBuf::from(path), text);
            self
        }

        fn detect(&self) -> Detection {
            let env = |k: &str| self.env.get(k).map(|v| v.to_string());
            let tmux = || self.tmux.map(str::to_string);
            let read = |p: &Path| self.files.get(p).map(|t| t.to_string());
            detect(&Facts { env: &env, reply: &self.reply, tmux_client: &tmux, read: &read, home: Path::new("/h") })
        }
    }

    #[test]
    fn nerd_font_variable_wins() {
        assert!(World::new(&[("NERD_FONT", "1"), ("TERM", "xterm")]).detect().icons);
        assert!(!World::new(&[("NERD_FONT", "0"), ("TERM", "xterm-kitty")]).detect().icons);
    }

    #[test]
    fn terminals_with_built_in_symbols() {
        for env in [
            vec![("KITTY_WINDOW_ID", "1")],
            vec![("TERM", "xterm-kitty")],
            vec![("TERM_PROGRAM", "WezTerm")],
            vec![("WEZTERM_PANE", "0")],
            vec![("TERM_PROGRAM", "ghostty")],
            vec![("TERM", "xterm-ghostty")],
        ] {
            let found = World::new(&env).detect();
            assert!(found.icons, "{env:?}: {}", found.reason);
            assert!(found.reason.contains("built in"), "{}", found.reason);
        }
    }

    #[test]
    fn old_kitty_falls_back_to_its_font() {
        let mut world = World::new(&[("KITTY_WINDOW_ID", "1")]);
        world.reply.version = Some("kitty(0.35.2)".into());
        world.reply.font = Some("JetBrainsMonoNF-Regular".into());
        assert!(world.detect().icons);
        world.reply.font = Some("DejaVuSansMono".into());
        let plain = world.detect();
        assert!(!plain.icons && plain.reason.contains("DejaVuSansMono"), "{}", plain.reason);
        world.reply.font = None;
        let world =
            world.file("/h/.config/kitty/kitty.conf", "# font_family Nope\nfont_family   Hack Nerd Font Mono\n");
        assert!(world.detect().icons, "{}", world.detect().reason);
    }

    #[test]
    fn the_terminal_version_answer_identifies_terminals_over_ssh() {
        let mut world = World::new(&[("TERM", "xterm-256color"), ("SSH_CONNECTION", "1.2.3.4 5 6.7.8.9 22")]);
        world.reply.version = Some("WezTerm 20240203-110809-5046fc22".into());
        assert!(world.detect().icons);
        world.reply.version = Some("kitty(0.45.0)".into());
        assert!(world.detect().icons);
        world.reply.version = Some("foot(1.18.1)".into());
        let remote = world.detect();
        assert!(!remote.icons && remote.reason.contains("machine you connected from"), "{}", remote.reason);
    }

    #[test]
    fn inside_tmux_the_outer_terminal_counts() {
        let mut world =
            World::new(&[("TERM", "tmux-256color"), ("TERM_PROGRAM", "tmux"), ("TMUX", "/tmp/tmux-1000/default,1,0")]);
        world.reply.version = Some("tmux 3.6".into());
        assert!(!world.detect().icons, "tmux alone tells nothing");
        world.tmux = Some("kitty(0.45.0)");
        let found = world.detect();
        assert!(found.icons && found.reason.contains("outside tmux"), "{}", found.reason);
        // kitty's variables usually survive into tmux, which is enough.
        let inherited = World::new(&[("TERM_PROGRAM", "tmux"), ("TMUX", "x"), ("KITTY_WINDOW_ID", "1")]);
        assert!(inherited.detect().icons);
    }

    #[test]
    fn other_terminals_are_judged_by_their_configured_font() {
        let alacritty = World::new(&[("ALACRITTY_WINDOW_ID", "1")]).file(
            "/h/.config/alacritty/alacritty.toml",
            "[font.normal]\nfamily = \"JetBrainsMono Nerd Font\"\nstyle = \"Regular\"\n",
        );
        assert!(alacritty.detect().icons, "{}", alacritty.detect().reason);
        let plain = World::new(&[("TERM", "alacritty")])
            .file("/h/.config/alacritty/alacritty.toml", "[font.normal]\nfamily = \"Fira Code\"\n");
        assert!(!plain.detect().icons);
        assert!(plain.detect().reason.contains("Fira Code"));

        let foot = World::new(&[("TERM", "foot")])
            .file("/h/.config/foot/foot.ini", "[main]\nfont=FiraCode Nerd Font:size=11\n");
        assert!(foot.detect().icons);

        let konsole = World::new(&[("KONSOLE_VERSION", "240802")])
            .file("/h/.config/konsolerc", "[Desktop Entry]\nDefaultProfile=Mine.profile\n")
            .file("/h/.local/share/konsole/Mine.profile", "[Appearance]\nFont=MesloLGS NF,11,-1,5,50,0,0,0,0,0\n");
        assert!(konsole.detect().icons, "{}", konsole.detect().reason);

        let vscode = World::new(&[("TERM_PROGRAM", "vscode"), ("XDG_CONFIG_HOME", "/cfg")]).file(
            "/cfg/Code/User/settings.json",
            "{\n  // comment\n  \"editor.fontFamily\": \"Menlo\",\n  \"terminal.integrated.fontFamily\": \"'CaskaydiaCove NFM', monospace\"\n}",
        );
        assert!(vscode.detect().icons, "{}", vscode.detect().reason);

        let missing = World::new(&[("TERM", "foot")]);
        assert!(!missing.detect().icons && missing.detect().reason.contains("font setting"));
    }

    #[test]
    fn only_the_active_normal_font_counts() {
        let toml = |text: &'static str| {
            World::new(&[("ALACRITTY_WINDOW_ID", "1")]).file("/h/.config/alacritty/alacritty.toml", text).detect()
        };
        let italic = toml("[font.normal]\nfamily = \"Arial\"\n[font.italic]\nfamily = \"JetBrainsMono Nerd Font\"\n");
        assert!(!italic.icons && italic.reason.contains("Arial"), "{}", italic.reason);
        assert!(toml("[font]\nnormal = { family = \"Hack Nerd Font\", style = \"Regular\" }\n").icons);
        assert!(toml("font.normal.family = \"MesloLGS NF\"\n").icons);
        assert!(!toml("[font.normal]\n# family = \"Hack Nerd Font\"\nfamily = \"Menlo\"\n").icons);
        let yaml = World::new(&[("ALACRITTY_WINDOW_ID", "1")])
            .file(
                "/h/.config/alacritty/alacritty.yml",
                "font:\n  normal:\n    family: Menlo\n  bold:\n    family: Hack Nerd Font\n",
            )
            .detect();
        assert!(!yaml.icons && yaml.reason.contains("Menlo"), "{}", yaml.reason);

        let vscode = |text: &'static str| {
            World::new(&[("TERM_PROGRAM", "vscode"), ("XDG_CONFIG_HOME", "/cfg")])
                .file("/cfg/Code/User/settings.json", text)
                .detect()
        };
        let commented = vscode(
            "{\n  // \"terminal.integrated.fontFamily\": \"Hack Nerd Font\",\n  \"terminal.integrated.fontFamily\": \"Menlo\"\n}",
        );
        assert!(!commented.icons && commented.reason.contains("Menlo"), "{}", commented.reason);
        let block =
            vscode("{ /* \"terminal.integrated.fontFamily\": \"Hack NF\" */ \"editor.fontFamily\": \"Menlo\" }");
        assert!(!block.icons, "{}", block.reason);
        let url = vscode("{ \"x\": \"http://a\", \"terminal.integrated.fontFamily\": \"Hack Nerd Font\" }");
        assert!(url.icons, "`//` inside a string is not a comment: {}", url.reason);
    }

    #[cfg(unix)]
    #[test]
    fn a_slow_tmux_is_stopped() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let slow = dir.path().join("tmux");
        std::fs::write(&slow, "#!/bin/sh\nsleep 5\necho kitty\n").unwrap();
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(tmux_client_with(slow.to_str().unwrap(), std::time::Duration::from_millis(100)), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
        let quick = dir.path().join("quick");
        std::fs::write(&quick, "#!/bin/sh\nprintf 'xterm-kitty\\tx'\n").unwrap();
        std::fs::set_permissions(&quick, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(tmux_client_with(quick.to_str().unwrap(), TMUX_TIMEOUT).as_deref(), Some("xterm-kitty"));
    }

    #[test]
    fn unknown_terminals_get_no_icons() {
        let unknown = World::new(&[("TERM", "xterm-256color"), ("TERM_PROGRAM", "Apple_Terminal")]).detect();
        assert!(!unknown.icons);
        assert!(
            unknown.reason.contains("Apple_Terminal") && unknown.reason.contains("NERD_FONT=1"),
            "{}",
            unknown.reason
        );
    }

    #[test]
    fn nerd_font_names() {
        for yes in [
            "JetBrainsMono Nerd Font",
            "MesloLGS NF",
            "CaskaydiaCove NFM",
            "JetBrainsMonoNF-Regular",
            "Hack NFP",
            "FiraCodeNerdFontMono-Retina",
            "'0xProto NF', monospace",
        ] {
            assert!(looks_like_nerd_font(yes), "{yes}");
        }
        for no in ["Fira Code", "DejaVu Sans Mono", "CONF Sans", "Inconsolata", "Menlo"] {
            assert!(!looks_like_nerd_font(no), "{no}");
        }
    }

    #[test]
    fn kitty_versions() {
        assert_eq!(kitty_version("kitty(0.45.0)"), Some((0, 45)));
        assert_eq!(kitty_version("kitty 0.36.1"), Some((0, 36)));
        assert_eq!(kitty_version("WezTerm 2024"), None);
    }

    #[test]
    fn choices() {
        assert!(IconChoice::On.resolve(|| panic!("an explicit choice does not detect")).icons);
        assert!(!IconChoice::Off.resolve(|| panic!("an explicit choice does not detect")).icons);
        assert!(IconChoice::Auto.resolve(|| Detection::yes("x")).icons);
        assert_eq!(IconChoice::from_env(Some("nerd")), Some(IconChoice::On));
        assert_eq!(IconChoice::from_env(Some(" OFF ")), Some(IconChoice::Off));
        assert_eq!(IconChoice::from_env(Some("sparkles")), None);
    }

    #[test]
    fn glyphs_are_one_cell_wide() {
        use unicode_width::UnicodeWidthStr;
        for glyph in [glyph::FOLDER, glyph::BRANCH, glyph::ROBOT, glyph::CODE, glyph::WRENCH, glyph::USER, glyph::LOGO]
        {
            assert_eq!(glyph.width(), 1, "{glyph:?}");
        }
    }
}
