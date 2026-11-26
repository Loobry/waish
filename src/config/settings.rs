//! Serializer / deserializer `config.toml`.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::wizard::detector::{self, DesktopBackend};

pub const DEFAULT_BOT_NAME: &str = "alice";
pub const DEFAULT_USER_ALIAS: &str = "me";
pub const DEFAULT_USER_REAL_NAME: &str = "steve";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub general: General,
    pub api: Api,
    pub behavior: Behavior,
    pub tools: Tools,
    pub desktop: DesktopConfig,
    pub style: Style,
}

/// Konfigurasi backend window manager / desktop (`[desktop]` di `config.toml`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopConfig {
    pub backend: String,
}

impl DesktopConfig {
    pub fn effective_backend(&self) -> DesktopBackend {
        match self.backend.trim() {
            "" => detector::detect_desktop_backend(),
            trimmed => DesktopBackend::from_str_lossy(trimmed),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub bot_name: String,
    pub user_alias: String,
    pub user_real_name: String,
    pub language: String,
    pub auto_detect_desktop: bool,
    pub fallback_desktop_name: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Api {
    pub provider: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f32,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Behavior {
    pub auto_confirm_safe: bool,
    pub safety_interceptor: bool,
    pub notify_on_long_task: bool,
    pub long_task_threshold_secs: u64,
}

pub const DEFAULT_PACKAGE_MANAGER: &str = "pacman";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Tools {
    pub aur_helper: String,
    pub package_manager: String,
    pub shell_path: String,
}

impl Tools {
    pub fn preferred_package_manager(&self) -> &str {
        let pm = self.package_manager.trim();
        if !pm.is_empty() {
            return pm;
        }
        let aur = self.aur_helper.trim();
        if !aur.is_empty() {
            aur
        } else {
            DEFAULT_PACKAGE_MANAGER
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Style {
    pub bracket_color: String,
    pub user_prompt_color: String,
    pub bot_prompt_color: String,
}

impl Default for General {
    fn default() -> Self {
        Self {
            bot_name: DEFAULT_BOT_NAME.to_string(),
            user_alias: DEFAULT_USER_ALIAS.to_string(),
            user_real_name: DEFAULT_USER_REAL_NAME.to_string(),
            language: "en".to_string(),
            auto_detect_desktop: true,
            fallback_desktop_name: "linux".to_string(),
        }
    }
}

impl Default for Api {
    fn default() -> Self {
        Self {
            provider: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            temperature: 0.7,
            timeout_secs: 60,
        }
    }
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("temperature", &self.temperature)
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            auto_confirm_safe: true,
            safety_interceptor: true,
            notify_on_long_task: true,
            long_task_threshold_secs: 10,
        }
    }
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            aur_helper: DEFAULT_PACKAGE_MANAGER.to_string(),
            package_manager: DEFAULT_PACKAGE_MANAGER.to_string(),
            shell_path: "/bin/sh".to_string(),
        }
    }
}

impl Default for Style {
    fn default() -> Self {
        Self {
            bracket_color: "magenta".to_string(),
            user_prompt_color: "cyan".to_string(),
            bot_prompt_color: "magenta".to_string(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> crate::Result<Self> {
        let raw = fs::read_to_string(path)
            .map_err(|e| format!("Gagal membaca {}: {e}", path.display()))?;
        Self::from_toml(&raw).map_err(|e| format!("config.toml tidak valid: {e}").into())
    }

    pub fn from_toml(raw: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(raw)
    }

    pub fn to_toml(&self) -> crate::Result<String> {
        let body = toml::to_string_pretty(self)?;
        let generated = chrono::Local::now().format("%Y-%m-%d %H:%M");
        Ok(format!(
            "# waish runtime configuration\n\
             # Dibuat otomatis oleh setup wizard pada {generated}.\n\
             # Edit dengan: waish -c\n\n{body}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_values() {
        let mut s = Settings::default();
        s.general.bot_name = "alice".into();
        s.general.user_real_name = "steve \"the\" user".into();
        s.api.api_key = "sk-ant-secret".into();
        s.desktop.backend = "hyprland".into();
        let raw = s.to_toml().unwrap();
        assert!(raw.contains("[desktop]"));
        assert!(raw.contains("backend = \"hyprland\""));
        let back = Settings::from_toml(&raw).unwrap();
        assert_eq!(back.general.user_real_name, s.general.user_real_name);
        assert_eq!(back.api.api_key, "sk-ant-secret");
        assert_eq!(back.desktop.backend, "hyprland");
        assert_eq!(back.desktop.effective_backend(), DesktopBackend::Hyprland);
        assert!(!format!("{:?}", back.api).contains("secret"));
    }

    #[test]
    fn partial_config_uses_defaults() {
        let s = Settings::from_toml("[general]\nbot_name = \"rin\"\n").unwrap();
        assert_eq!(s.general.bot_name, "rin");
        assert_eq!(s.api.timeout_secs, 60);
        assert!(s.behavior.safety_interceptor);
    }
}
