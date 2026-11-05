use chrono::{DateTime, NaiveDateTime, Utc};

/// RFC 3339, so fractions and offsets from webhooks parse, or the short form without seconds.
pub fn parse_ts(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s).map(|parsed| parsed.timestamp()).ok().or_else(|| {
        let parsed = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%MZ").ok()?;
        Some(parsed.and_utc().timestamp())
    })
}

pub fn format_ts(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .expect("timestamp in chrono's range")
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

pub fn format_date(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .expect("timestamp in chrono's range")
        .format("%b %-d")
        .to_string()
}

pub fn wall_clock() -> i64 {
    Utc::now().timestamp()
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
    fn parses_fractions_and_offsets() {
        for same in [
            "2026-08-17 13:00:00Z",
            "2026-08-17T13:00:00.000Z",
            "2026-08-17T13:00:00.123456Z",
            "2026-08-17T13:00:00+00:00",
            "2026-08-17T09:00:00-04:00",
        ] {
            assert_eq!(parse_ts(same), Some(1786971600), "{same}");
        }
    }

    #[test]
    fn rejects_bad_values() {
        for bad in [
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

    #[test]
    fn formats_short_dates() {
        assert_eq!(format_date(parse_ts("2026-08-06T03:00:00Z").unwrap()), "Aug 6");
        assert_eq!(format_date(parse_ts("2026-08-17T13:00:00Z").unwrap()), "Aug 17");
    }
}
