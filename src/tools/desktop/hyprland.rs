//! Native Hyprland Unix Domain Socket IPC client and event-driven Window Trapper (`WindowBackend`).

use std::collections::{BTreeSet, HashSet};
use std::env;
use std::io::{Read as SyncRead, Write as SyncWrite};
use std::net::Shutdown;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use regex::RegexBuilder;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream as TokioUnixStream;

use super::{WindowBackend, WindowInfo};
use crate::tools::workspace_tracker::{
    collect_positive_workspace_ids, parse_clients_json, parse_workspaces_json, HyprClient,
    HyprClientWorkspace,
};

const HYPR_SOCKET_IO_TIMEOUT: Duration = Duration::from_millis(1500);
pub const HEAVY_APP_TRAP_TIMEOUT: Duration = Duration::from_secs(8);
pub const STANDARD_APP_TRAP_TIMEOUT: Duration = Duration::from_secs(5);

const HEAVY_GUI_KEYWORDS: &[&str] = &[
    "onlyoffice",
    "desktopeditors",
    "libreoffice",
    "soffice",
    "antigravity",
    "electron",
    "code",
    "codium",
    "obsidian",
    "discord",
    "spotify",
    "firefox",
    "brave",
    "chromium",
    "chrome",
    "zen",
    "vivaldi",
    "gimp",
    "inkscape",
    "blender",
    "idea",
    "studio",
];

const TERMINAL_CLASSES: &[&str] = &[
    "kitty",
    "alacritty",
    "foot",
    "footclient",
    "wezterm",
    "org.wezfurlong.wezterm",
    "ghostty",
    "com.mitchellh.ghostty",
    "konsole",
    "gnome-terminal",
    "org.gnome.console",
    "xterm",
    "st",
    "urxvt",
    "tilix",
    "terminator",
    "rio",
];

/// Hyprland Unix Domain Socket paths (`.socket.sock` and `.socket2.sock`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyprSocketPaths {
    pub command_sock: PathBuf,
    pub event_sock: PathBuf,
}

pub fn resolve_hyprland_socket_paths() -> Option<HyprSocketPaths> {
    let sig = env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let sig = sig.trim();
    if sig.is_empty() {
        return None;
    }

    let mut roots = Vec::with_capacity(2);
    if let Ok(xdg) = env::var("XDG_RUNTIME_DIR") {
        let trimmed = xdg.trim();
        if !trimmed.is_empty() {
            roots.push(PathBuf::from(trimmed).join("hypr").join(sig));
        }
    }
    roots.push(PathBuf::from("/tmp/hypr").join(sig));

    for dir in roots {
        let cmd = dir.join(".socket.sock");
        let evt = dir.join(".socket2.sock");
        if cmd.exists() {
            return Some(HyprSocketPaths {
                command_sock: cmd,
                event_sock: evt,
            });
        }
    }
    None
}

// Each `.socket.sock` connection must shutdown(Write), read to EOF, and drop immediately to avoid stalling Hyprland's single-threaded IPC accept loop.
pub async fn hypr_socket_request_on_path_async(
    command_sock: &Path,
    payload: &str,
) -> Result<String, String> {
    let raw_bytes = tokio::time::timeout(HYPR_SOCKET_IO_TIMEOUT, async {
        let mut stream = TokioUnixStream::connect(command_sock)
            .await
            .map_err(|e| format!("gagal connect ke {}: {e}", command_sock.display()))?;
        stream
            .write_all(payload.as_bytes())
            .await
            .map_err(|e| format!("gagal menulis ke {}: {e}", command_sock.display()))?;
        stream
            .shutdown()
            .await
            .map_err(|e| format!("gagal shutdown write {}: {e}", command_sock.display()))?;
        let mut buf = Vec::with_capacity(2048);
        stream
            .read_to_end(&mut buf)
            .await
            .map_err(|e| format!("gagal membaca respons {}: {e}", command_sock.display()))?;
        drop(stream);
        Ok::<Vec<u8>, String>(buf)
    })
    .await
    .map_err(|_| format!("timeout membaca respons Hyprland socket `{payload}`"))??;

    interpret_hypr_socket_reply(payload, &String::from_utf8_lossy(&raw_bytes))
}

pub fn hypr_socket_request_on_path_sync(
    command_sock: &Path,
    payload: &str,
) -> Result<String, String> {
    let raw_bytes = {
        let mut stream = StdUnixStream::connect(command_sock)
            .map_err(|e| format!("gagal connect ke {}: {e}", command_sock.display()))?;
        let _ = stream.set_write_timeout(Some(HYPR_SOCKET_IO_TIMEOUT));
        let _ = stream.set_read_timeout(Some(HYPR_SOCKET_IO_TIMEOUT));
        stream
            .write_all(payload.as_bytes())
            .map_err(|e| format!("gagal menulis ke {}: {e}", command_sock.display()))?;
        let _ = stream.shutdown(Shutdown::Write);
        let mut buf = Vec::with_capacity(2048);
        stream
            .read_to_end(&mut buf)
            .map_err(|e| format!("gagal membaca respons {}: {e}", command_sock.display()))?;
        buf
    };

    interpret_hypr_socket_reply(payload, &String::from_utf8_lossy(&raw_bytes))
}

fn interpret_hypr_socket_reply(payload: &str, raw_reply: &str) -> Result<String, String> {
    let trimmed = raw_reply.trim();
    let low = trimmed.to_ascii_lowercase();
    if low.starts_with("unknown request")
        || low.starts_with("invalid ")
        || low.starts_with("err:")
        || low.starts_with("error:")
    {
        return Err(format!("Hyprland IPC `{payload}` ditolak: {trimmed}"));
    }
    Ok(trimmed.to_string())
}

pub fn translate_hyprctl_args_to_socket_payload(args: &[&str]) -> Option<String> {
    match args {
        ["activeworkspace", "-j"] | ["-j", "activeworkspace"] => {
            Some("j/activeworkspace".to_string())
        }
        ["workspaces", "-j"] | ["-j", "workspaces"] => Some("j/workspaces".to_string()),
        ["clients", "-j"] | ["-j", "clients"] => Some("j/clients".to_string()),
        ["--batch", batch_cmds] => Some(format!("[[BATCH]]{batch_cmds}")),
        ["dispatch", dispatcher, rest @ ..] => {
            if rest.is_empty() {
                Some(format!("dispatch {dispatcher}"))
            } else {
                Some(format!("dispatch {dispatcher} {}", rest.join(" ")))
            }
        }
        ["keyword", key, val] => Some(format!("keyword {key} {val}")),
        ["eval", lua_code] => Some(format!("eval {lua_code}")),
        _ => None,
    }
}

pub fn run_hyprctl_sync(args: &[&str]) -> Result<String, String> {
    if let (Some(paths), Some(payload)) = (
        resolve_hyprland_socket_paths(),
        translate_hyprctl_args_to_socket_payload(args),
    ) {
        if let Ok(out) = hypr_socket_request_on_path_sync(&paths.command_sock, &payload) {
            return Ok(out);
        }
    }

    let output = Command::new("hyprctl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            format!("Gagal menghubungi Hyprland IPC maupun `hyprctl` (pastikan di sesi Hyprland): {e}")
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return Err(format!("`hyprctl {}` gagal: {detail}", args.join(" ")));
    }
    Ok(stdout)
}

pub async fn run_hyprctl_async(args: &[&str]) -> Result<String, String> {
    if let (Some(paths), Some(payload)) = (
        resolve_hyprland_socket_paths(),
        translate_hyprctl_args_to_socket_payload(args),
    ) {
        if let Ok(out) = hypr_socket_request_on_path_async(&paths.command_sock, &payload).await {
            return Ok(out);
        }
    }

    let output = tokio::process::Command::new("hyprctl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| {
            format!("Gagal menghubungi Hyprland IPC maupun `hyprctl` (pastikan di sesi Hyprland): {e}")
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return Err(format!("`hyprctl {}` gagal: {detail}", args.join(" ")));
    }
    Ok(stdout)
}

/// Parsed `openwindow>><hex>,<ws>,<class>,<title>` event from `.socket2.sock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenWindowEvent {
    pub address: String,
    pub workspace_id: Option<i32>,
    pub workspace_name: String,
    pub class: String,
    pub title: String,
}

pub fn parse_socket2_openwindow_line(line: &str) -> Option<OpenWindowEvent> {
    let payload = line.trim().strip_prefix("openwindow>>")?;
    let mut parts = payload.splitn(4, ',');
    let raw_hex = parts.next()?.trim().trim_start_matches("0x").trim_start_matches("0X");
    if raw_hex.is_empty() || !raw_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let ws_raw = parts.next().unwrap_or("").trim();
    let class = parts.next().unwrap_or("").trim().to_string();
    let title = parts.next().unwrap_or("").trim().to_string();

    let ws_id = ws_raw
        .split_whitespace()
        .next()
        .and_then(|tok| tok.parse::<i32>().ok());

    Some(OpenWindowEvent {
        address: format!("0x{}", raw_hex.to_ascii_lowercase()),
        workspace_id: ws_id,
        workspace_name: ws_raw.to_string(),
        class,
        title,
    })
}

pub fn parse_clients_with_pid(raw_json: &str) -> Result<Vec<(HyprClient, Option<u32>)>, String> {
    let vals: Vec<Value> = serde_json::from_str(raw_json)
        .map_err(|e| format!("Gagal mem-parsing JSON clients Hyprland: {e}"))?;
    let mut out = Vec::with_capacity(vals.len());

    for v in vals {
        let addr_raw = v
            .get("address")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if addr_raw.is_empty() {
            continue;
        }
        let class = v
            .get("class")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let initial_class = v
            .get("initialClass")
            .and_then(Value::as_str)
            .unwrap_or(&class)
            .to_string();
        let title = v
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let ws_obj = v.get("workspace");
        let ws_id = ws_obj
            .and_then(|w| w.get("id"))
            .and_then(Value::as_i64)
            .unwrap_or(0) as i32;
        let ws_name = ws_obj
            .and_then(|w| w.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| ws_id.to_string());
        let pid = v
            .get("pid")
            .and_then(Value::as_i64)
            .and_then(|p| u32::try_from(p).ok());

        out.push((
            HyprClient {
                address: addr_raw.to_string(),
                class,
                initial_class,
                title,
                workspace: HyprClientWorkspace {
                    id: ws_id,
                    name: ws_name,
                },
            },
            pid,
        ));
    }

    Ok(out)
}

/// Hyprland window manager backend implementing `WindowBackend`.
#[derive(Debug, Default, Clone, Copy)]
pub struct HyprlandBackend;

impl HyprlandBackend {
    pub fn new() -> Self {
        Self
    }

    pub async fn move_window_silent(&self, raw_address: &str, workspace: i32) -> Result<(), String> {
        if workspace <= 0 {
            return Ok(());
        }
        let addr_arg =
            normalize_window_address(raw_address).unwrap_or_else(|| format!("address:{raw_address}"));
        let legacy_rule = format!("{workspace},{addr_arg}");
        if run_hyprctl_async(&["dispatch", "movetoworkspacesilent", &legacy_rule])
            .await
            .is_ok()
        {
            return Ok(());
        }
        let lua = format!(
            "hl.dispatch(hl.dsp.window.move({{ workspace = {workspace}, silent = true, window = {} }}))",
            lua_string_literal(&addr_arg)
        );
        hypr_dispatch_lua_or_legacy(&lua, &["dispatch", "movetoworkspacesilent", &legacy_rule])
            .await
            .map(|_| ())
    }

    pub async fn launch_app_in_workspace_async(
        &self,
        command: &str,
        workspace: i32,
    ) -> Result<Option<WindowInfo>, String> {
        let cleaned_command = sanitize_gui_launch_command(command)?;
        let target_ws = (workspace > 0).then_some(workspace);
        let class_hint = extract_app_class_hint(&cleaned_command);
        let trap_timeout = adaptive_trap_timeout(&cleaned_command);

        let paths = resolve_hyprland_socket_paths();
        self.launch_and_trap_with_sockets(
            &cleaned_command,
            target_ws,
            class_hint.as_deref(),
            trap_timeout,
            paths.as_ref(),
        )
        .await
    }

    pub async fn launch_and_trap_with_sockets(
        &self,
        cleaned_command: &str,
        target_ws: Option<i32>,
        class_hint: Option<&str>,
        trap_timeout: Duration,
        socket_paths: Option<&HyprSocketPaths>,
    ) -> Result<Option<WindowInfo>, String> {
        let existing_ids: HashSet<String> = self
            .query_clients_via_optional_socket(socket_paths)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(c, _)| normalize_window_address(&c.address))
            .collect();

        // Connect to `.socket2.sock` before `dispatch exec` so fast window spawns are not missed.
        let mut event_lines = match socket_paths {
            Some(p) if p.event_sock.exists() => TokioUnixStream::connect(&p.event_sock)
                .await
                .ok()
                .map(|s| BufReader::new(s).lines()),
            _ => None,
        };

        if let (Some(ws), Some(hint)) = (target_ws, class_hint) {
            let rule = build_temp_windowrule_spec(ws, Some(hint));
            let _ = self
                .send_ipc_via_optional_socket(socket_paths, &["keyword", "windowrulev2", &rule])
                .await;
        }

        let exec_arg = format_legacy_exec_arg(target_ws, cleaned_command);
        let primary_dispatch = self
            .send_ipc_via_optional_socket(socket_paths, &["dispatch", "exec", &exec_arg])
            .await;

        if let Err(dispatch_err) = primary_dispatch {
            let lua_code = format_lua_exec_code(target_ws, cleaned_command);
            self.send_ipc_via_optional_socket(socket_paths, &["eval", &lua_code])
                .await
                .map_err(|lua_err| {
                    format!("Hyprland IPC dispatch exec gagal ({dispatch_err} | {lua_err})")
                })?;
        }

        let deadline = Instant::now() + trap_timeout;
        let poll_step = Duration::from_millis(350);

        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let wait_slice = remaining.min(poll_step);

            if let Some(ref mut reader) = event_lines {
                match tokio::time::timeout(wait_slice, reader.next_line()).await {
                    Ok(Ok(Some(line))) => {
                        if let Some(ev) = parse_socket2_openwindow_line(&line) {
                            let norm_ev_addr = normalize_window_address(&ev.address)
                                .unwrap_or_else(|| format!("address:{}", ev.address));
                            if !existing_ids.contains(&norm_ev_addr)
                                && event_matches_hint(&ev, class_hint)
                            {
                                let mut final_ws =
                                    ev.workspace_id.unwrap_or_else(|| target_ws.unwrap_or(1));
                                if let Some(ws) = target_ws.filter(|&w| final_ws != w) {
                                    let move_arg = format!("{ws},{norm_ev_addr}");
                                    let _ = self
                                        .send_ipc_via_optional_socket(
                                            socket_paths,
                                            &["dispatch", "movetoworkspacesilent", &move_arg],
                                        )
                                        .await;
                                    final_ws = ws;
                                }

                                drop(event_lines);
                                if let Ok(clients) =
                                    self.query_clients_via_optional_socket(socket_paths).await
                                {
                                    if let Some((c, _)) = clients.into_iter().find(|(c, _)| {
                                        normalize_window_address(&c.address).as_deref()
                                            == Some(norm_ev_addr.as_str())
                                    }) {
                                        let mut info = WindowInfo::from(c);
                                        if let Some(ws) = target_ws {
                                            info.workspace = ws;
                                        }
                                        return Ok(Some(info));
                                    }
                                }

                                return Ok(Some(WindowInfo {
                                    id: ev.address,
                                    class: ev.class.clone(),
                                    initial_class: ev.class,
                                    title: ev.title,
                                    workspace: final_ws,
                                }));
                            }
                        }
                        continue;
                    }
                    Ok(Ok(None)) | Ok(Err(_)) => {
                        event_lines = None;
                    }
                    Err(_) => {}
                }
            } else {
                tokio::time::sleep(wait_slice).await;
            }

            if let Ok(clients) = self.query_clients_via_optional_socket(socket_paths).await {
                if let Some(found) =
                    find_newly_spawned_client_normalized(&clients, &existing_ids, class_hint)
                {
                    let mut info = WindowInfo::from(found.clone());
                    if let Some(ws) = target_ws.filter(|&w| info.workspace != w) {
                        let norm_addr = normalize_window_address(&info.id)
                            .unwrap_or_else(|| format!("address:{}", info.id));
                        let move_arg = format!("{ws},{norm_addr}");
                        let _ = self
                            .send_ipc_via_optional_socket(
                                socket_paths,
                                &["dispatch", "movetoworkspacesilent", &move_arg],
                            )
                            .await;
                        info.workspace = ws;
                    }
                    drop(event_lines);
                    return Ok(Some(info));
                }
            }
        }

        drop(event_lines);
        Ok(None)
    }

    async fn send_ipc_via_optional_socket(
        &self,
        socket_paths: Option<&HyprSocketPaths>,
        args: &[&str],
    ) -> Result<String, String> {
        if let (Some(p), Some(payload)) =
            (socket_paths, translate_hyprctl_args_to_socket_payload(args))
        {
            return hypr_socket_request_on_path_async(&p.command_sock, &payload).await;
        }
        run_hyprctl_async(args).await
    }

    async fn query_clients_via_optional_socket(
        &self,
        socket_paths: Option<&HyprSocketPaths>,
    ) -> Result<Vec<(HyprClient, Option<u32>)>, String> {
        let raw = self
            .send_ipc_via_optional_socket(socket_paths, &["clients", "-j"])
            .await?;
        parse_clients_with_pid(&raw)
    }
}

impl WindowBackend for HyprlandBackend {
    fn get_active_workspace(&self) -> Result<i32, String> {
        let raw = run_hyprctl_sync(&["activeworkspace", "-j"])?;
        parse_active_workspace_id(&raw)
    }

    fn list_workspaces(&self) -> Result<Vec<i32>, String> {
        let raw = run_hyprctl_sync(&["workspaces", "-j"])?;
        let parsed = parse_workspaces_json(&raw)?;
        let positive = collect_positive_workspace_ids(&parsed);
        Ok(positive.into_iter().collect())
    }

    fn list_windows(&self) -> Result<Vec<WindowInfo>, String> {
        let raw = run_hyprctl_sync(&["clients", "-j"])?;
        if let Ok(tolerant) = parse_clients_with_pid(&raw) {
            return Ok(tolerant
                .into_iter()
                .map(|(c, _)| WindowInfo::from(c))
                .collect());
        }
        let clients = parse_clients_json(&raw)?;
        Ok(clients.into_iter().map(WindowInfo::from).collect())
    }

    fn close_window(&self, target: &str) -> Result<String, String> {
        let trimmed = target.trim();
        if trimmed.is_empty() {
            return Err("Target jendela tidak boleh kosong.".to_string());
        }

        if let Some(norm_addr) = normalize_window_address(trimmed) {
            let method = close_address_sync_with_fallback(&norm_addr, trimmed)?;
            return Ok(format!("Berhasil menutup jendela {norm_addr} ({method})."));
        }

        let windows = self.list_windows()?;
        let matched = select_matching_windows(&windows, None, Some(trimmed));
        if matched.is_empty() {
            return Err(format!(
                "Tidak ditemukan jendela Hyprland yang cocok dengan target `{trimmed}`."
            ));
        }

        let mut closed = Vec::new();
        let mut errors = Vec::new();
        for win in matched {
            let addr =
                normalize_window_address(&win.id).unwrap_or_else(|| format!("address:{}", win.id));
            match close_address_sync_with_fallback(&addr, &win.class) {
                Ok(method) => closed.push(format!(
                    "[ws:{}] {} ({}, {})",
                    win.workspace, win.class, win.id, method
                )),
                Err(err) => errors.push(err),
            }
        }

        if closed.is_empty() {
            Err(errors.join("; "))
        } else {
            Ok(format!(
                "Berhasil menutup {} jendela: {}",
                closed.len(),
                closed.join(", ")
            ))
        }
    }

    fn close_workspaces(&self, workspace_ids: &[i32]) -> Result<String, String> {
        let clean_ids: Vec<i32> = workspace_ids
            .iter()
            .copied()
            .filter(|&id| id > 0)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if clean_ids.is_empty() {
            return Err(
                "Daftar `workspaces` kosong atau tidak berisi ID workspace positif.".to_string(),
            );
        }

        let active_ws = self.get_active_workspace().ok();
        let protected_pids: HashSet<u32> =
            crate::tools::runner::collect_protected_pid_chain(std::process::id())
                .into_iter()
                .collect();

        let raw_clients = run_hyprctl_sync(&["clients", "-j"])?;
        let clients_with_pid = parse_clients_with_pid(&raw_clients)?;
        let annotated: Vec<(WindowInfo, Option<u32>)> = clients_with_pid
            .into_iter()
            .map(|(c, pid)| (WindowInfo::from(c), pid))
            .collect();

        let (to_close, protected) = select_batch_close_targets_with_pids(
            &annotated,
            &clean_ids,
            active_ws,
            &protected_pids,
        );

        if to_close.is_empty() {
            let ws_list = clean_ids
                .iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let mut msg =
                format!("Tidak ada jendela yang perlu ditutup pada workspace [{ws_list}].");
            if !protected.is_empty() {
                msg.push_str(&format!(
                    " ({} jendela terminal waish dilindungi oleh suicide protection.)",
                    protected.len()
                ));
            }
            return Ok(msg);
        }

        let mut target_addrs: Vec<(String, String, i32)> = Vec::with_capacity(to_close.len());
        let mut batch_parts: Vec<String> = Vec::with_capacity(to_close.len());
        for win in &to_close {
            let addr =
                normalize_window_address(&win.id).unwrap_or_else(|| format!("address:{}", win.id));
            batch_parts.push(format!("dispatch closewindow {addr}"));
            target_addrs.push((addr, win.class.clone(), win.workspace));
        }
        let batch_joined = batch_parts.join(";");
        if run_hyprctl_sync(&["--batch", &batch_joined]).is_err() {
            for (addr, _, _) in &target_addrs {
                let addr_lit = lua_string_literal(addr);
                let lua_close =
                    format!("hl.dispatch(hl.dsp.window.close({{ window = {addr_lit} }}))");
                let _ = hypr_dispatch_lua_or_legacy_sync(
                    &lua_close,
                    &["dispatch", "closewindow", addr],
                );
            }
        }

        std::thread::sleep(Duration::from_millis(250));
        let alive_now: HashSet<String> = self
            .list_windows()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|w| normalize_window_address(&w.id))
            .collect();

        let mut closed_items = Vec::new();
        let mut failed_items = Vec::new();
        let mut escalated = Vec::new();

        for (addr, class_name, ws_id) in &target_addrs {
            if !alive_now.contains(addr) {
                closed_items.push(format!("[ws:{ws_id}] {class_name} ({addr}, closed)"));
            } else {
                let addr_lit = lua_string_literal(addr);
                let lua_kill =
                    format!("hl.dispatch(hl.dsp.window.kill({{ window = {addr_lit} }}))");
                if hypr_dispatch_lua_or_legacy_sync(&lua_kill, &["dispatch", "focuswindow", addr])
                    .is_ok()
                {
                    let lua_focus_kill = format!(
                        "hl.dispatch(hl.dsp.focus({{ window = {addr_lit} }})); hl.dispatch(hl.dsp.window.kill())"
                    );
                    let _ = hypr_dispatch_lua_or_legacy_sync(
                        &lua_focus_kill,
                        &["dispatch", "killactive"],
                    );
                }
                escalated.push((addr.clone(), class_name.clone(), *ws_id));
            }
        }

        if !escalated.is_empty() {
            std::thread::sleep(Duration::from_millis(200));
            let alive_final: HashSet<String> = self
                .list_windows()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|w| normalize_window_address(&w.id))
                .collect();
            for (addr, class_name, ws_id) in escalated {
                if !alive_final.contains(&addr) {
                    closed_items.push(format!("[ws:{ws_id}] {class_name} ({addr}, killed)"));
                } else {
                    failed_items.push(format!("[ws:{ws_id}] {class_name} ({addr})"));
                }
            }
        }

        let ws_list = clean_ids
            .iter()
            .map(i32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let mut msg = format!(
            "Batch close workspace [{ws_list}] selesai — {} jendela ditutup:\n- {}",
            closed_items.len(),
            closed_items.join("\n- ")
        );
        if !protected.is_empty() {
            let names: Vec<String> = protected
                .iter()
                .map(|w| format!("[ws:{}] {}", w.workspace, w.class))
                .collect();
            msg.push_str(&format!(
                "\nSuicide protection: menjaga {} jendela terminal aktif ({}).",
                protected.len(),
                names.join(", ")
            ));
        }
        if !failed_items.is_empty() {
            msg.push_str(&format!(
                "\nSebagian jendela masih bertahan: {}",
                failed_items.join(", ")
            ));
        }
        Ok(msg)
    }

    fn launch_app_in_workspace(
        &self,
        command: &str,
        workspace: i32,
    ) -> Result<Option<WindowInfo>, String> {
        let cmd_owned = command.to_string();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("gagal menginisialisasi runtime Window Trapper: {e}"))?;
            rt.block_on(HyprlandBackend::new().launch_app_in_workspace_async(&cmd_owned, workspace))
        })
        .join()
        .map_err(|_| "thread Window Trapper mengalami panic".to_string())?
    }
}

pub fn adaptive_trap_timeout(command: &str) -> Duration {
    let low = command.to_ascii_lowercase();
    if HEAVY_GUI_KEYWORDS.iter().any(|kw| low.contains(kw)) {
        HEAVY_APP_TRAP_TIMEOUT
    } else {
        STANDARD_APP_TRAP_TIMEOUT
    }
}

fn event_matches_hint(ev: &OpenWindowEvent, class_hint: Option<&str>) -> bool {
    let candidate_win = WindowInfo {
        id: ev.address.clone(),
        class: ev.class.clone(),
        initial_class: ev.class.clone(),
        title: ev.title.clone(),
        workspace: ev.workspace_id.unwrap_or(1),
    };
    if is_terminal_window(&candidate_win)
        && class_hint.is_none_or(|h| !TERMINAL_CLASSES.contains(&h.to_ascii_lowercase().as_str()))
    {
        return false;
    }

    let Some(hint) = class_hint.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    let h_low = hint.to_ascii_lowercase();
    let cls_low = ev.class.to_ascii_lowercase();
    let title_low = ev.title.to_ascii_lowercase();

    // OnlyOffice spawns binary `onlyoffice-desktopeditors` with X11/Wayland class `DesktopEditors`.
    if h_low.contains("onlyoffice")
        && (cls_low.contains("desktopeditors") || title_low.contains("onlyoffice"))
    {
        return true;
    }
    if h_low.contains("libreoffice") && cls_low.contains("soffice") {
        return true;
    }

    let stem = h_low
        .rsplit(['-', '.', '_'])
        .next()
        .filter(|s| s.len() >= 3)
        .unwrap_or(h_low.as_str());

    cls_low.contains(&h_low)
        || title_low.contains(&h_low)
        || cls_low.contains(stem)
        || title_low.contains(stem)
        || !is_terminal_window(&candidate_win)
}

pub fn is_terminal_window(win: &WindowInfo) -> bool {
    let cls = win.class.trim().to_ascii_lowercase();
    let init = win.initial_class.trim().to_ascii_lowercase();
    let title = win.title.trim().to_ascii_lowercase();
    TERMINAL_CLASSES.contains(&cls.as_str())
        || TERMINAL_CLASSES.contains(&init.as_str())
        || title.contains("waish")
}

pub fn select_batch_close_targets_with_pids<'a>(
    windows_with_pids: &'a [(WindowInfo, Option<u32>)],
    workspace_ids: &[i32],
    active_workspace: Option<i32>,
    protected_pids: &HashSet<u32>,
) -> (Vec<&'a WindowInfo>, Vec<&'a WindowInfo>) {
    let target_set: HashSet<i32> = workspace_ids.iter().copied().filter(|&id| id > 0).collect();
    let mut to_close = Vec::new();
    let mut protected = Vec::new();

    for (win, maybe_pid) in windows_with_pids {
        if win.workspace <= 0 || !target_set.contains(&win.workspace) {
            continue;
        }
        let pid_is_host_ancestor = maybe_pid.is_some_and(|pid| protected_pids.contains(&pid));
        let active_terminal_guard = active_workspace == Some(win.workspace) && is_terminal_window(win);

        if pid_is_host_ancestor || active_terminal_guard {
            protected.push(win);
        } else {
            to_close.push(win);
        }
    }

    (to_close, protected)
}

pub fn select_batch_close_targets<'a>(
    windows: &'a [WindowInfo],
    workspace_ids: &[i32],
    active_workspace: Option<i32>,
) -> (Vec<&'a WindowInfo>, Vec<&'a WindowInfo>) {
    let target_set: HashSet<i32> = workspace_ids.iter().copied().filter(|&id| id > 0).collect();
    let mut to_close = Vec::new();
    let mut protected = Vec::new();

    for win in windows {
        if win.workspace <= 0 || !target_set.contains(&win.workspace) {
            continue;
        }
        if active_workspace == Some(win.workspace) && is_terminal_window(win) {
            protected.push(win);
        } else {
            to_close.push(win);
        }
    }

    (to_close, protected)
}

pub fn extract_app_class_hint(command: &str) -> Option<String> {
    let mut tokens = command.split_whitespace();
    while let Some(raw) = tokens.next() {
        let tok = raw.trim_matches(|c| c == '\'' || c == '"');
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
        if matches!(
            base,
            "env" | "setsid" | "nohup" | "sh" | "bash" | "xdg-open"
        ) {
            if base == "sh" || base == "bash" {
                let _ = tokens.next();
            }
            continue;
        }
        let clean: String = base
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .collect();
        if clean.len() >= 2 {
            return Some(clean);
        }
        break;
    }
    None
}

pub fn build_temp_windowrule_spec(workspace: i32, class_hint: Option<&str>) -> String {
    match class_hint.map(str::trim).filter(|s| !s.is_empty()) {
        Some(hint) => {
            let escaped = regex::escape(hint);
            format!("workspace {workspace} silent, class:^(?i).*{escaped}.*$")
        }
        None => format!("workspace {workspace} silent, class:^(.*)$"),
    }
}

pub fn sanitize_gui_launch_command(command: &str) -> Result<String, String> {
    let mut trimmed = command.trim().trim_end_matches('&').trim().to_string();
    if let Some(after_nohup) = trimmed.strip_prefix("nohup ") {
        trimmed = after_nohup.trim().to_string();
    }
    if trimmed.is_empty() {
        return Err("Perintah peluncuran aplikasi GUI kosong.".to_string());
    }

    trimmed = strip_forced_qt_wayland_for_onlyoffice(&trimmed);

    let real_home = crate::tools::runner::resolve_real_home_dir();
    let rewritten = crate::tools::runner::rewrite_companion_home_and_tilde_with_env(
        &trimmed,
        crate::config::settings::DEFAULT_USER_REAL_NAME,
        &real_home,
    );

    if !rewritten.contains("env -u XAUTHORITY")
        && extract_app_class_hint(&rewritten).is_some_and(|hint| {
            crate::tools::runner::LEGACY_X11_GUI_BINS
                .iter()
                .any(|b| b.eq_ignore_ascii_case(&hint))
        })
    {
        let disp = crate::tools::runner::detect_active_x11_display();
        return Ok(format!("env -u XAUTHORITY DISPLAY={disp} {rewritten}"));
    }

    Ok(rewritten)
}

pub fn strip_forced_qt_wayland_for_onlyoffice(command: &str) -> String {
    let low = command.to_ascii_lowercase();
    if !(low.contains("onlyoffice") || low.contains("desktopeditors")) {
        return command.to_string();
    }
    let re = RegexBuilder::new(r"\bQT_QPA_PLATFORM\s*=\s*['\x22]?wayland['\x22]?\s*")
        .case_insensitive(true)
        .build()
        .expect("valid qt wayland regex");
    let cleaned = re.replace_all(command, "").trim().to_string();
    cleaned
        .strip_prefix("env ")
        .map(|rest| {
            let r = rest.trim_start();
            let first = r.split_whitespace().next().unwrap_or("");
            if !first.contains('=') && !first.starts_with('-') {
                r.to_string()
            } else {
                cleaned.clone()
            }
        })
        .unwrap_or(cleaned)
}

fn find_newly_spawned_client_normalized<'a>(
    clients: &'a [(HyprClient, Option<u32>)],
    existing_norm_ids: &HashSet<String>,
    class_hint: Option<&str>,
) -> Option<&'a HyprClient> {
    let plain_existing: HashSet<String> = existing_norm_ids
        .iter()
        .map(|s| s.strip_prefix("address:").unwrap_or(s).to_string())
        .collect();
    find_newly_spawned_client(clients, &plain_existing, class_hint, None)
}

pub fn find_newly_spawned_client<'a>(
    clients: &'a [(HyprClient, Option<u32>)],
    existing_ids: &HashSet<String>,
    class_hint: Option<&str>,
    spawned_pid: Option<u32>,
) -> Option<&'a HyprClient> {
    let norm_existing: HashSet<String> = existing_ids
        .iter()
        .map(|id| normalize_window_address(id).unwrap_or_else(|| id.clone()))
        .collect();

    let new_clients: Vec<&(HyprClient, Option<u32>)> = clients
        .iter()
        .filter(|(c, _)| {
            if c.workspace.id <= 0 {
                return false;
            }
            let norm_c = normalize_window_address(&c.address).unwrap_or_else(|| c.address.clone());
            !norm_existing.contains(&norm_c) && !existing_ids.contains(&c.address)
        })
        .collect();
    if new_clients.is_empty() {
        return None;
    }

    if let Some((c, _)) = spawned_pid.and_then(|target_pid| {
        new_clients.iter().find(|(_, p)| *p == Some(target_pid))
    }) {
        return Some(c);
    }

    if let Some(hint) = class_hint.map(str::trim).filter(|s| !s.is_empty()) {
        let needle = hint.to_ascii_lowercase();
        let needle_stem = needle
            .rsplit(['-', '.', '_'])
            .next()
            .filter(|s| s.len() >= 3)
            .unwrap_or(needle.as_str());

        if let Some((c, _)) = new_clients.iter().find(|(c, _)| {
            let cls = c.class.to_ascii_lowercase();
            let init = c.initial_class.to_ascii_lowercase();
            let title = c.title.to_ascii_lowercase();
            let onlyoffice_alias = needle.contains("onlyoffice")
                && (cls.contains("desktopeditors") || title.contains("onlyoffice"));
            let libreoffice_alias = needle.contains("libreoffice") && cls.contains("soffice");
            onlyoffice_alias
                || libreoffice_alias
                || cls.contains(&needle)
                || init.contains(&needle)
                || title.contains(&needle)
                || cls.contains(needle_stem)
                || init.contains(needle_stem)
        }) {
            return Some(c);
        }
    }

    new_clients
        .iter()
        .find(|(c, _)| !is_terminal_window(&WindowInfo::from((*c).clone())))
        .or_else(|| new_clients.first())
        .map(|(c, _)| c)
}

impl From<HyprClient> for WindowInfo {
    fn from(c: HyprClient) -> Self {
        Self {
            id: c.address,
            class: c.class,
            initial_class: c.initial_class,
            title: c.title,
            workspace: c.workspace.id,
        }
    }
}

pub fn parse_active_workspace_id(raw_json: &str) -> Result<i32, String> {
    let val: Value = serde_json::from_str(raw_json)
        .map_err(|e| format!("Gagal mem-parsing `activeworkspace` JSON: {e}"))?;
    val.get("id")
        .and_then(Value::as_i64)
        .map(|id| id as i32)
        .ok_or_else(|| "Field `id` tidak ditemukan pada `activeworkspace` JSON.".to_string())
}

pub fn normalize_window_address(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let without_prefix = if trimmed.len() >= 8 && trimmed[..8].eq_ignore_ascii_case("address:") {
        trimmed[8..].trim()
    } else {
        trimmed
    };
    let hex_part = without_prefix
        .strip_prefix("0x")
        .or_else(|| without_prefix.strip_prefix("0X"))?;
    if hex_part.is_empty() || !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("address:0x{}", hex_part.to_ascii_lowercase()))
}

pub fn select_matching_windows<'a>(
    windows: &'a [WindowInfo],
    workspace: Option<i32>,
    selector: Option<&str>,
) -> Vec<&'a WindowInfo> {
    if let Some(norm_addr) = selector.and_then(normalize_window_address) {
        let matched: Vec<&WindowInfo> = windows
            .iter()
            .filter(|w| normalize_window_address(&w.id).as_deref() == Some(norm_addr.as_str()))
            .filter(|w| workspace.is_none_or(|ws| w.workspace == ws))
            .collect();
        if !matched.is_empty() {
            return matched;
        }
    }

    let base: Vec<&WindowInfo> = windows
        .iter()
        .filter(|w| w.workspace > 0)
        .filter(|w| workspace.is_none_or(|ws| w.workspace == ws))
        .collect();

    let Some(sel) = selector.map(str::trim).filter(|s| !s.is_empty()) else {
        return base;
    };

    let needle = sel.to_ascii_lowercase();
    let by_class: Vec<&WindowInfo> = base
        .iter()
        .copied()
        .filter(|w| {
            w.class.to_ascii_lowercase().contains(&needle)
                || w.initial_class.to_ascii_lowercase().contains(&needle)
        })
        .collect();
    if !by_class.is_empty() {
        return by_class;
    }

    let by_title: Vec<&WindowInfo> = base
        .iter()
        .copied()
        .filter(|w| w.title.to_ascii_lowercase().contains(&needle))
        .collect();
    if !by_title.is_empty() {
        return by_title;
    }

    if let Ok(re) = RegexBuilder::new(sel).case_insensitive(true).build() {
        return base
            .into_iter()
            .filter(|w| {
                re.is_match(&w.class) || re.is_match(&w.initial_class) || re.is_match(&w.title)
            })
            .collect();
    }

    Vec::new()
}

pub fn lua_string_literal(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

pub fn format_legacy_exec_arg(target_workspace: Option<i32>, command: &str) -> String {
    if let Some(ws) = target_workspace.filter(|&w| w > 0) {
        format!("[workspace {ws} silent] {command}")
    } else {
        command.to_string()
    }
}

pub fn format_lua_exec_code(target_workspace: Option<i32>, command: &str) -> String {
    let cmd_lit = lua_string_literal(command);
    if let Some(ws) = target_workspace.filter(|&w| w > 0) {
        format!("hl.exec_cmd({cmd_lit}, {{ workspace = \"{ws} silent\" }})")
    } else {
        format!("hl.exec_cmd({cmd_lit})")
    }
}

pub async fn hypr_dispatch_lua_or_legacy(
    lua_code: &str,
    legacy_args: &[&str],
) -> Result<String, String> {
    match run_hyprctl_async(legacy_args).await {
        Ok(out) => Ok(out),
        Err(legacy_err) => match run_hyprctl_async(&["eval", lua_code]).await {
            Ok(out) => Ok(out),
            Err(lua_err) => Err(format!(
                "Hyprland IPC gagal (Dispatch: {legacy_err} | Lua: {lua_err})"
            )),
        },
    }
}

pub fn hypr_dispatch_lua_or_legacy_sync(
    lua_code: &str,
    legacy_args: &[&str],
) -> Result<String, String> {
    match run_hyprctl_sync(legacy_args) {
        Ok(out) => Ok(out),
        Err(legacy_err) => match run_hyprctl_sync(&["eval", lua_code]) {
            Ok(out) => Ok(out),
            Err(lua_err) => Err(format!(
                "Hyprland IPC gagal (Dispatch: {legacy_err} | Lua: {lua_err})"
            )),
        },
    }
}

#[cfg(test)]
pub async fn hypr_exec(command: &str, target_workspace: Option<i32>) -> Result<String, String> {
    let lua_code = format_lua_exec_code(target_workspace, command);
    let legacy_arg = format_legacy_exec_arg(target_workspace, command);
    hypr_dispatch_lua_or_legacy(&lua_code, &["dispatch", "exec", &legacy_arg]).await
}

fn close_address_sync_with_fallback(addr_arg: &str, label: &str) -> Result<&'static str, String> {
    let addr_lit = lua_string_literal(addr_arg);
    let lua_close = format!("hl.dispatch(hl.dsp.window.close({{ window = {addr_lit} }}))");
    let close_res =
        hypr_dispatch_lua_or_legacy_sync(&lua_close, &["dispatch", "closewindow", addr_arg]);

    if close_res.is_ok() {
        std::thread::sleep(Duration::from_millis(250));
        if !is_address_alive_sync(addr_arg) {
            return Ok("closed");
        }
    }

    let lua_kill = format!("hl.dispatch(hl.dsp.window.kill({{ window = {addr_lit} }}))");
    if hypr_dispatch_lua_or_legacy_sync(&lua_kill, &["dispatch", "focuswindow", addr_arg]).is_ok() {
        if is_address_alive_sync(addr_arg) {
            let lua_focus_kill = format!(
                "hl.dispatch(hl.dsp.focus({{ window = {addr_lit} }})); hl.dispatch(hl.dsp.window.kill())"
            );
            let _ = hypr_dispatch_lua_or_legacy_sync(&lua_focus_kill, &["dispatch", "killactive"]);
        }
        std::thread::sleep(Duration::from_millis(200));
        if !is_address_alive_sync(addr_arg) {
            return Ok("killed");
        }
    }

    match close_res {
        Err(e) => Err(e),
        Ok(_) => Err(format!(
            "Jendela `{label}` ({addr_arg}) masih terbuka setelah percobaan close dan killactive."
        )),
    }
}

fn is_address_alive_sync(addr_arg: &str) -> bool {
    run_hyprctl_sync(&["clients", "-j"])
        .ok()
        .and_then(|raw| parse_clients_with_pid(&raw).ok())
        .map(|list| {
            list.iter()
                .any(|(c, _)| normalize_window_address(&c.address).as_deref() == Some(addr_arg))
        })
        .unwrap_or(false)
}

pub async fn close_client_async_with_fallback(
    client: &HyprClient,
) -> Result<&'static str, String> {
    let addr_arg = normalize_window_address(&client.address)
        .unwrap_or_else(|| format!("address:{}", client.address));
    let addr_lit = lua_string_literal(&addr_arg);

    let lua_close = format!("hl.dispatch(hl.dsp.window.close({{ window = {addr_lit} }}))");
    let close_res =
        hypr_dispatch_lua_or_legacy(&lua_close, &["dispatch", "closewindow", &addr_arg]).await;

    if close_res.is_ok() {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if !is_address_alive_async(&addr_arg).await {
            return Ok("closed");
        }
    }

    let lua_kill = format!("hl.dispatch(hl.dsp.window.kill({{ window = {addr_lit} }}))");
    if hypr_dispatch_lua_or_legacy(&lua_kill, &["dispatch", "focuswindow", &addr_arg])
        .await
        .is_ok()
    {
        if is_address_alive_async(&addr_arg).await {
            let lua_focus_kill = format!(
                "hl.dispatch(hl.dsp.focus({{ window = {addr_lit} }})); hl.dispatch(hl.dsp.window.kill())"
            );
            let _ =
                hypr_dispatch_lua_or_legacy(&lua_focus_kill, &["dispatch", "killactive"]).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        if !is_address_alive_async(&addr_arg).await {
            return Ok("killed");
        }
    }

    match close_res {
        Err(e) => Err(e),
        Ok(_) => Err(format!(
            "Jendela `{}` ({addr_arg}) masih terbuka setelah percobaan close dan killactive.",
            client.class
        )),
    }
}

async fn is_address_alive_async(addr_arg: &str) -> bool {
    run_hyprctl_async(&["clients", "-j"])
        .await
        .ok()
        .and_then(|raw| parse_clients_with_pid(&raw).ok())
        .map(|list| {
            list.iter()
                .any(|(c, _)| normalize_window_address(&c.address).as_deref() == Some(addr_arg))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::UnixListener;

    #[test]
    fn parses_active_workspace_and_formats_safe_exec_rules() {
        assert_eq!(
            parse_active_workspace_id(r#"{"id": 4, "name": "4"}"#).unwrap(),
            4
        );
        assert!(parse_active_workspace_id("{}").is_err());

        let cmd = "firefox --new-window 'https://www.youtube.com/watch?v=rz1B4B2yBZY&autoplay=1'";
        assert_eq!(
            format_legacy_exec_arg(Some(5), cmd),
            "[workspace 5 silent] firefox --new-window 'https://www.youtube.com/watch?v=rz1B4B2yBZY&autoplay=1'"
        );
        let lua = format_lua_exec_code(Some(5), cmd);
        assert!(lua.contains("hl.exec_cmd("));
        assert!(lua.contains(r#"workspace = "5 silent""#));
    }

    #[test]
    fn selects_windows_by_address_class_and_title_regex() {
        let windows = vec![
            WindowInfo {
                id: "0x1234abcd".to_string(),
                class: "Spotify".to_string(),
                initial_class: "Spotify".to_string(),
                title: "Spotify Premium".to_string(),
                workspace: 5,
            },
            WindowInfo {
                id: "0x5678ef01".to_string(),
                class: "firefox".to_string(),
                initial_class: "firefox".to_string(),
                title: "GTA VI Trailer - YouTube — Mozilla Firefox".to_string(),
                workspace: 5,
            },
        ];

        assert_eq!(
            select_matching_windows(&windows, None, Some("address:0x1234ABCD")).len(),
            1
        );
        assert_eq!(
            select_matching_windows(&windows, Some(5), Some("GTA.*YouTube"))[0].id,
            "0x5678ef01"
        );
    }

    #[test]
    fn batch_close_workspaces_protects_host_pid_chain_and_active_terminal() {
        let annotated = vec![
            (
                WindowInfo {
                    id: "0x1".to_string(),
                    class: "firefox".to_string(),
                    initial_class: "firefox".to_string(),
                    title: "Docs".to_string(),
                    workspace: 1,
                },
                Some(2001),
            ),
            (
                WindowInfo {
                    id: "0x2".to_string(),
                    class: "Spotify".to_string(),
                    initial_class: "Spotify".to_string(),
                    title: "Spotify".to_string(),
                    workspace: 2,
                },
                Some(2002),
            ),
            (
                WindowInfo {
                    id: "0x3host".to_string(),
                    class: "custom-term".to_string(),
                    initial_class: "custom-term".to_string(),
                    title: "shell".to_string(),
                    workspace: 3,
                },
                Some(7777),
            ),
            (
                WindowInfo {
                    id: "0x4a".to_string(),
                    class: "kitty".to_string(),
                    initial_class: "kitty".to_string(),
                    title: "waish".to_string(),
                    workspace: 4,
                },
                Some(8888),
            ),
            (
                WindowInfo {
                    id: "0x5".to_string(),
                    class: "DesktopEditors".to_string(),
                    initial_class: "DesktopEditors".to_string(),
                    title: "tabel.xlsx".to_string(),
                    workspace: 5,
                },
                Some(9999),
            ),
        ];

        let protected_pids = HashSet::from([7777u32]);
        let (to_close, protected) = select_batch_close_targets_with_pids(
            &annotated,
            &[2, 3, 4, 5],
            Some(4),
            &protected_pids,
        );
        let closed_ids: Vec<&str> = to_close.iter().map(|w| w.id.as_str()).collect();
        let protected_ids: Vec<&str> = protected.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(closed_ids, vec!["0x2", "0x5"]);
        assert_eq!(protected_ids, vec!["0x3host", "0x4a"]);
    }

    #[test]
    fn sanitizes_onlyoffice_wayland_env_and_assigns_8s_adaptive_timeout() {
        let stripped = sanitize_gui_launch_command(
            "env QT_QPA_PLATFORM=wayland onlyoffice-desktopeditors ~/laporan.xlsx &",
        )
        .unwrap();
        assert!(!stripped.contains("QT_QPA_PLATFORM"));
        assert!(stripped.starts_with("onlyoffice-desktopeditors "));

        assert_eq!(
            adaptive_trap_timeout("onlyoffice-desktopeditors /tmp/a.xlsx"),
            Duration::from_secs(8)
        );
        assert_eq!(
            adaptive_trap_timeout("/usr/bin/puzzles-mines"),
            Duration::from_secs(5)
        );

        let ev = parse_socket2_openwindow_line(
            "openwindow>>55c84c991a20,2,DesktopEditors,Laporan.xlsx - ONLYOFFICE",
        )
        .unwrap();
        assert_eq!(ev.address, "0x55c84c991a20");
        assert_eq!(ev.workspace_id, Some(2));
        assert_eq!(ev.class, "DesktopEditors");
    }

    #[tokio::test]
    async fn unix_socket_trapper_captures_openwindow_relocates_drift_and_closes_sockets_immediately()
    {
        let sock_dir =
            env::temp_dir().join(format!("waish-hypr-sock-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sock_dir);
        std::fs::create_dir_all(&sock_dir).unwrap();
        let cmd_sock = sock_dir.join(".socket.sock");
        let evt_sock = sock_dir.join(".socket2.sock");

        let cmd_listener = UnixListener::bind(&cmd_sock).unwrap();
        let evt_listener = UnixListener::bind(&evt_sock).unwrap();

        let received_cmds: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let received_srv = Arc::clone(&received_cmds);

        tokio::spawn(async move {
            if let Ok((mut evt_stream, _)) = evt_listener.accept().await {
                tokio::time::sleep(Duration::from_millis(60)).await;
                let _ = evt_stream
                    .write_all(
                        b"openwindow>>deadbeef1234,2,DesktopEditors,Budget.xlsx - ONLYOFFICE\n",
                    )
                    .await;
            }
        });

        tokio::spawn(async move {
            for _ in 0..5 {
                let Ok((mut s, _)) = cmd_listener.accept().await else {
                    break;
                };
                let mut req = Vec::new();
                s.read_to_end(&mut req).await.unwrap();
                let cmd_str = String::from_utf8_lossy(&req).into_owned();
                received_srv.lock().unwrap().push(cmd_str.clone());

                let reply = if cmd_str == "j/clients" {
                    let cmds_so_far = received_srv.lock().unwrap().len();
                    if cmds_so_far <= 1 {
                        r#"[{"address":"0x1111","pid":100,"class":"kitty","initialClass":"kitty","title":"waish","workspace":{"id":2,"name":"2"}}]"#
                    } else {
                        r#"[
                            {"address":"0x1111","pid":100,"class":"kitty","initialClass":"kitty","title":"waish","workspace":{"id":2,"name":"2"}},
                            {"address":"0xdeadbeef1234","pid":4321,"class":"DesktopEditors","initialClass":"DesktopEditors","title":"Budget.xlsx - ONLYOFFICE","workspace":{"id":6,"name":"6"}}
                        ]"#
                    }
                } else {
                    "ok"
                };
                let _ = s.write_all(reply.as_bytes()).await;
            }
        });

        let paths = HyprSocketPaths {
            command_sock: cmd_sock,
            event_sock: evt_sock,
        };
        let backend = HyprlandBackend::new();
        let trapped = backend
            .launch_and_trap_with_sockets(
                "onlyoffice-desktopeditors /tmp/Budget.xlsx",
                Some(6),
                Some("onlyoffice-desktopeditors"),
                Duration::from_secs(2),
                Some(&paths),
            )
            .await
            .expect("socket trapper should succeed")
            .expect("must return verified WindowInfo");

        assert_eq!(trapped.id, "0xdeadbeef1234");
        assert_eq!(trapped.workspace, 6);
        assert_eq!(trapped.class, "DesktopEditors");

        let cmds = received_cmds.lock().unwrap().clone();
        assert!(
            cmds.iter().any(|c| c.starts_with(
                "dispatch exec [workspace 6 silent] onlyoffice-desktopeditors /tmp/Budget.xlsx"
            )),
            "must launch via compositor IPC dispatch exec: {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| c == "dispatch movetoworkspacesilent 6,address:0xdeadbeef1234"),
            "must immediately relocate drifted window via movetoworkspacesilent: {cmds:?}"
        );

        let _ = std::fs::remove_dir_all(&sock_dir);
    }
}
