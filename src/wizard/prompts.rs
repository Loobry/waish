//! Input interaktif wizard.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use colored::Colorize;
use inquire::ui::{Color, RenderConfig, Styled};
use inquire::validator::Validation;
use inquire::{Confirm, Password, PasswordDisplayMode, Text};

use super::detector::{self, DetectedProvider, Provider, SystemProfile};
use super::i18n::WizardLang;
use super::{WizardDraft, WizardError};
use crate::cli::ui::{self, display_width, Kaomoji};
use crate::config::persona::{Personality, DEFAULT_PERSONALITY_LABEL};
use crate::config::settings::{DEFAULT_BOT_NAME, DEFAULT_USER_REAL_NAME};
use crate::config::PRIVATE_FILE_MODE;

const MAX_NAME_CHARS: usize = 32;
const MENU_INDENT: &str = "  ";
const HINT_INDENT: &str = "      ";

const EDITOR_TEMPLATE_HEADER: &[&str] = &[
    "# --- waish Setup ---",
    "# Delete these comment lines and paste your custom content below.",
    "# Save and exit the editor when you are finished.",
    "# -------------------",
];

const PERSONA_EDITOR_HINT: &str = "# Delete this and paste your custom persona here...";
const MEMORY_EDITOR_HINT: &str =
    "# Delete this and paste your profile, facts, or memory here...";

pub fn install_render_config(lang: WizardLang) {
    let cancel_label = lang.canceled_indicator();
    let base = if colored::control::SHOULD_COLORIZE.should_colorize() {
        RenderConfig::default_colored()
            .with_prompt_prefix(Styled::new(" +").with_fg(Color::LightMagenta))
            .with_answered_prompt_prefix(Styled::new(" +").with_fg(Color::LightCyan))
            .with_canceled_prompt_indicator(Styled::new(cancel_label).with_fg(Color::DarkRed))
    } else {
        RenderConfig::empty()
            .with_prompt_prefix(Styled::new(" +"))
            .with_answered_prompt_prefix(Styled::new(" +"))
            .with_canceled_prompt_indicator(Styled::new(cancel_label))
    };
    inquire::set_global_render_config(base);
}

pub fn print_banner() {
    let lines = vec![
        format!(
            "Welcome! Let's set up your terminal companion {}",
            Kaomoji::Greet.as_str()
        ),
        "Answer 6 quick steps to personalize your assistant.".to_string(),
    ];
    println!();
    println!("{}", ui::render_plain_box("waish setup wizard", &lines, 0));
    println!(
        "{}",
        "  Ctrl+C anytime to cancel. No files are written until final confirmation."
            .bright_black()
    );
    println!();
}

pub fn collect_with_lang(system: SystemProfile) -> (WizardLang, Result<WizardDraft, WizardError>) {
    let lang = match step_language() {
        Ok(l) => l,
        Err(err) => return (WizardLang::En, Err(err)),
    };

    install_render_config(lang);

    let res = (|| {
        let provider = step_api_and_model(lang)?;
        let bot_name = step_bot_name(lang)?;
        let user_name = step_user_name(lang)?;
        let personality = step_personality(lang)?;
        let imported_memory = step_import_memory(lang)?;

        Ok(WizardDraft {
            language: lang.code().to_string(),
            provider,
            bot_name,
            user_name,
            personality,
            imported_memory,
            system,
        })
    })();

    (lang, res)
}

fn step_language() -> Result<WizardLang, WizardError> {
    println!(
        "{} {}",
        "[1/6]".magenta().bold(),
        "Select your preferred language / Pilih bahasa:".bold()
    );
    for (idx, lang) in WizardLang::ALL.iter().enumerate() {
        println!(
            "{HINT_INDENT}{} {}",
            format!("[{}]", idx + 1).magenta().bold(),
            lang.menu_label()
        );
    }

    let choice = ask_numeric_choice(
        "Select option [1-3] (default: 1):",
        WizardLang::ALL.len(),
        Some(1),
        "Type 1, 2, or 3 and press Enter (Enter = 1: English).",
        WizardLang::En,
    )?;

    let chosen = WizardLang::from_choice(choice).unwrap_or(WizardLang::En);
    println!();
    Ok(chosen)
}

#[cfg(test)]
pub fn render_provider_menu_lines(lang: WizardLang) -> Vec<String> {
    let mut lines = Vec::with_capacity(Provider::ALL.len() + 1);
    lines.push(format!("{MENU_INDENT}{}", lang.provider_header()));
    for (idx, p) in Provider::ALL.iter().enumerate() {
        lines.push(format!(
            "{MENU_INDENT}[{}] {}",
            idx + 1,
            p.menu_label_for(lang)
        ));
    }
    lines
}

#[cfg(test)]
pub fn render_model_menu_lines(provider: Provider, lang: WizardLang) -> Vec<String> {
    let presets = provider.preset_options();
    if presets.is_empty() {
        return Vec::new();
    }
    let custom_option_num = presets.len() + 1;
    let mut lines = Vec::with_capacity(presets.len() + 2);
    lines.push(format!(
        "{MENU_INDENT}{}",
        lang.model_header_for_provider(provider.display_name_for(lang))
    ));
    for (idx, preset) in presets.iter().enumerate() {
        lines.push(format!(
            "{MENU_INDENT}[{}] {}",
            idx + 1,
            preset.menu_display(lang)
        ));
    }
    lines.push(format!(
        "{MENU_INDENT}[{custom_option_num}] {}",
        lang.custom_model_option()
    ));
    lines
}

fn step_api_and_model(lang: WizardLang) -> Result<DetectedProvider, WizardError> {
    println!(
        "{} {}",
        "[2/6]".magenta().bold(),
        "Provider, API Key & Model".bold()
    );
    println!("{MENU_INDENT}{}", lang.provider_header().bold());
    for (idx, p) in Provider::ALL.iter().enumerate() {
        println!(
            "{MENU_INDENT}{} {}",
            format!("[{}]", idx + 1).magenta().bold(),
            p.menu_label_for(lang)
        );
    }

    let provider_help = lang.numeric_help(Provider::ALL.len());
    let provider_choice = ask_numeric_choice(
        lang.provider_prompt(),
        Provider::ALL.len(),
        Some(1),
        &provider_help,
        lang,
    )?;
    let provider = Provider::from_number(provider_choice).unwrap_or(Provider::Custom);

    let base_url = match provider.default_base_url() {
        Some(url) => url.to_string(),
        None => ask_text(
            lang.base_url_prompt(),
            None,
            lang.base_url_help(),
            lang,
            validate_non_empty,
        )?,
    };

    println!();
    let formatter = |input: &str| detector::mask_key(input);
    let empty_err = lang.api_key_empty_err();
    let key_prompt = lang.api_key_prompt_for_provider(provider);

    let raw_key = Password::new(key_prompt)
        .with_display_mode(PasswordDisplayMode::Masked)
        .without_confirmation()
        .with_display_toggle_enabled()
        .with_help_message(lang.api_key_help())
        .with_validator(move |input: &str| {
            Ok(match detector::validate_api_key(input) {
                Ok(_) => Validation::Valid,
                Err(_) => Validation::Invalid(empty_err.into()),
            })
        })
        .with_formatter(&formatter)
        .prompt()?;

    let api_key = detector::validate_api_key(&raw_key)
        .map_err(|_| WizardError::Io(io::Error::new(io::ErrorKind::InvalidInput, empty_err)))?;

    let model = select_model_for_provider(provider, lang)?;

    println!(
        "{MENU_INDENT}{} Provider: {}  |  Model: {}  |  Endpoint: {}",
        "[ok]".cyan(),
        provider.display_name_for(lang).bold(),
        model.cyan().bold(),
        base_url.bright_black()
    );
    println!();

    Ok(DetectedProvider {
        provider,
        api_key,
        base_url,
        model,
    })
}

fn select_model_for_provider(provider: Provider, lang: WizardLang) -> Result<String, WizardError> {
    let presets = provider.preset_options();
    if presets.is_empty() {
        println!();
        return ask_text(
            lang.custom_model_prompt(),
            None,
            lang.custom_model_help(),
            lang,
            validate_non_empty,
        );
    }

    let custom_option_num = presets.len() + 1;
    println!();
    println!(
        "{MENU_INDENT}{}",
        lang.model_header_for_provider(provider.display_name_for(lang))
            .bold()
    );
    for (idx, preset) in presets.iter().enumerate() {
        println!(
            "{MENU_INDENT}{} {}",
            format!("[{}]", idx + 1).magenta().bold(),
            preset.menu_display(lang)
        );
    }
    println!(
        "{MENU_INDENT}{} {}",
        format!("[{custom_option_num}]").magenta().bold(),
        lang.custom_model_option()
    );

    let prompt_label = lang.model_choice_prompt(custom_option_num);
    let help_msg = lang.numeric_help(custom_option_num);
    let choice = ask_numeric_choice(&prompt_label, custom_option_num, Some(1), &help_msg, lang)?;

    if choice == custom_option_num {
        ask_text(
            lang.custom_model_prompt(),
            None,
            lang.custom_model_help(),
            lang,
            validate_non_empty,
        )
    } else {
        Ok(presets[choice - 1].id.to_string())
    }
}

fn ask_numeric_choice(
    message: &str,
    max: usize,
    default: Option<usize>,
    help: &str,
    lang: WizardLang,
) -> Result<usize, WizardError> {
    let formatter = move |input: &str| {
        let trimmed = input.trim();
        match (trimmed.is_empty(), default) {
            (true, Some(d)) => d.to_string(),
            _ => trimmed.to_string(),
        }
    };

    let answer = Text::new(message)
        .with_help_message(help)
        .with_validator(move |input: &str| {
            Ok(
                match detector::parse_numeric_choice_with_default(input, max, default, lang) {
                    Ok(_) => Validation::Valid,
                    Err(msg) => Validation::Invalid(msg.into()),
                },
            )
        })
        .with_formatter(&formatter)
        .prompt()?;

    detector::parse_numeric_choice_with_default(&answer, max, default, lang)
        .map_err(|msg| WizardError::Io(io::Error::new(io::ErrorKind::InvalidInput, msg)))
}

fn step_bot_name(lang: WizardLang) -> Result<String, WizardError> {
    let message = lang.bot_name_prompt(DEFAULT_BOT_NAME);
    ask_text(
        &message,
        Some(DEFAULT_BOT_NAME),
        lang.bot_name_help(),
        lang,
        validate_name,
    )
}

fn step_user_name(lang: WizardLang) -> Result<String, WizardError> {
    let message = lang.user_name_prompt(DEFAULT_USER_REAL_NAME);
    ask_text(
        &message,
        Some(DEFAULT_USER_REAL_NAME),
        lang.user_name_help(),
        lang,
        validate_name,
    )
}

fn step_personality(lang: WizardLang) -> Result<Personality, WizardError> {
    println!();
    println!("{} {}", "[5/6]".magenta().bold(), lang.persona_header().bold());
    println!(
        "{HINT_INDENT}{} {}",
        "[1]".magenta().bold(),
        lang.persona_opt_default()
    );
    println!(
        "{HINT_INDENT}{} {}",
        "[2]".magenta().bold(),
        lang.persona_opt_editor()
    );

    let choice = ask_numeric_choice(lang.persona_prompt(), 2, Some(1), lang.persona_help(), lang)?;

    if choice == 1 {
        println!(
            "{HINT_INDENT}{} {} {}",
            "[ok]".cyan(),
            lang.persona_using_default(),
            lang.default_personality_summary().cyan()
        );
        Ok(Personality::Default)
    } else {
        let custom = open_in_editor(PERSONA_EDITOR_HINT, DEFAULT_PERSONALITY_LABEL)?;
        let personality = Personality::from_input(&custom);
        println!(
            "{HINT_INDENT}{} {} {}",
            "[ok]".cyan(),
            lang.persona_saved_draft(),
            truncate(&personality.summary_for(lang), 52).cyan()
        );
        Ok(personality)
    }
}

fn step_import_memory(lang: WizardLang) -> Result<Option<String>, WizardError> {
    println!();
    println!("{} {}", "[6/6]".magenta().bold(), lang.memory_header().bold());
    println!(
        "{HINT_INDENT}{} {}",
        "[1]".magenta().bold(),
        lang.memory_opt_clean()
    );
    println!(
        "{HINT_INDENT}{} {}",
        "[2]".magenta().bold(),
        lang.memory_opt_editor()
    );

    let choice = ask_numeric_choice(lang.memory_prompt(), 2, Some(1), lang.memory_help(), lang)?;

    if choice == 1 {
        println!(
            "{HINT_INDENT}{} {}",
            "[ok]".cyan(),
            lang.memory_clean_ok().bright_black()
        );
        Ok(None)
    } else {
        let content = open_in_editor(MEMORY_EDITOR_HINT, "")?;
        if content.trim().is_empty() {
            println!(
                "{HINT_INDENT}{} {}",
                "[i]".bright_black(),
                lang.memory_editor_empty().bright_black()
            );
            Ok(None)
        } else {
            let line_count = content.lines().count();
            println!(
                "{HINT_INDENT}{} {}",
                "[ok]".cyan(),
                lang.memory_lines_imported(line_count).cyan()
            );
            Ok(Some(content))
        }
    }
}

pub fn open_in_editor(header_hint: &str, default_content: &str) -> Result<String, WizardError> {
    let (program, extra_args) = resolve_wizard_editor();

    let timestamp = chrono::Utc::now().timestamp_millis();
    let pid = std::process::id();
    let tmp_path = env::temp_dir().join(format!(".waish-setup-{timestamp}-{pid}.md"));

    let guard = TempFileGuard::create(&tmp_path, header_hint)?;

    let _termios_guard = WizardTermiosGuard::capture_stdin();
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = io::stdout().flush();

    let status = Command::new(&program)
        .args(&extra_args)
        .arg(&guard.path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Failed to launch editor '{program}': {e}"),
            )
        })?;

    let _ = crossterm::terminal::disable_raw_mode();

    if !status.success() {
        return Ok(default_content.to_string());
    }

    let raw = fs::read_to_string(&guard.path).unwrap_or_default();
    Ok(parse_editor_content(&raw, header_hint, default_content))
}

pub fn resolve_wizard_editor() -> (String, Vec<String>) {
    if let Ok(raw_editor) = env::var("EDITOR") {
        let mut parts = raw_editor.split_whitespace().map(str::to_string);
        if let Some(bin) = parts
            .next()
            .filter(|b| detector::which(b).is_some() || Path::new(b).is_file())
        {
            return (bin, parts.collect());
        }
    }

    if detector::which("nano").is_some() {
        return ("nano".to_string(), Vec::new());
    }

    ("vi".to_string(), Vec::new())
}

pub fn build_editor_template(header_hint: &str) -> String {
    let mut out = EDITOR_TEMPLATE_HEADER.join("\n");
    out.push('\n');
    let hint = header_hint.trim();
    if !hint.is_empty() {
        out.push_str(hint);
        out.push('\n');
    }
    out.push('\n');
    out
}

pub fn parse_editor_content(raw: &str, header_hint: &str, default_content: &str) -> String {
    let hint_trimmed = header_hint.trim();
    let cleaned_lines: Vec<&str> = raw
        .lines()
        .filter(|line| {
            let t = line.trim();
            if t.starts_with("# ---") {
                return false;
            }
            if !hint_trimmed.is_empty() && t == hint_trimmed {
                return false;
            }
            !EDITOR_TEMPLATE_HEADER.contains(&t)
        })
        .collect();

    let joined = cleaned_lines.join("\n");
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        default_content.to_string()
    } else {
        trimmed.to_string()
    }
}

struct TempFileGuard {
    path: PathBuf,
}

impl TempFileGuard {
    fn create(path: &Path, header_hint: &str) -> io::Result<Self> {
        let _ = fs::remove_file(path);
        let template = build_editor_template(header_hint);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .open(path)?;
        file.write_all(template.as_bytes())?;
        file.sync_all()?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct WizardTermiosGuard {
    saved: Option<libc::termios>,
}

impl WizardTermiosGuard {
    fn capture_stdin() -> Self {
        if !io::stdin().is_terminal() {
            return Self { saved: None };
        }
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut term) } == 0 {
            Self { saved: Some(term) }
        } else {
            Self { saved: None }
        }
    }
}

impl Drop for WizardTermiosGuard {
    fn drop(&mut self) {
        if let Some(ref term) = self.saved {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, term);
            }
        }
    }
}

pub fn print_summary(draft: &WizardDraft) {
    let lang = WizardLang::from_code(&draft.language);
    let labels = lang.summary_labels();
    let memory = lang.summary_memory_value(
        draft
            .imported_memory
            .as_ref()
            .map(|text| text.lines().count()),
    );

    let values = [
        draft.bot_name.clone(),
        draft.user_name.clone(),
        draft.language.clone(),
        draft.provider.provider.display_name_for(lang).to_string(),
        draft.provider.model.clone(),
        truncate(&draft.provider.base_url, 48),
        draft.provider.masked_key(),
        format!("{} (Auto)", draft.system.desktop),
        format!("{} (Auto)", draft.system.aur_helper),
        truncate(&draft.personality.summary_for(lang), 40),
        memory,
    ];

    let label_col_width = labels
        .iter()
        .map(|l| display_width(l))
        .max()
        .unwrap_or(9)
        .max(9);

    let lines: Vec<String> = labels
        .iter()
        .zip(values.iter())
        .map(|(label, value)| {
            let pad = label_col_width.saturating_sub(display_width(label));
            format!("{label}{} : {value}", " ".repeat(pad))
        })
        .collect();

    println!();
    println!("{}", ui::render_plain_box(lang.summary_title(), &lines, 2));
    println!();
}

pub fn confirm_save(lang: WizardLang) -> Result<bool, WizardError> {
    Ok(Confirm::new(lang.confirm_save_prompt())
        .with_default(true)
        .with_help_message(lang.confirm_save_help())
        .prompt()?)
}

fn ask_text(
    message: &str,
    default: Option<&str>,
    help: &str,
    lang: WizardLang,
    validate: fn(&str, WizardLang) -> Result<(), String>,
) -> Result<String, WizardError> {
    let default_owned = default.map(str::to_string);
    let default_for_fmt = default_owned.clone();
    let formatter = move |input: &str| {
        let trimmed = input.trim();
        match (&default_for_fmt, trimmed.is_empty()) {
            (Some(d), true) => d.clone(),
            _ => trimmed.to_string(),
        }
    };

    let has_default = default_owned.is_some();
    let answer = Text::new(message)
        .with_help_message(help)
        .with_validator(move |input: &str| {
            if input.is_empty() && has_default {
                return Ok(Validation::Valid);
            }
            let trimmed = input.trim();
            if trimmed.is_empty() {
                return Ok(Validation::Invalid(
                    lang.empty_value_err(has_default).into(),
                ));
            }
            Ok(match validate(trimmed, lang) {
                Ok(()) => Validation::Valid,
                Err(msg) => Validation::Invalid(msg.into()),
            })
        })
        .with_formatter(&formatter)
        .prompt()?;

    let trimmed = answer.trim();
    Ok(match (trimmed.is_empty(), default_owned) {
        (true, Some(d)) => d,
        _ => trimmed.to_string(),
    })
}

fn validate_non_empty(input: &str, lang: WizardLang) -> Result<(), String> {
    if input.trim().is_empty() {
        Err(lang.empty_value_err(false).to_string())
    } else {
        Ok(())
    }
}

fn validate_name(input: &str, lang: WizardLang) -> Result<(), String> {
    if input.chars().any(char::is_control) {
        return Err(lang.name_control_err().to_string());
    }
    if input.chars().count() > MAX_NAME_CHARS {
        return Err(lang.name_too_long_err(MAX_NAME_CHARS));
    }
    Ok(())
}

fn truncate(s: &str, max_width: usize) -> String {
    if display_width(s) <= max_width {
        return s.to_string();
    }
    let mut out = String::new();
    for ch in s.chars() {
        let candidate = format!("{out}{ch}");
        if display_width(&candidate) + 3 > max_width {
            break;
        }
        out = candidate;
    }
    format!("{}...", out.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_language_default_and_validation() {
        assert_eq!(
            detector::parse_numeric_choice_with_default("", 3, Some(1), WizardLang::En).unwrap(),
            1
        );
        assert_eq!(
            detector::parse_numeric_choice_with_default("2", 3, Some(1), WizardLang::En).unwrap(),
            2
        );
        assert_eq!(
            detector::parse_numeric_choice_with_default("3", 3, Some(1), WizardLang::En).unwrap(),
            3
        );
        assert!(detector::parse_numeric_choice_with_default("4", 3, Some(1), WizardLang::En).is_err());
        assert!(detector::parse_numeric_choice_with_default("en", 3, Some(1), WizardLang::En).is_err());
    }

    #[test]
    fn editor_content_parser_strips_headers_and_falls_back_to_default() {
        let untouched = build_editor_template(PERSONA_EDITOR_HINT);
        assert_eq!(
            parse_editor_content(&untouched, PERSONA_EDITOR_HINT, "default_persona"),
            "default_persona"
        );

        let with_custom = format!(
            "{}\nRamah, kalem, suka memberi tips Arch Linux.\n",
            build_editor_template(PERSONA_EDITOR_HINT)
        );
        assert_eq!(
            parse_editor_content(&with_custom, PERSONA_EDITOR_HINT, "default_persona"),
            "Ramah, kalem, suka memberi tips Arch Linux."
        );
    }

    #[test]
    fn temp_file_guard_creates_and_cleans_up_file() {
        let path = env::temp_dir().join(format!(".waish-setup-test-{}.md", std::process::id()));
        {
            let _guard = TempFileGuard::create(&path, MEMORY_EDITOR_HINT).unwrap();
            assert!(path.is_file());
            let raw = fs::read_to_string(&path).unwrap();
            assert!(raw.contains("# --- waish Setup ---"));
            assert!(raw.contains(MEMORY_EDITOR_HINT));
        }
        assert!(!path.exists(), "TempFileGuard must delete temporary file on drop");
    }

    #[test]
    fn truncates_by_width() {
        assert_eq!(truncate("pendek", 40), "pendek");
        let t = truncate("ini deskripsi sifat yang sangat panjang sekali lho", 20);
        assert!(display_width(&t) <= 20, "{t}");
        assert!(t.ends_with("..."));
    }

    #[test]
    fn renders_exact_provider_contextual_key_and_vercel_model_menus() {
        let provider_menu = render_provider_menu_lines(WizardLang::Id);
        assert_eq!(
            provider_menu,
            vec![
                "  Pilih provider AI yang kamu pakai:",
                "  [1] Vercel (Recommend)",
                "  [2] OpenAI",
                "  [3] Anthropic",
                "  [4] OpenRouter",
                "  [5] Google Gemini",
                "  [6] DeepSeek",
                "  [7] Custom (Input Manual)",
            ]
        );
        assert_eq!(WizardLang::Id.provider_prompt(), "Pilih provider [1-7]:");

        assert_eq!(
            WizardLang::Id.api_key_prompt_for_provider(Provider::Vercel),
            "Masukkan Vercel AI Gateway Key / Token:"
        );

        let vercel_model_menu = render_model_menu_lines(Provider::Vercel, WizardLang::Id);
        assert_eq!(
            vercel_model_menu,
            vec![
                "  Pilih model Vercel AI Gateway:",
                "  [1] nvidia/nemotron-3.5-super-vl (Rekomendasi)",
                "  [2] alibaba/qwen3.8-omni-flash",
                "  [3] inclusionai/ling-3.1-flash-1019",
                "  [4] Input model ID manual",
            ]
        );
        assert_eq!(WizardLang::Id.model_choice_prompt(4), "Pilih model [1-4]:");

        let openrouter_model_menu = render_model_menu_lines(Provider::OpenRouter, WizardLang::Id);
        assert_eq!(
            openrouter_model_menu,
            vec![
                "  Pilih model OpenRouter AI Gateway:",
                "  [1] nvidia/nemotron-3.5-super-vl (Rekomendasi)",
                "  [2] Input model ID manual",
            ]
        );
        assert_eq!(WizardLang::Id.model_choice_prompt(2), "Pilih model [1-2]:");
    }
}
