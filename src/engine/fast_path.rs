//! Tier-0 Zero-Token Local Fast-Path Intent Router.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::sync::{Arc, Mutex, OnceLock};

use regex::Regex;

use crate::config::settings::Settings;
use crate::engine::kaomoji::KaomojiEngine;
use crate::engine::scratchpad::Scratchpad;
use crate::engine::telemetry::HardwareSnapshot;
use crate::tools::desktop::{self, ManageWorkspaceArgs};
use crate::tools::media::{self, ControlMediaArgs};
use crate::tools::workspace_tracker::SessionWorkspaceState;

/// Fast local telemetry metric category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastTelemetryTopic {
    Ram,
    Disk,
    Thermal,
    Battery,
}

/// Deterministic Tier-0 intent executed locally without LLM invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FastPathIntent {
    MediaNext,
    MediaPrevious,
    MediaPause,
    MediaPlay,
    MediaNowPlaying,
    WorkspaceCloseBatch(Vec<i32>),
    WorkspaceSwitch(i32),
    Telemetry(FastTelemetryTopic),
}

/// Fast-Path execution result for REPL output and scratchpad synchronization.
#[derive(Debug, Clone)]
pub struct FastPathOutcome {
    pub reply_for_user: String,
    pub scratchpad_summary: String,
}

const COMPOUND_CONNECTORS: &[&str] = &[
    " terus ",
    " lalu ",
    " habis itu ",
    " kemudian ",
    " sambil ",
    " sekalian ",
    " karena ",
    " kenapa ",
    " mengapa ",
    " ceritain ",
    " ceritakan ",
    " jelasin ",
    " jelaskan ",
    " dan ",
    " and ",
    " then ",
];

pub fn classify_fast_path_intent(raw_input: &str) -> Option<FastPathIntent> {
    let trimmed = raw_input.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.contains(';')
        || trimmed.contains("&&")
        || trimmed.contains('|')
    {
        return None;
    }

    let collapsed = trimmed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let clean = collapsed
        .trim_end_matches(['.', '!'])
        .trim()
        .to_ascii_lowercase();
    if clean.is_empty() || clean.len() > 72 {
        return None;
    }

    let padded = format!(" {clean} ");
    if COMPOUND_CONNECTORS.iter().any(|c| padded.contains(c)) {
        return None;
    }

    static NEXT_RE: OnceLock<Regex> = OnceLock::new();
    static PREV_RE: OnceLock<Regex> = OnceLock::new();
    static PAUSE_RE: OnceLock<Regex> = OnceLock::new();
    static PLAY_RE: OnceLock<Regex> = OnceLock::new();
    static NOW_PLAYING_RE: OnceLock<Regex> = OnceLock::new();
    static WS_CLOSE_RE: OnceLock<Regex> = OnceLock::new();
    static WS_SWITCH_RE: OnceLock<Regex> = OnceLock::new();
    static TELEMETRY_RE: OnceLock<Regex> = OnceLock::new();

    let next_re = NEXT_RE.get_or_init(|| {
        Regex::new(
            r"^(?:(?:next|skip)(?:\s+(?:lagu|musik|track)(?:nya)?)?|(?:ganti|lanjut)\s+(?:lagu|musik|track)(?:nya)?)(?:\s+(?:dong|ya|yaa|plis))?$",
        )
        .expect("valid next regex")
    });
    if next_re.is_match(&clean) {
        return Some(FastPathIntent::MediaNext);
    }

    let prev_re = PREV_RE.get_or_init(|| {
        Regex::new(
            r"^(?:prev|previous|(?:lagu|track)\s+sebelumnya|balik\s+lagu)(?:\s+(?:dong|ya|yaa|plis))?$",
        )
        .expect("valid prev regex")
    });
    if prev_re.is_match(&clean) {
        return Some(FastPathIntent::MediaPrevious);
    }

    let pause_re = PAUSE_RE.get_or_init(|| {
        Regex::new(
            r"^(?:(?:pause|jeda)(?:\s+(?:lagu|musik|track)(?:nya)?)?|stop\s+(?:lagu|musik|track)(?:nya)?)(?:\s+(?:dong|ya|yaa|plis))?$",
        )
        .expect("valid pause regex")
    });
    if pause_re.is_match(&clean) {
        return Some(FastPathIntent::MediaPause);
    }

    let play_re = PLAY_RE.get_or_init(|| {
        Regex::new(
            r"^(?:(?:play|resume)(?:\s+(?:lagu|musik|track)(?:nya)?)?|lanjut\s+(?:putar|puter)(?:\s+(?:lagu|musik)(?:nya)?)?|(?:putar|puter)\s+lagi)(?:\s+(?:dong|ya|yaa|plis))?$",
        )
        .expect("valid play regex")
    });
    if play_re.is_match(&clean) {
        return Some(FastPathIntent::MediaPlay);
    }

    let np_re = NOW_PLAYING_RE.get_or_init(|| {
        Regex::new(
            r"^(?:lagu\s+apa(?:\s+ini)?|now\s+playing|lagi\s+(?:muter|mutar|putar|puter)\s+(?:lagu\s+)?apa)\??$",
        )
        .expect("valid now playing regex")
    });
    if np_re.is_match(&clean) {
        return Some(FastPathIntent::MediaNowPlaying);
    }

    let ws_close_re = WS_CLOSE_RE.get_or_init(|| {
        Regex::new(
            r"^(?:tutup|close|bersihin|bersihkan)\s+(?:semua\s+)?(?:(?:tab|jendela|window|app)\s+(?:di\s+)?)?(?:ws|workspace)\s+([\d,\s]+?)(?:\s+(?:dong|ya|yaa|plis))?$",
        )
        .expect("valid ws close regex")
    });
    if let Some(caps) = ws_close_re.captures(&clean) {
        let ids: Vec<i32> = caps[1]
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<i32>().ok())
            .filter(|&n| n > 0)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if !ids.is_empty() {
            return Some(FastPathIntent::WorkspaceCloseBatch(ids));
        }
    }

    let ws_switch_re = WS_SWITCH_RE.get_or_init(|| {
        Regex::new(
            r"^(?:pindah|geser|switch|goto)\s+(?:ke\s+)?(?:ws|workspace)\s+(\d+)(?:\s+(?:dong|ya|yaa|plis))?$",
        )
        .expect("valid ws switch regex")
    });
    if let Some(ws_id) = ws_switch_re
        .captures(&clean)
        .and_then(|caps| caps[1].parse::<i32>().ok())
        .filter(|&id| id > 0)
    {
        return Some(FastPathIntent::WorkspaceSwitch(ws_id));
    }

    let telem_re = TELEMETRY_RE.get_or_init(|| {
        Regex::new(r"^(?:(?:cek|status)\s+(ram|disk|suhu|cpu|baterai)|suhu\s+cpu)$")
            .expect("valid telemetry regex")
    });
    if let Some(caps) = telem_re.captures(&clean) {
        let topic = match caps.get(1).map(|m| m.as_str()).unwrap_or("suhu") {
            "ram" => FastTelemetryTopic::Ram,
            "disk" => FastTelemetryTopic::Disk,
            "baterai" => FastTelemetryTopic::Battery,
            _ => FastTelemetryTopic::Thermal,
        };
        return Some(FastPathIntent::Telemetry(topic));
    }

    None
}

pub async fn execute_fast_path_intent(
    intent: &FastPathIntent,
    settings: &Settings,
    scratchpad_path: &std::path::Path,
    workspace_state: &SessionWorkspaceState,
    kaomoji_engine: &Arc<Mutex<KaomojiEngine>>,
) -> FastPathOutcome {
    let bot_name = &settings.general.bot_name;
    let outcome = match intent {
        FastPathIntent::MediaNext
        | FastPathIntent::MediaPrevious
        | FastPathIntent::MediaPause
        | FastPathIntent::MediaPlay
        | FastPathIntent::MediaNowPlaying => {
            let action = match intent {
                FastPathIntent::MediaNext => "next",
                FastPathIntent::MediaPrevious => "previous",
                FastPathIntent::MediaPause => "pause",
                FastPathIntent::MediaPlay => "play",
                FastPathIntent::MediaNowPlaying => "get_info",
                _ => unreachable!(),
            };
            let args = ControlMediaArgs {
                action: action.to_string(),
                player: None,
            };
            match media::execute_control_media(&args, kaomoji_engine, bot_name).await {
                Ok(raw) if raw == media::NO_ACTIVE_PLAYER_MSG => FastPathOutcome {
                    reply_for_user: "Belum ada pemutar musik yang aktif nih saat ini.".to_string(),
                    scratchpad_summary: format!("Fast-Path media `{action}`: tidak ada player aktif"),
                },
                Ok(raw) => {
                    let track = raw
                        .lines()
                        .find_map(|l| l.strip_prefix("track:"))
                        .map(str::trim)
                        .unwrap_or("(tidak diketahui)");
                    let reply = match intent {
                        FastPathIntent::MediaNext => {
                            format!("Oke, lanjut ke lagu berikutnya: {track}!")
                        }
                        FastPathIntent::MediaPrevious => {
                            format!("Sip, balik ke lagu sebelumnya: {track}!")
                        }
                        FastPathIntent::MediaPause => format!("Musik dijeda dulu yaa ({track})."),
                        FastPathIntent::MediaPlay => {
                            format!("Musik dilanjut putar lagi: {track}!")
                        }
                        FastPathIntent::MediaNowPlaying => {
                            format!("Sekarang lagi muter: {track}.")
                        }
                        _ => raw.clone(),
                    };
                    FastPathOutcome {
                        reply_for_user: reply,
                        scratchpad_summary: format!("Fast-Path media `{action}` -> {track}"),
                    }
                }
                Err(e) => FastPathOutcome {
                    reply_for_user: format!("Gagal mengontrol media lokal: {e}"),
                    scratchpad_summary: format!("Fast-Path media `{action}` gagal: {e}"),
                },
            }
        }
        FastPathIntent::WorkspaceCloseBatch(ids) => {
            let args = ManageWorkspaceArgs {
                action: "close_batch".to_string(),
                target: String::new(),
                command: String::new(),
                workspaces: ids.clone(),
            };
            let ws_label = ids
                .iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            match desktop::execute_manage_workspace_with_config(
                &args,
                workspace_state,
                Some(&settings.desktop),
            ) {
                Ok(summary) => {
                    let first_line = summary.lines().next().unwrap_or(&summary);
                    FastPathOutcome {
                        reply_for_user: first_line.to_string(),
                        scratchpad_summary: format!("Fast-Path close_batch workspace [{ws_label}]"),
                    }
                }
                Err(e) => FastPathOutcome {
                    reply_for_user: format!("Gagal menutup workspace [{ws_label}]: {e}"),
                    scratchpad_summary: format!("Fast-Path close_batch [{ws_label}] gagal: {e}"),
                },
            }
        }
        FastPathIntent::WorkspaceSwitch(ws_id) => {
            let args = ManageWorkspaceArgs {
                action: "switch".to_string(),
                target: ws_id.to_string(),
                command: String::new(),
                workspaces: Vec::new(),
            };
            match desktop::execute_manage_workspace_with_config(
                &args,
                workspace_state,
                Some(&settings.desktop),
            ) {
                Ok(_) => FastPathOutcome {
                    reply_for_user: format!("Sip, langsung pindah ke workspace {ws_id}!"),
                    scratchpad_summary: format!("Fast-Path pindah ke workspace {ws_id}"),
                },
                Err(e) => FastPathOutcome {
                    reply_for_user: format!("Gagal pindah ke workspace {ws_id}: {e}"),
                    scratchpad_summary: format!("Fast-Path switch ws {ws_id} gagal: {e}"),
                },
            }
        }
        FastPathIntent::Telemetry(topic) => {
            let snap = HardwareSnapshot::collect();
            let reply = match topic {
                FastTelemetryTopic::Ram => match snap.ram_used_pct {
                    Some(pct) => format!("Pemakaian RAM saat ini berada di {pct:.1}%."),
                    None => "Data pemakaian RAM belum terbaca dari /proc/meminfo.".to_string(),
                },
                FastTelemetryTopic::Disk => format_root_disk_usage(),
                FastTelemetryTopic::Thermal => match snap.cpu_temp_c {
                    Some(t) => format!("Suhu puncak sensor CPU/termal saat ini {t:.1} C."),
                    None => "Sensor suhu termal tidak terdeteksi pada perangkat ini.".to_string(),
                },
                FastTelemetryTopic::Battery => match snap.battery {
                    Some(b) => format!("Baterai saat ini {}% (status: {}).", b.capacity, b.status),
                    None => "Perangkat ini menggunakan daya langsung (baterai tidak terdeteksi)."
                        .to_string(),
                },
            };
            FastPathOutcome {
                scratchpad_summary: format!("Fast-Path telemetri: {reply}"),
                reply_for_user: reply,
            }
        }
    };

    if let Ok(mut pad) = Scratchpad::load(scratchpad_path) {
        pad.record_fast_path_action(&outcome.scratchpad_summary);
        let _ = pad.save(scratchpad_path);
    }

    outcome
}

fn format_root_disk_usage() -> String {
    let Ok(c_root) = CString::new("/") else {
        return "Gagal memeriksa ruang penyimpanan `/`.".to_string();
    };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_root.as_ptr(), &mut stat) };
    if rc != 0 || stat.f_blocks == 0 {
        return "Gagal membaca statvfs pada partisi `/`.".to_string();
    }
    let block_size = stat.f_frsize as f64;
    let total_gib = (stat.f_blocks as f64 * block_size) / (1024.0 * 1024.0 * 1024.0);
    let avail_gib = (stat.f_bavail as f64 * block_size) / (1024.0 * 1024.0 * 1024.0);
    let used_gib = (total_gib - avail_gib).max(0.0);
    let used_pct = if total_gib > 0.0 {
        (used_gib / total_gib) * 100.0
    } else {
        0.0
    };
    format!(
        "Penyimpanan `/` terpakai {used_gib:.1} GiB dari {total_gib:.1} GiB ({used_pct:.1}%, sisa {avail_gib:.1} GiB)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_single_deterministic_commands_and_bypasses_compound_prompts() {
        assert_eq!(classify_fast_path_intent("next"), Some(FastPathIntent::MediaNext));
        assert_eq!(
            classify_fast_path_intent("skip lagu dong!"),
            Some(FastPathIntent::MediaNext)
        );
        assert_eq!(
            classify_fast_path_intent("ganti lagunya"),
            Some(FastPathIntent::MediaNext)
        );
        assert_eq!(classify_fast_path_intent("pause"), Some(FastPathIntent::MediaPause));
        assert_eq!(
            classify_fast_path_intent("jeda musiknya"),
            Some(FastPathIntent::MediaPause)
        );
        assert_eq!(classify_fast_path_intent("resume"), Some(FastPathIntent::MediaPlay));
        assert_eq!(
            classify_fast_path_intent("lanjut puter"),
            Some(FastPathIntent::MediaPlay)
        );
        assert_eq!(
            classify_fast_path_intent("lagu apa?"),
            Some(FastPathIntent::MediaNowPlaying)
        );

        assert_eq!(
            classify_fast_path_intent("tutup ws 3"),
            Some(FastPathIntent::WorkspaceCloseBatch(vec![3]))
        );
        assert_eq!(
            classify_fast_path_intent("tutup semua ws 2, 5, 6 dong"),
            Some(FastPathIntent::WorkspaceCloseBatch(vec![2, 5, 6]))
        );
        assert_eq!(
            classify_fast_path_intent("pindah ke ws 4"),
            Some(FastPathIntent::WorkspaceSwitch(4))
        );

        assert_eq!(
            classify_fast_path_intent("cek ram"),
            Some(FastPathIntent::Telemetry(FastTelemetryTopic::Ram))
        );
        assert_eq!(
            classify_fast_path_intent("suhu cpu"),
            Some(FastPathIntent::Telemetry(FastTelemetryTopic::Thermal))
        );

        assert_eq!(
            classify_fast_path_intent("next lagunya terus ceritain sejarah band ini"),
            None
        );
        assert_eq!(
            classify_fast_path_intent("tutup ws 2 lalu buatkan tabel excel"),
            None
        );
        assert_eq!(classify_fast_path_intent("lanjut"), None);
        assert_eq!(classify_fast_path_intent("cek sistem dong"), None);
        assert_eq!(classify_fast_path_intent("/help"), None);
    }
}
