use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DESCRIPTION: &str = concat!(
    "Search the public web through the configured Web Search provider. ",
    "Use for current or external web information. The model supplies a query and optional limits; ",
    "provider routing is controlled by duckagent web config, not by tool arguments."
);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchFreshness {
    Any,
    Day,
    Week,
    Month,
    Year,
}

fn default_limit() -> usize {
    5
}

fn default_freshness() -> WebSearchFreshness {
    WebSearchFreshness::Any
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebSearchArgs {
    /// Natural language web search query.
    pub query: String,
    /// Maximum number of results to return. Defaults to 5 and is capped by the provider.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Freshness preference for providers that support time filtering.
    #[serde(default = "default_freshness")]
    pub freshness: WebSearchFreshness,
    /// Optional domain allow-list for providers that support domain filtering.
    #[serde(default)]
    pub include_domains: Vec<String>,
    /// Optional domain deny-list for providers that support domain filtering.
    #[serde(default)]
    pub exclude_domains: Vec<String>,
}

pub fn execute(args: Value) -> Result<String> {
    let input: WebSearchArgs =
        serde_json::from_value(args).context("failed to parse web_search args")?;
    if input.query.trim().is_empty() {
        bail!("web_search.query must be non-empty");
    }
    crate::web::search(input)
}
