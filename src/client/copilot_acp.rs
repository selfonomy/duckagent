use crate::client::types::{AssistantToolCall, AssistantTurn};
use crate::model::{LanguageModelResponseContentType, Message, Tool};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

pub(crate) fn request(messages: &[Message], tools: &[Tool]) -> Result<AssistantTurn> {
    let command = std::env::var("COPILOT_ACP_COMMAND").unwrap_or_else(|_| "copilot".to_string());
    let mut child = Command::new(command)
        .args(["--acp", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start `copilot --acp --stdio`")?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open ACP stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to open ACP stdout"))?;
    let mut reader = BufReader::new(stdout);

    write_jsonrpc(&mut stdin, 1, "initialize", json!({"client":"duckagent"}))?;
    let _ = read_jsonrpc_response(&mut reader, 1)?;
    write_jsonrpc(&mut stdin, 2, "session/new", json!({}))?;
    let session = read_jsonrpc_response(&mut reader, 2)?;
    let session_id = session
        .get("id")
        .or_else(|| session.get("session_id"))
        .and_then(Value::as_str)
        .unwrap_or("duckagent-session");
    let prompt = format_messages_as_acp_prompt(messages, tools);
    write_jsonrpc(
        &mut stdin,
        3,
        "session/prompt",
        json!({"session_id": session_id, "prompt": prompt}),
    )?;
    let text = read_jsonrpc_collecting_acp(&mut reader, 3)?;
    let (tool_calls, cleaned) = extract_acp_tool_calls(&text);
    Ok(AssistantTurn {
        text: (!cleaned.is_empty()).then_some(cleaned),
        reasoning: None,
        tool_calls,
    })
}

fn format_messages_as_acp_prompt(messages: &[Message], tools: &[Tool]) -> String {
    let mut sections = vec![
        "You are the active ACP backend for duckagent.".to_string(),
        "If you need a tool, output <tool_call>{...}</tool_call> with OpenAI function-call JSON exactly.".to_string(),
    ];
    if !tools.is_empty() {
        sections.push(format!("Available tools:\n{}", build_acp_tools(tools)));
    }
    for message in messages {
        match message {
            Message::System(system) => sections.push(format!("System:\n{}", system.content)),
            Message::Developer(text) => sections.push(format!("Developer:\n{text}")),
            Message::User(user) => sections.push(format!("User:\n{}", user.content)),
            Message::Assistant(assistant) => match &assistant.content {
                LanguageModelResponseContentType::Text(text) => {
                    sections.push(format!("Assistant:\n{text}"))
                }
                LanguageModelResponseContentType::Reasoning { .. } => {}
                LanguageModelResponseContentType::ToolCall(tool_call) => sections.push(format!(
                    "Assistant tool call {}({})",
                    tool_call.tool.name, tool_call.input
                )),
            },
            Message::Tool(tool) => sections.push(format!(
                "Tool {} result:\n{}",
                tool.tool.name,
                tool.output
                    .as_ref()
                    .map(|value| value.to_string())
                    .unwrap_or_else(|err| err.to_string())
            )),
        }
    }
    sections.join("\n\n")
}

fn build_acp_tools(tools: &[Tool]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    }
                })
            })
            .collect(),
    )
}

fn write_jsonrpc<W: Write>(writer: &mut W, id: u64, method: &str, params: Value) -> Result<()> {
    writeln!(
        writer,
        "{}",
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
    )
    .context("failed to write JSON-RPC request")
}

fn read_jsonrpc_response<R: BufRead>(reader: &mut R, id: u64) -> Result<Value> {
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            bail!("ACP process closed before response {id}");
        }
        let msg: Value = serde_json::from_str(line.trim()).unwrap_or_else(|_| json!({}));
        if msg.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = msg.get("error") {
            bail!("ACP request {id} failed: {error}");
        }
        return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
    }
}

fn read_jsonrpc_collecting_acp<R: BufRead>(reader: &mut R, id: u64) -> Result<String> {
    let mut text = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            bail!("ACP process closed before prompt response");
        }
        let msg: Value = serde_json::from_str(line.trim()).unwrap_or_else(|_| json!({}));
        if let Some(delta) = msg
            .pointer("/params/content/text")
            .or_else(|| msg.pointer("/params/delta/text"))
            .and_then(Value::as_str)
        {
            text.push_str(delta);
        }
        if msg.get("id").and_then(Value::as_u64) == Some(id) {
            if let Some(error) = msg.get("error") {
                bail!("ACP prompt failed: {error}");
            }
            if let Some(result_text) = msg.pointer("/result/content/text").and_then(Value::as_str) {
                text.push_str(result_text);
            }
            return Ok(text);
        }
    }
}

fn extract_acp_tool_calls(text: &str) -> (Vec<AssistantToolCall>, String) {
    let mut tool_calls = Vec::new();
    let mut cleaned = text.to_string();
    while let Some(start) = cleaned.find("<tool_call>") {
        let after = start + "<tool_call>".len();
        let Some(relative_end) = cleaned[after..].find("</tool_call>") else {
            break;
        };
        let end = after + relative_end;
        let raw = cleaned[after..end].trim();
        if let Ok(value) = serde_json::from_str::<Value>(raw)
            && let Some(function) = value.get("function")
        {
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let raw_arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let input = serde_json::from_str(raw_arguments)
                .unwrap_or_else(|_| Value::String(raw_arguments.to_string()));
            tool_calls.push(AssistantToolCall {
                call_id: value
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("acp_call")
                    .to_string(),
                name,
                input,
            });
        }
        let remove_end = end + "</tool_call>".len();
        cleaned.replace_range(start..remove_end, "");
    }
    (tool_calls, cleaned.trim().to_string())
}
