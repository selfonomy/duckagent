use super::super::{
    ChannelAdapter, ChannelCapabilities, GatewayApprovalPrompt, GatewayInboundDispatch,
    GatewayOutbox, GatewayRoute, OutboundMessage, TypingEvent,
};
use anyhow::Result;

pub(in crate::gateway) struct ApiServerAdapter {
    outbox: GatewayOutbox,
}

impl ApiServerAdapter {
    pub(in crate::gateway) fn new(outbox: GatewayOutbox) -> Self {
        Self { outbox }
    }
}

impl ChannelAdapter for ApiServerAdapter {
    fn start(&self, _inbound: GatewayInboundDispatch) -> Result<()> {
        Ok(())
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        self.outbox
            .push(route, "message", serde_json::to_value(message)?);
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
