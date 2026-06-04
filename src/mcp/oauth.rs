use crate::mcp::auth_store::{McpAuthEntry, McpAuthStore};
use crate::mcp::config::{McpServerConfig, McpTransportKind};
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::blocking::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;
use uuid::Uuid;

const DEFAULT_CALLBACK_PORT: u16 = 19876;
const CALLBACK_PATH: &str = "/mcp/oauth/callback";

pub fn authenticate(server_name: &str, config: &McpServerConfig) -> Result<()> {
    if matches!(config.oauth.as_ref(), Some(Value::Bool(false))) {
        bail!("OAuth is disabled for MCP server `{server_name}`");
    }
    let transport = config.effective_transport()?;
    if !matches!(transport, McpTransportKind::Http | McpTransportKind::Sse) {
        bail!("OAuth is only supported for remote HTTP/SSE MCP servers");
    }
    let server_url = config
        .url
        .as_deref()
        .context("remote MCP server missing url")?;
    let callback_port = oauth_u16(config, "callbackPort").unwrap_or(DEFAULT_CALLBACK_PORT);
    let redirect_uri = format!("http://127.0.0.1:{callback_port}{CALLBACK_PATH}");
    let http = Client::builder()
        .timeout(Duration::from_millis(config.timeout_ms()))
        .build()
        .context("failed to build OAuth HTTP client")?;

    let metadata = discover_metadata(&http, server_url, config)?;
    let client = resolve_client_registration(&http, &metadata, config, &redirect_uri)?;
    let verifier = pkce_verifier();
    let challenge = pkce_challenge(&verifier);
    let state = Uuid::now_v7().to_string();
    let scope = oauth_string(config, "scope")
        .or_else(|| oauth_string(config, "scopes"))
        .or_else(|| {
            metadata.get("scopes_supported").and_then(|value| {
                value.as_array().map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            })
        });

    let mut auth_url = Url::parse(
        metadata
            .get("authorization_endpoint")
            .and_then(Value::as_str)
            .context("OAuth metadata missing authorization_endpoint")?,
    )
    .context("invalid OAuth authorization_endpoint")?;
    {
        let mut query = auth_url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &client.client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        if let Some(scope) = scope.as_deref().filter(|value| !value.trim().is_empty()) {
            query.append_pair("scope", scope);
        }
    }

    println!("Open this URL to authenticate MCP server `{server_name}`:\n{auth_url}");
    let _ = Command::new("open").arg(auth_url.as_str()).status();
    let code = wait_for_callback(callback_port, &state)?;
    let token = exchange_code(&http, &metadata, &client, &redirect_uri, &verifier, &code)?;

    let mut store = McpAuthStore::load_active_profile().unwrap_or_default();
    store.servers.insert(
        server_name.to_string(),
        McpAuthEntry {
            access_token: token
                .get("access_token")
                .and_then(Value::as_str)
                .map(str::to_string),
            refresh_token: token
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(str::to_string),
            token_type: token
                .get("token_type")
                .and_then(Value::as_str)
                .map(str::to_string),
            expires_at: token
                .get("expires_in")
                .and_then(Value::as_i64)
                .map(|seconds| now_unix_seconds() + seconds),
            client_id: Some(client.client_id),
            client_secret: client.client_secret,
            scope,
        },
    );
    store.save_active_profile()?;
    println!("Authenticated MCP server `{server_name}`.");
    Ok(())
}

struct OAuthClientRegistration {
    client_id: String,
    client_secret: Option<String>,
}

fn discover_metadata(http: &Client, server_url: &str, config: &McpServerConfig) -> Result<Value> {
    if let Some(url) = oauth_string(config, "authServerMetadataUrl") {
        return get_json(http, &url);
    }
    let parsed = Url::parse(server_url).context("invalid MCP server url")?;
    let origin = parsed.origin().ascii_serialization();
    let protected_url = format!("{origin}/.well-known/oauth-protected-resource");
    let auth_server = get_json(http, &protected_url)
        .ok()
        .and_then(|value| {
            value
                .get("authorization_servers")
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or(origin);
    let metadata_url = format!(
        "{}/.well-known/oauth-authorization-server",
        auth_server.trim_end_matches('/')
    );
    get_json(http, &metadata_url)
}

fn resolve_client_registration(
    http: &Client,
    metadata: &Value,
    config: &McpServerConfig,
    redirect_uri: &str,
) -> Result<OAuthClientRegistration> {
    if let Some(client_id) = oauth_string(config, "clientId") {
        return Ok(OAuthClientRegistration {
            client_id,
            client_secret: oauth_string(config, "clientSecret"),
        });
    }
    let registration_endpoint = metadata
        .get("registration_endpoint")
        .and_then(Value::as_str)
        .context("OAuth clientId is not configured and metadata has no registration_endpoint")?;
    let response = http
        .post(registration_endpoint)
        .json(&json!({
            "client_name": "duckagent",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        }))
        .send()
        .with_context(|| {
            format!("failed POST OAuth registration endpoint: {registration_endpoint}")
        })?
        .error_for_status()
        .context("OAuth dynamic client registration failed")?
        .json::<Value>()
        .context("failed to parse OAuth registration response")?;
    Ok(OAuthClientRegistration {
        client_id: response
            .get("client_id")
            .and_then(Value::as_str)
            .context("OAuth registration response missing client_id")?
            .to_string(),
        client_secret: response
            .get("client_secret")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn wait_for_callback(port: u16, expected_state: &str) -> Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("failed to bind OAuth callback port {port}"))?;
    let (mut stream, _) = listener
        .accept()
        .context("failed to accept OAuth callback")?;
    let mut buffer = [0_u8; 8192];
    let bytes = stream
        .read(&mut buffer)
        .context("failed to read OAuth callback request")?;
    let request = String::from_utf8_lossy(&buffer[..bytes]);
    let first_line = request.lines().next().unwrap_or_default();
    let target = first_line
        .split_whitespace()
        .nth(1)
        .context("OAuth callback request missing path")?;
    let url = Url::parse(&format!("http://127.0.0.1{target}"))
        .context("failed to parse OAuth callback URL")?;
    let params = url
        .query_pairs()
        .collect::<std::collections::BTreeMap<_, _>>();
    let state = params
        .get("state")
        .map(|value| value.as_ref())
        .context("OAuth callback missing state")?;
    if state != expected_state {
        bail!("OAuth callback state mismatch");
    }
    if let Some(error) = params.get("error") {
        bail!("OAuth authorization failed: {error}");
    }
    let code = params
        .get("code")
        .map(|value| value.to_string())
        .context("OAuth callback missing code")?;
    let body = "MCP authentication complete. You can return to duckagent.";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    Ok(code)
}

fn exchange_code(
    http: &Client,
    metadata: &Value,
    client: &OAuthClientRegistration,
    redirect_uri: &str,
    verifier: &str,
    code: &str,
) -> Result<Value> {
    let token_endpoint = metadata
        .get("token_endpoint")
        .and_then(Value::as_str)
        .context("OAuth metadata missing token_endpoint")?;
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("client_id", client.client_id.clone()),
        ("code_verifier", verifier.to_string()),
    ];
    if let Some(secret) = client.client_secret.as_ref() {
        form.push(("client_secret", secret.clone()));
    }
    http.post(token_endpoint)
        .form(&form)
        .send()
        .with_context(|| format!("failed POST OAuth token endpoint: {token_endpoint}"))?
        .error_for_status()
        .context("OAuth token exchange failed")?
        .json::<Value>()
        .context("failed to parse OAuth token response")
}

fn get_json(http: &Client, url: &str) -> Result<Value> {
    http.get(url)
        .send()
        .with_context(|| format!("failed GET {url}"))?
        .error_for_status()
        .with_context(|| format!("OAuth discovery request failed: {url}"))?
        .json::<Value>()
        .with_context(|| format!("failed to parse OAuth metadata JSON: {url}"))
}

fn oauth_string(config: &McpServerConfig, key: &str) -> Option<String> {
    config
        .oauth
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|object| object.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn oauth_u16(config: &McpServerConfig, key: &str) -> Option<u16> {
    config
        .oauth
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|object| object.get(key))
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
}

fn pkce_verifier() -> String {
    format!("{}{}", Uuid::now_v7().simple(), Uuid::now_v7().simple())
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_base64_url_without_padding() {
        let challenge = pkce_challenge("verifier");
        assert!(!challenge.contains('='));
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
    }
}
