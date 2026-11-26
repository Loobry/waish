//! Native D-Bus MPRIS media orchestration (`zbus`), PipeWire audio pre-flight checks, and YouTube direct playback.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use zbus::{Connection, OwnedObjectPath, OwnedVariant};

use super::desktop;
use super::web_search;
use super::workspace_tracker::{self, SessionWorkspaceState};
use crate::cli::ui::Kaomoji;
use crate::config::settings::DesktopConfig;
use crate::engine::client::sanitize_terminal_text;
use crate::engine::kaomoji::{EmotionSpectrum, KaomojiEngine};

pub const LOGIN_TIMEOUT_SECS: u64 = 15 * 60;
pub const MPRIS_COLD_BOOT_RETRIES: usize = 8;
pub const MPRIS_DEFAULT_MAX_RETRIES: usize = 8;
pub const MPRIS_DEFAULT_DELAY_MS: u64 = 400;
const YOUTUBE_RESOLVE_TIMEOUT_SECS: u64 = 8;
pub const LOW_VOLUME_THRESHOLD: f32 = 0.05;
pub const NO_ACTIVE_PLAYER_MSG: &str = "Tidak ada pemutar musik MPRIS yang aktif saat ini.";

const MPRIS_BUS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
const DBUS_PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";

/// Volume and mute status of the default PipeWire audio sink (`wpctl`).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioStatus {
    pub volume: f32,
    pub muted: bool,
}

impl AudioStatus {
    pub fn warning_message(&self) -> Option<&'static str> {
        if self.muted {
            Some("( •ˋ _ ˊ•) Lagunya udah kupasang, tapi speaker kamu lagi di-mute tuh! Nyalain dulu gih.")
        } else if self.volume < LOW_VOLUME_THRESHOLD {
            Some("( •ˋ _ ˊ•) Lagunya udah kupasang, tapi volume speaker kamu hampir 0% tuh! Gedein dulu gih.")
        } else {
            None
        }
    }
}

pub fn parse_wpctl_volume(output: &str) -> Option<AudioStatus> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }

    let muted = trimmed.to_ascii_uppercase().contains("[MUTED]");
    let after_colon = trimmed.split(':').nth(1).unwrap_or(trimmed).trim();
    let num_token = after_colon.split_whitespace().next()?;
    let volume = num_token.parse::<f32>().ok()?;

    Some(AudioStatus { volume, muted })
}

pub async fn check_audio_preflight() -> Option<AudioStatus> {
    let out = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_wpctl_volume(&stdout)
}

/// Deferred REPL action state (e.g. awaiting interactive Spotify login completion).
#[derive(Debug, Clone, Default)]
pub enum PendingAction {
    #[default]
    None,
    WaitingForLogin {
        service: String,
        workspace: i32,
        target_query: String,
        created_at: Instant,
    },
}

impl PendingAction {
    pub fn is_expired_at(&self, now: Instant) -> bool {
        match self {
            PendingAction::None => false,
            PendingAction::WaitingForLogin { created_at, .. } => {
                now.saturating_duration_since(*created_at) > Duration::from_secs(LOGIN_TIMEOUT_SECS)
            }
        }
    }

    pub fn is_login_confirmation_slang(input: &str) -> bool {
        let trimmed = input.trim();
        if trimmed.is_empty() || trimmed.starts_with('/') {
            return false;
        }

        let normalized = trimmed
            .trim_matches(|c: char| c.is_ascii_punctuation())
            .trim()
            .to_ascii_lowercase();

        matches!(
            normalized.as_str(),
            "udah"
                | "udh"
                | "dah"
                | "sudah"
                | "udah login"
                | "udh login"
                | "dah login"
                | "sudah login"
                | "beres"
                | "siap"
                | "done"
                | "selesai"
                | "udah beres"
                | "dah beres"
                | "oke udah"
                | "ok udah"
                | "udah kok"
                | "gas"
        )
    }
}

/// Playback status and track metadata read from `org.mpris.MediaPlayer2.Player`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaStatusSnapshot {
    pub status: String,
    pub is_playing: bool,
    pub track_info: Option<String>,
}

pub fn mpris_list_contains_spotify(names: &[String]) -> bool {
    names
        .iter()
        .any(|n| n.to_ascii_lowercase().contains("spotify"))
}

#[cfg(test)]
pub fn playerctl_list_contains_spotify(raw: &str) -> bool {
    let items: Vec<String> = raw
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    mpris_list_contains_spotify(&items)
}

pub async fn list_mpris_bus_names(conn: &Connection) -> Vec<String> {
    let Ok(reply) = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "ListNames",
            &(),
        )
        .await
    else {
        return Vec::new();
    };

    let Ok(all_names) = reply.body().deserialize::<Vec<String>>() else {
        return Vec::new();
    };

    all_names
        .into_iter()
        .filter(|n| n.starts_with(MPRIS_BUS_PREFIX))
        .collect()
}

pub async fn resolve_target_mpris_bus(
    conn: &Connection,
    player_hint: Option<&str>,
) -> Option<String> {
    let names = list_mpris_bus_names(conn).await;
    if names.is_empty() {
        return None;
    }

    if let Some(hint) = player_hint.map(str::trim).filter(|s| !s.is_empty()) {
        let h_low = hint.to_ascii_lowercase();
        let exact_bus = format!("{MPRIS_BUS_PREFIX}{h_low}");
        if let Some(found) = names.iter().find(|n| n.eq_ignore_ascii_case(&exact_bus)) {
            return Some(found.clone());
        }
        return names
            .into_iter()
            .find(|n| {
                n.strip_prefix(MPRIS_BUS_PREFIX)
                    .unwrap_or(n)
                    .to_ascii_lowercase()
                    .contains(&h_low)
            });
    }

    for candidate in &names {
        if read_mpris_properties_on_bus(conn, candidate)
            .await
            .is_some_and(|(snap, _)| snap.is_playing)
        {
            return Some(candidate.clone());
        }
    }

    if let Some(sp) = names
        .iter()
        .find(|n| n.eq_ignore_ascii_case("org.mpris.MediaPlayer2.spotify"))
    {
        return Some(sp.clone());
    }

    names.into_iter().next()
}

async fn read_mpris_properties_on_bus(
    conn: &Connection,
    bus_name: &str,
) -> Option<(MediaStatusSnapshot, Option<String>)> {
    let reply = conn
        .call_method(
            Some(bus_name),
            MPRIS_OBJECT_PATH,
            Some(DBUS_PROPS_IFACE),
            "GetAll",
            &(MPRIS_PLAYER_IFACE,),
        )
        .await
        .ok()?;

    let props: HashMap<String, OwnedVariant> = reply.body().deserialize().ok()?;
    let status = props
        .get("PlaybackStatus")
        .and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_else(|| "Stopped".to_string());
    let is_playing = status.eq_ignore_ascii_case("Playing");

    let mut title_opt: Option<String> = None;
    let mut artist_opt: Option<String> = None;
    let mut track_id_opt: Option<String> = None;

    if let Some(meta_map) = props
        .get("Metadata")
        .and_then(|v| HashMap::<String, OwnedVariant>::try_from(v.clone()).ok())
    {
        if let Some(t) = meta_map.get("xesam:title") {
            title_opt = String::try_from(t.clone())
                .ok()
                .map(|s| sanitize_terminal_text(s.trim()))
                .filter(|s| !s.is_empty());
        }
        if let Some(a) = meta_map.get("xesam:artist") {
            artist_opt = Vec::<String>::try_from(a.clone())
                .ok()
                .map(|v| v.join(", "))
                .or_else(|| String::try_from(a.clone()).ok())
                .map(|s| sanitize_terminal_text(s.trim()))
                .filter(|s| !s.is_empty());
        }
        if let Some(tid) = meta_map.get("mpris:trackid") {
            let raw_tid = OwnedObjectPath::try_from(tid.clone())
                .map(|p| p.to_string())
                .or_else(|_| String::try_from(tid.clone()))
                .ok();
            track_id_opt = raw_tid.and_then(|s| parse_mpris_trackid_to_spotify_uri(&s));
        }
    }

    let track_info = match (title_opt, artist_opt) {
        (Some(t), Some(a)) => Some(format!("{t} - {a}")),
        (Some(t), None) => Some(t),
        (None, Some(a)) => Some(a),
        (None, None) => None,
    };

    Some((
        MediaStatusSnapshot {
            status,
            is_playing,
            track_info,
        },
        track_id_opt,
    ))
}

async fn call_mpris_method_unit(conn: &Connection, bus_name: &str, method: &str) -> Result<(), String> {
    conn.call_method(
        Some(bus_name),
        MPRIS_OBJECT_PATH,
        Some(MPRIS_PLAYER_IFACE),
        method,
        &(),
    )
    .await
    .map(|_| ())
    .map_err(|e| format!("D-Bus `{method}` pada `{bus_name}` gagal: {e}"))
}

async fn call_mpris_open_uri(conn: &Connection, bus_name: &str, uri: &str) -> Result<(), String> {
    conn.call_method(
        Some(bus_name),
        MPRIS_OBJECT_PATH,
        Some(MPRIS_PLAYER_IFACE),
        "OpenUri",
        &(uri,),
    )
    .await
    .map(|_| ())
    .map_err(|e| format!("D-Bus `OpenUri` pada `{bus_name}` gagal: {e}"))
}

pub async fn is_spotify_on_session_dbus() -> bool {
    let Ok(conn) = Connection::session().await else {
        return false;
    };
    let names = list_mpris_bus_names(&conn).await;
    mpris_list_contains_spotify(&names)
}

pub async fn wait_for_mpris_ready(player_name: &str, max_retries: usize, delay_ms: u64) -> bool {
    let Ok(conn) = Connection::session().await else {
        return false;
    };
    for attempt in 0..max_retries {
        if let Some(bus) = resolve_target_mpris_bus(&conn, Some(player_name)).await {
            if read_mpris_properties_on_bus(&conn, &bus).await.is_some() {
                return true;
            }
        }
        if attempt + 1 < max_retries {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    false
}

pub async fn query_mpris_snapshot(player: Option<&str>) -> Option<MediaStatusSnapshot> {
    let conn = Connection::session().await.ok()?;
    let bus = resolve_target_mpris_bus(&conn, player).await?;
    read_mpris_properties_on_bus(&conn, &bus)
        .await
        .map(|(snap, _)| snap)
}

pub async fn sync_waybar_idle_or_media_with_engine(
    bot_name: &str,
    kaomoji_engine: &Arc<Mutex<KaomojiEngine>>,
    drowsy_idle: bool,
) -> bool {
    if let Some(snap) = query_mpris_snapshot(None).await.filter(|s| s.is_playing) {
        let face = kaomoji_engine
            .lock()
            .map(|mut eng| eng.pick(EmotionSpectrum::MelodicChill))
            .unwrap_or(Kaomoji::Music.as_str());
        desktop::sync_waybar_for_media_status_with_kaomoji(
            bot_name,
            true,
            Some(face),
            snap.track_info.as_deref(),
        );
        return true;
    }

    let idle_face = kaomoji_engine
        .lock()
        .map(|mut eng| {
            if drowsy_idle {
                eng.pick(EmotionSpectrum::DrowsyIdle)
            } else {
                eng.current_kaomoji()
            }
        })
        .unwrap_or(Kaomoji::Greet.as_str());
    desktop::sync_waybar_for_media_status_with_kaomoji(bot_name, false, Some(idle_face), None);
    false
}

pub fn is_spotify_session_ready(
    spotify_window_active: bool,
    on_session_dbus: bool,
    snap: Option<&MediaStatusSnapshot>,
) -> bool {
    spotify_window_active || on_session_dbus || snap.is_some()
}

pub fn normalize_spotify_open_uri(query: &str) -> (String, bool) {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return (String::new(), false);
    }

    if let Some(rest) = trimmed.strip_prefix("spotify:") {
        let is_playable = rest.starts_with("track:")
            || rest.starts_with("album:")
            || rest.starts_with("playlist:")
            || rest.starts_with("artist:")
            || rest.starts_with("episode:")
            || rest.starts_with("show:");
        return (trimmed.to_string(), is_playable);
    }

    if let Some((kind, id)) = extract_spotify_kind_and_id_from_text(trimmed) {
        return (format!("spotify:{kind}:{id}"), true);
    }

    (
        format!("spotify:search:{}", web_search::encode_uri_component(trimmed)),
        false,
    )
}

pub fn extract_spotify_kind_and_id_from_text(raw: &str) -> Option<(String, String)> {
    static OPEN_URL_RE: OnceLock<Regex> = OnceLock::new();
    let re = OPEN_URL_RE.get_or_init(|| {
        Regex::new(
            r"(?i)open\.spotify\.com/(?:intl-[a-z]{2}/)?(track|album|playlist|artist|episode|show)/([a-zA-Z0-9]{22})",
        )
        .expect("valid spotify url regex")
    });
    let caps = re.captures(raw)?;
    Some((
        caps.get(1)?.as_str().to_ascii_lowercase(),
        caps.get(2)?.as_str().to_string(),
    ))
}

pub fn parse_mpris_trackid_to_spotify_uri(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("spotify:") {
        return Some(trimmed.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("/com/spotify/") {
        let mut parts = rest.split('/');
        let kind = parts.next()?.trim();
        let id = parts.next()?.trim();
        if !kind.is_empty() && id.len() == 22 && id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Some(format!("spotify:{kind}:{id}"));
        }
    }
    None
}

async fn resolve_spotify_track_uri(query: &str) -> Option<String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(client) = Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(4))
        .user_agent(web_search::QUICK_SEARCH_USER_AGENT)
        .build()
    {
        let ddg_q = web_search::encode_uri_component(&format!("site:open.spotify.com/track {trimmed}"));
        let ddg_url = format!("https://html.duckduckgo.com/html/?q={ddg_q}");
        if let Ok(resp) = client.get(&ddg_url).send().await {
            if resp.status().is_success() {
                if let Ok(html) = resp.text().await {
                    let decoded = html
                        .replace("%2F", "/")
                        .replace("%2f", "/")
                        .replace("%3A", ":")
                        .replace("%3a", ":");
                    if let Some((_, id)) = extract_spotify_kind_and_id_from_text(&decoded)
                        .filter(|(kind, _)| *kind == "track")
                    {
                        return Some(format!("spotify:track:{id}"));
                    }
                }
            }
        }
    }

    resolve_spotify_track_uri_via_wikidata(trimmed).await
}

async fn resolve_spotify_track_uri_via_wikidata(query: &str) -> Option<String> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(3))
        .user_agent("waish/0.1 (Linux desktop companion)")
        .build()
        .ok()?;

    let sr = web_search::encode_uri_component(&format!("{query} haswbstatement:P2207"));
    let search_url = format!(
        "https://www.wikidata.org/w/api.php?action=query&list=search&srsearch={sr}&srlimit=1&format=json"
    );
    let resp1: serde_json::Value = client.get(&search_url).send().await.ok()?.json().await.ok()?;
    let qid = resp1
        .get("query")?
        .get("search")?
        .as_array()?
        .first()?
        .get("title")?
        .as_str()?
        .trim();
    if qid.is_empty() {
        return None;
    }

    let entity_url = format!(
        "https://www.wikidata.org/w/api.php?action=wbgetentities&ids={qid}&props=claims&format=json"
    );
    let resp2: serde_json::Value = client.get(&entity_url).send().await.ok()?.json().await.ok()?;
    let claims = resp2
        .get("entities")?
        .get(qid)?
        .get("claims")?
        .get("P2207")?
        .as_array()?;

    for claim in claims {
        if let Some(tid) = claim
            .get("mainsnak")
            .and_then(|m| m.get("datavalue"))
            .and_then(|d| d.get("value"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|t| t.len() == 22 && t.chars().all(|c| c.is_ascii_alphanumeric()))
        {
            return Some(format!("spotify:track:{tid}"));
        }
    }
    None
}

async fn trigger_spotify_playback(query: &str) -> (bool, String) {
    let (mut uri, mut is_direct_playable) = normalize_spotify_open_uri(query);

    if !is_direct_playable
        && !query.trim().is_empty()
        && !query.trim().starts_with("spotify:")
    {
        if let Some(track_uri) = resolve_spotify_track_uri(query).await {
            uri = track_uri;
            is_direct_playable = true;
        }
    }

    let Ok(conn) = Connection::session().await else {
        return (false, uri);
    };
    let Some(spotify_bus) = resolve_target_mpris_bus(&conn, Some("spotify")).await else {
        if let Some(any_bus) = resolve_target_mpris_bus(&conn, None).await {
            let ok = call_mpris_method_unit(&conn, &any_bus, "Play").await.is_ok();
            return (ok, uri);
        }
        return (false, uri);
    };

    if !uri.is_empty() {
        let open_ok = call_mpris_open_uri(&conn, &spotify_bus, &uri).await.is_ok();
        tokio::time::sleep(Duration::from_millis(350)).await;

        if open_ok && is_direct_playable {
            let _ = call_mpris_method_unit(&conn, &spotify_bus, "Play").await;
            for _ in 0..4 {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if read_mpris_properties_on_bus(&conn, &spotify_bus)
                    .await
                    .is_some_and(|(s, _)| s.is_playing)
                {
                    return (true, uri);
                }
            }
        }
    }

    for attempt in 0..3 {
        if call_mpris_method_unit(&conn, &spotify_bus, "Play")
            .await
            .is_ok()
        {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if read_mpris_properties_on_bus(&conn, &spotify_bus)
                .await
                .is_some_and(|(s, _)| s.is_playing)
            {
                return (true, uri);
            }
        }
        if attempt + 1 < 3 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    if let Some((_, Some(current_track_uri))) =
        read_mpris_properties_on_bus(&conn, &spotify_bus).await
    {
        if call_mpris_open_uri(&conn, &spotify_bus, &current_track_uri)
            .await
            .is_ok()
        {
            tokio::time::sleep(Duration::from_millis(350)).await;
            let _ = call_mpris_method_unit(&conn, &spotify_bus, "Play").await;
            if read_mpris_properties_on_bus(&conn, &spotify_bus)
                .await
                .is_some_and(|(s, _)| s.is_playing)
            {
                return (true, uri);
            }
        }
    }

    (false, uri)
}

/// Resolved YouTube video or search fallback URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YouTubeDirectVideo {
    pub video_id: Option<String>,
    pub url: String,
    pub resolver: &'static str,
}

pub fn build_youtube_autoplay_url(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}&autoplay=1")
}

pub fn is_valid_youtube_video_id(candidate: &str) -> bool {
    let trimmed = candidate.trim();
    trimmed.len() == 11
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn extract_direct_youtube_url_if_given(query: &str) -> Option<YouTubeDirectVideo> {
    let trimmed = query.trim();
    static DIRECT_YT_RE: OnceLock<Regex> = OnceLock::new();
    let re = DIRECT_YT_RE.get_or_init(|| {
        Regex::new(
            r"(?i)(?:youtube\.com/watch\?(?:[^#\s]*&)?v=|youtu\.be/|youtube\.com/shorts/)([a-zA-Z0-9_-]{11})",
        )
        .expect("valid direct youtube regex")
    });

    let caps = re.captures(trimmed)?;
    let vid = caps.get(1)?.as_str();
    if is_valid_youtube_video_id(vid) {
        Some(YouTubeDirectVideo {
            video_id: Some(vid.to_string()),
            url: build_youtube_autoplay_url(vid),
            resolver: "direct_url",
        })
    } else {
        None
    }
}

pub fn extract_youtube_video_id_from_html(html: &str) -> Option<String> {
    static VIDEO_RENDERER_RE: OnceLock<Regex> = OnceLock::new();
    static GENERIC_VIDEO_ID_RE: OnceLock<Regex> = OnceLock::new();

    let vr_re = VIDEO_RENDERER_RE.get_or_init(|| {
        Regex::new(r#""videoRenderer"\s*:\s*\{\s*"videoId"\s*:\s*"([a-zA-Z0-9_-]{11})""#)
            .expect("valid videoRenderer regex")
    });
    if let Some(id) = vr_re
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .filter(|id| is_valid_youtube_video_id(id))
    {
        return Some(id.to_string());
    }

    let generic_re = GENERIC_VIDEO_ID_RE.get_or_init(|| {
        Regex::new(r#""videoId"\s*:\s*"([a-zA-Z0-9_-]{11})""#).expect("valid videoId regex")
    });
    generic_re
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .filter(|id| is_valid_youtube_video_id(id))
        .map(str::to_string)
}

pub async fn resolve_youtube_direct_url(query: &str) -> YouTubeDirectVideo {
    if let Some(direct) = extract_direct_youtube_url_if_given(query) {
        return direct;
    }

    let trimmed = query.trim();
    let encoded = web_search::encode_uri_component(trimmed);
    let search_url = format!("https://www.youtube.com/results?search_query={encoded}");

    if let Ok(client) = Client::builder()
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(YOUTUBE_RESOLVE_TIMEOUT_SECS))
        .user_agent(web_search::QUICK_SEARCH_USER_AGENT)
        .build()
    {
        if let Ok(resp) = client
            .get(&search_url)
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await
        {
            if resp.status().is_success() {
                if let Some(vid) = resp
                    .text()
                    .await
                    .ok()
                    .and_then(|html| extract_youtube_video_id_from_html(&html))
                {
                    return YouTubeDirectVideo {
                        url: build_youtube_autoplay_url(&vid),
                        video_id: Some(vid),
                        resolver: "ytInitialData",
                    };
                }
            }
        }
    }

    if let Some(vid) = web_search::find_youtube_video_id_via_ddg(trimmed)
        .await
        .filter(|v| is_valid_youtube_video_id(v))
    {
        return YouTubeDirectVideo {
            url: build_youtube_autoplay_url(&vid),
            video_id: Some(vid),
            resolver: "duckduckgo_html",
        };
    }

    YouTubeDirectVideo {
        video_id: None,
        url: search_url,
        resolver: "search_fallback",
    }
}

fn default_media_platform() -> String {
    "spotify".to_string()
}

/// Arguments for `open_media`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenMediaArgs {
    pub query: String,
    #[serde(default = "default_media_platform")]
    pub platform: String,
    #[serde(default)]
    pub workspace: Option<i32>,
}

impl OpenMediaArgs {
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        let mut parsed: Self = serde_json::from_str(raw)
            .map_err(|e| format!("argumen JSON open_media tidak valid: {e}"))?;
        parsed.query = parsed.query.trim().to_string();
        parsed.platform = parsed.platform.trim().to_ascii_lowercase();
        if parsed.platform.is_empty() {
            parsed.platform = default_media_platform();
        }
        if parsed.query.is_empty() {
            return Err("field `query` tidak boleh kosong".to_string());
        }
        Ok(parsed)
    }
}

/// Outcome of resuming playback after interactive login confirmation.
#[derive(Debug, Clone)]
pub struct LoginResumeOutcome {
    pub face: &'static str,
    pub message: String,
}

pub async fn execute_open_media_with_config(
    args: &OpenMediaArgs,
    ws_state: &SessionWorkspaceState,
    pending_action: &Arc<Mutex<PendingAction>>,
    kaomoji_engine: &Arc<Mutex<KaomojiEngine>>,
    bot_name: &str,
    desktop_cfg: Option<&DesktopConfig>,
) -> crate::Result<String> {
    let target_ws = workspace_tracker::resolve_target_workspace_with_config(
        args.workspace,
        ws_state,
        desktop_cfg,
    )
    .await
    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    let audio_warning = check_audio_preflight()
        .await
        .and_then(|st| st.warning_message());

    if args.platform == "youtube" || args.platform == "yt" {
        let resolved = resolve_youtube_direct_url(&args.query).await;
        let report =
            desktop::launch_browser_in_workspace_with_config(&resolved.url, target_ws, desktop_cfg)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        let status_label = if resolved.video_id.is_some() {
            "playing_youtube"
        } else {
            "opened_youtube"
        };
        let mut res = format!(
            "status: {status_label}\n\
             workspace: {}\n\
             query: {}\n\
             video_id: {}\n\
             resolver: {}\n\
             url: {}\n\
             new_window: {}\n\
             command: {}",
            report.target_workspace,
            args.query,
            resolved.video_id.as_deref().unwrap_or("(search)"),
            resolved.resolver,
            resolved.url,
            report.used_new_window,
            report.launch_command
        );
        if let Some(warn) = audio_warning {
            res.push_str(&format!("\naudio_warning: {warn}"));
        }
        return Ok(res);
    }

    let backend = desktop::active_window_backend(desktop_cfg);
    let desktop_windows = backend.list_windows().unwrap_or_default();
    let existing_spotify_ws = desktop_windows
        .iter()
        .find(|w| w.is_spotify())
        .map(|w| w.workspace);
    let spotify_window_active = existing_spotify_ws.is_some();
    let mut on_session_dbus = is_spotify_on_session_dbus().await;

    let effective_ws = existing_spotify_ws
        .filter(|&sp_ws| args.workspace.is_none() && sp_ws > 0)
        .unwrap_or(target_ws);

    let mut snap = query_mpris_snapshot(Some("spotify")).await;

    if spotify_window_active || on_session_dbus {
        if snap.is_none() {
            let _ = wait_for_mpris_ready("spotify", 3, MPRIS_DEFAULT_DELAY_MS).await;
            snap = query_mpris_snapshot(Some("spotify")).await;
        }
    } else if snap.is_none() {
        if crate::wizard::detector::which("spotify").is_some() {
            backend
                .launch_app_in_workspace("spotify", effective_ws)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        } else {
            let encoded = web_search::encode_uri_component(&args.query);
            let spotify_web = format!("https://open.spotify.com/search/{encoded}");
            desktop::launch_browser_in_workspace_with_config(&spotify_web, effective_ws, desktop_cfg)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        }

        if wait_for_mpris_ready("spotify", MPRIS_COLD_BOOT_RETRIES, MPRIS_DEFAULT_DELAY_MS).await {
            tokio::time::sleep(Duration::from_millis(500)).await;
            on_session_dbus = is_spotify_on_session_dbus().await;
            snap = query_mpris_snapshot(Some("spotify")).await;
        }
    }

    if !is_spotify_session_ready(spotify_window_active, on_session_dbus, snap.as_ref()) {
        if let Ok(mut guard) = pending_action.lock() {
            *guard = PendingAction::WaitingForLogin {
                service: "spotify".to_string(),
                workspace: effective_ws,
                target_query: args.query.clone(),
                created_at: Instant::now(),
            };
        }

        let mut msg = format!(
            "status: waiting_for_login\n\
             service: spotify\n\
             workspace: {effective_ws}\n\
             query: {}\n\
             instruction_to_user: Spotify udah kubukain di workspace {effective_ws} ya! \
             Login dulu gih di sana (bisa via aplikasi atau web: https://open.spotify.com). \
             Nanti kalau udah selesai login, ketik \"udah\" yaa biar langsung kuputarin lagunya!",
            args.query
        );
        if let Some(warn) = audio_warning {
            msg.push_str(&format!("\naudio_warning: {warn}"));
        }
        return Ok(msg);
    }

    if let Ok(mut guard) = pending_action.lock() {
        *guard = PendingAction::None;
    }

    let (played, opened_uri) = trigger_spotify_playback(&args.query).await;
    let updated_snap = query_mpris_snapshot(Some("spotify")).await;
    let now_playing = updated_snap.as_ref().and_then(|s| s.track_info.as_deref());
    let is_actually_playing = updated_snap.as_ref().is_some_and(|s| s.is_playing) || played;
    let media_face = kaomoji_engine
        .lock()
        .map(|mut eng| {
            if is_actually_playing {
                eng.pick(EmotionSpectrum::MelodicChill)
            } else {
                eng.pick(EmotionSpectrum::TsunderePout)
            }
        })
        .unwrap_or(Kaomoji::Music.as_str());
    desktop::sync_waybar_for_media_status_with_kaomoji(
        bot_name,
        is_actually_playing,
        Some(media_face),
        now_playing,
    );

    let mut out = format!(
        "status: playing\n\
         service: spotify\n\
         workspace: {effective_ws}\n\
         spotify_window_detected: {spotify_window_active}\n\
         query: {}\n\
         uri: {}\n\
         player_state: {}\n\
         now_playing: {}",
        args.query,
        opened_uri,
        updated_snap
            .as_ref()
            .map(|s| s.status.as_str())
            .unwrap_or(if played { "Playing" } else { "Active" }),
        now_playing.unwrap_or(&args.query)
    );
    if let Some(warn) = audio_warning {
        out.push_str(&format!("\naudio_warning: {warn}"));
    }
    Ok(out)
}

pub async fn resume_after_login(
    service: &str,
    workspace: i32,
    target_query: &str,
    kaomoji_engine: &Arc<Mutex<KaomojiEngine>>,
    bot_name: &str,
) -> LoginResumeOutcome {
    let mpris_ready = wait_for_mpris_ready(
        service,
        MPRIS_DEFAULT_MAX_RETRIES,
        MPRIS_DEFAULT_DELAY_MS,
    )
    .await;

    let played = if mpris_ready || service.eq_ignore_ascii_case("spotify") {
        trigger_spotify_playback(target_query).await.0
    } else if let Ok(conn) = Connection::session().await {
        if let Some(bus) = resolve_target_mpris_bus(&conn, None).await {
            call_mpris_method_unit(&conn, &bus, "Play").await.is_ok()
        } else {
            false
        }
    } else {
        false
    };

    let audio_warning = check_audio_preflight()
        .await
        .and_then(|st| st.warning_message());

    let snap = query_mpris_snapshot(Some(service))
        .await
        .or(query_mpris_snapshot(None).await);

    if played || snap.as_ref().is_some_and(|s| s.is_playing) {
        let track = snap.as_ref().and_then(|s| s.track_info.as_deref());
        let face = kaomoji_engine
            .lock()
            .map(|mut eng| eng.pick(EmotionSpectrum::MelodicChill))
            .unwrap_or(Kaomoji::Music.as_str());
        desktop::sync_waybar_for_media_status_with_kaomoji(bot_name, true, Some(face), track);

        let mut msg = format!(
            "Sip! Lagunya ({target_query}) udah mulai diputar di workspace {workspace}. Selamat mendengarkan!"
        );
        if let Some(warn) = audio_warning {
            msg.push_str(&format!("\n{warn}"));
        }
        LoginResumeOutcome { face, message: msg }
    } else {
        let face = kaomoji_engine
            .lock()
            .map(|mut eng| {
                eng.pick_from_group(&[
                    EmotionSpectrum::MelancholyFail,
                    EmotionSpectrum::ProtectiveCare,
                ])
            })
            .unwrap_or(Kaomoji::Confused.as_str());
        let mut msg = format!(
            "Hmm, D-Bus MPRIS `{service}` belum merespons penuh setelah 4 detik di workspace {workspace}. \
             Pastikan akunnya sudah masuk ya, atau coba tekan Play sekali dulu di sana."
        );
        if let Some(warn) = audio_warning {
            msg.push_str(&format!("\n{warn}"));
        }
        LoginResumeOutcome { face, message: msg }
    }
}

/// Arguments for `control_media`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlMediaArgs {
    pub action: String,
    #[serde(default)]
    pub player: Option<String>,
}

impl ControlMediaArgs {
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        let mut parsed: Self = serde_json::from_str(raw)
            .map_err(|e| format!("argumen JSON control_media tidak valid: {e}"))?;
        parsed.action = parsed.action.trim().to_ascii_lowercase();
        parsed.player = parsed
            .player
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());
        if parsed.action.is_empty() {
            return Err("field `action` tidak boleh kosong".to_string());
        }
        Ok(parsed)
    }
}

pub async fn execute_control_media(
    args: &ControlMediaArgs,
    kaomoji_engine: &Arc<Mutex<KaomojiEngine>>,
    bot_name: &str,
) -> crate::Result<String> {
    let player_ref = args.player.as_deref();

    match args.action.as_str() {
        "get_info" => {
            let Some(snap) = query_mpris_snapshot(player_ref).await else {
                let idle_face = kaomoji_engine
                    .lock()
                    .map(|mut eng| eng.pick(EmotionSpectrum::TsunderePout))
                    .unwrap_or(Kaomoji::Greet.as_str());
                desktop::sync_waybar_for_media_status_with_kaomoji(
                    bot_name,
                    false,
                    Some(idle_face),
                    None,
                );
                return Ok(NO_ACTIVE_PLAYER_MSG.to_string());
            };

            let face = kaomoji_engine
                .lock()
                .map(|mut eng| {
                    if snap.is_playing {
                        eng.pick(EmotionSpectrum::MelodicChill)
                    } else {
                        eng.pick(EmotionSpectrum::SmugSuperior)
                    }
                })
                .unwrap_or(Kaomoji::Music.as_str());
            desktop::sync_waybar_for_media_status_with_kaomoji(
                bot_name,
                snap.is_playing,
                Some(face),
                snap.track_info.as_deref(),
            );

            Ok(format!(
                "status: {}\ntrack: {}",
                snap.status,
                snap.track_info
                    .as_deref()
                    .unwrap_or("(metadata judul/artis tidak tersedia)")
            ))
        }
        "play_pause" | "play" | "pause" | "next" | "previous" => {
            let dbus_method = match args.action.as_str() {
                "play_pause" => "PlayPause",
                "play" => "Play",
                "pause" => "Pause",
                "next" => "Next",
                "previous" => "Previous",
                _ => unreachable!(),
            };

            let Ok(conn) = Connection::session().await else {
                return Ok(NO_ACTIVE_PLAYER_MSG.to_string());
            };
            let Some(bus) = resolve_target_mpris_bus(&conn, player_ref).await else {
                let idle_face = kaomoji_engine
                    .lock()
                    .map(|mut eng| eng.pick(EmotionSpectrum::TsunderePout))
                    .unwrap_or(Kaomoji::Greet.as_str());
                desktop::sync_waybar_for_media_status_with_kaomoji(
                    bot_name,
                    false,
                    Some(idle_face),
                    None,
                );
                return Ok(NO_ACTIVE_PLAYER_MSG.to_string());
            };

            if call_mpris_method_unit(&conn, &bus, dbus_method)
                .await
                .is_err()
            {
                return Ok(NO_ACTIVE_PLAYER_MSG.to_string());
            }

            tokio::time::sleep(Duration::from_millis(80)).await;
            let snap = read_mpris_properties_on_bus(&conn, &bus)
                .await
                .map(|(s, _)| s);
            let is_playing = snap.as_ref().is_some_and(|s| s.is_playing);
            let track = snap.as_ref().and_then(|s| s.track_info.as_deref());
            let face = kaomoji_engine
                .lock()
                .map(|mut eng| {
                    if is_playing {
                        eng.pick(EmotionSpectrum::MelodicChill)
                    } else {
                        eng.pick(EmotionSpectrum::DrowsyIdle)
                    }
                })
                .unwrap_or(Kaomoji::Music.as_str());
            desktop::sync_waybar_for_media_status_with_kaomoji(
                bot_name,
                is_playing,
                Some(face),
                track,
            );

            let mut response = format!(
                "status: success\naction: {}\nplayer_state: {}\ntrack: {}",
                args.action,
                snap.as_ref()
                    .map(|s| s.status.as_str())
                    .unwrap_or("Unknown"),
                track.unwrap_or("(tidak tersedia)")
            );

            if is_playing {
                if let Some(warn) = check_audio_preflight()
                    .await
                    .and_then(|st| st.warning_message())
                {
                    response.push_str(&format!("\naudio_warning: {warn}"));
                }
            }

            Ok(response)
        }
        other => Err(format!(
            "Aksi `{other}` tidak dikenal pada control_media. Gunakan: play_pause, play, pause, next, previous, atau get_info."
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wpctl_volume_muted_low_and_normal() {
        let muted = parse_wpctl_volume("Volume: 0.50 [MUTED]\n").unwrap();
        assert!(muted.muted);
        assert!((muted.volume - 0.50).abs() < 0.001);
        assert!(muted.warning_message().unwrap().contains("di-mute"));

        let low = parse_wpctl_volume("Volume: 0.02").unwrap();
        assert!(!low.muted);
        assert!(low.volume < LOW_VOLUME_THRESHOLD);
        assert!(low.warning_message().is_some());

        let normal = parse_wpctl_volume("Volume: 0.65").unwrap();
        assert!(!normal.muted);
        assert_eq!(normal.warning_message(), None);

        assert_eq!(parse_wpctl_volume(""), None);
    }

    #[test]
    fn pending_action_matches_slang_rejects_slash_commands_and_expires_after_15_minutes() {
        for ok in [
            "udah",
            "UDH!",
            "dah.",
            "udah login",
            "beres",
            "siap!",
            "done",
            "selesai",
        ] {
            assert!(
                PendingAction::is_login_confirmation_slang(ok),
                "should match slang: {ok}"
            );
        }

        for nope in ["/telemetry", "/note halo", "cek ram dong", "pacman -Syu", ""] {
            assert!(
                !PendingAction::is_login_confirmation_slang(nope),
                "should NOT match slang: {nope}"
            );
        }

        let t0 = Instant::now();
        let pending = PendingAction::WaitingForLogin {
            service: "spotify".to_string(),
            workspace: 4,
            target_query: "YOASOBI Idol".to_string(),
            created_at: t0,
        };
        assert!(!pending.is_expired_at(t0 + Duration::from_secs(600)));
        assert!(pending.is_expired_at(t0 + Duration::from_secs(LOGIN_TIMEOUT_SECS + 1)));
    }

    #[test]
    fn spotify_session_ready_never_loops_login_when_window_or_mpris_exists() {
        assert!(!is_spotify_session_ready(false, false, None));

        assert!(playerctl_list_contains_spotify(
            "org.mpris.MediaPlayer2.firefox\norg.mpris.MediaPlayer2.spotify\n"
        ));
        assert!(!playerctl_list_contains_spotify(
            "org.mpris.MediaPlayer2.firefox\norg.mpris.MediaPlayer2.chromium\n"
        ));

        assert!(is_spotify_session_ready(false, true, None));
        assert!(is_spotify_session_ready(true, false, None));
        assert!(is_spotify_session_ready(
            false,
            false,
            Some(&MediaStatusSnapshot {
                status: "Paused".to_string(),
                is_playing: false,
                track_info: Some("Idol - YOASOBI".to_string()),
            })
        ));

        assert_eq!(
            normalize_spotify_open_uri("spotify:track:0aym2LBJBk9DAYuHHutrIl"),
            ("spotify:track:0aym2LBJBk9DAYuHHutrIl".to_string(), true)
        );
        assert_eq!(
            normalize_spotify_open_uri(
                "https://open.spotify.com/intl-id/track/0aym2LBJBk9DAYuHHutrIl?si=abc"
            ),
            ("spotify:track:0aym2LBJBk9DAYuHHutrIl".to_string(), true)
        );
        assert_eq!(
            normalize_spotify_open_uri("Hey Jude The Beatles"),
            (
                "spotify:search:Hey%20Jude%20The%20Beatles".to_string(),
                false
            )
        );
        assert_eq!(
            parse_mpris_trackid_to_spotify_uri("/com/spotify/track/0aym2LBJBk9DAYuHHutrIl"),
            Some("spotify:track:0aym2LBJBk9DAYuHHutrIl".to_string())
        );
        assert_eq!(parse_mpris_trackid_to_spotify_uri("/invalid"), None);
    }

    #[test]
    fn extracts_youtube_video_id_from_yt_initial_data_and_direct_urls() {
        let sample_html = r#"
            <script>var ytInitialData = {"contents":{"twoColumnSearchResultsRenderer":{"primaryContents":{"sectionListRenderer":{"contents":[{"itemSectionRenderer":{"contents":[{"videoRenderer":{"videoId":"QdBZY2fkU-0","title":{"runs":[{"text":"Grand Theft Auto VI Trailer 1"}]}}}]}}]}}}}};</script>
        "#;
        assert_eq!(
            extract_youtube_video_id_from_html(sample_html).as_deref(),
            Some("QdBZY2fkU-0")
        );

        let fallback_json = r#"{"watchEndpoint":{"videoId":"rz1B4B2yBZY"}}"#;
        assert_eq!(
            extract_youtube_video_id_from_html(fallback_json).as_deref(),
            Some("rz1B4B2yBZY")
        );

        assert_eq!(extract_youtube_video_id_from_html("<html>no video</html>"), None);
        assert!(is_valid_youtube_video_id("QdBZY2fkU-0"));
        assert!(!is_valid_youtube_video_id("too_short"));
        assert_eq!(
            build_youtube_autoplay_url("QdBZY2fkU-0"),
            "https://www.youtube.com/watch?v=QdBZY2fkU-0&autoplay=1"
        );
    }

    #[tokio::test]
    async fn mpris_retry_loop_returns_false_quickly_for_nonexistent_player() {
        let ready = wait_for_mpris_ready("waish_nonexistent_player_xyz", 2, 10).await;
        assert!(!ready);
    }

    #[tokio::test]
    async fn control_media_returns_graceful_ok_when_no_player_active() {
        let engine = Arc::new(Mutex::new(KaomojiEngine::with_seed(7)));
        let info_args = ControlMediaArgs {
            action: "get_info".to_string(),
            player: Some("waish_nonexistent_player_xyz".to_string()),
        };
        let info_res = execute_control_media(&info_args, &engine, "alice")
            .await
            .expect("get_info without active player must return Ok, not Err");
        assert_eq!(info_res, NO_ACTIVE_PLAYER_MSG);

        let pp_args = ControlMediaArgs {
            action: "play_pause".to_string(),
            player: Some("waish_nonexistent_player_xyz".to_string()),
        };
        let pp_res = execute_control_media(&pp_args, &engine, "alice")
            .await
            .expect("play_pause without active player must return Ok, not Err");
        assert_eq!(pp_res, NO_ACTIVE_PLAYER_MSG);
    }
}
