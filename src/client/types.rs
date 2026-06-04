use crate::model::Message;
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub steps: Vec<Vec<Message>>,
    pub tool_turn_texts: Vec<String>,
    pub final_text: Option<String>,
}

#[derive(Debug, Clone)]
pub enum StreamUpdate {
    Status(String),
    TextDelta(String),
    ToolTurnText(String),
    ToolCallDelta(String),
}

#[derive(Debug, Clone)]
pub(crate) struct AssistantToolCall {
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) input: Value,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AssistantTurn {
    pub(crate) text: Option<String>,
    pub(crate) reasoning: Option<String>,
    pub(crate) tool_calls: Vec<AssistantToolCall>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PartialToolCall {
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) raw_arguments: String,
}

pub(crate) fn partial_tool_calls_to_final(
    tool_calls: BTreeMap<usize, PartialToolCall>,
) -> Result<Vec<AssistantToolCall>> {
    tool_calls
        .into_values()
        .map(|tool| {
            let input = if tool.raw_arguments.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(&tool.raw_arguments)
                    .unwrap_or_else(|_| Value::String(tool.raw_arguments.clone()))
            };
            Ok(AssistantToolCall {
                call_id: tool.call_id,
                name: tool.name,
                input,
            })
        })
        .collect::<Result<Vec<_>>>()
}
