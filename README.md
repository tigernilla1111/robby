# Robby follow-up engine

Automated follow-up for open home-service quotes. For a given "now" it decides which customers to contact and why, drafts the message, and sends it only after a human approves.

## How to run

_TODO_

## Follow-up policy and why

Each open quote is checked against an ordered list of rules. Every matching rule is shown, and the draft uses the first. There is no scoring, so a shop owner can read exactly why a customer is on the list.

| # | Reason | Rule | Why it sits here |
|---|---|---|---|
| 1 | unanswered_reply | The customer replied and we have sent nothing since | They reached out, so leaving them waiting is the most costly miss |
| 2 | xl_quote_stale | $14,000 or more, nothing sent for 3+ days | The most revenue at stake from a single quote |
| 3 | repeat_views | The customer viewed 2+ times since we last sent and never replied | Coming back to the quote is the strongest sign of interest without a reply |
| 4 | viewed_no_response | The customer viewed within 72 hours and we have sent nothing since | Interest is recent |
| 5 | never_contacted | We never sent anything and the quote is past minimum age | Nobody has followed up at all |
| 6 | never_viewed | The customer never viewed, and we sent 3+ days ago | The quote may not have reached them |
| 7 | stale | Nothing sent for 7+ days | Catch-all so no open quote is forgotten |

Within a rule, the highest amount goes first, then the longest time since we sent. Each phone gets one candidate.

**Pacing.** Waits grow with each send: 48 hours, 72 hours, 72 hours, 7 days, then 14 days. A quote gets at most 6 follow-ups, stays visible as exhausted for 30 days, then is hidden. The first nudge waits until the quote is 48 hours old.

**Suppressions.** Each suppressed quote shows its reason.

- **Always apply:** quote not open, pending message for the phone, phone contacted within 48 hours on any quote, or quote rejected within that window.
- **Skipped for rule 1:** minimum age, ladder wait, follow-up cap and give-up, because the customer reached out.

**Day one is a backlog.** Nearly every open quote qualifies on the first run, so the ranking matters most. [exact count after Phase 2]

**Considered, not built:** view time of day, reply channel, quote expiry, tech capacity, customer history, job type and seasonality.

## Guardrails

_TODO: cooldown per customer phone, closed quotes, no double-send on rerun or retry, must-edit drafts for unanswered replies._

## Data model

SQLite, with all timestamps stored as integer UTC seconds.

| Table | Holds | Key rule |
|---|---|---|
| quotes | One row per quote, from `quotes.json` | Status is open, accepted or dismissed |
| events | Every unique event, with its raw line | `event_id` is unique, so duplicates are ignored. Unknown types are kept |
| messages | Every draft and send | One row per message, tracked by status |
| provider_outbox | What the fake sender delivered | Unique idempotency key, so a retry cannot send twice |

**Message lifecycle.** A message starts as `draft`, moves to `approved`, then `sending`, and ends as `sent` or `failed`. A failed message retries with the same idempotency key. `rejected` and `cancelled` are the other end states. Each status change only applies if the row is still in the expected state, so two runs cannot both send it.

**One pending message per phone.** Draft, approved, sending and failed all count as pending. The database enforces the limit with a partial unique index, so a bug in the code still cannot queue two texts to one customer.

**Last contact is derived.** The seed's `last_contact_at` and `message_sent` events never agree, so neither is trusted alone. "Last sent" is the latest of those two and our own sent messages, and "sends" counts their distinct timestamps. Both are computed on read, so a new event corrects them automatically.

## Decisions and assumptions

_TODO: status source of truth, handling of duplicate and out-of-order events, the extra quote_sent event type, what "now" defaults to._

## Running this for a parent company with 50 shops

_TODO: about two paragraphs, no code._

## What I'd build next

_TODO_

## Where I stopped

_TODO_
