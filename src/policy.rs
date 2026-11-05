use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};

pub const HOUR: i64 = 3600;
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

pub fn names(reasons: &[Reason]) -> String {
    let names: Vec<&str> = reasons.iter().map(Reason::as_str).collect();
    names.join(", ")
}

/// One quote as of a given now.
#[derive(Default)]
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
    /// Newest send or stamped cancelled attempt to this phone, on any quote of any status.
    pub phone_last_sent: Option<i64>,
    /// When this quote's newest rejected message was rejected.
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

    /// `None` orders below `Some`, so a reply on a quote we never sent on counts.
    pub fn unanswered_reply(&self) -> bool {
        self.replies.last() > self.sends.last()
    }
}

pub fn load_facts(conn: &Connection, now: i64) -> rusqlite::Result<BTreeMap<String, Facts>> {
    let mut facts = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT id, customer_name, customer_phone, tech_name, amount, status = 'open', created_at,
                last_contact_at,
                (SELECT MAX(rejected_at) FROM messages WHERE quote_id = quotes.id)
         FROM quotes",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let last_contact_at: Option<i64> = row.get(7)?;
        let quote = Facts {
            id: row.get(0)?,
            customer_name: row.get(1)?,
            phone: row.get(2)?,
            tech_name: row.get(3)?,
            amount: row.get(4)?,
            open: row.get(5)?,
            created_at: row.get(6)?,
            sends: last_contact_at.into_iter().filter(|ts| *ts <= now).collect(),
            rejected_at: row.get(8)?,
            ..Facts::default()
        };
        facts.insert(quote.id.clone(), quote);
    }

    // Comparing in SQL means a non-text direction cannot fail the row read.
    let mut stmt = conn.prepare(
        "SELECT quote_id, type, ts FROM events
         WHERE ts <= ?1
           AND NOT (type = 'message_sent'
                    AND lower(json_extract(raw, '$.direction')) IS 'inbound')",
    )?;
    let mut rows = stmt.query([now])?;
    while let Some(row) = rows.next()? {
        let quote_id: String = row.get(0)?;
        let event_type: String = row.get(1)?;
        let Some(quote) = facts.get_mut(&quote_id) else { continue };
        let timestamps = match event_type.as_str() {
            "quote_viewed" => &mut quote.views,
            "customer_replied" => &mut quote.replies,
            "message_sent" => &mut quote.sends,
            _ => continue,
        };
        timestamps.insert(row.get(2)?);
    }

    let mut pending_phones = BTreeSet::new();
    let mut phone_sends: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
    let mut stmt = conn.prepare("SELECT quote_id, customer_phone, status, sent_at FROM messages")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let quote_id: String = row.get(0)?;
        let phone: String = row.get(1)?;
        let status: String = row.get(2)?;
        let sent_at: Option<i64> = row.get(3)?;
        let quote = facts.get_mut(&quote_id);
        match status.as_str() {
            "sent" => {
                if let Some(quote) = quote {
                    quote.sends.extend(sent_at);
                }
                // Keyed by the row's phone: a re-export may have moved the quote off it.
                phone_sends.entry(phone).or_default().extend(sent_at);
            }
            // A stamped row was attempted and may have arrived: the phone waits, the ladder doesn't.
            "cancelled" => phone_sends.entry(phone).or_default().extend(sent_at),
            "draft" | "approved" | "sending" | "failed" => {
                pending_phones.insert(phone);
                // A quote that moved numbers is still blocked by its own pending message.
                if let Some(quote) = quote {
                    pending_phones.insert(quote.phone.clone());
                }
            }
            _ => {}
        }
    }

    // Closed quotes count too: the cooldown protects the customer, not the quote.
    for quote in facts.values() {
        phone_sends.entry(quote.phone.clone()).or_default().extend(quote.last_sent());
    }
    for quote in facts.values_mut() {
        quote.phone_pending = pending_phones.contains(&quote.phone);
        quote.phone_last_sent = phone_sends[&quote.phone].last().copied();
    }
    Ok(facts)
}

fn matching_rules(policy: &Policy, facts: &Facts, now: i64) -> Vec<Reason> {
    let last_sent = facts.last_sent();
    let last_view = facts.views.last().copied();
    let waited = now - facts.since_sent();
    // `None` orders below `Some`, so these comparisons also cover "never sent".
    let views_since = facts.views.iter().filter(|view| Some(**view) > last_sent).count();
    let recent_view = last_view.is_some_and(|view| now - view < policy.view_window);

    [
        (facts.unanswered_reply(), Reason::UnansweredReply),
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

/// The matching rules, or why the quote is suppressed. "hidden" keeps it off both lists.
fn decide(policy: &Policy, facts: &Facts, now: i64) -> Result<Vec<Reason>, &'static str> {
    if !facts.open {
        return Err("not open");
    }
    if facts.phone_pending {
        return Err("pending message");
    }
    let reasons = matching_rules(policy, facts, now);
    let replied = facts.unanswered_reply();
    if !replied && policy.in_cooldown(facts.phone_last_sent, now) {
        return Err("cooldown");
    }
    if policy.in_cooldown(facts.rejected_at, now) {
        return Err("rejected");
    }

    if !replied {
        let sends = facts.sends.len();
        let waited = now - facts.since_sent();
        if now - facts.created_at < policy.min_age {
            return Err("too young");
        }
        if sends >= policy.max_sends {
            return Err(if waited < policy.give_up { "exhausted" } else { "hidden" });
        }
        if sends > 0 && policy.ladder.get(sends - 1).is_some_and(|wait| waited < *wait) {
            return Err("ladder wait");
        }
    }
    if reasons.is_empty() {
        return Err("no rule");
    }
    Ok(reasons)
}

pub type Ranked = Vec<(Facts, Vec<Reason>)>;
pub type Suppressed = Vec<(Facts, String)>;

/// Candidates in rank order, and every suppressed quote by id.
pub fn candidates(
    conn: &Connection,
    policy: &Policy,
    now: i64,
) -> rusqlite::Result<(Ranked, Suppressed)> {
    let mut matched = Vec::new();
    let mut suppressed = Vec::new();
    for quote in load_facts(conn, now)?.into_values() {
        match decide(policy, &quote, now) {
            Ok(reasons) => matched.push((quote, reasons)),
            Err("hidden") => {}
            Err(label) => suppressed.push((quote, label.to_string())),
        }
    }
    matched.sort_by(|(a, a_reasons), (b, b_reasons)| {
        a_reasons[0]
            .cmp(&b_reasons[0])
            .then(b.amount.total_cmp(&a.amount))
            .then(a.since_sent().cmp(&b.since_sent()))
    });

    let mut winners: BTreeMap<String, String> = BTreeMap::new();
    let mut ranked = Vec::new();
    for (quote, reasons) in matched {
        if let Some(winner) = winners.get(&quote.phone) {
            suppressed.push((quote, format!("same phone as {winner}")));
        } else {
            winners.insert(quote.phone.clone(), quote.id.clone());
            ranked.push((quote, reasons));
        }
    }
    suppressed.sort_by(|(a, _), (b, _)| a.id.cmp(&b.id));
    Ok((ranked, suppressed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::ingest;
    use crate::testutil::*;
    use rusqlite::params;
    use std::fs;

    fn facts(id: &str) -> Facts {
        Facts {
            id: id.to_string(),
            customer_name: "Ann".to_string(),
            phone: format!("+1{id}"),
            tech_name: "Brad".to_string(),
            amount: 1000.0,
            open: true,
            created_at: NOW - 10 * DAY,
            ..Facts::default()
        }
    }

    /// `count` sends a day apart, ending at `last`.
    fn sends(count: i64, last: i64) -> BTreeSet<i64> {
        (0..count).map(|i| last - i * DAY).collect()
    }

    fn label(quote: &Facts) -> String {
        decide(&Policy::default(), quote, NOW).map_or_else(str::to_string, |rules| names(&rules))
    }

    fn add_message(
        conn: &Connection,
        quote_id: &str,
        phone: &str,
        status: &str,
        at: Option<i64>,
    ) {
        conn.execute(
            "INSERT INTO messages (quote_id, customer_phone, reason, body, status,
                                   idempotency_key, created_at, sent_at, rejected_at)
             VALUES (?1, ?2, 'test', 'hi', ?3, hex(randomblob(8)), 0, ?4, ?4)",
            params![quote_id, phone, status, at],
        )
        .unwrap();
    }

    #[test]
    fn golden_seed_at_demo_now() {
        let seed = seed();
        let mut conn = memory_db();
        ingest(&mut conn, &seed).unwrap();
        assert_eq!(labels(&conn, NOW), GOLDEN);

        let loaded = load_facts(&conn, NOW).unwrap();
        assert_eq!(loaded["Q-1012"].views.len(), 6);
        assert_eq!(loaded["Q-1002"].sends.len(), 2);

        let text = fs::read_to_string(seed.join("events.jsonl")).unwrap();
        let reversed: Vec<&str> = text.lines().rev().collect();
        let dir = temp_dir("reversed");
        let copy = dir.join("events.jsonl");
        fs::write(&copy, reversed.join("\n")).unwrap();

        let mut fresh = memory_db();
        ingest(&mut fresh, &seed.join("quotes.json")).unwrap();
        ingest(&mut fresh, &copy).unwrap();
        assert_eq!(labels(&fresh, NOW), GOLDEN);
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
        add_quote(&conn, "Q-2", PHONE);
        for sent_at in sends(6, NOW - 31 * DAY) {
            add_message(&conn, "Q-2", PHONE, "sent", Some(sent_at));
        }
        assert!(labels(&conn, NOW).is_empty());

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

        // Only a `message_sent` is dropped for being inbound; a reply never is.
        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        add_raw_event(&conn, "customer_replied", "Q-1", NOW - DAY, r#"{"direction":"inbound"}"#);
        assert_eq!(labels(&conn, NOW), ["Q-1: unanswered_reply, never_contacted, stale"]);

        let conn = memory_db();
        add_quote_with(&conn, "Q-1", PHONE, "open", Some(NOW - DAY));
        add_quote(&conn, "Q-2", PHONE);
        assert_eq!(labels(&conn, NOW), ["Q-1: cooldown", "Q-2: cooldown"]);

        let conn = memory_db();
        add_quote_with(&conn, "Q-1", PHONE, "accepted", Some(NOW - DAY));
        add_quote(&conn, "Q-2", PHONE);
        assert_eq!(labels(&conn, NOW), ["Q-1: not open", "Q-2: cooldown"]);

        for status in ["draft", "approved", "sending", "failed"] {
            let conn = memory_db();
            add_quote(&conn, "Q-1", PHONE);
            add_quote(&conn, "Q-2", PHONE);
            add_message(&conn, "Q-1", PHONE, status, None);
            assert_eq!(labels(&conn, NOW), ["Q-1: pending message", "Q-2: pending message"]);
        }

        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        add_message(&conn, "Q-1", PHONE, "sent", Some(NOW + DAY));
        assert_eq!(labels(&conn, NOW), cooldown);

        // Q-1 has moved to a new number; the text went to the old one, now Q-2's.
        let conn = memory_db();
        add_quote(&conn, "Q-1", "+15559999");
        add_quote(&conn, "Q-2", PHONE);
        add_message(&conn, "Q-1", PHONE, "sent", Some(NOW - DAY));
        assert_eq!(labels(&conn, NOW), ["Q-1: cooldown", "Q-2: cooldown"]);

        // Q-1 has moved to a new number; its pending text is still on the old one.
        let conn = memory_db();
        add_quote(&conn, "Q-1", "+15559999");
        add_quote(&conn, "Q-2", PHONE);
        add_message(&conn, "Q-1", PHONE, "draft", None);
        assert_eq!(labels(&conn, NOW), ["Q-1: pending message", "Q-2: pending message"]);

        // The newest send on a phone wins, across quotes and across sent rows.
        let conn = memory_db();
        add_quote_with(&conn, "Q-1", PHONE, "open", Some(NOW - 5 * DAY));
        add_quote_with(&conn, "Q-2", PHONE, "open", Some(NOW - DAY));
        assert_eq!(labels(&conn, NOW), ["Q-1: cooldown", "Q-2: cooldown"]);

        let conn = memory_db();
        add_quote(&conn, "Q-1", "+15559999");
        add_quote(&conn, "Q-2", PHONE);
        add_message(&conn, "Q-1", PHONE, "sent", Some(NOW - 5 * DAY));
        add_message(&conn, "Q-1", PHONE, "sent", Some(NOW - DAY));
        assert_eq!(labels(&conn, NOW), ["Q-1: cooldown", "Q-2: cooldown"]);

        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        add_message(&conn, "Q-1", PHONE, "rejected", Some(NOW - HOUR));
        assert_eq!(labels(&conn, NOW), ["Q-1: rejected"]);
        add_message(&conn, "Q-1", PHONE, "rejected", Some(NOW - 30 * DAY));
        assert_eq!(labels(&conn, NOW), ["Q-1: rejected"]);

        // A reject holds back its own quote, not every quote.
        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        add_quote(&conn, "Q-2", "+15550001");
        add_message(&conn, "Q-1", PHONE, "rejected", Some(NOW - HOUR));
        assert_eq!(labels(&conn, NOW), ["Q-2: never_contacted, stale", "Q-1: rejected"]);

        let conn = memory_db();
        add_quote_with(&conn, "Q-1", PHONE, "open", Some(NOW + DAY));
        add_event(&conn, "message_sent", "Q-1", NOW + HOUR);
        assert_eq!(labels(&conn, NOW), never_contacted);

        add_event(&conn, "message_sent", "Q-404", NOW - DAY);
        assert_eq!(labels(&conn, NOW), never_contacted);

        add_raw_event(&conn, "message_sent", "Q-1", NOW - DAY, r#"{"direction":"inbound"}"#);
        add_raw_event(&conn, "message_sent", "Q-1", NOW - DAY, r#"{"direction":"Inbound"}"#);
        add_raw_event(&conn, "message_sent", "Q-1", NOW - DAY, r#"{"direction":"INBOUND"}"#);
        assert_eq!(labels(&conn, NOW), never_contacted);

        add_event(&conn, "message_sent", "Q-1", NOW - DAY);
        assert_eq!(labels(&conn, NOW), cooldown);

        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        add_raw_event(&conn, "message_sent", "Q-1", NOW - DAY, r#"{"direction":1}"#);
        assert_eq!(labels(&conn, NOW), cooldown);

        // Exactly at now counts, for both sources.
        let conn = memory_db();
        add_quote_with(&conn, "Q-1", PHONE, "open", Some(NOW));
        assert_eq!(labels(&conn, NOW), cooldown);

        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        add_event(&conn, "message_sent", "Q-1", NOW);
        assert_eq!(labels(&conn, NOW), cooldown);

        // Two event ids at one timestamp are one view, so rule 3 stays out.
        let conn = memory_db();
        add_quote(&conn, "Q-1", PHONE);
        add_event(&conn, "quote_viewed", "Q-1", NOW - 4 * DAY);
        add_event(&conn, "quote_viewed", "Q-1", NOW - 4 * DAY);
        assert_eq!(labels(&conn, NOW), never_contacted);
    }
}
