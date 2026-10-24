//! Fast line iteration over JSONL buffers.
//!
//! Session files can be many megabytes, and most lines are irrelevant for the
//! session list. Parsers split lines with SIMD `memchr` and test cheap byte
//! markers (see [`Marker`]) before paying for a JSON parse.

use memchr::memmem::Finder;
use serde::Deserialize;
use std::borrow::Cow;

/// Non-empty lines of `bytes`, front to back. A trailing `\r` is removed.
pub fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = bytes;
    std::iter::from_fn(move || {
        while !rest.is_empty() {
            let (line, next) = match memchr::memchr(b'\n', rest) {
                Some(i) => (&rest[..i], &rest[i + 1..]),
                None => (rest, &rest[rest.len()..]),
            };
            rest = next;
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if !line.iter().all(u8::is_ascii_whitespace) {
                return Some(line);
            }
        }
        None
    })
}

/// Non-empty lines of `bytes`, back to front.
pub fn lines_rev(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = bytes;
    std::iter::from_fn(move || {
        while !rest.is_empty() {
            let trimmed = rest.strip_suffix(b"\n").unwrap_or(rest);
            let (next, line) = match memchr::memrchr(b'\n', trimmed) {
                Some(i) => (&trimmed[..=i], &trimmed[i + 1..]),
                None => (&trimmed[..0], trimmed),
            };
            rest = next;
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if !line.iter().all(u8::is_ascii_whitespace) {
                return Some(line);
            }
        }
        None
    })
}

/// A precompiled check used to skip lines before parsing them. It only
/// filters: a line that passes is still parsed and checked by its type.
///
/// It looks for the JSON string token `"value"`, which every valid way of
/// writing `"type": "value"` contains, whatever the spacing around `:`.
/// A line with `\u` escapes always passes, since the value might be
/// spelled with escapes. So the filter can never drop a record it needs.
pub struct Marker {
    token: Finder<'static>,
    escape: Finder<'static>,
}

impl Marker {
    pub fn token(value: &str) -> Marker {
        Marker {
            token: Finder::new(format!("\"{value}\"").as_bytes()).into_owned(),
            escape: Finder::new(b"\\u").into_owned(),
        }
    }

    pub fn in_line(&self, line: &[u8]) -> bool {
        self.token.find(line).is_some() || self.escape.find(line).is_some()
    }
}

#[derive(Deserialize)]
struct TimestampOnly<'a> {
    #[serde(borrow, default)]
    timestamp: Option<Cow<'a, str>>,
}

/// The newest top-level `"timestamp"` in a JSONL buffer, in Unix milliseconds.
/// Scans from the end, so it usually touches only the last few lines.
pub fn last_timestamp(bytes: &[u8]) -> Option<i64> {
    let marker = Marker::token("timestamp");
    lines_rev(bytes).filter(|line| marker.in_line(line)).find_map(|line| {
        let parsed: TimestampOnly = serde_json::from_slice(line).ok()?;
        crate::timefmt::parse_rfc3339_ms(parsed.timestamp?.as_ref())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect<'a>(it: impl Iterator<Item = &'a [u8]>) -> Vec<&'a str> {
        it.map(|l| std::str::from_utf8(l).unwrap()).collect()
    }

    #[test]
    fn lines_skip_blanks_and_crlf() {
        let data = b"a\n\nb\r\n  \nc";
        assert_eq!(collect(lines(data)), vec!["a", "b", "c"]);
        assert_eq!(collect(lines(b"")), Vec::<&str>::new());
        assert_eq!(collect(lines(b"\n\n")), Vec::<&str>::new());
        assert_eq!(collect(lines(b"x\n")), vec!["x"]);
    }

    #[test]
    fn lines_rev_is_reverse_of_lines() {
        for data in [&b"a\n\nb\r\n  \nc"[..], b"x\n", b"one\ntwo\nthree\n\n", b"", b"\n"] {
            let mut forward = collect(lines(data));
            forward.reverse();
            assert_eq!(collect(lines_rev(data)), forward, "{:?}", std::str::from_utf8(data));
        }
    }

    #[test]
    fn marker_accepts_any_valid_spacing() {
        let m = Marker::token("user");
        for line in [
            &br#"{"type":"user"}"#[..],
            br#"{"type": "user"}"#,
            br#"{"type" : "user"}"#,
            b"{\"type\":\t \"user\"}",
            br#"{ "type"  :   "user" }"#,
            br#"{"type":"\u0075ser"}"#,
        ] {
            assert!(m.in_line(line), "{}", String::from_utf8_lossy(line));
        }
        assert!(!m.in_line(br#"{"type": "users"}"#));
        assert!(!m.in_line(br#"{"type":"assistant","userType":"x"}"#));
        // JSON quoted inside a string value is escaped, so it does not match.
        assert!(!m.in_line(br#"{"text":"{\"type\":\"user\"}"}"#));
    }

    #[test]
    fn last_timestamp_reads_from_the_end() {
        let data = br#"{"timestamp":"1970-01-01T00:00:01Z"}
{"timestamp":"1970-01-01T00:00:05Z","x":{"timestamp":"bogus"}}
{"type":"last-prompt"}
not json with "timestamp"
"#;
        assert_eq!(last_timestamp(data), Some(5000));
        assert_eq!(last_timestamp(b"{\"a\":1}\n"), None);
    }
}
