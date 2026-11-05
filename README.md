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

- **Always apply:** quote not open, pending message for the phone, or quote rejected within 48 hours.
- **Applies to everything except rule 1:** phone contacted within 48 hours on any quote.
- **Skipped for rule 1:** minimum age, ladder wait, follow-up cap and give-up, because the customer reached out.

**Day one is a backlog.** Nearly every open quote qualifies on the first run, so the ranking matters most. At the demo now 25 of the 27 open quotes qualify. One loses the per-phone collapse, and one matches no rule while in its ladder wait.

**Considered, not built:** view time of day, reply channel, quote expiry, tech capacity, customer history, job type and seasonality.

## Guardrails

The cooldown, closed-quote and reply guardrails are checked when the policy picks candidates and again by `send`, against the database as it is at that moment.

- **One text per customer per 48 hours.** "Last sent" to a phone is the newest of the seed's `last_contact_at`, outbound `message_sent` events and our own sent messages, across every quote on that number. A phone inside the window is never a candidate, and `send` cancels an approved message if a send to that phone has appeared since, or if the quote's number has changed since it was drafted. The one exception is answering a customer's reply: the cooldown limits outreach the shop starts, and a person writing back is not outreach. The reply goes through the must-edit path, and once our send answers it the rule stops matching, so it cannot loop.
- **Never about a closed quote.** A quote that is accepted or dismissed in `quotes.json`, or has a `quote_accepted` event, is never a candidate. `ingest` cancels its pending messages, and `send` checks the status again before each message.
- **Never a double send.** Drafting again creates nothing while a message is pending; the database allows one pending message per phone. `send` moves each message from `approved` to `sending` with a compare-and-swap, so two runs cannot both send it, then calls the provider with the idempotency key stored on the message. A failed send is retried with the same key: the fake provider returns the original id, so a send whose acknowledgement was lost still ends as one delivered text. A message stuck in `sending` is never retried by the engine. Closing its quote cancels it; otherwise clearing one takes a database edit.
- **A reply is never answered by a template.** A customer whose last reply is after our last send gets an `unanswered_reply` draft: a placeholder that `approve` refuses without `--body` and that `approve --all` skips. If a reply on any open quote on that phone is later than our last send on that quote, `send` cancels the message rather than send a nudge over theirs.

**Possibly delivered texts.** `send` stamps a message before calling the provider. If that message is later cancelled, the stamp still counts for the phone's cooldown, because the text may have arrived. A customer's reply can still be answered. It does not count as a follow-up on the ladder. A text that never arrived costs a 48-hour wait, and that is the safer mistake.

`send` trusts `quotes.json` as ingested: a later export that omits a quote leaves it open. An earlier `--now` is replay, so it hides later events.

**In production.** Twilio, or any provider, sits behind the same `Sender` trait. If the provider ignores client keys, a failed send is reconciled against the provider's log before any retry, never resent blind. The opt-out footer and quiet hours live in the sender, so no policy change can skip them.

## Data model

SQLite, with all timestamps stored as integer UTC seconds.

| Table | Holds | Key rule |
|---|---|---|
| quotes | One row per quote, from `quotes.json` | Status is open, accepted or dismissed |
| events | Every unique event, with its raw line | `event_id` is unique, so duplicates are ignored. Unknown types are kept |
| messages | Every draft and send | One row per message, tracked by status |
| provider_outbox | What the fake sender delivered | Unique idempotency key, so a retry cannot send twice |

**Message lifecycle.** A message starts as `draft`, moves to `approved`, then `sending`, and ends as `sent` or `failed`. A failed message retries with the same idempotency key. `rejected` and `cancelled` are the other end states. Each status change only applies if the row is still in the expected state, so two runs cannot both send it.

**One pending message per phone.** Draft, approved, sending and failed all count as pending. The database enforces the limit with a partial unique index, so a bug in the code still cannot queue two texts to one phone number.

**Last contact is derived.** The seed's `last_contact_at` and `message_sent` events never agree, so neither is trusted alone. "Last sent" is the latest of those two and our own sent messages, and "sends" counts their distinct timestamps. Both are computed on read, so a new event corrects them automatically.

## Decisions and assumptions

- **Status source of truth:** `quotes.json` is authoritative. A `quote_accepted` event promotes an open quote to accepted, never the reverse. Status is checked again at send time.
- **Duplicates and ordering:** events dedupe on `event_id`, and nothing depends on file order. Ingesting the same data twice, or in a different order, gives the same stored rows.
- **Bad input:** unparseable lines and missing fields are skipped and counted, never fatal. Events for unknown quotes are stored.
- **Bad re-export:** if a later file has an invalid row for a stored quote, such as `"status": "Accepted"`, the row is skipped and the stored status stands. A closed quote could therefore stay open until a valid file arrives. The skip count and a stderr line show it.
- **Promotion count:** `promoted` repeats on each re-ingest for a quote the file lists as open but that has an accepted event. The stored status stays correct.
- **`quote_sent` events:** this type is not in the spec. It is stored but does not count as "we sent", because every seed quote has one and `never_contacted` could then never match.
- **Now:** wall clock by default, with `--now` to override, and every command prints the time it used. The demo uses `2026-08-17T13:00:00Z`, 9am in New York. An earlier `--now` sees only events and `last_contact_at` up to that time, but our own sends always count, so a replay cannot forget a real send. Quote status is not time-aware.
- **Closing a quote cancels stuck sends:** ingest cancels every pending message on a closed quote, including one stuck in `sending`.
- **Give up is display only:** nothing is written, and the quote stays open.
- **Templates:** the shop name is a constant, and messages never quote a dollar amount.
- **Drafts:** one per candidate. A rerun adds nothing for a phone with a pending message, because the policy suppresses it and the one-pending-per-phone index backs that up. A rejected, sent or cancelled message can lead to a new draft once what stopped it clears. The idempotency key is the quote plus the now the policy ran at. The quote date in a text uses a fixed New York offset, so a late-night winter quote can show the next day. A blank customer or tech name renders as "there" and "our team" rather than a broken greeting. The greeting uses the first word of the name, so a title or a surname-first name needs a human eye.
- **Phones:** numbers are trimmed but not normalized, and every phone guardrail compares them as text. `+19175550000` and `(917) 555-0000` would count as two customers, so an import must use one format. The seed does.

## Running this for a parent company with 50 shops

Every shop gets its own settings and approval queue, all running on the same engine. Shops can tune their wait times and attempt limits, but the parent company sets the limits nobody can override, like opt-outs, quiet hours and a cap on messages per customer. All quotes and events flow into one central data store, which shows which follow-ups actually win jobs and lets us improve the defaults for everyone. If one shop's feed breaks, it shouldn't touch the other 49.

The harder problem is people. Fifty owners won't all check their drafts every day, so each shop needs someone responsible for the queue, and drafts should expire instead of going out late. I'd start with two or three shops and keep human approval on everywhere. Once the data shows which messages rarely need editing, those could start sending automatically.

## What I'd build next

**Before any real customer gets a text:**

- An opt-out footer and handling for STOP replies
- Quiet hours in the customer's time zone
- A retry cap on failed sends
- Outcome tracking, to see which reasons and wait times lead to accepted quotes

**Then, in priority order:**

1. An interactive review loop: yes, no or edit for each draft, in the terminal first and later on a simple localhost page
2. A central data store for all quotes and events, to find better ways to prioritize clients, plus an admin portal for one person running several shops
3. Automated orchestration, possibly an LLM that tailors, audits and approves messages
4. A review pass to simplify the code
5. Showing the customer's reply next to must-edit drafts
6. More channels: email, then WhatsApp, Facebook Messenger and Discord
7. Per-shop policy: wait times, attempt limits, give-up period and time zone
8. Research into the best timing and wording, by type of trade
9. Dynamic and multiple quote thresholds, so "big" scales with the business
10. Auto-tuning the policy from each shop's own results

## Where I stopped

_TODO_
