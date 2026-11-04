use crate::time::parse_ts;
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

// `last_contact_at` only moves forward. Two-argument MAX is NULL if either side is, hence COALESCE.
const UPSERT_QUOTE: &str = "
INSERT INTO quotes (id, customer_name, customer_phone, tech_name, amount, status, created_at, last_contact_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
ON CONFLICT(id) DO UPDATE SET
  customer_name = excluded.customer_name, customer_phone = excluded.customer_phone,
  tech_name = excluded.tech_name, amount = excluded.amount, status = excluded.status,
  created_at = excluded.created_at,
  last_contact_at = COALESCE(MAX(quotes.last_contact_at, excluded.last_contact_at), quotes.last_contact_at, excluded.last_contact_at)";

const INSERT_EVENT: &str =
    "INSERT OR IGNORE INTO events (event_id, type, quote_id, ts, raw) VALUES (?1, ?2, ?3, ?4, ?5)";

const PROMOTE: &str = "
UPDATE quotes SET status = 'accepted'
WHERE status = 'open'
  AND id IN (SELECT quote_id FROM events WHERE type = 'quote_accepted')";

const CANCEL: &str = "
UPDATE messages SET status = 'cancelled', error = 'quote not open', updated_at = ?1
WHERE status IN ('draft','approved','sending','failed')
  AND quote_id IN (SELECT id FROM quotes WHERE status <> 'open')";

#[derive(Debug, Default)]
pub struct Counts {
    pub quotes_stored: usize,
    pub quotes_skipped: usize,
    pub events_stored: usize,
    pub events_duplicate: usize,
    pub events_skipped: usize,
    pub promoted: usize,
    pub cancelled: usize,
}

#[derive(Deserialize)]
struct RawQuote {
    id: String,
    customer_name: String,
    customer_phone: String,
    tech_name: String,
    amount: f64,
    status: String,
    created_at: String,
    last_contact_at: Option<String>,
}

#[derive(Deserialize)]
struct RawEvent {
    event_id: String,
    #[serde(rename = "type")]
    event_type: String,
    quote_id: String,
    timestamp: String,
}

/// A quote with its parsed `created_at` and `last_contact_at`.
type Quote = (RawQuote, i64, Option<i64>);
/// An event with its parsed timestamp and raw line.
type Event = (RawEvent, i64, String);

pub fn ingest(conn: &mut Connection, path: &Path, now: i64) -> Result<Counts, Box<dyn Error>> {
    let (quotes_path, events_path) = classify(path)?;

    let mut counts = Counts::default();
    let mut quotes = Vec::new();
    let mut events = Vec::new();
    if let Some(path) = quotes_path {
        (quotes, counts.quotes_skipped) = read_quotes(&path)?;
    }
    if let Some(path) = events_path {
        (events, counts.events_skipped) = read_events(&path)?;
    }

    let tx = conn.transaction()?;
    for (quote, created_at, last_contact_at) in &quotes {
        tx.execute(
            UPSERT_QUOTE,
            params![
                quote.id,
                quote.customer_name,
                quote.customer_phone,
                quote.tech_name,
                quote.amount,
                quote.status,
                created_at,
                last_contact_at
            ],
        )?;
        counts.quotes_stored += 1;
    }
    for (event, ts, raw) in &events {
        let inserted = tx.execute(
            INSERT_EVENT,
            params![event.event_id, event.event_type, event.quote_id, ts, raw],
        )?;
        if inserted == 1 {
            counts.events_stored += 1;
        } else {
            counts.events_duplicate += 1;
        }
    }
    counts.promoted = tx.execute(PROMOTE, [])?;
    counts.cancelled = tx.execute(CANCEL, [now])?;
    tx.commit()?;
    Ok(counts)
}

pub fn stale_days(conn: &Connection, now: i64) -> rusqlite::Result<Option<i64>> {
    let newest: Option<i64> = conn.query_row("SELECT MAX(ts) FROM events", [], |row| row.get(0))?;
    if let Some(newest) = newest
        && now - newest > 30 * 86400
    {
        return Ok(Some((now - newest) / 86400));
    }
    Ok(None)
}

fn classify(path: &Path) -> Result<(Option<PathBuf>, Option<PathBuf>), String> {
    if !path.exists() {
        return Err(format!("no such path: {}", path.display()));
    }
    if path.is_dir() {
        let quotes = path.join("quotes.json");
        let events = path.join("events.jsonl");
        if !quotes.is_file() || !events.is_file() {
            return Err(format!("{} must contain quotes.json and events.jsonl", path.display()));
        }
        return Ok((Some(quotes), Some(events)));
    }
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("json") => Ok((Some(path.to_path_buf()), None)),
        Some("jsonl") => Ok((None, Some(path.to_path_buf()))),
        _ => Err("expected a directory, a .json quotes file or a .jsonl events file".to_string()),
    }
}

fn read_quotes(path: &Path) -> Result<(Vec<Quote>, usize), Box<dyn Error>> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let elements: Vec<Value> =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;

    let mut quotes = Vec::new();
    let mut skipped = 0;
    for (index, element) in elements.into_iter().enumerate() {
        match parse_quote(element) {
            Ok(quote) => quotes.push(quote),
            Err(reason) => {
                eprintln!("skip {}[{index}]: {reason}", path.display());
                skipped += 1;
            }
        }
    }
    Ok((quotes, skipped))
}

fn parse_quote(element: Value) -> Result<Quote, String> {
    let mut quote: RawQuote = from_object(element)?;
    quote.id = quote.id.trim().to_string();
    quote.customer_phone = quote.customer_phone.trim().to_string();
    if quote.id.is_empty() {
        return Err("empty id".to_string());
    }
    if quote.customer_phone.is_empty() {
        return Err("empty customer_phone".to_string());
    }
    if !["open", "accepted", "dismissed"].contains(&quote.status.as_str()) {
        return Err(format!("bad status: {}", quote.status));
    }
    let created_at = parse_ts(&quote.created_at).ok_or("bad created_at")?;
    let last_contact_at = quote
        .last_contact_at
        .as_deref()
        .map(|ts| parse_ts(ts).ok_or("bad last_contact_at"))
        .transpose()?;
    Ok((quote, created_at, last_contact_at))
}

fn read_events(path: &Path) -> Result<(Vec<Event>, usize), Box<dyn Error>> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;

    let mut events = Vec::new();
    let mut skipped = 0;
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        match parse_event(line) {
            Ok(Some(event)) => events.push(event),
            Ok(None) => {}
            Err(reason) => {
                eprintln!("skip {}:{}: {reason}", path.display(), index + 1);
                skipped += 1;
            }
        }
    }
    Ok((events, skipped))
}

/// `Ok(None)` is a blank line.
fn parse_event(line: &[u8]) -> Result<Option<Event>, String> {
    let line = std::str::from_utf8(line).map_err(|_| "invalid UTF-8")?.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(line).map_err(|e| format!("invalid JSON: {e}"))?;
    let mut event: RawEvent = from_object(value)?;
    for (name, field) in [
        ("event_id", &mut event.event_id),
        ("type", &mut event.event_type),
        ("quote_id", &mut event.quote_id),
    ] {
        *field = field.trim().to_string();
        if field.is_empty() {
            return Err(format!("empty {name}"));
        }
    }
    let ts = parse_ts(&event.timestamp).ok_or("bad timestamp")?;
    Ok(Some((event, ts, line.to_string())))
}

fn from_object<T: DeserializeOwned>(value: Value) -> Result<T, String> {
    // Derived structs also deserialize from positional arrays.
    if !value.is_object() {
        return Err("not an object".to_string());
    }
    serde_json::from_value(value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    const NOW: i64 = 1786971600;

    fn memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        db::migrate(&conn).unwrap();
        conn
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("robby-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn dump(conn: &Connection, sql: &str) -> Vec<String> {
        let mut stmt = conn.prepare(sql).unwrap();
        let columns = stmt.column_count();
        stmt.query_map([], |row| {
            let values: Vec<String> = (0..columns)
                .map(|i| format!("{:?}", row.get_ref(i).unwrap()))
                .collect();
            Ok(values.join("|"))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    fn dump_quotes(conn: &Connection) -> Vec<String> {
        dump(conn, "SELECT * FROM quotes ORDER BY id")
    }

    fn dump_events(conn: &Connection) -> Vec<String> {
        dump(conn, "SELECT * FROM events ORDER BY event_id")
    }

    fn status(conn: &Connection, table: &str, id: &str) -> String {
        conn.query_row(&format!("SELECT status FROM {table} WHERE id = ?1"), [id], |row| row.get(0))
            .unwrap()
    }

    fn assert_counts(
        counts: &Counts,
        quotes: (usize, usize),
        events: (usize, usize, usize),
        changes: (usize, usize),
    ) {
        assert_eq!((counts.quotes_stored, counts.quotes_skipped), quotes);
        assert_eq!(
            (counts.events_stored, counts.events_duplicate, counts.events_skipped),
            events
        );
        assert_eq!((counts.promoted, counts.cancelled), changes);
    }

    fn quote_json(id: &str, status: &str) -> String {
        format!(
            r#"{{"id":"{id}","customer_name":"Ann","customer_phone":"+1{id}","tech_name":"Brad",
                "amount":100,"status":"{status}","created_at":"2026-08-01T00:00:00Z"}}"#
        )
    }

    fn event_line(event_id: &str, event_type: &str, quote_id: &str) -> String {
        format!(
            r#"{{"event_id":"{event_id}","type":"{event_type}","quote_id":"{quote_id}","timestamp":"2026-08-02T00:00:00Z"}}"#
        )
    }

    #[test]
    fn seed_ingest_is_idempotent_in_any_order() {
        let seed = Path::new(env!("CARGO_MANIFEST_DIR")).join("seed");
        let mut conn = memory_db();
        let counts = ingest(&mut conn, &seed, NOW).unwrap();
        assert_counts(&counts, (30, 0), (82, 6, 0), (0, 0));
        let quotes = dump_quotes(&conn);
        let events = dump_events(&conn);

        let text = fs::read_to_string(seed.join("events.jsonl")).unwrap();
        let mut reversed: Vec<&str> = text.lines().collect();
        reversed.reverse();
        let mut front = Vec::new();
        let mut back = Vec::new();
        for (i, line) in reversed.into_iter().enumerate() {
            if i % 3 == 0 { front.push(line) } else { back.push(line) }
        }
        front.append(&mut back);

        let shuffled = temp_dir("shuffled");
        fs::copy(seed.join("quotes.json"), shuffled.join("quotes.json")).unwrap();
        fs::write(shuffled.join("events.jsonl"), front.join("\n")).unwrap();

        let counts = ingest(&mut conn, &shuffled, NOW).unwrap();
        assert_counts(&counts, (30, 0), (0, 88, 0), (0, 0));
        assert_eq!(dump_quotes(&conn), quotes);
        assert_eq!(dump_events(&conn), events);

        // Only a fresh database can detect order dependence.
        let mut fresh = memory_db();
        let counts = ingest(&mut fresh, &shuffled, NOW).unwrap();
        assert_counts(&counts, (30, 0), (82, 6, 0), (0, 0));
        assert_eq!(dump_quotes(&fresh), quotes);
        assert_eq!(dump_events(&fresh), events);
    }

    #[test]
    fn quote_accepted_promotes_open_quote() {
        let dir = temp_dir("promotion");
        let quotes = dir.join("quotes.json");
        let events = dir.join("events.jsonl");
        fs::write(&quotes, format!("[{}]", quote_json("Q-1", "open"))).unwrap();
        fs::write(&events, event_line("e1", "quote_accepted", "Q-1")).unwrap();

        let mut conn = memory_db();
        let counts = ingest(&mut conn, &dir, NOW).unwrap();
        assert_counts(&counts, (1, 0), (1, 0, 0), (1, 0));
        assert_eq!(status(&conn, "quotes", "Q-1"), "accepted");

        let counts = ingest(&mut conn, &dir, NOW).unwrap();
        assert_counts(&counts, (1, 0), (0, 1, 0), (1, 0));
        assert_eq!(status(&conn, "quotes", "Q-1"), "accepted");

        let mut events_first = memory_db();
        ingest(&mut events_first, &events, NOW).unwrap();
        ingest(&mut events_first, &quotes, NOW).unwrap();
        assert_eq!(status(&events_first, "quotes", "Q-1"), "accepted");

        fs::write(&quotes, format!("[{}]", quote_json("Q-1", "dismissed"))).unwrap();
        let mut dismissed = memory_db();
        let counts = ingest(&mut dismissed, &dir, NOW).unwrap();
        assert_eq!(counts.promoted, 0);
        assert_eq!(status(&dismissed, "quotes", "Q-1"), "dismissed");
    }

    #[test]
    fn malformed_input_is_skipped_and_counted() {
        let dir = temp_dir("malformed");
        let unknown_type = event_line("e1", "mystery", "Q-1");
        let unknown_quote = event_line("e2", "quote_viewed", "Q-404");
        let mut bytes = Vec::new();
        for line in [
            "not json".to_string(),
            r#"{"event_id":"e3","type":"quote_viewed","quote_id":"Q-1"}"#.to_string(),
            r#"{"event_id":"e4","type":"quote_viewed","quote_id":"Q-1","timestamp":"yesterday"}"#
                .to_string(),
            event_line("  ", "quote_viewed", "Q-1"),
            "   ".to_string(),
            unknown_type.clone(),
            unknown_quote,
        ] {
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(
            b"{\"event_id\":\"e5\",\"type\":\"quote_viewed\",\"quote_id\":\"Q-1\",\
              \"timestamp\":\"2026-08-02T00:00:00Z\",\"note\":\"\xff\"}\n",
        );
        fs::write(dir.join("events.jsonl"), bytes).unwrap();

        let bad_date = quote_json("Q-3", "open").replace("2026-08-01T00:00:00Z", "soon");
        let no_phone = r#"{"id":"Q-2","customer_name":"Ann","tech_name":"Brad","amount":1,
                           "status":"open","created_at":"2026-08-01T00:00:00Z"}"#;
        let quotes = format!(
            "[{no_phone},{},{bad_date},{}]",
            quote_json("Q-4", "OPEN"),
            quote_json("Q-1", "open")
        );
        fs::write(dir.join("quotes.json"), quotes).unwrap();

        let mut conn = memory_db();
        let counts = ingest(&mut conn, &dir, NOW).unwrap();
        assert_counts(&counts, (1, 3), (2, 0, 5), (0, 0));
        assert_eq!(status(&conn, "quotes", "Q-1"), "open");
        let raw: String = conn
            .query_row("SELECT raw FROM events WHERE type = 'mystery'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(raw, unknown_type);
        let stored: i64 = conn
            .query_row("SELECT COUNT(*) FROM events WHERE quote_id = 'Q-404'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored, 1);
    }

    #[test]
    fn positional_arrays_are_not_objects() {
        let dir = temp_dir("arrays");
        fs::write(
            dir.join("quotes.json"),
            r#"[["Q-1","Ann","+1555","Brad",100,"open","2026-08-01T00:00:00Z",null]]"#,
        )
        .unwrap();
        fs::write(
            dir.join("events.jsonl"),
            r#"["e1","quote_viewed","Q-1","2026-08-02T00:00:00Z"]"#,
        )
        .unwrap();
        let counts = ingest(&mut memory_db(), &dir, NOW).unwrap();
        assert_counts(&counts, (0, 1), (0, 0, 1), (0, 0));
    }

    #[test]
    fn closed_quotes_cancel_pending_messages() {
        let mut conn = memory_db();
        for (id, quote_id, message_status) in [
            (1, "Q-1", "draft"),
            (2, "Q-2", "approved"),
            (3, "Q-3", "sending"),
            (4, "Q-4", "failed"),
            (5, "Q-5", "sent"),
            (6, "Q-6", "draft"),
        ] {
            conn.execute(
                "INSERT INTO messages (id, quote_id, customer_phone, reason, body, status,
                                       policy_now, idempotency_key, created_at, updated_at)
                 VALUES (?1, ?2, ?2, 'test', 'hi', ?3, 0, ?2, 0, 0)",
                params![id, quote_id, message_status],
            )
            .unwrap();
        }

        let dir = temp_dir("cancellation");
        let quotes = dir.join("quotes.json");
        let elements = [
            quote_json("Q-1", "accepted"),
            quote_json("Q-2", "dismissed"),
            quote_json("Q-3", "accepted"),
            quote_json("Q-4", "dismissed"),
            quote_json("Q-5", "accepted"),
            quote_json("Q-6", "open"),
        ];
        fs::write(&quotes, format!("[{}]", elements.join(","))).unwrap();

        let counts = ingest(&mut conn, &quotes, NOW).unwrap();
        assert_counts(&counts, (6, 0), (0, 0, 0), (0, 4));
        for id in ["1", "2", "3", "4"] {
            let row: (String, String, i64) = conn
                .query_row(
                    "SELECT status, error, updated_at FROM messages WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(row, ("cancelled".to_string(), "quote not open".to_string(), NOW));
        }
        assert_eq!(status(&conn, "messages", "5"), "sent");
        assert_eq!(status(&conn, "messages", "6"), "draft");

        let before = dump_quotes(&conn);
        let events = dir.join("events.jsonl");
        fs::write(&events, event_line("e1", "quote_viewed", "Q-6")).unwrap();
        let counts = ingest(&mut conn, &events, NOW).unwrap();
        assert_counts(&counts, (0, 0), (1, 0, 0), (0, 0));
        assert_eq!(dump_quotes(&conn), before);

        assert!(ingest(&mut conn, &dir.join("missing"), NOW).is_err());
        let text_file = dir.join("notes.txt");
        fs::write(&text_file, "[]").unwrap();
        assert!(ingest(&mut conn, &text_file, NOW).is_err());
        fs::write(&quotes, "not json").unwrap();
        assert!(ingest(&mut conn, &quotes, NOW).is_err());
        assert_eq!(dump_quotes(&conn), before);
    }

    #[test]
    fn accepted_event_alone_cancels_pending_message() {
        let dir = temp_dir("accepted");
        let quotes = dir.join("quotes.json");
        let events = dir.join("events.jsonl");
        fs::write(&quotes, format!("[{}]", quote_json("Q-1", "open"))).unwrap();
        fs::write(&events, event_line("e1", "quote_accepted", "Q-1")).unwrap();

        let mut conn = memory_db();
        ingest(&mut conn, &quotes, NOW).unwrap();
        conn.execute(
            "INSERT INTO messages (id, quote_id, customer_phone, reason, body, status,
                                   policy_now, idempotency_key, created_at, updated_at)
             VALUES (1, 'Q-1', '+1Q-1', 'test', 'hi', 'approved', 0, 'k', 0, 0)",
            [],
        )
        .unwrap();

        let counts = ingest(&mut conn, &events, NOW).unwrap();
        assert_counts(&counts, (0, 0), (1, 0, 0), (1, 1));
        assert_eq!(status(&conn, "quotes", "Q-1"), "accepted");
        assert_eq!(status(&conn, "messages", "1"), "cancelled");
    }

    #[test]
    fn upsert_updates_every_field() {
        let dir = temp_dir("upsert");
        let quotes = dir.join("quotes.json");
        fs::write(&quotes, format!("[{}]", quote_json("Q-1", "open"))).unwrap();
        let mut conn = memory_db();
        ingest(&mut conn, &quotes, NOW).unwrap();

        let changed = r#"[{"id":"Q-1","customer_name":"Bo","customer_phone":"+2","tech_name":"Cy",
                           "amount":7.5,"status":"dismissed","created_at":"2026-08-03T00:00:00Z",
                           "last_contact_at":"2026-08-04T00:00:00Z"}]"#;
        fs::write(&quotes, changed).unwrap();
        ingest(&mut conn, &quotes, NOW).unwrap();

        let row: (String, String, String, f64, String, i64, i64) = conn
            .query_row(
                "SELECT customer_name, customer_phone, tech_name, amount, status, created_at,
                        last_contact_at
                 FROM quotes WHERE id = 'Q-1'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        let expected = (
            "Bo".to_string(),
            "+2".to_string(),
            "Cy".to_string(),
            7.5,
            "dismissed".to_string(),
            parse_ts("2026-08-03T00:00:00Z").unwrap(),
            parse_ts("2026-08-04T00:00:00Z").unwrap(),
        );
        assert_eq!(row, expected);
    }

    #[test]
    fn reexport_never_moves_last_contact_back() {
        let dir = temp_dir("last-contact");
        let quotes = dir.join("quotes.json");
        let mut conn = memory_db();
        let mut reingest = |last_contact: &str| -> Option<i64> {
            let field = format!(r#""last_contact_at":{last_contact},"status""#);
            let quote = quote_json("Q-1", "open").replace(r#""status""#, &field);
            fs::write(&quotes, format!("[{quote}]")).unwrap();
            ingest(&mut conn, &quotes, NOW).unwrap();
            conn.query_row("SELECT last_contact_at FROM quotes WHERE id = 'Q-1'", [], |row| row.get(0))
                .unwrap()
        };
        let stored = parse_ts("2026-08-05T00:00:00Z");
        let newer = parse_ts("2026-08-06T00:00:00Z");

        assert_eq!(reingest("null"), None);
        assert_eq!(reingest(r#""2026-08-05T00:00:00Z""#), stored);
        assert_eq!(reingest("null"), stored);
        assert_eq!(reingest(r#""2026-08-04T00:00:00Z""#), stored);
        assert_eq!(reingest(r#""2026-08-06T00:00:00Z""#), newer);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_quote_guards() {
        let parse = |json: &str| parse_quote(serde_json::from_str(json).unwrap());
        let good = quote_json("Q-1", "open");

        let padded = good.replace(r#""Q-1""#, r#""  Q-1 ""#).replace("+1Q-1", "  +1555 ");
        let (quote, _, _) = parse(&padded).unwrap();
        assert_eq!(quote.id, "Q-1");
        assert_eq!(quote.customer_phone, "+1555");

        for (bad, reason) in [
            (good.replace(r#""Q-1""#, r#""  ""#), "empty id"),
            (good.replace("+1Q-1", "  "), "empty customer_phone"),
            (good.replace(r#""status""#, r#""last_contact_at":"never","status""#), "bad last_contact_at"),
        ] {
            assert_eq!(parse(&bad).err().as_deref(), Some(reason));
        }
    }

    #[test]
    fn padded_event_fields_are_trimmed() {
        let dir = temp_dir("padded");
        let accepted = event_line("e1", " quote_accepted ", " Q-1 ");
        let copy = event_line(" e1 ", "quote_viewed", "Q-1");
        fs::write(dir.join("quotes.json"), format!("[{}]", quote_json("Q-1", "open"))).unwrap();
        fs::write(dir.join("events.jsonl"), format!("{accepted}\n{copy}\n")).unwrap();

        let mut conn = memory_db();
        let counts = ingest(&mut conn, &dir, NOW).unwrap();
        assert_counts(&counts, (1, 0), (1, 1, 0), (1, 0));
        assert_eq!(status(&conn, "quotes", "Q-1"), "accepted");
        let raw: String = conn
            .query_row("SELECT raw FROM events WHERE event_id = 'e1'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(raw, accepted);
    }

    #[test]
    fn failed_ingest_rolls_back() {
        let dir = temp_dir("rollback");
        let quotes = dir.join("quotes.json");
        let events = dir.join("events.jsonl");
        fs::write(&quotes, format!("[{}]", quote_json("Q-1", "open"))).unwrap();
        fs::write(&events, event_line("e1", "quote_accepted", "Q-1")).unwrap();
        let mut conn = memory_db();
        ingest(&mut conn, &dir, NOW).unwrap();
        let stored_quotes = dump_quotes(&conn);
        let stored_events = dump_events(&conn);

        let elements = [quote_json("Q-1", "open"), quote_json("Q-2", "open")];
        fs::write(&quotes, format!("[{}]", elements.join(","))).unwrap();
        fs::write(&events, event_line("e2", "quote_viewed", "Q-2")).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER no_events BEFORE INSERT ON events
             BEGIN SELECT RAISE(ABORT, 'no events'); END",
        )
        .unwrap();

        assert!(ingest(&mut conn, &dir, NOW).is_err());
        assert_eq!(status(&conn, "quotes", "Q-1"), "accepted");
        assert_eq!(dump_quotes(&conn), stored_quotes);
        assert_eq!(dump_events(&conn), stored_events);
    }

    #[test]
    fn stale_days_boundaries() {
        let mut conn = memory_db();
        assert_eq!(stale_days(&conn, NOW).unwrap(), None);

        let seed = Path::new(env!("CARGO_MANIFEST_DIR")).join("seed");
        ingest(&mut conn, &seed, NOW).unwrap();
        assert_eq!(stale_days(&conn, NOW).unwrap(), None);

        let newest = parse_ts("2026-08-16T16:00:00Z").unwrap();
        assert_eq!(stale_days(&conn, newest + 30 * 86400).unwrap(), None);
        assert_eq!(stale_days(&conn, newest + 31 * 86400).unwrap(), Some(31));
    }
}
