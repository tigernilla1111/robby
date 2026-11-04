use crate::policy::{self, Facts, Listed, Outcome, Policy, Reason};
use crate::time;
use rusqlite::{Connection, params};

const SHOP: &str = "Sunrise Home Services";
/// New York daylight time.
const SHOP_UTC_OFFSET: i64 = -4 * 3600;

pub struct Draft {
    pub id: i64,
    pub quote: Facts,
    pub outcome: Outcome,
    pub body: String,
}

fn render(reason: &Reason, name: &str, tech: &str, created_at: i64) -> String {
    let first = match name.split_whitespace().next().unwrap_or("").trim_end_matches(',') {
        "" => "there",
        first => first,
    };
    let tech = match tech.trim() {
        "" => "our team",
        tech => tech,
    };
    let date = time::format_date(created_at + SHOP_UTC_OFFSET);
    match reason {
        Reason::UnansweredReply => format!(
            "EDIT BEFORE SENDING: the customer replied about the quote {tech} prepared on {date}. Write a reply to their message."
        ),
        Reason::XlQuoteStale => format!(
            "Hi {first}, checking in on the quote {tech} put together on {date}. Happy to walk through it or answer any questions. - {SHOP}"
        ),
        Reason::RepeatViews => format!(
            "Hi {first}, {tech} put together a quote for you on {date}. Still thinking it over? Happy to answer any questions. - {SHOP}"
        ),
        Reason::ViewedNoResponse => format!(
            "Hi {first}, checking in on the quote from {tech} on {date}. Questions or changes? Just reply here any time. - {SHOP}"
        ),
        Reason::NeverContacted => format!(
            "Hi {first}, {tech} prepared a quote for you on {date}. Happy to go over it or answer any questions, just reply here. - {SHOP}"
        ),
        Reason::NeverViewed => format!(
            "Hi {first}, following up on the quote from {tech} on {date}. Want us to resend it, or have any questions? - {SHOP}"
        ),
        Reason::Stale => format!(
            "Hi {first}, checking in on the quote {tech} put together on {date}. Still something you'd like to do? Either way, happy to help. - {SHOP}"
        ),
    }
}

/// One draft per ranked candidate, and every suppressed quote, with its reason.
pub fn draft(
    conn: &mut Connection,
    policy: &Policy,
    now: i64,
) -> rusqlite::Result<(Vec<Draft>, Listed)> {
    let tx = conn.transaction()?;
    let (ranked, mut skipped) = policy::candidates(&tx, policy, now)?;
    let mut drafts = Vec::new();
    for (quote, outcome) in ranked {
        let Outcome::Candidate(reasons) = &outcome else { unreachable!() };
        let reason = &reasons[0];
        let body = render(reason, &quote.customer_name, &quote.tech_name, quote.created_at);
        let key = format!("{}:{}", quote.id, time::format_ts(now));
        let inserted = tx.execute(
            "INSERT INTO messages (quote_id, customer_phone, reason, body, must_edit, status,
                                   policy_now, idempotency_key, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'draft', ?6, ?7, ?6, ?6)
             ON CONFLICT DO NOTHING",
            params![
                quote.id,
                quote.phone,
                reason.as_str(),
                body,
                *reason == Reason::UnansweredReply,
                now,
                key
            ],
        )?;
        if inserted == 1 {
            drafts.push(Draft { id: tx.last_insert_rowid(), quote, outcome, body });
        } else {
            skipped.push((quote, Outcome::Suppressed("already drafted".to_string())));
        }
    }
    tx.commit()?;
    Ok((drafts, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::ingest::ingest;
    use crate::policy::DAY;
    use std::path::Path;

    const NOW: i64 = 1786971600;
    const PHONE: &str = "+15550000";

    const EXPECTED: [(&str, &str); 25] = [
        ("Q-1016", "unanswered_reply"),
        ("Q-1007", "unanswered_reply"),
        ("Q-1015", "unanswered_reply"),
        ("Q-1021", "xl_quote_stale"),
        ("Q-1026", "xl_quote_stale"),
        ("Q-1012", "xl_quote_stale"),
        ("Q-1027", "xl_quote_stale"),
        ("Q-1023", "xl_quote_stale"),
        ("Q-1020", "xl_quote_stale"),
        ("Q-1022", "repeat_views"),
        ("Q-1008", "repeat_views"),
        ("Q-1001", "repeat_views"),
        ("Q-1000", "repeat_views"),
        ("Q-1028", "repeat_views"),
        ("Q-1010", "never_contacted"),
        ("Q-1024", "never_contacted"),
        ("Q-1025", "never_contacted"),
        ("Q-1018", "never_contacted"),
        ("Q-1002", "never_viewed"),
        ("Q-1014", "never_viewed"),
        ("Q-1005", "never_viewed"),
        ("Q-1013", "never_viewed"),
        ("Q-1006", "stale"),
        ("Q-1011", "stale"),
        ("Q-1029", "stale"),
    ];

    const SKIPPED: [&str; 5] = [
        "Q-1003: same phone as Q-1025",
        "Q-1004: not open",
        "Q-1009: not open",
        "Q-1017: not open",
        "Q-1019: ladder wait",
    ];

    struct Row {
        id: i64,
        quote_id: String,
        customer_phone: String,
        reason: String,
        body: String,
        must_edit: bool,
        status: String,
        policy_now: i64,
        idempotency_key: String,
        created_at: i64,
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

    fn rows(conn: &Connection) -> Vec<Row> {
        let mut stmt = conn
            .prepare(
                "SELECT id, quote_id, customer_phone, reason, body, must_edit, status, policy_now,
                        idempotency_key, created_at
                 FROM messages ORDER BY id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok(Row {
                id: row.get(0)?,
                quote_id: row.get(1)?,
                customer_phone: row.get(2)?,
                reason: row.get(3)?,
                body: row.get(4)?,
                must_edit: row.get(5)?,
                status: row.get(6)?,
                policy_now: row.get(7)?,
                idempotency_key: row.get(8)?,
                created_at: row.get(9)?,
            })
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    fn run(conn: &mut Connection, now: i64) -> (Vec<Draft>, Vec<String>) {
        let (drafts, skipped) = draft(conn, &Policy::default(), now).unwrap();
        let labels = skipped.iter().map(|(quote, outcome)| format!("{}: {outcome}", quote.id));
        (drafts, labels.collect())
    }

    fn set_newest(conn: &Connection, status: &str, updated_at: i64, sent_at: Option<i64>) {
        conn.execute(
            "UPDATE messages SET status = ?1, updated_at = ?2, sent_at = ?3
             WHERE id = (SELECT MAX(id) FROM messages)",
            params![status, updated_at, sent_at],
        )
        .unwrap();
    }

    #[test]
    fn seed_draft_twice_inserts_once() {
        let seed = Path::new(env!("CARGO_MANIFEST_DIR")).join("seed");
        let mut conn = memory_db();
        ingest(&mut conn, &seed, NOW).unwrap();

        let (drafts, skipped) = run(&mut conn, NOW);
        assert_eq!(skipped, SKIPPED);

        let stored = rows(&conn);
        let reasons: Vec<(&str, &str)> =
            stored.iter().map(|row| (row.quote_id.as_str(), row.reason.as_str())).collect();
        assert_eq!(reasons, EXPECTED);
        assert_eq!(drafts.len(), 25);

        for (row, draft) in stored.iter().zip(&drafts) {
            assert_eq!(row.id, draft.id);
            assert_eq!(row.quote_id, draft.quote.id);
            assert!(draft.outcome.to_string().starts_with(&row.reason), "{}", row.quote_id);
            assert_eq!(row.body, draft.body);
            assert!(row.body.len() <= 160, "{}", row.body);
            assert!(!row.body.contains('$'), "{}", row.body);
            assert_eq!(row.status, "draft");
            assert_eq!(row.policy_now, NOW);
            assert_eq!(row.created_at, NOW);
            assert_eq!(row.customer_phone, draft.quote.phone);
            assert_eq!(row.idempotency_key, format!("{}:2026-08-17T13:00:00Z", row.quote_id));
            assert_eq!(
                row.must_edit,
                ["Q-1016", "Q-1007", "Q-1015"].contains(&row.quote_id.as_str())
            );
        }

        assert_eq!(
            stored[0].body,
            "EDIT BEFORE SENDING: the customer replied about the quote Jaden prepared on Aug 2. Write a reply to their message."
        );
        assert_eq!(
            stored[3].body,
            "Hi Judy, checking in on the quote Ridge put together on Aug 5. Happy to walk through it or answer any questions. - Sunrise Home Services"
        );
        assert_eq!(
            stored[18].body,
            "Hi Dave, following up on the quote from Brad on Aug 8. Want us to resend it, or have any questions? - Sunrise Home Services"
        );

        let (drafts, skipped) = run(&mut conn, NOW);
        assert!(drafts.is_empty());
        assert_eq!(skipped.len(), 30);
        assert_eq!(skipped.iter().filter(|label| label.ends_with(": pending message")).count(), 26);
        assert_eq!(rows(&conn).len(), 25);

        let (drafts, _) = run(&mut conn, NOW + DAY);
        assert!(drafts.is_empty());
        assert_eq!(rows(&conn).len(), 25);
    }

    #[test]
    fn second_quote_on_pending_phone_is_skipped() {
        let mut conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        add_quote(&conn, "Q-2", PHONE);

        let (drafts, skipped) = run(&mut conn, NOW);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].quote.id, "Q-1");
        assert_eq!(skipped, ["Q-2: same phone as Q-1"]);

        let (drafts, skipped) = run(&mut conn, NOW);
        assert!(drafts.is_empty());
        assert_eq!(skipped, ["Q-1: pending message", "Q-2: pending message"]);
    }

    #[test]
    fn redraft_after_each_end_state() {
        let mut conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        assert_eq!(run(&mut conn, NOW).0.len(), 1);
        assert_eq!(rows(&conn).len(), 1);

        // The only path to the conflict clause: not pending, same now, same key.
        set_newest(&conn, "cancelled", NOW, None);
        let (drafts, skipped) = run(&mut conn, NOW);
        assert!(drafts.is_empty());
        assert_eq!(skipped, ["Q-1: already drafted"]);
        assert_eq!(rows(&conn).len(), 1);

        set_newest(&conn, "rejected", NOW, None);
        let (drafts, skipped) = run(&mut conn, NOW);
        assert!(drafts.is_empty());
        assert_eq!(skipped, ["Q-1: rejected"]);

        let later = NOW + 3 * DAY;
        assert_eq!(run(&mut conn, later).0.len(), 1);
        let stored = rows(&conn);
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[1].idempotency_key, format!("Q-1:{}", time::format_ts(later)));

        set_newest(&conn, "cancelled", later, None);
        assert_eq!(run(&mut conn, NOW + 4 * DAY).0.len(), 1);
        assert_eq!(rows(&conn).len(), 3);

        set_newest(&conn, "sent", NOW + 4 * DAY, Some(NOW + 4 * DAY));
        let (drafts, skipped) = run(&mut conn, NOW + 5 * DAY);
        assert!(drafts.is_empty());
        assert_eq!(skipped, ["Q-1: cooldown"]);
        assert_eq!(rows(&conn).len(), 3);
    }

    #[test]
    fn every_template_is_exact() {
        let created_at = time::parse_ts("2026-08-07T04:00:00Z").unwrap();
        let table = [
            (
                Reason::UnansweredReply,
                "EDIT BEFORE SENDING: the customer replied about the quote Jose M. prepared on Aug 7. Write a reply to their message.",
            ),
            (
                Reason::XlQuoteStale,
                "Hi Ann, checking in on the quote Jose M. put together on Aug 7. Happy to walk through it or answer any questions. - Sunrise Home Services",
            ),
            (
                Reason::RepeatViews,
                "Hi Ann, Jose M. put together a quote for you on Aug 7. Still thinking it over? Happy to answer any questions. - Sunrise Home Services",
            ),
            (
                Reason::ViewedNoResponse,
                "Hi Ann, checking in on the quote from Jose M. on Aug 7. Questions or changes? Just reply here any time. - Sunrise Home Services",
            ),
            (
                Reason::NeverContacted,
                "Hi Ann, Jose M. prepared a quote for you on Aug 7. Happy to go over it or answer any questions, just reply here. - Sunrise Home Services",
            ),
            (
                Reason::NeverViewed,
                "Hi Ann, following up on the quote from Jose M. on Aug 7. Want us to resend it, or have any questions? - Sunrise Home Services",
            ),
            (
                Reason::Stale,
                "Hi Ann, checking in on the quote Jose M. put together on Aug 7. Still something you'd like to do? Either way, happy to help. - Sunrise Home Services",
            ),
        ];
        let before = time::parse_ts("2026-08-07T03:59:59Z").unwrap();
        for (reason, expected) in &table {
            let body = render(reason, "Ann Lee", "Jose M.", created_at);
            assert_eq!(body, *expected);
            assert!(body.len() <= 160, "{body}");
            assert!(body.is_ascii(), "{body}");
            assert!(!body.contains('$'), "{body}");
            assert_eq!(
                render(reason, "Ann Lee", "Jose M.", before),
                expected.replace("Aug 7", "Aug 6")
            );
        }
    }

    #[test]
    fn missing_names_never_break_the_greeting() {
        let greeting = |name: &str| render(&Reason::Stale, name, "Brad", 0);
        assert!(greeting("").starts_with("Hi there,"));
        assert!(greeting("   ").starts_with("Hi there,"));
        assert!(greeting("  karen   nguyen ").starts_with("Hi karen,"));
        assert!(greeting("{tech}").starts_with("Hi {tech},"));
        assert!(greeting("Smith, John").starts_with("Hi Smith, checking"));
        assert!(greeting(",").starts_with("Hi there,"));

        let credit = |tech: &str| render(&Reason::Stale, "Ann", tech, 0);
        assert!(credit("").contains("the quote our team put together"));
        assert!(credit(" Jose M. ").contains("the quote Jose M. put together"));
    }
}
