use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundAttachmentInput,
    InboundMessageInput, OutboundMessage, TypingEvent,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::{Client, multipart};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::form_urlencoded;
use uuid::Uuid;

const DEFAULT_BLUEBUBBLES_WEBHOOK_PATH: &str = "/bluebubbles-webhook";
const MAX_TEXT_LENGTH: usize = 4_000;
const DEDUPE_LIMIT: usize = 1_000;
const TAPBACK_ADDED_START: i64 = 2_000;
const TAPBACK_ADDED_END: i64 = 2_005;
const TAPBACK_REMOVED_START: i64 = 3_000;
const TAPBACK_REMOVED_END: i64 = 3_005;

#[derive(Clone)]
pub(in crate::gateway) struct BlueBubblesAdapter {
    channel: String,
    server_url: String,
    password: String,
    allowed_users: HashSet<String>,
    allowed_chats: HashSet<String>,
    dm_policy: Policy,
    group_policy: Policy,
    max_download_bytes: u64,
    webhook_path: String,
    webhook_url: Option<String>,
    client: Client,
    guid_cache: Arc<Mutex<HashMap<String, String>>>,
    seen_message_ids: Arc<Mutex<VecDeque<String>>>,
    private_api_enabled: Arc<Mutex<Option<bool>>>,
    helper_connected: Arc<Mutex<bool>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    Open,
    Allowlist,
    Disabled,
}

#[derive(Debug, Clone)]
struct BlueBubblesInbound {
    conversation_id: String,
    sender_id: String,
    message_id: Option<String>,
    reply_to_message_id: Option<String>,
    text: String,
    attachments: Vec<InboundAttachmentInput>,
    is_group: bool,
}

impl BlueBubblesAdapter {
    pub(in crate::gateway) fn new(
        channel: &str,
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let server_url = config
            .api_base
            .as_deref()
            .or(credentials.extra.get("server_url").map(String::as_str))
            .map(normalize_server_url)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("{channel} gateway config requires api_base/server_url"))?;
        let password = credentials
            .password
            .as_deref()
            .or(credentials.token.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("{channel} gateway credential requires password"))?
            .to_string();
        let webhook_path = config
            .extra
            .get("webhook_path")
            .map(String::as_str)
            .unwrap_or(DEFAULT_BLUEBUBBLES_WEBHOOK_PATH);
        let webhook_path = if webhook_path.starts_with('/') {
            webhook_path.to_string()
        } else {
            format!("/{webhook_path}")
        };
        let client = Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .context("failed to build BlueBubbles HTTP client")?;
        Ok(Self {
            channel: channel.to_string(),
            server_url,
            password,
            allowed_users: config
                .allowed_users
                .iter()
                .map(|value| normalize_handle(value))
                .collect(),
            allowed_chats: config.allowed_chats.iter().cloned().collect(),
            dm_policy: parse_policy(config.extra.get("dm_policy").map(String::as_str), "open")?,
            group_policy: parse_policy(
                config.extra.get("group_policy").map(String::as_str),
                "allowlist",
            )?,
            max_download_bytes: config.media.max_download_bytes,
            webhook_path,
            webhook_url: config.extra.get("webhook_url").cloned(),
            client,
            guid_cache: Arc::new(Mutex::new(HashMap::new())),
            seen_message_ids: Arc::new(Mutex::new(VecDeque::new())),
            private_api_enabled: Arc::new(Mutex::new(None)),
            helper_connected: Arc::new(Mutex::new(false)),
        })
    }

    fn api_url(&self, path: &str) -> String {
        let sep = if path.contains('?') { '&' } else { '?' };
        format!(
            "{}{}{}password={}",
            self.server_url.trim_end_matches('/'),
            path,
            sep,
            encode_component(&self.password)
        )
    }

    fn api_get(&self, path: &str) -> Result<Value> {
        let response = self
            .client
            .get(self.api_url(path))
            .send()
            .with_context(|| format!("BlueBubbles GET {path} failed"))?;
        parse_json_response(response, path)
    }

    fn api_post<T: serde::Serialize>(&self, path: &str, payload: &T) -> Result<Value> {
        let response = self
            .client
            .post(self.api_url(path))
            .json(payload)
            .send()
            .with_context(|| format!("BlueBubbles POST {path} failed"))?;
        parse_json_response(response, path)
    }

    fn register_webhook(&self) {
        let Some(url) = self.webhook_url.as_deref() else {
            return;
        };
        let register_url = webhook_register_url(url, &self.password);
        if self.webhook_already_registered(&register_url) {
            return;
        }
        let payload = json!({
            "url": register_url,
            "events": ["new-message", "updated-message"],
        });
        match self.api_post("/api/v1/webhook", &payload) {
            Ok(value) => {
                if value
                    .get("status")
                    .and_then(Value::as_i64)
                    .is_some_and(|status| !(200..300).contains(&status))
                {
                    eprintln!(
                        "{} gateway BlueBubbles webhook registration returned: {value}",
                        self.channel
                    );
                }
            }
            Err(error) => {
                eprintln!(
                    "{} gateway BlueBubbles webhook registration failed: {error:#}",
                    self.channel
                );
            }
        }
    }

    fn webhook_already_registered(&self, register_url: &str) -> bool {
        let Ok(value) = self.api_get("/api/v1/webhook") else {
            return false;
        };
        value["data"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|webhook| string_value(&[&webhook["url"]]).as_deref() == Some(register_url))
    }

    fn check_server(&self) {
        if let Err(error) = self.api_get("/api/v1/ping") {
            eprintln!(
                "{} gateway BlueBubbles server ping failed: {error:#}",
                self.channel
            );
            return;
        }
        if let Ok(info) = self.api_get("/api/v1/server/info") {
            let data = info.get("data").unwrap_or(&info);
            if let Some(enabled) = bool_field(data, &["private_api", "privateApi"]) {
                if let Ok(mut guard) = self.private_api_enabled.lock() {
                    *guard = Some(enabled);
                }
            }
            if let Some(connected) = bool_field(data, &["helper_connected", "helperConnected"]) {
                if let Ok(mut guard) = self.helper_connected.lock() {
                    *guard = connected;
                }
            }
        }
    }

    fn authenticate_webhook(&self, request: &ChannelHttpRequest) -> bool {
        let candidate = request
            .query
            .get("password")
            .or_else(|| request.query.get("guid"))
            .map(String::as_str)
            .or_else(|| request.header("x-password"))
            .or_else(|| request.header("x-guid"))
            .or_else(|| request.header("x-bluebubbles-guid"));
        candidate.is_some_and(|value| constant_time_eq(value.as_bytes(), self.password.as_bytes()))
    }

    fn handle_webhook(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        if !self.authenticate_webhook(&request) {
            return Ok(json_response(401, json!({"error": "unauthorized"})));
        }
        let payload = parse_webhook_payload(&request.body)?;
        let event_type = string_value(&[&payload["type"], &payload["event"]]).unwrap_or_default();
        if !event_type.is_empty()
            && !matches!(
                event_type.as_str(),
                "new-message" | "message" | "updated-message"
            )
        {
            return Ok(text_response(200, "ok"));
        }
        let records = extract_payload_records(&payload);
        if records.is_empty() {
            return Ok(json_response(400, json!({"error": "missing message"})));
        }
        let mut accepted = 0usize;
        for message in records {
            if let Some(input) = self.parse_inbound(&payload, &message)? {
                inbound.submit(input)?;
                accepted += 1;
            }
        }
        Ok(json_response(
            200,
            json!({"status": "ok", "accepted": accepted}),
        ))
    }

    fn parse_inbound(
        &self,
        payload: &Value,
        record: &Value,
    ) -> Result<Option<InboundMessageInput>> {
        if bool_value(record, &["isFromMe", "fromMe", "is_from_me"]) {
            return Ok(None);
        }
        if is_tapback(record) {
            return Ok(None);
        }
        let parsed = self.extract_inbound(payload, record)?;
        if parsed
            .message_id
            .as_deref()
            .is_some_and(|id| self.is_duplicate(id))
        {
            return Ok(None);
        }
        if !self.should_process(&parsed) {
            return Ok(None);
        }
        let mut text = parsed.text;
        if let Some(reply_to) = parsed.reply_to_message_id.as_deref() {
            let label = if self.channel == "imessage" {
                "iMessage Reply"
            } else {
                "BlueBubbles Reply"
            };
            text = format!("[{label}]\nreply_to: {reply_to}\n\n{text}");
        }
        Ok(Some(InboundMessageInput {
            channel: self.channel.clone(),
            conversation_id: parsed.conversation_id,
            thread_id: None,
            chat_type: Some(if parsed.is_group { "group" } else { "dm" }.to_string()),
            sender_id: Some(parsed.sender_id),
            message_id: parsed.message_id,
            text,
            attachments: parsed.attachments,
            timestamp: None,
        }))
    }

    fn extract_inbound(&self, payload: &Value, record: &Value) -> Result<BlueBubblesInbound> {
        let mut attachments = Vec::new();
        let mut seen_attachment_guids = HashSet::new();
        for attachment in attachment_values(record)
            .into_iter()
            .chain(attachment_values(payload))
        {
            let guid = string_value(&[
                &attachment["guid"],
                &attachment["attachmentGuid"],
                &attachment["attachment_guid"],
            ])
            .unwrap_or_default();
            if guid.is_empty() {
                continue;
            }
            if !seen_attachment_guids.insert(guid.clone()) {
                continue;
            }
            match self.download_attachment(&guid, attachment) {
                Ok(attachment) => attachments.push(attachment),
                Err(error) => eprintln!(
                    "{} gateway BlueBubbles attachment skipped: {error:#}",
                    self.channel
                ),
            }
        }
        let mut text = string_value(&[&record["text"], &record["message"], &record["body"]])
            .or_else(|| string_value(&[&payload["text"], &payload["message"], &payload["body"]]))
            .unwrap_or_default();
        if text.trim().is_empty() && !attachments.is_empty() {
            text = "(attachment)".to_string();
        }
        let payload_guid =
            string_value(&[&payload["guid"]]).filter(|value| looks_like_chat_guid(value));
        let chat_guid = string_value(&[
            &record["chatGuid"],
            &payload["chatGuid"],
            &record["chat_guid"],
            &payload["chat_guid"],
            &record["chats"][0]["guid"],
            &record["chats"][0]["chatGuid"],
            &payload["chats"][0]["guid"],
            &payload["chats"][0]["chatGuid"],
        ])
        .or(payload_guid);
        let chat_identifier = string_value(&[
            &record["chatIdentifier"],
            &record["identifier"],
            &payload["chatIdentifier"],
            &payload["identifier"],
            &record["chats"][0]["chatIdentifier"],
            &record["chats"][0]["identifier"],
            &payload["chats"][0]["chatIdentifier"],
            &payload["chats"][0]["identifier"],
        ]);
        let sender = string_value(&[
            &record["handle"]["address"],
            &record["sender"],
            &record["from"],
            &record["address"],
            &payload["sender"],
            &payload["from"],
            &payload["address"],
        ])
        .or_else(|| chat_identifier.clone())
        .or_else(|| chat_guid.clone())
        .unwrap_or_default();
        let conversation_id = chat_guid
            .clone()
            .or(chat_identifier)
            .or_else(|| (!sender.is_empty()).then(|| sender.clone()))
            .ok_or_else(|| anyhow!("BlueBubbles inbound missing chat id"))?;
        if sender.is_empty() || text.trim().is_empty() {
            bail!("BlueBubbles inbound missing sender or text");
        }
        let is_group = bool_value(record, &["isGroup", "is_group"])
            || bool_value(payload, &["isGroup", "is_group"])
            || conversation_id.contains(";+;");
        Ok(BlueBubblesInbound {
            conversation_id,
            sender_id: normalize_handle(&sender),
            message_id: string_value(&[
                &record["guid"],
                &record["messageGuid"],
                &record["message_guid"],
                &record["id"],
                &record["dateCreated"],
                &record["date_created"],
            ]),
            reply_to_message_id: string_value(&[
                &record["threadOriginatorGuid"],
                &record["thread_originator_guid"],
                &record["associatedMessageGuid"],
                &record["associated_message_guid"],
                &record["replyToMessageGuid"],
                &record["reply_to_message_guid"],
            ]),
            text,
            attachments,
            is_group,
        })
    }

    fn download_attachment(&self, guid: &str, metadata: &Value) -> Result<InboundAttachmentInput> {
        let path = format!("/api/v1/attachment/{}/download", encode_component(guid));
        let response = self
            .client
            .get(self.api_url(&path))
            .send()
            .with_context(|| format!("BlueBubbles attachment download failed for {guid}"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("BlueBubbles attachment download failed with status {status}");
        }
        let mime = string_value(&[
            &metadata["mimeType"],
            &metadata["mime_type"],
            &metadata["type"],
        ])
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| "application/octet-stream".to_string());
        let filename = string_value(&[&metadata["transferName"], &metadata["transfer_name"]])
            .unwrap_or_else(|| attachment_filename(guid, &mime));
        let bytes = response
            .bytes()
            .context("BlueBubbles attachment body unreadable")?;
        if bytes.len() as u64 > self.max_download_bytes {
            bail!("BlueBubbles attachment exceeds configured max_download_bytes: {guid}");
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: Some(filename),
            mime: Some(mime),
        })
    }

    fn should_process(&self, message: &BlueBubblesInbound) -> bool {
        if message.is_group {
            match self.group_policy {
                Policy::Disabled => false,
                Policy::Open => {
                    self.allowed_chats.is_empty()
                        || self.allowed_chats.contains(&message.conversation_id)
                }
                Policy::Allowlist => self.allowed_chats.contains(&message.conversation_id),
            }
        } else {
            match self.dm_policy {
                Policy::Disabled => false,
                Policy::Open => {
                    self.allowed_users.is_empty() || self.allowed_users.contains(&message.sender_id)
                }
                Policy::Allowlist => self.allowed_users.contains(&message.sender_id),
            }
        }
    }

    fn is_duplicate(&self, message_id: &str) -> bool {
        let mut guard = self
            .seen_message_ids
            .lock()
            .expect("bluebubbles dedupe mutex poisoned");
        if guard.iter().any(|seen| seen == message_id) {
            return true;
        }
        guard.push_back(message_id.to_string());
        while guard.len() > DEDUPE_LIMIT {
            guard.pop_front();
        }
        false
    }

    fn resolve_chat_guid(&self, target: &str) -> Result<String> {
        if target.contains(';') {
            return Ok(target.to_string());
        }
        if let Some(cached) = self
            .guid_cache
            .lock()
            .expect("bluebubbles guid cache mutex poisoned")
            .get(target)
            .cloned()
        {
            return Ok(cached);
        }
        let payload = json!({"limit": 100, "offset": 0, "with": ["participants"]});
        let normalized_target = normalize_handle(target);
        let value = self.api_post("/api/v1/chat/query", &payload)?;
        for chat in value["data"].as_array().into_iter().flatten() {
            let guid = string_value(&[&chat["guid"], &chat["chatGuid"]]);
            let identifier = string_value(&[&chat["chatIdentifier"], &chat["identifier"]]);
            if identifier
                .as_deref()
                .is_some_and(|identifier| normalize_handle(identifier) == normalized_target)
            {
                if let Some(guid) = guid {
                    self.guid_cache
                        .lock()
                        .expect("bluebubbles guid cache mutex poisoned")
                        .insert(target.to_string(), guid.clone());
                    return Ok(guid);
                }
            }
            for participant in chat["participants"].as_array().into_iter().flatten() {
                if string_value(&[&participant["address"]])
                    .as_deref()
                    .is_some_and(|address| normalize_handle(address) == normalized_target)
                {
                    if let Some(guid) = guid {
                        self.guid_cache
                            .lock()
                            .expect("bluebubbles guid cache mutex poisoned")
                            .insert(target.to_string(), guid.clone());
                        return Ok(guid);
                    }
                }
            }
        }
        bail!("BlueBubbles chat not found for target {target}");
    }

    fn send_text_chunk(&self, route: &GatewayRoute, text: &str) -> Result<()> {
        let text = format_imessage_text(text);
        if text.is_empty() {
            return Ok(());
        }
        let target = &route.key.conversation_id;
        let chat_guid = match self.resolve_chat_guid(target) {
            Ok(guid) => guid,
            Err(_error) if looks_like_address(target) => {
                let value = self.api_post(
                    "/api/v1/chat/new",
                    &json!({
                        "addresses": [target],
                        "message": text,
                        "tempGuid": format!("temp-{}", Uuid::now_v7().simple()),
                    }),
                )?;
                let guid = string_value(&[&value["data"]["guid"], &value["data"]["messageGuid"]])
                    .unwrap_or_else(|| target.to_string());
                self.guid_cache
                    .lock()
                    .expect("bluebubbles guid cache mutex poisoned")
                    .insert(target.to_string(), guid);
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let mut payload = json!({
            "chatGuid": chat_guid,
            "tempGuid": format!("temp-{}", Uuid::now_v7().simple()),
            "message": text,
        });
        if self.can_send_private_reply() {
            if let Some(reply_to) = route
                .key
                .thread_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                payload["method"] = json!("private-api");
                payload["selectedMessageGuid"] = json!(reply_to);
                payload["partIndex"] = json!(0);
            }
        }
        self.api_post("/api/v1/message/text", &payload)?;
        Ok(())
    }

    fn can_send_private_reply(&self) -> bool {
        let private_api_enabled = self
            .private_api_enabled
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .unwrap_or(false);
        let helper_connected = self
            .helper_connected
            .lock()
            .map(|guard| *guard)
            .unwrap_or(false);
        private_api_enabled && helper_connected
    }

    fn send_attachment(&self, route: &GatewayRoute, path: &str) -> Result<()> {
        if path.starts_with("http://") || path.starts_with("https://") {
            return self.send_text_chunk(route, path);
        }
        let chat_guid = self.resolve_chat_guid(&route.key.conversation_id)?;
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read BlueBubbles upload file {path}"))?;
        let filename = Path::new(path)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("duckagent-upload")
            .to_string();
        let part = multipart::Part::bytes(bytes).file_name(filename.clone());
        let form = multipart::Form::new()
            .text("chatGuid", chat_guid)
            .text("name", filename)
            .text("tempGuid", Uuid::now_v7().simple().to_string())
            .part("attachment", part);
        let form = if is_audio_path(path) {
            form.text("isAudioMessage", "true")
        } else {
            form
        };
        let response = self
            .client
            .post(self.api_url("/api/v1/message/attachment"))
            .multipart(form)
            .send()
            .context("BlueBubbles attachment upload failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("BlueBubbles attachment upload failed with status {status}");
        }
        Ok(())
    }
}

impl ChannelAdapter for BlueBubblesAdapter {
    fn start(&self, _inbound: GatewayInboundDispatch) -> Result<()> {
        self.check_server();
        self.register_webhook();
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        if request.method != "POST" {
            return Ok(None);
        }
        if request.path == self.webhook_path
            || request.path == format!("/{}/events", self.channel)
            || request.path == format!("/{}-webhook", self.channel)
        {
            return self.handle_webhook(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        for chunk in outbound_text_chunks(&message.text) {
            self.send_text_chunk(route, &chunk)?;
        }
        for media_path in message.media_paths {
            self.send_attachment(route, &media_path)?;
        }
        Ok(())
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        if !self.can_send_private_reply() {
            return Ok(());
        }
        let chat_guid = match self.resolve_chat_guid(&route.key.conversation_id) {
            Ok(guid) => guid,
            Err(_) => return Ok(()),
        };
        let path = format!("/api/v1/chat/{}/typing", encode_component(&chat_guid));
        let request = if event.active {
            self.client.post(self.api_url(&path))
        } else {
            self.client.delete(self.api_url(&path))
        };
        let _ = request.send();
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        self.send_text_chunk(
            route,
            &format!(
                "{}\n\nCommands:\n/approve {} once\n/approve {} session\n/approve {} always\n/deny {}",
                prompt.message, prompt.id, prompt.id, prompt.id, prompt.id
            ),
        )
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: true,
            typing: true,
            approval_prompt: true,
        }
    }
}

fn parse_webhook_payload(body: &[u8]) -> Result<Value> {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return Ok(value);
    }
    let body_text = String::from_utf8_lossy(body);
    for (key, value) in form_urlencoded::parse(body_text.as_bytes()) {
        if matches!(key.as_ref(), "payload" | "data" | "message") {
            return serde_json::from_str(&value)
                .context("failed to parse BlueBubbles form JSON payload");
        }
    }
    bail!("failed to parse BlueBubbles webhook payload")
}

fn extract_payload_record(payload: &Value) -> Option<Value> {
    extract_payload_records(payload).into_iter().next()
}

fn extract_payload_records(payload: &Value) -> Vec<Value> {
    if let Some(items) = payload.as_array() {
        return items
            .iter()
            .filter(|item| item.is_object())
            .cloned()
            .collect();
    }
    if payload["data"].is_object() {
        return vec![payload["data"].clone()];
    }
    if let Some(items) = payload["data"].as_array() {
        return items
            .iter()
            .filter(|item| item.is_object())
            .cloned()
            .collect();
    }
    if payload["message"].is_object() {
        return vec![payload["message"].clone()];
    }
    for key in ["messages", "events", "payload", "record"] {
        if payload[key].is_object() {
            return vec![payload[key].clone()];
        }
        if let Some(items) = payload[key].as_array() {
            return items
                .iter()
                .filter(|item| item.is_object())
                .cloned()
                .collect();
        }
    }
    if payload.is_object() {
        vec![payload.clone()]
    } else {
        Vec::new()
    }
}

fn string_value(values: &[&Value]) -> Option<String> {
    values.iter().find_map(|value| match value {
        Value::String(raw) => {
            let trimmed = raw.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    })
}

fn bool_value(record: &Value, keys: &[&str]) -> bool {
    keys.iter()
        .any(|key| value_as_bool(&record[*key]).unwrap_or(false))
}

fn bool_field(record: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|key| value_as_bool(&record[*key]))
}

fn is_tapback(record: &Value) -> bool {
    let assoc_type = int_value(&[
        &record["associatedMessageType"],
        &record["associated_message_type"],
    ]);
    assoc_type.is_some_and(|value| {
        (TAPBACK_ADDED_START..=TAPBACK_ADDED_END).contains(&value)
            || (TAPBACK_REMOVED_START..=TAPBACK_REMOVED_END).contains(&value)
    })
}

fn parse_policy(raw: Option<&str>, default: &str) -> Result<Policy> {
    match raw.unwrap_or(default).trim().to_ascii_lowercase().as_str() {
        "" | "open" => Ok(Policy::Open),
        "allowlist" | "allow-list" => Ok(Policy::Allowlist),
        "disabled" | "off" => Ok(Policy::Disabled),
        other => bail!("invalid BlueBubbles policy `{other}`"),
    }
}

fn normalize_server_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

fn normalize_handle(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.contains('@') {
        trimmed.to_ascii_lowercase()
    } else if trimmed.starts_with('+') {
        trimmed.replace([' ', '-', '(', ')'], "")
    } else {
        trimmed.replace(char::is_whitespace, "")
    }
}

fn attachment_values(value: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    for key in ["attachments", "attachment", "files", "file", "media"] {
        match value.get(key) {
            Some(Value::Array(items)) => out.extend(items.iter()),
            Some(Value::Object(_)) => out.push(&value[key]),
            _ => {}
        }
    }
    out
}

fn value_as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        Value::Number(number) => number.as_i64().map(|value| value != 0),
        _ => None,
    }
}

fn int_value(values: &[&Value]) -> Option<i64> {
    values.iter().find_map(|value| match value {
        Value::Number(number) => number.as_i64(),
        Value::String(value) => value.trim().parse::<i64>().ok(),
        _ => None,
    })
}

fn looks_like_chat_guid(value: &str) -> bool {
    value.contains(';')
}

fn text_chunks(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed.chars().count() <= MAX_TEXT_LENGTH {
        return vec![trimmed.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in trimmed.chars() {
        if current.chars().count() >= MAX_TEXT_LENGTH {
            chunks.push(current.clone());
            current.clear();
        }
        current.push(ch);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn outbound_text_chunks(text: &str) -> Vec<String> {
    let formatted = format_imessage_text(text);
    if formatted.is_empty() {
        return Vec::new();
    }
    let mut paragraphs = Vec::new();
    let mut current = Vec::new();
    for line in formatted.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                paragraphs.push(current.join("\n"));
                current.clear();
            }
        } else {
            current.push(line.to_string());
        }
    }
    if !current.is_empty() {
        paragraphs.push(current.join("\n"));
    }
    if paragraphs.is_empty() {
        return text_chunks(&formatted);
    }
    paragraphs
        .into_iter()
        .flat_map(|paragraph| text_chunks(&paragraph))
        .collect()
}

fn format_imessage_text(text: &str) -> String {
    text.lines()
        .map(strip_markdown_line)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn strip_markdown_line(line: &str) -> String {
    let trimmed = line.trim_start();
    let without_heading = if trimmed.starts_with('#') {
        let hashes = trimmed.chars().take_while(|ch| *ch == '#').count();
        let rest = &trimmed[hashes..];
        if (1..=6).contains(&hashes) && rest.starts_with(char::is_whitespace) {
            rest.trim_start()
        } else {
            line
        }
    } else {
        line
    };
    strip_markdown_inline(without_heading)
        .trim_end()
        .to_string()
}

fn strip_markdown_inline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '!' && chars.get(i + 1) == Some(&'[') {
            if let Some((label, next)) = markdown_link_label(&chars, i + 1) {
                out.push_str(&label);
                i = next;
                continue;
            }
        }
        if chars[i] == '[' {
            if let Some((label, next)) = markdown_link_label(&chars, i) {
                out.push_str(&label);
                i = next;
                continue;
            }
        }
        if matches!(chars[i], '*' | '_' | '`' | '~') {
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn markdown_link_label(chars: &[char], start: usize) -> Option<(String, usize)> {
    if chars.get(start) != Some(&'[') {
        return None;
    }
    let close = chars[start + 1..]
        .iter()
        .position(|ch| *ch == ']')
        .map(|offset| start + 1 + offset)?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = chars[close + 2..]
        .iter()
        .position(|ch| *ch == ')')
        .map(|offset| close + 2 + offset)?;
    let label = chars[start + 1..close].iter().collect::<String>();
    Some((label, end + 1))
}

fn looks_like_address(value: &str) -> bool {
    value.contains('@') || value.starts_with('+') || value.chars().all(|ch| ch.is_ascii_digit())
}

fn is_audio_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "aac" | "caf" | "m4a" | "mp3" | "oga" | "ogg" | "opus" | "wav"
            )
        })
        .unwrap_or(false)
}

fn attachment_filename(guid: &str, mime: &str) -> String {
    let extension = match mime {
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/heic" => ".heic",
        "image/heif" => ".heic",
        "image/tiff" => ".tiff",
        "audio/mpeg" => ".mp3",
        "audio/mp3" => ".mp3",
        "audio/mp4" => ".m4a",
        "audio/aac" => ".m4a",
        "audio/ogg" => ".ogg",
        "audio/opus" => ".opus",
        "audio/wav" => ".wav",
        "audio/x-caf" => ".caf",
        "video/mp4" => ".mp4",
        "application/pdf" => ".pdf",
        _ => "",
    };
    format!("bluebubbles-{guid}{extension}")
}

fn webhook_register_url(url: &str, password: &str) -> String {
    if password.is_empty() {
        return url.to_string();
    }
    if url.contains('?') {
        format!("{url}&password={}", encode_component(password))
    } else {
        format!("{url}?password={}", encode_component(password))
    }
}

fn encode_component(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn parse_json_response(response: reqwest::blocking::Response, path: &str) -> Result<Value> {
    let status = response.status();
    let value: Value = response
        .json()
        .with_context(|| format!("BlueBubbles {path} returned invalid JSON"))?;
    if !status.is_success() {
        bail!("BlueBubbles {path} failed with status {status}: {value}");
    }
    Ok(value)
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
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"error\":\"json\"}".to_vec()),
    }
}

fn text_response(status: u16, text: &str) -> ChannelHttpResponse {
    ChannelHttpResponse {
        status,
        content_type: "text/plain",
        body: text.as_bytes().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bluebubbles_extracts_inbound_dm() -> Result<()> {
        let adapter = test_adapter("bluebubbles")?;
        let payload = json!({
            "type": "new-message",
            "data": {
                "guid": "m1",
                "chatGuid": "iMessage;-;+15551234567",
                "handle": {"address": "+1 (555) 123-4567"},
                "text": "hello"
            }
        });
        let record = extract_payload_record(&payload).expect("record");
        let inbound = adapter.extract_inbound(&payload, &record)?;
        assert_eq!(inbound.conversation_id, "iMessage;-;+15551234567");
        assert_eq!(inbound.sender_id, "+15551234567");
        assert_eq!(inbound.text, "hello");
        Ok(())
    }

    #[test]
    fn bluebubbles_skips_tapback() {
        let record = json!({"associatedMessageType": 2001});
        assert!(is_tapback(&record));
    }

    #[test]
    fn bluebubbles_group_policy_defaults_allowlist() -> Result<()> {
        let adapter = test_adapter("bluebubbles")?;
        let message = BlueBubblesInbound {
            conversation_id: "iMessage;+;group".to_string(),
            sender_id: "+15551234567".to_string(),
            message_id: Some("m1".to_string()),
            reply_to_message_id: None,
            text: "hello".to_string(),
            attachments: Vec::new(),
            is_group: true,
        };
        assert!(!adapter.should_process(&message));
        Ok(())
    }

    #[test]
    fn bluebubbles_form_payload_parses() -> Result<()> {
        let raw = "payload=%7B%22type%22%3A%22new-message%22%7D";
        let value = parse_webhook_payload(raw.as_bytes())?;
        assert_eq!(value["type"].as_str(), Some("new-message"));
        Ok(())
    }

    #[test]
    fn imessage_alias_keeps_channel_namespace() -> Result<()> {
        let adapter = test_adapter("imessage")?;
        assert_eq!(adapter.channel, "imessage");
        Ok(())
    }

    fn test_adapter(channel: &str) -> Result<BlueBubblesAdapter> {
        BlueBubblesAdapter::new(
            channel,
            &GatewayChannelConfig {
                api_base: Some("http://127.0.0.1:1234".to_string()),
                ..Default::default()
            },
            &GatewayCredentialEntry {
                channel: channel.to_string(),
                password: Some("pw".to_string()),
                ..Default::default()
            },
        )
    }
}
