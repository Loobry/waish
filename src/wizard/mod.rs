//! First-run onboarding wizard.

pub mod detector;
pub mod i18n;
pub mod prompts;
pub mod writer;

use std::io::IsTerminal;

use colored::Colorize;

use crate::cli::ui::Kaomoji;
use crate::config::persona::Personality;
use crate::config::settings::Settings;
use crate::config::Paths;
use detector::{DetectedProvider, SystemProfile};
use i18n::WizardLang;

/// In-memory wizard draft prior to atomic disk commit.
#[derive(Debug, Clone)]
pub struct WizardDraft {
    pub language: String,
    pub provider: DetectedProvider,
    pub bot_name: String,
    pub user_name: String,
    pub personality: Personality,
    pub imported_memory: Option<String>,
    pub system: SystemProfile,
}

/// Final wizard execution outcome.
#[derive(Debug)]
pub enum WizardOutcome {
    Saved(Box<Settings>),
    Declined,
    Cancelled,
}

/// Wizard error type.
#[derive(Debug)]
pub enum WizardError {
    Cancelled,
    Io(std::io::Error),
    Prompt(inquire::InquireError),
}

impl std::fmt::Display for WizardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WizardError::Cancelled => write!(f, "setup dibatalkan"),
            WizardError::Io(e) => write!(f, "gagal membaca input: {e}"),
            WizardError::Prompt(e) => write!(f, "prompt wizard gagal: {e}"),
        }
    }
}

impl std::error::Error for WizardError {}

impl From<std::io::Error> for WizardError {
    fn from(e: std::io::Error) -> Self {
        WizardError::Io(e)
    }
}

impl From<inquire::InquireError> for WizardError {
    fn from(e: inquire::InquireError) -> Self {
        use inquire::InquireError::*;
        match e {
            OperationCanceled | OperationInterrupted => WizardError::Cancelled,
            other => WizardError::Prompt(other),
        }
    }
}

pub fn run(paths: &Paths) -> crate::Result<WizardOutcome> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(format!(
            "Setup wizard requires an interactive terminal (TTY). Run `waish` directly from a terminal. \
             (config not found: {})",
            paths.config.display()
        )
        .into());
    }

    prompts::install_render_config(WizardLang::En);
    prompts::print_banner();

    let (lang, draft_result) = prompts::collect_with_lang(SystemProfile::detect());
    let draft = match draft_result {
        Ok(draft) => draft,
        Err(WizardError::Cancelled) => {
            print_cancelled(lang);
            return Ok(WizardOutcome::Cancelled);
        }
        Err(e) => return Err(e.into()),
    };

    prompts::print_summary(&draft);

    match prompts::confirm_save(lang) {
        Ok(true) => {}
        Ok(false) => {
            println!();
            println!(
                "{} {}",
                Kaomoji::Greet.as_str().magenta(),
                lang.declined_message()
            );
            return Ok(WizardOutcome::Declined);
        }
        Err(WizardError::Cancelled) => {
            print_cancelled(lang);
            return Ok(WizardOutcome::Cancelled);
        }
        Err(e) => return Err(e.into()),
    }

    let settings = writer::commit(&draft, paths)?;
    println!();
    Ok(WizardOutcome::Saved(Box::new(settings)))
}

fn print_cancelled(lang: WizardLang) {
    println!();
    println!(
        "{} {}",
        Kaomoji::Fail.as_str().magenta(),
        lang.cancelled_line1()
    );
    println!("        {}", lang.cancelled_line2().bright_black());
}
