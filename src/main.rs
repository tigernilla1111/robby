mod db;
mod ingest;
mod policy;
mod time;

use clap::{Parser, Subcommand};
use policy::{DAY, Facts, Outcome, Policy};
use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "robby", about = "Quote follow-up engine")]
struct Cli {
    /// SQLite database file
    #[arg(long, global = true, default_value = "robby.db")]
    db: PathBuf,

    /// Current time, YYYY-MM-DDTHH:MM:SSZ or YYYY-MM-DDTHH:MMZ (default: wall clock)
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
}

fn parse_now(s: &str) -> Result<i64, String> {
    time::parse_ts(s)
        .ok_or_else(|| "expected YYYY-MM-DDTHH:MM:SSZ or YYYY-MM-DDTHH:MMZ".to_string())
}

fn quote_line(quote: &Facts, outcome: &Outcome, now: i64) -> String {
    let days = (now - quote.since_sent()) / DAY;
    let contact = match quote.sends.len() {
        0 => format!("never sent, created {days}d ago"),
        sends => format!("sent {sends}, last {days}d ago"),
    };
    format!(
        "{:<6}  {:<16}  {:>6}  {:<31}  {contact}, views {}, replies {}",
        quote.id,
        quote.customer_name,
        format!("${:.0}", quote.amount),
        outcome.to_string(),
        quote.views.len(),
        quote.replies.len()
    )
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    let now = cli.now.unwrap_or_else(time::wall_clock);
    println!("now: {}", time::format_ts(now));
    let mut conn = db::open(&cli.db)?;
    match cli.command {
        Command::Ingest { path } => {
            let counts = ingest::ingest(&mut conn, &path, now)?;
            println!("quotes: {} stored, {} skipped", counts.quotes_stored, counts.quotes_skipped);
            println!(
                "events: {} stored, {} duplicates ignored, {} skipped",
                counts.events_stored, counts.events_duplicate, counts.events_skipped
            );
            println!("promoted: {}, cancelled: {}", counts.promoted, counts.cancelled);
        }
        Command::Candidates => {
            let (ranked, suppressed) = policy::candidates(&conn, &Policy::default(), now)?;
            println!("candidates: {}", ranked.len());
            for (rank, (quote, outcome)) in ranked.iter().enumerate() {
                println!("{:>2}. {}", rank + 1, quote_line(quote, outcome, now));
            }
            println!("suppressed: {}", suppressed.len());
            for (quote, outcome) in &suppressed {
                println!("    {}", quote_line(quote, outcome, now));
            }
        }
    }
    if let Some(days) = ingest::stale_days(&conn, now)? {
        eprintln!("warning: newest event is {days} days before now");
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
