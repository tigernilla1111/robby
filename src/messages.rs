use crate::policy::{Facts, Policy, load_facts};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;
use std::error::Error;

#[derive(Debug, PartialEq)]
pub struct Message {
    pub id: i64,
    pub quote_id: String,
    pub phone: String,
    pub reason: String,
    pub body: String,
    pub must_edit: bool,
    pub key: String,
    pub status: String,
    pub created_at: i64,
    pub sent_at: Option<i64>,
    pub provider_id: Option<String>,
    pub error: Option<String>,
}

pub trait Sender {
    /// Deliver one message and return the provider's id. `key` must be idempotent there.
    fn send(&self, msg: &Message) -> Result<String, String>;
}

/// Stands in for the provider: a key it has seen stores nothing and returns the first id.
pub struct FakeSender<'a> {
    pub conn: &'a Connection,
    /// Quote whose ack is lost: the text is stored, then the call errors.
    pub fail: Option<String>,
}

impl Sender for FakeSender<'_> {
    fn send(&self, msg: &Message) -> Result<String, String> {
        self.conn
            .execute(
                "INSERT INTO provider_outbox (idempotency_key, provider_id, to_phone, body)
                 VALUES (?1, 'sm_' || lower(hex(randomblob(4))), ?2, ?3)
                 ON CONFLICT DO NOTHING",
                params![msg.key, msg.phone, msg.body],
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
        "SELECT id, quote_id, customer_phone, reason, body, must_edit, idempotency_key, status,
                created_at, sent_at, provider_id, error
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
            status: row.get(7)?,
            created_at: row.get(8)?,
            sent_at: row.get(9)?,
            provider_id: row.get(10)?,
            error: row.get(11)?,
        })
    })?
    .collect()
}

pub fn list(conn: &Connection) -> rusqlite::Result<Vec<Message>> {
    select(conn, "TRUE")
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
pub fn approve(conn: &Connection, id: i64, body: Option<&str>) -> Result<(), Box<dyn Error>> {
    let body = body.map(str::trim);
    if body == Some("") {
        return Err("empty body".into());
    }
    let changed = conn.execute(
        "UPDATE messages SET status = 'approved', body = COALESCE(?2, body)
         WHERE id = ?1 AND status IN ('draft','approved')
           AND (must_edit = 0 OR ?2 <> body)",
        params![id, body],
    )?;
    if changed == 0 {
        return refusal(conn, id, "not draft");
    }
    Ok(())
}

/// Approves every plain draft. Returns the approved rows and the must-edit rows it skipped.
pub fn approve_all(conn: &Connection) -> rusqlite::Result<(Vec<Message>, Vec<Message>)> {
    let (need_edit, approved) =
        select(conn, "status = 'draft'")?.into_iter().partition(|msg| msg.must_edit);
    let sql = "UPDATE messages SET status = 'approved' WHERE status = 'draft' AND must_edit = 0";
    conn.execute(sql, [])?;
    Ok((approved, need_edit))
}

pub fn reject(conn: &Connection, id: i64, now: i64) -> Result<(), Box<dyn Error>> {
    let changed = conn.execute(
        "UPDATE messages SET status = 'rejected', rejected_at = ?2
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
    let replied = facts.values().any(|q| q.open && q.phone == msg.phone && q.unanswered_reply());
    // A must-edit row is the answer, so neither check below applies. Once a send answers the
    // reply, `replied` is false and the row is outreach like any other.
    if msg.must_edit && replied {
        return None;
    }
    if policy.in_cooldown(quote.phone_last_sent, now) {
        return Some("cooldown");
    }
    replied.then_some("customer replied")
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
    for mut msg in select(conn, "status IN ('approved','failed')")? {
        // Per row, so a reply or a closure ingested during the run reaches the next row.
        let facts = load_facts(conn, now)?;
        if let Some(reason) = blocked(policy, &facts, &msg, now) {
            let cancelled = conn.execute(
                "UPDATE messages SET status = 'cancelled', error = ?2
                 WHERE id = ?1 AND status IN ('approved','failed')",
                params![msg.id, reason],
            )?;
            if cancelled == 1 {
                results.push((msg, Delivery::Cancelled(reason)));
            }
            continue;
        }

        // The stamp records the attempt before the provider can deliver it. The body comes back
        // with the claim, so an edit approved since the run began is the text that goes out.
        let claimed = conn
            .query_row(
                "UPDATE messages SET status = 'sending', sent_at = ?2
                 WHERE id = ?1 AND status IN ('approved','failed')
                 RETURNING body",
                params![msg.id, now],
                |row| row.get(0),
            )
            .optional()?;
        let Some(body) = claimed else {
            // Another run or ingest got there first, and reports its own result.
            continue;
        };
        msg.body = body;

        // No row marked means ingest cancelled it mid-send, so the report matches the row.
        let delivery = match sender.send(&msg) {
            Ok(provider_id) => {
                let marked = conn.execute(
                    "UPDATE messages SET status = 'sent', provider_id = ?2, error = NULL
                     WHERE id = ?1 AND status = 'sending'",
                    params![msg.id, provider_id],
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
                    "UPDATE messages SET status = 'failed', error = ?2
                     WHERE id = ?1 AND status = 'sending'",
                    params![msg.id, error],
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
    use crate::draft::draft;
    use crate::ingest::ingest;
    use crate::policy::{DAY, HOUR};
    use crate::testutil::*;
    use crate::time;

    fn fake<'a>(conn: &'a Connection, fail: Option<&str>) -> FakeSender<'a> {
        FakeSender { conn, fail: fail.map(str::to_string) }
    }

    fn row(conn: &Connection, id: i64) -> Message {
        select(conn, &format!("id = {id}")).unwrap().remove(0)
    }

    fn outbox(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT idempotency_key, provider_id FROM provider_outbox ORDER BY rowid")
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
        approve(&conn, 1, None).unwrap();
        conn
    }

    /// Q-1 with a customer reply, answered by a human as #1.
    fn approved_answer() -> Connection {
        let mut conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        add_event(&conn, "customer_replied", "Q-1", NOW - DAY);
        run_draft(&mut conn, NOW);
        approve(&conn, 1, Some("Hi Ann")).unwrap();
        conn
    }

    /// Runs ingest's real cancellation: the seed's events touch none of these tests' quotes.
    fn close_and_ingest(conn: &mut Connection, quote_id: &str) {
        conn.execute("UPDATE quotes SET status = 'accepted' WHERE id = ?1", [quote_id]).unwrap();
        assert_eq!(ingest(conn, &seed().join("events.jsonl")).unwrap().cancelled, 1);
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

        assert!(refused(approve(&conn, 1, None)).contains("must be edited"));
        assert_eq!(row(&conn, 1).status, "draft");
        assert_eq!(refused(approve(&conn, 1, Some("  "))), "empty body");
        assert_eq!(
            refused(approve(&conn, 1, Some(&placeholder))),
            "message #1 must be edited: pass a new --body"
        );
        assert_eq!(row(&conn, 1).status, "draft");

        approve(&conn, 1, Some(" Hi Ann ")).unwrap();
        let approved = row(&conn, 1);
        assert_eq!((approved.status.as_str(), approved.body.as_str()), ("approved", "Hi Ann"));
        assert!(approved.must_edit);

        approve(&conn, 1, Some("Hi again")).unwrap();
        assert_eq!(row(&conn, 1).body, "Hi again");
        assert!(refused(approve(&conn, 1, None)).contains("must be edited"));
        assert_eq!(row(&conn, 1).body, "Hi again");

        assert_eq!(refused(approve(&conn, 99, None)), "no message #99");

        let template = row(&conn, 2).body;
        approve(&conn, 2, None).unwrap();
        let plain = row(&conn, 2);
        assert_eq!((plain.status.as_str(), plain.body), ("approved", template));
        assert!(!plain.must_edit);

        approve(&conn, 2, Some("custom")).unwrap();
        let plain = row(&conn, 2);
        assert_eq!(plain.body, "custom");
        assert!(!plain.must_edit);

        assert_eq!(run_send(&conn, None).len(), 2);
        assert_eq!(refused(approve(&conn, 2, None)), "message #2 is sent, not draft");
    }

    #[test]
    fn approve_all_and_reject() {
        let mut conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        add_quote(&conn, "Q-2", "+15550002");
        add_quote(&conn, "Q-3", "+15550003");
        add_event(&conn, "customer_replied", "Q-1", NOW - DAY);
        assert_eq!(run_draft(&mut conn, NOW - HOUR), 3);

        let (approved, need_edit) = approve_all(&conn).unwrap();
        assert_eq!((ids(&approved), ids(&need_edit)), (vec![2, 3], vec![1]));
        let statuses = |conn: &Connection| [1, 2, 3].map(|id| row(conn, id).status);
        assert_eq!(statuses(&conn), ["draft", "approved", "approved"]);

        let (approved, need_edit) = approve_all(&conn).unwrap();
        assert_eq!((ids(&approved), ids(&need_edit)), (vec![], vec![1]));
        assert_eq!(statuses(&conn), ["draft", "approved", "approved"]);

        reject(&conn, 3, NOW + 1).unwrap();
        assert_eq!(row(&conn, 3).status, "rejected");
        let sql = "SELECT rejected_at FROM messages WHERE id = 3";
        assert_eq!(conn.query_row(sql, [], |row| row.get(0)), Ok(NOW + 1));
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

        let conn = approved_answer();
        assert!(matches!(run_send(&conn, None)[..], [(1, Delivery::Sent(_))]));

        // A sibling's send doesn't hold back the answer to a reply.
        let conn = approved_answer();
        add_quote(&conn, "Q-2", PHONE);
        add_event(&conn, "message_sent", "Q-2", NOW - HOUR);
        assert!(matches!(run_send(&conn, None)[..], [(1, Delivery::Sent(_))]));

        // Our later send answered the reply, so the row is no longer one.
        let conn = approved_answer();
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
        approve(&conn, 1, None).unwrap();
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
        approve_all(&conn).unwrap();

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

    /// Edits the next row mid-call, as a human running `approve --body` would.
    struct EditsNext<'a>(FakeSender<'a>);

    impl Sender for EditsNext<'_> {
        fn send(&self, msg: &Message) -> Result<String, String> {
            if msg.id == 1 {
                approve(self.0.conn, 2, Some("edited")).unwrap();
            }
            self.0.send(msg)
        }
    }

    #[test]
    fn edit_during_a_run_is_what_goes_out() {
        let mut conn = memory_db();
        add_quote(&conn, "Q-1", "+15550001");
        add_quote(&conn, "Q-2", "+15550002");
        assert_eq!(run_draft(&mut conn, NOW), 2);
        approve_all(&conn).unwrap();

        send(&conn, &Policy::default(), &EditsNext(fake(&conn, None)), NOW).unwrap();

        let sql = "SELECT body FROM provider_outbox WHERE to_phone = '+15550002'";
        let delivered: String = conn.query_row(sql, [], |row| row.get(0)).unwrap();
        assert_eq!(delivered, "edited");
        assert_eq!(row(&conn, 2).status, "sent");
    }
}
