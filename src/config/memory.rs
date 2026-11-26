//! Loader, template, dan atomic writer `memory.md` (blueprint bagian 3.3).

use std::fs;
use std::path::Path;

use super::settings::DEFAULT_USER_REAL_NAME;
use super::{atomic_write, PRIVATE_FILE_MODE};

/// Placeholder bagian impor eksternal bila wizard Step 6 dilewati.
const IMPORT_PLACEHOLDER: &str =
    "<!-- Teks yang di-paste pengguna saat wizard Step 6 disimpan di sini -->";

/// Data profil awal yang dicatat ke memory bank.
#[derive(Debug, Clone)]
pub struct MemoryProfile {
    pub user_name: String,
    pub os: String,
    pub window_manager: String,
    pub shell: String,
    pub aur_helper: String,
    pub text_editor: Option<String>,
    pub sound_server: Option<String>,
}

/// Merender `memory.md` bersih. `imported` berisi blok Markdown hasil
/// wizard Step 6 (disematkan apa adanya); `session_note` untuk
/// bagian `[catatan_sesi_terakhir]`.
pub fn render_template(profile: &MemoryProfile, imported: Option<&str>, session_note: &str) -> String {
    let mut prefs = vec![format!("- aur_helper: {}", profile.aur_helper)];
    if let Some(sound) = &profile.sound_server {
        prefs.push(format!("- sound_server: {sound}"));
    }
    if let Some(editor) = &profile.text_editor {
        prefs.push(format!("- text_editor: {editor}"));
    }

    let imported_block = imported
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(IMPORT_PLACEHOLDER);

    let user_name = if profile.user_name.trim().is_empty() {
        DEFAULT_USER_REAL_NAME
    } else {
        profile.user_name.trim()
    };

    format!(
        "# Long-Term Memory Bank

[profil]
- user_name: {user}
- os: {os}
- window_manager: {wm}
- shell: {shell}

[preferensi_sistem]
{prefs}

[kebiasaan_interaksi]
- Belum ada catatan. Akan dipelajari dari sesi-sesi berikutnya.

[memori_impor_eksternal]
{imported_block}

[catatan_sesi_terakhir]
- {session_note}
",
        user = user_name,
        os = profile.os,
        wm = profile.window_manager,
        shell = profile.shell,
        prefs = prefs.join("\n"),
    )
}

pub fn load(path: &Path) -> crate::Result<String> {
    fs::read_to_string(path).map_err(|e| format!("Gagal membaca {}: {e}", path.display()).into())
}

/// Menimpa memory bank secara atomik (temp file + rename).
pub fn save(path: &Path, contents: &str) -> crate::Result<()> {
    atomic_write(path, contents, PRIVATE_FILE_MODE)
        .map_err(|e| format!("Gagal menulis {}: {e}", path.display()).into())
}

/// Versi ringkas untuk ditampilkan di layar (`waish -m`):
/// komentar HTML dibuang, tab diganti spasi, baris kosong beruntun dirapatkan.
pub fn summarize(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_comment = false;

    for line in raw.lines() {
        let mut text = line.replace('\t', "    ");

        // Buang komentar HTML (bisa satu baris maupun multi-baris).
        let mut cleaned = String::new();
        let mut rest = text.as_str();
        loop {
            if in_comment {
                match rest.find("-->") {
                    Some(end) => {
                        rest = &rest[end + 3..];
                        in_comment = false;
                    }
                    None => break,
                }
            } else {
                match rest.find("<!--") {
                    Some(start) => {
                        cleaned.push_str(&rest[..start]);
                        rest = &rest[start + 4..];
                        in_comment = true;
                    }
                    None => {
                        cleaned.push_str(rest);
                        break;
                    }
                }
            }
        }
        let had_content = !text.trim().is_empty();
        text = cleaned.trim_end().to_string();

        // Baris yang isinya hanya komentar tidak meninggalkan baris kosong.
        if text.trim().is_empty() {
            if had_content {
                continue;
            }
            if out.last().is_none_or(|l| l.is_empty()) {
                continue;
            }
            out.push(String::new());
        } else {
            out.push(text);
        }
    }

    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> MemoryProfile {
        MemoryProfile {
            user_name: "steve".into(),
            os: "Arch Linux x86_64".into(),
            window_manager: "Hyprland (Wayland)".into(),
            shell: "fish".into(),
            aur_helper: "yay".into(),
            text_editor: Some("micro".into()),
            sound_server: None,
        }
    }

    #[test]
    fn template_contains_all_sections() {
        let out = render_template(&profile(), None, "init");
        for section in [
            "[profil]",
            "[preferensi_sistem]",
            "[kebiasaan_interaksi]",
            "[memori_impor_eksternal]",
            "[catatan_sesi_terakhir]",
        ] {
            assert!(out.contains(section), "missing {section}");
        }
        assert!(out.contains("- user_name: steve"));
        assert!(out.contains(IMPORT_PLACEHOLDER));
    }

    #[test]
    fn imported_memory_replaces_placeholder() {
        let out = render_template(&profile(), Some("\n- Suka kopi\n- Pakai neovim\n"), "init");
        assert!(out.contains("[memori_impor_eksternal]\n- Suka kopi\n- Pakai neovim\n"));
        assert!(!out.contains(IMPORT_PLACEHOLDER));
    }

    #[test]
    fn summary_strips_comments_and_blank_runs() {
        let raw = "# A\n\n\n[x]\n<!-- hidden -->\n- a <!-- c --> b\n<!--\nmulti\n-->\n\n";
        assert_eq!(summarize(raw), vec!["# A", "", "[x]", "- a  b"]);
    }
}
