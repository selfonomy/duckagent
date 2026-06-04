use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundAttachmentInput,
    InboundMessageInput, OutboundMessage, StreamMessageHandle, TypingEvent,
};
use super::websocket::{
    is_transient_read_error, read_json_message as read_ws_json_message,
    send_json_message as send_ws_json_message, set_read_timeout,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::{Client, multipart};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tungstenite::connect;
use url::form_urlencoded;

const DEFAULT_SLACK_API_BASE: &str = "https://slack.com/api";
const SLACK_EVENTS_PATH: &str = "/slack/events";
const SLACK_INTERACTIONS_PATH: &str = "/slack/interactions";
const SLACK_TEXT_LIMIT: usize = 39_000;
const SLACK_SIGNATURE_TOLERANCE_SECONDS: i64 = 60 * 5;

#[derive(Clone)]
pub(in crate::gateway) struct SlackAdapter {
    bot_token: String,
    app_token: Option<String>,
    signing_secret: Option<String>,
    bot_user_id: Option<String>,
    api_base: String,
    transport: String,
    require_mention: bool,
    allowed_users: Vec<String>,
    allowed_chats: Vec<String>,
    free_response_chats: HashSet<String>,
    max_download_bytes: u64,
    mentioned_threads: Arc<Mutex<HashSet<String>>>,
    seen_events: Arc<Mutex<HashSet<String>>>,
    client: Client,
}

impl SlackAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let bot_token = credentials
            .token
            .as_deref()
            .or(credentials.api_key.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("slack gateway credential requires bot token"))?
            .to_string();
        let app_token = credentials
            .extra
            .get("app_token")
            .or(credentials.client_secret.as_ref())
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let signing_secret = credentials
            .signing_secret
            .as_deref()
            .or(credentials.webhook_secret.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let bot_user_id = credentials
            .extra
            .get("bot_user_id")
            .or_else(|| config.extra.get("bot_user_id"))
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let transport = config
            .transport
            .as_deref()
            .unwrap_or("socket_mode")
            .trim()
            .to_ascii_lowercase();
        let require_mention = config
            .extra
            .get("require_mention")
            .map(|value| {
                !matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "false" | "0" | "no"
                )
            })
            .unwrap_or(true);
        if matches!(transport.as_str(), "socket_mode" | "socket") && app_token.is_none() {
            bail!("slack socket_mode requires an app-level token (xapp-...) from gateway setup");
        }
        if matches!(transport.as_str(), "events_api" | "http" | "webhook")
            && signing_secret.is_none()
        {
            bail!("slack HTTP Events API transport requires a signing secret");
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build Slack HTTP client")?;
        let mut allowed_chats = config.allowed_chats.clone();
        allowed_chats.extend(split_slack_csv(config.extra.get("allowed_channels")));
        Ok(Self {
            bot_token,
            app_token,
            signing_secret,
            bot_user_id,
            api_base: config
                .api_base
                .clone()
                .unwrap_or_else(|| DEFAULT_SLACK_API_BASE.to_string()),
            transport,
            require_mention,
            allowed_users: config.allowed_users.clone(),
            allowed_chats,
            free_response_chats: slack_extra_set(
                config,
                &["free_response_channels", "free_response_chats"],
            ),
            max_download_bytes: config.media.max_download_bytes,
            mentioned_threads: Arc::new(Mutex::new(HashSet::new())),
            seen_events: Arc::new(Mutex::new(HashSet::new())),
            client,
        })
    }

    fn api_url(&self, method: &str) -> String {
        format!("{}/{}", self.api_base.trim_end_matches('/'), method)
    }

    fn post_json<T: Serialize>(&self, method: &str, body: &T) -> Result<Value> {
        let response = self
            .client
            .post(self.api_url(method))
            .bearer_auth(&self.bot_token)
            .json(body)
            .send()
            .with_context(|| format!("slack {method} request failed"))?;
        let status = response.status();
        let value: Value = response
            .json()
            .with_context(|| format!("slack {method} returned invalid JSON"))?;
        if !status.is_success() || value.get("ok") == Some(&Value::Bool(false)) {
            bail!("slack {method} failed with status {status}: {value}");
        }
        Ok(value)
    }

    fn send_text_chunk(&self, route: &GatewayRoute, text: &str) -> Result<()> {
        self.send_text_chunk_with_response(route, text).map(|_| ())
    }

    fn send_text_chunk_with_response(&self, route: &GatewayRoute, text: &str) -> Result<Value> {
        let mut body = json!({
            "channel": route.key.conversation_id,
            "text": text,
            "mrkdwn": true,
            "unfurl_links": false,
            "unfurl_media": false,
        });
        if let Some(thread_ts) = route.key.thread_id.as_deref() {
            body["thread_ts"] = json!(thread_ts);
        }
        self.post_json("chat.postMessage", &body)
    }

    fn update_text_message(&self, route: &GatewayRoute, ts: &str, text: &str) -> Result<()> {
        let mut body = json!({
            "channel": route.key.conversation_id,
            "ts": ts,
            "text": text,
            "mrkdwn": true,
            "unfurl_links": false,
            "unfurl_media": false,
        });
        if let Some(thread_ts) = route.key.thread_id.as_deref() {
            body["thread_ts"] = json!(thread_ts);
        }
        self.post_json("chat.update", &body)?;
        Ok(())
    }

    fn send_media_path(
        &self,
        route: &GatewayRoute,
        path: &str,
        comment: Option<&str>,
    ) -> Result<()> {
        if path.starts_with("http://") || path.starts_with("https://") {
            let text = comment
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!("{value}\n{path}"))
                .unwrap_or_else(|| path.to_string());
            return self.send_text_chunk(route, &text);
        }
        let path = Path::new(path);
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read Slack upload file {}", path.display()))?;
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("duckagent-upload")
            .to_string();
        let upload = self.post_json(
            "files.getUploadURLExternal",
            &json!({
                "filename": filename,
                "length": bytes.len(),
            }),
        )?;
        let upload_url = upload["upload_url"]
            .as_str()
            .ok_or_else(|| anyhow!("slack files.getUploadURLExternal missing upload_url"))?;
        let file_id = upload["file_id"]
            .as_str()
            .ok_or_else(|| anyhow!("slack files.getUploadURLExternal missing file_id"))?;
        let part = multipart::Part::bytes(bytes).file_name(filename.clone());
        let upload_response = self
            .client
            .post(upload_url)
            .multipart(multipart::Form::new().part("file", part))
            .send()
            .context("slack external file upload failed")?;
        if !upload_response.status().is_success() {
            bail!(
                "slack external file upload failed with status {}",
                upload_response.status()
            );
        }
        let mut complete = json!({
            "files": [{"id": file_id, "title": filename}],
            "channel_id": route.key.conversation_id,
        });
        if let Some(comment) = comment.filter(|value| !value.trim().is_empty()) {
            complete["initial_comment"] = json!(comment);
        }
        if let Some(thread_ts) = route.key.thread_id.as_deref() {
            complete["thread_ts"] = json!(thread_ts);
        }
        self.post_json("files.completeUploadExternal", &complete)?;
        Ok(())
    }

    fn socket_mode_loop(self, inbound: GatewayInboundDispatch) {
        loop {
            if let Err(error) = self.consume_socket_mode_once(&inbound) {
                eprintln!("slack socket mode disconnected: {error:#}");
            }
            thread::sleep(Duration::from_secs(5));
        }
    }

    fn consume_socket_mode_once(&self, inbound: &GatewayInboundDispatch) -> Result<()> {
        let url = self.socket_mode_url()?;
        let (mut socket, _) =
            connect(url.as_str()).with_context(|| format!("slack socket mode connect: {url}"))?;
        set_read_timeout(&mut socket, Duration::from_secs(10));
        loop {
            match read_ws_json_message(
                &mut socket,
                "slack socket mode read failed",
                "slack socket mode message is not valid JSON",
            ) {
                Ok(Some(value)) => self.handle_socket_envelope(value, inbound, &mut socket)?,
                Ok(None) => {}
                Err(error) if is_transient_read_error(&error) => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn socket_mode_url(&self) -> Result<String> {
        let app_token = self
            .app_token
            .as_deref()
            .ok_or_else(|| anyhow!("slack socket mode requires app token"))?;
        let response = self
            .client
            .post(self.api_url("apps.connections.open"))
            .bearer_auth(app_token)
            .send()
            .context("slack apps.connections.open failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .context("slack apps.connections.open returned invalid JSON")?;
        if !status.is_success() || value.get("ok") == Some(&Value::Bool(false)) {
            bail!("slack apps.connections.open failed with status {status}: {value}");
        }
        value["url"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("slack apps.connections.open missing url"))
    }

    fn handle_socket_envelope(
        &self,
        value: Value,
        inbound: &GatewayInboundDispatch,
        socket: &mut super::websocket::ChannelWebSocket,
    ) -> Result<()> {
        let envelope: SlackSocketEnvelope =
            serde_json::from_value(value).context("failed to parse Slack socket envelope")?;
        if let Some(envelope_id) = envelope.envelope_id.as_deref() {
            send_ws_json_message(
                socket,
                &json!({"envelope_id": envelope_id}),
                "slack socket mode ack failed",
            )?;
        }
        let Some(payload) = envelope.payload else {
            return Ok(());
        };
        match envelope.kind.as_str() {
            "events_api" => {
                let event_envelope: SlackEventEnvelope = serde_json::from_value(payload)
                    .context("failed to parse Slack socket event payload")?;
                if event_envelope.kind == "event_callback" {
                    if let Some(event) = event_envelope.event {
                        if let Some(message) = self.event_to_inbound(event) {
                            inbound.submit(message)?;
                        }
                    }
                }
            }
            "interactive" => {
                let payload: SlackInteractionPayload = serde_json::from_value(payload)
                    .context("failed to parse Slack socket interaction payload")?;
                if let Some(message) = self.interaction_to_inbound(payload) {
                    inbound.submit(message)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_events(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        self.verify_signature(&request)?;
        let envelope: SlackEventEnvelope =
            serde_json::from_slice(&request.body).context("failed to parse Slack event JSON")?;
        if envelope.kind == "url_verification" {
            let challenge = envelope
                .challenge
                .ok_or_else(|| anyhow!("Slack url_verification missing challenge"))?;
            return Ok(text_response(200, challenge));
        }
        if envelope.kind == "event_callback" {
            if let Some(event) = envelope.event {
                if let Some(message) = self.event_to_inbound(event) {
                    inbound.submit(message)?;
                }
            }
            return Ok(json_response(200, json!({"ok": true})));
        }
        Ok(json_response(200, json!({"ok": true})))
    }

    fn handle_interaction(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        self.verify_signature(&request)?;
        let form = form_urlencoded::parse(&request.body)
            .into_owned()
            .collect::<Vec<_>>();
        let payload_text = form
            .iter()
            .find(|(key, _)| key == "payload")
            .map(|(_, value)| value.as_str())
            .ok_or_else(|| anyhow!("Slack interaction missing payload"))?;
        let payload: SlackInteractionPayload =
            serde_json::from_str(payload_text).context("failed to parse Slack interaction JSON")?;
        if let Some(message) = self.interaction_to_inbound(payload) {
            inbound.submit(message)?;
        }
        Ok(json_response(200, json!({"ok": true})))
    }

    fn verify_signature(&self, request: &ChannelHttpRequest) -> Result<()> {
        let signing_secret = self.signing_secret.as_deref().ok_or_else(|| {
            anyhow!("Slack HTTP request received but no signing secret is configured")
        })?;
        let timestamp = request
            .header("x-slack-request-timestamp")
            .ok_or_else(|| anyhow!("Slack request missing timestamp"))?
            .parse::<i64>()
            .context("Slack timestamp is not an integer")?;
        let now = chrono::Utc::now().timestamp();
        if (now - timestamp).abs() > SLACK_SIGNATURE_TOLERANCE_SECONDS {
            bail!("Slack request timestamp is outside tolerance");
        }
        let signature = request
            .header("x-slack-signature")
            .ok_or_else(|| anyhow!("Slack request missing signature"))?;
        let mut base = format!("v0:{timestamp}:").into_bytes();
        base.extend_from_slice(&request.body);
        let expected = format!("v0={}", hmac_sha256_hex(signing_secret, &base));
        if !constant_time_eq(signature.as_bytes(), expected.as_bytes()) {
            bail!("Slack request signature mismatch");
        }
        Ok(())
    }

    fn event_to_inbound(&self, event: SlackEvent) -> Option<InboundMessageInput> {
        if !matches!(event.kind.as_str(), "message" | "app_mention") {
            return None;
        }
        if event.bot_id.is_some() {
            return None;
        }
        if event
            .subtype
            .as_deref()
            .is_some_and(|subtype| subtype != "file_share")
        {
            return None;
        }
        if !identity_allowed(&self.allowed_chats, &event.channel, None) {
            return None;
        }
        if let Some(user) = event.user.as_deref() {
            if !identity_allowed(&self.allowed_users, user, None) {
                return None;
            }
        }
        let chat_type = slack_channel_type(&event.channel);
        let mut text = slack_event_text(&event);
        if !self.accepts_message_by_mention(&event, &text, &chat_type) {
            return None;
        }
        if !self.remember_event_once(&event) {
            return None;
        }
        text = slack_strip_bot_mention(&text, self.bot_user_id.as_deref());
        let (attachments, skipped) = self.collect_event_attachments(&event.files);
        for note in skipped {
            if !text.trim().is_empty() {
                text.push('\n');
            }
            text.push_str(&note);
        }
        let thread_id = slack_route_thread_id(&event, &chat_type);
        Some(InboundMessageInput {
            channel: "slack".to_string(),
            conversation_id: event.channel,
            thread_id,
            chat_type: Some(chat_type),
            sender_id: event.user,
            message_id: event.ts.clone(),
            text,
            attachments,
            timestamp: event.ts.and_then(|ts| slack_ts_to_rfc3339(&ts)),
        })
    }

    fn accepts_message_by_mention(&self, event: &SlackEvent, text: &str, chat_type: &str) -> bool {
        if chat_type == "dm"
            || !self.require_mention
            || set_allows(&self.free_response_chats, &event.channel)
        {
            return true;
        }
        let Some(thread_anchor) = slack_thread_anchor(event) else {
            return false;
        };
        let key = slack_thread_key(&event.channel, &thread_anchor);
        if self.event_mentions_bot(event, text) {
            if let Ok(mut threads) = self.mentioned_threads.lock() {
                if threads.len() > 5000 {
                    threads.clear();
                }
                threads.insert(key);
            }
            return true;
        }
        self.mentioned_threads
            .lock()
            .map(|threads| threads.contains(&key))
            .unwrap_or(false)
    }

    fn event_mentions_bot(&self, event: &SlackEvent, text: &str) -> bool {
        if event.kind == "app_mention" {
            return true;
        }
        self.bot_user_id
            .as_deref()
            .is_some_and(|bot_id| text.contains(&format!("<@{bot_id}>")))
    }

    fn remember_event_once(&self, event: &SlackEvent) -> bool {
        let Some(ts) = event.ts.as_deref() else {
            return true;
        };
        let key = format!("{}:{ts}", event.channel);
        self.seen_events
            .lock()
            .map(|mut seen| {
                if seen.contains(&key) {
                    return false;
                }
                if seen.len() > 10_000 {
                    seen.clear();
                }
                seen.insert(key);
                true
            })
            .unwrap_or(true)
    }

    fn interaction_to_inbound(
        &self,
        payload: SlackInteractionPayload,
    ) -> Option<InboundMessageInput> {
        let action = payload.actions.into_iter().next()?;
        if !identity_allowed(&self.allowed_chats, &payload.channel.id, None) {
            return None;
        }
        if !identity_allowed(&self.allowed_users, &payload.user.id, None) {
            return None;
        }
        let command = slack_action_to_command_from_action(&action)?;
        let chat_type = slack_channel_type(&payload.channel.id);
        let message_ts = payload
            .message
            .as_ref()
            .and_then(|message| message.ts.clone())
            .or_else(|| {
                payload
                    .container
                    .as_ref()
                    .and_then(|container| container.message_ts.clone())
            });
        let interaction_key = slack_interaction_key(
            &payload.channel.id,
            &payload.user.id,
            message_ts.as_deref(),
            &command,
        );
        if !self.remember_key_once(&interaction_key) {
            return None;
        }
        Some(InboundMessageInput {
            channel: "slack".to_string(),
            conversation_id: payload.channel.id,
            thread_id: payload
                .message
                .as_ref()
                .and_then(|message| message.thread_ts.clone())
                .or_else(|| {
                    payload
                        .container
                        .as_ref()
                        .and_then(|container| container.thread_ts.clone())
                }),
            chat_type: Some(chat_type),
            sender_id: Some(payload.user.id),
            message_id: message_ts,
            text: command,
            attachments: Vec::new(),
            timestamp: Some(now_rfc3339_like()),
        })
    }

    fn remember_key_once(&self, key: &str) -> bool {
        self.seen_events
            .lock()
            .map(|mut seen| {
                if seen.contains(key) {
                    return false;
                }
                if seen.len() > 10_000 {
                    seen.clear();
                }
                seen.insert(key.to_string());
                true
            })
            .unwrap_or(true)
    }

    fn collect_event_attachments(
        &self,
        files: &[SlackFile],
    ) -> (Vec<InboundAttachmentInput>, Vec<String>) {
        let mut attachments = Vec::new();
        let mut skipped = Vec::new();
        for file in files {
            match self.download_file(file) {
                Ok(attachment) => attachments.push(attachment),
                Err(error) => skipped.push(format!(
                    "[Slack file skipped: id={}, reason={error:#}]",
                    file.id
                )),
            }
        }
        (attachments, skipped)
    }

    fn download_file(&self, file: &SlackFile) -> Result<InboundAttachmentInput> {
        let file = self.resolve_file_info(file)?;
        if let Some(size) = file.size {
            if size > self.max_download_bytes {
                bail!(
                    "Slack file is {size} bytes, over max_download_bytes {}",
                    self.max_download_bytes
                );
            }
        }
        let url = file
            .url_private_download
            .as_deref()
            .or(file.url_private.as_deref())
            .ok_or_else(|| anyhow!("Slack file has no private download URL"))?;
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.bot_token)
            .send()
            .context("Slack file download failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("Slack file download failed with status {status}");
        }
        if let Some(length) = response.content_length() {
            if length > self.max_download_bytes {
                bail!(
                    "Slack file download is {length} bytes, over max_download_bytes {}",
                    self.max_download_bytes
                );
            }
        }
        let bytes = response.bytes().context("Slack file body is unreadable")?;
        if bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "Slack file download is {} bytes, over max_download_bytes {}",
                bytes.len(),
                self.max_download_bytes
            );
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: file.name.clone().or_else(|| Some(file.id.clone())),
            mime: file.mimetype.clone(),
        })
    }

    fn resolve_file_info(&self, file: &SlackFile) -> Result<SlackFile> {
        if file.file_access.as_deref() != Some("check_file_info") {
            return Ok(file.clone());
        }
        let value = self.post_json("files.info", &json!({"file": file.id}))?;
        serde_json::from_value(value["file"].clone())
            .context("slack files.info response missing file object")
    }
}

impl ChannelAdapter for SlackAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        match self.transport.as_str() {
            "socket_mode" | "socket" => {
                let adapter = self.clone();
                thread::spawn(move || adapter.socket_mode_loop(inbound));
                Ok(())
            }
            "events_api" | "http" | "webhook" => Ok(()),
            other => bail!("slack transport `{other}` is not supported"),
        }
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        if request.method != "POST" {
            return Ok(None);
        }
        match request.path.as_str() {
            SLACK_EVENTS_PATH => self.handle_events(request, inbound).map(Some),
            SLACK_INTERACTIONS_PATH => self.handle_interaction(request, inbound).map(Some),
            _ => Ok(None),
        }
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let mut text_sent = false;
        for chunk in slack_text_chunks(&message.text) {
            self.send_text_chunk(route, &chunk)?;
            text_sent = true;
        }
        let mut comment = (!text_sent)
            .then_some(message.text.trim())
            .filter(|value| !value.is_empty());
        for media_path in message.media_paths {
            self.send_media_path(route, &media_path, comment)?;
            comment = None;
        }
        Ok(())
    }

    fn send_stream_start(
        &self,
        route: &GatewayRoute,
        text: &str,
    ) -> Result<Option<StreamMessageHandle>> {
        let value = self.send_text_chunk_with_response(route, text)?;
        let message_id = value["ts"]
            .as_str()
            .ok_or_else(|| anyhow!("slack stream start did not return ts"))?
            .to_string();
        Ok(Some(StreamMessageHandle { message_id }))
    }

    fn update_stream(
        &self,
        route: &GatewayRoute,
        handle: &StreamMessageHandle,
        text: &str,
        _final_update: bool,
    ) -> Result<()> {
        self.update_text_message(route, &handle.message_id, text)
    }

    fn stream_text_limit(&self) -> usize {
        12_000
    }

    fn send_typing(&self, _route: &GatewayRoute, _event: TypingEvent) -> Result<()> {
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        let mut body = json!({
            "channel": route.key.conversation_id,
            "text": prompt.message,
            "mrkdwn": true,
            "blocks": slack_approval_blocks(&prompt),
        });
        if let Some(thread_ts) = route.key.thread_id.as_deref() {
            body["thread_ts"] = json!(thread_ts);
        }
        self.post_json("chat.postMessage", &body)?;
        Ok(())
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: true,
            typing: false,
            approval_prompt: true,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SlackSocketEnvelope {
    #[serde(default)]
    envelope_id: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct SlackEventEnvelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    challenge: Option<String>,
    #[serde(default)]
    event: Option<SlackEvent>,
}

#[derive(Debug, Deserialize)]
struct SlackEvent {
    #[serde(rename = "type")]
    kind: String,
    channel: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    files: Vec<SlackFile>,
    #[serde(default)]
    blocks: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct SlackFile {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    mimetype: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    file_access: Option<String>,
    #[serde(default)]
    url_private: Option<String>,
    #[serde(default)]
    url_private_download: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackInteractionPayload {
    user: SlackIdObject,
    channel: SlackIdObject,
    #[serde(default)]
    message: Option<SlackInteractionMessage>,
    #[serde(default)]
    container: Option<SlackInteractionContainer>,
    #[serde(default)]
    actions: Vec<SlackAction>,
}

#[derive(Debug, Deserialize)]
struct SlackIdObject {
    id: String,
}

#[derive(Debug, Deserialize)]
struct SlackInteractionMessage {
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackInteractionContainer {
    #[serde(default)]
    message_ts: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackAction {
    #[serde(default)]
    action_id: Option<String>,
    #[serde(default)]
    value: Option<String>,
}

fn slack_approval_blocks(prompt: &GatewayApprovalPrompt) -> Value {
    json!([
        {
            "type": "section",
            "text": {"type": "mrkdwn", "text": prompt.message}
        },
        {
            "type": "section",
            "text": {"type": "mrkdwn", "text": format!("*Command:*\n```{}```", prompt.command)}
        },
        {
            "type": "actions",
            "elements": [
                {"type": "button", "action_id": "duckagent_approve_once", "text": {"type": "plain_text", "text": "Once"}, "value": format!("approve:{}:once", prompt.id)},
                {"type": "button", "action_id": "duckagent_approve_session", "text": {"type": "plain_text", "text": "Session"}, "value": format!("approve:{}:session", prompt.id)},
                {"type": "button", "action_id": "duckagent_approve_always", "text": {"type": "plain_text", "text": "Always"}, "value": format!("approve:{}:always", prompt.id)},
                {"type": "button", "action_id": "duckagent_deny", "style": "danger", "text": {"type": "plain_text", "text": "Deny"}, "value": format!("deny:{}", prompt.id)}
            ]
        }
    ])
}

fn slack_action_to_command_from_action(action: &SlackAction) -> Option<String> {
    let value = action.value.as_deref().unwrap_or_default();
    slack_action_to_command(value).or_else(|| {
        let action_id = action.action_id.as_deref()?;
        let id = value.trim();
        if id.is_empty() {
            return None;
        }
        match action_id {
            "duckagent_approve_once" | "approve_once" => Some(format!("/approve {id} once")),
            "duckagent_approve_session" | "approve_session" => {
                Some(format!("/approve {id} session"))
            }
            "duckagent_approve_always" | "approve_always" => Some(format!("/approve {id} always")),
            "duckagent_deny" | "deny" => Some(format!("/deny {id}")),
            _ => None,
        }
    })
}

fn slack_action_to_command(value: &str) -> Option<String> {
    let parts = value.split(':').collect::<Vec<_>>();
    match parts.as_slice() {
        ["approve", id, decision] => Some(format!("/approve {id} {decision}")),
        ["deny", id] => Some(format!("/deny {id}")),
        _ => None,
    }
}

fn slack_event_text(event: &SlackEvent) -> String {
    let mut text = event.text.clone().unwrap_or_default();
    let block_text = slack_blocks_to_text(&event.blocks);
    if !block_text.trim().is_empty() && block_text.trim() != text.trim() {
        if !text.trim().is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&block_text);
    }
    text
}

fn slack_strip_bot_mention(text: &str, bot_user_id: Option<&str>) -> String {
    let Some(bot_user_id) = bot_user_id else {
        return text.to_string();
    };
    text.replace(&format!("<@{bot_user_id}>"), "")
        .trim()
        .to_string()
}

fn slack_blocks_to_text(blocks: &[Value]) -> String {
    let mut lines = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("rich_text") {
            if let Some(elements) = block.get("elements").and_then(Value::as_array) {
                slack_walk_rich_elements(elements, 0, "", &mut lines);
            }
        } else {
            slack_collect_plain_block_text(block, &mut lines);
        }
    }
    lines.join("\n")
}

fn slack_collect_plain_block_text(block: &Value, lines: &mut Vec<String>) {
    if let Some(text) = block
        .get("text")
        .and_then(|value| value.get("text"))
        .and_then(Value::as_str)
    {
        slack_push_rich_line(lines, text, 0, "");
    }
    if let Some(fields) = block.get("fields").and_then(Value::as_array) {
        for field in fields {
            if let Some(text) = field.get("text").and_then(Value::as_str) {
                slack_push_rich_line(lines, text, 0, "");
            }
        }
    }
    if let Some(elements) = block.get("elements").and_then(Value::as_array) {
        for element in elements {
            if let Some(text) = element
                .get("text")
                .and_then(|value| value.get("text"))
                .and_then(Value::as_str)
                .or_else(|| element.get("text").and_then(Value::as_str))
            {
                slack_push_rich_line(lines, text, 0, "");
            }
        }
    }
}

fn slack_walk_rich_elements(
    elements: &[Value],
    quote_depth: usize,
    bullet: &str,
    lines: &mut Vec<String>,
) {
    for element in elements {
        match element
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "rich_text_section" => {
                if let Some(inline) = element.get("elements").and_then(Value::as_array) {
                    slack_push_rich_line(
                        lines,
                        &slack_render_inline_elements(inline),
                        quote_depth,
                        bullet,
                    );
                }
            }
            "rich_text_quote" => {
                if let Some(children) = element.get("elements").and_then(Value::as_array) {
                    slack_walk_rich_elements(children, quote_depth + 1, bullet, lines);
                }
            }
            "rich_text_list" => {
                let ordered = element.get("style").and_then(Value::as_str) == Some("ordered");
                if let Some(items) = element.get("elements").and_then(Value::as_array) {
                    for (idx, item) in items.iter().enumerate() {
                        let item_bullet = if ordered {
                            format!("{}. ", idx + 1)
                        } else {
                            "- ".to_string()
                        };
                        slack_walk_rich_elements(
                            std::slice::from_ref(item),
                            quote_depth,
                            &item_bullet,
                            lines,
                        );
                    }
                }
            }
            "rich_text_preformatted" => {
                if let Some(children) = element.get("elements").and_then(Value::as_array) {
                    let rendered = slack_render_inline_elements(children);
                    if !rendered.trim().is_empty() {
                        slack_push_rich_line(
                            lines,
                            &format!("```\n{rendered}\n```"),
                            quote_depth,
                            bullet,
                        );
                    }
                }
            }
            _ => {
                let rendered = slack_render_inline_elements(std::slice::from_ref(element));
                slack_push_rich_line(lines, &rendered, quote_depth, bullet);
            }
        }
    }
}

fn slack_render_inline_elements(elements: &[Value]) -> String {
    let mut text = String::new();
    for element in elements {
        match element
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" => text.push_str(
                element
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            "link" => {
                let url = element
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let label = element.get("text").and_then(Value::as_str).unwrap_or(url);
                text.push_str(label);
                if !url.is_empty() && label != url {
                    text.push_str(" (");
                    text.push_str(url);
                    text.push(')');
                }
            }
            "user" => text.push_str(&format!(
                "<@{}>",
                element
                    .get("user_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            )),
            "channel" => text.push_str(&format!(
                "<#{}>",
                element
                    .get("channel_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            )),
            "usergroup" => text.push_str(&format!(
                "<!subteam^{}>",
                element
                    .get("usergroup_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            )),
            "emoji" => text.push_str(&format!(
                ":{}:",
                element
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            )),
            "broadcast" => text.push_str(&format!(
                "<!{}>",
                element
                    .get("range")
                    .and_then(Value::as_str)
                    .unwrap_or("here")
            )),
            "date" => text.push_str(
                element
                    .get("fallback")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            _ => {}
        }
    }
    text
}

fn slack_push_rich_line(lines: &mut Vec<String>, text: &str, quote_depth: usize, bullet: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let quote_prefix = if quote_depth == 0 {
        String::new()
    } else {
        format!("{} ", ">".repeat(quote_depth))
    };
    lines.push(format!("{quote_prefix}{bullet}{text}"));
}

fn slack_thread_anchor(event: &SlackEvent) -> Option<String> {
    event.thread_ts.clone().or_else(|| event.ts.clone())
}

fn slack_route_thread_id(event: &SlackEvent, chat_type: &str) -> Option<String> {
    if chat_type == "dm" {
        return event
            .thread_ts
            .clone()
            .filter(|thread_ts| Some(thread_ts) != event.ts.as_ref());
    }
    slack_thread_anchor(event)
}

fn slack_thread_key(channel_id: &str, thread_anchor: &str) -> String {
    format!("{channel_id}:{thread_anchor}")
}

fn slack_text_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.len() + ch.len_utf8() > SLACK_TEXT_LIMIT && !current.is_empty() {
            chunks.push(current);
            current = String::new();
        }
        current.push(ch);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn slack_channel_type(channel_id: &str) -> String {
    if channel_id.starts_with('D') {
        "dm"
    } else if channel_id.starts_with('G') {
        "group"
    } else {
        "channel"
    }
    .to_string()
}

fn identity_allowed(allowed: &[String], id: &str, username: Option<&str>) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let username = username.map(|value| value.trim_start_matches('@').to_ascii_lowercase());
    allowed.iter().any(|allowed| {
        let allowed = allowed.trim();
        if allowed == "*" {
            return true;
        }
        if allowed == id {
            return true;
        }
        username
            .as_deref()
            .is_some_and(|name| allowed.trim_start_matches('@').eq_ignore_ascii_case(name))
    })
}

fn split_slack_csv(value: Option<&String>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn slack_extra_set(config: &GatewayChannelConfig, keys: &[&str]) -> HashSet<String> {
    keys.iter()
        .flat_map(|key| split_slack_csv(config.extra.get(*key)))
        .collect()
}

fn set_allows(values: &HashSet<String>, id: &str) -> bool {
    values.contains("*") || values.contains(id)
}

fn slack_interaction_key(
    channel_id: &str,
    user_id: &str,
    message_ts: Option<&str>,
    command: &str,
) -> String {
    format!(
        "interaction:{channel_id}:{user_id}:{}:{command}",
        message_ts.unwrap_or_default()
    )
}

fn slack_ts_to_rfc3339(ts: &str) -> Option<String> {
    let seconds = ts.split('.').next()?.parse::<i64>().ok()?;
    chrono::DateTime::from_timestamp(seconds, 0).map(|value| value.to_rfc3339())
}

fn now_rfc3339_like() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn hmac_sha256_hex(key: &str, message: &[u8]) -> String {
    const BLOCK_SIZE: usize = 64;
    let mut key_bytes = key.as_bytes().to_vec();
    if key_bytes.len() > BLOCK_SIZE {
        key_bytes = Sha256::digest(&key_bytes).to_vec();
    }
    key_bytes.resize(BLOCK_SIZE, 0);
    let mut outer = [0x5c; BLOCK_SIZE];
    let mut inner = [0x36; BLOCK_SIZE];
    for (idx, byte) in key_bytes.iter().enumerate() {
        outer[idx] ^= byte;
        inner[idx] ^= byte;
    }
    let mut inner_hash = Sha256::new();
    inner_hash.update(inner);
    inner_hash.update(message);
    let inner_digest = inner_hash.finalize();
    let mut outer_hash = Sha256::new();
    outer_hash.update(outer);
    outer_hash.update(inner_digest);
    outer_hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"ok\":false}".to_vec()),
    }
}

fn text_response(status: u16, text: String) -> ChannelHttpResponse {
    ChannelHttpResponse {
        status,
        content_type: "text/plain",
        body: text.into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_action_values_map_to_approval_commands() {
        assert_eq!(
            slack_action_to_command("approve:appr_123:always").as_deref(),
            Some("/approve appr_123 always")
        );
        assert_eq!(
            slack_action_to_command("deny:appr_123").as_deref(),
            Some("/deny appr_123")
        );
        assert!(slack_action_to_command("other").is_none());
    }

    #[test]
    fn slack_event_maps_message_to_inbound() -> Result<()> {
        let adapter = test_adapter()?;
        let event: SlackEvent = serde_json::from_value(json!({
            "type": "message",
            "channel": "C123",
            "user": "U123",
            "text": "hello",
            "ts": "1710000000.000100",
            "thread_ts": "1710000000.000000"
        }))?;

        let inbound = adapter.event_to_inbound(event).expect("inbound");

        assert_eq!(inbound.channel, "slack");
        assert_eq!(inbound.conversation_id, "C123");
        assert_eq!(inbound.thread_id.as_deref(), Some("1710000000.000000"));
        assert_eq!(inbound.sender_id.as_deref(), Some("U123"));
        assert_eq!(inbound.message_id.as_deref(), Some("1710000000.000100"));
        assert_eq!(inbound.text, "hello");
        Ok(())
    }

    #[test]
    fn slack_signature_uses_expected_hmac() {
        let base = b"v0:1531420618:token=xyzz";
        assert_eq!(
            hmac_sha256_hex("8f742231b10e8888abcd99yyyzzz85a5", base),
            "26d1e87ce021e48173a364b385d9cb4621b83003216ce83dcdf775dee5b3e7c3"
        );
    }

    #[test]
    fn slack_text_chunks_respect_limit() {
        let text = "a".repeat(SLACK_TEXT_LIMIT + 1);
        let chunks = slack_text_chunks(&text);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| chunk.len() <= SLACK_TEXT_LIMIT));
    }

    #[test]
    fn slack_channel_message_requires_mention_by_default() -> Result<()> {
        let adapter = test_adapter_with_bot_mention()?;
        let event: SlackEvent = serde_json::from_value(json!({
            "type": "message",
            "channel": "C123",
            "user": "U123",
            "text": "hello",
            "ts": "1710000000.000100"
        }))?;

        assert!(adapter.event_to_inbound(event).is_none());
        Ok(())
    }

    #[test]
    fn slack_mention_starts_thread_session() -> Result<()> {
        let adapter = test_adapter_with_bot_mention()?;
        let event: SlackEvent = serde_json::from_value(json!({
            "type": "app_mention",
            "channel": "C123",
            "user": "U123",
            "text": "<@UBOT> hello",
            "ts": "1710000000.000100"
        }))?;

        let inbound = adapter.event_to_inbound(event).expect("inbound");

        assert_eq!(inbound.thread_id.as_deref(), Some("1710000000.000100"));
        assert_eq!(inbound.text, "hello");
        Ok(())
    }

    #[test]
    fn slack_thread_reply_follows_mentioned_thread() -> Result<()> {
        let adapter = test_adapter_with_bot_mention()?;
        let mention: SlackEvent = serde_json::from_value(json!({
            "type": "app_mention",
            "channel": "C123",
            "user": "U123",
            "text": "<@UBOT> start",
            "ts": "1710000000.000100"
        }))?;
        assert!(adapter.event_to_inbound(mention).is_some());

        let reply: SlackEvent = serde_json::from_value(json!({
            "type": "message",
            "channel": "C123",
            "user": "U123",
            "text": "follow-up",
            "ts": "1710000001.000100",
            "thread_ts": "1710000000.000100"
        }))?;

        let inbound = adapter.event_to_inbound(reply).expect("thread reply");
        assert_eq!(inbound.thread_id.as_deref(), Some("1710000000.000100"));
        assert_eq!(inbound.text, "follow-up");
        Ok(())
    }

    #[test]
    fn slack_rich_text_blocks_are_visible() -> Result<()> {
        let event: SlackEvent = serde_json::from_value(json!({
            "type": "message",
            "channel": "D123",
            "user": "U123",
            "text": "",
            "ts": "1710000000.000100",
            "blocks": [{
                "type": "rich_text",
                "elements": [{
                    "type": "rich_text_quote",
                    "elements": [{
                        "type": "rich_text_section",
                        "elements": [{"type": "text", "text": "quoted"}]
                    }]
                }]
            }]
        }))?;

        assert_eq!(slack_event_text(&event), "> quoted");
        Ok(())
    }

    fn test_adapter() -> Result<SlackAdapter> {
        let mut config = GatewayChannelConfig {
            enabled: true,
            transport: Some("events_api".to_string()),
            ..Default::default()
        };
        config
            .extra
            .insert("require_mention".to_string(), "false".to_string());
        SlackAdapter::new(
            &config,
            &GatewayCredentialEntry {
                channel: "slack".to_string(),
                token: Some("xoxb-token".to_string()),
                signing_secret: Some("secret".to_string()),
                ..Default::default()
            },
        )
    }

    fn test_adapter_with_bot_mention() -> Result<SlackAdapter> {
        let mut config = GatewayChannelConfig {
            enabled: true,
            transport: Some("events_api".to_string()),
            ..Default::default()
        };
        config
            .extra
            .insert("require_mention".to_string(), "true".to_string());
        config
            .extra
            .insert("bot_user_id".to_string(), "UBOT".to_string());
        SlackAdapter::new(
            &config,
            &GatewayCredentialEntry {
                channel: "slack".to_string(),
                token: Some("xoxb-token".to_string()),
                signing_secret: Some("secret".to_string()),
                ..Default::default()
            },
        )
    }
}
