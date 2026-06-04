use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundAttachmentInput,
    InboundMessageInput, OutboundMessage, TypingEvent,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use reqwest::blocking::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LINE_API_BASE: &str = "https://api.line.me";
const LINE_DATA_BASE: &str = "https://api-data.line.me";
const LINE_TEXT_LIMIT: usize = 4_900;
const LINE_MAX_MESSAGES_PER_CALL: usize = 5;
const LINE_REPLY_TOKEN_TTL: Duration = Duration::from_secs(50);
const LINE_MEDIA_TOKEN_TTL: Duration = Duration::from_secs(30 * 60);
const LINE_LOCAL_IMAGE_LIMIT: u64 = 10 * 1024 * 1024;
const LINE_LOCAL_AV_LIMIT: u64 = 200 * 1024 * 1024;

#[derive(Clone)]
struct LineMediaEntry {
    path: PathBuf,
    expires_at: Instant,
}

#[derive(Clone)]
pub(in crate::gateway) struct LineAdapter {
    channel_secret: String,
    access_token: String,
    api_base: String,
    data_base: String,
    public_base_url: Option<String>,
    bot_user_id: Option<String>,
    group_policy: String,
    allowed_users: HashSet<String>,
    allowed_chats: HashSet<String>,
    client: Client,
    reply_tokens: Arc<Mutex<HashMap<String, (String, Instant)>>>,
    seen_event_ids: Arc<Mutex<VecDeque<String>>>,
    media_tokens: Arc<Mutex<HashMap<String, LineMediaEntry>>>,
}

impl LineAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let access_token = credentials
            .token
            .as_deref()
            .or(credentials.api_key.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("line gateway credential requires channel access token"))?
            .to_string();
        let channel_secret = credentials
            .webhook_secret
            .as_deref()
            .or(credentials.signing_secret.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("line gateway credential requires channel secret"))?
            .to_string();
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build LINE HTTP client")?;
        Ok(Self {
            channel_secret,
            access_token,
            api_base: config
                .api_base
                .clone()
                .unwrap_or_else(|| LINE_API_BASE.to_string()),
            data_base: config
                .extra
                .get("data_base")
                .cloned()
                .unwrap_or_else(|| LINE_DATA_BASE.to_string()),
            public_base_url: line_public_base(config),
            bot_user_id: credentials
                .extra
                .get("bot_user_id")
                .or_else(|| config.extra.get("bot_user_id"))
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            group_policy: config
                .extra
                .get("group_policy")
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| {
                    matches!(
                        value.as_str(),
                        "mention" | "open" | "allowlist" | "disabled"
                    )
                })
                .unwrap_or_else(|| "mention".to_string()),
            allowed_users: config.allowed_users.iter().cloned().collect(),
            allowed_chats: config.allowed_chats.iter().cloned().collect(),
            client,
            reply_tokens: Arc::new(Mutex::new(HashMap::new())),
            seen_event_ids: Arc::new(Mutex::new(VecDeque::new())),
            media_tokens: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn is_duplicate_event(&self, event_id: &str) -> bool {
        if event_id.trim().is_empty() {
            return false;
        }
        let mut seen = self
            .seen_event_ids
            .lock()
            .expect("line seen event ids mutex poisoned");
        if seen.iter().any(|existing| existing == event_id) {
            return true;
        }
        seen.push_back(event_id.to_string());
        while seen.len() > 1000 {
            seen.pop_front();
        }
        false
    }

    fn handle_webhook(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        let signature = request
            .header("x-line-signature")
            .ok_or_else(|| anyhow!("LINE webhook missing x-line-signature"))?;
        if !verify_line_signature(&request.body, signature, &self.channel_secret) {
            return Ok(json_response(401, json!({"error": "invalid signature"})));
        }
        let value: Value =
            serde_json::from_slice(&request.body).context("failed to parse LINE webhook JSON")?;
        for event in value["events"].as_array().into_iter().flatten() {
            if let Some(event_id) = event["webhookEventId"].as_str() {
                if self.is_duplicate_event(event_id) {
                    continue;
                }
            }
            if let Some(input) = self.event_to_inbound(event)? {
                inbound.submit(input)?;
            }
        }
        Ok(json_response(200, json!({"status": "ok"})))
    }

    fn event_to_inbound(&self, event: &Value) -> Result<Option<InboundMessageInput>> {
        if event["type"].as_str() != Some("message") && event["type"].as_str() != Some("postback") {
            return Ok(None);
        }
        let source = &event["source"];
        let source_type = source["type"].as_str().unwrap_or_default();
        let is_group_context = matches!(source_type, "group" | "room");
        let user_id = source["userId"].as_str().unwrap_or_default();
        let conversation_id = source["groupId"]
            .as_str()
            .or_else(|| source["roomId"].as_str())
            .or_else(|| source["userId"].as_str())
            .ok_or_else(|| anyhow!("LINE event missing source id"))?;
        if is_group_context
            && !self.allowed_chats.is_empty()
            && !self.allowed_chats.contains(conversation_id)
        {
            return Ok(None);
        }
        if is_group_context {
            match self.group_policy.as_str() {
                "disabled" => return Ok(None),
                "allowlist" if self.allowed_chats.is_empty() => return Ok(None),
                "mention"
                    if line_mention_range(event, self.bot_user_id.as_deref()).is_none()
                        && !line_is_approval_command_event(event) =>
                {
                    return Ok(None);
                }
                _ => {}
            }
        }
        if !self.allowed_users.is_empty() && !self.allowed_users.contains(user_id) {
            return Ok(None);
        }
        if let Some(reply_token) = event["replyToken"].as_str() {
            self.reply_tokens
                .lock()
                .expect("line reply token mutex poisoned")
                .insert(
                    conversation_id.to_string(),
                    (reply_token.to_string(), Instant::now()),
                );
        }
        let message = &event["message"];
        let mut attachments = Vec::new();
        let mut text = if event["type"].as_str() == Some("postback") {
            event["postback"]["data"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        } else if message["type"].as_str() == Some("text") {
            message["text"].as_str().unwrap_or_default().to_string()
        } else {
            let message_id = message["id"].as_str().unwrap_or_default();
            if !message_id.is_empty() {
                match self.download_content(message_id, message["type"].as_str().unwrap_or("file"))
                {
                    Ok(attachment) => attachments.push(attachment),
                    Err(error) => eprintln!("line gateway content skipped: {error:#}"),
                }
            }
            format!(
                "[LINE {} message]",
                message["type"].as_str().unwrap_or("media")
            )
        };
        if is_group_context {
            text = line_strip_bot_mention(&text, event, self.bot_user_id.as_deref());
        }
        Ok(Some(InboundMessageInput {
            channel: "line".to_string(),
            conversation_id: conversation_id.to_string(),
            thread_id: None,
            chat_type: Some(line_chat_type(event["source"]["type"].as_str())),
            sender_id: (!user_id.is_empty()).then(|| user_id.to_string()),
            message_id: message["id"].as_str().map(str::to_string),
            text,
            attachments,
            timestamp: event["timestamp"].as_i64().and_then(|millis| {
                chrono::DateTime::from_timestamp_millis(millis).map(|value| value.to_rfc3339())
            }),
        }))
    }

    fn download_content(
        &self,
        message_id: &str,
        message_type: &str,
    ) -> Result<InboundAttachmentInput> {
        let response = self
            .client
            .get(format!(
                "{}/v2/bot/message/{}/content",
                self.data_base.trim_end_matches('/'),
                message_id
            ))
            .bearer_auth(&self.access_token)
            .send()
            .context("LINE content download failed")?;
        if !response.status().is_success() {
            bail!(
                "LINE content download failed with status {}",
                response.status()
            );
        }
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).to_string())
            .unwrap_or_else(|| mime_for_line_type(message_type).to_string());
        let bytes = response.bytes().context("LINE content body unreadable")?;
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: Some(format!("line-{message_id}{}", extension_for_mime(&mime))),
            mime: Some(mime),
        })
    }

    fn send_messages(&self, conversation_id: &str, messages: Vec<Value>) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let reply_token = self
            .reply_tokens
            .lock()
            .expect("line reply token mutex poisoned")
            .remove(conversation_id)
            .and_then(|(token, created_at)| {
                (created_at.elapsed() <= LINE_REPLY_TOKEN_TTL).then_some(token)
            });
        let mut reply_token = reply_token;
        for batch in messages.chunks(LINE_MAX_MESSAGES_PER_CALL) {
            let batch = batch.to_vec();
            if let Some(token) = reply_token.take() {
                let url = format!(
                    "{}/v2/bot/message/reply",
                    self.api_base.trim_end_matches('/')
                );
                let response = self
                    .client
                    .post(url)
                    .bearer_auth(&self.access_token)
                    .json(&json!({"replyToken": token, "messages": batch.clone()}))
                    .send()
                    .context("LINE reply failed")?;
                if response.status().is_success() {
                    continue;
                }
            }
            let url = format!(
                "{}/v2/bot/message/push",
                self.api_base.trim_end_matches('/')
            );
            let response = self
                .client
                .post(url)
                .bearer_auth(&self.access_token)
                .json(&json!({"to": conversation_id, "messages": batch}))
                .send()
                .context("LINE push failed")?;
            if !response.status().is_success() {
                bail!("LINE push failed with status {}", response.status());
            }
        }
        Ok(())
    }

    fn media_response(&self, path: &str) -> Result<ChannelHttpResponse> {
        let Some(rest) = path.strip_prefix("/line/media/") else {
            return Ok(text_response(404, "not found"));
        };
        let Some((token, _filename)) = rest.split_once('/') else {
            return Ok(text_response(404, "not found"));
        };
        let entry = {
            let mut tokens = self
                .media_tokens
                .lock()
                .expect("line media token mutex poisoned");
            let Some(entry) = tokens.get(token).cloned() else {
                return Ok(text_response(404, "not found"));
            };
            if Instant::now() > entry.expires_at {
                tokens.remove(token);
                return Ok(text_response(410, "gone"));
            }
            entry
        };
        let bytes = fs::read(&entry.path).context("LINE media file unreadable")?;
        Ok(ChannelHttpResponse {
            status: 200,
            content_type: line_media_content_type(&entry.path),
            body: bytes,
        })
    }

    fn local_media_message(&self, media: &str) -> Result<Value> {
        let base = self.public_base_url.as_ref().ok_or_else(|| {
            anyhow!(
                "LINE local MEDIA requires a public HTTPS webhook/public URL or an already-public media URL: {media}"
            )
        })?;
        let path = Path::new(media);
        if !path.is_file() {
            bail!("LINE local MEDIA file not found: {media}");
        }
        let limit = if is_line_image_path(path) {
            LINE_LOCAL_IMAGE_LIMIT
        } else {
            LINE_LOCAL_AV_LIMIT
        };
        if path.metadata().map(|metadata| metadata.len()).unwrap_or(0) > limit {
            bail!("LINE local MEDIA exceeds channel size limit: {media}");
        }
        let token = self.register_media(path)?;
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("media.bin");
        let url = format!(
            "{}/line/media/{}/{}",
            base.trim_end_matches('/'),
            token,
            url_path_component(filename)
        );
        Ok(line_media_message(&url).unwrap_or_else(|| json!({"type": "text", "text": url})))
    }

    fn register_media(&self, path: &Path) -> Result<String> {
        let resolved = path
            .canonicalize()
            .with_context(|| format!("failed to resolve LINE media path {}", path.display()))?;
        let now = Instant::now();
        let mut tokens = self
            .media_tokens
            .lock()
            .expect("line media token mutex poisoned");
        tokens.retain(|_, entry| entry.expires_at > now);
        let token = line_media_token(&resolved);
        tokens.insert(
            token.clone(),
            LineMediaEntry {
                path: resolved,
                expires_at: now + LINE_MEDIA_TOKEN_TTL,
            },
        );
        Ok(token)
    }
}

impl ChannelAdapter for LineAdapter {
    fn start(&self, _inbound: GatewayInboundDispatch) -> Result<()> {
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        if request.method == "POST"
            && matches!(request.path.as_str(), "/line/webhook" | "/line/events")
        {
            return self.handle_webhook(request, inbound).map(Some);
        }
        if request.method == "GET" && request.path.starts_with("/line/media/") {
            return self.media_response(&request.path).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let mut messages = Vec::new();
        for chunk in line_chunks(&strip_line_markdown(&message.text)) {
            messages.push(json!({"type": "text", "text": chunk}));
        }
        for media in message.media_paths {
            if media.starts_with("http://") || media.starts_with("https://") {
                messages.push(
                    line_media_message(&media)
                        .unwrap_or_else(|| json!({"type": "text", "text": media})),
                );
            } else {
                messages.push(self.local_media_message(&media)?);
            }
        }
        if !messages.is_empty() {
            self.send_messages(&route.key.conversation_id, messages)?;
        }
        Ok(())
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        if event.active && route.key.conversation_id.starts_with('U') {
            let _ = self
                .client
                .post(format!(
                    "{}/v2/bot/chat/loading/start",
                    self.api_base.trim_end_matches('/')
                ))
                .bearer_auth(&self.access_token)
                .json(&json!({"chatId": route.key.conversation_id}))
                .send();
        }
        Ok(())
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
        self.send_messages(
            &route.key.conversation_id,
            vec![json!({"type": "text", "text": line_template_text(&approval_text, 4_900)}), json!({
                "type": "template",
                "altText": line_template_text(&approval_text, 400),
                "template": {
                    "type": "buttons",
                    "text": line_template_text(&prompt.message, 160),
                    "actions": [
                        {"type": "message", "label": "Once", "text": format!("/approve {} once", approval_id.as_str())},
                        {"type": "message", "label": "Session", "text": format!("/approve {} session", approval_id.as_str())},
                        {"type": "message", "label": "Always", "text": format!("/approve {} always", approval_id.as_str())},
                        {"type": "message", "label": "Deny", "text": format!("/deny {}", approval_id.as_str())}
                    ]
                }
            })],
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

fn verify_line_signature(body: &[u8], signature: &str, secret: &str) -> bool {
    let computed =
        base64::engine::general_purpose::STANDARD.encode(hmac_sha256(secret.as_bytes(), body));
    constant_time_eq(computed.as_bytes(), signature.as_bytes())
}

fn line_mention_range(event: &Value, bot_user_id: Option<&str>) -> Option<(usize, usize)> {
    let mentionees = event
        .pointer("/message/mention/mentionees")
        .and_then(Value::as_array)?;
    for mention in mentionees {
        let is_self = mention["isSelf"]
            .as_bool()
            .or_else(|| mention["is_self"].as_bool())
            .unwrap_or(false);
        let user_matches = bot_user_id.is_some_and(|bot_id| {
            mention["userId"]
                .as_str()
                .or_else(|| mention["user_id"].as_str())
                .is_some_and(|user_id| user_id == bot_id)
        });
        if is_self || user_matches {
            let index = mention["index"].as_u64()? as usize;
            let length = mention["length"].as_u64()? as usize;
            return Some((index, length));
        }
    }
    None
}

fn line_strip_bot_mention(text: &str, event: &Value, bot_user_id: Option<&str>) -> String {
    let Some((index, length)) = line_mention_range(event, bot_user_id) else {
        return text.to_string();
    };
    let start = text
        .char_indices()
        .nth(index)
        .map(|(offset, _)| offset)
        .unwrap_or(text.len());
    let end = text
        .char_indices()
        .nth(index.saturating_add(length))
        .map(|(offset, _)| offset)
        .unwrap_or(text.len());
    if start >= end || start >= text.len() {
        return text.to_string();
    }
    let mut stripped = String::with_capacity(text.len());
    stripped.push_str(&text[..start]);
    stripped.push_str(&text[end..]);
    stripped.trim_start().to_string()
}

fn line_is_approval_command_event(event: &Value) -> bool {
    let text = if event["type"].as_str() == Some("postback") {
        event["postback"]["data"].as_str()
    } else {
        event["message"]["text"].as_str()
    };
    text.map(str::trim)
        .is_some_and(|text| text.starts_with("/approve") || text.starts_with("/deny"))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut key_block = [0u8; 64];
    if key.len() > 64 {
        let digest = Sha256::digest(key);
        key_block[..32].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut outer = [0x5c_u8; 64];
    let mut inner = [0x36_u8; 64];
    for index in 0..64 {
        outer[index] ^= key_block[index];
        inner[index] ^= key_block[index];
    }
    let mut inner_hash = Sha256::new();
    inner_hash.update(inner);
    inner_hash.update(data);
    let inner_digest = inner_hash.finalize();
    let mut outer_hash = Sha256::new();
    outer_hash.update(outer);
    outer_hash.update(inner_digest);
    outer_hash.finalize().to_vec()
}

fn line_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if current.len() + character.len_utf8() > LINE_TEXT_LIMIT {
            chunks.push(current);
            current = String::new();
            if chunks.len() == LINE_MAX_MESSAGES_PER_CALL {
                break;
            }
        }
        current.push(character);
    }
    if !current.is_empty() && chunks.len() < LINE_MAX_MESSAGES_PER_CALL {
        chunks.push(current);
    }
    if chunks.len() == LINE_MAX_MESSAGES_PER_CALL
        && text.len() > chunks.iter().map(String::len).sum()
    {
        if let Some(last) = chunks.last_mut() {
            while last.len() + "…".len() > LINE_TEXT_LIMIT {
                last.pop();
            }
            last.push('…');
        }
    }
    chunks
}

fn line_chat_type(source_type: Option<&str>) -> String {
    match source_type {
        Some("group") => "group".to_string(),
        Some("room") => "room".to_string(),
        _ => "dm".to_string(),
    }
}

fn strip_line_markdown(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '`' | '*' => {}
            '[' => {
                let mut label = String::new();
                while let Some(next) = chars.next() {
                    if next == ']' {
                        break;
                    }
                    label.push(next);
                }
                if chars.peek() == Some(&'(') {
                    chars.next();
                    let mut url = String::new();
                    while let Some(next) = chars.next() {
                        if next == ')' {
                            break;
                        }
                        url.push(next);
                    }
                    if !label.is_empty() && !url.is_empty() {
                        output.push_str(&label);
                        output.push_str(" (");
                        output.push_str(&url);
                        output.push(')');
                    } else {
                        output.push_str(&label);
                        output.push_str(&url);
                    }
                } else {
                    output.push_str(&label);
                }
            }
            _ => output.push(character),
        }
    }
    output
}

fn line_template_text(text: &str, limit: usize) -> String {
    let mut output = String::new();
    for character in text.chars() {
        if output.len() + character.len_utf8() > limit {
            break;
        }
        output.push(character);
    }
    output
}

fn line_media_message(media: &str) -> Option<Value> {
    let lower = media
        .split(['?', '#'])
        .next()
        .unwrap_or(media)
        .to_ascii_lowercase();
    if matches!(lower.rsplit('.').next(), Some("jpg" | "jpeg" | "png")) {
        return Some(json!({
            "type": "image",
            "originalContentUrl": media,
            "previewImageUrl": media
        }));
    }
    if matches!(
        lower.rsplit('.').next(),
        Some("mp3" | "m4a" | "aac" | "wav" | "ogg")
    ) {
        return Some(json!({
            "type": "audio",
            "originalContentUrl": media,
            "duration": 60000
        }));
    }
    None
}

fn line_public_base(config: &GatewayChannelConfig) -> Option<String> {
    for key in ["public_url", "public_base_url", "webhook_url"] {
        let Some(value) = config.extra.get(key).map(String::as_str) else {
            continue;
        };
        let value = value.trim().trim_end_matches('/');
        if value.is_empty() || !value.starts_with("https://") {
            continue;
        }
        let base = value
            .strip_suffix("/line/webhook")
            .or_else(|| value.strip_suffix("/line/events"))
            .unwrap_or(value)
            .trim_end_matches('/')
            .to_string();
        if !base.is_empty() {
            return Some(base);
        }
    }
    None
}

fn is_line_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png"
            )
        })
        .unwrap_or(false)
}

fn line_media_content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("mp3") => "audio/mpeg",
        Some("m4a") | Some("aac") => "audio/aac",
        Some("wav") => "audio/wav",
        Some("ogg") => "audio/ogg",
        Some("mp4") => "video/mp4",
        _ => "application/octet-stream",
    }
}

fn line_media_token(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize())
}

fn url_path_component(value: &str) -> String {
    let mut output = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
            output.push(*byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn text_response(status: u16, text: &str) -> ChannelHttpResponse {
    ChannelHttpResponse {
        status,
        content_type: "text/plain",
        body: text.as_bytes().to_vec(),
    }
}

fn mime_for_line_type(message_type: &str) -> &'static str {
    match message_type {
        "image" => "image/jpeg",
        "video" => "video/mp4",
        "audio" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "video/mp4" => ".mp4",
        "audio/mpeg" => ".mp3",
        "application/pdf" => ".pdf",
        _ => ".bin",
    }
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
    fn line_signature_matches() {
        let body = br#"{"events":[]}"#;
        let sig = base64::engine::general_purpose::STANDARD.encode(hmac_sha256(b"secret", body));
        assert!(verify_line_signature(body, &sig, "secret"));
    }

    #[test]
    fn line_chunks_split() {
        assert_eq!(line_chunks(&"x".repeat(LINE_TEXT_LIMIT + 1)).len(), 2);
    }

    #[test]
    fn line_public_image_media_uses_native_message() {
        let value = line_media_message("https://example.com/cat.png").unwrap();
        assert_eq!(value["type"].as_str(), Some("image"));
    }
}
