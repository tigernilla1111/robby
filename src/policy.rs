use rusqlite::{Connection, params};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

const HOUR: i64 = 3600;
pub const DAY: i64 = 86_400;

pub struct Policy {
    pub cooldown: i64,
    pub min_age: i64,
    pub xl_amount: f64,
    pub view_window: i64,
    pub max_sends: usize,
    /// Wait after send 1..5.
    pub ladder: [i64; 5],
    pub give_up: i64,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            cooldown: 48 * HOUR,
            min_age: 48 * HOUR,
            xl_amount: 11_000.0,
            view_window: 72 * HOUR,
            max_sends: 6,
            ladder: [48 * HOUR, 72 * HOUR, 72 * HOUR, 7 * DAY, 14 * DAY],
            give_up: 30 * DAY,
        }
    }
}

impl Policy {
    pub fn in_cooldown(&self, at: Option<i64>, now: i64) -> bool {
        at.is_some_and(|at| now - at < self.cooldown)
    }
}

/// Declaration order is rule order.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    UnansweredReply,
    XlQuoteStale,
    RepeatViews,
    ViewedNoResponse,
    NeverContacted,
    NeverViewed,
    Stale,
}

impl Reason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Reason::UnansweredReply => "unanswered_reply",
            Reason::XlQuoteStale => "xl_quote_stale",
            Reason::RepeatViews => "repeat_views",
            Reason::ViewedNoResponse => "viewed_no_response",
            Reason::NeverContacted => "never_contacted",
            Reason::NeverViewed => "never_viewed",
            Reason::Stale => "stale",
        }
    }
}

pub enum Outcome {
    Candidate(Vec<Reason>),
    Suppressed(String),
    Hidden,
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Outcome::Candidate(reasons) => {
                let names: Vec<&str> = reasons.iter().map(Reason::as_str).collect();
                f.write_str(&names.join(", "))
            }
            Outcome::Suppressed(label) => f.write_str(label),
            Outcome::Hidden => f.write_str("hidden"),
        }
    }
}

/// One quote as of a given now.
pub struct Facts {
    pub id: String,
    pub customer_name: String,
    pub phone: String,
    pub tech_name: String,
    pub amount: f64,
    pub open: bool,
    pub created_at: i64,
    /// Distinct outbound timestamps from all three sources.
    pub sends: BTreeSet<i64>,
    pub views: BTreeSet<i64>,
    pub replies: BTreeSet<i64>,
    /// A pending message exists for this customer, matched by the row's phone or by the row's quote.
    pub phone_pending: bool,
    /// Newest send to this phone, on any quote of any status.
    pub phone_last_sent: Option<i64>,
    /// `updated_at` of this quote's newest rejected message.
    pub rejected_at: Option<i64>,
}

impl Facts {
    pub fn last_sent(&self) -> Option<i64> {
        self.sends.last().copied()
    }

    /// The base for every "nothing sent for N days".
    pub fn since_sent(&self) -> i64 {
        self.last_sent().unwrap_or(self.created_at)
    }
}

pub fn load_facts(conn: &Connection, now: i64) -> rusqlite::Result<BTreeMap<String, Facts>> {
    let mut facts = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT id, customer_name, customer_phone, tech_name, amount, status, created_at,
                last_contact_at
         FROM quotes",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let status: String = row.get(5)?;
        let last_contact_at: Option<i64> = row.get(7)?;
        let quote = Facts {
            id: id.clone(),
            customer_name: row.get(1)?,
            phone: row.get(2)?,
            tech_name: row.get(3)?,
            amount: row.get(4)?,
            open: status == "open",
            created_at: row.get(6)?,
            sends: last_contact_at.into_iter().filter(|ts| *ts <= now).collect(),
            views: BTreeSet::new(),
            replies: BTreeSet::new(),
            phone_pending: false,
            phone_last_sent: None,
            rejected_at: None,
        };
        facts.insert(id, quote);
    }

    // Comparing in SQL means a non-text direction cannot fail the row read.
    let mut stmt = conn.prepare(
        "SELECT quote_id, type, ts,
                COALESCE(lower(json_extract(raw, '$.direction')) = 'inbound', 0)
         FROM events WHERE ts <= ?1",
    )?;
    let mut rows = stmt.query(params![now])?;
    while let Some(row) = rows.next()? {
        let quote_id: String = row.get(0)?;
        let event_type: String = row.get(1)?;
        let ts: i64 = row.get(2)?;
        let inbound: bool = row.get(3)?;
        let Some(quote) = facts.get_mut(&quote_id) else { continue };
        match event_type.as_str() {
            "quote_viewed" => {
                quote.views.insert(ts);
            }
            "customer_replied" => {
                quote.replies.insert(ts);
            }
            "message_sent" if !inbound => {
                quote.sends.insert(ts);
            }
            _ => {}
        }
    }

    let mut pending_phones = BTreeSet::new();
    let mut phone_last_sent = BTreeMap::new();
    let mut stmt =
        conn.prepare("SELECT quote_id, customer_phone, status, sent_at, updated_at FROM messages")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let quote_id: String = row.get(0)?;
        let phone: String = row.get(1)?;
        let status: String = row.get(2)?;
        let sent_at: Option<i64> = row.get(3)?;
        let updated_at: i64 = row.get(4)?;
        let quote = facts.get_mut(&quote_id);
        match status.as_str() {
            "sent" => {
                let sent_at = sent_at.unwrap_or(updated_at);
                if let Some(quote) = quote {
                    quote.sends.insert(sent_at);
                }
                // Keyed by the row's phone: a re-export may have moved the quote off it.
                let newest = phone_last_sent.entry(phone).or_insert(sent_at);
                *newest = sent_at.max(*newest);
            }
            // A stamped row was attempted and may have arrived: the phone waits, the ladder doesn't.
            "cancelled" => {
                if let Some(sent_at) = sent_at {
                    let newest = phone_last_sent.entry(phone).or_insert(sent_at);
                    *newest = sent_at.max(*newest);
                }
            }
            "draft" | "approved" | "sending" | "failed" => {
                pending_phones.insert(phone);
                // A quote that moved numbers is still blocked by its own pending message.
                if let Some(quote) = quote {
                    pending_phones.insert(quote.phone.clone());
                }
            }
            "rejected" => {
                if let Some(quote) = quote {
                    quote.rejected_at = quote.rejected_at.max(Some(updated_at));
                }
            }
            _ => {}
        }
    }

    // Closed quotes count too: the cooldown protects the customer, not the quote.
    for quote in facts.values() {
        if let Some(last_sent) = quote.last_sent() {
            let newest = phone_last_sent.entry(quote.phone.clone()).or_insert(last_sent);
            *newest = last_sent.max(*newest);
        }
    }
    for quote in facts.values_mut() {
        quote.phone_pending = pending_phones.contains(&quote.phone);
        quote.phone_last_sent = phone_last_sent.get(&quote.phone).copied();
    }
    Ok(facts)
}

fn matching_rules(policy: &Policy, facts: &Facts, now: i64) -> Vec<Reason> {
    let last_sent = facts.last_sent();
    let last_reply = facts.replies.last().copied();
    let last_view = facts.views.last().copied();
    let waited = now - facts.since_sent();
    // `None` orders below `Some`, so these comparisons also cover "never sent".
    let views_since = facts.views.iter().filter(|view| Some(**view) > last_sent).count();
    let recent_view = last_view.is_some_and(|view| now - view < policy.view_window);

    [
        (last_reply > last_sent, Reason::UnansweredReply),
        (facts.amount >= policy.xl_amount && waited >= 3 * DAY, Reason::XlQuoteStale),
        (views_since >= 2 && facts.replies.is_empty(), Reason::RepeatViews),
        (recent_view && last_view > last_sent, Reason::ViewedNoResponse),
        (
            facts.sends.is_empty() && now - facts.created_at >= policy.min_age,
            Reason::NeverContacted,
        ),
        (
            facts.views.is_empty() && last_sent.is_some() && waited >= 3 * DAY,
            Reason::NeverViewed,
        ),
        (waited >= 7 * DAY, Reason::Stale),
    ]
    .into_iter()
    .filter_map(|(fits, reason)| fits.then_some(reason))
    .collect()
}

pub fn decide(policy: &Policy, facts: &Facts, now: i64) -> Outcome {
    let suppressed = |label: &str| Outcome::Suppressed(label.to_string());

    if !facts.open {
        return suppressed("not open");
    }
    if facts.phone_pending {
        return suppressed("pending message");
    }
    let reasons = matching_rules(policy, facts, now);
    let replied = reasons.first() == Some(&Reason::UnansweredReply);
    if !replied && policy.in_cooldown(facts.phone_last_sent, now) {
        return suppressed("cooldown");
    }
    if policy.in_cooldown(facts.rejected_at, now) {
        return suppressed("rejected");
    }

    if !replied {
        let sends = facts.sends.len();
        let waited = now - facts.since_sent();
        if now - facts.created_at < policy.min_age {
            return suppressed("too young");
        }
        if sends >= policy.max_sends {
            return if waited < policy.give_up { suppressed("exhausted") } else { Outcome::Hidden };
        }
        if sends > 0 && policy.ladder.get(sends - 1).is_some_and(|wait| waited < *wait) {
            return suppressed("ladder wait");
        }
    }
    if reasons.is_empty() {
        return suppressed("no rule");
    }
    Outcome::Candidate(reasons)
}

pub type Listed = Vec<(Facts, Outcome)>;

/// Candidates in rank order, and every suppressed quote by id.
pub fn candidates(
    conn: &Connection,
    policy: &Policy,
    now: i64,
) -> rusqlite::Result<(Listed, Listed)> {
    let mut matched = Vec::new();
    let mut suppressed = Vec::new();
    for quote in load_facts(conn, now)?.into_values() {
        match decide(policy, &quote, now) {
            Outcome::Candidate(reasons) => matched.push((quote, reasons)),
            Outcome::Hidden => {}
            outcome => suppressed.push((quote, outcome)),
        }
    }
    matched.sort_by(|(a, a_reasons), (b, b_reasons)| {
        a_reasons[0]
            .cmp(&b_reasons[0])
            .then(b.amount.total_cmp(&a.amount))
            .then(a.since_sent().cmp(&b.since_sent()))
            .then(a.id.cmp(&b.id))
    });

    let mut winners: BTreeMap<String, String> = BTreeMap::new();
    let mut ranked = Vec::new();
    for (quote, reasons) in matched {
        if let Some(winner) = winners.get(&quote.phone) {
            let label = format!("same phone as {winner}");
            suppressed.push((quote, Outcome::Suppressed(label)));
        } else {
            winners.insert(quote.phone.clone(), quote.id.clone());
            ranked.push((quote, Outcome::Candidate(reasons)));
        }
    }
    suppressed.sort_by(|(a, _), (b, _)| a.id.cmp(&b.id));
    Ok((ranked, suppressed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::ingest::ingest;
    use std::fs;
    use std::path::Path;

    const NOW: i64 = 1786971600;
    const PHONE: &str = "+15550000";

    const GOLDEN: [&str; 30] = [
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

    fn facts(id: &str) -> Facts {
        Facts {
            id: id.to_string(),
            customer_name: "Ann".to_string(),
            phone: format!("+1{id}"),
            tech_name: "Brad".to_string(),
            amount: 1000.0,
            open: true,
            created_at: NOW - 10 * DAY,
            sends: BTreeSet::new(),
            views: BTreeSet::new(),
            replies: BTreeSet::new(),
            phone_pending: false,
            phone_last_sent: None,
            rejected_at: None,
        }
    }

    /// `count` sends a day apart, ending at `last`.
    fn sends(count: i64, last: i64) -> BTreeSet<i64> {
        (0..count).map(|i| last - i * DAY).collect()
    }

    fn label(quote: &Facts) -> String {
        decide(&Policy::default(), quote, NOW).to_string()
    }

    fn memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        db::migrate(&conn).unwrap();
        conn
    }

    fn add_quote(conn: &Connection, id: &str, phone: &str, status: &str, last_contact: Option<i64>) {
        conn.execute(
            "INSERT INTO quotes (id, customer_name, customer_phone, tech_name, amount, status,
                                 created_at, last_contact_at)
             VALUES (?1, 'Ann', ?2, 'Brad', 1000, ?3, ?4, ?5)",
            params![id, phone, status, NOW - 10 * DAY, last_contact],
        )
        .unwrap();
    }

    fn add_message(
        conn: &Connection,
        quote_id: &str,
        phone: &str,
        status: &str,
        sent_at: Option<i64>,
        updated_at: i64,
    ) {
        conn.execute(
            "INSERT INTO messages (quote_id, customer_phone, reason, body, status, policy_now,
                                   idempotency_key, created_at, updated_at, sent_at)
             VALUES (?1, ?2, 'test', 'hi', ?3, 0, hex(randomblob(8)), 0, ?4, ?5)",
            params![quote_id, phone, status, updated_at, sent_at],
        )
        .unwrap();
    }

    fn add_event(conn: &Connection, event_type: &str, quote_id: &str, ts: i64, raw: &str) {
        conn.execute(
            "INSERT INTO events (event_id, type, quote_id, ts, raw)
             VALUES (hex(randomblob(8)), ?1, ?2, ?3, ?4)",
            params![event_type, quote_id, ts, raw],
        )
        .unwrap();
    }

    fn outcomes(conn: &Connection) -> Vec<String> {
        let (ranked, suppressed) = candidates(conn, &Policy::default(), NOW).unwrap();
        ranked
            .into_iter()
            .chain(suppressed)
            .map(|(quote, outcome)| format!("{}: {outcome}", quote.id))
            .collect()
    }

    #[test]
    fn golden_seed_at_demo_now() {
        let seed = Path::new(env!("CARGO_MANIFEST_DIR")).join("seed");
        let mut conn = memory_db();
        ingest(&mut conn, &seed, NOW).unwrap();
        assert_eq!(outcomes(&conn), GOLDEN);

        let loaded = load_facts(&conn, NOW).unwrap();
        assert_eq!(loaded["Q-1012"].views.len(), 6);
        assert_eq!(loaded["Q-1002"].sends.len(), 2);

        let text = fs::read_to_string(seed.join("events.jsonl")).unwrap();
        let reversed: Vec<&str> = text.lines().rev().collect();
        let copy = std::env::temp_dir().join(format!("robby-reversed-{}.jsonl", std::process::id()));
        fs::write(&copy, reversed.join("\n")).unwrap();

        let mut fresh = memory_db();
        ingest(&mut fresh, &seed.join("quotes.json"), NOW).unwrap();
        let ingested = ingest(&mut fresh, &copy, NOW);
        fs::remove_file(&copy).unwrap();
        ingested.unwrap();
        assert_eq!(outcomes(&fresh), GOLDEN);
    }

    #[test]
    fn rule_matching() {
        let mut quote = facts("Q-1");
        quote.sends = sends(1, NOW - 5 * DAY);
        quote.views = BTreeSet::from([NOW - DAY]);
        assert_eq!(label(&quote), "viewed_no_response");

        quote.views = BTreeSet::from([NOW - 4 * DAY]);
        assert_eq!(label(&quote), "no rule");

        // A view exactly at the window's edge is outside it.
        quote.views = BTreeSet::from([NOW - 72 * HOUR]);
        assert_eq!(label(&quote), "no rule");

        let mut quote = facts("Q-2");
        quote.sends = sends(1, NOW - 55 * HOUR);
        quote.phone_last_sent = quote.last_sent();
        quote.views = BTreeSet::from([NOW - 60 * HOUR]);
        assert_eq!(label(&quote), "no rule");

        let mut quote = facts("Q-3");
        quote.sends = sends(1, NOW - 5 * DAY);
        quote.views = BTreeSet::from([NOW - 6 * DAY, NOW - 6 * DAY + HOUR, NOW - 4 * DAY]);
        assert_eq!(label(&quote), "no rule");

        quote.views = BTreeSet::from([NOW - 4 * DAY, NOW - 4 * DAY + HOUR]);
        assert_eq!(label(&quote), "repeat_views");

        quote.replies = BTreeSet::from([NOW - 6 * DAY]);
        assert_eq!(label(&quote), "no rule");

        let mut quote = facts("Q-4");
        quote.created_at = NOW - 47 * HOUR;
        assert_eq!(label(&quote), "too young");
    }

    #[test]
    fn ladder_cap_and_age() {
        let mut quote = facts("Q-1");
        quote.sends = sends(2, NOW - 2 * DAY);
        quote.views = BTreeSet::from([NOW - DAY, NOW - DAY + HOUR]);
        assert_eq!(label(&quote), "ladder wait");

        quote.sends = sends(2, NOW - 4 * DAY);
        quote.views = BTreeSet::from([NOW - 3 * DAY, NOW - 3 * DAY - HOUR]);
        assert_eq!(label(&quote), "repeat_views");

        quote.sends = sends(4, NOW - 6 * DAY);
        assert_eq!(label(&quote), "ladder wait");

        quote.sends = sends(4, NOW - 7 * DAY);
        assert_eq!(label(&quote), "repeat_views, stale");

        let mut quote = facts("Q-2");
        quote.sends = sends(6, NOW - 10 * DAY);
        assert_eq!(label(&quote), "exhausted");

        quote.sends = sends(6, NOW - 31 * DAY);
        assert_eq!(label(&quote), "hidden");

        let conn = memory_db();
        add_quote(&conn, "Q-2", PHONE, "open", None);
        for sent_at in sends(6, NOW - 31 * DAY) {
            add_message(&conn, "Q-2", PHONE, "sent", Some(sent_at), sent_at);
        }
        assert!(outcomes(&conn).is_empty());

        let mut quote = facts("Q-3");
        quote.created_at = NOW - 47 * HOUR;
        quote.views = BTreeSet::from([NOW - HOUR]);
        assert_eq!(label(&quote), "too young");

        let mut quote = facts("Q-4");
        quote.sends = sends(1, NOW - 48 * HOUR);
        quote.phone_last_sent = quote.last_sent();
        quote.views = BTreeSet::from([NOW - HOUR]);
        assert_eq!(label(&quote), "viewed_no_response");
    }

    #[test]
    fn rule_one_exemptions() {
        let replied = |sent: BTreeSet<i64>| {
            let mut quote = facts("Q-1");
            quote.sends = sent;
            quote.replies = BTreeSet::from([NOW - HOUR]);
            quote
        };
        let first_reason = |quote: &Facts| label(quote).split(", ").next().unwrap().to_string();

        // Exhausted, inside a ladder wait, past give-up.
        for sent in [sends(6, NOW - 10 * DAY), sends(2, NOW - 2 * DAY), sends(6, NOW - 31 * DAY)] {
            assert_eq!(first_reason(&replied(sent)), "unanswered_reply");
        }

        let mut quote = replied(BTreeSet::new());
        quote.created_at = NOW - 10 * HOUR;
        assert_eq!(first_reason(&quote), "unanswered_reply");

        // A reply skips the cooldown; the same quote without one does not.
        let mut quote = replied(sends(1, NOW - DAY));
        quote.phone_last_sent = quote.last_sent();
        assert_eq!(first_reason(&quote), "unanswered_reply");

        quote.replies.clear();
        assert_eq!(label(&quote), "cooldown");

        let mut quote = replied(sends(1, NOW - 5 * DAY));
        quote.phone_last_sent = Some(NOW - DAY);
        quote.phone_pending = true;
        assert_eq!(label(&quote), "pending message");

        quote.phone_pending = false;
        quote.rejected_at = Some(NOW - DAY);
        assert_eq!(label(&quote), "rejected");
    }

    #[test]
    fn boundaries() {
        // The view before the send keeps rule 6 out of the XL cases.
        let mut quote = facts("Q-1");
        quote.amount = 11_000.0;
        quote.sends = sends(1, NOW - 3 * DAY);
        quote.views = BTreeSet::from([NOW - 4 * DAY]);
        assert_eq!(label(&quote), "xl_quote_stale");

        quote.sends = sends(1, NOW - 3 * DAY + 1);
        assert_eq!(label(&quote), "no rule");

        quote.amount = 10_999.0;
        quote.sends = sends(1, NOW - 3 * DAY);
        assert_eq!(label(&quote), "no rule");

        let mut quote = facts("Q-2");
        quote.sends = sends(1, NOW - 3 * DAY);
        assert_eq!(label(&quote), "never_viewed");

        quote.sends = sends(1, NOW - 3 * DAY + 1);
        assert_eq!(label(&quote), "no rule");

        let mut quote = facts("Q-3");
        quote.sends = sends(1, NOW - 48 * HOUR + 1);
        quote.phone_last_sent = quote.last_sent();
        assert_eq!(label(&quote), "cooldown");

        let mut quote = facts("Q-4");
        quote.sends = sends(1, NOW - 5 * DAY);
        quote.views = BTreeSet::from([NOW - 72 * HOUR + 1]);
        assert_eq!(label(&quote), "viewed_no_response");

        let mut quote = facts("Q-5");
        quote.created_at = NOW - 48 * HOUR;
        assert_eq!(label(&quote), "never_contacted");

        // Rule 1 skips the age check, so only rule 5's own age clause keeps it out.
        quote.created_at = NOW - 48 * HOUR + 1;
        quote.replies = BTreeSet::from([NOW - HOUR]);
        assert_eq!(label(&quote), "unanswered_reply");

        for (count, wait) in (1..).zip([48 * HOUR, 72 * HOUR, 72 * HOUR, 7 * DAY, 14 * DAY]) {
            let mut quote = facts("Q-6");
            quote.sends = sends(count, NOW - wait + 1);
            assert_eq!(label(&quote), "ladder wait");

            quote.sends = sends(count, NOW - wait);
            assert_ne!(label(&quote), "ladder wait");
        }

        let mut quote = facts("Q-7");
        quote.sends = sends(6, NOW - 30 * DAY + 1);
        assert_eq!(label(&quote), "exhausted");

        quote.sends = sends(6, NOW - 30 * DAY);
        assert_eq!(label(&quote), "hidden");

        let mut quote = facts("Q-8");
        quote.sends = sends(1, NOW - 50 * HOUR);
        quote.replies = BTreeSet::from([NOW - 50 * HOUR]);
        quote.views = BTreeSet::from([NOW - 50 * HOUR]);
        assert_eq!(label(&quote), "no rule");

        // Only the later view is since the send, so rule 3 stays out.
        let mut quote = facts("Q-9");
        quote.sends = sends(1, NOW - 5 * DAY);
        quote.views = BTreeSet::from([NOW - 5 * DAY, NOW - 4 * DAY]);
        assert_eq!(label(&quote), "no rule");
    }

    #[test]
    fn phone_guards_across_quotes() {
        let never_contacted = ["Q-1: never_contacted, stale"];
        let cooldown = ["Q-1: cooldown"];

        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE, "open", Some(NOW - DAY));
        add_quote(&conn, "Q-2", PHONE, "open", None);
        assert_eq!(outcomes(&conn), ["Q-1: cooldown", "Q-2: cooldown"]);

        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE, "accepted", Some(NOW - DAY));
        add_quote(&conn, "Q-2", PHONE, "open", None);
        assert_eq!(outcomes(&conn), ["Q-1: not open", "Q-2: cooldown"]);

        for status in ["draft", "approved", "sending", "failed"] {
            let conn = memory_db();
            add_quote(&conn, "Q-1", PHONE, "open", None);
            add_quote(&conn, "Q-2", PHONE, "open", None);
            add_message(&conn, "Q-1", PHONE, status, None, NOW);
            assert_eq!(outcomes(&conn), ["Q-1: pending message", "Q-2: pending message"]);
        }

        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE, "open", None);
        add_message(&conn, "Q-1", PHONE, "sent", Some(NOW + DAY), NOW + DAY);
        assert_eq!(outcomes(&conn), cooldown);

        // Q-1 has moved to a new number; the text went to the old one, now Q-2's.
        let conn = memory_db();
        add_quote(&conn, "Q-1", "+15559999", "open", None);
        add_quote(&conn, "Q-2", PHONE, "open", None);
        add_message(&conn, "Q-1", PHONE, "sent", Some(NOW - DAY), NOW - DAY);
        assert_eq!(outcomes(&conn), ["Q-1: cooldown", "Q-2: cooldown"]);

        // Q-1 has moved to a new number; its pending text is still on the old one.
        let conn = memory_db();
        add_quote(&conn, "Q-1", "+15559999", "open", None);
        add_quote(&conn, "Q-2", PHONE, "open", None);
        add_message(&conn, "Q-1", PHONE, "draft", None, NOW);
        assert_eq!(outcomes(&conn), ["Q-1: pending message", "Q-2: pending message"]);

        // The newest send on a phone wins, across quotes and across sent rows.
        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE, "open", Some(NOW - 5 * DAY));
        add_quote(&conn, "Q-2", PHONE, "open", Some(NOW - DAY));
        assert_eq!(outcomes(&conn), ["Q-1: cooldown", "Q-2: cooldown"]);

        let conn = memory_db();
        add_quote(&conn, "Q-1", "+15559999", "open", None);
        add_quote(&conn, "Q-2", PHONE, "open", None);
        add_message(&conn, "Q-1", PHONE, "sent", Some(NOW - 5 * DAY), NOW - 5 * DAY);
        add_message(&conn, "Q-1", PHONE, "sent", Some(NOW - DAY), NOW - DAY);
        assert_eq!(outcomes(&conn), ["Q-1: cooldown", "Q-2: cooldown"]);

        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE, "open", None);
        add_message(&conn, "Q-1", PHONE, "rejected", None, NOW - HOUR);
        assert_eq!(outcomes(&conn), ["Q-1: rejected"]);
        add_message(&conn, "Q-1", PHONE, "rejected", None, NOW - 30 * DAY);
        assert_eq!(outcomes(&conn), ["Q-1: rejected"]);

        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE, "open", Some(NOW + DAY));
        add_event(&conn, "message_sent", "Q-1", NOW + HOUR, "{}");
        assert_eq!(outcomes(&conn), never_contacted);

        add_event(&conn, "message_sent", "Q-404", NOW - DAY, "{}");
        assert_eq!(outcomes(&conn), never_contacted);

        add_event(&conn, "message_sent", "Q-1", NOW - DAY, r#"{"direction":"inbound"}"#);
        add_event(&conn, "message_sent", "Q-1", NOW - DAY, r#"{"direction":"Inbound"}"#);
        add_event(&conn, "message_sent", "Q-1", NOW - DAY, r#"{"direction":"INBOUND"}"#);
        assert_eq!(outcomes(&conn), never_contacted);

        add_event(&conn, "message_sent", "Q-1", NOW - DAY, "{}");
        assert_eq!(outcomes(&conn), cooldown);

        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE, "open", None);
        add_event(&conn, "message_sent", "Q-1", NOW - DAY, r#"{"direction":1}"#);
        assert_eq!(outcomes(&conn), cooldown);

        // Exactly at now counts, for both sources.
        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE, "open", Some(NOW));
        assert_eq!(outcomes(&conn), cooldown);

        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE, "open", None);
        add_event(&conn, "message_sent", "Q-1", NOW, "{}");
        assert_eq!(outcomes(&conn), cooldown);

        // Two event ids at one timestamp are one view, so rule 3 stays out.
        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE, "open", None);
        add_event(&conn, "quote_viewed", "Q-1", NOW - 4 * DAY, "{}");
        add_event(&conn, "quote_viewed", "Q-1", NOW - 4 * DAY, "{}");
        assert_eq!(outcomes(&conn), never_contacted);
    }
}
