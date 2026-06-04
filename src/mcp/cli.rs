use crate::mcp::auth_store::McpAuthStore;
use crate::mcp::config::{DuckAgentConfig, McpServerConfig, McpTransportKind};
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Args)]
pub struct McpCommand {
    #[command(subcommand)]
    pub command: McpSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum McpSubcommand {
    Add(McpAddCommand),
    List,
    Get { name: String },
    Remove { name: String },
    Auth { name: String },
    Logout { name: String },
}

#[derive(Debug, Args)]
pub struct McpAddCommand {
    #[arg(long, value_enum)]
    pub transport: Option<McpTransportKind>,
    #[arg(long = "env")]
    pub env: Vec<String>,
    #[arg(long = "header")]
    pub headers: Vec<String>,
    #[arg(long)]
    pub oauth: bool,
    #[arg(long = "no-oauth")]
    pub no_oauth: bool,
    #[arg(long)]
    pub timeout: Option<u64>,
    #[arg(long = "callback-port")]
    pub callback_port: Option<u16>,
    pub name: String,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub values: Vec<String>,
}

pub fn run(command: McpCommand) -> Result<()> {
    match command.command {
        McpSubcommand::Add(input) => add_server(input),
        McpSubcommand::List => list_servers(),
        McpSubcommand::Get { name } => get_server(&name),
        McpSubcommand::Remove { name } => remove_server(&name),
        McpSubcommand::Auth { name } => auth_server(&name),
        McpSubcommand::Logout { name } => logout_server(&name),
    }
}

fn add_server(input: McpAddCommand) -> Result<()> {
    let server_config = build_add_server_config(&input)?;

    let mut config = DuckAgentConfig::load_active_profile()?;
    let mut servers = config.mcp_servers()?;
    servers.insert(input.name.clone(), server_config);
    config.set_mcp_servers(servers)?;
    config.save_active_profile()?;
    println!("Added MCP server `{}`.", input.name);
    Ok(())
}

fn list_servers() -> Result<()> {
    let servers = DuckAgentConfig::load_active_profile()?.mcp_servers()?;
    let auth = McpAuthStore::load_active_profile().unwrap_or_default();
    println!("{}", format_server_list(&servers, &auth));
    Ok(())
}

fn get_server(name: &str) -> Result<()> {
    let servers = DuckAgentConfig::load_active_profile()?.mcp_servers()?;
    let config = servers
        .get(name)
        .with_context(|| format!("MCP server `{name}` is not configured"))?;
    println!("{}", serde_json::to_string_pretty(config)?);
    Ok(())
}

fn remove_server(name: &str) -> Result<()> {
    let mut config = DuckAgentConfig::load_active_profile()?;
    let mut servers = config.mcp_servers()?;
    remove_server_from_map(&mut servers, name)?;
    config.set_mcp_servers(servers)?;
    config.save_active_profile()?;
    println!("Removed MCP server `{name}`.");
    Ok(())
}

fn auth_server(name: &str) -> Result<()> {
    let servers = DuckAgentConfig::load_active_profile()?.mcp_servers()?;
    let config = servers
        .get(name)
        .with_context(|| format!("MCP server `{name}` is not configured"))?;
    crate::mcp::oauth::authenticate(name, config)
}

fn logout_server(name: &str) -> Result<()> {
    let mut store = McpAuthStore::load_active_profile().unwrap_or_default();
    let removed = store.remove_server(name);
    store.save_active_profile()?;
    if removed {
        println!("Removed MCP auth for `{name}`.");
    } else {
        println!("No MCP auth entry found for `{name}`.");
    }
    Ok(())
}

fn resolve_add_transport(input: &McpAddCommand) -> Result<McpTransportKind> {
    if let Some(transport) = input.transport {
        return Ok(transport);
    }
    if input.values.len() == 1 && looks_like_url(&input.values[0]) {
        return Ok(McpTransportKind::Http);
    }
    if !input.values.is_empty() {
        return Ok(McpTransportKind::Stdio);
    }
    bail!("cannot infer MCP transport; provide --transport")
}

fn looks_like_url(value: &str) -> bool {
    let value = value.trim();
    value.starts_with("http://") || value.starts_with("https://")
}

fn build_add_server_config(input: &McpAddCommand) -> Result<McpServerConfig> {
    if input.oauth && input.no_oauth {
        bail!("--oauth and --no-oauth cannot be used together");
    }
    let transport = resolve_add_transport(input)?;
    let mut config = McpServerConfig {
        transport: Some(transport),
        env: parse_key_values(&input.env, "env", '=')?,
        headers: parse_key_values(&input.headers, "header", ':')?,
        enabled: Some(true),
        timeout: input.timeout,
        oauth: build_oauth_config(input),
        ..Default::default()
    };

    match transport {
        McpTransportKind::Stdio => {
            if input.values.is_empty() {
                bail!("stdio MCP server requires `-- <command> [args...]`");
            }
            config.command = input.values.first().cloned();
            config.args = input.values.iter().skip(1).cloned().collect();
        }
        McpTransportKind::Http | McpTransportKind::Sse => {
            if input.values.len() != 1 {
                bail!("remote MCP server requires exactly one <url>");
            }
            let url = input.values[0].trim();
            if url.is_empty() {
                bail!("remote MCP server url must be non-empty");
            }
            config.url = Some(url.to_string());
        }
    }

    Ok(config)
}

fn remove_server_from_map(
    servers: &mut BTreeMap<String, McpServerConfig>,
    name: &str,
) -> Result<()> {
    if servers.remove(name).is_none() {
        bail!("MCP server `{name}` is not configured");
    }
    Ok(())
}

fn format_server_list(servers: &BTreeMap<String, McpServerConfig>, auth: &McpAuthStore) -> String {
    if servers.is_empty() {
        return "No MCP servers configured.".to_string();
    }

    servers
        .iter()
        .map(|(name, config)| {
            let transport = config
                .effective_transport()
                .map(|transport| transport.as_str().to_string())
                .unwrap_or_else(|_| "invalid".to_string());
            let target = config
                .url
                .clone()
                .or_else(|| {
                    config.command.as_ref().map(|command| {
                        std::iter::once(command.clone())
                            .chain(config.args.clone())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                })
                .unwrap_or_else(|| "(missing target)".to_string());
            let auth_status = if auth.servers.contains_key(name) {
                "authenticated"
            } else {
                "not authenticated"
            };
            format!(
                "{}\t{}\t{}\t{}\t{}",
                name,
                transport,
                if config.is_enabled() {
                    "enabled"
                } else {
                    "disabled"
                },
                auth_status,
                target
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_key_values(
    values: &[String],
    label: &str,
    separator: char,
) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for raw in values {
        let Some((key, value)) = raw.split_once(separator) else {
            bail!("invalid --{label} value `{raw}`");
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            bail!("invalid --{label} value `{raw}`: key is empty");
        }
        map.insert(key.to_string(), value.to_string());
    }
    Ok(map)
}

fn build_oauth_config(input: &McpAddCommand) -> Option<Value> {
    if input.no_oauth {
        return Some(json!(false));
    }
    let mut object = Map::new();
    if let Some(port) = input.callback_port {
        object.insert("callbackPort".to_string(), json!(port));
    }
    if !object.is_empty() {
        return Some(Value::Object(object));
    }
    if input.oauth {
        return Some(json!(true));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct TestAddCli {
        #[command(flatten)]
        add: McpAddCommand,
    }

    #[test]
    fn infers_stdio_when_command_is_present() -> Result<()> {
        let input = McpAddCommand {
            transport: None,
            env: Vec::new(),
            headers: Vec::new(),
            oauth: false,
            no_oauth: false,
            timeout: None,
            callback_port: None,
            name: "playwright".to_string(),
            values: vec!["npx".to_string(), "@playwright/mcp@latest".to_string()],
        };
        assert_eq!(resolve_add_transport(&input)?, McpTransportKind::Stdio);
        Ok(())
    }

    #[test]
    fn parses_http_add_without_trailing_var_arg_panic() -> Result<()> {
        let parsed = TestAddCli::try_parse_from([
            "test",
            "--transport",
            "http",
            "cloudfare-doc",
            "https://docs.mcp.cloudflare.com/mcp",
        ])?;
        assert_eq!(parsed.add.transport, Some(McpTransportKind::Http));
        assert_eq!(parsed.add.name, "cloudfare-doc");
        assert_eq!(
            parsed.add.values,
            vec!["https://docs.mcp.cloudflare.com/mcp"]
        );
        Ok(())
    }

    #[test]
    fn parses_stdio_command_after_double_dash() -> Result<()> {
        let parsed = TestAddCli::try_parse_from([
            "test",
            "--transport",
            "stdio",
            "playwright",
            "--",
            "npx",
            "-y",
            "@playwright/mcp@latest",
        ])?;
        assert_eq!(parsed.add.transport, Some(McpTransportKind::Stdio));
        assert_eq!(parsed.add.name, "playwright");
        assert_eq!(
            parsed.add.values,
            vec!["npx", "-y", "@playwright/mcp@latest"]
        );
        Ok(())
    }

    #[test]
    fn builds_http_config_with_header_oauth_and_timeout() -> Result<()> {
        let input = McpAddCommand {
            transport: Some(McpTransportKind::Http),
            env: Vec::new(),
            headers: vec!["Authorization: Bearer token".to_string()],
            oauth: true,
            no_oauth: false,
            timeout: Some(15_000),
            callback_port: Some(8080),
            name: "docs".to_string(),
            values: vec!["https://example.com/mcp".to_string()],
        };

        let config = build_add_server_config(&input)?;
        assert_eq!(config.transport, Some(McpTransportKind::Http));
        assert_eq!(config.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(config.headers["Authorization"], "Bearer token");
        assert_eq!(config.timeout, Some(15_000));
        assert_eq!(config.oauth.as_ref().unwrap()["callbackPort"], 8080);
        Ok(())
    }

    #[test]
    fn builds_stdio_config_with_env_and_args() -> Result<()> {
        let input = McpAddCommand {
            transport: Some(McpTransportKind::Stdio),
            env: vec!["API_KEY=secret".to_string()],
            headers: Vec::new(),
            oauth: false,
            no_oauth: false,
            timeout: None,
            callback_port: None,
            name: "playwright".to_string(),
            values: vec![
                "npx".to_string(),
                "-y".to_string(),
                "@playwright/mcp@latest".to_string(),
            ],
        };

        let config = build_add_server_config(&input)?;
        assert_eq!(config.transport, Some(McpTransportKind::Stdio));
        assert_eq!(config.command.as_deref(), Some("npx"));
        assert_eq!(config.args, vec!["-y", "@playwright/mcp@latest"]);
        assert_eq!(config.env["API_KEY"], "secret");
        Ok(())
    }

    #[test]
    fn remove_server_errors_when_missing() {
        let mut servers = BTreeMap::new();
        let err = remove_server_from_map(&mut servers, "missing").unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[test]
    fn list_output_is_stable_and_marks_auth() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "docs".to_string(),
            McpServerConfig {
                transport: Some(McpTransportKind::Http),
                url: Some("https://example.com/mcp".to_string()),
                ..Default::default()
            },
        );
        let mut auth = McpAuthStore::default();
        auth.servers.insert(
            "docs".to_string(),
            crate::mcp::auth_store::McpAuthEntry {
                access_token: Some("token".to_string()),
                ..Default::default()
            },
        );

        let output = format_server_list(&servers, &auth);
        assert_eq!(
            output,
            "docs\thttp\tenabled\tauthenticated\thttps://example.com/mcp"
        );
    }

    #[test]
    fn parses_header_colon_and_env_equals() -> Result<()> {
        let env = parse_key_values(&["KEY=value".to_string()], "env", '=')?;
        let headers =
            parse_key_values(&["Authorization: Bearer token".to_string()], "header", ':')?;
        assert_eq!(env["KEY"], "value");
        assert_eq!(headers["Authorization"], "Bearer token");
        Ok(())
    }
}
