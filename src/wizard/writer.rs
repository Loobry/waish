//! Atomic disk writer untuk hasil wizard (`config.toml`, `persona.md`, `memory.md`).

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use colored::Colorize;

use super::i18n::WizardLang;
use super::WizardDraft;
use crate::cli::ui::{self, Kaomoji};
use crate::config::memory;
use crate::config::persona;
use crate::config::settings::{
    Api, DesktopConfig, General, Settings, Tools, DEFAULT_PACKAGE_MANAGER, DEFAULT_USER_ALIAS,
};
use crate::config::{
    sync_dir, write_new_synced, Paths, CONFIG_FILE, MEMORY_FILE, PERSONA_FILE, PRIVATE_DIR_MODE,
    PRIVATE_FILE_MODE,
};

const STAGING_PREFIX: &str = ".waish-setup-";

pub fn build_settings(draft: &WizardDraft) -> Settings {
    Settings {
        general: General {
            bot_name: draft.bot_name.clone(),
            user_alias: DEFAULT_USER_ALIAS.to_string(),
            user_real_name: draft.user_name.clone(),
            language: draft.language.clone(),
            auto_detect_desktop: true,
            fallback_desktop_name: draft.system.desktop.clone(),
        },
        api: Api {
            provider: draft.provider.provider.id().to_string(),
            base_url: draft.provider.base_url.clone(),
            api_key: draft.provider.api_key.clone(),
            model: draft.provider.model.clone(),
            ..Api::default()
        },
        tools: Tools {
            aur_helper: draft.system.aur_helper.clone(),
            package_manager: DEFAULT_PACKAGE_MANAGER.to_string(),
            shell_path: draft.system.shell_path.clone(),
        },
        desktop: DesktopConfig {
            backend: draft.system.desktop.clone(),
        },
        ..Settings::default()
    }
}

pub fn commit(draft: &WizardDraft, paths: &Paths) -> crate::Result<Settings> {
    let lang = WizardLang::from_code(&draft.language);

    let settings = build_settings(draft);
    let config_toml = settings.to_toml()?;
    let persona_md = persona::render(&draft.bot_name, &draft.language, &draft.personality);
    let memory_md = memory::render_template(
        &draft.system.memory_profile(&draft.user_name),
        draft.imported_memory.as_deref(),
        lang.initial_memory_session_note(),
    );

    // Commit config.toml last as the atomic setup-completion sentinel.
    let files = [
        (PERSONA_FILE, persona_md.as_str()),
        (MEMORY_FILE, memory_md.as_str()),
        (CONFIG_FILE, config_toml.as_str()),
    ];

    let backups = flush(paths, &files)
        .map_err(|e| format!("Failed to save configuration to {}: {e}", paths.dir.display()))?;

    let dir_display = ui::tilde(&paths.dir);
    println!();
    println!(
        "{} {}",
        Kaomoji::Success.as_str().magenta(),
        lang.setup_complete_message(&draft.user_name, &dir_display)
    );
    for (name, _) in &files {
        println!("    {} {}", "+--".magenta(), name);
    }
    for backup in backups {
        println!(
            "    {} {}",
            "[i]".bright_black(),
            lang.backup_notice(&ui::tilde(&backup)).bright_black()
        );
    }

    Ok(settings)
}

fn flush(paths: &Paths, files: &[(&str, &str)]) -> io::Result<Vec<PathBuf>> {
    let parent = paths
        .dir
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "direktori config tanpa parent"))?;

    fs::create_dir_all(parent)?;
    cleanup_stale_staging(parent);

    let staging = parent.join(format!("{STAGING_PREFIX}{}", std::process::id()));
    let guard = StagingGuard::create(staging)?;

    for (name, contents) in files {
        write_new_synced(&guard.path.join(name), contents, PRIVATE_FILE_MODE)?;
    }
    sync_dir(&guard.path)?;

    let mut backups = Vec::new();

    match fs::symlink_metadata(&paths.dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::rename(&guard.path, &paths.dir)?;
            guard.disarm();
        }
        Err(e) => return Err(e),
        Ok(_) => {
            for (name, _) in files {
                let target = paths.dir.join(name);
                if *name != CONFIG_FILE && target.exists() {
                    let backup = paths.dir.join(format!("{name}.bak"));
                    fs::copy(&target, &backup)?;
                    backups.push(backup);
                }
                fs::rename(guard.path.join(name), &target)?;
            }
        }
    }

    sync_dir(&paths.dir)?;
    sync_dir(parent)?;
    Ok(backups)
}

fn cleanup_stale_staging(parent: &Path) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(STAGING_PREFIX)
            && entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

struct StagingGuard {
    path: PathBuf,
    armed: std::cell::Cell<bool>,
}

impl StagingGuard {
    fn create(path: PathBuf) -> io::Result<Self> {
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        DirBuilder::new().mode(PRIVATE_DIR_MODE).create(&path)?;
        Ok(Self {
            path,
            armed: std::cell::Cell::new(true),
        })
    }

    fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if self.armed.get() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::persona::Personality;
    use crate::wizard::detector::{DetectedProvider, Provider, SystemProfile};
    use std::os::unix::fs::PermissionsExt;

    fn draft(memory: Option<&str>) -> WizardDraft {
        WizardDraft {
            language: "id".into(),
            provider: DetectedProvider {
                provider: Provider::Anthropic,
                api_key: concat!("sk-ant-", "api03-testkey1234").into(),
                base_url: "https://api.anthropic.com/v1".into(),
                model: "claude-3-5-sonnet-latest".into(),
            },
            bot_name: "alice".into(),
            user_name: "steve".into(),
            personality: Personality::Default,
            imported_memory: memory.map(str::to_string),
            system: SystemProfile {
                desktop: "hyprland".into(),
                session_type: Some("wayland".into()),
                aur_helper: "yay".into(),
                shell_path: "/usr/bin/fish".into(),
                os: "Arch Linux x86_64".into(),
                editor: Some("micro".into()),
                sound_server: Some("PipeWire".into()),
            },
        }
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("waish-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fresh_commit_writes_all_files_with_private_modes() {
        let root = temp_root("fresh");
        let paths = Paths::from_dir(root.join("waish"));

        let settings = commit(&draft(Some("- Suka kopi")), &paths).unwrap();
        assert_eq!(settings.api.provider, "anthropic");

        for p in [&paths.config, &paths.persona, &paths.memory] {
            assert!(p.is_file(), "{} missing", p.display());
            assert_eq!(fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(fs::metadata(&paths.dir).unwrap().permissions().mode() & 0o777, 0o700);

        let loaded = Settings::load(&paths.config).unwrap();
        assert_eq!(loaded.general.bot_name, "alice");
        assert_eq!(loaded.tools.aur_helper, "yay");
        assert_eq!(loaded.desktop.backend, "hyprland");
        assert!(fs::read_to_string(&paths.memory).unwrap().contains("- Suka kopi"));

        // Tidak ada sisa staging.
        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(STAGING_PREFIX))
            .collect();
        assert!(leftovers.is_empty());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn existing_dir_is_updated_and_old_memory_backed_up() {
        let root = temp_root("existing");
        let paths = Paths::from_dir(root.join("waish"));
        fs::create_dir_all(&paths.dir).unwrap();
        fs::write(&paths.memory, "memori lama").unwrap();
        // Sisa staging dari proses yang mati paksa.
        fs::create_dir_all(root.join(format!("{STAGING_PREFIX}99999"))).unwrap();

        commit(&draft(None), &paths).unwrap();

        assert!(paths.config.is_file());
        assert_eq!(fs::read_to_string(paths.dir.join("memory.md.bak")).unwrap(), "memori lama");
        assert!(!root.join(format!("{STAGING_PREFIX}99999")).exists());
        assert!(!root.join(format!("{STAGING_PREFIX}{}", std::process::id())).exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn vercel_gateway_draft_saves_clean_config_toml() {
        let root = temp_root("vercel");
        let paths = Paths::from_dir(root.join("waish"));
        let mut d = draft(None);
        d.provider = DetectedProvider {
            provider: Provider::Vercel,
            api_key: "vercel-gateway-token-example".into(),
            base_url: Provider::Vercel.default_base_url().unwrap().into(),
            model: Provider::Vercel.preset_options()[0].id.into(),
        };

        commit(&d, &paths).unwrap();
        let loaded = Settings::load(&paths.config).unwrap();
        assert_eq!(loaded.api.provider, "vercel");
        assert_eq!(loaded.api.base_url, "https://ai-gateway.vercel.sh/v1");
        assert_eq!(loaded.api.model, "nvidia/nemotron-3.5-super-vl");
        assert_eq!(loaded.api.api_key, "vercel-gateway-token-example");

        // Verifikasi juga preset utama OpenRouter tersimpan rapi ke config.toml.
        let root_or = temp_root("openrouter");
        let paths_or = Paths::from_dir(root_or.join("waish"));
        d.provider = DetectedProvider {
            provider: Provider::OpenRouter,
            api_key: "openrouter-token-example".into(),
            base_url: Provider::OpenRouter.default_base_url().unwrap().into(),
            model: Provider::OpenRouter.preset_options()[0].id.into(),
        };
        commit(&d, &paths_or).unwrap();
        let loaded_or = Settings::load(&paths_or.config).unwrap();
        assert_eq!(loaded_or.api.provider, "openrouter");
        assert_eq!(loaded_or.api.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(loaded_or.api.model, "nvidia/nemotron-3.5-super-vl");
        assert_eq!(loaded_or.api.api_key, "openrouter-token-example");

        fs::remove_dir_all(&root_or).unwrap();

        fs::remove_dir_all(&root).unwrap();
    }
}
