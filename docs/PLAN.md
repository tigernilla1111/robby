# Plan: Robby follow-up engine

High level. Build strictly top to bottom; the hour ends wherever it ends and the README says where. Phases 0 and 1 build as written. Phases 2 to 4 get detailed before they are built.

## Constraints and decisions

- Rust CLI, single binary, SQLite via rusqlite (bundled). No web UI in the MVP.
- Graded: correctness on messy input, guardrails, quote/message state model, README judgment. Not graded: UI, deployment, auth, framework.
- Timestamps stored as integer UTC seconds. `--now` on every command, wall clock default, accepts with or without seconds. First output line prints the now used. Warn if the newest ingested event is more than 30 days before now. Demo now is `2026-08-17T13:00:00Z`, 9am New York.
- An earlier `--now` sees only events and `last_contact_at` at or before now. Our own sent rows always count, so replay can never forget a real send. Quote status is not time-aware, so a promotion from a later event still shows.
- quotes.json status is authoritative. A `quote_accepted` event promotes open to accepted, never the reverse. Status is rechecked at send time.
- "Last sent" per quote is derived: max of `last_contact_at`, outbound `message_sent` events, and our own sent rows. "Sends" is the count of distinct outbound timestamps across those sources. When nothing was ever sent, "nothing sent for N days" counts from `created_at`.
- Policy is an ordered rule list. Rules are matched first, then suppressions apply. The candidate list prints every matching rule; the draft uses the first. No scoring.
- Hard suppressions always apply: not open, pending message for the phone, phone within cooldown. Soft suppressions skip rule 1 because the customer reached out: minimum age, ladder wait, follow-up cap, give-up.
- Pending means draft, approved, sending, or failed. One pending message per customer phone, enforced by a partial unique index. When email lands, the index and the cooldown check extend to the email address too. The ladder stays per quote.
- Give up is display only: an exhausted quote stays in the suppressed list; GIVE_UP_DAYS after send 6 it is hidden. Nothing is written.
- `reject` suppresses the quote for one cooldown window so the next run does not re-draft it.
- Shop name is a constant. Templates have no dollar amount.

Knobs live in one `Policy` struct with defaults. Tests build their own. No CLI overrides in the MVP.

| Knob | Default |
|---|---|
| cooldown per customer phone | 48h |
| minimum quote age before first nudge | 48h |
| XL quote | $14,000 and up |
| recent view window | 72h |
| max follow-ups per quote | 6 |
| nudge ladder, wait after send 1..5 | 48h, 72h, 72h, 7d, 14d |
| GIVE_UP_DAYS, hide after send 6 | 30 |

## Phase 0: Scaffold (MVP)

- cargo project, clap subcommands, `--db` path, `--now`.
- Schema on startup: `quotes`, `events` (id, type, quote_id, ts, raw line), `messages` (quote, phone, channel, reason, body, must_edit, status, policy_now, idempotency_key, provider id, error, timestamps), `provider_outbox` for the fake sender with `UNIQUE(idempotency_key)`, partial unique index on `customer_phone` where status is pending.
- `messages.status`: draft, approved, sending, sent, failed, rejected, cancelled.

## Phase 1: Ingest (MVP)

- `ingest <dir|file>`: upsert quotes; `INSERT OR IGNORE` events by `event_id`; the raw line is stored and channel, direction, body are read from it when needed. Unknown types kept. Events for unknown quotes stored.
- Unparseable lines or missing fields are skipped and counted, never fatal.
- Promotion is one UPDATE over all stored `quote_accepted` events after each upsert. Pending messages on any non-open quote are cancelled.
- Print counts: stored, duplicates ignored, skipped, promoted, cancelled.
- Re-ingest never moves `last_contact_at` backward, so a stale export cannot erase a contact and reopen the cooldown.
- Tests: ingest the seed twice, the second time shuffled: 82 events, 6 duplicates, identical stored rows. Phase 2's golden test adds the same check on candidate output. One fixture: open quote plus `quote_accepted`, re-ingested, stays accepted.

## Phase 2: Follow-up policy (MVP)

- `candidates --now`: derive per-quote facts, match rules, apply suppressions, sort, collapse to one candidate per phone, print the ranked list and the suppressed list with reasons.
- Suppressions, each named: not open; pending message for the phone; phone contacted within cooldown on any quote; rejected within cooldown; younger than minimum age; ladder wait not elapsed; exhausted (sends at max, within GIVE_UP_DAYS); given up (hidden). The last four do not apply to rule 1.
- Rules, in order. "We sent" is any outbound contact, and `quote_sent` events do not count; "they" is the customer. Views and replies count distinct timestamps.

| # | Reason | Rule |
|---|---|---|
| 1 | unanswered_reply | They replied and we have sent nothing since |
| 2 | xl_quote_stale | XL amount and nothing sent for 3+ days |
| 3 | repeat_views | They viewed 2+ times since we last sent and never replied |
| 4 | viewed_no_response | They viewed within the recent view window and nothing sent since that view |
| 5 | never_contacted | Never sent anything, quote past minimum age |
| 6 | never_viewed | Never viewed, sent 3+ days ago |
| 7 | stale | Nothing sent for 7+ days |

- Sort within a rule: amount descending, then longest since we sent.
- Tests: one golden test of the seed at the demo now, expected reason or suppression per quote. It covers precedence, Karen Nguyen's collapse, closed quotes, and the ladder. Synthetic fixtures only for viewed_no_response, exhausted, rule 1 on an exhausted quote, and cooldown across two quotes. If time runs short, cut these before any Phase 4 test.
- Other signals go in the README only: view time of day, reply channel, quote expiry, tech capacity, prior customer history, job type, seasonality.

## Phase 3: Message drafting (MVP)

- `draft --now`: one `INSERT OR IGNORE` per candidate. Rerun inserts nothing. A second quote on a phone with a pending message is skipped and listed.
- One SMS template per reason. Placeholders: first name, tech, quote date.
- `unanswered_reply` gets a placeholder body and `must_edit = 1`.
- Each row stores `reason`, `policy_now`, and a generated `idempotency_key`.

## Phase 4: Approve and send (MVP)

- `approve <id> [--body]` or `approve --all`, which skips must-edit drafts and lists them. A must-edit draft is refused without `--body`. `reject <id>`.
- `send --now`: for each approved or failed row, recheck quote status, phone cooldown, and whether the customer replied after the draft was made (cancels unless must-edit). A failed recheck cancels the row with the reason. Then compare-and-swap to sending, call the `Sender`, mark sent with provider id or failed with error. Retries reuse the stored key. Rows stuck in sending are never auto-retried; `outbox` flags them. If the quote closes first, ingest cancels the row like any pending message, so a possibly delivered text stops counting as sent. That trade-off is accepted.
- `Sender` trait. `FakeSender` writes `provider_outbox`; a repeated key returns the stored provider id. `--fail <quote>` makes it store the message and then error, which is the lost-ack case.
- Tests: draft twice, one row; second quote on the same phone not drafted while the first is pending; approve must-edit without body refused; send twice, one provider row; stored-then-error retry, same key, one provider row; accepted between approve and send, cancelled; reply after draft, cancelled.
- README production note: Twilio behind the same trait; if the provider ignores client keys, ambiguous failures are reconciled against the provider, not resent; opt-out footer and quiet hours live in the sender.

## Phase 5: Run and display (MVP)

- `outbox`: table of messages with status, reason, timestamps.
- `run --now`: ingest, candidates, draft in one command.
- README demo script exercises every guardrail: `run`, `approve --all`, `send`, `send` again (no-op), `run` again (cooldown suppressions, no new drafts), `send --fail` then `send`.

## README checkpoints

- After Phase 2: policy and why, data model, decisions. Note that day one is a backlog, nearly every open quote qualifies, and the ranking is what matters. Take the exact count from Phase 2's golden test.
- After Phase 4: guardrails.
- Last 10 minutes, wherever the hour ends: how to run, where I stopped, what next, 50 shops. Export every conversation to `docs/conversations/`.
- "What I'd build next" leads with production guardrails: opt-out footer, quiet hours, retry cap, outcome tracking. Then the build list below in its order.

## After MVP, in priority order

1. Interactive review loop: yes, no, edit per draft. Medium-high.
2. Central cloud data store: record all quotes and events in one place, then use that data to find new patterns and improve existing ones, so clients are prioritized better over time. May also need an admin portal that can act on any shop, for one person managing several shops. Medium-high.
3. Automated orchestration: a more sophisticated process to run everything end to end. Possibly an LLM that tailors each message, audits it, and approves it for sending. Medium-high.
4. Implementation review pass: go over the code, simplify it, and remove anything unnecessary. Most of the owner's time went to the plan and little to reviewing the code, so treat none of it as reviewed. Runs before the features below so they land on simpler code. Medium.
5. Reply text: show "Customer said: ..." next to a must-edit draft when the event carries a body. Medium.
6. Email and other channels: pick email when the customer's last reply was email; templates per channel; pending index and cooldown check extended to email address. Then other providers such as WhatsApp, Facebook Messenger and Discord behind the same `Sender` trait. Medium.
7. Per-shop policy. Three choices a shop makes: wait between messages to the same customer, as `Constant(x days)` or `Exponential(x days)` doubling from x; max attempts, 0 to 100; give-up period, since eager shops wait longer. The MVP ladder stays as the default mode. Question for Robby: should there be a hard global cap so a shop cannot message a customer indefinitely? Medium.
8. Follow-up research: find the most effective marketing techniques, covering the best waiting periods, the wording of a good follow-up message, and specialized values for the type of contract work a shop does. Medium.
9. Outcome tracking: which reason and ladder step preceded each acceptance, to tune the ladder. Low-medium.
10. `send_attempts` table with a retry cap. Low.
11. Thin localhost page listing candidates and drafts with approve buttons. Low.
12. Dynamic XL threshold as the top 10% of open quotes by amount, as a policy mode. Fixed thresholds stop discriminating once most deals clear $10k, ignore inflation, and percentages are an easy strategy lever. Low.
13. Multiple quote thresholds: replace the single money-based reason with several tiers a shop can define, as dollar amounts and as percentages once item 12 lands. Low.
14. Auto-tuning: use a shop's own outcomes to find the policy values that convert best, so the knobs in item 7 and the thresholds in item 12 are set empirically instead of by hand. Depends on item 9. Low.
