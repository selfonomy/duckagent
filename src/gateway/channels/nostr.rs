use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundAttachmentInput,
    InboundMessageInput, OutboundMessage, TypingEvent,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use nostr_sdk::prelude::{
    Client as NostrClient, EventBuilder, Filter, Keys, Kind, PublicKey, RelayPoolNotification, Tag,
    Timestamp,
};
use reqwest::blocking::Client as BlockingClient;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const NOSTR_CHANNEL: &str = "nostr";
const DEFAULT_SEND_ENDPOINT: &str = "/send";
const NOSTR_TEXT_LIMIT: usize = 4_000;
const NOSTR_RECONNECT_BACKOFF: &[u64] = &[2, 5, 10, 30, 60];

#[derive(Clone)]
pub(in crate::gateway) struct NostrAdapter {
    transport: String,
    private_key: Option<String>,
    bridge_base: Option<String>,
    token: Option<String>,
    webhook_secret: Option<String>,
    allowed_pubkeys: HashSet<String>,
    allowed_conversations: HashSet<String>,
    relay_urls: Vec<String>,
    max_download_bytes: u64,
    send_endpoint: String,
    client: BlockingClient,
    seen_event_ids: Arc<Mutex<VecDeque<String>>>,
    sender_protocols: Arc<Mutex<HashMap<String, NostrDirectProtocol>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NostrDirectProtocol {
    Nip04,
    Nip17,
}

impl NostrAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let client = BlockingClient::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .context("failed to build Nostr bridge HTTP client")?;
        Ok(Self {
            transport: config
                .transport
                .as_deref()
                .unwrap_or("relay")
                .trim()
                .to_ascii_lowercase(),
            private_key: credentials
                .password
                .clone()
                .or_else(|| credentials.extra.get("private_key").cloned())
                .or_else(|| credentials.extra.get("signer_secret").cloned()),
            bridge_base: config.api_base.clone(),
            token: credentials.token.clone().or(credentials.api_key.clone()),
            webhook_secret: credentials
                .webhook_secret
                .clone()
                .or_else(|| credentials.signing_secret.clone()),
            allowed_pubkeys: config
                .allowed_users
                .iter()
                .map(|value| normalize_nostr_id(value))
                .collect(),
            allowed_conversations: config
                .allowed_chats
                .iter()
                .map(|value| normalize_nostr_id(value))
                .collect(),
            relay_urls: split_csv(config.extra.get("relay_urls")),
            max_download_bytes: config.media.max_download_bytes,
            send_endpoint: config
                .extra
                .get("send_endpoint")
                .cloned()
                .unwrap_or_else(|| DEFAULT_SEND_ENDPOINT.to_string()),
            client,
            seen_event_ids: Arc::new(Mutex::new(VecDeque::new())),
            sender_protocols: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn uses_bridge_transport(&self) -> bool {
        matches!(
            self.transport.as_str(),
            "nostr_bridge" | "bridge" | "webhook" | "http" | "http_callback" | "callback"
        )
    }

    fn handle_nostr_event(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        if !self.verify_webhook(&request) {
            return Ok(json_response(401, json!({"error": "unauthorized"})));
        }
        let value: Value =
            serde_json::from_slice(&request.body).context("failed to parse Nostr event JSON")?;
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

    fn event_to_inbound(&self, event: &Value) -> Result<Option<InboundMessageInput>> {
        let body = nostr_event_body(event);
        let event_id = first_str(body, &["id", "event_id", "message_id"]).map(str::to_string);
        if let Some(event_id) = event_id.as_deref() {
            if self.is_duplicate(event_id) {
                return Ok(None);
            }
        }
        let pubkey = first_str(body, &["pubkey", "sender_pubkey", "from", "author"])
            .map(str::to_string)
            .unwrap_or_default();
        if !self.allowed_pubkeys.is_empty()
            && !self.allowed_pubkeys.contains("*")
            && !self.allowed_pubkeys.contains(&normalize_nostr_id(&pubkey))
        {
            return Ok(None);
        }
        let conversation_id = nostr_conversation_id(body, &pubkey, event_id.as_deref())
            .ok_or_else(|| anyhow!("Nostr event missing conversation id/pubkey/id"))?;
        if !self.allowed_conversations.is_empty()
            && !self.allowed_conversations.contains("*")
            && !self
                .allowed_conversations
                .contains(&normalize_nostr_id(&conversation_id))
        {
            return Ok(None);
        }
        let mut text = first_str(body, &["content", "text", "message", "body"])
            .unwrap_or_default()
            .to_string();
        let tag_text = nostr_tag_summary(body);
        if !tag_text.is_empty() {
            if !text.trim().is_empty() {
                text.push('\n');
            }
            text.push_str(&tag_text);
        }
        let media_from_text = nostr_media_urls(&text);
        let attachments = self.parse_attachments(body);
        let attachments = if media_from_text.is_empty() {
            attachments
        } else {
            let mut merged = attachments;
            for url in media_from_text {
                merged.push(InboundAttachmentInput {
                    bytes: None,
                    path: Some(url),
                    filename: None,
                    mime: None,
                });
            }
            merged
        };
        if text.trim().is_empty() && attachments.is_empty() {
            return Ok(None);
        }
        Ok(Some(InboundMessageInput {
            channel: NOSTR_CHANNEL.to_string(),
            conversation_id: conversation_id.clone(),
            thread_id: first_e_tag(body).map(str::to_string),
            chat_type: Some(nostr_chat_type(body, &conversation_id)),
            sender_id: (!pubkey.is_empty()).then_some(pubkey),
            message_id: event_id,
            text: if text.trim().is_empty() {
                "[Nostr attachment]".to_string()
            } else {
                text
            },
            attachments,
            timestamp: first_str(body, &["created_at", "timestamp", "time"]).map(str::to_string),
        }))
    }

    fn parse_attachments(&self, event: &Value) -> Vec<InboundAttachmentInput> {
        let mut out = Vec::new();
        for attachment in event["attachments"]
            .as_array()
            .or_else(|| event["media"].as_array())
            .or_else(|| event["files"].as_array())
            .into_iter()
            .flatten()
        {
            if let Some(input) = attachment_from_value(attachment) {
                out.push(input);
                continue;
            }
            if let Some(url) = first_str(attachment, &["url", "download_url", "media_url"]) {
                match self.download_attachment(url, attachment) {
                    Ok(input) => out.push(input),
                    Err(error) => eprintln!("Nostr attachment skipped: {error:#}"),
                }
            }
        }
        out
    }

    fn download_attachment(&self, url: &str, attachment: &Value) -> Result<InboundAttachmentInput> {
        let mut request = self.client.get(url);
        if let Some(token) = self.token.as_deref() {
            request = request.bearer_auth(token);
        }
        let response = request.send().context("Nostr attachment download failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("Nostr attachment download failed with status {status}");
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
            .context("Nostr attachment body unreadable")?;
        if self.max_download_bytes > 0 && bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "Nostr attachment exceeds max_download_bytes ({})",
                self.max_download_bytes
            );
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: first_str(attachment, &["filename", "name", "file_name"])
                .map(str::to_string)
                .or_else(|| Some("nostr-attachment.bin".to_string())),
            mime,
        })
    }

    fn verify_webhook(&self, request: &ChannelHttpRequest) -> bool {
        let Some(secret) = self.webhook_secret.as_deref() else {
            return true;
        };
        let candidate = request
            .header("x-duckagent-gateway-secret")
            .or_else(|| request.header("x-nostr-secret"))
            .or_else(|| request.query.get("secret").map(String::as_str));
        candidate.is_some_and(|value| constant_time_eq(value.as_bytes(), secret.as_bytes()))
    }

    fn is_duplicate(&self, event_id: &str) -> bool {
        if event_id.trim().is_empty() {
            return false;
        }
        let mut seen = self
            .seen_event_ids
            .lock()
            .expect("nostr seen event ids mutex poisoned");
        if seen.iter().any(|existing| existing == event_id) {
            return true;
        }
        seen.push_back(event_id.to_string());
        while seen.len() > 1000 {
            seen.pop_front();
        }
        false
    }

    fn post_bridge(&self, endpoint: &str, body: Value) -> Result<()> {
        let bridge_base = self
            .bridge_base
            .as_deref()
            .ok_or_else(|| anyhow!("nostr channel requires bridge API URL"))?;
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
        let response = request.send().context("Nostr bridge POST failed")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            bail!("Nostr bridge POST failed with status {status}: {text}");
        }
        Ok(())
    }

    fn direct_runtime(&self) -> Result<tokio::runtime::Runtime> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build Nostr async runtime")
    }

    async fn direct_client(&self) -> Result<NostrClient> {
        let private_key = self
            .private_key
            .as_deref()
            .ok_or_else(|| anyhow!("nostr relay transport requires nsec/private key"))?;
        if self.relay_urls.is_empty() {
            bail!("nostr relay transport requires at least one relay URL");
        }
        let keys = Keys::parse(private_key).context("invalid Nostr private key/nsec")?;
        let client = NostrClient::builder().signer(keys).build();
        for relay in &self.relay_urls {
            client
                .add_relay(relay)
                .await
                .with_context(|| format!("failed to add Nostr relay {relay}"))?;
        }
        client.connect().await;
        Ok(client)
    }

    fn send_direct_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let mut chunks = text_chunks(&message.text);
        if !message.media_paths.is_empty() {
            let media_text = message.media_paths.join("\n");
            if let Some(last) = chunks.last_mut() {
                if !last.trim().is_empty() {
                    last.push('\n');
                }
                last.push_str(&media_text);
            } else {
                chunks.push(media_text);
            }
        }
        if chunks.is_empty() {
            return Ok(());
        }
        let recipient = nostr_route_pubkey(&route.key.conversation_id)?;
        let runtime = self.direct_runtime()?;
        runtime.block_on(async {
            let client = self.direct_client().await?;
            for chunk in chunks {
                self.send_direct_chunk(&client, recipient, &chunk).await?;
            }
            client.shutdown().await;
            Ok::<(), anyhow::Error>(())
        })
    }

    async fn send_direct_chunk(
        &self,
        client: &NostrClient,
        recipient: PublicKey,
        content: &str,
    ) -> Result<()> {
        let protocol = self
            .sender_protocols
            .lock()
            .expect("nostr sender protocol mutex poisoned")
            .get(&recipient.to_hex())
            .copied()
            .unwrap_or(NostrDirectProtocol::Nip17);
        match protocol {
            NostrDirectProtocol::Nip17 => {
                client
                    .send_private_msg(recipient, content, None)
                    .await
                    .context("failed to send Nostr NIP-17 private message")?;
            }
            NostrDirectProtocol::Nip04 => {
                let signer = client
                    .signer()
                    .await
                    .context("Nostr client missing signer")?;
                let encrypted = signer
                    .nip04_encrypt(&recipient, content)
                    .await
                    .context("failed to encrypt Nostr NIP-04 message")?;
                let builder = EventBuilder::new(Kind::EncryptedDirectMessage, encrypted)
                    .tag(Tag::public_key(recipient));
                client
                    .send_event_builder(builder)
                    .await
                    .context("failed to send Nostr NIP-04 message")?;
            }
        }
        Ok(())
    }

    fn direct_listen_loop(self, inbound: GatewayInboundDispatch) {
        let mut attempt = 0usize;
        loop {
            let result = self
                .direct_runtime()
                .and_then(|runtime| runtime.block_on(self.direct_listen_once(&inbound)));
            match result {
                Ok(()) => attempt = 0,
                Err(error) => eprintln!("nostr relay listener disconnected: {error:#}"),
            }
            let sleep = NOSTR_RECONNECT_BACKOFF
                .get(attempt)
                .copied()
                .unwrap_or(*NOSTR_RECONNECT_BACKOFF.last().unwrap_or(&60));
            attempt = attempt.saturating_add(1);
            thread::sleep(Duration::from_secs(sleep));
        }
    }

    async fn direct_listen_once(&self, inbound: &GatewayInboundDispatch) -> Result<()> {
        let private_key = self
            .private_key
            .as_deref()
            .ok_or_else(|| anyhow!("nostr relay transport requires nsec/private key"))?;
        let public_key = Keys::parse(private_key)
            .context("invalid Nostr private key/nsec")?
            .public_key();
        let client = self.direct_client().await?;
        let listen_start = Timestamp::now();
        let filter = Filter::new()
            .pubkey(public_key)
            .kinds(vec![Kind::EncryptedDirectMessage, Kind::GiftWrap])
            .limit(10);
        client
            .subscribe(filter, None)
            .await
            .context("failed to subscribe to Nostr private messages")?;
        let signer = client
            .signer()
            .await
            .context("Nostr client missing signer")?;
        loop {
            let notification = client
                .notifications()
                .recv()
                .await
                .context("Nostr notification channel closed")?;
            let Some((event_id, sender, content, timestamp, protocol)) = (match notification {
                RelayPoolNotification::Event { event, .. } => match event.kind {
                    Kind::EncryptedDirectMessage => {
                        if event.created_at < listen_start {
                            None
                        } else if !self.direct_pubkey_allowed(&event.pubkey.to_hex()) {
                            None
                        } else {
                            match signer.nip04_decrypt(&event.pubkey, &event.content).await {
                                Ok(content) => Some((
                                    event.id.to_hex(),
                                    event.pubkey.to_hex(),
                                    content,
                                    event.created_at.as_secs(),
                                    NostrDirectProtocol::Nip04,
                                )),
                                Err(error) => {
                                    eprintln!("nostr NIP-04 decrypt failed: {error:#}");
                                    None
                                }
                            }
                        }
                    }
                    Kind::GiftWrap => match client.unwrap_gift_wrap(&event).await {
                        Ok(unwrapped) => {
                            let rumor = unwrapped.rumor;
                            if rumor.created_at < listen_start {
                                None
                            } else if !self.direct_pubkey_allowed(&rumor.pubkey.to_hex()) {
                                None
                            } else {
                                Some((
                                    event.id.to_hex(),
                                    rumor.pubkey.to_hex(),
                                    rumor.content.clone(),
                                    rumor.created_at.as_secs(),
                                    NostrDirectProtocol::Nip17,
                                ))
                            }
                        }
                        Err(error) => {
                            eprintln!("nostr NIP-17 unwrap failed: {error:#}");
                            None
                        }
                    },
                    _ => None,
                },
                RelayPoolNotification::Shutdown => break Ok(()),
                RelayPoolNotification::Message { .. } => None,
            }) else {
                continue;
            };
            self.sender_protocols
                .lock()
                .expect("nostr sender protocol mutex poisoned")
                .insert(sender.clone(), protocol);
            if self.is_duplicate(&event_id) {
                continue;
            }
            inbound.submit(InboundMessageInput {
                channel: NOSTR_CHANNEL.to_string(),
                conversation_id: format!("dm:{sender}"),
                thread_id: None,
                chat_type: Some("dm".to_string()),
                sender_id: Some(sender),
                message_id: Some(event_id),
                text: content,
                attachments: Vec::new(),
                timestamp: chrono::DateTime::from_timestamp(timestamp as i64, 0)
                    .map(|value| value.to_rfc3339()),
            })?;
        }
    }

    fn direct_pubkey_allowed(&self, pubkey: &str) -> bool {
        self.allowed_pubkeys.is_empty()
            || self.allowed_pubkeys.contains("*")
            || self.allowed_pubkeys.contains(&normalize_nostr_id(pubkey))
    }
}

impl ChannelAdapter for NostrAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        if !self.uses_bridge_transport() {
            let adapter = self.clone();
            thread::spawn(move || adapter.direct_listen_loop(inbound));
        }
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        if self.uses_bridge_transport()
            && request.method == "POST"
            && matches!(request.path.as_str(), "/nostr/events" | "/nostr/webhook")
        {
            return self.handle_nostr_event(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        if !self.uses_bridge_transport() {
            return self.send_direct_message(route, message);
        }
        let conversation_id = route.key.conversation_id.as_str();
        let thread_id = route.key.thread_id.as_deref();
        let reply_to = message.reply_to.as_deref();
        for chunk in text_chunks(&message.text) {
            self.post_bridge(
                &self.send_endpoint,
                json!({
                    "channel": NOSTR_CHANNEL,
                    "conversation_id": conversation_id,
                    "chat_type": nostr_route_chat_type(conversation_id),
                    "thread_id": thread_id,
                    "reply_to": reply_to,
                    "kind": nostr_outbound_kind(conversation_id),
                    "tags": nostr_outbound_tags(conversation_id, thread_id),
                    "relay_urls": self.relay_urls.clone(),
                    "content": chunk,
                    "text": chunk,
                    "media_paths": [],
                }),
            )?;
        }
        if !message.media_paths.is_empty() {
            self.post_bridge(
                &self.send_endpoint,
                json!({
                    "channel": NOSTR_CHANNEL,
                    "conversation_id": conversation_id,
                    "chat_type": nostr_route_chat_type(conversation_id),
                    "thread_id": thread_id,
                    "kind": nostr_outbound_kind(conversation_id),
                    "tags": nostr_outbound_tags(conversation_id, thread_id),
                    "relay_urls": self.relay_urls.clone(),
                    "content": "",
                    "text": "",
                    "media_paths": &message.media_paths,
                    "media_mode": "nostr_upload_or_link",
                }),
            )?;
        }
        Ok(())
    }

    fn send_typing(&self, _route: &GatewayRoute, _event: TypingEvent) -> Result<()> {
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        if !self.uses_bridge_transport() {
            return self.send_direct_message(
                route,
                OutboundMessage {
                    text: format!(
                        "{}\n\nCommands:\n/approve {} once\n/approve {} session\n/approve {} always\n/deny {}",
                        prompt.message, prompt.id, prompt.id, prompt.id, prompt.id
                    ),
                    media_paths: Vec::new(),
                    reply_to: None,
                    approval_prompt: None,
                    typing_event: None,
                },
            );
        }
        let conversation_id = route.key.conversation_id.as_str();
        let thread_id = route.key.thread_id.as_deref();
        let approval_id = prompt.id.clone();
        let approval_text = format!(
            "{}\n\nCommands:\n/approve {} once\n/approve {} session\n/approve {} always\n/deny {}",
            prompt.message,
            approval_id.as_str(),
            approval_id.as_str(),
            approval_id.as_str(),
            approval_id.as_str()
        );
        self.post_bridge(
            &self.send_endpoint,
            json!({
                "channel": NOSTR_CHANNEL,
                "conversation_id": conversation_id,
                "chat_type": nostr_route_chat_type(conversation_id),
                "thread_id": thread_id,
                "kind": nostr_outbound_kind(conversation_id),
                "tags": nostr_outbound_tags(conversation_id, thread_id),
                "relay_urls": self.relay_urls.clone(),
                "content": approval_text,
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
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: true,
            typing: false,
            approval_prompt: true,
        }
    }
}

pub(in crate::gateway::channels) fn new_adapter(
    config: &GatewayChannelConfig,
    credentials: &GatewayCredentialEntry,
) -> Result<NostrAdapter> {
    NostrAdapter::new(config, credentials)
}

fn nostr_event_body(event: &Value) -> &Value {
    event
        .get("event")
        .or_else(|| event.pointer("/nostr/event"))
        .or_else(|| event.pointer("/relay/event"))
        .or_else(|| event.pointer("/subscription/event"))
        .or_else(|| event.pointer("/data/event"))
        .or_else(|| event.get("message"))
        .or_else(|| event.get("payload"))
        .unwrap_or(event)
}

fn nostr_conversation_id(event: &Value, pubkey: &str, event_id: Option<&str>) -> Option<String> {
    first_str(
        event,
        &["conversation_id", "chat_id", "peer_id", "recipient_pubkey"],
    )
    .map(str::to_string)
    .or_else(|| {
        if !is_direct_kind(event) {
            return None;
        }
        if !pubkey.is_empty() {
            Some(format!("dm:{pubkey}"))
        } else {
            first_p_tag(event).map(|value| format!("dm:{value}"))
        }
    })
    .or_else(|| first_e_tag(event).map(|value| format!("event:{value}")))
    .or_else(|| event_id.map(|value| format!("event:{value}")))
    .or_else(|| first_p_tag(event).map(|value| format!("dm:{value}")))
}

fn nostr_chat_type(event: &Value, conversation_id: &str) -> String {
    if conversation_id.starts_with("dm:") || is_direct_kind(event) {
        "dm"
    } else {
        "event"
    }
    .to_string()
}

fn nostr_route_chat_type(conversation_id: &str) -> &'static str {
    if conversation_id.starts_with("dm:") {
        "dm"
    } else {
        "event"
    }
}

fn nostr_route_pubkey(conversation_id: &str) -> Result<PublicKey> {
    let pubkey = conversation_id
        .strip_prefix("dm:")
        .unwrap_or(conversation_id)
        .trim();
    PublicKey::parse(pubkey).with_context(|| {
        format!("Nostr relay transport can only send direct messages to pubkeys: {conversation_id}")
    })
}

fn is_direct_kind(event: &Value) -> bool {
    matches!(nostr_kind(event), Some(4 | 44 | 1059))
        || first_str(event, &["chat_type", "type"])
            .is_some_and(|value| matches!(value, "dm" | "direct"))
        || first_p_tag(event).is_some() && nostr_kind(event).is_none()
}

fn nostr_kind(event: &Value) -> Option<i64> {
    event["kind"]
        .as_i64()
        .or_else(|| event["kind"].as_str().and_then(|value| value.parse().ok()))
}

fn nostr_outbound_kind(conversation_id: &str) -> i64 {
    if conversation_id.starts_with("dm:") {
        4
    } else {
        1
    }
}

fn nostr_outbound_tags(conversation_id: &str, thread_id: Option<&str>) -> Vec<Vec<String>> {
    let mut tags = Vec::new();
    if let Some(pubkey) = conversation_id.strip_prefix("dm:") {
        tags.push(vec!["p".to_string(), pubkey.to_string()]);
    }
    if let Some(event_id) = conversation_id
        .strip_prefix("event:")
        .or(thread_id)
        .filter(|value| !value.is_empty())
    {
        tags.push(vec!["e".to_string(), event_id.to_string()]);
    }
    tags
}

fn first_p_tag(event: &Value) -> Option<&str> {
    first_tag_value(event, "p")
}

fn first_e_tag(event: &Value) -> Option<&str> {
    first_tag_value(event, "e")
}

fn first_tag_value<'a>(event: &'a Value, tag_name: &str) -> Option<&'a str> {
    for tag in event["tags"].as_array().into_iter().flatten() {
        let Some(items) = tag.as_array() else {
            continue;
        };
        if items.first().and_then(Value::as_str) == Some(tag_name) {
            if let Some(value) = items.get(1).and_then(Value::as_str) {
                return Some(value);
            }
        }
    }
    None
}

fn nostr_tag_summary(event: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(subject) = first_tag_value(event, "subject") {
        parts.push(format!("[Nostr subject: {subject}]"));
    }
    if let Some(event_id) = first_e_tag(event) {
        parts.push(format!("[Nostr reply/event: {event_id}]"));
    }
    for tag in event["tags"].as_array().into_iter().flatten() {
        let Some(items) = tag.as_array() else {
            continue;
        };
        if items.first().and_then(Value::as_str) == Some("r") {
            if let Some(url) = items.get(1).and_then(Value::as_str) {
                parts.push(url.to_string());
            }
        }
    }
    parts.join("\n")
}

fn nostr_media_urls(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|token| {
            let trimmed = token.trim_matches(|ch: char| {
                matches!(
                    ch,
                    '"' | '\'' | '(' | ')' | '[' | ']' | '<' | '>' | ',' | '.'
                )
            });
            let lower = trimmed
                .split(['?', '#'])
                .next()
                .unwrap_or(trimmed)
                .to_ascii_lowercase();
            let is_media = matches!(
                lower.rsplit('.').next(),
                Some(
                    "jpg"
                        | "jpeg"
                        | "png"
                        | "gif"
                        | "webp"
                        | "mp4"
                        | "webm"
                        | "mov"
                        | "mp3"
                        | "m4a"
                        | "ogg"
                        | "wav"
                        | "pdf"
                )
            );
            (trimmed.starts_with("https://") && is_media).then(|| trimmed.to_string())
        })
        .collect()
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
    keys.iter().find_map(|key| value[*key].as_str())
}

fn split_csv(value: Option<&String>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn normalize_nostr_id(value: &str) -> String {
    let mut value = value.trim().to_ascii_lowercase();
    for prefix in [
        "nostr:",
        "pubkey:",
        "npub:",
        "nprofile:",
        "dm:",
        "event:",
        "note:",
        "nevent:",
        "naddr:",
    ] {
        if let Some(stripped) = value.strip_prefix(prefix) {
            value = stripped.to_string();
        }
    }
    value
}

fn endpoint_path(endpoint: &str) -> String {
    if endpoint.starts_with('/') {
        endpoint.to_string()
    } else {
        format!("/{endpoint}")
    }
}

fn text_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if current.len() + character.len_utf8() > NOSTR_TEXT_LIMIT && !current.is_empty() {
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
    fn nostr_dm_event_routes_to_pubkey() {
        assert_eq!(
            nostr_conversation_id(
                &json!({"kind": 4, "pubkey": "abc", "content": "hi"}),
                "abc",
                Some("event1")
            )
            .as_deref(),
            Some("dm:abc")
        );
    }

    #[test]
    fn nostr_event_reply_uses_e_tag() {
        assert_eq!(first_e_tag(&json!({"tags": [["e", "root"]]})), Some("root"));
    }
}
