//! Dynamic workspace allocation and active browser window inspection.

use std::collections::BTreeSet;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use serde::{Deserialize, Serialize};

use super::desktop::{self, hyprland, WindowBackend};
use crate::config::settings::DesktopConfig;
use crate::executor::pty::shell_single_quote;

/// Session-scoped state for companion workspace allocation and IPC launch verification.
#[derive(Debug)]
pub struct SessionWorkspaceState {
    companion_workspace: AtomicI32,
    turn_ipc_verified: AtomicBool,
}

impl SessionWorkspaceState {
    pub const UNSET: i32 = -1;

    pub fn new() -> Self {
        Self {
            companion_workspace: AtomicI32::new(Self::UNSET),
            turn_ipc_verified: AtomicBool::new(false),
        }
    }

    pub fn get(&self) -> Option<i32> {
        let val = self.companion_workspace.load(Ordering::Relaxed);
        (val > 0).then_some(val)
    }

    pub fn set(&self, ws: i32) {
        if ws > 0 {
            self.companion_workspace.store(ws, Ordering::Relaxed);
        }
    }

    pub fn reset(&self) {
        self.companion_workspace.store(Self::UNSET, Ordering::Relaxed);
    }

    pub fn clear_companion(&self) {
        self.reset();
    }

    pub fn mark_turn_ipc_verified(&self) {
        self.turn_ipc_verified.store(true, Ordering::SeqCst);
    }

    pub fn clear_turn_ipc_verified(&self) {
        self.turn_ipc_verified.store(false, Ordering::SeqCst);
    }

    pub fn is_turn_ipc_verified(&self) -> bool {
        self.turn_ipc_verified.load(Ordering::SeqCst)
    }
}

impl Default for SessionWorkspaceState {
    fn default() -> Self {
        Self::new()
    }
}

/// Workspace entry parsed from `hyprctl workspaces -j`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyprWorkspace {
    pub id: i32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub windows: i32,
}

/// Nested workspace descriptor on a Hyprland client JSON entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyprClientWorkspace {
    pub id: i32,
    #[serde(default)]
    pub name: String,
}

/// Client window entry parsed from `hyprctl clients -j`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyprClient {
    pub address: String,
    #[serde(default)]
    pub class: String,
    #[serde(default, rename = "initialClass")]
    pub initial_class: String,
    #[serde(default)]
    pub title: String,
    pub workspace: HyprClientWorkspace,
}

/// Active browser window detected across workspaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveBrowserInfo {
    pub binary: String,
    pub class_name: String,
    pub workspace_id: i32,
    pub address: String,
}

pub const KNOWN_BROWSER_CLASSES: &[(&str, &str)] = &[
    ("brave-browser", "brave"),
    ("Brave-browser", "brave"),
    ("firefox", "firefox"),
    ("org.mozilla.firefox", "firefox"),
    ("firefox-developer-edition", "firefox-developer-edition"),
    ("zen-alpha", "zen-browser"),
    ("zen-beta", "zen-browser"),
    ("zen-browser", "zen-browser"),
    ("zen", "zen-browser"),
    ("chromium", "chromium"),
    ("google-chrome", "google-chrome-stable"),
    ("Google-chrome", "google-chrome-stable"),
    ("librewolf", "librewolf"),
    ("floorp", "floorp"),
    ("vivaldi-stable", "vivaldi-stable"),
];

const FALLBACK_BROWSER_BINARIES: &[&str] = &[
    "firefox",
    "zen-browser",
    "brave",
    "chromium",
    "google-chrome-stable",
    "google-chrome",
    "librewolf",
    "floorp",
    "vivaldi-stable",
];

/// Summary of a browser launch dispatched to a target workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserLaunchReport {
    pub target_workspace: i32,
    pub launch_command: String,
    pub used_new_window: bool,
}

pub fn parse_workspaces_json(raw: &str) -> Result<Vec<HyprWorkspace>, String> {
    serde_json::from_str::<Vec<HyprWorkspace>>(raw)
        .map_err(|e| format!("Gagal mem-parsing output `hyprctl workspaces -j`: {e}"))
}

pub fn parse_clients_json(raw: &str) -> Result<Vec<HyprClient>, String> {
    serde_json::from_str::<Vec<HyprClient>>(raw)
        .map_err(|e| format!("Gagal mem-parsing output `hyprctl clients -j`: {e}"))
}

pub fn collect_positive_workspace_ids(workspaces: &[HyprWorkspace]) -> BTreeSet<i32> {
    workspaces
        .iter()
        .filter(|ws| ws.id > 0)
        .map(|ws| ws.id)
        .collect()
}

#[cfg(test)]
pub fn parse_positive_workspace_ids(raw: &str) -> Result<BTreeSet<i32>, String> {
    let list = parse_workspaces_json(raw)?;
    Ok(collect_positive_workspace_ids(&list))
}

pub fn lowest_unused_positive_workspace(used_positive_ids: &BTreeSet<i32>) -> i32 {
    let mut candidate = 1;
    while used_positive_ids.contains(&candidate) {
        candidate += 1;
    }
    candidate
}

#[cfg(test)]
pub fn workspace_has_active_windows_in_clients(clients: &[HyprClient], ws_id: i32) -> bool {
    ws_id > 0 && clients.iter().any(|c| c.workspace.id == ws_id)
}

#[cfg(test)]
pub fn resolve_target_workspace_from_snapshot(
    explicit_override: Option<i32>,
    state: &SessionWorkspaceState,
    workspaces: &[HyprWorkspace],
    clients: &[HyprClient],
) -> i32 {
    if let Some(target) = explicit_override.filter(|&t| t > 0) {
        return target;
    }

    if let Some(active_companion_ws) = state.get() {
        if workspace_has_active_windows_in_clients(clients, active_companion_ws) {
            return active_companion_ws;
        }
        state.reset();
    }

    let used = collect_positive_workspace_ids(workspaces);
    let next_ws = lowest_unused_positive_workspace(&used);
    state.set(next_ws);
    next_ws
}

#[cfg(test)]
pub fn is_spotify_client(client: &HyprClient) -> bool {
    let cls = client.class.trim().to_ascii_lowercase();
    let init = client.initial_class.trim().to_ascii_lowercase();
    matches!(cls.as_str(), "spotify" | "com.spotify.client")
        || matches!(init.as_str(), "spotify" | "com.spotify.client")
}

#[cfg(test)]
pub fn find_spotify_clients(clients: &[HyprClient]) -> Vec<&HyprClient> {
    clients.iter().filter(|c| is_spotify_client(c)).collect()
}

fn match_browser_binary(client: &HyprClient) -> Option<&'static str> {
    for raw in [&client.class, &client.initial_class] {
        let lower = raw.trim().to_ascii_lowercase();
        if lower.is_empty() {
            continue;
        }
        if let Some((_, binary)) = KNOWN_BROWSER_CLASSES
            .iter()
            .find(|(cls, _)| cls.eq_ignore_ascii_case(&lower))
        {
            return Some(*binary);
        }
    }
    None
}

#[cfg(test)]
pub fn is_known_browser_client(client: &HyprClient) -> bool {
    match_browser_binary(client).is_some()
}

pub fn detect_active_browsers(clients: &[HyprClient]) -> Vec<ActiveBrowserInfo> {
    clients
        .iter()
        .filter_map(|client| {
            let binary = match_browser_binary(client)?;
            let class_name = if !client.class.trim().is_empty() {
                client.class.clone()
            } else {
                client.initial_class.clone()
            };
            Some(ActiveBrowserInfo {
                binary: binary.to_string(),
                class_name,
                workspace_id: client.workspace.id,
                address: client.address.clone(),
            })
        })
        .collect()
}

pub fn browser_binary_from_desktop_entry(desktop_id: &str) -> Option<&'static str> {
    let low = desktop_id.trim().to_ascii_lowercase();
    if low.contains("firefox-developer") {
        Some("firefox-developer-edition")
    } else if low.contains("firefox") {
        Some("firefox")
    } else if low.contains("brave") {
        Some("brave")
    } else if low.contains("zen") {
        Some("zen-browser")
    } else if low.contains("chromium") {
        Some("chromium")
    } else if low.contains("chrome") {
        Some("google-chrome-stable")
    } else if low.contains("librewolf") {
        Some("librewolf")
    } else if low.contains("floorp") {
        Some("floorp")
    } else if low.contains("vivaldi") {
        Some("vivaldi-stable")
    } else {
        None
    }
}

pub fn detect_installed_browser_binary() -> Option<String> {
    if let Ok(out) = std::process::Command::new("xdg-settings")
        .args(["get", "default-web-browser"])
        .stdin(Stdio::null())
        .output()
    {
        if out.status.success() {
            let desktop = String::from_utf8_lossy(&out.stdout);
            if let Some(bin) = browser_binary_from_desktop_entry(&desktop)
                .filter(|b| crate::wizard::detector::which(b).is_some())
            {
                return Some(bin.to_string());
            }
        }
    }

    FALLBACK_BROWSER_BINARIES
        .iter()
        .find(|&&bin| crate::wizard::detector::which(bin).is_some())
        .map(|&bin| bin.to_string())
}

pub fn build_browser_launch_command_with_default(
    url: &str,
    target_ws: i32,
    active_browsers: &[ActiveBrowserInfo],
    default_browser: Option<&str>,
) -> (String, bool) {
    let quoted_url = shell_single_quote(url);

    if let Some(chosen) = active_browsers
        .iter()
        .find(|b| b.workspace_id == target_ws)
        .or_else(|| active_browsers.first())
    {
        return (format!("{} --new-window {quoted_url}", chosen.binary), true);
    }

    if let Some(bin) = default_browser.map(str::trim).filter(|s| !s.is_empty()) {
        return (format!("{bin} --new-window {quoted_url}"), true);
    }

    (format!("xdg-open {quoted_url}"), false)
}

#[cfg(test)]
pub fn build_browser_launch_command(
    url: &str,
    target_ws: i32,
    active_browsers: &[ActiveBrowserInfo],
) -> (String, bool) {
    build_browser_launch_command_with_default(url, target_ws, active_browsers, None)
}

#[cfg(test)]
pub use super::desktop::hyprland::{format_legacy_exec_arg, format_lua_exec_code, hypr_exec};

pub async fn query_hypr_clients() -> Result<Vec<HyprClient>, String> {
    let raw = hyprland::run_hyprctl_async(&["clients", "-j"]).await?;
    parse_clients_json(&raw)
}

pub fn backend_has_active_windows(backend: &dyn WindowBackend, ws_id: i32) -> Result<bool, String> {
    if ws_id <= 0 {
        return Ok(false);
    }
    let windows = backend.list_windows()?;
    Ok(windows.iter().any(|w| w.workspace == ws_id))
}

pub fn find_first_available_workspace_via(backend: &dyn WindowBackend) -> Result<i32, String> {
    let mut used: BTreeSet<i32> = backend
        .list_workspaces()?
        .into_iter()
        .filter(|&id| id > 0)
        .collect();
    if let Some(active_ws) = backend.get_active_workspace().ok().filter(|&ws| ws > 0) {
        used.insert(active_ws);
    }
    Ok(lowest_unused_positive_workspace(&used))
}

pub fn resolve_target_workspace_via_backend(
    explicit_override: Option<i32>,
    state: &SessionWorkspaceState,
    backend: &dyn WindowBackend,
) -> Result<i32, String> {
    if let Some(target) = explicit_override.filter(|&t| t > 0) {
        return Ok(target);
    }

    if let Some(active_companion_ws) = state.get() {
        if backend_has_active_windows(backend, active_companion_ws).unwrap_or(false) {
            return Ok(active_companion_ws);
        }
        state.reset();
    }

    let next_ws = find_first_available_workspace_via(backend)?;
    state.set(next_ws);
    Ok(next_ws)
}

pub async fn resolve_target_workspace_with_config(
    explicit_override: Option<i32>,
    state: &SessionWorkspaceState,
    desktop_cfg: Option<&DesktopConfig>,
) -> Result<i32, String> {
    let backend = desktop::active_window_backend(desktop_cfg);
    resolve_target_workspace_via_backend(explicit_override, state, backend.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_ws(id: i32, name: &str) -> HyprWorkspace {
        HyprWorkspace {
            id,
            name: name.to_string(),
            windows: 1,
        }
    }

    fn mk_client(addr: &str, class: &str, title: &str, ws_id: i32) -> HyprClient {
        HyprClient {
            address: addr.to_string(),
            class: class.to_string(),
            initial_class: class.to_string(),
            title: title.to_string(),
            workspace: HyprClientWorkspace {
                id: ws_id,
                name: ws_id.to_string(),
            },
        }
    }

    #[test]
    fn filters_out_negative_scratchpad_workspace_ids() {
        let raw = r#"[
            {"id": -99, "name": "special:scratchpad", "windows": 2},
            {"id": -98, "name": "special:music", "windows": 1},
            {"id": 0, "name": "invalid", "windows": 0},
            {"id": 1, "name": "1", "windows": 2},
            {"id": 2, "name": "2", "windows": 1},
            {"id": 4, "name": "4", "windows": 1}
        ]"#;
        let used = parse_positive_workspace_ids(raw).unwrap();
        assert_eq!(used, BTreeSet::from([1, 2, 4]));
        assert_eq!(lowest_unused_positive_workspace(&used), 3);
    }

    #[test]
    fn workspace_resolver_respects_priority_override_persistence_and_reset() {
        let state = SessionWorkspaceState::new();
        let workspaces = vec![mk_ws(-99, "special:scratchpad"), mk_ws(1, "1"), mk_ws(2, "2")];
        let clients_ws1 = vec![mk_client("0x1", "kitty", "term", 1)];

        assert_eq!(
            resolve_target_workspace_from_snapshot(Some(5), &state, &workspaces, &clients_ws1),
            5
        );
        assert_eq!(state.get(), None);

        assert_eq!(
            resolve_target_workspace_from_snapshot(None, &state, &workspaces, &clients_ws1),
            3
        );
        assert_eq!(state.get(), Some(3));

        let workspaces_with_3 = vec![mk_ws(1, "1"), mk_ws(2, "2"), mk_ws(3, "3")];
        let clients_with_3 = vec![
            mk_client("0x1", "kitty", "term", 1),
            mk_client("0x3", "Spotify", "Spotify", 3),
        ];
        assert_eq!(
            resolve_target_workspace_from_snapshot(None, &state, &workspaces_with_3, &clients_with_3),
            3
        );

        let workspaces_after_close = vec![mk_ws(1, "1"), mk_ws(3, "3")];
        let clients_after_close = vec![mk_client("0x1", "kitty", "term", 1)];
        assert_eq!(
            resolve_target_workspace_from_snapshot(
                None,
                &state,
                &workspaces_after_close,
                &clients_after_close
            ),
            2
        );
        assert_eq!(state.get(), Some(2));
    }

    #[test]
    fn browser_inspector_injects_new_window_when_browser_runs_on_other_workspace() {
        let clients = vec![
            mk_client("0xaaa", "kitty", "fish", 1),
            mk_client("0xbbb", "Brave-browser", "GitHub", 2),
        ];
        let browsers = detect_active_browsers(&clients);
        assert_eq!(browsers.len(), 1);
        assert_eq!(browsers[0].binary, "brave");
        assert_eq!(browsers[0].workspace_id, 2);

        let (cmd_diff, used_new) =
            build_browser_launch_command("https://wiki.archlinux.org", 4, &browsers);
        assert!(used_new);
        assert_eq!(cmd_diff, "brave --new-window 'https://wiki.archlinux.org'");

        let (cmd_same, used_new_same) =
            build_browser_launch_command("https://wiki.archlinux.org", 2, &browsers);
        assert!(used_new_same);
        assert_eq!(cmd_same, "brave --new-window 'https://wiki.archlinux.org'");

        let (cmd_default, used_new_def) = build_browser_launch_command_with_default(
            "https://www.youtube.com/watch?v=QdBZY2fkU-0",
            5,
            &[],
            Some("firefox"),
        );
        assert!(used_new_def);
        assert_eq!(
            cmd_default,
            "firefox --new-window 'https://www.youtube.com/watch?v=QdBZY2fkU-0'"
        );

        let (cmd_fallback, used_new_fb) =
            build_browser_launch_command("https://wiki.archlinux.org", 4, &[]);
        assert!(!used_new_fb);
        assert_eq!(cmd_fallback, "xdg-open 'https://wiki.archlinux.org'");

        let sp_clients = vec![
            HyprClient {
                address: "0xsp1".to_string(),
                class: "".to_string(),
                initial_class: "Spotify".to_string(),
                title: "Spotify Premium".to_string(),
                workspace: HyprClientWorkspace {
                    id: 5,
                    name: "5".to_string(),
                },
            },
            mk_client("0xff1", "firefox", "Spotify Web", 1),
        ];
        let detected_sp = find_spotify_clients(&sp_clients);
        assert_eq!(detected_sp.len(), 1);
        assert_eq!(detected_sp[0].address, "0xsp1");
    }

    #[test]
    fn formats_lua_and_legacy_hyprland_exec_rules_safely() {
        let cmd = "firefox --new-window 'https://www.google.com/search?q=Arch%20Linux&lang=id'";
        assert_eq!(
            format_legacy_exec_arg(Some(5), cmd),
            "[workspace 5 silent] firefox --new-window 'https://www.google.com/search?q=Arch%20Linux&lang=id'"
        );
        assert_eq!(format_legacy_exec_arg(None, "kitty"), "kitty");

        let lua_ws = format_lua_exec_code(Some(5), cmd);
        assert_eq!(
            lua_ws,
            "hl.exec_cmd(\"firefox --new-window 'https://www.google.com/search?q=Arch%20Linux&lang=id'\", { workspace = \"5 silent\" })"
        );
        let lua_quoted = format_lua_exec_code(Some(3), r#"sh -c "echo hi""#);
        assert!(lua_quoted.contains(r#"\"echo hi\""#));
        assert!(lua_quoted.contains(r#"workspace = "3 silent""#));
    }
}
