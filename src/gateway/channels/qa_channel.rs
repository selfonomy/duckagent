use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayOutbox, GatewayRoute,
    InboundMessageInput, OutboundMessage, StreamMessageHandle, TypingEvent,
};
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use uuid::Uuid;

pub(in crate::gateway) struct QaChannelAdapter {
    outbox: GatewayOutbox,
}

impl QaChannelAdapter {
    pub(in crate::gateway) fn new(outbox: GatewayOutbox) -> Self {
        Self { outbox }
    }

    fn handle_inbound(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        let value: Value = serde_json::from_slice(&request.body)
            .context("failed to parse qa-channel inbound JSON")?;
        let conversation_id = value["conversation_id"]
            .as_str()
            .unwrap_or("qa")
            .to_string();
        let text = value["text"]
            .as_str()
            .or_else(|| value["message"].as_str())
            .ok_or_else(|| anyhow!("qa-channel inbound requires text"))?
            .to_string();
        inbound.submit(InboundMessageInput {
            channel: "qa-channel".to_string(),
            conversation_id,
            thread_id: value["thread_id"].as_str().map(str::to_string),
            chat_type: value["chat_type"].as_str().map(str::to_string),
            sender_id: value["sender_id"].as_str().map(str::to_string),
            message_id: value["message_id"].as_str().map(str::to_string),
            text,
            attachments: Vec::new(),
            timestamp: value["timestamp"].as_str().map(str::to_string),
        })?;
        Ok(json_response(200, json!({"ok": true})))
    }
}

impl ChannelAdapter for QaChannelAdapter {
    fn start(&self, _inbound: GatewayInboundDispatch) -> Result<()> {
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        if request.method == "POST"
            && matches!(
                request.path.as_str(),
                "/qa-channel/inbound" | "/qa-channel/events"
            )
        {
            return self.handle_inbound(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        self.outbox
            .push(route, "qa-message", serde_json::to_value(message)?);
        Ok(())
    }

    fn send_stream_start(
        &self,
        route: &GatewayRoute,
        text: &str,
    ) -> Result<Option<StreamMessageHandle>> {
        let handle = StreamMessageHandle {
            message_id: format!("qa_stream_{}", Uuid::now_v7().simple()),
        };
        self.outbox.push(
            route,
            "qa-stream-start",
            json!({"message_id": handle.message_id, "text": text}),
        );
        Ok(Some(handle))
    }

    fn update_stream(
        &self,
        route: &GatewayRoute,
        handle: &StreamMessageHandle,
        text: &str,
        final_update: bool,
    ) -> Result<()> {
        self.outbox.push(
            route,
            "qa-stream-update",
            json!({
                "message_id": handle.message_id,
                "text": text,
                "final": final_update,
            }),
        );
        Ok(())
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        self.outbox
            .push(route, "qa-typing", serde_json::to_value(event)?);
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        self.outbox
            .push(route, "qa-approval", serde_json::to_value(prompt)?);
        Ok(())
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: true,
            typing: true,
            approval_prompt: true,
        }
    }
}

fn json_response(status: u16, value: Value) -> ChannelHttpResponse {
    ChannelHttpResponse {
        status,
        content_type: "application/json",
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::ChannelAdapter;

    #[test]
    fn qa_channel_exposes_fixture_capabilities() {
        let adapter = QaChannelAdapter::new(GatewayOutbox::new());
        let capabilities = adapter.capabilities();
        assert!(capabilities.media);
        assert!(capabilities.typing);
    }

    #[test]
    fn qa_channel_records_stream_events() -> Result<()> {
        let outbox = GatewayOutbox::new();
        let adapter = QaChannelAdapter::new(outbox.clone());
        let route = GatewayRoute {
            session_id: "s1".to_string(),
            key: crate::gateway::GatewaySessionKey {
                channel: "qa-channel".to_string(),
                conversation_id: "c1".to_string(),
                thread_id: None,
            },
        };
        let handle = adapter
            .send_stream_start(&route, "hello")?
            .expect("stream handle");
        adapter.update_stream(&route, &handle, "hello world", true)?;
        let events = outbox.list_since(None);
        assert_eq!(events.len(), 2);
        let value = serde_json::to_value(events)?;
        assert_eq!(value[0]["kind"], "qa-stream-start");
        assert_eq!(value[1]["kind"], "qa-stream-update");
        assert_eq!(value[1]["payload"]["final"], true);
        Ok(())
    }
}
