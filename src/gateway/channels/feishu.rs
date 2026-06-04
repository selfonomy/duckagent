use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundAttachmentInput,
    InboundMessageInput, OutboundMessage, StreamMessageHandle, TypingEvent,
};
use super::feishu_ws::{FeishuWsConfig, spawn_feishu_ws_loop};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use reqwest::blocking::{Client, multipart};
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use url::form_urlencoded;

const DEFAULT_FEISHU_API_BASE: &str = "https://open.feishu.cn";
const FEISHU_TEXT_LIMIT: usize = 4_000;
const FEISHU_STREAM_MIN_DELTA_CHARS: usize = 400;
const FEISHU_STREAM_FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const FEISHU_STREAM_UPDATE_BUDGET: usize = 8;

static FEISHU_MARKDOWN_HINT_RE: OnceLock<Regex> = OnceLock::new();
static FEISHU_MARKDOWN_TABLE_RE: OnceLock<Regex> = OnceLock::new();
static FEISHU_MARKDOWN_LINK_RE: OnceLock<Regex> = OnceLock::new();

#[derive(Clone)]
pub(in crate::gateway) struct FeishuAdapter {
    channel: String,
    app_id: String,
    app_secret: String,
    verification_token: Option<String>,
    signing_secret: Option<String>,
    api_base: String,
    transport: String,
    allowed_users: Vec<String>,
    allowed_chats: Vec<String>,
    max_download_bytes: u64,
    client: Client,
    token: Arc<Mutex<Option<CachedFeishuToken>>>,
}

#[derive(Clone)]
struct CachedFeishuToken {
    value: String,
    expires_at: Instant,
}

impl FeishuAdapter {
    pub(in crate::gateway) fn new(
        channel: &str,
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let app_id = credentials
            .app_id
            .as_deref()
            .or(credentials.api_key.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("feishu gateway credential requires app_id"))?
            .to_string();
        let app_secret = credentials
            .app_secret
            .as_deref()
            .or(credentials.client_secret.as_deref())
            .or(credentials.token.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("feishu gateway credential requires app_secret"))?
            .to_string();
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build Feishu HTTP client")?;
        Ok(Self {
            channel: channel.to_string(),
            app_id,
            app_secret,
            verification_token: credentials
                .webhook_secret
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            signing_secret: credentials
                .signing_secret
                .as_deref()
                .or_else(|| credentials.extra.get("encrypt_key").map(String::as_str))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            api_base: config
                .api_base
                .clone()
                .unwrap_or_else(|| DEFAULT_FEISHU_API_BASE.to_string()),
            transport: config
                .transport
                .clone()
                .unwrap_or_else(|| "websocket".to_string())
                .to_ascii_lowercase(),
            allowed_users: config.allowed_users.clone(),
            allowed_chats: config.allowed_chats.clone(),
            max_download_bytes: config.media.max_download_bytes,
            client,
            token: Arc::new(Mutex::new(None)),
        })
    }

    fn events_path(&self) -> String {
        format!("/{}/events", self.channel)
    }

    fn webhook_path(&self) -> String {
        format!("/{}/webhook", self.channel)
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}{}", self.api_base.trim_end_matches('/'), path)
    }

    fn tenant_access_token(&self) -> Result<String> {
        {
            let guard = self.token.lock().expect("feishu token mutex poisoned");
            if let Some(token) = guard.as_ref() {
                if token.expires_at > Instant::now() + Duration::from_secs(60) {
                    return Ok(token.value.clone());
                }
            }
        }

        let response = self
            .client
            .post(self.api_url("/open-apis/auth/v3/tenant_access_token/internal"))
            .json(&json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .context("feishu tenant access token request failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .context("feishu tenant access token returned invalid JSON")?;
        if !status.is_success() || value["code"].as_i64().unwrap_or(-1) != 0 {
            bail!("feishu tenant access token failed with status {status}: {value}");
        }
        let token = value["tenant_access_token"]
            .as_str()
            .ok_or_else(|| anyhow!("feishu tenant_access_token missing"))?
            .to_string();
        let expire = value["expire"].as_u64().unwrap_or(7_200);
        *self.token.lock().expect("feishu token mutex poisoned") = Some(CachedFeishuToken {
            value: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expire),
        });
        Ok(token)
    }

    fn post_api<T: serde::Serialize>(&self, path: &str, body: &T) -> Result<Value> {
        let token = self.tenant_access_token()?;
        let response = self
            .client
            .post(self.api_url(path))
            .bearer_auth(token)
            .json(body)
            .send()
            .with_context(|| format!("feishu POST {path} failed"))?;
        let status = response.status();
        let value: Value = response
            .json()
            .with_context(|| format!("feishu POST {path} returned invalid JSON"))?;
        if !status.is_success() || value["code"].as_i64().unwrap_or(-1) != 0 {
            bail!("feishu POST {path} failed with status {status}: {value}");
        }
        Ok(value)
    }

    fn put_api<T: serde::Serialize>(&self, path: &str, body: &T) -> Result<Value> {
        let token = self.tenant_access_token()?;
        let response = self
            .client
            .put(self.api_url(path))
            .bearer_auth(token)
            .json(body)
            .send()
            .with_context(|| format!("feishu PUT {path} failed"))?;
        let status = response.status();
        let value: Value = response
            .json()
            .with_context(|| format!("feishu PUT {path} returned invalid JSON"))?;
        if !status.is_success() || value["code"].as_i64().unwrap_or(-1) != 0 {
            bail!("feishu PUT {path} failed with status {status}: {value}");
        }
        Ok(value)
    }

    fn post_multipart_api(&self, path: &str, form: multipart::Form) -> Result<Value> {
        let token = self.tenant_access_token()?;
        let response = self
            .client
            .post(self.api_url(path))
            .bearer_auth(token)
            .multipart(form)
            .send()
            .with_context(|| format!("feishu multipart POST {path} failed"))?;
        let status = response.status();
        let value: Value = response
            .json()
            .with_context(|| format!("feishu multipart POST {path} returned invalid JSON"))?;
        if !status.is_success() || value["code"].as_i64().unwrap_or(-1) != 0 {
            bail!("feishu multipart POST {path} failed with status {status}: {value}");
        }
        Ok(value)
    }

    fn send_feishu_message(
        &self,
        route: &GatewayRoute,
        msg_type: &str,
        content: Value,
    ) -> Result<()> {
        self.send_feishu_message_with_response(route, msg_type, content)
            .map(|_| ())
    }

    fn send_feishu_message_with_response(
        &self,
        route: &GatewayRoute,
        msg_type: &str,
        content: Value,
    ) -> Result<Value> {
        let content_text =
            serde_json::to_string(&content).context("failed to serialize Feishu content")?;
        let value = if let Some(reply_to) = route.key.thread_id.as_deref() {
            let path = format!(
                "/open-apis/im/v1/messages/{}/reply",
                encode_component(reply_to)
            );
            self.post_api(
                &path,
                &json!({
                    "msg_type": msg_type,
                    "content": content_text,
                }),
            )?
        } else {
            self.post_api(
                "/open-apis/im/v1/messages?receive_id_type=chat_id",
                &json!({
                    "receive_id": route.key.conversation_id,
                    "msg_type": msg_type,
                    "content": content_text,
                }),
            )?
        };
        Ok(value)
    }

    fn update_feishu_message(
        &self,
        message_id: &str,
        msg_type: &str,
        content: Value,
    ) -> Result<()> {
        let content_text =
            serde_json::to_string(&content).context("failed to serialize Feishu update content")?;
        let path = format!("/open-apis/im/v1/messages/{}", encode_component(message_id));
        self.put_api(
            &path,
            &json!({
                "msg_type": msg_type,
                "content": content_text,
            }),
        )?;
        Ok(())
    }

    fn send_text_chunk(&self, route: &GatewayRoute, text: &str) -> Result<()> {
        let (msg_type, content) = feishu_outbound_payload(text);
        match self.send_feishu_message(route, msg_type, content) {
            Ok(()) => Ok(()),
            Err(error)
                if msg_type == "post"
                    && error
                        .to_string()
                        .to_ascii_lowercase()
                        .contains("content format of the post type is incorrect") =>
            {
                self.send_feishu_message(
                    route,
                    "text",
                    json!({"text": strip_markdown_to_plain_text(text)}),
                )
                .with_context(|| {
                    format!(
                        "Feishu/Lark rejected markdown post payload; fallback text also failed: {error:#}"
                    )
                })
            }
            Err(error) => Err(error),
        }
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
            return self.send_text_chunk(route, &text);
        }
        let path = Path::new(path);
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read Feishu upload file {}", path.display()))?;
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("duckagent-upload")
            .to_string();
        let mime = guess_mime_from_path(path);
        if mime.starts_with("image/") {
            let image_key = self.upload_image(bytes, &filename, &mime)?;
            if let Some(caption) = caption.filter(|value| !value.trim().is_empty()) {
                self.send_text_chunk(route, caption)?;
            }
            self.send_feishu_message(route, "image", json!({"image_key": image_key}))?;
        } else {
            let file_key = self.upload_file(bytes, &filename, &mime)?;
            if let Some(caption) = caption.filter(|value| !value.trim().is_empty()) {
                self.send_text_chunk(route, caption)?;
            }
            self.send_feishu_message(route, "file", json!({"file_key": file_key}))?;
        }
        Ok(())
    }

    fn upload_image(&self, bytes: Vec<u8>, filename: &str, mime: &str) -> Result<String> {
        let part = multipart::Part::bytes(bytes.clone())
            .file_name(filename.to_string())
            .mime_str(mime)
            .unwrap_or_else(|_| multipart::Part::bytes(bytes).file_name(filename.to_string()));
        let form = multipart::Form::new()
            .text("image_type", "message")
            .part("image", part);
        let value = self.post_multipart_api("/open-apis/im/v1/images", form)?;
        value["data"]["image_key"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("feishu image upload missing image_key"))
    }

    fn upload_file(&self, bytes: Vec<u8>, filename: &str, mime: &str) -> Result<String> {
        let part = multipart::Part::bytes(bytes.clone())
            .file_name(filename.to_string())
            .mime_str(mime)
            .unwrap_or_else(|_| multipart::Part::bytes(bytes).file_name(filename.to_string()));
        let form = multipart::Form::new()
            .text("file_type", feishu_file_type(filename))
            .text("file_name", filename.to_string())
            .part("file", part);
        let value = self.post_multipart_api("/open-apis/im/v1/files", form)?;
        value["data"]["file_key"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("feishu file upload missing file_key"))
    }

    fn handle_events(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        self.verify_signature(&request)?;
        let value: Value =
            serde_json::from_slice(&request.body).context("failed to parse Feishu event JSON")?;
        if value.get("encrypt").is_some() {
            bail!(
                "encrypted Feishu callbacks are not supported by this adapter; disable event encryption or configure a decrypt-capable adapter"
            );
        }
        self.verify_token(&value)?;
        if value["type"].as_str() == Some("url_verification") {
            let challenge = value["challenge"]
                .as_str()
                .ok_or_else(|| anyhow!("Feishu url_verification missing challenge"))?;
            return Ok(json_response(200, json!({"challenge": challenge})));
        }

        match feishu_event_type(&value).as_deref() {
            Some("im.message.receive_v1")
            | Some("card.action.trigger")
            | Some("interactive.card.action") => self.dispatch_event_value(&value, &inbound)?,
            _ => {}
        }
        Ok(json_response(200, json!({"code": 0, "msg": "ok"})))
    }

    fn dispatch_event_value(&self, value: &Value, inbound: &GatewayInboundDispatch) -> Result<()> {
        match feishu_event_type(value).as_deref() {
            Some("im.message.receive_v1") => {
                if let Some(message) = self.event_to_inbound(value)? {
                    inbound.submit(message)?;
                }
            }
            Some("card.action.trigger") | Some("interactive.card.action") => {
                if let Some(message) = self.card_action_to_inbound(value) {
                    inbound.submit(message)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn verify_token(&self, value: &Value) -> Result<()> {
        let Some(expected) = self.verification_token.as_deref() else {
            return Ok(());
        };
        let actual = value["token"]
            .as_str()
            .or_else(|| value["header"]["token"].as_str());
        match actual {
            Some(actual) if constant_time_eq(actual.as_bytes(), expected.as_bytes()) => Ok(()),
            Some(_) => bail!("Feishu verification token mismatch"),
            None => bail!("Feishu verification token missing"),
        }
    }

    fn verify_signature(&self, request: &ChannelHttpRequest) -> Result<()> {
        let Some(secret) = self.signing_secret.as_deref() else {
            return Ok(());
        };
        let timestamp = request
            .header("x-lark-request-timestamp")
            .ok_or_else(|| anyhow!("Feishu request missing timestamp"))?;
        let nonce = request
            .header("x-lark-request-nonce")
            .ok_or_else(|| anyhow!("Feishu request missing nonce"))?;
        let signature = request
            .header("x-lark-signature")
            .ok_or_else(|| anyhow!("Feishu request missing signature"))?;
        let timestamp_seconds = timestamp
            .parse::<i64>()
            .context("Feishu timestamp is not an integer")?;
        let now = chrono::Utc::now().timestamp();
        if (now - timestamp_seconds).abs() > 60 * 5 {
            bail!("Feishu request timestamp is outside tolerance");
        }
        let mut hasher = Sha256::new();
        hasher.update(timestamp.as_bytes());
        hasher.update(nonce.as_bytes());
        hasher.update(secret.as_bytes());
        hasher.update(&request.body);
        let expected = to_hex(&hasher.finalize());
        if !constant_time_eq(
            signature.to_ascii_lowercase().as_bytes(),
            expected.as_bytes(),
        ) {
            bail!("Feishu request signature mismatch");
        }
        Ok(())
    }

    fn event_to_inbound(&self, value: &Value) -> Result<Option<InboundMessageInput>> {
        let event = &value["event"];
        let message = &event["message"];
        let chat_id = message["chat_id"]
            .as_str()
            .or_else(|| message["open_chat_id"].as_str())
            .ok_or_else(|| anyhow!("Feishu message event missing chat_id"))?;
        if !identity_allowed(&self.allowed_chats, chat_id) {
            return Ok(None);
        }
        let sender_ids = feishu_sender_ids(event);
        let sender_id = sender_ids.first().cloned();
        if !self.allowed_users.is_empty() {
            if !sender_ids
                .iter()
                .any(|sender_id| identity_allowed(&self.allowed_users, sender_id))
            {
                return Ok(None);
            }
        }
        let message_id = message["message_id"].as_str().map(str::to_string);
        let message_type = message["message_type"].as_str().unwrap_or("text");
        let raw_content = message["content"].as_str().unwrap_or("{}");
        let content = parse_content(raw_content);
        let mut text = normalize_feishu_content(message_type, &content);
        let attachments = match message_id.as_deref() {
            Some(message_id) => self.collect_attachments(message_type, message_id, &content),
            None => Vec::new(),
        };
        if text.trim().is_empty() && attachments.is_empty() {
            text = format!("[Feishu {message_type} message]");
        }

        Ok(Some(InboundMessageInput {
            channel: self.channel.clone(),
            conversation_id: chat_id.to_string(),
            thread_id: message["thread_id"]
                .as_str()
                .or_else(|| message["root_id"].as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            chat_type: message["chat_type"].as_str().map(str::to_string),
            sender_id,
            message_id,
            text,
            attachments,
            timestamp: feishu_timestamp(value, message),
        }))
    }

    fn collect_attachments(
        &self,
        message_type: &str,
        message_id: &str,
        content: &Value,
    ) -> Vec<InboundAttachmentInput> {
        let refs = feishu_media_refs(message_type, content);
        refs.into_iter()
            .filter_map(
                |reference| match self.download_resource(message_id, &reference) {
                    Ok(attachment) => Some(attachment),
                    Err(error) => {
                        eprintln!("feishu attachment skipped: {error:#}");
                        None
                    }
                },
            )
            .collect()
    }

    fn download_resource(
        &self,
        message_id: &str,
        reference: &FeishuMediaRef,
    ) -> Result<InboundAttachmentInput> {
        let token = self.tenant_access_token()?;
        let path = format!(
            "/open-apis/im/v1/messages/{}/resources/{}?type={}",
            encode_component(message_id),
            encode_component(&reference.key),
            reference.resource_type
        );
        let response = self
            .client
            .get(self.api_url(&path))
            .bearer_auth(token)
            .send()
            .context("feishu resource download failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("feishu resource download failed with status {status}");
        }
        if let Some(length) = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            if length > self.max_download_bytes {
                bail!(
                    "feishu resource is {length} bytes, over max_download_bytes {}",
                    self.max_download_bytes
                );
            }
        }
        let mime = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).to_string())
            .unwrap_or_else(|| reference.mime.clone());
        let bytes = response
            .bytes()
            .context("feishu resource body unreadable")?;
        if bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "feishu resource is {} bytes, over max_download_bytes {}",
                bytes.len(),
                self.max_download_bytes
            );
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: Some(reference.filename(&mime)),
            mime: Some(mime),
        })
    }

    fn card_action_to_inbound(&self, value: &Value) -> Option<InboundMessageInput> {
        let event = &value["event"];
        let action_value = &event["action"]["value"];
        let command = feishu_action_to_command(action_value)?;
        let chat_id = event["context"]["open_chat_id"]
            .as_str()
            .or_else(|| event["open_chat_id"].as_str())
            .or_else(|| event["chat_id"].as_str())?;
        if !identity_allowed(&self.allowed_chats, chat_id) {
            return None;
        }
        let operator_id = event["operator"]["open_id"]
            .as_str()
            .or_else(|| event["operator"]["user_id"].as_str())
            .or_else(|| event["operator"]["union_id"].as_str())
            .map(str::to_string);
        if !self.allowed_users.is_empty()
            && !operator_id
                .as_deref()
                .is_some_and(|id| identity_allowed(&self.allowed_users, id))
        {
            return None;
        }
        Some(InboundMessageInput {
            channel: self.channel.clone(),
            conversation_id: chat_id.to_string(),
            thread_id: event["message_id"].as_str().map(str::to_string),
            chat_type: event["context"]["chat_type"].as_str().map(str::to_string),
            sender_id: operator_id,
            message_id: event["token"].as_str().map(str::to_string),
            text: command,
            attachments: Vec::new(),
            timestamp: Some(now_rfc3339_like()),
        })
    }
}

impl ChannelAdapter for FeishuAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        if self.transport == "websocket" {
            let adapter = self.clone();
            spawn_feishu_ws_loop(
                FeishuWsConfig {
                    channel: self.channel.clone(),
                    api_base: self.api_base.clone(),
                    app_id: self.app_id.clone(),
                    app_secret: self.app_secret.clone(),
                },
                inbound,
                move |value, inbound| adapter.dispatch_event_value(&value, inbound),
            )?;
        }
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
        if request.path == self.events_path() || request.path == self.webhook_path() {
            return self.handle_events(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let mut text_sent = false;
        for chunk in feishu_text_chunks(&message.text) {
            self.send_text_chunk(route, &chunk)?;
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

    fn send_stream_start(
        &self,
        route: &GatewayRoute,
        text: &str,
    ) -> Result<Option<StreamMessageHandle>> {
        let (msg_type, content) = feishu_outbound_payload(text);
        let value = self.send_feishu_message_with_response(route, msg_type, content)?;
        let message_id = feishu_response_message_id(&value)
            .ok_or_else(|| anyhow!("Feishu/Lark stream start did not return message_id"))?;
        Ok(Some(StreamMessageHandle { message_id }))
    }

    fn update_stream(
        &self,
        _route: &GatewayRoute,
        handle: &StreamMessageHandle,
        text: &str,
        _final_update: bool,
    ) -> Result<()> {
        let (msg_type, content) = feishu_outbound_payload(text);
        match self.update_feishu_message(&handle.message_id, msg_type, content) {
            Ok(()) => Ok(()),
            Err(error)
                if msg_type == "post"
                    && error
                        .to_string()
                        .to_ascii_lowercase()
                        .contains("content format of the post type is incorrect") =>
            {
                self.update_feishu_message(
                    &handle.message_id,
                    "text",
                    json!({"text": strip_markdown_to_plain_text(text)}),
                )
            }
            Err(error) => Err(error),
        }
    }

    fn stream_text_limit(&self) -> usize {
        FEISHU_TEXT_LIMIT
    }

    fn stream_min_delta_chars(&self) -> usize {
        FEISHU_STREAM_MIN_DELTA_CHARS
    }

    fn stream_flush_interval(&self) -> Duration {
        FEISHU_STREAM_FLUSH_INTERVAL
    }

    fn stream_update_budget(&self) -> Option<usize> {
        Some(FEISHU_STREAM_UPDATE_BUDGET)
    }

    fn send_typing(&self, _route: &GatewayRoute, _event: TypingEvent) -> Result<()> {
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        self.send_feishu_message(route, "interactive", feishu_approval_card(&prompt))
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: true,
            typing: false,
            approval_prompt: true,
        }
    }
}

#[derive(Debug, Clone)]
struct FeishuMediaRef {
    key: String,
    filename: Option<String>,
    mime: String,
    resource_type: &'static str,
}

impl FeishuMediaRef {
    fn filename(&self, mime: &str) -> String {
        self.filename.clone().unwrap_or_else(|| {
            format!(
                "feishu-{}{}",
                self.key,
                extension_for_mime(mime).unwrap_or("")
            )
        })
    }
}

fn feishu_event_type(value: &Value) -> Option<String> {
    value["header"]["event_type"]
        .as_str()
        .or_else(|| value["type"].as_str())
        .map(str::to_string)
}

fn feishu_sender_ids(event: &Value) -> Vec<String> {
    let id = &event["sender"]["sender_id"];
    ["open_id", "user_id", "union_id"]
        .into_iter()
        .filter_map(|key| id[key].as_str())
        .map(str::to_string)
        .collect()
}

fn feishu_timestamp(envelope: &Value, message: &Value) -> Option<String> {
    message["create_time"]
        .as_str()
        .or_else(|| envelope["header"]["create_time"].as_str())
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|millis| chrono::DateTime::from_timestamp_millis(millis))
        .map(|value| value.to_rfc3339())
}

fn parse_content(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn normalize_feishu_content(message_type: &str, content: &Value) -> String {
    match message_type {
        "text" => content["text"].as_str().unwrap_or_default().to_string(),
        "image" => "[Image]".to_string(),
        "audio" => "[Audio]".to_string(),
        "media" => "[Video]".to_string(),
        "file" => content["file_name"]
            .as_str()
            .map(|name| format!("[File] {name}"))
            .unwrap_or_else(|| "[File]".to_string()),
        "post" | "interactive" | "merge_forward" | "share_chat" => {
            let mut texts = Vec::new();
            collect_text_values(content, &mut texts);
            texts.join("\n")
        }
        other => {
            let mut texts = Vec::new();
            collect_text_values(content, &mut texts);
            if texts.is_empty() {
                format!("[Feishu {other} message]")
            } else {
                texts.join("\n")
            }
        }
    }
}

fn collect_text_values(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if !trimmed.is_empty() && !looks_like_feishu_id(trimmed) {
                out.push(trimmed.to_string());
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_text_values(item, out);
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "text" | "content" | "title" | "name" | "summary" | "description"
                ) {
                    collect_text_values(value, out);
                } else if !matches!(
                    key.as_str(),
                    "tag"
                        | "type"
                        | "msg_type"
                        | "message_type"
                        | "chat_id"
                        | "file_key"
                        | "image_key"
                        | "user_id"
                        | "open_id"
                        | "union_id"
                ) {
                    collect_text_values(value, out);
                }
            }
        }
        _ => {}
    }
}

fn feishu_media_refs(message_type: &str, content: &Value) -> Vec<FeishuMediaRef> {
    let mut refs = Vec::new();
    collect_media_refs(message_type, content, &mut refs);
    refs
}

fn collect_media_refs(message_type: &str, value: &Value, out: &mut Vec<FeishuMediaRef>) {
    match value {
        Value::Object(map) => {
            if let Some(key) = map.get("image_key").and_then(Value::as_str) {
                out.push(FeishuMediaRef {
                    key: key.to_string(),
                    filename: map
                        .get("file_name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    mime: "image/jpeg".to_string(),
                    resource_type: "image",
                });
            }
            if let Some(key) = map
                .get("file_key")
                .or_else(|| map.get("media_key"))
                .and_then(Value::as_str)
            {
                let mime = match message_type {
                    "audio" => "audio/ogg",
                    "media" => "video/mp4",
                    _ => "application/octet-stream",
                };
                out.push(FeishuMediaRef {
                    key: key.to_string(),
                    filename: map
                        .get("file_name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    mime: mime.to_string(),
                    resource_type: "file",
                });
            }
            for value in map.values() {
                collect_media_refs(message_type, value, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_media_refs(message_type, item, out);
            }
        }
        _ => {}
    }
}

fn feishu_action_to_command(value: &Value) -> Option<String> {
    if let Some(action) = value["duckagent_action"].as_str() {
        let id = value["approval_id"].as_str()?;
        return match action {
            "approve" => Some(format!(
                "/approve {id} {}",
                value["decision"].as_str().unwrap_or("once")
            )),
            "deny" => Some(format!("/deny {id}")),
            _ => None,
        };
    }
    None
}

fn feishu_approval_card(prompt: &GatewayApprovalPrompt) -> Value {
    let content = feishu_approval_card_content(prompt);
    json!({
        "config": {"wide_screen_mode": true},
        "header": {
            "template": "orange",
            "title": {"tag": "plain_text", "content": "DuckAgent approval"}
        },
        "elements": [
            {
                "tag": "markdown",
                "content": content
            },
            {
                "tag": "action",
                "actions": [
                    feishu_button("Once", "approve", &prompt.id, Some("once"), "primary"),
                    feishu_button("Session", "approve", &prompt.id, Some("session"), "default"),
                    feishu_button("Always", "approve", &prompt.id, Some("always"), "default"),
                    feishu_button("Deny", "deny", &prompt.id, None, "danger")
                ]
            }
        ]
    })
}

fn feishu_approval_card_content(prompt: &GatewayApprovalPrompt) -> String {
    let mut content = format!("Approval required.\n\nCommand:\n```{}\n```", prompt.command);
    if !prompt.rule_hits.is_empty() {
        content.push_str("\n\nRules:\n");
        for rule in &prompt.rule_hits {
            content.push_str("- ");
            content.push_str(rule.trim());
            content.push('\n');
        }
        content.truncate(content.trim_end().len());
    }
    content
}

fn feishu_button(
    text: &str,
    action: &str,
    approval_id: &str,
    decision: Option<&str>,
    button_type: &str,
) -> Value {
    let mut value = json!({
        "duckagent_action": action,
        "approval_id": approval_id,
    });
    if let Some(decision) = decision {
        value["decision"] = json!(decision);
    }
    json!({
        "tag": "button",
        "text": {"tag": "plain_text", "content": text},
        "type": button_type,
        "value": value,
    })
}

fn feishu_text_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.len() + ch.len_utf8() > FEISHU_TEXT_LIMIT && !current.is_empty() {
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

fn feishu_outbound_payload(text: &str) -> (&'static str, Value) {
    if feishu_markdown_table_re().is_match(text) {
        return ("text", json!({"text": text}));
    }
    if feishu_markdown_hint_re().is_match(text) {
        return ("post", feishu_markdown_post_payload(text));
    }
    ("text", json!({"text": text}))
}

fn feishu_response_message_id(value: &Value) -> Option<String> {
    value["data"]["message_id"]
        .as_str()
        .or_else(|| value["data"]["message"]["message_id"].as_str())
        .or_else(|| value["message_id"].as_str())
        .map(str::to_string)
}

fn feishu_markdown_hint_re() -> &'static Regex {
    FEISHU_MARKDOWN_HINT_RE.get_or_init(|| {
        Regex::new(
            r"(?m)(^#{1,6}\s)|(^\s*[-*]\s)|(^\s*\d+\.\s)|(^\s*---+\s*$)|(```)|(`[^`\n]+`)|(\*\*[^*\n].+?\*\*)|(~~[^~\n].+?~~)|(<u>.+?</u>)|(\*[^*\n]+\*)|(\[[^\]]+\]\([^)]+\))|(^>\s)",
        )
        .expect("valid Feishu markdown hint regex")
    })
}

fn feishu_markdown_table_re() -> &'static Regex {
    FEISHU_MARKDOWN_TABLE_RE.get_or_init(|| {
        Regex::new(r"(?m)^\|.*\|\n\|[-|: ]+\|").expect("valid Feishu markdown table regex")
    })
}

fn feishu_markdown_link_re() -> &'static Regex {
    FEISHU_MARKDOWN_LINK_RE.get_or_init(|| {
        Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").expect("valid Feishu markdown link regex")
    })
}

fn feishu_markdown_post_payload(content: &str) -> Value {
    json!({
        "zh_cn": {
            "content": feishu_markdown_post_rows(content),
        }
    })
}

fn feishu_markdown_post_rows(content: &str) -> Vec<Vec<Value>> {
    if content.is_empty() {
        return vec![vec![json!({"tag": "md", "text": ""})]];
    }
    if !content.contains("```") {
        return vec![vec![json!({"tag": "md", "text": content})]];
    }

    let mut rows = Vec::new();
    let mut current = Vec::new();
    let mut in_code_block = false;

    for line in content.lines() {
        let is_fence = feishu_fence_line(line, in_code_block);
        if is_fence {
            if !in_code_block {
                flush_feishu_markdown_row(&mut rows, &mut current);
            }
            current.push(line.to_string());
            in_code_block = !in_code_block;
            if !in_code_block {
                flush_feishu_markdown_row(&mut rows, &mut current);
            }
            continue;
        }
        current.push(line.to_string());
    }
    flush_feishu_markdown_row(&mut rows, &mut current);
    if rows.is_empty() {
        rows.push(vec![json!({"tag": "md", "text": content})]);
    }
    rows
}

fn feishu_fence_line(line: &str, in_code_block: bool) -> bool {
    let trimmed = line.trim();
    if in_code_block {
        trimmed == "```"
    } else {
        trimmed
            .strip_prefix("```")
            .is_some_and(|suffix| !suffix.contains('`'))
    }
}

fn flush_feishu_markdown_row(rows: &mut Vec<Vec<Value>>, current: &mut Vec<String>) {
    if current.is_empty() {
        return;
    }
    let segment = current.join("\n");
    if !segment.trim().is_empty() {
        rows.push(vec![json!({"tag": "md", "text": segment})]);
    }
    current.clear();
}

fn strip_markdown_to_plain_text(text: &str) -> String {
    let mut plain = text.replace("\r\n", "\n");
    plain = feishu_markdown_link_re()
        .replace_all(&plain, |captures: &regex::Captures<'_>| {
            let label = captures.get(1).map_or("", |value| value.as_str());
            let href = captures.get(2).map_or("", |value| value.as_str()).trim();
            if href.is_empty() {
                label.to_string()
            } else {
                format!("{label} ({href})")
            }
        })
        .to_string();

    let mut out = Vec::new();
    for line in plain.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            continue;
        }
        let line = line
            .trim_start_matches('>')
            .trim_start()
            .trim_start_matches("- ")
            .trim_start_matches("* ")
            .to_string();
        out.push(line);
    }

    out.join("\n")
        .replace("**", "")
        .replace("__", "")
        .replace("~~", "")
        .replace('`', "")
        .replace("<u>", "")
        .replace("</u>", "")
}

fn identity_allowed(allowed: &[String], id: &str) -> bool {
    allowed.is_empty()
        || allowed
            .iter()
            .any(|allowed| allowed.trim() == "*" || allowed.trim() == id)
}

fn feishu_file_type(filename: &str) -> &'static str {
    match Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("pdf") => "pdf",
        Some("doc") | Some("docx") => "doc",
        Some("xls") | Some("xlsx") => "xls",
        Some("ppt") | Some("pptx") => "ppt",
        _ => "stream",
    }
}

fn guess_mime_from_path(path: &Path) -> String {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("pdf") => "application/pdf",
        Some("txt") | Some("md") => "text/plain",
        Some("mp3") => "audio/mpeg",
        Some("ogg") | Some("opus") => "audio/ogg",
        Some("mp4") => "video/mp4",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn extension_for_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "image/jpeg" => Some(".jpg"),
        "image/png" => Some(".png"),
        "image/gif" => Some(".gif"),
        "image/webp" => Some(".webp"),
        "application/pdf" => Some(".pdf"),
        "audio/ogg" => Some(".ogg"),
        "audio/mpeg" => Some(".mp3"),
        "video/mp4" => Some(".mp4"),
        _ => None,
    }
}

fn looks_like_feishu_id(value: &str) -> bool {
    value.starts_with("ou_")
        || value.starts_with("on_")
        || value.starts_with("oc_")
        || value.starts_with("om_")
        || value.starts_with("img_")
        || value.starts_with("file_")
}

fn encode_component(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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

fn now_rfc3339_like() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn json_response(status: u16, value: Value) -> ChannelHttpResponse {
    ChannelHttpResponse {
        status,
        content_type: "application/json",
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"code\":1}".to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feishu_text_event_maps_to_inbound() -> Result<()> {
        let adapter = test_adapter()?;
        let value = json!({
            "schema": "2.0",
            "header": {
                "event_type": "im.message.receive_v1",
                "event_id": "evt_1",
                "create_time": "1710000000000",
                "token": "verify"
            },
            "event": {
                "sender": {"sender_id": {"open_id": "ou_user"}},
                "message": {
                    "message_id": "om_1",
                    "chat_id": "oc_chat",
                    "message_type": "text",
                    "content": "{\"text\":\"hello\"}"
                }
            }
        });
        let inbound = adapter.event_to_inbound(&value)?.expect("inbound");
        assert_eq!(inbound.channel, "feishu");
        assert_eq!(inbound.conversation_id, "oc_chat");
        assert_eq!(inbound.sender_id.as_deref(), Some("ou_user"));
        assert_eq!(inbound.message_id.as_deref(), Some("om_1"));
        assert_eq!(inbound.text, "hello");
        Ok(())
    }

    #[test]
    fn feishu_card_action_maps_to_approval_command() {
        let value = json!({
            "duckagent_action": "approve",
            "approval_id": "appr_1",
            "decision": "session"
        });
        assert_eq!(
            feishu_action_to_command(&value).as_deref(),
            Some("/approve appr_1 session")
        );
        let deny = json!({"duckagent_action": "deny", "approval_id": "appr_1"});
        assert_eq!(
            feishu_action_to_command(&deny).as_deref(),
            Some("/deny appr_1")
        );
    }

    #[test]
    fn feishu_approval_card_hides_text_fallback_commands() {
        let prompt = GatewayApprovalPrompt {
            id: "appr_1".to_string(),
            command: "ls /tmp".to_string(),
            options: vec!["once".to_string(), "session".to_string()],
            rule_hits: vec!["shell: read outside workspace".to_string()],
            message: "Reply with one of: /approve appr_1 once".to_string(),
        };
        let content = feishu_approval_card_content(&prompt);
        assert!(content.contains("Approval required."));
        assert!(content.contains("ls /tmp"));
        assert!(content.contains("shell: read outside workspace"));
        assert!(!content.contains("Reply with one"));
        assert!(!content.contains("/approve appr_1 once"));
    }

    #[test]
    fn feishu_text_chunks_respect_limit() {
        let text = "x".repeat(FEISHU_TEXT_LIMIT + 4);
        let chunks = feishu_text_chunks(&text);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| chunk.len() <= FEISHU_TEXT_LIMIT));
    }

    #[test]
    fn feishu_outbound_payload_uses_post_for_markdown() {
        let (msg_type, payload) = feishu_outbound_payload("**1** hello");
        assert_eq!(msg_type, "post");
        assert_eq!(payload["zh_cn"]["content"][0][0]["tag"], "md");
        assert_eq!(payload["zh_cn"]["content"][0][0]["text"], "**1** hello");
    }

    #[test]
    fn feishu_outbound_payload_keeps_markdown_tables_as_text() {
        let table = "| a | b |\n| - | - |\n| 1 | 2 |";
        let (msg_type, payload) = feishu_outbound_payload(table);
        assert_eq!(msg_type, "text");
        assert_eq!(payload["text"], table);
    }

    #[test]
    fn feishu_markdown_post_rows_split_fenced_blocks() {
        let rows = feishu_markdown_post_rows("before\n```rust\nfn main() {}\n```\nafter");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0]["text"], "before");
        assert_eq!(rows[1][0]["text"], "```rust\nfn main() {}\n```");
        assert_eq!(rows[2][0]["text"], "after");
    }

    #[test]
    fn feishu_signature_matches_lark_formula() -> Result<()> {
        let adapter = test_adapter()?;
        let body = br#"{"token":"verify"}"#.to_vec();
        let now = chrono::Utc::now().timestamp().to_string();
        let nonce = "nonce";
        let mut hasher = Sha256::new();
        hasher.update(now.as_bytes());
        hasher.update(nonce.as_bytes());
        hasher.update(b"sign");
        hasher.update(&body);
        let signature = to_hex(&hasher.finalize());
        adapter.verify_signature(&ChannelHttpRequest {
            method: "POST".to_string(),
            path: "/feishu/events".to_string(),
            query: Default::default(),
            headers: vec![
                ("x-lark-request-timestamp".to_string(), now),
                ("x-lark-request-nonce".to_string(), nonce.to_string()),
                ("x-lark-signature".to_string(), signature),
            ],
            body,
        })?;
        Ok(())
    }

    #[test]
    fn feishu_media_refs_extract_nested_keys() {
        let content = json!({
            "content": [[{"tag": "img", "image_key": "img_x"}]],
            "file_key": "file_x",
            "file_name": "demo.pdf"
        });
        let refs = feishu_media_refs("file", &content);
        assert!(refs.iter().any(|reference| reference.key == "img_x"));
        assert!(refs.iter().any(|reference| reference.key == "file_x"));
    }

    fn test_adapter() -> Result<FeishuAdapter> {
        FeishuAdapter::new(
            "feishu",
            &GatewayChannelConfig {
                ..test_gateway_config()
            },
            &GatewayCredentialEntry {
                channel: "feishu".to_string(),
                app_id: Some("cli_xxx".to_string()),
                app_secret: Some("secret".to_string()),
                webhook_secret: Some("verify".to_string()),
                signing_secret: Some("sign".to_string()),
                ..Default::default()
            },
        )
    }

    fn test_gateway_config() -> GatewayChannelConfig {
        GatewayChannelConfig {
            enabled: true,
            transport: Some("event_callback".to_string()),
            api_base: Some(DEFAULT_FEISHU_API_BASE.to_string()),
            allowed_users: Vec::new(),
            allowed_chats: Vec::new(),
            home: None,
            extra: Default::default(),
            typing: Default::default(),
            media: Default::default(),
            approval: Default::default(),
            access: Default::default(),
        }
    }
}
