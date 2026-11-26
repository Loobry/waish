//! waish - Sovereign Unix Waifu Companion & System Automation Shell.

mod cli;
mod config;
mod engine;
mod executor;
mod tools;
mod wizard;

use std::process::ExitCode;

use clap::Parser;
use colored::Colorize;

use cli::args::Args;
use cli::ui::Kaomoji;
use config::settings::Settings;
use config::Paths;
use wizard::WizardOutcome;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const EXIT_INTERRUPTED: u8 = 130;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    match run(args).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{} {}", Kaomoji::Fail.as_str().red(), err);
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<ExitCode> {
    let paths = Paths::resolve()?;

    if let Some(action) = args.action() {
        return cli::commands::dispatch(action, &paths).await;
    }

    let settings = if !paths.config.exists() {
        match wizard::run(&paths)? {
            WizardOutcome::Saved(settings) => *settings,
            WizardOutcome::Declined => return Ok(ExitCode::SUCCESS),
            WizardOutcome::Cancelled => return Ok(ExitCode::from(EXIT_INTERRUPTED)),
        }
    } else {
        Settings::load(&paths.config)?
    };

    cli::repl::run(settings, paths, args.initial_prompt()).await
}
