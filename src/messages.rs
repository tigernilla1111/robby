use crate::policy::{Facts, Policy, load_facts};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;
use std::error::Error;

pub struct Message {
    pub id: i64,
    pub quote_id: String,
    pub phone: String,
    pub reason: String,
    pub body: String,
    pub must_edit: bool,
    pub key: String,
}

pub trait Sender {
    /// Deliver one message and return the provider's id. `key` must be idempotent there.
    fn send(&self, msg: &Message) -> Result<String, String>;
}

/// Stands in for the provider: a key it has seen stores nothing and returns the first id.
pub struct FakeSender<'a> {
    pub conn: &'a Connection,
    pub now: i64,
    /// Quote whose ack is lost: the text is stored, then the call errors.
    pub fail: Option<String>,
}

impl Sender for FakeSender<'_> {
    fn send(&self, msg: &Message) -> Result<String, String> {
        self.conn
            .execute(
                "INSERT INTO provider_outbox (idempotency_key, provider_id, to_phone, body, sent_at)
                 VALUES (?1, 'sm_' || lower(hex(randomblob(4))), ?2, ?3, ?4)
                 ON CONFLICT DO NOTHING",
                params![msg.key, msg.phone, msg.body, self.now],
            )
            .map_err(|e| e.to_string())?;
        let provider_id = self
            .conn
            .query_row(
                "SELECT provider_id FROM provider_outbox WHERE idempotency_key = ?1",
                [&msg.key],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if self.fail.as_ref() == Some(&msg.quote_id) {
            return Err("ack lost after the provider stored it".to_string());
        }
        Ok(provider_id)
    }
}

fn select(conn: &Connection, filter: &str) -> rusqlite::Result<Vec<Message>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, quote_id, customer_phone, reason, body, must_edit, idempotency_key
         FROM messages WHERE {filter} ORDER BY id"
    ))?;
    stmt.query_map([], |row| {
        Ok(Message {
            id: row.get(0)?,
            quote_id: row.get(1)?,
            phone: row.get(2)?,
            reason: row.get(3)?,
            body: row.get(4)?,
            must_edit: row.get(5)?,
            key: row.get(6)?,
        })
    })?
    .collect()
}

/// Why a guarded update changed no row.
fn refusal(conn: &Connection, id: i64, wrong_status: &str) -> Result<(), Box<dyn Error>> {
    let status: Option<String> = conn
        .query_row("SELECT status FROM messages WHERE id = ?1", [id], |row| row.get(0))
        .optional()?;
    let reason = match status.as_deref() {
        None => format!("no message #{id}"),
        Some("draft" | "approved") => format!("message #{id} must be edited: pass a new --body"),
        Some(status) => format!("message #{id} is {status}, {wrong_status}"),
    };
    Err(reason.into())
}

/// A must-edit row needs a `body` unlike the stored text, so its placeholder never goes out.
pub fn approve(
    conn: &Connection,
    id: i64,
    body: Option<&str>,
    now: i64,
) -> Result<(), Box<dyn Error>> {
    let body = body.map(str::trim);
    if body == Some("") {
        return Err("empty body".into());
    }
    let changed = conn.execute(
        "UPDATE messages SET status = 'approved', body = COALESCE(?2, body), updated_at = ?3
         WHERE id = ?1 AND status IN ('draft','approved')
           AND (must_edit = 0 OR (?2 IS NOT NULL AND ?2 <> body))",
        params![id, body, now],
    )?;
    if changed == 0 {
        return refusal(conn, id, "not draft");
    }
    Ok(())
}

/// Approves every plain draft. Returns the approved rows and the must-edit rows it skipped.
pub fn approve_all(conn: &Connection, now: i64) -> rusqlite::Result<(Vec<Message>, Vec<Message>)> {
    let (need_edit, approved) =
        select(conn, "status = 'draft'")?.into_iter().partition(|msg| msg.must_edit);
    conn.execute(
        "UPDATE messages SET status = 'approved', updated_at = ?1
         WHERE status = 'draft' AND must_edit = 0",
        [now],
    )?;
    Ok((approved, need_edit))
}

/// `updated_at` starts the quote's rejected window.
pub fn reject(conn: &Connection, id: i64, now: i64) -> Result<(), Box<dyn Error>> {
    let changed = conn.execute(
        "UPDATE messages SET status = 'rejected', updated_at = ?2
         WHERE id = ?1 AND status IN ('draft','approved')",
        params![id, now],
    )?;
    if changed == 0 {
        return refusal(conn, id, "cannot reject");
    }
    Ok(())
}

/// Why an approved message must not go out now, if anything. Not `decide`: the row being sent
/// is itself pending, so `decide` would suppress every send.
fn blocked(
    policy: &Policy,
    facts: &BTreeMap<String, Facts>,
    msg: &Message,
    now: i64,
) -> Option<&'static str> {
    let Some(quote) = facts.get(&msg.quote_id).filter(|quote| quote.open) else {
        return Some("quote not open");
    };
    if quote.phone != msg.phone {
        return Some("phone changed");
    }
    // The customer is the phone, so a reply on an open sibling quote counts. Compared with our
    // last send, not the draft's age: a reply can be stamped before the draft and ingested after.
    let replied = facts
        .values()
        .filter(|q| q.open && q.phone == msg.phone)
        .any(|q| q.replies.last() > q.sends.last());
    // Once a send answers the reply, a must-edit row is outreach like any other.
    if policy.in_cooldown(quote.phone_last_sent, now) && !(msg.must_edit && replied) {
        return Some("cooldown");
    }
    if !msg.must_edit && replied {
        return Some("customer replied");
    }
    None
}

#[derive(Debug, PartialEq)]
pub enum Delivery {
    Sent(String),
    Failed(String),
    Cancelled(&'static str),
}

/// Sends every approved or failed row that still passes the recheck, in id order.
///
/// No transaction, by design: `sending` must be durable before the provider is called. A crash
/// during the call then leaves a visible `sending` row, never retried, instead of rolling back
/// to `approved` and delivering twice.
pub fn send(
    conn: &Connection,
    policy: &Policy,
    sender: &dyn Sender,
    now: i64,
) -> rusqlite::Result<Vec<(Message, Delivery)>> {
    let mut results = Vec::new();
    for msg in select(conn, "status IN ('approved','failed')")? {
        // Per row, so a reply or a closure ingested during the run reaches the next row.
        let facts = load_facts(conn, now)?;
        if let Some(reason) = blocked(policy, &facts, &msg, now) {
            let cancelled = conn.execute(
                "UPDATE messages SET status = 'cancelled', error = ?2, updated_at = ?3
                 WHERE id = ?1 AND status IN ('approved','failed')",
                params![msg.id, reason, now],
            )?;
            if cancelled == 1 {
                results.push((msg, Delivery::Cancelled(reason)));
            }
            continue;
        }

        // The stamp records the attempt before the provider can deliver it.
        let claimed = conn.execute(
            "UPDATE messages SET status = 'sending', sent_at = ?2, updated_at = ?2
             WHERE id = ?1 AND status IN ('approved','failed')",
            params![msg.id, now],
        )?;
        if claimed == 0 {
            // Another run or ingest got there first, and reports its own result.
            continue;
        }

        // No row marked means ingest cancelled it mid-send, so the report matches the row.
        let delivery = match sender.send(&msg) {
            Ok(provider_id) => {
                let marked = conn.execute(
                    "UPDATE messages SET status = 'sent', provider_id = ?2, error = NULL,
                                         updated_at = ?3
                     WHERE id = ?1 AND status = 'sending'",
                    params![msg.id, provider_id, now],
                )?;
                if marked == 0 {
                    // The text still went out, so its id is kept.
                    conn.execute(
                        "UPDATE messages SET provider_id = ?2 WHERE id = ?1",
                        params![msg.id, provider_id],
                    )?;
                    Delivery::Cancelled("delivered, then cancelled")
                } else {
                    Delivery::Sent(provider_id)
                }
            }
            Err(error) => {
                let marked = conn.execute(
                    "UPDATE messages SET status = 'failed', error = ?2, updated_at = ?3
                     WHERE id = ?1 AND status = 'sending'",
                    params![msg.id, error, now],
                )?;
                if marked == 0 {
                    Delivery::Cancelled("cancelled during send")
                } else {
                    Delivery::Failed(error)
                }
            }
        };
        results.push((msg, delivery));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::draft::draft;
    use crate::ingest::ingest;
    use crate::policy::{self, DAY};
    use crate::time;
    use std::path::Path;

    const NOW: i64 = 1786971600;
    const HOUR: i64 = 3600;
    const PHONE: &str = "+15550000";

    #[derive(Debug, PartialEq)]
    struct Row {
        status: String,
        provider_id: Option<String>,
        error: Option<String>,
        sent_at: Option<i64>,
        must_edit: bool,
        body: String,
        updated_at: i64,
    }

    fn memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        db::migrate(&conn).unwrap();
        conn
    }

    fn add_quote(conn: &Connection, id: &str, phone: &str) {
        conn.execute(
            "INSERT INTO quotes (id, customer_name, customer_phone, tech_name, amount, status,
                                 created_at)
             VALUES (?1, 'Ann', ?2, 'Brad', 1000, 'open', ?3)",
            params![id, phone, NOW - 10 * DAY],
        )
        .unwrap();
    }

    fn add_event(conn: &Connection, event_type: &str, quote_id: &str, ts: i64) {
        conn.execute(
            "INSERT INTO events (event_id, type, quote_id, ts, raw)
             VALUES (hex(randomblob(8)), ?1, ?2, ?3, '{}')",
            params![event_type, quote_id, ts],
        )
        .unwrap();
    }

    fn fake<'a>(conn: &'a Connection, fail: Option<&str>) -> FakeSender<'a> {
        FakeSender { conn, now: NOW, fail: fail.map(str::to_string) }
    }

    fn row(conn: &Connection, id: i64) -> Row {
        conn.query_row(
            "SELECT status, provider_id, error, sent_at, must_edit, body, updated_at
             FROM messages WHERE id = ?1",
            [id],
            |row| {
                Ok(Row {
                    status: row.get(0)?,
                    provider_id: row.get(1)?,
                    error: row.get(2)?,
                    sent_at: row.get(3)?,
                    must_edit: row.get(4)?,
                    body: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            },
        )
        .unwrap()
    }

    fn outbox(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT idempotency_key, provider_id FROM provider_outbox ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn run_draft(conn: &mut Connection, now: i64) -> usize {
        draft(conn, &Policy::default(), now).unwrap().0.len()
    }

    fn run_send(conn: &Connection, fail: Option<&str>) -> Vec<(i64, Delivery)> {
        let sent = send(conn, &Policy::default(), &fake(conn, fail), NOW).unwrap();
        sent.into_iter().map(|(msg, delivery)| (msg.id, delivery)).collect()
    }

    fn labels(conn: &Connection, now: i64) -> Vec<String> {
        let (ranked, suppressed) = policy::candidates(conn, &Policy::default(), now).unwrap();
        let all = ranked.into_iter().chain(suppressed);
        all.map(|(quote, outcome)| format!("{}: {outcome}", quote.id)).collect()
    }

    fn ids(messages: &[Message]) -> Vec<i64> {
        messages.iter().map(|msg| msg.id).collect()
    }

    fn refused(result: Result<(), Box<dyn Error>>) -> String {
        result.unwrap_err().to_string()
    }

    /// Q-1 alone, drafted and approved as #1.
    fn approved_row() -> Connection {
        let mut conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        run_draft(&mut conn, NOW);
        approve(&conn, 1, None, NOW).unwrap();
        conn
    }

    /// Runs ingest's real cancellation: the seed's events touch none of these tests' quotes.
    fn close_and_ingest(conn: &mut Connection, quote_id: &str) {
        conn.execute("UPDATE quotes SET status = 'accepted' WHERE id = ?1", [quote_id]).unwrap();
        let events = Path::new(env!("CARGO_MANIFEST_DIR")).join("seed/events.jsonl");
        assert_eq!(ingest(conn, &events, NOW).unwrap().cancelled, 1);
    }

    #[test]
    fn approve_rules() {
        let mut conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        add_quote(&conn, "Q-2", "+15550002");
        add_event(&conn, "customer_replied", "Q-1", NOW - DAY);
        assert_eq!(run_draft(&mut conn, NOW), 2);
        let placeholder = row(&conn, 1).body;
        assert!(row(&conn, 1).must_edit);

        assert!(refused(approve(&conn, 1, None, NOW)).contains("must be edited"));
        assert_eq!(row(&conn, 1).status, "draft");
        assert_eq!(refused(approve(&conn, 1, Some("  "), NOW)), "empty body");
        assert_eq!(
            refused(approve(&conn, 1, Some(&placeholder), NOW)),
            "message #1 must be edited: pass a new --body"
        );
        assert_eq!(row(&conn, 1).status, "draft");

        approve(&conn, 1, Some(" Hi Ann "), NOW).unwrap();
        let approved = row(&conn, 1);
        assert_eq!((approved.status.as_str(), approved.body.as_str()), ("approved", "Hi Ann"));
        assert!(approved.must_edit);

        approve(&conn, 1, Some("Hi again"), NOW).unwrap();
        assert_eq!(row(&conn, 1).body, "Hi again");
        assert!(refused(approve(&conn, 1, None, NOW)).contains("must be edited"));
        assert_eq!(row(&conn, 1).body, "Hi again");

        assert_eq!(refused(approve(&conn, 99, None, NOW)), "no message #99");

        let template = row(&conn, 2).body;
        approve(&conn, 2, None, NOW).unwrap();
        let plain = row(&conn, 2);
        assert_eq!((plain.status.as_str(), plain.body), ("approved", template));
        assert!(!plain.must_edit);

        approve(&conn, 2, Some("custom"), NOW).unwrap();
        let plain = row(&conn, 2);
        assert_eq!(plain.body, "custom");
        assert!(!plain.must_edit);

        assert_eq!(run_send(&conn, None).len(), 2);
        assert_eq!(refused(approve(&conn, 2, None, NOW)), "message #2 is sent, not draft");
    }

    #[test]
    fn approve_all_and_reject() {
        let mut conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        add_quote(&conn, "Q-2", "+15550002");
        add_quote(&conn, "Q-3", "+15550003");
        add_event(&conn, "customer_replied", "Q-1", NOW - DAY);
        assert_eq!(run_draft(&mut conn, NOW - HOUR), 3);

        let (approved, need_edit) = approve_all(&conn, NOW).unwrap();
        assert_eq!((ids(&approved), ids(&need_edit)), (vec![2, 3], vec![1]));
        let statuses = |conn: &Connection| [1, 2, 3].map(|id| row(conn, id).status);
        assert_eq!(statuses(&conn), ["draft", "approved", "approved"]);

        let (approved, need_edit) = approve_all(&conn, NOW).unwrap();
        assert_eq!((ids(&approved), ids(&need_edit)), (vec![], vec![1]));
        assert_eq!(statuses(&conn), ["draft", "approved", "approved"]);

        reject(&conn, 3, NOW + 1).unwrap();
        assert_eq!(row(&conn, 3).status, "rejected");
        assert_eq!(row(&conn, 3).updated_at, NOW + 1);
        assert_eq!(refused(reject(&conn, 3, NOW)), "message #3 is rejected, cannot reject");

        reject(&conn, 1, NOW).unwrap();
        assert_eq!(row(&conn, 1).status, "rejected");

        assert_eq!(run_send(&conn, None).len(), 1);
        assert_eq!(refused(reject(&conn, 2, NOW)), "message #2 is sent, cannot reject");
        assert_eq!(refused(reject(&conn, 99, NOW)), "no message #99");
        assert!(labels(&conn, NOW).contains(&"Q-3: rejected".to_string()));

        let conn = approved_row();
        assert!(matches!(run_send(&conn, Some("Q-1"))[..], [(1, Delivery::Failed(_))]));
        assert_eq!(refused(reject(&conn, 1, NOW)), "message #1 is failed, cannot reject");
    }

    #[test]
    fn send_twice_sends_once() {
        let conn = approved_row();

        let results = run_send(&conn, None);
        let sent = outbox(&conn);
        assert_eq!(sent.len(), 1);
        assert_eq!(results, [(1, Delivery::Sent(sent[0].1.clone()))]);
        let first = row(&conn, 1);
        assert_eq!(first.status, "sent");
        assert_eq!(first.provider_id.as_ref(), Some(&sent[0].1));
        assert_eq!(first.sent_at, Some(NOW));
        assert_eq!(first.error, None);

        assert_eq!(run_send(&conn, None), []);
        assert_eq!(outbox(&conn), sent);
        assert_eq!(row(&conn, 1), first);

        conn.execute("UPDATE messages SET status = 'sending'", []).unwrap();
        assert_eq!(run_send(&conn, None), []);
        assert_eq!(row(&conn, 1).status, "sending");
        assert_eq!(outbox(&conn), sent);
    }

    #[test]
    fn lost_ack_retries_with_same_key() {
        let conn = approved_row();
        let key = format!("Q-1:{}", time::format_ts(NOW));

        for _ in 0..2 {
            let results = run_send(&conn, Some("Q-1"));
            let [(1, Delivery::Failed(error))] = &results[..] else { panic!("{results:?}") };
            let failed = row(&conn, 1);
            assert_eq!(failed.status, "failed");
            assert_eq!(failed.error.as_ref(), Some(error));
            assert_eq!(failed.provider_id, None);
            assert_eq!(outbox(&conn).len(), 1);
            assert_eq!(outbox(&conn)[0].0, key);
        }

        // The retry stamps its own time, so the cooldown runs from it.
        let stored = outbox(&conn);
        let later = NOW + HOUR;
        let results = send(&conn, &Policy::default(), &fake(&conn, None), later).unwrap();
        assert!(matches!(&results[..], [(_, Delivery::Sent(id))] if *id == stored[0].1));
        let sent = row(&conn, 1);
        assert_eq!(sent.status, "sent");
        assert_eq!(sent.provider_id.as_ref(), Some(&stored[0].1));
        assert_eq!(sent.error, None);
        assert_eq!(sent.sent_at, Some(later));
        assert_eq!(outbox(&conn), stored);
    }

    fn assert_cancelled(conn: &Connection, reason: &'static str) {
        assert_eq!(run_send(conn, None), [(1, Delivery::Cancelled(reason))]);
        let cancelled = row(conn, 1);
        assert_eq!(cancelled.status, "cancelled");
        assert_eq!(cancelled.error.as_deref(), Some(reason));
        assert_eq!(cancelled.sent_at, None);
        assert_eq!(outbox(conn), []);
    }

    #[test]
    fn recheck_cancels() {
        let conn = approved_row();
        conn.execute("UPDATE quotes SET status = 'accepted'", []).unwrap();
        assert_cancelled(&conn, "quote not open");

        let conn = approved_row();
        conn.execute("DELETE FROM quotes", []).unwrap();
        assert_cancelled(&conn, "quote not open");

        let conn = approved_row();
        add_event(&conn, "message_sent", "Q-1", NOW - HOUR);
        assert_cancelled(&conn, "cooldown");

        // Stamped before the draft existed, ingested after it.
        let conn = approved_row();
        add_event(&conn, "customer_replied", "Q-1", NOW - DAY);
        assert_cancelled(&conn, "customer replied");

        let mut conn = memory_db();
        add_quote(&conn, "Q-2", PHONE);
        add_event(&conn, "customer_replied", "Q-2", NOW - DAY);
        run_draft(&mut conn, NOW);
        approve(&conn, 1, Some("Hi Ann"), NOW).unwrap();
        assert!(matches!(run_send(&conn, None)[..], [(1, Delivery::Sent(_))]));

        // A sibling's send doesn't hold back the answer to a reply.
        let mut conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        add_event(&conn, "customer_replied", "Q-1", NOW - DAY);
        run_draft(&mut conn, NOW);
        approve(&conn, 1, Some("Hi Ann"), NOW).unwrap();
        add_quote(&conn, "Q-2", PHONE);
        add_event(&conn, "message_sent", "Q-2", NOW - HOUR);
        assert!(matches!(run_send(&conn, None)[..], [(1, Delivery::Sent(_))]));

        // Our later send answered the reply, so the row is no longer one.
        let mut conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        add_event(&conn, "customer_replied", "Q-1", NOW - DAY);
        run_draft(&mut conn, NOW);
        approve(&conn, 1, Some("Hi Ann"), NOW).unwrap();
        add_event(&conn, "message_sent", "Q-1", NOW - HOUR);
        assert_cancelled(&conn, "cooldown");

        let conn = approved_row();
        conn.execute("UPDATE quotes SET customer_phone = '+15559999'", []).unwrap();
        assert_cancelled(&conn, "phone changed");

        let conn = approved_row();
        add_quote(&conn, "Q-2", PHONE);
        conn.execute("UPDATE quotes SET last_contact_at = ?1 WHERE id = 'Q-2'", [NOW - HOUR])
            .unwrap();
        assert_cancelled(&conn, "cooldown");

        let conn = approved_row();
        add_quote(&conn, "Q-2", PHONE);
        add_event(&conn, "customer_replied", "Q-2", NOW - DAY);
        assert_cancelled(&conn, "customer replied");

        // A closed sibling's old reply must not block the phone for good.
        let conn = approved_row();
        add_quote(&conn, "Q-2", PHONE);
        conn.execute("UPDATE quotes SET status = 'accepted' WHERE id = 'Q-2'", []).unwrap();
        add_event(&conn, "customer_replied", "Q-2", NOW - DAY);
        assert!(matches!(run_send(&conn, None)[..], [(1, Delivery::Sent(_))]));
    }

    #[test]
    fn failed_row_blocks_the_phone() {
        let mut conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        add_quote(&conn, "Q-2", PHONE);
        assert_eq!(run_draft(&mut conn, NOW), 1);
        approve(&conn, 1, None, NOW).unwrap();
        run_send(&conn, Some("Q-1"));
        assert_eq!(row(&conn, 1).status, "failed");

        assert_eq!(run_draft(&mut conn, NOW), 0);
        assert_eq!(labels(&conn, NOW), ["Q-1: pending message", "Q-2: pending message"]);

        conn.execute("UPDATE quotes SET status = 'accepted' WHERE id = 'Q-1'", []).unwrap();
        assert_eq!(run_send(&conn, None), [(1, Delivery::Cancelled("quote not open"))]);
        assert_eq!(run_draft(&mut conn, NOW), 0);
        assert_eq!(labels(&conn, NOW), ["Q-1: not open", "Q-2: cooldown"]);
        assert_eq!(run_draft(&mut conn, NOW + 48 * HOUR), 1);
        assert_eq!(row(&conn, 2).status, "draft");

        let mut conn = approved_row();
        run_send(&conn, Some("Q-1"));
        add_event(&conn, "customer_replied", "Q-1", NOW - HOUR);
        assert_eq!(run_send(&conn, None), [(1, Delivery::Cancelled("customer replied"))]);
        assert_eq!(labels(&conn, NOW), ["Q-1: unanswered_reply, never_contacted, stale"]);
        // Still inside the stamp's cooldown. At NOW itself the key would match the cancelled row.
        assert_eq!(run_draft(&mut conn, NOW + HOUR), 1);
        assert!(row(&conn, 2).must_edit);
        assert!(load_facts(&conn, NOW + HOUR).unwrap()["Q-1"].sends.is_empty());

        let mut conn = approved_row();
        add_quote(&conn, "Q-2", PHONE);
        conn.execute("UPDATE messages SET status = 'sending', sent_at = ?1", [NOW]).unwrap();
        close_and_ingest(&mut conn, "Q-1");
        assert_eq!(row(&conn, 1).status, "cancelled");
        assert_eq!(labels(&conn, NOW), ["Q-1: not open", "Q-2: cooldown"]);

        // The stamp stays with the number that may have got the text, not the quote's new one.
        let conn = approved_row();
        add_quote(&conn, "Q-2", PHONE);
        run_send(&conn, Some("Q-1"));
        conn.execute("UPDATE quotes SET customer_phone = '+15559999' WHERE id = 'Q-1'", [])
            .unwrap();
        assert_eq!(run_send(&conn, None), [(1, Delivery::Cancelled("phone changed"))]);
        assert_eq!(labels(&conn, NOW), ["Q-1: never_contacted, stale", "Q-2: cooldown"]);

        let mut conn = approved_row();
        add_quote(&conn, "Q-2", PHONE);
        close_and_ingest(&mut conn, "Q-1");
        assert_eq!(labels(&conn, NOW), ["Q-2: never_contacted, stale", "Q-1: not open"]);
    }

    /// Takes rows from under the run mid-call, as an ingest or a rival run would.
    struct Probe<'a>(FakeSender<'a>);

    impl Sender for Probe<'_> {
        fn send(&self, msg: &Message) -> Result<String, String> {
            let conn = self.0.conn;
            assert_eq!(row(conn, msg.id).status, "sending");
            conn.execute("UPDATE messages SET status = 'cancelled' WHERE id <= 3", []).unwrap();
            add_event(conn, "customer_replied", "Q-2", NOW - HOUR);
            add_event(conn, "customer_replied", "Q-4", NOW - HOUR);
            self.0.send(msg)
        }
    }

    #[test]
    fn claim_guards_hold() {
        let mut conn = memory_db();
        for n in 1..=4 {
            add_quote(&conn, &format!("Q-{n}"), &format!("+1555000{n}"));
        }
        assert_eq!(run_draft(&mut conn, NOW), 4);
        approve_all(&conn, NOW).unwrap();

        let probe = Probe(fake(&conn, None));
        let results = send(&conn, &Policy::default(), &probe, NOW).unwrap();
        let results: Vec<_> = results.into_iter().map(|(msg, result)| (msg.id, result)).collect();

        // #2 is blocked and #3 is not, so each loses at a different guard, silently.
        assert_eq!(
            results,
            [
                (1, Delivery::Cancelled("delivered, then cancelled")),
                (4, Delivery::Cancelled("customer replied"))
            ]
        );
        assert_eq!(row(&conn, 1).status, "cancelled");
        assert_eq!(row(&conn, 1).provider_id.as_ref(), Some(&outbox(&conn)[0].1));
        assert_eq!(row(&conn, 2).error, None);
        assert_eq!(row(&conn, 3).sent_at, None);
        assert_eq!(row(&conn, 4).error.as_deref(), Some("customer replied"));
        assert_eq!(outbox(&conn).len(), 1);

        let conn = approved_row();
        let probe = Probe(fake(&conn, Some("Q-1")));
        let results = send(&conn, &Policy::default(), &probe, NOW).unwrap();
        assert!(matches!(results[..], [(_, Delivery::Cancelled("cancelled during send"))]));
        assert_eq!(row(&conn, 1).status, "cancelled");
    }

    fn count(labels: &[String], label: &str) -> usize {
        labels.iter().filter(|line| line.ends_with(&format!(": {label}"))).count()
    }

    fn statuses(conn: &Connection, status: &str) -> i64 {
        let sql = "SELECT COUNT(*) FROM messages WHERE status = ?1";
        conn.query_row(sql, [status], |row| row.get(0)).unwrap()
    }

    #[test]
    fn seed_scripted_run() {
        let seed = Path::new(env!("CARGO_MANIFEST_DIR")).join("seed");
        let mut conn = memory_db();
        let counts = ingest(&mut conn, &seed, NOW).unwrap();
        assert_eq!((counts.quotes_stored, counts.events_stored), (30, 82));
        assert_eq!((counts.events_duplicate, counts.promoted, counts.cancelled), (6, 0, 0));

        assert_eq!(run_draft(&mut conn, NOW), 25);

        let (approved, need_edit) = approve_all(&conn, NOW).unwrap();
        assert_eq!(ids(&approved), (4..=25).collect::<Vec<_>>());
        let must_edit: Vec<_> = need_edit.iter().map(|msg| (msg.id, &*msg.quote_id)).collect();
        assert_eq!(must_edit, [(1, "Q-1016"), (2, "Q-1007"), (3, "Q-1015")]);

        let results = run_send(&conn, None);
        assert_eq!(results.iter().map(|(id, _)| *id).collect::<Vec<_>>(), ids(&approved));
        assert!(results.iter().all(|(_, delivery)| matches!(delivery, Delivery::Sent(_))));
        assert_eq!((statuses(&conn, "sent"), statuses(&conn, "draft")), (22, 3));
        assert_eq!(row(&conn, 4).sent_at, Some(NOW));
        assert_eq!(outbox(&conn).len(), 22);

        assert_eq!(run_send(&conn, None), []);
        assert_eq!(outbox(&conn).len(), 22);

        let lines = labels(&conn, NOW);
        assert_eq!(lines.len(), 30);
        assert_eq!(count(&lines, "cooldown"), 23);
        assert_eq!(count(&lines, "pending message"), 3);
        assert_eq!(count(&lines, "not open"), 3);
        assert!(lines.contains(&"Q-1019: ladder wait".to_string()));

        assert!(refused(approve(&conn, 1, None, NOW)).contains("must be edited"));
        let reply = "Hi Ray, thanks for your message about the quote. We can do Tuesday.";
        approve(&conn, 1, Some(reply), NOW).unwrap();
        assert!(row(&conn, 1).must_edit);

        let results = run_send(&conn, Some("Q-1016"));
        assert!(matches!(results[..], [(1, Delivery::Failed(_))]));
        let failed = row(&conn, 1);
        assert_eq!(failed.status, "failed");
        assert!(failed.error.is_some());
        assert_eq!(failed.provider_id, None);
        assert_eq!(outbox(&conn).len(), 23);

        let stored = outbox(&conn)[22].1.clone();
        assert_eq!(run_send(&conn, None), [(1, Delivery::Sent(stored.clone()))]);
        let sent = row(&conn, 1);
        assert_eq!(sent.status, "sent");
        assert_eq!(sent.provider_id, Some(stored));
        assert_eq!(sent.error, None);
        assert!(sent.must_edit);
        assert_eq!(outbox(&conn).len(), 23);

        let lines = labels(&conn, NOW);
        assert_eq!(lines.len(), 30);
        assert_eq!(count(&lines, "cooldown"), 24);
        assert_eq!(count(&lines, "pending message"), 2);
        assert_eq!(count(&lines, "not open"), 3);
        assert_eq!(count(&lines, "ladder wait"), 1);
        assert_eq!(run_draft(&mut conn, NOW), 0);
    }
}
