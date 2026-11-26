//! Kamus lokalisasi (i18n) untuk Setup Wizard, sapaan REPL, dan penutupan sesi.

use super::detector::Provider;
use crate::config::persona::DEFAULT_PERSONALITY_LABEL;

pub const RUNNER_KAOMOJI: &str = "( •̀ ᴗ •́ )و";

/// UI language selected during Setup Wizard onboarding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardLang {
    En,
    Id,
    Ja,
}

impl WizardLang {
    pub const ALL: &'static [WizardLang] = &[WizardLang::En, WizardLang::Id, WizardLang::Ja];

    pub fn from_choice(choice: usize) -> Option<Self> {
        match choice {
            1 => Some(WizardLang::En),
            2 => Some(WizardLang::Id),
            3 => Some(WizardLang::Ja),
            _ => None,
        }
    }

    pub fn from_code(code: &str) -> Self {
        match code.trim().to_ascii_lowercase().as_str() {
            "id" => WizardLang::Id,
            "ja" => WizardLang::Ja,
            _ => WizardLang::En,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            WizardLang::En => "en",
            WizardLang::Id => "id",
            WizardLang::Ja => "ja",
        }
    }

    pub fn menu_label(self) -> &'static str {
        match self {
            WizardLang::En => "English (en)",
            WizardLang::Id => "Indonesian (id)",
            WizardLang::Ja => "Japanese (ja)",
        }
    }

    pub fn canceled_indicator(self) -> &'static str {
        match self {
            WizardLang::En => "<cancelled>",
            WizardLang::Id => "<dibatalkan>",
            WizardLang::Ja => "<キャンセル>",
        }
    }

    pub fn api_key_prompt_for_provider(self, provider: Provider) -> &'static str {
        match (self, provider) {
            (WizardLang::En, Provider::Vercel) => "Enter Vercel AI Gateway Key / Token:",
            (WizardLang::En, Provider::OpenAi) => "Enter OpenAI API Key:",
            (WizardLang::En, Provider::Anthropic) => "Enter Anthropic API Key:",
            (WizardLang::En, Provider::OpenRouter) => "Enter OpenRouter API Key:",
            (WizardLang::En, Provider::Gemini) => "Enter Google Gemini API Key:",
            (WizardLang::En, Provider::DeepSeek) => "Enter DeepSeek API Key:",
            (WizardLang::En, Provider::Custom) => "Enter Custom API Key / Token:",

            (WizardLang::Id, Provider::Vercel) => "Masukkan Vercel AI Gateway Key / Token:",
            (WizardLang::Id, Provider::OpenAi) => "Masukkan OpenAI API Key:",
            (WizardLang::Id, Provider::Anthropic) => "Masukkan Anthropic API Key:",
            (WizardLang::Id, Provider::OpenRouter) => "Masukkan OpenRouter API Key:",
            (WizardLang::Id, Provider::Gemini) => "Masukkan Google Gemini API Key:",
            (WizardLang::Id, Provider::DeepSeek) => "Masukkan DeepSeek API Key:",
            (WizardLang::Id, Provider::Custom) => "Masukkan Custom API Key / Token:",

            (WizardLang::Ja, Provider::Vercel) => {
                "Vercel AI Gateway キー / トークンを入力してください:"
            }
            (WizardLang::Ja, Provider::OpenAi) => "OpenAI API キーを入力してください:",
            (WizardLang::Ja, Provider::Anthropic) => "Anthropic API キーを入力してください:",
            (WizardLang::Ja, Provider::OpenRouter) => "OpenRouter API キーを入力してください:",
            (WizardLang::Ja, Provider::Gemini) => "Google Gemini API キーを入力してください:",
            (WizardLang::Ja, Provider::DeepSeek) => "DeepSeek API キーを入力してください:",
            (WizardLang::Ja, Provider::Custom) => {
                "カスタム API キー / トークンを入力してください:"
            }
        }
    }

    pub fn api_key_help(self) -> &'static str {
        match self {
            WizardLang::En => "Masked (*). Ctrl+R to reveal. Cannot be empty.",
            WizardLang::Id => "Disamarkan (*). Ctrl+R = intip. Tidak boleh kosong.",
            WizardLang::Ja => "マスク表示 (*)。Ctrl+R で表示切替。空欄不可。",
        }
    }

    pub fn api_key_empty_err(self) -> &'static str {
        match self {
            WizardLang::En => "API key cannot be empty (｡•́︿•̀｡)",
            WizardLang::Id => "API key tidak boleh kosong yaa (｡•́︿•̀｡)",
            WizardLang::Ja => "APIキーは空にできません (｡•́︿•̀｡)",
        }
    }

    pub fn provider_header(self) -> &'static str {
        match self {
            WizardLang::En => "Select your AI provider:",
            WizardLang::Id => "Pilih provider AI yang kamu pakai:",
            WizardLang::Ja => "使用するAIプロバイダーを選択してください:",
        }
    }

    pub fn provider_prompt(self) -> &'static str {
        match self {
            WizardLang::En => "Select provider [1-7]:",
            WizardLang::Id => "Pilih provider [1-7]:",
            WizardLang::Ja => "プロバイダーを選択 [1-7]:",
        }
    }

    pub fn numeric_help(self, max: usize) -> String {
        match self {
            WizardLang::En => format!("Type a number from 1 to {max} and press Enter (Enter = 1)."),
            WizardLang::Id => {
                format!("Ketik angka 1 sampai {max} lalu tekan Enter (Enter = 1).")
            }
            WizardLang::Ja => {
                format!("1 から {max} の数字を入力して Enter を押してください (Enter = 1)。")
            }
        }
    }

    pub fn numeric_invalid_err(self, max: usize) -> String {
        match self {
            WizardLang::En => {
                format!("Invalid choice (｡•́︿•̀｡) Please type a number between 1 and {max}.")
            }
            WizardLang::Id => {
                format!("Pilihan nggak valid nih (｡•́︿•̀｡) Ketik angka antara 1 sampai {max} yaa.")
            }
            WizardLang::Ja => {
                format!("無効な選択です (｡•́︿•̀｡) 1 から {max} の数字を入力してください。")
            }
        }
    }

    pub fn custom_provider_label(self) -> &'static str {
        match self {
            WizardLang::En => "Custom (Manual Input)",
            WizardLang::Id => "Custom (Input Manual)",
            WizardLang::Ja => "カスタム (手動入力)",
        }
    }

    pub fn base_url_prompt(self) -> &'static str {
        match self {
            WizardLang::En => "Enter custom Base URL (e.g. https://api.openai.com/v1):",
            WizardLang::Id => "Masukkan Base URL manual (contoh: https://api.openai.com/v1):",
            WizardLang::Ja => "Base URL を入力してください (例: https://api.openai.com/v1):",
        }
    }

    pub fn base_url_help(self) -> &'static str {
        match self {
            WizardLang::En => "OpenAI-compatible API base URL endpoint.",
            WizardLang::Id => "Endpoint base URL yang kompatibel dengan OpenAI.",
            WizardLang::Ja => "OpenAI 互換の API ベース URL エンドポイント。",
        }
    }

    pub fn model_header_for_provider(self, provider_label: &str) -> String {
        match self {
            WizardLang::En => format!("Select {provider_label} model:"),
            WizardLang::Id => format!("Pilih model {provider_label}:"),
            WizardLang::Ja => format!("{provider_label} のモデルを選択:"),
        }
    }

    pub fn custom_model_option(self) -> &'static str {
        match self {
            WizardLang::En => "Enter model ID manually",
            WizardLang::Id => "Input model ID manual",
            WizardLang::Ja => "モデルIDを手動入力",
        }
    }

    pub fn model_choice_prompt(self, max: usize) -> String {
        match self {
            WizardLang::En => format!("Select model [1-{max}]:"),
            WizardLang::Id => format!("Pilih model [1-{max}]:"),
            WizardLang::Ja => format!("モデルを選択 [1-{max}]:"),
        }
    }

    pub fn custom_model_prompt(self) -> &'static str {
        match self {
            WizardLang::En => "Enter model ID manually:",
            WizardLang::Id => "Masukkan model ID manual:",
            WizardLang::Ja => "モデルIDを入力してください:",
        }
    }

    pub fn custom_model_help(self) -> &'static str {
        match self {
            WizardLang::En => "Type the model identifier from your provider (cannot be empty).",
            WizardLang::Id => "Ketik model ID secara manual (tidak boleh kosong).",
            WizardLang::Ja => "プロバイダーのモデル識別子を入力してください (空欄不可)。",
        }
    }

    pub fn bot_name_prompt(self, default_bot: &str) -> String {
        match self {
            WizardLang::En => {
                format!("[3/6] What would you like to name your AI companion? [default: {default_bot}]:")
            }
            WizardLang::Id => {
                format!("[3/6] Kamu mau panggil AI companion ini siapa? [default: {default_bot}]:")
            }
            WizardLang::Ja => {
                format!("[3/6] AIコンパニオンの名前を決めてください [default: {default_bot}]:")
            }
        }
    }

    pub fn bot_name_help(self) -> &'static str {
        match self {
            WizardLang::En => "Used as the prompt label: <name>@waish. Press Enter for default.",
            WizardLang::Id => "Nama ini jadi label prompt: <nama>@waish. Enter = pakai default.",
            WizardLang::Ja => "プロンプト表示 <名前>@waish に使われます。Enter でデフォルト。",
        }
    }

    pub fn user_name_prompt(self, default_user: &str) -> String {
        match self {
            WizardLang::En => {
                format!("[4/6] What should your companion call you? [default: {default_user}]:")
            }
            WizardLang::Id => {
                format!("[4/6] Companion kamu harus manggil kamu apa? [default: {default_user}]:")
            }
            WizardLang::Ja => {
                format!("[4/6] コンパニオンに何と呼ばれたいですか？ [default: {default_user}]:")
            }
        }
    }

    pub fn user_name_help(self) -> &'static str {
        match self {
            WizardLang::En => "Saved in memory.md so your companion always addresses you by this name.",
            WizardLang::Id => "Disimpan di memory.md agar companion selalu menyapa dengan nama ini.",
            WizardLang::Ja => "memory.md に保存され、会話でいつもこの名前で呼ばれます。",
        }
    }

    pub fn persona_header(self) -> &'static str {
        match self {
            WizardLang::En => "Companion Character & Personality (Persona):",
            WizardLang::Id => "Karakter & Sifat Companion (Persona):",
            WizardLang::Ja => "コンパニオンの性格・ペルソナ設定 (Persona):",
        }
    }

    pub fn persona_opt_default(self) -> &'static str {
        match self {
            WizardLang::En => "Use built-in default persona",
            WizardLang::Id => "Gunakan persona bawaan (default)",
            WizardLang::Ja => "デフォルトのペルソナを使う",
        }
    }

    pub fn persona_opt_editor(self) -> &'static str {
        match self {
            WizardLang::En => "Open editor to write/paste custom persona",
            WizardLang::Id => "Buka editor untuk tulis/paste persona sendiri",
            WizardLang::Ja => "エディタを開いてカスタムペルソナを入力する",
        }
    }

    pub fn persona_prompt(self) -> &'static str {
        match self {
            WizardLang::En => "Select persona option [1-2]:",
            WizardLang::Id => "Pilih opsi persona [1-2]:",
            WizardLang::Ja => "ペルソナの選択 [1-2]:",
        }
    }

    pub fn persona_help(self) -> &'static str {
        match self {
            WizardLang::En => "Type 1 for standard persona, or 2 to open your terminal editor.",
            WizardLang::Id => "Ketik 1 untuk sifat standar, atau 2 untuk membuka editor terminal.",
            WizardLang::Ja => "1 で標準ペルソナ、2 でターミナルエディタを開きます。",
        }
    }

    pub fn default_personality_summary(self) -> &'static str {
        match self {
            WizardLang::En => "protective tsundere system guardian (default)",
            WizardLang::Id => DEFAULT_PERSONALITY_LABEL,
            WizardLang::Ja => "少しツンデレで頼れるシステム守護者 (デフォルト)",
        }
    }

    pub fn persona_using_default(self) -> &'static str {
        match self {
            WizardLang::En => "Using default persona:",
            WizardLang::Id => "Menggunakan persona standar:",
            WizardLang::Ja => "デフォルトのペルソナを使用します:",
        }
    }

    pub fn persona_saved_draft(self) -> &'static str {
        match self {
            WizardLang::En => "Custom persona staged in draft:",
            WizardLang::Id => "Persona tersimpan di draft:",
            WizardLang::Ja => "カスタムペルソナを下書きに保存しました:",
        }
    }

    pub fn memory_header(self) -> &'static str {
        match self {
            WizardLang::En => "Initial Memory & Context (Memory Bank):",
            WizardLang::Id => "Memori & Konteks Awal (Memory Bank):",
            WizardLang::Ja => "初期メモリとコンテキスト (Memory Bank):",
        }
    }

    pub fn memory_opt_clean(self) -> &'static str {
        match self {
            WizardLang::En => "Start with a clean memory template (default)",
            WizardLang::Id => "Mulai dengan memori bersih (default template)",
            WizardLang::Ja => "クリーンなメモリテンプレートで開始する (デフォルト)",
        }
    }

    pub fn memory_opt_editor(self) -> &'static str {
        match self {
            WizardLang::En => "Open editor to paste profile/history (from ChatGPT/Claude)",
            WizardLang::Id => "Buka editor untuk paste riwayat/profil (dari ChatGPT/Claude)",
            WizardLang::Ja => "エディタを開いてプロフィールや履歴を貼り付ける (ChatGPT/Claude等)",
        }
    }

    pub fn memory_prompt(self) -> &'static str {
        match self {
            WizardLang::En => "Select memory option [1-2]:",
            WizardLang::Id => "Pilih opsi memori [1-2]:",
            WizardLang::Ja => "メモリの選択 [1-2]:",
        }
    }

    pub fn memory_help(self) -> &'static str {
        match self {
            WizardLang::En => "Type 1 for a clean template, or 2 to open your terminal editor.",
            WizardLang::Id => "Ketik 1 untuk template bersih, atau 2 untuk membuka editor terminal.",
            WizardLang::Ja => "1 でクリーンなテンプレート、2 でエディタを開きます。",
        }
    }

    pub fn memory_clean_ok(self) -> &'static str {
        match self {
            WizardLang::En => "memory.md will start with a clean template.",
            WizardLang::Id => "memory.md akan memakai template bersih.",
            WizardLang::Ja => "memory.md はクリーンなテンプレートで初期化されます。",
        }
    }

    pub fn memory_editor_empty(self) -> &'static str {
        match self {
            WizardLang::En => "Editor closed with no new content; memory.md will use a clean template.",
            WizardLang::Id => "Editor ditutup tanpa isi baru; memory.md akan memakai template bersih.",
            WizardLang::Ja => "内容が空のまま閉じられました。memory.md はクリーンなテンプレートを使用します。",
        }
    }

    pub fn memory_lines_imported(self, count: usize) -> String {
        match self {
            WizardLang::En => format!("{count} lines of memory imported from editor."),
            WizardLang::Id => format!("{count} baris memori diterima dari editor."),
            WizardLang::Ja => format!("エディタから {count} 行のメモリを取り込みました。"),
        }
    }

    pub fn summary_title(self) -> &'static str {
        match self {
            WizardLang::En => "Configuration Summary",
            WizardLang::Id => "Ringkasan Pengaturan",
            WizardLang::Ja => "設定サマリー",
        }
    }

    pub fn summary_labels(self) -> [&'static str; 11] {
        match self {
            WizardLang::En => [
                "Companion",
                "Call you",
                "Language",
                "Provider",
                "Model",
                "Base URL",
                "API Key",
                "Desktop",
                "AUR Tool",
                "Persona",
                "Memory",
            ],
            WizardLang::Id => [
                "Companion",
                "Panggilan",
                "Bahasa",
                "Provider",
                "Model",
                "Base URL",
                "API Key",
                "Desktop",
                "AUR Tool",
                "Sifat",
                "Memori",
            ],
            WizardLang::Ja => [
                "Companion",
                "呼び名",
                "言語",
                "Provider",
                "Model",
                "Base URL",
                "API Key",
                "Desktop",
                "AUR Tool",
                "ペルソナ",
                "メモリ",
            ],
        }
    }

    pub fn summary_memory_value(self, imported_lines: Option<usize>) -> String {
        match (self, imported_lines) {
            (WizardLang::En, Some(n)) => format!("imported ({n} lines)"),
            (WizardLang::En, None) => "clean template".to_string(),
            (WizardLang::Id, Some(n)) => format!("diimpor ({n} baris)"),
            (WizardLang::Id, None) => "template bersih".to_string(),
            (WizardLang::Ja, Some(n)) => format!("インポート済み ({n} 行)"),
            (WizardLang::Ja, None) => "クリーンテンプレート".to_string(),
        }
    }

    pub fn confirm_save_prompt(self) -> &'static str {
        match self {
            WizardLang::En => "Save configuration and start session?",
            WizardLang::Id => "Simpan dan mulai sesi?",
            WizardLang::Ja => "この設定を保存してセッションを開始しますか？",
        }
    }

    pub fn confirm_save_help(self) -> &'static str {
        match self {
            WizardLang::En => "Y = write config.toml, persona.md, and memory.md atomically",
            WizardLang::Id => "Y = tulis config.toml, persona.md, memory.md sekaligus",
            WizardLang::Ja => "Y = config.toml, persona.md, memory.md を一括保存します",
        }
    }

    pub fn setup_complete_message(self, user_name: &str, dir_display: &str) -> String {
        match self {
            WizardLang::En => format!(
                "Setup complete! Nice to meet you, {user_name}. Configuration saved to {dir_display}"
            ),
            WizardLang::Id => format!(
                "Setup selesai! Senang bertemu denganmu, {user_name}. Konfigurasi tersimpan di {dir_display}"
            ),
            WizardLang::Ja => format!(
                "セットアップ完了！よろしくね、{user_name}。設定を {dir_display} に保存したよ"
            ),
        }
    }

    pub fn backup_notice(self, backup_display: &str) -> String {
        match self {
            WizardLang::En => format!("previous file backed up to {backup_display}"),
            WizardLang::Id => format!("file lama dicadangkan ke {backup_display}"),
            WizardLang::Ja => format!("以前のファイルを {backup_display} にバックアップしました"),
        }
    }

    pub fn declined_message(self) -> &'static str {
        match self {
            WizardLang::En => {
                "Okay, nothing was saved. Run `waish` again anytime to restart setup."
            }
            WizardLang::Id => {
                "Oke, tidak ada yang disimpan. Jalankan `waish` lagi kapan pun untuk mengulang setup."
            }
            WizardLang::Ja => {
                "了解、何も保存しなかったよ。やり直すときはいつでも `waish` を実行してね。"
            }
        }
    }

    pub fn cancelled_line1(self) -> &'static str {
        match self {
            WizardLang::En => "Setup cancelled. No files were written to disk.",
            WizardLang::Id => "Setup dibatalkan. Tenang, tidak ada file yang ditulis ke disk.",
            WizardLang::Ja => "セットアップをキャンセルしたよ。ディスクには何も書き込まれていないよ。",
        }
    }

    pub fn cancelled_line2(self) -> &'static str {
        match self {
            WizardLang::En => "Run `waish` again to start from Step 1.",
            WizardLang::Id => "Jalankan `waish` lagi untuk mulai dari Step 1.",
            WizardLang::Ja => "最初からやり直すには `waish` を再実行してね。",
        }
    }

    pub fn empty_value_err(self, has_default: bool) -> &'static str {
        match (self, has_default) {
            (WizardLang::En, true) => {
                "Cannot be whitespace only (｡•́︿•̀｡) Clear spaces and press Enter for default."
            }
            (WizardLang::En, false) => "This value cannot be empty (｡•́︿•̀｡)",
            (WizardLang::Id, true) => {
                "Tidak boleh kosong yaa (｡•́︿•̀｡) Hapus spasinya lalu Enter untuk pakai default."
            }
            (WizardLang::Id, false) => "Tidak boleh kosong yaa (｡•́︿•̀｡)",
            (WizardLang::Ja, true) => {
                "空白のみは入力できません (｡•́︿•̀｡) デフォルトを使うには空のまま Enter を押してください。"
            }
            (WizardLang::Ja, false) => "空欄にはできません (｡•́︿•̀｡)",
        }
    }

    pub fn name_control_err(self) -> &'static str {
        match self {
            WizardLang::En => "Name cannot contain control characters (｡•́︿•̀｡)",
            WizardLang::Id => "Nama tidak boleh mengandung karakter kontrol (｡•́︿•̀｡)",
            WizardLang::Ja => "名前に制御文字を含めることはできません (｡•́︿•̀｡)",
        }
    }

    pub fn name_too_long_err(self, max: usize) -> String {
        match self {
            WizardLang::En => format!("Too long, maximum {max} characters (｡•́︿•̀｡)"),
            WizardLang::Id => format!("Kepanjangan, maksimal {max} karakter yaa (｡•́︿•̀｡)"),
            WizardLang::Ja => format!("長すぎます。最大 {max} 文字までです (｡•́︿•̀｡)"),
        }
    }

    pub fn initial_memory_session_note(self) -> &'static str {
        match self {
            WizardLang::En => "Initial waish setup completed successfully.",
            WizardLang::Id => "Sesi inisialisasi awal sistem waish berhasil diselesaikan.",
            WizardLang::Ja => "waish の初期セットアップが完了しました。",
        }
    }

    pub fn repl_greeting(self, user_name: &str) -> (String, &'static str) {
        match self {
            WizardLang::En => (
                format!("Hello, {user_name}! What are we building or tinkering with today?"),
                "(type /help for commands)",
            ),
            WizardLang::Id => (
                format!("Halo, {user_name}! Mau pasang atau oprek apa kita hari ini?"),
                "(ketik /help untuk daftar perintah)",
            ),
            WizardLang::Ja => (
                format!("こんにちは、{user_name}！今日は何を構築・カスタマイズしようか？"),
                "(/help でコマンド一覧)",
            ),
        }
    }

    pub fn repl_farewell_empty(self, user_name: &str) -> String {
        match self {
            WizardLang::En => {
                format!("See you, {user_name}! Don't forget to take a break. Catch you next session!")
            }
            WizardLang::Id => {
                format!("Dahhh, {user_name}! Jangan lupa istirahat yaa. Sampai ketemu di sesi berikutnya!")
            }
            WizardLang::Ja => {
                format!("またね、{user_name}！ちゃんと休憩してね。次のセッションでまた会おう！")
            }
        }
    }

    pub fn repl_farewell_consolidating(self, user_name: &str) -> String {
        match self {
            WizardLang::En => format!("See you, {user_name}! Tidying up system memory for a moment..."),
            WizardLang::Id => format!("Dahhh, {user_name}! Menata ingatan sistem sebentar..."),
            WizardLang::Ja => format!("またね、{user_name}！システムの記憶を少し整理しているよ..."),
        }
    }

    pub fn action_box_title(self) -> &'static str {
        match self {
            WizardLang::En => "Action Details",
            WizardLang::Id => "Detail Aksi",
            WizardLang::Ja => "アクション詳細",
        }
    }

    pub fn yes_no(self, yes: bool) -> &'static str {
        match (self, yes) {
            (WizardLang::En, true) => "yes",
            (WizardLang::En, false) => "no",
            (WizardLang::Id, true) => "ya",
            (WizardLang::Id, false) => "tidak",
            (WizardLang::Ja, true) => "はい",
            (WizardLang::Ja, false) => "いいえ",
        }
    }

    pub fn shell_action_lines(
        self,
        command: &str,
        intent: &str,
        requires_root: bool,
    ) -> Vec<String> {
        let root_val = self.yes_no(requires_root);
        match self {
            WizardLang::En => vec![
                format!("Command : {command}"),
                format!("Intent  : {intent}"),
                format!("Root    : {root_val}"),
            ],
            WizardLang::Id => vec![
                format!("Command  : {command}"),
                format!("Tujuan   : {intent}"),
                format!("Root     : {root_val}"),
            ],
            WizardLang::Ja => vec![
                format!("コマンド : {command}"),
                format!("目的     : {intent}"),
                format!("ルート   : {root_val}"),
            ],
        }
    }

    pub fn file_action_lines(
        self,
        action: &str,
        target: &str,
        detail: &str,
        requires_root: bool,
    ) -> Vec<String> {
        let root_val = self.yes_no(requires_root);
        match self {
            WizardLang::En => vec![
                format!("Action  : {action}"),
                format!("Target  : {target}"),
                format!("Detail  : {detail}"),
                format!("Root    : {root_val}"),
            ],
            WizardLang::Id => vec![
                format!("Action   : {action}"),
                format!("Target   : {target}"),
                format!("Detail   : {detail}"),
                format!("Root     : {root_val}"),
            ],
            WizardLang::Ja => vec![
                format!("操作     : {action}"),
                format!("対象     : {target}"),
                format!("詳細     : {detail}"),
                format!("ルート   : {root_val}"),
            ],
        }
    }

    pub fn inline_correction_hint_tag(self) -> &'static str {
        match self {
            WizardLang::En => "[y/N/type your correction]",
            WizardLang::Id => "[y/N/ketik koreksimu]",
            WizardLang::Ja => "[y/N/修正内容を入力]",
        }
    }

    pub fn action_confirm_prompt_styled(
        self,
        kaomoji_styled: &str,
        command_styled: &str,
        hint_tag_styled: &str,
    ) -> String {
        match self {
            WizardLang::En => {
                format!("{kaomoji_styled} Execute command '{command_styled}' now? {hint_tag_styled}: ")
            }
            WizardLang::Id => {
                format!("{kaomoji_styled} Jalankan command '{command_styled}' sekarang? {hint_tag_styled}: ")
            }
            WizardLang::Ja => {
                format!("{kaomoji_styled} コマンド '{command_styled}' を実行しますか？ {hint_tag_styled}: ")
            }
        }
    }

    #[cfg(test)]
    pub fn action_confirm_prompt(self, command: &str) -> String {
        self.action_confirm_prompt_styled("( > < ) !", command, self.inline_correction_hint_tag())
    }

    #[cfg(test)]
    pub fn root_confirm_prompt(self, command: &str, yn_tag: &str) -> String {
        match self {
            WizardLang::En => format!("Execute command '{command}' now? {yn_tag}: "),
            WizardLang::Id => format!("Jalankan command '{command}' sekarang? {yn_tag}: "),
            WizardLang::Ja => format!("コマンド '{command}' を実行しますか？ {yn_tag}: "),
        }
    }

    pub fn inline_correction_adjusting(self) -> &'static str {
        match self {
            WizardLang::En => "Okay, adjusting the command for you...",
            WizardLang::Id => "Oke, aku sesuaikan perintahnya yaa...",
            WizardLang::Ja => "わかったよ、コマンドを調整するね...",
        }
    }

    pub fn root_cancelled(self) -> &'static str {
        match self {
            WizardLang::En => "Root command execution cancelled.",
            WizardLang::Id => "Eksekusi command root dibatalkan yaa.",
            WizardLang::Ja => "ルートコマンドの実行がキャンセルされました。",
        }
    }

    pub fn root_blocked_non_tty(self) -> &'static str {
        match self {
            WizardLang::En => {
                "Root execution was automatically blocked because this is not an interactive terminal."
            }
            WizardLang::Id => {
                "Eksekusi command root diblokir otomatis karena tidak berjalan di terminal interaktif."
            }
            WizardLang::Ja => {
                "対話型ターミナルではないため、ルートコマンドの実行を自動ブロックしました。"
            }
        }
    }

    pub fn danger_confirm_keyword(self) -> &'static str {
        match self {
            WizardLang::En => "confirm",
            WizardLang::Id => "konfirmasi",
            WizardLang::Ja => "confirm",
        }
    }

    pub fn danger_warning_header(self, risk: &str) -> String {
        match self {
            WizardLang::En => format!("Hold on! This action touches a critical system area ({risk}):"),
            WizardLang::Id => {
                format!("Tunggu dulu! Aksi ini menyentuh area kritis sistem ({risk}):")
            }
            WizardLang::Ja => {
                format!("ちょっと待って！この操作は重要なシステム領域に影響します ({risk}):")
            }
        }
    }

    pub fn danger_blocked_non_tty(self) -> &'static str {
        match self {
            WizardLang::En => {
                "Execution blocked automatically because this is not an interactive terminal."
            }
            WizardLang::Id => {
                "Eksekusi diblokir otomatis karena tidak berjalan di terminal interaktif."
            }
            WizardLang::Ja => "対話型ターミナルではないため、実行を自動ブロックしました。",
        }
    }

    pub fn danger_type_keyword_prompt(self, keyword_colored: &str) -> String {
        match self {
            WizardLang::En => {
                format!("Type '{keyword_colored}' if you really want to force execution: ")
            }
            WizardLang::Id => {
                format!("Ketik '{keyword_colored}' kalau kamu beneran mau lanjut eksekusi: ")
            }
            WizardLang::Ja => {
                format!("本当に実行する場合は '{keyword_colored}' と入力してください: ")
            }
        }
    }

    pub fn danger_confirmed(self) -> &'static str {
        match self {
            WizardLang::En => "Okay, confirmed. Be careful, executing now...",
            WizardLang::Id => "Oke, sudah dikonfirmasi. Hati-hati yaa, aku jalankan sekarang...",
            WizardLang::Ja => "了解、確認できたよ。気をつけて実行するね...",
        }
    }

    pub fn danger_cancelled(self) -> &'static str {
        match self {
            WizardLang::En => "Phew, blocked in time! Critical system operation cancelled.",
            WizardLang::Id => "Huft, untung dicegat! Operasi pada area kritis sistem dibatalkan.",
            WizardLang::Ja => "ふぅ、危なかった！重要なシステム操作をキャンセルしました。",
        }
    }

    pub fn runner_executing(self, tool_colored: &str) -> String {
        match self {
            WizardLang::En => format!("Executing {tool_colored}..."),
            WizardLang::Id => format!("Sedang menjalankan {tool_colored}..."),
            WizardLang::Ja => format!("{tool_colored} を実行中..."),
        }
    }

    pub fn pty_progress_prefix(self) -> &'static str {
        match self {
            WizardLang::En => "Processing...",
            WizardLang::Id => "Memproses...",
            WizardLang::Ja => "処理中...",
        }
    }

    pub fn pty_done(self) -> &'static str {
        match self {
            WizardLang::En => "Done!",
            WizardLang::Id => "Selesai!",
            WizardLang::Ja => "完了!",
        }
    }

    pub fn pty_cancelled(self) -> &'static str {
        match self {
            WizardLang::En => "Cancelled (Ctrl+C)",
            WizardLang::Id => "Dibatalkan (Ctrl+C)",
            WizardLang::Ja => "キャンセル (Ctrl+C)",
        }
    }

    pub fn pty_failed(self) -> &'static str {
        match self {
            WizardLang::En => "Error!",
            WizardLang::Id => "Error!",
            WizardLang::Ja => "エラー!",
        }
    }

    pub fn pty_needs_input_label(self) -> &'static str {
        match self {
            WizardLang::En => "[Input required]:",
            WizardLang::Id => "[Sistem butuh pilihan]:",
            WizardLang::Ja => "[入力が必要です]:",
        }
    }

    pub fn empty_server_response(self) -> &'static str {
        match self {
            WizardLang::En => "(server returned no response text)",
            WizardLang::Id => "(server tidak mengirimkan teks balasan)",
            WizardLang::Ja => "(サーバーからテキスト応答がありませんでした)",
        }
    }

    pub fn err_missing_api_key(self, provider: &str, cfg_cmd: &str) -> String {
        match self {
            WizardLang::En => format!(
                "API key for {provider} is not set yet. Check your config via {cfg_cmd}."
            ),
            WizardLang::Id => format!(
                "API key untuk {provider} belum ada nih. Cek config lewat {cfg_cmd} yaa."
            ),
            WizardLang::Ja => format!(
                "{provider} のAPIキーがまだ設定されていません。{cfg_cmd} で設定を確認してね。"
            ),
        }
    }

    pub fn err_unauthorized(self, provider: &str, cfg_cmd: &str) -> String {
        match self {
            WizardLang::En => format!(
                "Hmm, the {provider} API key was rejected (401/403). Please verify it via {cfg_cmd}."
            ),
            WizardLang::Id => format!(
                "Hmm, API key {provider} ditolak server (401/403). Coba cek lagi lewat {cfg_cmd} yaa."
            ),
            WizardLang::Ja => format!(
                "うーん、{provider} のAPIキーが拒否されました (401/403)。{cfg_cmd} で再確認してね。"
            ),
        }
    }

    pub fn err_rate_limited(self, provider: &str, model_cmd: &str) -> String {
        match self {
            WizardLang::En => format!(
                "We hit a rate limit or quota cap on {provider} (429). Wait a bit or switch models via {model_cmd}."
            ),
            WizardLang::Id => format!(
                "Aduh, kita kena rate-limit atau kuota {provider} habis (429). Tunggu sebentar atau ganti model via {model_cmd} yaa."
            ),
            WizardLang::Ja => format!(
                "{provider} のレート制限またはクォータ上限に達しました (429)。少し待つか {model_cmd} でモデルを切り替えてね。"
            ),
        }
    }

    pub fn err_timeout(self, secs: u64) -> String {
        match self {
            WizardLang::En => format!(
                "Server did not respond within {secs} seconds (timeout). Try sending your message again."
            ),
            WizardLang::Id => format!(
                "Server belum merespon setelah {secs} detik (timeout). Coba kirim ulang pesannya yaa."
            ),
            WizardLang::Ja => format!(
                "{secs} 秒以内にサーバーから応答がありませんでした (タイムアウト)。もう一度送ってみてね。"
            ),
        }
    }

    pub fn err_network(self, provider: &str) -> String {
        match self {
            WizardLang::En => format!(
                "Connection to {provider} failed or dropped. Make sure the internet (or local server) is running."
            ),
            WizardLang::Id => format!(
                "Koneksi ke {provider} error atau terputus. Pastikan internet (atau server lokal) nyala yaa."
            ),
            WizardLang::Ja => format!(
                "{provider} への接続に失敗しました。インターネットまたはローカルサーバーを確認してね。"
            ),
        }
    }

    pub fn err_http_status(self, status: &str, model: &str) -> String {
        match self {
            WizardLang::En => format!(
                "Server returned HTTP error {status}. Check model {model} or retry in a moment."
            ),
            WizardLang::Id => format!(
                "Server mengembalikan HTTP error {status}. Coba cek model {model} atau ulangi sebentar lagi yaa."
            ),
            WizardLang::Ja => format!(
                "サーバーが HTTP エラー {status} を返しました。モデル {model} を確認するか、少し待って再試行してね。"
            ),
        }
    }

    pub fn help_box_title(self) -> &'static str {
        match self {
            WizardLang::En => "Slash Commands",
            WizardLang::Id => "Daftar Slash Commands",
            WizardLang::Ja => "スラッシュコマンド一覧",
        }
    }

    pub fn help_rows(self) -> Vec<String> {
        match self {
            WizardLang::En => vec![
                "/sh <cmd>      : Run a local shell command directly".to_string(),
                "/clear         : Clear the terminal screen".to_string(),
                "/drop          : Clear active session conversation history".to_string(),
                "/model [name]  : Show or switch the active LLM model".to_string(),
                "/memory        : Show long-term memory bank (memory.md)".to_string(),
                "/note [text]   : View, add, or remove scratchpad notes (/note del <id>)"
                    .to_string(),
                "/telemetry     : Show battery, RAM, CPU temp & watcher state".to_string(),
                "/uninstall     : Run the interactive waish uninstaller".to_string(),
                "/help          : Show this slash commands menu".to_string(),
                "/exit, /q      : Exit REPL safely (or press Ctrl+D / Ctrl+Q)".to_string(),
            ],
            WizardLang::Id => vec![
                "/sh <cmd>      : Jalankan command shell lokal secara langsung".to_string(),
                "/clear         : Bersihkan layar terminal".to_string(),
                "/drop          : Kosongkan riwayat percakapan sesi aktif".to_string(),
                "/model [nama]  : Tampilkan atau ganti model LLM sesi ini".to_string(),
                "/memory        : Tampilkan isi memory bank (memory.md)".to_string(),
                "/note [teks]   : Lihat, tambah, atau hapus catatan scratchpad (/note del <id>)"
                    .to_string(),
                "/telemetry     : Tampilkan status baterai, RAM, suhu CPU & watcher".to_string(),
                "/uninstall     : Jalankan skrip uninstall interaktif waish".to_string(),
                "/help          : Tampilkan menu bantuan slash commands ini".to_string(),
                "/exit, /q      : Keluar dari sesi REPL dengan aman (atau Ctrl+D / Ctrl+Q)"
                    .to_string(),
            ],
            WizardLang::Ja => vec![
                "/sh <cmd>      : ローカルのシェルコマンドを直接実行".to_string(),
                "/clear         : ターミナル画面をクリア".to_string(),
                "/drop          : 現在のセッション会話履歴をクリア".to_string(),
                "/model [名前]  : 使用中のLLMモデルを表示または切替".to_string(),
                "/memory        : メモリバンク (memory.md) を表示".to_string(),
                "/note [内容]   : スクラッチパッドを表示・追加・削除 (/note del <id>)"
                    .to_string(),
                "/telemetry     : バッテリー・RAM・CPU温度とウォッチャー状態を表示".to_string(),
                "/uninstall     : waish の対話型アンインストーラを実行".to_string(),
                "/help          : このスラッシュコマンド一覧を表示".to_string(),
                "/exit, /q      : REPL を安全に終了 (Ctrl+D / Ctrl+Q でも可)".to_string(),
            ],
        }
    }

    pub fn drop_cleared(self, count: usize, mem_file: &str) -> String {
        match self {
            WizardLang::En => format!(
                "Active session context cleared ({count} turns removed). {mem_file} remains intact."
            ),
            WizardLang::Id => format!(
                "Konteks percakapan sesi aktif dikosongkan ({count} pesan dihapus). File {mem_file} tetap aman."
            ),
            WizardLang::Ja => format!(
                "現在の会話履歴をクリアしました ({count} 件削除)。{mem_file} はそのまま安全です。"
            ),
        }
    }

    pub fn model_current(self, current: &str, provider: &str, hint: &str) -> String {
        match self {
            WizardLang::En => {
                format!("Active model: {current} (provider: {provider}). Use {hint} to switch.")
            }
            WizardLang::Id => {
                format!("Model aktif sesi ini: {current} (provider: {provider}). Ketik {hint} untuk mengganti.")
            }
            WizardLang::Ja => {
                format!("現在のモデル: {current} (プロバイダー: {provider})。{hint} で切り替えられます。")
            }
        }
    }

    pub fn model_switched(self, prev: &str, next: &str) -> String {
        match self {
            WizardLang::En => format!("Active session model switched: {prev} -> {next}"),
            WizardLang::Id => format!("Model sesi aktif diganti: {prev} -> {next}"),
            WizardLang::Ja => format!("セッションモデルを切り替えました: {prev} -> {next}"),
        }
    }

    pub fn unknown_slash_cmd(self, unknown: &str, help_hint: &str) -> String {
        match self {
            WizardLang::En => {
                format!("Unknown command {unknown}. Type {help_hint} to see available commands.")
            }
            WizardLang::Id => {
                format!("Command {unknown} belum dikenali. Ketik {help_hint} untuk lihat daftar slash commands yaa.")
            }
            WizardLang::Ja => {
                format!("不明なコマンド {unknown} です。{help_hint} でコマンド一覧を確認してね。")
            }
        }
    }

    pub fn sh_missing_cmd(self, sh_cmd: &str, example: &str) -> String {
        match self {
            WizardLang::En => format!("Enter a shell command after {sh_cmd}. Example: {example}"),
            WizardLang::Id => {
                format!("Masukkan command shell setelah {sh_cmd}. Contoh: {example}")
            }
            WizardLang::Ja => format!("{sh_cmd} の後にシェルコマンドを入力してね。例: {example}"),
        }
    }

    pub fn sh_exit_status(self, code: &str) -> String {
        match self {
            WizardLang::En => format!("Command exited with status {code}."),
            WizardLang::Id => format!("Command selesai dengan exit status {code}."),
            WizardLang::Ja => format!("コマンドは終了コード {code} で終了しました。"),
        }
    }

    pub fn sh_spawn_failed(self, shell: &str, err: &std::io::Error) -> String {
        match self {
            WizardLang::En => format!("Failed to start shell '{shell}': {err}"),
            WizardLang::Id => format!("Error menjalankan shell '{shell}': {err}"),
            WizardLang::Ja => format!("シェル '{shell}' の起動に失敗しました: {err}"),
        }
    }

    pub fn scratchpad_cleared(self, count: usize) -> String {
        match self {
            WizardLang::En => format!("Scratchpad cleared ({count} notes removed)."),
            WizardLang::Id => format!("Scratchpad dikosongkan ({count} catatan dihapus)."),
            WizardLang::Ja => format!("スクラッチパッドをクリアしました ({count} 件削除)。"),
        }
    }

    pub fn scratchpad_removed(self, id: u64) -> String {
        match self {
            WizardLang::En => format!("Scratchpad note #{id} removed."),
            WizardLang::Id => format!("Catatan scratchpad #{id} dihapus."),
            WizardLang::Ja => format!("スクラッチパッドのメモ #{id} を削除しました。"),
        }
    }

    pub fn scratchpad_not_found(self, id: u64) -> String {
        match self {
            WizardLang::En => format!("Note #{id} was not found in scratchpad."),
            WizardLang::Id => format!("Catatan #{id} tidak ditemukan di scratchpad."),
            WizardLang::Ja => format!("メモ #{id} はスクラッチパッドに見つかりませんでした。"),
        }
    }

    pub fn scratchpad_del_usage(self) -> &'static str {
        match self {
            WizardLang::En => "Usage: `/note del <id>` (example: `/note del 1`).",
            WizardLang::Id => "Format hapus catatan: `/note del <id>` (contoh: `/note del 1`).",
            WizardLang::Ja => "削除形式: `/note del <id>` (例: `/note del 1`)。",
        }
    }

    pub fn scratchpad_saved(self, id: u64, content: &str) -> String {
        match self {
            WizardLang::En => format!("Saved note #{id} to scratchpad: {content}"),
            WizardLang::Id => format!("Catatan #{id} disimpan ke scratchpad: {content}"),
            WizardLang::Ja => format!("メモ #{id} をスクラッチパッドに保存しました: {content}"),
        }
    }

    pub fn status_checking(self, provider: &str, model: &str) -> String {
        match self {
            WizardLang::En => format!("Checking connection to {provider} (model: {model})..."),
            WizardLang::Id => format!("Mengecek koneksi ke provider {provider} (model: {model})..."),
            WizardLang::Ja => format!("{provider} への接続を確認中 (モデル: {model})..."),
        }
    }

    pub fn memory_view_header(self, user_name: &str) -> String {
        match self {
            WizardLang::En => format!("Here is what I remember about {user_name}:"),
            WizardLang::Id => format!("Ini yang aku ingat tentang {user_name}:"),
            WizardLang::Ja => format!("{user_name} について覚えていること:"),
        }
    }

    pub fn memory_empty(self) -> &'static str {
        match self {
            WizardLang::En => "(memory bank is empty)",
            WizardLang::Id => "(memory bank masih kosong)",
            WizardLang::Ja => "(メモリバンクは空です)",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_numeric_language_choices_and_codes_accurately() {
        assert_eq!(WizardLang::from_choice(1), Some(WizardLang::En));
        assert_eq!(WizardLang::from_choice(2), Some(WizardLang::Id));
        assert_eq!(WizardLang::from_choice(3), Some(WizardLang::Ja));
        assert_eq!(WizardLang::from_choice(0), None);
        assert_eq!(WizardLang::from_choice(4), None);

        assert_eq!(WizardLang::from_code("en"), WizardLang::En);
        assert_eq!(WizardLang::from_code("ID"), WizardLang::Id);
        assert_eq!(WizardLang::from_code("ja"), WizardLang::Ja);
        assert_eq!(WizardLang::from_code("unknown"), WizardLang::En);
    }

    #[test]
    fn localized_greetings_and_completion_messages_match_language() {
        assert!(WizardLang::En.repl_greeting("steve").0.starts_with("Hello, steve!"));
        assert!(WizardLang::Id.repl_greeting("steve").0.starts_with("Halo, steve!"));
        assert!(WizardLang::Ja.repl_greeting("steve").0.starts_with("こんにちは、steve！"));

        assert!(WizardLang::En
            .setup_complete_message("steve", "~/.config/waish")
            .contains("Setup complete! Nice to meet you, steve."));
        assert!(WizardLang::Id
            .setup_complete_message("steve", "~/.config/waish")
            .contains("Setup selesai! Senang bertemu denganmu, steve."));
        assert!(WizardLang::Ja
            .setup_complete_message("steve", "~/.config/waish")
            .contains("セットアップ完了！よろしくね、steve"));

        assert_eq!(WizardLang::En.action_box_title(), "Action Details");
        assert_eq!(
            WizardLang::En.shell_action_lines("sudo pacman -Syu", "Update", true),
            vec!["Command : sudo pacman -Syu", "Intent  : Update", "Root    : yes"]
        );
        assert_eq!(
            WizardLang::En.root_confirm_prompt(
                "sudo pacman -Syu",
                WizardLang::En.inline_correction_hint_tag()
            ),
            "Execute command 'sudo pacman -Syu' now? [y/N/type your correction]: "
        );
        assert_eq!(
            WizardLang::En.action_confirm_prompt("sudo pacman -Syu"),
            "( > < ) ! Execute command 'sudo pacman -Syu' now? [y/N/type your correction]: "
        );
        assert_eq!(
            WizardLang::En.root_cancelled(),
            "Root command execution cancelled."
        );
        assert_eq!(
            WizardLang::En.runner_executing("run_shell_command"),
            "Executing run_shell_command..."
        );

        assert_eq!(WizardLang::Id.action_box_title(), "Detail Aksi");
        assert_eq!(
            WizardLang::Id.shell_action_lines("sudo pacman -Syu", "Update", true),
            vec!["Command  : sudo pacman -Syu", "Tujuan   : Update", "Root     : ya"]
        );
        assert_eq!(
            WizardLang::Id.root_confirm_prompt(
                "sudo pacman -Syu",
                WizardLang::Id.inline_correction_hint_tag()
            ),
            "Jalankan command 'sudo pacman -Syu' sekarang? [y/N/ketik koreksimu]: "
        );
        assert_eq!(
            WizardLang::Id.action_confirm_prompt("sudo pacman -Syu"),
            "( > < ) ! Jalankan command 'sudo pacman -Syu' sekarang? [y/N/ketik koreksimu]: "
        );
        assert_eq!(
            WizardLang::Id.root_cancelled(),
            "Eksekusi command root dibatalkan yaa."
        );
        assert_eq!(
            WizardLang::Id.runner_executing("run_shell_command"),
            "Sedang menjalankan run_shell_command..."
        );

        assert_eq!(WizardLang::Ja.action_box_title(), "アクション詳細");
        assert_eq!(
            WizardLang::Ja.shell_action_lines("sudo pacman -Syu", "Update", true),
            vec!["コマンド : sudo pacman -Syu", "目的     : Update", "ルート   : はい"]
        );
        assert_eq!(
            WizardLang::Ja.root_confirm_prompt(
                "sudo pacman -Syu",
                WizardLang::Ja.inline_correction_hint_tag()
            ),
            "コマンド 'sudo pacman -Syu' を実行しますか？ [y/N/修正内容を入力]: "
        );
        assert_eq!(
            WizardLang::Ja.action_confirm_prompt("sudo pacman -Syu"),
            "( > < ) ! コマンド 'sudo pacman -Syu' を実行しますか？ [y/N/修正内容を入力]: "
        );
        assert_eq!(
            WizardLang::Ja.root_cancelled(),
            "ルートコマンドの実行がキャンセルされました。"
        );
        assert_eq!(
            WizardLang::Ja.runner_executing("run_shell_command"),
            "run_shell_command を実行中..."
        );
    }
}
