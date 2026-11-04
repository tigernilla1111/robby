mod db;
mod time;

use clap::Parser;
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
}

fn parse_now(s: &str) -> Result<i64, String> {
    time::parse_ts(s)
        .ok_or_else(|| "expected YYYY-MM-DDTHH:MM:SSZ or YYYY-MM-DDTHH:MMZ".to_string())
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    let now = cli.now.unwrap_or_else(time::wall_clock);
    println!("now: {}", time::format_ts(now));
    db::open(&cli.db)?;
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
