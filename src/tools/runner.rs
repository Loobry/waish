//! Asynchronous tool execution dispatcher, binary defensive interceptors, process group armor, and text tool-call parser.

use std::env;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use regex::RegexBuilder;

use crate::cli::ui::{self, Kaomoji};
use crate::config::settings::{Settings, DEFAULT_USER_REAL_NAME};
use crate::config::Paths;
use crate::engine::client::ToolCall;
use crate::engine::kaomoji::{EmotionSpectrum, KaomojiEngine};
use crate::engine::scratchpad::Scratchpad;
use crate::engine::shadow_git::ShadowGit;
use crate::engine::watcher::HardwareWatcherState;
use crate::executor::safety::{self, ConfirmationDecision, SafetyVerdict};
use crate::executor::{notifier, pty};
use crate::wizard::i18n::WizardLang;

use super::desktop::{
    self, LaunchGuiAppArgs, ManageWindowArgs, ManageWorkspaceArgs, WaybarState,
};
use super::document::{self, ManageDocumentArgs, ManageSpreadsheetArgs};
use super::file_ops::{
    self, ReadFileArgs, ReplaceInFileArgs, RollbackConfigArgs, WriteFileArgs,
};
use super::media::{self, ControlMediaArgs, OpenMediaArgs, PendingAction};
use super::recipes::{self, ExecuteRecipeArgs, RecipeExecutionContext};
use super::registry::{
    is_known_tool_name, ManageScratchpadArgs, ShellCommandArgs, UpdateBatteryPromiseArgs,
    CONTROL_MEDIA_NAME, EXECUTE_RECIPE_NAME, FIND_PACKAGE_OWNER_NAME, LAUNCH_GUI_APP_NAME,
    MANAGE_ALARM_NAME, MANAGE_DOCUMENT_NAME, MANAGE_SCRATCHPAD_NAME, MANAGE_SPREADSHEET_NAME,
    MANAGE_WINDOW_NAME, MANAGE_WORKSPACE_NAME, OPEN_BROWSER_SEARCH_NAME, OPEN_MEDIA_NAME,
    QUICK_SEARCH_NAME, READ_CLIPBOARD_NAME, READ_FILE_NAME, REPLACE_IN_FILE_NAME,
    ROLLBACK_CONFIG_NAME, RUN_SHELL_COMMAND_NAME, UPDATE_BATTERY_PROMISE_NAME, WRITE_FILE_NAME,
};
use super::system_ops::{self, FindPackageOwnerArgs};
use super::timer::{self, ManageAlarmArgs};
use super::web_search::{self, OpenBrowserSearchArgs, QuickSearchArgs};
use super::workspace_tracker::SessionWorkspaceState;

pub const AD_HOC_OFFICE_SCRIPT_NOTICE: &str = "[System Notice]: Dilarang membuat script Python sementara di /tmp untuk spreadsheet/dokumen. Gunakan tool native 'execute_recipe' ('create_and_open_document' / 'hot_edit_document'), 'manage_document', atau 'manage_spreadsheet' yang sudah tersedia di schema!";

pub const BACKGROUND_GUI_SHELL_NOTICE: &str = "[System Notice]: Dilarang meluncurkan aplikasi GUI/editor dokumen menggunakan `nohup ... &` atau `&` di `run_shell_command` karena proses GUI akan mati oleh SIGHUP saat PTY ditutup dan meleset dari workspace! Gunakan tool native 'execute_recipe' ('open_file_in_workspace' / 'create_and_open_document') atau 'launch_gui_app' secara langsung!";

pub const POST_IPC_VERIFICATION_NOTICE: &str = "status: already_verified_by_ipc\ndo_not_poll_clients: true\nnote: Verifikasi WindowInfo sudah dikembalikan secara definitif oleh Window Trapper IPC Rust. Dilarang menjalankan sleep/hyprctl clients/pgrep tambahan via shell; langsung sampaikan hasilnya ke pengguna!";

pub const ALARM_FS_RUMMAGE_NOTICE: &str = "[System Notice]: Dilarang menjalankan `find /` atau menggeledah `~/.config/` untuk mencari unit alarm! Gunakan tool native `manage_alarm(action='cancel' | 'list')` secara langsung.";

pub const LEGACY_X11_GUI_BINS: &[&str] = &[
    "oneko",
    "xeyes",
    "xclock",
    "xcalc",
    "xlogo",
    "xmag",
    "xev",
    "glxgears",
    "feh",
    "sxiv",
    "nsxiv",
];

/// Inline user course-correction entered at the Safety Gatekeeper confirmation prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserCourseCorrection {
    pub rejected_command: String,
    pub feedback: String,
}

/// Outcome of executing a single `ToolCall`.
#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub output_for_llm: String,
    pub succeeded: bool,
    pub cancelled_cleanly: bool,
    pub course_correction: Option<UserCourseCorrection>,
}

/// Runtime dependencies passed to the tool execution dispatcher.
pub struct ToolExecutionContext<'a> {
    pub settings: &'a Settings,
    pub paths: &'a Paths,
    pub shadow_git: &'a ShadowGit,
    pub watcher_state: Arc<Mutex<HardwareWatcherState>>,
    pub workspace_state: Arc<SessionWorkspaceState>,
    pub pending_action: Arc<Mutex<PendingAction>>,
    pub kaomoji_engine: Arc<Mutex<KaomojiEngine>>,
    pub show_progress: bool,
}

pub fn render_action_preview_box(args: &ShellCommandArgs, lang: WizardLang) -> String {
    let lines = lang.shell_action_lines(&args.command, &args.intent, args.requires_root);
    ui::render_plain_box(lang.action_box_title(), &lines, 2)
}

pub fn posix_shell_for_runner() -> &'static str {
    pty::resolve_posix_shell()
}

/// Host OS identity (`$HOME` and active username) cached once per process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIdentity {
    pub real_home: String,
    pub os_username: String,
}

pub fn resolve_host_identity() -> &'static HostIdentity {
    static IDENTITY: OnceLock<HostIdentity> = OnceLock::new();
    IDENTITY.get_or_init(|| {
        let real_home = env::var("HOME")
            .ok()
            .map(|h| h.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                dirs::home_dir()
                    .map(|p| p.to_string_lossy().trim_end_matches('/').to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "/tmp".to_string());

        let os_username = env::var("USER")
            .or_else(|_| env::var("LOGNAME"))
            .ok()
            .map(|u| u.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                Command::new("whoami")
                    .stdin(Stdio::null())
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .or_else(|| {
                Path::new(&real_home)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "linux".to_string());

        HostIdentity {
            real_home,
            os_username,
        }
    })
}

pub fn resolve_real_home_dir() -> String {
    resolve_host_identity().real_home.clone()
}

fn interception_ring_buffer() -> &'static Mutex<std::collections::VecDeque<String>> {
    static INTERCEPT_LOG: OnceLock<Mutex<std::collections::VecDeque<String>>> = OnceLock::new();
    INTERCEPT_LOG.get_or_init(|| Mutex::new(std::collections::VecDeque::with_capacity(32)))
}

pub fn record_defensive_interception(category: &str, detail: impl Into<String>) {
    let msg = format!("[{category}] {}", detail.into());
    if env::var("WAISH_DEBUG")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    {
        eprintln!("[waish-interceptor] {msg}");
    }
    if let Ok(mut guard) = interception_ring_buffer().lock() {
        if guard.len() >= 32 {
            guard.pop_front();
        }
        guard.push_back(msg);
    }
}

#[cfg(test)]
pub fn recent_defensive_interceptions() -> Vec<String> {
    interception_ring_buffer()
        .lock()
        .map(|g| g.iter().cloned().collect())
        .unwrap_or_default()
}

pub fn rewrite_companion_home_and_tilde_with_env(
    input: &str,
    companion_user_name: &str,
    real_home: &str,
) -> String {
    let clean_home = real_home.trim().trim_end_matches('/');
    if input.is_empty() || clean_home.is_empty() {
        return input.to_string();
    }

    let mut result = input.to_string();

    let mut candidate_names: Vec<&str> = Vec::new();
    let trimmed_user = companion_user_name.trim();
    if !trimmed_user.is_empty() {
        candidate_names.push(trimmed_user);
    }
    if !candidate_names
        .iter()
        .any(|n| n.eq_ignore_ascii_case(DEFAULT_USER_REAL_NAME))
    {
        candidate_names.push(DEFAULT_USER_REAL_NAME);
    }

    for alias_name in candidate_names {
        let safe_alias: String = alias_name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            .collect();
        if safe_alias.is_empty() {
            continue;
        }
        let fake_prefix = format!("/home/{safe_alias}");
        if fake_prefix.eq_ignore_ascii_case(clean_home) {
            continue;
        }

        let pattern = format!(
            r#"/home/{}(/|[\s'"`;|&><)\],:]|$)"#,
            regex::escape(&safe_alias)
        );
        if let Ok(re) = RegexBuilder::new(&pattern).case_insensitive(true).build() {
            let replacement = format!("{clean_home}$1");
            result = re.replace_all(&result, replacement.as_str()).into_owned();
        }
    }

    expand_tilde_tokens_in_text(&result, clean_home)
}

fn expand_tilde_tokens_in_text(input: &str, clean_home: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len() + 16);
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'~' {
            let prev_ch = if i == 0 {
                None
            } else {
                input[..i].chars().next_back()
            };
            let is_bash_regex_op = prev_ch == Some('=')
                && i >= 2
                && input[..i - 1].ends_with(char::is_whitespace);

            let at_token_start = !is_bash_regex_op
                && match prev_ch {
                    None => true,
                    Some(c) => {
                        c.is_whitespace()
                            || matches!(c, '\'' | '"' | '=' | ':' | '(' | '[' | '>' | ',')
                    }
                };

            if at_token_start {
                let next_ch = input[i + 1..].chars().next();
                if next_ch == Some('/') {
                    out.push_str(clean_home);
                    out.push('/');
                    i += 2;
                    continue;
                }
                let is_standalone_tilde = match next_ch {
                    None => true,
                    Some(c) => {
                        c.is_whitespace()
                            || matches!(c, '\'' | '"' | ';' | '|' | '&' | ')' | ']' | ',')
                    }
                };
                if is_standalone_tilde {
                    out.push_str(clean_home);
                    i += 1;
                    continue;
                }
            }
        }

        if let Some(ch) = input[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }

    out
}

pub fn rewrite_shell_command_paths(command: &str, settings: &Settings) -> String {
    let real_home = resolve_real_home_dir();
    let rewritten = rewrite_companion_home_and_tilde_with_env(
        command,
        &settings.general.user_real_name,
        &real_home,
    );
    if rewritten != command {
        record_defensive_interception(
            "path_rewrite",
            format!("rewrote shell path(s) -> {rewritten}"),
        );
    }
    rewritten
}

pub fn sanitize_path_for_user(raw_path: &str, companion_user_name: &str) -> PathBuf {
    let real_home = resolve_real_home_dir();
    let trimmed = raw_path.trim();
    let rewritten =
        rewrite_companion_home_and_tilde_with_env(trimmed, companion_user_name, &real_home);
    if rewritten != trimmed {
        record_defensive_interception(
            "path_rewrite",
            format!("rewrote file arg `{trimmed}` -> `{rewritten}`"),
        );
    }
    PathBuf::from(rewritten)
}

pub fn configure_isolated_process_group(cmd: &mut Command) {
    // SAFETY: `libc::setpgid(0, 0)` is POSIX async-signal-safe inside `pre_exec`.
    unsafe {
        cmd.pre_exec(|| {
            let _ = libc::setpgid(0, 0);
            Ok(())
        });
    }
}

pub fn spawn_in_isolated_process_group(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    configure_isolated_process_group(cmd);
    cmd.spawn()
}

fn pkill_supports_ignore_ancestors() -> bool {
    static SUPPORTS: OnceLock<bool> = OnceLock::new();
    *SUPPORTS.get_or_init(|| {
        Command::new("pkill")
            .arg("--help")
            .stdin(Stdio::null())
            .output()
            .ok()
            .map(|out| {
                let text = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                text.contains("--ignore-ancestors")
            })
            .unwrap_or(false)
    })
}

pub fn collect_protected_pid_chain(start_pid: u32) -> Vec<u32> {
    let mut chain = vec![start_pid];
    let mut cur = start_pid;
    for _ in 0..16 {
        let stat_path = format!("/proc/{cur}/stat");
        let Ok(raw) = std::fs::read_to_string(&stat_path) else {
            break;
        };
        let Some(close_paren) = raw.rfind(')') else {
            break;
        };
        let after = raw[close_paren + 1..].trim_start();
        let mut fields = after.split_whitespace();
        let _state = fields.next();
        let Some(ppid_str) = fields.next() else {
            break;
        };
        let Ok(ppid) = ppid_str.parse::<u32>() else {
            break;
        };
        if ppid <= 1 || chain.contains(&ppid) {
            break;
        }
        chain.push(ppid);
        cur = ppid;
    }
    chain
}

pub fn bracketize_pkill_pattern(raw_pattern: &str) -> String {
    let trimmed = raw_pattern.trim();
    let unquoted = trimmed
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(trimmed);

    let mut chars = unquoted.chars();
    let Some(first) = chars.next() else {
        return raw_pattern.to_string();
    };
    let rest = chars.as_str();

    if matches!(first, '[' | '^' | '(' | '\\' | '.' | '*' | '?') || !first.is_ascii_alphanumeric() {
        return raw_pattern.to_string();
    }

    let inner = format!("[{first}]{rest}");
    pty::shell_single_quote(&inner)
}

fn rewrite_pkill_segment_patterns(segment: &str) -> String {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let Some(pkill_idx) = tokens.iter().position(|t| {
        let clean = t.trim_matches(|c| c == '(' || c == ')');
        let base = Path::new(clean)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(clean);
        base == "pkill"
    }) else {
        return segment.to_string();
    };

    let after = &tokens[pkill_idx + 1..];
    let has_full_flag = after.iter().any(|tok| {
        *tok == "--full"
            || (tok.starts_with('-') && !tok.starts_with("--") && tok[1..].contains('f'))
    });
    if !has_full_flag {
        return segment.to_string();
    }

    let mut skip_next = false;
    let mut pattern_tok_idx: Option<usize> = None;
    for (rel_i, &tok) in after.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if matches!(
            tok,
            "-P" | "--parent"
                | "-u"
                | "--euid"
                | "-U"
                | "--uid"
                | "-G"
                | "--group"
                | "-g"
                | "--pgroup"
                | "-s"
                | "--session"
                | "-t"
                | "--terminal"
                | "-F"
                | "--pidfile"
                | "--signal"
                | "--ns"
                | "--cgroup"
        ) {
            skip_next = true;
            continue;
        }
        if tok.starts_with('-') {
            continue;
        }
        pattern_tok_idx = Some(pkill_idx + 1 + rel_i);
    }

    let Some(target_idx) = pattern_tok_idx else {
        return segment.to_string();
    };
    let original_pat = tokens[target_idx];
    let clean_pat = original_pat.trim_end_matches(')');
    let trailing_parens = &original_pat[clean_pat.len()..];
    let bracketed = format!("{}{trailing_parens}", bracketize_pkill_pattern(clean_pat));
    if bracketed == original_pat {
        return segment.to_string();
    }

    if let Some(pos) = segment.rfind(original_pat) {
        let mut out = String::with_capacity(segment.len() + 6);
        out.push_str(&segment[..pos]);
        out.push_str(&bracketed);
        out.push_str(&segment[pos + original_pat.len()..]);
        out
    } else {
        segment.to_string()
    }
}

pub fn armor_process_kill_command(command: &str) -> Result<String, String> {
    let armored = armor_process_kill_command_with_pid(
        command,
        std::process::id(),
        pkill_supports_ignore_ancestors(),
    )?;
    if armored != command {
        record_defensive_interception(
            "process_armor",
            format!("sanitized pkill command -> {armored}"),
        );
    }
    Ok(armored)
}

pub fn armor_process_kill_command_with_pid(
    command: &str,
    waish_pid: u32,
    supports_ignore_ancestors: bool,
) -> Result<String, String> {
    let protected_pids: Vec<String> = collect_protected_pid_chain(waish_pid)
        .into_iter()
        .map(|p| p.to_string())
        .collect();
    let self_pid_str = waish_pid.to_string();

    for segment in command.split([';', '|', '&', '\n']) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        let mut idx = 0usize;
        while idx < tokens.len() {
            let t = tokens[idx].trim_matches(|c| c == '(' || c == ')' || c == '\'' || c == '"');
            if t.contains('=') && !t.starts_with('/') && !t.starts_with('-') {
                idx += 1;
                continue;
            }
            let base = Path::new(t)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(t);
            if matches!(base, "sudo" | "doas" | "env" | "command" | "nohup") {
                idx += 1;
                continue;
            }

            if matches!(base, "pkill" | "killall" | "kill") {
                let args_slice = &tokens[idx + 1..];
                for (a_idx, raw_arg) in args_slice.iter().enumerate() {
                    let clean_arg =
                        raw_arg.trim_matches(|c| c == '\'' || c == '"' || c == ')' || c == '(');
                    let arg_low = clean_arg.to_ascii_lowercase();

                    if protected_pids.iter().any(|p| p == clean_arg) {
                        return Err(format!(
                            "[System Notice]: Perintah `{base}` diblokir oleh Anti-Self-Kill Process Armor karena menargetkan PID proses waish/induk ({clean_arg})."
                        ));
                    }
                    if matches!(clean_arg, "-P" | "--parent")
                        && args_slice
                            .get(a_idx + 1)
                            .is_some_and(|next| next.trim_matches(['\'', '"']) == self_pid_str)
                    {
                        return Err(format!(
                            "[System Notice]: Perintah `{base} -P {waish_pid}` diblokir oleh Anti-Self-Kill Process Armor karena membunuh subshell runner waish."
                        ));
                    }

                    if !clean_arg.starts_with('-') {
                        if matches!(base, "pkill" | "killall") && arg_low.contains("waish") {
                            return Err(format!(
                                "[System Notice]: Perintah `{base}` dengan pola `{clean_arg}` diblokir oleh Anti-Self-Kill Process Armor agar tidak menghentikan proses waish aktif (PID {waish_pid})."
                            ));
                        }
                        if base == "killall" && matches!(arg_low.as_str(), "bash" | "sh") {
                            return Err(
                                "[System Notice]: Perintah `killall bash/sh` diblokir oleh Anti-Self-Kill Process Armor agar tidak membunuh subshell runner."
                                    .to_string(),
                            );
                        }
                    }
                }
            }
            break;
        }
    }

    let mut bracketed_cmd = String::with_capacity(command.len() + 16);
    let mut seg_start = 0usize;
    for (byte_i, ch) in command.char_indices() {
        if matches!(ch, ';' | '|' | '&' | '\n') {
            bracketed_cmd.push_str(&rewrite_pkill_segment_patterns(&command[seg_start..byte_i]));
            bracketed_cmd.push(ch);
            seg_start = byte_i + ch.len_utf8();
        }
    }
    bracketed_cmd.push_str(&rewrite_pkill_segment_patterns(&command[seg_start..]));

    if supports_ignore_ancestors {
        let pkill_re = RegexBuilder::new(r"(^|[\s;|&(])((?:/usr/bin/|/bin/)?pkill)\b(\s+)")
            .build()
            .expect("valid pkill regex");
        let rewritten = pkill_re
            .replace_all(&bracketed_cmd, |caps: &regex::Captures<'_>| {
                let prefix = &caps[1];
                let bin = &caps[2];
                let ws = &caps[3];
                let after_pos = caps.get(0).map(|m| m.end()).unwrap_or(0);
                let tail = &bracketed_cmd[after_pos..];
                let next_seg = tail.split([';', '|', '&', '\n']).next().unwrap_or("");
                let already_armored = next_seg
                    .split_whitespace()
                    .any(|tok| tok == "-A" || tok == "--ignore-ancestors");
                if already_armored {
                    format!("{prefix}{bin}{ws}")
                } else {
                    format!("{prefix}{bin} --ignore-ancestors{ws}")
                }
            })
            .into_owned();
        Ok(rewritten)
    } else {
        Ok(bracketed_cmd)
    }
}

pub fn is_adhoc_tmp_office_python_command(command: &str) -> bool {
    let low = command.to_ascii_lowercase();

    let touches_office = low.contains(".xlsx")
        || low.contains(".docx")
        || low.contains("openpyxl")
        || low.contains("xlsxwriter")
        || low.contains("python-docx")
        || low.contains("docx.document")
        || low.contains("to_excel(")
        || low.contains("read_excel(")
        || (low.contains("pandas") && (low.contains(".csv") || low.contains("excel")));

    let touches_csv_via_tmp_py = low.contains("/tmp/") && low.contains(".py") && low.contains(".csv");

    let invokes_python_or_tmp_py = (low.contains("/tmp/") && low.contains(".py"))
        || low.contains("python3 ")
        || low.contains("python ")
        || low.contains("pip install openpyxl")
        || low.contains("pip install python-docx")
        || low.contains("pip install xlsxwriter")
        || low.contains("pip install pandas")
        || low.contains("python-openpyxl")
        || low.contains("python-xlsxwriter")
        || low.contains("python-pandas");

    (touches_office && invokes_python_or_tmp_py) || touches_csv_via_tmp_py
}

pub fn is_adhoc_tmp_office_python_write(path: &str, content: &str) -> bool {
    let p_low = path.trim().to_ascii_lowercase();
    if !(p_low.starts_with("/tmp/") && p_low.ends_with(".py")) {
        return false;
    }
    let c_low = content.to_ascii_lowercase();
    c_low.contains(".xlsx")
        || c_low.contains(".docx")
        || c_low.contains(".csv")
        || c_low.contains("openpyxl")
        || c_low.contains("xlsxwriter")
        || c_low.contains("pandas")
        || c_low.contains("docx")
}

pub fn is_background_gui_shell_launch(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return false;
    }
    let low = trimmed.to_ascii_lowercase();

    if low.contains("--version") || low.contains("--help") {
        return false;
    }

    const GUI_DESKTOP_BINS: &[&str] = &[
        "onlyoffice-desktopeditors",
        "onlyoffice",
        "desktopeditors",
        "libreoffice",
        "soffice",
        "zathura",
        "evince",
        "okular",
        "firefox",
        "brave",
        "chromium",
        "google-chrome",
        "obsidian",
    ];

    for seg in trimmed.split([';', '\n']) {
        let seg_trim = seg.trim();
        if seg_trim.is_empty() {
            continue;
        }
        let seg_low = seg_trim.to_ascii_lowercase();
        let is_bg_style = seg_low.starts_with("nohup ")
            || seg_low.contains(" nohup ")
            || seg_low.starts_with("setsid ")
            || (seg_trim.ends_with('&') && !seg_trim.ends_with("&&"));

        let tokens: Vec<&str> = seg_trim
            .trim_end_matches('&')
            .split_whitespace()
            .collect();
        if tokens.is_empty() {
            continue;
        }
        let first_base = Path::new(tokens[0])
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(tokens[0])
            .to_ascii_lowercase();
        if matches!(
            first_base.as_str(),
            "pacman" | "yay" | "paru" | "sudo" | "pkill" | "kill" | "killall" | "pgrep" | "which" | "type" | "command"
        ) {
            continue;
        }

        let invokes_gui_bin = tokens.iter().any(|tok| {
            let clean = tok.trim_matches(['\'', '"', '(', ')']);
            let base = Path::new(clean)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(clean)
                .to_ascii_lowercase();
            GUI_DESKTOP_BINS.iter().any(|b| *b == base)
        });

        let invokes_office_directly = matches!(
            first_base.as_str(),
            "onlyoffice-desktopeditors" | "desktopeditors" | "libreoffice" | "soffice"
        );

        if invokes_gui_bin && (is_bg_style || invokes_office_directly) {
            return true;
        }
    }
    false
}

pub fn is_alarm_filesystem_rummage_command(command: &str) -> bool {
    let low = command.to_ascii_lowercase();
    let uses_find = low.contains("find /") || low.contains("find ~") || low.contains("find $home");
    uses_find && (low.contains("alarm") || low.contains(".timer"))
}

pub fn is_redundant_post_ipc_window_poll(command: &str) -> bool {
    let low = command.to_ascii_lowercase();
    if low.contains("hyprctl dispatch") || low.contains("pacman ") || low.contains("yay ") {
        return false;
    }
    low.contains("hyprctl clients")
        || low.contains("hyprctl -j clients")
        || (low.contains("sleep ") && (low.contains("pgrep") || low.contains("hyprctl") || low.contains("ps aux")))
        || low.starts_with("pgrep ")
}

pub fn detect_active_x11_display() -> String {
    if let Ok(disp) = env::var("DISPLAY") {
        let trimmed = disp.trim();
        if trimmed.starts_with(':') && trimmed.len() >= 2 {
            return trimmed.to_string();
        }
    }

    let uid = unsafe { libc::geteuid() };
    if let Ok(entries) = std::fs::read_dir("/tmp/.X11-unix") {
        let mut owned_displays: Vec<u32> = Vec::new();
        let mut any_displays: Vec<u32> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(s) = name.to_str() else {
                continue;
            };
            let Some(num_str) = s.strip_prefix('X') else {
                continue;
            };
            let Ok(num) = num_str.parse::<u32>() else {
                continue;
            };
            any_displays.push(num);
            if entry.metadata().is_ok_and(|meta| meta.uid() == uid) {
                owned_displays.push(num);
            }
        }
        owned_displays.sort_unstable();
        if let Some(best) = owned_displays.first() {
            return format!(":{best}");
        }
        any_displays.sort_unstable();
        if let Some(first) = any_displays.first() {
            return format!(":{first}");
        }
    }

    ":0".to_string()
}

pub fn detach_background_gui_from_pty_sighup(command: &str) -> String {
    let trimmed = command.trim();
    if !trimmed.ends_with('&')
        || trimmed.ends_with("&&")
        || trimmed.contains("setsid ")
        || trimmed.contains(';')
        || trimmed.contains('|')
    {
        return trimmed.to_string();
    }

    let without_amp = trimmed.trim_end_matches('&').trim();
    let without_nohup = without_amp
        .strip_prefix("nohup ")
        .map(str::trim)
        .unwrap_or(without_amp);
    if without_nohup.is_empty() {
        return trimmed.to_string();
    }

    if without_nohup.contains('>') {
        format!("setsid {without_nohup} </dev/null &")
    } else {
        format!("setsid {without_nohup} </dev/null >/dev/null 2>&1 &")
    }
}

pub fn wrap_xwayland_gui_command(command: &str, display: &str) -> String {
    let clean_disp = if display.trim().is_empty() {
        ":0"
    } else {
        display.trim()
    };
    let clean_cmd = command.trim().trim_end_matches('&').trim();
    format!("env -u XAUTHORITY DISPLAY={clean_disp} setsid {clean_cmd} >/dev/null 2>&1 &")
}

pub fn maybe_wrap_legacy_x11_command(command: &str) -> String {
    let trimmed = command.trim();
    if trimmed.contains("env -u XAUTHORITY")
        || trimmed.contains(';')
        || trimmed.contains("&&")
        || trimmed.contains('|')
    {
        return trimmed.to_string();
    }

    let first_tok = trimmed.split_whitespace().next().unwrap_or("");
    let base = Path::new(first_tok)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(first_tok);
    let second_arg = trimmed.split_whitespace().nth(1).unwrap_or("");
    if matches!(second_arg, "--help" | "-h" | "--version" | "-v" | "-V") {
        return trimmed.to_string();
    }

    if LEGACY_X11_GUI_BINS
        .iter()
        .any(|b| b.eq_ignore_ascii_case(base))
    {
        let disp = detect_active_x11_display();
        wrap_xwayland_gui_command(trimmed, &disp)
    } else {
        trimmed.to_string()
    }
}

pub fn is_proc_or_xauth_rummage_command(command: &str) -> bool {
    let low = command.to_ascii_lowercase();
    let rummages_proc_env = low.contains("/proc/") && (low.contains("/environ") || low.contains("xauthority") || low.contains("display="));
    let rummages_xauth_fs = (low.contains("find /") || low.contains("find ~") || low.contains("locate "))
        && (low.contains("xauthority") || low.contains("xauth"));
    rummages_proc_env || rummages_xauth_fs
}

pub fn ensure_root_sudo_prefix(command: &str, requires_root: bool) -> String {
    let trimmed = command.trim();
    if !requires_root || trimmed.is_empty() {
        return trimmed.to_string();
    }

    let first_word = trimmed.split_whitespace().next().unwrap_or("");
    let base_bin = std::path::Path::new(first_word)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(first_word);

    if matches!(
        base_bin,
        "sudo" | "doas" | "pkexec" | "su" | "yay" | "paru" | "trizen" | "pikaur" | "makepkg"
    ) {
        trimmed.to_string()
    } else {
        format!("sudo {trimmed}")
    }
}

pub fn is_readonly_command(command: &str, requires_root: bool) -> bool {
    if requires_root {
        return false;
    }
    let trimmed = command.trim();
    if trimmed.contains('>') || trimmed.contains("tee ") {
        return false;
    }

    let first_word = trimmed.split_whitespace().next().unwrap_or("");
    match first_word {
        "ls" | "pwd" | "whoami" | "id" | "uname" | "hostname" | "uptime" | "date" | "cal"
        | "df" | "du" | "free" | "lscpu" | "lsblk" | "lspci" | "lsusb" | "cat" | "head"
        | "tail" | "wc" | "which" | "type" | "file" | "stat" | "ps" | "env" | "printenv" => true,
        "pacman" | "yay" | "paru" => trimmed
            .split_whitespace()
            .nth(1)
            .is_some_and(|flag| flag.starts_with("-Q") || flag == "-Si" || flag == "-Ss"),
        "git" => matches!(
            trimmed.split_whitespace().nth(1),
            Some("status" | "log" | "diff" | "branch" | "show" | "remote")
        ),
        _ => false,
    }
}

pub async fn execute_tool_call(
    tool_call: &ToolCall,
    ctx: &ToolExecutionContext<'_>,
) -> ToolExecutionResult {
    let bot = &ctx.settings.general.bot_name;
    let is_media_tool = matches!(
        tool_call.name.as_str(),
        OPEN_MEDIA_NAME | CONTROL_MEDIA_NAME
    );

    if !is_media_tool {
        desktop::export_waybar_status(
            WaybarState::Busy,
            bot,
            Some(&format!("Running tool: {}", tool_call.name)),
        );
    }

    let result = match tool_call.name.as_str() {
        RUN_SHELL_COMMAND_NAME => dispatch_shell_command(tool_call, ctx),
        EXECUTE_RECIPE_NAME => dispatch_execute_recipe(tool_call, ctx).await,
        READ_FILE_NAME => dispatch_read_file(tool_call, ctx),
        WRITE_FILE_NAME => dispatch_write_file(tool_call, ctx),
        REPLACE_IN_FILE_NAME => dispatch_replace_in_file(tool_call, ctx),
        ROLLBACK_CONFIG_NAME => dispatch_rollback_config(tool_call, ctx),
        MANAGE_WORKSPACE_NAME => dispatch_manage_workspace(tool_call, ctx),
        LAUNCH_GUI_APP_NAME => dispatch_launch_gui_app(tool_call, ctx).await,
        MANAGE_WINDOW_NAME => dispatch_manage_window(tool_call, ctx).await,
        OPEN_MEDIA_NAME => dispatch_open_media(tool_call, ctx).await,
        CONTROL_MEDIA_NAME => dispatch_control_media(tool_call, ctx).await,
        OPEN_BROWSER_SEARCH_NAME => dispatch_open_browser_search(tool_call, ctx).await,
        QUICK_SEARCH_NAME => dispatch_quick_search(tool_call).await,
        READ_CLIPBOARD_NAME => dispatch_read_clipboard(tool_call),
        FIND_PACKAGE_OWNER_NAME => dispatch_find_package_owner(tool_call, ctx),
        MANAGE_SCRATCHPAD_NAME => dispatch_manage_scratchpad(tool_call, ctx),
        UPDATE_BATTERY_PROMISE_NAME => dispatch_update_battery_promise(tool_call, ctx),
        MANAGE_ALARM_NAME => dispatch_manage_alarm(tool_call),
        MANAGE_SPREADSHEET_NAME => dispatch_manage_spreadsheet(tool_call, ctx),
        MANAGE_DOCUMENT_NAME => dispatch_manage_document(tool_call, ctx).await,
        other => ToolExecutionResult {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            output_for_llm: format!("status: error\nmessage: Tool '{other}' tidak dikenal."),
            succeeded: false,
            cancelled_cleanly: false,
            course_correction: None,
        },
    };

    if !is_media_tool {
        if result.succeeded {
            let face = ctx
                .kaomoji_engine
                .lock()
                .map(|mut eng| {
                    eng.pick_from_group(&[
                        EmotionSpectrum::SmugSuperior,
                        EmotionSpectrum::CuriousHacker,
                    ])
                })
                .unwrap_or(Kaomoji::Success.as_str());
            desktop::export_waybar_status_with_kaomoji(WaybarState::Success, bot, face, None);
        } else {
            let face = ctx
                .kaomoji_engine
                .lock()
                .map(|mut eng| {
                    eng.pick_from_group(&[
                        EmotionSpectrum::MelancholyFail,
                        EmotionSpectrum::SystemPanic,
                    ])
                })
                .unwrap_or(Kaomoji::Angry.as_str());
            desktop::export_waybar_status_with_kaomoji(WaybarState::Warning, bot, face, None);
        }
    } else if !result.succeeded {
        let face = ctx
            .kaomoji_engine
            .lock()
            .map(|mut eng| {
                eng.pick_from_group(&[
                    EmotionSpectrum::MelancholyFail,
                    EmotionSpectrum::SystemPanic,
                ])
            })
            .unwrap_or(Kaomoji::Angry.as_str());
        desktop::export_waybar_status_with_kaomoji(WaybarState::Warning, bot, face, None);
    }

    result
}

fn ok_res(tc: &ToolCall, output: String) -> ToolExecutionResult {
    ToolExecutionResult {
        tool_call_id: tc.id.clone(),
        tool_name: tc.name.clone(),
        output_for_llm: output,
        succeeded: true,
        cancelled_cleanly: false,
        course_correction: None,
    }
}

fn err_res(tc: &ToolCall, msg: impl Into<String>) -> ToolExecutionResult {
    ToolExecutionResult {
        tool_call_id: tc.id.clone(),
        tool_name: tc.name.clone(),
        output_for_llm: format!("status: error\nmessage: {}", msg.into()),
        succeeded: false,
        cancelled_cleanly: false,
        course_correction: None,
    }
}

fn dispatch_shell_command(
    tool_call: &ToolCall,
    ctx: &ToolExecutionContext<'_>,
) -> ToolExecutionResult {
    let mut args = match ShellCommandArgs::from_json_str(&tool_call.arguments) {
        Ok(a) => a,
        Err(err) => return err_res(tool_call, err),
    };

    if is_adhoc_tmp_office_python_command(&args.command) {
        return ToolExecutionResult {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            output_for_llm: AD_HOC_OFFICE_SCRIPT_NOTICE.to_string(),
            succeeded: false,
            cancelled_cleanly: false,
            course_correction: None,
        };
    }

    if is_background_gui_shell_launch(&args.command) {
        return ToolExecutionResult {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            output_for_llm: BACKGROUND_GUI_SHELL_NOTICE.to_string(),
            succeeded: false,
            cancelled_cleanly: false,
            course_correction: None,
        };
    }

    if is_alarm_filesystem_rummage_command(&args.command) {
        return ToolExecutionResult {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            output_for_llm: ALARM_FS_RUMMAGE_NOTICE.to_string(),
            succeeded: false,
            cancelled_cleanly: false,
            course_correction: None,
        };
    }

    if ctx.workspace_state.is_turn_ipc_verified() && is_redundant_post_ipc_window_poll(&args.command)
    {
        return ToolExecutionResult {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            output_for_llm: POST_IPC_VERIFICATION_NOTICE.to_string(),
            succeeded: true,
            cancelled_cleanly: false,
            course_correction: None,
        };
    }

    if is_proc_or_xauth_rummage_command(&args.command) {
        let disp = detect_active_x11_display();
        return ToolExecutionResult {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            output_for_llm: format!(
                "[System Notice]: Dilarang menggeledah `/proc/` atau mencari cookie xauth yang tidak ada! \
                 DISPLAY aktif terdeteksi `{disp}` dan wrapper XWayland `env -u XAUTHORITY DISPLAY={disp} setsid <cmd> >/dev/null 2>&1 &` \
                 sudah diterapkan otomatis oleh waish. Jika aplikasi tetap gagal tampil, laporkan error ke user dan minta arahan."
            ),
            succeeded: false,
            cancelled_cleanly: false,
            course_correction: None,
        };
    }

    args.command = rewrite_shell_command_paths(&args.command, ctx.settings);

    match armor_process_kill_command(&args.command) {
        Ok(armored) => args.command = armored,
        Err(notice) => {
            return ToolExecutionResult {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                output_for_llm: notice,
                succeeded: false,
                cancelled_cleanly: false,
                course_correction: None,
            };
        }
    }

    args.command = desktop::hyprland::strip_forced_qt_wayland_for_onlyoffice(&args.command);
    args.command = maybe_wrap_legacy_x11_command(&args.command);
    args.command = detach_background_gui_from_pty_sighup(&args.command);
    args.command = ensure_root_sudo_prefix(&args.command, args.requires_root);

    if system_ops::command_invokes_git_commit(&args.command) {
        let target_repo = system_ops::extract_git_commit_workdir(&args.command)
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        if let Err(findings) = system_ops::inspect_git_commit_shield(&target_repo, ctx.settings) {
            let details: Vec<String> = findings
                .iter()
                .map(|f| format!("- [{}] {}", f.category, f.detail))
                .collect();
            return ToolExecutionResult {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                output_for_llm: format!(
                    "status: blocked_by_git_shield\n\
                     command: {}\n\
                     findings:\n{}\n\
                     action_required: Peringatkan user dengan tegas agar meng-unstage atau menghapus secret tersebut sebelum commit.",
                    args.command,
                    details.join("\n")
                ),
                succeeded: false,
                cancelled_cleanly: false,
                course_correction: None,
            };
        }
    }

    let already_confirmed_danger = match safety::inspect_and_confirm(&args.command, ctx.settings) {
        Ok(SafetyVerdict::Safe) => false,
        Ok(SafetyVerdict::Confirmed { .. }) => true,
        Ok(SafetyVerdict::Blocked { risk }) => {
            return ToolExecutionResult {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                output_for_llm: format!(
                    "status: cancelled_by_user\n\
                     command: {}\n\
                     reason: Perintah diblokir oleh Safety Interceptor ({risk}) dan ditolak pengguna.",
                    args.command
                ),
                succeeded: false,
                cancelled_cleanly: true,
                course_correction: None,
            };
        }
        Err(err) => return err_res(tool_call, format!("Gagal membaca konfirmasi keamanan: {err}")),
    };

    let lang = WizardLang::from_code(&ctx.settings.general.language);
    let is_readonly = is_readonly_command(&args.command, args.requires_root);
    if !is_readonly || !ctx.settings.behavior.auto_confirm_safe {
        println!();
        println!("{}", render_action_preview_box(&args, lang));
        println!();
    }

    if args.requires_root && !already_confirmed_danger {
        match safety::confirm_root_execution(&args.command, ctx.settings) {
            Ok(ConfirmationDecision::Confirmed) => {}
            Ok(ConfirmationDecision::Cancelled) => {
                return ToolExecutionResult {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    output_for_llm: format!(
                        "status: cancelled_by_user\n\
                         do_not_retry: true\n\
                         command: {}\n\
                         reason: User cancelled [y/N] confirmation for root command. Acknowledge briefly and do not retry.",
                        args.command
                    ),
                    succeeded: false,
                    cancelled_cleanly: true,
                    course_correction: None,
                };
            }
            Ok(ConfirmationDecision::Feedback(correction_text)) => {
                return ToolExecutionResult {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    output_for_llm: format!(
                        "status: user_inline_correction\n\
                         rejected_command: {}\n\
                         user_correction: {}\n\
                         instruction: Pengguna menolak perintah ini dan memberikan koreksi langsung. Segera patuhi koreksi pengguna dan panggil tool yang diperbaiki.",
                        args.command, correction_text
                    ),
                    succeeded: false,
                    cancelled_cleanly: false,
                    course_correction: Some(UserCourseCorrection {
                        rejected_command: args.command,
                        feedback: correction_text,
                    }),
                };
            }
            Err(err) => {
                return err_res(tool_call, format!("Failed to read [y/N] confirmation: {err}"));
            }
        }
    }

    match pty::execute_in_pty_localized(
        &args.command,
        posix_shell_for_runner(),
        args.requires_root,
        ctx.show_progress,
        &ctx.settings.general.bot_name,
        lang,
    ) {
        Ok(pty_out) => {
            notifier::notify_if_long_task(
                ctx.settings,
                &args.intent,
                pty_out.elapsed,
                pty_out.succeeded,
            );

            let mut llm_text = pty_out.format_for_llm(&args.command);

            if pty_out.succeeded {
                let installed_pkgs = system_ops::extract_installed_packages_from_command(&args.command);
                if !installed_pkgs.is_empty() {
                    if let Some(bins_report) =
                        system_ops::inspect_installed_package_binaries(&installed_pkgs)
                    {
                        llm_text.push_str(&format!("\n\n{bins_report}"));
                    }
                }
            }

            if !pty_out.succeeded && !pty_out.interrupted {
                if let Some(missing) =
                    system_ops::detect_missing_command_or_lib(&pty_out.clean_output)
                {
                    if let Ok(autopsy) = system_ops::execute_find_package_owner(
                        &missing,
                        ctx.settings.tools.preferred_package_manager(),
                    ) {
                        llm_text.push_str(&format!(
                            "\n\n[AUTO ERROR AUTOPSY untuk `{missing}`]:\n{autopsy}"
                        ));
                    }
                }
            }

            ToolExecutionResult {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                output_for_llm: llm_text,
                succeeded: pty_out.succeeded,
                cancelled_cleanly: false,
                course_correction: None,
            }
        }
        Err(err) => err_res(tool_call, format!("Gagal mengeksekusi PTY: {err}")),
    }
}

fn dispatch_read_file(tc: &ToolCall, ctx: &ToolExecutionContext<'_>) -> ToolExecutionResult {
    let args: ReadFileArgs = match serde_json::from_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, format!("argumen read_file tidak valid: {e}")),
    };
    match file_ops::execute_read_file_for_user(&args, &ctx.settings.general.user_real_name) {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

fn dispatch_write_file(tc: &ToolCall, ctx: &ToolExecutionContext<'_>) -> ToolExecutionResult {
    let args: WriteFileArgs = match serde_json::from_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, format!("argumen write_file tidak valid: {e}")),
    };
    if is_adhoc_tmp_office_python_write(&args.path, &args.content) {
        return ToolExecutionResult {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            output_for_llm: AD_HOC_OFFICE_SCRIPT_NOTICE.to_string(),
            succeeded: false,
            cancelled_cleanly: false,
            course_correction: None,
        };
    }
    match file_ops::execute_write_file(&args, ctx.settings, ctx.shadow_git) {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

fn dispatch_manage_spreadsheet(
    tc: &ToolCall,
    ctx: &ToolExecutionContext<'_>,
) -> ToolExecutionResult {
    let args = match ManageSpreadsheetArgs::from_json_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, e),
    };
    match document::execute_manage_spreadsheet(&args, ctx.settings) {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

async fn dispatch_manage_document(
    tc: &ToolCall,
    ctx: &ToolExecutionContext<'_>,
) -> ToolExecutionResult {
    let args = match ManageDocumentArgs::from_json_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, e),
    };
    let is_hot_edit = args.action == "hot_edit";
    let res = document::execute_manage_document_with_context(
        &args,
        ctx.settings,
        ctx.shadow_git,
        &ctx.workspace_state,
    )
    .await;
    if is_hot_edit {
        ctx.workspace_state.mark_turn_ipc_verified();
    }
    match res {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

async fn dispatch_execute_recipe(
    tc: &ToolCall,
    ctx: &ToolExecutionContext<'_>,
) -> ToolExecutionResult {
    let args = match ExecuteRecipeArgs::from_json_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, e),
    };
    let touches_ipc = matches!(
        args.recipe.as_str(),
        "create_and_open_document"
            | "create_and_open"
            | "open_file_in_workspace"
            | "open_document"
            | "open_file"
            | "hot_edit_document"
            | "hot_edit"
            | "trigger_scene"
            | "scene"
    );
    let recipe_ctx = RecipeExecutionContext {
        settings: ctx.settings,
        shadow_git: ctx.shadow_git,
        workspace_state: &ctx.workspace_state,
        pending_action: &ctx.pending_action,
        kaomoji_engine: &ctx.kaomoji_engine,
    };
    let res = recipes::execute_recipe(&args, &recipe_ctx).await;
    if touches_ipc {
        ctx.workspace_state.mark_turn_ipc_verified();
    }
    match res {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

async fn dispatch_launch_gui_app(
    tc: &ToolCall,
    ctx: &ToolExecutionContext<'_>,
) -> ToolExecutionResult {
    let args = match LaunchGuiAppArgs::from_json_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, e),
    };
    let res = desktop::execute_launch_gui_app_with_config(
        &args,
        &ctx.workspace_state,
        Some(&ctx.settings.desktop),
    )
    .await;
    ctx.workspace_state.mark_turn_ipc_verified();
    match res {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

fn dispatch_replace_in_file(tc: &ToolCall, ctx: &ToolExecutionContext<'_>) -> ToolExecutionResult {
    let args: ReplaceInFileArgs = match serde_json::from_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, format!("argumen replace_in_file tidak valid: {e}")),
    };
    match file_ops::execute_replace_in_file(&args, ctx.settings, ctx.shadow_git) {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

fn dispatch_rollback_config(tc: &ToolCall, ctx: &ToolExecutionContext<'_>) -> ToolExecutionResult {
    let args: RollbackConfigArgs = match serde_json::from_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, format!("argumen rollback_config tidak valid: {e}")),
    };
    match file_ops::execute_rollback_config(&args, ctx.settings, ctx.shadow_git) {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

fn dispatch_manage_workspace(
    tc: &ToolCall,
    ctx: &ToolExecutionContext<'_>,
) -> ToolExecutionResult {
    let args = match ManageWorkspaceArgs::from_json_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, e),
    };
    let is_launch = matches!(args.action.as_str(), "launch_app" | "launch");
    let res = desktop::execute_manage_workspace_with_config(
        &args,
        &ctx.workspace_state,
        Some(&ctx.settings.desktop),
    );
    if is_launch {
        ctx.workspace_state.mark_turn_ipc_verified();
    }
    match res {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

fn dispatch_manage_alarm(tc: &ToolCall) -> ToolExecutionResult {
    let args = match ManageAlarmArgs::from_json_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, e),
    };
    match timer::execute_manage_alarm(&args) {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

async fn dispatch_manage_window(
    tc: &ToolCall,
    ctx: &ToolExecutionContext<'_>,
) -> ToolExecutionResult {
    let args = match ManageWindowArgs::from_json_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, e),
    };
    match desktop::execute_manage_window_with_config(
        &args,
        &ctx.workspace_state,
        Some(&ctx.settings.desktop),
    )
    .await
    {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

async fn dispatch_open_media(
    tc: &ToolCall,
    ctx: &ToolExecutionContext<'_>,
) -> ToolExecutionResult {
    let args = match OpenMediaArgs::from_json_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, e),
    };
    match media::execute_open_media_with_config(
        &args,
        &ctx.workspace_state,
        &ctx.pending_action,
        &ctx.kaomoji_engine,
        &ctx.settings.general.bot_name,
        Some(&ctx.settings.desktop),
    )
    .await
    {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

async fn dispatch_control_media(
    tc: &ToolCall,
    ctx: &ToolExecutionContext<'_>,
) -> ToolExecutionResult {
    let args = match ControlMediaArgs::from_json_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, e),
    };
    match media::execute_control_media(&args, &ctx.kaomoji_engine, &ctx.settings.general.bot_name)
        .await
    {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

async fn dispatch_open_browser_search(
    tc: &ToolCall,
    ctx: &ToolExecutionContext<'_>,
) -> ToolExecutionResult {
    let args = match OpenBrowserSearchArgs::from_json_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, e),
    };
    match web_search::execute_open_browser_search_with_config(
        &args,
        &ctx.workspace_state,
        Some(&ctx.settings.desktop),
    )
    .await
    {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

async fn dispatch_quick_search(tc: &ToolCall) -> ToolExecutionResult {
    let args = match QuickSearchArgs::from_json_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, e),
    };
    match web_search::execute_quick_search(&args).await {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

fn dispatch_read_clipboard(tc: &ToolCall) -> ToolExecutionResult {
    match system_ops::execute_read_clipboard() {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

fn dispatch_find_package_owner(
    tc: &ToolCall,
    ctx: &ToolExecutionContext<'_>,
) -> ToolExecutionResult {
    let args = match FindPackageOwnerArgs::from_json_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, e),
    };
    match system_ops::execute_find_package_owner(
        &args.query,
        ctx.settings.tools.preferred_package_manager(),
    ) {
        Ok(out) => ok_res(tc, out),
        Err(e) => err_res(tc, e.to_string()),
    }
}

fn dispatch_manage_scratchpad(
    tc: &ToolCall,
    ctx: &ToolExecutionContext<'_>,
) -> ToolExecutionResult {
    let args: ManageScratchpadArgs = match serde_json::from_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, format!("argumen manage_scratchpad tidak valid: {e}")),
    };

    let mut pad = match Scratchpad::load(&ctx.paths.scratchpad) {
        Ok(p) => p,
        Err(e) => return err_res(tc, e.to_string()),
    };

    match args.action.trim().to_ascii_lowercase().as_str() {
        "add" => {
            if args.content.trim().is_empty() {
                return err_res(tc, "field `content` wajib diisi untuk action `add`.");
            }
            let note = pad.add_note(&args.tag, &args.content);
            if let Err(e) = pad.save(&ctx.paths.scratchpad) {
                return err_res(tc, e.to_string());
            }
            ok_res(
                tc,
                format!(
                    "Catatan scratchpad #{} [{}] tersimpan: {}",
                    note.id, note.tag, note.content
                ),
            )
        }
        "list" => ok_res(tc, pad.render_box_lines().join("\n")),
        "remove" => {
            let Some(id) = args.id else {
                return err_res(tc, "field `id` wajib diisi untuk action `remove`.");
            };
            if pad.remove_note(id) {
                if let Err(e) = pad.save(&ctx.paths.scratchpad) {
                    return err_res(tc, e.to_string());
                }
                ok_res(tc, format!("Catatan scratchpad #{id} telah dihapus."))
            } else {
                err_res(tc, format!("Catatan scratchpad #{id} tidak ditemukan."))
            }
        }
        "clear" => {
            let count = pad.clear();
            if let Err(e) = pad.save(&ctx.paths.scratchpad) {
                return err_res(tc, e.to_string());
            }
            ok_res(tc, format!("Seluruh scratchpad dikosongkan ({count} catatan dihapus)."))
        }
        other => err_res(tc, format!("Aksi scratchpad `{other}` tidak dikenal.")),
    }
}

fn dispatch_update_battery_promise(
    tc: &ToolCall,
    ctx: &ToolExecutionContext<'_>,
) -> ToolExecutionResult {
    let args: UpdateBatteryPromiseArgs = match serde_json::from_str(&tc.arguments) {
        Ok(a) => a,
        Err(e) => return err_res(tc, format!("argumen update_battery_promise tidak valid: {e}")),
    };

    let Ok(mut st) = ctx.watcher_state.lock() else {
        return err_res(tc, "Gagal mengunci HardwareWatcherState.");
    };

    match args.action.trim().to_ascii_lowercase().as_str() {
        "postpone" => {
            st.postpone_battery_warning(Instant::now());
            ok_res(
                tc,
                "State baterai disetel ke Postponed (ditunda 45 menit kecuali turun di bawah 7%)."
                    .to_string(),
            )
        }
        "promise" => {
            st.record_charge_promise();
            ok_res(
                tc,
                format!(
                    "Janji charge pengguna dicatat pada kapasitas {}% (akan ditegur jika turun >= 3%).",
                    st.last_battery_pct
                ),
            )
        }
        "clear" => {
            st.clear_battery_promise();
            ok_res(tc, "State janji baterai direset.".to_string())
        }
        other => err_res(tc, format!("Aksi `{other}` tidak dikenal pada update_battery_promise.")),
    }
}

pub fn intercept_text_tool_calls(raw_text: &str) -> (String, Vec<ToolCall>) {
    let mut cleaned = String::with_capacity(raw_text.len());
    let mut calls = Vec::new();
    let bytes = raw_text.as_bytes();
    let len = raw_text.len();
    let mut i = 0usize;

    while i < len {
        if let Some(rest) = raw_text[i..].strip_prefix("<tool_call>") {
            let close_rel = rest.find("</tool_call>");
            let inner = match close_rel {
                Some(pos) => &rest[..pos],
                None => rest,
            };
            if let Some(tc) = parse_tool_call_inner(inner, calls.len() + 1) {
                calls.push(tc);
                i += "<tool_call>".len()
                    + match close_rel {
                        Some(pos) => pos + "</tool_call>".len(),
                        None => rest.len(),
                    };
                continue;
            }
        }

        if bytes[i] == b'[' {
            if let Some((tc, consumed)) =
                try_parse_bracketed_tool_call(&raw_text[i..], calls.len() + 1)
            {
                calls.push(tc);
                i += consumed;
                continue;
            }
        }

        if bytes[i] == b'{' {
            if let Some((tc, consumed)) =
                try_parse_inline_json_tool_call(&raw_text[i..], calls.len() + 1)
            {
                calls.push(tc);
                i += consumed;
                continue;
            }
        }

        if bytes[i].is_ascii_alphabetic()
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_')
        {
            if let Some((tc, consumed)) = parse_function_call_expr(&raw_text[i..], calls.len() + 1)
            {
                calls.push(tc);
                i += consumed;
                continue;
            }
        }

        if let Some(ch) = raw_text[i..].chars().next() {
            cleaned.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }

    (cleaned.trim().to_string(), calls)
}

fn try_parse_bracketed_tool_call(slice: &str, call_index: usize) -> Option<(ToolCall, usize)> {
    let after_open = slice.strip_prefix('[')?;
    let mut cursor = 1usize;

    let trimmed_start = after_open.trim_start();
    cursor += after_open.len() - trimmed_start.len();

    let candidate_after_label = if let Some(colon_idx) = trimmed_start.find(':') {
        let label = trimmed_start[..colon_idx].trim();
        let paren_before_colon = trimmed_start[..colon_idx].contains('(');
        if !paren_before_colon && is_pseudo_tool_label(label) {
            let after_colon = &trimmed_start[colon_idx + 1..];
            let trimmed_post = after_colon.trim_start();
            cursor += colon_idx + 1 + (after_colon.len() - trimmed_post.len());
            trimmed_post
        } else {
            trimmed_start
        }
    } else {
        trimmed_start
    };

    let (tc, fn_consumed) = parse_function_call_expr(candidate_after_label, call_index)?;
    cursor += fn_consumed;

    let tail = &slice[cursor..];
    let tail_trimmed = tail.trim_start();
    let ws_len = tail.len() - tail_trimmed.len();
    if tail_trimmed.starts_with(']') {
        cursor += ws_len + 1;
    }

    Some((tc, cursor))
}

fn is_pseudo_tool_label(label: &str) -> bool {
    let up = label.trim().to_ascii_uppercase();
    if up.is_empty() || up.len() > 36 {
        return false;
    }
    up.contains("TOOL")
        || up.contains("MEMANGGIL")
        || up.contains("PANGGIL")
        || up.contains("CALL")
        || up.contains("FUNCTION")
        || up.contains("EXEC")
        || up.contains("AKSI")
}

fn parse_tool_call_inner(inner: &str, call_index: usize) -> Option<ToolCall> {
    let trimmed = inner.trim();
    if trimmed.starts_with('{') {
        if let Some((tc, _)) = try_parse_inline_json_tool_call(trimmed, call_index) {
            return Some(tc);
        }
    }
    parse_function_call_expr(trimmed, call_index).map(|(tc, _)| tc)
}

fn try_parse_inline_json_tool_call(slice: &str, call_index: usize) -> Option<(ToolCall, usize)> {
    let end = find_balanced_delimiter(slice, '{', '}')?;
    let json_slice = &slice[..=end];
    let val: serde_json::Value = serde_json::from_str(json_slice).ok()?;
    let name = val.get("name")?.as_str()?.trim();
    if !is_known_tool_name(name) {
        return None;
    }
    let args_val = val
        .get("arguments")
        .or_else(|| val.get("parameters"))
        .or_else(|| val.get("input"))?;
    let arguments = match args_val {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).ok()?,
    };
    Some((
        ToolCall {
            id: format!("fallback_call_{call_index}"),
            name: name.to_string(),
            arguments,
        },
        end + 1,
    ))
}

fn parse_function_call_expr(slice: &str, call_index: usize) -> Option<(ToolCall, usize)> {
    let open_paren = slice.find('(')?;
    let name = slice[..open_paren].trim();
    if !is_known_tool_name(name) {
        return None;
    }

    let paren_slice = &slice[open_paren..];
    let close_rel = find_balanced_delimiter(paren_slice, '(', ')')?;
    let args_body = paren_slice[1..close_rel].trim();
    let arguments = parse_call_arguments_to_json(name, args_body)?;

    Some((
        ToolCall {
            id: format!("fallback_call_{call_index}"),
            name: name.to_string(),
            arguments,
        },
        open_paren + close_rel + 1,
    ))
}

fn find_balanced_delimiter(slice: &str, open_ch: char, close_ch: char) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (byte_idx, ch) in slice.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }

        if ch == '"' || ch == '\'' {
            quote = Some(ch);
        } else if ch == open_ch {
            depth += 1;
        } else if ch == close_ch {
            if depth == 0 {
                return None;
            }
            depth -= 1;
            if depth == 0 {
                return Some(byte_idx);
            }
        }
    }
    None
}

fn parse_call_arguments_to_json(tool_name: &str, args_body: &str) -> Option<String> {
    let trimmed = args_body.trim();
    if trimmed.is_empty() {
        return Some("{}".to_string());
    }

    if trimmed.starts_with('{') && serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }

    let parts = split_top_level_commas(trimmed);
    let mut map = serde_json::Map::new();

    for part in &parts {
        let piece = part.trim();
        if piece.is_empty() {
            continue;
        }

        if let Some(eq_idx) = find_top_level_equals(piece) {
            let raw_key = piece[..eq_idx].trim().trim_matches(|c| c == '"' || c == '\'');
            let raw_val = piece[eq_idx + 1..].trim();
            if !raw_key.is_empty() {
                map.insert(raw_key.to_string(), parse_scalar_value(raw_val));
            }
        } else if parts.len() == 1 {
            let primary_key = primary_arg_key_for_tool(tool_name)?;
            map.insert(primary_key.to_string(), parse_scalar_value(piece));
        }
    }

    if map.is_empty() {
        return None;
    }

    serde_json::to_string(&serde_json::Value::Object(map)).ok()
}

fn primary_arg_key_for_tool(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        RUN_SHELL_COMMAND_NAME | LAUNCH_GUI_APP_NAME => Some("command"),
        READ_FILE_NAME | ROLLBACK_CONFIG_NAME | MANAGE_SPREADSHEET_NAME | MANAGE_DOCUMENT_NAME => {
            Some("path")
        }
        OPEN_MEDIA_NAME | OPEN_BROWSER_SEARCH_NAME | QUICK_SEARCH_NAME | FIND_PACKAGE_OWNER_NAME => {
            Some("query")
        }
        CONTROL_MEDIA_NAME | MANAGE_WORKSPACE_NAME | MANAGE_WINDOW_NAME | MANAGE_SCRATCHPAD_NAME
        | UPDATE_BATTERY_PROMISE_NAME | MANAGE_ALARM_NAME => Some("action"),
        _ => None,
    }
}

fn split_top_level_commas(input: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut depth = 0usize;

    for (idx, ch) in input.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }

        match ch {
            '"' | '\'' => quote = Some(ch),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(&input[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    out.push(&input[start..]);
    out
}

fn find_top_level_equals(piece: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (idx, ch) in piece.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '=' => return Some(idx),
            _ => {}
        }
    }
    None
}

fn parse_scalar_value(raw: &str) -> serde_json::Value {
    let s = raw.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        let inner = &s[1..s.len() - 1];
        return serde_json::Value::String(unescape_quoted_string(inner));
    }
    if s.eq_ignore_ascii_case("true") {
        return serde_json::Value::Bool(true);
    }
    if s.eq_ignore_ascii_case("false") {
        return serde_json::Value::Bool(false);
    }
    if s.eq_ignore_ascii_case("null") || s.eq_ignore_ascii_case("none") {
        return serde_json::Value::Null;
    }
    if let Ok(n) = s.parse::<i64>() {
        return serde_json::Value::Number(n.into());
    }
    serde_json::Value::String(s.to_string())
}

fn unescape_quoted_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\'') => out.push('\''),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_paths(tag: &str) -> Paths {
        let dir = env::temp_dir().join(format!("waish-runner-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Paths::from_dir(dir)
    }

    #[test]
    fn renders_action_preview_box_with_required_fields() {
        let args = ShellCommandArgs {
            command: "sudo pacman -S neovim".to_string(),
            intent: "Install neovim".to_string(),
            requires_root: true,
        };
        let rendered_en = render_action_preview_box(&args, WizardLang::En);
        assert!(rendered_en.contains("Action Details"));
        assert!(rendered_en.contains("Command : sudo pacman -S neovim"));
        assert!(rendered_en.contains("Intent  : Install neovim"));
        assert!(rendered_en.contains("Root    : yes"));

        let rendered_id = render_action_preview_box(&args, WizardLang::Id);
        assert!(rendered_id.contains("Detail Aksi"));
        assert!(rendered_id.contains("Command  : sudo pacman -S neovim"));
        assert!(rendered_id.contains("Tujuan   : Install neovim"));
        assert!(rendered_id.contains("Root     : ya"));

        let rendered_ja = render_action_preview_box(&args, WizardLang::Ja);
        assert!(rendered_ja.contains("アクション詳細"));
        assert!(rendered_ja.contains("コマンド : sudo pacman -S neovim"));
        assert!(rendered_ja.contains("目的     : Install neovim"));
        assert!(rendered_ja.contains("ルート   : はい"));
    }

    #[test]
    fn classifies_readonly_vs_mutating_commands() {
        assert!(is_readonly_command("ls -la", false));
        assert!(is_readonly_command("pacman -Qi neovim", false));
        assert!(is_readonly_command("git status", false));
        assert!(!is_readonly_command("pacman -S neovim", false));
        assert!(!is_readonly_command("ls -la > out.txt", false));
        assert!(!is_readonly_command("ls -la", true));
    }

    #[tokio::test]
    async fn executes_safe_tool_call_end_to_end() {
        let paths = test_paths("shell");
        let settings = Settings::default();
        let shadow = ShadowGit::new(paths.shadow_repo.clone());
        let watcher = Arc::new(Mutex::new(HardwareWatcherState::default()));
        let ws_state = Arc::new(SessionWorkspaceState::new());
        let pending = Arc::new(Mutex::new(PendingAction::None));
        let kaomoji_engine = Arc::new(Mutex::new(KaomojiEngine::with_seed(1)));
        let ctx = ToolExecutionContext {
            settings: &settings,
            paths: &paths,
            shadow_git: &shadow,
            watcher_state: watcher,
            workspace_state: ws_state,
            pending_action: pending,
            kaomoji_engine,
            show_progress: false,
        };

        let tc = ToolCall {
            id: "call_test_1".to_string(),
            name: RUN_SHELL_COMMAND_NAME.to_string(),
            arguments: r#"{"command":"printf 'sukses_tool_runner\n'","intent":"Tes runner","requires_root":false}"#.to_string(),
        };
        let result = execute_tool_call(&tc, &ctx).await;
        assert!(result.succeeded);
        assert!(result.output_for_llm.contains("sukses_tool_runner"));
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[tokio::test]
    async fn dispatches_scratchpad_and_battery_promise_tools() {
        let paths = test_paths("scratch_bat");
        let settings = Settings::default();
        let shadow = ShadowGit::new(paths.shadow_repo.clone());
        let watcher = Arc::new(Mutex::new(HardwareWatcherState {
            last_battery_pct: 17,
            last_status: "Discharging".to_string(),
            ..HardwareWatcherState::default()
        }));
        let ws_state = Arc::new(SessionWorkspaceState::new());
        let pending = Arc::new(Mutex::new(PendingAction::None));
        let kaomoji_engine = Arc::new(Mutex::new(KaomojiEngine::with_seed(2)));
        let ctx = ToolExecutionContext {
            settings: &settings,
            paths: &paths,
            shadow_git: &shadow,
            watcher_state: Arc::clone(&watcher),
            workspace_state: ws_state,
            pending_action: pending,
            kaomoji_engine,
            show_progress: false,
        };

        let tc_add = ToolCall {
            id: "c_pad".to_string(),
            name: MANAGE_SCRATCHPAD_NAME.to_string(),
            arguments: r#"{"action":"add","tag":"dev","content":"server port 3000"}"#.to_string(),
        };
        let r_add = execute_tool_call(&tc_add, &ctx).await;
        assert!(r_add.succeeded);
        assert!(r_add.output_for_llm.contains("port 3000"));

        let tc_bat = ToolCall {
            id: "c_bat".to_string(),
            name: UPDATE_BATTERY_PROMISE_NAME.to_string(),
            arguments: r#"{"action":"promise"}"#.to_string(),
        };
        let r_bat = execute_tool_call(&tc_bat, &ctx).await;
        assert!(r_bat.succeeded);
        assert!(r_bat.output_for_llm.contains("17%"));

        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[tokio::test]
    async fn git_shield_blocks_commit_with_staged_secret_end_to_end() {
        let paths = test_paths("git_shield");
        let repo = paths.dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(status.success());

        std::fs::write(
            repo.join("leak.rs"),
            concat!(
                "const KEY: &str = \"sk-ant-api03-",
                "1234567890abcdefghijklmnopqrstuvwxyz\";\n"
            ),
        )
        .unwrap();
        let add_ok = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["add", "leak.rs"])
            .status()
            .unwrap();
        assert!(add_ok.success());

        let settings = Settings::default();
        let shadow = ShadowGit::new(paths.shadow_repo.clone());
        let watcher = Arc::new(Mutex::new(HardwareWatcherState::default()));
        let ws_state = Arc::new(SessionWorkspaceState::new());
        let pending = Arc::new(Mutex::new(PendingAction::None));
        let kaomoji_engine = Arc::new(Mutex::new(KaomojiEngine::with_seed(1)));
        let ctx = ToolExecutionContext {
            settings: &settings,
            paths: &paths,
            shadow_git: &shadow,
            watcher_state: watcher,
            workspace_state: ws_state,
            pending_action: pending,
            kaomoji_engine,
            show_progress: false,
        };

        let args_json = serde_json::json!({
            "command": format!("git -C {} commit -m 'accidental leak'", repo.display()),
            "intent": "Commit kode",
            "requires_root": false
        })
        .to_string();

        let tc = ToolCall {
            id: "call_git_shield".to_string(),
            name: RUN_SHELL_COMMAND_NAME.to_string(),
            arguments: args_json,
        };

        let res = execute_tool_call(&tc, &ctx).await;
        assert!(!res.succeeded);
        assert!(res.output_for_llm.contains("blocked_by_git_shield"));
        assert!(res.output_for_llm.contains("Anthropic API Key"));

        let _ = std::fs::remove_dir_all(&paths.dir);
        let _ = PathBuf::from(".");
    }

    #[test]
    fn intercepts_hermes_bracketed_xml_and_json_fallback_tool_calls() {
        let hermes_raw = "Oki, aku update sekarang yaa ( •̀ ω •́ )✧\n\
            [MEMANGGIL TOOLS: run_shell_command(command=\"yay -Syu\", intent=\"Memperbarui sistem\", requires_root=true)]";
        let (clean, calls) = intercept_text_tool_calls(hermes_raw);
        assert_eq!(clean, "Oki, aku update sekarang yaa ( •̀ ω •́ )✧");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, RUN_SHELL_COMMAND_NAME);
        let parsed = ShellCommandArgs::from_json_str(&calls[0].arguments).unwrap();
        assert_eq!(parsed.command, "yay -Syu");
        assert_eq!(parsed.intent, "Memperbarui sistem");
        assert!(parsed.requires_root);

        let id_raw = "[Panggil tool: run_shell_command(command=\"yay -Syu\")]";
        let (clean2, calls2) = intercept_text_tool_calls(id_raw);
        assert!(clean2.is_empty());
        assert_eq!(calls2.len(), 1);
        let parsed2 = ShellCommandArgs::from_json_str(&calls2[0].arguments).unwrap();
        assert_eq!(parsed2.command, "yay -Syu");
        assert!(parsed2.requires_root);

        let xml_raw = "<tool_call>{\"name\":\"quick_search\",\"arguments\":{\"query\":\"arch linux wiki\"}}</tool_call>";
        let (clean3, calls3) = intercept_text_tool_calls(xml_raw);
        assert!(clean3.is_empty());
        assert_eq!(calls3.len(), 1);
        assert_eq!(calls3[0].name, QUICK_SEARCH_NAME);

        let bare_raw = "Siap! alice@waish -> run_shell_command(command=\"yay -Syu\", requires_root=true)";
        let (_clean_bare, calls_bare) = intercept_text_tool_calls(bare_raw);
        assert_eq!(calls_bare.len(), 1);
        assert_eq!(calls_bare[0].name, RUN_SHELL_COMMAND_NAME);
        let parsed_bare = ShellCommandArgs::from_json_str(&calls_bare[0].arguments).unwrap();
        assert_eq!(parsed_bare.command, "yay -Syu");
        assert!(parsed_bare.requires_root);

        let normal = "Catatan [info] fungsi foo(bar=1) tidak boleh dicegat.";
        let (clean4, calls4) = intercept_text_tool_calls(normal);
        assert_eq!(clean4, normal);
        assert!(calls4.is_empty());
    }

    #[tokio::test]
    async fn root_shell_command_requires_confirmation_gatekeeper_in_non_tty() {
        let paths = test_paths("root_gatekeeper");
        let settings = Settings::default();
        let shadow = ShadowGit::new(paths.shadow_repo.clone());
        let watcher = Arc::new(Mutex::new(HardwareWatcherState::default()));
        let ws_state = Arc::new(SessionWorkspaceState::new());
        let pending = Arc::new(Mutex::new(PendingAction::None));
        let kaomoji_engine = Arc::new(Mutex::new(KaomojiEngine::with_seed(5)));
        let ctx = ToolExecutionContext {
            settings: &settings,
            paths: &paths,
            shadow_git: &shadow,
            watcher_state: watcher,
            workspace_state: ws_state,
            pending_action: pending,
            kaomoji_engine,
            show_progress: false,
        };

        let tc = ToolCall {
            id: "call_root_update".to_string(),
            name: RUN_SHELL_COMMAND_NAME.to_string(),
            arguments: r#"{"command":"rm /alice.txt","intent":"Hapus file root","requires_root":true}"#
                .to_string(),
        };
        let res = execute_tool_call(&tc, &ctx).await;
        assert!(!res.succeeded);
        assert!(res.output_for_llm.contains("cancelled_by_user"));
        assert!(res.output_for_llm.contains("sudo rm /alice.txt"));
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn auto_prefixes_sudo_on_root_commands_except_sudo_and_aur_helpers() {
        assert_eq!(
            ensure_root_sudo_prefix("rm /alice.txt", true),
            "sudo rm /alice.txt"
        );
        assert_eq!(
            ensure_root_sudo_prefix("pacman -Syu", true),
            "sudo pacman -Syu"
        );
        assert_eq!(
            ensure_root_sudo_prefix("sudo rm /alice.txt", true),
            "sudo rm /alice.txt"
        );
        assert_eq!(ensure_root_sudo_prefix("yay -Syu", true), "yay -Syu");
        assert_eq!(ensure_root_sudo_prefix("paru -Syu", true), "paru -Syu");
        assert_eq!(ensure_root_sudo_prefix("rm ./tmp.txt", false), "rm ./tmp.txt");
    }

    #[tokio::test]
    async fn shell_runner_uses_posix_even_when_configured_shell_is_fish() {
        let paths = test_paths("posix_fish");
        let mut settings = Settings::default();
        settings.tools.shell_path = "/usr/bin/fish".to_string();
        let shadow = ShadowGit::new(paths.shadow_repo.clone());
        let watcher = Arc::new(Mutex::new(HardwareWatcherState::default()));
        let ws_state = Arc::new(SessionWorkspaceState::new());
        let pending = Arc::new(Mutex::new(PendingAction::None));
        let kaomoji_engine = Arc::new(Mutex::new(KaomojiEngine::with_seed(13)));
        let ctx = ToolExecutionContext {
            settings: &settings,
            paths: &paths,
            shadow_git: &shadow,
            watcher_state: watcher,
            workspace_state: ws_state,
            pending_action: pending,
            kaomoji_engine,
            show_progress: false,
        };

        let tc = ToolCall {
            id: "call_posix_fish".to_string(),
            name: RUN_SHELL_COMMAND_NAME.to_string(),
            arguments: serde_json::json!({
                "command": "VAR=$(printf 'posix_ok') && test -n \"$HOME\" && test -n \"$PATH\" && printf '%s|%s\\n' \"$VAR\" \"$USER\"",
                "intent": "Verifikasi shell POSIX pada lingkungan Fish",
                "requires_root": false
            })
            .to_string(),
        };
        let res = execute_tool_call(&tc, &ctx).await;
        assert!(res.succeeded, "POSIX command failed: {}", res.output_for_llm);
        assert!(res.output_for_llm.contains("posix_ok|"));
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[tokio::test]
    #[ignore]
    async fn live_four_arch_bugs_proof() {
        let paths = test_paths("live_4bugs");
        let mut settings = Settings::default();
        settings.tools.shell_path = "/usr/bin/fish".to_string();
        let shadow = ShadowGit::new(paths.shadow_repo.clone());
        let watcher = Arc::new(Mutex::new(HardwareWatcherState::default()));
        let ws_state = Arc::new(SessionWorkspaceState::new());
        let pending = Arc::new(Mutex::new(PendingAction::None));
        let kaomoji_engine = Arc::new(Mutex::new(KaomojiEngine::with_seed(17)));
        let ctx = ToolExecutionContext {
            settings: &settings,
            paths: &paths,
            shadow_git: &shadow,
            watcher_state: watcher,
            workspace_state: ws_state,
            pending_action: pending,
            kaomoji_engine,
            show_progress: false,
        };

        let posix_tc = ToolCall {
            id: "live_posix".to_string(),
            name: RUN_SHELL_COMMAND_NAME.to_string(),
            arguments: serde_json::json!({
                "command": "VAL=$(echo 'posix_live_ok') && test -n \"$HOME\" && test -n \"$PATH\" && test -n \"$WAYLAND_DISPLAY\" && test -n \"$XDG_RUNTIME_DIR\" && echo \"$VAL:$USER:$WAYLAND_DISPLAY\"",
                "intent": "Uji POSIX runner dan pewarisan env",
                "requires_root": false
            })
            .to_string(),
        };
        let posix_res = execute_tool_call(&posix_tc, &ctx).await;
        println!("=== LIVE 1: POSIX SHELL (FISH ENV) ===\n{}\n", posix_res.output_for_llm);
        assert!(posix_res.succeeded, "POSIX live failed: {}", posix_res.output_for_llm);
        assert!(posix_res.output_for_llm.contains("posix_live_ok:"));

        let clients_initial = crate::tools::workspace_tracker::query_hypr_clients()
            .await
            .unwrap_or_default();
        let spotify_already_open =
            !crate::tools::workspace_tracker::find_spotify_clients(&clients_initial).is_empty();

        if !spotify_already_open {
            crate::tools::workspace_tracker::hypr_exec("spotify", Some(5))
                .await
                .expect("launch spotify on ws 5 for pre-open test");
            for _ in 0..20 {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                let now = crate::tools::workspace_tracker::query_hypr_clients()
                    .await
                    .unwrap_or_default();
                if !crate::tools::workspace_tracker::find_spotify_clients(&now).is_empty() {
                    break;
                }
            }
            let _ = crate::tools::media::wait_for_mpris_ready("spotify", 8, 500).await;
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        }

        let sp_tc = ToolCall {
            id: "live_spotify_open".to_string(),
            name: OPEN_MEDIA_NAME.to_string(),
            arguments: r#"{"query":"Hey Jude The Beatles","platform":"spotify","workspace":5}"#
                .to_string(),
        };
        let sp_res = execute_tool_call(&sp_tc, &ctx).await;
        println!("=== LIVE 2: SPOTIFY ALREADY OPEN ===\n{}\n", sp_res.output_for_llm);
        assert!(sp_res.succeeded, "open_media Spotify failed: {}", sp_res.output_for_llm);
        assert!(
            !sp_res.output_for_llm.contains("waiting_for_login"),
            "Spotify MUST NOT return waiting_for_login when already open: {}",
            sp_res.output_for_llm
        );
        assert!(
            sp_res.output_for_llm.contains("status: playing"),
            "Spotify should return status: playing: {}",
            sp_res.output_for_llm
        );
        assert!(
            sp_res.output_for_llm.contains("spotify_window_detected: true"),
            "Spotify window must be detected via hyprctl clients -j: {}",
            sp_res.output_for_llm
        );

        let sp_snap = crate::tools::media::query_mpris_snapshot(Some("spotify")).await;
        println!("=== LIVE 2 MPRIS SNAPSHOT ===\n{sp_snap:?}\n");
        assert!(
            sp_snap.as_ref().is_some_and(|s| s.is_playing),
            "Spotify MPRIS must actually be Playing: {sp_snap:?}"
        );
        let pause_sp = ToolCall {
            id: "live_pause_sp".to_string(),
            name: CONTROL_MEDIA_NAME.to_string(),
            arguments: r#"{"action":"pause","player":"spotify"}"#.to_string(),
        };
        let _ = execute_tool_call(&pause_sp, &ctx).await;
        if !spotify_already_open {
            let close_sp = ToolCall {
                id: "live_close_sp".to_string(),
                name: MANAGE_WINDOW_NAME.to_string(),
                arguments: r#"{"action":"close","workspace":5,"window_class":"spotify"}"#
                    .to_string(),
            };
            let _ = execute_tool_call(&close_sp, &ctx).await;
        }

        let yt_tc = ToolCall {
            id: "live_yt_gtavi".to_string(),
            name: OPEN_MEDIA_NAME.to_string(),
            arguments: r#"{"query":"GTA VI gameplay trailer","platform":"youtube","workspace":5}"#
                .to_string(),
        };
        let yt_res = execute_tool_call(&yt_tc, &ctx).await;
        println!("=== LIVE 3: YOUTUBE GTA VI ON WS 5 ===\n{}\n", yt_res.output_for_llm);
        assert!(yt_res.succeeded, "open_media YouTube failed: {}", yt_res.output_for_llm);
        assert!(
            yt_res.output_for_llm.contains("status: playing_youtube"),
            "expected direct video resolution: {}",
            yt_res.output_for_llm
        );
        assert!(
            yt_res.output_for_llm.contains("https://www.youtube.com/watch?v="),
            "expected watch?v= direct URL: {}",
            yt_res.output_for_llm
        );
        assert!(
            yt_res.output_for_llm.contains("&autoplay=1"),
            "expected &autoplay=1 parameter: {}",
            yt_res.output_for_llm
        );
        assert!(
            yt_res.output_for_llm.contains("--new-window"),
            "expected --new-window flag: {}",
            yt_res.output_for_llm
        );

        tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
        let clients_ws5 = crate::tools::workspace_tracker::query_hypr_clients()
            .await
            .expect("query hypr clients after YouTube launch");
        let ff_on_ws5: Vec<_> = clients_ws5
            .iter()
            .filter(|c| {
                c.workspace.id == 5 && crate::tools::workspace_tracker::is_known_browser_client(c)
            })
            .collect();
        println!("=== LIVE 3: BROWSER WINDOWS ON WS 5 ===\n{ff_on_ws5:#?}\n");
        assert!(
            !ff_on_ws5.is_empty(),
            "Firefox MUST be open on Workspace 5, not hijacked to Workspace 1!"
        );

        let close_ws5 = ToolCall {
            id: "live_close_yt".to_string(),
            name: MANAGE_WINDOW_NAME.to_string(),
            arguments: r#"{"action":"close","workspace":5,"window_class":"firefox"}"#.to_string(),
        };
        let close_res = execute_tool_call(&close_ws5, &ctx).await;
        println!("=== LIVE 3 CLEANUP ===\n{}\n", close_res.output_for_llm);
        assert!(close_res.succeeded);
        assert!(close_res.output_for_llm.contains("Berhasil menutup"));

        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[tokio::test]
    #[ignore]
    async fn live_hyprland_field_bug_proof() {
        let paths = test_paths("live_proof");
        let settings = Settings::default();
        let shadow = ShadowGit::new(paths.shadow_repo.clone());
        let watcher = Arc::new(Mutex::new(HardwareWatcherState::default()));
        let ws_state = Arc::new(SessionWorkspaceState::new());
        let pending = Arc::new(Mutex::new(PendingAction::None));
        let kaomoji_engine = Arc::new(Mutex::new(KaomojiEngine::with_seed(11)));
        let ctx = ToolExecutionContext {
            settings: &settings,
            paths: &paths,
            shadow_git: &shadow,
            watcher_state: watcher,
            workspace_state: ws_state,
            pending_action: pending,
            kaomoji_engine,
            show_progress: false,
        };

        let qs = ToolCall {
            id: "live_qs".to_string(),
            name: QUICK_SEARCH_NAME.to_string(),
            arguments: r#"{"query":"Arch Linux latest news","max_results":3}"#.to_string(),
        };
        let qs_res = execute_tool_call(&qs, &ctx).await;
        println!("=== LIVE quick_search ===\n{}\n", qs_res.output_for_llm);
        assert!(qs_res.succeeded, "quick_search failed: {}", qs_res.output_for_llm);
        assert!(
            qs_res.output_for_llm.contains("status: success"),
            "expected live snippets from quick_search: {}",
            qs_res.output_for_llm
        );

        let cm = ToolCall {
            id: "live_cm".to_string(),
            name: CONTROL_MEDIA_NAME.to_string(),
            arguments: r#"{"action":"get_info"}"#.to_string(),
        };
        let cm_res = execute_tool_call(&cm, &ctx).await;
        println!("=== LIVE control_media ===\n{}\n", cm_res.output_for_llm);
        assert!(cm_res.succeeded, "control_media get_info must not fail fatally");

        crate::tools::workspace_tracker::hypr_exec(
            "kitty --title waish-live-proof -o confirm_os_window_close=0 sh -c 'sleep 25'",
            Some(9),
        )
        .await
        .expect("hypr_exec on ws 9 must succeed without syntax error");
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

        let close_kitty = ToolCall {
            id: "live_close_kitty".to_string(),
            name: MANAGE_WINDOW_NAME.to_string(),
            arguments: r#"{"action":"close","workspace":9,"target":"waish-live-proof"}"#
                .to_string(),
        };
        let ck_res = execute_tool_call(&close_kitty, &ctx).await;
        println!("=== LIVE manage_window close (kitty) ===\n{}\n", ck_res.output_for_llm);
        assert!(ck_res.succeeded, "manage_window close failed: {}", ck_res.output_for_llm);
        assert!(ck_res.output_for_llm.contains("Berhasil menutup"));

        let obs = ToolCall {
            id: "live_obs".to_string(),
            name: OPEN_BROWSER_SEARCH_NAME.to_string(),
            arguments: r#"{"query":"Arch Linux latest","engine":"google","workspace":9}"#
                .to_string(),
        };
        let obs_res = execute_tool_call(&obs, &ctx).await;
        println!("=== LIVE open_browser_search ===\n{}\n", obs_res.output_for_llm);
        assert!(obs_res.succeeded, "open_browser_search failed: {}", obs_res.output_for_llm);
        assert!(!obs_res.output_for_llm.contains("']' expected"));
        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;

        let close_ws9_1 = ToolCall {
            id: "live_close_obs".to_string(),
            name: MANAGE_WINDOW_NAME.to_string(),
            arguments: r#"{"action":"close","workspace":9}"#.to_string(),
        };
        let c1_res = execute_tool_call(&close_ws9_1, &ctx).await;
        println!("=== LIVE close after open_browser_search ===\n{}\n", c1_res.output_for_llm);
        assert!(c1_res.succeeded, "close ws 9 after open_browser_search failed: {}", c1_res.output_for_llm);
        assert!(
            c1_res.output_for_llm.contains("Berhasil menutup"),
            "expected ws 9 browser search window to be closed: {}",
            c1_res.output_for_llm
        );

        let om = ToolCall {
            id: "live_om".to_string(),
            name: OPEN_MEDIA_NAME.to_string(),
            arguments: r#"{"query":"Hey Jude The Beatles","platform":"youtube","workspace":9}"#
                .to_string(),
        };
        let om_res = execute_tool_call(&om, &ctx).await;
        println!("=== LIVE open_media (YouTube) ===\n{}\n", om_res.output_for_llm);
        assert!(om_res.succeeded, "open_media YouTube failed: {}", om_res.output_for_llm);
        assert!(!om_res.output_for_llm.contains("']' expected"));
        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;

        let close_ws9_2 = ToolCall {
            id: "live_close_om".to_string(),
            name: MANAGE_WINDOW_NAME.to_string(),
            arguments: r#"{"action":"close","workspace":9}"#.to_string(),
        };
        let c2_res = execute_tool_call(&close_ws9_2, &ctx).await;
        println!("=== LIVE close after open_media ===\n{}\n", c2_res.output_for_llm);
        assert!(c2_res.succeeded, "close ws 9 after open_media failed: {}", c2_res.output_for_llm);
        assert!(
            c2_res.output_for_llm.contains("Berhasil menutup"),
            "expected ws 9 media window to be closed: {}",
            c2_res.output_for_llm
        );

        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn defensive_path_rewriter_rewrites_companion_home_and_expands_tilde_tokens() {
        assert_eq!(
            rewrite_companion_home_and_tilde_with_env(
                "/home/steve/file.txt",
                "steve",
                "/home/archdev"
            ),
            "/home/archdev/file.txt"
        );

        let host = resolve_host_identity();
        if !host.real_home.eq_ignore_ascii_case("/home/steve") {
            let mut settings = Settings::default();
            settings.general.user_real_name = "steve".to_string();
            let rewritten_host = rewrite_shell_command_paths("cat /home/steve/file.txt", &settings);
            assert_eq!(rewritten_host, format!("cat {}/file.txt", host.real_home));
            assert!(
                recent_defensive_interceptions()
                    .iter()
                    .any(|entry| entry.contains("[path_rewrite]")),
                "interception ring buffer should record path_rewrite event"
            );
        }

        let cmd =
            r#"mkdir -p "/home/steve/Laporan Bulanan" && cp ~/template.xlsx --out=~/hasil.xlsx"#;
        assert_eq!(
            rewrite_companion_home_and_tilde_with_env(cmd, "steve", "/home/archdev"),
            r#"mkdir -p "/home/archdev/Laporan Bulanan" && cp /home/archdev/template.xlsx --out=/home/archdev/hasil.xlsx"#
        );

        assert_eq!(
            rewrite_companion_home_and_tilde_with_env(
                "ls /home/steven/file.txt",
                "steve",
                "/home/archdev"
            ),
            "ls /home/steven/file.txt"
        );
    }

    #[test]
    fn pkill_armor_blocks_self_kill_and_applies_bracket_trick_plus_isolated_pgroup() {
        let fake_pid = 42424u32;

        assert!(armor_process_kill_command_with_pid("pkill -f waish", fake_pid, true).is_err());
        assert!(armor_process_kill_command_with_pid("killall waish", fake_pid, true).is_err());
        assert!(armor_process_kill_command_with_pid("kill -9 42424", fake_pid, true).is_err());
        assert!(armor_process_kill_command_with_pid("pkill -P 42424", fake_pid, true).is_err());
        assert!(armor_process_kill_command_with_pid("killall bash", fake_pid, true).is_err());

        assert_eq!(
            armor_process_kill_command_with_pid("pkill -f oneko", fake_pid, true).unwrap(),
            "pkill --ignore-ancestors -f '[o]neko'"
        );
        assert_eq!(
            armor_process_kill_command_with_pid("pkill -f Antigravity", fake_pid, true).unwrap(),
            "pkill --ignore-ancestors -f '[A]ntigravity'"
        );
        assert_eq!(
            armor_process_kill_command_with_pid("pkill -f Antigravity", fake_pid, false).unwrap(),
            "pkill -f '[A]ntigravity'"
        );

        let armored_live = armor_process_kill_command("pkill -f nonexistent_target_process_991")
            .unwrap_or_else(|_| {
                armor_process_kill_command_with_pid(
                    "pkill -f nonexistent_target_process_991",
                    std::process::id(),
                    false,
                )
                .unwrap()
            });
        let subshell_cmd = format!("{armored_live}; printf 'SUBSHELL_SURVIVED:%s\\n' \"$(ps -o pgid= -p $$)\"");
        let mut cmd = Command::new(posix_shell_for_runner());
        cmd.args(["-c", &subshell_cmd])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let child = spawn_in_isolated_process_group(&mut cmd).expect("spawn isolated pgroup");
        let child_pid = child.id() as i32;
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "subshell must survive pkill -f without friendly fire!"
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("SUBSHELL_SURVIVED:"));
        let pgid_part = stdout
            .trim()
            .strip_prefix("SUBSHELL_SURVIVED:")
            .unwrap()
            .trim();
        let child_pgid: i32 = pgid_part.parse().expect("valid pgid");
        let self_pgid = unsafe { libc::getpgrp() };
        assert_eq!(child_pgid, child_pid);
        assert_ne!(child_pgid, self_pgid);
    }

    #[test]
    fn intercepts_adhoc_tmp_python_office_scripts_and_wraps_xwayland_apps() {
        assert!(is_adhoc_tmp_office_python_command(
            "cat << 'EOF' > /tmp/gen.py\nimport openpyxl\nwb.save('data.xlsx')\nEOF\npython3 /tmp/gen.py"
        ));
        assert!(is_adhoc_tmp_office_python_command(
            "python3 -c \"import docx; d = docx.Document(); d.save('out.docx')\""
        ));
        assert!(is_adhoc_tmp_office_python_write(
            "/tmp/make_sheet.py",
            "import pandas as pd\ndf.to_excel('laporan.xlsx')"
        ));
        assert!(!is_adhoc_tmp_office_python_command("python3 --version"));

        assert_eq!(
            wrap_xwayland_gui_command("oneko -dog &", ":1"),
            "env -u XAUTHORITY DISPLAY=:1 setsid oneko -dog >/dev/null 2>&1 &"
        );
        assert!(is_proc_or_xauth_rummage_command(
            "grep -z DISPLAY /proc/*/environ"
        ));

        assert!(is_background_gui_shell_launch(
            "nohup onlyoffice-desktopeditors ~/jadwal.xlsx >/dev/null 2>&1 &"
        ));
        assert!(is_background_gui_shell_launch(
            "libreoffice ~/laporan.docx &"
        ));
        assert!(!is_background_gui_shell_launch("onlyoffice-desktopeditors --version"));
        assert!(!is_background_gui_shell_launch("pacman -Qi libreoffice-fresh"));

        assert!(is_alarm_filesystem_rummage_command(
            "find / -name '*waish-alarm*' 2>/dev/null"
        ));
        assert!(is_redundant_post_ipc_window_poll(
            "sleep 2 && hyprctl clients | grep -i onlyoffice"
        ));
        assert!(is_redundant_post_ipc_window_poll("pgrep -fa desktopeditors"));
    }

    #[tokio::test]
    async fn blocks_nohup_gui_and_post_ipc_polling_in_dispatch_shell_command() {
        let paths = test_paths("phase3_guards");
        let settings = Settings::default();
        let shadow = ShadowGit::new(paths.shadow_repo.clone());
        let watcher = Arc::new(Mutex::new(HardwareWatcherState::default()));
        let ws_state = Arc::new(SessionWorkspaceState::new());
        let pending = Arc::new(Mutex::new(PendingAction::None));
        let kaomoji_engine = Arc::new(Mutex::new(KaomojiEngine::with_seed(9)));
        let ctx = ToolExecutionContext {
            settings: &settings,
            paths: &paths,
            shadow_git: &shadow,
            watcher_state: watcher,
            workspace_state: Arc::clone(&ws_state),
            pending_action: pending,
            kaomoji_engine,
            show_progress: false,
        };

        let nohup_tc = ToolCall {
            id: "c_nohup".to_string(),
            name: RUN_SHELL_COMMAND_NAME.to_string(),
            arguments: r#"{"command":"nohup onlyoffice-desktopeditors ~/tabel.xlsx &","intent":"Buka file","requires_root":false}"#.to_string(),
        };
        let nohup_res = execute_tool_call(&nohup_tc, &ctx).await;
        assert!(!nohup_res.succeeded);
        assert!(nohup_res.output_for_llm.contains("execute_recipe"));
        assert!(nohup_res.output_for_llm.contains("launch_gui_app"));

        ws_state.mark_turn_ipc_verified();
        let poll_tc = ToolCall {
            id: "c_poll".to_string(),
            name: RUN_SHELL_COMMAND_NAME.to_string(),
            arguments: r#"{"command":"sleep 2 && hyprctl clients | grep -i desktopeditors","intent":"Cek jendela","requires_root":false}"#.to_string(),
        };
        let poll_res = execute_tool_call(&poll_tc, &ctx).await;
        assert!(poll_res.succeeded);
        assert!(poll_res.output_for_llm.contains("already_verified_by_ipc"));

        let _ = std::fs::remove_dir_all(&paths.dir);
    }
}
