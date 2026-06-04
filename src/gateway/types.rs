use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct GatewaySessionKey {
    pub channel: String,
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub channel: String,
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<AttachmentRef>,
    pub timestamp: String,
}

#[derive(Debug, Clone)]
pub struct InboundMessageInput {
    pub channel: String,
    pub conversation_id: String,
    pub thread_id: Option<String>,
    pub chat_type: Option<String>,
    pub sender_id: Option<String>,
    pub message_id: Option<String>,
    pub text: String,
    pub attachments: Vec<InboundAttachmentInput>,
    pub timestamp: Option<String>,
}

impl InboundMessageInput {
    pub fn session_key(&self) -> GatewaySessionKey {
        GatewaySessionKey {
            channel: self.channel.clone(),
            conversation_id: self.conversation_id.clone(),
            thread_id: self.thread_id.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct InboundAttachmentInput {
    pub bytes: Option<Vec<u8>>,
    pub path: Option<String>,
    pub filename: Option<String>,
    pub mime: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub id: String,
    pub original_filename: String,
    pub mime: String,
    pub size_bytes: u64,
    pub storage_path: String,
    pub agent_path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub text: String,
    pub media_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_prompt: Option<GatewayApprovalPrompt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typing_event: Option<TypingEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamMessageHandle {
    pub message_id: String,
}

pub trait ChannelAdapter: Send + Sync {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()>;
    fn handle_http(
        &self,
        _request: ChannelHttpRequest,
        _inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        Ok(None)
    }
    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()>;
    fn send_stream_start(
        &self,
        _route: &GatewayRoute,
        _text: &str,
    ) -> Result<Option<StreamMessageHandle>> {
        Ok(None)
    }
    fn update_stream(
        &self,
        _route: &GatewayRoute,
        _handle: &StreamMessageHandle,
        _text: &str,
        _final_update: bool,
    ) -> Result<()> {
        Ok(())
    }
    fn stream_text_limit(&self) -> usize {
        3500
    }
    fn stream_min_delta_chars(&self) -> usize {
        48
    }
    fn stream_flush_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(900)
    }
    fn stream_update_budget(&self) -> Option<usize> {
        None
    }
    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()>;
    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()>;
    fn capabilities(&self) -> ChannelCapabilities;
}

#[derive(Debug, Clone)]
pub struct ChannelHttpRequest {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl ChannelHttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct ChannelHttpResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

#[derive(Clone)]
pub struct GatewayInboundDispatch {
    submit: Arc<dyn Fn(InboundMessageInput) -> Result<()> + Send + Sync>,
}

impl GatewayInboundDispatch {
    pub fn new<F>(submit: F) -> Self
    where
        F: Fn(InboundMessageInput) -> Result<()> + Send + Sync + 'static,
    {
        Self {
            submit: Arc::new(submit),
        }
    }

    pub fn submit(&self, input: InboundMessageInput) -> Result<()> {
        (self.submit)(input)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelCapabilities {
    pub media: bool,
    pub typing: bool,
    pub approval_prompt: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayRoute {
    pub session_id: String,
    pub key: GatewaySessionKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayApprovalPrompt {
    pub id: String,
    pub command: String,
    pub options: Vec<String>,
    pub rule_hits: Vec<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypingEvent {
    pub active: bool,
    pub reason: String,
}
