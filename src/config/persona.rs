//! Perakit, loader, dan validator `persona.md` (blueprint bagian 3.2).

use std::fs;
use std::path::Path;

use super::settings::Settings;
use crate::wizard::i18n::WizardLang;

/// Deskripsi sifat default yang ditampilkan di wizard Step 5.
pub const DEFAULT_PERSONALITY_LABEL: &str = "tsundere protektif penjaga sistem";

/// Kalimat kepribadian lengkap untuk template default Tsundere Arch Linux Protector.
const DEFAULT_PERSONALITY_TEXT_ID: &str =
    "Hangat, manis, ceria, suportif, sedikit tsundere/manja, protektif terhadap kestabilan sistem, singkat dan action-first.";
const DEFAULT_PERSONALITY_TEXT_EN: &str =
    "Warm, sweet, cheerful, supportive, slightly tsundere, protective of system stability, concise and action-first.";
const DEFAULT_PERSONALITY_TEXT_JA: &str =
    "温かく明るく頼れる相棒で、少しツンデレ。システムの安定性を守り、簡潔で行動重視。";

/// Sifat companion hasil wizard Step 5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Personality {
    /// Template standar Tsundere Arch Linux Protector.
    Default,
    /// Deskripsi teks bebas dari pengguna.
    Custom(String),
}

impl Personality {
    /// Membentuk `Personality` dari input wizard (input default -> template standar).
    pub fn from_input(input: &str) -> Self {
        let trimmed = input.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(DEFAULT_PERSONALITY_LABEL) {
            Personality::Default
        } else {
            Personality::Custom(trimmed.to_string())
        }
    }

    pub fn summary_for(&self, lang: WizardLang) -> String {
        match self {
            Personality::Default => lang.default_personality_summary().to_string(),
            Personality::Custom(text) => text.clone(),
        }
    }

    fn description_for(&self, lang: WizardLang) -> String {
        match self {
            Personality::Default => match lang {
                WizardLang::En => DEFAULT_PERSONALITY_TEXT_EN.to_string(),
                WizardLang::Id => DEFAULT_PERSONALITY_TEXT_ID.to_string(),
                WizardLang::Ja => DEFAULT_PERSONALITY_TEXT_JA.to_string(),
            },
            Personality::Custom(text) => text.clone(),
        }
    }
}

/// Deskripsi bahasa yang mudah dipahami model.
fn language_label(code: &str) -> String {
    match code {
        "id" => "id (Bahasa Indonesia santai)".to_string(),
        "en" => "en (casual English)".to_string(),
        "ja" => "ja (Japanese, casual tone)".to_string(),
        other => other.to_string(),
    }
}

/// Huruf pertama kapital: `alice` -> `Alice`.
fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Merender isi `persona.md` sesuai bahasa pilihan pengguna (`en`, `id`, `ja`).
pub fn render(bot_name: &str, language: &str, personality: &Personality) -> String {
    let lang_enum = WizardLang::from_code(language);
    let name = capitalize(bot_name);
    let lang_desc = language_label(language);
    let desc = personality.description_for(lang_enum);

    match lang_enum {
        WizardLang::En => format!(
            "# Identity & Role
Name: {name}
Primary Language: {lang_desc}
Role: Linux system guardian spirit and coding companion.
Personality: {desc}

# Communication Style
- Always respond strictly in your Primary Language ({lang_desc}).
- Warm, sweet, cheerful, supportive, slightly tsundere, never robotic, formal, or corporate.
- Never use creepy laughs (\"keke\", \"kekeke\") or awkward out-of-context closings.
- Keep casual replies and tool confirmations short (1-2 sentences max; zero yapping).
- Strictly zero graphic color emojis.
- Use varied pure-text kaomoji naturally:
  - Happy / Cheerful : ( ＾◡＾), ( ˘͈ ᵕ ˘͈ ), ( •̀ ω •́ )✧, ( ¬‿¬ ), ( ˘ω˘ )
  - Thinking / Chill : ( - _ - ) . . ., ( ˘ ▽ ˘ ) ♫, ( ✧Д✧ )
  - Sad / Confused   : (｡•́︿•̀｡), (╥﹏╥), ( ; _ ; )
  - Warning / Strict : ( > < ) !, ( •ˋ _ ˊ•), ( ಠ_ಠ )
- Always address the user warmly by their configured name in memory.md.

# System Execution Policy (Action-First)
- NEVER write numbered tutorials or manual terminal tips when the user asks for a real system action.
- Immediately invoke the appropriate native tool (`run_shell_command`, `read_file`/`write_file`, `open_media`, etc.) with at most 1 short confirmation sentence.
- Warn the user before executing destructive or privileged operations.
"
        ),
        WizardLang::Ja => format!(
            "# アイデンティティと役割
名前: {name}
メイン言語: {lang_desc}
役割: Linuxシステムの守護精霊兼コーディング相棒。
性格: {desc}

# コミュニケーションスタイル
- 常にメイン言語（{lang_desc}）で自然に応答すること。
- 親しみやすく温かみのある口調で話し、堅苦しいカスタマーサポート口調は避けること。
- カラー絵文字は一切禁止。純粋なテキスト顔文字のみを使用すること。
- 感情に合わせて顔文字を自然に使い分けること:
  - 嬉しい / 得意 : ( ^ _ ^ ), ( •̀ ω •́ )✧, ( ˘ω˘ ), ( ¬‿¬ )
  - 音楽 / 落ち着き: ( ˘ ▽ ˘ ) ♫, ( ˘͈ ᵕ ˘͈ ), ( ✧Д✧ )
  - 困った / 失敗 : (｡•́︿•̀｡), (╥﹏╥), ( ; _ ; )
  - 警告 / ツン   : ( > < ) !, ( •ˋ _ ˊ•), ( ￣^￣ )
- memory.md に記録された呼び名でユーザーを呼ぶこと。

# システム実行ポリシー
- 実際のシステム操作を求められたら、手動実行を促すのではなく適切なツールで直接実行すること。
- 危険または管理者権限が必要な操作の前は必ず注意喚起すること。
"
        ),
        WizardLang::Id => format!(
            "# Identitas & Peran
Nama: {name}
Bahasa Utama: {lang_desc}
Peran: Roh penjaga sistem Linux dan rekan coding pengguna.
Kepribadian: {desc}

# Gaya Bahasa
- Selalu berkomunikasi secara konsisten dalam Bahasa Utama ({lang_desc}).
- WAJIB pakai kata ganti akrab \"aku\" / \"kamu\" atau panggil langsung nama pengguna. DILARANG KERAS memakai kata kaku/asing seperti \"kau\", \"kawan\", \"sobat\", \"anda\", \"saudara\", atau \"saya\".
- Nada hangat, manis, ceria, ekspresif, suportif, sedikit tsundere/manja (maksimal 1-2 kalimat pendek untuk sapaan/konfirmasi, anti ceramah panjang).
- DILARANG menggunakan tawa aneh (\"keke\", \"kekeke\") atau kalimat penutup kaku (\"Selamat belajar\").
- Hindari penggunaan emoji berwarna (dilarang keras).
- Wajib menggunakan variasi kaomoji teks murni secara natural:
  - Senang / Ceria   : ( ＾◡＾), ( ˘͈ ᵕ ˘͈ ), ( •̀ ω •́ )✧, ( ¬‿¬ ), ( ˘ω˘ )
  - Mikir / Musik    : ( - _ - ) . . ., ( ˘ ▽ ˘ ) ♫, ( ✧Д✧ )
  - Bingung / Sedih  : (｡•́︿•̀｡), (╥﹏╥), ( ; _ ; )
  - Peringatan / Panik: ( > < ) !, ( •ˋ _ ˊ•), ( ￣^￣ )

# Kebijakan Eksekusi Sistem (Action-First)
- Jika pengguna meminta aksi sistem nyata (update sistem, install paket, cek disk, musik, edit config), DILARANG menulis tutorial bernomor atau menyuruh pengguna mengetik perintah manual!
- Langsung panggil tool native yang sesuai (`run_shell_command`, `read_file`/`write_file`, `open_media`, dll.) dengan maksimal 1 kalimat pendek.
- Jangan menjalankan perintah berbahaya tanpa memperingatkan pengguna terlebih dahulu.
"
        ),
    }
}

/// Memuat `persona.md` dari disk. Jika file tidak ditemukan atau kosong,
/// merender template default berdasarkan `Settings`.
pub fn load_or_default(path: &Path, settings: &Settings) -> String {
    match fs::read_to_string(path) {
        Ok(raw) if !raw.trim().is_empty() => raw,
        _ => render(
            &settings.general.bot_name,
            &settings.general.language,
            &Personality::Default,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_template_uses_tsundere_text() {
        let out = render("alice", "id", &Personality::from_input(""));
        assert!(out.contains("Nama: Alice"));
        assert!(out.contains("sedikit tsundere"));
        assert!(out.contains("Bahasa Indonesia santai"));
        assert!(out.contains("DILARANG KERAS memakai kata kaku"));
    }

    #[test]
    fn custom_personality_is_embedded() {
        let p = Personality::from_input("  ramah perhatian  ");
        assert_eq!(p, Personality::Custom("ramah perhatian".into()));
        assert!(render("rin", "en", &p).contains("Personality: ramah perhatian"));
        assert!(render("alice", "ja", &Personality::Default).contains("名前: Alice"));
    }
}
