//! Dynamic Memory Consolidation Worker.

use std::time::Duration;

use super::client::{sanitize_terminal_text, LlmClient};
use crate::cli::prompt::print_bot_inline;
use crate::cli::repl::{ConversationTurn, ReplSession};
use crate::cli::ui::Kaomoji;
use crate::config::memory;
use crate::config::settings::Settings;
use crate::config::Paths;

pub const CONSOLIDATION_TIMEOUT_SECS: u64 = 8;

const EXTRACTOR_SYSTEM_PROMPT: &str = "\
Kamu adalah modul Memory Consolidation Worker untuk shell Linux `waish`.
Tugasmu adalah memperbarui dokumen Long-Term Memory Bank (`memory.md`) berdasarkan isi memori lama dan transkrip sesi percakapan yang baru saja selesai.

ATURAN EKSTRAKSI (WAJIB DIPATUHI):
1. Pertahankan informasi penting dari `memory.md` lama kecuali pengguna secara eksplisit mengubahnya.
2. Identifikasi preferensi baru, paket/alat yang baru di-install atau dikonfigurasi, catatan kebiasaan sistem, atau masalah teknis yang baru diselesaikan pada sesi ini.
3. DILARANG KERAS menggunakan emoji grafis berwarna.
4. DILARANG menambahkan kalimat pengantar, basa-basi, penjelasan, atau pembungkus blok kode (jangan gunakan ```markdown).
5. HANYA keluarkan isi dokumen Markdown mentah dengan struktur tag persis seperti berikut:

# Long-Term Memory Bank

[profil]
- user_name: ...
- os: ...
- window_manager: ...
- shell: ...

[preferensi]
- ...

[catatan_sistem]
- ...

[riwayat_terakhir]
- ...";

/// Outcome of the post-session memory consolidation pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsolidationOutcome {
    SkippedEmptySession,
    Updated,
    PreservedOldMemory { reason: String },
}

pub async fn farewell_and_consolidate(session: &ReplSession) -> ConsolidationOutcome {
    let user_name = session.user_display_name();
    let lang = crate::wizard::i18n::WizardLang::from_code(&session.settings.general.language);

    if session.conversation.is_empty() {
        print_bot_inline(
            &session.settings,
            Kaomoji::Farewell,
            &lang.repl_farewell_empty(user_name),
        );
        return ConsolidationOutcome::SkippedEmptySession;
    }

    print_bot_inline(
        &session.settings,
        Kaomoji::Farewell,
        &lang.repl_farewell_consolidating(user_name),
    );

    consolidate_session_memory(
        &session.client,
        &session.settings,
        &session.paths,
        &session.conversation,
        Duration::from_secs(CONSOLIDATION_TIMEOUT_SECS),
    )
    .await
}

pub async fn consolidate_session_memory(
    client: &LlmClient,
    settings: &Settings,
    paths: &Paths,
    conversation: &[ConversationTurn],
    timeout: Duration,
) -> ConsolidationOutcome {
    if conversation.is_empty() {
        return ConsolidationOutcome::SkippedEmptySession;
    }

    let existing_memory = memory::load(&paths.memory).unwrap_or_default();
    let user_payload = build_extraction_payload(&existing_memory, conversation);

    let raw_response = match client
        .complete_oneshot(settings, EXTRACTOR_SYSTEM_PROMPT, &user_payload, timeout)
        .await
    {
        Ok(text) => text,
        Err(err) => {
            return ConsolidationOutcome::PreservedOldMemory {
                reason: err.to_string(),
            };
        }
    };

    let Some(clean_markdown) = normalize_extracted_markdown(&raw_response) else {
        return ConsolidationOutcome::PreservedOldMemory {
            reason: "output model tidak sesuai struktur Markdown memory bank".to_string(),
        };
    };

    match memory::save(&paths.memory, &clean_markdown) {
        Ok(()) => ConsolidationOutcome::Updated,
        Err(err) => ConsolidationOutcome::PreservedOldMemory {
            reason: err.to_string(),
        },
    }
}

pub fn build_extraction_payload(
    existing_memory: &str,
    conversation: &[ConversationTurn],
) -> String {
    let today = chrono::Local::now().format("%Y-%m-%d %H:%M");
    let mut transcript = String::new();
    for (idx, turn) in conversation.iter().enumerate() {
        transcript.push_str(&format!(
            "Turn {}:\nUser: {}\nAssistant: {}\n\n",
            idx + 1,
            turn.user.trim(),
            turn.assistant.trim()
        ));
    }

    let current_mem = match existing_memory.trim() {
        "" => "(belum ada isi memory.md)",
        trimmed => trimmed,
    };

    format!(
        "WAKTU SESI SELESAI: {today}\n\n\
         === ISI MEMORY.MD SAAT INI ===\n\
         {current_mem}\n\n\
         === TRANSKRIP SESI PERCAKAPAN BARU ===\n\
         {transcript}\
         Perbarui dokumen memory.md di atas dengan menggabungkan fakta/preferensi/aksi penting dari transkrip sesi baru. \
         Keluarkan HANYA Markdown mentah."
    )
}

pub fn normalize_extracted_markdown(raw: &str) -> Option<String> {
    let sanitized = sanitize_terminal_text(raw);
    let stripped = strip_code_fences(sanitized.trim());
    let trimmed = stripped.trim();

    if trimmed.len() < 30 {
        return None;
    }

    let section_count = trimmed
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('[') && line.ends_with(']') && line.len() >= 4)
        .count();

    if section_count < 2 {
        return None;
    }

    let mut normalized = if trimmed.starts_with("# ") {
        trimmed.to_string()
    } else {
        format!("# Long-Term Memory Bank\n\n{trimmed}")
    };

    normalized.push('\n');
    Some(normalized)
}

fn strip_code_fences(text: &str) -> &str {
    let Some(after_open) = text.strip_prefix("```") else {
        return text;
    };
    let Some(first_newline) = after_open.find('\n') else {
        return text;
    };
    let body = &after_open[first_newline + 1..];
    body.strip_suffix("```").unwrap_or(body).trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn normalizes_and_validates_markdown_output() {
        let fenced = "```markdown\n# Long-Term Memory Bank\n\n[profil]\n- user_name: steve\n\n[preferensi]\n- editor: neovim\n\n[catatan_sistem]\n- kernel: 6.12\n\n[riwayat_terakhir]\n- Pasang neovim\n```";
        let clean = normalize_extracted_markdown(fenced).expect("should validate fenced markdown");
        assert!(clean.starts_with("# Long-Term Memory Bank\n"));
        assert!(clean.contains("[profil]"));
        assert!(clean.contains("[preferensi]"));
        assert!(clean.contains("[catatan_sistem]"));
        assert!(clean.contains("[riwayat_terakhir]"));
        assert!(!clean.contains("```"));

        // Output ngawur / basa-basi tanpa tag ditolak agar tidak merusak memory.md.
        assert_eq!(
            normalize_extracted_markdown("Maaf saya tidak bisa memproses permintaan ini."),
            None
        );
    }

    #[tokio::test]
    async fn consolidates_and_writes_memory_atomically_from_mock_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut req_buf = [0u8; 4096];
                let n = socket.read(&mut req_buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&req_buf[..n]);
                assert!(req_str.contains("install ripgrep"));

                let json_body = serde_json::json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "# Long-Term Memory Bank\n\n[profil]\n- user_name: steve\n\n[preferensi]\n- search_tool: ripgrep\n\n[catatan_sistem]\n- aur_helper: yay\n\n[riwayat_terakhir]\n- Memasang paket ripgrep di sesi terakhir."
                        }
                    }]
                })
                .to_string();

                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    json_body.len(),
                    json_body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            }
        });

        let tmp_dir = std::env::temp_dir().join(format!("waish-mem-ext-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();
        let paths = Paths::from_dir(tmp_dir.clone());
        fs::write(&paths.memory, "# Long-Term Memory Bank\n\n[profil]\n- user_name: steve\n").unwrap();

        let mut settings = Settings::default();
        settings.api.provider = "openai".to_string();
        settings.api.api_key = "sk-test-1234567890".to_string();
        settings.api.base_url = format!("http://{addr}/v1");

        let client = LlmClient::new(&settings).unwrap();
        let turns = vec![ConversationTurn {
            user: "tolong install ripgrep".to_string(),
            assistant: "Beres, ripgrep sudah terpasang! ( > - < ) v".to_string(),
        }];

        let outcome = consolidate_session_memory(
            &client,
            &settings,
            &paths,
            &turns,
            Duration::from_secs(5),
        )
        .await;

        assert_eq!(outcome, ConsolidationOutcome::Updated);
        let updated = fs::read_to_string(&paths.memory).unwrap();
        assert!(updated.contains("search_tool: ripgrep"));
        assert!(updated.contains("[riwayat_terakhir]"));

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn timeout_or_failure_preserves_existing_memory_file() {
        use tokio::net::TcpListener;

        // Server menerima koneksi tapi sengaja diam melewati batas timeout.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((_socket, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        let tmp_dir: PathBuf =
            std::env::temp_dir().join(format!("waish-mem-timeout-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();
        let paths = Paths::from_dir(tmp_dir.clone());
        let original = "# Long-Term Memory Bank\n\n[profil]\n- user_name: steve\n- catatan: jangan sampai hilang\n";
        fs::write(&paths.memory, original).unwrap();

        let mut settings = Settings::default();
        settings.api.provider = "openai".to_string();
        settings.api.api_key = "sk-test-1234567890".to_string();
        settings.api.base_url = format!("http://{addr}/v1");

        let client = LlmClient::new(&settings).unwrap();
        let turns = vec![ConversationTurn {
            user: "halo".to_string(),
            assistant: "halo juga!".to_string(),
        }];

        let outcome = consolidate_session_memory(
            &client,
            &settings,
            &paths,
            &turns,
            Duration::from_millis(200),
        )
        .await;

        assert!(matches!(
            outcome,
            ConsolidationOutcome::PreservedOldMemory { .. }
        ));
        let after = fs::read_to_string(&paths.memory).unwrap();
        assert_eq!(after, original, "existing memory.md must remain completely intact");

        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
