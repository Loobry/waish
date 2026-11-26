//! HTTP Streaming Client untuk LLM API.

use std::fmt;
use std::io::{self, Write};
use std::sync::OnceLock;
use std::time::Duration;

use colored::Colorize;
use regex::Regex;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::context::{AssembledContext, ChatMessage};
use crate::cli::prompt::{
    format_bot_prompt_colored_face, print_bot_inline, print_bot_inline_face,
};
use crate::cli::ui::Kaomoji;
use crate::config::settings::Settings;
use crate::tools::registry::{anthropic_tools_schema, openai_tools_schema};
use crate::tools::runner::ToolExecutionResult;
use crate::wizard::i18n::WizardLang;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Wire protocol selected from `config.toml` provider settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireProtocol {
    Anthropic,
    OpenAiCompatible,
}

impl WireProtocol {
    pub fn detect(settings: &Settings) -> Self {
        let provider = settings.api.provider.trim().to_ascii_lowercase();
        let key = settings.api.api_key.trim();
        let base = settings.api.base_url.trim().to_ascii_lowercase();
        if provider == "anthropic"
            || (provider.is_empty() && key.starts_with("sk-ant-"))
            || base.contains("api.anthropic.com")
        {
            WireProtocol::Anthropic
        } else {
            WireProtocol::OpenAiCompatible
        }
    }
}

/// Tool call emitted by the LLM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Single completed ReAct execution round.
#[derive(Debug, Clone)]
pub struct ToolRound {
    pub assistant_text: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolExecutionResult>,
    pub steering_message: Option<String>,
}

/// Output of a single streaming LLM turn.
#[derive(Debug, Clone, Default)]
pub struct StreamResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

/// Classified API and transport errors.
#[derive(Debug, Clone)]
pub enum ClientError {
    MissingApiKey { provider: String },
    Unauthorized { provider: String, detail: String },
    RateLimited { provider: String, detail: String },
    Cancelled { detail: String },
    Timeout { secs: u64 },
    Network { provider: String, detail: String },
    ApiError { status: u16, detail: String },
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::MissingApiKey { provider } => {
                write!(f, "API key untuk provider '{provider}' belum diisi")
            }
            ClientError::Unauthorized { provider, detail } => {
                write!(f, "autentikasi '{provider}' ditolak: {detail}")
            }
            ClientError::RateLimited { provider, detail } => {
                write!(f, "rate limit '{provider}' tercapai: {detail}")
            }
            ClientError::Cancelled { detail } => {
                write!(f, "{detail}")
            }
            ClientError::Timeout { secs } => {
                write!(f, "permintaan ke server melebihi batas waktu ({secs} detik)")
            }
            ClientError::Network { provider, detail } => {
                write!(f, "koneksi ke '{provider}' terputus: {detail}")
            }
            ClientError::ApiError { status, detail } => {
                write!(f, "server mengembalikan HTTP {status}: {detail}")
            }
        }
    }
}

impl std::error::Error for ClientError {}

impl ClientError {
    pub fn print_friendly(&self, settings: &Settings) {
        self.print_friendly_with_face(settings, Kaomoji::Fail.as_str());
    }

    pub fn print_friendly_with_face(&self, settings: &Settings, face: &str) {
        let lang = WizardLang::from_code(&settings.general.language);
        match self {
            ClientError::MissingApiKey { provider } => {
                let msg = lang.err_missing_api_key(
                    &provider.bold().to_string(),
                    &"waish -c".cyan().bold().to_string(),
                );
                print_bot_inline_face(settings, face, &msg);
            }
            ClientError::Unauthorized { provider, detail } => {
                let msg = lang.err_unauthorized(
                    &provider.bold().to_string(),
                    &"waish -c".cyan().bold().to_string(),
                );
                print_bot_inline_face(settings, face, &msg);
                if !detail.is_empty() {
                    println!("  {} {}", "detail:".bright_black(), detail.bright_black());
                }
            }
            ClientError::RateLimited { provider, detail } => {
                let msg = lang.err_rate_limited(
                    &provider.bold().to_string(),
                    &"/model".cyan().bold().to_string(),
                );
                print_bot_inline_face(settings, face, &msg);
                if !detail.is_empty() {
                    println!("  {} {}", "detail:".bright_black(), detail.bright_black());
                }
            }
            ClientError::Cancelled { .. } => {}
            ClientError::Timeout { secs } => {
                print_bot_inline_face(settings, face, &lang.err_timeout(*secs));
            }
            ClientError::Network { provider, detail } => {
                let msg = lang.err_network(&provider.bold().to_string());
                print_bot_inline_face(settings, face, &msg);
                if !detail.is_empty() {
                    println!("  {} {}", "detail:".bright_black(), detail.bright_black());
                }
            }
            ClientError::ApiError { status, detail } => {
                let msg = lang.err_http_status(
                    &status.to_string().yellow().bold().to_string(),
                    &settings.api.model.cyan().to_string(),
                );
                print_bot_inline_face(settings, face, &msg);
                if !detail.is_empty() {
                    println!("  {} {}", "detail:".bright_black(), detail.bright_black());
                }
            }
        }
    }
}

/// Asynchronous HTTP client for streaming LLM communication.
#[derive(Debug, Clone)]
pub struct LlmClient {
    http: Client,
    timeout_secs: u64,
}

impl LlmClient {
    pub fn new(settings: &Settings) -> crate::Result<Self> {
        let timeout_secs = settings.api.timeout_secs.max(5);
        let http = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("waish/", env!("CARGO_PKG_VERSION")))
            .build()?;

        Ok(Self { http, timeout_secs })
    }

    #[cfg(test)]
    pub async fn stream_chat(
        &self,
        settings: &Settings,
        context: &AssembledContext,
    ) -> Result<StreamResponse, ClientError> {
        self.stream_chat_with_face(settings, context, Kaomoji::Ready.as_str())
            .await
    }

    pub async fn stream_chat_with_face(
        &self,
        settings: &Settings,
        context: &AssembledContext,
        face: &str,
    ) -> Result<StreamResponse, ClientError> {
        let protocol = WireProtocol::detect(settings);
        let endpoint = resolve_endpoint(settings, protocol);

        self.execute_streaming_request(settings, protocol, face, || match protocol {
            WireProtocol::Anthropic => self.build_anthropic_request(settings, &endpoint, context),
            WireProtocol::OpenAiCompatible => {
                self.build_openai_request(settings, &endpoint, context)
            }
        })
        .await
    }

    pub async fn stream_react_followup_with_face(
        &self,
        settings: &Settings,
        context: &AssembledContext,
        rounds: &[ToolRound],
        allow_tools: bool,
        face: &str,
    ) -> Result<StreamResponse, ClientError> {
        let protocol = WireProtocol::detect(settings);
        let endpoint = resolve_endpoint(settings, protocol);

        self.execute_streaming_request(settings, protocol, face, || match protocol {
            WireProtocol::Anthropic => self.build_anthropic_followup_request(
                settings,
                &endpoint,
                context,
                rounds,
                allow_tools,
            ),
            WireProtocol::OpenAiCompatible => self.build_openai_followup_request(
                settings,
                &endpoint,
                context,
                rounds,
                allow_tools,
            ),
        })
        .await
    }

    async fn execute_streaming_request<F>(
        &self,
        settings: &Settings,
        protocol: WireProtocol,
        face: &str,
        build_request: F,
    ) -> Result<StreamResponse, ClientError>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let provider_label = provider_name(settings);

        if !is_ollama(settings) && settings.api.api_key.trim().is_empty() {
            return Err(ClientError::MissingApiKey {
                provider: provider_label,
            });
        }

        let frozen_request = super::provider::FrozenHttpRequest::capture(
            build_request(),
            &provider_label,
            self.timeout_secs,
        )?;

        let mut rate_limit_attempts: u32 = 0;
        let mut empty_stream_retries: u32 = 0;

        loop {
            let mut response = super::provider::send_frozen_with_rate_limit_retry_stateful(
                &self.http,
                &frozen_request,
                settings,
                self.timeout_secs,
                &mut rate_limit_attempts,
            )
            .await?;

            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(classify_http_status(status, &body, &provider_label));
            }

            let mut parser = SseStreamParser::new(protocol);
            let mut display_filter = StreamToolCallFilter::default();
            let mut printed_prefix = false;
            let mut full_text = String::new();
            let mut in_stream_rate_limit_detail: Option<String> = None;

            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|err| classify_reqwest_error(err, &provider_label, self.timeout_secs))?
            {
                let tokens = match parser.feed_bytes(&chunk) {
                    Ok(t) => t,
                    Err(ClientError::RateLimited { detail, .. })
                        if !printed_prefix && full_text.is_empty() =>
                    {
                        in_stream_rate_limit_detail = Some(detail);
                        break;
                    }
                    Err(err) => return Err(err),
                };

                for token in tokens {
                    let clean = sanitize_terminal_text(&token);
                    if clean.is_empty() {
                        continue;
                    }
                    full_text.push_str(&clean);
                    let visible = display_filter.push(&clean);
                    if !visible.is_empty() {
                        if !printed_prefix {
                            print!("{}", format_bot_prompt_colored_face(settings, face));
                            printed_prefix = true;
                        }
                        print!("{visible}");
                        let _ = io::stdout().flush();
                    }
                }
            }

            if let Some(detail) = in_stream_rate_limit_detail {
                super::provider::wait_for_stream_rate_limit_cooldown_stateful(
                    settings,
                    &detail,
                    &mut rate_limit_attempts,
                )
                .await?;
                continue;
            }

            let finished_tokens = match parser.finish() {
                Ok(t) => t,
                Err(ClientError::RateLimited { detail, .. })
                    if !printed_prefix && full_text.is_empty() =>
                {
                    super::provider::wait_for_stream_rate_limit_cooldown_stateful(
                        settings,
                        &detail,
                        &mut rate_limit_attempts,
                    )
                    .await?;
                    continue;
                }
                Err(err) => return Err(err),
            };

            for token in finished_tokens {
                let clean = sanitize_terminal_text(&token);
                if clean.is_empty() {
                    continue;
                }
                full_text.push_str(&clean);
                let visible = display_filter.push(&clean);
                if !visible.is_empty() {
                    if !printed_prefix {
                        print!("{}", format_bot_prompt_colored_face(settings, face));
                        printed_prefix = true;
                    }
                    print!("{visible}");
                    let _ = io::stdout().flush();
                }
            }

            let remaining_visible = display_filter.finish();
            if !remaining_visible.is_empty() {
                if !printed_prefix {
                    print!("{}", format_bot_prompt_colored_face(settings, face));
                    printed_prefix = true;
                }
                print!("{remaining_visible}");
                let _ = io::stdout().flush();
            }

            let sanitized_full = sanitize_llm_output(&full_text);
            let tool_calls = parser.into_tool_calls();
            let has_fallback_calls =
                !crate::tools::runner::intercept_text_tool_calls(&sanitized_full)
                    .1
                    .is_empty();

            if !printed_prefix
                && sanitized_full.trim().is_empty()
                && tool_calls.is_empty()
                && !has_fallback_calls
                && empty_stream_retries == 0
            {
                empty_stream_retries += 1;
                continue;
            }

            if printed_prefix {
                println!();
            } else if tool_calls.is_empty() && !has_fallback_calls {
                let lang = WizardLang::from_code(&settings.general.language);
                print_bot_inline(settings, Kaomoji::Confused, lang.empty_server_response());
            }

            return Ok(StreamResponse {
                text: sanitized_full,
                tool_calls,
            });
        }
    }

    pub async fn check_status(&self, settings: &Settings) -> Result<String, ClientError> {
        let ping_ctx = AssembledContext {
            system_prompt: "Balas hanya dengan kata: OK".to_string(),
            messages: vec![ChatMessage::user("ping")],
        };

        let provider_label = provider_name(settings);
        if !is_ollama(settings) && settings.api.api_key.trim().is_empty() {
            return Err(ClientError::MissingApiKey {
                provider: provider_label,
            });
        }

        let protocol = WireProtocol::detect(settings);
        let endpoint = resolve_endpoint(settings, protocol);
        let request = match protocol {
            WireProtocol::Anthropic => {
                let body = json!({
                    "model": effective_model(settings, protocol),
                    "max_tokens": 8,
                    "system": ping_ctx.system_prompt,
                    "messages": ping_ctx.messages,
                    "stream": false,
                });
                self.http
                    .post(&endpoint)
                    .header("x-api-key", settings.api.api_key.trim())
                    .header("anthropic-version", ANTHROPIC_VERSION)
                    .json(&body)
            }
            WireProtocol::OpenAiCompatible => {
                let mut msgs = vec![ChatMessage::system(ping_ctx.system_prompt)];
                msgs.extend(ping_ctx.messages);
                let body = json!({
                    "model": effective_model(settings, protocol),
                    "max_tokens": 8,
                    "messages": msgs,
                    "stream": false,
                });
                let mut req = self.http.post(&endpoint);
                if !settings.api.api_key.trim().is_empty() {
                    req = req.bearer_auth(settings.api.api_key.trim());
                }
                req.json(&body)
            }
        };

        let resp = request
            .send()
            .await
            .map_err(|e| classify_reqwest_error(e, &provider_label, self.timeout_secs))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(classify_http_status(status, &body, &provider_label));
        }

        Ok(format!(
            "Terhubung ke {} (model: {}, endpoint: {})",
            provider_label,
            effective_model(settings, protocol),
            endpoint
        ))
    }

    pub async fn complete_oneshot(
        &self,
        settings: &Settings,
        system_prompt: &str,
        user_prompt: &str,
        timeout: Duration,
    ) -> Result<String, ClientError> {
        let provider_label = provider_name(settings);
        if !is_ollama(settings) && settings.api.api_key.trim().is_empty() {
            return Err(ClientError::MissingApiKey {
                provider: provider_label,
            });
        }

        let protocol = WireProtocol::detect(settings);
        let endpoint = resolve_endpoint(settings, protocol);
        let timeout_secs = timeout.as_secs().max(1);

        let request = match protocol {
            WireProtocol::Anthropic => {
                let body = json!({
                    "model": effective_model(settings, protocol),
                    "max_tokens": DEFAULT_MAX_TOKENS,
                    "temperature": 0.2,
                    "system": system_prompt,
                    "messages": [ChatMessage::user(user_prompt)],
                    "stream": false,
                });
                self.http
                    .post(&endpoint)
                    .timeout(timeout)
                    .header("x-api-key", settings.api.api_key.trim())
                    .header("anthropic-version", ANTHROPIC_VERSION)
                    .json(&body)
            }
            WireProtocol::OpenAiCompatible => {
                let mut body = json!({
                    "model": effective_model(settings, protocol),
                    "temperature": 0.2,
                    "messages": [
                        ChatMessage::system(system_prompt),
                        ChatMessage::user(user_prompt),
                    ],
                    "stream": false,
                });
                if is_ollama(settings) {
                    body["stop"] = ollama_stop_sequences();
                }
                let mut req = self.http.post(&endpoint).timeout(timeout);
                if !settings.api.api_key.trim().is_empty() {
                    req = req.bearer_auth(settings.api.api_key.trim());
                }
                if settings.api.provider.eq_ignore_ascii_case("openrouter") {
                    req = req
                        .header("HTTP-Referer", "https://github.com/waish-sh/waish")
                        .header("X-Title", "waish");
                }
                req.json(&body)
            }
        };

        let resp = tokio::time::timeout(timeout, request.send())
            .await
            .map_err(|_| ClientError::Timeout { secs: timeout_secs })?
            .map_err(|e| classify_reqwest_error(e, &provider_label, timeout_secs))?;

        let status = resp.status();
        let raw_body = tokio::time::timeout(timeout, resp.text())
            .await
            .map_err(|_| ClientError::Timeout { secs: timeout_secs })?
            .map_err(|e| classify_reqwest_error(e, &provider_label, timeout_secs))?;

        if !status.is_success() {
            return Err(classify_http_status(status, &raw_body, &provider_label));
        }

        extract_oneshot_text(&raw_body, protocol).ok_or_else(|| ClientError::ApiError {
            status: status.as_u16(),
            detail: "respons JSON tidak memuat teks hasil".to_string(),
        })
    }

    fn build_anthropic_request(
        &self,
        settings: &Settings,
        endpoint: &str,
        context: &AssembledContext,
    ) -> reqwest::RequestBuilder {
        let body = json!({
            "model": effective_model(settings, WireProtocol::Anthropic),
            "max_tokens": DEFAULT_MAX_TOKENS,
            "temperature": settings.api.temperature,
            "system": context.system_prompt,
            "messages": context.messages,
            "tools": anthropic_tools_schema(),
            "stream": true,
        });

        self.http
            .post(endpoint)
            .header("x-api-key", settings.api.api_key.trim())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("accept", "text/event-stream")
            .json(&body)
    }

    fn build_anthropic_followup_request(
        &self,
        settings: &Settings,
        endpoint: &str,
        context: &AssembledContext,
        rounds: &[ToolRound],
        allow_tools: bool,
    ) -> reqwest::RequestBuilder {
        let mut messages: Vec<Value> = context
            .messages
            .iter()
            .filter_map(|m| serde_json::to_value(m).ok())
            .collect();

        for (round_idx, round) in rounds.iter().enumerate() {
            let mut assistant_blocks = Vec::new();
            if !round.assistant_text.trim().is_empty() {
                assistant_blocks.push(json!({
                    "type": "text",
                    "text": round.assistant_text,
                }));
            }
            for (idx, tc) in round.tool_calls.iter().enumerate() {
                let id = if tc.id.is_empty() {
                    format!("toolu_{round_idx}_{idx}")
                } else {
                    tc.id.clone()
                };
                let input_obj = serde_json::from_str::<Value>(&tc.arguments)
                    .unwrap_or_else(|_| json!({ "command": tc.arguments }));
                assistant_blocks.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": tc.name,
                    "input": input_obj,
                }));
            }
            messages.push(json!({
                "role": "assistant",
                "content": assistant_blocks,
            }));

            let mut result_blocks: Vec<Value> = round
                .tool_results
                .iter()
                .enumerate()
                .map(|(idx, res)| {
                    let id = if res.tool_call_id.is_empty() {
                        format!("toolu_{round_idx}_{idx}")
                    } else {
                        res.tool_call_id.clone()
                    };
                    json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": res.output_for_llm,
                        "is_error": !res.succeeded,
                    })
                })
                .collect();

            if let Some(steering) = round
                .steering_message
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                result_blocks.push(json!({
                    "type": "text",
                    "text": steering,
                }));
            }

            messages.push(json!({
                "role": "user",
                "content": result_blocks,
            }));
        }

        let mut body = json!({
            "model": effective_model(settings, WireProtocol::Anthropic),
            "max_tokens": DEFAULT_MAX_TOKENS,
            "temperature": settings.api.temperature,
            "system": context.system_prompt,
            "messages": messages,
            "stream": true,
        });
        if allow_tools {
            body["tools"] = anthropic_tools_schema();
        }

        self.http
            .post(endpoint)
            .header("x-api-key", settings.api.api_key.trim())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("accept", "text/event-stream")
            .json(&body)
    }

    fn build_openai_request(
        &self,
        settings: &Settings,
        endpoint: &str,
        context: &AssembledContext,
    ) -> reqwest::RequestBuilder {
        let mut messages = Vec::with_capacity(context.messages.len() + 1);
        messages.push(ChatMessage::system(&context.system_prompt));
        messages.extend(context.messages.iter().cloned());

        let mut body = json!({
            "model": effective_model(settings, WireProtocol::OpenAiCompatible),
            "temperature": settings.api.temperature,
            "messages": messages,
            "tools": openai_tools_schema(),
            "stream": true,
        });
        if is_ollama(settings) {
            body["stop"] = ollama_stop_sequences();
        }

        self.decorate_openai_headers(settings, endpoint).json(&body)
    }

    fn build_openai_followup_request(
        &self,
        settings: &Settings,
        endpoint: &str,
        context: &AssembledContext,
        rounds: &[ToolRound],
        allow_tools: bool,
    ) -> reqwest::RequestBuilder {
        let mut messages: Vec<Value> = Vec::with_capacity(context.messages.len() + rounds.len() * 3 + 2);
        messages.push(json!({
            "role": "system",
            "content": context.system_prompt,
        }));
        for m in &context.messages {
            if let Ok(val) = serde_json::to_value(m) {
                messages.push(val);
            }
        }

        for (round_idx, round) in rounds.iter().enumerate() {
            let formatted_calls: Vec<Value> = round
                .tool_calls
                .iter()
                .enumerate()
                .map(|(idx, tc)| {
                    let id = if tc.id.is_empty() {
                        format!("call_{round_idx}_{idx}")
                    } else {
                        tc.id.clone()
                    };
                    json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": tc.name,
                            "arguments": tc.arguments,
                        }
                    })
                })
                .collect();

            let content_val = if round.assistant_text.trim().is_empty() {
                Value::Null
            } else {
                Value::String(round.assistant_text.clone())
            };

            messages.push(json!({
                "role": "assistant",
                "content": content_val,
                "tool_calls": formatted_calls,
            }));

            for (idx, res) in round.tool_results.iter().enumerate() {
                let id = if res.tool_call_id.is_empty() {
                    format!("call_{round_idx}_{idx}")
                } else {
                    res.tool_call_id.clone()
                };
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "name": res.tool_name,
                    "content": res.output_for_llm,
                }));
            }

            if let Some(steering) = round
                .steering_message
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                messages.push(json!({
                    "role": "user",
                    "content": steering,
                }));
            }
        }

        let mut body = json!({
            "model": effective_model(settings, WireProtocol::OpenAiCompatible),
            "temperature": settings.api.temperature,
            "messages": messages,
            "stream": true,
        });
        if allow_tools {
            body["tools"] = openai_tools_schema();
        }
        if is_ollama(settings) {
            body["stop"] = ollama_stop_sequences();
        }

        self.decorate_openai_headers(settings, endpoint).json(&body)
    }

    fn decorate_openai_headers(
        &self,
        settings: &Settings,
        endpoint: &str,
    ) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .post(endpoint)
            .header("accept", "text/event-stream");

        let key = settings.api.api_key.trim();
        if !key.is_empty() {
            req = req.bearer_auth(key);
        }

        if settings.api.provider.eq_ignore_ascii_case("openrouter") {
            req = req
                .header("HTTP-Referer", "https://github.com/waish-sh/waish")
                .header("X-Title", "waish");
        }

        req
    }
}

fn is_ollama(settings: &Settings) -> bool {
    let p = settings.api.provider.trim();
    p.eq_ignore_ascii_case("ollama") || p.eq_ignore_ascii_case("local")
}

fn ollama_stop_sequences() -> Value {
    json!(["<|im_end|>", "<|eot_id|>", "<|endoftext|>", "</s>"])
}

pub(super) fn provider_name(settings: &Settings) -> String {
    let p = settings.api.provider.trim();
    if p.is_empty() {
        match WireProtocol::detect(settings) {
            WireProtocol::Anthropic => "anthropic".to_string(),
            WireProtocol::OpenAiCompatible => "openai".to_string(),
        }
    } else {
        p.to_string()
    }
}

pub(super) fn effective_model_for_settings(settings: &Settings) -> String {
    let protocol = WireProtocol::detect(settings);
    effective_model(settings, protocol)
}

fn effective_model(settings: &Settings, protocol: WireProtocol) -> String {
    let m = settings.api.model.trim();
    if !m.is_empty() {
        return m.to_string();
    }
    match protocol {
        WireProtocol::Anthropic => "claude-3-5-sonnet-latest".to_string(),
        WireProtocol::OpenAiCompatible if is_ollama(settings) => "qwen2.5-coder".to_string(),
        WireProtocol::OpenAiCompatible => "gpt-4o".to_string(),
    }
}

pub fn resolve_endpoint(settings: &Settings, protocol: WireProtocol) -> String {
    let base = settings.api.base_url.trim().trim_end_matches('/');
    match protocol {
        WireProtocol::Anthropic => {
            let root = if base.is_empty() {
                "https://api.anthropic.com/v1"
            } else {
                base
            };
            if root.ends_with("/messages") {
                root.to_string()
            } else {
                format!("{root}/messages")
            }
        }
        WireProtocol::OpenAiCompatible => {
            let root = if base.is_empty() {
                match settings.api.provider.trim().to_ascii_lowercase().as_str() {
                    "vercel" => "https://ai-gateway.vercel.sh/v1",
                    "gemini" => "https://generativelanguage.googleapis.com/v1beta",
                    "openrouter" => "https://openrouter.ai/api/v1",
                    "grok" | "xai" => "https://api.x.ai/v1",
                    "deepseek" => "https://api.deepseek.com/v1",
                    "ollama" | "local" => "http://localhost:11434",
                    _ => "https://api.openai.com/v1",
                }
            } else {
                base
            };

            if root.ends_with("/chat/completions") || root.ends_with("/api/chat") {
                root.to_string()
            } else if root.eq_ignore_ascii_case("https://generativelanguage.googleapis.com/v1beta")
            {
                format!("{root}/openai/chat/completions")
            } else if is_ollama(settings) && !root.ends_with("/v1") {
                format!("{root}/v1/chat/completions")
            } else {
                format!("{root}/chat/completions")
            }
        }
    }
}

pub(super) fn classify_reqwest_error(
    err: reqwest::Error,
    provider: &str,
    timeout_secs: u64,
) -> ClientError {
    if err.is_timeout() {
        ClientError::Timeout { secs: timeout_secs }
    } else {
        ClientError::Network {
            provider: provider.to_string(),
            detail: err.to_string(),
        }
    }
}

fn classify_http_status(status: StatusCode, body: &str, provider: &str) -> ClientError {
    let detail = extract_error_message(body);
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ClientError::Unauthorized {
            provider: provider.to_string(),
            detail,
        },
        StatusCode::TOO_MANY_REQUESTS => ClientError::RateLimited {
            provider: provider.to_string(),
            detail,
        },
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => ClientError::Timeout {
            secs: 60,
        },
        other => ClientError::ApiError {
            status: other.as_u16(),
            detail,
        },
    }
}

pub fn extract_error_message(raw_body: &str) -> String {
    let trimmed = raw_body.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
        if let Some(msg) = val
            .get("error")
            .and_then(|e| e.get("message").or(Some(e)))
            .and_then(Value::as_str)
        {
            return msg.trim().to_string();
        }
        if let Some(msg) = val.get("message").and_then(Value::as_str) {
            return msg.trim().to_string();
        }
    }

    trimmed.lines().next().unwrap_or("").chars().take(160).collect()
}

pub fn extract_oneshot_text(raw_body: &str, protocol: WireProtocol) -> Option<String> {
    let val: Value = serde_json::from_str(raw_body.trim()).ok()?;
    match protocol {
        WireProtocol::Anthropic => {
            let blocks = val.get("content")?.as_array()?;
            let mut out = String::new();
            for b in blocks {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        out.push_str(t);
                    }
                }
            }
            let cleaned = sanitize_llm_output(out.trim());
            (!cleaned.is_empty()).then_some(cleaned)
        }
        WireProtocol::OpenAiCompatible => {
            if let Some(text) = val
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|arr| arr.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
            {
                let cleaned = sanitize_llm_output(text.trim());
                return (!cleaned.is_empty()).then_some(cleaned);
            }
            if let Some(text) = val
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
            {
                let cleaned = sanitize_llm_output(text.trim());
                return (!cleaned.is_empty()).then_some(cleaned);
            }
            None
        }
    }
}

/// Incremental parser for Server-Sent Events (SSE) and NDJSON streams.
#[derive(Debug)]
pub struct SseStreamParser {
    protocol: WireProtocol,
    buffer: Vec<u8>,
    tool_calls: Vec<ToolCall>,
}

impl SseStreamParser {
    pub fn new(protocol: WireProtocol) -> Self {
        Self {
            protocol,
            buffer: Vec::new(),
            tool_calls: Vec::new(),
        }
    }

    pub fn feed_bytes(&mut self, chunk: &[u8]) -> Result<Vec<String>, ClientError> {
        self.buffer.extend_from_slice(chunk);
        let mut tokens = Vec::new();

        while let Some(newline_idx) = self.buffer.iter().position(|&b| b == b'\n') {
            let raw_line: Vec<u8> = self.buffer.drain(..=newline_idx).collect();
            let line = String::from_utf8_lossy(&raw_line[..raw_line.len() - 1])
                .trim_end_matches('\r')
                .to_string();

            if let Some(piece) = self.parse_line(&line)?.filter(|p| !p.is_empty()) {
                tokens.push(piece);
            }
        }

        Ok(tokens)
    }

    pub fn finish(&mut self) -> Result<Vec<String>, ClientError> {
        if self.buffer.is_empty() {
            return Ok(Vec::new());
        }
        let leftover = String::from_utf8_lossy(&self.buffer).trim().to_string();
        self.buffer.clear();
        match self.parse_line(&leftover)? {
            Some(piece) if !piece.is_empty() => Ok(vec![piece]),
            _ => Ok(Vec::new()),
        }
    }

    pub fn into_tool_calls(self) -> Vec<ToolCall> {
        self.tool_calls
    }

    fn parse_line(&mut self, line: &str) -> Result<Option<String>, ClientError> {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with(':')
            || trimmed.starts_with("event:")
            || trimmed.starts_with("id:")
            || trimmed.starts_with("retry:")
        {
            return Ok(None);
        }

        let payload = match trimmed.strip_prefix("data:") {
            Some(rest) => rest.trim(),
            None if trimmed.starts_with('{') => trimmed,
            None => return Ok(None),
        };

        if payload.is_empty() || payload == "[DONE]" {
            return Ok(None);
        }

        let Ok(json) = serde_json::from_str::<Value>(payload) else {
            return Ok(None);
        };

        match self.protocol {
            WireProtocol::Anthropic => self.parse_anthropic_event(&json),
            WireProtocol::OpenAiCompatible => self.parse_openai_event(&json),
        }
    }

    fn parse_anthropic_event(&mut self, json: &Value) -> Result<Option<String>, ClientError> {
        let event_type = json.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "error" => {
                let detail = extract_error_message(&json.to_string());
                let err_type = json
                    .get("error")
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if err_type.eq_ignore_ascii_case("rate_limit_error")
                    || super::provider::is_rate_limit_error_text(&detail)
                {
                    Err(ClientError::RateLimited {
                        provider: "anthropic".to_string(),
                        detail,
                    })
                } else {
                    Err(ClientError::ApiError {
                        status: 500,
                        detail,
                    })
                }
            }
            "content_block_start" => {
                if let Some(block) = json
                    .get("content_block")
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    self.tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: String::new(),
                    });
                }
                Ok(None)
            }
            "content_block_delta" => {
                let Some(delta) = json.get("delta") else {
                    return Ok(None);
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => Ok(delta
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)),
                    Some("input_json_delta") => {
                        if let (Some(partial), Some(last)) = (
                            delta.get("partial_json").and_then(Value::as_str),
                            self.tool_calls.last_mut(),
                        ) {
                            last.arguments.push_str(partial);
                        }
                        Ok(None)
                    }
                    _ => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }

    fn parse_openai_event(&mut self, json: &Value) -> Result<Option<String>, ClientError> {
        if let Some(err_obj) = json.get("error") {
            let detail = err_obj
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_else(|| err_obj.as_str().unwrap_or("unknown stream error"))
                .to_string();
            let code_is_429 = err_obj.get("code").and_then(Value::as_u64) == Some(429)
                || err_obj.get("status").and_then(Value::as_u64) == Some(429)
                || err_obj
                    .get("code")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c == "429" || c.eq_ignore_ascii_case("rate_limit_exceeded"));
            if code_is_429 || super::provider::is_rate_limit_error_text(&detail) {
                return Err(ClientError::RateLimited {
                    provider: "openai".to_string(),
                    detail,
                });
            }
            return Err(ClientError::ApiError {
                status: 500,
                detail,
            });
        }

        if let Some(delta) = json
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|choice| choice.get("delta"))
        {
            if let Some(tc_array) = delta.get("tool_calls").and_then(Value::as_array) {
                for tc in tc_array {
                    self.merge_openai_tool_call_delta(tc);
                }
            }
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                return Ok(Some(content.to_string()));
            }
        }

        if let Some(content) = json
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
        {
            return Ok(Some(content.to_string()));
        }

        Ok(None)
    }

    fn merge_openai_tool_call_delta(&mut self, tc: &Value) {
        let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        while self.tool_calls.len() <= index {
            self.tool_calls.push(ToolCall {
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            });
        }

        let entry = &mut self.tool_calls[index];
        if let Some(id) = tc.get("id").and_then(Value::as_str) {
            entry.id.push_str(id);
        }
        if let Some(func) = tc.get("function") {
            if let Some(name) = func.get("name").and_then(Value::as_str) {
                entry.name.push_str(name);
            }
            if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                entry.arguments.push_str(args);
            }
        }
    }
}

const PSEUDO_TOOL_PREFIXES: &[&str] = &[
    "[memanggil",
    "[panggil",
    "[tool_call",
    "[tool call",
    "[tool:",
    "[call tool",
    "[call:",
    "[function",
    "[run_shell_command(",
    "[read_file(",
    "[write_file(",
    "[replace_in_file(",
    "[rollback_config(",
    "[manage_workspace(",
    "[launch_gui_app(",
    "[manage_window(",
    "[open_media(",
    "[control_media(",
    "[open_browser_search(",
    "[quick_search(",
    "[read_clipboard(",
    "[find_package_owner(",
    "[manage_scratchpad(",
    "[manage_alarm(",
    "[manage_spreadsheet(",
    "[manage_document(",
    "[update_battery_promise(",
    "<tool_call>",
];

const EXACT_GLITCH_STOP_TOKENS: &[&str] = &[
    "</s>",
    "<s>",
    "[inst]",
    "[/inst]",
    "[baisuspil]",
    "<<sys>>",
    "<</sys>>",
];

#[derive(Debug, Default)]
enum FilterMode {
    #[default]
    Passthrough,
    Checking {
        buf: String,
    },
    SuppressBracket {
        depth: usize,
        quote: Option<char>,
        escaped: bool,
    },
    SuppressXml {
        tail: String,
    },
    SuppressAngleStop {
        len: usize,
    },
    CheckingReversedParen {
        buf: String,
    },
    CheckingKeke {
        buf: String,
        matched_keke: bool,
        in_dots: bool,
    },
    SuppressRawAnsi {
        kind: RawAnsiKind,
        len: usize,
    },
    CheckingCaretEscape {
        buf: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawAnsiKind {
    Start,
    Csi,
    Osc,
    OscEsc,
}

#[derive(Debug)]
struct StreamToolCallFilter {
    mode: FilterMode,
    at_word_boundary: bool,
    last_emitted_space: bool,
}

impl Default for StreamToolCallFilter {
    fn default() -> Self {
        Self {
            mode: FilterMode::Passthrough,
            at_word_boundary: true,
            last_emitted_space: false,
        }
    }
}

impl StreamToolCallFilter {
    fn push(&mut self, text: &str) -> String {
        let mut out = String::new();
        for ch in text.chars() {
            self.push_char(ch, &mut out);
        }
        out
    }

    fn emit_char(&mut self, ch: char, out: &mut String) {
        out.push(ch);
        self.at_word_boundary = !ch.is_alphanumeric() && ch != '_';
        self.last_emitted_space = ch == ' ';
    }

    fn emit_str(&mut self, s: &str, out: &mut String) {
        out.push_str(s);
        if let Some(last) = s.chars().last() {
            self.at_word_boundary = !last.is_alphanumeric() && last != '_';
            self.last_emitted_space = last == ' ';
        }
    }

    fn finish(self) -> String {
        match self.mode {
            FilterMode::Checking { buf } => {
                let lower = buf.to_ascii_lowercase();
                if PSEUDO_TOOL_PREFIXES.iter().any(|p| lower.starts_with(p))
                    || EXACT_GLITCH_STOP_TOKENS.iter().any(|t| lower == *t)
                    || lower.starts_with("<|")
                {
                    String::new()
                } else {
                    buf
                }
            }
            FilterMode::CheckingReversedParen { buf } => {
                if buf.to_ascii_lowercase().contains("baisuspil") {
                    String::new()
                } else {
                    buf
                }
            }
            FilterMode::CheckingKeke {
                buf, matched_keke, ..
            } => {
                if matched_keke {
                    String::new()
                } else {
                    buf
                }
            }
            FilterMode::CheckingCaretEscape { buf } => {
                if buf.starts_with("^[[") || buf.starts_with("^[O") {
                    String::new()
                } else {
                    buf
                }
            }
            _ => String::new(),
        }
    }

    fn push_char(&mut self, ch: char, out: &mut String) {
        let mode = std::mem::take(&mut self.mode);
        match mode {
            FilterMode::Passthrough => {
                if ch == '\x1b' {
                    self.mode = FilterMode::SuppressRawAnsi {
                        kind: RawAnsiKind::Start,
                        len: 1,
                    };
                } else if ch.is_control() && !matches!(ch, '\n' | '\r' | '\t') {
                    self.mode = FilterMode::Passthrough;
                } else if ch == '^' {
                    let mut buf = String::with_capacity(10);
                    buf.push(ch);
                    self.mode = FilterMode::CheckingCaretEscape { buf };
                } else if ch == '[' || ch == '<' {
                    let mut buf = String::with_capacity(24);
                    buf.push(ch);
                    self.mode = FilterMode::Checking { buf };
                } else if ch == ')' {
                    let mut buf = String::with_capacity(16);
                    buf.push(ch);
                    self.mode = FilterMode::CheckingReversedParen { buf };
                } else if self.at_word_boundary && (ch == 'k' || ch == 'K') {
                    let mut buf = String::with_capacity(16);
                    buf.push(ch);
                    self.mode = FilterMode::CheckingKeke {
                        buf,
                        matched_keke: false,
                        in_dots: false,
                    };
                } else {
                    self.emit_char(ch, out);
                    self.mode = FilterMode::Passthrough;
                }
            }
            FilterMode::Checking { mut buf } => {
                buf.push(ch);
                let lower = buf.to_ascii_lowercase();

                if lower == "<|" {
                    self.mode = FilterMode::SuppressAngleStop { len: 2 };
                    return;
                }

                if EXACT_GLITCH_STOP_TOKENS.iter().any(|&tok| lower == tok) {
                    self.at_word_boundary = true;
                    self.mode = FilterMode::Passthrough;
                    return;
                }

                if let Some(&matched) = PSEUDO_TOOL_PREFIXES.iter().find(|&&p| lower == p) {
                    if matched.starts_with('<') {
                        self.mode = FilterMode::SuppressXml {
                            tail: String::new(),
                        };
                    } else {
                        self.mode = FilterMode::SuppressBracket {
                            depth: 1,
                            quote: None,
                            escaped: false,
                        };
                    }
                    return;
                }

                let still_possible = "<|".starts_with(&lower)
                    || EXACT_GLITCH_STOP_TOKENS
                        .iter()
                        .any(|t| t.starts_with(&lower))
                    || PSEUDO_TOOL_PREFIXES.iter().any(|p| p.starts_with(&lower));
                if still_possible && buf.len() <= 28 {
                    self.mode = FilterMode::Checking { buf };
                } else {
                    let last = buf.pop();
                    self.emit_str(&buf, out);
                    self.mode = FilterMode::Passthrough;
                    if let Some(c) = last {
                        self.push_char(c, out);
                    }
                }
            }
            FilterMode::SuppressBracket {
                mut depth,
                mut quote,
                mut escaped,
            } => {
                if let Some(q) = quote {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == q {
                        quote = None;
                    }
                } else if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                } else if ch == '[' {
                    depth += 1;
                } else if ch == ']' {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        self.at_word_boundary = true;
                        self.mode = FilterMode::Passthrough;
                        return;
                    }
                }
                self.mode = FilterMode::SuppressBracket {
                    depth,
                    quote,
                    escaped,
                };
            }
            FilterMode::SuppressXml { mut tail } => {
                tail.push(ch);
                if tail.len() > 24 {
                    let drop_n = tail.len() - 16;
                    if tail.is_char_boundary(drop_n) {
                        tail.drain(..drop_n);
                    }
                }
                if tail.to_ascii_lowercase().ends_with("</tool_call>") {
                    self.at_word_boundary = true;
                    self.mode = FilterMode::Passthrough;
                } else {
                    self.mode = FilterMode::SuppressXml { tail };
                }
            }
            FilterMode::SuppressAngleStop { len } => {
                if ch == '>' || ch == '\n' || len >= 44 {
                    self.at_word_boundary = true;
                    self.mode = FilterMode::Passthrough;
                    if ch == '\n' {
                        self.emit_char('\n', out);
                    }
                } else {
                    self.mode = FilterMode::SuppressAngleStop { len: len + 1 };
                }
            }
            FilterMode::CheckingReversedParen { mut buf } => {
                let is_valid_inner = if buf.len() == 1 {
                    ch.is_ascii_alphabetic()
                } else {
                    ch.is_ascii_alphanumeric() || ch == '_'
                };

                if is_valid_inner && buf.len() <= 32 {
                    buf.push(ch);
                    self.mode = FilterMode::CheckingReversedParen { buf };
                } else if ch == '(' && buf.len() >= 6 {
                    self.at_word_boundary = true;
                    self.mode = FilterMode::Passthrough;
                } else {
                    self.emit_str(&buf, out);
                    self.mode = FilterMode::Passthrough;
                    self.push_char(ch, out);
                }
            }
            FilterMode::CheckingKeke {
                mut buf,
                mut matched_keke,
                in_dots,
            } => {
                if in_dots {
                    if (ch == '.' || ch == '…') && buf.len() < 32 {
                        buf.push(ch);
                        self.mode = FilterMode::CheckingKeke {
                            buf,
                            matched_keke,
                            in_dots: true,
                        };
                    } else {
                        self.at_word_boundary = true;
                        self.mode = FilterMode::Passthrough;
                        if !(ch == ' ' && self.last_emitted_space) {
                            self.push_char(ch, out);
                        }
                    }
                    return;
                }

                let expect_e = buf.len() % 2 == 1;
                let matches_next = if expect_e {
                    ch == 'e' || ch == 'E'
                } else {
                    ch == 'k' || ch == 'K'
                };

                if matches_next && buf.len() < 24 {
                    buf.push(ch);
                    matched_keke = buf.len() >= 4 && buf.len() % 2 == 0;
                    self.mode = FilterMode::CheckingKeke {
                        buf,
                        matched_keke,
                        in_dots: false,
                    };
                } else if matched_keke {
                    if ch.is_alphanumeric() || ch == '_' {
                        self.emit_str(&buf, out);
                        self.mode = FilterMode::Passthrough;
                        self.push_char(ch, out);
                    } else if ch == '.' || ch == '…' {
                        buf.push(ch);
                        self.mode = FilterMode::CheckingKeke {
                            buf,
                            matched_keke: true,
                            in_dots: true,
                        };
                    } else {
                        self.at_word_boundary = true;
                        self.mode = FilterMode::Passthrough;
                        if !(ch == ' ' && self.last_emitted_space) {
                            self.push_char(ch, out);
                        }
                    }
                } else {
                    self.emit_str(&buf, out);
                    self.mode = FilterMode::Passthrough;
                    self.push_char(ch, out);
                }
            }
            FilterMode::SuppressRawAnsi { kind, len } => {
                if len >= 64 || ch == '\n' {
                    self.mode = FilterMode::Passthrough;
                    if ch == '\n' {
                        self.emit_char('\n', out);
                    }
                    return;
                }
                match kind {
                    RawAnsiKind::Start => match ch {
                        '[' => {
                            self.mode = FilterMode::SuppressRawAnsi {
                                kind: RawAnsiKind::Csi,
                                len: len + 1,
                            };
                        }
                        ']' => {
                            self.mode = FilterMode::SuppressRawAnsi {
                                kind: RawAnsiKind::Osc,
                                len: len + 1,
                            };
                        }
                        _ => {
                            self.mode = FilterMode::Passthrough;
                        }
                    },
                    RawAnsiKind::Csi => {
                        if ('@'..='~').contains(&ch) {
                            self.mode = FilterMode::Passthrough;
                        } else {
                            self.mode = FilterMode::SuppressRawAnsi {
                                kind: RawAnsiKind::Csi,
                                len: len + 1,
                            };
                        }
                    }
                    RawAnsiKind::Osc => {
                        if ch == '\x07' {
                            self.mode = FilterMode::Passthrough;
                        } else if ch == '\x1b' {
                            self.mode = FilterMode::SuppressRawAnsi {
                                kind: RawAnsiKind::OscEsc,
                                len: len + 1,
                            };
                        } else {
                            self.mode = FilterMode::SuppressRawAnsi {
                                kind: RawAnsiKind::Osc,
                                len: len + 1,
                            };
                        }
                    }
                    RawAnsiKind::OscEsc => {
                        if ch == '\\' {
                            self.mode = FilterMode::Passthrough;
                        } else {
                            self.mode = FilterMode::SuppressRawAnsi {
                                kind: RawAnsiKind::Osc,
                                len: len + 1,
                            };
                        }
                    }
                }
            }
            FilterMode::CheckingCaretEscape { mut buf } => {
                if buf.len() == 1 {
                    if ch == '[' {
                        buf.push(ch);
                        self.mode = FilterMode::CheckingCaretEscape { buf };
                    } else {
                        self.emit_str(&buf, out);
                        self.mode = FilterMode::Passthrough;
                        self.push_char(ch, out);
                    }
                } else if buf.len() == 2 {
                    if ch == '[' || ch == 'O' {
                        buf.push(ch);
                        self.mode = FilterMode::CheckingCaretEscape { buf };
                    } else {
                        self.emit_str(&buf, out);
                        self.mode = FilterMode::Passthrough;
                        self.push_char(ch, out);
                    }
                } else if buf.starts_with("^[O") {
                    if ch.is_ascii_alphabetic() {
                        self.mode = FilterMode::Passthrough;
                    } else {
                        self.emit_str(&buf, out);
                        self.mode = FilterMode::Passthrough;
                        self.push_char(ch, out);
                    }
                } else if (ch.is_ascii_digit() || ch == ';' || ch == '?') && buf.len() < 16 {
                    buf.push(ch);
                    self.mode = FilterMode::CheckingCaretEscape { buf };
                } else if ch.is_ascii_alphabetic() || ch == '~' {
                    self.mode = FilterMode::Passthrough;
                } else {
                    self.emit_str(&buf, out);
                    self.mode = FilterMode::Passthrough;
                    self.push_char(ch, out);
                }
            }
        }
    }
}

pub fn strip_terminal_escapes(input: &str) -> String {
    static CARET_ESC_RE: OnceLock<Regex> = OnceLock::new();
    let caret_re = CARET_ESC_RE.get_or_init(|| {
        Regex::new(r"\^\[\[[0-9;?]*[A-Za-z~]|\^\[O[A-Za-z]")
            .expect("valid caret escape regex")
    });

    let mut no_raw = String::with_capacity(input.len());
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
        if ch.is_control() && !matches!(ch, '\n' | '\r' | '\t') {
            continue;
        }
        no_raw.push(ch);
    }

    caret_re.replace_all(&no_raw, "").into_owned()
}

pub fn sanitize_llm_output(input: &str) -> String {
    static GLITCH_AND_STOP_RE: OnceLock<Regex> = OnceLock::new();
    let re = GLITCH_AND_STOP_RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)",
            r"<\|[a-z0-9_/:.-]{1,32}\|?>?",
            r"|</?s>",
            r"|\[/?inst\]",
            r"|<</?sys>>",
            r"|\[baisuspil\]",
            r"|\)[a-z0-9_]{5,}\(",
            r"|\bbaisuspil\b",
            r"|\b(?:ke){2,}\b(?:[\s.!…]*\b(?:ke){2,}\b)*(?:\.{1,3}|…)?",
        ))
        .expect("valid llm output sanitizer regex")
    });

    let no_esc = strip_terminal_escapes(input);
    let no_emoji = sanitize_terminal_text(&no_esc);
    let cleaned = strip_matches_preserving_spacing(&no_emoji, re);
    cleaned.trim().to_string()
}

fn strip_matches_preserving_spacing(text: &str, re: &Regex) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;

    for m in re.find_iter(text) {
        out.push_str(&text[last..m.start()]);
        let after = &text[m.end()..];
        let next_is_space = after.starts_with(' ');
        let next_is_punct_or_end = after.is_empty()
            || after.starts_with(['\n', '\r', '.', ',', '!', '?', ';', ':']);
        if out.ends_with(' ') && (next_is_space || next_is_punct_or_end) {
            out.pop();
        }
        last = m.end();
    }

    out.push_str(&text[last..]);
    out
}

pub fn sanitize_terminal_text(input: &str) -> String {
    input
        .chars()
        .filter(|&ch| !is_prohibited_graphic_emoji(ch))
        .collect()
}

fn is_prohibited_graphic_emoji(ch: char) -> bool {
    let code = ch as u32;
    matches!(
        code,
        0xFE0F
        | 0x1F1E6..=0x1F1FF
        | 0x1F300..=0x1F64F
        | 0x1F680..=0x1F6FF
        | 0x1F900..=0x1F9FF
        | 0x1FA70..=0x1FAFF
        // Exclude pure-text kaomoji symbols: '☆'/'★' (0x2605-0x2606), '♪'/'♫'/'♬' (0x266A-0x266C), '⚆' (0x2686), '✧' (0x2727)
        | 0x2600..=0x2604
        | 0x2607..=0x2669
        | 0x266D..=0x2685
        | 0x2687..=0x26FF
        | 0x2700..=0x2726
        | 0x2728..=0x27BF
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_wire_protocol_and_endpoints() {
        let mut s = Settings::default();
        s.api.provider = "anthropic".to_string();
        s.api.base_url = "https://api.anthropic.com/v1".to_string();
        assert_eq!(WireProtocol::detect(&s), WireProtocol::Anthropic);
        assert_eq!(
            resolve_endpoint(&s, WireProtocol::Anthropic),
            "https://api.anthropic.com/v1/messages"
        );

        s.api.provider = "ollama".to_string();
        s.api.base_url = "http://localhost:11434".to_string();
        assert_eq!(WireProtocol::detect(&s), WireProtocol::OpenAiCompatible);
        assert_eq!(
            resolve_endpoint(&s, WireProtocol::OpenAiCompatible),
            "http://localhost:11434/v1/chat/completions"
        );

        s.api.provider = "vercel".to_string();
        s.api.base_url = "https://ai-gateway.vercel.sh/v1".to_string();
        assert_eq!(WireProtocol::detect(&s), WireProtocol::OpenAiCompatible);
        assert_eq!(
            resolve_endpoint(&s, WireProtocol::OpenAiCompatible),
            "https://ai-gateway.vercel.sh/v1/chat/completions"
        );

        s.api.provider = "gemini".to_string();
        s.api.base_url = "https://generativelanguage.googleapis.com/v1beta".to_string();
        assert_eq!(WireProtocol::detect(&s), WireProtocol::OpenAiCompatible);
        assert_eq!(
            resolve_endpoint(&s, WireProtocol::OpenAiCompatible),
            "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
        );
    }

    #[test]
    fn parses_openai_sse_chunks_and_tool_calls() {
        let mut parser = SseStreamParser::new(WireProtocol::OpenAiCompatible);
        let raw = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Halo \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"steve! ( ^ _ ^ )\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"run_shell_command\",\"arguments\":\"{\\\"cmd\\\":\\\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ls\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );

        let mid = raw.len() / 2;
        let mut out = parser.feed_bytes(&raw.as_bytes()[..mid]).unwrap();
        out.extend(parser.feed_bytes(&raw.as_bytes()[mid..]).unwrap());
        out.extend(parser.finish().unwrap());

        assert_eq!(out.concat(), "Halo steve! ( ^ _ ^ )");
        let tools = parser.into_tool_calls();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].id, "call_1");
        assert_eq!(tools[0].name, "run_shell_command");
        assert_eq!(tools[0].arguments, "{\"cmd\":\"ls\"}");
    }

    #[test]
    fn parses_anthropic_sse_chunks_and_tool_use() {
        let mut parser = SseStreamParser::new(WireProtocol::Anthropic);
        let raw = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Oki \"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"siap! ( •̀ ω •́ )✧\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"run_shell_command\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"uname -a\\\"}\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );

        let tokens = parser.feed_bytes(raw.as_bytes()).unwrap();
        assert_eq!(tokens.concat(), "Oki siap! ( •̀ ω •́ )✧");
        let tools = parser.into_tool_calls();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].id, "toolu_1");
        assert_eq!(tools[0].name, "run_shell_command");
        assert_eq!(tools[0].arguments, "{\"command\":\"uname -a\"}");
    }

    #[test]
    fn sanitizer_removes_color_emojis_but_keeps_kaomoji() {
        let sample = format!(
            "Halo {}{}{}! ( •̀ ω •́ )✧ ( > - < ) v (╥﹏╥) (｡•́︿•̀｡) ( ˘ ▽ ˘ ) ♫",
            '\u{1F338}', // cherry blossom
            '\u{1F680}', // rocket
            '\u{1F916}', // robot
        );
        let cleaned = sanitize_terminal_text(&sample);
        assert_eq!(
            cleaned,
            "Halo ! ( •̀ ω •́ )✧ ( > - < ) v (╥﹏╥) (｡•́︿•̀｡) ( ˘ ▽ ˘ ) ♫"
        );
    }

    #[test]
    fn stream_tool_call_filter_suppresses_pseudo_calls_but_keeps_normal_brackets() {
        let mut filter = StreamToolCallFilter::default();
        let mut visible = String::new();
        for chunk in [
            "Oki, bentar yaa [info] ",
            "[MEMANG",
            "GIL TOOLS: run_shell_command(command=\"yay -Syu\", requires_root=true)]",
            " siap!",
        ] {
            visible.push_str(&filter.push(chunk));
        }
        visible.push_str(&filter.finish());
        assert_eq!(visible, "Oki, bentar yaa [info]  siap!");
    }

    #[test]
    fn sanitizer_and_stream_filter_strip_glitch_stop_tokens_and_keke_while_keeping_kaomoji() {
        let raw = "Oke siap steve, langsung ku-update pake pacman yaa! ( ＾◡＾) ( ˘͈ ᵕ ˘͈ ) ( •̀ ω •́ )✧ ( ¬‿¬ ) keke... kekeke... [baisuspil] )baisuspil(<|im_end|></s>[INST]";
        let cleaned = sanitize_llm_output(raw);
        assert_eq!(
            cleaned,
            "Oke siap steve, langsung ku-update pake pacman yaa! ( ＾◡＾) ( ˘͈ ᵕ ˘͈ ) ( •̀ ω •́ )✧ ( ¬‿¬ )"
        );

        // Pastikan kata Indonesia normal (`kekerasan`, `oke`, `keamanan`) tidak ikut terpotong.
        let valid_words = "Oke steve! Keamanan sistem dan anti kekerasan digital itu penting ( •̀ ω •́ )✧";
        assert_eq!(sanitize_llm_output(valid_words), valid_words);

        // Streaming chunk-by-chunk juga harus menekan token glitch & stop-sequence secara real-time.
        let mut filter = StreamToolCallFilter::default();
        let mut streamed = String::new();
        for chunk in [
            "Oke siap steve! ",
            "( •̀ ω •́ )✧ ",
            "ke",
            "ke... ke",
            "keke... ",
            "Oke aman! ",
            "( ＾◡＾)",
            " )bais",
            "uspil(",
            "<|im_",
            "end|>",
            "[baisuspil]",
            "</s>",
        ] {
            streamed.push_str(&filter.push(chunk));
        }
        streamed.push_str(&filter.finish());
        assert_eq!(
            streamed.trim(),
            "Oke siap steve! ( •̀ ω •́ )✧ Oke aman! ( ＾◡＾)"
        );

        // Pastikan seluruh 82 kaomoji di palet KaomojiEngine lolos utuh tanpa berubah 1 byte pun.
        for spectrum in crate::engine::kaomoji::EmotionSpectrum::ALL {
            for &face in spectrum.palette() {
                assert_eq!(
                    sanitize_llm_output(face),
                    face,
                    "kaomoji must stay intact in sanitize_llm_output: {face}"
                );
                let mut f = StreamToolCallFilter::default();
                let mut s = f.push(face);
                s.push_str(&f.finish());
                assert_eq!(s, face, "kaomoji must stay intact in stream filter: {face}");
            }
        }

        // Pastikan karakter escape ANSI (`\x1b[D`, `\x1b[31m`) & caret-escape (`^[[D`, `^[OD`) dibersihkan
        // sementara kaomoji dengan caret `( ^ _ ^ )`, `( ￣^￣ )`, `( •̀ ᴗ •́ )و` tetap aman.
        let esc_sample = "Halo ^[[D\x1b[31msteve\x1b[0m^[OD! ( ^ _ ^ ) ( ￣^￣ ) ( •̀ ᴗ •́ )و <\x07|im_end";
        assert_eq!(
            sanitize_llm_output(esc_sample),
            "Halo steve! ( ^ _ ^ ) ( ￣^￣ ) ( •̀ ᴗ •́ )و"
        );

        let mut esc_filter = StreamToolCallFilter::default();
        let mut esc_streamed = String::new();
        for chunk in ["Halo ^", "[[D", "\x1b[32msteve\x1b[0m", "^[O", "D! ( ^ _ ^ )"] {
            esc_streamed.push_str(&esc_filter.push(chunk));
        }
        esc_streamed.push_str(&esc_filter.finish());
        assert_eq!(esc_streamed, "Halo steve! ( ^ _ ^ )");
    }

    #[tokio::test]
    async fn streams_from_local_mock_sse_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut req_buf = [0u8; 2048];
                let _ = socket.read(&mut req_buf).await;
                let response = concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Type: text/event-stream\r\n",
                    "Transfer-Encoding: chunked\r\n\r\n"
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let body = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"Beres, \"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"steve! ( > - < ) v\"}}]}\n\n",
                    "data: [DONE]\n\n"
                );
                let chunk = format!("{:X}\r\n{}\r\n0\r\n\r\n", body.len(), body);
                let _ = socket.write_all(chunk.as_bytes()).await;
            }
        });

        let mut settings = Settings::default();
        settings.api.provider = "openai".to_string();
        settings.api.api_key = "sk-test-1234567890".to_string();
        settings.api.base_url = format!("http://{addr}/v1");
        settings.api.model = "gpt-4o".to_string();

        let client = LlmClient::new(&settings).unwrap();
        let ctx = AssembledContext {
            system_prompt: "test".to_string(),
            messages: vec![ChatMessage::user("halo")],
        };

        let result = client.stream_chat(&settings, &ctx).await.unwrap();
        assert_eq!(result.text, "Beres, steve! ( > - < ) v");
    }

    #[tokio::test]
    async fn stream_chat_transparently_retries_after_http_429_cooldown() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            for attempt in 0..2 {
                if let Ok((mut socket, _)) = listener.accept().await {
                    let mut req_buf = [0u8; 4096];
                    let _ = socket.read(&mut req_buf).await;
                    if attempt == 0 {
                        let body = r#"{"error":{"message":"Rate limit exceeded. Retry after 1s."}}"#;
                        let resp = format!(
                            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = socket.write_all(resp.as_bytes()).await;
                    } else {
                        let header = concat!(
                            "HTTP/1.1 200 OK\r\n",
                            "Content-Type: text/event-stream\r\n",
                            "Transfer-Encoding: chunked\r\n\r\n"
                        );
                        let _ = socket.write_all(header.as_bytes()).await;
                        let body = concat!(
                            "data: {\"choices\":[{\"delta\":{\"content\":\"Lanjut setelah cooldown! ( •̀ ω •́ )✧\"}}]}\n\n",
                            "data: [DONE]\n\n"
                        );
                        let chunk = format!("{:X}\r\n{}\r\n0\r\n\r\n", body.len(), body);
                        let _ = socket.write_all(chunk.as_bytes()).await;
                    }
                }
            }
        });

        let mut settings = Settings::default();
        settings.api.provider = "custom".to_string();
        settings.api.api_key = "sk-test-1234567890".to_string();
        settings.api.base_url = format!("http://{addr}/v1");
        settings.api.model = "qwen-plus".to_string();

        let client = LlmClient::new(&settings).unwrap();
        let ctx = AssembledContext {
            system_prompt: "test".to_string(),
            messages: vec![ChatMessage::user("halo")],
        };

        let result = client
            .stream_chat(&settings, &ctx)
            .await
            .expect("stream_chat must recover transparently after 429 cooldown");
        assert_eq!(result.text, "Lanjut setelah cooldown! ( •̀ ω •́ )✧");
    }
}
