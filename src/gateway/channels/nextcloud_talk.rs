use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundAttachmentInput,
    InboundMessageInput, OutboundMessage, TypingEvent,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use reqwest::blocking::{Client, RequestBuilder};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const NEXTCLOUD_TALK_CHANNEL: &str = "nextcloud-talk";
const DEFAULT_TRANSPORT: &str = "nextcloud_talk_ocs";
const DEFAULT_SEND_ENDPOINT: &str = "/send";
const DEFAULT_TYPING_ENDPOINT: &str = "/typing";
const NEXTCLOUD_TEXT_LIMIT: usize = 3_900;

#[derive(Clone)]
pub(in crate::gateway) struct NextcloudTalkAdapter {
    transport: String,
    server_url: Option<String>,
    bridge_base: Option<String>,
    token: Option<String>,
    username: Option<String>,
    app_password: Option<String>,
    webhook_secret: Option<String>,
    bot_username: Option<String>,
    allowed_users: HashSet<String>,
    allowed_chats: HashSet<String>,
    max_download_bytes: u64,
    send_endpoint: String,
    typing_endpoint: Option<String>,
    client: Client,
    seen_message_ids: Arc<Mutex<VecDeque<String>>>,
}

impl NextcloudTalkAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .context("failed to build Nextcloud Talk HTTP client")?;
        let transport = config
            .transport
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_TRANSPORT)
            .to_string();
        let uses_bridge = is_nextcloud_bridge_transport(&transport);
        let server_url = config
            .extra
            .get("server_url")
            .cloned()
            .or_else(|| credentials.extra.get("server_url").cloned())
            .or_else(|| {
                if uses_bridge {
                    None
                } else {
                    config.api_base.clone()
                }
            })
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty());
        let bridge_base = if uses_bridge {
            config
                .api_base
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.trim_end_matches('/').to_string())
        } else {
            None
        };
        let username = credentials
            .username
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        Ok(Self {
            transport,
            server_url,
            bridge_base,
            token: credentials.token.clone().or(credentials.api_key.clone()),
            username: username.clone(),
            app_password: credentials
                .password
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .or_else(|| credentials.extra.get("app_password").cloned()),
            webhook_secret: credentials
                .webhook_secret
                .clone()
                .or_else(|| credentials.signing_secret.clone()),
            bot_username: username,
            allowed_users: config.allowed_users.iter().cloned().collect(),
            allowed_chats: config.allowed_chats.iter().cloned().collect(),
            max_download_bytes: config.media.max_download_bytes,
            send_endpoint: config
                .extra
                .get("send_endpoint")
                .cloned()
                .unwrap_or_else(|| DEFAULT_SEND_ENDPOINT.to_string()),
            typing_endpoint: config
                .extra
                .get("typing_endpoint")
                .cloned()
                .or_else(|| Some(DEFAULT_TYPING_ENDPOINT.to_string())),
            client,
            seen_message_ids: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    fn handle_talk_event(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        if !self.verify_webhook(&request) {
            return Ok(json_response(401, json!({"error": "unauthorized"})));
        }
        let value: Value = serde_json::from_slice(&request.body)
            .context("failed to parse Nextcloud Talk event JSON")?;
        let events = value["events"]
            .as_array()
            .or_else(|| value["messages"].as_array())
            .cloned()
            .unwrap_or_else(|| vec![value]);
        for event in &events {
            if let Some(input) = self.event_to_inbound(event)? {
                inbound.submit(input)?;
            }
        }
        Ok(json_response(200, json!({"ok": true})))
    }

    fn uses_bridge_transport(&self) -> bool {
        is_nextcloud_bridge_transport(&self.transport)
    }

    fn event_to_inbound(&self, event: &Value) -> Result<Option<InboundMessageInput>> {
        let body = talk_event_body(event);
        let conversation_id = talk_conversation_id(body)
            .ok_or_else(|| anyhow!("Nextcloud Talk event missing room/conversation token"))?;
        if !self.allowed_chats.is_empty()
            && !self.allowed_chats.contains("*")
            && !self.allowed_chats.contains(&conversation_id)
        {
            return Ok(None);
        }
        let sender_id = first_str(
            body,
            &[
                "actor_id",
                "actorId",
                "actor.id",
                "actor.id",
                "sender_id",
                "user_id",
                "from",
                "author_id",
                "actorDisplayName",
                "actor_display_name",
            ],
        );
        if first_str(body, &["actor_type", "actor.type", "actorType"]).is_some_and(|value| {
            value.eq_ignore_ascii_case("application") || value.eq_ignore_ascii_case("bot")
        }) {
            return Ok(None);
        }
        if let Some(bot_username) = self.bot_username.as_deref() {
            if sender_id.is_some_and(|value| value.eq_ignore_ascii_case(bot_username))
                || first_str(
                    body,
                    &[
                        "actor_name",
                        "actor.name",
                        "actorDisplayName",
                        "actor_display_name",
                    ],
                )
                .is_some_and(|value| value.eq_ignore_ascii_case(bot_username))
            {
                return Ok(None);
            }
        }
        if let Some(sender_id) = sender_id {
            if !self.allowed_users.is_empty()
                && !self.allowed_users.contains("*")
                && !self.allowed_users.contains(sender_id)
            {
                return Ok(None);
            }
        }
        let message_id = first_str(
            body,
            &[
                "message_id",
                "messageId",
                "event_id",
                "id",
                "message.id",
                "token",
            ],
        )
        .map(str::to_string);
        if let Some(message_id) = message_id.as_deref() {
            if self.is_duplicate(message_id) {
                return Ok(None);
            }
        }
        let mut text = first_str(body, &["message", "text", "body", "content"])
            .unwrap_or_default()
            .to_string();
        if let Some(reply) = first_str(
            body,
            &[
                "parent_message",
                "parent.message",
                "parent.text",
                "reply_to_text",
                "quoted_text",
            ],
        ) {
            if !reply.trim().is_empty() {
                text = format!("[replying to]\n{reply}\n\n{text}");
            }
        }
        let attachments = self.parse_attachments(body);
        if text.trim().is_empty() && attachments.is_empty() {
            return Ok(None);
        }
        Ok(Some(InboundMessageInput {
            channel: NEXTCLOUD_TALK_CHANNEL.to_string(),
            conversation_id,
            thread_id: first_str(
                body,
                &[
                    "thread_id",
                    "reply_to",
                    "parent_id",
                    "parent.id",
                    "parentMessageId",
                    "parent.message_id",
                ],
            )
            .map(str::to_string),
            chat_type: Some(talk_chat_type(body)),
            sender_id: sender_id.map(str::to_string),
            message_id,
            text: if text.trim().is_empty() {
                "[Nextcloud Talk attachment]".to_string()
            } else {
                text
            },
            attachments,
            timestamp: first_str(
                body,
                &[
                    "timestamp",
                    "created_at",
                    "datetime",
                    "creationDateTime",
                    "creation_date_time",
                ],
            )
            .map(str::to_string),
        }))
    }

    fn parse_attachments(&self, event: &Value) -> Vec<InboundAttachmentInput> {
        let mut out = Vec::new();
        for attachment in event["attachments"]
            .as_array()
            .or_else(|| event["files"].as_array())
            .into_iter()
            .flatten()
        {
            if let Some(input) = attachment_from_value(attachment) {
                out.push(input);
                continue;
            }
            if let Some(url) = first_str(attachment, &["url", "download_url", "preview_url"]) {
                match self.download_attachment(url, attachment) {
                    Ok(input) => out.push(input),
                    Err(error) => eprintln!("Nextcloud Talk attachment skipped: {error:#}"),
                }
            }
        }
        if let Some(parameters) = event["messageParameters"]
            .as_object()
            .or_else(|| event["message_parameters"].as_object())
        {
            for attachment in parameters.values() {
                if let Some(input) = attachment_from_value(attachment) {
                    out.push(input);
                    continue;
                }
                if let Some(url) =
                    first_str(attachment, &["url", "link", "download_url", "preview_url"])
                {
                    match self.download_attachment(url, attachment) {
                        Ok(input) => out.push(input),
                        Err(error) => eprintln!("Nextcloud Talk attachment skipped: {error:#}"),
                    }
                }
            }
        }
        out
    }

    fn download_attachment(&self, url: &str, attachment: &Value) -> Result<InboundAttachmentInput> {
        let mut request = self.client.get(url);
        if self.uses_bridge_transport() {
            if let Some(token) = self.token.as_deref() {
                request = request.bearer_auth(token);
            }
        } else {
            request = self.apply_nextcloud_auth(request);
        }
        let response = request
            .send()
            .context("Nextcloud Talk attachment download failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("Nextcloud Talk attachment download failed with status {status}");
        }
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).to_string())
            .or_else(|| {
                first_str(attachment, &["mime", "mime_type", "content_type"]).map(str::to_string)
            });
        let bytes = response
            .bytes()
            .context("Nextcloud Talk attachment body unreadable")?;
        if self.max_download_bytes > 0 && bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "Nextcloud Talk attachment exceeds max_download_bytes ({})",
                self.max_download_bytes
            );
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: first_str(attachment, &["filename", "name", "file_name"])
                .map(str::to_string)
                .or_else(|| Some("nextcloud-talk-attachment.bin".to_string())),
            mime,
        })
    }

    fn apply_nextcloud_auth(&self, request: RequestBuilder) -> RequestBuilder {
        if let (Some(username), Some(app_password)) =
            (self.username.as_deref(), self.app_password.as_deref())
        {
            return request.basic_auth(username, Some(app_password));
        }
        if let Some(token) = self.token.as_deref() {
            return request.bearer_auth(token);
        }
        request
    }

    fn post_ocs_message(&self, room_token: &str, message: &str) -> Result<()> {
        if message.trim().is_empty() {
            return Ok(());
        }
        let server_url = self
            .server_url
            .as_deref()
            .ok_or_else(|| anyhow!("nextcloud-talk channel requires Nextcloud server URL"))?;
        let url = format!(
            "{}/ocs/v2.php/apps/spreed/api/v1/chat/{}?format=json",
            server_url.trim_end_matches('/'),
            encode_path_segment(room_token)
        );
        let request = self
            .client
            .post(url)
            .header("OCS-APIRequest", "true")
            .header("Accept", "application/json")
            .json(&json!({ "message": message }));
        let response = self
            .apply_nextcloud_auth(request)
            .send()
            .context("Nextcloud Talk OCS API POST failed")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            bail!("Nextcloud Talk OCS API POST failed with status {status}: {text}");
        }
        Ok(())
    }

    fn verify_webhook(&self, request: &ChannelHttpRequest) -> bool {
        let Some(secret) = self.webhook_secret.as_deref() else {
            return true;
        };
        let candidate = request
            .header("x-duckagent-gateway-secret")
            .or_else(|| request.header("x-nextcloud-talk-secret"))
            .or_else(|| request.query.get("secret").map(String::as_str));
        candidate.is_some_and(|value| constant_time_eq(value.as_bytes(), secret.as_bytes()))
    }

    fn is_duplicate(&self, message_id: &str) -> bool {
        if message_id.trim().is_empty() {
            return false;
        }
        let mut seen = self
            .seen_message_ids
            .lock()
            .expect("nextcloud talk seen message ids mutex poisoned");
        if seen.iter().any(|existing| existing == message_id) {
            return true;
        }
        seen.push_back(message_id.to_string());
        while seen.len() > 1000 {
            seen.pop_front();
        }
        false
    }

    fn post_bridge(&self, endpoint: &str, body: Value) -> Result<()> {
        let bridge_base = self
            .bridge_base
            .as_deref()
            .ok_or_else(|| anyhow!("nextcloud-talk channel requires bridge API URL"))?;
        let mut request = self
            .client
            .post(format!(
                "{}{}",
                bridge_base.trim_end_matches('/'),
                endpoint_path(endpoint)
            ))
            .json(&body);
        if let Some(token) = self.token.as_deref() {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .context("Nextcloud Talk bridge POST failed")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            bail!("Nextcloud Talk bridge POST failed with status {status}: {text}");
        }
        Ok(())
    }
}

impl ChannelAdapter for NextcloudTalkAdapter {
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
                "/nextcloud-talk/events" | "/nextcloud-talk/webhook" | "/nextcloud-talk-webhook"
            )
        {
            return self.handle_talk_event(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let conversation_id = route.key.conversation_id.as_str();
        let thread_id = route.key.thread_id.as_deref();
        let reply_to = message.reply_to.as_deref();
        for chunk in text_chunks(&message.text) {
            if self.uses_bridge_transport() {
                self.post_bridge(
                    &self.send_endpoint,
                    json!({
                        "channel": NEXTCLOUD_TALK_CHANNEL,
                        "room_token": conversation_id,
                        "conversation_id": conversation_id,
                        "thread_id": thread_id,
                        "reply_to": reply_to,
                        "message": chunk,
                        "text": chunk,
                        "media_paths": [],
                    }),
                )?;
            } else {
                self.post_ocs_message(conversation_id, &chunk)?;
            }
        }
        if !message.media_paths.is_empty() {
            if self.uses_bridge_transport() {
                self.post_bridge(
                    &self.send_endpoint,
                    json!({
                        "channel": NEXTCLOUD_TALK_CHANNEL,
                        "room_token": conversation_id,
                        "conversation_id": conversation_id,
                        "thread_id": thread_id,
                        "reply_to": reply_to,
                        "message": "",
                        "text": "",
                        "media_paths": &message.media_paths,
                        "media_mode": "nextcloud_talk_upload_or_share",
                    }),
                )?;
            } else {
                let media_text = format!(
                    "Generated media files:\n{}",
                    message
                        .media_paths
                        .iter()
                        .map(|path| format!("- {path}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                self.post_ocs_message(conversation_id, &media_text)?;
            }
        }
        Ok(())
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        if !self.uses_bridge_transport() {
            return Ok(());
        }
        let Some(endpoint) = self.typing_endpoint.as_deref() else {
            return Ok(());
        };
        self.post_bridge(
            endpoint,
            json!({
                "channel": NEXTCLOUD_TALK_CHANNEL,
                "room_token": route.key.conversation_id.as_str(),
                "conversation_id": route.key.conversation_id.as_str(),
                "thread_id": route.key.thread_id.as_deref(),
                "active": event.active,
                "reason": event.reason,
            }),
        )
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        let approval_id = prompt.id.clone();
        let approval_text = format!(
            "{}\n\nCommands:\n/approve {} once\n/approve {} session\n/approve {} always\n/deny {}",
            prompt.message,
            approval_id.as_str(),
            approval_id.as_str(),
            approval_id.as_str(),
            approval_id.as_str()
        );
        if self.uses_bridge_transport() {
            self.post_bridge(
                &self.send_endpoint,
                json!({
                    "channel": NEXTCLOUD_TALK_CHANNEL,
                    "room_token": route.key.conversation_id.as_str(),
                    "conversation_id": route.key.conversation_id.as_str(),
                    "thread_id": route.key.thread_id.as_deref(),
                    "message": approval_text,
                    "approval": {
                        "id": approval_id.as_str(),
                        "commands": [
                            format!("/approve {} once", approval_id.as_str()),
                            format!("/approve {} session", approval_id.as_str()),
                            format!("/approve {} always", approval_id.as_str()),
                            format!("/deny {}", approval_id.as_str())
                        ]
                    }
                }),
            )
        } else {
            self.post_ocs_message(route.key.conversation_id.as_str(), &approval_text)
        }
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: true,
            typing: self.uses_bridge_transport() && self.typing_endpoint.is_some(),
            approval_prompt: true,
        }
    }
}

pub(in crate::gateway::channels) fn new_adapter(
    config: &GatewayChannelConfig,
    credentials: &GatewayCredentialEntry,
) -> Result<NextcloudTalkAdapter> {
    NextcloudTalkAdapter::new(config, credentials)
}

fn talk_conversation_id(event: &Value) -> Option<String> {
    first_str(
        event,
        &[
            "room_token",
            "roomToken",
            "room.token",
            "room.id",
            "token",
            "conversation_id",
            "conversation.token",
            "conversation.id",
            "chat_id",
            "room_id",
        ],
    )
    .map(str::to_string)
}

fn talk_chat_type(event: &Value) -> String {
    first_str(
        event,
        &[
            "chat_type",
            "conversation_type",
            "room_type",
            "room.type",
            "type",
        ],
    )
    .map(|value| match value {
        "one2one" | "dm" | "direct" => "dm",
        "group" | "public" | "private" => "room",
        _ => "room",
    })
    .unwrap_or("room")
    .to_string()
}

fn attachment_from_value(value: &Value) -> Option<InboundAttachmentInput> {
    if let Some(path) = first_str(value, &["path", "file_path", "local_path"]) {
        return Some(InboundAttachmentInput {
            bytes: None,
            path: Some(path.to_string()),
            filename: first_str(value, &["filename", "name", "file_name"]).map(str::to_string),
            mime: first_str(value, &["mime", "mime_type", "content_type"]).map(str::to_string),
        });
    }
    if let Some(bytes) = first_str(value, &["bytes_base64", "base64"]) {
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(bytes) {
            return Some(InboundAttachmentInput {
                bytes: Some(decoded),
                path: None,
                filename: first_str(value, &["filename", "name", "file_name"]).map(str::to_string),
                mime: first_str(value, &["mime", "mime_type", "content_type"]).map(str::to_string),
            });
        }
    }
    None
}

fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        value[*key].as_str().or_else(|| {
            let pointer = format!("/{}", key.replace('.', "/"));
            value.pointer(&pointer).and_then(Value::as_str)
        })
    })
}

fn talk_event_body(event: &Value) -> &Value {
    event
        .pointer("/ocs/data")
        .or_else(|| event.get("data"))
        .or_else(|| event.get("payload"))
        .or_else(|| event.get("message"))
        .or_else(|| event.get("event"))
        .unwrap_or(event)
}

fn endpoint_path(endpoint: &str) -> String {
    if endpoint.starts_with('/') {
        endpoint.to_string()
    } else {
        format!("/{endpoint}")
    }
}

fn is_nextcloud_bridge_transport(transport: &str) -> bool {
    matches!(
        transport,
        "nextcloud_talk_bridge" | "bridge" | "webhook" | "http" | "http_callback" | "callback"
    )
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if matches!(
            byte,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~'
        ) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn text_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if current.len() + character.len_utf8() > NEXTCLOUD_TEXT_LIMIT && !current.is_empty() {
            chunks.push(current);
            current = String::new();
        }
        current.push(character);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
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

    #[test]
    fn nextcloud_talk_extracts_room_token() {
        assert_eq!(
            talk_conversation_id(&json!({"room_token": "abc"})).as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn nextcloud_talk_has_media_and_direct_approval_fallback() -> Result<()> {
        let adapter = new_adapter(
            &GatewayChannelConfig::default(),
            &GatewayCredentialEntry {
                channel: "nextcloud-talk".to_string(),
                ..Default::default()
            },
        )?;
        let capabilities = adapter.capabilities();
        assert!(capabilities.media);
        assert!(!capabilities.typing);
        assert!(capabilities.approval_prompt);
        Ok(())
    }
}
