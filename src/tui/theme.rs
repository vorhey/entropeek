//! Light and dark color themes, and picking one to match the terminal.
//!
//! Like Claude Code, entropeek asks the terminal for its background color
//! (the `OSC 11` query, see [`super::terminal`]) when it starts. If the terminal does not answer, the
//! `COLORFGBG` variable some terminals set is used, and otherwise dark.
//! `--theme` / `ENTROPEEK_THEME` override the detection, and `t` switches
//! themes while running.

use ratatui::style::{Color, Modifier, Style};

use crate::model::AgentKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Dark,
    Light,
}

/// What the user asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ThemeChoice {
    /// Match the terminal's background.
    #[default]
    Auto,
    Dark,
    Light,
}

impl ThemeChoice {
    /// Reads `ENTROPEEK_THEME` (`auto`, `dark` or `light`).
    pub fn from_env(value: Option<&str>) -> Option<ThemeChoice> {
        match value?.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(ThemeChoice::Auto),
            "dark" => Some(ThemeChoice::Dark),
            "light" => Some(ThemeChoice::Light),
            _ => None,
        }
    }
}

/// The colors the UI is drawn with: showagent's palette (GitHub's light
/// and dark colors), so the two tools look alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub mode: Mode,
    /// entropeek's name and logo: dark green.
    pub brand: Color,
    /// Title, key caps, cursor, box titles, the search prompt.
    pub accent: Color,
    /// The selected row (with white bold text).
    pub selection: Color,
    /// Neutral chip background, also the selected row while the preview
    /// has focus.
    pub chip: Color,
    /// Column header row.
    pub header_fg: Color,
    pub header_bg: Color,
    /// Workspace group headers and key group titles.
    pub group: Color,
    /// Main text: titles, messages, key descriptions.
    pub text: Color,
    /// The last part of a workspace path.
    pub strong: Color,
    /// Secondary text: dates, hints, counts.
    pub muted: Color,
    /// Labels in the preview and live values in the keys box.
    pub value: Color,
    /// The preview box border.
    pub detail: Color,
    /// The keys box border and separators.
    pub edge: Color,
    /// The row about to be deleted (white bold text).
    pub danger: Color,
    pub warn: Color,
    pub error: Color,
    /// Draw Nerd Font icons (see [`super::icons`]).
    pub icons: bool,
}

const fn hex(rgb: u32) -> Color {
    Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

impl Theme {
    pub fn dark() -> Theme {
        Theme {
            mode: Mode::Dark,
            brand: hex(0x238636),
            accent: hex(0x7DCAFF),
            selection: hex(0x1F6FEB),
            chip: hex(0x21262D),
            header_fg: hex(0xC9D1D9),
            header_bg: hex(0x30363D),
            group: hex(0xD2A8FF),
            text: hex(0xC9D1D9),
            strong: hex(0xE6EDF3),
            muted: hex(0x8B949E),
            value: hex(0xFFA657),
            detail: hex(0x3FB950),
            edge: hex(0x3D444D),
            danger: hex(0x8E1519),
            warn: hex(0xFFA657),
            error: hex(0xF85149),
            icons: false,
        }
    }

    pub fn light() -> Theme {
        Theme {
            mode: Mode::Light,
            brand: hex(0x1A7F37),
            accent: hex(0x0969DA),
            selection: hex(0x0969DA),
            chip: hex(0xEAEEF2),
            header_fg: hex(0x1F2328),
            header_bg: hex(0xEAEEF2),
            group: hex(0x8250DF),
            text: hex(0x1F2328),
            strong: hex(0x1F2328),
            muted: hex(0x57606A),
            value: hex(0xBC4C00),
            detail: hex(0x1A7F37),
            edge: hex(0xD0D7DE),
            danger: hex(0xCF222E),
            warn: hex(0xBC4C00),
            error: hex(0xCF222E),
            icons: false,
        }
    }

    pub fn for_mode(mode: Mode) -> Theme {
        match mode {
            Mode::Dark => Theme::dark(),
            Mode::Light => Theme::light(),
        }
    }

    /// The other mode, keeping the icon setting.
    pub fn toggled(self) -> Theme {
        Theme::for_mode(match self.mode {
            Mode::Dark => Mode::Light,
            Mode::Light => Mode::Dark,
        })
        .with_icons(self.icons)
    }

    pub fn with_icons(self, icons: bool) -> Theme {
        Theme { icons, ..self }
    }

    /// `glyph` followed by a space with icons on, `fallback` otherwise.
    pub fn icon(&self, glyph: &str, fallback: &'static str) -> String {
        if self.icons { format!("{glyph} ") } else { fallback.to_string() }
    }

    pub fn dim(&self) -> Style {
        Style::new().fg(self.muted)
    }

    pub fn accent(&self) -> Style {
        Style::new().fg(self.accent)
    }

    pub fn text(&self) -> Style {
        Style::new().fg(self.text)
    }

    pub fn bold(&self, color: Color) -> Style {
        Style::new().fg(color).add_modifier(Modifier::BOLD)
    }

    /// The selected row: white bold text on the selection color, or a quiet
    /// chip color while the preview has the focus.
    pub fn selected(&self, focused: bool) -> Style {
        if focused {
            Style::new().fg(hex(0xFFFFFF)).bg(self.selection).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(self.text).bg(self.chip).add_modifier(Modifier::BOLD)
        }
    }

    pub fn header(&self) -> Style {
        Style::new().fg(self.header_fg).bg(self.header_bg).add_modifier(Modifier::BOLD)
    }

    pub fn danger(&self) -> Style {
        Style::new().fg(hex(0xFFFFFF)).bg(self.danger).add_modifier(Modifier::BOLD)
    }

    /// An agent's color: its badge background.
    pub fn agent(&self, agent: AgentKind) -> Color {
        self.badge_colors(agent).0
    }

    /// An agent's badge (showagent's provider colors).
    pub fn badge(&self, agent: AgentKind) -> Style {
        let (bg, fg) = self.badge_colors(agent);
        Style::new().fg(fg).bg(bg).add_modifier(Modifier::BOLD)
    }

    fn badge_colors(&self, agent: AgentKind) -> (Color, Color) {
        let white = hex(0xFFFFFF);
        match (agent, self.mode) {
            (AgentKind::Codex, Mode::Dark) => (hex(0x1F6FEB), white),
            (AgentKind::Codex, Mode::Light) => (hex(0x0969DA), white),
            (AgentKind::Claude, Mode::Dark) => (hex(0xD2A8FF), hex(0x0D1117)),
            (AgentKind::Claude, Mode::Light) => (hex(0x8250DF), white),
        }
    }
}

impl Default for Theme {
    fn default() -> Theme {
        Theme::dark()
    }
}

/// Picks the mode: an explicit choice wins, then the terminal's answer to
/// the background query, then `COLORFGBG`, then dark.
pub fn resolve(choice: ThemeChoice, query: impl FnOnce() -> Option<Rgb>, colorfgbg: Option<&str>) -> Mode {
    match choice {
        ThemeChoice::Dark => Mode::Dark,
        ThemeChoice::Light => Mode::Light,
        ThemeChoice::Auto => {
            query().map(Rgb::mode).or_else(|| colorfgbg.and_then(mode_from_colorfgbg)).unwrap_or(Mode::Dark)
        }
    }
}

/// A color with channels from 0.0 to 1.0.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgb(pub f32, pub f32, pub f32);

impl Rgb {
    /// Light if the background's relative luminance is above the middle.
    pub fn mode(self) -> Mode {
        let linear = |c: f32| if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
        let luminance = 0.2126 * linear(self.0) + 0.7152 * linear(self.1) + 0.0722 * linear(self.2);
        // 0.18 is perceptual mid-gray (L* = 50).
        if luminance > 0.18 { Mode::Light } else { Mode::Dark }
    }
}

/// Parses a terminal's answer to `OSC 11 ; ?`, e.g.
/// `ESC ] 11 ; rgb:ffff/ffff/ffff BEL`. Channels have 1 to 4 hex digits.
/// Anything else in `bytes` (such as other replies) is ignored.
pub fn parse_osc11(bytes: &[u8]) -> Option<Rgb> {
    let text = String::from_utf8_lossy(bytes);
    let start = text.find("]11;")? + 4;
    let rest = &text[start..];
    let end = rest.find(['\u{7}', '\u{1b}']).unwrap_or(rest.len());
    let value = &rest[..end];
    let channels = value.strip_prefix("rgb:").or_else(|| value.strip_prefix("rgba:"))?;
    let mut parts = channels.split('/').map(|hex| {
        let digits = hex.len();
        if !(1..=4).contains(&digits) {
            return None;
        }
        let max = (1u32 << (4 * digits)) - 1;
        u32::from_str_radix(hex, 16).ok().map(|v| v as f32 / max as f32)
    });
    let (r, g, b) = (parts.next()??, parts.next()??, parts.next()??);
    Some(Rgb(r, g, b))
}

/// `COLORFGBG` is `fg;bg` (sometimes `fg;default;bg`) with palette indexes:
/// backgrounds 7 and 15 (white) are light, 0-6 and 8 dark.
pub fn mode_from_colorfgbg(value: &str) -> Option<Mode> {
    match value.rsplit(';').next()?.trim().parse::<u8>().ok()? {
        7 | 15 => Some(Mode::Light),
        0..=6 | 8 => Some(Mode::Dark),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_terminal_background_answers() {
        assert_eq!(parse_osc11(b"\x1b]11;rgb:ffff/ffff/ffff\x07"), Some(Rgb(1.0, 1.0, 1.0)));
        assert_eq!(parse_osc11(b"\x1b]11;rgb:0000/0000/0000\x1b\\"), Some(Rgb(0.0, 0.0, 0.0)));
        assert_eq!(parse_osc11(b"\x1b]11;rgb:ff/80/00\x07").map(|c| c.1), Some(128.0 / 255.0));
        assert_eq!(parse_osc11(b"\x1b]11;rgba:f/f/f/f\x07"), Some(Rgb(1.0, 1.0, 1.0)));
        // Mixed with the device attributes answer.
        assert!(parse_osc11(b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\\x1b[?62;22c").is_some());
        for bad in [&b""[..], b"\x1b[?62c", b"\x1b]11;rgb:zz/00/00\x07", b"\x1b]11;rgb:00000/0/0\x07", b"\x1b]11;?\x07"]
        {
            assert_eq!(parse_osc11(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn background_brightness_picks_the_mode() {
        assert_eq!(Rgb(1.0, 1.0, 1.0).mode(), Mode::Light);
        assert_eq!(Rgb(0.99, 0.96, 0.89).mode(), Mode::Light, "solarized light");
        assert_eq!(Rgb(0.0, 0.0, 0.0).mode(), Mode::Dark);
        assert_eq!(Rgb(0.12, 0.12, 0.18).mode(), Mode::Dark, "a typical dark theme");
        assert_eq!(Rgb(0.0, 0.17, 0.21).mode(), Mode::Dark, "solarized dark");
    }

    #[test]
    fn colorfgbg_fallback() {
        assert_eq!(mode_from_colorfgbg("15;0"), Some(Mode::Dark));
        assert_eq!(mode_from_colorfgbg("0;15"), Some(Mode::Light));
        assert_eq!(mode_from_colorfgbg("0;default;7"), Some(Mode::Light));
        assert_eq!(mode_from_colorfgbg("garbage"), None);
        assert_eq!(mode_from_colorfgbg("0;200"), None);
    }

    #[test]
    fn explicit_choice_beats_detection_which_beats_colorfgbg() {
        let white = || Some(Rgb(1.0, 1.0, 1.0));
        let silent = || None;
        assert_eq!(resolve(ThemeChoice::Dark, white, None), Mode::Dark);
        assert_eq!(resolve(ThemeChoice::Light, silent, Some("15;0")), Mode::Light);
        assert_eq!(resolve(ThemeChoice::Auto, white, Some("15;0")), Mode::Light);
        assert_eq!(resolve(ThemeChoice::Auto, silent, Some("0;15")), Mode::Light);
        assert_eq!(resolve(ThemeChoice::Auto, silent, None), Mode::Dark);
        // An explicit choice does not even ask the terminal.
        assert_eq!(resolve(ThemeChoice::Dark, || panic!("queried"), None), Mode::Dark);
    }

    #[test]
    fn env_choice() {
        assert_eq!(ThemeChoice::from_env(Some(" Light ")), Some(ThemeChoice::Light));
        assert_eq!(ThemeChoice::from_env(Some("auto")), Some(ThemeChoice::Auto));
        assert_eq!(ThemeChoice::from_env(Some("blue")), None);
        assert_eq!(ThemeChoice::from_env(None), None);
    }

    #[test]
    fn palettes_match_showagent() {
        let dark = Theme::dark();
        assert_eq!(dark.badge(AgentKind::Claude).bg, Some(Color::Rgb(0xD2, 0xA8, 0xFF)));
        assert_eq!(dark.badge(AgentKind::Codex).bg, Some(Color::Rgb(0x1F, 0x6F, 0xEB)));
        assert_eq!(dark.header(), Style::new().fg(hex(0xC9D1D9)).bg(hex(0x30363D)).add_modifier(Modifier::BOLD));
        let light = Theme::light();
        assert_eq!(light.badge(AgentKind::Claude).bg, Some(Color::Rgb(0x82, 0x50, 0xDF)));
        assert_eq!(light.badge(AgentKind::Codex).fg, Some(Color::Rgb(0xFF, 0xFF, 0xFF)));
        assert_eq!(light.accent, hex(0x0969DA));
    }

    #[test]
    fn themes_toggle() {
        assert_eq!(Theme::dark().toggled(), Theme::light());
        assert_eq!(Theme::light().toggled().mode, Mode::Dark);
    }
}
