//! Composite one-shot recipes and parallel desktop scene orchestration (`execute_recipe`).

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::settings::{DesktopConfig, Settings};
use crate::engine::kaomoji::KaomojiEngine;
use crate::engine::shadow_git::ShadowGit;
use crate::wizard::detector;

use super::desktop::{
    self, create_window_backend, WindowBackend, WindowInfo,
};
use super::document::{
    self, launch_command_in_workspace_auto, resolve_document_target_path,
    resolve_gui_opener_command, ManageDocumentArgs,
};
use super::media::{self, OpenMediaArgs, PendingAction};
use super::timer::{self, ManageAlarmArgs};
use super::workspace_tracker::{self, SessionWorkspaceState};

/// Arguments for `execute_recipe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteRecipeArgs {
    pub recipe: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub paragraphs: Vec<String>,
    #[serde(default)]
    pub sheet_name: Option<String>,
    #[serde(default)]
    pub headers: Vec<String>,
    #[serde(default, alias = "table_data")]
    pub rows: Vec<Vec<Value>>,
    #[serde(default)]
    pub sequence: Option<String>,
    #[serde(default)]
    pub append: bool,
    #[serde(default)]
    pub editor: Option<String>,
    #[serde(default)]
    pub workspace: Option<i32>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub scene: Option<String>,
    #[serde(default)]
    pub alarm_time: Option<String>,
}

impl ExecuteRecipeArgs {
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        let mut parsed: Self = serde_json::from_str(raw)
            .map_err(|e| format!("argumen JSON execute_recipe tidak valid: {e}"))?;
        parsed.recipe = parsed.recipe.trim().to_ascii_lowercase();
        if parsed.recipe.is_empty() {
            return Err("field `recipe` wajib diisi pada execute_recipe.".to_string());
        }
        parsed.path = parsed
            .path
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        parsed.editor = parsed
            .editor
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        parsed.query = parsed
            .query
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        parsed.platform = parsed
            .platform
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty());
        parsed.scene = parsed
            .scene
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty());
        parsed.alarm_time = parsed
            .alarm_time
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok(parsed)
    }

    fn to_document_args(&self, action: &str) -> Result<ManageDocumentArgs, String> {
        let raw_path = self.path.as_deref().ok_or_else(|| {
            format!(
                "Field `path` wajib diisi untuk resep `{}` (contoh: `~/jadwal.xlsx` atau `laporan.docx`).",
                self.recipe
            )
        })?;
        Ok(ManageDocumentArgs {
            action: action.to_string(),
            path: raw_path.to_string(),
            title: self.title.clone(),
            content: self.content.clone(),
            paragraphs: self.paragraphs.clone(),
            sheet_name: self.sheet_name.clone(),
            headers: self.headers.clone(),
            rows: self.rows.clone(),
            sequence: self.sequence.clone(),
            editor: self.editor.clone(),
            workspace: self.workspace,
        })
    }
}

/// Runtime dependencies passed into `execute_recipe`.
pub struct RecipeExecutionContext<'a> {
    pub settings: &'a Settings,
    pub shadow_git: &'a ShadowGit,
    pub workspace_state: &'a Arc<SessionWorkspaceState>,
    pub pending_action: &'a Arc<Mutex<PendingAction>>,
    pub kaomoji_engine: &'a Arc<Mutex<KaomojiEngine>>,
}

pub async fn execute_recipe(
    args: &ExecuteRecipeArgs,
    ctx: &RecipeExecutionContext<'_>,
) -> crate::Result<String> {
    let desktop_cfg = Some(&ctx.settings.desktop);
    match args.recipe.as_str() {
        "create_and_open_document" | "create_and_open" => {
            let backend_kind = desktop_cfg
                .map(DesktopConfig::effective_backend)
                .unwrap_or_else(detector::detect_desktop_backend);
            let backend = create_window_backend(backend_kind);
            let backend_for_launch = create_window_backend(backend_kind);
            let cfg_clone = desktop_cfg.cloned();

            recipe_create_and_open_with_backend(
                args,
                ctx.settings,
                Some(ctx.shadow_git),
                ctx.workspace_state,
                backend.as_ref(),
                move |cmd, ws| async move {
                    launch_command_in_workspace_auto(
                        &cmd,
                        ws,
                        cfg_clone.as_ref(),
                        backend_for_launch.as_ref(),
                    )
                    .await
                },
            )
            .await
        }
        "open_file_in_workspace" | "open_document" | "open_file" => {
            let backend_kind = desktop_cfg
                .map(DesktopConfig::effective_backend)
                .unwrap_or_else(detector::detect_desktop_backend);
            let backend = create_window_backend(backend_kind);
            let backend_for_launch = create_window_backend(backend_kind);
            let cfg_clone = desktop_cfg.cloned();

            recipe_open_file_with_backend(
                args,
                ctx.settings,
                ctx.workspace_state,
                backend.as_ref(),
                move |cmd, ws| async move {
                    launch_command_in_workspace_auto(
                        &cmd,
                        ws,
                        cfg_clone.as_ref(),
                        backend_for_launch.as_ref(),
                    )
                    .await
                },
            )
            .await
        }
        "hot_edit_document" | "hot_edit" => {
            let doc_args = args
                .to_document_args("hot_edit")
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            document::execute_hot_edit_document(
                &doc_args,
                ctx.settings,
                Some(ctx.shadow_git),
                ctx.workspace_state,
                desktop_cfg,
                args.append,
            )
            .await
        }
        "play_media_direct" | "play_media" => {
            let query = args.query.clone().ok_or(
                "Field `query` wajib diisi untuk resep `play_media_direct` (contoh: judul lagu atau video).",
            )?;
            let media_args = OpenMediaArgs {
                query,
                platform: args.platform.clone().unwrap_or_else(|| "spotify".to_string()),
                workspace: args.workspace,
            };
            media::execute_open_media_with_config(
                &media_args,
                ctx.workspace_state,
                ctx.pending_action,
                ctx.kaomoji_engine,
                &ctx.settings.general.bot_name,
                desktop_cfg,
            )
            .await
        }
        "trigger_scene" | "scene" => execute_trigger_scene(args, ctx).await,
        other => Err(format!(
            "Resep `{other}` tidak dikenal pada execute_recipe. Gunakan: create_and_open_document, open_file_in_workspace, hot_edit_document, play_media_direct, atau trigger_scene."
        )
        .into()),
    }
}

pub async fn recipe_create_and_open_with_backend<L, Fut>(
    args: &ExecuteRecipeArgs,
    settings: &Settings,
    shadow_git: Option<&ShadowGit>,
    ws_state: &SessionWorkspaceState,
    backend: &dyn WindowBackend,
    launch_fn: L,
) -> crate::Result<String>
where
    L: FnOnce(String, i32) -> Fut,
    Fut: std::future::Future<Output = Result<Option<WindowInfo>, String>>,
{
    let doc_args = args
        .to_document_args("create")
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    let summary =
        document::write_or_update_document_on_disk(&doc_args, settings, shadow_git, args.append)?;

    let target_ws = workspace_tracker::resolve_target_workspace_via_backend(
        args.workspace.filter(|&w| w > 0),
        ws_state,
        backend,
    )
    .unwrap_or(2);

    let launch_cmd = resolve_gui_opener_command(&summary.path, args.editor.as_deref());
    let maybe_win = launch_fn(launch_cmd.clone(), target_ws)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    if let Some(win_info) = maybe_win {
        let norm_addr = desktop::normalize_window_address(&win_info.id)
            .unwrap_or_else(|| format!("address:{}", win_info.id));
        Ok(format!(
            "status: success\n\
             recipe: create_and_open_document\n\
             path: {}\n\
             format: {}\n\
             rows_or_items: {}\n\
             bytes: {}\n\
             verified_on_disk: true\n\
             verified_window: true\n\
             address: {norm_addr}\n\
             workspace_id: {}\n\
             class: {}\n\
             title: {}\n\
             do_not_poll_clients: true",
            summary.path.display(),
            summary.format,
            summary.rows_or_items,
            summary.bytes,
            win_info.workspace,
            win_info.class,
            win_info.title
        ))
    } else {
        Ok(format!(
            "status: success\n\
             recipe: create_and_open_document\n\
             path: {}\n\
             format: {}\n\
             rows_or_items: {}\n\
             bytes: {}\n\
             verified_on_disk: true\n\
             verified_window: false\n\
             workspace_id: {target_ws}\n\
             command: {launch_cmd}\n\
             do_not_poll_clients: true\n\
             note: File telah dibuat di disk dan editor diluncurkan ke workspace {target_ws}.",
            summary.path.display(),
            summary.format,
            summary.rows_or_items,
            summary.bytes
        ))
    }
}

pub async fn recipe_open_file_with_backend<L, Fut>(
    args: &ExecuteRecipeArgs,
    settings: &Settings,
    ws_state: &SessionWorkspaceState,
    backend: &dyn WindowBackend,
    launch_fn: L,
) -> crate::Result<String>
where
    L: FnOnce(String, i32) -> Fut,
    Fut: std::future::Future<Output = Result<Option<WindowInfo>, String>>,
{
    let raw_path = args.path.as_deref().ok_or_else(|| {
        "Field `path` wajib diisi untuk resep `open_file_in_workspace`.".to_string()
    })?;
    let target = resolve_document_target_path(raw_path, &settings.general.user_real_name);
    if !target.exists() {
        return Err(format!(
            "File target tidak ditemukan di disk: {}. Gunakan `create_and_open_document` bila ingin membuat file baru.",
            target.display()
        )
        .into());
    }

    let target_ws = workspace_tracker::resolve_target_workspace_via_backend(
        args.workspace.filter(|&w| w > 0),
        ws_state,
        backend,
    )
    .unwrap_or(2);

    let launch_cmd = resolve_gui_opener_command(&target, args.editor.as_deref());
    let maybe_win = launch_fn(launch_cmd.clone(), target_ws)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    if let Some(win_info) = maybe_win {
        let norm_addr = desktop::normalize_window_address(&win_info.id)
            .unwrap_or_else(|| format!("address:{}", win_info.id));
        Ok(format!(
            "status: success\n\
             recipe: open_file_in_workspace\n\
             path: {}\n\
             verified_window: true\n\
             address: {norm_addr}\n\
             workspace_id: {}\n\
             class: {}\n\
             title: {}\n\
             do_not_poll_clients: true",
            target.display(),
            win_info.workspace,
            win_info.class,
            win_info.title
        ))
    } else {
        Ok(format!(
            "status: success\n\
             recipe: open_file_in_workspace\n\
             path: {}\n\
             verified_window: false\n\
             workspace_id: {target_ws}\n\
             command: {launch_cmd}\n\
             do_not_poll_clients: true\n\
             note: Aplikasi pembuka telah di-spawn ke workspace {target_ws}.",
            target.display()
        ))
    }
}

async fn execute_trigger_scene(
    args: &ExecuteRecipeArgs,
    ctx: &RecipeExecutionContext<'_>,
) -> crate::Result<String> {
    let scene_name = args.scene.as_deref().unwrap_or("focus");
    let desktop_cfg = Some(&ctx.settings.desktop);
    let backend_kind = desktop_cfg
        .map(DesktopConfig::effective_backend)
        .unwrap_or_else(detector::detect_desktop_backend);

    match scene_name {
        "clean_slate" | "clean" => {
            let backend = create_window_backend(backend_kind);
            let active_ws = backend.get_active_workspace().unwrap_or(1);
            let all_ws = backend.list_workspaces().unwrap_or_default();
            let to_close: Vec<i32> = all_ws
                .into_iter()
                .filter(|&ws| ws > 0 && ws != active_ws)
                .collect();
            if to_close.is_empty() {
                return Ok(format!(
                    "status: success\nrecipe: trigger_scene\nscene: clean_slate\nactive_workspace: {active_ws}\nnote: Tidak ada workspace lain yang terbuka selain workspace aktif."
                ));
            }
            let close_report = backend
                .close_workspaces(&to_close)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            ctx.workspace_state.clear_companion();
            Ok(format!(
                "status: success\nrecipe: trigger_scene\nscene: clean_slate\nactive_workspace: {active_ws}\nreport: {close_report}\ndo_not_poll_clients: true"
            ))
        }
        "focus" | "study" => {
            let media_fut = async {
                let q = args.query.clone()?;
                let m_args = OpenMediaArgs {
                    query: q,
                    platform: args
                        .platform
                        .clone()
                        .unwrap_or_else(|| "spotify".to_string()),
                    workspace: args.workspace,
                };
                Some(
                    media::execute_open_media_with_config(
                        &m_args,
                        ctx.workspace_state,
                        ctx.pending_action,
                        ctx.kaomoji_engine,
                        &ctx.settings.general.bot_name,
                        desktop_cfg,
                    )
                    .await,
                )
            };

            let file_fut = async {
                args.path.as_ref()?;
                let b1 = create_window_backend(backend_kind);
                let b2 = create_window_backend(backend_kind);
                let cfg_clone = desktop_cfg.cloned();
                Some(
                    recipe_open_file_with_backend(
                        args,
                        ctx.settings,
                        ctx.workspace_state,
                        b1.as_ref(),
                        move |cmd, ws| async move {
                            launch_command_in_workspace_auto(
                                &cmd,
                                ws,
                                cfg_clone.as_ref(),
                                b2.as_ref(),
                            )
                            .await
                        },
                    )
                    .await,
                )
            };

            let alarm_fut = async {
                let t = args.alarm_time.as_ref()?;
                let a_args = ManageAlarmArgs {
                    action: "set".to_string(),
                    time: Some(t.clone()),
                    label: Some(format!("scene-{scene_name}")),
                };
                Some(timer::execute_manage_alarm(&a_args))
            };

            let (media_res, file_res, alarm_res) = tokio::join!(media_fut, file_fut, alarm_fut);
            let mut steps = Vec::new();
            if let Some(res) = file_res {
                match res {
                    Ok(ok) => steps.push(format!("[file_opened]\n{ok}")),
                    Err(e) => steps.push(format!("[file_error] {e}")),
                }
            }
            if let Some(res) = media_res {
                match res {
                    Ok(ok) => steps.push(format!("[media]\n{ok}")),
                    Err(e) => steps.push(format!("[media_error] {e}")),
                }
            }
            if let Some(res) = alarm_res {
                match res {
                    Ok(ok) => steps.push(format!("[alarm]\n{ok}")),
                    Err(e) => steps.push(format!("[alarm_error] {e}")),
                }
            }
            if steps.is_empty() {
                steps.push("Mode fokus siap (sertakan `path`, `query`, atau `alarm_time` untuk membuka materi/musik/Pomodoro sekaligus).".to_string());
            }

            Ok(format!(
                "status: success\nrecipe: trigger_scene\nscene: {scene_name}\ndo_not_poll_clients: true\n{}",
                steps.join("\n---\n")
            ))
        }
        "entertainment" => {
            let q = args
                .query
                .clone()
                .unwrap_or_else(|| "lofi chill beats".to_string());
            let m_args = OpenMediaArgs {
                query: q,
                platform: args
                    .platform
                    .clone()
                    .unwrap_or_else(|| "spotify".to_string()),
                workspace: args.workspace,
            };
            let out = media::execute_open_media_with_config(
                &m_args,
                ctx.workspace_state,
                ctx.pending_action,
                ctx.kaomoji_engine,
                &ctx.settings.general.bot_name,
                desktop_cfg,
            )
            .await?;
            Ok(format!(
                "status: success\nrecipe: trigger_scene\nscene: entertainment\ndo_not_poll_clients: true\n{out}"
            ))
        }
        other => Err(format!(
            "Scene `{other}` tidak dikenal pada trigger_scene. Gunakan: focus, study, clean_slate, atau entertainment."
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::sync::Mutex as StdMutex;

    struct MockRecipeBackend {
        active_ws: i32,
        workspaces: Vec<i32>,
        windows: StdMutex<Vec<WindowInfo>>,
        closed_ws: StdMutex<Vec<Vec<i32>>>,
    }

    impl WindowBackend for MockRecipeBackend {
        fn get_active_workspace(&self) -> Result<i32, String> {
            Ok(self.active_ws)
        }

        fn list_workspaces(&self) -> Result<Vec<i32>, String> {
            Ok(self.workspaces.clone())
        }

        fn list_windows(&self) -> Result<Vec<WindowInfo>, String> {
            Ok(self.windows.lock().unwrap().clone())
        }

        fn close_window(&self, target: &str) -> Result<String, String> {
            let clean = target.trim_start_matches("address:");
            self.windows
                .lock()
                .unwrap()
                .retain(|w| w.id != clean && w.id != target);
            Ok("ok".to_string())
        }

        fn close_workspaces(&self, workspace_ids: &[i32]) -> Result<String, String> {
            self.closed_ws.lock().unwrap().push(workspace_ids.to_vec());
            Ok(format!("closed {:?}", workspace_ids))
        }

        fn launch_app_in_workspace(
            &self,
            _command: &str,
            _workspace: i32,
        ) -> Result<Option<WindowInfo>, String> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn create_and_open_document_and_open_file_in_workspace_succeed_in_single_call() {
        let dir = std::env::temp_dir().join(format!("waish-recipe-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let settings = Settings::default();
        let ws_state = SessionWorkspaceState::new();
        let mock_backend = MockRecipeBackend {
            active_ws: 1,
            workspaces: vec![1, 2],
            windows: StdMutex::new(Vec::new()),
            closed_ws: StdMutex::new(Vec::new()),
        };

        let xlsx_path = dir.join("jadwal_kuliah.xlsx");
        let create_recipe = ExecuteRecipeArgs {
            recipe: "create_and_open_document".to_string(),
            path: Some(xlsx_path.to_string_lossy().into_owned()),
            title: None,
            content: String::new(),
            paragraphs: Vec::new(),
            sheet_name: Some("Semester5".to_string()),
            headers: vec!["No".to_string(), "Mata Kuliah".to_string()],
            rows: vec![vec![json!(1), json!("Matematika Aktuaria")]],
            sequence: None,
            append: false,
            editor: Some("QT_QPA_PLATFORM=wayland onlyoffice-desktopeditors".to_string()),
            workspace: None,
            query: None,
            platform: None,
            scene: None,
            alarm_time: None,
        };

        let out = recipe_create_and_open_with_backend(
            &create_recipe,
            &settings,
            None,
            &ws_state,
            &mock_backend,
            |cmd, ws| async move {
                assert_eq!(ws, 3);
                assert!(!cmd.contains("QT_QPA_PLATFORM=wayland"));
                assert!(cmd.contains("onlyoffice-desktopeditors"));
                Ok(Some(WindowInfo {
                    id: "0x998877".to_string(),
                    class: "DesktopEditors".to_string(),
                    initial_class: "DesktopEditors".to_string(),
                    title: "jadwal_kuliah.xlsx - ONLYOFFICE".to_string(),
                    workspace: ws,
                }))
            },
        )
        .await
        .unwrap();

        assert!(xlsx_path.exists());
        assert!(out.contains("recipe: create_and_open_document"));
        assert!(out.contains("verified_on_disk: true"));
        assert!(out.contains("verified_window: true"));
        assert!(out.contains("workspace_id: 3"));
        assert!(out.contains("do_not_poll_clients: true"));

        let open_recipe = ExecuteRecipeArgs {
            recipe: "open_file_in_workspace".to_string(),
            path: Some(xlsx_path.to_string_lossy().into_owned()),
            title: None,
            content: String::new(),
            paragraphs: Vec::new(),
            sheet_name: None,
            headers: Vec::new(),
            rows: Vec::new(),
            sequence: None,
            append: false,
            editor: Some("libreoffice".to_string()),
            workspace: Some(5),
            query: None,
            platform: None,
            scene: None,
            alarm_time: None,
        };

        let open_out = recipe_open_file_with_backend(
            &open_recipe,
            &settings,
            &ws_state,
            &mock_backend,
            |cmd, ws| async move {
                assert_eq!(ws, 5);
                assert!(cmd.contains("libreoffice"));
                Ok(Some(WindowInfo {
                    id: "0x554433".to_string(),
                    class: "libreoffice-calc".to_string(),
                    initial_class: "libreoffice-calc".to_string(),
                    title: "jadwal_kuliah.xlsx - LibreOffice Calc".to_string(),
                    workspace: ws,
                }))
            },
        )
        .await
        .unwrap();

        assert!(open_out.contains("recipe: open_file_in_workspace"));
        assert!(open_out.contains("verified_window: true"));
        assert!(open_out.contains("workspace_id: 5"));
        assert!(open_out.contains("do_not_poll_clients: true"));

        let _ = fs::remove_dir_all(&dir);
    }
}
