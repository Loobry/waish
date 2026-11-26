//! Graceful Rate-Limit Cooldown (HTTP 429 Handler) & Bit-for-Bit Transparent Replay Engine.

use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use reqwest::header::HeaderMap;
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode};

use super::client::{
    classify_reqwest_error, effective_model_for_settings, provider_name, ClientError,
};
use crate::cli::cooldown;
use crate::config::settings::Settings;

pub const MAX_RATE_LIMIT_RETRIES: u32 = 3;
pub const DEFAULT_RATE_LIMIT_COOLDOWN_SECS: u64 = 20;
pub const MIN_RATE_LIMIT_COOLDOWN_SECS: u64 = 1;
pub const MAX_RATE_LIMIT_COOLDOWN_SECS: u64 = 120;

/// Bit-for-bit frozen HTTP request snapshot for transparent HTTP 429 replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenHttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body_bytes: Vec<u8>,
}

impl FrozenHttpRequest {
    pub fn capture(
        builder: RequestBuilder,
        provider: &str,
        timeout_secs: u64,
    ) -> Result<Self, ClientError> {
        let req = builder
            .build()
            .map_err(|err| classify_reqwest_error(err, provider, timeout_secs))?;
        let method = req.method().clone();
        let url = req.url().to_string();
        let headers = req.headers().clone();
        let body_bytes = req
            .body()
            .and_then(|b| b.as_bytes())
            .unwrap_or(&[])
            .to_vec();
        Ok(Self {
            method,
            url,
            headers,
            body_bytes,
        })
    }

    pub fn build_on(&self, http: &Client) -> RequestBuilder {
        http.request(self.method.clone(), &self.url)
            .headers(self.headers.clone())
            .body(self.body_bytes.clone())
    }
}

pub fn compute_rate_limit_backoff(base_delay: Duration, attempt: u32) -> Duration {
    let buffer_secs = match attempt {
        0 | 1 => 3,
        2 => 8,
        _ => 15,
    };
    base_delay + Duration::from_secs(buffer_secs)
}

pub fn is_rate_limit_error_text(text: &str) -> bool {
    let low = text.to_ascii_lowercase();
    [
        "429",
        "rate limit",
        "rate_limit",
        "ratelimit",
        "too many requests",
        "retry after",
        "retry-after",
        "quota exceeded",
        "rpm limit",
        "limit reached",
    ]
    .iter()
    .any(|kw| low.contains(kw))
}

pub fn extract_retry_duration(headers: &HeaderMap, error_body: &str) -> Duration {
    if let Some(val) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
        let trimmed = val.trim();
        if let Ok(secs) = trimmed.parse::<u64>() {
            return Duration::from_secs(
                secs.clamp(MIN_RATE_LIMIT_COOLDOWN_SECS, MAX_RATE_LIMIT_COOLDOWN_SECS),
            );
        }
        if let Some(secs_f) = trimmed
            .parse::<f64>()
            .ok()
            .filter(|&f| f.is_finite() && f > 0.0)
        {
            let rounded = secs_f.ceil() as u64;
            return Duration::from_secs(
                rounded.clamp(MIN_RATE_LIMIT_COOLDOWN_SECS, MAX_RATE_LIMIT_COOLDOWN_SECS),
            );
        }
    }

    static RETRY_BODY_RE: OnceLock<Regex> = OnceLock::new();
    let re = RETRY_BODY_RE.get_or_init(|| {
        Regex::new(r"(?i)retry\s+(?:after|in)\s+(\d+)\s*(?:s|sec|secs|second|seconds)?")
            .expect("valid retry-after body regex")
    });
    if let Some(secs) = re
        .captures(error_body)
        .and_then(|caps| caps.get(1))
        .and_then(|m| m.as_str().parse::<u64>().ok())
    {
        return Duration::from_secs(
            secs.clamp(MIN_RATE_LIMIT_COOLDOWN_SECS, MAX_RATE_LIMIT_COOLDOWN_SECS),
        );
    }

    static RETRY_ALT_RE: OnceLock<Regex> = OnceLock::new();
    let alt_re = RETRY_ALT_RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?:"retry_after"\s*:\s*(\d+)|try\s+again\s+in\s+(\d+)\s*(?:s|sec|secs|second|seconds)\b)"#,
        )
        .expect("valid retry-after alt regex")
    });
    if let Some(secs) = alt_re
        .captures(error_body)
        .and_then(|caps| caps.get(1).or_else(|| caps.get(2)))
        .and_then(|m| m.as_str().parse::<u64>().ok())
    {
        return Duration::from_secs(
            secs.clamp(MIN_RATE_LIMIT_COOLDOWN_SECS, MAX_RATE_LIMIT_COOLDOWN_SECS),
        );
    }

    Duration::from_secs(DEFAULT_RATE_LIMIT_COOLDOWN_SECS)
}

fn rate_limit_exhausted_error(provider: String) -> ClientError {
    ClientError::RateLimited {
        provider,
        detail: "Rate limit 429 masih penuh setelah 3x retry otomatis. Tunggu sebentar atau ganti model via /model yaa!"
            .to_string(),
    }
}

pub async fn send_frozen_with_rate_limit_retry_stateful(
    http: &Client,
    frozen: &FrozenHttpRequest,
    settings: &Settings,
    timeout_secs: u64,
    attempts: &mut u32,
) -> Result<Response, ClientError> {
    let bot_name = settings.general.bot_name.clone();
    let user_name = settings.general.user_real_name.clone();

    send_frozen_with_rate_limit_retry_stateful_with_waiter(
        http,
        frozen,
        settings,
        timeout_secs,
        attempts,
        move |delay, model_name, _attempt| {
            let model_owned = model_name.to_string();
            let bot_owned = bot_name.clone();
            let user_owned = user_name.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    cooldown::wait_with_interactive_countdown_for_user(
                        delay,
                        &model_owned,
                        &bot_owned,
                        &user_owned,
                    )
                })
                .await
                .unwrap_or(false)
            }
        },
    )
    .await
}

pub async fn send_frozen_with_rate_limit_retry_stateful_with_waiter<W, Fut>(
    http: &Client,
    frozen: &FrozenHttpRequest,
    settings: &Settings,
    timeout_secs: u64,
    attempts: &mut u32,
    mut waiter: W,
) -> Result<Response, ClientError>
where
    W: FnMut(Duration, &str, u32) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let provider_label = provider_name(settings);
    let model_label = effective_model_for_settings(settings);
    let mut stale_conn_retried = false;

    loop {
        let response = match frozen.build_on(http).send().await {
            Ok(resp) => resp,
            Err(err) if !err.is_timeout() && !stale_conn_retried => {
                stale_conn_retried = true;
                continue;
            }
            Err(err) => return Err(classify_reqwest_error(err, &provider_label, timeout_secs)),
        };

        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            *attempts = attempts.saturating_add(1);
            if *attempts > MAX_RATE_LIMIT_RETRIES {
                return Err(rate_limit_exhausted_error(provider_label));
            }

            let headers = response.headers().clone();
            let body = response.text().await.unwrap_or_default();
            let base_delay = extract_retry_duration(&headers, &body);
            let total_delay = compute_rate_limit_backoff(base_delay, *attempts);

            let should_retry = waiter(total_delay, &model_label, *attempts).await;
            if !should_retry {
                return Err(ClientError::Cancelled {
                    detail: "Permintaan dibatalkan saat menunggu kuota.".to_string(),
                });
            }
            continue;
        }

        return Ok(response);
    }
}

#[cfg(test)]
pub async fn send_frozen_with_rate_limit_retry_with_waiter<W, Fut>(
    http: &Client,
    frozen: &FrozenHttpRequest,
    settings: &Settings,
    timeout_secs: u64,
    waiter: W,
) -> Result<Response, ClientError>
where
    W: FnMut(Duration, &str, u32) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut attempts = 0u32;
    send_frozen_with_rate_limit_retry_stateful_with_waiter(
        http,
        frozen,
        settings,
        timeout_secs,
        &mut attempts,
        waiter,
    )
    .await
}

pub async fn wait_for_stream_rate_limit_cooldown_stateful(
    settings: &Settings,
    detail: &str,
    attempts: &mut u32,
) -> Result<(), ClientError> {
    let provider_label = provider_name(settings);
    let model_label = effective_model_for_settings(settings);

    *attempts = attempts.saturating_add(1);
    if *attempts > MAX_RATE_LIMIT_RETRIES {
        return Err(rate_limit_exhausted_error(provider_label));
    }

    let base_delay = extract_retry_duration(&HeaderMap::new(), detail);
    let total_delay = compute_rate_limit_backoff(base_delay, *attempts);
    let model_owned = model_label;
    let bot_owned = settings.general.bot_name.clone();
    let user_owned = settings.general.user_real_name.clone();

    let should_retry = tokio::task::spawn_blocking(move || {
        cooldown::wait_with_interactive_countdown_for_user(
            total_delay,
            &model_owned,
            &bot_owned,
            &user_owned,
        )
    })
    .await
    .unwrap_or(false);

    if !should_retry {
        return Err(ClientError::Cancelled {
            detail: "Permintaan dibatalkan saat menunggu kuota.".to_string(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn read_full_http_request(sock: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut raw = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut expected_total: Option<usize> = None;

        loop {
            let n = sock.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..n]);

            if expected_total.is_none() {
                if let Some(hdr_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let body_start = hdr_end + 4;
                    let hdr_str = String::from_utf8_lossy(&raw[..hdr_end]);
                    let content_len = hdr_str
                        .lines()
                        .find_map(|line| {
                            let (k, v) = line.split_once(':')?;
                            if k.trim().eq_ignore_ascii_case("content-length") {
                                v.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    expected_total = Some(body_start + content_len);
                }
            }

            if let Some(total) = expected_total {
                if raw.len() >= total {
                    break;
                }
            }
        }

        if let Some(hdr_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            raw[hdr_end + 4..].to_vec()
        } else {
            Vec::new()
        }
    }

    #[test]
    fn extract_retry_duration_and_exponential_safety_buffers_are_accurate() {
        let empty_headers = HeaderMap::new();

        assert_eq!(
            extract_retry_duration(
                &empty_headers,
                "Rate limit exceeded on custom gateway. Retry after 32s."
            ),
            Duration::from_secs(32)
        );
        assert_eq!(
            extract_retry_duration(&empty_headers, "limit reached. Retry after 60s."),
            Duration::from_secs(60)
        );
        assert_eq!(
            extract_retry_duration(
                &empty_headers,
                "Too many requests, please retry in 45 seconds"
            ),
            Duration::from_secs(45)
        );
        assert_eq!(
            extract_retry_duration(&empty_headers, r#"{"error":"429","retry_after":18}"#),
            Duration::from_secs(18)
        );

        let mut hdrs = HeaderMap::new();
        hdrs.insert("retry-after", HeaderValue::from_static("10"));
        let base = extract_retry_duration(&hdrs, "Retry after 60s.");
        assert_eq!(base, Duration::from_secs(10));

        // Safety buffer + exponential backoff wajib: +3s, +8s, +15s!
        assert_eq!(compute_rate_limit_backoff(base, 1), Duration::from_secs(13));
        assert_eq!(compute_rate_limit_backoff(base, 2), Duration::from_secs(18));
        assert_eq!(compute_rate_limit_backoff(base, 3), Duration::from_secs(25));
    }

    #[tokio::test]
    async fn bit_for_bit_429_replay_preserves_exact_payload_without_mutating_messages() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = Arc::clone(&hits);
        let captured_bodies: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_srv = Arc::clone(&captured_bodies);

        tokio::spawn(async move {
            for _ in 0..2 {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let body_bytes = read_full_http_request(&mut sock).await;
                    captured_srv.lock().unwrap().push(body_bytes);

                    let n = hits_srv.fetch_add(1, Ordering::SeqCst);
                    let resp = if n == 0 {
                        let body = r#"{"error":{"message":"Rate limit exceeded. Retry after 32s."}}"#;
                        format!(
                            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    } else {
                        let body = "ok_after_cooldown";
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    };
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            }
        });

        let mut settings = Settings::default();
        settings.api.provider = "custom".to_string();
        settings.api.model = "qwen-plus".to_string();
        settings.api.api_key = "test-key".to_string();

        let http = Client::new();
        let url = format!("http://{addr}/v1/chat/completions");
        let original_payload = serde_json::json!({
            "model": "qwen-plus",
            "messages": [
                {"role": "system", "content": "System instructions"},
                {"role": "user", "content": "Buatkan file laporan.xlsx"}
            ],
            "tools": [{"type": "function", "function": {"name": "manage_spreadsheet"}}]
        });

        let frozen = FrozenHttpRequest::capture(
            http.post(&url)
                .header("x-custom-trace", "waish-frozen-check")
                .json(&original_payload),
            "custom",
            10,
        )
        .expect("capture frozen request");

        let observed_delay = Arc::new(Mutex::new(None));
        let obs_clone = Arc::clone(&observed_delay);

        let res = send_frozen_with_rate_limit_retry_with_waiter(
            &http,
            &frozen,
            &settings,
            10,
            move |delay, _model, _attempt| {
                let obs = Arc::clone(&obs_clone);
                async move {
                    *obs.lock().unwrap() = Some(delay);
                    true
                }
            },
        )
        .await
        .expect("should transparently replay identical request on second attempt");

        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.text().await.unwrap(), "ok_after_cooldown");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        // Attempt 1 wajib memiliki safety buffer +3s (32s + 3s = 35s).
        assert_eq!(*observed_delay.lock().unwrap(), Some(Duration::from_secs(35)));

        let bodies = captured_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0], bodies[1]);
        assert_eq!(bodies[0], frozen.body_bytes);
    }

    #[tokio::test]
    async fn hard_cap_3_retries_never_resets_and_applies_3s_8s_15s_backoff() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = Arc::clone(&hits);

        tokio::spawn(async move {
            for _ in 0..6 {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    hits_srv.fetch_add(1, Ordering::SeqCst);
                    let body = "Retry after 10s.";
                    let resp = format!(
                        "HTTP/1.1 429 Too Many Requests\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            }
        });

        let mut settings = Settings::default();
        settings.api.provider = "custom".to_string();
        settings.api.model = "qwen-plus".to_string();
        let http = Client::new();
        let url = format!("http://{addr}/v1/chat/completions");
        let frozen = FrozenHttpRequest::capture(http.post(&url).body("{}"), "custom", 10).unwrap();

        let observed_waits: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
        let waits_clone = Arc::clone(&observed_waits);
        let mut shared_attempts = 0u32;

        let err = send_frozen_with_rate_limit_retry_stateful_with_waiter(
            &http,
            &frozen,
            &settings,
            10,
            &mut shared_attempts,
            move |delay, _model, _attempt| {
                let w = Arc::clone(&waits_clone);
                async move {
                    w.lock().unwrap().push(delay);
                    true
                }
            },
        )
        .await
        .expect_err("must stop immediately when retry #3 still returns 429");

        // Total HTTP calls = 1 initial + 3 retries = 4 calls, 3 waits ([13s, 18s, 25s]), langsung berhenti!
        assert_eq!(hits.load(Ordering::SeqCst), 4);
        assert_eq!(shared_attempts, 4);
        assert_eq!(
            *observed_waits.lock().unwrap(),
            vec![
                Duration::from_secs(13),
                Duration::from_secs(18),
                Duration::from_secs(25),
            ]
        );
        match err {
            ClientError::RateLimited { detail, .. } => {
                assert!(detail.contains("/model"));
            }
            other => panic!("expected ClientError::RateLimited, got {other:?}"),
        }
    }
}
