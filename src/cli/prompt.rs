//! Dynamic Prompt Generator.

use std::borrow::Cow::{self, Borrowed, Owned};
use std::env;
use std::fs;

use colored::{Color, ColoredString, Colorize};
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};

use super::ui::Kaomoji;
use crate::config::settings::{Settings, DEFAULT_BOT_NAME, DEFAULT_USER_ALIAS};

const SLASH_COMMANDS: &[&str] = &[
    "/sh ",
    "/clear",
    "/drop",
    "/model ",
    "/memory",
    "/note ",
    "/telemetry",
    "/uninstall",
    "/help",
    "/exit",
    "/q",
];

pub fn resolve_desktop_name(settings: &Settings) -> String {
    if settings.general.auto_detect_desktop {
        let xdg = env::var("XDG_CURRENT_DESKTOP").ok();
        let host = env::var("HOSTNAME").ok().or_else(|| {
            fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        });
        resolve_desktop_from_sources(
            xdg.as_deref(),
            host.as_deref(),
            &settings.general.fallback_desktop_name,
        )
    } else {
        match settings.general.fallback_desktop_name.trim() {
            "" => "linux".to_string(),
            fb => fb.to_lowercase(),
        }
    }
}

pub fn resolve_desktop_from_sources(
    xdg_current_desktop: Option<&str>,
    hostname: Option<&str>,
    fallback: &str,
) -> String {
    if let Some(first) = xdg_current_desktop
        .and_then(|raw| raw.split(':').map(str::trim).find(|seg| !seg.is_empty()))
    {
        return first.to_lowercase();
    }

    if let Some(host) = hostname.map(str::trim).filter(|s| !s.is_empty()) {
        return host.to_lowercase();
    }

    match fallback.trim() {
        "" => "linux".to_string(),
        fb => fb.to_lowercase(),
    }
}

/// Plain user prompt passed to `rustyline::Editor::readline` for accurate column width calculation.
pub fn format_user_prompt_plain(settings: &Settings) -> String {
    let alias = normalized_user_alias(&settings.general.user_alias);
    let desktop = resolve_desktop_name(settings);
    format!("{alias}@{desktop}  ~> ")
}

pub fn format_user_prompt_colored(settings: &Settings) -> String {
    let alias = normalized_user_alias(&settings.general.user_alias);
    let desktop = resolve_desktop_name(settings);
    let color = parse_color(&settings.style.user_prompt_color, Color::Cyan);
    format!(
        "{}{}{}  {} ",
        alias.color(color).bold(),
        "@".bright_white().bold(),
        desktop.color(color),
        "~>".bright_black()
    )
}

pub fn format_bot_prompt_plain_face(bot_name: &str, kaomoji: &str) -> String {
    let name = normalized_bot_name(bot_name);
    format!("{name}@waish  {kaomoji} ~> ")
}

#[cfg(test)]
pub fn format_bot_prompt_plain(bot_name: &str, mood: Kaomoji) -> String {
    format_bot_prompt_plain_face(bot_name, mood.as_str())
}

pub fn format_bot_prompt_colored_face(settings: &Settings, kaomoji: &str) -> String {
    if !colored::control::SHOULD_COLORIZE.should_colorize() {
        return format_bot_prompt_plain_face(&settings.general.bot_name, kaomoji);
    }
    let name = normalized_bot_name(&settings.general.bot_name);
    let color = parse_color(&settings.style.bot_prompt_color, Color::BrightMagenta);
    let mood_colored = colorize_kaomoji(kaomoji, color);
    format!(
        "{}{}{}  {} {} ",
        name.color(color).bold(),
        "@".bright_white().bold(),
        "waish".color(color),
        mood_colored,
        "~>".bright_black()
    )
}

pub fn print_bot_inline_face(settings: &Settings, kaomoji: &str, message: &str) {
    println!(
        "{}{}",
        format_bot_prompt_colored_face(settings, kaomoji),
        message
    );
}

pub fn print_bot_inline(settings: &Settings, mood: Kaomoji, message: &str) {
    print_bot_inline_face(settings, mood.as_str(), message);
}

pub fn print_bot_header_face(settings: &Settings, kaomoji: &str) {
    println!(
        "{}",
        format_bot_prompt_colored_face(settings, kaomoji).trim_end()
    );
}

pub fn print_bot_header(settings: &Settings, mood: Kaomoji) {
    print_bot_header_face(settings, mood.as_str());
}

fn normalized_user_alias(raw: &str) -> &str {
    match raw.trim() {
        "" => DEFAULT_USER_ALIAS,
        trimmed => trimmed,
    }
}

fn normalized_bot_name(raw: &str) -> &str {
    match raw.trim() {
        "" => DEFAULT_BOT_NAME,
        trimmed => trimmed,
    }
}

fn parse_color(name: &str, fallback: Color) -> Color {
    match name.trim().to_ascii_lowercase().as_str() {
        "magenta" | "pink" | "bright_magenta" | "purple" => Color::BrightMagenta,
        "cyan" | "bright_cyan" => Color::Cyan,
        "green" => Color::Green,
        "blue" => Color::Blue,
        "yellow" => Color::Yellow,
        "red" => Color::Red,
        "white" | "bright_white" => Color::BrightWhite,
        _ => fallback,
    }
}

fn colorize_kaomoji(text: &str, base: Color) -> ColoredString {
    match base {
        Color::Magenta | Color::BrightMagenta => text.bright_magenta(),
        Color::Cyan => text.bright_cyan(),
        Color::Green => text.bright_green(),
        Color::Blue => text.bright_blue(),
        Color::Yellow => text.bright_yellow(),
        Color::Red => text.bright_red(),
        _ => text.color(base),
    }
}

/// `rustyline` helper for prompt syntax highlighting and slash-command completion.
#[derive(Debug, Clone)]
pub struct PromptHelper {
    colored_prompt: String,
}

impl PromptHelper {
    pub fn new(settings: &Settings) -> Self {
        Self {
            colored_prompt: format_user_prompt_colored(settings),
        }
    }
}

impl Completer for PromptHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let slice = &line[..pos];
        let trimmed_start = slice.trim_start();
        let leading_ws = slice.len() - trimmed_start.len();

        if !trimmed_start.starts_with('/') || trimmed_start.contains(char::is_whitespace) {
            return Ok((0, Vec::new()));
        }

        let candidates = SLASH_COMMANDS
            .iter()
            .filter(|cmd| cmd.starts_with(trimmed_start))
            .map(|cmd| Pair {
                display: cmd.trim_end().to_string(),
                replacement: (*cmd).to_string(),
            })
            .collect();

        Ok((leading_ws, candidates))
    }
}

impl Hinter for PromptHelper {
    type Hint = String;
}

impl Validator for PromptHelper {}

impl Highlighter for PromptHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        default: bool,
    ) -> Cow<'b, str> {
        if default && colored::control::SHOULD_COLORIZE.should_colorize() {
            Owned(self.colored_prompt.clone())
        } else {
            Borrowed(prompt)
        }
    }
}

impl Helper for PromptHelper {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_resolution_prioritizes_xdg_then_hostname_then_fallback() {
        assert_eq!(
            resolve_desktop_from_sources(Some("Hyprland"), Some("archbox"), "linux"),
            "hyprland"
        );
        assert_eq!(
            resolve_desktop_from_sources(Some("WLROOTS:Hyprland"), Some("archbox"), "linux"),
            "wlroots"
        );
        assert_eq!(
            resolve_desktop_from_sources(Some("   "), Some("ArchHost"), "hyprland"),
            "archhost"
        );
        assert_eq!(
            resolve_desktop_from_sources(None, None, "Hyprland"),
            "hyprland"
        );
        assert_eq!(resolve_desktop_from_sources(None, None, ""), "linux");
    }

    #[test]
    fn formats_plain_user_and_bot_prompts() {
        let mut settings = Settings::default();
        settings.general.bot_name = "alice".to_string();
        settings.general.user_alias = "me".to_string();
        settings.general.auto_detect_desktop = false;
        settings.general.fallback_desktop_name = "hyprland".to_string();

        assert_eq!(format_user_prompt_plain(&settings), "me@hyprland  ~> ");
        assert_eq!(
            format_bot_prompt_plain(&settings.general.bot_name, Kaomoji::Ready),
            "alice@waish  ( •̀ ω •́ )✧ ~> "
        );
        assert_eq!(
            format_bot_prompt_plain(&settings.general.bot_name, Kaomoji::Farewell),
            "alice@waish  ( ´ ▽ ` )ﾉ ~> "
        );
    }

    #[test]
    fn completes_slash_commands() {
        let helper = PromptHelper::new(&Settings::default());
        let history = rustyline::history::DefaultHistory::new();
        let ctx = Context::new(&history);

        let (start, matches) = helper.complete("/mo", 3, &ctx).unwrap();
        assert_eq!(start, 0);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].replacement, "/model ");
    }
}
