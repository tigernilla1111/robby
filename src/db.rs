use rusqlite::Connection;
use std::path::Path;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS quotes (
  id              TEXT PRIMARY KEY,
  customer_name   TEXT NOT NULL,
  customer_phone  TEXT NOT NULL,
  tech_name       TEXT NOT NULL,
  amount          REAL NOT NULL,
  status          TEXT NOT NULL CHECK (status IN ('open','accepted','dismissed')),
  created_at      INTEGER NOT NULL,
  last_contact_at INTEGER
);

CREATE TABLE IF NOT EXISTS events (
  event_id TEXT PRIMARY KEY,
  type     TEXT NOT NULL,
  quote_id TEXT NOT NULL,
  ts       INTEGER NOT NULL,
  raw      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_quote_ts ON events (quote_id, ts);

CREATE TABLE IF NOT EXISTS messages (
  id              INTEGER PRIMARY KEY,
  quote_id        TEXT NOT NULL,
  customer_phone  TEXT NOT NULL,
  channel         TEXT NOT NULL DEFAULT 'sms',
  reason          TEXT NOT NULL,
  body            TEXT NOT NULL,
  must_edit       INTEGER NOT NULL DEFAULT 0,
  status          TEXT NOT NULL CHECK (status IN
                    ('draft','approved','sending','sent','failed','rejected','cancelled')),
  policy_now      INTEGER NOT NULL,
  idempotency_key TEXT NOT NULL UNIQUE,
  provider_id     TEXT,
  error           TEXT,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL,
  sent_at         INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS messages_one_pending_per_phone
  ON messages (customer_phone)
  WHERE status IN ('draft','approved','sending','failed');

CREATE TABLE IF NOT EXISTS provider_outbox (
  id              INTEGER PRIMARY KEY,
  idempotency_key TEXT NOT NULL UNIQUE,
  provider_id     TEXT NOT NULL,
  to_phone        TEXT NOT NULL,
  body            TEXT NOT NULL,
  sent_at         INTEGER NOT NULL
);
";

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)
}

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    migrate(&conn)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_draft(conn: &Connection, key: &str) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT INTO messages (quote_id, customer_phone, reason, body, status, policy_now,
                                   idempotency_key, created_at, updated_at)
             VALUES ('Q-1', '+15550000', 'test', 'hi', 'draft', 0, ?1, 0, 0)",
            [key],
        )
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
    }

    #[test]
    fn one_pending_message_per_phone() {
        for (status, blocks) in [
            ("draft", true),
            ("approved", true),
            ("sending", true),
            ("failed", true),
            ("sent", false),
            ("rejected", false),
            ("cancelled", false),
        ] {
            let conn = Connection::open_in_memory().unwrap();
            migrate(&conn).unwrap();
            insert_draft(&conn, "a").unwrap();
            conn.execute("UPDATE messages SET status = ?1", [status]).unwrap();
            assert_eq!(insert_draft(&conn, "b").is_err(), blocks, "{status}");
        }
    }
}
