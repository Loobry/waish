//! Atomic file read, write, in-place replace, and Shadow-Git rollback operations.

use std::fs;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cli::ui;
use crate::config::settings::Settings;
use crate::engine::shadow_git::{ShadowGit, SnapshotOutcome};
use crate::executor::pty;
use crate::executor::safety::{self, SafetyVerdict};
use crate::tools::desktop;
use crate::wizard::i18n::WizardLang;

pub const DEFAULT_READ_MAX_LINES: usize = 250;

/// Arguments for `read_file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFileArgs {
    pub path: String,
    #[serde(default)]
    pub max_lines: Option<usize>,
}

/// Arguments for `write_file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteFileArgs {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub requires_root: bool,
}

/// Arguments for `replace_in_file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaceInFileArgs {
    pub path: String,
    pub target: String,
    pub replacement: String,
    #[serde(default)]
    pub requires_root: bool,
}

/// Arguments for `rollback_config`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackConfigArgs {
    pub path: String,
    #[serde(default)]
    pub steps_back: Option<u32>,
    #[serde(default)]
    pub requires_root: bool,
}

#[cfg(test)]
pub fn expand_tilde(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(trimmed));
    }
    if let Some((rest, home)) = trimmed.strip_prefix("~/").zip(dirs::home_dir()) {
        return home.join(rest);
    }
    PathBuf::from(trimmed)
}

fn render_file_action_box(
    action: &str,
    path: &Path,
    requires_root: bool,
    detail: &str,
    lang: WizardLang,
) -> String {
    let target = ui::tilde(path);
    let lines = lang.file_action_lines(action, &target, detail, requires_root);
    ui::render_plain_box(lang.action_box_title(), &lines, 2)
}

#[cfg(test)]
pub fn execute_read_file(args: &ReadFileArgs) -> crate::Result<String> {
    execute_read_file_for_user(args, crate::config::settings::DEFAULT_USER_REAL_NAME)
}

pub fn execute_read_file_for_user(
    args: &ReadFileArgs,
    companion_user_name: &str,
) -> crate::Result<String> {
    let target = super::runner::sanitize_path_for_user(&args.path, companion_user_name);
    if !target.exists() {
        return Err(format!("File tidak ditemukan: {}", target.display()).into());
    }
    if target.is_dir() {
        return Err(format!("Target adalah direktori, bukan file: {}", target.display()).into());
    }

    let max_lines = args.max_lines.unwrap_or(DEFAULT_READ_MAX_LINES).max(1);
    let raw = fs::read_to_string(&target)
        .map_err(|e| format!("Gagal membaca {}: {e}", target.display()))?;

    let all_lines: Vec<&str> = raw.lines().collect();
    let total = all_lines.len();
    let shown = total.min(max_lines);

    let mut out = String::new();
    out.push_str(&format!(
        "path: {}\ntotal_lines: {total} (menampilkan {shown} baris)\n---\n",
        target.display()
    ));
    for (idx, line) in all_lines.iter().take(shown).enumerate() {
        out.push_str(&format!("{:>4} | {line}\n", idx + 1));
    }
    if total > shown {
        out.push_str(&format!(
            "... [{} baris berikutnya tidak ditampilkan; naikkan `max_lines` bila perlu]\n",
            total - shown
        ));
    }

    Ok(out)
}

pub fn execute_write_file(
    args: &WriteFileArgs,
    settings: &Settings,
    shadow_git: &ShadowGit,
) -> crate::Result<String> {
    let target = super::runner::sanitize_path_for_user(&args.path, &settings.general.user_real_name);

    match safety::inspect_path_and_confirm(&target, "write_file", settings)? {
        SafetyVerdict::Safe | SafetyVerdict::Confirmed { .. } => {}
        SafetyVerdict::Blocked { risk } => {
            return Err(format!(
                "cancelled_by_user: write_file ke {} diblokir Safety Gatekeeper ({risk}) dan ditolak pengguna.",
                target.display()
            )
            .into());
        }
    }

    let lang = WizardLang::from_code(&settings.general.language);
    let line_count = args.content.lines().count();
    let detail = match lang {
        WizardLang::En => format!("{line_count} lines ({} bytes)", args.content.len()),
        WizardLang::Id => format!("{line_count} baris ({} byte)", args.content.len()),
        WizardLang::Ja => format!("{line_count} 行 ({} バイト)", args.content.len()),
    };
    println!();
    println!(
        "{}",
        render_file_action_box("write_file", &target, args.requires_root, &detail, lang)
    );
    println!();

    let snap_note = record_shadow_snapshot(shadow_git, &target);
    write_content_dispatch(&target, &args.content, args.requires_root, settings)?;

    let waybar_note = desktop::reload_waybar_if_applicable(&target)
        .map(|m| format!("\nwaybar_hook: {m}"))
        .unwrap_or_default();

    Ok(format!(
        "status: success\naction: write_file\npath: {}\nbytes: {}\nshadow_git: {snap_note}{waybar_note}",
        target.display(),
        args.content.len()
    ))
}

pub fn execute_replace_in_file(
    args: &ReplaceInFileArgs,
    settings: &Settings,
    shadow_git: &ShadowGit,
) -> crate::Result<String> {
    if args.target.is_empty() {
        return Err("field `target` tidak boleh kosong pada replace_in_file.".into());
    }

    let path = super::runner::sanitize_path_for_user(&args.path, &settings.general.user_real_name);
    if !path.exists() {
        return Err(format!("File tidak ditemukan: {}", path.display()).into());
    }

    match safety::inspect_path_and_confirm(&path, "replace_in_file", settings)? {
        SafetyVerdict::Safe | SafetyVerdict::Confirmed { .. } => {}
        SafetyVerdict::Blocked { risk } => {
            return Err(format!(
                "cancelled_by_user: replace_in_file pada {} diblokir Safety Gatekeeper ({risk}) dan ditolak pengguna.",
                path.display()
            )
            .into());
        }
    }

    let original = fs::read_to_string(&path)
        .map_err(|e| format!("Gagal membaca {} sebelum replace: {e}", path.display()))?;

    let match_count = original.matches(&args.target).count();
    if match_count == 0 {
        return Err(format!(
            "Teks target tidak ditemukan di {}. Pastikan whitespace/indentasi sesuai dengan isi file (gunakan read_file dulu).",
            path.display()
        )
        .into());
    }

    let updated = original.replace(&args.target, &args.replacement);

    let lang = WizardLang::from_code(&settings.general.language);
    let detail = match lang {
        WizardLang::En => format!("replace {match_count} matching block(s)"),
        WizardLang::Id => format!("ganti {match_count} blok teks yang cocok"),
        WizardLang::Ja => format!("{match_count} 箇所のテキストブロックを置換"),
    };
    println!();
    println!(
        "{}",
        render_file_action_box("replace_in_file", &path, args.requires_root, &detail, lang)
    );
    println!();

    let snap_note = record_shadow_snapshot(shadow_git, &path);
    write_content_dispatch(&path, &updated, args.requires_root, settings)?;

    let waybar_note = desktop::reload_waybar_if_applicable(&path)
        .map(|m| format!("\nwaybar_hook: {m}"))
        .unwrap_or_default();

    Ok(format!(
        "status: success\naction: replace_in_file\npath: {}\nreplacements: {match_count}\nshadow_git: {snap_note}{waybar_note}",
        path.display()
    ))
}

pub fn execute_rollback_config(
    args: &RollbackConfigArgs,
    settings: &Settings,
    shadow_git: &ShadowGit,
) -> crate::Result<String> {
    let path = super::runner::sanitize_path_for_user(&args.path, &settings.general.user_real_name);
    let steps = args.steps_back.unwrap_or(1).max(1);

    match safety::inspect_path_and_confirm(&path, "rollback_config", settings)? {
        SafetyVerdict::Safe | SafetyVerdict::Confirmed { .. } => {}
        SafetyVerdict::Blocked { risk } => {
            return Err(format!(
                "cancelled_by_user: rollback_config pada {} diblokir Safety Gatekeeper ({risk}) dan ditolak pengguna.",
                path.display()
            )
            .into());
        }
    }

    let rev = shadow_git.fetch_revision(&path, steps)?;

    let lang = WizardLang::from_code(&settings.general.language);
    let detail = match lang {
        WizardLang::En => format!("restore {steps} step(s) back from {}", rev.source_label),
        WizardLang::Id => format!("pulihkan {steps} langkah mundur dari {}", rev.source_label),
        WizardLang::Ja => format!("{} から {steps} ステップ前へ復元", rev.source_label),
    };
    println!();
    println!(
        "{}",
        render_file_action_box("rollback_config", &path, args.requires_root, &detail, lang)
    );
    println!();

    write_content_dispatch(&path, &rev.content, args.requires_root, settings)?;

    let waybar_note = desktop::reload_waybar_if_applicable(&path)
        .map(|m| format!("\nwaybar_hook: {m}"))
        .unwrap_or_default();

    Ok(format!(
        "status: success\naction: rollback_config\npath: {}\nsource: {}\nbytes_restored: {}{waybar_note}",
        path.display(),
        rev.source_label,
        rev.content.len()
    ))
}

fn record_shadow_snapshot(shadow_git: &ShadowGit, target: &Path) -> String {
    match shadow_git.snapshot_before_edit(target) {
        Ok(SnapshotOutcome::NewFile) => "file baru (belum ada versi sebelumnya)".to_string(),
        Ok(SnapshotOutcome::Committed { commit_hash, .. }) => {
            format!("snapshot tercatat di commit {commit_hash}")
        }
        Ok(SnapshotOutcome::Unchanged { .. }) => {
            "snapshot sudah mutakhir (tidak ada diff baru)".to_string()
        }
        Err(err) => format!("peringatan snapshot dilewati: {err}"),
    }
}

fn write_content_dispatch(
    target: &Path,
    content: &str,
    requires_root: bool,
    settings: &Settings,
) -> crate::Result<()> {
    if requires_root {
        let out = pty::write_file_as_root_via_pty(
            target,
            content,
            &settings.tools.shell_path,
            &settings.general.bot_name,
        )?;
        if !out.succeeded {
            return Err(format!(
                "Penulisan root via `sudo tee` gagal (exit {}): {}",
                out.exit_code,
                out.clean_output.trim()
            )
            .into());
        }
        return Ok(());
    }

    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Gagal membuat direktori {}: {e}", parent.display()))?;
    }

    let mode = {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(target)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o644)
    };

    crate::config::atomic_write(target, content, mode)
        .map_err(|e| format!("Gagal menulis file {}: {e}", target.display()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tilde_to_home_dir() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_tilde("~"), home);
            assert_eq!(expand_tilde("~/.config/waybar/style.css"), home.join(".config/waybar/style.css"));
        }
        assert_eq!(expand_tilde("/etc/environment"), PathBuf::from("/etc/environment"));
    }

    #[test]
    fn write_replace_read_and_rollback_full_lifecycle() {
        let root = std::env::temp_dir().join(format!("waish-fileops-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let target = root.join("app.conf");
        let shadow = ShadowGit::new(root.join("shadow_repo"));
        let settings = Settings::default();

        let w1 = execute_write_file(
            &WriteFileArgs {
                path: target.to_string_lossy().into_owned(),
                content: "theme = \"dark\"\nfont = 12\n".to_string(),
                requires_root: false,
            },
            &settings,
            &shadow,
        )
        .unwrap();
        assert!(w1.contains("status: success"));

        let r1 = execute_replace_in_file(
            &ReplaceInFileArgs {
                path: target.to_string_lossy().into_owned(),
                target: "font = 12".to_string(),
                replacement: "font = 14".to_string(),
                requires_root: false,
            },
            &settings,
            &shadow,
        )
        .unwrap();
        assert!(r1.contains("replacements: 1"));

        let read_out = execute_read_file(&ReadFileArgs {
            path: target.to_string_lossy().into_owned(),
            max_lines: Some(10),
        })
        .unwrap();
        assert!(read_out.contains("font = 14"));

        let rb = execute_rollback_config(
            &RollbackConfigArgs {
                path: target.to_string_lossy().into_owned(),
                steps_back: Some(1),
                requires_root: false,
            },
            &settings,
            &shadow,
        )
        .unwrap();
        assert!(rb.contains("status: success"));
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "theme = \"dark\"\nfont = 12\n"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn blocks_critical_system_paths_in_non_tty_safety_gatekeeper() {
        let settings = Settings::default();
        let shadow = ShadowGit::new(std::env::temp_dir().join("waish-unused-shadow"));
        let res = execute_write_file(
            &WriteFileArgs {
                path: "/etc/fstab".to_string(),
                content: "broken".to_string(),
                requires_root: true,
            },
            &settings,
            &shadow,
        );
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("cancelled_by_user"));
    }
}
