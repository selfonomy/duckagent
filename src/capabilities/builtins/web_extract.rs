use crate::client::ModelClient;
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DESCRIPTION: &str = concat!(
    "Fetch and extract readable content from one or more web URLs using the configured Web Extract provider. ",
    "Pass a natural language purpose describing what matters for this task. ",
    "Provider routing and local browser fallback are controlled by duckagent web config; ",
    "do not pass curl/browser/provider choices."
);

fn default_max_chars() -> usize {
    8_000
}

fn default_format() -> String {
    "markdown".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebExtractArgs {
    /// One or more HTTP(S) URLs to read.
    pub urls: Vec<String>,
    /// Natural language purpose: what content should be preserved or emphasized.
    pub purpose: String,
    /// Maximum returned characters. Defaults to 8000.
    #[serde(default = "default_max_chars")]
    pub max_chars: usize,
    /// Output format. v1 supports markdown or text.
    #[serde(default = "default_format")]
    pub format: String,
    /// Crawl depth. v1 supports only 0.
    #[serde(default)]
    pub depth: usize,
}

pub fn execute(args: Value, client: &ModelClient) -> Result<String> {
    let input: WebExtractArgs =
        serde_json::from_value(args).context("failed to parse web_extract args")?;
    if input.urls.is_empty() {
        bail!("web_extract.urls must contain at least one URL");
    }
    if input.purpose.trim().is_empty() {
        bail!("web_extract.purpose must be non-empty");
    }
    crate::web::extract(input, client)
}
