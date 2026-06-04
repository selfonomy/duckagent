use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundAttachmentInput,
    InboundMessageInput, OutboundMessage, TypingEvent,
};
use super::websocket::{
    ChannelWebSocket, is_transient_read_error, read_json_message as read_ws_json_message,
    send_json_message as send_ws_json_message, set_read_timeout,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use aes::Aes256;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use cbc::cipher::block_padding::NoPadding;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use reqwest::blocking::{Client, multipart};
use serde_json::{Value, json};
use sha1::{Digest as Sha1Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::connect;

const WECOM_CALLBACK_PATH: &str = "/wecom/callback";
const WECOM_EVENTS_PATH: &str = "/wecom/events";
const WECOM_CALLBACK_ALIAS_PATH: &str = "/wecom_callback/events";
const WECOM_API_BASE: &str = "https://qyapi.weixin.qq.com";
const ACCESS_TOKEN_TTL_SECONDS: u64 = 7_200;
const WECOM_BOT_DEFAULT_WS_URL: &str = "wss://openws.work.weixin.qq.com";
const WECOM_BOT_TEXT_LIMIT: usize = 4000;
const WECOM_BOT_SUBSCRIBE_CMD: &str = "aibot_subscribe";
const WECOM_BOT_SEND_CMD: &str = "aibot_send_msg";
const WECOM_BOT_RESPONSE_CMD: &str = "aibot_respond_msg";
const WECOM_BOT_MSG_CALLBACK_CMD: &str = "aibot_msg_callback";
const WECOM_BOT_EVENT_CALLBACK_CMD: &str = "aibot_event_callback";
const WECOM_BOT_UPLOAD_INIT_CMD: &str = "aibot_upload_media_init";
const WECOM_BOT_UPLOAD_CHUNK_CMD: &str = "aibot_upload_media_chunk";
const WECOM_BOT_UPLOAD_FINISH_CMD: &str = "aibot_upload_media_finish";
const WECOM_BOT_READ_TIMEOUT_SECONDS: u64 = 2;
const WECOM_BOT_REQUEST_TIMEOUT_SECONDS: u64 = 30;
const WECOM_BOT_UPLOAD_CHUNK_BYTES: usize = 512 * 1024;
const WECOM_BOT_MAX_MEDIA_BYTES: usize = 20 * 1024 * 1024;
const WECOM_BOT_CACHE_LIMIT: usize = 10_000;

static WECOM_BOT_REQ_COUNTER: AtomicU64 = AtomicU64::new(1);

type Aes256CbcDec = cbc::Decryptor<Aes256>;

#[derive(Clone)]
pub(in crate::gateway) struct WeComAdapter {
    channel: String,
    mode: WeComMode,
    corp_id: String,
    corp_secret: String,
    agent_id: String,
    token: String,
    encoding_aes_key: String,
    allowed_users: HashSet<String>,
    allowed_chats: HashSet<String>,
    dm_policy: String,
    group_policy: String,
    api_base: String,
    client: Client,
    access_token: Arc<Mutex<Option<CachedWeComToken>>>,
    seen_messages: Arc<Mutex<HashMap<String, Instant>>>,
    bot_id: String,
    bot_secret: String,
    bot_websocket_url: String,
    bot_device_id: String,
    bot_socket: Arc<Mutex<Option<ChannelWebSocket>>>,
    bot_pending_responses: Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>,
    bot_reply_req_ids: Arc<Mutex<HashMap<String, String>>>,
    bot_last_chat_req_ids: Arc<Mutex<HashMap<String, String>>>,
    max_download_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WeComMode {
    AiBot,
    Callback,
}

#[derive(Clone)]
struct CachedWeComToken {
    token: String,
    expires_at: Instant,
}

#[derive(Debug, Clone)]
struct WeComPlainMessage {
    corp_id: String,
    user_id: String,
    content: String,
    msg_id: String,
    create_time: Option<String>,
}

impl WeComAdapter {
    pub(in crate::gateway) fn new(
        channel: &str,
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        if channel.trim().eq_ignore_ascii_case("wecom") {
            return Self::new_bot(channel, config, credentials);
        }
        Self::new_callback(channel, config, credentials)
    }

    fn new_bot(
        channel: &str,
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let bot_id = credentials
            .app_id
            .as_deref()
            .or(credentials.api_key.as_deref())
            .or_else(|| credentials.extra.get("bot_id").map(String::as_str))
            .or_else(|| config.extra.get("bot_id").map(String::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("wecom gateway credential requires bot_id/app_id"))?
            .to_string();
        let bot_secret = credentials
            .app_secret
            .as_deref()
            .or(credentials.client_secret.as_deref())
            .or(credentials.token.as_deref())
            .or_else(|| credentials.extra.get("secret").map(String::as_str))
            .or_else(|| credentials.extra.get("bot_secret").map(String::as_str))
            .or_else(|| config.extra.get("secret").map(String::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("wecom gateway credential requires bot secret/app_secret"))?
            .to_string();
        let bot_websocket_url = credentials
            .extra
            .get("websocket_url")
            .or_else(|| credentials.extra.get("websocketUrl"))
            .or_else(|| config.extra.get("websocket_url"))
            .or_else(|| config.extra.get("websocketUrl"))
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(WECOM_BOT_DEFAULT_WS_URL)
            .to_string();
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build WeCom HTTP client")?;
        Ok(Self {
            channel: channel.to_string(),
            mode: WeComMode::AiBot,
            corp_id: String::new(),
            corp_secret: String::new(),
            agent_id: String::new(),
            token: String::new(),
            encoding_aes_key: String::new(),
            allowed_users: config
                .allowed_users
                .iter()
                .chain(split_extra_list(config.extra.get("allowed_users")).iter())
                .map(|value| normalize_wecom_allow_entry(value))
                .collect(),
            allowed_chats: config
                .allowed_chats
                .iter()
                .chain(split_extra_list(config.extra.get("allowed_chats")).iter())
                .map(|value| normalize_wecom_allow_entry(value))
                .collect(),
            dm_policy: parse_wecom_access_policy(
                config.extra.get("dm_policy").map(String::as_str),
                "open",
            )?,
            group_policy: parse_wecom_access_policy(
                config.extra.get("group_policy").map(String::as_str),
                "open",
            )?,
            api_base: config
                .api_base
                .clone()
                .unwrap_or_else(|| WECOM_API_BASE.to_string()),
            client,
            access_token: Arc::new(Mutex::new(None)),
            seen_messages: Arc::new(Mutex::new(HashMap::new())),
            bot_id,
            bot_secret,
            bot_websocket_url,
            bot_device_id: new_bot_device_id(),
            bot_socket: Arc::new(Mutex::new(None)),
            bot_pending_responses: Arc::new(Mutex::new(HashMap::new())),
            bot_reply_req_ids: Arc::new(Mutex::new(HashMap::new())),
            bot_last_chat_req_ids: Arc::new(Mutex::new(HashMap::new())),
            max_download_bytes: config.media.max_download_bytes,
        })
    }

    fn new_callback(
        channel: &str,
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let corp_id = credentials
            .app_id
            .as_deref()
            .or_else(|| credentials.extra.get("corp_id").map(String::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("wecom gateway credential requires corp_id/app_id"))?
            .to_string();
        let corp_secret = credentials
            .app_secret
            .as_deref()
            .or(credentials.client_secret.as_deref())
            .or(credentials.token.as_deref())
            .or_else(|| credentials.extra.get("corp_secret").map(String::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("wecom gateway credential requires corp_secret/app_secret"))?
            .to_string();
        let agent_id = credentials
            .extra
            .get("agent_id")
            .or_else(|| config.extra.get("agent_id"))
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("wecom gateway credential requires agent_id"))?
            .to_string();
        let token = credentials
            .webhook_secret
            .as_deref()
            .or(credentials.signing_secret.as_deref())
            .or_else(|| credentials.extra.get("callback_token").map(String::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("wecom gateway credential requires callback token"))?
            .to_string();
        let encoding_aes_key = credentials
            .extra
            .get("encoding_aes_key")
            .or_else(|| config.extra.get("encoding_aes_key"))
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("wecom gateway credential requires encoding_aes_key"))?
            .to_string();
        if encoding_aes_key.len() != 43 {
            bail!("wecom encoding_aes_key must be 43 characters");
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build WeCom HTTP client")?;
        Ok(Self {
            channel: channel.to_string(),
            mode: WeComMode::Callback,
            corp_id,
            corp_secret,
            agent_id,
            token,
            encoding_aes_key,
            allowed_users: config
                .allowed_users
                .iter()
                .chain(split_extra_list(config.extra.get("allowed_users")).iter())
                .map(|value| normalize_wecom_allow_entry(value))
                .collect(),
            allowed_chats: config
                .allowed_chats
                .iter()
                .chain(split_extra_list(config.extra.get("allowed_chats")).iter())
                .map(|value| normalize_wecom_allow_entry(value))
                .collect(),
            dm_policy: "open".to_string(),
            group_policy: "open".to_string(),
            api_base: config
                .api_base
                .clone()
                .unwrap_or_else(|| WECOM_API_BASE.to_string()),
            client,
            access_token: Arc::new(Mutex::new(None)),
            seen_messages: Arc::new(Mutex::new(HashMap::new())),
            bot_id: String::new(),
            bot_secret: String::new(),
            bot_websocket_url: String::new(),
            bot_device_id: String::new(),
            bot_socket: Arc::new(Mutex::new(None)),
            bot_pending_responses: Arc::new(Mutex::new(HashMap::new())),
            bot_reply_req_ids: Arc::new(Mutex::new(HashMap::new())),
            bot_last_chat_req_ids: Arc::new(Mutex::new(HashMap::new())),
            max_download_bytes: config.media.max_download_bytes,
        })
    }

    fn start_bot(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        let adapter = self.clone();
        thread::Builder::new()
            .name("duckagent-wecom-aibot".to_string())
            .spawn(move || adapter.bot_run_loop(inbound))
            .context("failed to spawn WeCom AI Bot websocket thread")?;
        Ok(())
    }

    fn bot_run_loop(&self, inbound: GatewayInboundDispatch) {
        let backoff = [
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(5),
            Duration::from_secs(10),
            Duration::from_secs(30),
        ];
        let mut backoff_idx = 0usize;
        loop {
            match self.open_bot_socket() {
                Ok(socket) => {
                    if let Ok(mut guard) = self.bot_socket.lock() {
                        *guard = Some(socket);
                    }
                    backoff_idx = 0;
                    if let Err(error) = self.bot_read_loop(&inbound) {
                        eprintln!("WeCom AI Bot websocket disconnected: {error:#}");
                    }
                    if let Ok(mut guard) = self.bot_socket.lock() {
                        *guard = None;
                    }
                }
                Err(error) => {
                    eprintln!("WeCom AI Bot websocket connect failed: {error:#}");
                }
            }
            let delay = backoff[backoff_idx.min(backoff.len() - 1)];
            backoff_idx = (backoff_idx + 1).min(backoff.len() - 1);
            thread::sleep(delay);
        }
    }

    fn open_bot_socket(&self) -> Result<ChannelWebSocket> {
        let (mut socket, _) = connect(self.bot_websocket_url.as_str()).with_context(|| {
            format!(
                "failed to connect WeCom AI Bot websocket {}",
                self.bot_websocket_url
            )
        })?;
        set_read_timeout(
            &mut socket,
            Duration::from_secs(WECOM_BOT_READ_TIMEOUT_SECONDS),
        );
        let req_id = self.new_bot_req_id("subscribe");
        send_ws_json_message(
            &mut socket,
            &json!({
                "cmd": WECOM_BOT_SUBSCRIBE_CMD,
                "headers": {"req_id": req_id},
                "body": {
                    "bot_id": self.bot_id,
                    "secret": self.bot_secret,
                    "device_id": self.bot_device_id,
                },
            }),
            "WeCom AI Bot subscribe send failed",
        )?;
        let deadline = Instant::now() + Duration::from_secs(WECOM_BOT_REQUEST_TIMEOUT_SECONDS);
        loop {
            if Instant::now() >= deadline {
                bail!("timed out waiting for WeCom AI Bot subscribe acknowledgement");
            }
            match read_ws_json_message(
                &mut socket,
                "WeCom AI Bot websocket read failed",
                "WeCom AI Bot websocket returned invalid JSON",
            ) {
                Ok(Some(payload)) => {
                    if payload_req_id(&payload).as_deref() != Some(req_id.as_str()) {
                        continue;
                    }
                    if let Some(error) = wecom_response_error(&payload) {
                        bail!("WeCom AI Bot subscribe failed: {error}");
                    }
                    return Ok(socket);
                }
                Ok(None) => {}
                Err(error) if is_transient_read_error(&error) => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn bot_read_loop(&self, inbound: &GatewayInboundDispatch) -> Result<()> {
        loop {
            let payload = {
                let mut guard = self
                    .bot_socket
                    .lock()
                    .expect("wecom bot socket mutex poisoned");
                let socket = guard
                    .as_mut()
                    .ok_or_else(|| anyhow!("WeCom AI Bot websocket is not connected"))?;
                match read_ws_json_message(
                    socket,
                    "WeCom AI Bot websocket read failed",
                    "WeCom AI Bot websocket returned invalid JSON",
                ) {
                    Ok(value) => value,
                    Err(error) if is_transient_read_error(&error) => None,
                    Err(error) => return Err(error),
                }
            };
            let Some(payload) = payload else {
                continue;
            };
            self.handle_bot_payload(payload, inbound)?;
        }
    }

    fn handle_bot_payload(&self, payload: Value, inbound: &GatewayInboundDispatch) -> Result<()> {
        let cmd = value_string(payload.get("cmd")).unwrap_or_default();
        let is_unsolicited = matches!(
            cmd.as_str(),
            WECOM_BOT_MSG_CALLBACK_CMD
                | "aibot_message_callback"
                | "aibot_callback"
                | WECOM_BOT_EVENT_CALLBACK_CMD
                | "event_callback"
                | "aibot_ping"
                | "ping"
        );
        if !is_unsolicited && let Some(req_id) = payload_req_id(&payload) {
            if let Some(sender) = self
                .bot_pending_responses
                .lock()
                .expect("wecom bot pending responses mutex poisoned")
                .remove(&req_id)
            {
                let _ = sender.send(payload.clone());
                return Ok(());
            }
        }
        match cmd.as_str() {
            WECOM_BOT_MSG_CALLBACK_CMD | "aibot_message_callback" | "aibot_callback" => {
                self.handle_bot_message(&payload, inbound)
            }
            WECOM_BOT_EVENT_CALLBACK_CMD | "event_callback" | "aibot_ping" | "ping" => Ok(()),
            _ => Ok(()),
        }
    }

    fn handle_bot_message(&self, payload: &Value, inbound: &GatewayInboundDispatch) -> Result<()> {
        let body = payload
            .get("body")
            .filter(|value| value.is_object())
            .ok_or_else(|| anyhow!("WeCom AI Bot callback missing body"))?;
        let req_id = payload_req_id(payload).unwrap_or_default();
        let sender = body.get("from").filter(|value| value.is_object());
        let sender_id = sender
            .and_then(|value| value_string(value.get("userid")))
            .or_else(|| value_string(body.get("from_userid")))
            .or_else(|| value_string(body.get("userid")))
            .unwrap_or_default();
        let chat_id = value_string(body.get("chatid"))
            .or_else(|| value_string(body.get("chat_id")))
            .or_else(|| (!sender_id.is_empty()).then(|| sender_id.clone()))
            .unwrap_or_default();
        if chat_id.is_empty() {
            return Ok(());
        }
        let msg_id = value_string(body.get("msgid"))
            .or_else(|| value_string(body.get("msg_id")))
            .unwrap_or_else(|| req_id.clone());
        if self.is_duplicate(&msg_id) {
            return Ok(());
        }
        let is_group = value_string(body.get("chattype"))
            .or_else(|| value_string(body.get("chat_type")))
            .map(|value| value.eq_ignore_ascii_case("group"))
            .unwrap_or(false);
        if !self.is_bot_message_allowed(&chat_id, &sender_id, is_group) {
            return Ok(());
        }
        self.remember_bot_reply_req_id(&msg_id, &req_id);
        self.remember_bot_last_chat_req_id(&chat_id, &req_id);
        let mut text = extract_bot_text(body);
        if is_group {
            text = strip_leading_mention(&text);
        }
        let mut attachments = Vec::new();
        let mut notes = Vec::new();
        for media in bot_media_candidates(body) {
            match self.download_bot_media(&media) {
                Ok(attachment) => attachments.push(attachment),
                Err(error) => notes.push(format!(
                    "[WeCom media skipped: {}, reason={error:#}]",
                    media.url
                )),
            }
        }
        if !notes.is_empty() {
            if !text.trim().is_empty() {
                text.push('\n');
            }
            text.push_str(&notes.join("\n"));
        }
        if text.trim().is_empty() && attachments.is_empty() {
            return Ok(());
        }
        inbound.submit(InboundMessageInput {
            channel: self.channel.clone(),
            conversation_id: chat_id,
            thread_id: None,
            chat_type: Some(if is_group { "group" } else { "dm" }.to_string()),
            sender_id: (!sender_id.is_empty()).then_some(sender_id),
            message_id: (!msg_id.is_empty()).then_some(msg_id),
            text,
            attachments,
            timestamp: None,
        })?;
        Ok(())
    }

    fn is_bot_message_allowed(&self, chat_id: &str, sender_id: &str, is_group: bool) -> bool {
        let chat = normalize_wecom_allow_entry(chat_id);
        let sender = normalize_wecom_allow_entry(sender_id);
        if is_group {
            match self.group_policy.as_str() {
                "disabled" => return false,
                "allowlist"
                    if self.allowed_chats.is_empty()
                        || (!self.allowed_chats.contains("*")
                            && !self.allowed_chats.contains(&chat)) =>
                {
                    return false;
                }
                _ => {}
            }
            if !self.allowed_users.is_empty()
                && !self.allowed_users.contains("*")
                && !self.allowed_users.contains(&sender)
            {
                return false;
            }
        } else {
            match self.dm_policy.as_str() {
                "disabled" => return false,
                "allowlist"
                    if self.allowed_users.is_empty()
                        || (!self.allowed_users.contains("*")
                            && !self.allowed_users.contains(&sender)
                            && !self.allowed_users.contains(&chat)) =>
                {
                    return false;
                }
                _ => {}
            }
        }
        true
    }

    fn remember_bot_reply_req_id(&self, message_id: &str, req_id: &str) {
        if message_id.trim().is_empty() || req_id.trim().is_empty() {
            return;
        }
        let mut guard = self
            .bot_reply_req_ids
            .lock()
            .expect("wecom bot reply req ids mutex poisoned");
        guard.insert(message_id.to_string(), req_id.to_string());
        while guard.len() > WECOM_BOT_CACHE_LIMIT {
            if let Some(key) = guard.keys().next().cloned() {
                guard.remove(&key);
            } else {
                break;
            }
        }
    }

    fn remember_bot_last_chat_req_id(&self, chat_id: &str, req_id: &str) {
        if chat_id.trim().is_empty() || req_id.trim().is_empty() {
            return;
        }
        let mut guard = self
            .bot_last_chat_req_ids
            .lock()
            .expect("wecom bot last chat req ids mutex poisoned");
        guard.insert(chat_id.to_string(), req_id.to_string());
        while guard.len() > WECOM_BOT_CACHE_LIMIT {
            if let Some(key) = guard.keys().next().cloned() {
                guard.remove(&key);
            } else {
                break;
            }
        }
    }

    fn bot_reply_req_id_for_route(&self, route: &GatewayRoute) -> Option<String> {
        if let Some(thread_id) = route.key.thread_id.as_deref() {
            if let Some(req_id) = self
                .bot_reply_req_ids
                .lock()
                .expect("wecom bot reply req ids mutex poisoned")
                .get(thread_id)
                .cloned()
            {
                return Some(req_id);
            }
        }
        self.bot_last_chat_req_ids
            .lock()
            .expect("wecom bot last chat req ids mutex poisoned")
            .get(&route.key.conversation_id)
            .cloned()
    }

    fn send_bot_text(&self, route: &GatewayRoute, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        if let Some(reply_req_id) = self.bot_reply_req_id_for_route(route) {
            return self.send_bot_request(
                WECOM_BOT_RESPONSE_CMD,
                &reply_req_id,
                json!({
                    "msgtype": "markdown",
                    "markdown": {"content": text.chars().take(WECOM_BOT_TEXT_LIMIT).collect::<String>()},
                }),
            );
        }
        self.send_bot_request(
            WECOM_BOT_SEND_CMD,
            &self.new_bot_req_id("send"),
            json!({
                "chatid": route.key.conversation_id,
                "msgtype": "markdown",
                "markdown": {"content": text.chars().take(WECOM_BOT_TEXT_LIMIT).collect::<String>()},
            }),
        )
    }

    fn send_bot_media_fallback(
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
            return self.send_bot_text(route, &text);
        }
        let media = self.upload_bot_media(Path::new(path))?;
        if let Some(caption) = caption.filter(|value| !value.trim().is_empty()) {
            self.send_bot_text(route, caption)?;
        }
        let media_type = media.media_type;
        let media_id = media.media_id;
        let body = json!({
            "chatid": route.key.conversation_id,
            "msgtype": media_type.clone(),
            media_type.clone(): {"media_id": media_id.clone()},
        });
        if let Some(reply_req_id) = self.bot_reply_req_id_for_route(route) {
            self.send_bot_request(
                WECOM_BOT_RESPONSE_CMD,
                &reply_req_id,
                json!({
                    "msgtype": media_type.clone(),
                    media_type: {"media_id": media_id},
                }),
            )
        } else {
            self.send_bot_request(WECOM_BOT_SEND_CMD, &self.new_bot_req_id("media"), body)
        }
    }

    fn send_bot_request(&self, cmd: &str, req_id: &str, body: Value) -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        self.bot_pending_responses
            .lock()
            .expect("wecom bot pending responses mutex poisoned")
            .insert(req_id.to_string(), sender);
        let payload = json!({
            "cmd": cmd,
            "headers": {"req_id": req_id},
            "body": body,
        });
        let send_result = {
            let mut guard = self
                .bot_socket
                .lock()
                .expect("wecom bot socket mutex poisoned");
            if guard.is_none() {
                *guard = Some(self.open_bot_socket()?);
            }
            let socket = guard
                .as_mut()
                .ok_or_else(|| anyhow!("WeCom AI Bot websocket is not connected"))?;
            send_ws_json_message(socket, &payload, "WeCom AI Bot websocket send failed")
        };
        if let Err(error) = send_result {
            self.bot_pending_responses
                .lock()
                .expect("wecom bot pending responses mutex poisoned")
                .remove(req_id);
            return Err(error);
        }
        match receiver.recv_timeout(Duration::from_secs(WECOM_BOT_REQUEST_TIMEOUT_SECONDS)) {
            Ok(response) => {
                if let Some(error) = wecom_response_error(&response) {
                    bail!("WeCom AI Bot request failed: {error}");
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("WeCom AI Bot response channel disconnected");
            }
        }
        self.bot_pending_responses
            .lock()
            .expect("wecom bot pending responses mutex poisoned")
            .remove(req_id);
        Ok(())
    }

    fn upload_bot_media(&self, path: &Path) -> Result<UploadedBotMedia> {
        if !path.exists() {
            bail!("WeCom outbound media file not found: {}", path.display());
        }
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read WeCom media {}", path.display()))?;
        if bytes.len() > WECOM_BOT_MAX_MEDIA_BYTES {
            bail!(
                "WeCom outbound media is {} bytes, over {} bytes",
                bytes.len(),
                WECOM_BOT_MAX_MEDIA_BYTES
            );
        }
        let media_type = infer_wecom_media_type(path);
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("duckagent-media")
            .to_string();
        let init_req_id = self.new_bot_req_id("upload_init");
        let (sender, receiver) = mpsc::channel();
        self.bot_pending_responses
            .lock()
            .expect("wecom bot pending responses mutex poisoned")
            .insert(init_req_id.clone(), sender);
        self.send_bot_frame(json!({
            "cmd": WECOM_BOT_UPLOAD_INIT_CMD,
            "headers": {"req_id": init_req_id},
            "body": {
                "type": media_type,
                "filename": filename,
                "total_size": bytes.len(),
                "total_chunks": bytes.len().div_ceil(WECOM_BOT_UPLOAD_CHUNK_BYTES),
            },
        }))?;
        let init_response = self.wait_bot_response(&init_req_id, receiver)?;
        let upload_id = init_response
            .get("body")
            .and_then(|body| value_string(body.get("upload_id")))
            .or_else(|| value_string(init_response.get("upload_id")))
            .ok_or_else(|| anyhow!("WeCom AI Bot media upload init missing upload_id"))?;
        for (index, chunk) in bytes.chunks(WECOM_BOT_UPLOAD_CHUNK_BYTES).enumerate() {
            let chunk_req_id = self.new_bot_req_id("upload_chunk");
            let (sender, receiver) = mpsc::channel();
            self.bot_pending_responses
                .lock()
                .expect("wecom bot pending responses mutex poisoned")
                .insert(chunk_req_id.clone(), sender);
            self.send_bot_frame(json!({
                "cmd": WECOM_BOT_UPLOAD_CHUNK_CMD,
                "headers": {"req_id": chunk_req_id},
                "body": {
                    "upload_id": upload_id,
                    "chunk_index": index,
                    "base64_data": base64::engine::general_purpose::STANDARD.encode(chunk),
                },
            }))?;
            let _ = self.wait_bot_response(&chunk_req_id, receiver)?;
        }
        let finish_req_id = self.new_bot_req_id("upload_finish");
        let (sender, receiver) = mpsc::channel();
        self.bot_pending_responses
            .lock()
            .expect("wecom bot pending responses mutex poisoned")
            .insert(finish_req_id.clone(), sender);
        self.send_bot_frame(json!({
            "cmd": WECOM_BOT_UPLOAD_FINISH_CMD,
            "headers": {"req_id": finish_req_id},
            "body": {"upload_id": upload_id},
        }))?;
        let finish_response = self.wait_bot_response(&finish_req_id, receiver)?;
        let finish_body = finish_response.get("body").unwrap_or(&finish_response);
        let media_id = value_string(finish_body.get("media_id"))
            .ok_or_else(|| anyhow!("WeCom AI Bot media upload finish missing media_id"))?;
        let media_type =
            value_string(finish_body.get("type")).unwrap_or_else(|| media_type.to_string());
        Ok(UploadedBotMedia {
            media_type,
            media_id,
        })
    }

    fn send_bot_frame(&self, payload: Value) -> Result<()> {
        let mut guard = self
            .bot_socket
            .lock()
            .expect("wecom bot socket mutex poisoned");
        if guard.is_none() {
            *guard = Some(self.open_bot_socket()?);
        }
        let socket = guard
            .as_mut()
            .ok_or_else(|| anyhow!("WeCom AI Bot websocket is not connected"))?;
        send_ws_json_message(socket, &payload, "WeCom AI Bot websocket send failed")
    }

    fn wait_bot_response(&self, req_id: &str, receiver: mpsc::Receiver<Value>) -> Result<Value> {
        let response = receiver
            .recv_timeout(Duration::from_secs(WECOM_BOT_REQUEST_TIMEOUT_SECONDS))
            .with_context(|| format!("timed out waiting for WeCom AI Bot response {req_id}"))?;
        self.bot_pending_responses
            .lock()
            .expect("wecom bot pending responses mutex poisoned")
            .remove(req_id);
        if let Some(error) = wecom_response_error(&response) {
            bail!("WeCom AI Bot request failed: {error}");
        }
        Ok(response)
    }

    fn new_bot_req_id(&self, prefix: &str) -> String {
        let count = WECOM_BOT_REQ_COUNTER.fetch_add(1, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_millis())
            .unwrap_or_default();
        format!("duckagent-{prefix}-{now}-{count}")
    }

    fn download_bot_media(&self, media: &WeComBotMediaCandidate) -> Result<InboundAttachmentInput> {
        let response = self
            .client
            .get(&media.url)
            .send()
            .with_context(|| format!("WeCom media download failed: {}", media.url))?;
        let status = response.status();
        if !status.is_success() {
            bail!("WeCom media download failed with status {status}");
        }
        if let Some(length) = response.content_length() {
            if length > self.max_download_bytes {
                bail!(
                    "WeCom media is {length} bytes, over max_download_bytes {}",
                    self.max_download_bytes
                );
            }
        }
        let mime = media.mime.clone().or_else(|| {
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        });
        let bytes = response.bytes().context("WeCom media body is unreadable")?;
        if bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "WeCom media is {} bytes, over max_download_bytes {}",
                bytes.len(),
                self.max_download_bytes
            );
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: media
                .filename
                .clone()
                .or_else(|| filename_from_url(&media.url)),
            mime,
        })
    }

    fn handle_verify(&self, request: ChannelHttpRequest) -> Result<ChannelHttpResponse> {
        let msg_signature = query_required(&request, "msg_signature")?;
        let timestamp = query_required(&request, "timestamp")?;
        let nonce = query_required(&request, "nonce")?;
        let echostr = query_required(&request, "echostr")?;
        let plain = self.decrypt_payload(msg_signature, timestamp, nonce, echostr)?;
        Ok(text_response(
            200,
            String::from_utf8_lossy(&plain).to_string(),
        ))
    }

    fn handle_callback(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        let msg_signature = query_required(&request, "msg_signature")?;
        let timestamp = query_required(&request, "timestamp")?;
        let nonce = query_required(&request, "nonce")?;
        let body = String::from_utf8_lossy(&request.body);
        let encrypt =
            xml_tag(&body, "Encrypt").ok_or_else(|| anyhow!("WeCom callback missing Encrypt"))?;
        let plain = self.decrypt_payload(msg_signature, timestamp, nonce, &encrypt)?;
        let xml = String::from_utf8(plain).context("WeCom decrypted payload is not UTF-8")?;
        let Some(message) = parse_plain_wecom_message(&xml) else {
            return Ok(text_response(200, "success".to_string()));
        };
        if !self.is_message_allowed(&message) {
            return Ok(text_response(200, "success".to_string()));
        }
        if self.is_duplicate(&message.msg_id) {
            return Ok(text_response(200, "success".to_string()));
        }
        inbound.submit(InboundMessageInput {
            channel: self.channel.clone(),
            conversation_id: scoped_user_key(&message.corp_id, &message.user_id),
            thread_id: None,
            chat_type: Some("dm".to_string()),
            sender_id: Some(message.user_id),
            message_id: Some(message.msg_id),
            text: message.content,
            attachments: Vec::new(),
            timestamp: message.create_time,
        })?;
        Ok(text_response(200, "success".to_string()))
    }

    fn decrypt_payload(
        &self,
        msg_signature: &str,
        timestamp: &str,
        nonce: &str,
        encrypt: &str,
    ) -> Result<Vec<u8>> {
        let expected = wecom_signature(&self.token, timestamp, nonce, encrypt);
        if expected != msg_signature {
            bail!("WeCom callback signature mismatch");
        }
        let key = base64::engine::general_purpose::STANDARD
            .decode(format!("{}=", self.encoding_aes_key))
            .context("WeCom encoding_aes_key is invalid base64")?;
        let iv = &key[..16];
        let mut ciphertext = base64::engine::general_purpose::STANDARD
            .decode(encrypt)
            .context("WeCom Encrypt payload is invalid base64")?;
        let decrypted = Aes256CbcDec::new_from_slices(&key, iv)
            .context("failed to initialize WeCom AES-CBC decryptor")?
            .decrypt_padded_mut::<NoPadding>(&mut ciphertext)
            .map_err(|_| anyhow!("WeCom AES-CBC decrypt failed"))?;
        let plain = pkcs7_unpad_32(decrypted)?;
        if plain.len() < 20 {
            bail!("WeCom decrypted payload too short");
        }
        let content = &plain[16..];
        let len = u32::from_be_bytes([content[0], content[1], content[2], content[3]]) as usize;
        if content.len() < 4 + len {
            bail!("WeCom decrypted XML length is invalid");
        }
        let xml = content[4..4 + len].to_vec();
        let receive_id = String::from_utf8_lossy(&content[4 + len..]).to_string();
        if receive_id != self.corp_id {
            bail!("WeCom receive_id mismatch");
        }
        Ok(xml)
    }

    fn is_duplicate(&self, message_id: &str) -> bool {
        if message_id.is_empty() {
            return false;
        }
        let mut guard = self
            .seen_messages
            .lock()
            .expect("wecom seen messages mutex poisoned");
        let now = Instant::now();
        guard.retain(|_, seen_at| now.duration_since(*seen_at) < Duration::from_secs(300));
        guard.insert(message_id.to_string(), now).is_some()
    }

    fn is_message_allowed(&self, message: &WeComPlainMessage) -> bool {
        let scoped = scoped_user_key(&message.corp_id, &message.user_id).to_ascii_lowercase();
        let user = message.user_id.to_ascii_lowercase();
        if !self.allowed_users.is_empty()
            && !self.allowed_users.contains("*")
            && !self.allowed_users.contains(&user)
            && !self.allowed_users.contains(&scoped)
        {
            return false;
        }
        if !self.allowed_chats.is_empty()
            && !self.allowed_chats.contains("*")
            && !self.allowed_chats.contains(&scoped)
            && !self.allowed_chats.contains(&user)
        {
            return false;
        }
        true
    }

    fn access_token(&self) -> Result<String> {
        {
            let guard = self
                .access_token
                .lock()
                .expect("wecom access token mutex poisoned");
            if let Some(token) = guard.as_ref() {
                if token.expires_at > Instant::now() + Duration::from_secs(60) {
                    return Ok(token.token.clone());
                }
            }
        }
        let response = self
            .client
            .get(format!(
                "{}/cgi-bin/gettoken",
                self.api_base.trim_end_matches('/')
            ))
            .query(&[
                ("corpid", self.corp_id.as_str()),
                ("corpsecret", self.corp_secret.as_str()),
            ])
            .send()
            .context("WeCom gettoken request failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .context("WeCom gettoken returned invalid JSON")?;
        if !status.is_success() || value["errcode"].as_i64().unwrap_or(-1) != 0 {
            bail!("WeCom gettoken failed with status {status}: {value}");
        }
        let token = value["access_token"]
            .as_str()
            .ok_or_else(|| anyhow!("WeCom gettoken missing access_token"))?
            .to_string();
        let expires = value["expires_in"]
            .as_u64()
            .unwrap_or(ACCESS_TOKEN_TTL_SECONDS);
        *self
            .access_token
            .lock()
            .expect("wecom access token mutex poisoned") = Some(CachedWeComToken {
            token: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires),
        });
        Ok(token)
    }

    fn send_text(&self, route: &GatewayRoute, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let token = self.access_token()?;
        let touser = self.touser(route);
        let response = self
            .client
            .post(format!(
                "{}/cgi-bin/message/send",
                self.api_base.trim_end_matches('/')
            ))
            .query(&[("access_token", token.as_str())])
            .json(&json!({
                "touser": touser,
                "msgtype": "text",
                "agentid": self.agent_id.parse::<i64>().unwrap_or_default(),
                "text": {"content": text.chars().take(2048).collect::<String>()},
                "safe": 0,
            }))
            .send()
            .context("WeCom message/send request failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .unwrap_or_else(|_| json!({"errmsg": "non-json response"}));
        if !status.is_success() || value["errcode"].as_i64().unwrap_or(-1) != 0 {
            bail!("WeCom message/send failed with status {status}: {value}");
        }
        Ok(())
    }

    fn touser<'a>(&self, route: &'a GatewayRoute) -> &'a str {
        route
            .key
            .conversation_id
            .split_once(':')
            .map(|(_, user)| user)
            .unwrap_or(route.key.conversation_id.as_str())
    }

    fn upload_media(&self, path: &Path) -> Result<(&'static str, String)> {
        if !path.exists() {
            bail!("WeCom outbound media file not found: {}", path.display());
        }
        let media_type = infer_wecom_media_type(path);
        let token = self.access_token()?;
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read WeCom media {}", path.display()))?;
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("duckagent-media")
            .to_string();
        let part = multipart::Part::bytes(bytes.clone())
            .file_name(filename)
            .mime_str(infer_wecom_mime(path, media_type))
            .unwrap_or_else(|_| multipart::Part::bytes(bytes).file_name("duckagent-media"));
        let response = self
            .client
            .post(format!(
                "{}/cgi-bin/media/upload",
                self.api_base.trim_end_matches('/')
            ))
            .query(&[("access_token", token.as_str()), ("type", media_type)])
            .multipart(multipart::Form::new().part("media", part))
            .send()
            .context("WeCom media/upload request failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .unwrap_or_else(|_| json!({"errmsg": "non-json response"}));
        if !status.is_success() || value["errcode"].as_i64().unwrap_or(-1) != 0 {
            bail!("WeCom media/upload failed with status {status}: {value}");
        }
        let media_id = value["media_id"]
            .as_str()
            .ok_or_else(|| anyhow!("WeCom media/upload missing media_id"))?
            .to_string();
        Ok((media_type, media_id))
    }

    fn send_uploaded_media(
        &self,
        route: &GatewayRoute,
        media_type: &str,
        media_id: &str,
    ) -> Result<()> {
        let token = self.access_token()?;
        let touser = self.touser(route);
        let media_body = if media_type == "video" {
            json!({
                "media_id": media_id,
                "title": "DuckAgent media",
                "description": "Sent by DuckAgent",
            })
        } else {
            json!({"media_id": media_id})
        };
        let mut body = json!({
            "touser": touser,
            "msgtype": media_type,
            "agentid": self.agent_id.parse::<i64>().unwrap_or_default(),
            "safe": 0,
        });
        body[media_type] = media_body;
        let response = self
            .client
            .post(format!(
                "{}/cgi-bin/message/send",
                self.api_base.trim_end_matches('/')
            ))
            .query(&[("access_token", token.as_str())])
            .json(&body)
            .send()
            .context("WeCom media message/send request failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .unwrap_or_else(|_| json!({"errmsg": "non-json response"}));
        if !status.is_success() || value["errcode"].as_i64().unwrap_or(-1) != 0 {
            bail!("WeCom media message/send failed with status {status}: {value}");
        }
        Ok(())
    }

    fn send_media_fallback(
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
            return self.send_text(route, &text);
        }
        if let Some(caption) = caption.filter(|value| !value.trim().is_empty()) {
            self.send_text(route, caption)?;
        }
        let (media_type, media_id) = self.upload_media(Path::new(path))?;
        self.send_uploaded_media(route, media_type, &media_id)
    }
}

impl ChannelAdapter for WeComAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        if self.mode == WeComMode::AiBot {
            return self.start_bot(inbound);
        }
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        if self.mode == WeComMode::AiBot {
            return Ok(None);
        }
        let is_path = matches!(
            request.path.as_str(),
            WECOM_CALLBACK_PATH | WECOM_EVENTS_PATH | WECOM_CALLBACK_ALIAS_PATH
        );
        if !is_path {
            return Ok(None);
        }
        match request.method.as_str() {
            "GET" => self.handle_verify(request).map(Some),
            "POST" => self.handle_callback(request, inbound).map(Some),
            _ => Ok(Some(text_response(404, "not found".to_string()))),
        }
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        if self.mode == WeComMode::AiBot {
            let mut text_sent = false;
            for chunk in text_chunks(&message.text, WECOM_BOT_TEXT_LIMIT) {
                self.send_bot_text(route, &chunk)?;
                text_sent = true;
            }
            let mut caption = (!text_sent)
                .then_some(message.text.trim())
                .filter(|value| !value.is_empty());
            for path in message.media_paths {
                self.send_bot_media_fallback(route, &path, caption)?;
                caption = None;
            }
            return Ok(());
        }
        let mut text_sent = false;
        for chunk in text_chunks(&message.text, 2048) {
            self.send_text(route, &chunk)?;
            text_sent = true;
        }
        let mut caption = (!text_sent)
            .then_some(message.text.trim())
            .filter(|value| !value.is_empty());
        for path in message.media_paths {
            self.send_media_fallback(route, &path, caption)?;
            caption = None;
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
        if self.mode == WeComMode::AiBot {
            return self.send_bot_text(
                route,
                &format!(
                    "{}\n\nCommands:\n/approve {} once\n/approve {} session\n/approve {} always\n/deny {}",
                    prompt.message, prompt.id, prompt.id, prompt.id, prompt.id
                ),
            );
        }
        self.send_text(
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
            typing: false,
            approval_prompt: true,
        }
    }
}

#[derive(Clone, Debug)]
struct WeComBotMediaCandidate {
    url: String,
    filename: Option<String>,
    mime: Option<String>,
}

#[derive(Clone, Debug)]
struct UploadedBotMedia {
    media_type: String,
    media_id: String,
}

fn parse_wecom_access_policy(raw: Option<&str>, default: &str) -> Result<String> {
    let policy = raw.unwrap_or(default).trim().to_ascii_lowercase();
    match policy.as_str() {
        "" => Ok(default.to_string()),
        "open" | "allowlist" | "allow-list" | "disabled" | "off" => {
            if policy == "allow-list" {
                Ok("allowlist".to_string())
            } else if policy == "off" {
                Ok("disabled".to_string())
            } else {
                Ok(policy)
            }
        }
        other => bail!("invalid WeCom access policy `{other}`"),
    }
}

fn parse_plain_wecom_message(xml: &str) -> Option<WeComPlainMessage> {
    let msg_type = xml_tag(xml, "MsgType")?.to_ascii_lowercase();
    if msg_type == "event" {
        let event = xml_tag(xml, "Event")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(event.as_str(), "enter_agent" | "subscribe") {
            return None;
        }
    } else if msg_type != "text" {
        return None;
    }
    let corp_id = xml_tag(xml, "ToUserName").unwrap_or_default();
    let user_id = xml_tag(xml, "FromUserName").unwrap_or_default();
    let content = xml_tag(xml, "Content").unwrap_or_else(|| {
        if msg_type == "event" {
            "/start".to_string()
        } else {
            String::new()
        }
    });
    let msg_id = xml_tag(xml, "MsgId").unwrap_or_else(|| {
        format!(
            "{}:{}",
            user_id,
            xml_tag(xml, "CreateTime").unwrap_or_default()
        )
    });
    Some(WeComPlainMessage {
        corp_id,
        user_id,
        content,
        msg_id,
        create_time: xml_tag(xml, "CreateTime"),
    })
}

fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(unescape_xml(xml[start..end].trim()))
}

fn unescape_xml(value: &str) -> String {
    value
        .replace("<![CDATA[", "")
        .replace("]]>", "")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn query_required<'a>(request: &'a ChannelHttpRequest, key: &str) -> Result<&'a str> {
    request
        .query
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("WeCom callback missing query `{key}`"))
}

fn scoped_user_key(corp_id: &str, user_id: &str) -> String {
    if corp_id.is_empty() {
        user_id.to_string()
    } else {
        format!("{corp_id}:{user_id}")
    }
}

fn split_extra_list(value: Option<&String>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn normalize_wecom_allow_entry(value: &str) -> String {
    let mut value = value.trim().to_ascii_lowercase();
    for prefix in ["wecom:", "user:", "group:"] {
        if let Some(stripped) = value.strip_prefix(prefix) {
            value = stripped.to_string();
        }
    }
    value
}

fn new_bot_device_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("duckagent-{}-{now:x}", std::process::id())
}

fn payload_req_id(payload: &Value) -> Option<String> {
    payload
        .get("headers")
        .and_then(|headers| value_string(headers.get("req_id")))
        .or_else(|| value_string(payload.get("req_id")))
}

fn value_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn wecom_response_error(payload: &Value) -> Option<String> {
    let errcode = payload
        .get("errcode")
        .and_then(Value::as_i64)
        .or_else(|| {
            payload
                .get("body")
                .and_then(|body| body.get("errcode"))
                .and_then(Value::as_i64)
        })
        .unwrap_or(0);
    if errcode == 0 {
        return None;
    }
    let errmsg = value_string(payload.get("errmsg"))
        .or_else(|| {
            payload
                .get("body")
                .and_then(|body| value_string(body.get("errmsg")))
        })
        .unwrap_or_else(|| "unknown error".to_string());
    Some(format!("errcode {errcode}: {errmsg}"))
}

fn extract_bot_text(body: &Value) -> String {
    let mut parts = Vec::new();
    let msgtype = value_string(body.get("msgtype"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    if msgtype == "mixed" {
        if let Some(items) = body
            .get("mixed")
            .and_then(|value| value.get("msg_item"))
            .and_then(Value::as_array)
        {
            for item in items {
                push_wecom_text_part(&mut parts, item);
            }
        }
    } else {
        push_wecom_text_part(&mut parts, body);
    }
    if let Some(quote) = body.get("quote").filter(|value| value.is_object()) {
        let quoted = extract_bot_text(quote);
        if !quoted.trim().is_empty() {
            if parts.is_empty() {
                parts.push(quoted);
            } else {
                parts.push(format!("[Quoted]\n{quoted}"));
            }
        }
    }
    parts.join("\n").trim().to_string()
}

fn strip_leading_mention(text: &str) -> String {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('@') {
        return text.trim().to_string();
    }
    trimmed
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim().to_string())
        .unwrap_or_default()
}

fn bot_media_candidates(body: &Value) -> Vec<WeComBotMediaCandidate> {
    let mut out = Vec::new();
    for key in [
        "image",
        "voice",
        "video",
        "file",
        "attachment",
        "appmsg",
        "media",
    ] {
        push_bot_media_candidate(&mut out, body.get(key));
    }
    if let Some(items) = body
        .get("mixed")
        .and_then(|value| value.get("msg_item"))
        .and_then(Value::as_array)
    {
        for item in items {
            for key in [
                "image",
                "voice",
                "video",
                "file",
                "attachment",
                "appmsg",
                "media",
            ] {
                push_bot_media_candidate(&mut out, item.get(key));
            }
        }
    }
    if let Some(quote) = body.get("quote").filter(|value| value.is_object()) {
        for key in [
            "image",
            "voice",
            "video",
            "file",
            "attachment",
            "appmsg",
            "media",
        ] {
            push_bot_media_candidate(&mut out, quote.get(key));
        }
    }
    if let Some(items) = body.get("attachments").and_then(Value::as_array) {
        for item in items {
            push_bot_media_candidate(&mut out, Some(item));
        }
    }
    out
}

fn push_bot_media_candidate(out: &mut Vec<WeComBotMediaCandidate>, value: Option<&Value>) {
    let Some(value) = value else {
        return;
    };
    for nested in ["file", "image", "voice", "video", "media"] {
        push_bot_media_candidate(out, value.get(nested));
    }
    let url = value_string(value.get("url"))
        .or_else(|| value_string(value.get("download_url")))
        .or_else(|| value_string(value.get("downloadurl")))
        .or_else(|| value_string(value.get("file_url")))
        .or_else(|| value_string(value.get("fileurl")))
        .or_else(|| value_string(value.get("media_url")))
        .or_else(|| value_string(value.get("cdn_url")))
        .or_else(|| value_string(value.get("preview_url")));
    let Some(url) = url.filter(|url| url.starts_with("http://") || url.starts_with("https://"))
    else {
        return;
    };
    out.push(WeComBotMediaCandidate {
        url,
        filename: value_string(value.get("filename"))
            .or_else(|| value_string(value.get("file_name")))
            .or_else(|| value_string(value.get("name")))
            .or_else(|| value_string(value.get("title"))),
        mime: value_string(value.get("content_type"))
            .or_else(|| value_string(value.get("mimetype")))
            .or_else(|| value_string(value.get("mime"))),
    });
}

fn push_wecom_text_part(parts: &mut Vec<String>, value: &Value) {
    for candidate in [
        value_string(value.get("text").and_then(|value| value.get("content"))),
        value_string(value.get("markdown").and_then(|value| value.get("content"))),
        value_string(value.get("voice").and_then(|value| value.get("content"))),
        value_string(value.get("appmsg").and_then(|value| value.get("title"))),
        value_string(value.get("content")),
        value_string(value.get("msgcontent")),
    ]
    .into_iter()
    .flatten()
    {
        if !candidate.trim().is_empty() {
            parts.push(candidate);
        }
    }
}

fn filename_from_url(url: &str) -> Option<String> {
    let path = url.split('?').next().unwrap_or(url);
    path.rsplit('/')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn infer_wecom_media_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" | "png" => "image",
        "amr" | "mp3" | "wav" | "m4a" | "ogg" => "voice",
        "mp4" | "mov" | "m4v" => "video",
        _ => "file",
    }
}

fn infer_wecom_mime(path: &Path, media_type: &str) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "amr" => "audio/amr",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        _ if media_type == "image" => "image/jpeg",
        _ if media_type == "voice" => "audio/mpeg",
        _ if media_type == "video" => "video/mp4",
        _ => "application/octet-stream",
    }
}

fn wecom_signature(token: &str, timestamp: &str, nonce: &str, encrypt: &str) -> String {
    let mut parts = [
        token.to_string(),
        timestamp.to_string(),
        nonce.to_string(),
        encrypt.to_string(),
    ];
    parts.sort();
    let mut sha1 = Sha1::new();
    sha1.update(parts.join("").as_bytes());
    format!("{:x}", sha1.finalize())
}

fn pkcs7_unpad_32(value: &[u8]) -> Result<&[u8]> {
    let Some(&pad) = value.last() else {
        bail!("WeCom decrypted payload is empty");
    };
    if pad == 0 || pad > 32 || pad as usize > value.len() {
        bail!("WeCom decrypted payload has invalid padding");
    }
    if !value[value.len() - pad as usize..]
        .iter()
        .all(|byte| *byte == pad)
    {
        bail!("WeCom decrypted payload padding mismatch");
    }
    Ok(&value[..value.len() - pad as usize])
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

fn text_response(status: u16, text: String) -> ChannelHttpResponse {
    ChannelHttpResponse {
        status,
        content_type: "text/plain; charset=utf-8",
        body: text.into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cbc::cipher::BlockEncryptMut;
    type Aes256CbcEnc = cbc::Encryptor<Aes256>;

    fn test_adapter() -> WeComAdapter {
        let mut credentials = GatewayCredentialEntry {
            channel: "wecom_callback".to_string(),
            app_id: Some("ww1234567890".to_string()),
            app_secret: Some("corp-secret".to_string()),
            webhook_secret: Some("callback-token".to_string()),
            ..Default::default()
        };
        let aes_key = base64::engine::general_purpose::STANDARD
            .encode([7u8; 32])
            .trim_end_matches('=')
            .to_string();
        credentials
            .extra
            .insert("encoding_aes_key".to_string(), aes_key);
        credentials
            .extra
            .insert("agent_id".to_string(), "1000002".to_string());
        WeComAdapter::new(
            "wecom_callback",
            &GatewayChannelConfig::default(),
            &credentials,
        )
        .expect("adapter")
    }

    #[test]
    fn wecom_plain_message_extracts_text() {
        let message = parse_plain_wecom_message(
            "<xml><ToUserName>ww</ToUserName><FromUserName>alice</FromUserName><CreateTime>1</CreateTime><MsgType>text</MsgType><Content>hello</Content><MsgId>m1</MsgId></xml>",
        )
        .expect("message");
        assert_eq!(message.corp_id, "ww");
        assert_eq!(message.user_id, "alice");
        assert_eq!(message.content, "hello");
        assert_eq!(
            scoped_user_key(&message.corp_id, &message.user_id),
            "ww:alice"
        );
    }

    #[test]
    fn wecom_signature_is_sorted_sha1() {
        assert_eq!(
            wecom_signature("token", "1", "nonce", "encrypt"),
            wecom_signature("token", "1", "nonce", "encrypt")
        );
    }

    #[test]
    fn wecom_decrypt_roundtrip() -> Result<()> {
        let adapter = test_adapter();
        let xml = "<xml><ToUserName>ww1234567890</ToUserName><FromUserName>alice</FromUserName><MsgType>text</MsgType><Content>hi</Content><MsgId>m1</MsgId></xml>";
        let timestamp = "123456";
        let nonce = "nonce";
        let encrypt = encrypt_for_test(&adapter, xml)?;
        let signature = wecom_signature(&adapter.token, timestamp, nonce, &encrypt);
        let decrypted = adapter.decrypt_payload(&signature, timestamp, nonce, &encrypt)?;
        assert_eq!(String::from_utf8(decrypted)?, xml);
        Ok(())
    }

    fn encrypt_for_test(adapter: &WeComAdapter, xml: &str) -> Result<String> {
        let key = base64::engine::general_purpose::STANDARD
            .decode(format!("{}=", adapter.encoding_aes_key))?;
        let iv = &key[..16];
        let mut payload = vec![0u8; 16];
        payload.extend_from_slice(&(xml.len() as u32).to_be_bytes());
        payload.extend_from_slice(xml.as_bytes());
        payload.extend_from_slice(adapter.corp_id.as_bytes());
        let pad = 32 - (payload.len() % 32);
        payload.extend(std::iter::repeat_n(pad as u8, pad));
        let mut buf = payload.clone();
        let encrypted = Aes256CbcEnc::new_from_slices(&key, iv)
            .context("encryptor")?
            .encrypt_padded_mut::<NoPadding>(&mut buf, payload.len())
            .map_err(|_| anyhow!("encrypt failed"))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(encrypted))
    }
}
