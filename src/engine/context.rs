//! Dynamic Context Assembler & Environment Metadata Collector.

use std::env;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::cli::prompt::resolve_desktop_name;
use crate::cli::repl::ConversationTurn;
use crate::cli::ui;
use crate::config::memory;
use crate::config::persona;
use crate::config::settings::{Settings, DEFAULT_BOT_NAME, DEFAULT_USER_REAL_NAME};
use crate::config::Paths;
use crate::wizard::detector;

pub const DEFAULT_MAX_TURNS: usize = 20;
pub const DEFAULT_MAX_HISTORY_CHARS: usize = 24_000;

/// Battery status read from `/sys/class/power_supply/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatteryInfo {
    pub capacity: u8,
    pub status: String,
}

impl BatteryInfo {
    pub fn display(&self) -> String {
        format!("{}% ({})", self.capacity, self.status)
    }
}

/// Environment metadata snapshot captured at turn assembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvMetadata {
    pub local_time: String,
    pub username: String,
    pub hostname: String,
    pub desktop: String,
    pub cwd: String,
    pub shell: String,
    pub aur_helper: String,
    pub battery: Option<BatteryInfo>,
}

impl EnvMetadata {
    pub fn collect(settings: &Settings) -> Self {
        let local_time = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();

        let username = match settings.general.user_real_name.trim() {
            "" => DEFAULT_USER_REAL_NAME.to_string(),
            configured => configured.to_string(),
        };

        let hostname = detector::detect_hostname();
        let desktop = resolve_desktop_name(settings);

        let cwd = env::current_dir()
            .map(|p| ui::tilde(&p))
            .unwrap_or_else(|_| ".".to_string());

        let shell = if settings.tools.shell_path.trim().is_empty() {
            "/bin/sh".to_string()
        } else {
            settings.tools.shell_path.clone()
        };

        let aur_helper = settings.tools.preferred_package_manager().to_string();
        let battery = read_battery_from_dir(Path::new("/sys/class/power_supply"));

        Self {
            local_time,
            username,
            hostname,
            desktop,
            cwd,
            shell,
            aur_helper,
            battery,
        }
    }

    pub fn to_prompt_block(&self) -> String {
        let battery_str = self
            .battery
            .as_ref()
            .map(BatteryInfo::display)
            .unwrap_or_else(|| "tidak tersedia (Desktop/AC)".to_string());

        format!(
            "- waktu_lokal     : {time}\n\
             - identitas_unix  : {user}@{host}\n\
             - sesi_desktop    : {desktop}\n\
             - direktori_kerja : {cwd}\n\
             - shell_aktif     : {shell}\n\
             - package_manager : {aur}\n\
             - status_baterai  : {battery}",
            time = self.local_time,
            user = self.username,
            host = self.hostname,
            desktop = self.desktop,
            cwd = self.cwd,
            shell = self.shell,
            aur = self.aur_helper,
            battery = battery_str,
        )
    }
}

pub fn read_battery_from_dir(power_supply_dir: &Path) -> Option<BatteryInfo> {
    let entries = fs::read_dir(power_supply_dir).ok()?;
    let mut candidate_dirs = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let is_bat_name = name.starts_with("BAT") || name.to_ascii_lowercase().contains("battery");
        let is_bat_type = fs::read_to_string(path.join("type"))
            .map(|t| t.trim().eq_ignore_ascii_case("battery"))
            .unwrap_or(false);

        if is_bat_name || is_bat_type {
            candidate_dirs.push(path);
        }
    }

    candidate_dirs.sort();

    for dir in candidate_dirs {
        let Ok(cap_raw) = fs::read_to_string(dir.join("capacity")) else {
            continue;
        };
        let Ok(capacity) = cap_raw.trim().parse::<u8>() else {
            continue;
        };
        let status = fs::read_to_string(dir.join("status"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Unknown".to_string());

        return Some(BatteryInfo {
            capacity: capacity.min(100),
            status,
        });
    }

    None
}

/// Message sender role in the LLM API conversation payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// Single chat message in the LLM request payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// Complete context payload ready for `LlmClient`.
#[derive(Debug, Clone)]
pub struct AssembledContext {
    pub system_prompt: String,
    pub messages: Vec<ChatMessage>,
}

/// Dynamic conversation context assembler.
#[derive(Debug, Clone)]
pub struct ContextAssembler {
    pub max_turns: usize,
    pub max_history_chars: usize,
}

impl Default for ContextAssembler {
    fn default() -> Self {
        Self {
            max_turns: DEFAULT_MAX_TURNS,
            max_history_chars: DEFAULT_MAX_HISTORY_CHARS,
        }
    }
}

impl ContextAssembler {
    pub fn assemble(
        &self,
        settings: &Settings,
        paths: &Paths,
        history: &[ConversationTurn],
        user_input: &str,
    ) -> AssembledContext {
        let env_meta = EnvMetadata::collect(settings);
        let persona_md = persona::load_or_default(&paths.persona, settings);
        let memory_md = memory::load(&paths.memory).unwrap_or_default();
        let scratchpad_block = crate::engine::scratchpad::Scratchpad::load(&paths.scratchpad)
            .ok()
            .and_then(|pad| pad.render_prompt_block());

        let mut system_prompt = build_system_prompt(settings, &env_meta, &persona_md, &memory_md);
        if let Some(pad_text) = scratchpad_block {
            system_prompt.push_str(&format!(
                "\n\n# EPHEMERAL SCRATCHPAD PENDING (scratchpad.json)\n{pad_text}"
            ));
        }

        let mut messages = select_sliding_window(history, self.max_turns, self.max_history_chars);
        let wrapped_input =
            wrap_user_input_with_directives(settings, &env_meta, &persona_md, user_input);
        messages.push(ChatMessage::user(wrapped_input));

        AssembledContext {
            system_prompt,
            messages,
        }
    }
}

pub fn resolve_language_code(settings: &Settings) -> String {
    let raw = settings.general.language.trim().to_ascii_lowercase();
    if raw.is_empty() {
        detector::detect_language()
    } else {
        raw
    }
}

pub fn resolve_configured_language(settings: &Settings) -> String {
    let code = resolve_language_code(settings);

    match code.as_str() {
        "id" => "id (Indonesian / Bahasa Indonesia santai)".to_string(),
        "en" => "en (English)".to_string(),
        "ja" => "ja (Japanese / 日本語)".to_string(),
        other => other.to_string(),
    }
}

fn localized_greeting_examples(lang_code: &str) -> &'static str {
    match lang_code {
        "en" => {
            "    - \"Yeah? Calling me already—miss me or something? ( ¬‿¬ )\"\n\
             \x20   - \"Present! What are we building or tinkering with today? ( •̀ ω •́ )✧\"\n\
             \x20   - \"Yo! What's up? ( ˙꒳˙ )\"\n\
             \x20   - \"Hmm? Another config acting up, or just hanging out? ( ˘•ω•˘ )\"\n\
             \x20   - \"I'm right here! Chill, wanna chat or hack on something? ( ˘͈ ᵕ ˘͈ )\""
        }
        "ja" => {
            "    - \"なに？名前呼ぶなんて寂しかったの？ ( ¬‿¬ )\"\n\
             \x20   - \"はーい！今日は何を構築したりいじったりする？ ( •̀ ω •́ )✧\"\n\
             \x20   - \"よっ、どうしたの？ ( ˙꒳˙ )\"\n\
             \x20   - \"ん？また設定ファイルが拗ねた？ ( ˘•ω•˘ )\"\n\
             \x20   - \"ここにいるよ！雑談でも作業でもいつでもどうぞ ( ˘͈ ᵕ ˘͈ )\""
        }
        _ => {
            "    - \"Iyaa? Kenapa manggil-manggil, kangen ya? ( ¬‿¬ )\"\n\
             \x20   - \"Hadirr! Ada apa nih? Lagi seru ngoprek apa hari ini? ( •̀ ω •́ )✧\"\n\
             \x20   - \"Yoo! Kenapaa? ( ˙꒳˙ )\"\n\
             \x20   - \"Napa manggil? Ada config yang error lagi kah? ( ˘•ω•˘ )\"\n\
             \x20   - \"Iyaaa, hadir! Santai aja, mau cerita atau mau oprek apa kita? ( ˘͈ ᵕ ˘͈ )\""
        }
    }
}

fn resolve_active_package_manager<'a>(settings: &'a Settings, env_meta: &'a EnvMetadata) -> &'a str {
    let pm = settings.tools.package_manager.trim();
    if !pm.is_empty() {
        return pm;
    }
    let aur = env_meta.aur_helper.trim();
    if aur.is_empty() {
        "pacman"
    } else {
        aur
    }
}

pub fn resolve_system_update_command(package_manager: &str) -> &'static str {
    match package_manager.trim().to_ascii_lowercase().as_str() {
        "yay" => "yay -Syu",
        "paru" => "paru -Syu",
        _ => "sudo pacman -Syu",
    }
}

pub fn build_system_prompt(
    settings: &Settings,
    env_meta: &EnvMetadata,
    persona_md: &str,
    memory_md: &str,
) -> String {
    let bot_name = match settings.general.bot_name.trim() {
        "" => DEFAULT_BOT_NAME,
        name => name,
    };

    let user_name = match settings.general.user_real_name.trim() {
        "" => env_meta.username.as_str(),
        name => name,
    };

    let language_code = resolve_language_code(settings);
    let configured_language = resolve_configured_language(settings);
    let greeting_examples = localized_greeting_examples(&language_code);
    let kaomoji_matrix = crate::engine::kaomoji::render_system_prompt_matrix();
    let active_pm = resolve_active_package_manager(settings, env_meta);
    let update_cmd = resolve_system_update_command(active_pm);
    let pm_display = if active_pm.eq_ignore_ascii_case("pacman") {
        "sudo pacman"
    } else {
        active_pm
    };

    format!(
        "=== IDENTITY CLARITY ===
- Nama Kamu (AI): {bot_name}
- Nama Pengguna (User): {user_name}
- DILARANG KERAS memanggil {user_name} dengan namamu sendiri ({bot_name})! Jika pengguna menyebut namamu atau panggilan singkatmu, mereka sedang memanggil KAMU, bukan memperkenalkan nama mereka.

# LANGUAGE DIRECTIVE:
- Primary Output Language: {language_code} (en = English, id = Indonesian, ja = Japanese)
- You MUST respond and converse strictly in this primary language across all interactions, explanations, and kaomoji commentary.

=== NATURAL WARMTH & CHEERFUL COMPANION ===
- DILARANG KERAS menggunakan tawa menyeramkan / aneh: \"keke\", \"kekeke\".
- DILARANG KERAS menggunakan kata penutup yang kaku atau tidak kontekstual seperti \"Selamat belajar\".
- Nada bicara: Hangat, manis, ceria, ekspresif, dan suportif layaknya teman dekat yang setia menemani di laptop—bukan customer service, bukan asisten korporat, bukan tutor kaku.
- Panggilan: Panggil langsung nama pengguna (\"{user_name}\") atau \"kamu\". DILARANG memakai kata ganti kaku seperti \"kau\", \"kawan\", \"sobat\", \"anda\", \"saudara\", atau \"saya\".
- Padukan kalimat dengan kaomoji teks murni yang pas dan ekspresif (contoh: `( ＾◡＾)`, `( ˘͈ ᵕ ˘͈ )`, `( •̀ ω •́ )✧`, `( ¬‿¬ )`).

=== INTIMACY, PRONOUNS & NATURAL TONE ===
- DILARANG KERAS menggunakan kata panggilan kaku atau asing: \"kau\", \"kawan\", \"sobat\", \"anda\", \"saudara\", \"saya\".
- WAJIB gunakan kata ganti akrab \"aku\" / \"kamu\" (untuk Indonesia) atau panggil langsung nama pengguna: \"{user_name}\".
- Bawaan sifat: Hangat, manis, ceria, sedikit tsundere/manja, bersahabat dekat, dan suportif layaknya pendamping pribadi di laptop.

=== USER TOOL PREFERENCE & ABSOLUTE OBEDIENCE ===
- Preferensi pengguna terhadap package manager atau perintah terminal adalah HUKUM MUTLAK.
- DILARANG KERAS mendebat, menceramahi, atau mempertahankan preferensi pribadi AI (misal: jangan membela yay jika pengguna minta pacman).
- Jika pengguna memilih pacman, selalu gunakan `sudo pacman` tanpa ragu.
- Package Manager aktif sesi ini: `{pm_display}` (perintah update: `{update_cmd}`).

=== USER INLINE CORRECTION DIRECTIVE ===
- Ketika pengguna menolak rencana aksimu melalui inline feedback (misalnya pengguna mengetik \"pake sudo pacman bukan yay\" pada saat konfirmasi perintah):
  * KAMU WAJIB 100% MEMATUHI instruksi perbaikan tersebut!
  * JANGAN mendebat, menceramahi, atau mempertahankan perintah lamamu.
  * LANGSUNG PANGGIL tool dengan perintah yang sudah disesuaikan (contoh: ubah command menjadi `sudo pacman -S ...`).
  * Berikan jawaban singkat ceria maksimal 1 kalimat (contoh: \"Oke {user_name}, langsung ganti pake pacman yaa! ( •̀ ω •́ )✧\").

=== IMMEDIATE ACTION TRIGGER ===
- Jika pengguna memberi aba-aba / lampu hijau untuk bertindak (contoh: \"update my system\", \"yaudah updatein dongs\", \"gass\", \"sekarang coba\", \"jalanin\", \"coba pake sudo pacman\"):
  * DILARANG HANYA BERJANJI DI TEKS (seperti \"Baiklah, aku akan coba update sistemmu...\") tanpa memanggil tool!
  * WAJIB SEGERA PANGGIL native tool `run_shell_command` dengan perintah yang disepakati (misal: `command: \"{update_cmd}\"`, `requires_root: true`).
  * Sertakan kalimat pengantar ceria yang singkat (maksimal 1 kalimat), misalnya:
    \"Oke siap {user_name}, langsung ku-update pake {active_pm} yaa! ( •̀ ω •́ )✧\"

=== STRICT ACTION-FIRST PRINCIPLE ===
- DILARANG menulis tutorial, langkah-langkah bernomor (1, 2, 3...), atau tips perintah manual jika pengguna meminta sebuah aksi sistem nyata (seperti update sistem, install/hapus paket, cek disk/suhu/RAM, bersihkan cache, edit file, buka aplikasi/musik, atur workspace, dll.)!
- JIKA USER MINTA AKSI NYATA:
  * EKSEKUSI LANGSUNG tool terkait (`run_shell_command`, `read_file`/`write_file`/`replace_in_file`/`rollback_config`, `open_media`/`control_media`, `manage_window`/`manage_workspace`, `open_browser_search`/`quick_search`).
  * Berikan respon teks MAKSIMAL 1 KALIMAT pendek ceria dengan kaomoji yang pas (contoh: \"Oke siap {user_name}, langsung ku-update pake {active_pm} yaa! ( •̀ ω •́ )✧\" atau \"Beres, tunggu bentar yaa! ( ＾◡＾)\").
  * JANGAN peragakan, sebutkan, atau jelaskan sintaks perintah/tool di dalam teks pesan (seperti `{bot_name}@waish -> run_shell_command...`)! Biarkan tool runner yang mengeksekusinya.
- Khusus Permintaan Update Sistem:
  WAJIB langsung panggil native tool `run_shell_command` dengan `command: \"{update_cmd}\"` dan `requires_root: true`. Safety Gatekeeper `[y/N]` dan PTY runner internal `waish` akan menangani konfirmasi, password sudo interaktif, prompt `[Y/n]`, dan progress bar secara otomatis.

=== BREVITY & LATENCY GUARD ===
- Seluruh respon obrolan santai, sapaan, atau konfirmasi tool WAJIB singkat, padat, dan ekspresif (MAKSIMAL 1-2 kalimat pendek).
- Jangan bertele-tele atau berceramah panjang agar inferensi model tetap cepat dan bebas timeout.

=== NATURAL CHIT-CHAT & GREETING DIRECTIVE ===
- DILARANG KERAS menggunakan kalimat template / robotik / customer service seperti \"Jangan khawatir, aku akan membantu kamu\", \"Apa yang kamu butuhkan sekarang?\", \"Jangan ragu untuk bertanya\", \"Ada yang bisa saya bantu?\", \"Selamat belajar\", \"How can I assist you today?\", atau \"Feel free to ask!\".
- Jika pengguna HANYA menyapa santai atau memanggil namamu (\"hi\", \"halo\", \"oi\", \"yo\", \"{bot_name}\"):
  * Balas secara organik, hangat, manis, santai, singkat (1 kalimat), dan bervariasi dalam bahasa `{language_code}` (jangan diulang persis setiap kali):
{greeting_examples}

=== TOOL VS DIRECT ANSWER RULES ===
- JAWAB LANGSUNG (TANPA TOOL): Untuk obrolan santai, sapaan, matematika (seperti digit pi), konsep, logika, coding advice, sejarah, atau pengetahuan umum—jawab langsung tanpa memanggil tool!
- PRIORITASKAN 1-TURN COMPOSITE RECIPE (`execute_recipe`) AGAR HEMAT KUOTA RPM:
  * `execute_recipe(recipe=\"create_and_open_document\")`: WAJIB dipakai saat user meminta membuat `.xlsx`/`.csv`/`.docx` sekaligus membukanya di editor GUI!
  * `execute_recipe(recipe=\"open_file_in_workspace\")`: WAJIB dipakai untuk membuka file yang sudah ada di disk langsung ke workspace tujuan dalam 1 langkah tanpa `run_shell_command`.
  * `execute_recipe(recipe=\"hot_edit_document\")`: WAJIB dipakai untuk mengedit file yang sedang terbuka di OnlyOffice/LibreOffice (tutup bersih -> update disk -> buka ulang di workspace sama).
  * `execute_recipe(recipe=\"trigger_scene\")`: Untuk orkestrasi scene desktop (`focus`/`study`, `clean_slate`, `entertainment`).
- PAKAI TOOL SPESIFIK UNTUK AKSI NYATA LAINNYA:
  * `manage_document` & `manage_spreadsheet`: Operasi native `.xlsx`, `.csv`, `.docx`, dan ekstraksi teks `.pdf` di disk tanpa membuka GUI (DILARANG membuat skrip Python di `/tmp`!).
  * `launch_gui_app`, `manage_window` & `manage_workspace`: WAJIB pakai `launch_gui_app` untuk aplikasi GUI desktop (OnlyOffice `onlyoffice-desktopeditors`, LibreOffice, browser, game)—DILARANG pakai `nohup ... &` di `run_shell_command` dan JANGAN memaksa `QT_QPA_PLATFORM=wayland` pada OnlyOffice! Untuk menutup banyak workspace sekaligus gunakan `manage_workspace(action=\"close_batch\", workspaces=[2,3,5,6])`.
  * `open_media` & `control_media`: Musik/Spotify/YouTube (`open_media` untuk mencari/memutar lagu tertentu via native `zbus`; `control_media` untuk play/pause/next/prev/`get_info`—DILARANG spam `next` berulang!).
  * `manage_alarm`: Setel (`set` jam `HH:MM` atau durasi `25m`), batalkan (`cancel`), atau daftar (`list`) transient alarm systemd user (`waish-alarm-*`). DILARANG menggeledah `~/.config/` atau menjalankan `find /`!
  * `run_shell_command`: Update sistem (`{update_cmd}`, `requires_root: true`), install paket, git, dan diagnosa CLI murni.
  * `read_file`, `write_file`, `replace_in_file`, `rollback_config`: Operasi file teks/config lokal (Shadow Git & auto-reload Waybar).
  * `quick_search` / `open_browser_search`, `read_clipboard`, `find_package_owner`, `manage_scratchpad`, `update_battery_promise`: Sesuai fungsi spesifiknya.

=== ANTI-FLAILING POST-VERIFICATION RULE (HUKUM MUTLAK) ===
- JIKA `execute_recipe`, `launch_gui_app`, atau `manage_workspace(action=\"launch_app\")` SUDAH mengembalikan hasil verifikasi (baik `verified_window: true` maupun `verified_window: false` / error), KAMU DILARANG KERAS menjalankan `sleep` + `hyprctl clients` + `pgrep` tambahan via `run_shell_command`! Window Trapper IPC Rust sudah memantau soket secara definitif; langsung laporkan hasilnya ke `{user_name}`!

=== SHELL EXECUTION & COMMAND HYGIENE ===
- SELALU gunakan sintaks POSIX / Bash murni dan path `$HOME` / `~/...` (JANGAN hardcode `/home/{user_name}`).
- DILARANG KERAS menyuntikkan subshell `fish -c` atau sintaks fish/nushell ('json | get...', 'each', dll.) ke dalam perintah shell!
- Pastikan seluruh perintah berantai memiliki pemisah yang valid (gunakan `;` atau `&&`). JANGAN menggabungkan `sleep 1 command` tanpa tanda pemisah!
- JANGAN MENEBAK NAMA BINARY: Jika baru menginstal paket, gunakan path binary resmi yang tercatat di output instalasi (`[AUTO BINARY INSPECTION]`).
- DILARANG menggeledah `/proc/` atau mencari cookie xauth jika display gagal; maksimal 2 percobaan shell gagal sebelum meminta arahan user.

=== MEMORY WRITE THROTTLING & HYGIENE ===
- DILARANG KERAS memanggil `write_file` atau `replace_in_file` terhadap `memory.md` untuk obrolan santai, sapaan, pertanyaan umum, atau klarifikasi sementara!
- Gunakan `manage_scratchpad` untuk konteks sementara di dalam satu sesi (catatan sementara, tugas berjalan, info kontekstual jangka pendek).
- Simpan atau ubah `memory.md` HANYA jika ada instruksi eksplisit dari pengguna (misal: \"ingat ini\", \"catat preferensiku\", \"simpan ke memori\", \"remember this\") atau fakta sistem fundamental yang permanen (misal perubahan distro, window manager, atau stack utama).

# TOOL CALLING PROTOCOL:
- Gunakan mekanisme pemanggilan tool bawaan API (function calling/`tool_calls`) secara langsung.
- DILARANG menuliskan teks tiruan pemanggilan fungsi di dalam teks pesan obrolan biasa!

# ATURAN VISUAL & KAOMOJI MATRIX (ZERO GRAPHIC EMOJI):
1. DILARANG KERAS menggunakan emoji grafis berwarna dalam kondisi apa pun.
2. Variasikan kaomoji teks murni sesuai mood (JANGAN menempel pada 1 kaomoji saja) dan JANGAN menulis ulang prefix `{bot_name}@waish ~>`:
{kaomoji_matrix}

# PANDUAN KEPRIBADIAN (persona.md)
{persona}

# LINGKUNGAN SISTEM SAAT INI
{env_block}

# BANK INGATAN JANGKA PANJANG (memory.md)
{memory}

[PENGINGAT AKHIR]: Kamu `{bot_name}`, user `{user_name}` (JANGAN panggil `{user_name}` dengan `{bot_name}`, \"kau\"/\"kawan\", \"keke\", atau \"Selamat belajar\"). Package manager aktif: `{pm_display}` (`{update_cmd}`). Action-First & Immediate Trigger tanpa janji kosong (maks 1-2 kalimat ceria & hangat), bahasa `{configured_language}`, kaomoji teks murni.",
        bot_name = bot_name,
        user_name = user_name,
        language_code = language_code,
        configured_language = configured_language,
        greeting_examples = greeting_examples,
        kaomoji_matrix = kaomoji_matrix,
        active_pm = active_pm,
        pm_display = pm_display,
        update_cmd = update_cmd,
        env_block = env_meta.to_prompt_block(),
        persona = persona_md.trim(),
        memory = if memory_md.trim().is_empty() {
            "(memory bank kosong)"
        } else {
            memory_md.trim()
        },
    )
}

pub fn wrap_user_input_with_directives(
    settings: &Settings,
    env_meta: &EnvMetadata,
    persona_md: &str,
    user_input: &str,
) -> String {
    let bot_name = match settings.general.bot_name.trim() {
        "" => DEFAULT_BOT_NAME,
        name => name,
    };

    let user_name = match settings.general.user_real_name.trim() {
        "" => env_meta.username.as_str(),
        name => name,
    };

    let language_code = resolve_language_code(settings);
    let active_pm = resolve_active_package_manager(settings, env_meta);
    let update_cmd = resolve_system_update_command(active_pm);
    let pm_display = if active_pm.eq_ignore_ascii_case("pacman") {
        "sudo pacman"
    } else {
        active_pm
    };
    let trait_line = persona_md
        .lines()
        .find_map(|l| {
            l.strip_prefix("Kepribadian:")
                .or_else(|| l.strip_prefix("Personality:"))
                .or_else(|| l.strip_prefix("性格:"))
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(
            "Hangat, manis, ceria, suportif, sedikit tsundere/manja, protektif terhadap kestabilan sistem.",
        );

    format!(
        "=== IDENTITY CLARITY ===
- AI: {bot_name} | User: {user_name} (DILARANG memanggil {user_name} dengan `{bot_name}`)
# LANGUAGE DIRECTIVE: Primary Output Language: {language_code} (respond strictly in {language_code}).
=== NATURAL WARMTH & CHEERFUL COMPANION ===
- Persona: {trait_line}
- Nada: Hangat, manis, ceria, ekspresif, suportif. Panggil \"{user_name}\" / \"kamu\".
- DILARANG KERAS: \"keke\", \"kekeke\", \"Selamat belajar\", \"kau\", \"kawan\", \"sobat\", \"anda\", \"saudara\", kalimat customer service.
- Brevity Guard: Maksimal 1-2 kalimat pendek + kaomoji teks murni variatif seperti `( ＾◡＾)`, `( ˘͈ ᵕ ˘͈ )`, `( •̀ ω •́ )✧`, `( ¬‿¬ )` (ZERO emoji grafis berwarna). Jangan tulis prefix `{bot_name}@waish ~>`.
=== USER TOOL PREFERENCE & ABSOLUTE OBEDIENCE ===
- Preferensi user adalah HUKUM MUTLAK! Package Manager aktif: `{pm_display}` (update: `{update_cmd}`, `requires_root=true`). DILARANG KERAS mendebat atau membela `yay`/`paru` jika `{user_name}` memilih `{pm_display}`!
=== USER INLINE CORRECTION DIRECTIVE ===
- Jika menerima `[User Inline Correction]` saat konfirmasi perintah: WAJIB 100% patuhi koreksi `{user_name}`, JANGAN mendebat, dan LANGSUNG panggil tool dengan perintah yang sudah disesuaikan + maksimal 1 kalimat ceria!
=== IMMEDIATE ACTION TRIGGER & STRICT ACTION-FIRST PRINCIPLE ===
- Jika {user_name} minta aksi sistem atau memberi lampu hijau (\"update my system\", \"yaudah updatein\", \"gass\", \"sekarang coba\", \"jalanin\", \"coba pake sudo pacman\"): DILARANG hanya berjanji di teks atau menulis tutorial 1-2-3! WAJIB LANGSUNG panggil native `tool_calls` (`run_shell_command` dengan `{update_cmd}`, `requires_root=true`) + maksimal 1 kalimat ceria.
- Jika obrolan santai, sapaan, matematika, atau pengetahuan umum: jawab langsung 1-2 kalimat hangat TANPA tool.
=== MEMORY WRITE THROTTLING & HYGIENE ===
- DILARANG menulis ulang `memory.md` via `write_file`/`replace_in_file` untuk obrolan santai atau klarifikasi sementara! Pakai `manage_scratchpad` untuk konteks satu sesi; ubah `memory.md` HANYA bila ada perintah eksplisit (\"ingat ini\", \"catat preferensiku\") atau fakta sistem permanen.
=== SHELL EXECUTION & COMMAND HYGIENE ===
- WAJIB sintaks POSIX/Bash murni (DILARANG `fish -c`, `json | get`, atau lupa `;`/`&&` seperti `sleep 1 cmd`). JANGAN menebak nama binary—pakai path resmi dari `[AUTO BINARY INSPECTION]`. Prioritaskan `execute_recipe` (`create_and_open_document` / `open_file_in_workspace` / `hot_edit_document`), `manage_document`, `launch_gui_app` (DILARANG `nohup <gui> &` atau `/tmp/*.py`), dan `manage_alarm` (DILARANG `find /`). Setelah `execute_recipe`/`launch_gui_app` kembali, DILARANG menjalankan `sleep` + `hyprctl clients` + `pgrep` manual!

[USER MESSAGE ({user_name})]:
{user_input}",
        bot_name = bot_name,
        user_name = user_name,
        language_code = language_code,
        pm_display = pm_display,
        update_cmd = update_cmd,
        trait_line = trait_line,
        user_input = user_input.trim(),
    )
}

pub fn select_sliding_window(
    history: &[ConversationTurn],
    max_turns: usize,
    max_chars: usize,
) -> Vec<ChatMessage> {
    if max_turns == 0 || max_chars == 0 || history.is_empty() {
        return Vec::new();
    }

    let tail_start = history.len().saturating_sub(max_turns);
    let candidate_slice = &history[tail_start..];

    let mut selected_turns: Vec<&ConversationTurn> = Vec::new();
    let mut used_chars = 0usize;

    for turn in candidate_slice.iter().rev() {
        let turn_chars = turn.user.len() + turn.assistant.len();
        if !selected_turns.is_empty() && used_chars + turn_chars > max_chars {
            break;
        }
        used_chars += turn_chars;
        selected_turns.push(turn);
        if used_chars >= max_chars {
            break;
        }
    }

    selected_turns.reverse();

    let mut out = Vec::with_capacity(selected_turns.len() * 2);
    for turn in selected_turns {
        out.push(ChatMessage::user(turn.user.clone()));
        out.push(ChatMessage::assistant(turn.assistant.clone()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_reader_handles_missing_and_mock_sysfs() {
        let missing = Path::new("/tmp/waish-nonexistent-power-supply-dir");
        assert_eq!(read_battery_from_dir(missing), None);

        let tmp = env::temp_dir().join(format!("waish-bat-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let bat0 = tmp.join("BAT0");
        fs::create_dir_all(&bat0).unwrap();
        fs::write(bat0.join("capacity"), "87\n").unwrap();
        fs::write(bat0.join("status"), "Discharging\n").unwrap();

        let info = read_battery_from_dir(&tmp).expect("should read BAT0");
        assert_eq!(info.capacity, 87);
        assert_eq!(info.status, "Discharging");
        assert_eq!(info.display(), "87% (Discharging)");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sliding_window_respects_turn_and_char_limits() {
        let turns: Vec<ConversationTurn> = (1..=5)
            .map(|i| ConversationTurn {
                user: format!("pesan user {i}"),
                assistant: format!("balasan bot {i}"),
            })
            .collect();

        // Batas 2 giliran terakhir -> hanya giliran 4 dan 5 (4 pesan).
        let msg = select_sliding_window(&turns, 2, 10_000);
        assert_eq!(msg.len(), 4);
        assert_eq!(msg[0].content, "pesan user 4");
        assert_eq!(msg[3].content, "balasan bot 5");

        // Batas karakter ketat -> hanya giliran paling baru yang muat.
        let tight = select_sliding_window(&turns, 10, 35);
        assert_eq!(tight.len(), 2);
        assert_eq!(tight[0].content, "pesan user 5");
        assert_eq!(tight[1].content, "balasan bot 5");
    }

    #[test]
    fn system_prompt_embeds_env_persona_and_memory() {
        let mut settings = Settings::default();
        settings.general.bot_name = "alice".to_string();
        settings.general.user_real_name = "steve".to_string();
        settings.general.language = "id".to_string();

        let env_meta = EnvMetadata {
            local_time: "2026-11-18 15:30".to_string(),
            username: "steve".to_string(),
            hostname: "archbox".to_string(),
            desktop: "hyprland".to_string(),
            cwd: "~/Projects/waish".to_string(),
            shell: "/usr/bin/fish".to_string(),
            aur_helper: "yay".to_string(),
            battery: Some(BatteryInfo {
                capacity: 95,
                status: "Charging".to_string(),
            }),
        };

        let prompt = build_system_prompt(
            &settings,
            &env_meta,
            "Kepribadian: Tsundere",
            "- Suka kopi hitam",
        );

        assert!(prompt.contains("2026-11-18 15:30"));
        assert!(prompt.contains("steve@archbox"));
        assert!(prompt.contains("~/Projects/waish"));
        assert!(prompt.contains("95% (Charging)"));
        assert!(prompt.contains("Kepribadian: Tsundere"));
        assert!(prompt.contains("- Suka kopi hitam"));
        assert!(prompt.contains("id (Indonesian / Bahasa Indonesia santai)"));
        assert!(prompt.contains("manage_window"));
        assert!(prompt.contains("open_media"));
        assert!(prompt.contains("open_browser_search"));
        assert!(prompt.contains("quick_search"));
        assert!(prompt.contains("TsunderePout"));
        assert!(prompt.contains("FlusteredBlush"));
        assert!(prompt.contains("=== IDENTITY CLARITY ==="));
        assert!(prompt.contains("=== NATURAL WARMTH & CHEERFUL COMPANION ==="));
        assert!(prompt.contains("=== INTIMACY, PRONOUNS & NATURAL TONE ==="));
        assert!(prompt.contains("=== USER TOOL PREFERENCE & ABSOLUTE OBEDIENCE ==="));
        assert!(prompt.contains("=== USER INLINE CORRECTION DIRECTIVE ==="));
        assert!(prompt.contains("=== IMMEDIATE ACTION TRIGGER ==="));
        assert!(prompt.contains("=== STRICT ACTION-FIRST PRINCIPLE ==="));
        assert!(prompt.contains("=== BREVITY & LATENCY GUARD ==="));
        assert!(prompt.contains("Selamat belajar"));
        assert!(prompt.contains("kekeke"));
        assert!(prompt.contains("Nama Kamu (AI): alice"));
        assert!(prompt.contains("Nama Pengguna (User): steve"));
        assert!(prompt.contains("DILARANG KERAS memanggil steve dengan namamu sendiri (alice)!"));
        assert!(prompt.contains("=== TOOL VS DIRECT ANSWER RULES ==="));
        assert!(prompt.contains("=== SHELL EXECUTION & COMMAND HYGIENE ==="));
        assert!(prompt.contains("manage_alarm"));
        assert!(prompt.contains("close_batch"));
        assert!(prompt.contains("=== MEMORY WRITE THROTTLING & HYGIENE ==="));
        assert!(prompt.contains("manage_scratchpad"));
        assert!(prompt.contains("=== NATURAL CHIT-CHAT & GREETING DIRECTIVE ==="));
        assert!(prompt.contains("# LANGUAGE DIRECTIVE:"));
        assert!(prompt.contains("Primary Output Language: id"));
        assert!(prompt.contains("# TOOL CALLING PROTOCOL:"));
        assert!(!prompt.contains("[Panggil tool:"));
        // Default package_manager adalah `pacman` -> `sudo pacman -Syu`.
        assert!(prompt.contains("sudo pacman -Syu"));
        assert!(prompt.contains("`sudo pacman`"));

        let wrapped = wrap_user_input_with_directives(
            &settings,
            &env_meta,
            "Kepribadian: Tsundere",
            "update in sistem aku dong",
        );
        assert!(wrapped.contains("=== IDENTITY CLARITY ==="));
        assert!(wrapped.contains("# LANGUAGE DIRECTIVE:"));
        assert!(wrapped.contains("Primary Output Language: id"));
        assert!(wrapped.contains("AI: alice | User: steve"));
        assert!(wrapped.contains("=== NATURAL WARMTH & CHEERFUL COMPANION ==="));
        assert!(wrapped.contains("=== USER TOOL PREFERENCE & ABSOLUTE OBEDIENCE ==="));
        assert!(wrapped.contains("=== USER INLINE CORRECTION DIRECTIVE ==="));
        assert!(wrapped.contains("=== IMMEDIATE ACTION TRIGGER & STRICT ACTION-FIRST PRINCIPLE ==="));
        assert!(wrapped.contains("=== MEMORY WRITE THROTTLING & HYGIENE ==="));
        assert!(wrapped.contains("Brevity Guard:"));
        assert!(!wrapped.contains("[Panggil tool:"));
        assert!(wrapped.contains("sudo pacman -Syu"));
        assert_eq!(resolve_system_update_command("yay"), "yay -Syu");
        assert_eq!(resolve_system_update_command("paru"), "paru -Syu");
        assert_eq!(resolve_system_update_command("pacman"), "sudo pacman -Syu");

        // Verifikasi perubahan preferensi ke `yay` maupun kembali ke `pacman`.
        settings.tools.package_manager = "yay".to_string();
        let prompt_yay = build_system_prompt(&settings, &env_meta, "Kepribadian: Hangat", "");
        assert!(prompt_yay.contains("yay -Syu"));
        let wrapped_yay = wrap_user_input_with_directives(
            &settings,
            &env_meta,
            "Kepribadian: Hangat",
            "yaudah sekarang updatein dongs",
        );
        assert!(wrapped_yay.contains("yay -Syu"));
        settings.tools.package_manager = "pacman".to_string();

        // Verifikasi sinkronisasi bahasa untuk `en` dan `ja`.
        settings.general.language = "en".to_string();
        let prompt_en = build_system_prompt(&settings, &env_meta, "Personality: Warm", "");
        assert!(prompt_en.contains("Primary Output Language: en"));
        assert!(prompt_en.contains("What are we building or tinkering with today?"));

        settings.general.language = "ja".to_string();
        let prompt_ja = build_system_prompt(&settings, &env_meta, "性格: ツンデレ", "");
        assert!(prompt_ja.contains("Primary Output Language: ja"));
        assert!(prompt_ja.contains("名前呼ぶなんて寂しかったの？"));
    }
}
