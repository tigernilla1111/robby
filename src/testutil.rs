//! Fixtures the unit tests share.

use crate::db;
use crate::policy::{self, DAY, Policy};
use rusqlite::{Connection, params};
use std::fs;
use std::path::{Path, PathBuf};

pub const NOW: i64 = 1786971600;
pub const PHONE: &str = "+15550000";

/// The seed at `NOW`: the 25 candidates in rank order, then the 5 suppressed by id.
pub const GOLDEN: [&str; 30] = [
    "Q-1016: unanswered_reply, xl_quote_stale, stale",
    "Q-1007: unanswered_reply, stale",
    "Q-1015: unanswered_reply, never_contacted, stale",
    "Q-1021: xl_quote_stale, never_viewed, stale",
    "Q-1026: xl_quote_stale, viewed_no_response",
    "Q-1012: xl_quote_stale, repeat_views, stale",
    "Q-1027: xl_quote_stale, stale",
    "Q-1023: xl_quote_stale, viewed_no_response",
    "Q-1020: xl_quote_stale, never_viewed",
    "Q-1010: xl_quote_stale, never_contacted, stale",
    "Q-1022: xl_quote_stale, repeat_views, stale",
    "Q-1002: xl_quote_stale, never_viewed",
    "Q-1008: repeat_views, stale",
    "Q-1001: repeat_views, never_contacted, stale",
    "Q-1000: repeat_views, viewed_no_response, never_contacted, stale",
    "Q-1028: repeat_views, never_contacted, stale",
    "Q-1024: never_contacted, stale",
    "Q-1025: never_contacted, stale",
    "Q-1018: never_contacted, stale",
    "Q-1014: never_viewed, stale",
    "Q-1005: never_viewed, stale",
    "Q-1013: never_viewed, stale",
    "Q-1006: stale",
    "Q-1011: stale",
    "Q-1029: stale",
    "Q-1003: same phone as Q-1025",
    "Q-1004: not open",
    "Q-1009: not open",
    "Q-1017: not open",
    "Q-1019: ladder wait",
];

pub fn memory_db() -> Connection {
    db::open(Path::new(":memory:")).unwrap()
}

/// Deleted on drop, so a test that panics cleans up too.
pub struct TempDir(PathBuf);

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn temp_dir(name: &str) -> TempDir {
    let dir = std::env::temp_dir().join(format!("robby-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}

pub fn seed() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("seed")
}

/// An open quote that was never contacted.
pub fn add_quote(conn: &Connection, id: &str, phone: &str) {
    add_quote_with(conn, id, phone, "open", None);
}

pub fn add_quote_with(
    conn: &Connection,
    id: &str,
    phone: &str,
    status: &str,
    last_contact: Option<i64>,
) {
    conn.execute(
        "INSERT INTO quotes (id, customer_name, customer_phone, tech_name, amount, status,
                             created_at, last_contact_at)
         VALUES (?1, 'Ann', ?2, 'Brad', 1000, ?3, ?4, ?5)",
        params![id, phone, status, NOW - 10 * DAY, last_contact],
    )
    .unwrap();
}

pub fn add_event(conn: &Connection, event_type: &str, quote_id: &str, ts: i64) {
    add_raw_event(conn, event_type, quote_id, ts, "{}");
}

pub fn add_raw_event(conn: &Connection, event_type: &str, quote_id: &str, ts: i64, raw: &str) {
    conn.execute(
        "INSERT INTO events (event_id, type, quote_id, ts, raw)
         VALUES (hex(randomblob(8)), ?1, ?2, ?3, ?4)",
        params![event_type, quote_id, ts, raw],
    )
    .unwrap();
}

/// Every listed quote as `id: label`, candidates first.
pub fn labels(conn: &Connection, now: i64) -> Vec<String> {
    let (ranked, suppressed) = policy::candidates(conn, &Policy::default(), now).unwrap();
    let ranked =
        ranked.iter().map(|(quote, rules)| format!("{}: {}", quote.id, policy::names(rules)));
    let suppressed = suppressed.iter().map(|(quote, label)| format!("{}: {label}", quote.id));
    ranked.chain(suppressed).collect()
}
