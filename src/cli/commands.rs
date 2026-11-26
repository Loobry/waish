//! Handler untuk flag manajemen CLI (-c, -p, -m, --memory-edit, --memory-clear, -s).

use std::io::IsTerminal;
use std::path::Path;
use std::process::{Command, ExitCode};

use colored::Colorize;

use super::args::Action;
use super::ui::{self, Kaomoji};
use crate::config::memory;
use crate::config::settings::{Settings, DEFAULT_BOT_NAME, DEFAULT_USER_REAL_NAME};
use crate::config::Paths;
use crate::wizard::detector::{self, SystemProfile};
use crate::wizard::i18n::WizardLang;

const FALLBACK_EDITORS: &[&str] = &["nano", "micro", "nvim", "vim", "vi"];

pub async fn dispatch(action: Action, paths: &Paths) -> crate::Result<ExitCode> {
    match action {
        Action::EditConfig => edit_file(paths, &paths.config, true),
        Action::EditPersona => edit_file(paths, &paths.persona, false),
        Action::EditMemory => edit_file(paths, &paths.memory, false),
        Action::ShowMemory => show_memory(paths),
        Action::ClearMemory => clear_memory(paths),
        Action::Status => check_status(paths).await,
    }
}

async fn check_status(paths: &Paths) -> crate::Result<ExitCode> {
    if let Some(code) = require_setup(paths, &paths.config) {
        return Ok(code);
    }
    let settings = Settings::load(&paths.config)?;
    let lang = WizardLang::from_code(&settings.general.language);
    let client = crate::engine::client::LlmClient::new(&settings)?;
    ui::print_bot_line(&settings.general.bot_name, Kaomoji::Wait);
    println!(
        "{}",
        lang.status_checking(
            &settings.api.provider.bold().to_string(),
            &settings.api.model.cyan().to_string()
        )
    );
    match client.check_status(&settings).await {
        Ok(summary) => {
            ui::print_bot_line(&settings.general.bot_name, Kaomoji::Success);
            println!("{summary}");
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => {
            err.print_friendly(&settings);
            Ok(ExitCode::FAILURE)
        }
    }
}

fn require_setup(paths: &Paths, file: &Path) -> Option<ExitCode> {
    if paths.config.exists() && file.exists() {
        return None;
    }
    ui::print_bot_line(DEFAULT_BOT_NAME, Kaomoji::Fail);
    if !paths.config.exists() {
        println!(
            "Setup belum dilakukan, jadi {} belum ada. Jalankan {} dulu yaa.",
            ui::tilde(file).cyan(),
            "waish".cyan().bold()
        );
    } else {
        println!(
            "File {} tidak ditemukan. Hapus {} lalu jalankan {} untuk setup ulang.",
            ui::tilde(file).cyan(),
            ui::tilde(&paths.config).cyan(),
            "waish".cyan().bold()
        );
    }
    Some(ExitCode::FAILURE)
}

fn edit_file(paths: &Paths, file: &Path, validate_config: bool) -> crate::Result<ExitCode> {
    if let Some(code) = require_setup(paths, file) {
        return Ok(code);
    }

    let (program, extra_args) = resolve_editor()
        .ok_or("Tidak ada editor yang ditemukan. Set $EDITOR dulu, misal: export EDITOR=nano")?;

    let status = Command::new(&program)
        .args(&extra_args)
        .arg(file)
        .status()
        .map_err(|e| format!("Gagal menjalankan editor '{program}': {e}"))?;

    if !status.success() {
        return Err(format!("Editor '{program}' keluar dengan status {status}").into());
    }

    if validate_config {
        match Settings::load(file) {
            Ok(s) => {
                ui::print_bot_line(&s.general.bot_name, Kaomoji::Success);
                println!("config.toml valid dan siap dipakai.");
            }
            Err(e) => {
                ui::print_bot_line(DEFAULT_BOT_NAME, Kaomoji::Warn);
                println!("Hmm, config.toml-nya jadi tidak valid:\n  {e}");
                println!("Perbaiki lagi lewat {} yaa.", "waish -c".cyan());
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn resolve_editor() -> Option<(String, Vec<String>)> {
    for var in ["EDITOR", "VISUAL"] {
        if let Ok(value) = std::env::var(var) {
            let mut parts = value.split_whitespace().map(str::to_string);
            if let Some(program) = parts.next() {
                return Some((program, parts.collect()));
            }
        }
    }
    FALLBACK_EDITORS
        .iter()
        .find(|e| detector::which(e).is_some())
        .map(|e| (e.to_string(), Vec::new()))
}

pub fn show_memory(paths: &Paths) -> crate::Result<ExitCode> {
    if let Some(code) = require_setup(paths, &paths.memory) {
        return Ok(code);
    }

    let settings = Settings::load(&paths.config).unwrap_or_default();
    let lang = WizardLang::from_code(&settings.general.language);
    let raw = memory::load(&paths.memory)?;
    let mut lines = memory::summarize(&raw);
    if lines.is_empty() {
        lines.push(lang.memory_empty().to_string());
    }

    let name = if settings.general.user_real_name.is_empty() {
        DEFAULT_USER_REAL_NAME.to_string()
    } else {
        settings.general.user_real_name.clone()
    };

    ui::print_bot_line(&settings.general.bot_name, Kaomoji::Greet);
    println!("{}", lang.memory_view_header(&name.bold().to_string()));
    println!();
    println!(
        "{}",
        ui::render_box("memory.md", &lines, 2, |line| {
            let t = line.trim();
            if t.starts_with('[') && t.ends_with(']') {
                line.cyan().bold()
            } else if t.starts_with('#') {
                line.magenta().bold()
            } else {
                line.normal()
            }
        })
    );
    println!("  {}", ui::tilde(&paths.memory).bright_black());
    Ok(ExitCode::SUCCESS)
}

fn clear_memory(paths: &Paths) -> crate::Result<ExitCode> {
    if let Some(code) = require_setup(paths, &paths.memory) {
        return Ok(code);
    }
    let settings = Settings::load(&paths.config)?;

    if !std::io::stdin().is_terminal() {
        return Err("--memory-clear butuh terminal interaktif untuk konfirmasi.".into());
    }

    ui::print_bot_line(&settings.general.bot_name, Kaomoji::Warn);
    println!("Ini bakal menghapus semua yang aku ingat (termasuk memori impor) dan kembali ke template awal.");

    crate::wizard::prompts::install_render_config(WizardLang::from_code(
        &settings.general.language,
    ));
    let confirmed = match inquire::Confirm::new("Yakin mau mengosongkan memory bank?")
        .with_default(false)
        .prompt()
    {
        Ok(v) => v,
        Err(inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted) => false,
        Err(e) => return Err(e.into()),
    };

    if !confirmed {
        ui::print_bot_line(&settings.general.bot_name, Kaomoji::Greet);
        println!("Fiuh, batal. Memori kamu tetap aman.");
        return Ok(ExitCode::SUCCESS);
    }

    let user = if settings.general.user_real_name.trim().is_empty() {
        DEFAULT_USER_REAL_NAME.to_string()
    } else {
        settings.general.user_real_name.clone()
    };
    let note = format!(
        "Memory bank direset pada {}.",
        chrono::Local::now().format("%Y-%m-%d %H:%M")
    );
    let fresh = memory::render_template(&SystemProfile::detect().memory_profile(&user), None, &note);
    memory::save(&paths.memory, &fresh)?;

    ui::print_bot_line(&settings.general.bot_name, Kaomoji::Success);
    println!("Memory bank sudah bersih. Kita mulai lembaran baru yaa, {}.", user.bold());
    Ok(ExitCode::SUCCESS)
}
