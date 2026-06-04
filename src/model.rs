use anyhow::{Result, anyhow};
use serde_json::Value;
use std::fmt;
use std::sync::Arc;

pub type Messages = Vec<Message>;

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    System(SystemMessage),
    Developer(String),
    User(UserMessage),
    Assistant(AssistantMessage),
    Tool(ToolResultInfo),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SystemMessage {
    pub content: String,
}

impl From<String> for SystemMessage {
    fn from(value: String) -> Self {
        Self { content: value }
    }
}

impl From<&str> for SystemMessage {
    fn from(value: &str) -> Self {
        Self {
            content: value.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserMessage {
    pub content: String,
}

impl From<String> for UserMessage {
    fn from(value: String) -> Self {
        Self { content: value }
    }
}

impl From<&str> for UserMessage {
    fn from(value: &str) -> Self {
        Self {
            content: value.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssistantMessage {
    pub content: LanguageModelResponseContentType,
}

impl AssistantMessage {
    pub fn new(content: LanguageModelResponseContentType, _unused: Option<()>) -> Self {
        Self { content }
    }
}

impl From<String> for AssistantMessage {
    fn from(value: String) -> Self {
        Self {
            content: LanguageModelResponseContentType::Text(value),
        }
    }
}

impl From<&str> for AssistantMessage {
    fn from(value: &str) -> Self {
        Self {
            content: LanguageModelResponseContentType::Text(value.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LanguageModelResponseContentType {
    Text(String),
    Reasoning { content: String },
    ToolCall(ToolCallInfo),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallInfo {
    pub tool: ToolCall,
    pub input: Value,
    pub content: Option<String>,
}

impl ToolCallInfo {
    pub fn new(name: String) -> Self {
        Self {
            tool: ToolCall {
                id: String::new(),
                name,
            },
            input: Value::Null,
            content: None,
        }
    }

    pub fn id(&mut self, id: String) {
        self.tool.id = id;
    }

    pub fn input(&mut self, input: Value) {
        self.input = input;
    }

    pub fn content(&mut self, content: String) {
        self.content = Some(content);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolResultInfo {
    pub tool: ToolCall,
    pub output: std::result::Result<Value, ModelToolError>,
}

impl ToolResultInfo {
    pub fn new(name: String) -> Self {
        Self {
            tool: ToolCall {
                id: String::new(),
                name,
            },
            output: Ok(Value::Null),
        }
    }

    pub fn id(&mut self, id: String) {
        self.tool.id = id;
    }

    pub fn output(&mut self, output: Value) {
        self.output = Ok(output);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelToolError(pub String);

impl fmt::Display for ModelToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub execute: Arc<dyn Fn(Value) -> std::result::Result<String, String> + Send + Sync>,
}

impl fmt::Debug for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("input_schema", &self.input_schema)
            .finish_non_exhaustive()
    }
}

pub struct ToolBuilder {
    name: Option<String>,
    description: Option<String>,
    input_schema: Option<Value>,
    execute: Option<Arc<dyn Fn(Value) -> std::result::Result<String, String> + Send + Sync>>,
}

impl Tool {
    pub fn builder() -> ToolBuilder {
        ToolBuilder {
            name: None,
            description: None,
            input_schema: None,
            execute: None,
        }
    }
}

impl ToolBuilder {
    pub fn name(mut self, value: &str) -> Self {
        self.name = Some(value.to_string());
        self
    }

    pub fn description(mut self, value: &str) -> Self {
        self.description = Some(value.to_string());
        self
    }

    pub fn input_schema(mut self, value: schemars::Schema) -> Self {
        self.input_schema = serde_json::to_value(value).ok();
        self
    }

    pub fn execute(mut self, value: ToolExecute) -> Self {
        self.execute = Some(value.0);
        self
    }

    pub fn build(self) -> Result<Tool> {
        Ok(Tool {
            name: self.name.ok_or_else(|| anyhow!("tool missing name"))?,
            description: self
                .description
                .ok_or_else(|| anyhow!("tool missing description"))?,
            input_schema: self
                .input_schema
                .ok_or_else(|| anyhow!("tool missing input schema"))?,
            execute: self
                .execute
                .ok_or_else(|| anyhow!("tool missing executor"))?,
        })
    }
}

#[derive(Clone)]
pub struct ToolExecute(pub Arc<dyn Fn(Value) -> std::result::Result<String, String> + Send + Sync>);

impl ToolExecute {
    pub fn new(
        execute: Box<dyn Fn(Value) -> std::result::Result<String, String> + Send + Sync>,
    ) -> Self {
        Self(Arc::from(execute))
    }
}
