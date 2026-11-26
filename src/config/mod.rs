//! Lokasi file XDG dan utilitas penulisan atomik.

pub mod memory;
pub mod persona;
pub mod settings;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "config.toml";
pub const PERSONA_FILE: &str = "persona.md";
pub const MEMORY_FILE: &str = "memory.md";
pub const HISTORY_FILE: &str = "history.txt";
pub const SCRATCHPAD_FILE: &str = "scratchpad.json";
pub const SHADOW_REPO_DIR: &str = "shadow_repo";

/// Permission untuk file yang memuat rahasia (API key) atau data personal.
pub const PRIVATE_FILE_MODE: u32 = 0o600;
/// Permission untuk direktori konfigurasi waish.
pub const PRIVATE_DIR_MODE: u32 = 0o700;

/// Kumpulan path standar waish:
/// - Config: `$XDG_CONFIG_HOME/waish/` (default `~/.config/waish/`)
/// - Data  : `$XDG_DATA_HOME/waish/` (default `~/.local/share/waish/`)
#[derive(Debug, Clone)]
pub struct Paths {
    pub dir: PathBuf,
    pub config: PathBuf,
    pub persona: PathBuf,
    pub memory: PathBuf,
    pub history: PathBuf,
    pub scratchpad: PathBuf,
    pub shadow_repo: PathBuf,
}

impl Paths {
    pub fn resolve() -> crate::Result<Self> {
        // `dirs::config_dir()` di Linux sudah menghormati $XDG_CONFIG_HOME.
        let config_base = dirs::config_dir()
            .ok_or("Tidak bisa menentukan direktori konfigurasi ($XDG_CONFIG_HOME / $HOME kosong)")?;
        let data_base = dirs::data_local_dir().unwrap_or_else(|| config_base.clone());
        let dir = config_base.join("waish");
        let shadow_repo = data_base.join("waish").join(SHADOW_REPO_DIR);
        Ok(Self {
            config: dir.join(CONFIG_FILE),
            persona: dir.join(PERSONA_FILE),
            memory: dir.join(MEMORY_FILE),
            history: dir.join(HISTORY_FILE),
            scratchpad: dir.join(SCRATCHPAD_FILE),
            shadow_repo,
            dir,
        })
    }

    #[cfg(test)]
    pub fn from_dir(dir: PathBuf) -> Self {
        Self {
            config: dir.join(CONFIG_FILE),
            persona: dir.join(PERSONA_FILE),
            memory: dir.join(MEMORY_FILE),
            history: dir.join(HISTORY_FILE),
            scratchpad: dir.join(SCRATCHPAD_FILE),
            shadow_repo: dir.join(SHADOW_REPO_DIR),
            dir,
        }
    }
}

/// Menulis file baru lalu `fsync` sebelum kembali. Gagal jika file sudah ada.
pub fn write_new_synced(path: &Path, contents: &str, mode: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()
}

/// `fsync` pada direktori agar operasi rename/create tercatat permanen di disk.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Penulisan atomik satu file: tulis ke file temp di direktori yang sama,
/// `fsync`, lalu `rename` menimpa target. Pembaca tidak akan pernah melihat
/// file setengah jadi.
pub fn atomic_write(path: &Path, contents: &str, mode: u32) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path tanpa parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path tanpa nama file"))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));

    let _ = fs::remove_file(&tmp);
    let result = write_new_synced(&tmp, contents, mode)
        .and_then(|_| fs::rename(&tmp, path))
        .and_then(|_| sync_dir(dir));

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
