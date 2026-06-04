use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundAttachmentInput,
    InboundMessageInput, OutboundMessage, TypingEvent,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use regex::{Regex, RegexBuilder};
use reqwest::blocking::{Client, multipart};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const DEFAULT_WHATSAPP_BRIDGE_BASE: &str = "http://127.0.0.1:3000";
const DEFAULT_WHATSAPP_CLOUD_API_BASE: &str = "https://graph.facebook.com/v18.0";
const DEFAULT_WHATSAPP_REPLY_PREFIX: &str = "*DuckAgent*\n----------\n";
const WHATSAPP_TEXT_LIMIT: usize = 4_096;
const POLL_IDLE_SLEEP: Duration = Duration::from_millis(700);
const POLL_ERROR_SLEEP: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub(in crate::gateway) struct WhatsAppAdapter {
    transport: WhatsAppTransport,
    bridge_base: String,
    cloud_api_base: String,
    cloud_access_token: Option<String>,
    cloud_phone_number_id: Option<String>,
    cloud_verify_token: Option<String>,
    cloud_app_secret: Option<String>,
    allowed_users: Vec<String>,
    allowed_chats: Vec<String>,
    max_download_bytes: u64,
    require_mention: bool,
    free_response_chats: HashSet<String>,
    dm_policy: WhatsAppPolicy,
    group_policy: WhatsAppPolicy,
    group_allow_from: HashSet<String>,
    mention_patterns: Vec<Regex>,
    reply_prefix: Option<String>,
    mode: String,
    managed_bridge: bool,
    bridge_script: Option<PathBuf>,
    bridge_port: u16,
    session_path: PathBuf,
    child: Arc<Mutex<Option<Child>>>,
    seen_message_ids: Arc<Mutex<VecDeque<String>>>,
    client: Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WhatsAppTransport {
    CloudApi,
    BridgeHttp,
}

impl WhatsAppTransport {
    fn from_config(value: Option<&str>) -> Result<Self> {
        match value
            .unwrap_or("cloud_api")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "cloud" | "cloud_api" | "business_cloud_api" | "business_api" | "graph_api"
            | "webhook" => Ok(Self::CloudApi),
            "bridge" | "bridge_http" | "whatsapp_bridge" | "managed_bridge" | "external_bridge"
            | "baileys" | "whatsapp_web" | "web" => Ok(Self::BridgeHttp),
            other => {
                bail!("unsupported whatsapp transport `{other}`; use cloud_api or bridge_http")
            }
        }
    }

    fn is_cloud(self) -> bool {
        matches!(self, Self::CloudApi)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WhatsAppPolicy {
    Open,
    Allowlist,
    Disabled,
}

#[derive(Debug, Clone)]
struct WhatsAppBridgeEvent {
    message_id: Option<String>,
    chat_id: String,
    sender_id: String,
    text: String,
    is_group: bool,
    from_me: bool,
    media_paths: Vec<String>,
    media_type: Option<String>,
    mentioned_ids: Vec<String>,
    quoted_participant: Option<String>,
    bot_ids: Vec<String>,
    timestamp: Option<String>,
}

impl WhatsAppAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let transport = WhatsAppTransport::from_config(config.transport.as_deref())?;
        let managed_bridge = parse_bool(config.extra.get("managed_bridge"), false);
        let configured_bridge_base = config
            .api_base
            .clone()
            .or_else(|| config.extra.get("bridge_base").cloned())
            .map(|value| value.trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty());
        let configured_bridge_port = config
            .extra
            .get("bridge_port")
            .and_then(|value| value.parse::<u16>().ok());
        let bridge_port = configured_bridge_port
            .or_else(|| {
                configured_bridge_base
                    .as_deref()
                    .and_then(port_from_bridge_base)
            })
            .map(Ok)
            .unwrap_or_else(|| {
                if managed_bridge {
                    pick_unused_whatsapp_bridge_port()
                } else {
                    Ok(3000)
                }
            })?;
        let bridge_base =
            configured_bridge_base.unwrap_or_else(|| format!("http://127.0.0.1:{bridge_port}"));
        let session_path = config
            .extra
            .get("session_path")
            .map(PathBuf::from)
            .unwrap_or_else(default_whatsapp_session_path);
        let bridge_script = config
            .extra
            .get("bridge_script")
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let client = Client::builder()
            .timeout(Duration::from_secs(35))
            .build()
            .context("failed to build WhatsApp HTTP client")?;
        let cloud_api_base = config
            .extra
            .get("cloud_api_base")
            .cloned()
            .or_else(|| {
                transport
                    .is_cloud()
                    .then(|| config.api_base.clone())
                    .flatten()
            })
            .unwrap_or_else(|| DEFAULT_WHATSAPP_CLOUD_API_BASE.to_string());
        let cloud_access_token = first_non_empty(&[
            credentials.token.as_deref(),
            credentials.api_key.as_deref(),
            credentials.extra.get("access_token").map(String::as_str),
        ]);
        let cloud_phone_number_id = first_non_empty(&[
            credentials.extra.get("phone_number_id").map(String::as_str),
            credentials.extra.get("endpoint_id").map(String::as_str),
            config.extra.get("phone_number_id").map(String::as_str),
            config.extra.get("endpoint_id").map(String::as_str),
        ]);
        let cloud_verify_token = first_non_empty(&[
            credentials.webhook_secret.as_deref(),
            credentials.extra.get("verify_token").map(String::as_str),
            config.extra.get("verify_token").map(String::as_str),
        ]);
        let cloud_app_secret = first_non_empty(&[
            credentials.signing_secret.as_deref(),
            credentials.client_secret.as_deref(),
            credentials.app_secret.as_deref(),
            credentials.extra.get("app_secret").map(String::as_str),
        ]);
        if transport.is_cloud() {
            if cloud_access_token.is_none() {
                bail!("whatsapp cloud_api requires a WhatsApp Cloud API access token");
            }
            if cloud_phone_number_id.is_none() {
                bail!("whatsapp cloud_api requires extra.phone_number_id");
            }
            if cloud_verify_token.is_none() {
                bail!("whatsapp cloud_api requires webhook verify token");
            }
            if cloud_app_secret.is_none() {
                bail!(
                    "whatsapp cloud_api requires Meta app secret for webhook signature verification"
                );
            }
        }
        let require_mention = parse_bool(config.extra.get("require_mention"), false);
        let free_response_chats =
            parse_extra_list(config.extra.get("free_response_chats")).collect();
        let dm_policy = parse_policy(config.extra.get("dm_policy").map(String::as_str), "open")?;
        let group_policy =
            parse_policy(config.extra.get("group_policy").map(String::as_str), "open")?;
        let mut group_allow_from: HashSet<String> = config.allowed_chats.iter().cloned().collect();
        group_allow_from.extend(parse_extra_list(config.extra.get("group_allow_from")));
        let mention_patterns = parse_mention_patterns(config.extra.get("mention_patterns"))?;
        Ok(Self {
            transport,
            bridge_base: bridge_base.trim_end_matches('/').to_string(),
            cloud_api_base: cloud_api_base.trim_end_matches('/').to_string(),
            cloud_access_token,
            cloud_phone_number_id,
            cloud_verify_token,
            cloud_app_secret,
            allowed_users: config.allowed_users.clone(),
            allowed_chats: config.allowed_chats.clone(),
            max_download_bytes: config.media.max_download_bytes,
            require_mention,
            free_response_chats,
            dm_policy,
            group_policy,
            group_allow_from,
            mention_patterns,
            reply_prefix: config.extra.get("reply_prefix").cloned(),
            mode: config
                .extra
                .get("mode")
                .cloned()
                .unwrap_or_else(|| "bot".to_string()),
            managed_bridge,
            bridge_script,
            bridge_port,
            session_path,
            child: Arc::new(Mutex::new(None)),
            seen_message_ids: Arc::new(Mutex::new(VecDeque::new())),
            client,
        })
    }

    fn bridge_url(&self, path: &str) -> String {
        format!("{}{}", self.bridge_base, path)
    }

    fn start_managed_bridge(&self) -> Result<()> {
        if !self.managed_bridge {
            return Ok(());
        }
        let script = self
            .bridge_script
            .as_ref()
            .ok_or_else(|| anyhow!("whatsapp managed_bridge requires extra.bridge_script"))?;
        if !script.exists() {
            bail!(
                "whatsapp bridge script does not exist: {}",
                script.display()
            );
        }
        fs::create_dir_all(&self.session_path).with_context(|| {
            format!(
                "failed to create WhatsApp session dir {}",
                self.session_path.display()
            )
        })?;
        let log_path = self
            .session_path
            .parent()
            .unwrap_or(self.session_path.as_path())
            .join("bridge.log");
        let stdout = append_log_file(&log_path)?;
        let stderr = stdout.try_clone().with_context(|| {
            format!("failed to clone WhatsApp bridge log {}", log_path.display())
        })?;
        let allowed_users = self.allowed_users.join(",");
        let mut command = Command::new("node");
        command
            .arg(script)
            .arg("--port")
            .arg(self.bridge_port.to_string())
            .arg("--session")
            .arg(&self.session_path)
            .arg("--mode")
            .arg(&self.mode)
            .env("WHATSAPP_MODE", &self.mode)
            .env("WHATSAPP_ALLOWED_USERS", allowed_users)
            .env(
                "WHATSAPP_MAX_MESSAGE_LENGTH",
                WHATSAPP_TEXT_LIMIT.to_string(),
            )
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        if let Some(prefix) = self
            .reply_prefix
            .as_deref()
            .or_else(|| (self.mode == "self-chat").then_some(DEFAULT_WHATSAPP_REPLY_PREFIX))
        {
            command.env("WHATSAPP_REPLY_PREFIX", prefix);
        }
        let child = command
            .spawn()
            .with_context(|| format!("failed to start WhatsApp bridge {}", script.display()))?;
        *self.child.lock().expect("whatsapp child mutex poisoned") = Some(child);
        Ok(())
    }

    fn poll_loop(self, inbound: GatewayInboundDispatch) {
        loop {
            match self.poll_once(&inbound) {
                Ok(has_messages) => {
                    if !has_messages {
                        thread::sleep(POLL_IDLE_SLEEP);
                    }
                }
                Err(error) => {
                    eprintln!("whatsapp gateway poll failed: {error:#}");
                    thread::sleep(POLL_ERROR_SLEEP);
                }
            }
        }
    }

    fn poll_once(&self, inbound: &GatewayInboundDispatch) -> Result<bool> {
        let response = self
            .client
            .get(self.bridge_url("/messages"))
            .send()
            .context("whatsapp bridge GET /messages failed")?;
        let status = response.status();
        let value = response
            .json::<Value>()
            .unwrap_or_else(|_| json!({"error": "non-json response"}));
        if !status.is_success() {
            bail!("whatsapp bridge /messages failed with status {status}: {value}");
        }
        let messages = whatsapp_event_items(&value)?;
        for message in &messages {
            if let Some(input) = self.bridge_event_to_inbound(message)? {
                inbound.submit(input)?;
            }
        }
        Ok(!messages.is_empty())
    }

    fn bridge_event_to_inbound(&self, value: &Value) -> Result<Option<InboundMessageInput>> {
        let event = parse_bridge_event(value)?;
        if event.from_me {
            return Ok(None);
        }
        if !self.should_process_message(&event) {
            return Ok(None);
        }
        let text = self.clean_bot_mention_text(&event.text, &event);
        let attachments = event
            .media_paths
            .iter()
            .map(|path| self.attachment_from_bridge_path(path, event.media_type.as_deref()))
            .collect::<Result<Vec<_>>>()?;
        let conversation_id = if event.is_group {
            event.chat_id.clone()
        } else {
            self.canonical_whatsapp_identifier(&event.chat_id)
                .filter(|value| !value.is_empty())
                .unwrap_or(event.chat_id.clone())
        };
        Ok(Some(InboundMessageInput {
            channel: "whatsapp".to_string(),
            conversation_id,
            thread_id: None,
            chat_type: Some(if event.is_group { "group" } else { "dm" }.to_string()),
            sender_id: Some(
                self.canonical_whatsapp_identifier(&event.sender_id)
                    .unwrap_or(event.sender_id),
            ),
            message_id: event.message_id,
            text,
            attachments,
            timestamp: event.timestamp,
        }))
    }

    fn attachment_from_bridge_path(
        &self,
        path: &str,
        media_type: Option<&str>,
    ) -> Result<InboundAttachmentInput> {
        if let Ok(metadata) = fs::metadata(path) {
            if metadata.len() > self.max_download_bytes {
                bail!(
                    "WhatsApp attachment exceeds configured max_download_bytes: {}",
                    path
                );
            }
        }
        let filename = Path::new(path)
            .file_name()
            .and_then(|value| value.to_str())
            .map(str::to_string);
        Ok(InboundAttachmentInput {
            bytes: None,
            path: Some(path.to_string()),
            filename,
            mime: Some(infer_whatsapp_mime(path, media_type)),
        })
    }

    fn should_process_message(&self, event: &WhatsAppBridgeEvent) -> bool {
        if event.is_group {
            if !self.is_group_allowed(&event.chat_id) {
                return false;
            }
            if list_contains_alias_set(&self.free_response_chats, &event.chat_id) {
                return true;
            }
            if !self.require_mention {
                return true;
            }
            let text = event.text.trim();
            if text.starts_with('/') {
                return true;
            }
            self.message_is_reply_to_bot(event)
                || self.message_mentions_bot(event)
                || self.message_matches_mention_patterns(event)
        } else {
            let sender = self
                .canonical_whatsapp_identifier(&event.sender_id)
                .unwrap_or_else(|| event.sender_id.clone());
            self.is_dm_allowed(&sender)
        }
    }

    fn is_dm_allowed(&self, sender_id: &str) -> bool {
        match self.dm_policy {
            WhatsAppPolicy::Disabled => false,
            WhatsAppPolicy::Open => list_allows(&self.allowed_users, sender_id),
            WhatsAppPolicy::Allowlist => {
                !self.allowed_users.is_empty()
                    && list_contains_alias(&self.allowed_users, sender_id)
            }
        }
    }

    fn is_group_allowed(&self, chat_id: &str) -> bool {
        match self.group_policy {
            WhatsAppPolicy::Disabled => false,
            WhatsAppPolicy::Open => {
                self.allowed_chats.is_empty() || list_contains_alias(&self.allowed_chats, chat_id)
            }
            WhatsAppPolicy::Allowlist => list_contains_alias_set(&self.group_allow_from, chat_id),
        }
    }

    fn message_is_reply_to_bot(&self, event: &WhatsAppBridgeEvent) -> bool {
        event.quoted_participant.as_deref().is_some_and(|quoted| {
            event
                .bot_ids
                .iter()
                .any(|bot| same_whatsapp_id(bot, quoted))
        })
    }

    fn message_mentions_bot(&self, event: &WhatsAppBridgeEvent) -> bool {
        if event.bot_ids.is_empty() {
            return false;
        }
        if event.mentioned_ids.iter().any(|mention| {
            event
                .bot_ids
                .iter()
                .any(|bot| same_whatsapp_id(bot, mention))
        }) {
            return true;
        }
        let lower = event.text.to_ascii_lowercase();
        event.bot_ids.iter().any(|bot| {
            let bare = normalize_whatsapp_identifier(bot).to_ascii_lowercase();
            !bare.is_empty() && (lower.contains(&format!("@{bare}")) || lower.contains(&bare))
        })
    }

    fn message_matches_mention_patterns(&self, event: &WhatsAppBridgeEvent) -> bool {
        self.mention_patterns
            .iter()
            .any(|pattern| pattern.is_match(&event.text))
    }

    fn clean_bot_mention_text(&self, text: &str, event: &WhatsAppBridgeEvent) -> String {
        let mut cleaned = text.to_string();
        for bot_id in &event.bot_ids {
            let bare = normalize_whatsapp_identifier(bot_id);
            if bare.is_empty() {
                continue;
            }
            let pattern = format!(r"@{}\b[,:\-]*\s*", regex::escape(&bare));
            if let Ok(regex) = RegexBuilder::new(&pattern).case_insensitive(true).build() {
                cleaned = regex.replace_all(&cleaned, "").to_string();
            }
        }
        let trimmed = cleaned.trim();
        if trimmed.is_empty() {
            text.to_string()
        } else {
            trimmed.to_string()
        }
    }

    fn canonical_whatsapp_identifier(&self, identifier: &str) -> Option<String> {
        let normalized = normalize_whatsapp_identifier(identifier);
        if normalized.is_empty() {
            return None;
        }
        let aliases = expand_whatsapp_aliases(&self.session_path, &normalized);
        aliases
            .into_iter()
            .min_by_key(|candidate| (candidate.len(), candidate.clone()))
            .or(Some(normalized))
    }

    fn send_bridge_json(&self, path: &str, body: Value) -> Result<Value> {
        let response = self
            .client
            .post(self.bridge_url(path))
            .json(&body)
            .send()
            .with_context(|| format!("whatsapp bridge POST {path} failed"))?;
        let status = response.status();
        let value = response
            .json::<Value>()
            .unwrap_or_else(|_| json!({"error": "non-json response"}));
        if !status.is_success() {
            bail!("whatsapp bridge POST {path} failed with status {status}: {value}");
        }
        Ok(value)
    }

    fn send_text_chunk(
        &self,
        route: &GatewayRoute,
        text: &str,
        reply_to: Option<&str>,
    ) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let mut body = json!({
            "chatId": route.key.conversation_id,
            "message": format_whatsapp_message(text),
        });
        if let Some(reply_to) = reply_to.filter(|value| !value.trim().is_empty()) {
            body["replyTo"] = json!(reply_to);
        }
        self.send_bridge_json("/send", body)?;
        Ok(())
    }

    fn send_media_path(
        &self,
        route: &GatewayRoute,
        path: &str,
        caption: Option<&str>,
    ) -> Result<()> {
        if path.starts_with("http://") || path.starts_with("https://") {
            let text = caption
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!("{value}\n{path}"))
                .unwrap_or_else(|| path.to_string());
            return self.send_text_chunk(route, &text, None);
        }
        let path_obj = Path::new(path);
        if !path_obj.exists() {
            bail!("WhatsApp outbound media file not found: {path}");
        }
        let filename = path_obj
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("duckagent-upload");
        self.send_bridge_json(
            "/send-media",
            json!({
                "chatId": route.key.conversation_id,
                "filePath": path,
                "mediaType": infer_whatsapp_media_type(path),
                "caption": caption.unwrap_or_default(),
                "fileName": filename,
            }),
        )?;
        Ok(())
    }

    fn cloud_access_token(&self) -> Result<&str> {
        self.cloud_access_token
            .as_deref()
            .ok_or_else(|| anyhow!("whatsapp cloud_api access token missing"))
    }

    fn cloud_phone_number_id(&self) -> Result<&str> {
        self.cloud_phone_number_id
            .as_deref()
            .ok_or_else(|| anyhow!("whatsapp cloud_api phone_number_id missing"))
    }

    fn cloud_api_url(&self, path: &str) -> Result<String> {
        Ok(format!(
            "{}/{}{}",
            self.cloud_api_base.trim_end_matches('/'),
            self.cloud_phone_number_id()?,
            path
        ))
    }

    fn send_cloud_json(&self, body: Value) -> Result<Value> {
        let response = self
            .client
            .post(self.cloud_api_url("/messages")?)
            .bearer_auth(self.cloud_access_token()?)
            .json(&body)
            .send()
            .context("whatsapp cloud_api POST /messages failed")?;
        let status = response.status();
        let value = response
            .json::<Value>()
            .unwrap_or_else(|_| json!({"error": "non-json response"}));
        if !status.is_success() {
            bail!("whatsapp cloud_api /messages failed with status {status}: {value}");
        }
        Ok(value)
    }

    fn cloud_recipient(recipient: &str) -> String {
        normalize_whatsapp_identifier(recipient)
    }

    fn send_cloud_text_chunk(
        &self,
        route: &GatewayRoute,
        text: &str,
        _reply_to: Option<&str>,
    ) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        self.send_cloud_json(json!({
            "messaging_product": "whatsapp",
            "recipient_type": "individual",
            "to": Self::cloud_recipient(&route.key.conversation_id),
            "type": "text",
            "text": {
                "preview_url": false,
                "body": format_whatsapp_message(text),
            }
        }))?;
        Ok(())
    }

    fn send_cloud_media_path(
        &self,
        route: &GatewayRoute,
        path: &str,
        caption: Option<&str>,
    ) -> Result<()> {
        let media_type = infer_whatsapp_media_type(path);
        let mut media = if path.starts_with("http://") || path.starts_with("https://") {
            json!({"link": path})
        } else {
            json!({"id": self.upload_cloud_media(path, media_type)?})
        };
        if matches!(media_type, "image" | "video" | "document") {
            if let Some(caption) = caption.filter(|value| !value.trim().is_empty()) {
                media["caption"] = json!(caption);
            }
        }
        if media_type == "document" {
            if let Some(filename) = Path::new(path).file_name().and_then(|value| value.to_str()) {
                media["filename"] = json!(filename);
            }
        }
        let mut body = json!({
            "messaging_product": "whatsapp",
            "recipient_type": "individual",
            "to": Self::cloud_recipient(&route.key.conversation_id),
            "type": media_type,
        });
        body[media_type] = media;
        self.send_cloud_json(body)?;
        Ok(())
    }

    fn upload_cloud_media(&self, path: &str, media_type: &str) -> Result<String> {
        let path_obj = Path::new(path);
        if !path_obj.exists() {
            bail!("WhatsApp outbound media file not found: {path}");
        }
        let mime = infer_whatsapp_mime(path, Some(media_type));
        let part = multipart::Part::file(path_obj)
            .with_context(|| format!("failed to open WhatsApp media file {path}"))?
            .mime_str(&mime)
            .with_context(|| format!("invalid WhatsApp media mime {mime}"))?;
        let form = multipart::Form::new()
            .text("messaging_product", "whatsapp")
            .part("file", part);
        let response = self
            .client
            .post(self.cloud_api_url("/media")?)
            .bearer_auth(self.cloud_access_token()?)
            .multipart(form)
            .send()
            .context("whatsapp cloud_api POST /media failed")?;
        let status = response.status();
        let value = response
            .json::<Value>()
            .unwrap_or_else(|_| json!({"error": "non-json response"}));
        if !status.is_success() {
            bail!("whatsapp cloud_api /media failed with status {status}: {value}");
        }
        value["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("whatsapp cloud_api media upload response missing id"))
    }

    fn handle_cloud_verify(&self, request: ChannelHttpRequest) -> Result<ChannelHttpResponse> {
        let verify_token = self
            .cloud_verify_token
            .as_deref()
            .ok_or_else(|| anyhow!("whatsapp cloud_api verify token missing"))?;
        let mode = request
            .query
            .get("hub.mode")
            .map(String::as_str)
            .unwrap_or_default();
        let token = request
            .query
            .get("hub.verify_token")
            .map(String::as_str)
            .unwrap_or_default();
        let challenge = request
            .query
            .get("hub.challenge")
            .map(String::as_str)
            .unwrap_or_default();
        if mode == "subscribe" && token == verify_token {
            return Ok(text_response(200, challenge));
        }
        Ok(json_response(
            403,
            json!({"error": "invalid WhatsApp verification token"}),
        ))
    }

    fn handle_cloud_webhook(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        if !self.verify_cloud_signature(&request) {
            return Ok(json_response(
                401,
                json!({"error": "invalid WhatsApp webhook signature"}),
            ));
        }
        let value: Value = serde_json::from_slice(&request.body)
            .context("failed to parse WhatsApp Cloud webhook JSON")?;
        for message in whatsapp_cloud_messages(&value) {
            if let Some(input) = self.cloud_message_to_inbound(message)? {
                inbound.submit(input)?;
            }
        }
        Ok(json_response(200, json!({"status": "ok"})))
    }

    fn verify_cloud_signature(&self, request: &ChannelHttpRequest) -> bool {
        let Some(secret) = self.cloud_app_secret.as_deref() else {
            return false;
        };
        let Some(signature) = request.header("x-hub-signature-256") else {
            return false;
        };
        let expected = format!(
            "sha256={}",
            hmac_sha256_hex(secret.as_bytes(), &request.body)
        );
        constant_time_eq(signature.as_bytes(), expected.as_bytes())
    }

    fn cloud_message_to_inbound(&self, message: &Value) -> Result<Option<InboundMessageInput>> {
        let message_id = message["id"].as_str().map(str::to_string);
        if message_id
            .as_deref()
            .is_some_and(|id| self.is_duplicate_message_id(id))
        {
            return Ok(None);
        }
        let from = message["from"]
            .as_str()
            .map(normalize_cloud_phone_number)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("WhatsApp Cloud message missing sender"))?;
        let group_id = message["context"]["group_id"]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let is_group = group_id.is_some();
        let conversation_id = group_id.map(str::to_string).unwrap_or_else(|| from.clone());
        let message_type = message["type"].as_str().unwrap_or("text");
        let mut text = whatsapp_cloud_message_text(message, message_type);
        let mut attachments = Vec::new();
        if let Some(attachment) = self.cloud_attachment_from_message(message, message_type)? {
            attachments.push(attachment);
            if text.trim().is_empty() {
                text = format!("[WhatsApp {message_type} message]");
            }
        }
        if text.trim().is_empty() && attachments.is_empty() {
            return Ok(None);
        }
        if !self.should_process_cloud_message(&from, &conversation_id, is_group, &text) {
            return Ok(None);
        }
        Ok(Some(InboundMessageInput {
            channel: "whatsapp".to_string(),
            conversation_id,
            thread_id: None,
            chat_type: Some(if is_group { "group" } else { "dm" }.to_string()),
            sender_id: Some(from),
            message_id,
            text,
            attachments,
            timestamp: message["timestamp"]
                .as_str()
                .and_then(|value| value.parse::<i64>().ok())
                .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
                .map(|value| value.to_rfc3339()),
        }))
    }

    fn cloud_attachment_from_message(
        &self,
        message: &Value,
        message_type: &str,
    ) -> Result<Option<InboundAttachmentInput>> {
        if !matches!(
            message_type,
            "image" | "video" | "audio" | "voice" | "document" | "sticker"
        ) {
            return Ok(None);
        }
        let Some(media) = message.get(message_type) else {
            return Ok(None);
        };
        let Some(media_id) = media["id"].as_str() else {
            return Ok(None);
        };
        self.download_cloud_media(media_id, media, message_type)
            .map(Some)
    }

    fn download_cloud_media(
        &self,
        media_id: &str,
        media: &Value,
        message_type: &str,
    ) -> Result<InboundAttachmentInput> {
        let response = self
            .client
            .get(format!(
                "{}/{}",
                self.cloud_api_base.trim_end_matches('/'),
                media_id
            ))
            .bearer_auth(self.cloud_access_token()?)
            .send()
            .context("whatsapp cloud_api media metadata request failed")?;
        let status = response.status();
        let metadata = response
            .json::<Value>()
            .unwrap_or_else(|_| json!({"error": "non-json response"}));
        if !status.is_success() {
            bail!("whatsapp cloud_api media metadata failed with status {status}: {metadata}");
        }
        if metadata["file_size"]
            .as_u64()
            .is_some_and(|size| size > self.max_download_bytes)
        {
            bail!("WhatsApp Cloud media exceeds configured max_download_bytes");
        }
        let url = metadata["url"]
            .as_str()
            .ok_or_else(|| anyhow!("whatsapp cloud_api media metadata missing url"))?;
        let response = self
            .client
            .get(url)
            .bearer_auth(self.cloud_access_token()?)
            .send()
            .context("whatsapp cloud_api media download failed")?;
        if !response.status().is_success() {
            bail!(
                "whatsapp cloud_api media download failed with status {}",
                response.status()
            );
        }
        if response
            .content_length()
            .is_some_and(|size| size > self.max_download_bytes)
        {
            bail!("WhatsApp Cloud media exceeds configured max_download_bytes");
        }
        let mime = metadata["mime_type"]
            .as_str()
            .or_else(|| {
                response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
            })
            .map(|value| value.split(';').next().unwrap_or(value).to_string())
            .unwrap_or_else(|| infer_whatsapp_mime("", Some(message_type)));
        let bytes = response
            .bytes()
            .context("failed to read WhatsApp Cloud media bytes")?
            .to_vec();
        if bytes.len() as u64 > self.max_download_bytes {
            bail!("WhatsApp Cloud media exceeds configured max_download_bytes");
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes),
            path: None,
            filename: media["filename"]
                .as_str()
                .or_else(|| metadata["filename"].as_str())
                .map(str::to_string)
                .or_else(|| Some(format!("whatsapp-{media_id}"))),
            mime: Some(mime),
        })
    }

    fn should_process_cloud_message(
        &self,
        sender: &str,
        conversation_id: &str,
        is_group: bool,
        text: &str,
    ) -> bool {
        if is_group {
            if !self.is_group_allowed(conversation_id) {
                return false;
            }
            if !self.require_mention {
                return true;
            }
            text.trim().starts_with('/')
                || self
                    .mention_patterns
                    .iter()
                    .any(|pattern| pattern.is_match(text))
        } else {
            match self.dm_policy {
                WhatsAppPolicy::Disabled => false,
                WhatsAppPolicy::Open => list_allows(&self.allowed_users, sender),
                WhatsAppPolicy::Allowlist => {
                    !self.allowed_users.is_empty()
                        && self.allowed_users.iter().any(|allowed| {
                            normalize_cloud_phone_number(allowed)
                                == normalize_cloud_phone_number(sender)
                        })
                }
            }
        }
    }

    fn is_duplicate_message_id(&self, message_id: &str) -> bool {
        let mut seen = self
            .seen_message_ids
            .lock()
            .expect("whatsapp seen message ids mutex poisoned");
        if seen.iter().any(|existing| existing == message_id) {
            return true;
        }
        seen.push_back(message_id.to_string());
        while seen.len() > 1000 {
            seen.pop_front();
        }
        false
    }

    fn effective_reply_prefix(&self) -> &str {
        match self.reply_prefix.as_deref() {
            Some(prefix) => prefix,
            None if self.mode == "self-chat" => DEFAULT_WHATSAPP_REPLY_PREFIX,
            None => "",
        }
    }

    fn outgoing_text_limit(&self) -> usize {
        WHATSAPP_TEXT_LIMIT
            .saturating_sub(self.effective_reply_prefix().chars().count())
            .max(1024)
    }
}

impl Drop for WhatsAppAdapter {
    fn drop(&mut self) {
        if Arc::strong_count(&self.child) != 1 {
            return;
        }
        if let Some(mut child) = self
            .child
            .lock()
            .expect("whatsapp child mutex poisoned")
            .take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl ChannelAdapter for WhatsAppAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        if self.transport.is_cloud() {
            return Ok(());
        }
        self.start_managed_bridge()?;
        let adapter = self.clone();
        thread::spawn(move || adapter.poll_loop(inbound));
        Ok(())
    }

    fn handle_http(
        &self,
        request: super::super::ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<super::super::ChannelHttpResponse>> {
        if !self.transport.is_cloud() {
            return Ok(None);
        }
        if matches!(
            request.path.as_str(),
            "/whatsapp" | "/whatsapp/webhook" | "/whatsapp/events"
        ) {
            return match request.method.as_str() {
                "GET" => self.handle_cloud_verify(request).map(Some),
                "POST" => self.handle_cloud_webhook(request, inbound).map(Some),
                _ => Ok(Some(json_response(
                    405,
                    json!({"error": "method not allowed"}),
                ))),
            };
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        if self.transport.is_cloud() {
            let chunks = whatsapp_text_chunks_with_limit(&message.text, WHATSAPP_TEXT_LIMIT);
            let mut text_sent = false;
            for (index, chunk) in chunks.iter().enumerate() {
                self.send_cloud_text_chunk(
                    route,
                    chunk,
                    (index == 0)
                        .then_some(message.reply_to.as_deref())
                        .flatten(),
                )?;
                text_sent = true;
            }
            let mut caption = (!text_sent)
                .then_some(message.text.trim())
                .filter(|value| !value.is_empty());
            for media_path in message.media_paths {
                self.send_cloud_media_path(route, &media_path, caption)?;
                caption = None;
            }
            return Ok(());
        }
        let chunks = whatsapp_text_chunks_with_limit(&message.text, self.outgoing_text_limit());
        let mut text_sent = false;
        for (index, chunk) in chunks.iter().enumerate() {
            self.send_text_chunk(
                route,
                chunk,
                (index == 0)
                    .then_some(message.reply_to.as_deref())
                    .flatten(),
            )?;
            text_sent = true;
        }
        let mut caption = (!text_sent)
            .then_some(message.text.trim())
            .filter(|value| !value.is_empty());
        for media_path in message.media_paths {
            self.send_media_path(route, &media_path, caption)?;
            caption = None;
        }
        Ok(())
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        if self.transport.is_cloud() {
            let _ = (route, event);
            return Ok(());
        }
        if !event.active {
            return Ok(());
        }
        let _ = self.send_bridge_json(
            "/typing",
            json!({
                "chatId": route.key.conversation_id,
            }),
        );
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        if self.transport.is_cloud() {
            return self.send_cloud_text_chunk(
                route,
                &format!(
                    "{}\n\nCommands:\n/approve {} once\n/approve {} session\n/approve {} always\n/deny {}",
                    prompt.message, prompt.id, prompt.id, prompt.id, prompt.id
                ),
                None,
            );
        }
        self.send_text_chunk(
            route,
            &format!(
                "{}\n\nCommands:\n/approve {} once\n/approve {} session\n/approve {} always\n/deny {}",
                prompt.message, prompt.id, prompt.id, prompt.id, prompt.id
            ),
            None,
        )
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: true,
            typing: !self.transport.is_cloud(),
            approval_prompt: true,
        }
    }
}

fn first_non_empty(values: &[Option<&str>]) -> Option<String> {
    values
        .iter()
        .flatten()
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

fn whatsapp_cloud_messages(value: &Value) -> Vec<&Value> {
    value["entry"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|entry| entry["changes"].as_array().into_iter().flatten())
        .flat_map(|change| change["value"]["messages"].as_array().into_iter().flatten())
        .collect()
}

fn whatsapp_cloud_message_text(message: &Value, message_type: &str) -> String {
    match message_type {
        "text" => message["text"]["body"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        "button" => message["button"]["text"]
            .as_str()
            .or_else(|| message["button"]["payload"].as_str())
            .unwrap_or_default()
            .to_string(),
        "interactive" => message["interactive"]["button_reply"]["title"]
            .as_str()
            .or_else(|| message["interactive"]["button_reply"]["id"].as_str())
            .or_else(|| message["interactive"]["list_reply"]["title"].as_str())
            .or_else(|| message["interactive"]["list_reply"]["id"].as_str())
            .unwrap_or_default()
            .to_string(),
        "image" | "video" | "document" => message[message_type]["caption"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        "audio" | "voice" | "sticker" => String::new(),
        "location" => {
            let latitude = message["location"]["latitude"].as_f64();
            let longitude = message["location"]["longitude"].as_f64();
            match (latitude, longitude) {
                (Some(latitude), Some(longitude)) => {
                    format!("[WhatsApp location: {latitude},{longitude}]")
                }
                _ => "[WhatsApp location]".to_string(),
            }
        }
        "contacts" => "[WhatsApp contacts message]".to_string(),
        "reaction" => message["reaction"]["emoji"]
            .as_str()
            .map(|emoji| format!("[WhatsApp reaction: {emoji}]"))
            .unwrap_or_else(|| "[WhatsApp reaction]".to_string()),
        other => format!("[WhatsApp {other} message]"),
    }
}

fn normalize_cloud_phone_number(value: &str) -> String {
    normalize_whatsapp_identifier(value)
}

fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    let mut key = key.to_vec();
    if key.len() > 64 {
        key = Sha256::digest(&key).to_vec();
    }
    key.resize(64, 0);
    let mut outer_key_pad = [0x5c; 64];
    let mut inner_key_pad = [0x36; 64];
    for (index, byte) in key.iter().enumerate() {
        outer_key_pad[index] ^= byte;
        inner_key_pad[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_key_pad);
    inner.update(message);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_key_pad);
    outer.update(inner_hash);
    to_hex(&outer.finalize())
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (left, right)| acc | (left ^ right))
        == 0
}

fn json_response(status: u16, value: Value) -> super::super::ChannelHttpResponse {
    super::super::ChannelHttpResponse {
        status,
        content_type: "application/json",
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
    }
}

fn text_response(status: u16, text: &str) -> super::super::ChannelHttpResponse {
    super::super::ChannelHttpResponse {
        status,
        content_type: "text/plain; charset=utf-8",
        body: text.as_bytes().to_vec(),
    }
}

fn parse_bridge_event(value: &Value) -> Result<WhatsAppBridgeEvent> {
    let body = whatsapp_event_body(value);
    let chat_id = body["chatId"]
        .as_str()
        .or_else(|| body["chat_id"].as_str())
        .or_else(|| body["remoteJid"].as_str())
        .or_else(|| body["remote_jid"].as_str())
        .or_else(|| body["key"]["remoteJid"].as_str())
        .or_else(|| body["id"]["remote"].as_str())
        .ok_or_else(|| anyhow!("whatsapp bridge event missing chatId"))?
        .to_string();
    let sender_id = body["senderId"]
        .as_str()
        .or_else(|| body["from"].as_str())
        .or_else(|| body["sender_id"].as_str())
        .or_else(|| body["participant"].as_str())
        .or_else(|| body["key"]["participant"].as_str())
        .or_else(|| body["author"].as_str())
        .unwrap_or(&chat_id)
        .to_string();
    let text = whatsapp_event_text(body).unwrap_or_default();
    let media_paths = whatsapp_media_paths(body);
    let from_me = body["fromMe"]
        .as_bool()
        .or_else(|| body["from_me"].as_bool())
        .or_else(|| body["key"]["fromMe"].as_bool())
        .unwrap_or(false);
    Ok(WhatsAppBridgeEvent {
        message_id: body["messageId"]
            .as_str()
            .or_else(|| body["message_id"].as_str())
            .or_else(|| body["id"].as_str())
            .or_else(|| body["key"]["id"].as_str())
            .map(str::to_string),
        chat_id,
        sender_id,
        text,
        is_group: body["isGroup"].as_bool().unwrap_or_else(|| {
            body["chatId"]
                .as_str()
                .or_else(|| body["chat_id"].as_str())
                .or_else(|| body["remoteJid"].as_str())
                .or_else(|| body["key"]["remoteJid"].as_str())
                .is_some_and(|id| id.ends_with("@g.us"))
        }),
        from_me,
        media_paths,
        media_type: body["mediaType"]
            .as_str()
            .or_else(|| body["media_type"].as_str())
            .or_else(|| body["message"]["imageMessage"]["mimetype"].as_str())
            .or_else(|| body["message"]["videoMessage"]["mimetype"].as_str())
            .or_else(|| body["message"]["audioMessage"]["mimetype"].as_str())
            .or_else(|| body["message"]["documentMessage"]["mimetype"].as_str())
            .map(str::to_string),
        mentioned_ids: string_array(&body["mentionedIds"])
            .into_iter()
            .chain(string_array(&body["mentioned_ids"]))
            .chain(string_array(
                &body["message"]["extendedTextMessage"]["contextInfo"]["mentionedJid"],
            ))
            .chain(string_array(
                &body["message"]["imageMessage"]["contextInfo"]["mentionedJid"],
            ))
            .chain(string_array(
                &body["message"]["videoMessage"]["contextInfo"]["mentionedJid"],
            ))
            .collect(),
        quoted_participant: body["quotedParticipant"]
            .as_str()
            .or_else(|| body["quoted_participant"].as_str())
            .or_else(|| {
                body["message"]["extendedTextMessage"]["contextInfo"]["participant"].as_str()
            })
            .map(str::to_string),
        bot_ids: string_array(&body["botIds"])
            .into_iter()
            .chain(string_array(&body["bot_ids"]))
            .collect(),
        timestamp: body["timestamp"]
            .as_i64()
            .map(|ts| ts.to_string())
            .or_else(|| {
                body["timestamp"]
                    .as_str()
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
            }),
    })
}

fn whatsapp_event_items(value: &Value) -> Result<Vec<&Value>> {
    if let Some(items) = value.as_array() {
        return Ok(items.iter().collect());
    }
    for key in ["messages", "events", "data", "items"] {
        if let Some(items) = value[key].as_array() {
            return Ok(items.iter().collect());
        }
    }
    if value.is_object() {
        Ok(vec![value])
    } else {
        bail!("whatsapp bridge /messages must return an array or object envelope")
    }
}

fn whatsapp_event_body(mut value: &Value) -> &Value {
    for _ in 0..4 {
        let Some(next) = value
            .get("data")
            .or_else(|| value.get("payload"))
            .or_else(|| value.get("event"))
            .or_else(|| value.get("message_event"))
        else {
            break;
        };
        if !next.is_object() {
            break;
        }
        value = next;
    }
    value
}

fn whatsapp_event_text(value: &Value) -> Option<String> {
    for path in [
        "/body",
        "/text",
        "/messageText",
        "/message/conversation",
        "/message/extendedTextMessage/text",
        "/message/imageMessage/caption",
        "/message/videoMessage/caption",
        "/message/documentMessage/caption",
    ] {
        if let Some(text) = value.pointer(path).and_then(Value::as_str) {
            if !text.trim().is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

fn whatsapp_media_paths(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in [
        "mediaUrls",
        "media_paths",
        "mediaPaths",
        "files",
        "attachments",
    ] {
        if let Some(items) = value[key].as_array() {
            for item in items {
                if let Some(path) = item
                    .as_str()
                    .or_else(|| item["path"].as_str())
                    .or_else(|| item["filePath"].as_str())
                    .or_else(|| item["file_path"].as_str())
                    .or_else(|| item["url"].as_str())
                    .or_else(|| item["mediaUrl"].as_str())
                    .or_else(|| item["media_url"].as_str())
                {
                    if !path.trim().is_empty() {
                        out.push(path.to_string());
                    }
                }
            }
        }
    }
    for key in ["mediaUrl", "media_url", "filePath", "file_path", "path"] {
        if let Some(path) = value[key].as_str() {
            if !path.trim().is_empty() {
                out.push(path.to_string());
            }
        }
    }
    out
}

fn string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_policy(raw: Option<&str>, default: &str) -> Result<WhatsAppPolicy> {
    match raw.unwrap_or(default).trim().to_ascii_lowercase().as_str() {
        "" | "open" => Ok(WhatsAppPolicy::Open),
        "allowlist" | "allow-list" => Ok(WhatsAppPolicy::Allowlist),
        "disabled" | "off" => Ok(WhatsAppPolicy::Disabled),
        other => bail!("invalid WhatsApp policy `{other}`"),
    }
}

fn parse_bool(value: Option<&String>, default: bool) -> bool {
    value
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}

fn parse_extra_list(value: Option<&String>) -> impl Iterator<Item = String> + '_ {
    value
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn parse_mention_patterns(raw: Option<&String>) -> Result<Vec<Regex>> {
    let Some(raw) = raw
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(Vec::new());
    };
    let patterns = if raw.starts_with('[') {
        serde_json::from_str::<Vec<String>>(raw).unwrap_or_else(|_| vec![raw.to_string()])
    } else {
        raw.split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect()
    };
    patterns
        .into_iter()
        .map(|pattern| {
            RegexBuilder::new(&pattern)
                .case_insensitive(true)
                .build()
                .with_context(|| format!("invalid WhatsApp mention pattern `{pattern}`"))
        })
        .collect()
}

fn list_allows(configured: &[String], sender_id: &str) -> bool {
    configured.is_empty() || list_contains_alias(configured, sender_id)
}

fn list_contains_alias(configured: &[String], sender_id: &str) -> bool {
    configured
        .iter()
        .any(|allowed| same_whatsapp_id(allowed, sender_id))
}

fn list_contains_alias_set(configured: &HashSet<String>, sender_id: &str) -> bool {
    configured
        .iter()
        .any(|allowed| same_whatsapp_id(allowed, sender_id))
}

fn same_whatsapp_id(left: &str, right: &str) -> bool {
    normalize_whatsapp_identifier(left) == normalize_whatsapp_identifier(right)
}

fn normalize_whatsapp_identifier(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('+')
        .split(':')
        .next()
        .unwrap_or_default()
        .split('@')
        .next()
        .unwrap_or_default()
        .to_string()
}

fn expand_whatsapp_aliases(session_path: &Path, identifier: &str) -> HashSet<String> {
    let mut resolved = HashSet::new();
    let mut queue = VecDeque::from([identifier.to_string()]);
    while let Some(current) = queue.pop_front() {
        if current.is_empty()
            || !is_safe_whatsapp_identifier(&current)
            || !resolved.insert(current.clone())
        {
            continue;
        }
        for suffix in ["", "_reverse"] {
            let path = session_path.join(format!("lid-mapping-{current}{suffix}.json"));
            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if let Some(alias) = value.as_str().map(normalize_whatsapp_identifier) {
                if !alias.is_empty() && !resolved.contains(&alias) {
                    queue.push_back(alias);
                }
            }
        }
    }
    resolved
}

fn is_safe_whatsapp_identifier(value: &str) -> bool {
    value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '@' | '.' | '+' | '-'))
}

fn format_whatsapp_message(content: &str) -> String {
    if content.is_empty() {
        return String::new();
    }
    let mut fences = Vec::new();
    let mut text = protect_regex(content, r"(?s)```.*?```", "FENCE", &mut fences);
    let mut codes = Vec::new();
    text = protect_regex(&text, r"`[^`\n]+`", "CODE", &mut codes);
    text = Regex::new(r"\*\*(.+?)\*\*")
        .expect("valid regex")
        .replace_all(&text, "*$1*")
        .to_string();
    text = Regex::new(r"__(.+?)__")
        .expect("valid regex")
        .replace_all(&text, "*$1*")
        .to_string();
    text = Regex::new(r"~~(.+?)~~")
        .expect("valid regex")
        .replace_all(&text, "~$1~")
        .to_string();
    text = Regex::new(r"(?m)^#{1,6}\s+(.+)$")
        .expect("valid regex")
        .replace_all(&text, "*$1*")
        .to_string();
    text = Regex::new(r"\[([^\]]+)\]\(([^)]+)\)")
        .expect("valid regex")
        .replace_all(&text, "$1 ($2)")
        .to_string();
    restore_placeholders(&mut text, "FENCE", &fences);
    restore_placeholders(&mut text, "CODE", &codes);
    text
}

fn protect_regex(content: &str, pattern: &str, label: &str, saved: &mut Vec<String>) -> String {
    let regex = Regex::new(pattern).expect("valid regex");
    let mut out = String::new();
    let mut last = 0usize;
    for found in regex.find_iter(content) {
        out.push_str(&content[last..found.start()]);
        let index = saved.len();
        saved.push(found.as_str().to_string());
        out.push_str(&format!("\u{0}{label}{index}\u{0}"));
        last = found.end();
    }
    out.push_str(&content[last..]);
    out
}

fn restore_placeholders(text: &mut String, label: &str, saved: &[String]) {
    for (index, value) in saved.iter().enumerate() {
        *text = text.replace(&format!("\u{0}{label}{index}\u{0}"), value);
    }
}

fn whatsapp_text_chunks(text: &str) -> Vec<String> {
    whatsapp_text_chunks_with_limit(text, WHATSAPP_TEXT_LIMIT)
}

fn whatsapp_text_chunks_with_limit(text: &str, limit: usize) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let limit = limit.max(1);
    if trimmed.chars().count() <= limit {
        return vec![trimmed.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in trimmed.lines() {
        if current.chars().count() + line.chars().count() + 1 > limit && !current.is_empty() {
            chunks.push(current.trim_end().to_string());
            current.clear();
        }
        if line.chars().count() > limit {
            for ch in line.chars() {
                if current.chars().count() >= limit {
                    chunks.push(current.clone());
                    current.clear();
                }
                current.push(ch);
            }
            current.push('\n');
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current.trim_end().to_string());
    }
    chunks
}

fn infer_whatsapp_media_type(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" | "png" | "webp" | "gif" => "image",
        "mp4" | "mov" | "avi" | "mkv" | "3gp" => "video",
        "ogg" | "opus" | "mp3" | "wav" | "m4a" => "audio",
        _ => "document",
    }
}

fn infer_whatsapp_mime(path: &str, media_type: Option<&str>) -> String {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "png" => "image/png".to_string(),
        "webp" => "image/webp".to_string(),
        "gif" => "image/gif".to_string(),
        "mp4" => "video/mp4".to_string(),
        "ogg" | "opus" => "audio/ogg".to_string(),
        "mp3" => "audio/mpeg".to_string(),
        "m4a" => "audio/mp4".to_string(),
        "pdf" => "application/pdf".to_string(),
        _ => match media_type {
            Some("image") => "image/jpeg".to_string(),
            Some("video") => "video/mp4".to_string(),
            Some("audio" | "ptt") => "audio/ogg".to_string(),
            _ => "application/octet-stream".to_string(),
        },
    }
}

fn port_from_bridge_base(value: &str) -> Option<u16> {
    let without_scheme = value.split("://").nth(1).unwrap_or(value);
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    host_port.rsplit(':').next()?.parse().ok()
}

fn pick_unused_whatsapp_bridge_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("failed to allocate WhatsApp managed bridge port")?;
    Ok(listener
        .local_addr()
        .context("failed to read WhatsApp managed bridge port")?
        .port())
}

fn default_whatsapp_session_path() -> PathBuf {
    super::super::default_gateway_channel_state_dir("whatsapp")
        .unwrap_or_else(|_| std::env::temp_dir().join("duckagent").join("gateway"))
        .join("session")
}

fn append_log_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create WhatsApp log dir {}", parent.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open WhatsApp bridge log {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_adapter(extra: &[(&str, &str)]) -> WhatsAppAdapter {
        let mut config = GatewayChannelConfig {
            transport: Some("bridge_http".to_string()),
            ..Default::default()
        };
        for (key, value) in extra {
            config
                .extra
                .insert((*key).to_string(), (*value).to_string());
        }
        WhatsAppAdapter::new(
            &config,
            &GatewayCredentialEntry {
                channel: "whatsapp".to_string(),
                ..Default::default()
            },
        )
        .expect("test adapter")
    }

    fn group_event(text: &str) -> WhatsAppBridgeEvent {
        WhatsAppBridgeEvent {
            message_id: Some("m1".to_string()),
            chat_id: "120363001234567890@g.us".to_string(),
            sender_id: "6281234567890@s.whatsapp.net".to_string(),
            text: text.to_string(),
            is_group: true,
            from_me: false,
            media_paths: Vec::new(),
            media_type: None,
            mentioned_ids: Vec::new(),
            quoted_participant: None,
            bot_ids: vec!["15551230000@s.whatsapp.net".to_string()],
            timestamp: None,
        }
    }

    #[test]
    fn whatsapp_formatting_matches_bridge_expectations() {
        let formatted = format_whatsapp_message("# Title\n**bold** ~~gone~~ [x](https://e)");
        assert!(formatted.contains("*Title*"));
        assert!(formatted.contains("*bold*"));
        assert!(formatted.contains("~gone~"));
        assert!(formatted.contains("x (https://e)"));
    }

    #[test]
    fn group_require_mention_blocks_unmentioned_messages() {
        let adapter = test_adapter(&[("require_mention", "true")]);
        assert!(!adapter.should_process_message(&group_event("hello")));
        assert!(adapter.should_process_message(&group_event("/status")));
    }

    #[test]
    fn group_mention_patterns_wake_adapter() {
        let adapter = test_adapter(&[
            ("require_mention", "true"),
            ("mention_patterns", r"^\s*duckagent\b"),
        ]);
        assert!(adapter.should_process_message(&group_event("duckagent help")));
        assert!(!adapter.should_process_message(&group_event("hello duckagent")));
    }

    #[test]
    fn whatsapp_identifier_aliases_can_be_canonicalized() -> Result<()> {
        let temp = TempDir::new()?;
        fs::write(
            temp.path().join("lid-mapping-999999999999999.json"),
            json!("15551234567@s.whatsapp.net").to_string(),
        )?;
        let aliases = expand_whatsapp_aliases(temp.path(), "999999999999999");
        assert!(aliases.contains("999999999999999"));
        assert!(aliases.contains("15551234567"));
        Ok(())
    }

    #[test]
    fn bridge_event_extracts_media_paths() -> Result<()> {
        let event = parse_bridge_event(&json!({
            "messageId": "abc",
            "chatId": "15551234567@s.whatsapp.net",
            "senderId": "15551234567@s.whatsapp.net",
            "body": "[image received]",
            "hasMedia": true,
            "mediaType": "image",
            "mediaUrls": ["/tmp/a.png"]
        }))?;
        assert_eq!(event.chat_id, "15551234567@s.whatsapp.net");
        assert_eq!(event.media_paths, vec!["/tmp/a.png"]);
        assert_eq!(event.media_type.as_deref(), Some("image"));
        Ok(())
    }
}
