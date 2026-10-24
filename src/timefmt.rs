//! Timestamp parsing and human-friendly formatting.

use jiff::{Timestamp, tz::TimeZone};

/// Parses an RFC 3339 timestamp (as written by Claude and Codex) into Unix milliseconds.
pub fn parse_rfc3339_ms(value: &str) -> Option<i64> {
    value.parse::<Timestamp>().ok().map(|ts| ts.as_millisecond())
}

/// Current time in Unix milliseconds.
pub fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

/// Compact age such as `now`, `5m`, `3h`, `2d`, `6w`, `4mo`, `2y`.
pub fn relative(then_ms: i64, now_ms: i64) -> String {
    let secs = (now_ms - then_ms).max(0) / 1000;
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    match secs {
        s if s < MIN => "now".to_string(),
        s if s < HOUR => format!("{}m", s / MIN),
        s if s < DAY => format!("{}h", s / HOUR),
        s if s < 14 * DAY => format!("{}d", s / DAY),
        s if s < 60 * DAY => format!("{}w", s / (7 * DAY)),
        s if s < 365 * DAY => format!("{}mo", s / (30 * DAY)),
        s => format!("{}y", s / (365 * DAY)),
    }
}

/// `just now` or `5m ago`, for sentences.
pub fn ago(then_ms: i64, now_ms: i64) -> String {
    match relative(then_ms, now_ms).as_str() {
        "now" => "just now".to_string(),
        age => format!("{age} ago"),
    }
}

/// How long ago, the way showagent's UPDATED column says it: `just now`,
/// `5m ago`, `3h ago`, `2d ago`, then the date (`Oct 02`).
pub fn updated(then_ms: i64, now_ms: i64) -> String {
    let secs = (now_ms - then_ms) / 1000;
    match secs {
        s if s < 0 => month_day(then_ms),
        s if s < 60 => "just now".to_string(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 7 * 86_400 => format!("{}d ago", s / 86_400),
        _ => month_day(then_ms),
    }
}

fn month_day(ms: i64) -> String {
    Timestamp::from_millisecond(ms)
        .map(|ts| ts.to_zoned(TimeZone::system()).strftime("%b %d").to_string())
        .unwrap_or_else(|_| "?".to_string())
}

/// Local date and time, e.g. `2026-10-24 13:38`.
pub fn local(ms: i64) -> String {
    format_in(ms, &TimeZone::system())
}

fn format_in(ms: i64, tz: &TimeZone) -> String {
    match Timestamp::from_millisecond(ms) {
        Ok(ts) => ts.to_zoned(tz.clone()).strftime("%Y-%m-%d %H:%M").to_string(),
        Err(_) => "?".to_string(),
    }
}

/// UTC RFC 3339, used in JSON output.
pub fn rfc3339(ms: i64) -> String {
    Timestamp::from_millisecond(ms).map(|ts| ts.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agent_timestamps() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(parse_rfc3339_ms("2026-10-24T16:39:45.993Z"), Some(1_792_859_985_993));
        assert_eq!(parse_rfc3339_ms("2026-10-24T16:39:48.159449511Z"), Some(1_792_859_988_159));
        assert_eq!(parse_rfc3339_ms("not a time"), None);
    }

    #[test]
    fn relative_buckets() {
        let now = 1_000_000_000_000;
        let s = 1000;
        assert_eq!(relative(now, now), "now");
        assert_eq!(relative(now - 59 * s, now), "now");
        assert_eq!(relative(now - 5 * 60 * s, now), "5m");
        assert_eq!(relative(now - 3 * 3600 * s, now), "3h");
        assert_eq!(relative(now - 2 * 86400 * s, now), "2d");
        assert_eq!(relative(now - 21 * 86400 * s, now), "3w");
        assert_eq!(relative(now - 120 * 86400 * s, now), "4mo");
        assert_eq!(relative(now - 800 * 86400 * s, now), "2y");
        assert_eq!(ago(now, now), "just now");
        assert_eq!(ago(now - 3 * 3600 * s, now), "3h ago");
        // Clock skew never produces negative ages.
        assert_eq!(relative(now + 10 * s, now), "now");
    }

    #[test]
    fn formats_in_a_fixed_zone() {
        assert_eq!(format_in(1_792_859_985_993, &TimeZone::UTC), "2026-10-24 16:39");
        assert_eq!(rfc3339(1000), "1970-01-01T00:00:01Z");
    }

    #[test]
    fn updated_reads_like_showagent() {
        let now = 1_792_859_985_993;
        assert_eq!(updated(now - 10_000, now), "just now");
        assert_eq!(updated(now - 5 * 60_000, now), "5m ago");
        assert_eq!(updated(now - 3 * 3_600_000, now), "3h ago");
        assert_eq!(updated(now - 2 * 86_400_000, now), "2d ago");
        let old = updated(now - 30 * 86_400_000, now);
        assert_eq!(old.len(), 6, "a date like Sep 24: {old}");
        assert!(updated(now + 86_400_000, now).len() == 6, "future times show the date");
    }
}
