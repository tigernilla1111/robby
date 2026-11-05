use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Deleted on drop, so a test that panics cleans up too.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn robby(db: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_robby"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .arg("--db")
        .arg(db)
        .args(["--now", "2026-08-17T13:00:00Z"])
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn count(text: &str, needle: &str) -> usize {
    text.lines().filter(|line| line.contains(needle)).count()
}

/// Asserts `text` contains `needle`, printing all of `text` if it does not.
#[track_caller]
fn has(text: &str, needle: &str) {
    assert!(text.contains(needle), "expected {needle:?} in:\n{text}");
}

/// The first line starting with `prefix`.
#[track_caller]
fn row<'a>(text: &'a str, prefix: &str) -> &'a str {
    match text.lines().find(|line| line.starts_with(prefix)) {
        Some(line) => line,
        None => panic!("no line starting {prefix:?} in:\n{text}"),
    }
}

fn without_now(text: &str) -> Vec<&str> {
    text.lines().filter(|line| !line.starts_with("now:")).collect()
}

fn provider_rows(db: &Path) -> i64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row("SELECT COUNT(*) FROM provider_outbox", [], |row| row.get(0)).unwrap()
}

/// The README's demo sequence, so `cargo test` checks the numbers it quotes.
#[test]
fn readme_demo_sequence() {
    let dir = TempDir(std::env::temp_dir().join(format!("robby-demo-{}", std::process::id())));
    let _ = fs::remove_dir_all(&dir.0);
    fs::create_dir_all(&dir.0).unwrap();
    let db = dir.0.join("demo.db");

    let first_run = robby(&db, &["run"]);
    assert!(first_run.status.success());
    let first_text = stdout(&first_run);
    has(&first_text, "quotes: 30 stored, 0 skipped");
    has(&first_text, "events: 82 stored, 6 duplicates ignored, 0 skipped");
    has(&first_text, "drafted: 25");
    has(&first_text, "skipped: 5");
    assert_eq!(count(&first_text, " 1. #1   Q-1016"), 1);
    assert_eq!(count(&first_text, " 2. #2   Q-1007"), 1);
    assert_eq!(count(&first_text, " 3. #3   Q-1015"), 1);

    let text = stdout(&robby(&db, &["approve", "--all"]));
    has(&text, "approved: 22");
    has(&text, "need edit: 3");

    let send = robby(&db, &["send"]);
    assert!(send.status.success());
    has(&stdout(&send), "sent: 22, failed: 0, cancelled: 0");
    assert_eq!(provider_rows(&db), 22);

    has(&stdout(&robby(&db, &["send"])), "sent: 0, failed: 0, cancelled: 0");
    assert_eq!(provider_rows(&db), 22);

    let text = stdout(&robby(&db, &["run"]));
    has(&text, "events: 0 stored, 88 duplicates ignored, 0 skipped");
    has(&text, "drafted: 0");
    has(&text, "skipped: 30");
    assert_eq!(count(&text, "cooldown"), 23);
    assert_eq!(count(&text, "pending message"), 3);
    assert_eq!(count(&text, "not open"), 3);
    assert_eq!(count(&text, "ladder wait"), 1);

    let refused = robby(&db, &["approve", "1"]);
    assert_eq!(refused.status.code(), Some(1));
    has(&String::from_utf8_lossy(&refused.stderr), "must be edited");

    let reply = "Hi Ray, thanks for your message about the quote. We can do Tuesday.";
    has(&stdout(&robby(&db, &["approve", "1", "--body", reply])), "approved #1");

    let lost_ack = robby(&db, &["send", "--fail", "Q-1016"]);
    assert_eq!(lost_ack.status.code(), Some(1));
    let text = stdout(&lost_ack);
    has(&text, "failed     #1");
    has(&text, "sent: 0, failed: 1, cancelled: 0");
    assert_eq!(provider_rows(&db), 23);

    let text = stdout(&robby(&db, &["outbox"]));
    let first = row(&text, "#1 ");
    has(first, "failed");
    has(first, "ack lost");
    assert_eq!(count(&text, "STUCK"), 0);

    let retry = robby(&db, &["send"]);
    assert!(retry.status.success());
    let text = stdout(&retry);
    has(&text, "sent       #1");
    has(&text, "sent: 1, failed: 0, cancelled: 0");
    assert_eq!(provider_rows(&db), 23);

    let text = stdout(&robby(&db, &["outbox"]));
    let rows: Vec<&str> = text.lines().filter(|line| line.starts_with('#')).collect();
    assert_eq!(rows.len(), 25);
    let rows = rows.join("\n");
    assert_eq!(count(&rows, "sent"), 23);
    assert_eq!(count(&rows, "draft"), 2);
    let first = row(&rows, "#1 ");
    has(first, "sent");
    has(first, "unanswered_reply");
    has(first, "sm_");
    assert_eq!(first.matches("2026-08-17T13:00:00Z").count(), 2, "{first}");
    assert!(first.ends_with("  -"), "{first}");
    has(&text, &format!("\n     {reply}\n"));
    assert_eq!(
        row(&rows, "#2 "),
        "#2   Q-1007  +19175552007  draft      unanswered_reply    2026-08-17T13:00:00Z  -                     -            -"
    );
    assert_eq!(count(&text, "STUCK"), 0);

    has(&stdout(&robby(&db, &["reject", "3"])), "rejected #3");
    let text = stdout(&robby(&db, &["outbox"]));
    has(row(&text, "#3 "), "rejected");

    let conn = Connection::open(&db).unwrap();
    conn.execute("UPDATE messages SET status = 'sending' WHERE id = 2", []).unwrap();
    let text = stdout(&robby(&db, &["outbox"]));
    assert_eq!(count(&text, "STUCK"), 1);
    has(row(&text, "#2 "), "STUCK");

    let bad_path = robby(&db, &["run", "no-such-dir"]);
    assert_eq!(bad_path.status.code(), Some(1));
    has(&String::from_utf8_lossy(&bad_path.stderr), "no such path");

    let second_db = dir.0.join("second.db");
    let split = stdout(&robby(&second_db, &["ingest", "seed"]))
        + &stdout(&robby(&second_db, &["draft"]));
    assert_eq!(without_now(&split), without_now(&first_text));
}
