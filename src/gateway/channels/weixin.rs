use super::super::{
    ChannelAdapter, ChannelCapabilities, GatewayApprovalPrompt, GatewayInboundDispatch,
    GatewayRoute, InboundMessageInput, OutboundMessage, TypingEvent,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use aes::Aes128;
use aes::cipher::{BlockEncrypt, KeyInit};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use url::form_urlencoded::byte_serialize;
use uuid::Uuid;

const ILINK_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const WEIXIN_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";
const EP_GET_UPDATES: &str = "ilink/bot/getupdates";
const EP_SEND_MESSAGE: &str = "ilink/bot/sendmessage";
const EP_SEND_TYPING: &str = "ilink/bot/sendtyping";
const EP_GET_UPLOAD_URL: &str = "ilink/bot/getuploadurl";
const CHANNEL_VERSION: &str = "2.2.0";
const ILINK_APP_ID: &str = "bot";
const ILINK_APP_CLIENT_VERSION: i64 = (2 << 16) | (2 << 8);
const WEIXIN_TEXT_LIMIT: usize = 2_000;
const MSG_TYPE_BOT: i64 = 2;
const MSG_STATE_FINISH: i64 = 2;
const ITEM_TEXT: i64 = 1;
const ITEM_IMAGE: i64 = 2;
const ITEM_VIDEO: i64 = 3;
const ITEM_FILE: i64 = 4;
const ITEM_VOICE: i64 = 5;
const MEDIA_IMAGE: i64 = 1;
const MEDIA_VIDEO: i64 = 2;
const MEDIA_FILE: i64 = 3;
const MEDIA_VOICE: i64 = 4;
const DEDUP_LIMIT: usize = 1_000;

#[derive(Clone)]
pub(in crate::gateway) struct WeixinAdapter {
    bot_user_id: Option<String>,
    token: String,
    base_url: String,
    cdn_base_url: String,
    allowed_users: HashSet<String>,
    allowed_groups: HashSet<String>,
    dm_policy: Policy,
    group_policy: Policy,
    client: Client,
    sync_buf: Arc<Mutex<String>>,
    context_tokens: Arc<Mutex<HashMap<String, String>>>,
    seen_message_ids: Arc<Mutex<VecDeque<String>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    Open,
    Allowlist,
    Disabled,
}

#[derive(Debug, Clone)]
struct WeixinMessage {
    message_id: String,
    sender_id: String,
    conversation_id: String,
    chat_type: String,
    text: String,
    context_token: Option<String>,
    media_notes: Vec<String>,
}

impl WeixinAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let token = credentials
            .token
            .as_deref()
            .or(credentials.api_key.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("weixin gateway credential requires token"))?
            .to_string();
        let client = Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .context("failed to build Weixin iLink HTTP client")?;
        Ok(Self {
            bot_user_id: config
                .extra
                .get("bot_user_id")
                .or_else(|| credentials.extra.get("bot_user_id"))
                .or_else(|| credentials.extra.get("bot_id"))
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            token,
            base_url: config
                .api_base
                .clone()
                .unwrap_or_else(|| ILINK_BASE_URL.to_string()),
            cdn_base_url: config
                .extra
                .get("cdn_base_url")
                .or_else(|| credentials.extra.get("cdn_base_url"))
                .map(|value| value.trim().trim_end_matches('/').to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| WEIXIN_CDN_BASE_URL.to_string()),
            allowed_users: config.allowed_users.iter().cloned().collect(),
            allowed_groups: config.allowed_chats.iter().cloned().collect(),
            dm_policy: parse_policy(config.extra.get("dm_policy").map(String::as_str), "open")?,
            group_policy: parse_policy(
                config.extra.get("group_policy").map(String::as_str),
                "disabled",
            )?,
            client,
            sync_buf: Arc::new(Mutex::new(String::new())),
            context_tokens: Arc::new(Mutex::new(HashMap::new())),
            seen_message_ids: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    fn api_url(&self, endpoint: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), endpoint)
    }

    fn api_post(&self, endpoint: &str, payload: Value, timeout_ms: u64) -> Result<Value> {
        let body = json_merge(
            payload,
            json!({"base_info": {"channel_version": CHANNEL_VERSION}}),
        );
        let body_text = serde_json::to_string(&body)?;
        let response = self
            .client
            .post(self.api_url(endpoint))
            .headers(ilink_headers(&self.token, &body_text)?)
            .body(body_text)
            .timeout(Duration::from_millis(timeout_ms))
            .send()
            .with_context(|| format!("Weixin iLink POST {endpoint} failed"))?;
        let status = response.status();
        let value: Value = response
            .json()
            .with_context(|| format!("Weixin iLink POST {endpoint} returned invalid JSON"))?;
        if !status.is_success() {
            bail!("Weixin iLink POST {endpoint} failed with status {status}: {value}");
        }
        Ok(value)
    }

    fn poll_loop(self, inbound: GatewayInboundDispatch) {
        loop {
            if let Err(error) = self.poll_once(&inbound) {
                eprintln!("weixin gateway poll failed: {error:#}");
                thread::sleep(Duration::from_secs(3));
            }
        }
    }

    fn poll_once(&self, inbound: &GatewayInboundDispatch) -> Result<()> {
        let sync_buf = self
            .sync_buf
            .lock()
            .expect("weixin sync mutex poisoned")
            .clone();
        let response =
            self.api_post(EP_GET_UPDATES, json!({"get_updates_buf": sync_buf}), 35_000)?;
        let ret = response["ret"].as_i64().unwrap_or(0);
        let errcode = response["errcode"].as_i64().unwrap_or(0);
        if ret != 0 || errcode != 0 {
            bail!(
                "Weixin getupdates failed ret={ret} errcode={errcode}: {}",
                response["errmsg"].as_str().unwrap_or_default()
            );
        }
        if let Some(next) = response["get_updates_buf"].as_str() {
            *self.sync_buf.lock().expect("weixin sync mutex poisoned") = next.to_string();
        }
        for raw in response["msgs"].as_array().into_iter().flatten() {
            if let Some(message) = parse_weixin_message(raw, self.bot_user_id.as_deref()) {
                if self.is_duplicate(&message.message_id) || !self.should_process(&message) {
                    continue;
                }
                if let Some(token) = message.context_token.as_deref() {
                    self.context_tokens
                        .lock()
                        .expect("weixin context mutex poisoned")
                        .insert(message.conversation_id.clone(), token.to_string());
                }
                let text = if message.media_notes.is_empty() {
                    message.text
                } else if message.text.trim().is_empty() {
                    message.media_notes.join("\n")
                } else {
                    format!("{}\n\n{}", message.text, message.media_notes.join("\n"))
                };
                inbound.submit(InboundMessageInput {
                    channel: "weixin".to_string(),
                    conversation_id: message.conversation_id,
                    thread_id: None,
                    chat_type: Some(
                        if message.chat_type == "group" {
                            "group"
                        } else {
                            "dm"
                        }
                        .to_string(),
                    ),
                    sender_id: Some(message.sender_id),
                    message_id: Some(message.message_id),
                    text,
                    attachments: Vec::new(),
                    timestamp: None,
                })?;
            }
        }
        Ok(())
    }

    fn is_duplicate(&self, message_id: &str) -> bool {
        if message_id.is_empty() {
            return false;
        }
        let mut guard = self
            .seen_message_ids
            .lock()
            .expect("weixin dedup mutex poisoned");
        if guard.iter().any(|seen| seen == message_id) {
            return true;
        }
        guard.push_back(message_id.to_string());
        while guard.len() > DEDUP_LIMIT {
            guard.pop_front();
        }
        false
    }

    fn should_process(&self, message: &WeixinMessage) -> bool {
        if message.chat_type == "group" {
            match self.group_policy {
                Policy::Disabled => false,
                Policy::Open => {
                    self.allowed_groups.is_empty()
                        || self.allowed_groups.contains(&message.conversation_id)
                }
                Policy::Allowlist => self.allowed_groups.contains(&message.conversation_id),
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

    fn send_text_chunk(&self, route: &GatewayRoute, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let context_token = self
            .context_tokens
            .lock()
            .expect("weixin context mutex poisoned")
            .get(&route.key.conversation_id)
            .cloned();
        let mut msg = json!({
            "from_user_id": "",
            "to_user_id": route.key.conversation_id,
            "client_id": format!("duckagent-weixin-{}", Uuid::now_v7().simple()),
            "message_type": MSG_TYPE_BOT,
            "message_state": MSG_STATE_FINISH,
            "item_list": [{"type": ITEM_TEXT, "text_item": {"text": text}}],
        });
        if let Some(context_token) = context_token {
            msg["context_token"] = json!(context_token);
        }
        let response = self.api_post(EP_SEND_MESSAGE, json!({"msg": msg}), 15_000)?;
        let ret = response["ret"].as_i64().unwrap_or(0);
        let errcode = response["errcode"].as_i64().unwrap_or(0);
        if ret != 0 || errcode != 0 {
            bail!("Weixin sendmessage failed ret={ret} errcode={errcode}: {response}");
        }
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
            return self.send_text_chunk(route, &text);
        }
        let path = Path::new(path);
        let plaintext = fs::read(path)
            .with_context(|| format!("failed to read Weixin upload file {}", path.display()))?;
        if plaintext.is_empty() {
            bail!("Weixin cannot upload empty media file {}", path.display());
        }
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("duckagent-upload.bin")
            .to_string();
        let (media_type, media_item) =
            self.build_outbound_media_item(route, &plaintext, &filename)?;
        if let Some(caption) = caption.filter(|value| !value.trim().is_empty()) {
            self.send_text_chunk(route, caption)?;
        }
        self.send_media_item(route, media_type, media_item)
    }

    fn build_outbound_media_item(
        &self,
        route: &GatewayRoute,
        plaintext: &[u8],
        filename: &str,
    ) -> Result<(i64, Value)> {
        let media_type = outbound_media_type(filename);
        let filekey = hex_lower(&rand::random::<[u8; 16]>());
        let aes_key = rand::random::<[u8; 16]>();
        let ciphertext = aes128_ecb_encrypt_pkcs7(plaintext, &aes_key);
        let ciphertext_size = ciphertext.len();
        let raw_md5 = md5_hex(plaintext);
        let upload = self.api_post(
            EP_GET_UPLOAD_URL,
            json!({
                "filekey": filekey,
                "media_type": media_type,
                "to_user_id": route.key.conversation_id,
                "rawsize": plaintext.len(),
                "rawfilemd5": raw_md5,
                "filesize": ciphertext_size,
                "no_need_thumb": true,
                "aeskey": hex_lower(&aes_key),
            }),
            15_000,
        )?;
        let upload_url = upload["upload_full_url"]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                upload["upload_param"]
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|param| cdn_upload_url(&self.cdn_base_url, param, &filekey))
            })
            .ok_or_else(|| {
                anyhow!("Weixin getuploadurl returned neither upload_full_url nor upload_param")
            })?;
        let encrypted_query_param = self.upload_ciphertext(&upload_url, ciphertext)?;
        let aes_key_for_api = BASE64_STANDARD.encode(hex_lower(&aes_key));
        Ok((
            media_type,
            outbound_media_item(
                media_type,
                filename,
                &encrypted_query_param,
                &aes_key_for_api,
                ciphertext_size,
                plaintext.len(),
                &raw_md5,
            ),
        ))
    }

    fn upload_ciphertext(&self, upload_url: &str, ciphertext: Vec<u8>) -> Result<String> {
        let response = self
            .client
            .post(upload_url)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(ciphertext)
            .timeout(Duration::from_secs(120))
            .send()
            .with_context(|| format!("Weixin CDN upload failed: {upload_url}"))?;
        let status = response.status();
        let encrypted = response
            .headers()
            .get("x-encrypted-param")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if status.is_success() {
            return encrypted.ok_or_else(|| anyhow!("Weixin CDN upload missing x-encrypted-param"));
        }
        let body = response.text().unwrap_or_default();
        bail!("Weixin CDN upload failed with status {status}: {body}");
    }

    fn send_media_item(&self, route: &GatewayRoute, _media_type: i64, item: Value) -> Result<()> {
        let context_token = self
            .context_tokens
            .lock()
            .expect("weixin context mutex poisoned")
            .get(&route.key.conversation_id)
            .cloned();
        let mut msg = json!({
            "from_user_id": "",
            "to_user_id": route.key.conversation_id,
            "client_id": format!("duckagent-weixin-{}", Uuid::now_v7().simple()),
            "message_type": MSG_TYPE_BOT,
            "message_state": MSG_STATE_FINISH,
            "item_list": [item],
        });
        if let Some(context_token) = context_token {
            msg["context_token"] = json!(context_token);
        }
        let response = self.api_post(EP_SEND_MESSAGE, json!({"msg": msg}), 15_000)?;
        let ret = response["ret"].as_i64().unwrap_or(0);
        let errcode = response["errcode"].as_i64().unwrap_or(0);
        if ret != 0 || errcode != 0 {
            bail!("Weixin send media failed ret={ret} errcode={errcode}: {response}");
        }
        Ok(())
    }
}

impl ChannelAdapter for WeixinAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        let adapter = self.clone();
        thread::spawn(move || adapter.poll_loop(inbound));
        Ok(())
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        for chunk in text_chunks(&message.text, WEIXIN_TEXT_LIMIT) {
            self.send_text_chunk(route, &chunk)?;
        }
        for path in message.media_paths {
            self.send_media_path(route, &path, None)?;
        }
        Ok(())
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        if !event.active {
            return Ok(());
        }
        let _ = self.api_post(
            EP_SEND_TYPING,
            json!({
                "to_user_id": route.key.conversation_id,
                "status": 1,
            }),
            5_000,
        );
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        self.send_text_chunk(route, &prompt.message)
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: true,
            typing: true,
            approval_prompt: true,
        }
    }
}

fn parse_weixin_message(value: &Value, bot_user_id: Option<&str>) -> Option<WeixinMessage> {
    let sender_id = value["from_user_id"].as_str()?.trim().to_string();
    if sender_id.is_empty() || bot_user_id.is_some_and(|bot_user_id| sender_id == bot_user_id) {
        return None;
    }
    let message_id = value["message_id"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    let room_id = value["room_id"]
        .as_str()
        .or_else(|| value["chat_room_id"].as_str())
        .unwrap_or_default()
        .trim();
    let to_user_id = value["to_user_id"].as_str().unwrap_or_default().trim();
    let is_group = !room_id.is_empty()
        || bot_user_id
            .is_some_and(|bot_user_id| !to_user_id.is_empty() && to_user_id != bot_user_id);
    let conversation_id = if is_group {
        room_id
            .is_empty()
            .then_some(to_user_id)
            .unwrap_or(room_id)
            .to_string()
    } else {
        sender_id.clone()
    };
    let items = value["item_list"].as_array().cloned().unwrap_or_default();
    let text = extract_weixin_text(&items);
    let media_notes = items.iter().filter_map(media_note).collect::<Vec<_>>();
    if text.trim().is_empty() && media_notes.is_empty() {
        return None;
    }
    Some(WeixinMessage {
        message_id,
        sender_id,
        conversation_id,
        chat_type: if is_group { "group" } else { "dm" }.to_string(),
        text,
        context_token: value["context_token"].as_str().map(str::to_string),
        media_notes,
    })
}

fn extract_weixin_text(items: &[Value]) -> String {
    items
        .iter()
        .filter(|item| item["type"].as_i64() == Some(ITEM_TEXT))
        .filter_map(|item| item["text_item"]["text"].as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn media_note(item: &Value) -> Option<String> {
    let item_type = item["type"].as_i64()?;
    if item_type == ITEM_TEXT {
        return None;
    }
    Some(format!(
        "[Weixin Media]\nitem_type: {item_type}\nraw: {}",
        serde_json::to_string(item).unwrap_or_else(|_| "{}".to_string())
    ))
}

fn ilink_headers(token: &str, body: &str) -> Result<reqwest::header::HeaderMap> {
    use reqwest::header::{HeaderMap, HeaderValue};
    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    headers.insert(
        "AuthorizationType",
        HeaderValue::from_static("ilink_bot_token"),
    );
    headers.insert("iLink-App-Id", HeaderValue::from_static(ILINK_APP_ID));
    headers.insert(
        "iLink-App-ClientVersion",
        HeaderValue::from_str(&ILINK_APP_CLIENT_VERSION.to_string())?,
    );
    headers.insert(
        "Content-Length",
        HeaderValue::from_str(&body.len().to_string())?,
    );
    headers.insert(
        "X-WECHAT-UIN",
        HeaderValue::from_str(&Uuid::now_v7().simple().to_string())?,
    );
    headers.insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    Ok(headers)
}

fn json_merge(mut left: Value, right: Value) -> Value {
    if let (Some(left), Some(right)) = (left.as_object_mut(), right.as_object()) {
        for (key, value) in right {
            left.insert(key.clone(), value.clone());
        }
    }
    left
}

fn parse_policy(raw: Option<&str>, default: &str) -> Result<Policy> {
    match raw.unwrap_or(default).trim().to_ascii_lowercase().as_str() {
        "" | "open" => Ok(Policy::Open),
        "allowlist" | "allow-list" => Ok(Policy::Allowlist),
        "disabled" | "off" => Ok(Policy::Disabled),
        other => bail!("invalid Weixin policy `{other}`"),
    }
}

fn text_chunks(text: &str, limit: usize) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed.chars().count() <= limit {
        return vec![trimmed.to_string()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in trimmed.chars() {
        if current.chars().count() >= limit {
            out.push(current.clone());
            current.clear();
        }
        current.push(ch);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn outbound_media_type(filename: &str) -> i64 {
    let ext = Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" => MEDIA_IMAGE,
        "mp4" | "mov" | "avi" | "mkv" | "webm" | "3gp" => MEDIA_VIDEO,
        "silk" => MEDIA_VOICE,
        _ => MEDIA_FILE,
    }
}

fn outbound_media_item(
    media_type: i64,
    filename: &str,
    encrypted_query_param: &str,
    aes_key_for_api: &str,
    ciphertext_size: usize,
    plaintext_size: usize,
    raw_md5: &str,
) -> Value {
    let media = json!({
        "encrypt_query_param": encrypted_query_param,
        "aes_key": aes_key_for_api,
        "encrypt_type": 1,
    });
    match media_type {
        MEDIA_IMAGE => json!({
            "type": ITEM_IMAGE,
            "image_item": {
                "media": media,
                "mid_size": ciphertext_size,
            },
        }),
        MEDIA_VIDEO => json!({
            "type": ITEM_VIDEO,
            "video_item": {
                "media": media,
                "video_size": ciphertext_size,
                "play_length": 0,
                "video_md5": raw_md5,
            },
        }),
        MEDIA_VOICE => json!({
            "type": ITEM_VOICE,
            "voice_item": {
                "media": media,
                "encode_type": 6,
                "bits_per_sample": 16,
                "sample_rate": 24000,
                "playtime": 0,
            },
        }),
        _ => json!({
            "type": ITEM_FILE,
            "file_item": {
                "media": media,
                "file_name": filename,
                "len": plaintext_size.to_string(),
            },
        }),
    }
}

fn aes_padded_size(len: usize) -> usize {
    len + (16 - (len % 16))
}

fn aes128_ecb_encrypt_pkcs7(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let cipher = Aes128::new_from_slice(key).expect("fixed-size AES key");
    let pad_len = 16 - (plaintext.len() % 16);
    let mut padded = Vec::with_capacity(plaintext.len() + pad_len);
    padded.extend_from_slice(plaintext);
    padded.extend(std::iter::repeat_n(pad_len as u8, pad_len));
    let mut out = Vec::with_capacity(padded.len());
    for chunk in padded.chunks_exact(16) {
        let mut block = aes::cipher::generic_array::GenericArray::clone_from_slice(chunk);
        cipher.encrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    out
}

fn cdn_upload_url(cdn_base_url: &str, upload_param: &str, filekey: &str) -> String {
    format!(
        "{}/upload?encrypted_query_param={}&filekey={}",
        cdn_base_url.trim_end_matches('/'),
        url_encode(upload_param),
        url_encode(filekey)
    )
}

fn url_encode(value: &str) -> String {
    byte_serialize(value.as_bytes()).collect()
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn md5_hex(input: &[u8]) -> String {
    let digest = md5_digest(input);
    hex_lower(&digest)
}

fn md5_digest(input: &[u8]) -> [u8; 16] {
    let mut message = input.to_vec();
    let bit_len = (message.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_le_bytes());

    let mut a0: u32 = 0x67452301;
    let mut b0: u32 = 0xefcdab89;
    let mut c0: u32 = 0x98badcfe;
    let mut d0: u32 = 0x10325476;
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    for chunk in message.chunks_exact(64) {
        let mut words = [0u32; 16];
        for (idx, word) in words.iter_mut().enumerate() {
            let start = idx * 4;
            *word = u32::from_le_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }
        let mut a = a0;
        let mut b = b0;
        let mut c = c0;
        let mut d = d0;
        for i in 0..64 {
            let (f, g) = if i < 16 {
                ((b & c) | ((!b) & d), i)
            } else if i < 32 {
                ((d & b) | ((!d) & c), (5 * i + 1) % 16)
            } else if i < 48 {
                (b ^ c ^ d, (3 * i + 5) % 16)
            } else {
                (c ^ (b | (!d)), (7 * i) % 16)
            };
            let temp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(K[i])
                    .wrapping_add(words[g])
                    .rotate_left(S[i]),
            );
            a = temp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weixin_message_extracts_text_and_context() {
        let message = parse_weixin_message(
            &json!({
                "from_user_id": "wxid_1",
                "to_user_id": "bot",
                "message_id": "m1",
                "context_token": "ctx",
                "item_list": [{"type": 1, "text_item": {"text": "hello"}}]
            }),
            Some("bot"),
        )
        .expect("message");
        assert_eq!(message.conversation_id, "wxid_1");
        assert_eq!(message.text, "hello");
        assert_eq!(message.context_token.as_deref(), Some("ctx"));
    }

    #[test]
    fn weixin_group_policy_defaults_disabled() -> Result<()> {
        let adapter = WeixinAdapter::new(
            &GatewayChannelConfig {
                ..Default::default()
            },
            &GatewayCredentialEntry {
                channel: "weixin".to_string(),
                token: Some("token".to_string()),
                ..Default::default()
            },
        )?;
        let message = parse_weixin_message(
            &json!({
                "from_user_id": "wxid_1",
                "to_user_id": "room@chatroom",
                "message_id": "m1",
                "item_list": [{"type": 1, "text_item": {"text": "hello"}}]
            }),
            Some("bot"),
        )
        .expect("message");
        assert_eq!(message.chat_type, "group");
        assert!(!adapter.should_process(&message));
        Ok(())
    }

    #[test]
    fn weixin_media_notes_are_preserved() {
        let message = parse_weixin_message(
            &json!({
                "from_user_id": "wxid_1",
                "message_id": "m1",
                "item_list": [{"type": 2, "image_item": {"aeskey": "abc"}}]
            }),
            Some("bot"),
        )
        .expect("message");
        assert!(message.media_notes[0].contains("[Weixin Media]"));
    }

    #[test]
    fn weixin_outbound_media_item_uses_base64_hex_aes_key() {
        let aes_key_for_api = BASE64_STANDARD.encode("00112233445566778899aabbccddeeff");
        let item = outbound_media_item(
            MEDIA_IMAGE,
            "demo.png",
            "enc-param",
            &aes_key_for_api,
            32,
            20,
            "md5",
        );
        assert_eq!(item["type"].as_i64(), Some(ITEM_IMAGE));
        assert_eq!(
            item["image_item"]["media"]["aes_key"].as_str(),
            Some(aes_key_for_api.as_str())
        );
        assert_eq!(
            item["image_item"]["media"]["encrypt_query_param"].as_str(),
            Some("enc-param")
        );
    }

    #[test]
    fn weixin_outbound_file_item_preserves_filename_and_plaintext_len() {
        let item = outbound_media_item(MEDIA_FILE, "report.pdf", "eq", "key", 48, 33, "md5");
        assert_eq!(item["type"].as_i64(), Some(ITEM_FILE));
        assert_eq!(item["file_item"]["file_name"].as_str(), Some("report.pdf"));
        assert_eq!(item["file_item"]["len"].as_str(), Some("33"));
    }

    #[test]
    fn weixin_crypto_helpers_match_known_values() {
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(aes_padded_size(0), 16);
        assert_eq!(aes_padded_size(16), 32);
        assert_eq!(
            aes128_ecb_encrypt_pkcs7(b"abc", &[0u8; 16]).len(),
            aes_padded_size(3)
        );
    }

    #[test]
    fn weixin_cdn_upload_url_escapes_query_parts() {
        assert_eq!(
            cdn_upload_url("https://cdn.example.com/c2c/", "a+b/c=", "key/1"),
            "https://cdn.example.com/c2c/upload?encrypted_query_param=a%2Bb%2Fc%3D&filekey=key%2F1"
        );
    }
}
