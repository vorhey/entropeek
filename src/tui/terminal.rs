//! Asking the terminal about itself, in a single round trip at startup.
//!
//! Sent together, in raw mode, before anything else reads input:
//! - `OSC 11 ; ?` — the background color (for the light/dark theme);
//! - `CSI > q` (XTVERSION) — the terminal's name and version, e.g.
//!   `kitty(0.45.0)`; works over SSH;
//! - `DCS + q kitty-query-font_family` (XTGETTCAP) — kitty's font;
//! - `CSI c` (device attributes) — answered by every terminal, after the
//!   others, so the wait ends as soon as it arrives.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
/// Anything else read meanwhile is what the user typed; it is returned as
/// key events, so no keystroke is lost.
#[cfg(unix)]
pub fn query(timeout: std::time::Duration) -> (Reply, Vec<KeyEvent>) {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    let Ok(mut tty) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") else {
        return Default::default();
    };
    if tty.write_all(QUERY).and_then(|()| tty.flush()).is_err() {
        return Default::default();
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
        if device_attributes_done(&reply) || reply.len() > MAX_REPLY {
            break;
        }
    }
    let (answers, typed) = split_input(&reply);
    (parse(&answers), keys(&typed))
}

#[cfg(not(unix))]
pub fn query(_timeout: std::time::Duration) -> (Reply, Vec<KeyEvent>) {
    Default::default()
}

/// Terminals answer in well under this; more is not an answer.
const MAX_REPLY: usize = 64 * 1024;

/// Separates the terminal's answers (OSC and DCS strings, and the device
/// attributes report `ESC [ ? … c`) from everything else, which the user
/// typed. An unfinished answer at the end counts as an answer.
pub fn split_input(bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (mut answers, mut typed) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        let len = if rest.starts_with(b"\x1b]") || rest.starts_with(b"\x1bP") {
            // Up to BEL or ST (ESC \\).
            let body = &rest[2..];
            let end = (0..body.len()).find(|&j| body[j] == 0x07 || body[j..].starts_with(b"\x1b\\"));
            match end {
                Some(j) if body[j] == 0x07 => 2 + j + 1,
                Some(j) => 2 + j + 2,
                None => rest.len(),
            }
        } else if rest.starts_with(b"\x1b[?") {
            match rest[3..].iter().position(|b| !(b.is_ascii_digit() || *b == b';')) {
                Some(j) if rest[3 + j] == b'c' => 3 + j + 1,
                Some(_) => 0,
                None => rest.len(),
            }
        } else {
            0
        };
        if len > 0 {
            answers.extend_from_slice(&rest[..len]);
            i += len;
        } else {
            typed.push(bytes[i]);
            i += 1;
        }
    }
    (answers, typed)
}

/// Decodes keys typed in raw mode: text, Enter, Tab, Backspace, Escape,
/// Ctrl+letter, Alt+key, arrows and the usual navigation keys. Other
/// escape sequences are skipped.
pub fn keys(bytes: &[u8]) -> Vec<KeyEvent> {
    let mut out = Vec::new();
    let mut i = 0;
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            let rest = &bytes[i + 1..];
            match rest.first() {
                None => {
                    out.push(key(KeyCode::Esc));
                    i += 1;
                }
                Some(b'[') | Some(b'O') => {
                    // CSI / SS3: parameters, then one final byte.
                    let body = &rest[1..];
                    let end = body.iter().position(|b| (0x40..=0x7e).contains(b)).unwrap_or(body.len());
                    let (params, last) = (&body[..end], body.get(end).copied());
                    let code = match (params, last) {
                        (_, Some(b'A')) => Some(KeyCode::Up),
                        (_, Some(b'B')) => Some(KeyCode::Down),
                        (_, Some(b'C')) => Some(KeyCode::Right),
                        (_, Some(b'D')) => Some(KeyCode::Left),
                        (_, Some(b'H')) => Some(KeyCode::Home),
                        (_, Some(b'F')) => Some(KeyCode::End),
                        (b"5", Some(b'~')) => Some(KeyCode::PageUp),
                        (b"6", Some(b'~')) => Some(KeyCode::PageDown),
                        (b"3", Some(b'~')) => Some(KeyCode::Delete),
                        _ => None,
                    };
                    out.extend(code.map(key));
                    i += 2 + end + usize::from(last.is_some());
                }
                Some(&next) if (0x20..0x7f).contains(&next) => {
                    out.push(KeyEvent::new(KeyCode::Char(next as char), KeyModifiers::ALT));
                    i += 2;
                }
                Some(_) => {
                    out.push(key(KeyCode::Esc));
                    i += 1;
                }
            }
            continue;
        }
        match b {
            b'\r' | b'\n' => out.push(key(KeyCode::Enter)),
            b'\t' => out.push(key(KeyCode::Tab)),
            0x7f | 0x08 => out.push(key(KeyCode::Backspace)),
            0x01..=0x1a => out.push(KeyEvent::new(KeyCode::Char((b'a' + b - 1) as char), KeyModifiers::CONTROL)),
            0x20..=0x7e => out.push(key(KeyCode::Char(b as char))),
            0x80.. => {
                // One UTF-8 character.
                let len = match b {
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf7 => 4,
                    _ => 1,
                };
                let end = (i + len).min(bytes.len());
                if let Ok(text) = std::str::from_utf8(&bytes[i..end]) {
                    out.extend(text.chars().map(|c| key(KeyCode::Char(c))));
                }
                i = end;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
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
/// Works on the raw bytes: anything that is not an even number of ASCII
/// hex digits (or is implausibly long) is no answer, never a panic.
fn parse_kitty_font(bytes: &[u8]) -> Option<String> {
    let needle = b"\x1bP1+r6b697474792d71756572792d666f6e745f66616d696c79=";
    let start = bytes.windows(needle.len()).position(|w| w == needle)? + needle.len();
    let rest = &bytes[start..];
    let end = rest.iter().position(|b| matches!(b, 0x1b | 0x07 | b';')).unwrap_or(rest.len());
    let hex = &rest[..end];
    if !hex.len().is_multiple_of(2) || hex.len() > 512 {
        return None;
    }
    let digit = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let decoded: Option<Vec<u8>> =
        hex.as_chunks::<2>().0.iter().map(|[hi, lo]| Some(digit(*hi)? << 4 | digit(*lo)?)).collect();
    let font = String::from_utf8(decoded?).ok()?;
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
    fn malformed_font_answers_are_ignored() {
        let prefix = format!("\x1bP1+r{}=", hex("kitty-query-font_family"));
        for payload in ["€", "\u{fffd}a", "4a6", "zz", "4a€6"] {
            let reply = format!("{prefix}{payload}\x1b\\");
            assert_eq!(parse(reply.as_bytes()).font, None, "{payload:?}");
        }
        let mut invalid_utf8 = prefix.into_bytes();
        invalid_utf8.extend_from_slice(&[0xff, 0xfe, 0x1b, b'\\']);
        assert_eq!(parse(&invalid_utf8).font, None);
        assert_eq!(parse(&invalid_utf8[..invalid_utf8.len() - 2]).font, None, "unterminated");
    }

    #[test]
    fn keys_typed_during_the_query_are_kept() {
        let answer = "\x1b]11;rgb:0000/0000/0000\x1b\\\x1bP>|tmux 3.6\x1b\\\x1b[?62;22c";
        // Keys arrive before, between and after the answers.
        let (osc, rest) = answer.split_at(answer.find("\x1bP").unwrap());
        let mixed = format!("q{osc}j\x1b[B{rest}é\r");
        let (answers, typed) = split_input(mixed.as_bytes());
        assert_eq!(parse(&answers).version.as_deref(), Some("tmux 3.6"));
        assert_eq!(String::from_utf8(answers).unwrap(), answer);
        let codes: Vec<KeyCode> = keys(&typed).into_iter().map(|k| k.code).collect();
        assert_eq!(
            codes,
            vec![KeyCode::Char('q'), KeyCode::Char('j'), KeyCode::Down, KeyCode::Char('é'), KeyCode::Enter]
        );
    }

    #[test]
    fn typed_keys_decode() {
        let k = keys(b"\x04\x1bx\x1b[5~\x7f\t\x1b");
        assert_eq!(k[0], KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(k[1], KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
        let codes: Vec<KeyCode> = k[2..].iter().map(|k| k.code).collect();
        assert_eq!(codes, vec![KeyCode::PageUp, KeyCode::Backspace, KeyCode::Tab, KeyCode::Esc]);
        assert!(keys(b"\x1b[200~").is_empty(), "unknown sequences are skipped");
        let (answers, typed) = split_input(b"\x1b]11;rgb:0/0/0");
        assert_eq!((answers.len(), typed.len()), (14, 0), "an unfinished answer is not typed text");
    }

    #[test]
    fn device_attributes_end_the_wait() {
        assert!(device_attributes_done(b"\x1b]11;rgb:0/0/0\x07\x1b[?62;22c"));
        assert!(!device_attributes_done(b"\x1b]11;rgb:0/0/0\x07\x1b[?62;2"));
        assert!(!device_attributes_done(b""));
    }
}
