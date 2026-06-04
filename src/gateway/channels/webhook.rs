use super::super::{
    ChannelAdapter, ChannelCapabilities, GatewayApprovalPrompt, GatewayInboundDispatch,
    GatewayOutbox, GatewayRoute, OutboundMessage, StreamMessageHandle, TypingEvent,
};
use anyhow::Result;
use serde_json::json;
use uuid::Uuid;

pub(in crate::gateway) struct WebhookAdapter {
    outbox: GatewayOutbox,
}

impl WebhookAdapter {
    pub(in crate::gateway) fn new(outbox: GatewayOutbox) -> Self {
        Self { outbox }
    }
}

impl ChannelAdapter for WebhookAdapter {
    fn start(&self, _inbound: GatewayInboundDispatch) -> Result<()> {
        Ok(())
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        self.outbox
            .push(route, "message", serde_json::to_value(message)?);
        Ok(())
    }

    fn send_stream_start(
        &self,
        route: &GatewayRoute,
        text: &str,
    ) -> Result<Option<StreamMessageHandle>> {
        let handle = StreamMessageHandle {
            message_id: format!("stream_{}", Uuid::now_v7().simple()),
        };
        self.outbox.push(
            route,
            "stream_start",
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
            "stream_update",
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
            .push(route, "typing", serde_json::to_value(event)?);
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        self.outbox
            .push(route, "approval", serde_json::to_value(prompt)?);
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
