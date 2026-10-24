//! Asking the terminal about itself, in a single round trip at startup.
//!
//! Sent together, in raw mode, before anything else reads input:
//! - `OSC 11 ; ?` — the background color (for the light/dark theme);
//! - `CSI > q` (XTVERSION) — the terminal's name and version, e.g.
//!   `kitty(0.45.0)`; works over SSH;
//! - `DCS + q kitty-query-font_family` (XTGETTCAP) — kitty's font;
//! - `CSI c` (device attributes) — answered by every terminal, after the
//!   others, so the wait ends as soon as it arrives.

use super::theme::{Rgb, parse_osc11};

/// What the terminal answered. Every field is optional: terminals answer
/// what they support and ignore the rest.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reply {
    pub background: Option<Rgb>,
    /// XTVERSION, e.g. `kitty(0.45.0)`, `WezTerm 20240203`, `tmux 3.6`.
    pub version: Option<String>,
    /// kitty's current font (PostScript name).
    pub font: Option<String>,
}

const QUERY: &[u8] = b"\x1b]11;?\x1b\\\x1b[>q\x1bP+q6b697474792d71756572792d666f6e745f66616d696c79\x1b\\\x1b[c";

/// Sends the queries and collects the answers, waiting at most `timeout`.
#[cfg(unix)]
pub fn query(timeout: std::time::Duration) -> Reply {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    let Ok(mut tty) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") else {
        return Reply::default();
    };
    if tty.write_all(QUERY).and_then(|()| tty.flush()).is_err() {
        return Reply::default();
    }
    let deadline = Instant::now() + timeout;
    let mut reply = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut fds = libc::pollfd { fd: tty.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd, and a bounded timeout.
        let ready = unsafe { libc::poll(&mut fds, 1, left.as_millis().min(1000) as libc::c_int) };
        if ready <= 0 {
            break;
        }
        match tty.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => reply.extend_from_slice(&buf[..n]),
        }
        if device_attributes_done(&reply) {
            break;
        }
    }
    parse(&reply)
}

#[cfg(not(unix))]
pub fn query(_timeout: std::time::Duration) -> Reply {
    Reply::default()
}

/// Reads every answer out of the bytes the terminal sent back.
pub fn parse(bytes: &[u8]) -> Reply {
    Reply { background: parse_osc11(bytes), version: parse_xtversion(bytes), font: parse_kitty_font(bytes) }
}

/// `DCS > | text ST`.
fn parse_xtversion(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let start = text.find("\u{1b}P>|")? + 4;
    let rest = &text[start..];
    let end = rest.find(['\u{1b}', '\u{7}']).unwrap_or(rest.len());
    let version = rest[..end].trim();
    (!version.is_empty()).then(|| version.to_string())
}

/// `DCS 1 + r <hex name> = <hex value> ST` for `kitty-query-font_family`.
fn parse_kitty_font(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let needle = "\u{1b}P1+r6b697474792d71756572792d666f6e745f66616d696c79=";
    let start = text.find(needle)? + needle.len();
    let rest = &text[start..];
    let end = rest.find(['\u{1b}', '\u{7}', ';']).unwrap_or(rest.len());
    let hex = &rest[..end];
    let bytes: Option<Vec<u8>> =
        (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()).collect();
    let font = String::from_utf8(bytes?).ok()?;
    (!font.trim().is_empty()).then(|| font.trim().to_string())
}

/// Whether `reply` contains the end of a device attributes answer
/// (`ESC [ ? ... c`), which comes after every other answer.
fn device_attributes_done(reply: &[u8]) -> bool {
    let Some(start) = reply.windows(3).rposition(|w| w == b"\x1b[?") else { return false };
    reply[start + 3..].contains(&b'c')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> String {
        text.bytes().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn the_query_asks_for_the_kitty_font() {
        let text = String::from_utf8_lossy(QUERY);
        assert!(text.contains(&hex("kitty-query-font_family")));
    }

    #[test]
    fn parses_a_kitty_reply() {
        let reply = format!(
            "\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1bP>|kitty(0.45.0)\x1b\\\x1bP1+r{}={}\x1b\\\x1b[?62;c",
            hex("kitty-query-font_family"),
            hex("JetBrainsMonoNF-Regular")
        );
        let parsed = parse(reply.as_bytes());
        assert_eq!(parsed.background, Some(Rgb(1.0, 1.0, 1.0)));
        assert_eq!(parsed.version.as_deref(), Some("kitty(0.45.0)"));
        assert_eq!(parsed.font.as_deref(), Some("JetBrainsMonoNF-Regular"));
        assert!(device_attributes_done(reply.as_bytes()));
    }

    #[test]
    fn terminals_that_answer_less_still_parse() {
        let parsed = parse(b"\x1bP>|WezTerm 20240203-110809-5046fc22\x1b\\\x1bP0+r\x1b\\\x1b[?65;4c");
        assert_eq!(parsed.version.as_deref(), Some("WezTerm 20240203-110809-5046fc22"));
        assert_eq!(parsed.font, None, "an unsupported XTGETTCAP query");
        assert_eq!(parsed.background, None);
        assert_eq!(parse(b"\x1b[?1;2c"), Reply::default());
        assert_eq!(parse(b""), Reply::default());
        assert_eq!(parse(b"\x1bP>|\x1b\\").version, None);
    }

    #[test]
    fn device_attributes_end_the_wait() {
        assert!(device_attributes_done(b"\x1b]11;rgb:0/0/0\x07\x1b[?62;22c"));
        assert!(!device_attributes_done(b"\x1b]11;rgb:0/0/0\x07\x1b[?62;2"));
        assert!(!device_attributes_done(b""));
    }
}
