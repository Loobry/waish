//! Definisi argumen CLI.

use clap::{ArgAction, ArgGroup, Parser};

const AFTER_HELP: &str = "\
FILE KONFIGURASI:
    ~/.config/waish/config.toml    Konfigurasi runtime (provider, model, perilaku)
    ~/.config/waish/persona.md     Kepribadian companion
    ~/.config/waish/memory.md      Long-term memory bank

    Lokasi mengikuti $XDG_CONFIG_HOME bila diset.
    Jika config.toml belum ada, menjalankan `waish` tanpa flag akan memulai setup wizard.";

#[derive(Debug, Parser)]
#[command(
    name = "waish",
    version,
    about = "waish - Sovereign Unix Waifu Companion & System Automation Shell",
    long_about = None,
    after_help = AFTER_HELP,
    disable_version_flag = true,
    group(
        ArgGroup::new("action")
            .multiple(false)
            .args(["config", "persona", "memory", "memory_edit", "memory_clear", "status", "initial_prompt"])
    ),
)]
pub struct Args {
    /// Prompt pembuka. waish akan merespon pesan ini di awal,
    /// lalu tetap berada di REPL loop untuk percakapan berikutnya.
    #[arg(value_name = "INITIAL_PROMPT")]
    pub initial_prompt: Vec<String>,

    /// Membuka ~/.config/waish/config.toml di $EDITOR
    #[arg(short = 'c', long = "config")]
    pub config: bool,

    /// Membuka ~/.config/waish/persona.md di $EDITOR
    #[arg(short = 'p', long = "persona")]
    pub persona: bool,

    /// Menampilkan isi ringkas memory.md ke layar
    #[arg(short = 'm', long = "memory")]
    pub memory: bool,

    /// Membuka ~/.config/waish/memory.md di $EDITOR
    #[arg(long = "memory-edit")]
    pub memory_edit: bool,

    /// Mengosongkan memory bank (reset riwayat)
    #[arg(long = "memory-clear")]
    pub memory_clear: bool,

    /// Mengecek validitas API key dan status koneksi internet
    #[arg(short = 's', long = "status")]
    pub status: bool,

    /// Menampilkan versi waish
    #[arg(short = 'v', long = "version", action = ArgAction::Version)]
    pub version: Option<bool>,
}

/// Aksi non-REPL yang dipicu oleh flag CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    EditConfig,
    EditPersona,
    ShowMemory,
    EditMemory,
    ClearMemory,
    Status,
}

impl Args {
    pub fn action(&self) -> Option<Action> {
        if self.config {
            Some(Action::EditConfig)
        } else if self.persona {
            Some(Action::EditPersona)
        } else if self.memory {
            Some(Action::ShowMemory)
        } else if self.memory_edit {
            Some(Action::EditMemory)
        } else if self.memory_clear {
            Some(Action::ClearMemory)
        } else if self.status {
            Some(Action::Status)
        } else {
            None
        }
    }

    pub fn initial_prompt(&self) -> Option<String> {
        let joined = self.initial_prompt.join(" ");
        let trimmed = joined.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_definition_is_valid() {
        Args::command().debug_assert();
    }

    #[test]
    fn joins_positional_prompt() {
        let args = Args::try_parse_from(["waish", "update", "dong"]).unwrap();
        assert_eq!(args.initial_prompt().as_deref(), Some("update dong"));
        assert_eq!(args.action(), None);
    }

    #[test]
    fn parses_short_flags() {
        let args = Args::try_parse_from(["waish", "-m"]).unwrap();
        assert_eq!(args.action(), Some(Action::ShowMemory));
        let args = Args::try_parse_from(["waish", "--memory-clear"]).unwrap();
        assert_eq!(args.action(), Some(Action::ClearMemory));
    }

    #[test]
    fn rejects_conflicting_flags() {
        assert!(Args::try_parse_from(["waish", "-m", "-c"]).is_err());
        assert!(Args::try_parse_from(["waish", "-m", "halo"]).is_err());
    }

    #[test]
    fn lowercase_v_is_version() {
        let err = Args::try_parse_from(["waish", "-v"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }
}
