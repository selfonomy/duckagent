use crate::approval::ApprovalProvider;
use crate::mcp::auth_store::load_access_token;
use crate::mcp::config::{McpServerConfig, McpTransportKind};
use crate::mcp::transport_stdio::McpToolDefinition;
use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
const MCP_SESSION_ID: &str = "mcp-session-id";

#[derive(Debug, Deserialize)]
struct ToolsListResult {
    #[serde(default)]
    tools: Vec<McpToolDefinition>,
}

pub fn list_tools(
    server_name: &str,
    config: &McpServerConfig,
    _approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<Vec<McpToolDefinition>> {
    let mut client = HttpMcpClient::new(server_name, config)?;
    client.initialize()?;
    let result = client.request("tools/list", json!({}))?;
    let parsed: ToolsListResult =
        serde_json::from_value(result).context("failed to parse MCP tools/list result")?;
    Ok(parsed.tools)
}

pub fn call_tool(
    server_name: &str,
    config: &McpServerConfig,
    tool_name: &str,
    arguments: Value,
    _approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<Value> {
    let mut client = HttpMcpClient::new(server_name, config)?;
    client.initialize()?;
    client.request(
        "tools/call",
        json!({
            "name": tool_name,
            "arguments": arguments,
        }),
    )
}

struct HttpMcpClient<'a> {
    server_name: &'a str,
    config: &'a McpServerConfig,
    http: Client,
    session_id: Option<String>,
    next_id: u64,
}

impl<'a> HttpMcpClient<'a> {
    fn new(server_name: &'a str, config: &'a McpServerConfig) -> Result<Self> {
        let url = config
            .url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("remote MCP server missing url")?;
        Url::parse(url).with_context(|| format!("invalid MCP HTTP url: {url}"))?;
        let http = Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms()))
            .build()
            .context("failed to build MCP HTTP client")?;
        Ok(Self {
            server_name,
            config,
            http,
            session_id: None,
            next_id: 1,
        })
    }

    fn initialize(&mut self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "duckagent",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        )?;
        self.notify("notifications/initialized", json!({}))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let response = self.post_json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        if let Some(error) = response.get("error") {
            bail!("MCP HTTP request {method} failed: {error}");
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.post_json(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))?;
        Ok(())
    }

    fn post_json(&mut self, body: Value) -> Result<Value> {
        let url = self
            .config
            .url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("remote MCP server missing url")?;
        let mut request = self.http.post(url).headers(self.headers()?).json(&body);
        if let Some(session_id) = self.session_id.as_ref() {
            request = request.header(MCP_SESSION_ID, session_id);
        }
        let response = request
            .send()
            .with_context(|| format!("failed POST MCP endpoint: {url}"))?;
        if response.status().as_u16() == 401 {
            bail!(
                "MCP server `{}` requires authentication. Run `duck mcp auth {}`.",
                self.server_name,
                self.server_name
            );
        }
        let headers = response.headers().clone();
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let text = response
            .error_for_status()
            .with_context(|| format!("MCP HTTP request failed: {url}"))?
            .text()
            .context("failed to read MCP HTTP response")?;
        if let Some(session_id) = headers
            .get(MCP_SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty())
        {
            self.session_id = Some(session_id.to_string());
        }
        if content_type.contains("text/event-stream")
            || self.config.effective_transport()? == McpTransportKind::Sse
        {
            return parse_sse_response(&text);
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).context("failed to parse MCP HTTP JSON response")
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            HeaderName::from_static("mcp-protocol-version"),
            HeaderValue::from_static(MCP_PROTOCOL_VERSION),
        );
        for (key, value) in &self.config.headers {
            headers.insert(
                HeaderName::from_bytes(key.as_bytes())
                    .with_context(|| format!("invalid MCP header name: {key}"))?,
                HeaderValue::from_str(value)
                    .with_context(|| format!("invalid MCP header value for {key}"))?,
            );
        }
        if let Some(token) = load_access_token(self.server_name)? {
            headers.insert(
                HeaderName::from_static("authorization"),
                HeaderValue::from_str(&format!("Bearer {token}"))
                    .context("invalid MCP OAuth bearer token")?,
            );
        }
        Ok(headers)
    }
}

fn parse_sse_response(text: &str) -> Result<Value> {
    for line in text.lines() {
        let Some(data) = line.trim_start().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        return serde_json::from_str(data).context("failed to parse MCP SSE data JSON");
    }
    bail!("MCP SSE response did not contain a data JSON event")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::config::McpTransportKind;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn parses_first_sse_data_event() -> Result<()> {
        let value = parse_sse_response(
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"result\":{\"ok\":true}}\n\n",
        )?;
        assert_eq!(value["result"]["ok"], true);
        Ok(())
    }

    #[test]
    fn http_mcp_client_ignores_sandbox_network_policy() -> Result<()> {
        let config = McpServerConfig {
            transport: Some(McpTransportKind::Http),
            url: Some("https://api.example.com/mcp".to_string()),
            ..Default::default()
        };

        let client = HttpMcpClient::new("docs", &config)?;
        assert_eq!(client.server_name, "docs");
        Ok(())
    }

    #[test]
    #[ignore = "sandboxed test environments can disallow local TCP listeners"]
    fn streamable_http_mock_server_lists_tools() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let server = thread::spawn(move || -> Result<()> {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept()?;
                let body = read_http_body(&mut stream)?;
                let result = if body.contains("\"method\":\"initialize\"") {
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock", "version": "1" }
                        }
                    })
                } else if body.contains("\"method\":\"tools/list\"") {
                    json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "result": {
                            "tools": [{
                                "name": "remote_ping",
                                "description": "Remote ping",
                                "inputSchema": { "type": "object" }
                            }]
                        }
                    })
                } else {
                    json!({ "jsonrpc": "2.0", "result": {} })
                };
                let text = result.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    text.len(),
                    text
                );
                stream.write_all(response.as_bytes())?;
            }
            Ok(())
        });

        let config = McpServerConfig {
            transport: Some(McpTransportKind::Http),
            url: Some(url),
            ..Default::default()
        };
        let tools = list_tools("mock", &config, None)?;
        server.join().expect("mock server panicked")?;
        assert_eq!(tools[0].name, "remote_ping");
        Ok(())
    }

    fn read_http_body(stream: &mut std::net::TcpStream) -> Result<String> {
        let mut buffer = Vec::new();
        let mut temp = [0_u8; 1024];
        let header_end = loop {
            let n = stream.read(&mut temp)?;
            if n == 0 {
                bail!("unexpected EOF while reading request headers");
            }
            buffer.extend_from_slice(&temp[..n]);
            if let Some(pos) = find_subslice(&buffer, b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(key, value)| {
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
            })
            .unwrap_or(0);
        while buffer.len() < header_end + content_length {
            let n = stream.read(&mut temp)?;
            if n == 0 {
                break;
            }
            buffer.extend_from_slice(&temp[..n]);
        }
        Ok(String::from_utf8_lossy(&buffer[header_end..header_end + content_length]).to_string())
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }
}
