//! Dual-mode web search (`open_browser_search` GUI launcher and `quick_search` headless scraper).

use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::desktop;
use super::workspace_tracker::{self, SessionWorkspaceState};
use crate::config::settings::DesktopConfig;
use crate::engine::client::sanitize_terminal_text;

pub const DDG_HTML_URL: &str = "https://html.duckduckgo.com/html/";
pub const DDG_INSTANT_URL: &str = "https://api.duckduckgo.com/";
pub const GOOGLE_NEWS_RSS_URL: &str = "https://news.google.com/rss/search";
pub const WIKIPEDIA_API_URL: &str = "https://en.wikipedia.org/w/api.php";
pub const QUICK_SEARCH_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";
pub const NO_QUICK_RESULTS_MSG: &str = "Tidak ditemukan ringkasan cepat untuk query ini";
pub const DEFAULT_QUICK_SEARCH_LIMIT: usize = 3;
const QUICK_SEARCH_CONNECT_TIMEOUT_SECS: u64 = 3;
const QUICK_SEARCH_TIMEOUT_SECS: u64 = 5;

/// Arguments for `open_browser_search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenBrowserSearchArgs {
    pub query: String,
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub workspace: Option<i32>,
}

impl OpenBrowserSearchArgs {
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        let mut parsed: Self = serde_json::from_str(raw)
            .map_err(|e| format!("argumen JSON open_browser_search tidak valid: {e}"))?;
        parsed.query = parsed.query.trim().to_string();
        parsed.engine = parsed
            .engine
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| !e.is_empty());
        if parsed.query.is_empty() {
            return Err("field `query` tidak boleh kosong".to_string());
        }
        Ok(parsed)
    }
}

/// Arguments for `quick_search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickSearchArgs {
    pub query: String,
    #[serde(default)]
    pub max_results: Option<usize>,
}

impl QuickSearchArgs {
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        let mut parsed: Self = serde_json::from_str(raw)
            .map_err(|e| format!("argumen JSON quick_search tidak valid: {e}"))?;
        parsed.query = parsed.query.trim().to_string();
        if parsed.query.is_empty() {
            return Err("field `query` tidak boleh kosong".to_string());
        }
        Ok(parsed)
    }
}

/// Extracted snippet from a headless search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSnippet {
    pub title: String,
    pub snippet: String,
    pub url: String,
}

pub fn encode_uri_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    for byte in input.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push_str("%20"),
            b => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

pub fn decode_uri_component(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn build_search_url(query: &str, engine: Option<&str>) -> (String, &'static str) {
    let encoded = encode_uri_component(query.trim());
    match engine.unwrap_or("google").trim().to_ascii_lowercase().as_str() {
        "duckduckgo" | "ddg" => (format!("https://duckduckgo.com/?q={encoded}"), "duckduckgo"),
        "brave" => (
            format!("https://search.brave.com/search?q={encoded}"),
            "brave",
        ),
        "youtube" | "yt" => (
            format!("https://www.youtube.com/results?search_query={encoded}"),
            "youtube",
        ),
        _ => (
            format!("https://www.google.com/search?q={encoded}"),
            "google",
        ),
    }
}

pub async fn execute_open_browser_search_with_config(
    args: &OpenBrowserSearchArgs,
    ws_state: &SessionWorkspaceState,
    desktop_cfg: Option<&DesktopConfig>,
) -> crate::Result<String> {
    let target_ws =
        workspace_tracker::resolve_target_workspace_with_config(args.workspace, ws_state, desktop_cfg)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    let (url, engine_label) = build_search_url(&args.query, args.engine.as_deref());
    let report = desktop::launch_browser_in_workspace_with_config(&url, target_ws, desktop_cfg)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    let isolation_note = if report.used_new_window {
        "jendela baru terisolasi (--new-window)"
    } else {
        "peluncuran langsung"
    };

    Ok(format!(
        "status: success\n\
         engine: {engine_label}\n\
         workspace: {}\n\
         url: {url}\n\
         mode: {isolation_note}\n\
         command: {}",
        report.target_workspace, report.launch_command
    ))
}

pub fn extract_youtube_video_id_from_ddg_html(html: &str) -> Option<String> {
    static YT_WATCH_RE: OnceLock<Regex> = OnceLock::new();
    let re = YT_WATCH_RE.get_or_init(|| {
        Regex::new(
            r"(?i)(?:youtube\.com/watch\?(?:[^#\s&]*&)*v=|youtu\.be/|youtube\.com/shorts/)([a-zA-Z0-9_-]{11})",
        )
        .expect("valid ddg youtube watch regex")
    });

    for snippet in parse_ddg_html(html, 8) {
        if let Some(vid) = re
            .captures(&snippet.url)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
        {
            return Some(vid.to_string());
        }
    }

    re.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

pub async fn find_youtube_video_id_via_ddg(query: &str) -> Option<String> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(QUICK_SEARCH_CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(QUICK_SEARCH_TIMEOUT_SECS))
        .user_agent(QUICK_SEARCH_USER_AGENT)
        .build()
        .ok()?;

    let encoded = encode_uri_component(&format!("site:youtube.com/watch {}", query.trim()));
    let url = format!("{DDG_HTML_URL}?q={encoded}");
    let resp = client
        .get(&url)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.8")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    extract_youtube_video_id_from_ddg_html(&body)
}

pub async fn execute_quick_search(args: &QuickSearchArgs) -> crate::Result<String> {
    execute_quick_search_with_fallbacks(
        args,
        DDG_HTML_URL,
        DDG_INSTANT_URL,
        Some(GOOGLE_NEWS_RSS_URL),
        Some(WIKIPEDIA_API_URL),
    )
    .await
}

#[cfg(test)]
pub async fn execute_quick_search_with_endpoints(
    args: &QuickSearchArgs,
    html_base_url: &str,
    instant_base_url: &str,
) -> crate::Result<String> {
    execute_quick_search_with_fallbacks(args, html_base_url, instant_base_url, None, None).await
}

async fn execute_quick_search_with_fallbacks(
    args: &QuickSearchArgs,
    html_base_url: &str,
    instant_base_url: &str,
    news_rss_base_url: Option<&str>,
    wiki_api_base_url: Option<&str>,
) -> crate::Result<String> {
    let limit = args
        .max_results
        .unwrap_or(DEFAULT_QUICK_SEARCH_LIMIT)
        .clamp(1, 5);

    let client = Client::builder()
        .connect_timeout(Duration::from_secs(QUICK_SEARCH_CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(QUICK_SEARCH_TIMEOUT_SECS))
        .user_agent(QUICK_SEARCH_USER_AGENT)
        .build()
        .map_err(|e| format!("Gagal membangun HTTP client quick_search: {e}"))?;

    let encoded = encode_uri_component(&args.query);
    let html_url = format!("{html_base_url}?q={encoded}");

    let mut snippets = Vec::new();
    let mut source_label = "DuckDuckGo HTML";

    if let Ok(resp) = client
        .get(&html_url)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.7,id;q=0.5")
        .send()
        .await
    {
        if resp.status().is_success() {
            if let Ok(body) = resp.text().await {
                snippets = parse_ddg_html(&body, limit);
            }
        }
    }

    if snippets.is_empty() {
        let instant_url =
            format!("{instant_base_url}?q={encoded}&format=json&no_html=1&skip_disambig=1");
        if let Ok(resp) = client.get(&instant_url).send().await {
            if resp.status().is_success() {
                if let Ok(body) = resp.text().await {
                    snippets = parse_ddg_instant_json(&body, limit);
                    if !snippets.is_empty() {
                        source_label = "DuckDuckGo Instant Answer";
                    }
                }
            }
        }
    }

    if snippets.is_empty() {
        let prefers_news = is_news_or_recency_query(&args.query);
        if prefers_news {
            if let Some(rss_base) = news_rss_base_url {
                snippets = fetch_google_news_rss(&client, rss_base, &encoded, limit).await;
                if !snippets.is_empty() {
                    source_label = "Google News RSS";
                }
            }
            if snippets.is_empty() {
                if let Some(wiki_base) = wiki_api_base_url {
                    snippets = fetch_wikipedia_search(&client, wiki_base, &encoded, limit).await;
                    if !snippets.is_empty() {
                        source_label = "Wikipedia Search";
                    }
                }
            }
        } else {
            if let Some(wiki_base) = wiki_api_base_url {
                snippets = fetch_wikipedia_search(&client, wiki_base, &encoded, limit).await;
                if !snippets.is_empty() {
                    source_label = "Wikipedia Search";
                }
            }
            if snippets.is_empty() {
                if let Some(rss_base) = news_rss_base_url {
                    snippets = fetch_google_news_rss(&client, rss_base, &encoded, limit).await;
                    if !snippets.is_empty() {
                        source_label = "Google News RSS";
                    }
                }
            }
        }
    }

    if snippets.is_empty() {
        return Ok(format!(
            "status: no_results\n\
             query: {}\n\
             message: {NO_QUICK_RESULTS_MSG}",
            args.query
        ));
    }

    Ok(format_snippets_for_llm(&args.query, source_label, &snippets))
}

fn is_news_or_recency_query(query: &str) -> bool {
    let low = query.to_ascii_lowercase();
    [
        "news", "latest", "berita", "terbaru", "update", "release", "rilis", "today", "hari ini",
        "2025", "2026", "2027",
    ]
    .iter()
    .any(|kw| low.contains(kw))
}

async fn fetch_google_news_rss(
    client: &Client,
    rss_base_url: &str,
    encoded_query: &str,
    limit: usize,
) -> Vec<SearchSnippet> {
    let url = format!("{rss_base_url}?q={encoded_query}&hl=en-US&gl=US&ceid=US:en");
    let Ok(resp) = client
        .get(&url)
        .header("User-Agent", "waish/0.1 (RSS reader)")
        .header("Accept", "application/rss+xml, application/xml, text/xml")
        .send()
        .await
    else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(body) = resp.text().await else {
        return Vec::new();
    };
    parse_google_news_rss(&body, limit)
}

async fn fetch_wikipedia_search(
    client: &Client,
    wiki_base_url: &str,
    encoded_query: &str,
    limit: usize,
) -> Vec<SearchSnippet> {
    let url = format!(
        "{wiki_base_url}?action=query&list=search&srsearch={encoded_query}&srlimit={limit}&format=json"
    );
    let Ok(resp) = client.get(&url).send().await else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(body) = resp.text().await else {
        return Vec::new();
    };
    parse_wikipedia_search_json(&body, limit)
}

pub fn parse_ddg_html(html: &str, limit: usize) -> Vec<SearchSnippet> {
    static DDG_HTML_A_RE: OnceLock<Regex> = OnceLock::new();
    static DDG_HTML_TITLE_RE: OnceLock<Regex> = OnceLock::new();
    static DDG_HTML_SNIPPET_RE: OnceLock<Regex> = OnceLock::new();

    let a_re = DDG_HTML_A_RE.get_or_init(|| {
        Regex::new(
            r#"(?is)<a[^>]*class=['"][^'"]*result__a[^'"]*['"][^>]*href=['"]([^'"]+)['"][^>]*>(.*?)</a>|<a[^>]*href=['"]([^'"]+)['"][^>]*class=['"][^'"]*result__a[^'"]*['"][^>]*>(.*?)</a>"#,
        )
        .expect("valid ddg html result__a regex")
    });
    let title_block_re = DDG_HTML_TITLE_RE.get_or_init(|| {
        Regex::new(
            r#"(?is)<[^>]*class=['"][^'"]*result__title[^'"]*['"][^>]*>.*?<a[^>]*href=['"]([^'"]+)['"][^>]*>(.*?)</a>"#,
        )
        .expect("valid ddg html result__title regex")
    });
    let snippet_re = DDG_HTML_SNIPPET_RE.get_or_init(|| {
        Regex::new(
            r#"(?is)<(?:a|div|span|td)[^>]*class=['"][^'"]*result__snippet[^'"]*['"][^>]*>(.*?)</(?:a|div|span|td)>"#,
        )
        .expect("valid ddg html result__snippet regex")
    });

    let mut links: Vec<(String, String)> = a_re
        .captures_iter(html)
        .filter_map(|cap| {
            let (href, raw_title) = match (cap.get(1), cap.get(2)) {
                (Some(h), Some(t)) => (h.as_str(), t.as_str()),
                _ => (cap.get(3)?.as_str(), cap.get(4)?.as_str()),
            };
            let url = unwrap_ddg_redirect(href);
            let title = clean_html_text(raw_title);
            if title.is_empty() || url.is_empty() {
                None
            } else {
                Some((title, url))
            }
        })
        .collect();

    if links.is_empty() {
        links = title_block_re
            .captures_iter(html)
            .filter_map(|cap| {
                let url = unwrap_ddg_redirect(cap.get(1)?.as_str());
                let title = clean_html_text(cap.get(2)?.as_str());
                if title.is_empty() || url.is_empty() {
                    None
                } else {
                    Some((title, url))
                }
            })
            .collect();
    }

    if !links.is_empty() {
        let desc_list: Vec<String> = snippet_re
            .captures_iter(html)
            .filter_map(|cap| cap.get(1).map(|m| clean_html_text(m.as_str())))
            .collect();

        return links
            .into_iter()
            .take(limit)
            .enumerate()
            .map(|(idx, (title, url))| SearchSnippet {
                title,
                snippet: desc_list.get(idx).cloned().unwrap_or_default(),
                url,
            })
            .collect();
    }

    parse_ddg_lite_html(html, limit)
}

pub fn parse_ddg_lite_html(html: &str, limit: usize) -> Vec<SearchSnippet> {
    static LINK_RE: OnceLock<Regex> = OnceLock::new();
    static SNIPPET_RE: OnceLock<Regex> = OnceLock::new();

    let link_re = LINK_RE.get_or_init(|| {
        Regex::new(
            r#"(?is)<a[^>]*class=['"][^'"]*result-link[^'"]*['"][^>]*href=['"]([^'"]+)['"][^>]*>(.*?)</a>|<a[^>]*href=['"]([^'"]+)['"][^>]*class=['"][^'"]*result-link[^'"]*['"][^>]*>(.*?)</a>"#,
        )
        .expect("valid ddg link regex")
    });
    let snippet_re = SNIPPET_RE.get_or_init(|| {
        Regex::new(r#"(?is)<td[^>]*class=['"][^'"]*result-snippet[^'"]*['"][^>]*>(.*?)</td>"#)
            .expect("valid ddg snippet regex")
    });

    let links: Vec<(String, String)> = link_re
        .captures_iter(html)
        .filter_map(|cap| {
            let (href, raw_title) = match (cap.get(1), cap.get(2)) {
                (Some(h), Some(t)) => (h.as_str(), t.as_str()),
                _ => (cap.get(3)?.as_str(), cap.get(4)?.as_str()),
            };
            let url = unwrap_ddg_redirect(href);
            let title = clean_html_text(raw_title);
            if title.is_empty() || url.is_empty() {
                None
            } else {
                Some((title, url))
            }
        })
        .collect();

    let desc_list: Vec<String> = snippet_re
        .captures_iter(html)
        .filter_map(|cap| cap.get(1).map(|m| clean_html_text(m.as_str())))
        .collect();

    links
        .into_iter()
        .take(limit)
        .enumerate()
        .map(|(idx, (title, url))| SearchSnippet {
            title,
            snippet: desc_list.get(idx).cloned().unwrap_or_default(),
            url,
        })
        .collect()
}

pub fn parse_google_news_rss(xml: &str, limit: usize) -> Vec<SearchSnippet> {
    static ITEM_RE: OnceLock<Regex> = OnceLock::new();
    static TITLE_RE: OnceLock<Regex> = OnceLock::new();
    static LINK_RE: OnceLock<Regex> = OnceLock::new();
    static DATE_RE: OnceLock<Regex> = OnceLock::new();

    let item_re = ITEM_RE.get_or_init(|| Regex::new(r"(?is)<item>(.*?)</item>").expect("valid rss item regex"));
    let title_re = TITLE_RE.get_or_init(|| Regex::new(r"(?is)<title>(.*?)</title>").expect("valid rss title regex"));
    let link_re = LINK_RE.get_or_init(|| Regex::new(r"(?is)<link>(.*?)</link>").expect("valid rss link regex"));
    let date_re = DATE_RE.get_or_init(|| Regex::new(r"(?is)<pubDate>(.*?)</pubDate>").expect("valid rss date regex"));

    item_re
        .captures_iter(xml)
        .take(limit)
        .filter_map(|cap| {
            let block = cap.get(1)?.as_str();
            let raw_title = title_re.captures(block)?.get(1)?.as_str();
            let title = clean_html_text(
                raw_title
                    .trim()
                    .strip_prefix("<![CDATA[")
                    .and_then(|s| s.strip_suffix("]]>"))
                    .unwrap_or(raw_title),
            );
            if title.is_empty() {
                return None;
            }
            let url = link_re
                .captures(block)
                .and_then(|c| c.get(1))
                .map(|m| decode_html_entities(m.as_str().trim()))
                .unwrap_or_default();
            let date = date_re
                .captures(block)
                .and_then(|c| c.get(1))
                .map(|m| clean_html_text(m.as_str()))
                .unwrap_or_default();
            let snippet = if date.is_empty() {
                title.clone()
            } else {
                format!("Terbit: {date}")
            };
            Some(SearchSnippet { title, snippet, url })
        })
        .collect()
}

pub fn parse_wikipedia_search_json(raw_json: &str, limit: usize) -> Vec<SearchSnippet> {
    let Ok(val) = serde_json::from_str::<Value>(raw_json) else {
        return Vec::new();
    };
    let Some(items) = val
        .get("query")
        .and_then(|q| q.get("search"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    items
        .iter()
        .take(limit)
        .filter_map(|item| {
            let title = sanitize_terminal_text(item.get("title")?.as_str()?.trim());
            if title.is_empty() {
                return None;
            }
            let raw_snip = item
                .get("snippet")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let snippet = clean_html_text(raw_snip);
            let url = format!(
                "https://en.wikipedia.org/wiki/{}",
                encode_uri_component(&title.replace(' ', "_"))
            );
            Some(SearchSnippet { title, snippet, url })
        })
        .collect()
}

pub fn parse_ddg_instant_json(raw_json: &str, limit: usize) -> Vec<SearchSnippet> {
    let Ok(val) = serde_json::from_str::<Value>(raw_json) else {
        return Vec::new();
    };
    let mut out = Vec::new();

    let heading = val
        .get("Heading")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let abstract_text = val
        .get("AbstractText")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let abstract_url = val
        .get("AbstractURL")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();

    if !abstract_text.is_empty() {
        out.push(SearchSnippet {
            title: sanitize_terminal_text(if heading.is_empty() { "Ringkasan" } else { heading }),
            snippet: sanitize_terminal_text(abstract_text),
            url: abstract_url.to_string(),
        });
    }

    if let Some(topics) = val.get("RelatedTopics").and_then(Value::as_array) {
        for item in topics {
            if out.len() >= limit {
                break;
            }
            if let (Some(text), Some(url)) = (
                item.get("Text").and_then(Value::as_str),
                item.get("FirstURL").and_then(Value::as_str),
            ) {
                let cleaned = sanitize_terminal_text(text.trim());
                if !cleaned.is_empty() {
                    let title = cleaned
                        .split(" - ")
                        .next()
                        .unwrap_or(&cleaned)
                        .chars()
                        .take(80)
                        .collect();
                    out.push(SearchSnippet {
                        title,
                        snippet: cleaned,
                        url: url.trim().to_string(),
                    });
                }
            }
        }
    }

    out.truncate(limit);
    out
}

fn unwrap_ddg_redirect(href: &str) -> String {
    let decoded_href = decode_html_entities(href.trim());
    if let Some(idx) = decoded_href.find("uddg=") {
        let rest = &decoded_href[idx + 5..];
        let end = rest.find('&').unwrap_or(rest.len());
        return decode_uri_component(&rest[..end]);
    }
    if decoded_href.starts_with("//") {
        format!("https:{decoded_href}")
    } else {
        decoded_href
    }
}

fn clean_html_text(raw: &str) -> String {
    static TAG_RE: OnceLock<Regex> = OnceLock::new();
    let tag_re = TAG_RE.get_or_init(|| Regex::new(r"(?s)<[^>]+>").expect("valid tag regex"));
    let stripped = tag_re.replace_all(raw, " ");
    let decoded = decode_html_entities(&stripped);
    let compact: String = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    sanitize_terminal_text(&compact)
}

fn decode_html_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
}

fn format_snippets_for_llm(query: &str, source: &str, snippets: &[SearchSnippet]) -> String {
    let mut lines = vec![
        "status: success".to_string(),
        format!("source: {source}"),
        format!("query: {query}"),
        format!("results ({}):", snippets.len()),
    ];
    for (i, item) in snippets.iter().enumerate() {
        lines.push(format!("{}. {}", i + 1, item.title));
        if !item.snippet.is_empty() {
            lines.push(format!("   Ringkasan: {}", item.snippet));
        }
        if !item.url.is_empty() {
            lines.push(format!("   URL: {}", item.url));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_search_urls_with_proper_encoding_for_all_engines() {
        let (g_url, g_eng) = build_search_url("hyprland keybind guide", None);
        assert_eq!(g_eng, "google");
        assert_eq!(
            g_url,
            "https://www.google.com/search?q=hyprland%20keybind%20guide"
        );

        let (d_url, d_eng) = build_search_url("rust tokio mpsc", Some("duckduckgo"));
        assert_eq!(d_eng, "duckduckgo");
        assert_eq!(d_url, "https://duckduckgo.com/?q=rust%20tokio%20mpsc");

        let (b_url, b_eng) = build_search_url("arch wiki pipewire", Some("brave"));
        assert_eq!(b_eng, "brave");
        assert_eq!(
            b_url,
            "https://search.brave.com/search?q=arch%20wiki%20pipewire"
        );

        let (y_url, y_eng) = build_search_url("yoasobi idol", Some("youtube"));
        assert_eq!(y_eng, "youtube");
        assert_eq!(
            y_url,
            "https://www.youtube.com/results?search_query=yoasobi%20idol"
        );
    }

    #[test]
    fn parses_duckduckgo_lite_html_and_unwraps_redirects() {
        let html = r#"
        <table>
          <tr><td><a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwiki.hyprland.org%2FConfiguring%2FDispatchers%2F&amp;rut=abc" class='result-link'>Dispatchers &amp; IPC - Hyprland Wiki</a></td></tr>
          <tr><td class='result-snippet'>Complete reference for <b>hyprctl dispatch</b> commands and window rules.</td></tr>
          <tr><td><a class="result-link" href="https://wiki.archlinux.org/title/PipeWire">PipeWire - ArchWiki</a></td></tr>
          <tr><td class="result-snippet">PipeWire is a low-latency multimedia server with wpctl CLI.</td></tr>
        </table>
        "#;

        let snippets = parse_ddg_lite_html(html, 3);
        assert_eq!(snippets.len(), 2);
        assert_eq!(snippets[0].title, "Dispatchers & IPC - Hyprland Wiki");
        assert_eq!(
            snippets[0].url,
            "https://wiki.hyprland.org/Configuring/Dispatchers/"
        );
        assert!(snippets[0].snippet.contains("hyprctl dispatch"));
        assert_eq!(snippets[1].title, "PipeWire - ArchWiki");
    }

    #[tokio::test]
    async fn quick_search_fetches_and_parses_from_local_mock_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let body = concat!(
                    "<html><body>",
                    "<a class='result-link' href='https://doc.rust-lang.org/std/sync/atomic/'>std::sync::atomic - Rust</a>",
                    "<td class='result-snippet'>Atomic types provide primitive shared-memory communication.</td>",
                    "</body></html>"
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let args = QuickSearchArgs {
            query: "rust atomic".to_string(),
            max_results: Some(3),
        };
        let lite_ep = format!("http://{addr}/lite/");
        let inst_ep = format!("http://{addr}/instant/");
        let out = execute_quick_search_with_endpoints(&args, &lite_ep, &inst_ep)
            .await
            .unwrap();
        assert!(out.contains("status: success"));
        assert!(out.contains("std::sync::atomic - Rust"));
        assert!(out.contains("shared-memory communication"));
    }

    #[test]
    fn parses_duckduckgo_html_result_title_snippet_and_rss_wiki_fallbacks() {
        let ddg_html = r#"
        <div class="result results_links">
          <h2 class="result__title">
            <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Farchlinux.org%2Fnews%2F&amp;rut=1">Arch Linux - News</a>
          </h2>
          <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Farchlinux.org%2Fnews%2F">Latest announcements for <b>Arch Linux</b> packages and kernel updates.</a>
        </div>
        "#;
        let parsed = parse_ddg_html(ddg_html, 3);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].title, "Arch Linux - News");
        assert_eq!(parsed[0].url, "https://archlinux.org/news/");
        assert!(parsed[0].snippet.contains("Latest announcements for Arch Linux"));

        let rss_xml = r#"
        <rss><channel>
          <item>
            <title>Arch Linux Kernel 6.18 Update</title>
            <link>https://news.google.com/rss/articles/abc</link>
            <pubDate>Thu, 19 Nov 2026 10:00:00 GMT</pubDate>
          </item>
        </channel></rss>
        "#;
        let rss_res = parse_google_news_rss(rss_xml, 3);
        assert_eq!(rss_res.len(), 1);
        assert_eq!(rss_res[0].title, "Arch Linux Kernel 6.18 Update");
        assert!(rss_res[0].snippet.contains("19 Nov 2026"));

        let wiki_json = r#"{"query":{"search":[{"title":"Arch Linux","snippet":"<span class=\"searchmatch\">Arch</span> Linux is a rolling release Linux distro."}]}}"#;
        let wiki_res = parse_wikipedia_search_json(wiki_json, 3);
        assert_eq!(wiki_res.len(), 1);
        assert_eq!(wiki_res[0].title, "Arch Linux");
        assert!(wiki_res[0].snippet.contains("rolling release"));
        assert_eq!(wiki_res[0].url, "https://en.wikipedia.org/wiki/Arch_Linux");
    }

    #[tokio::test]
    async fn quick_search_returns_clear_fallback_message_when_empty() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            for _ in 0..2 {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let body = "<html><body>empty</body></html>";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            }
        });

        let args = QuickSearchArgs {
            query: "zxqvbnm nonexistent".to_string(),
            max_results: Some(3),
        };
        let ep = format!("http://{addr}/");
        let out = execute_quick_search_with_endpoints(&args, &ep, &ep)
            .await
            .unwrap();
        assert!(out.contains("status: no_results"));
        assert!(out.contains(NO_QUICK_RESULTS_MSG));
    }

    #[test]
    fn extracts_youtube_video_id_from_ddg_html_results() {
        let ddg_html = r#"
        <div class="result">
          <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3DQdBZY2fkU-0&amp;rut=1">
            Grand Theft Auto VI Trailer 1 - YouTube
          </a>
        </div>
        "#;
        assert_eq!(
            extract_youtube_video_id_from_ddg_html(ddg_html).as_deref(),
            Some("QdBZY2fkU-0")
        );
    }
}
