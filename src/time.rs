use chrono::{DateTime, NaiveDateTime};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn parse_ts(s: &str) -> Option<i64> {
    ["%Y-%m-%dT%H:%M:%SZ", "%Y-%m-%dT%H:%MZ"]
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(s, format).ok())
        .map(|parsed| parsed.and_utc().timestamp())
}

pub fn format_ts(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .expect("timestamp in chrono's range")
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

pub fn wall_clock() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after 1970")
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_shapes() {
        assert_eq!(parse_ts("2026-08-17T13:00:00Z"), Some(1786971600));
        assert_eq!(parse_ts("2026-08-17T13:00Z"), Some(1786971600));
        assert_eq!(parse_ts("2000-03-01T00:00:00Z"), Some(951868800));
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn rejects_bad_values() {
        for bad in [
            "2026-08-17 13:00:00Z",
            "2026-08-17T13:00:00",
            "2026-13-01T00:00:00Z",
            "2026-02-31T00:00:00Z",
            "2026-08-17T13:00:0é",
            "",
        ] {
            assert_eq!(parse_ts(bad), None, "{bad}");
        }
    }

    #[test]
    fn format_round_trips() {
        let s = "2026-08-17T13:00:00Z";
        assert_eq!(format_ts(parse_ts(s).unwrap()), s);
    }
}
