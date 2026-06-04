use super::{BuiltinToolSpec, schema_value};
use anyhow::{Context, Result};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct LoadMcpArgs {
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct LoadMcpToolMetadata {
    pub name: String,
    pub server: String,
    pub original_name: String,
    pub description: String,
    pub input_schema: Value,
}

pub fn spec() -> BuiltinToolSpec {
    BuiltinToolSpec {
        name: "load_mcp",
        description: "Load the full schema and metadata for one MCP tool by exposed name.",
        input_schema: schema_value(schema_for!(LoadMcpArgs)),
    }
}

pub fn parse_requested_name(args: Value) -> Result<String> {
    let input: LoadMcpArgs =
        serde_json::from_value(args).context("failed to parse load_mcp args")?;
    Ok(input.name.trim().to_string())
}

pub fn render_tool_metadata(metadata: LoadMcpToolMetadata) -> Result<String> {
    serde_json::to_string_pretty(&serde_json::json!({
        "name": metadata.name,
        "server": metadata.server,
        "original_name": metadata.original_name,
        "description": metadata.description,
        "input_schema": metadata.input_schema,
    }))
    .context("failed to serialize load_mcp result")
}
