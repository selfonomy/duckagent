use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundMessageInput,
    OutboundMessage, TypingEvent,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::time::Duration;

#[derive(Clone)]
pub(in crate::gateway) struct MsGraphWebhookAdapter {
    client_state: Option<String>,
    accepted_resources: HashSet<String>,
    api_base: Option<String>,
    token: Option<String>,
    prompt_template: Option<String>,
    max_seen_receipts: usize,
    seen_receipts: std::sync::Arc<std::sync::Mutex<VecDeque<String>>>,
    client: Client,
}

impl MsGraphWebhookAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build MS Graph webhook HTTP client")?;
        let mut accepted_resources = config.allowed_chats.iter().cloned().collect::<HashSet<_>>();
        if let Some(extra_resources) = config.extra.get("accepted_resources") {
            accepted_resources.extend(
                extra_resources
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
            );
        }
        Ok(Self {
            client_state: credentials
                .webhook_secret
                .as_deref()
                .or_else(|| credentials.extra.get("client_state").map(String::as_str))
                .map(str::to_string),
            accepted_resources,
            api_base: config.api_base.clone(),
            token: credentials.token.clone().or(credentials.api_key.clone()),
            prompt_template: config
                .extra
                .get("prompt")
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            max_seen_receipts: config
                .extra
                .get("max_seen_receipts")
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(5_000),
            seen_receipts: std::sync::Arc::new(std::sync::Mutex::new(VecDeque::new())),
            client,
        })
    }

    fn handle_notification(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        if let Some(token) = request.query.get("validationToken") {
            return Ok(ChannelHttpResponse {
                status: 200,
                content_type: "text/plain",
                body: token.as_bytes().to_vec(),
            });
        }
        if request.method == "GET" {
            return Ok(json_response(
                400,
                json!({"error": "missing validationToken"}),
            ));
        }
        let value: Value = serde_json::from_slice(&request.body)
            .context("failed to parse MS Graph webhook JSON")?;
        if !value["value"].is_array() {
            return Ok(json_response(
                400,
                json!({"error": "missing value notifications"}),
            ));
        }
        let mut accepted = 0usize;
        let mut duplicates = 0usize;
        let mut auth_rejected = 0usize;
        let mut other_rejected = 0usize;
        for notification in value["value"].as_array().into_iter().flatten() {
            if !notification.is_object() {
                other_rejected += 1;
                continue;
            }
            let accepted_resource = self.accept_resource(notification);
            let accepted_state = self.accept_client_state(notification);
            if !accepted_resource {
                other_rejected += 1;
                continue;
            }
            if !accepted_state {
                auth_rejected += 1;
                continue;
            }
            let receipt_key = self.receipt_key(notification);
            if receipt_key
                .as_deref()
                .is_some_and(|receipt_key| self.is_duplicate(receipt_key))
            {
                duplicates += 1;
                continue;
            }
            accepted += 1;
            let resource = notification["resource"]
                .as_str()
                .unwrap_or("msgraph")
                .to_string();
            let subscription_id = notification["subscriptionId"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let change_type = notification["changeType"].as_str().unwrap_or("updated");
            let resource_id = notification["resourceData"]["id"]
                .as_str()
                .or_else(|| notification["resourceData"]["@odata.id"].as_str())
                .unwrap_or_default();
            let text = self.render_prompt(notification, change_type, &resource, resource_id);
            inbound.submit(InboundMessageInput {
                channel: "msgraph_webhook".to_string(),
                conversation_id: if subscription_id.is_empty() {
                    "msgraph:unknown".to_string()
                } else {
                    format!("msgraph:{subscription_id}")
                },
                thread_id: if resource.is_empty() {
                    (!resource_id.is_empty()).then(|| resource_id.to_string())
                } else {
                    Some(resource.clone())
                },
                chat_type: Some("notification".to_string()),
                sender_id: Some("msgraph".to_string()),
                message_id: receipt_key,
                text,
                attachments: Vec::new(),
                timestamp: None,
            })?;
        }
        if accepted > 0 || duplicates > 0 {
            return Ok(empty_response(202));
        }
        if auth_rejected > 0 && other_rejected == 0 {
            return Ok(json_response(403, json!({"error": "clientState mismatch"})));
        }
        Ok(json_response(
            400,
            json!({"error": "no accepted notifications"}),
        ))
    }

    fn accept_notification(&self, value: &Value) -> bool {
        self.accept_client_state(value) && self.accept_resource(value)
    }

    fn accept_client_state(&self, value: &Value) -> bool {
        let Some(expected) = self.client_state.as_deref() else {
            return true;
        };
        let Some(provided) = value["clientState"].as_str() else {
            return false;
        };
        constant_time_eq(provided.as_bytes(), expected.as_bytes())
    }

    fn accept_resource(&self, value: &Value) -> bool {
        if self.accepted_resources.is_empty() {
            return true;
        }
        let resource = normalize_resource(value["resource"].as_str().unwrap_or_default());
        self.accepted_resources.iter().any(|allowed| {
            let allowed = normalize_resource(allowed);
            if allowed == "*" {
                return true;
            }
            if let Some(prefix) = allowed.strip_suffix("/*") {
                return resource == prefix || resource.starts_with(&format!("{prefix}/"));
            }
            resource == allowed || resource.starts_with(&format!("{allowed}/"))
        })
    }

    fn receipt_key(&self, notification: &Value) -> Option<String> {
        notification["id"]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|id| format!("id:{id}"))
    }

    fn is_duplicate(&self, receipt_key: &str) -> bool {
        let mut seen = self
            .seen_receipts
            .lock()
            .expect("msgraph seen receipts mutex poisoned");
        if seen.iter().any(|value| value == receipt_key) {
            return true;
        }
        seen.push_back(receipt_key.to_string());
        while seen.len() > self.max_seen_receipts {
            seen.pop_front();
        }
        false
    }

    fn render_prompt(
        &self,
        notification: &Value,
        change_type: &str,
        resource: &str,
        resource_id: &str,
    ) -> String {
        if let Some(template) = self.prompt_template.as_deref() {
            return render_template(
                template,
                notification,
                &[
                    ("change_type", change_type),
                    ("resource", resource),
                    ("resource_id", resource_id),
                    (
                        "subscription_id",
                        notification["subscriptionId"].as_str().unwrap_or_default(),
                    ),
                ],
            );
        }
        format!(
            "[MS Graph Notification]\nchange_type: {change_type}\nresource: {resource}\nresource_id: {resource_id}\n\nraw: {}",
            serde_json::to_string(notification).unwrap_or_default()
        )
    }

    fn send_bridge(&self, route: &GatewayRoute, message: &OutboundMessage) -> Result<()> {
        let Some(api_base) = self.api_base.as_deref() else {
            eprintln!("msgraph_webhook outbound bridge disabled; response not delivered");
            return Ok(());
        };
        let mut request = self
            .client
            .post(format!("{}/send", api_base.trim_end_matches('/')));
        if let Some(token) = self.token.as_deref() {
            request = request.bearer_auth(token);
        }
        let subscription = route.key.conversation_id.as_str();
        let resource = route.key.thread_id.as_deref().unwrap_or(subscription);
        let thread_id = route.key.thread_id.as_deref();
        let text = message.text.as_str();
        let response = request
            .json(&json!({
                "resource": resource,
                "subscription": subscription,
                "thread_id": thread_id,
                "conversation_id": route.key.conversation_id.as_str(),
                "text": text,
                "media_paths": &message.media_paths,
            }))
            .send()
            .context("msgraph webhook bridge send failed")?;
        if !response.status().is_success() {
            bail!(
                "msgraph webhook bridge send failed with status {}",
                response.status()
            );
        }
        Ok(())
    }
}

impl ChannelAdapter for MsGraphWebhookAdapter {
    fn start(&self, _inbound: GatewayInboundDispatch) -> Result<()> {
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        if matches!(
            request.path.as_str(),
            "/msgraph_webhook/events" | "/msgraph/events" | "/msgraph/webhook"
        ) {
            return self.handle_notification(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        self.send_bridge(route, &message)
    }

    fn send_typing(&self, _route: &GatewayRoute, _event: TypingEvent) -> Result<()> {
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        self.send_message(
            route,
            OutboundMessage {
                text: format!(
                    "{}\n\nCommands:\n/approve {} once\n/approve {} session\n/approve {} always\n/deny {}",
                    prompt.message, prompt.id, prompt.id, prompt.id, prompt.id
                ),
                media_paths: Vec::new(),
                reply_to: None,
                approval_prompt: Some(prompt),
                typing_event: None,
            },
        )
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: false,
            typing: false,
            approval_prompt: true,
        }
    }
}

fn json_response(status: u16, value: Value) -> ChannelHttpResponse {
    ChannelHttpResponse {
        status,
        content_type: "application/json",
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"ok\":false}".to_vec()),
    }
}

fn empty_response(status: u16) -> ChannelHttpResponse {
    ChannelHttpResponse {
        status,
        content_type: "text/plain",
        body: Vec::new(),
    }
}

fn normalize_resource(value: &str) -> String {
    value.trim().trim_matches('/').to_string()
}

fn render_template(template: &str, notification: &Value, fields: &[(&str, &str)]) -> String {
    let mut rendered = template.to_string();
    for (key, value) in fields {
        rendered = rendered.replace(&format!("{{{key}}}"), value);
    }
    rendered = rendered.replace(
        "{notification}",
        &serde_json::to_string(notification).unwrap_or_default(),
    );
    let re = regex::Regex::new(r"\{notification\.([a-zA-Z0-9_@.\-]+)\}")
        .expect("valid msgraph notification template regex");
    re.replace_all(&rendered, |captures: &regex::Captures<'_>| {
        let Some(path) = captures.get(1).map(|value| value.as_str()) else {
            return String::new();
        };
        notification_path(notification, path)
            .map(|value| match value {
                Value::String(text) => text.clone(),
                Value::Number(number) => number.to_string(),
                Value::Bool(value) => value.to_string(),
                Value::Null => String::new(),
                other => serde_json::to_string(other).unwrap_or_default(),
            })
            .unwrap_or_else(|| {
                captures
                    .get(0)
                    .map(|value| value.as_str())
                    .unwrap_or("")
                    .to_string()
            })
    })
    .to_string()
}

fn notification_path<'a>(notification: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = notification;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0u8, |acc, (left, right)| acc | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msgraph_accepts_matching_client_state() -> Result<()> {
        let adapter = MsGraphWebhookAdapter::new(
            &GatewayChannelConfig::default(),
            &GatewayCredentialEntry {
                channel: "msgraph_webhook".to_string(),
                webhook_secret: Some("state".to_string()),
                ..Default::default()
            },
        )?;
        assert!(
            adapter.accept_notification(&json!({"clientState": "state", "resource": "chats/1"}))
        );
        assert!(
            !adapter.accept_notification(&json!({"clientState": "bad", "resource": "chats/1"}))
        );
        Ok(())
    }

    #[test]
    fn msgraph_validation_returns_plain_token() -> Result<()> {
        let adapter = MsGraphWebhookAdapter::new(
            &GatewayChannelConfig::default(),
            &GatewayCredentialEntry {
                channel: "msgraph_webhook".to_string(),
                ..Default::default()
            },
        )?;
        let response = adapter.handle_notification(
            ChannelHttpRequest {
                method: "POST".to_string(),
                path: "/msgraph/events".to_string(),
                query: [("validationToken".to_string(), "abc".to_string())].into(),
                headers: Vec::new(),
                body: Vec::new(),
            },
            GatewayInboundDispatch::new(|_| Ok(())),
        )?;
        assert_eq!(response.content_type, "text/plain");
        assert_eq!(response.body, b"abc");
        Ok(())
    }
}
