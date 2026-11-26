//! Konfigurasi Provider, Model Preset, Validator API Key, dan Deteksi Lingkungan Sistem.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use super::i18n::WizardLang;
use crate::config::memory::MemoryProfile;

/// Model preset entry in the Setup Wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPreset {
    pub id: &'static str,
    pub badge_en: Option<&'static str>,
    pub badge_id: Option<&'static str>,
    pub badge_ja: Option<&'static str>,
}

impl ModelPreset {
    pub const fn plain(id: &'static str) -> Self {
        Self {
            id,
            badge_en: None,
            badge_id: None,
            badge_ja: None,
        }
    }

    pub const fn with_badge(
        id: &'static str,
        badge_en: &'static str,
        badge_id: &'static str,
        badge_ja: &'static str,
    ) -> Self {
        Self {
            id,
            badge_en: Some(badge_en),
            badge_id: Some(badge_id),
            badge_ja: Some(badge_ja),
        }
    }

    pub fn menu_display(self, lang: WizardLang) -> String {
        let badge = match lang {
            WizardLang::En => self.badge_en,
            WizardLang::Id => self.badge_id,
            WizardLang::Ja => self.badge_ja,
        };
        match badge {
            Some(b) => format!("{} ({b})", self.id),
            None => self.id.to_string(),
        }
    }
}

/// Supported LLM provider choices in the Setup Wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Vercel,
    OpenAi,
    Anthropic,
    OpenRouter,
    Gemini,
    DeepSeek,
    Custom,
}

const VERCEL_PRESETS: &[ModelPreset] = &[
    ModelPreset::with_badge(
        "nvidia/nemotron-3.5-super-vl",
        "Recommended",
        "Rekomendasi",
        "推奨",
    ),
    ModelPreset::plain("alibaba/qwen3.8-omni-flash"),
    ModelPreset::plain("inclusionai/ling-3.1-flash-1019"),
];

const OPENAI_PRESETS: &[ModelPreset] = &[
    ModelPreset::with_badge("gpt-4o-mini", "Recommended", "Rekomendasi", "推奨"),
    ModelPreset::plain("gpt-4o"),
    ModelPreset::plain("o3-mini"),
];

const ANTHROPIC_PRESETS: &[ModelPreset] = &[
    ModelPreset::with_badge(
        "claude-3-5-sonnet-latest",
        "Recommended",
        "Rekomendasi",
        "推奨",
    ),
    ModelPreset::plain("claude-3-5-haiku-latest"),
    ModelPreset::plain("claude-3-7-sonnet-latest"),
];

const OPENROUTER_PRESETS: &[ModelPreset] = &[ModelPreset::with_badge(
    "nvidia/nemotron-3.5-super-vl",
    "Recommended",
    "Rekomendasi",
    "推奨",
)];

const GEMINI_PRESETS: &[ModelPreset] = &[
    ModelPreset::with_badge("gemini-2.5-flash", "Default", "Default", "デフォルト"),
    ModelPreset::plain("gemini-2.5-pro"),
    ModelPreset::plain("gemini-2.0-flash"),
];

const DEEPSEEK_PRESETS: &[ModelPreset] = &[
    ModelPreset::with_badge("deepseek-chat", "Default", "Default", "デフォルト"),
    ModelPreset::plain("deepseek-reasoner"),
];

impl Provider {
    pub const ALL: &'static [Provider] = &[
        Provider::Vercel,
        Provider::OpenAi,
        Provider::Anthropic,
        Provider::OpenRouter,
        Provider::Gemini,
        Provider::DeepSeek,
        Provider::Custom,
    ];

    pub fn from_number(choice: usize) -> Option<Self> {
        Self::ALL.get(choice.checked_sub(1)?).copied()
    }

    pub fn id(self) -> &'static str {
        match self {
            Provider::Vercel => "vercel",
            Provider::OpenAi => "openai",
            Provider::Anthropic => "anthropic",
            Provider::OpenRouter => "openrouter",
            Provider::Gemini => "gemini",
            Provider::DeepSeek => "deepseek",
            Provider::Custom => "custom",
        }
    }

    pub fn menu_label_for(self, lang: WizardLang) -> &'static str {
        match self {
            Provider::Vercel => "Vercel (Recommend)",
            Provider::OpenAi => "OpenAI",
            Provider::Anthropic => "Anthropic",
            Provider::OpenRouter => "OpenRouter",
            Provider::Gemini => "Google Gemini",
            Provider::DeepSeek => "DeepSeek",
            Provider::Custom => lang.custom_provider_label(),
        }
    }

    pub fn display_name_for(self, lang: WizardLang) -> &'static str {
        match self {
            Provider::Custom => lang.custom_provider_label(),
            other => other.display_name(),
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Provider::Vercel => "Vercel AI Gateway",
            Provider::OpenAi => "OpenAI",
            Provider::Anthropic => "Anthropic",
            Provider::OpenRouter => "OpenRouter AI Gateway",
            Provider::Gemini => "Google Gemini",
            Provider::DeepSeek => "DeepSeek",
            Provider::Custom => "Custom (Manual Input)",
        }
    }

    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            Provider::Vercel => Some("https://ai-gateway.vercel.sh/v1"),
            Provider::OpenAi => Some("https://api.openai.com/v1"),
            Provider::Anthropic => Some("https://api.anthropic.com/v1"),
            Provider::OpenRouter => Some("https://openrouter.ai/api/v1"),
            Provider::Gemini => Some("https://generativelanguage.googleapis.com/v1beta"),
            Provider::DeepSeek => Some("https://api.deepseek.com/v1"),
            Provider::Custom => None,
        }
    }

    pub fn preset_options(self) -> &'static [ModelPreset] {
        match self {
            Provider::Vercel => VERCEL_PRESETS,
            Provider::OpenAi => OPENAI_PRESETS,
            Provider::Anthropic => ANTHROPIC_PRESETS,
            Provider::OpenRouter => OPENROUTER_PRESETS,
            Provider::Gemini => GEMINI_PRESETS,
            Provider::DeepSeek => DEEPSEEK_PRESETS,
            Provider::Custom => &[],
        }
    }
}

/// Selected API provider configuration in the wizard.
#[derive(Clone, PartialEq, Eq)]
pub struct DetectedProvider {
    pub provider: Provider,
    pub api_key: String,
    pub base_url: String,
    pub model: String,
}

impl DetectedProvider {
    pub fn masked_key(&self) -> String {
        mask_key(&self.api_key)
    }
}

impl std::fmt::Debug for DetectedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DetectedProvider")
            .field("provider", &self.provider)
            .field("api_key", &self.masked_key())
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish()
    }
}

pub fn validate_api_key(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        Err("API key tidak boleh kosong yaa (｡•́︿•̀｡)")
    } else {
        Ok(trimmed.to_string())
    }
}

pub fn parse_numeric_choice_with_default(
    raw: &str,
    max: usize,
    default: Option<usize>,
    lang: WizardLang,
) -> Result<usize, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        if let Some(def) = default {
            return Ok(def);
        }
    }
    match trimmed.parse::<usize>() {
        Ok(n) if (1..=max).contains(&n) => Ok(n),
        _ => Err(lang.numeric_invalid_err(max)),
    }
}

#[cfg(test)]
pub fn parse_numeric_choice(raw: &str, max: usize) -> Result<usize, String> {
    parse_numeric_choice_with_default(raw, max, None, WizardLang::En)
}

pub fn mask_key(key: &str) -> String {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return "(kosong)".to_string();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() > 8 {
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}****{tail}")
    } else if chars.len() > 2 {
        let tail: String = chars[chars.len() - 2..].iter().collect();
        format!("****{tail}")
    } else {
        "****".to_string()
    }
}

pub fn detect_language() -> String {
    language_from_locale(&env::var("LANG").unwrap_or_default()).to_string()
}

fn language_from_locale(locale: &str) -> &'static str {
    if locale.contains("id_ID") {
        "id"
    } else if locale.contains("ja_JP") {
        "ja"
    } else {
        "en"
    }
}

/// Detected desktop environment / window manager backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopBackend {
    Hyprland,
    Sway,
    Kde,
    Gnome,
    Generic,
}

impl DesktopBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            DesktopBackend::Hyprland => "hyprland",
            DesktopBackend::Sway => "sway",
            DesktopBackend::Kde => "kde",
            DesktopBackend::Gnome => "gnome",
            DesktopBackend::Generic => "generic",
        }
    }

    pub fn display_label(self) -> &'static str {
        match self {
            DesktopBackend::Hyprland => "Hyprland",
            DesktopBackend::Sway => "Sway",
            DesktopBackend::Kde => "KDE Plasma",
            DesktopBackend::Gnome => "GNOME",
            DesktopBackend::Generic => "Generic Linux",
        }
    }

    pub fn from_str_lossy(raw: &str) -> Self {
        let low = raw.trim().to_ascii_lowercase();
        if low.contains("hyprland") {
            DesktopBackend::Hyprland
        } else if low.contains("sway") {
            DesktopBackend::Sway
        } else if low.contains("kde") || low.contains("plasma") {
            DesktopBackend::Kde
        } else if low.contains("gnome") {
            DesktopBackend::Gnome
        } else {
            DesktopBackend::Generic
        }
    }
}

/// Detected system environment profile.
#[derive(Debug, Clone)]
pub struct SystemProfile {
    pub desktop: String,
    pub session_type: Option<String>,
    pub aur_helper: String,
    pub shell_path: String,
    pub os: String,
    pub editor: Option<String>,
    pub sound_server: Option<String>,
}

impl SystemProfile {
    pub fn detect() -> Self {
        Self {
            desktop: detect_desktop(),
            session_type: detect_session_type(),
            aur_helper: detect_aur_helper(),
            shell_path: env_nonempty("SHELL").unwrap_or_else(|| "/bin/sh".to_string()),
            os: detect_os(),
            editor: env_nonempty("EDITOR")
                .or_else(|| env_nonempty("VISUAL"))
                .map(|e| basename(e.split_whitespace().next().unwrap_or(&e))),
            sound_server: detect_sound_server(),
        }
    }

    pub fn window_manager_label(&self) -> String {
        let mut name = DesktopBackend::from_str_lossy(&self.desktop)
            .display_label()
            .to_string();
        if let Some(session) = &self.session_type {
            name.push_str(&format!(" ({})", capitalize(session)));
        }
        name
    }

    pub fn shell_name(&self) -> String {
        basename(&self.shell_path)
    }

    pub fn memory_profile(&self, user_name: &str) -> MemoryProfile {
        MemoryProfile {
            user_name: user_name.to_string(),
            os: self.os.clone(),
            window_manager: self.window_manager_label(),
            shell: self.shell_name(),
            aur_helper: self.aur_helper.clone(),
            text_editor: self.editor.clone(),
            sound_server: self.sound_server.clone(),
        }
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

pub fn detect_session_type() -> Option<String> {
    if let Some(st) = env_nonempty("XDG_SESSION_TYPE") {
        let low = st.to_ascii_lowercase();
        if low == "wayland" || low == "x11" {
            return Some(low);
        }
    }
    if env_nonempty("WAYLAND_DISPLAY").is_some() {
        return Some("wayland".to_string());
    }
    if env_nonempty("DISPLAY").is_some() {
        return Some("x11".to_string());
    }
    env_nonempty("XDG_SESSION_TYPE").map(|s| s.to_ascii_lowercase())
}

pub fn detect_desktop_backend_from_env(
    hypr_sig: Option<&str>,
    sway_sock: Option<&str>,
    xdg_desktop: Option<&str>,
) -> DesktopBackend {
    let xdg_low = xdg_desktop
        .map(str::trim)
        .unwrap_or("")
        .to_ascii_lowercase();

    if hypr_sig.is_some_and(|s| !s.trim().is_empty()) || xdg_low.contains("hyprland") {
        return DesktopBackend::Hyprland;
    }
    if sway_sock.is_some_and(|s| !s.trim().is_empty()) || xdg_low.contains("sway") {
        return DesktopBackend::Sway;
    }
    if xdg_low.contains("kde") || xdg_low.contains("plasma") {
        return DesktopBackend::Kde;
    }
    if xdg_low.contains("gnome") {
        return DesktopBackend::Gnome;
    }
    DesktopBackend::Generic
}

pub fn detect_desktop_backend() -> DesktopBackend {
    let hypr = env_nonempty("HYPRLAND_INSTANCE_SIGNATURE");
    let sway = env_nonempty("SWAYSOCK");
    let xdg = env_nonempty("XDG_CURRENT_DESKTOP");
    detect_desktop_backend_from_env(hypr.as_deref(), sway.as_deref(), xdg.as_deref())
}

pub fn detect_desktop() -> String {
    detect_desktop_backend().as_str().to_string()
}

pub fn detect_hostname() -> String {
    env_nonempty("HOSTNAME")
        .or_else(|| {
            fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "linux".to_string())
}

pub fn detect_aur_helper() -> String {
    ["yay", "paru"]
        .iter()
        .find(|bin| which(bin).is_some())
        .map(|bin| bin.to_string())
        .unwrap_or_else(|| "pacman".to_string())
}

pub fn detect_os() -> String {
    let name = fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|raw| {
            raw.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim().trim_matches('"').to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Linux".to_string());
    format!("{name} {}", env::consts::ARCH)
}

fn detect_sound_server() -> Option<String> {
    if which("pipewire").is_some() {
        Some("PipeWire".to_string())
    } else if which("pulseaudio").is_some() {
        Some("PulseAudio".to_string())
    } else {
        None
    }
}

pub fn which(bin: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| {
            p.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_any_non_empty_api_key_prefix() {
        assert_eq!(validate_api_key("  AQ.12345 \n").unwrap(), "AQ.12345");
        assert_eq!(validate_api_key("AIzaSyTest").unwrap(), "AIzaSyTest");
        assert_eq!(validate_api_key("sk-proj-abc").unwrap(), "sk-proj-abc");
        assert_eq!(validate_api_key("bebas-format-apapun").unwrap(), "bebas-format-apapun");
        assert!(validate_api_key("   \n\t  ").is_err());
    }

    #[test]
    fn maps_provider_numbers_base_urls_and_vercel_model_presets() {
        assert_eq!(Provider::from_number(1), Some(Provider::Vercel));
        assert_eq!(
            Provider::Vercel.menu_label_for(WizardLang::Id),
            "Vercel (Recommend)"
        );
        assert_eq!(
            Provider::Vercel.default_base_url(),
            Some("https://ai-gateway.vercel.sh/v1")
        );
        let vercel_models = Provider::Vercel.preset_options();
        assert_eq!(vercel_models.len(), 3);
        assert_eq!(vercel_models[0].id, "nvidia/nemotron-3.5-super-vl");
        assert_eq!(
            vercel_models[0].menu_display(WizardLang::Id),
            "nvidia/nemotron-3.5-super-vl (Rekomendasi)"
        );
        assert_eq!(vercel_models[1].id, "alibaba/qwen3.8-omni-flash");
        assert_eq!(vercel_models[2].id, "inclusionai/ling-3.1-flash-1019");

        let openrouter_models = Provider::OpenRouter.preset_options();
        assert_eq!(openrouter_models.len(), 1);
        assert_eq!(openrouter_models[0].id, "nvidia/nemotron-3.5-super-vl");
        assert_eq!(
            openrouter_models[0].menu_display(WizardLang::Id),
            "nvidia/nemotron-3.5-super-vl (Rekomendasi)"
        );

        assert_eq!(Provider::from_number(2), Some(Provider::OpenAi));
        assert_eq!(
            Provider::OpenAi.default_base_url(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(Provider::from_number(3), Some(Provider::Anthropic));
        assert_eq!(
            Provider::Anthropic.default_base_url(),
            Some("https://api.anthropic.com/v1")
        );
        assert_eq!(Provider::from_number(4), Some(Provider::OpenRouter));
        assert_eq!(
            Provider::OpenRouter.default_base_url(),
            Some("https://openrouter.ai/api/v1")
        );
        assert_eq!(Provider::from_number(5), Some(Provider::Gemini));
        assert_eq!(
            Provider::Gemini.default_base_url(),
            Some("https://generativelanguage.googleapis.com/v1beta")
        );
        assert_eq!(Provider::from_number(6), Some(Provider::DeepSeek));
        assert_eq!(
            Provider::DeepSeek.default_base_url(),
            Some("https://api.deepseek.com/v1")
        );
        assert_eq!(Provider::from_number(7), Some(Provider::Custom));
        assert_eq!(
            Provider::Custom.menu_label_for(WizardLang::Id),
            "Custom (Input Manual)"
        );
        assert_eq!(Provider::Custom.default_base_url(), None);
        assert_eq!(Provider::from_number(0), None);
        assert_eq!(Provider::from_number(8), None);
    }

    #[test]
    fn validates_numeric_choices_strictly() {
        assert_eq!(parse_numeric_choice("1", 7).unwrap(), 1);
        assert_eq!(parse_numeric_choice(" 7 ", 7).unwrap(), 7);
        assert!(parse_numeric_choice("0", 7).is_err());
        assert!(parse_numeric_choice("8", 7).is_err());
        assert!(parse_numeric_choice("abc", 7).is_err());
        assert!(parse_numeric_choice("", 7).is_err());
    }

    #[test]
    fn masks_keys_safely() {
        assert_eq!(mask_key("AQ.secretkey1234"), "AQ.s****1234");
        assert_eq!(mask_key("pendek"), "****ek");
    }

    #[test]
    fn language_from_lang_env() {
        assert_eq!(language_from_locale("id_ID.UTF-8"), "id");
        assert_eq!(language_from_locale("ja_JP.UTF-8"), "ja");
        assert_eq!(language_from_locale("en_US.UTF-8"), "en");
        assert_eq!(language_from_locale(""), "en");
    }

    #[test]
    fn detects_all_supported_desktop_backends() {
        assert_eq!(
            detect_desktop_backend_from_env(Some("sig123"), None, None),
            DesktopBackend::Hyprland
        );
        assert_eq!(
            detect_desktop_backend_from_env(None, None, Some("Hyprland")),
            DesktopBackend::Hyprland
        );
        assert_eq!(
            detect_desktop_backend_from_env(None, Some("/run/user/1000/sway.sock"), None),
            DesktopBackend::Sway
        );
        assert_eq!(
            detect_desktop_backend_from_env(None, None, Some("KDE")),
            DesktopBackend::Kde
        );
        assert_eq!(
            detect_desktop_backend_from_env(None, None, Some("ubuntu:GNOME")),
            DesktopBackend::Gnome
        );
        assert_eq!(
            detect_desktop_backend_from_env(None, None, Some("i3")),
            DesktopBackend::Generic
        );
        assert_eq!(
            detect_desktop_backend_from_env(None, None, None),
            DesktopBackend::Generic
        );
        assert_eq!(DesktopBackend::Hyprland.as_str(), "hyprland");
        assert_eq!(DesktopBackend::Sway.as_str(), "sway");
        assert_eq!(DesktopBackend::Kde.as_str(), "kde");
        assert_eq!(DesktopBackend::Gnome.as_str(), "gnome");
        assert_eq!(DesktopBackend::Generic.as_str(), "generic");
    }
}
