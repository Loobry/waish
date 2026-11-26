//! Pseudo-Terminal (PTY) Execution Engine.

use std::env;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use portable_pty::{native_pty_system, Child, CommandBuilder, ExitStatus, PtySize};
use regex::Regex;
use unicode_width::UnicodeWidthChar;

use crate::cli::ui::{display_width, Kaomoji};
use crate::config::settings::DEFAULT_BOT_NAME;
use crate::wizard::i18n::WizardLang;

pub const ERROR_TAIL_LINES: usize = 12;
pub const SUCCESS_MAX_LINES: usize = 30;
pub const MAX_TOOL_OUTPUT_CHARS: usize = 2400;
const MAX_CAPTURED_PTY_BYTES: usize = 256 * 1024;
const MAX_UNHANDLED_BYTES: usize = 32 * 1024;
const MAX_CLEAN_OUTPUT_CHARS: usize = 32_768;
const PROMPT_IDLE_DETECT_MS: u64 = 160;
const MAX_PROGRESS_MSG_WIDTH: usize = 54;

pub const INTERACTIVE_YN_PATTERN: &str =
    r"(?i)(\[y/n\]|\(y/n\)|proceed with installation\?|apakah anda yakin|lanjutkan\?)";

pub const INTERACTIVE_SELECT_PATTERN: &str =
    r"(?i)(enter a selection|enter a number|pilih nomor|pilihan \[|[0-9]+-[0-9]+\]:)";

static CHILD_SIGINT_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_child_sigint(_: libc::c_int) {
    CHILD_SIGINT_RECEIVED.store(true, Ordering::SeqCst);
}

/// Interactive prompt detected from the tail of a PTY output stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractivePrompt {
    pub context_lines: Vec<String>,
    pub question_line: String,
}

#[derive(Debug)]
struct SharedPtyBuffer {
    all_bytes: Vec<u8>,
    unhandled_bytes: Vec<u8>,
    last_received: Instant,
}

impl SharedPtyBuffer {
    fn new() -> Self {
        Self {
            all_bytes: Vec::with_capacity(4096),
            unhandled_bytes: Vec::with_capacity(1024),
            last_received: Instant::now(),
        }
    }
}

/// Execution output from a command run inside a PTY.
#[derive(Debug, Clone)]
pub struct PtyExecutionOutput {
    pub exit_code: u32,
    pub succeeded: bool,
    pub interrupted: bool,
    pub elapsed: Duration,
    pub clean_output: String,
}

impl PtyExecutionOutput {
    pub fn format_for_llm(&self, command: &str) -> String {
        if self.interrupted {
            return format!(
                "status: interrupted (SIGINT / Ctrl+C by user)\n\
                 do_not_retry: true\n\
                 command: {command}\n\
                 note: User cancelled the command with Ctrl+C. Acknowledge briefly and DO NOT automatically retry.\n\
                 tail_output:\n{}",
                extract_tail_lines(&self.clean_output, ERROR_TAIL_LINES)
            );
        }

        if self.succeeded {
            let snippet =
                truncate_output(&self.clean_output, SUCCESS_MAX_LINES, MAX_TOOL_OUTPUT_CHARS);
            if snippet.is_empty() {
                format!(
                    "status: success (exit_code: 0, duration_ms: {})\n\
                     command: {command}\n\
                     output: (selesai tanpa output teks)",
                    self.elapsed.as_millis()
                )
            } else {
                format!(
                    "status: success (exit_code: 0, duration_ms: {})\n\
                     command: {command}\n\
                     output:\n{snippet}",
                    self.elapsed.as_millis()
                )
            }
        } else {
            let tail = extract_tail_lines(&self.clean_output, ERROR_TAIL_LINES);
            let detail = if tail.is_empty() {
                "(tidak ada pesan stderr/stdout)".to_string()
            } else {
                tail
            };
            format!(
                "status: error (exit_code: {}, duration_ms: {})\n\
                 command: {command}\n\
                 error_tail:\n{detail}",
                self.exit_code,
                self.elapsed.as_millis()
            )
        }
    }
}

pub fn requires_true_interactive_passthrough(command: &str, requires_root: bool) -> bool {
    if requires_root {
        return true;
    }

    const INTERACTIVE_BINS: &[&str] = &[
        "sudo",
        "doas",
        "su",
        "pkexec",
        "passwd",
        "chsh",
        "visudo",
        "pacman",
        "yay",
        "paru",
        "trizen",
        "pikaur",
        "aura",
        "makepkg",
        "apt",
        "apt-get",
        "dnf",
        "zypper",
        "flatpak",
        "nix-env",
        "vim",
        "nvim",
        "nano",
        "micro",
        "vi",
        "less",
        "more",
        "man",
        "htop",
        "btop",
        "top",
        "ssh",
        "sftp",
        "gpg",
        "systemctl",
    ];

    const WRAPPERS: &[&str] = &[
        "env", "command", "time", "nice", "nohup", "timeout", "stdbuf", "exec",
    ];

    for segment in command.split([';', '|', '&', '\n']) {
        let mut iter = segment.split_whitespace();
        while let Some(raw_tok) = iter.next() {
            let tok = raw_tok.trim_matches(|c| c == '(' || c == ')' || c == '\'' || c == '"');
            if tok.is_empty() {
                continue;
            }
            if tok.contains('=') && !tok.starts_with('/') && !tok.starts_with('-') {
                continue;
            }
            let base = Path::new(tok)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(tok);
            if WRAPPERS.contains(&base) {
                if base == "timeout" {
                    let _ = iter.next();
                }
                continue;
            }
            if INTERACTIVE_BINS.contains(&base) {
                return true;
            }
            break;
        }
    }

    false
}

fn push_bounded_bytes(buf: &mut Vec<u8>, chunk: &[u8], cap: usize) {
    if cap == 0 || chunk.is_empty() {
        return;
    }
    if chunk.len() >= cap {
        buf.clear();
        buf.extend_from_slice(&chunk[chunk.len() - cap..]);
        return;
    }
    let total = buf.len() + chunk.len();
    if total > cap {
        buf.drain(..total - cap);
    }
    buf.extend_from_slice(chunk);
}

fn cap_clean_output(clean: String, max_chars: usize) -> String {
    let count = clean.chars().count();
    if count <= max_chars {
        clean
    } else {
        clean.chars().skip(count - max_chars).collect()
    }
}

#[cfg(test)]
pub fn execute_in_pty(
    command: &str,
    shell_path: &str,
    requires_root: bool,
    show_progress: bool,
) -> crate::Result<PtyExecutionOutput> {
    execute_in_pty_localized(
        command,
        shell_path,
        requires_root,
        show_progress,
        DEFAULT_BOT_NAME,
        WizardLang::En,
    )
}

pub fn execute_in_pty_with_bot(
    command: &str,
    shell_path: &str,
    requires_root: bool,
    show_progress: bool,
    bot_name: &str,
) -> crate::Result<PtyExecutionOutput> {
    execute_in_pty_localized(
        command,
        shell_path,
        requires_root,
        show_progress,
        bot_name,
        WizardLang::En,
    )
}

pub fn execute_in_pty_localized(
    command: &str,
    shell_path: &str,
    requires_root: bool,
    show_progress: bool,
    bot_name: &str,
    lang: WizardLang,
) -> crate::Result<PtyExecutionOutput> {
    if requires_true_interactive_passthrough(command, requires_root) {
        execute_interactive_passthrough(command, shell_path)
    } else {
        execute_buffered_with_progress(command, shell_path, show_progress, bot_name, lang)
    }
}

fn execute_interactive_passthrough(
    command: &str,
    shell_path: &str,
) -> crate::Result<PtyExecutionOutput> {
    let start = Instant::now();
    let pty_system = native_pty_system();
    let mut last_size = current_pty_size();
    let pair = pty_system
        .openpty(last_size)
        .map_err(|e| format!("Failed to open pseudo-terminal (PTY): {e}"))?;

    let _termios_guard = TermiosRestoreGuard::capture_stdin();
    let _sigint_guard = PtySigintGuard::install();

    let effective_shell = resolve_shell(shell_path);
    let mut cmd = CommandBuilder::new(effective_shell);
    apply_inherited_user_env(&mut cmd);
    cmd.arg("-c");
    cmd.arg(command);
    if let Ok(cwd) = env::current_dir() {
        cmd.cwd(cwd);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("Failed to spawn command in PTY: {e}"))?;
    let mut child_guard = ChildReapGuard::new(child);

    // Close slave fd in parent so master receives EOF/EIO immediately on child exit.
    drop(pair.slave);

    let mut pty_writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("Failed to open PTY writer: {e}"))?;
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("Failed to clone PTY reader: {e}"))?;
    let master_fd = pair.master.as_raw_fd();

    let interactive_tty = !cfg!(test) && io::stdin().is_terminal() && io::stdout().is_terminal();
    let raw_mode_guard = if interactive_tty {
        RawModeGuard::enable()
    } else {
        None
    };

    let mut captured_bytes: Vec<u8> = Vec::with_capacity(8192);
    let mut interrupted = false;
    let mut saw_ctrl_c = false;
    let mut ctrl_c_count = 0usize;
    let mut last_ctrl_c: Option<Instant> = None;
    let mut buf = [0u8; 4096];

    let exit_status = if let Some(mfd) = master_fd {
        let mut master_eof = false;
        let stdin_fd = if interactive_tty {
            libc::STDIN_FILENO
        } else {
            -1
        };

        loop {
            if interactive_tty {
                let new_size = current_pty_size();
                if new_size != last_size {
                    let _ = pair.master.resize(new_size);
                    last_size = new_size;
                }
            }

            if CHILD_SIGINT_RECEIVED.swap(false, Ordering::SeqCst) {
                saw_ctrl_c = true;
                ctrl_c_count += 1;
                last_ctrl_c = Some(Instant::now());
                let _ = pty_writer.write_all(&[0x03]);
                let _ = pty_writer.flush();
                if ctrl_c_count >= 2 {
                    interrupted = true;
                    break child_guard.kill_and_reap();
                }
            }

            let mut fds = [
                libc::pollfd {
                    fd: if master_eof { -1 } else { mfd },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: stdin_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];

            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 20) };

            if rc > 0 {
                if (fds[0].revents & libc::POLLIN) != 0 {
                    match reader.read(&mut buf) {
                        Ok(0) => master_eof = true,
                        Ok(n) => {
                            push_bounded_bytes(&mut captured_bytes, &buf[..n], MAX_CAPTURED_PTY_BYTES);
                            if !cfg!(test) {
                                let mut out = io::stdout().lock();
                                let _ = out.write_all(&buf[..n]);
                                let _ = out.flush();
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => master_eof = true,
                    }
                } else if (fds[0].revents & (libc::POLLHUP | libc::POLLERR)) != 0 {
                    master_eof = true;
                }

                if stdin_fd >= 0 && (fds[1].revents & libc::POLLIN) != 0 {
                    let n = unsafe {
                        libc::read(
                            libc::STDIN_FILENO,
                            buf.as_mut_ptr().cast(),
                            buf.len(),
                        )
                    };
                    if n > 0 {
                        let slice = &buf[..n as usize];
                        if slice.contains(&0x03) {
                            saw_ctrl_c = true;
                            ctrl_c_count += 1;
                            last_ctrl_c = Some(Instant::now());
                        }
                        let _ = pty_writer.write_all(slice);
                        let _ = pty_writer.flush();

                        if ctrl_c_count >= 2 {
                            interrupted = true;
                            break child_guard.kill_and_reap();
                        }
                    }
                }
            }

            if saw_ctrl_c
                && last_ctrl_c.is_some_and(|t| t.elapsed() >= Duration::from_secs(2))
            {
                interrupted = true;
                break child_guard.kill_and_reap();
            }

            if let Some(status) = child_guard.try_wait()? {
                if !master_eof {
                    for _ in 0..16 {
                        let mut drain_fd = libc::pollfd {
                            fd: mfd,
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        let drc = unsafe { libc::poll(&mut drain_fd, 1, 10) };
                        if drc <= 0 || (drain_fd.revents & libc::POLLIN) == 0 {
                            break;
                        }
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                push_bounded_bytes(
                                    &mut captured_bytes,
                                    &buf[..n],
                                    MAX_CAPTURED_PTY_BYTES,
                                );
                                if !cfg!(test) {
                                    let mut out = io::stdout().lock();
                                    let _ = out.write_all(&buf[..n]);
                                    let _ = out.flush();
                                }
                            }
                        }
                    }
                }
                break status;
            }
        }
    } else {
        let status = child_guard.wait_reap();
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            push_bounded_bytes(&mut captured_bytes, &buf[..n], MAX_CAPTURED_PTY_BYTES);
            if !cfg!(test) {
                let mut out = io::stdout().lock();
                let _ = out.write_all(&buf[..n]);
                let _ = out.flush();
            }
        }
        status
    };

    drop(pty_writer);
    drop(reader);
    drop(pair.master);
    drop(raw_mode_guard);
    drop(_termios_guard);

    if saw_ctrl_c && !exit_status.success() {
        interrupted = true;
    }

    if interrupted && interactive_tty {
        unsafe {
            libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
        }
    }

    if !cfg!(test) && !captured_bytes.is_empty() && !captured_bytes.ends_with(b"\n") {
        println!();
        let _ = io::stdout().flush();
    }

    let clean_output = {
        let raw_text = String::from_utf8_lossy(&captured_bytes);
        cap_clean_output(strip_ansi_codes(&raw_text), MAX_CLEAN_OUTPUT_CHARS)
    };
    drop(captured_bytes);

    let succeeded = !interrupted && exit_status.success();
    let exit_code = if interrupted {
        130
    } else {
        exit_status.exit_code()
    };

    Ok(PtyExecutionOutput {
        exit_code,
        succeeded,
        interrupted,
        elapsed: start.elapsed(),
        clean_output,
    })
}

fn execute_buffered_with_progress(
    command: &str,
    shell_path: &str,
    show_progress: bool,
    bot_name: &str,
    lang: WizardLang,
) -> crate::Result<PtyExecutionOutput> {
    let start = Instant::now();
    let pty_system = native_pty_system();
    let size = current_pty_size();
    let pair = pty_system
        .openpty(size)
        .map_err(|e| format!("Failed to open pseudo-terminal (PTY): {e}"))?;

    let _termios_guard = TermiosRestoreGuard::capture_stdin();
    let _sigint_guard = PtySigintGuard::install();

    let mut pty_writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("Failed to open PTY writer: {e}"))?;

    let effective_shell = resolve_shell(shell_path);
    let mut cmd = CommandBuilder::new(effective_shell);
    apply_inherited_user_env(&mut cmd);
    cmd.arg("-c");
    cmd.arg(command);
    if let Ok(cwd) = env::current_dir() {
        cmd.cwd(cwd);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("Failed to spawn command in PTY: {e}"))?;
    let mut child_guard = ChildReapGuard::new(child);

    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("Failed to clone PTY reader: {e}"))?;

    let shared_buf = Arc::new(Mutex::new(SharedPtyBuffer::new()));
    let shared_for_reader = Arc::clone(&shared_buf);

    let reader_handle = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut guard) = shared_for_reader.lock() {
                        push_bounded_bytes(&mut guard.all_bytes, &buf[..n], MAX_CAPTURED_PTY_BYTES);
                        push_bounded_bytes(
                            &mut guard.unhandled_bytes,
                            &buf[..n],
                            MAX_UNHANDLED_BYTES,
                        );
                        guard.last_received = Instant::now();
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    let pb = if show_progress && io::stderr().is_terminal() {
        Some(create_ascii_progress_bar(lang))
    } else {
        None
    };

    let mut pos = 0u64;
    let mut interrupted = false;

    let exit_status = loop {
        if CHILD_SIGINT_RECEIVED.load(Ordering::SeqCst) {
            interrupted = true;
            break child_guard.kill_and_reap();
        }

        let mut detected_prompt: Option<InteractivePrompt> = None;
        let mut live_status: Option<(Option<u64>, String)> = None;

        if let Ok(mut guard) = shared_buf.lock() {
            if !guard.unhandled_bytes.is_empty() {
                let tail_start = guard.unhandled_bytes.len().saturating_sub(4096);
                let raw_tail = String::from_utf8_lossy(&guard.unhandled_bytes[tail_start..]);
                let clean_tail = strip_ansi_codes(&raw_tail);

                if let Some(msg) = extract_live_progress_message(&clean_tail) {
                    let pct = extract_progress_percent(&clean_tail);
                    live_status = Some((pct, msg));
                }

                if guard.last_received.elapsed() >= Duration::from_millis(PROMPT_IDLE_DETECT_MS) {
                    if let Some(prompt) = detect_interactive_prompt(&clean_tail) {
                        guard.unhandled_bytes.clear();
                        detected_prompt = Some(prompt);
                    }
                }
            }
        }

        if let (Some(bar), Some((maybe_pct, msg))) = (&pb, live_status) {
            if let Some(pct) = maybe_pct {
                pos = pct.clamp(pos, 98);
                bar.set_position(pos);
            }
            bar.set_message(msg);
        }

        if let Some(prompt) = detected_prompt {
            let reply_res = match &pb {
                Some(bar) => bar.suspend(|| prompt_user_for_pty(bot_name, &prompt, lang)),
                None => prompt_user_for_pty(bot_name, &prompt, lang),
            };

            match reply_res {
                Ok(Some(user_line)) => {
                    let payload = format!("{user_line}\n");
                    let _ = pty_writer.write_all(payload.as_bytes());
                    let _ = pty_writer.flush();
                }
                Ok(None) => {
                    interrupted = true;
                    break child_guard.kill_and_reap();
                }
                Err(_) => {
                    let _ = pty_writer.write_all(b"\n");
                    let _ = pty_writer.flush();
                }
            }
        }

        match child_guard.try_wait()? {
            Some(status) => break status,
            None => {
                if let Some(bar) = &pb {
                    if pos < 92 {
                        pos += 1;
                        bar.set_position(pos);
                    } else {
                        bar.tick();
                    }
                }
                thread::sleep(Duration::from_millis(45));
            }
        }
    };

    drop(pty_writer);
    drop(pair.master);
    let _ = reader_handle.join();

    if let Some(bar) = pb {
        if interrupted {
            bar.finish_with_message(lang.pty_cancelled());
        } else if exit_status.success() {
            bar.set_position(100);
            bar.finish_with_message(lang.pty_done());
        } else {
            bar.abandon_with_message(lang.pty_failed());
        }
        println!();
    }

    let clean_output = {
        let raw_bytes = shared_buf
            .lock()
            .map(|mut g| {
                g.unhandled_bytes.clear();
                g.unhandled_bytes.shrink_to_fit();
                std::mem::take(&mut g.all_bytes)
            })
            .unwrap_or_default();
        let raw_text = String::from_utf8_lossy(&raw_bytes);
        cap_clean_output(strip_ansi_codes(&raw_text), MAX_CLEAN_OUTPUT_CHARS)
    };

    let succeeded = !interrupted && exit_status.success();
    let exit_code = if interrupted {
        130
    } else {
        exit_status.exit_code()
    };

    Ok(PtyExecutionOutput {
        exit_code,
        succeeded,
        interrupted,
        elapsed: start.elapsed(),
        clean_output,
    })
}

/// Atomically writes a root-owned file via `sudo tee` over PTY using a 0600 temporary staging file.
pub fn write_file_as_root_via_pty(
    target: &Path,
    content: &str,
    shell_path: &str,
    bot_name: &str,
) -> crate::Result<PtyExecutionOutput> {
    use std::fs::{self, OpenOptions};
    use std::os::unix::fs::OpenOptionsExt;

    struct RootTempGuard(std::path::PathBuf);
    impl Drop for RootTempGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    let ts = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let tmp_path = env::temp_dir().join(format!(".waish-rootwrite-{}-{ts}.tmp", std::process::id()));
    let _ = fs::remove_file(&tmp_path);
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(crate::config::PRIVATE_FILE_MODE)
            .open(&tmp_path)
            .map_err(|e| format!("Gagal membuat staging file root-write: {e}"))?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    let _guard = RootTempGuard(tmp_path.clone());

    let quoted_target = shell_single_quote(&target.to_string_lossy());
    let quoted_tmp = shell_single_quote(&tmp_path.to_string_lossy());

    let cmd = match target.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) => {
            let quoted_parent = shell_single_quote(&parent.to_string_lossy());
            format!(
                "sudo mkdir -p {quoted_parent} && sudo tee {quoted_target} < {quoted_tmp} >/dev/null"
            )
        }
        None => format!("sudo tee {quoted_target} < {quoted_tmp} >/dev/null"),
    };

    execute_in_pty_with_bot(&cmd, shell_path, true, false, bot_name)
}

pub fn shell_single_quote(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('\'');
    for ch in raw.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn interactive_regexes() -> &'static (Regex, Regex) {
    static RE: OnceLock<(Regex, Regex)> = OnceLock::new();
    RE.get_or_init(|| {
        let yn = Regex::new(INTERACTIVE_YN_PATTERN).expect("valid Y/N regex");
        let sel = Regex::new(INTERACTIVE_SELECT_PATTERN).expect("valid selection regex");
        (yn, sel)
    })
}

pub fn detect_interactive_prompt(clean_tail: &str) -> Option<InteractivePrompt> {
    let non_empty: Vec<&str> = clean_tail
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    let last_line = *non_empty.last()?;
    let (yn_re, sel_re) = interactive_regexes();

    let is_yn = yn_re.is_match(last_line);
    let is_sel = sel_re.is_match(last_line);

    if !is_yn && !is_sel {
        return None;
    }

    let mut context_lines = Vec::new();
    if non_empty.len() > 1 {
        let start = non_empty.len().saturating_sub(6);
        for &line in &non_empty[start..non_empty.len() - 1] {
            if is_sel
                || line.starts_with("::")
                || line.contains(')')
                || line.starts_with('[')
            {
                context_lines.push(line.to_string());
            }
        }
    }

    Some(InteractivePrompt {
        context_lines,
        question_line: last_line.to_string(),
    })
}

fn prompt_user_for_pty(
    bot_name: &str,
    prompt: &InteractivePrompt,
    lang: WizardLang,
) -> io::Result<Option<String>> {
    let name = match bot_name.trim() {
        "" => DEFAULT_BOT_NAME,
        trimmed => trimmed,
    };

    println!();
    for ctx_line in &prompt.context_lines {
        println!("  {}", ctx_line.bright_white());
    }

    print!(
        "{}{}{}  {} {} {} {} ",
        name.bright_magenta().bold(),
        "@".bright_white().bold(),
        "waish".bright_magenta(),
        Kaomoji::Ready.as_str().bright_magenta(),
        "~>".bright_black(),
        lang.pty_needs_input_label().cyan().bold(),
        prompt.question_line.bold()
    );
    io::stdout().flush()?;

    loop {
        if CHILD_SIGINT_RECEIVED.load(Ordering::SeqCst) {
            println!();
            return Ok(None);
        }

        let mut pfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, 50) };
        if rc > 0 && (pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0 {
            let mut input = String::new();
            let mut locked = io::stdin().lock();
            match locked.read_line(&mut input) {
                Ok(0) => return Ok(Some(String::new())),
                Ok(_) => {
                    let clean = input.trim_end_matches(['\r', '\n']).to_string();
                    return Ok(Some(clean));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    if CHILD_SIGINT_RECEIVED.load(Ordering::SeqCst) {
                        println!();
                        return Ok(None);
                    }
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

pub fn extract_live_progress_message(clean_tail: &str) -> Option<String> {
    let (yn_re, sel_re) = interactive_regexes();

    let candidate = clean_tail
        .lines()
        .map(str::trim)
        .rfind(|line| {
            !line.is_empty() && !yn_re.is_match(line) && !sel_re.is_match(line)
        })?;

    let compact: String = candidate.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return None;
    }

    let truncated = truncate_by_width(&compact, MAX_PROGRESS_MSG_WIDTH);
    if truncated.ends_with("...") {
        Some(truncated)
    } else {
        Some(format!("{truncated}..."))
    }
}

pub fn extract_progress_percent(clean_tail: &str) -> Option<u64> {
    static PCT_RE: OnceLock<Regex> = OnceLock::new();
    let re = PCT_RE.get_or_init(|| Regex::new(r"\b(\d{1,3})%").expect("valid pct regex"));

    let last_line = clean_tail
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())?;

    re.captures_iter(last_line)
        .filter_map(|cap| cap.get(1)?.as_str().parse::<u64>().ok())
        .filter(|&val| val <= 100)
        .last()
}

fn truncate_by_width(text: &str, max_width: usize) -> String {
    if display_width(text) <= max_width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut current_w = 0usize;
    let budget = max_width.saturating_sub(3);
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_w + w > budget {
            break;
        }
        out.push(ch);
        current_w += w;
    }
    format!("{}...", out.trim_end())
}

fn create_ascii_progress_bar(lang: WizardLang) -> ProgressBar {
    let pb = ProgressBar::new(100);
    let template = format!(
        "{} [{{bar:30.cyan/blue}}] {{pos:>3}}% {{msg}}",
        lang.pty_progress_prefix()
    );
    let style = ProgressStyle::with_template(&template)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> ");
    pb.set_style(style);
    pb.set_position(0);
    pb
}

fn current_pty_size() -> PtySize {
    match crossterm::terminal::size() {
        Ok((cols, rows)) if cols > 0 && rows > 0 => PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        },
        _ => PtySize::default(),
    }
}

const INHERITED_USER_ENV_VARS: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "PATH",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_TYPE",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "HYPRLAND_INSTANCE_SIGNATURE",
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "TERM",
    "LANG",
    "LC_ALL",
];

pub fn resolve_posix_shell() -> &'static str {
    if Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "/bin/sh"
    }
}

pub fn is_posix_compatible_shell(shell_path: &str) -> bool {
    let trimmed = shell_path.trim();
    if trimmed.is_empty() || !Path::new(trimmed).exists() {
        return false;
    }
    let base = Path::new(trimmed)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    matches!(base, "sh" | "bash" | "dash" | "ash")
}

fn apply_inherited_user_env(cmd: &mut CommandBuilder) {
    for &key in INHERITED_USER_ENV_VARS {
        if let Some(val) = env::var_os(key) {
            cmd.env(key, val);
        }
    }
}

fn resolve_shell(configured: &str) -> &str {
    let trimmed = configured.trim();
    if is_posix_compatible_shell(trimmed) {
        trimmed
    } else {
        resolve_posix_shell()
    }
}

pub fn strip_ansi_codes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }

        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
            continue;
        }

        out.push(ch);
    }

    out
}

pub fn extract_tail_lines(output: &str, max_lines: usize) -> String {
    if max_lines == 0 {
        return String::new();
    }
    let non_empty: Vec<&str> = output
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();

    let start = non_empty.len().saturating_sub(max_lines);
    non_empty[start..].join("\n")
}

pub fn truncate_output(output: &str, max_lines: usize, max_chars: usize) -> String {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let all_lines: Vec<&str> = trimmed.lines().collect();
    let total_lines = all_lines.len();
    let joined = if total_lines > max_lines {
        let head_count = max_lines / 2;
        let tail_count = max_lines - head_count;
        let omitted = total_lines - max_lines;
        let mut parts = Vec::with_capacity(max_lines + 1);
        parts.extend_from_slice(&all_lines[..head_count]);
        parts.push("... [baris tengah disingkat] ...");
        parts.extend_from_slice(&all_lines[total_lines - tail_count..]);
        let mut s = parts.join("\n");
        s.push_str(&format!(
            "\n({omitted} baris disingkat dari total {total_lines} baris)"
        ));
        s
    } else {
        all_lines.join("\n")
    };

    if joined.chars().count() <= max_chars {
        joined
    } else {
        let cut: String = joined.chars().take(max_chars).collect();
        format!("{cut}\n... [output dipotong pada {max_chars} karakter]")
    }
}

struct TermiosRestoreGuard {
    saved: Option<libc::termios>,
}

impl TermiosRestoreGuard {
    fn capture_stdin() -> Self {
        if !io::stdin().is_terminal() {
            return Self { saved: None };
        }
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut term) };
        if rc == 0 {
            Self { saved: Some(term) }
        } else {
            Self { saved: None }
        }
    }
}

impl Drop for TermiosRestoreGuard {
    fn drop(&mut self) {
        if let Some(ref term) = self.saved {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, term);
            }
        }
    }
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Option<Self> {
        crossterm::terminal::enable_raw_mode().ok().map(|()| Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = io::stdout().flush();
    }
}

/// Terminates the child process group (`-pgid`) with `SIGTERM` followed by `SIGKILL`,
/// guarding against signalling `waish`'s own process group.
pub fn terminate_process_group_gracefully(child_pid: u32) {
    let Ok(raw_pid) = libc::pid_t::try_from(child_pid) else {
        return;
    };
    if raw_pid <= 1 {
        return;
    }

    let self_pgid = unsafe { libc::getpgrp() };
    let child_pgid = unsafe { libc::getpgid(raw_pid) };
    let target_pgid = if child_pgid > 1 && child_pgid != self_pgid {
        child_pgid
    } else if raw_pid != unsafe { libc::getpid() } && raw_pid != self_pgid {
        raw_pid
    } else {
        return;
    };

    let term_rc = unsafe { libc::kill(-target_pgid, libc::SIGTERM) };
    if term_rc != 0 {
        return;
    }

    for _ in 0..6 {
        thread::sleep(Duration::from_millis(20));
        let alive = unsafe { libc::kill(-target_pgid, 0) } == 0;
        if !alive {
            return;
        }
    }

    unsafe {
        let _ = libc::kill(-target_pgid, libc::SIGKILL);
    }
}

struct ChildReapGuard {
    child: Box<dyn Child + Send + Sync>,
    reaped: bool,
}

impl ChildReapGuard {
    fn new(child: Box<dyn Child + Send + Sync>) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }

    fn wait_reap(&mut self) -> ExitStatus {
        let status = self
            .child
            .wait()
            .unwrap_or_else(|_| ExitStatus::with_exit_code(1));
        self.reaped = true;
        status
    }

    fn kill_and_reap(&mut self) -> ExitStatus {
        if let Some(pid) = self.child.process_id() {
            terminate_process_group_gracefully(pid);
        }
        let _ = self.child.kill();
        let status = self
            .child
            .wait()
            .unwrap_or_else(|_| ExitStatus::with_exit_code(130));
        self.reaped = true;
        status
    }
}

impl Drop for ChildReapGuard {
    fn drop(&mut self) {
        if !self.reaped {
            if let Some(pid) = self.child.process_id() {
                terminate_process_group_gracefully(pid);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}

struct PtySigintGuard {
    previous: libc::sighandler_t,
}

impl PtySigintGuard {
    fn install() -> Self {
        CHILD_SIGINT_RECEIVED.store(false, Ordering::SeqCst);
        let handler = on_child_sigint as extern "C" fn(libc::c_int);
        let previous = unsafe { libc::signal(libc::SIGINT, handler as libc::sighandler_t) };
        Self { previous }
    }
}

impl Drop for PtySigintGuard {
    fn drop(&mut self) {
        unsafe {
            libc::signal(libc::SIGINT, self.previous);
        }
        CHILD_SIGINT_RECEIVED.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_and_normalizes_crlf_and_standalone_cr() {
        let raw = "\x1b[32mHijau\x1b[0m\r\nBaris 2\x1b]0;Judul\x07\r\n";
        assert_eq!(strip_ansi_codes(raw), "Hijau\nBaris 2\n");

        let cr_progress = "downloading foo... 10%\rdownloading foo... 85%";
        assert_eq!(
            strip_ansi_codes(cr_progress),
            "downloading foo... 10%\ndownloading foo... 85%"
        );
    }

    #[test]
    fn detects_interactive_yn_and_numeric_selection_prompts() {
        let pacman_yn = ":: Synchronizing package databases...\n:: Proceed with installation? [Y/n] ";
        let p1 = detect_interactive_prompt(pacman_yn).expect("should detect [Y/n]");
        assert_eq!(p1.question_line, ":: Proceed with installation? [Y/n]");

        let provider_sel = ":: There are 2 providers available for jack:\n   1) jack2  2) pipewire-jack\n\nEnter a number (default=1): ";
        let p2 = detect_interactive_prompt(provider_sel).expect("should detect provider selection");
        assert_eq!(p2.question_line, "Enter a number (default=1):");
        assert_eq!(p2.context_lines.len(), 2);

        let range_sel = "Pilih paket [1-4]: ";
        let p3 = detect_interactive_prompt(range_sel).expect("should detect [1-4]:");
        assert_eq!(p3.question_line, "Pilih paket [1-4]:");

        let normal_log = "compiling serde v1.0.229\ninstalling neovim (1/1)... 100%";
        assert_eq!(detect_interactive_prompt(normal_log), None);
    }

    #[test]
    fn extracts_live_progress_message_and_percentage() {
        let log = "resolving dependencies...\ndownloading neovim-0.10.2-1-x86_64.pkg.tar.zst [####----] 64%\n";
        let msg = extract_live_progress_message(log).unwrap();
        assert!(msg.contains("downloading neovim"));
        assert!(msg.ends_with("..."));
        assert_eq!(extract_progress_percent(log), Some(64));
    }

    #[test]
    fn extracts_last_n_non_empty_lines() {
        let sample = "baris 1\nbaris 2\n\nbaris 3\nbaris 4\nbaris 5\n\n";
        assert_eq!(extract_tail_lines(sample, 2), "baris 4\nbaris 5");
    }

    #[test]
    fn executes_command_in_pty_and_captures_output() {
        let res = execute_in_pty("printf 'halo_pty\\nbaris_dua\\n'", "/bin/sh", false, false)
            .expect("PTY execution should succeed");
        assert!(res.succeeded);
        assert_eq!(res.exit_code, 0);
        assert!(res.clean_output.contains("halo_pty"));
        assert!(res.clean_output.contains("baris_dua"));
    }

    #[test]
    fn quotes_paths_safely_for_posix_shell() {
        assert_eq!(shell_single_quote("/etc/environment"), "'/etc/environment'");
        assert_eq!(
            shell_single_quote("/etc/my file's.conf"),
            "'/etc/my file'\\''s.conf'"
        );
    }

    #[test]
    fn captures_nonzero_exit_code_and_error_tail() {
        let res = execute_in_pty(
            "echo 'awal'; echo 'error baris 1' >&2; echo 'error baris 2' >&2; exit 42",
            "/bin/sh",
            false,
            false,
        )
        .expect("PTY should return output even on non-zero exit");
        assert!(!res.succeeded);
        assert_eq!(res.exit_code, 42);
        let llm_report = res.format_for_llm("tes_gagal");
        assert!(llm_report.contains("exit_code: 42"));
        assert!(llm_report.contains("error baris 2"));
    }

    #[test]
    fn classifies_interactive_passthrough_commands_accurately() {
        assert!(requires_true_interactive_passthrough("sudo pacman -Syu", true));
        assert!(requires_true_interactive_passthrough("sudo pacman -Syu", false));
        assert!(requires_true_interactive_passthrough("pacman -Syu", false));
        assert!(requires_true_interactive_passthrough("yay -Syu", false));
        assert!(requires_true_interactive_passthrough("paru -Syu", false));
        assert!(requires_true_interactive_passthrough("FOO=1 /usr/bin/sudo apt update", false));
        assert!(requires_true_interactive_passthrough("echo hi && sudo pacman -S neovim", false));
        assert!(requires_true_interactive_passthrough("printf 'ok'", true));
        assert!(requires_true_interactive_passthrough("nvim ~/.config/waybar/config", false));

        assert!(!requires_true_interactive_passthrough("printf 'halo'", false));
        assert!(!requires_true_interactive_passthrough("df -h", false));
        assert!(!requires_true_interactive_passthrough("git status", false));
    }

    #[test]
    fn bounded_tail_buffer_caps_memory_for_long_outputs() {
        let mut buf = Vec::new();
        push_bounded_bytes(&mut buf, b"0123456789", 8);
        assert_eq!(buf, b"23456789");
        push_bounded_bytes(&mut buf, b"ABCD", 8);
        assert_eq!(buf, b"6789ABCD");
    }

    #[test]
    fn interactive_passthrough_executes_cleanly_and_marks_interrupted_no_retry() {
        // Memicu jalur True Interactive PTY Passthrough lewat `requires_root = true` pada command aman.
        let res = execute_in_pty("printf 'passthrough_live_ok\\n'", "/bin/sh", true, true)
            .expect("interactive passthrough should succeed in test mode");
        assert!(res.succeeded);
        assert_eq!(res.exit_code, 0);
        assert!(res.clean_output.contains("passthrough_live_ok"));

        let interrupted = PtyExecutionOutput {
            exit_code: 130,
            succeeded: false,
            interrupted: true,
            elapsed: Duration::from_millis(50),
            clean_output: "Interrupt signal received".to_string(),
        };
        let report = interrupted.format_for_llm("sudo pacman -Syu");
        assert!(report.contains("status: interrupted"));
        assert!(report.contains("do_not_retry: true"));
    }

    #[test]
    fn resolve_shell_forces_posix_for_fish_zsh_and_nushell() {
        let posix = resolve_posix_shell();
        assert!(posix == "/bin/bash" || posix == "/bin/sh");
        assert_eq!(resolve_shell("/usr/bin/fish"), posix);
        assert_eq!(resolve_shell("/bin/zsh"), posix);
        assert_eq!(resolve_shell("/usr/bin/nu"), posix);
        assert_eq!(resolve_shell(""), posix);
        assert_eq!(resolve_shell("/bin/sh"), "/bin/sh");

        // Pastikan sintaks POSIX `VAR=$(...) && ...` & pewarisan env dasar sukses meski `shell_path` = Fish.
        let res = execute_in_pty(
            "VAR=$(printf 'fish_bypass_ok') && test -n \"$HOME\" && test -n \"$PATH\" && printf '%s\n' \"$VAR\"",
            "/usr/bin/fish",
            false,
            false,
        )
        .expect("POSIX command should succeed even when configured shell is fish");
        assert!(res.succeeded, "output: {}", res.clean_output);
        assert!(res.clean_output.contains("fish_bypass_ok"));
    }

    #[test]
    fn isolated_pgroup_termination_reaps_subshell_and_grandchildren() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let mut cmd = Command::new(resolve_posix_shell());
        cmd.args(["-c", "sleep 30 & wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                let _ = libc::setpgid(0, 0);
                Ok(())
            });
        }

        let mut child = cmd.spawn().expect("spawn isolated subshell");
        let child_pid = child.id();
        thread::sleep(Duration::from_millis(60));

        terminate_process_group_gracefully(child_pid);
        let _ = child.wait();

        // Pastikan seluruh grup proses `-child_pid` benar-benar bersih tanpa proses cucu yatim.
        let still_alive = unsafe { libc::kill(-(child_pid as libc::pid_t), 0) } == 0;
        assert!(
            !still_alive,
            "entire child process group (including background sleep) must be terminated"
        );
    }
}
