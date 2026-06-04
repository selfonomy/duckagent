use crate::capabilities::builtins::web_extract::WebExtractArgs;
use crate::capabilities::builtins::web_search::{WebSearchArgs, WebSearchFreshness};
use crate::client::ModelClient;
use crate::mcp::config::{DuckAgentConfig, McpServerConfig, McpTransportKind};
use crate::mcp::result::map_mcp_tool_result;
use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_TYPE, USER_AGENT};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use url::Url;

const EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";
const DEFAULT_SEARCH_PROVIDER: WebSearchProviderKind = WebSearchProviderKind::Exa;
const DEFAULT_EXTRACT_PROVIDER: WebExtractProviderKind = WebExtractProviderKind::Local;
const DEFAULT_BROWSER_FALLBACK: BrowserFallbackMode = BrowserFallbackMode::Auto;
const HTTP_TIMEOUT_SECONDS: u64 = 30;
const MAX_HTTP_BYTES: usize = 2_000_000;
const MIN_USEFUL_CHARS: usize = 500;
const LARGE_HTML_CHARS: usize = 5_000;
const MAX_EXTRACT_CHARS: usize = 50_000;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchProviderKind {
    Exa,
    Disabled,
}

impl Default for WebSearchProviderKind {
    fn default() -> Self {
        DEFAULT_SEARCH_PROVIDER
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebExtractProviderKind {
    Local,
    Exa,
    Disabled,
}

impl Default for WebExtractProviderKind {
    fn default() -> Self {
        DEFAULT_EXTRACT_PROVIDER
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserFallbackMode {
    Auto,
    Off,
}

impl Default for BrowserFallbackMode {
    fn default() -> Self {
        DEFAULT_BROWSER_FALLBACK
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub search: WebSearchConfig,
    pub extract: WebExtractConfig,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            search: WebSearchConfig::default(),
            extract: WebExtractConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchConfig {
    pub provider: WebSearchProviderKind,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: DEFAULT_SEARCH_PROVIDER,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WebExtractConfig {
    pub provider: WebExtractProviderKind,
    pub browser_fallback: BrowserFallbackMode,
}

impl Default for WebExtractConfig {
    fn default() -> Self {
        Self {
            provider: DEFAULT_EXTRACT_PROVIDER,
            browser_fallback: DEFAULT_BROWSER_FALLBACK,
        }
    }
}

pub fn load_web_config() -> Result<WebConfig> {
    DuckAgentConfig::load_active_profile()?.web_config()
}

pub fn save_web_config(web: WebConfig) -> Result<()> {
    let mut config = DuckAgentConfig::load_active_profile()?;
    config.set_web_config(web)?;
    config.save_active_profile()
}

pub fn search(input: WebSearchArgs) -> Result<String> {
    let config = load_web_config().unwrap_or_default();
    match config.search.provider {
        WebSearchProviderKind::Exa => ExaMcpProvider.search(input),
        WebSearchProviderKind::Disabled => Ok(json!({
            "status": "unavailable",
            "capability": "web_search",
            "message": "Web Search is disabled in duckagent web config."
        })
        .to_string()),
    }
}

pub fn extract(input: WebExtractArgs, client: &ModelClient) -> Result<String> {
    let config = load_web_config().unwrap_or_default();
    match config.extract.provider {
        WebExtractProviderKind::Local => {
            LocalExtractProvider::new(config.extract).extract(input, client)
        }
        WebExtractProviderKind::Exa => ExaMcpProvider.extract(input, client),
        WebExtractProviderKind::Disabled => Ok(json!({
            "status": "unavailable",
            "capability": "web_extract",
            "message": "Web Extract is disabled in duckagent web config."
        })
        .to_string()),
    }
}

struct ExaMcpProvider;

impl ExaMcpProvider {
    fn search(&self, input: WebSearchArgs) -> Result<String> {
        let limit = input.limit.clamp(1, 20);
        let query = input.query.clone();
        let mut args = json!({
            "query": input.query,
            "numResults": limit,
            "livecrawl": freshness_to_livecrawl(&input.freshness),
            "contextMaxCharacters": 10_000,
        });
        if !input.include_domains.is_empty() {
            args["includeDomains"] = json!(input.include_domains);
        }
        if !input.exclude_domains.is_empty() {
            args["excludeDomains"] = json!(input.exclude_domains);
        }
        let text = call_exa_mcp_tool("web_search_exa", args)?;
        let normalized = normalize_search_output("exa", &query, &text, limit);
        Ok(serde_json::to_string_pretty(&normalized)?)
    }

    fn extract(&self, input: WebExtractArgs, client: &ModelClient) -> Result<String> {
        if input.depth > 0 {
            return Ok(json!({
                "status": "error",
                "provider": "exa",
                "error_code": "depth_not_supported",
                "message": "web_extract depth > 0 is not supported by the Exa v1 provider."
            })
            .to_string());
        }
        let max_chars = input.max_chars.clamp(1_000, MAX_EXTRACT_CHARS);
        let urls = input.urls.clone();
        let args = json!({
            "urls": input.urls,
            "maxCharacters": max_chars,
        });
        let text = call_exa_mcp_tool("web_fetch_exa", args)?;
        let compressed = maybe_compress_text(&text, &input.purpose, max_chars, client);
        let sources = urls
            .into_iter()
            .map(|url| {
                json!({
                    "url": url,
                    "title": null,
                    "content": compressed,
                })
            })
            .collect::<Vec<_>>();
        Ok(serde_json::to_string_pretty(&json!({
            "status": "ok",
            "provider": "exa",
            "used_browser": false,
            "sources": sources,
            "warnings": [],
        }))?)
    }
}

fn call_exa_mcp_tool(tool: &str, args: Value) -> Result<String> {
    let mut headers = BTreeMap::new();
    if let Some(api_key) = crate::auth::resolve_web_api_key("exa") {
        headers.insert("x-api-key".to_string(), api_key);
    }
    let config = McpServerConfig {
        transport: Some(McpTransportKind::Http),
        url: Some(EXA_MCP_URL.to_string()),
        headers,
        timeout: Some(30_000),
        ..Default::default()
    };
    let value = crate::mcp::transport_http::call_tool("exa_internal", &config, tool, args, None)
        .with_context(|| format!("failed to call internal Exa MCP tool `{tool}`"))?;
    Ok(map_mcp_tool_result(value))
}

fn freshness_to_livecrawl(freshness: &WebSearchFreshness) -> &'static str {
    match freshness {
        WebSearchFreshness::Any => "fallback",
        WebSearchFreshness::Day
        | WebSearchFreshness::Week
        | WebSearchFreshness::Month
        | WebSearchFreshness::Year => "preferred",
    }
}

struct LocalExtractProvider {
    config: WebExtractConfig,
}

impl LocalExtractProvider {
    fn new(config: WebExtractConfig) -> Self {
        Self { config }
    }

    fn extract(&self, input: WebExtractArgs, client: &ModelClient) -> Result<String> {
        if input.depth > 0 {
            return Ok(json!({
                "status": "error",
                "provider": "local",
                "error_code": "depth_not_supported",
                "message": "Local web_extract v1 supports depth=0 only."
            })
            .to_string());
        }
        let max_chars = input.max_chars.clamp(1_000, MAX_EXTRACT_CHARS);
        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut used_browser = false;
        let mut any_error = false;

        for url in input.urls {
            match extract_one_url(&url, max_chars, &self.config, client, &input.purpose) {
                Ok(page) => {
                    used_browser |= page.used_browser;
                    warnings.extend(page.warnings.clone());
                    sources.push(json!({
                        "url": page.url,
                        "title": page.title,
                        "content": page.content,
                        "used_browser": page.used_browser,
                        "fallback_reason": page.fallback_reason,
                    }));
                }
                Err(error) => {
                    any_error = true;
                    warnings.push(format!("{url}: {error:#}"));
                    sources.push(json!({
                        "url": url,
                        "title": null,
                        "content": "",
                        "error": format!("{error:#}"),
                    }));
                }
            }
        }

        let status =
            if sources.is_empty() || (any_error && sources.iter().all(source_content_empty)) {
                "error"
            } else if any_error || !warnings.is_empty() {
                "partial"
            } else {
                "ok"
            };
        Ok(serde_json::to_string_pretty(&json!({
            "status": status,
            "provider": "local",
            "used_browser": used_browser,
            "sources": sources,
            "warnings": warnings,
        }))?)
    }
}

fn source_content_empty(value: &Value) -> bool {
    value
        .get("content")
        .and_then(Value::as_str)
        .is_none_or(|text| text.trim().is_empty())
}

struct ExtractedPage {
    url: String,
    title: Option<String>,
    content: String,
    used_browser: bool,
    fallback_reason: Option<String>,
    warnings: Vec<String>,
}

fn extract_one_url(
    url: &str,
    max_chars: usize,
    config: &WebExtractConfig,
    client: &ModelClient,
    purpose: &str,
) -> Result<ExtractedPage> {
    let parsed = Url::parse(url).with_context(|| format!("invalid URL: {url}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("web_extract only supports http and https URLs");
    }

    let http_result = fetch_http(url);
    let mut warnings = Vec::new();
    let mut candidate = match http_result {
        Ok(response) => {
            if !is_html_content_type(&response.content_type) {
                let content = truncate_chars(&response.body, max_chars);
                return Ok(ExtractedPage {
                    url: url.to_string(),
                    title: None,
                    content,
                    used_browser: false,
                    fallback_reason: None,
                    warnings,
                });
            }
            let extracted = extract_html(&response.body, Some(url));
            HtmlCandidate {
                status: Some(response.status),
                html: response.body,
                extracted,
                error: None,
            }
        }
        Err(error) => HtmlCandidate {
            status: None,
            html: String::new(),
            extracted: HtmlExtract::default(),
            error: Some(error.to_string()),
        },
    };

    let fallback_reason = browser_fallback_reason(&candidate);
    let mut used_browser = false;
    if matches!(config.browser_fallback, BrowserFallbackMode::Auto) && fallback_reason.is_some() {
        match render_with_local_browser(url) {
            Ok(rendered_html) => {
                let rendered = extract_html(&rendered_html, Some(url));
                if rendered.content_chars() >= candidate.extracted.content_chars() {
                    used_browser = true;
                    candidate = HtmlCandidate {
                        status: candidate.status,
                        html: rendered_html,
                        extracted: rendered,
                        error: None,
                    };
                } else {
                    warnings.push("Browser fallback returned less content than HTTP extraction; kept HTTP content.".to_string());
                }
            }
            Err(error) => {
                let message = format!("Browser fallback failed: {error:#}");
                if candidate.extracted.content_chars() > 0 {
                    warnings.push(message);
                } else {
                    return Ok(ExtractedPage {
                        url: url.to_string(),
                        title: None,
                        content: String::new(),
                        used_browser: false,
                        fallback_reason,
                        warnings: vec![format!(
                            "browser_required_but_unavailable: {message}. Install Chrome/Chromium or switch Web Extract provider."
                        )],
                    });
                }
            }
        }
    }

    if let Some(error) = candidate.error.as_ref() {
        warnings.push(format!("HTTP fetch failed: {error}"));
    }
    let mut content = candidate.extracted.content;
    if content.trim().is_empty() {
        bail!("no readable content extracted");
    }
    content = maybe_compress_text(&content, purpose, max_chars, client);
    Ok(ExtractedPage {
        url: url.to_string(),
        title: candidate.extracted.title,
        content,
        used_browser,
        fallback_reason,
        warnings,
    })
}

struct HttpResponse {
    status: u16,
    content_type: String,
    body: String,
}

fn fetch_http(url: &str) -> Result<HttpResponse> {
    let client = Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECONDS))
        .redirect(reqwest::redirect::Policy::limited(8))
        .build()
        .context("failed to build web extract HTTP client")?;
    let response = client
        .get(url)
        .header(USER_AGENT, user_agent())
        .send()
        .with_context(|| format!("failed GET {url}"))?;
    let status = response.status().as_u16();
    if matches!(status, 404 | 410) {
        bail!("HTTP {status}");
    }
    let headers = response.headers().clone();
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let mut body = response.text().context("failed to read response body")?;
    if body.len() > MAX_HTTP_BYTES {
        body.truncate(MAX_HTTP_BYTES);
    }
    Ok(HttpResponse {
        status,
        content_type,
        body,
    })
}

fn is_html_content_type(content_type: &str) -> bool {
    let lower = content_type.to_ascii_lowercase();
    lower.is_empty() || lower.contains("text/html") || lower.contains("application/xhtml")
}

fn user_agent() -> &'static str {
    "duckagent/0.1 web_extract"
}

#[derive(Debug)]
struct HtmlCandidate {
    status: Option<u16>,
    html: String,
    extracted: HtmlExtract,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct HtmlExtract {
    title: Option<String>,
    content: String,
    paragraph_count: usize,
    link_text_ratio: f32,
    js_hint_count: usize,
}

impl HtmlExtract {
    fn content_chars(&self) -> usize {
        self.content.chars().count()
    }
}

fn extract_html(html: &str, base_url: Option<&str>) -> HtmlExtract {
    let title = capture_tag_text(html, "title");
    let js_hint_count = js_hint_count(html);
    let without_scripts = remove_tag_blocks(html, "script");
    let without_styles = remove_tag_blocks(&without_scripts, "style");
    let paragraph_count = count_tag(&without_styles, "p");
    let link_text = collect_tag_text(&without_styles, "a");
    let text = html_to_text(&without_styles);
    let content = if let Some(title) = title.as_ref().filter(|value| !value.trim().is_empty()) {
        format!("# {}\n\n{}", title.trim(), text.trim())
    } else {
        text.trim().to_string()
    };
    let link_text_ratio = if content.is_empty() {
        0.0
    } else {
        link_text.chars().count() as f32 / content.chars().count() as f32
    };
    let content = normalize_relative_links(&content, base_url);
    HtmlExtract {
        title,
        content,
        paragraph_count,
        link_text_ratio,
        js_hint_count,
    }
}

fn browser_fallback_reason(candidate: &HtmlCandidate) -> Option<String> {
    if let Some(status) = candidate.status
        && matches!(status, 403 | 429 | 503)
    {
        return Some(format!("http_status_{status}"));
    }
    if candidate
        .error
        .as_ref()
        .is_some_and(|error| is_timeout_like(error))
    {
        return Some("http_timeout_or_transport_error".to_string());
    }
    let extracted_chars = candidate.extracted.content_chars();
    let html_chars = candidate.html.chars().count();
    if extracted_chars < MIN_USEFUL_CHARS
        && (html_chars > LARGE_HTML_CHARS
            || candidate.extracted.js_hint_count > 0
            || candidate.extracted.paragraph_count < 3)
    {
        return Some("html_shell_with_low_text".to_string());
    }
    if candidate.extracted.paragraph_count < 3
        && candidate.extracted.link_text_ratio > 0.6
        && extracted_chars < 2_000
    {
        return Some("navigation_heavy_low_text".to_string());
    }
    None
}

fn is_timeout_like(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("connection reset")
        || lower.contains("unexpected eof")
}

fn render_with_local_browser(url: &str) -> Result<String> {
    let chrome = find_chrome_executable().ok_or_else(|| anyhow!("chrome_not_found"))?;
    let output = Command::new(&chrome)
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--dump-dom")
        .arg(url)
        .output()
        .with_context(|| format!("failed to launch Chrome: {}", chrome.display()))?;
    if !output.status.success() {
        bail!(
            "Chrome exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let html = String::from_utf8(output.stdout).context("Chrome DOM output was not UTF-8")?;
    if html.trim().is_empty() {
        bail!("Chrome returned empty DOM");
    }
    Ok(html)
}

fn find_chrome_executable() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("DUCKAGENT_CHROME_PATH") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
    ];
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
        .or_else(|| command_in_path("google-chrome"))
        .or_else(|| command_in_path("chromium"))
        .or_else(|| command_in_path("chromium-browser"))
        .or_else(|| command_in_path("chrome"))
}

fn command_in_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|path| path.exists())
}

fn maybe_compress_text(
    text: &str,
    purpose: &str,
    max_chars: usize,
    client: &ModelClient,
) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let prompt = format!(
        "Compress the following extracted web content for this purpose:\n\n{purpose}\n\nKeep key facts, quotes, code blocks, links, names, dates, prices, and source-specific details. Return at most {max_chars} characters.\n\nCONTENT:\n{}",
        truncate_chars(text, 200_000)
    );
    let messages = vec![
        crate::model::Message::System("You are a precise web content compressor.".into()),
        crate::model::Message::User(prompt.into()),
    ];
    client
        .generate_with_tools(messages, Vec::new())
        .ok()
        .and_then(|response| response.final_text)
        .map(|text| truncate_chars(text.trim(), max_chars))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| truncate_chars(text, max_chars))
}

fn normalize_search_output(provider: &str, query: &str, text: &str, limit: usize) -> Value {
    let parsed = serde_json::from_str::<Value>(text).ok();
    let results = parsed
        .as_ref()
        .and_then(|value| value.get("results").and_then(Value::as_array))
        .map(|items| normalize_result_items(items, limit))
        .unwrap_or_default();
    json!({
        "status": "ok",
        "provider": provider,
        "query": query,
        "results": results,
        "raw": if results.is_empty() { Some(text.to_string()) } else { None },
    })
}

fn normalize_result_items(items: &[Value], limit: usize) -> Vec<Value> {
    items
        .iter()
        .take(limit)
        .map(|item| {
            json!({
                "title": item.get("title").and_then(Value::as_str),
                "url": item.get("url").or_else(|| item.get("id")).and_then(Value::as_str),
                "snippet": item
                    .get("snippet")
                    .or_else(|| item.get("text"))
                    .or_else(|| item.get("summary"))
                    .and_then(Value::as_str),
                "published_at": item.get("publishedDate").or_else(|| item.get("published_at")).and_then(Value::as_str),
                "score": item.get("score").and_then(Value::as_f64),
            })
        })
        .collect()
}

fn remove_tag_blocks(html: &str, tag: &str) -> String {
    Regex::new(&format!(r"(?is)<{tag}\b[^>]*>.*?</{tag}>"))
        .ok()
        .map(|regex| regex.replace_all(html, " ").to_string())
        .unwrap_or_else(|| html.to_string())
}

fn capture_tag_text(html: &str, tag: &str) -> Option<String> {
    Regex::new(&format!(r"(?is)<{tag}\b[^>]*>(.*?)</{tag}>"))
        .ok()?
        .captures(html)
        .and_then(|captures| captures.get(1))
        .map(|match_| html_to_text(match_.as_str()))
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn collect_tag_text(html: &str, tag: &str) -> String {
    let Some(regex) = Regex::new(&format!(r"(?is)<{tag}\b[^>]*>(.*?)</{tag}>")).ok() else {
        return String::new();
    };
    regex
        .captures_iter(html)
        .filter_map(|captures| captures.get(1))
        .map(|match_| html_to_text(match_.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn count_tag(html: &str, tag: &str) -> usize {
    Regex::new(&format!(r"(?is)<{tag}\b"))
        .ok()
        .map(|regex| regex.find_iter(html).count())
        .unwrap_or(0)
}

fn html_to_text(html: &str) -> String {
    let with_breaks = Regex::new(
        r"(?i)</?(p|div|section|article|main|header|footer|li|br|h[1-6]|tr|table)\b[^>]*>",
    )
    .map(|regex| regex.replace_all(html, "\n").to_string())
    .unwrap_or_else(|_| html.to_string());
    let stripped = Regex::new(r"(?is)<[^>]+>")
        .map(|regex| regex.replace_all(&with_breaks, " ").to_string())
        .unwrap_or(with_breaks);
    collapse_whitespace(&decode_basic_entities(&stripped))
}

fn decode_basic_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::new();
    let mut blank_lines = 0usize;
    for line in text.lines() {
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            blank_lines += 1;
            if blank_lines <= 1 {
                out.push('\n');
            }
        } else {
            blank_lines = 0;
            out.push_str(&collapsed);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

fn js_hint_count(html: &str) -> usize {
    let lower = html.to_ascii_lowercase();
    [
        "enable javascript",
        "javascript is disabled",
        "requires javascript",
        "id=\"root\"",
        "id=\"app\"",
        "__next_data__",
        "ng-version",
        "webpack",
        "vite",
    ]
    .iter()
    .filter(|hint| lower.contains(**hint))
    .count()
}

fn normalize_relative_links(content: &str, _base_url: Option<&str>) -> String {
    content.to_string()
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_web_config_matches_product_defaults() {
        let config = WebConfig::default();
        assert_eq!(config.search.provider, WebSearchProviderKind::Exa);
        assert_eq!(config.extract.provider, WebExtractProviderKind::Local);
        assert_eq!(config.extract.browser_fallback, BrowserFallbackMode::Auto);
    }

    #[test]
    fn html_shell_triggers_browser_fallback() {
        let candidate = HtmlCandidate {
            status: Some(200),
            html: format!(
                "<html><body><div id=\"root\"></div><script>{}</script></body></html>",
                "x".repeat(6000)
            ),
            extracted: extract_html("<html><body><div id=\"root\"></div></body></html>", None),
            error: None,
        };
        assert_eq!(
            browser_fallback_reason(&candidate),
            Some("html_shell_with_low_text".to_string())
        );
    }
}
