mod db;
mod draft;
mod ingest;
mod messages;
mod policy;
#[cfg(test)]
mod testutil;
mod time;

use clap::{Parser, Subcommand};
use draft::Draft;
use ingest::Counts;
use messages::{Delivery, FakeSender, Message};
use policy::{DAY, Facts, Policy, Suppressed};
use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

/// Quote follow-up engine
#[derive(Parser)]
struct Cli {
    /// SQLite database file
    #[arg(long, global = true, default_value = "robby.db")]
    db: PathBuf,

    /// Current time, RFC 3339 or YYYY-MM-DDTHH:MMZ (default: wall clock)
    #[arg(long, global = true, value_parser = parse_now)]
    now: Option<i64>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Load a seed directory, a .json quotes file or a .jsonl events file
    Ingest { path: PathBuf },
    /// List who to follow up with, and every suppressed quote with its reason
    Candidates,
    /// Create one draft per candidate, skipping any phone with a pending message
    Draft,
    /// Approve one message, or with --all every draft that needs no edit
    Approve {
        #[arg(required_unless_present = "all")]
        id: Option<i64>,
        /// Approve every draft that needs no edit
        #[arg(long, conflicts_with = "id")]
        all: bool,
        /// Replace the text; a must-edit draft requires this
        #[arg(long, conflicts_with = "all", allow_hyphen_values = true)]
        body: Option<String>,
    },
    /// Reject a draft or approved message
    Reject { id: i64 },
    /// Recheck every approved or failed message, then send it
    Send {
        /// Lose the provider's ack for this quote, after it stored the text
        #[arg(long, value_name = "QUOTE")]
        fail: Option<String>,
    },
    /// Ingest a path, then draft
    Run {
        #[arg(default_value = "seed")]
        path: PathBuf,
    },
    /// List every message with its status, reason, timestamps, provider id and error
    Outbox,
}

fn parse_now(s: &str) -> Result<i64, &'static str> {
    time::parse_ts(s).ok_or("expected RFC 3339 or YYYY-MM-DDTHH:MMZ")
}

fn quote_line(quote: &Facts, label: &str, now: i64) -> String {
    let days = (now - quote.since_sent()) / DAY;
    let contact = match quote.sends.len() {
        0 => format!("never sent, created {days}d ago"),
        sends => format!("sent {sends}, last {days}d ago"),
    };
    format!(
        "{:<6}  {:<16}  {:>6}  {label:<31}  {contact}, views {}, replies {}",
        quote.id,
        quote.customer_name,
        format!("${:.0}", quote.amount),
        quote.views.len(),
        quote.replies.len()
    )
}

fn message_line(msg: &Message) -> String {
    format!("#{:<3} {:<7} {:<13} {}", msg.id, msg.quote_id, msg.phone, msg.reason)
}

fn outbox_line(msg: &Message) -> String {
    let attempted = msg.sent_at.map_or("-".to_string(), time::format_ts);
    let stuck = if msg.status == "sending" { "  STUCK" } else { "" };
    format!(
        "#{:<3} {:<7} {:<13} {:<9}  {:<18}  {}  {attempted:<20}  {:<11}  {}{stuck}",
        msg.id,
        msg.quote_id,
        msg.phone,
        msg.status,
        msg.reason,
        time::format_ts(msg.created_at),
        msg.provider_id.as_deref().unwrap_or("-"),
        msg.error.as_deref().unwrap_or("-")
    )
}

fn print_ingest(counts: &Counts) {
    println!(
        "quotes: {} stored, {} skipped",
        counts.quotes_stored, counts.quotes_skipped
    );
    println!(
        "events: {} stored, {} duplicates ignored, {} skipped",
        counts.events_stored, counts.events_duplicate, counts.events_skipped
    );
    println!(
        "promoted: {}, cancelled: {}",
        counts.promoted, counts.cancelled
    );
}

fn print_draft((drafts, skipped): (Vec<Draft>, Suppressed), now: i64) {
    println!("drafted: {}", drafts.len());
    for (rank, draft) in drafts.iter().enumerate() {
        let line = quote_line(&draft.quote, &policy::names(&draft.reasons), now);
        println!("{:>2}. #{:<3} {line}", rank + 1, draft.id);
        println!("         {}", draft.body);
    }
    println!("skipped: {}", skipped.len());
    for (quote, label) in &skipped {
        println!("    {}", quote_line(quote, label, now));
    }
}

fn run(cli: Cli) -> Result<ExitCode, Box<dyn Error>> {
    let now = cli.now.unwrap_or_else(time::wall_clock);
    println!("now: {}", time::format_ts(now));
    let mut conn = db::open(&cli.db)?;
    let mut exit = ExitCode::SUCCESS;
    match cli.command {
        Command::Ingest { path } => print_ingest(&ingest::ingest(&mut conn, &path)?),
        Command::Candidates => {
            let (ranked, suppressed) = policy::candidates(&conn, &Policy::default(), now)?;
            println!("candidates: {}", ranked.len());
            for (rank, (quote, reasons)) in ranked.iter().enumerate() {
                println!("{:>2}. {}", rank + 1, quote_line(quote, &policy::names(reasons), now));
            }
            println!("suppressed: {}", suppressed.len());
            for (quote, label) in &suppressed {
                println!("    {}", quote_line(quote, label, now));
            }
        }
        Command::Draft => print_draft(draft::draft(&mut conn, &Policy::default(), now)?, now),
        Command::Approve { id: Some(id), body, .. } => {
            messages::approve(&conn, id, body.as_deref())?;
            println!("approved #{id}");
        }
        Command::Approve { .. } => {
            let (approved, need_edit) = messages::approve_all(&conn)?;
            for (title, list) in [("approved", approved), ("need edit", need_edit)] {
                println!("{title}: {}", list.len());
                for msg in &list {
                    println!(" {}", message_line(msg));
                }
            }
        }
        Command::Reject { id } => {
            messages::reject(&conn, id, now)?;
            println!("rejected #{id}");
        }
        Command::Send { fail } => {
            let sender = FakeSender { conn: &conn, fail };
            let (mut sent, mut failed, mut cancelled) = (0, 0, 0);
            for (msg, delivery) in messages::send(&conn, &Policy::default(), &sender, now)? {
                let line = message_line(&msg);
                match delivery {
                    Delivery::Sent(provider_id) => {
                        sent += 1;
                        println!("sent       {line}  {provider_id}");
                    }
                    Delivery::Failed(error) => {
                        failed += 1;
                        println!("failed     {line}  {error}");
                    }
                    Delivery::Cancelled(reason) => {
                        cancelled += 1;
                        println!("cancelled  {line}  {reason}");
                    }
                }
            }
            println!("sent: {sent}, failed: {failed}, cancelled: {cancelled}");
            if failed > 0 {
                exit = ExitCode::FAILURE;
            }
        }
        Command::Run { path } => {
            print_ingest(&ingest::ingest(&mut conn, &path)?);
            print_draft(draft::draft(&mut conn, &Policy::default(), now)?, now);
        }
        Command::Outbox => {
            println!(
                "id   quote   phone         status     reason              created               attempted             provider     error"
            );
            for msg in messages::list(&conn)? {
                println!("{}", outbox_line(&msg));
                println!("     {}", msg.body);
            }
        }
    }
    if let Some(days) = ingest::stale_days(&conn, now)? {
        eprintln!("warning: newest event is {days} days before now");
    }
    Ok(exit)
}

fn main() -> ExitCode {
    run(Cli::parse()).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        ExitCode::FAILURE
    })
}
