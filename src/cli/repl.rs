//! Interactive REPL Loop, Proactive Watcher Bridge & Local Slash Command Router.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Timelike;
use colored::Colorize;
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{Cmd, Config, Editor, ExternalPrinter, KeyEvent};
use tokio::sync::mpsc;

use super::commands;
use super::prompt::{
    format_bot_prompt_colored_face, format_user_prompt_plain, print_bot_header_face,
    print_bot_inline, print_bot_inline_face, PromptHelper,
};
use super::ui::{self, Kaomoji};
use crate::config::settings::{Settings, DEFAULT_USER_REAL_NAME};
use crate::config::{Paths, PRIVATE_DIR_MODE, PRIVATE_FILE_MODE};
use crate::engine::client::{strip_terminal_escapes, LlmClient, StreamResponse, ToolCall, ToolRound};
use crate::engine::context::{resolve_system_update_command, ContextAssembler};
use crate::engine::fast_path::{self, FastPathIntent};
use crate::engine::kaomoji::{
    is_late_night_hour, is_sweet_or_compliment_input, EmotionSpectrum, KaomojiEngine,
};
use crate::engine::scratchpad::{detect_package_manager_preference, Scratchpad};
use crate::engine::shadow_git::ShadowGit;
use crate::engine::telemetry::HardwareSnapshot;
use crate::engine::watcher::{
    self, BatteryPromiseState, HardwareWatcherState, ProactiveEvent, ProactiveEventKind,
};
use crate::tools::desktop::{self, WaybarState};
use crate::tools::media::{self, PendingAction};
use crate::tools::registry::{
    CONTROL_MEDIA_NAME, EXECUTE_RECIPE_NAME, LAUNCH_GUI_APP_NAME, MANAGE_DOCUMENT_NAME,
    MANAGE_SPREADSHEET_NAME, MANAGE_WORKSPACE_NAME, OPEN_MEDIA_NAME, RUN_SHELL_COMMAND_NAME,
    WRITE_FILE_NAME,
};
use crate::tools::runner::{
    execute_tool_call, sanitize_path_for_user, ToolExecutionContext, AD_HOC_OFFICE_SCRIPT_NOTICE,
    ALARM_FS_RUMMAGE_NOTICE, BACKGROUND_GUI_SHELL_NOTICE,
};
use crate::tools::workspace_tracker::SessionWorkspaceState;
use crate::wizard::i18n::{WizardLang, RUNNER_KAOMOJI};

const DROWSY_IDLE_TIMEOUT_SECS: u64 = 5 * 60;
const MAX_REACT_ROUNDS: usize = 6;
const MAX_COMPLETION_GUARD_RETRIES: usize = 2;
pub const COMPLETION_GUARD_WARNING: &str =
    "[Error]: Jendela aplikasi atau target file belum terverifikasi di sistem. Lakukan verifikasi sekarang!";
pub const SHELL_EXPERIMENT_CAP_WARNING: &str =
    "[System Notice]: Eksperimen shell telah gagal 2 kali berturut-turut. Hentikan percobaan menebak di shell/proc dan minta arahan langsung kepada pengguna!";
pub const EMPTY_PROMISE_GUARD_WARNING: &str =
    "[System Notice]: Kamu baru saja berjanji akan menjalankan aksi, tetapi belum memanggil tool apa pun! Segera panggil tool yang sesuai sekarang (prioritaskan execute_recipe, manage_document, launch_gui_app, manage_alarm, atau run_shell_command)!";
pub const EMPTY_PROMISE_STALL_FALLBACK_MSG: &str =
    "[waish] (｡•́︿•̀｡) Model belum mengirimkan panggilan tool eksekusi untuk perintah tadi. Silakan ulangi singkat instruksinya atau ganti model lewat `/model` bila antrian provider sedang padat.";
const MAX_SESSION_CONVERSATION_TURNS: usize = 60;
const MAX_STORED_ASSISTANT_CHARS: usize = 4_096;

/// Single conversation turn in the active REPL session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationTurn {
    pub user: String,
    pub assistant: String,
}

/// Runtime state for an active REPL session.
#[derive(Debug, Clone)]
pub struct ReplSession {
    pub settings: Settings,
    pub paths: Paths,
    pub conversation: Vec<ConversationTurn>,
    pub assembler: ContextAssembler,
    pub client: LlmClient,
    pub shadow_git: ShadowGit,
    pub watcher_state: Arc<Mutex<HardwareWatcherState>>,
    pub workspace_state: Arc<SessionWorkspaceState>,
    pub pending_action: Arc<Mutex<PendingAction>>,
    pub kaomoji_engine: Arc<Mutex<KaomojiEngine>>,
    pub last_activity: Instant,
}

impl ReplSession {
    pub fn new(mut settings: Settings, paths: Paths) -> crate::Result<Self> {
        if let Some(pm) = Scratchpad::load(&paths.scratchpad)
            .ok()
            .and_then(|pad| pad.preferred_package_manager().map(str::to_string))
        {
            settings.tools.package_manager = pm;
        }

        let client = LlmClient::new(&settings)?;
        let shadow_git = ShadowGit::new(paths.shadow_repo.clone());
        Ok(Self {
            settings,
            paths,
            conversation: Vec::new(),
            assembler: ContextAssembler::default(),
            client,
            shadow_git,
            watcher_state: Arc::new(Mutex::new(HardwareWatcherState::default())),
            workspace_state: Arc::new(SessionWorkspaceState::new()),
            pending_action: Arc::new(Mutex::new(PendingAction::None)),
            kaomoji_engine: Arc::new(Mutex::new(KaomojiEngine::new())),
            last_activity: Instant::now(),
        })
    }

    pub fn lang(&self) -> WizardLang {
        WizardLang::from_code(&self.settings.general.language)
    }

    pub fn push_conversation_turn(&mut self, user: String, assistant: String) {
        let bounded_assistant = if assistant.chars().count() > MAX_STORED_ASSISTANT_CHARS {
            let cut: String = assistant.chars().take(MAX_STORED_ASSISTANT_CHARS).collect();
            format!("{cut}...")
        } else {
            assistant
        };
        self.conversation.push(ConversationTurn {
            user,
            assistant: bounded_assistant,
        });
        if self.conversation.len() > MAX_SESSION_CONVERSATION_TURNS {
            let overflow = self.conversation.len() - MAX_SESSION_CONVERSATION_TURNS;
            self.conversation.drain(..overflow);
            self.conversation.shrink_to_fit();
        }
    }

    pub fn user_display_name(&self) -> &str {
        match self.settings.general.user_real_name.trim() {
            "" => DEFAULT_USER_REAL_NAME,
            name => name,
        }
    }

    pub fn pick_mood_and_sync_waybar(
        &self,
        emotions: &[EmotionSpectrum],
        waybar_state: WaybarState,
        tooltip: Option<&str>,
    ) -> &'static str {
        let face = self
            .kaomoji_engine
            .lock()
            .map(|mut eng| eng.pick_from_group(emotions))
            .unwrap_or(Kaomoji::Ready.as_str());
        desktop::export_waybar_status_with_kaomoji(
            waybar_state,
            &self.settings.general.bot_name,
            face,
            tooltip,
        );
        face
    }

    pub fn is_drowsy_idle(&self) -> bool {
        self.last_activity.elapsed() >= Duration::from_secs(DROWSY_IDLE_TIMEOUT_SECS)
            || is_late_night_hour(chrono::Local::now().hour())
    }
}

/// Evaluation outcome of a single REPL input line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopAction {
    Continue,
    Exit,
}

/// Parsed local slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand<'a> {
    Shell(&'a str),
    Clear,
    Drop,
    Model(Option<&'a str>),
    Memory,
    Note(&'a str),
    Telemetry,
    Uninstall,
    Help,
    Exit,
    Unknown(&'a str),
}

impl<'a> SlashCommand<'a> {
    pub fn parse(input: &'a str) -> Option<Self> {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') {
            return None;
        }

        let (cmd, rest) = match trimmed.find(char::is_whitespace) {
            Some(idx) => (&trimmed[..idx], trimmed[idx..].trim()),
            None => (trimmed, ""),
        };

        Some(match cmd {
            "/sh" => SlashCommand::Shell(rest),
            "/clear" => SlashCommand::Clear,
            "/drop" => SlashCommand::Drop,
            "/model" => SlashCommand::Model((!rest.is_empty()).then_some(rest)),
            "/memory" => SlashCommand::Memory,
            "/note" => SlashCommand::Note(rest),
            "/telemetry" => SlashCommand::Telemetry,
            "/uninstall" => SlashCommand::Uninstall,
            "/help" => SlashCommand::Help,
            "/exit" | "/q" => SlashCommand::Exit,
            other => SlashCommand::Unknown(other),
        })
    }
}

pub async fn run(
    settings: Settings,
    paths: Paths,
    initial_prompt: Option<String>,
) -> crate::Result<ExitCode> {
    let mut session = ReplSession::new(settings, paths)?;

    let _ = session.shadow_git.ensure_initialized();
    desktop::export_waybar_status(WaybarState::Active, &session.settings.general.bot_name, None);

    let rl_config = Config::builder().auto_add_history(false).build();
    let mut rl = Editor::<PromptHelper, DefaultHistory>::with_config(rl_config)?;
    rl.set_helper(Some(PromptHelper::new(&session.settings)));
    rl.bind_sequence(KeyEvent::ctrl('q'), Cmd::EndOfFile);

    if session.paths.history.exists() {
        let _ = rl.load_history(&session.paths.history);
    }

    let (event_tx, event_rx) = mpsc::channel::<ProactiveEvent>(16);
    let repl_is_idle = Arc::new(AtomicBool::new(false));
    let proactive_turns: Arc<Mutex<Vec<ConversationTurn>>> = Arc::new(Mutex::new(Vec::new()));
    let ext_printer = rl.create_external_printer().ok();

    let watcher_handle = watcher::spawn_hardware_watcher(
        Arc::clone(&session.watcher_state),
        Arc::clone(&session.kaomoji_engine),
        event_tx,
        session.settings.general.bot_name.clone(),
    );

    let consumer_handle = spawn_proactive_event_consumer(
        event_rx,
        ext_printer,
        ProactiveConsumerDeps {
            repl_is_idle: Arc::clone(&repl_is_idle),
            proactive_turns: Arc::clone(&proactive_turns),
            kaomoji_engine: Arc::clone(&session.kaomoji_engine),
            client: session.client.clone(),
            settings: session.settings.clone(),
            paths: session.paths.clone(),
        },
    );

    if let Some(first) = initial_prompt {
        let trimmed = first.trim();
        if !trimmed.is_empty() {
            record_history(&mut rl, &session.paths, trimmed);
            if process_line(trimmed, &mut session).await? == LoopAction::Exit {
                watcher_handle.abort();
                consumer_handle.abort();
                desktop::export_waybar_status(
                    WaybarState::Idle,
                    &session.settings.general.bot_name,
                    None,
                );
                let _ = crate::engine::extractor::farewell_and_consolidate(&session).await;
                return Ok(ExitCode::SUCCESS);
            }
            println!();
        }
    } else {
        print_initial_greeting(&session);
        print_startup_scratchpad_reminder(&session);
        println!();
    }

    let user_prompt = format_user_prompt_plain(&session.settings);

    loop {
        let drowsy = session.is_drowsy_idle();
        let _ = media::sync_waybar_idle_or_media_with_engine(
            &session.settings.general.bot_name,
            &session.kaomoji_engine,
            drowsy,
        )
        .await;
        repl_is_idle.store(true, Ordering::SeqCst);

        let read_result = rl.readline(&user_prompt);

        repl_is_idle.store(false, Ordering::SeqCst);
        absorb_proactive_turns(&mut session, &proactive_turns);

        match read_result {
            Ok(line) => {
                let sanitized = strip_terminal_escapes(&line);
                let trimmed = sanitized.trim();
                if trimmed.is_empty() {
                    continue;
                }

                if let Ok(mut ws) = session.watcher_state.lock() {
                    let _ = ws.observe_user_reply(trimmed, Instant::now());
                }

                record_history(&mut rl, &session.paths, trimmed);

                let loop_result = process_line(trimmed, &mut session).await?;
                session.last_activity = Instant::now();
                match loop_result {
                    LoopAction::Continue => println!(),
                    LoopAction::Exit => {
                        watcher_handle.abort();
                        consumer_handle.abort();
                        desktop::export_waybar_status(
                            WaybarState::Idle,
                            &session.settings.general.bot_name,
                            None,
                        );
                        let _ = crate::engine::extractor::farewell_and_consolidate(&session).await;
                        break;
                    }
                }
            }
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => {
                watcher_handle.abort();
                consumer_handle.abort();
                desktop::export_waybar_status(
                    WaybarState::Idle,
                    &session.settings.general.bot_name,
                    None,
                );
                let _ = crate::engine::extractor::farewell_and_consolidate(&session).await;
                break;
            }
            Err(err) => {
                watcher_handle.abort();
                consumer_handle.abort();
                return Err(err.into());
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}

struct ProactiveConsumerDeps {
    repl_is_idle: Arc<AtomicBool>,
    proactive_turns: Arc<Mutex<Vec<ConversationTurn>>>,
    kaomoji_engine: Arc<Mutex<KaomojiEngine>>,
    client: LlmClient,
    settings: Settings,
    paths: Paths,
}

fn spawn_proactive_event_consumer<P>(
    mut rx: mpsc::Receiver<ProactiveEvent>,
    mut printer: Option<P>,
    deps: ProactiveConsumerDeps,
) -> tokio::task::JoinHandle<()>
where
    P: ExternalPrinter + Send + 'static,
{
    tokio::spawn(async move {
        let ProactiveConsumerDeps {
            repl_is_idle,
            proactive_turns,
            kaomoji_engine,
            client,
            settings,
            paths,
        } = deps;
        while let Some(event) = rx.recv().await {
            while !repl_is_idle.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(120)).await;
            }

            let persona_md = crate::config::persona::load_or_default(&paths.persona, &settings);
            let sys_prompt = format!(
                "Kamu adalah \"{}\" (roh penjaga terminal Linux untuk \"{}\"). \
                 Jawab HANYA 1-2 kalimat singkat, ekspresif sesuai persona, dengan kaomoji teks murni (DILARANG emoji grafis berwarna) \
                 dan tanpa mengulang prefix `{}@waish ~>`.\n\n{}",
                settings.general.bot_name,
                settings.general.user_real_name,
                settings.general.bot_name,
                persona_md.trim()
            );

            let reply = match client
                .complete_oneshot(
                    &settings,
                    &sys_prompt,
                    &event.system_instruction,
                    Duration::from_secs(10),
                )
                .await
            {
                Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
                _ => event.fallback_message.clone(),
            };

            while !repl_is_idle.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(120)).await;
            }

            let alert_spectra: &[EmotionSpectrum] = match event.kind {
                ProactiveEventKind::BrokenPromise => &[EmotionSpectrum::AngryChastise],
                _ => &[
                    EmotionSpectrum::AngryChastise,
                    EmotionSpectrum::ProtectiveCare,
                ],
            };
            let dynamic_face = kaomoji_engine
                .lock()
                .map(|mut eng| eng.pick_from_group(alert_spectra))
                .unwrap_or_else(|_| event.mood.as_str());

            desktop::export_waybar_status_with_kaomoji(
                WaybarState::Warning,
                &settings.general.bot_name,
                dynamic_face,
                Some(&reply),
            );

            let formatted = format!(
                "\r\x1b[2K{}{}\n",
                format_bot_prompt_colored_face(&settings, dynamic_face),
                reply
            );

            if let Some(ref mut p) = printer {
                let _ = p.print(formatted);
            } else {
                print!("{formatted}");
                let _ = io::stdout().flush();
            }

            if let Ok(mut buf) = proactive_turns.lock() {
                buf.push(ConversationTurn {
                    user: event.system_instruction,
                    assistant: reply,
                });
            }
        }
    })
}

fn absorb_proactive_turns(
    session: &mut ReplSession,
    proactive_turns: &Arc<Mutex<Vec<ConversationTurn>>>,
) {
    let drained: Vec<ConversationTurn> = match proactive_turns.lock() {
        Ok(mut guard) if !guard.is_empty() => guard.drain(..).collect(),
        _ => return,
    };
    for turn in drained {
        session.push_conversation_turn(turn.user, turn.assistant);
    }
}

fn record_history(
    rl: &mut Editor<PromptHelper, DefaultHistory>,
    paths: &Paths,
    line: &str,
) {
    let _ = rl.add_history_entry(line);
    if ensure_config_dir(&paths.dir).is_ok() && rl.save_history(&paths.history).is_ok() {
        let _ = fs::set_permissions(
            &paths.history,
            fs::Permissions::from_mode(PRIVATE_FILE_MODE),
        );
    }
}

fn ensure_config_dir(dir: &Path) -> io::Result<()> {
    if !dir.exists() {
        fs::create_dir_all(dir)?;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(PRIVATE_DIR_MODE));
    }
    Ok(())
}

pub async fn process_line(line: &str, session: &mut ReplSession) -> crate::Result<LoopAction> {
    let sanitized = strip_terminal_escapes(line);
    let clean_line = sanitized.trim();
    if clean_line.is_empty() {
        return Ok(LoopAction::Continue);
    }

    if let Some(action) = handle_pending_login_if_active(clean_line, session).await {
        return Ok(action);
    }

    if is_uninstall_intent(clean_line) {
        return run_uninstall_flow(session);
    }

    if let Some(cmd) = SlashCommand::parse(clean_line) {
        return execute_slash_command(cmd, session);
    }

    sync_package_manager_preference(clean_line, session);

    if let Some(intent) = fast_path::classify_fast_path_intent(clean_line) {
        handle_fast_path_intent(clean_line, &intent, session).await;
        return Ok(LoopAction::Continue);
    }

    handle_streaming_chat(clean_line, session).await;
    Ok(LoopAction::Continue)
}

async fn handle_fast_path_intent(
    raw_input: &str,
    intent: &FastPathIntent,
    session: &mut ReplSession,
) {
    let outcome = fast_path::execute_fast_path_intent(
        intent,
        &session.settings,
        &session.paths.scratchpad,
        &session.workspace_state,
        &session.kaomoji_engine,
    )
    .await;

    let is_media = matches!(
        intent,
        FastPathIntent::MediaNext
            | FastPathIntent::MediaPrevious
            | FastPathIntent::MediaPause
            | FastPathIntent::MediaPlay
            | FastPathIntent::MediaNowPlaying
    );
    let face = if is_media {
        session.pick_mood_and_sync_waybar(
            &[EmotionSpectrum::MelodicChill, EmotionSpectrum::SmugSuperior],
            WaybarState::Media,
            Some(&outcome.scratchpad_summary),
        )
    } else {
        session.pick_mood_and_sync_waybar(
            &[EmotionSpectrum::SmugSuperior, EmotionSpectrum::CuriousHacker],
            WaybarState::Success,
            Some(&outcome.scratchpad_summary),
        )
    };

    print_bot_inline_face(&session.settings, face, &outcome.reply_for_user);
    session.push_conversation_turn(raw_input.to_string(), outcome.reply_for_user);
}

fn sync_package_manager_preference(line: &str, session: &mut ReplSession) {
    if let Some(pm) = detect_package_manager_preference(line) {
        session.settings.tools.package_manager = pm.to_string();
        if let Ok(mut pad) = Scratchpad::load(&session.paths.scratchpad) {
            pad.set_package_manager(pm);
            let _ = pad.save(&session.paths.scratchpad);
        }
    }
}

async fn handle_pending_login_if_active(
    line: &str,
    session: &mut ReplSession,
) -> Option<LoopAction> {
    let taken_login = {
        let Ok(mut guard) = session.pending_action.lock() else {
            return None;
        };

        if matches!(*guard, PendingAction::None) {
            return None;
        }

        if guard.is_expired_at(Instant::now())
            || is_uninstall_intent(line)
            || SlashCommand::parse(line).is_some()
            || !PendingAction::is_login_confirmation_slang(line)
        {
            *guard = PendingAction::None;
            return None;
        }

        match std::mem::take(&mut *guard) {
            PendingAction::WaitingForLogin {
                service,
                workspace,
                target_query,
                ..
            } => Some((service, workspace, target_query)),
            PendingAction::None => None,
        }
    };

    let (service, workspace, target_query) = taken_login?;
    let outcome = media::resume_after_login(
        &service,
        workspace,
        &target_query,
        &session.kaomoji_engine,
        &session.settings.general.bot_name,
    )
    .await;

    print_bot_inline_face(&session.settings, outcome.face, &outcome.message);
    session.push_conversation_turn(line.to_string(), outcome.message);

    Some(LoopAction::Continue)
}

pub fn is_uninstall_intent(input: &str) -> bool {
    let normalized = input
        .trim()
        .trim_matches(|c: char| c.is_ascii_punctuation() && c != '/')
        .trim()
        .to_ascii_lowercase();

    if matches!(
        normalized.as_str(),
        "/uninstall" | "uninstall" | "uninstall waish" | "hapus waish" | "copot waish"
    ) {
        return true;
    }

    let words: Vec<&str> = normalized.split_whitespace().collect();
    let has_waish = words.contains(&"waish");
    let has_verb = words
        .iter()
        .any(|&w| matches!(w, "uninstall" | "hapus" | "copot" | "remove"));

    has_waish && has_verb && words.len() <= 6
}

pub fn resolve_uninstall_script(paths: &Paths) -> Option<PathBuf> {
    let in_config = paths.dir.join("uninstall.sh");
    if in_config.is_file() {
        return Some(in_config);
    }
    let in_cwd = PathBuf::from("./uninstall.sh");
    if in_cwd.is_file() {
        return Some(in_cwd);
    }
    None
}

fn run_uninstall_flow(session: &ReplSession) -> crate::Result<LoopAction> {
    let Some(script_path) = resolve_uninstall_script(&session.paths) else {
        print_bot_inline(
            &session.settings,
            Kaomoji::Confused,
            &format!(
                "Skrip {} tidak ditemukan di {} maupun {}.",
                "uninstall.sh".cyan(),
                ui::tilde(&session.paths.dir.join("uninstall.sh")).cyan(),
                "./uninstall.sh".cyan()
            ),
        );
        return Ok(LoopAction::Continue);
    };

    let _ = crossterm::terminal::disable_raw_mode();
    let _ = io::stdout().flush();

    let _ = Command::new("bash")
        .arg(&script_path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    std::process::exit(0);
}

fn print_initial_greeting(session: &ReplSession) {
    let greeting_emotions: &[EmotionSpectrum] = if is_late_night_hour(chrono::Local::now().hour()) {
        &[EmotionSpectrum::DrowsyIdle]
    } else {
        &[
            EmotionSpectrum::SmugSuperior,
            EmotionSpectrum::CuriousHacker,
            EmotionSpectrum::TsunderePout,
        ]
    };
    let face = session.pick_mood_and_sync_waybar(greeting_emotions, WaybarState::Active, None);
    let lang = crate::wizard::i18n::WizardLang::from_code(&session.settings.general.language);
    let (greeting, help_hint) = lang.repl_greeting(session.user_display_name());
    print_bot_inline_face(
        &session.settings,
        face,
        &format!("{greeting} {}", help_hint.bright_black()),
    );
}

fn print_startup_scratchpad_reminder(session: &ReplSession) {
    if let Ok(pad) = Scratchpad::load(&session.paths.scratchpad) {
        if !pad.is_empty() {
            println!();
            println!(
                "{}",
                ui::render_plain_box("Scratchpad Pending", &pad.render_box_lines(), 2)
            );
        }
    }
}

fn is_casual_greeting_or_name_call(input: &str, bot_name: &str) -> bool {
    let cleaned = input
        .trim()
        .trim_matches(|c: char| c.is_ascii_punctuation())
        .trim()
        .to_ascii_lowercase();
    if cleaned.is_empty() || cleaned.starts_with('/') {
        return false;
    }
    let bot_lower = bot_name.trim().to_ascii_lowercase();
    let short_bot = bot_lower.chars().take(3).collect::<String>();
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    if words.len() > 3 {
        return false;
    }
    words.iter().all(|w| {
        matches!(
            *w,
            "hi" | "hai" | "halo" | "hallo" | "hello" | "hey" | "hei" | "oi" | "oy" | "yo"
                | "yoo" | "p" | "psst" | "pagi" | "siang" | "sore" | "malam" | "tes" | "test"
        ) || (!bot_lower.is_empty() && *w == bot_lower)
            || (short_bot.len() >= 3 && *w == short_bot)
    })
}

fn normalize_response_tool_calls(resp: &mut StreamResponse, allow_extract: bool) {
    if resp.tool_calls.is_empty() && !resp.text.is_empty() {
        let (cleaned_text, intercepted_calls) =
            crate::tools::runner::intercept_text_tool_calls(&resp.text);
        if !intercepted_calls.is_empty() {
            resp.text = cleaned_text;
            if allow_extract {
                resp.tool_calls = intercepted_calls;
            }
        }
    }
}

fn should_trigger_system_update_fallback(
    input: &str,
    history: &[ConversationTurn],
    assistant_text: &str,
) -> bool {
    let lower = input.trim().to_ascii_lowercase();
    if lower.is_empty() || lower.starts_with('/') || lower.contains('?') {
        return false;
    }

    let blocked_phrases = [
        "gimana",
        "bagaimana",
        "cara ",
        "apa bedanya",
        "kenapa",
        "mengapa",
        "how to",
        "what is",
        "why ",
        "jangan update",
        "jgn update",
        "gak jadi",
        "ga jadi",
        "nggak jadi",
        "batal",
        "cancel",
        "nanti aja",
        "ntar aja",
    ];
    if blocked_phrases.iter().any(|p| lower.contains(p)) {
        return false;
    }

    let mentions_single_pkg_op = ["install ", "pasang ", "hapus ", "remove ", "uninstall "]
        .iter()
        .any(|p| lower.contains(p));
    let mentions_system_update_imperative = [
        "updatein",
        "update-in",
        "perbarui sistem",
        "perbarui arch",
        "update sistem",
        "update arch",
        "update linux",
        "upgrade sistem",
        "upgrade arch",
        "update paket",
        "update semua paket",
        "pacman -syu",
        "yay -syu",
        "paru -syu",
        "system update",
        "update my arch",
        "update my system",
        "update sekarang",
        "coba update",
    ]
    .iter()
    .any(|p| lower.contains(p));

    if mentions_system_update_imperative && !mentions_single_pkg_op {
        return true;
    }

    let recent_discussed_update = history.iter().rev().take(2).any(|turn| {
        let u = turn.user.to_ascii_lowercase();
        let a = turn.assistant.to_ascii_lowercase();
        let combined = format!("{u} {a}");
        [
            "updatein",
            "update sistem",
            "update arch",
            "perbarui sistem",
            "pacman -syu",
            "yay -syu",
            "paru -syu",
            "sudo pacman",
            "update paket",
            "system update",
        ]
        .iter()
        .any(|k| combined.contains(k))
    });

    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();

    let is_greenlight_cue = tokens.iter().any(|&t| {
        matches!(
            t,
            "gas" | "gass" | "gasss" | "gasin" | "gaskeun" | "jalanin" | "jalankan" | "eksekusi"
                | "lanjut" | "lanjutkan"
        )
    }) || [
        "yaudah",
        "ya udah",
        "sekarang coba",
        "coba sekarang",
        "coba pake sudo pacman",
        "coba pake pacman",
        "coba pake yay",
        "coba pake paru",
        "pake sudo pacman aja",
        "go ahead",
        "do it",
        "run it",
    ]
    .iter()
    .any(|p| lower.contains(p));

    if recent_discussed_update && is_greenlight_cue && !mentions_single_pkg_op {
        return true;
    }

    let reply_low = assistant_text.to_ascii_lowercase();
    let assistant_promised_update = (reply_low.contains("update") || reply_low.contains("perbarui"))
        && (reply_low.contains("akan ")
            || reply_low.contains("coba ")
            || reply_low.contains("langsung ")
            || reply_low.contains("sekarang")
            || reply_low.contains("pacman")
            || reply_low.contains("yay")
            || reply_low.contains("paru"));

    is_greenlight_cue && assistant_promised_update && !mentions_single_pkg_op
}

fn synthesize_system_update_tool_call(settings: &Settings) -> ToolCall {
    let pm = settings.tools.preferred_package_manager();
    let cmd = resolve_system_update_command(pm);
    let args = serde_json::json!({
        "command": cmd,
        "intent": format!("Memperbarui seluruh paket sistem menggunakan {pm}"),
        "requires_root": true
    })
    .to_string();
    ToolCall {
        id: "call_immediate_update_trigger".to_string(),
        name: RUN_SHELL_COMMAND_NAME.to_string(),
        arguments: args,
    }
}

pub fn build_inline_correction_steering_prompt(
    rejected_command: &str,
    correction_text: &str,
) -> String {
    format!(
        "[User Inline Correction]: Pengguna menolak perintah '{}' dan memberikan instruksi perbaikan langsung: \"{}\". Evaluasi koreksi ini dan segera berikan tool call atau tindakan yang sesuai dengan permintaan pengguna!",
        rejected_command,
        correction_text.trim()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReactFsmState {
    ExecutingTools,
    AwaitingFollowup { allow_tools: bool },
    EvaluatingResponse,
    Completed,
}

#[derive(Debug, Default)]
struct TurnVerificationTracker {
    expected_files: Vec<PathBuf>,
    pending_gui_verification: bool,
    unresolved_system_notice: bool,
    last_round_failed_non_aborted: bool,
    consecutive_shell_failures: usize,
    guard_rejections: usize,
}

impl TurnVerificationTracker {
    fn from_user_input(input: &str, companion_user_name: &str) -> Self {
        Self {
            expected_files: extract_expected_creation_files(input, companion_user_name),
            ..Self::default()
        }
    }

    fn record_round(
        &mut self,
        calls: &[ToolCall],
        results: &[crate::tools::runner::ToolExecutionResult],
        companion_user_name: &str,
    ) {
        for (tc, res) in calls.iter().zip(results.iter()) {
            if matches!(
                tc.name.as_str(),
                MANAGE_SPREADSHEET_NAME
                    | MANAGE_DOCUMENT_NAME
                    | WRITE_FILE_NAME
                    | EXECUTE_RECIPE_NAME
            ) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                    let is_creating = if tc.name == EXECUTE_RECIPE_NAME {
                        matches!(
                            val.get("recipe")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or(""),
                            "create_and_open_document"
                                | "create_and_open"
                                | "hot_edit_document"
                                | "hot_edit"
                        )
                    } else {
                        val.get("action")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("write")
                            != "read"
                    };
                    if is_creating {
                        if let Some(p) = val.get("path").and_then(serde_json::Value::as_str) {
                            let clean = crate::tools::document::resolve_document_target_path(
                                p,
                                companion_user_name,
                            );
                            if !self.expected_files.contains(&clean) {
                                self.expected_files.push(clean);
                            }
                        }
                    }
                }
            }

            if matches!(
                tc.name.as_str(),
                MANAGE_WORKSPACE_NAME | LAUNCH_GUI_APP_NAME | EXECUTE_RECIPE_NAME
            ) {
                self.pending_gui_verification = false;
            }

            if tc.name == RUN_SHELL_COMMAND_NAME {
                if res.succeeded {
                    self.consecutive_shell_failures = 0;
                    self.unresolved_system_notice = false;
                } else {
                    if res.output_for_llm.contains("[System Notice]:")
                        || res.output_for_llm == AD_HOC_OFFICE_SCRIPT_NOTICE
                        || res.output_for_llm == BACKGROUND_GUI_SHELL_NOTICE
                        || res.output_for_llm == ALARM_FS_RUMMAGE_NOTICE
                    {
                        self.unresolved_system_notice = true;
                    }
                    if !res.cancelled_cleanly && res.course_correction.is_none() {
                        self.consecutive_shell_failures += 1;
                    }
                }
            } else if res.succeeded {
                self.unresolved_system_notice = false;
            }
        }

        let any_user_aborted = results.iter().any(|r| {
            r.cancelled_cleanly
                || r.output_for_llm.contains("status: interrupted")
                || r.output_for_llm.contains("status: cancelled_by_user")
                || r.output_for_llm.contains("do_not_retry: true")
        });
        let all_ok = results.iter().all(|r| r.succeeded);
        self.last_round_failed_non_aborted = !all_ok && !any_user_aborted;
    }

    fn has_missing_expected_file(&self) -> bool {
        self.expected_files
            .iter()
            .any(|p| !p.exists() || fs::metadata(p).map(|m| m.len() == 0).unwrap_or(true))
    }

    fn should_reject_completion(&self, assistant_text: &str, unexecuted_calls: usize) -> bool {
        if self.guard_rejections >= MAX_COMPLETION_GUARD_RETRIES
            || self.consecutive_shell_failures >= 2
        {
            return false;
        }
        let unverified = unexecuted_calls > 0
            || self.has_missing_expected_file()
            || self.pending_gui_verification
            || self.unresolved_system_notice;

        if (unverified || self.last_round_failed_non_aborted) && assistant_text.trim().is_empty() {
            return true;
        }
        unverified && looks_like_premature_closing_claim(assistant_text)
    }
}

pub fn looks_like_premature_closing_claim(text: &str) -> bool {
    let low = text.trim().to_ascii_lowercase();
    if low.is_empty() || low.contains('?') && low.len() < 40 {
        return false;
    }
    [
        "semua selesai",
        "semua langkah",
        "sudah selesai",
        "udah selesai",
        "beres",
        "sudah dibuat",
        "udah dibuat",
        "sudah tersimpan",
        "sudah dibuka",
        "udah dibuka",
        "sudah siap",
        "all done",
        "successfully created",
        "completed",
    ]
    .iter()
    .any(|kw| low.contains(kw))
}

pub fn looks_like_empty_action_promise(user_input: &str, assistant_text: &str) -> bool {
    let u_low = user_input.trim().to_ascii_lowercase();
    let a_low = assistant_text.trim().to_ascii_lowercase();
    if u_low.is_empty() || a_low.is_empty() || u_low.starts_with('/') || u_low.contains('?') {
        return false;
    }

    let user_requests_action = [
        "buka ",
        "bukain ",
        "jalankan ",
        "jalanin ",
        "buat ",
        "buatkan ",
        "bikin ",
        "bikinin ",
        "edit ",
        "editin ",
        "ubah ",
        "tambah ",
        "pasang ",
        "setel ",
        "batalkan ",
        "matikan ",
        "install ",
        "tutup ",
        "pindah ",
        "putar ",
        "puter ",
        "open ",
        "launch ",
        "create ",
        "gass",
    ]
    .iter()
    .any(|kw| u_low.contains(kw));
    if !user_requests_action {
        return false;
    }

    [
        "aku bukain",
        "kubukain",
        "aku buka sekarang",
        "aku jalankan",
        "kujalankan",
        "aku buatin",
        "kubuatin",
        "aku bikinin",
        "kubikinin",
        "aku editin",
        "kueditin",
        "aku pasang",
        "kupasang",
        "aku tutup",
        "kututup",
        "tunggu bentar",
        "tunggu sebentar",
        "sebentar aku",
        "bentar ya aku",
        "bentar aku",
        "akan kubuka",
        "akan kujalankan",
        "akan kubuat",
        "langsung kubuka",
        "langsung kubuat",
        "let me open",
        "let me launch",
        "let me create",
        "i'll open",
        "i will open",
        "i'll create",
    ]
    .iter()
    .any(|p| a_low.contains(p))
}

fn extract_expected_creation_files(input: &str, companion_user_name: &str) -> Vec<PathBuf> {
    let low = input.to_ascii_lowercase();
    let is_create_intent = [
        "buat", "buatkan", "bikin", "bikinin", "tulis", "simpan", "ekspor", "export", "create",
        "generate", "write", "save",
    ]
    .iter()
    .any(|kw| low.contains(kw));
    let is_delete_or_read_only = ["hapus", "delete", "rm ", "baca ", "lihat "]
        .iter()
        .any(|kw| low.contains(kw))
        && !is_create_intent;
    if !is_create_intent || is_delete_or_read_only {
        return Vec::new();
    }

    let mut out = Vec::new();
    for raw_tok in input.split_whitespace() {
        let trimmed = raw_tok.trim_matches(|c: char| {
            matches!(c, '\'' | '"' | '`' | ',' | ';' | ')' | '(' | '[' | ']')
        });
        if !(trimmed.starts_with('/') || trimmed.starts_with("~/")) {
            continue;
        }
        let ext_ok = [".xlsx", ".csv", ".docx", ".ods", ".odt"]
            .iter()
            .any(|ext| trimmed.to_ascii_lowercase().ends_with(ext));
        if ext_ok {
            let clean = sanitize_path_for_user(trimmed, companion_user_name);
            if !out.contains(&clean) {
                out.push(clean);
            }
        }
    }
    out
}

async fn handle_streaming_chat(input: &str, session: &mut ReplSession) {
    session.workspace_state.clear_turn_ipc_verified();
    let ctx = session.assembler.assemble(
        &session.settings,
        &session.paths,
        &session.conversation,
        input,
    );

    let initial_emotions: &[EmotionSpectrum] = if is_sweet_or_compliment_input(input) {
        &[EmotionSpectrum::FlusteredBlush]
    } else if is_casual_greeting_or_name_call(input, &session.settings.general.bot_name) {
        &[
            EmotionSpectrum::SmugSuperior,
            EmotionSpectrum::MelodicChill,
            EmotionSpectrum::FlusteredBlush,
            EmotionSpectrum::CuriousHacker,
        ]
    } else if session.is_drowsy_idle() {
        &[EmotionSpectrum::DrowsyIdle]
    } else {
        &[
            EmotionSpectrum::SmugSuperior,
            EmotionSpectrum::CuriousHacker,
            EmotionSpectrum::TsunderePout,
        ]
    };
    let initial_face =
        session.pick_mood_and_sync_waybar(initial_emotions, WaybarState::Active, None);

    let mut current_resp = match session
        .client
        .stream_chat_with_face(&session.settings, &ctx, initial_face)
        .await
    {
        Ok(resp) => resp,
        Err(err) => {
            let err_face = session.pick_mood_and_sync_waybar(
                &[
                    EmotionSpectrum::MelancholyFail,
                    EmotionSpectrum::SystemPanic,
                ],
                WaybarState::Warning,
                None,
            );
            err.print_friendly_with_face(&session.settings, err_face);
            return;
        }
    };

    normalize_response_tool_calls(&mut current_resp, true);

    if current_resp.tool_calls.is_empty()
        && should_trigger_system_update_fallback(input, &session.conversation, &current_resp.text)
    {
        current_resp
            .tool_calls
            .push(synthesize_system_update_tool_call(&session.settings));
    }

    let mut tracker =
        TurnVerificationTracker::from_user_input(input, &session.settings.general.user_real_name);

    if current_resp.tool_calls.is_empty()
        && looks_like_empty_action_promise(input, &current_resp.text)
    {
        let mut nudge_ctx = ctx.clone();
        nudge_ctx.messages.push(crate::engine::context::ChatMessage::assistant(
            &current_resp.text,
        ));
        nudge_ctx.messages.push(crate::engine::context::ChatMessage::user(
            EMPTY_PROMISE_GUARD_WARNING,
        ));
        let nudge_face = session.pick_mood_and_sync_waybar(
            &[EmotionSpectrum::CuriousHacker, EmotionSpectrum::SmugSuperior],
            WaybarState::Active,
            None,
        );
        if let Ok(mut retry_resp) = session
            .client
            .stream_chat_with_face(&session.settings, &nudge_ctx, nudge_face)
            .await
        {
            normalize_response_tool_calls(&mut retry_resp, true);
            current_resp = retry_resp;
        }
        if current_resp.tool_calls.is_empty() {
            println!("  {}", EMPTY_PROMISE_STALL_FALLBACK_MSG.yellow());
            if current_resp.text.trim().is_empty() {
                current_resp.text = EMPTY_PROMISE_STALL_FALLBACK_MSG.to_string();
            } else {
                current_resp.text =
                    format!("{}\n{}", current_resp.text, EMPTY_PROMISE_STALL_FALLBACK_MSG);
            }
            session.push_conversation_turn(input.to_string(), current_resp.text);
            return;
        }
    }

    if current_resp.tool_calls.is_empty() && !tracker.should_reject_completion(&current_resp.text, 0)
    {
        if !current_resp.text.is_empty() {
            session.push_conversation_turn(input.to_string(), current_resp.text);
        }
        return;
    }

    let lang = session.lang();
    let mut completed_rounds: Vec<ToolRound> = Vec::new();
    let mut assistant_segments: Vec<String> = Vec::new();
    let mut max_rounds_budget = MAX_REACT_ROUNDS;
    let mut followup_face = initial_face;

    let mut fsm_state = if !current_resp.tool_calls.is_empty() {
        ReactFsmState::ExecutingTools
    } else {
        ReactFsmState::EvaluatingResponse
    };

    while fsm_state != ReactFsmState::Completed {
        match fsm_state {
            ReactFsmState::ExecutingTools => {
                if !current_resp.text.trim().is_empty() {
                    assistant_segments.push(current_resp.text.clone());
                }

                let tool_ctx = ToolExecutionContext {
                    settings: &session.settings,
                    paths: &session.paths,
                    shadow_git: &session.shadow_git,
                    watcher_state: Arc::clone(&session.watcher_state),
                    workspace_state: Arc::clone(&session.workspace_state),
                    pending_action: Arc::clone(&session.pending_action),
                    kaomoji_engine: Arc::clone(&session.kaomoji_engine),
                    show_progress: true,
                };

                let mut tool_results = Vec::with_capacity(current_resp.tool_calls.len());
                let mut inline_correction = None;
                let mut cancelled_cleanly = false;

                for tc in &current_resp.tool_calls {
                    let status_plain = lang.runner_executing(&tc.name);
                    let _ = session.pick_mood_and_sync_waybar(
                        &[EmotionSpectrum::CuriousHacker, EmotionSpectrum::SmugSuperior],
                        WaybarState::Busy,
                        Some(&status_plain),
                    );
                    println!(
                        "  {} {}",
                        RUNNER_KAOMOJI.bright_magenta(),
                        lang.runner_executing(&tc.name.cyan().to_string())
                    );
                    let res = execute_tool_call(tc, &tool_ctx).await;
                    if res.course_correction.is_some() {
                        inline_correction = res.course_correction.clone();
                        tool_results.push(res);
                        break;
                    }
                    if res.cancelled_cleanly {
                        cancelled_cleanly = true;
                        tool_results.push(res);
                        break;
                    }
                    tool_results.push(res);
                }

                if cancelled_cleanly && inline_correction.is_none() {
                    let _ = session.pick_mood_and_sync_waybar(
                        &[EmotionSpectrum::TsunderePout, EmotionSpectrum::ProtectiveCare],
                        WaybarState::Idle,
                        None,
                    );
                    return;
                }

                tracker.record_round(
                    &current_resp.tool_calls,
                    &tool_results,
                    &session.settings.general.user_real_name,
                );

                let steering_message = if let Some(ref corr) = inline_correction {
                    sync_package_manager_preference(&corr.feedback, session);
                    max_rounds_budget = (max_rounds_budget + 1).min(MAX_REACT_ROUNDS + 3);
                    Some(build_inline_correction_steering_prompt(
                        &corr.rejected_command,
                        &corr.feedback,
                    ))
                } else if tracker.consecutive_shell_failures >= 2 {
                    Some(SHELL_EXPERIMENT_CAP_WARNING.to_string())
                } else {
                    None
                };

                let all_succeeded = tool_results.iter().all(|r| r.succeeded);
                let was_cancelled_or_interrupted = inline_correction.is_none()
                    && tool_results.iter().any(|r| {
                        r.output_for_llm.contains("status: interrupted")
                            || r.output_for_llm.contains("status: cancelled_by_user")
                            || r.output_for_llm.contains("do_not_retry: true")
                    });
                let ran_media_playing = current_resp
                    .tool_calls
                    .iter()
                    .any(|tc| matches!(tc.name.as_str(), OPEN_MEDIA_NAME | CONTROL_MEDIA_NAME))
                    && all_succeeded
                    && tool_results.iter().any(|r| {
                        r.output_for_llm.contains("Playing")
                            || r.output_for_llm.contains("status: playing")
                    });

                completed_rounds.push(ToolRound {
                    assistant_text: current_resp.text,
                    tool_calls: current_resp.tool_calls,
                    tool_results,
                    steering_message,
                });
                current_resp = StreamResponse::default();

                followup_face = if inline_correction.is_some() {
                    session.pick_mood_and_sync_waybar(
                        &[EmotionSpectrum::CuriousHacker, EmotionSpectrum::ProtectiveCare],
                        WaybarState::Active,
                        None,
                    )
                } else if ran_media_playing {
                    session.pick_mood_and_sync_waybar(
                        &[EmotionSpectrum::MelodicChill],
                        WaybarState::Media,
                        None,
                    )
                } else if all_succeeded {
                    session.pick_mood_and_sync_waybar(
                        &[
                            EmotionSpectrum::SmugSuperior,
                            EmotionSpectrum::CuriousHacker,
                        ],
                        WaybarState::Success,
                        None,
                    )
                } else {
                    session.pick_mood_and_sync_waybar(
                        &[
                            EmotionSpectrum::MelancholyFail,
                            EmotionSpectrum::SystemPanic,
                        ],
                        WaybarState::Warning,
                        None,
                    )
                };

                let allow_tools = inline_correction.is_some()
                    || (!was_cancelled_or_interrupted
                        && tracker.consecutive_shell_failures < 2
                        && completed_rounds.len() < max_rounds_budget);
                fsm_state = ReactFsmState::AwaitingFollowup { allow_tools };
            }

            ReactFsmState::AwaitingFollowup { allow_tools } => {
                match session
                    .client
                    .stream_react_followup_with_face(
                        &session.settings,
                        &ctx,
                        &completed_rounds,
                        allow_tools,
                        followup_face,
                    )
                    .await
                {
                    Ok(mut next_resp) => {
                        normalize_response_tool_calls(&mut next_resp, allow_tools);
                        current_resp = next_resp;
                        fsm_state = ReactFsmState::EvaluatingResponse;
                    }
                    Err(err) => {
                        let err_face = session.pick_mood_and_sync_waybar(
                            &[
                                EmotionSpectrum::MelancholyFail,
                                EmotionSpectrum::SystemPanic,
                            ],
                            WaybarState::Warning,
                            None,
                        );
                        err.print_friendly_with_face(&session.settings, err_face);
                        return;
                    }
                }
            }

            ReactFsmState::EvaluatingResponse => {
                if !current_resp.tool_calls.is_empty() {
                    if completed_rounds.len() < max_rounds_budget
                        && tracker.consecutive_shell_failures < 2
                    {
                        fsm_state = ReactFsmState::ExecutingTools;
                    } else if current_resp.text.trim().is_empty() {
                        followup_face = session.pick_mood_and_sync_waybar(
                            &[EmotionSpectrum::SmugSuperior, EmotionSpectrum::CuriousHacker],
                            WaybarState::Active,
                            None,
                        );
                        current_resp.tool_calls.clear();
                        fsm_state = ReactFsmState::AwaitingFollowup { allow_tools: false };
                    } else {
                        fsm_state = ReactFsmState::Completed;
                    }
                } else if tracker
                    .should_reject_completion(&current_resp.text, current_resp.tool_calls.len())
                {
                    tracker.guard_rejections += 1;
                    max_rounds_budget = (max_rounds_budget + 1).min(MAX_REACT_ROUNDS + 3);
                    if !current_resp.text.trim().is_empty() {
                        assistant_segments.push(current_resp.text.clone());
                    }
                    if let Some(last_round) = completed_rounds.last_mut() {
                        last_round.steering_message = Some(COMPLETION_GUARD_WARNING.to_string());
                    } else {
                        let mut guard_ctx = ctx.clone();
                        guard_ctx.messages.push(
                            crate::engine::context::ChatMessage::assistant(&current_resp.text),
                        );
                        guard_ctx.messages.push(
                            crate::engine::context::ChatMessage::user(COMPLETION_GUARD_WARNING),
                        );
                        followup_face = session.pick_mood_and_sync_waybar(
                            &[EmotionSpectrum::CuriousHacker, EmotionSpectrum::TsunderePout],
                            WaybarState::Warning,
                            None,
                        );
                        match session
                            .client
                            .stream_chat_with_face(&session.settings, &guard_ctx, followup_face)
                            .await
                        {
                            Ok(mut retry_resp) => {
                                normalize_response_tool_calls(&mut retry_resp, true);
                                current_resp = retry_resp;
                                fsm_state = ReactFsmState::EvaluatingResponse;
                                continue;
                            }
                            Err(err) => {
                                let err_face = session.pick_mood_and_sync_waybar(
                                    &[
                                        EmotionSpectrum::MelancholyFail,
                                        EmotionSpectrum::SystemPanic,
                                    ],
                                    WaybarState::Warning,
                                    None,
                                );
                                err.print_friendly_with_face(&session.settings, err_face);
                                return;
                            }
                        }
                    }
                    followup_face = session.pick_mood_and_sync_waybar(
                        &[EmotionSpectrum::CuriousHacker, EmotionSpectrum::TsunderePout],
                        WaybarState::Warning,
                        None,
                    );
                    fsm_state = ReactFsmState::AwaitingFollowup { allow_tools: true };
                } else {
                    fsm_state = ReactFsmState::Completed;
                }
            }

            ReactFsmState::Completed => break,
        }
    }

    if !current_resp.text.trim().is_empty() {
        assistant_segments.push(current_resp.text);
    }

    let combined = assistant_segments.join("\n");
    drop(completed_rounds);
    drop(assistant_segments);

    if !combined.is_empty() {
        session.push_conversation_turn(input.to_string(), combined);
    }
}

pub fn execute_slash_command(
    cmd: SlashCommand<'_>,
    session: &mut ReplSession,
) -> crate::Result<LoopAction> {
    match cmd {
        SlashCommand::Shell(command) => {
            run_local_shell(command, session);
            Ok(LoopAction::Continue)
        }
        SlashCommand::Clear => {
            print!("\x1b[2J\x1b[H");
            io::stdout().flush()?;
            Ok(LoopAction::Continue)
        }
        SlashCommand::Drop => {
            let lang = session.lang();
            let cleared = session.conversation.len();
            session.conversation.clear();
            session.conversation.shrink_to_fit();
            let face = session.pick_mood_and_sync_waybar(
                &[EmotionSpectrum::SmugSuperior, EmotionSpectrum::CuriousHacker],
                WaybarState::Success,
                None,
            );
            print_bot_inline_face(
                &session.settings,
                face,
                &lang.drop_cleared(cleared, &"memory.md".cyan().to_string()),
            );
            Ok(LoopAction::Continue)
        }
        SlashCommand::Model(maybe_model) => {
            handle_model_command(maybe_model, session);
            Ok(LoopAction::Continue)
        }
        SlashCommand::Memory => {
            let _ = commands::show_memory(&session.paths)?;
            Ok(LoopAction::Continue)
        }
        SlashCommand::Note(rest) => {
            handle_note_command(rest, session)?;
            Ok(LoopAction::Continue)
        }
        SlashCommand::Telemetry => {
            print_telemetry_box(session);
            Ok(LoopAction::Continue)
        }
        SlashCommand::Uninstall => run_uninstall_flow(session),
        SlashCommand::Help => {
            print_help_box(session);
            Ok(LoopAction::Continue)
        }
        SlashCommand::Exit => Ok(LoopAction::Exit),
        SlashCommand::Unknown(unknown) => {
            let lang = session.lang();
            let face = session.pick_mood_and_sync_waybar(
                &[EmotionSpectrum::TsunderePout, EmotionSpectrum::ProtectiveCare],
                WaybarState::Warning,
                None,
            );
            print_bot_inline_face(
                &session.settings,
                face,
                &lang.unknown_slash_cmd(
                    &unknown.yellow().to_string(),
                    &"/help".cyan().bold().to_string(),
                ),
            );
            Ok(LoopAction::Continue)
        }
    }
}

fn handle_note_command(rest: &str, session: &ReplSession) -> crate::Result<()> {
    let lang = session.lang();
    let mut pad = Scratchpad::load(&session.paths.scratchpad)?;
    let trimmed = rest.trim();

    if trimmed.is_empty() {
        let face = session.pick_mood_and_sync_waybar(
            &[EmotionSpectrum::CuriousHacker, EmotionSpectrum::SmugSuperior],
            WaybarState::Active,
            None,
        );
        print_bot_header_face(&session.settings, face);
        println!(
            "{}",
            ui::render_plain_box("Scratchpad", &pad.render_box_lines(), 2)
        );
        return Ok(());
    }

    if trimmed.eq_ignore_ascii_case("clear") {
        let count = pad.clear();
        pad.save(&session.paths.scratchpad)?;
        let face = session.pick_mood_and_sync_waybar(
            &[EmotionSpectrum::SmugSuperior, EmotionSpectrum::CuriousHacker],
            WaybarState::Success,
            None,
        );
        print_bot_inline_face(&session.settings, face, &lang.scratchpad_cleared(count));
        return Ok(());
    }

    if let Some(id_str) = trimmed.strip_prefix("del ").or_else(|| trimmed.strip_prefix("rm ")) {
        match id_str.trim().parse::<u64>() {
            Ok(id) if pad.remove_note(id) => {
                pad.save(&session.paths.scratchpad)?;
                let face = session.pick_mood_and_sync_waybar(
                    &[EmotionSpectrum::SmugSuperior, EmotionSpectrum::CuriousHacker],
                    WaybarState::Success,
                    None,
                );
                print_bot_inline_face(&session.settings, face, &lang.scratchpad_removed(id));
            }
            Ok(id) => {
                let face = session.pick_mood_and_sync_waybar(
                    &[EmotionSpectrum::TsunderePout, EmotionSpectrum::MelancholyFail],
                    WaybarState::Warning,
                    None,
                );
                print_bot_inline_face(&session.settings, face, &lang.scratchpad_not_found(id));
            }
            Err(_) => {
                let face = session.pick_mood_and_sync_waybar(
                    &[EmotionSpectrum::TsunderePout],
                    WaybarState::Warning,
                    None,
                );
                print_bot_inline_face(&session.settings, face, lang.scratchpad_del_usage());
            }
        }
        return Ok(());
    }

    let note = pad.add_note("memo", trimmed);
    pad.save(&session.paths.scratchpad)?;
    let face = session.pick_mood_and_sync_waybar(
        &[EmotionSpectrum::SmugSuperior, EmotionSpectrum::CuriousHacker],
        WaybarState::Success,
        None,
    );
    print_bot_inline_face(
        &session.settings,
        face,
        &lang.scratchpad_saved(note.id, &note.content.bold().to_string()),
    );
    Ok(())
}

fn print_telemetry_box(session: &ReplSession) {
    let snap = HardwareSnapshot::collect();
    let mut lines = snap.format_lines();
    if let Ok(ws) = session.watcher_state.lock() {
        let promise_str = match ws.promise_state {
            BatteryPromiseState::None => "Normal (aktif memantau)".to_string(),
            BatteryPromiseState::Postponed { .. } => "Ditunda 45 menit (mode di luar)".to_string(),
            BatteryPromiseState::PromisedToCharge { noted_capacity } => {
                format!("Berjanji charge (dicatat saat {noted_capacity}%)")
            }
        };
        lines.push(format!("Watcher  : {promise_str}"));
    }
    let ws_label = match session.workspace_state.get() {
        Some(ws) => format!("Companion WS {ws}"),
        None => "Belum dialokasikan".to_string(),
    };
    lines.push(format!("Workspace: {ws_label}"));
    if let Ok(eng) = session.kaomoji_engine.lock() {
        lines.push(format!(
            "Mood     : {} ({})",
            eng.current_kaomoji(),
            eng.current_emotion().label()
        ));
    }

    let face = session.pick_mood_and_sync_waybar(
        &[
            EmotionSpectrum::CuriousHacker,
            EmotionSpectrum::SmugSuperior,
        ],
        WaybarState::Active,
        None,
    );
    print_bot_header_face(&session.settings, face);
    println!("{}", ui::render_plain_box("Hardware Telemetry", &lines, 2));
}

fn run_local_shell(command: &str, session: &ReplSession) {
    let lang = session.lang();
    if command.is_empty() {
        print_bot_inline(
            &session.settings,
            Kaomoji::Warn,
            &lang.sh_missing_cmd(
                &"/sh".cyan().bold().to_string(),
                &"/sh uname -sr".cyan().to_string(),
            ),
        );
        return;
    }

    let shell = match session.settings.tools.shell_path.trim() {
        configured if !configured.is_empty() && Path::new(configured).exists() => configured,
        _ => "/bin/sh",
    };

    let status = Command::new(shell)
        .arg("-c")
        .arg(command)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    match status {
        Ok(s) if s.success() => {
            let _ = session.pick_mood_and_sync_waybar(
                &[EmotionSpectrum::SmugSuperior, EmotionSpectrum::CuriousHacker],
                WaybarState::Success,
                None,
            );
        }
        Ok(s) => {
            let code = s
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            let face = session.pick_mood_and_sync_waybar(
                &[EmotionSpectrum::MelancholyFail, EmotionSpectrum::SystemPanic],
                WaybarState::Warning,
                None,
            );
            print_bot_inline_face(
                &session.settings,
                face,
                &lang.sh_exit_status(&code.red().bold().to_string()),
            );
        }
        Err(err) => {
            let face = session.pick_mood_and_sync_waybar(
                &[EmotionSpectrum::SystemPanic, EmotionSpectrum::MelancholyFail],
                WaybarState::Warning,
                None,
            );
            print_bot_inline_face(
                &session.settings,
                face,
                &lang.sh_spawn_failed(shell, &err),
            );
        }
    }
}

fn handle_model_command(maybe_model: Option<&str>, session: &mut ReplSession) {
    let lang = session.lang();
    match maybe_model {
        None => {
            let current = if session.settings.api.model.is_empty() {
                "(unset)"
            } else {
                &session.settings.api.model
            };
            let provider = if session.settings.api.provider.is_empty() {
                "unknown"
            } else {
                &session.settings.api.provider
            };
            let face = session.pick_mood_and_sync_waybar(
                &[EmotionSpectrum::CuriousHacker, EmotionSpectrum::SmugSuperior],
                WaybarState::Active,
                None,
            );
            print_bot_inline_face(
                &session.settings,
                face,
                &lang.model_current(
                    &current.cyan().bold().to_string(),
                    &provider.bold().to_string(),
                    &"/model <model_name>".cyan().to_string(),
                ),
            );
        }
        Some(new_model) => {
            let previous = std::mem::replace(
                &mut session.settings.api.model,
                new_model.trim().to_string(),
            );
            let prev_display = if previous.is_empty() {
                "(empty)".to_string()
            } else {
                previous
            };
            let face = session.pick_mood_and_sync_waybar(
                &[EmotionSpectrum::SmugSuperior, EmotionSpectrum::CuriousHacker],
                WaybarState::Success,
                None,
            );
            print_bot_inline_face(
                &session.settings,
                face,
                &lang.model_switched(
                    &prev_display.bright_black().to_string(),
                    &session.settings.api.model.cyan().bold().to_string(),
                ),
            );
        }
    }
}

fn print_help_box(session: &ReplSession) {
    let lang = session.lang();
    let face = session.pick_mood_and_sync_waybar(
        &[EmotionSpectrum::CuriousHacker, EmotionSpectrum::SmugSuperior],
        WaybarState::Active,
        None,
    );
    print_bot_header_face(&session.settings, face);
    let rows = lang.help_rows();
    println!("{}", ui::render_plain_box(lang.help_box_title(), &rows, 2));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_session() -> ReplSession {
        let mut settings = Settings::default();
        settings.general.bot_name = "alice".to_string();
        settings.general.user_real_name = "steve".to_string();
        settings.api.provider = "anthropic".to_string();
        settings.api.model = "claude-3-5-sonnet-latest".to_string();
        let paths = Paths::from_dir(PathBuf::from("/tmp/waish-repl-unit-test"));
        ReplSession::new(settings, paths).unwrap()
    }

    #[test]
    fn parses_all_slash_commands() {
        assert_eq!(
            SlashCommand::parse("/sh ls -la"),
            Some(SlashCommand::Shell("ls -la"))
        );
        assert_eq!(SlashCommand::parse("/sh"), Some(SlashCommand::Shell("")));
        assert_eq!(SlashCommand::parse("/clear"), Some(SlashCommand::Clear));
        assert_eq!(SlashCommand::parse("/drop"), Some(SlashCommand::Drop));
        assert_eq!(
            SlashCommand::parse("/model"),
            Some(SlashCommand::Model(None))
        );
        assert_eq!(
            SlashCommand::parse("/model gpt-4o-mini"),
            Some(SlashCommand::Model(Some("gpt-4o-mini")))
        );
        assert_eq!(SlashCommand::parse("/memory"), Some(SlashCommand::Memory));
        assert_eq!(
            SlashCommand::parse("/note cek port 8080"),
            Some(SlashCommand::Note("cek port 8080"))
        );
        assert_eq!(
            SlashCommand::parse("/telemetry"),
            Some(SlashCommand::Telemetry)
        );
        assert_eq!(
            SlashCommand::parse("/uninstall"),
            Some(SlashCommand::Uninstall)
        );
        assert_eq!(SlashCommand::parse("/help"), Some(SlashCommand::Help));
        assert_eq!(SlashCommand::parse("/exit"), Some(SlashCommand::Exit));
        assert_eq!(SlashCommand::parse("/q"), Some(SlashCommand::Exit));
        assert_eq!(
            SlashCommand::parse("/tidakada arg"),
            Some(SlashCommand::Unknown("/tidakada"))
        );
        assert_eq!(SlashCommand::parse("halo alice"), None);
    }

    #[tokio::test]
    async fn streaming_chat_records_turn_and_drop_clears_buffer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await;
                let headers = concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Type: text/event-stream\r\n",
                    "Transfer-Encoding: chunked\r\n\r\n"
                );
                let _ = socket.write_all(headers.as_bytes()).await;
                let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Siap, steve! ( ^ _ ^ )\"}}]}\n\ndata: [DONE]\n\n";
                let chunk = format!("{:X}\r\n{}\r\n0\r\n\r\n", sse.len(), sse);
                let _ = socket.write_all(chunk.as_bytes()).await;
            }
        });

        let mut session = test_session();
        session.settings.api.provider = "openai".to_string();
        session.settings.api.api_key = "sk-test-1234567890".to_string();
        session.settings.api.base_url = format!("http://{addr}/v1");

        assert_eq!(
            process_line("halo alice", &mut session).await.unwrap(),
            LoopAction::Continue
        );
        assert_eq!(session.conversation.len(), 1);
        assert_eq!(session.conversation[0].user, "halo alice");
        assert_eq!(session.conversation[0].assistant, "Siap, steve! ( ^ _ ^ )");

        assert_eq!(
            process_line("/drop", &mut session).await.unwrap(),
            LoopAction::Continue
        );
        assert!(session.conversation.is_empty());
    }

    #[tokio::test]
    async fn api_error_is_caught_gracefully_without_crashing_repl() {
        let mut session = test_session();
        session.settings.api.api_key.clear();
        let action = process_line("cek error", &mut session).await.unwrap();
        assert_eq!(action, LoopAction::Continue);
        assert!(session.conversation.is_empty());
    }

    #[test]
    fn model_command_updates_runtime_model() {
        let mut session = test_session();
        assert_eq!(
            execute_slash_command(SlashCommand::Model(Some("gpt-4o-mini")), &mut session).unwrap(),
            LoopAction::Continue
        );
        assert_eq!(session.settings.api.model, "gpt-4o-mini");
    }

    #[test]
    fn exit_and_q_trigger_loop_exit() {
        let mut session = test_session();
        assert_eq!(
            execute_slash_command(SlashCommand::Exit, &mut session).unwrap(),
            LoopAction::Exit
        );
    }

    #[test]
    fn detects_uninstall_intents_accurately() {
        assert!(is_uninstall_intent("/uninstall"));
        assert!(is_uninstall_intent("  UNINSTALL  "));
        assert!(is_uninstall_intent("uninstall waish"));
        assert!(is_uninstall_intent("hapus waish"));
        assert!(is_uninstall_intent("tolong hapus waish"));
        assert!(!is_uninstall_intent("uninstall neovim"));
        assert!(!is_uninstall_intent("hapus file log"));
    }

    #[tokio::test]
    async fn closed_loop_tool_call_executes_pty_and_streams_followup() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut s1, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let _ = s1.read(&mut buf).await;
                let headers = concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Type: text/event-stream\r\n",
                    "Transfer-Encoding: chunked\r\n\r\n"
                );
                let _ = s1.write_all(headers.as_bytes()).await;
                let sse1 = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"Oki, aku cek dulu yaa ( •̀ ω •́ )✧\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_99\",\"function\":{\"name\":\"run_shell_command\",\"arguments\":\"{\\\"command\\\":\\\"printf 'arch_test_ok\\\\n'\\\",\\\"intent\\\":\\\"Cek sistem\\\",\\\"requires_root\\\":false}\"}}]}}]}\n\n",
                    "data: [DONE]\n\n"
                );
                let chunk1 = format!("{:X}\r\n{}\r\n0\r\n\r\n", sse1.len(), sse1);
                let _ = s1.write_all(chunk1.as_bytes()).await;
            }

            if let Ok((mut s2, _)) = listener.accept().await {
                let mut req_bytes = Vec::with_capacity(16384);
                let mut chunk_buf = [0u8; 4096];
                for _ in 0..16 {
                    let n = s2.read(&mut chunk_buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req_bytes.extend_from_slice(&chunk_buf[..n]);
                    if String::from_utf8_lossy(&req_bytes).contains("arch_test_ok") {
                        break;
                    }
                }
                let req_str = String::from_utf8_lossy(&req_bytes);
                assert!(
                    req_str.contains("arch_test_ok"),
                    "tool output must be sent back to LLM"
                );

                let headers = concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Type: text/event-stream\r\n",
                    "Transfer-Encoding: chunked\r\n\r\n"
                );
                let _ = s2.write_all(headers.as_bytes()).await;
                let sse2 = "data: {\"choices\":[{\"delta\":{\"content\":\"Beres! Sistem kamu arch_test_ok ( > - < ) v\"}}]}\n\ndata: [DONE]\n\n";
                let chunk2 = format!("{:X}\r\n{}\r\n0\r\n\r\n", sse2.len(), sse2);
                let _ = s2.write_all(chunk2.as_bytes()).await;
            }
        });

        let mut session = test_session();
        session.settings.api.provider = "openai".to_string();
        session.settings.api.api_key = "sk-test-1234567890".to_string();
        session.settings.api.base_url = format!("http://{addr}/v1");

        let action = process_line("cek sistem dong", &mut session).await.unwrap();
        assert_eq!(action, LoopAction::Continue);
        assert_eq!(session.conversation.len(), 1);
        assert!(session.conversation[0].assistant.contains("arch_test_ok"));
    }

    #[tokio::test]
    async fn text_fallback_tool_call_intercepts_and_executes_pty_closed_loop() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut s1, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let _ = s1.read(&mut buf).await;
                let headers = concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Type: text/event-stream\r\n",
                    "Transfer-Encoding: chunked\r\n\r\n"
                );
                let _ = s1.write_all(headers.as_bytes()).await;
                let sse1 = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"[MEMANGGIL TOOLS: run_shell_command(command=\\\"printf 'fallback_pty_ok\\\\n'\\\", intent=\\\"Tes fallback\\\", requires_root=false)]\"}}]}\n\n",
                    "data: [DONE]\n\n"
                );
                let chunk1 = format!("{:X}\r\n{}\r\n0\r\n\r\n", sse1.len(), sse1);
                let _ = s1.write_all(chunk1.as_bytes()).await;
            }

            if let Ok((mut s2, _)) = listener.accept().await {
                let mut req_bytes = Vec::with_capacity(16384);
                let mut chunk_buf = [0u8; 4096];
                for _ in 0..16 {
                    let n = s2.read(&mut chunk_buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req_bytes.extend_from_slice(&chunk_buf[..n]);
                    if String::from_utf8_lossy(&req_bytes).contains("fallback_pty_ok") {
                        break;
                    }
                }
                assert!(
                    String::from_utf8_lossy(&req_bytes).contains("fallback_pty_ok"),
                    "fallback tool output must be sent back to LLM"
                );

                let headers = concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Type: text/event-stream\r\n",
                    "Transfer-Encoding: chunked\r\n\r\n"
                );
                let _ = s2.write_all(headers.as_bytes()).await;
                let sse2 = "data: {\"choices\":[{\"delta\":{\"content\":\"Beres! Output fallback_pty_ok udah masuk ( ˘ω˘ )\"}}]}\n\ndata: [DONE]\n\n";
                let chunk2 = format!("{:X}\r\n{}\r\n0\r\n\r\n", sse2.len(), sse2);
                let _ = s2.write_all(chunk2.as_bytes()).await;
            }
        });

        let mut session = test_session();
        session.settings.api.provider = "openai".to_string();
        session.settings.api.api_key = "sk-test-1234567890".to_string();
        session.settings.api.base_url = format!("http://{addr}/v1");

        let action = process_line("jalankan tes fallback dong", &mut session)
            .await
            .unwrap();
        assert_eq!(action, LoopAction::Continue);
        assert_eq!(session.conversation.len(), 1);
        assert!(session.conversation[0].assistant.contains("fallback_pty_ok"));
        assert!(!session.conversation[0].assistant.contains("MEMANGGIL TOOLS"));
    }

    #[tokio::test]
    async fn multi_turn_react_loop_chains_two_rounds_and_synthesizes_final_answer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut s1, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let _ = s1.read(&mut buf).await;
                let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
                let _ = s1.write_all(headers.as_bytes()).await;
                let sse1 = concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"react_1\",\"function\":{\"name\":\"run_shell_command\",\"arguments\":\"{\\\"command\\\":\\\"printf 'step_one_ok\\\\n'\\\",\\\"intent\\\":\\\"Langkah 1\\\",\\\"requires_root\\\":false}\"}}]}}]}\n\n",
                    "data: [DONE]\n\n"
                );
                let chunk1 = format!("{:X}\r\n{}\r\n0\r\n\r\n", sse1.len(), sse1);
                let _ = s1.write_all(chunk1.as_bytes()).await;
            }

            if let Ok((mut s2, _)) = listener.accept().await {
                let mut req_bytes = Vec::with_capacity(16384);
                let mut chunk_buf = [0u8; 4096];
                for _ in 0..16 {
                    let n = s2.read(&mut chunk_buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req_bytes.extend_from_slice(&chunk_buf[..n]);
                    if String::from_utf8_lossy(&req_bytes).contains("step_one_ok") {
                        break;
                    }
                }
                assert!(String::from_utf8_lossy(&req_bytes).contains("step_one_ok"));

                let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
                let _ = s2.write_all(headers.as_bytes()).await;
                let sse2 = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"[MEMANGGIL TOOL: run_shell_command(command=\\\"printf 'step_two_ok\\\\n'\\\", intent=\\\"Langkah 2\\\", requires_root=false)]\"}}]}\n\n",
                    "data: [DONE]\n\n"
                );
                let chunk2 = format!("{:X}\r\n{}\r\n0\r\n\r\n", sse2.len(), sse2);
                let _ = s2.write_all(chunk2.as_bytes()).await;
            }

            if let Ok((mut s3, _)) = listener.accept().await {
                let mut req_bytes = Vec::with_capacity(16384);
                let mut chunk_buf = [0u8; 4096];
                for _ in 0..16 {
                    let n = s3.read(&mut chunk_buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req_bytes.extend_from_slice(&chunk_buf[..n]);
                    if String::from_utf8_lossy(&req_bytes).contains("step_two_ok") {
                        break;
                    }
                }
                let req_str = String::from_utf8_lossy(&req_bytes);
                assert!(req_str.contains("step_one_ok"));
                assert!(req_str.contains("step_two_ok"));

                let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
                let _ = s3.write_all(headers.as_bytes()).await;
                let sse3 = "data: {\"choices\":[{\"delta\":{\"content\":\"Kedua langkah beres: step_one_ok dan step_two_ok! ( ✧Д✧ )\"}}]}\n\ndata: [DONE]\n\n";
                let chunk3 = format!("{:X}\r\n{}\r\n0\r\n\r\n", sse3.len(), sse3);
                let _ = s3.write_all(chunk3.as_bytes()).await;
            }
        });

        let mut session = test_session();
        session.settings.api.provider = "openai".to_string();
        session.settings.api.api_key = "sk-test-1234567890".to_string();
        session.settings.api.base_url = format!("http://{addr}/v1");

        let action = process_line("jalankan dua langkah berurutan", &mut session)
            .await
            .unwrap();
        assert_eq!(action, LoopAction::Continue);
        assert_eq!(session.conversation.len(), 1);
        assert!(session.conversation[0].assistant.contains("step_one_ok"));
        assert!(session.conversation[0].assistant.contains("step_two_ok"));
    }

    #[tokio::test]
    async fn proactive_consumer_prints_via_external_printer_when_idle() {
        struct MockPrinter(Arc<Mutex<Vec<String>>>);
        impl ExternalPrinter for MockPrinter {
            fn print(&mut self, msg: String) -> rustyline::Result<()> {
                self.0.lock().unwrap().push(msg);
                Ok(())
            }
        }

        let printed = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel(4);
        let idle = Arc::new(AtomicBool::new(true));
        let turns = Arc::new(Mutex::new(Vec::new()));
        let kaomoji_engine = Arc::new(Mutex::new(KaomojiEngine::with_seed(99)));
        let mut settings = Settings::default();
        settings.api.api_key.clear();
        let client = LlmClient::new(&settings).unwrap();
        let paths = Paths::from_dir(PathBuf::from("/tmp/waish-proactive-unit-test"));

        let handle = spawn_proactive_event_consumer(
            rx,
            Some(MockPrinter(Arc::clone(&printed))),
            ProactiveConsumerDeps {
                repl_is_idle: Arc::clone(&idle),
                proactive_turns: Arc::clone(&turns),
                kaomoji_engine: Arc::clone(&kaomoji_engine),
                client,
                settings,
                paths,
            },
        );

        tx.send(ProactiveEvent {
            kind: crate::engine::watcher::ProactiveEventKind::BrokenPromise,
            mood: Kaomoji::Angry,
            system_instruction: "[SYSTEM EVENT: USER_BROKE_PROMISE]".to_string(),
            fallback_message: "Heh! Kenapa charger-nya belum dicolok juga?!".to_string(),
        })
        .await
        .unwrap();
        drop(tx);
        handle.await.unwrap();

        let captured = printed.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert!(captured[0].contains("\r\x1b[2K"));
        assert!(
            EmotionSpectrum::AngryChastise
                .palette()
                .iter()
                .any(|k| captured[0].contains(k)),
            "expected AngryChastise kaomoji in proactive output: {}",
            captured[0]
        );
        assert!(captured[0].contains("Kenapa charger-nya belum dicolok juga?!"));
        assert_eq!(turns.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn waiting_for_login_cancels_on_slash_command_and_expires_after_timeout() {
        let mut session = test_session();

        *session.pending_action.lock().unwrap() = PendingAction::WaitingForLogin {
            service: "spotify".to_string(),
            workspace: 4,
            target_query: "YOASOBI Racing into the Night".to_string(),
            created_at: Instant::now(),
        };
        let action = process_line("/model gpt-4o", &mut session).await.unwrap();
        assert_eq!(action, LoopAction::Continue);
        assert_eq!(session.settings.api.model, "gpt-4o");
        assert!(matches!(
            *session.pending_action.lock().unwrap(),
            PendingAction::None
        ));

        *session.pending_action.lock().unwrap() = PendingAction::WaitingForLogin {
            service: "spotify".to_string(),
            workspace: 4,
            target_query: "Kenshi Yonezu Kick Back".to_string(),
            created_at: Instant::now() - Duration::from_secs(media::LOGIN_TIMEOUT_SECS + 5),
        };
        let res = handle_pending_login_if_active("udah", &mut session).await;
        assert_eq!(res, None);
        assert!(matches!(
            *session.pending_action.lock().unwrap(),
            PendingAction::None
        ));
    }

    #[test]
    fn package_manager_preference_syncs_and_hydrates_across_sessions() {
        let dir = std::env::temp_dir().join(format!(
            "waish-repl-pm-pref-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let paths = Paths::from_dir(dir.clone());

        let mut settings = Settings::default();
        settings.tools.aur_helper = "yay".to_string();
        let mut session = ReplSession::new(settings.clone(), paths.clone()).unwrap();
        assert_eq!(session.settings.tools.preferred_package_manager(), "pacman");

        sync_package_manager_preference("pake yay aja", &mut session);
        assert_eq!(session.settings.tools.package_manager, "yay");
        assert_eq!(session.settings.tools.preferred_package_manager(), "yay");

        sync_package_manager_preference("aku biasa pake sudo pacman aja", &mut session);
        assert_eq!(session.settings.tools.package_manager, "pacman");
        assert_eq!(session.settings.tools.preferred_package_manager(), "pacman");

        let mut alt_settings = settings;
        alt_settings.tools.package_manager = "yay".to_string();
        let reloaded = ReplSession::new(alt_settings, paths).unwrap();
        assert_eq!(reloaded.settings.tools.package_manager, "pacman");
        assert_eq!(
            reloaded.settings.tools.preferred_package_manager(),
            "pacman"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn conversation_buffer_prunes_and_caps_memory_growth() {
        let mut session = test_session();
        let giant = "x".repeat(MAX_STORED_ASSISTANT_CHARS + 500);
        for i in 0..(MAX_SESSION_CONVERSATION_TURNS + 15) {
            session.push_conversation_turn(format!("u{i}"), giant.clone());
        }
        assert_eq!(session.conversation.len(), MAX_SESSION_CONVERSATION_TURNS);
        assert_eq!(session.conversation[0].user, "u15");
        assert!(session.conversation[0].assistant.len() <= MAX_STORED_ASSISTANT_CHARS + 3);
    }

    #[test]
    fn immediate_update_trigger_catches_greenlight_and_respects_pacman_preference() {
        let empty_history: Vec<ConversationTurn> = Vec::new();
        let update_history = vec![ConversationTurn {
            user: "updatein arch aku dong".to_string(),
            assistant: "Boleh, mau pake pacman aja steve?".to_string(),
        }];

        assert!(should_trigger_system_update_fallback(
            "yaudah sekarang updatein dongs",
            &empty_history,
            "Baiklah, steve. Aku akan coba update sistemmu..."
        ));
        assert!(should_trigger_system_update_fallback(
            "updatein arch aku dong",
            &empty_history,
            ""
        ));

        assert!(should_trigger_system_update_fallback(
            "gass",
            &update_history,
            ""
        ));
        assert!(should_trigger_system_update_fallback(
            "jalanin",
            &update_history,
            ""
        ));
        assert!(should_trigger_system_update_fallback(
            "sekarang coba",
            &update_history,
            ""
        ));
        assert!(should_trigger_system_update_fallback(
            "coba pake sudo pacman",
            &update_history,
            ""
        ));

        assert!(!should_trigger_system_update_fallback(
            "gimana cara update arch?",
            &empty_history,
            ""
        ));
        assert!(!should_trigger_system_update_fallback(
            "aku biasa pake pacman",
            &empty_history,
            ""
        ));
        assert!(!should_trigger_system_update_fallback(
            "gass",
            &empty_history,
            ""
        ));
        assert!(!should_trigger_system_update_fallback(
            "jangan update dulu",
            &update_history,
            ""
        ));
        assert!(!should_trigger_system_update_fallback(
            "halo alice",
            &empty_history,
            ""
        ));

        let mut settings = Settings::default();
        settings.tools.aur_helper = "yay".to_string();
        settings.tools.package_manager = "pacman".to_string();
        let call = synthesize_system_update_tool_call(&settings);
        assert_eq!(call.name, RUN_SHELL_COMMAND_NAME);
        let parsed: serde_json::Value = serde_json::from_str(&call.arguments).unwrap();
        assert_eq!(parsed["command"], "sudo pacman -Syu");
        assert_eq!(parsed["requires_root"], true);
    }

    #[tokio::test]
    async fn inline_course_correction_injects_steering_prompt_into_followup_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let steering =
            build_inline_correction_steering_prompt("yay -Syu", "coba pake sudo pacman aja");
        assert!(steering.contains("[User Inline Correction]:"));
        assert!(steering.contains("yay -Syu"));
        assert!(steering.contains("coba pake sudo pacman aja"));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured_body = Arc::new(Mutex::new(String::new()));
        let captured_srv = Arc::clone(&captured_body);

        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if let Ok(mut g) = captured_srv.lock() {
                    *g = String::from_utf8_lossy(&buf[..n]).into_owned();
                }
                let header = concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Type: text/event-stream\r\n",
                    "Transfer-Encoding: chunked\r\n\r\n"
                );
                let _ = sock.write_all(header.as_bytes()).await;
                let sse = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"Oke steve, langsung ganti pake pacman yaa! ( •̀ ω •́ )✧\"}}]}\n\n",
                    "data: [DONE]\n\n"
                );
                let chunk = format!("{:X}\r\n{}\r\n0\r\n\r\n", sse.len(), sse);
                let _ = sock.write_all(chunk.as_bytes()).await;
            }
        });

        let mut settings = Settings::default();
        settings.api.provider = "openai".to_string();
        settings.api.api_key = "sk-test-steering".to_string();
        settings.api.base_url = format!("http://{addr}/v1");
        settings.api.model = "gpt-4o".to_string();

        let client = LlmClient::new(&settings).unwrap();
        let ctx = crate::engine::context::AssembledContext {
            system_prompt: "test".to_string(),
            messages: vec![crate::engine::context::ChatMessage::user("update arch")],
        };
        let rounds = vec![ToolRound {
            assistant_text: "Oke update dulu yaa".to_string(),
            tool_calls: vec![ToolCall {
                id: "call_rej_1".to_string(),
                name: RUN_SHELL_COMMAND_NAME.to_string(),
                arguments: r#"{"command":"yay -Syu","intent":"Update","requires_root":true}"#
                    .to_string(),
            }],
            tool_results: vec![crate::tools::runner::ToolExecutionResult {
                tool_call_id: "call_rej_1".to_string(),
                tool_name: RUN_SHELL_COMMAND_NAME.to_string(),
                output_for_llm: "status: user_inline_correction".to_string(),
                succeeded: false,
                cancelled_cleanly: false,
                course_correction: Some(crate::tools::runner::UserCourseCorrection {
                    rejected_command: "yay -Syu".to_string(),
                    feedback: "coba pake sudo pacman aja".to_string(),
                }),
            }],
            steering_message: Some(steering.clone()),
        }];

        let resp = client
            .stream_react_followup_with_face(&settings, &ctx, &rounds, true, "( ˘ω˘ )")
            .await
            .expect("followup after inline correction should succeed");
        assert!(resp.text.contains("langsung ganti pake pacman"));

        let req_raw = captured_body.lock().unwrap().clone();
        assert!(
            req_raw.contains("[User Inline Correction]"),
            "steering message must be sent in HTTP request payload: {req_raw}"
        );
        assert!(req_raw.contains("coba pake sudo pacman aja"));
    }

    #[tokio::test]
    async fn react_followup_resumes_cleanly_after_429_and_instream_rate_limit_without_silent_drop() {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = Arc::clone(&hits);

        tokio::spawn(async move {
            for _ in 0..4 {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf = [0u8; 16384];
                    let _ = sock.read(&mut buf).await;
                    let step = hits_srv.fetch_add(1, Ordering::SeqCst);
                    match step {
                        0 => {
                            let header = concat!(
                                "HTTP/1.1 200 OK\r\n",
                                "Content-Type: text/event-stream\r\n",
                                "Connection: close\r\n",
                                "Transfer-Encoding: chunked\r\n\r\n"
                            );
                            let _ = sock.write_all(header.as_bytes()).await;
                            let sse = concat!(
                                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_cd_1\",\"type\":\"function\",\"function\":{\"name\":\"run_shell_command\",\"arguments\":\"{\\\"command\\\":\\\"echo waish_cooldown_ok\\\",\\\"intent\\\":\\\"Uji cooldown\\\",\\\"requires_root\\\":false}\"}}]}}]}\n\n",
                                "data: [DONE]\n\n"
                            );
                            let chunk = format!("{:X}\r\n{}\r\n0\r\n\r\n", sse.len(), sse);
                            let _ = sock.write_all(chunk.as_bytes()).await;
                        }
                        1 => {
                            let body = r#"{"error":{"message":"Rate limit exceeded. Retry after 1s."}}"#;
                            let resp = format!(
                                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 1\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = sock.write_all(resp.as_bytes()).await;
                        }
                        2 => {
                            let header = concat!(
                                "HTTP/1.1 200 OK\r\n",
                                "Content-Type: text/event-stream\r\n",
                                "Connection: close\r\n",
                                "Transfer-Encoding: chunked\r\n\r\n"
                            );
                            let _ = sock.write_all(header.as_bytes()).await;
                            let sse = "data: {\"error\":{\"code\":429,\"message\":\"Rate limit exceeded. Retry after 1s.\"}}\n\n";
                            let chunk = format!("{:X}\r\n{}\r\n0\r\n\r\n", sse.len(), sse);
                            let _ = sock.write_all(chunk.as_bytes()).await;
                        }
                        _ => {
                            let header = concat!(
                                "HTTP/1.1 200 OK\r\n",
                                "Content-Type: text/event-stream\r\n",
                                "Connection: close\r\n",
                                "Transfer-Encoding: chunked\r\n\r\n"
                            );
                            let _ = sock.write_all(header.as_bytes()).await;
                            let sse = concat!(
                                "data: {\"choices\":[{\"delta\":{\"content\":\"Selesai steve! Outputnya waish_cooldown_ok ( •̀ ω •́ )✧\"}}]}\n\n",
                                "data: [DONE]\n\n"
                            );
                            let chunk = format!("{:X}\r\n{}\r\n0\r\n\r\n", sse.len(), sse);
                            let _ = sock.write_all(chunk.as_bytes()).await;
                        }
                    }
                }
            }
        });

        let test_dir = std::env::temp_dir().join(format!(
            "waish-repl-cooldown-resume-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&test_dir);
        let paths = Paths::from_dir(test_dir.clone());

        let mut settings = Settings::default();
        settings.general.bot_name = "alice".to_string();
        settings.general.user_real_name = "steve".to_string();
        settings.api.provider = "openai".to_string();
        settings.api.api_key = "test-cooldown-resume".to_string();
        settings.api.base_url = format!("http://{addr}/v1");
        settings.api.model = "qwen-plus".to_string();

        let mut session = ReplSession::new(settings, paths).unwrap();
        handle_streaming_chat("cek output echo dong", &mut session).await;

        assert_eq!(
            hits.load(Ordering::SeqCst),
            4,
            "ReAct loop must complete initial call + HTTP 429 retry + in-stream 429 retry"
        );
        assert_eq!(
            session.conversation.len(),
            1,
            "final synthesized reply must be recorded without silent drop"
        );
        assert!(
            session.conversation[0]
                .assistant
                .contains("waish_cooldown_ok"),
            "unexpected assistant reply: {}",
            session.conversation[0].assistant
        );

        let _ = std::fs::remove_dir_all(&test_dir);
    }

    #[tokio::test]
    async fn completion_guard_rejects_premature_beres_and_enforces_native_spreadsheet() {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let test_dir = std::env::temp_dir().join(format!(
            "waish-repl-completion-guard-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&test_dir).unwrap();
        let target_xlsx = test_dir.join("rekap.xlsx");
        let target_xlsx_str = target_xlsx.to_string_lossy().into_owned();
        let target_xlsx_srv = target_xlsx_str.clone();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = Arc::clone(&hits);
        let saw_guard_warning = Arc::new(AtomicBool::new(false));
        let saw_guard_srv = Arc::clone(&saw_guard_warning);

        tokio::spawn(async move {
            for _ in 0..4 {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf = [0u8; 32768];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req_text = String::from_utf8_lossy(&buf[..n]);
                    if req_text.contains("belum terverifikasi di sistem") {
                        saw_guard_srv.store(true, Ordering::SeqCst);
                    }
                    let step = hits_srv.fetch_add(1, Ordering::SeqCst);
                    let header = concat!(
                        "HTTP/1.1 200 OK\r\n",
                        "Content-Type: text/event-stream\r\n",
                        "Connection: close\r\n",
                        "Transfer-Encoding: chunked\r\n\r\n"
                    );
                    let _ = sock.write_all(header.as_bytes()).await;

                    let sse = match step {
                        0 => concat!(
                            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_py_1\",\"type\":\"function\",\"function\":{\"name\":\"run_shell_command\",\"arguments\":\"{\\\"command\\\":\\\"python3 /tmp/gen.py > rekap.xlsx\\\",\\\"intent\\\":\\\"Buat excel\\\",\\\"requires_root\\\":false}\"}}]}}]}\n\n",
                            "data: [DONE]\n\n"
                        )
                        .to_string(),
                        1 => concat!(
                            "data: {\"choices\":[{\"delta\":{\"content\":\"Semua selesai dibuat yaa!\"}}]}\n\n",
                            "data: [DONE]\n\n"
                        )
                        .to_string(),
                        2 => {
                            let args_json = serde_json::json!({
                                "action": "create",
                                "path": target_xlsx_srv,
                                "headers": ["Bulan", "Pendapatan"],
                                "rows": [["November", 5000000]]
                            })
                            .to_string();
                            let delta = serde_json::json!({
                                "choices": [{
                                    "delta": {
                                        "tool_calls": [{
                                            "index": 0,
                                            "id": "call_xlsx_2",
                                            "type": "function",
                                            "function": {
                                                "name": "manage_spreadsheet",
                                                "arguments": args_json
                                            }
                                        }]
                                    }
                                }]
                            })
                            .to_string();
                            format!("data: {delta}\n\ndata: [DONE]\n\n")
                        }
                        _ => concat!(
                            "data: {\"choices\":[{\"delta\":{\"content\":\"Rekap spreadsheet sudah siap dan terverifikasi di disk ( •̀ ω •́ )✧\"}}]}\n\n",
                            "data: [DONE]\n\n"
                        )
                        .to_string(),
                    };
                    let chunk = format!("{:X}\r\n{}\r\n0\r\n\r\n", sse.len(), sse);
                    let _ = sock.write_all(chunk.as_bytes()).await;
                }
            }
        });

        let paths = Paths::from_dir(test_dir.join("cfg"));
        let mut settings = Settings::default();
        settings.general.bot_name = "alice".to_string();
        settings.general.user_real_name = "steve".to_string();
        settings.api.provider = "openai".to_string();
        settings.api.api_key = "sk-test-guard".to_string();
        settings.api.base_url = format!("http://{addr}/v1");
        settings.api.model = "qwen-plus".to_string();

        let mut session = ReplSession::new(settings, paths).unwrap();
        let prompt = format!("buatkan file {target_xlsx_str}");
        handle_streaming_chat(&prompt, &mut session).await;

        assert_eq!(hits.load(Ordering::SeqCst), 4);
        assert!(
            saw_guard_warning.load(Ordering::SeqCst),
            "Completion Guard warning must be injected when LLM claims premature completion"
        );
        assert!(
            target_xlsx.exists(),
            "native manage_spreadsheet must physically create the target .xlsx file"
        );
        assert!(session.conversation[0]
            .assistant
            .contains("terverifikasi di disk"));

        let _ = std::fs::remove_dir_all(&test_dir);
    }

    #[tokio::test]
    async fn fast_path_executes_locally_with_zero_api_calls_and_syncs_scratchpad() {
        let test_dir =
            std::env::temp_dir().join(format!("waish-fastpath-repl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&test_dir);
        let paths = Paths::from_dir(test_dir.clone());

        let mut settings = Settings::default();
        settings.api.api_key.clear();
        let mut session = ReplSession::new(settings, paths.clone()).unwrap();

        let action = process_line("cek ram", &mut session).await.unwrap();
        assert_eq!(action, LoopAction::Continue);
        assert_eq!(session.conversation.len(), 1);
        assert!(session.conversation[0].assistant.contains("RAM"));

        let pad = Scratchpad::load(&paths.scratchpad).unwrap();
        assert!(
            pad.notes
                .iter()
                .any(|n| n.tag == crate::engine::scratchpad::FAST_PATH_LOG_TAG),
            "scratchpad must record Fast-Path action summary: {:?}",
            pad.notes
        );

        assert!(looks_like_empty_action_promise(
            "bukain onlyoffice dong",
            "Oke steve, bentar aku bukain OnlyOffice dulu yaa!"
        ));
        assert!(!looks_like_empty_action_promise(
            "apa itu onlyoffice?",
            "OnlyOffice adalah aplikasi perkantoran."
        ));

        let _ = std::fs::remove_dir_all(&test_dir);
    }

    #[tokio::test]
    async fn empty_promise_stall_guard_nudges_at_most_once_and_surfaces_status() {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let test_dir =
            std::env::temp_dir().join(format!("waish-empty-promise-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&test_dir).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = Arc::clone(&hits);

        tokio::spawn(async move {
            for _ in 0..2 {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf = [0u8; 16384];
                    let _ = sock.read(&mut buf).await;
                    hits_srv.fetch_add(1, Ordering::SeqCst);
                    let header = concat!(
                        "HTTP/1.1 200 OK\r\n",
                        "Content-Type: text/event-stream\r\n",
                        "Connection: close\r\n",
                        "Transfer-Encoding: chunked\r\n\r\n"
                    );
                    let _ = sock.write_all(header.as_bytes()).await;
                    let sse = concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"Oke, tunggu bentar ya aku bukain OnlyOffice sekarang!\"}}]}\n\n",
                        "data: [DONE]\n\n"
                    );
                    let chunk = format!("{:X}\r\n{}\r\n0\r\n\r\n", sse.len(), sse);
                    let _ = sock.write_all(chunk.as_bytes()).await;
                }
            }
        });

        let paths = Paths::from_dir(test_dir.join("cfg"));
        let mut settings = Settings::default();
        settings.api.provider = "openai".to_string();
        settings.api.api_key = "test-key-local".to_string();
        settings.api.base_url = format!("http://{addr}/v1");
        settings.api.model = "qwen-plus".to_string();

        let mut session = ReplSession::new(settings, paths).unwrap();
        handle_streaming_chat("bukain onlyoffice dong", &mut session).await;

        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(session.conversation.len(), 1);
        assert!(
            session.conversation[0]
                .assistant
                .contains("belum mengirimkan panggilan tool eksekusi"),
            "expected explicit fallback notice, got: {}",
            session.conversation[0].assistant
        );

        let _ = std::fs::remove_dir_all(&test_dir);
    }
}
