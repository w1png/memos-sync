mod api;
mod config;
mod service;
mod storage;
mod sync;
#[cfg(test)]
mod sync_tests;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

const USAGE_GUIDE: &str = "Configuration (environment variables or .env in the current directory):
  MEMOS_URL   Your Memos instance URL (Memos 0.31.x)
  TOKEN       Your personal access token
  DATA_PATH   Local notes directory (default: ./data)

Create a note:
  mkdir -p data/my-note
  vim data/my-note/note.md
  memos sync
  Write plain Markdown; sync adds the ID and creates a private memo.
  Use DATA_PATH instead of data if you configured a different directory.

Resolve a conflict:
  Compare note.md with remote.md, edit note.md, then delete conflict.diff.
  Run memos sync again. A newer remote edit produces a fresh diff.

Schedule memos sync with cron for automatic syncing.
Delete notes in Memos; missing local files download again.";

#[derive(Parser)]
#[command(
    version,
    about = "Two-way Markdown sync for Memos 0.31",
    disable_help_subcommand = false,
    after_help = USAGE_GUIDE
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sync once (suitable for cron).
    #[command(after_help = USAGE_GUIDE)]
    Sync,
    /// Manage the per-user cron job through the Makefile.
    #[command(hide = true)]
    Service {
        #[command(subcommand)]
        command: service::ServiceCommand,
    },
}

fn cycle(config: &config::Config) -> Result<i32> {
    let Some(store) = storage::Store::open(&config.data_path)? else {
        println!("Another sync is running; skipping.");
        return Ok(0);
    };
    let api = api::HttpApi::new(&config.url, &config.token)?;
    let report = sync::synchronize(&api, &store)?;
    println!(
        "Downloaded: {}, uploaded: {}, trashed: {}, conflicts: {}, errors: {}",
        report.downloaded,
        report.uploaded,
        report.trashed,
        report.conflicts.len(),
        report.errors.len()
    );
    for path in &report.conflicts {
        eprintln!("Conflict: {}", path.display());
    }
    for error in &report.errors {
        eprintln!("Error: {error:#}");
    }
    Ok(if !report.errors.is_empty() {
        1
    } else if !report.conflicts.is_empty() {
        2
    } else {
        0
    })
}

fn execute() -> Result<i32> {
    match Cli::parse().command {
        Command::Service { command } => service::execute(command),
        Command::Sync => {
            match dotenvy::from_path(".env") {
                Ok(_) => {}
                Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("Cannot load .env"),
            }
            cycle(&config::Config::from_env()?)
        }
    }
}

fn main() {
    match execute() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("Error: {error:#}");
            std::process::exit(1);
        }
    }
}
