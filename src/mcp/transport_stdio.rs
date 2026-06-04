use crate::approval::ApprovalProvider;
use crate::mcp::config::McpServerConfig;
use crate::sandbox::config::{NetworkMode, PermissionAction, ResolvedSandbox, resolve_sandbox};
use crate::sandbox::runner::sandbox_command_with_target;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Stdio};
use std::sync::Arc;

const MCP_PROTOCOL_VERSION: &str = "2025-11-25";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpToolDefinition {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

#[derive(Debug, Deserialize)]
struct ToolsListResult {
    #[serde(default)]
    tools: Vec<McpToolDefinition>,
}

pub fn list_tools(
    config: &McpServerConfig,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<Vec<McpToolDefinition>> {
    let mut client = StdioMcpClient::start(config, approval_provider)?;
    client.initialize()?;
    let result = client.request("tools/list", json!({}))?;
    let parsed: ToolsListResult =
        serde_json::from_value(result).context("failed to parse MCP tools/list result")?;
    Ok(parsed.tools)
}

pub fn call_tool(
    config: &McpServerConfig,
    tool_name: &str,
    arguments: Value,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<Value> {
    let mut client = StdioMcpClient::start(config, approval_provider)?;
    client.initialize()?;
    client.request(
        "tools/call",
        json!({
            "name": tool_name,
            "arguments": arguments,
        }),
    )
}

struct StdioMcpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: Option<ChildStderr>,
    next_id: u64,
}

impl StdioMcpClient {
    fn start(
        config: &McpServerConfig,
        approval_provider: Option<Arc<dyn ApprovalProvider>>,
    ) -> Result<Self> {
        let command = config
            .command
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("stdio MCP server missing command")?;
        let sandbox =
            sandbox_for_mcp_stdio().context("failed to resolve sandbox for MCP stdio server")?;
        let mut env = BTreeMap::new();
        for (key, value) in &config.env {
            env.insert(key.clone(), value.clone());
        }
        let mut command_builder = sandbox_command_with_target(
            &sandbox,
            None,
            env,
            command,
            &config.args,
            approval_provider,
        )?;
        let mut child = command_builder
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start MCP stdio server: {command}"))?;
        let stdin = child.stdin.take().context("failed to open MCP stdin")?;
        let stdout = child.stdout.take().context("failed to open MCP stdout")?;
        let stderr = child.stderr.take();
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr,
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
        self.write_message(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        loop {
            let message = self.read_message()?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                bail!("MCP stdio request {method} failed: {error}");
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_message(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    fn write_message(&mut self, value: Value) -> Result<()> {
        let line = serde_json::to_string(&value).context("failed to serialize MCP message")?;
        self.stdin
            .write_all(line.as_bytes())
            .context("failed to write MCP message")?;
        self.stdin
            .write_all(b"\n")
            .context("failed to write MCP message newline")?;
        self.stdin.flush().context("failed to flush MCP stdin")
    }

    fn read_message(&mut self) -> Result<Value> {
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .context("failed to read MCP message")?;
        if n == 0 {
            let status = self.child.try_wait().ok().flatten();
            let mut stderr = String::new();
            if let Some(mut stderr_pipe) = self.stderr.take() {
                let _ = stderr_pipe.read_to_string(&mut stderr);
            }
            bail!(
                "MCP stdio server closed stdout; status: {status:?}; stderr: {}",
                stderr.trim()
            );
        }
        serde_json::from_str(line.trim()).context("failed to parse MCP JSON-RPC message")
    }
}

fn sandbox_for_mcp_stdio() -> Result<ResolvedSandbox> {
    resolve_sandbox().map(allow_network_for_mcp_stdio)
}

fn allow_network_for_mcp_stdio(mut sandbox: ResolvedSandbox) -> ResolvedSandbox {
    sandbox.preset.network.mode = NetworkMode::Allow;
    sandbox.preset.network.hosts.clear();
    sandbox.preset.network.addresses.clear();
    sandbox
        .preset
        .network
        .hosts
        .insert("*".to_string(), PermissionAction::Allow);
    sandbox
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::config::SandboxConfig;
    use serde_json::json;

    #[test]
    fn stdio_mock_server_lists_and_calls_tools() -> Result<()> {
        let _sandbox = crate::sandbox::config::TestSandboxOverrideGuard::new("danger");
        let script = r#"import json
import sys

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if method == "notifications/initialized":
        continue
    result = {}
    if method == "initialize":
        result = {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}}, "serverInfo": {"name": "mock", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "ping", "description": "Ping tool", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        value = msg.get("params", {}).get("arguments", {}).get("value", "")
        result = {"content": [{"type": "text", "text": "pong " + value}]}
    print(json.dumps({"jsonrpc": "2.0", "id": msg.get("id"), "result": result}), flush=True)
"#;
        let config = McpServerConfig {
            command: Some("python3".to_string()),
            args: vec!["-u".to_string(), "-c".to_string(), script.to_string()],
            ..Default::default()
        };

        let tools = list_tools(&config, None)?;
        assert_eq!(tools[0].name, "ping");
        let result = call_tool(&config, "ping", json!({"value": "ok"}), None)?;
        assert_eq!(result["content"][0]["text"], "pong ok");
        Ok(())
    }

    #[test]
    fn stdio_mcp_keeps_filesystem_sandbox_but_ignores_network_rules() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        let filesystem = sandbox.preset.filesystem.clone();

        let mcp_sandbox = allow_network_for_mcp_stdio(sandbox);

        assert_eq!(mcp_sandbox.preset.filesystem, filesystem);
        assert_eq!(mcp_sandbox.preset.network.mode, NetworkMode::Allow);
        assert_eq!(
            mcp_sandbox.preset.network.hosts.get("*"),
            Some(&PermissionAction::Allow)
        );
        assert!(mcp_sandbox.preset.network.addresses.is_empty());
        Ok(())
    }
}
