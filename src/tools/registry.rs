//! Tool calling JSON schemas for OpenAI Chat Completions and Anthropic Messages APIs.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const RUN_SHELL_COMMAND_NAME: &str = "run_shell_command";
pub const EXECUTE_RECIPE_NAME: &str = "execute_recipe";
pub const READ_FILE_NAME: &str = "read_file";
pub const WRITE_FILE_NAME: &str = "write_file";
pub const REPLACE_IN_FILE_NAME: &str = "replace_in_file";
pub const ROLLBACK_CONFIG_NAME: &str = "rollback_config";
pub const MANAGE_WORKSPACE_NAME: &str = "manage_workspace";
pub const MANAGE_WINDOW_NAME: &str = "manage_window";
pub const OPEN_MEDIA_NAME: &str = "open_media";
pub const CONTROL_MEDIA_NAME: &str = "control_media";
pub const OPEN_BROWSER_SEARCH_NAME: &str = "open_browser_search";
pub const QUICK_SEARCH_NAME: &str = "quick_search";
pub const READ_CLIPBOARD_NAME: &str = "read_clipboard";
pub const FIND_PACKAGE_OWNER_NAME: &str = "find_package_owner";
pub const MANAGE_SCRATCHPAD_NAME: &str = "manage_scratchpad";
pub const UPDATE_BATTERY_PROMISE_NAME: &str = "update_battery_promise";
pub const MANAGE_ALARM_NAME: &str = "manage_alarm";
pub const MANAGE_SPREADSHEET_NAME: &str = "manage_spreadsheet";
pub const MANAGE_DOCUMENT_NAME: &str = "manage_document";
pub const LAUNCH_GUI_APP_NAME: &str = "launch_gui_app";

pub fn is_known_tool_name(name: &str) -> bool {
    matches!(
        name,
        RUN_SHELL_COMMAND_NAME
            | EXECUTE_RECIPE_NAME
            | READ_FILE_NAME
            | WRITE_FILE_NAME
            | REPLACE_IN_FILE_NAME
            | ROLLBACK_CONFIG_NAME
            | MANAGE_WORKSPACE_NAME
            | LAUNCH_GUI_APP_NAME
            | MANAGE_WINDOW_NAME
            | OPEN_MEDIA_NAME
            | CONTROL_MEDIA_NAME
            | OPEN_BROWSER_SEARCH_NAME
            | QUICK_SEARCH_NAME
            | READ_CLIPBOARD_NAME
            | FIND_PACKAGE_OWNER_NAME
            | MANAGE_SCRATCHPAD_NAME
            | UPDATE_BATTERY_PROMISE_NAME
            | MANAGE_ALARM_NAME
            | MANAGE_SPREADSHEET_NAME
            | MANAGE_DOCUMENT_NAME
    )
}

/// Arguments for `run_shell_command`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellCommandArgs {
    pub command: String,
    #[serde(default = "default_intent")]
    pub intent: String,
    #[serde(default)]
    pub requires_root: bool,
}

/// Arguments for `manage_scratchpad`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManageScratchpadArgs {
    pub action: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub id: Option<u64>,
}

/// Arguments for `update_battery_promise`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateBatteryPromiseArgs {
    pub action: String,
}

fn default_intent() -> String {
    "Menjalankan perintah sistem".to_string()
}

impl ShellCommandArgs {
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        let mut parsed: Self = serde_json::from_str(raw)
            .map_err(|e| format!("argumen JSON tool tidak valid: {e}"))?;

        parsed.command = parsed.command.trim().to_string();
        if parsed.command.is_empty() {
            return Err("field `command` kosong".to_string());
        }

        parsed.intent = parsed.intent.trim().to_string();
        if parsed.intent.is_empty() {
            parsed.intent = default_intent();
        }

        if !parsed.requires_root && command_invokes_sudo(&parsed.command) {
            parsed.requires_root = true;
        }

        Ok(parsed)
    }
}

pub fn command_invokes_sudo(command: &str) -> bool {
    command.split(['|', ';', '&']).any(|segment| {
        let mut tokens = segment.split_whitespace();
        let Some(first) = tokens.next() else {
            return false;
        };
        if matches!(first, "sudo" | "doas") {
            return true;
        }
        if matches!(first, "yay" | "paru" | "pacman") {
            return tokens.any(|flag| {
                if !flag.starts_with('-') || flag.starts_with("--") {
                    return false;
                }
                let f = &flag[1..];
                if f.starts_with('Q') || f == "Si" || f == "Ss" || f == "F" {
                    return false;
                }
                f.contains('S') || f.contains('R') || f.contains('U') || (f.contains('F') && f.contains('y'))
            });
        }
        false
    })
}

struct ToolSpec {
    name: &'static str,
    description: &'static str,
    parameters: Value,
}

fn all_tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: RUN_SHELL_COMMAND_NAME,
            description: "Eksekusi perintah CLI sistem di PTY POSIX (sudo interaktif & auto binary discovery). DILARANG untuk: (1) script Python /tmp untuk .xlsx/.docx (pakai execute_recipe / manage_document), (2) `nohup <gui> &` (pakai execute_recipe / launch_gui_app), (3) `sleep` + `hyprctl clients`/`pgrep` setelah tool IPC mengembalikan verifikasi.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Perintah CLI POSIX murni (bukan `nohup <gui> &` atau `/tmp/*.py`)."
                    },
                    "intent": {
                        "type": "string",
                        "description": "Ringkasan singkat tujuan aksi."
                    },
                    "requires_root": {
                        "type": "boolean",
                        "description": "True bila membutuhkan sudo/root."
                    }
                },
                "required": ["command", "intent", "requires_root"]
            }),
        },
        ToolSpec {
            name: EXECUTE_RECIPE_NAME,
            description: "PRIORITAS UTAMA (1-Turn Atomic Recipe): Menuntaskan alur dokumen + workspace + media sekaligus tanpa multi-turn. Resep: 'create_and_open_document' (buat .xlsx/.csv/.docx di $HOME + buka editor di workspace via Window Trapper 8s), 'open_file_in_workspace' (buka file disk langsung ke workspace), 'hot_edit_document' (tutup jendela editor aktif -> update file -> buka ulang di workspace sama), 'play_media_direct', atau 'trigger_scene' ('focus'/'study'/'clean_slate'/'entertainment'). Hasil sudah final; DILARANG cek ulang via sleep/hyprctl/pgrep!",
            parameters: json!({
                "type": "object",
                "properties": {
                    "recipe": {
                        "type": "string",
                        "enum": ["create_and_open_document", "open_file_in_workspace", "hot_edit_document", "play_media_direct", "trigger_scene"],
                        "description": "Nama resep komposit 1-langkah."
                    },
                    "path": {
                        "type": "string",
                        "description": "Path file target (~/jadwal.xlsx, laporan.docx, materi.pdf)."
                    },
                    "title": {
                        "type": "string",
                        "description": "Judul dokumen opsional (.docx/.md)."
                    },
                    "content": {
                        "type": "string",
                        "description": "Isi teks/markdown dokumen (# Heading, - Bullet, paragraf)."
                    },
                    "sheet_name": {
                        "type": "string",
                        "description": "Nama sheet opsional untuk .xlsx."
                    },
                    "headers": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Header kolom tabel (.xlsx/.csv/.docx)."
                    },
                    "rows": {
                        "type": "array",
                        "items": { "type": "array", "items": {} },
                        "description": "Matriks baris 2D (angka, teks, atau formula '=SUM(B2:B10)')."
                    },
                    "sequence": {
                        "type": "string",
                        "description": "Generator urutan baris opsional (contoh: '1..100')."
                    },
                    "append": {
                        "type": "boolean",
                        "description": "True pada hot_edit_document bila ingin menambah isi tanpa menimpa."
                    },
                    "editor": {
                        "type": "string",
                        "description": "Editor GUI opsional (contoh: 'onlyoffice-desktopeditors' atau 'libreoffice')."
                    },
                    "workspace": {
                        "type": "integer",
                        "description": "Workspace tujuan positif (kosongkan untuk empty workspace otomatis)."
                    },
                    "query": {
                        "type": "string",
                        "description": "Judul lagu/video untuk play_media_direct atau trigger_scene."
                    },
                    "platform": {
                        "type": "string",
                        "enum": ["spotify", "youtube"],
                        "description": "Platform media opsional."
                    },
                    "scene": {
                        "type": "string",
                        "enum": ["focus", "study", "clean_slate", "entertainment"],
                        "description": "Nama scene untuk trigger_scene."
                    },
                    "alarm_time": {
                        "type": "string",
                        "description": "Waktu alarm/Pomodoro opsional pada trigger_scene ('25m' atau '21:00')."
                    }
                },
                "required": ["recipe"]
            }),
        },
        ToolSpec {
            name: MANAGE_DOCUMENT_NAME,
            description: "Engine dokumen universal native Rust (.xlsx via rust_xlsxwriter/calamine, .csv, .docx via docx-rs, ekstraksi baca .pdf via pdf-extract, .md/.txt) + 'hot_edit'. Gunakan 'execute_recipe(create_and_open_document)' bila file juga perlu langsung dibuka di GUI.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["create", "write", "append", "append_rows", "read", "hot_edit"],
                        "description": "Operasi dokumen ('read' mendukung .xlsx/.csv/.docx/.pdf/.md/.txt; 'hot_edit' me-reload editor aktif)."
                    },
                    "path": {
                        "type": "string",
                        "description": "Path file (.xlsx, .csv, .docx, .pdf, .md, .txt)."
                    },
                    "title": {
                        "type": "string",
                        "description": "Judul dokumen opsional (.docx/.md)."
                    },
                    "content": {
                        "type": "string",
                        "description": "Isi dokumen (# Heading, ## Subheading, - Bullet, paragraf)."
                    },
                    "paragraphs": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Daftar paragraf opsional."
                    },
                    "sheet_name": {
                        "type": "string",
                        "description": "Nama sheet opsional untuk .xlsx."
                    },
                    "headers": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Daftar kolom header tabel."
                    },
                    "rows": {
                        "type": "array",
                        "items": { "type": "array", "items": {} },
                        "description": "Baris tabel 2D (mendukung angka & formula '=SUM(...)')."
                    },
                    "sequence": {
                        "type": "string",
                        "description": "Generator baris berurutan opsional ('1..100')."
                    }
                },
                "required": ["action", "path"]
            }),
        },
        ToolSpec {
            name: MANAGE_SPREADSHEET_NAME,
            description: "Alias khusus spreadsheet native (.xlsx & .csv via rust_xlsxwriter/calamine): 'create'/'write', 'append_rows', atau 'read'.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["create", "write", "append_rows", "read"],
                        "description": "Operasi spreadsheet."
                    },
                    "path": {
                        "type": "string",
                        "description": "Path file (.xlsx atau .csv)."
                    },
                    "sheet_name": {
                        "type": "string",
                        "description": "Nama sheet opsional."
                    },
                    "headers": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Header kolom."
                    },
                    "rows": {
                        "type": "array",
                        "items": { "type": "array", "items": {} },
                        "description": "Matriks baris sel 2D."
                    }
                },
                "required": ["action", "path"]
            }),
        },
        ToolSpec {
            name: LAUNCH_GUI_APP_NAME,
            description: "Meluncurkan aplikasi GUI desktop langsung via IPC kompositor + Window Trapper 8s dan mengembalikan WindowInfo final. DILARANG menjalankan sleep/hyprctl clients/pgrep tambahan setelah tool ini selesai!",
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Perintah aplikasi GUI (contoh: 'onlyoffice-desktopeditors ~/tabel.xlsx' atau '/usr/bin/puzzles-mines')."
                    },
                    "workspace": {
                        "type": "integer",
                        "description": "Workspace positif tujuan (kosongkan untuk companion workspace otomatis)."
                    }
                },
                "required": ["command"]
            }),
        },
        ToolSpec {
            name: MANAGE_ALARM_NAME,
            description: "Kelola alarm & timer transient systemd user (`waish-alarm-*`): 'set' (jam 'HH:MM' atau durasi '25m'), 'cancel', atau 'list'. DILARANG mencari file unit via `find /`!",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["set", "cancel", "list"],
                        "description": "Operasi alarm systemd user."
                    },
                    "time": {
                        "type": "string",
                        "description": "Jam 24h 'HH:MM' ('05:30') atau durasi relatif ('25m', '1h30m') untuk 'set'."
                    },
                    "label": {
                        "type": "string",
                        "description": "Label opsional nama alarm."
                    }
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: MANAGE_WORKSPACE_NAME,
            description: "Kelola workspace via WindowBackend IPC: 'launch_app', 'switch', 'move_window', 'set_preset', 'list_clients', atau 'close_batch' (dengan suicide protection).",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["launch_app", "switch", "move_window", "set_preset", "list_clients", "close_batch"],
                        "description": "Aksi workspace."
                    },
                    "target": {
                        "type": "string",
                        "description": "Nomor workspace tujuan atau nama preset ('coding'/'floating')."
                    },
                    "command": {
                        "type": "string",
                        "description": "Binary GUI untuk 'launch_app'."
                    },
                    "workspaces": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "Daftar workspace untuk 'close_batch' (contoh: [2, 3, 5])."
                    }
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: MANAGE_WINDOW_NAME,
            description: "Kelola jendela per-workspace via IPC ('close', 'focus', 'toggle_float', 'move_to_workspace').",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["close", "focus", "toggle_float", "move_to_workspace"],
                        "description": "Aksi jendela."
                    },
                    "workspace": {
                        "type": "integer",
                        "description": "Filter nomor workspace positif."
                    },
                    "window_class": {
                        "type": "string",
                        "description": "Filter class/judul jendela."
                    },
                    "target": {
                        "type": "string",
                        "description": "Alamat hex ('0x...'), class, atau regex judul."
                    },
                    "destination_ws": {
                        "type": "integer",
                        "description": "Workspace tujuan untuk 'move_to_workspace'."
                    }
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: OPEN_MEDIA_NAME,
            description: "Putar lagu/video spesifik di Spotify (native zbus OpenUri) atau YouTube (browser --new-window autoplay) pada companion/target workspace.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Judul lagu, artis, atau kueri video."
                    },
                    "platform": {
                        "type": "string",
                        "enum": ["spotify", "youtube"],
                        "description": "Platform ('spotify' default atau 'youtube')."
                    },
                    "workspace": {
                        "type": "integer",
                        "description": "Workspace tujuan opsional."
                    }
                },
                "required": ["query"]
            }),
        },
        ToolSpec {
            name: CONTROL_MEDIA_NAME,
            description: "Kontrol MPRIS native D-Bus ('play_pause', 'play', 'pause', 'next', 'previous', 'get_info'). Untuk cari lagu spesifik gunakan open_media, bukan spam next.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["play_pause", "play", "pause", "next", "previous", "get_info"],
                        "description": "Aksi kontrol media."
                    },
                    "player": {
                        "type": "string",
                        "description": "Nama player opsional ('spotify')."
                    }
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: READ_FILE_NAME,
            description: "Baca file teks/konfigurasi lokal secara langsung (mendukung ~/). Untuk .xlsx/.docx/.pdf gunakan manage_document(action='read').",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path file target." },
                    "max_lines": { "type": "integer", "description": "Batas baris (default: 250)." }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: WRITE_FILE_NAME,
            description: "Tulis/timpa file teks atau config secara atomik + Shadow-Git backup. DILARANG menulis ulang `memory.md` untuk obrolan santai (pakai manage_scratchpad).",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path file target." },
                    "content": { "type": "string", "description": "Isi file lengkap." },
                    "requires_root": { "type": "boolean", "description": "True bila milik root (/etc/...)." }
                },
                "required": ["path", "content", "requires_root"]
            }),
        },
        ToolSpec {
            name: REPLACE_IN_FILE_NAME,
            description: "Ganti blok teks presisi di file teks/config + Shadow-Git backup & auto-reload Waybar. Jangan ubah `memory.md` untuk catatan sementara.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path file target." },
                    "target": { "type": "string", "description": "Teks lama persis." },
                    "replacement": { "type": "string", "description": "Teks pengganti." },
                    "requires_root": { "type": "boolean", "description": "True bila butuh root." }
                },
                "required": ["path", "target", "replacement", "requires_root"]
            }),
        },
        ToolSpec {
            name: ROLLBACK_CONFIG_NAME,
            description: "Pulihkan file config ke snapshot sebelumnya dari Shadow-Git (~/.local/share/waish/shadow_repo/).",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path file config." },
                    "steps_back": { "type": "integer", "description": "Langkah revisi mundur (default: 1)." },
                    "requires_root": { "type": "boolean", "description": "True bila butuh root." }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: OPEN_BROWSER_SEARCH_NAME,
            description: "Buka pencarian web visual di browser jendela baru (--new-window) pada workspace terarah.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Kata kunci pencarian." },
                    "engine": {
                        "type": "string",
                        "enum": ["google", "duckduckgo", "brave", "youtube"],
                        "description": "Mesin pencari."
                    },
                    "workspace": { "type": "integer", "description": "Workspace tujuan opsional." }
                },
                "required": ["query"]
            }),
        },
        ToolSpec {
            name: QUICK_SEARCH_NAME,
            description: "Pencarian web headless cepat (3 ringkasan teratas) khusus info terkini real-time.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Kueri ringkas." },
                    "max_results": { "type": "integer", "description": "Batas 1-5 (default: 3)." }
                },
                "required": ["query"]
            }),
        },
        ToolSpec {
            name: READ_CLIPBOARD_NAME,
            description: "Baca isi teks clipboard aktif (`wl-paste` / `xclip`).",
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolSpec {
            name: FIND_PACKAGE_OWNER_NAME,
            description: "Cari paket Arch penyedia command atau file `.so` via `pacman -F`.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Nama binary atau library." }
                },
                "required": ["query"]
            }),
        },
        ToolSpec {
            name: MANAGE_SCRATCHPAD_NAME,
            description: "Kelola catatan sesi sementara di scratchpad.json ('add', 'list', 'remove', 'clear').",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["add", "list", "remove", "clear"],
                        "description": "Operasi scratchpad."
                    },
                    "tag": { "type": "string", "description": "Kategori singkat." },
                    "content": { "type": "string", "description": "Isi catatan." },
                    "id": { "type": "integer", "description": "ID catatan untuk 'remove'." }
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: UPDATE_BATTERY_PROMISE_NAME,
            description: "Update state pengingat baterai ('postpone', 'promise', 'clear').",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["postpone", "promise", "clear"],
                        "description": "Status pengingat baterai."
                    }
                },
                "required": ["action"]
            }),
        },
    ]
}

pub fn openai_tools_schema() -> Value {
    let list: Vec<Value> = all_tool_specs()
        .into_iter()
        .map(|spec| {
            json!({
                "type": "function",
                "function": {
                    "name": spec.name,
                    "description": spec.description,
                    "parameters": spec.parameters
                }
            })
        })
        .collect();
    Value::Array(list)
}

pub fn anthropic_tools_schema() -> Value {
    let list: Vec<Value> = all_tool_specs()
        .into_iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "description": spec.description,
                "input_schema": spec.parameters
            })
        })
        .collect();
    Value::Array(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_shell_command_args_and_infers_sudo() {
        let raw = r#"{"command":"sudo pacman -S --noconfirm neovim","intent":"Pasang neovim","requires_root":false}"#;
        let args = ShellCommandArgs::from_json_str(raw).unwrap();
        assert_eq!(args.command, "sudo pacman -S --noconfirm neovim");
        assert_eq!(args.intent, "Pasang neovim");
        assert!(args.requires_root, "sudo prefix should infer requires_root = true");
    }

    #[test]
    fn rejects_empty_command() {
        let raw = r#"{"command":"   ","intent":"Tes","requires_root":false}"#;
        assert!(ShellCommandArgs::from_json_str(raw).is_err());
    }

    #[test]
    fn schemas_contain_all_tools_including_execute_recipe() {
        let oa = openai_tools_schema();
        let oa_arr = oa.as_array().unwrap();
        assert_eq!(oa_arr.len(), 20);
        assert_eq!(oa_arr[0]["function"]["name"], RUN_SHELL_COMMAND_NAME);
        assert_eq!(oa_arr[1]["function"]["name"], EXECUTE_RECIPE_NAME);

        let ant = anthropic_tools_schema();
        let ant_arr = ant.as_array().unwrap();
        assert_eq!(ant_arr.len(), 20);
        assert_eq!(ant_arr[0]["name"], RUN_SHELL_COMMAND_NAME);
        assert!(ant_arr.iter().any(|t| t["name"] == EXECUTE_RECIPE_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == LAUNCH_GUI_APP_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == READ_FILE_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == WRITE_FILE_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == REPLACE_IN_FILE_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == ROLLBACK_CONFIG_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == MANAGE_WORKSPACE_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == MANAGE_WINDOW_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == OPEN_MEDIA_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == CONTROL_MEDIA_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == OPEN_BROWSER_SEARCH_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == QUICK_SEARCH_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == READ_CLIPBOARD_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == FIND_PACKAGE_OWNER_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == MANAGE_SCRATCHPAD_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == UPDATE_BATTERY_PROMISE_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == MANAGE_ALARM_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == MANAGE_SPREADSHEET_NAME));
        assert!(ant_arr.iter().any(|t| t["name"] == MANAGE_DOCUMENT_NAME));
    }
}
