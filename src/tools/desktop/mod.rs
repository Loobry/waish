//! Cross-WM window management abstraction (`WindowBackend`), Hyprland IPC dispatchers, and Waybar state exporter.

pub mod hyprland;

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use hyprland::{
    close_client_async_with_fallback, hypr_dispatch_lua_or_legacy,
    hypr_dispatch_lua_or_legacy_sync, lua_string_literal, normalize_window_address,
    select_matching_windows, HyprlandBackend,
};

use super::workspace_tracker::{
    self, BrowserLaunchReport, HyprClient, SessionWorkspaceState, KNOWN_BROWSER_CLASSES,
};
use crate::cli::ui::Kaomoji;
use crate::config::settings::{DesktopConfig, DEFAULT_BOT_NAME};
use crate::wizard::detector::{self, DesktopBackend};

pub const WAYBAR_STATUS_PATH: &str = "/tmp/waish_status.json";

const BROWSER_CLASSES: &[&str] = &[
    "firefox",
    "firefox-developer-edition",
    "chromium",
    "google-chrome",
    "brave-browser",
    "zen",
    "zen-alpha",
    "zen-beta",
    "vivaldi-stable",
    "librewolf",
];

/// Desktop window metadata normalized across window managers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: String,
    pub class: String,
    #[serde(default)]
    pub initial_class: String,
    pub title: String,
    pub workspace: i32,
}

impl WindowInfo {
    pub fn is_spotify(&self) -> bool {
        let cls = self.class.trim().to_ascii_lowercase();
        let init = self.initial_class.trim().to_ascii_lowercase();
        matches!(cls.as_str(), "spotify" | "com.spotify.client")
            || matches!(init.as_str(), "spotify" | "com.spotify.client")
    }

    pub fn is_known_browser(&self) -> bool {
        for raw in [&self.class, &self.initial_class] {
            let low = raw.trim().to_ascii_lowercase();
            if low.is_empty() {
                continue;
            }
            if KNOWN_BROWSER_CLASSES
                .iter()
                .any(|(cls, _)| cls.eq_ignore_ascii_case(&low))
            {
                return true;
            }
        }
        false
    }

    pub fn format_verified_llm_report(&self) -> String {
        let norm_addr =
            normalize_window_address(&self.id).unwrap_or_else(|| format!("address:{}", self.id));
        let json_repr = serde_json::to_string(self).unwrap_or_default();
        format!(
            "status: success\n\
             verified_window: true\n\
             address: {norm_addr}\n\
             workspace_id: {}\n\
             class: {}\n\
             title: {}\n\
             do_not_poll_clients: true\n\
             window_info_json: {json_repr}",
            self.workspace, self.class, self.title
        )
    }
}

/// Cross-compositor workspace and window management interface.
pub trait WindowBackend: Send + Sync {
    fn get_active_workspace(&self) -> Result<i32, String>;
    fn list_workspaces(&self) -> Result<Vec<i32>, String>;
    fn list_windows(&self) -> Result<Vec<WindowInfo>, String>;
    fn close_window(&self, target: &str) -> Result<String, String>;
    fn close_workspaces(&self, workspace_ids: &[i32]) -> Result<String, String>;
    fn launch_app_in_workspace(
        &self,
        command: &str,
        workspace: i32,
    ) -> Result<Option<WindowInfo>, String>;
}

pub fn create_window_backend(kind: DesktopBackend) -> Box<dyn WindowBackend> {
    match kind {
        DesktopBackend::Hyprland => Box::new(HyprlandBackend::new()),
        DesktopBackend::Sway => Box::new(SwayBackend),
        DesktopBackend::Kde => Box::new(KdeBackend),
        DesktopBackend::Gnome => Box::new(GnomeBackend),
        DesktopBackend::Generic => Box::new(GenericBackend),
    }
}

pub fn active_window_backend(desktop_cfg: Option<&DesktopConfig>) -> Box<dyn WindowBackend> {
    let kind = desktop_cfg
        .map(DesktopConfig::effective_backend)
        .unwrap_or_else(detector::detect_desktop_backend);
    create_window_backend(kind)
}

/// Sway window manager backend (`swaymsg`).
#[derive(Debug, Default, Clone, Copy)]
pub struct SwayBackend;

impl SwayBackend {
    fn run_swaymsg(args: &[&str]) -> Result<String, String> {
        let out = Command::new("swaymsg")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("Gagal menjalankan `swaymsg`: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !out.status.success() {
            let detail = if stderr.is_empty() { stdout } else { stderr };
            return Err(format!("`swaymsg {}` gagal: {detail}", args.join(" ")));
        }
        Ok(stdout)
    }

    fn collect_tree_windows(node: &Value, current_ws: i32, out: &mut Vec<WindowInfo>) {
        let mut ws = current_ws;
        if node.get("type").and_then(Value::as_str) == Some("workspace") {
            if let Some(num) = node.get("num").and_then(Value::as_i64) {
                ws = num as i32;
            }
        }
        if node.get("pid").and_then(Value::as_i64).is_some() {
            let id = node
                .get("id")
                .and_then(Value::as_i64)
                .map(|i| i.to_string())
                .unwrap_or_default();
            let app_id = node
                .get("app_id")
                .and_then(Value::as_str)
                .or_else(|| {
                    node.get("window_properties")
                        .and_then(|wp| wp.get("class"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("")
                .to_string();
            let title = node
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !id.is_empty() {
                out.push(WindowInfo {
                    id,
                    class: app_id.clone(),
                    initial_class: app_id,
                    title,
                    workspace: ws,
                });
            }
        }
        for key in ["nodes", "floating_nodes"] {
            if let Some(children) = node.get(key).and_then(Value::as_array) {
                for child in children {
                    Self::collect_tree_windows(child, ws, out);
                }
            }
        }
    }
}

impl WindowBackend for SwayBackend {
    fn get_active_workspace(&self) -> Result<i32, String> {
        let raw = Self::run_swaymsg(&["-t", "get_workspaces"])?;
        let arr: Vec<Value> = serde_json::from_str(&raw)
            .map_err(|e| format!("Gagal mem-parsing `swaymsg -t get_workspaces`: {e}"))?;
        arr.iter()
            .find(|w| w.get("focused").and_then(Value::as_bool) == Some(true))
            .and_then(|w| w.get("num").and_then(Value::as_i64))
            .map(|n| n as i32)
            .ok_or_else(|| "Workspace aktif Sway tidak ditemukan.".to_string())
    }

    fn list_workspaces(&self) -> Result<Vec<i32>, String> {
        let raw = Self::run_swaymsg(&["-t", "get_workspaces"])?;
        let arr: Vec<Value> = serde_json::from_str(&raw)
            .map_err(|e| format!("Gagal mem-parsing `swaymsg -t get_workspaces`: {e}"))?;
        let set: BTreeSet<i32> = arr
            .iter()
            .filter_map(|w| w.get("num").and_then(Value::as_i64))
            .map(|n| n as i32)
            .filter(|&id| id > 0)
            .collect();
        Ok(set.into_iter().collect())
    }

    fn list_windows(&self) -> Result<Vec<WindowInfo>, String> {
        let raw = Self::run_swaymsg(&["-t", "get_tree"])?;
        let root: Value = serde_json::from_str(&raw)
            .map_err(|e| format!("Gagal mem-parsing `swaymsg -t get_tree`: {e}"))?;
        let mut windows = Vec::new();
        Self::collect_tree_windows(&root, 1, &mut windows);
        Ok(windows)
    }

    fn close_window(&self, target: &str) -> Result<String, String> {
        let windows = self.list_windows()?;
        let matched = select_matching_windows(&windows, None, Some(target));
        if matched.is_empty() {
            return Err(format!(
                "Tidak ditemukan jendela Sway yang cocok dengan target `{target}`."
            ));
        }
        let mut closed = 0usize;
        for win in matched {
            let criteria = format!("[con_id={}] kill", win.id);
            if Self::run_swaymsg(&[&criteria]).is_ok() {
                closed += 1;
            }
        }
        if closed > 0 {
            Ok(format!("Berhasil menutup {closed} jendela pada Sway."))
        } else {
            Err(format!("Gagal menutup jendela `{target}` pada Sway."))
        }
    }

    fn close_workspaces(&self, workspace_ids: &[i32]) -> Result<String, String> {
        let active_ws = self.get_active_workspace().ok();
        let windows = self.list_windows()?;
        let (to_close, protected) =
            hyprland::select_batch_close_targets(&windows, workspace_ids, active_ws);
        let mut closed = 0usize;
        for win in &to_close {
            let criteria = format!("[con_id={}] kill", win.id);
            if Self::run_swaymsg(&[&criteria]).is_ok() {
                closed += 1;
            }
        }
        Ok(format!(
            "Batch close Sway selesai: {closed} jendela ditutup (dilindungi: {}).",
            protected.len()
        ))
    }

    fn launch_app_in_workspace(
        &self,
        command: &str,
        workspace: i32,
    ) -> Result<Option<WindowInfo>, String> {
        let exec_cmd = if workspace > 0 {
            format!("workspace number {workspace}; exec {command}")
        } else {
            format!("exec {command}")
        };
        Self::run_swaymsg(&[&exec_cmd]).map(|_| None)
    }
}

fn spawn_detached_posix(command: &str) -> Result<Option<WindowInfo>, String> {
    let mut cmd = Command::new(crate::executor::pty::resolve_posix_shell());
    cmd.args(["-c", command])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::tools::runner::spawn_in_isolated_process_group(&mut cmd)
        .map(|_| None)
        .map_err(|e| format!("Gagal meluncurkan aplikasi `{command}`: {e}"))
}

fn list_windows_via_wmctrl() -> Result<Vec<WindowInfo>, String> {
    let Ok(out) = Command::new("wmctrl")
        .args(["-lx"])
        .stdin(Stdio::null())
        .output()
    else {
        return Ok(Vec::new());
    };
    if !out.status.success() {
        return Ok(Vec::new());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut windows = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        let id = parts[0].to_string();
        let ws = parts[1].parse::<i32>().unwrap_or(0) + 1;
        let class = parts[2]
            .split('.')
            .next_back()
            .unwrap_or(parts[2])
            .to_string();
        let title = if parts.len() > 4 {
            parts[4..].join(" ")
        } else {
            String::new()
        };
        windows.push(WindowInfo {
            id,
            class: class.clone(),
            initial_class: class,
            title,
            workspace: ws.max(1),
        });
    }
    Ok(windows)
}

fn close_window_via_wmctrl(target: &str, backend_label: &str) -> Result<String, String> {
    let windows = list_windows_via_wmctrl()?;
    let matched = select_matching_windows(&windows, None, Some(target));
    if matched.is_empty() {
        return Err(format!(
            "Tidak ditemukan jendela `{target}` pada backend {backend_label} (pastikan `wmctrl` terpasang untuk kontrol jendela EWMH)."
        ));
    }
    let mut closed = 0usize;
    for win in matched {
        if Command::new("wmctrl")
            .args(["-i", "-c", &win.id])
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            closed += 1;
        }
    }
    if closed > 0 {
        Ok(format!(
            "Berhasil menutup {closed} jendela pada backend {backend_label}."
        ))
    } else {
        Err(format!("Gagal menutup jendela `{target}` pada {backend_label}."))
    }
}

fn close_workspaces_via_wmctrl(workspace_ids: &[i32], backend_label: &str) -> Result<String, String> {
    let windows = list_windows_via_wmctrl()?;
    let (to_close, protected) =
        hyprland::select_batch_close_targets(&windows, workspace_ids, Some(1));
    let mut closed = 0usize;
    for win in &to_close {
        if Command::new("wmctrl")
            .args(["-i", "-c", &win.id])
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            closed += 1;
        }
    }
    Ok(format!(
        "Batch close {backend_label} selesai: {closed} jendela ditutup (dilindungi: {}).",
        protected.len()
    ))
}

/// KDE Plasma backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct KdeBackend;

impl WindowBackend for KdeBackend {
    fn get_active_workspace(&self) -> Result<i32, String> {
        Ok(1)
    }

    fn list_workspaces(&self) -> Result<Vec<i32>, String> {
        Ok(vec![1])
    }

    fn list_windows(&self) -> Result<Vec<WindowInfo>, String> {
        list_windows_via_wmctrl()
    }

    fn close_window(&self, target: &str) -> Result<String, String> {
        close_window_via_wmctrl(target, "KDE")
    }

    fn close_workspaces(&self, workspace_ids: &[i32]) -> Result<String, String> {
        close_workspaces_via_wmctrl(workspace_ids, "KDE")
    }

    fn launch_app_in_workspace(
        &self,
        command: &str,
        _workspace: i32,
    ) -> Result<Option<WindowInfo>, String> {
        spawn_detached_posix(command)
    }
}

/// GNOME Shell backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct GnomeBackend;

impl WindowBackend for GnomeBackend {
    fn get_active_workspace(&self) -> Result<i32, String> {
        Ok(1)
    }

    fn list_workspaces(&self) -> Result<Vec<i32>, String> {
        Ok(vec![1])
    }

    fn list_windows(&self) -> Result<Vec<WindowInfo>, String> {
        list_windows_via_wmctrl()
    }

    fn close_window(&self, target: &str) -> Result<String, String> {
        close_window_via_wmctrl(target, "GNOME")
    }

    fn close_workspaces(&self, workspace_ids: &[i32]) -> Result<String, String> {
        close_workspaces_via_wmctrl(workspace_ids, "GNOME")
    }

    fn launch_app_in_workspace(
        &self,
        command: &str,
        _workspace: i32,
    ) -> Result<Option<WindowInfo>, String> {
        spawn_detached_posix(command)
    }
}

/// Generic Linux desktop fallback backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct GenericBackend;

impl WindowBackend for GenericBackend {
    fn get_active_workspace(&self) -> Result<i32, String> {
        Ok(1)
    }

    fn list_workspaces(&self) -> Result<Vec<i32>, String> {
        Ok(vec![1])
    }

    fn list_windows(&self) -> Result<Vec<WindowInfo>, String> {
        list_windows_via_wmctrl()
    }

    fn close_window(&self, target: &str) -> Result<String, String> {
        close_window_via_wmctrl(target, "Generic")
    }

    fn close_workspaces(&self, workspace_ids: &[i32]) -> Result<String, String> {
        close_workspaces_via_wmctrl(workspace_ids, "Generic")
    }

    fn launch_app_in_workspace(
        &self,
        command: &str,
        _workspace: i32,
    ) -> Result<Option<WindowInfo>, String> {
        spawn_detached_posix(command)
    }
}

/// Companion visual state exported to Waybar custom JSON module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaybarState {
    Idle,
    Active,
    Busy,
    Media,
    Warning,
    Success,
}

impl WaybarState {
    pub fn kaomoji(self) -> &'static str {
        match self {
            WaybarState::Idle => Kaomoji::Greet.as_str(),
            WaybarState::Active => Kaomoji::Ready.as_str(),
            WaybarState::Busy => Kaomoji::Busy.as_str(),
            WaybarState::Media => Kaomoji::Music.as_str(),
            WaybarState::Warning => Kaomoji::Angry.as_str(),
            WaybarState::Success => Kaomoji::Success.as_str(),
        }
    }

    pub fn alt(self) -> &'static str {
        match self {
            WaybarState::Idle => "idle",
            WaybarState::Active => "active",
            WaybarState::Busy => "busy",
            WaybarState::Media => "media",
            WaybarState::Warning => "warning",
            WaybarState::Success => "success",
        }
    }

    pub fn css_class(self) -> &'static str {
        match self {
            WaybarState::Idle => "idle",
            WaybarState::Active => "normal",
            WaybarState::Busy => "busy",
            WaybarState::Media => "media",
            WaybarState::Warning => "warning",
            WaybarState::Success => "success",
        }
    }
}

/// Waybar custom module JSON payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaybarStatusPayload {
    pub text: String,
    pub alt: String,
    pub tooltip: String,
    pub class: String,
}

impl WaybarStatusPayload {
    pub fn build_with_kaomoji(
        state: WaybarState,
        bot_name: &str,
        kaomoji_override: Option<&str>,
        tooltip_override: Option<&str>,
    ) -> Self {
        let name = capitalize_bot(bot_name);
        let tooltip = match tooltip_override.map(str::trim).filter(|s| !s.is_empty()) {
            Some(custom) => custom.to_string(),
            None => match state {
                WaybarState::Idle => format!("{name} standby"),
                WaybarState::Active => {
                    format!("{name} is actively watching your Arch Linux system.")
                }
                WaybarState::Busy => "Sedang bekerja...".to_string(),
                WaybarState::Media => format!("{name} is listening to music."),
                WaybarState::Warning => "Perhatian sistem!".to_string(),
                WaybarState::Success => format!("{name} completed the last task cleanly."),
            },
        };

        let text = kaomoji_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| state.kaomoji())
            .to_string();

        Self {
            text,
            alt: state.alt().to_string(),
            tooltip,
            class: state.css_class().to_string(),
        }
    }
}

fn capitalize_bot(raw: &str) -> String {
    let trimmed = raw.trim();
    let base = if trimmed.is_empty() {
        DEFAULT_BOT_NAME
    } else {
        trimmed
    };
    let mut chars = base.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => DEFAULT_BOT_NAME.to_string(),
    }
}

pub fn export_waybar_status_to_path(
    path: &Path,
    state: WaybarState,
    bot_name: &str,
    tooltip_override: Option<&str>,
) -> io::Result<()> {
    export_waybar_status_to_path_with_kaomoji(path, state, bot_name, None, tooltip_override)
}

pub fn export_waybar_status_to_path_with_kaomoji(
    path: &Path,
    state: WaybarState,
    bot_name: &str,
    kaomoji_override: Option<&str>,
    tooltip_override: Option<&str>,
) -> io::Result<()> {
    let payload =
        WaybarStatusPayload::build_with_kaomoji(state, bot_name, kaomoji_override, tooltip_override);
    let json = serde_json::to_string_pretty(&payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path tanpa parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path tanpa nama file"))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));

    let _ = fs::remove_file(&tmp);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&tmp)?;
    file.write_all(json.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;

    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

pub fn export_waybar_status(state: WaybarState, bot_name: &str, tooltip_override: Option<&str>) {
    let _ = export_waybar_status_to_path(
        Path::new(WAYBAR_STATUS_PATH),
        state,
        bot_name,
        tooltip_override,
    );
}

pub fn export_waybar_status_with_kaomoji(
    state: WaybarState,
    bot_name: &str,
    kaomoji: &str,
    tooltip_override: Option<&str>,
) {
    let _ = export_waybar_status_to_path_with_kaomoji(
        Path::new(WAYBAR_STATUS_PATH),
        state,
        bot_name,
        Some(kaomoji),
        tooltip_override,
    );
}

pub fn sync_waybar_for_media_status_with_kaomoji(
    bot_name: &str,
    is_playing: bool,
    kaomoji_override: Option<&str>,
    track_info: Option<&str>,
) {
    if is_playing {
        let tooltip = match track_info.map(str::trim).filter(|s| !s.is_empty()) {
            Some(track) => format!("Now Playing: {track}"),
            None => "Now Playing".to_string(),
        };
        let _ = export_waybar_status_to_path_with_kaomoji(
            Path::new(WAYBAR_STATUS_PATH),
            WaybarState::Media,
            bot_name,
            kaomoji_override,
            Some(&tooltip),
        );
    } else {
        let _ = export_waybar_status_to_path_with_kaomoji(
            Path::new(WAYBAR_STATUS_PATH),
            WaybarState::Idle,
            bot_name,
            kaomoji_override,
            None,
        );
    }
}

pub fn is_waybar_config_path(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    lower.contains("/waybar/")
}

pub fn reload_waybar() -> crate::Result<String> {
    let output = Command::new("pkill")
        .args(["-SIGUSR2", "waybar"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Gagal menjalankan `pkill -SIGUSR2 waybar`: {e}"))?;

    if output.status.success() {
        Ok("Waybar berhasil di-reload via `pkill -SIGUSR2 waybar`.".to_string())
    } else {
        Ok("Proses `waybar` tidak sedang berjalan (reload dilewati).".to_string())
    }
}

pub fn reload_waybar_if_applicable(path: &Path) -> Option<String> {
    if !is_waybar_config_path(path) {
        return None;
    }
    match reload_waybar() {
        Ok(msg) => Some(msg),
        Err(err) => Some(format!("Peringatan reload Waybar: {err}")),
    }
}

pub async fn launch_browser_in_workspace_with_config(
    url: &str,
    target_ws: i32,
    desktop_cfg: Option<&DesktopConfig>,
) -> Result<BrowserLaunchReport, String> {
    let backend_kind = desktop_cfg
        .map(DesktopConfig::effective_backend)
        .unwrap_or_else(detector::detect_desktop_backend);
    let backend = create_window_backend(backend_kind);

    let windows_before = backend.list_windows().unwrap_or_default();
    let existing_ids: HashSet<String> = windows_before.iter().map(|w| w.id.clone()).collect();
    let clients_before: Vec<HyprClient> = windows_before
        .iter()
        .map(|w| HyprClient {
            address: w.id.clone(),
            class: w.class.clone(),
            initial_class: w.initial_class.clone(),
            title: w.title.clone(),
            workspace: workspace_tracker::HyprClientWorkspace {
                id: w.workspace,
                name: w.workspace.to_string(),
            },
        })
        .collect();

    let browsers = workspace_tracker::detect_active_browsers(&clients_before);
    let default_browser = if browsers.is_empty() {
        workspace_tracker::detect_installed_browser_binary()
    } else {
        None
    };
    let (launch_command, used_new_window) =
        workspace_tracker::build_browser_launch_command_with_default(
            url,
            target_ws,
            &browsers,
            default_browser.as_deref(),
        );

    let trapped_win = backend.launch_app_in_workspace(&launch_command, target_ws)?;

    if trapped_win.is_none() && backend_kind == DesktopBackend::Hyprland && target_ws > 0 {
        let hypr = HyprlandBackend::new();
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let Ok(windows_now) = hypr.list_windows() else {
                break;
            };
            let new_win = windows_now
                .iter()
                .find(|w| !existing_ids.contains(&w.id) && w.is_known_browser())
                .or_else(|| windows_now.iter().find(|w| !existing_ids.contains(&w.id)));

            if let Some(win) = new_win {
                let win_id = win.id.clone();
                if win.workspace != target_ws {
                    let _ = hypr.move_window_silent(&win_id, target_ws).await;
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    if hypr.list_windows().is_ok_and(|settled| {
                        settled
                            .iter()
                            .any(|w| w.id == win_id && w.workspace != target_ws)
                    }) {
                        let _ = hypr.move_window_silent(&win_id, target_ws).await;
                    }
                }
                break;
            }
        }
    }

    Ok(BrowserLaunchReport {
        target_workspace: target_ws,
        launch_command,
        used_new_window,
    })
}

/// Arguments for `manage_workspace`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManageWorkspaceArgs {
    pub action: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub workspaces: Vec<i32>,
}

impl ManageWorkspaceArgs {
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        let mut parsed: Self = serde_json::from_str(raw)
            .map_err(|e| format!("argumen JSON manage_workspace tidak valid: {e}"))?;
        parsed.action = parsed.action.trim().to_ascii_lowercase();
        parsed.target = parsed.target.trim().to_string();
        parsed.command = parsed.command.trim().to_string();
        if parsed.action.is_empty() {
            return Err("field `action` kosong".to_string());
        }
        Ok(parsed)
    }

    pub fn effective_workspaces(&self) -> Vec<i32> {
        let mut set = BTreeSet::new();
        for &ws in &self.workspaces {
            if ws > 0 {
                set.insert(ws);
            }
        }
        if set.is_empty() && !self.target.trim().is_empty() {
            for tok in self
                .target
                .split(|c: char| !c.is_ascii_digit())
                .filter(|s| !s.is_empty())
            {
                if let Some(ws) = tok.parse::<i32>().ok().filter(|&w| w > 0) {
                    set.insert(ws);
                }
            }
        }
        set.into_iter().collect()
    }
}

fn format_lua_workspace_value(target: &str) -> String {
    let trimmed = target.trim();
    if let Ok(id) = trimmed.parse::<i32>() {
        id.to_string()
    } else {
        lua_string_literal(trimmed)
    }
}

/// Arguments for `launch_gui_app`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchGuiAppArgs {
    pub command: String,
    #[serde(default)]
    pub workspace: Option<i32>,
}

impl LaunchGuiAppArgs {
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        let mut parsed: Self = serde_json::from_str(raw)
            .map_err(|e| format!("argumen JSON launch_gui_app tidak valid: {e}"))?;
        parsed.command = parsed.command.trim().to_string();
        if parsed.command.is_empty() {
            return Err("field `command` wajib diisi pada launch_gui_app".to_string());
        }
        Ok(parsed)
    }
}

pub async fn execute_launch_gui_app_with_config(
    args: &LaunchGuiAppArgs,
    ws_state: &SessionWorkspaceState,
    desktop_cfg: Option<&DesktopConfig>,
) -> crate::Result<String> {
    let backend_kind = desktop_cfg
        .map(DesktopConfig::effective_backend)
        .unwrap_or_else(detector::detect_desktop_backend);
    let backend = create_window_backend(backend_kind);
    let target_ws = workspace_tracker::resolve_target_workspace_via_backend(
        args.workspace.filter(|&w| w > 0),
        ws_state,
        backend.as_ref(),
    )
    .unwrap_or(2);

    let maybe_win = if backend_kind == DesktopBackend::Hyprland {
        HyprlandBackend::new()
            .launch_app_in_workspace_async(&args.command, target_ws)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?
    } else {
        backend
            .launch_app_in_workspace(&args.command, target_ws)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?
    };

    if let Some(win_info) = maybe_win {
        Ok(win_info.format_verified_llm_report())
    } else {
        Ok(format!(
            "status: launched_pending_window\n\
             verified_window: false\n\
             target_workspace: {target_ws}\n\
             command: {}\n\
             do_not_poll_clients: true\n\
             note: Aplikasi telah di-spawn melalui kompositor ke workspace {target_ws} (Window Trapper selesai; jangan jalankan sleep/hyprctl clients/pgrep manual).",
            args.command
        ))
    }
}

pub fn execute_manage_workspace_with_config(
    args: &ManageWorkspaceArgs,
    ws_state: &SessionWorkspaceState,
    desktop_cfg: Option<&DesktopConfig>,
) -> crate::Result<String> {
    match args.action.as_str() {
        "launch_app" | "launch" => {
            if args.command.is_empty() {
                return Err(
                    "Field `command` (contoh: `/usr/bin/puzzles-mines`) wajib diisi untuk action `launch_app`."
                        .into(),
                );
            }
            let explicit_ws = args
                .target
                .parse::<i32>()
                .ok()
                .filter(|&w| w > 0)
                .or_else(|| args.workspaces.iter().copied().find(|&w| w > 0));
            let backend = active_window_backend(desktop_cfg);
            let target_ws = workspace_tracker::resolve_target_workspace_via_backend(
                explicit_ws,
                ws_state,
                backend.as_ref(),
            )
            .unwrap_or(2);

            let maybe_win = backend
                .launch_app_in_workspace(&args.command, target_ws)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

            if let Some(win_info) = maybe_win {
                Ok(win_info.format_verified_llm_report())
            } else {
                Ok(format!(
                    "status: launched_pending_window\n\
                     verified_window: false\n\
                     target_workspace: {target_ws}\n\
                     command: {}\n\
                     do_not_poll_clients: true\n\
                     note: Proses telah di-spawn ke workspace {target_ws} (Window Trapper selesai; jangan jalankan sleep/hyprctl clients/pgrep manual).",
                    args.command
                ))
            }
        }
        "switch" => {
            if args.target.is_empty() {
                return Err("`target` workspace wajib diisi untuk action `switch`.".into());
            }
            let ws_val = format_lua_workspace_value(&args.target);
            let lua = format!("hl.dispatch(hl.dsp.focus({{ workspace = {ws_val} }}))");
            let out = hypr_dispatch_lua_or_legacy_sync(
                &lua,
                &["dispatch", "workspace", &args.target],
            )
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            Ok(format!(
                "Berpindah ke Hyprland workspace `{}` ({}).",
                args.target,
                out.trim()
            ))
        }
        "move_window" => {
            if args.target.is_empty() {
                return Err("`target` workspace wajib diisi untuk action `move_window`.".into());
            }
            let ws_val = format_lua_workspace_value(&args.target);
            let lua = format!("hl.dispatch(hl.dsp.window.move({{ workspace = {ws_val} }}))");
            let out = hypr_dispatch_lua_or_legacy_sync(
                &lua,
                &["dispatch", "movetoworkspace", &args.target],
            )
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            Ok(format!(
                "Jendela aktif dipindahkan ke workspace `{}` ({}).",
                args.target,
                out.trim()
            ))
        }
        "list_clients" => {
            let clients_json = hyprland::run_hyprctl_sync(&["clients", "-j"])
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            Ok(summarize_hyprland_clients(&clients_json))
        }
        "set_preset" => apply_hyprland_preset(&args.target),
        "close_batch" => {
            let ws_ids = args.effective_workspaces();
            if ws_ids.is_empty() {
                return Err(
                    "Field `workspaces` (array angka positif, contoh `[2, 3, 5, 6]`) wajib diisi untuk action `close_batch`."
                        .into(),
                );
            }
            let backend = active_window_backend(desktop_cfg);
            let mut summary = backend
                .close_workspaces(&ws_ids)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            if ws_state.get().is_some_and(|c| ws_ids.contains(&c)) {
                ws_state.reset();
                summary.push_str("\nCompanion workspace telah direset otomatis.");
            }
            Ok(summary)
        }
        other => Err(format!(
            "Aksi `{other}` tidak dikenal pada manage_workspace. Gunakan: switch, move_window, set_preset, list_clients, atau close_batch."
        )
        .into()),
    }
}

fn apply_hyprland_preset(preset: &str) -> crate::Result<String> {
    match preset.trim().to_ascii_lowercase().as_str() {
        "coding" => {
            let mut log = Vec::new();
            if let Ok(clients_raw) = hyprland::run_hyprctl_sync(&["clients", "-j"]) {
                for addr in find_browser_addresses(&clients_raw) {
                    let norm_addr = normalize_window_address(&addr)
                        .unwrap_or_else(|| format!("address:{addr}"));
                    let lua = format!(
                        "hl.dispatch(hl.dsp.window.move({{ workspace = 2, silent = true, window = {} }}))",
                        lua_string_literal(&norm_addr)
                    );
                    let rule = format!("2,{norm_addr}");
                    if hypr_dispatch_lua_or_legacy_sync(
                        &lua,
                        &["dispatch", "movetoworkspacesilent", &rule],
                    )
                    .is_ok()
                    {
                        log.push(format!("memindahkan browser ({addr}) ke workspace 2"));
                    }
                }
            }
            HyprlandBackend::new()
                .launch_app_in_workspace("kitty", 0)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            log.push("meluncurkan terminal `kitty` pada workspace aktif".to_string());
            Ok(format!("Preset `coding` selesai diterapkan: {}.", log.join("; ")))
        }
        "floating" => {
            let out = hypr_dispatch_lua_or_legacy_sync(
                "hl.dispatch(hl.dsp.window.float({ action = \"toggle\" }))",
                &["dispatch", "togglefloating"],
            )
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            Ok(format!(
                "Preset `floating` (togglefloating) diterapkan pada jendela aktif ({}).",
                out.trim()
            ))
        }
        other => Err(format!(
            "Preset Hyprland `{other}` tidak dikenal. Preset bawaan tersedia: `coding`, `floating`."
        )
        .into()),
    }
}

pub fn find_browser_addresses(clients_json: &str) -> Vec<String> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(clients_json) else {
        return Vec::new();
    };
    let mut addrs = Vec::new();
    for item in items {
        let class = item
            .get("class")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        if BROWSER_CLASSES.iter().any(|b| class.contains(b)) {
            if let Some(addr) = item.get("address").and_then(Value::as_str) {
                addrs.push(addr.to_string());
            }
        }
    }
    addrs
}

pub fn summarize_hyprland_clients(clients_json: &str) -> String {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(clients_json) else {
        return "Tidak ada data client Hyprland yang valid.".to_string();
    };
    if items.is_empty() {
        return "Tidak ada jendela aktif di Hyprland.".to_string();
    }

    let mut lines = Vec::new();
    for item in items {
        let ws = item
            .get("workspace")
            .and_then(|w| w.get("id"))
            .and_then(Value::as_i64)
            .map(|id| id.to_string())
            .unwrap_or_else(|| "?".to_string());
        let class = item.get("class").and_then(Value::as_str).unwrap_or("unknown");
        let title = item.get("title").and_then(Value::as_str).unwrap_or("");
        let floating = item
            .get("floating")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mode = if floating { "floating" } else { "tiled" };
        lines.push(format!("[ws:{ws}] {class} ({mode}) - {title}"));
    }
    lines.join("\n")
}

/// Arguments for `manage_window`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManageWindowArgs {
    pub action: String,
    #[serde(default)]
    pub workspace: Option<i32>,
    #[serde(default)]
    pub window_class: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub destination_ws: Option<i32>,
}

impl ManageWindowArgs {
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        let mut parsed: Self = serde_json::from_str(raw)
            .map_err(|e| format!("argumen JSON manage_window tidak valid: {e}"))?;
        parsed.action = parsed.action.trim().to_ascii_lowercase();
        parsed.window_class = parsed
            .window_class
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        parsed.target = parsed
            .target
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if parsed.action.is_empty() {
            return Err("field `action` kosong".to_string());
        }
        Ok(parsed)
    }

    pub fn effective_selector(&self) -> Option<&str> {
        self.window_class.as_deref().or(self.target.as_deref())
    }
}

pub fn select_target_clients<'a>(
    clients: &'a [HyprClient],
    workspace: Option<i32>,
    selector: Option<&str>,
) -> Vec<&'a HyprClient> {
    if let Some(norm_addr) = selector.and_then(normalize_window_address) {
        let matched: Vec<&HyprClient> = clients
            .iter()
            .filter(|c| normalize_window_address(&c.address).as_deref() == Some(norm_addr.as_str()))
            .filter(|c| workspace.is_none_or(|ws| c.workspace.id == ws))
            .collect();
        if !matched.is_empty() {
            return matched;
        }
    }

    let base: Vec<&HyprClient> = clients
        .iter()
        .filter(|c| c.workspace.id > 0)
        .filter(|c| workspace.is_none_or(|ws| c.workspace.id == ws))
        .collect();

    let Some(sel) = selector.map(str::trim).filter(|s| !s.is_empty()) else {
        return base;
    };

    let needle = sel.to_ascii_lowercase();
    let by_class: Vec<&HyprClient> = base
        .iter()
        .copied()
        .filter(|c| {
            c.class.to_ascii_lowercase().contains(&needle)
                || c.initial_class.to_ascii_lowercase().contains(&needle)
        })
        .collect();
    if !by_class.is_empty() {
        return by_class;
    }

    let by_title: Vec<&HyprClient> = base
        .iter()
        .copied()
        .filter(|c| c.title.to_ascii_lowercase().contains(&needle))
        .collect();
    if !by_title.is_empty() {
        return by_title;
    }

    if let Ok(re) = RegexBuilder::new(sel).case_insensitive(true).build() {
        return base
            .into_iter()
            .filter(|c| {
                re.is_match(&c.class) || re.is_match(&c.initial_class) || re.is_match(&c.title)
            })
            .collect();
    }

    Vec::new()
}

pub fn reconcile_companion_workspace_after_removal(
    clients_before: &[HyprClient],
    removed_addrs: &HashSet<String>,
    ws_state: &SessionWorkspaceState,
) -> bool {
    let Some(companion_ws) = ws_state.get() else {
        return false;
    };
    let still_has_windows = clients_before
        .iter()
        .any(|c| c.workspace.id == companion_ws && !removed_addrs.contains(&c.address));
    if !still_has_windows {
        ws_state.reset();
        true
    } else {
        false
    }
}

pub async fn execute_manage_window_with_config(
    args: &ManageWindowArgs,
    ws_state: &SessionWorkspaceState,
    desktop_cfg: Option<&DesktopConfig>,
) -> crate::Result<String> {
    let backend_kind = desktop_cfg
        .map(DesktopConfig::effective_backend)
        .unwrap_or_else(detector::detect_desktop_backend);

    if backend_kind != DesktopBackend::Hyprland && args.action == "close" {
        let backend = create_window_backend(backend_kind);
        let target = args.effective_selector().unwrap_or("");
        if !target.is_empty() {
            return backend
                .close_window(target)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() });
        }
    }

    let clients = workspace_tracker::query_hypr_clients()
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
    let selector = args.effective_selector();
    let targets = select_target_clients(&clients, args.workspace, selector);

    if targets.is_empty() {
        let scope = match (args.workspace, selector) {
            (Some(ws), Some(sel)) => format!("target `{sel}` di workspace {ws}"),
            (Some(ws), None) => format!("di workspace {ws}"),
            (None, Some(sel)) => format!("target `{sel}`"),
            (None, None) => "pada workspace aktif".to_string(),
        };
        return Ok(format!(
            "Tidak ditemukan jendela yang cocok ({scope}) untuk aksi `{}`.",
            args.action
        ));
    }

    match args.action.as_str() {
        "close" => {
            let mut closed_addrs = HashSet::new();
            let mut summary = Vec::new();
            let mut errors = Vec::new();
            for client in &targets {
                match close_client_async_with_fallback(client).await {
                    Ok(method) => {
                        closed_addrs.insert(client.address.clone());
                        let title_part = if client.title.trim().is_empty() {
                            String::new()
                        } else {
                            format!(" - \"{}\"", client.title.trim())
                        };
                        summary.push(format!(
                            "[ws:{}] {}{} ({}, {})",
                            client.workspace.id, client.class, title_part, client.address, method
                        ));
                    }
                    Err(err) => errors.push(err),
                }
            }

            if summary.is_empty() {
                return Err(errors.join("; ").into());
            }

            let companion_reset =
                reconcile_companion_workspace_after_removal(&clients, &closed_addrs, ws_state);

            let mut msg = format!(
                "Berhasil menutup {} jendela:\n- {}",
                summary.len(),
                summary.join("\n- ")
            );
            if !errors.is_empty() {
                msg.push_str(&format!("\nSebagian gagal: {}", errors.join("; ")));
            }
            if companion_reset {
                msg.push_str("\nCompanion workspace kini kosong dan telah direset otomatis.");
            }
            Ok(msg)
        }
        "focus" => {
            let client = targets[0];
            let target_arg = normalize_window_address(&client.address)
                .unwrap_or_else(|| format!("address:{}", client.address));
            let lua = format!(
                "hl.dispatch(hl.dsp.focus({{ window = {} }}))",
                lua_string_literal(&target_arg)
            );
            hypr_dispatch_lua_or_legacy(&lua, &["dispatch", "focuswindow", &target_arg])
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            Ok(format!(
                "Memfokuskan jendela `{}` ({}) di workspace {}.",
                client.class, client.address, client.workspace.id
            ))
        }
        "toggle_float" => {
            let mut toggled = Vec::new();
            for client in &targets {
                let target_arg = normalize_window_address(&client.address)
                    .unwrap_or_else(|| format!("address:{}", client.address));
                let lua = format!(
                    "hl.dispatch(hl.dsp.window.float({{ action = \"toggle\", window = {} }}))",
                    lua_string_literal(&target_arg)
                );
                hypr_dispatch_lua_or_legacy(&lua, &["dispatch", "togglefloating", &target_arg])
                    .await
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
                toggled.push(format!("[ws:{}] {}", client.workspace.id, client.class));
            }
            Ok(format!(
                "Mode floating di-toggle pada {} jendela: {}.",
                toggled.len(),
                toggled.join(", ")
            ))
        }
        "move_to_workspace" => {
            let Some(dest) = args.destination_ws.filter(|&w| w > 0) else {
                return Err(
                    "`destination_ws` (angka positif > 0) wajib diisi untuk aksi `move_to_workspace`."
                        .into(),
                );
            };
            let hypr = HyprlandBackend::new();
            let mut moved_addrs = HashSet::new();
            let mut moved = Vec::new();
            for client in &targets {
                hypr.move_window_silent(&client.address, dest)
                    .await
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
                if client.workspace.id != dest {
                    moved_addrs.insert(client.address.clone());
                }
                moved.push(format!(
                    "{} (ws {} -> ws {})",
                    client.class, client.workspace.id, dest
                ));
            }

            let companion_reset =
                reconcile_companion_workspace_after_removal(&clients, &moved_addrs, ws_state);
            let mut msg = format!(
                "Berhasil memindahkan {} jendela ke workspace {dest}: {}.",
                moved.len(),
                moved.join(", ")
            );
            if companion_reset {
                msg.push_str(" Companion workspace sebelumnya kini kosong dan telah direset.");
            }
            Ok(msg)
        }
        other => Err(format!(
            "Aksi `{other}` tidak dikenal pada manage_window. Gunakan: close, focus, toggle_float, atau move_to_workspace."
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_polymorphic_window_backends_for_all_desktops() {
        for kind in [
            DesktopBackend::Hyprland,
            DesktopBackend::Sway,
            DesktopBackend::Kde,
            DesktopBackend::Gnome,
            DesktopBackend::Generic,
        ] {
            let backend: Box<dyn WindowBackend> = create_window_backend(kind);
            if matches!(
                kind,
                DesktopBackend::Kde | DesktopBackend::Gnome | DesktopBackend::Generic
            ) {
                assert_eq!(backend.get_active_workspace().unwrap(), 1);
                assert_eq!(backend.list_workspaces().unwrap(), vec![1]);
            }
        }

        let sp = WindowInfo {
            id: "0xsp".to_string(),
            class: String::new(),
            initial_class: "Spotify".to_string(),
            title: "Spotify Premium".to_string(),
            workspace: 5,
        };
        assert!(sp.is_spotify());
        assert!(!sp.is_known_browser());

        let ff = WindowInfo {
            id: "0xff".to_string(),
            class: "firefox".to_string(),
            initial_class: "firefox".to_string(),
            title: "YouTube".to_string(),
            workspace: 5,
        };
        assert!(ff.is_known_browser());
        assert!(!ff.is_spotify());
    }

    #[test]
    fn exports_waybar_json_with_pure_kaomoji_and_classes() {
        let dir = std::env::temp_dir().join(format!("waish-waybar-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("waish_status.json");

        export_waybar_status_to_path(&path, WaybarState::Active, "alice", None).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let parsed: WaybarStatusPayload = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.text, "( •̀ ω •́ )✧");
        assert_eq!(parsed.alt, "active");
        assert_eq!(parsed.class, "normal");
        assert!(parsed.tooltip.contains("Alice"));

        export_waybar_status_to_path(&path, WaybarState::Busy, "alice", None).unwrap();
        let parsed_busy: WaybarStatusPayload =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed_busy.text, "( - _ - ) zZz");
        assert_eq!(parsed_busy.class, "busy");

        export_waybar_status_to_path(&path, WaybarState::Warning, "alice", None).unwrap();
        let parsed_warn: WaybarStatusPayload =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed_warn.text, "( •ˋ _ ˊ•)");
        assert_eq!(parsed_warn.class, "warning");

        export_waybar_status_to_path(
            &path,
            WaybarState::Media,
            "alice",
            Some("Now Playing: Yoru ni Kakeru - YOASOBI"),
        )
        .unwrap();
        let parsed_media: WaybarStatusPayload =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed_media.text, "( ˘ ▽ ˘ ) ♫");
        assert_eq!(parsed_media.alt, "media");
        assert_eq!(parsed_media.class, "media");
        assert_eq!(parsed_media.tooltip, "Now Playing: Yoru ni Kakeru - YOASOBI");

        export_waybar_status_to_path_with_kaomoji(
            &path,
            WaybarState::Active,
            "alice",
            Some("( ✧Д✧ )"),
            Some("Alice inspecting system"),
        )
        .unwrap();
        let parsed_dynamic: WaybarStatusPayload =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed_dynamic.text, "( ✧Д✧ )");
        assert_eq!(parsed_dynamic.tooltip, "Alice inspecting system");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn targeted_window_selector_filters_by_workspace_class_and_resets_companion_ws() {
        use super::workspace_tracker::HyprClientWorkspace;

        let clients = vec![
            HyprClient {
                address: "0x001".to_string(),
                class: "kitty".to_string(),
                initial_class: "kitty".to_string(),
                title: "fish".to_string(),
                workspace: HyprClientWorkspace {
                    id: 1,
                    name: "1".to_string(),
                },
            },
            HyprClient {
                address: "0x002".to_string(),
                class: "Spotify".to_string(),
                initial_class: "Spotify".to_string(),
                title: "Spotify Free".to_string(),
                workspace: HyprClientWorkspace {
                    id: 4,
                    name: "4".to_string(),
                },
            },
            HyprClient {
                address: "0x003".to_string(),
                class: "Brave-browser".to_string(),
                initial_class: "Brave-browser".to_string(),
                title: "Arch Wiki".to_string(),
                workspace: HyprClientWorkspace {
                    id: 4,
                    name: "4".to_string(),
                },
            },
            HyprClient {
                address: "0x004".to_string(),
                class: "scratchterm".to_string(),
                initial_class: "scratchterm".to_string(),
                title: "scratch".to_string(),
                workspace: HyprClientWorkspace {
                    id: -99,
                    name: "special:scratchpad".to_string(),
                },
            },
        ];

        let ws4_all = select_target_clients(&clients, Some(4), None);
        assert_eq!(ws4_all.len(), 2);

        let ws4_spotify = select_target_clients(&clients, Some(4), Some("spotify"));
        assert_eq!(ws4_spotify.len(), 1);
        assert_eq!(ws4_spotify[0].address, "0x002");

        let state = SessionWorkspaceState::new();
        state.set(4);
        let partial_closed = HashSet::from(["0x002".to_string()]);
        assert!(!reconcile_companion_workspace_after_removal(
            &clients,
            &partial_closed,
            &state
        ));
        assert_eq!(state.get(), Some(4));

        let all_ws4_closed = HashSet::from(["0x002".to_string(), "0x003".to_string()]);
        assert!(reconcile_companion_workspace_after_removal(
            &clients,
            &all_ws4_closed,
            &state
        ));
        assert_eq!(state.get(), None);
    }

    #[test]
    fn detects_waybar_config_paths_and_hyprland_browsers() {
        assert!(is_waybar_config_path(Path::new("/home/steve/.config/waybar/style.css")));
        assert!(is_waybar_config_path(Path::new("/home/steve/.config/waybar/config.jsonc")));
        assert!(!is_waybar_config_path(Path::new("/home/steve/.config/hypr/hyprland.conf")));

        let sample = r#"[
            {"address":"0x111","class":"kitty","title":"fish","workspace":{"id":1},"floating":false},
            {"address":"0x222","class":"firefox","title":"Arch Wiki","workspace":{"id":1},"floating":false}
        ]"#;
        assert_eq!(find_browser_addresses(sample), vec!["0x222".to_string()]);
        let summary = summarize_hyprland_clients(sample);
        assert!(summary.contains("[ws:1] kitty (tiled)"));
        assert!(summary.contains("[ws:1] firefox (tiled)"));
    }

    #[test]
    fn normalizes_window_addresses_and_matches_hex_or_regex_targets() {
        use super::workspace_tracker::HyprClientWorkspace;

        assert_eq!(
            normalize_window_address("0x56351FE90830"),
            Some("address:0x56351fe90830".to_string())
        );
        assert_eq!(
            normalize_window_address("address:0x00ab"),
            Some("address:0x00ab".to_string())
        );
        assert_eq!(normalize_window_address("kitty"), None);
        assert_eq!(normalize_window_address("0xZZZ"), None);

        let clients = vec![
            HyprClient {
                address: "0x56351fe90830".to_string(),
                class: "kitty".to_string(),
                initial_class: "kitty".to_string(),
                title: "waish ~/Projects/waish".to_string(),
                workspace: HyprClientWorkspace {
                    id: 4,
                    name: "4".to_string(),
                },
            },
            HyprClient {
                address: "0x56351fed6510".to_string(),
                class: "firefox".to_string(),
                initial_class: "firefox".to_string(),
                title: "Arch Linux News — Mozilla Firefox".to_string(),
                workspace: HyprClientWorkspace {
                    id: 5,
                    name: "5".to_string(),
                },
            },
        ];

        assert_eq!(
            select_target_clients(&clients, None, Some("0x56351fe90830")).len(),
            1
        );
        assert_eq!(
            select_target_clients(&clients, None, Some("address:0x56351FED6510"))[0].class,
            "firefox"
        );
        let regex_hit = select_target_clients(&clients, None, Some("Arch.*News"));
        assert_eq!(regex_hit.len(), 1);
        assert_eq!(regex_hit[0].address, "0x56351fed6510");

        let parsed = ManageWindowArgs::from_json_str(
            r#"{"action":"close","target":"0x56351fe90830"}"#,
        )
        .unwrap();
        assert_eq!(parsed.effective_selector(), Some("0x56351fe90830"));
    }
}
