//! Ephemeral Scratchpad Memory (`~/.config/waish/scratchpad.json`).

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{atomic_write, PRIVATE_DIR_MODE, PRIVATE_FILE_MODE};

pub const PACKAGE_MANAGER_PREF_TAG: &str = "pref:package_manager";
pub const RATE_LIMIT_STATUS_TAG: &str = "status:rate_limit";
pub const FAST_PATH_LOG_TAG: &str = "fast_path";

/// Single ephemeral scratchpad note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScratchpadNote {
    pub id: u64,
    pub tag: String,
    pub content: String,
    pub created_at: String,
}

/// JSON-persisted scratchpad container.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scratchpad {
    pub next_id: u64,
    pub notes: Vec<ScratchpadNote>,
}

impl Scratchpad {
    pub fn load(path: &Path) -> crate::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)
            .map_err(|e| format!("Gagal membaca scratchpad {}: {e}", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let mut parsed: Self = serde_json::from_str(&raw)
            .map_err(|e| format!("Format scratchpad.json tidak valid: {e}"))?;
        parsed.notes.retain(|n| n.tag != RATE_LIMIT_STATUS_TAG);
        Ok(parsed)
    }

    pub fn save(&self, path: &Path) -> crate::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.exists()) {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(
                    parent,
                    fs::Permissions::from_mode(PRIVATE_DIR_MODE),
                );
            }
        }
        let json = serde_json::to_string_pretty(self)?;
        atomic_write(path, &format!("{json}\n"), PRIVATE_FILE_MODE)
            .map_err(|e| format!("Gagal menyimpan scratchpad {}: {e}", path.display()).into())
    }

    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    pub fn add_note(&mut self, tag: &str, content: &str) -> ScratchpadNote {
        self.next_id = self.next_id.saturating_add(1).max(1);
        let clean_tag = match tag.trim() {
            "" => "memo".to_string(),
            t => t.to_string(),
        };
        let note = ScratchpadNote {
            id: self.next_id,
            tag: clean_tag,
            content: content.trim().to_string(),
            created_at: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
        };
        self.notes.push(note.clone());
        note
    }

    pub fn remove_note(&mut self, id: u64) -> bool {
        let before = self.notes.len();
        self.notes.retain(|n| n.id != id);
        self.notes.len() < before
    }

    pub fn clear(&mut self) -> usize {
        let count = self.notes.len();
        self.notes.clear();
        count
    }

    pub fn render_box_lines(&self) -> Vec<String> {
        if self.notes.is_empty() {
            return vec!["(scratchpad masih kosong)".to_string()];
        }
        self.notes
            .iter()
            .map(|n| format!("#{} [{}] {} ({})", n.id, n.tag, n.content, n.created_at))
            .collect()
    }

    pub fn set_package_manager(&mut self, pm: &str) {
        let normalized = normalize_package_manager(pm);
        self.notes.retain(|n| n.tag != PACKAGE_MANAGER_PREF_TAG);
        let label = match normalized {
            "pacman" => "sudo pacman (WAJIB pakai `sudo pacman -Syu` untuk update sistem)",
            "paru" => "paru (pakai `paru -Syu` untuk update sistem)",
            _ => "yay (pakai `yay -Syu` untuk update sistem)",
        };
        let _ = self.add_note(PACKAGE_MANAGER_PREF_TAG, label);
    }

    pub fn record_fast_path_action(&mut self, summary: &str) {
        let trimmed = summary.trim();
        if trimmed.is_empty() {
            return;
        }
        let fp_count = self
            .notes
            .iter()
            .filter(|n| n.tag == FAST_PATH_LOG_TAG)
            .count();
        if fp_count >= 5 {
            if let Some(pos) = self.notes.iter().position(|n| n.tag == FAST_PATH_LOG_TAG) {
                self.notes.remove(pos);
            }
        }
        let _ = self.add_note(FAST_PATH_LOG_TAG, trimmed);
    }

    pub fn preferred_package_manager(&self) -> Option<&'static str> {
        let note = self
            .notes
            .iter()
            .rev()
            .find(|n| n.tag == PACKAGE_MANAGER_PREF_TAG)?;
        let low = note.content.to_ascii_lowercase();
        if low.contains("pacman") {
            Some("pacman")
        } else if low.contains("paru") {
            Some("paru")
        } else if low.contains("yay") {
            Some("yay")
        } else {
            None
        }
    }

    pub fn render_prompt_block(&self) -> Option<String> {
        if self.notes.is_empty() {
            return None;
        }
        let lines: Vec<String> = self
            .notes
            .iter()
            .map(|n| format!("- #{} [{}]: {} ({})", n.id, n.tag, n.content, n.created_at))
            .collect();
        Some(lines.join("\n"))
    }
}

fn normalize_package_manager(raw: &str) -> &'static str {
    let low = raw.trim().to_ascii_lowercase();
    if low.contains("pacman") {
        "pacman"
    } else if low.contains("paru") {
        "paru"
    } else {
        "yay"
    }
}

pub fn detect_package_manager_preference(input: &str) -> Option<&'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.starts_with('/') {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    let has_intent = [
        "pake",
        "pakai",
        "biasa",
        "lebih suka",
        "ganti",
        "coba",
        "aja",
        "saja",
        "use",
        "prefer",
        "switch",
        "update",
        "install",
        "pasang",
    ]
    .iter()
    .any(|kw| lower.contains(kw));

    if !has_intent {
        return None;
    }

    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();

    let is_negated = |pm: &str| -> bool {
        tokens.iter().enumerate().any(|(idx, &tok)| {
            if tok != pm {
                return false;
            }
            let start = idx.saturating_sub(3);
            tokens[start..idx].iter().any(|&prev| {
                matches!(
                    prev,
                    "jangan" | "jgn" | "bukan" | "gak" | "ga" | "nggak" | "tidak" | "not" | "dont"
                )
            })
        })
    };

    let has_tok = |pm: &str| tokens.contains(&pm);

    if has_tok("pacman") && !is_negated("pacman") {
        return Some("pacman");
    }
    if has_tok("paru") && !is_negated("paru") {
        return Some("paru");
    }
    if has_tok("yay") && !is_negated("yay") {
        return Some("yay");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratchpad_add_save_load_remove_clear_roundtrip() {
        let dir = std::env::temp_dir().join(format!("waish-scratch-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("scratchpad.json");

        let mut pad = Scratchpad::load(&path).unwrap();
        assert!(pad.is_empty());

        let n1 = pad.add_note("dev", "Backend running on port 8080");
        let n2 = pad.add_note("", "Cek config waybar habis makan");
        assert_eq!(n1.id, 1);
        assert_eq!(n2.tag, "memo");
        pad.save(&path).unwrap();

        let mut loaded = Scratchpad::load(&path).unwrap();
        assert_eq!(loaded.notes.len(), 2);
        assert!(loaded.render_prompt_block().unwrap().contains("port 8080"));
        assert!(loaded.remove_note(1));
        assert!(!loaded.remove_note(99));
        assert_eq!(loaded.notes.len(), 1);

        assert_eq!(loaded.clear(), 1);
        assert!(loaded.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_and_persists_package_manager_preference() {
        assert_eq!(
            detect_package_manager_preference("aku biasa pake pacman"),
            Some("pacman")
        );
        assert_eq!(
            detect_package_manager_preference("coba pake sudo pacman aja dong"),
            Some("pacman")
        );
        assert_eq!(
            detect_package_manager_preference("jangan pake yay, pake pacman aja"),
            Some("pacman")
        );
        assert_eq!(
            detect_package_manager_preference("lebih suka yay aja"),
            Some("yay")
        );
        assert_eq!(
            detect_package_manager_preference("please use paru instead"),
            Some("paru")
        );
        assert_eq!(detect_package_manager_preference("halo alice"), None);
        assert_eq!(detect_package_manager_preference("/sh pacman -Q"), None);

        let mut pad = Scratchpad::default();
        pad.set_package_manager("yay");
        assert_eq!(pad.preferred_package_manager(), Some("yay"));
        pad.set_package_manager("pacman");
        assert_eq!(pad.preferred_package_manager(), Some("pacman"));
        // Tidak boleh menduplikasi entri pref:package_manager.
        assert_eq!(
            pad.notes
                .iter()
                .filter(|n| n.tag == PACKAGE_MANAGER_PREF_TAG)
                .count(),
            1
        );
    }
}
