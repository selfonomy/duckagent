use super::super::{
    ChannelAdapter, ChannelCapabilities, GatewayApprovalPrompt, GatewayInboundDispatch,
    GatewayRoute, InboundAttachmentInput, InboundMessageInput, OutboundMessage,
    StreamMessageHandle, TypingEvent,
};
use super::websocket::{
    ChannelWebSocket, is_transient_read_error, read_json_message as read_ws_json_message,
    send_json_message as send_ws_json_message, set_read_timeout,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::{Client, multipart};
use reqwest::header::CONTENT_LENGTH;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::connect;

const DEFAULT_DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const DEFAULT_DISCORD_GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const DISCORD_TEXT_LIMIT: usize = 2_000;
const DISCORD_GATEWAY_INTENTS: u64 = 1 | 512 | 4096 | 32768;
const DISCORD_RECONNECT_BACKOFF: &[u64] = &[2, 5, 10, 30, 60];

#[derive(Clone)]
pub(in crate::gateway) struct DiscordAdapter {
    bot_token: String,
    api_base: String,
    allowed_users: Vec<String>,
    allowed_chats: Vec<String>,
    free_response_channels: Vec<String>,
    ignored_chats: Vec<String>,
    require_mention: bool,
    allowed_mentions: DiscordAllowedMentions,
    max_download_bytes: u64,
    client: Client,
    bot_user_id: Arc<Mutex<Option<String>>>,
    processed_message_ids: Arc<Mutex<HashSet<String>>>,
    participated_threads: Arc<Mutex<HashSet<String>>>,
    known_thread_channels: Arc<Mutex<HashSet<String>>>,
    known_non_thread_channels: Arc<Mutex<HashSet<String>>>,
    allow_bot_messages: String,
}

#[derive(Clone, Debug)]
struct DiscordAllowedMentions {
    everyone: bool,
    roles: bool,
    users: bool,
    replied_user: bool,
}

#[derive(Debug, Clone, Default)]
struct DiscordGatewaySession {
    session_id: Option<String>,
    resume_gateway_url: Option<String>,
    sequence: Option<i64>,
}

impl DiscordAdapter {
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
            .ok_or_else(|| anyhow!("discord gateway credential requires bot token"))?
            .trim_start_matches("Bot ")
            .trim()
            .to_string();
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build Discord HTTP client")?;
        Ok(Self {
            bot_token,
            api_base: config
                .api_base
                .clone()
                .unwrap_or_else(|| DEFAULT_DISCORD_API_BASE.to_string()),
            allowed_users: config
                .allowed_users
                .iter()
                .map(|value| clean_discord_id(value))
                .filter(|value| !value.is_empty())
                .collect(),
            allowed_chats: config
                .allowed_chats
                .iter()
                .map(|value| clean_discord_channel(value))
                .filter(|value| !value.is_empty())
                .collect(),
            free_response_channels: config_csv(&config.extra, "free_response_channels")
                .into_iter()
                .map(|value| clean_discord_channel(&value))
                .filter(|value| !value.is_empty())
                .collect(),
            ignored_chats: config_csv(&config.extra, "ignored_channels")
                .into_iter()
                .map(|value| clean_discord_channel(&value))
                .filter(|value| !value.is_empty())
                .collect(),
            require_mention: config_bool(&config.extra, "require_mention", true),
            allowed_mentions: DiscordAllowedMentions::from_config(&config.extra),
            max_download_bytes: config.media.max_download_bytes,
            client,
            bot_user_id: Arc::new(Mutex::new(
                credentials
                    .extra
                    .get("bot_user_id")
                    .map(|value| clean_discord_id(value))
                    .filter(|value| !value.is_empty()),
            )),
            processed_message_ids: Arc::new(Mutex::new(HashSet::new())),
            participated_threads: Arc::new(Mutex::new(HashSet::new())),
            known_thread_channels: Arc::new(Mutex::new(HashSet::new())),
            known_non_thread_channels: Arc::new(Mutex::new(HashSet::new())),
            allow_bot_messages: config
                .extra
                .get("allow_bots")
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| matches!(value.as_str(), "none" | "mentions" | "all"))
                .unwrap_or_else(|| "none".to_string()),
        })
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}{}", self.api_base.trim_end_matches('/'), path)
    }

    fn auth_header(&self) -> String {
        if self.bot_token.starts_with("Bot ") {
            self.bot_token.clone()
        } else {
            format!("Bot {}", self.bot_token)
        }
    }

    fn gateway_url(&self) -> String {
        self.fetch_gateway_url()
            .unwrap_or_else(|_| DEFAULT_DISCORD_GATEWAY_URL.to_string())
    }

    fn fetch_gateway_url(&self) -> Result<String> {
        let response = self
            .client
            .get(self.api_url("/gateway/bot"))
            .header("Authorization", self.auth_header())
            .send()
            .context("discord gateway discovery failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .context("discord gateway discovery returned invalid JSON")?;
        if !status.is_success() {
            bail!("discord gateway discovery failed with status {status}: {value}");
        }
        let url = value["url"]
            .as_str()
            .ok_or_else(|| anyhow!("discord gateway discovery missing url"))?;
        Ok(format!("{}?v=10&encoding=json", url.trim_end_matches('/')))
    }

    fn gateway_loop(self, inbound: GatewayInboundDispatch) {
        let mut session = DiscordGatewaySession::default();
        let mut attempt = 0usize;
        loop {
            let result = self.consume_gateway_once(&inbound, &mut session);
            if let Err(error) = result {
                eprintln!("discord gateway disconnected: {error:#}");
            }
            let sleep = DISCORD_RECONNECT_BACKOFF
                .get(attempt)
                .copied()
                .unwrap_or(*DISCORD_RECONNECT_BACKOFF.last().unwrap_or(&60));
            attempt = attempt.saturating_add(1);
            thread::sleep(Duration::from_secs(sleep));
        }
    }

    fn consume_gateway_once(
        &self,
        inbound: &GatewayInboundDispatch,
        session: &mut DiscordGatewaySession,
    ) -> Result<()> {
        let url = session
            .resume_gateway_url
            .clone()
            .unwrap_or_else(|| self.gateway_url());
        let (mut socket, _) =
            connect(url.as_str()).with_context(|| format!("discord websocket connect: {url}"))?;
        set_read_timeout(&mut socket, Duration::from_secs(10));

        let hello =
            read_json_message(&mut socket)?.ok_or_else(|| anyhow!("discord hello missing"))?;
        let interval_ms = hello["d"]["heartbeat_interval"].as_u64().unwrap_or(45_000);
        let heartbeat_interval = Duration::from_millis(interval_ms);
        let mut next_heartbeat = Instant::now() + heartbeat_interval;

        if session.session_id.is_some() && session.sequence.is_some() {
            self.send_gateway_resume(&mut socket, session)?;
        } else {
            self.send_gateway_identify(&mut socket)?;
        }

        loop {
            if Instant::now() >= next_heartbeat {
                send_json_message(&mut socket, &json!({"op": 1, "d": session.sequence}))?;
                next_heartbeat = Instant::now() + heartbeat_interval;
            }
            match read_json_message(&mut socket) {
                Ok(Some(value)) => {
                    self.handle_gateway_payload(value, inbound, session, &mut socket)?
                }
                Ok(None) => {}
                Err(error) if is_transient_read_error(&error) => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn send_gateway_identify(&self, socket: &mut ChannelWebSocket) -> Result<()> {
        send_json_message(
            socket,
            &json!({
                "op": 2,
                "d": {
                    "token": self.bot_token,
                    "intents": DISCORD_GATEWAY_INTENTS,
                    "properties": {
                        "os": std::env::consts::OS,
                        "browser": "duckagent",
                        "device": "duckagent"
                    }
                }
            }),
        )
    }

    fn send_gateway_resume(
        &self,
        socket: &mut ChannelWebSocket,
        session: &DiscordGatewaySession,
    ) -> Result<()> {
        send_json_message(
            socket,
            &json!({
                "op": 6,
                "d": {
                    "token": self.bot_token,
                    "session_id": session.session_id,
                    "seq": session.sequence
                }
            }),
        )
    }

    fn handle_gateway_payload(
        &self,
        value: Value,
        inbound: &GatewayInboundDispatch,
        session: &mut DiscordGatewaySession,
        socket: &mut ChannelWebSocket,
    ) -> Result<()> {
        if let Some(seq) = value["s"].as_i64() {
            session.sequence = Some(seq);
        }
        match value["op"].as_i64() {
            Some(0) => match value["t"].as_str() {
                Some("READY") => {
                    session.session_id = value["d"]["session_id"].as_str().map(str::to_string);
                    session.resume_gateway_url = value["d"]["resume_gateway_url"]
                        .as_str()
                        .map(|url| format!("{}?v=10&encoding=json", url.trim_end_matches('/')));
                    if let Some(id) = value["d"]["user"]["id"].as_str() {
                        *self
                            .bot_user_id
                            .lock()
                            .expect("discord bot id mutex poisoned") = Some(id.to_string());
                    }
                }
                Some("MESSAGE_CREATE") => {
                    if let Some(message) = self.message_to_inbound(&value["d"])? {
                        inbound.submit(message)?;
                    }
                }
                Some("INTERACTION_CREATE") => {
                    if let Some(message) = self.interaction_to_inbound(&value["d"])? {
                        let _ = self.ack_interaction(&value["d"]);
                        inbound.submit(message)?;
                    }
                }
                _ => {}
            },
            Some(1) => send_json_message(socket, &json!({"op": 1, "d": session.sequence}))?,
            Some(7) => bail!("discord requested reconnect"),
            Some(9) => {
                session.session_id = None;
                session.sequence = None;
                bail!("discord invalid session")
            }
            Some(10) | Some(11) => {}
            _ => {}
        }
        Ok(())
    }

    fn message_to_inbound(&self, message: &Value) -> Result<Option<InboundMessageInput>> {
        if let Some(message_id) = message["id"].as_str() {
            if self.is_duplicate_message(message_id) {
                return Ok(None);
            }
        }
        if !discord_message_type_allowed(message["type"].as_i64()) {
            return Ok(None);
        }
        let channel_id = message["channel_id"]
            .as_str()
            .ok_or_else(|| anyhow!("discord MESSAGE_CREATE missing channel_id"))?;
        let author_id = message["author"]["id"].as_str().unwrap_or_default();
        if identity_list_matches(&self.ignored_chats, channel_id)
            || message["guild_id"]
                .as_str()
                .is_some_and(|guild_id| identity_list_matches(&self.ignored_chats, guild_id))
        {
            return Ok(None);
        }
        if !identity_allowed(&self.allowed_chats, channel_id)
            && !message["guild_id"]
                .as_str()
                .is_some_and(|guild_id| identity_allowed(&self.allowed_chats, guild_id))
        {
            return Ok(None);
        }
        if !identity_allowed(&self.allowed_users, author_id) {
            return Ok(None);
        }
        let bot_id = self
            .bot_user_id
            .lock()
            .expect("discord bot id mutex poisoned")
            .clone();
        if bot_id.as_deref() == Some(author_id) {
            return Ok(None);
        }
        let mentions_bot = bot_id
            .as_deref()
            .is_some_and(|bot_id| message_mentions_user(message, bot_id));
        if message["author"]["bot"].as_bool().unwrap_or(false) {
            match self.allow_bot_messages.as_str() {
                "all" => {}
                "mentions" if mentions_bot => {}
                _ => return Ok(None),
            }
        } else if !identity_allowed(&self.allowed_users, author_id) {
            return Ok(None);
        }
        let is_guild = message["guild_id"].as_str().is_some();
        if is_guild
            && !mentions_bot
            && bot_id
                .as_deref()
                .is_some_and(|bot_id| message_mentions_other_bot(message, bot_id))
        {
            return Ok(None);
        }
        let is_free_response = identity_list_matches(&self.free_response_channels, channel_id);
        let is_participated_thread = self.thread_participated(channel_id);
        if self.require_mention && is_guild && !is_free_response && !is_participated_thread {
            if !mentions_bot {
                return Ok(None);
            }
        }
        let mut text = message["content"].as_str().unwrap_or_default().to_string();
        if let Some(bot_id) = bot_id.as_deref() {
            text = strip_discord_bot_mention(&text, bot_id);
        }
        let attachments = self.collect_attachments(message);
        if text.trim().is_empty() && attachments.is_empty() {
            text = "[Discord message]".to_string();
        }
        if is_guild && (mentions_bot || is_participated_thread) {
            self.remember_thread_if_thread_channel(channel_id);
        }
        Ok(Some(InboundMessageInput {
            channel: "discord".to_string(),
            conversation_id: channel_id.to_string(),
            thread_id: message["message_reference"]["message_id"]
                .as_str()
                .map(str::to_string),
            chat_type: Some(
                if message["guild_id"].as_str().is_some() {
                    "guild"
                } else {
                    "dm"
                }
                .to_string(),
            ),
            sender_id: Some(author_id.to_string()),
            message_id: message["id"].as_str().map(str::to_string),
            text,
            attachments,
            timestamp: message["timestamp"].as_str().map(str::to_string),
        }))
    }

    fn is_duplicate_message(&self, message_id: &str) -> bool {
        self.processed_message_ids
            .lock()
            .map(|mut ids| {
                if ids.contains(message_id) {
                    return true;
                }
                if ids.len() > 1_000 {
                    ids.clear();
                }
                ids.insert(message_id.to_string());
                false
            })
            .unwrap_or(false)
    }

    fn collect_attachments(&self, message: &Value) -> Vec<InboundAttachmentInput> {
        message["attachments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|attachment| match self.download_attachment(attachment) {
                Ok(value) => Some(value),
                Err(error) => {
                    eprintln!("discord attachment skipped: {error:#}");
                    None
                }
            })
            .collect()
    }

    fn download_attachment(&self, attachment: &Value) -> Result<InboundAttachmentInput> {
        let url = attachment["url"]
            .as_str()
            .ok_or_else(|| anyhow!("discord attachment missing url"))?;
        let size = attachment["size"].as_u64().unwrap_or(0);
        if size > self.max_download_bytes {
            bail!(
                "discord attachment is {size} bytes, over max_download_bytes {}",
                self.max_download_bytes
            );
        }
        let response = self
            .client
            .get(url)
            .send()
            .with_context(|| format!("discord attachment download failed: {url}"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("discord attachment download failed with status {status}");
        }
        if let Some(length) = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            if length > self.max_download_bytes {
                bail!(
                    "discord attachment is {length} bytes, over max_download_bytes {}",
                    self.max_download_bytes
                );
            }
        }
        let bytes = response
            .bytes()
            .context("discord attachment body unreadable")?;
        if bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "discord attachment is {} bytes, over max_download_bytes {}",
                bytes.len(),
                self.max_download_bytes
            );
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: attachment["filename"].as_str().map(str::to_string),
            mime: attachment["content_type"].as_str().map(str::to_string),
        })
    }

    fn interaction_to_inbound(&self, interaction: &Value) -> Result<Option<InboundMessageInput>> {
        if interaction["type"].as_i64() != Some(3) {
            return Ok(None);
        }
        let custom_id = interaction["data"]["custom_id"]
            .as_str()
            .unwrap_or_default();
        let Some(command) = discord_custom_id_to_command(custom_id) else {
            return Ok(None);
        };
        let channel_id = interaction["channel_id"]
            .as_str()
            .ok_or_else(|| anyhow!("discord interaction missing channel_id"))?;
        if !identity_allowed(&self.allowed_chats, channel_id)
            && !interaction["guild_id"]
                .as_str()
                .is_some_and(|guild_id| identity_allowed(&self.allowed_chats, guild_id))
        {
            return Ok(None);
        }
        let user_id = interaction["member"]["user"]["id"]
            .as_str()
            .or_else(|| interaction["user"]["id"].as_str())
            .unwrap_or_default();
        if !identity_allowed(&self.allowed_users, user_id) {
            return Ok(None);
        }
        Ok(Some(InboundMessageInput {
            channel: "discord".to_string(),
            conversation_id: channel_id.to_string(),
            thread_id: interaction["message"]["id"].as_str().map(str::to_string),
            chat_type: Some(
                if interaction["guild_id"].as_str().is_some() {
                    "guild"
                } else {
                    "dm"
                }
                .to_string(),
            ),
            sender_id: Some(user_id.to_string()),
            message_id: interaction["id"].as_str().map(str::to_string),
            text: command,
            attachments: Vec::new(),
            timestamp: Some(now_rfc3339_like()),
        }))
    }

    fn ack_interaction(&self, interaction: &Value) -> Result<()> {
        let id = interaction["id"]
            .as_str()
            .ok_or_else(|| anyhow!("discord interaction missing id"))?;
        let token = interaction["token"]
            .as_str()
            .ok_or_else(|| anyhow!("discord interaction missing token"))?;
        let response = self
            .client
            .post(self.api_url(&format!("/interactions/{id}/{token}/callback")))
            .json(&json!({"type": 6}))
            .send()
            .context("discord interaction ack failed")?;
        if !response.status().is_success() {
            bail!(
                "discord interaction ack failed with status {}",
                response.status()
            );
        }
        Ok(())
    }

    fn post_json<T: Serialize>(&self, path: &str, body: &T) -> Result<Value> {
        let response = self
            .client
            .post(self.api_url(path))
            .header("Authorization", self.auth_header())
            .json(body)
            .send()
            .with_context(|| format!("discord POST {path} failed"))?;
        self.parse_rest_response(path, response)
    }

    fn patch_json<T: Serialize>(&self, path: &str, body: &T) -> Result<Value> {
        let response = self
            .client
            .patch(self.api_url(path))
            .header("Authorization", self.auth_header())
            .json(body)
            .send()
            .with_context(|| format!("discord PATCH {path} failed"))?;
        self.parse_rest_response(path, response)
    }

    fn post_multipart(&self, path: &str, form: multipart::Form) -> Result<Value> {
        let response = self
            .client
            .post(self.api_url(path))
            .header("Authorization", self.auth_header())
            .multipart(form)
            .send()
            .with_context(|| format!("discord multipart POST {path} failed"))?;
        self.parse_rest_response(path, response)
    }

    fn parse_rest_response(
        &self,
        path: &str,
        response: reqwest::blocking::Response,
    ) -> Result<Value> {
        let status = response.status();
        let value = response
            .json::<Value>()
            .unwrap_or_else(|_| json!({"message": "non-json response"}));
        if status.as_u16() == 429 {
            let retry_after = value["retry_after"].as_f64().unwrap_or(1.0);
            thread::sleep(Duration::from_millis((retry_after * 1000.0) as u64));
            bail!("discord POST {path} rate limited: {value}");
        }
        if !status.is_success() {
            bail!("discord POST {path} failed with status {status}: {value}");
        }
        Ok(value)
    }

    fn get_json(&self, path: &str) -> Result<Value> {
        let response = self
            .client
            .get(self.api_url(path))
            .header("Authorization", self.auth_header())
            .send()
            .with_context(|| format!("discord GET {path} failed"))?;
        self.parse_rest_response(path, response)
    }

    fn is_thread_channel(&self, channel_id: &str) -> bool {
        if self
            .known_thread_channels
            .lock()
            .map(|channels| channels.contains(channel_id))
            .unwrap_or(false)
        {
            return true;
        }
        if self
            .known_non_thread_channels
            .lock()
            .map(|channels| channels.contains(channel_id))
            .unwrap_or(false)
        {
            return false;
        }
        let is_thread = self
            .get_json(&format!("/channels/{channel_id}"))
            .ok()
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(Value::as_i64)
                    .map(is_discord_thread_type)
            })
            .unwrap_or(false);
        let target = if is_thread {
            &self.known_thread_channels
        } else {
            &self.known_non_thread_channels
        };
        if let Ok(mut channels) = target.lock() {
            if channels.len() > 10_000 {
                channels.clear();
            }
            channels.insert(channel_id.to_string());
        }
        is_thread
    }

    fn remember_thread_if_thread_channel(&self, channel_id: &str) {
        if !self.is_thread_channel(channel_id) {
            return;
        }
        if let Ok(mut threads) = self.participated_threads.lock() {
            if threads.len() > 10_000 {
                threads.clear();
            }
            threads.insert(channel_id.to_string());
        }
    }

    fn thread_participated(&self, channel_id: &str) -> bool {
        self.participated_threads
            .lock()
            .map(|threads| threads.contains(channel_id))
            .unwrap_or(false)
    }

    fn send_text_chunk(
        &self,
        route: &GatewayRoute,
        text: &str,
        reply_to: Option<&str>,
        components: Option<Value>,
    ) -> Result<()> {
        self.send_text_chunk_with_response(route, text, reply_to, components)
            .map(|_| ())
    }

    fn send_text_chunk_with_response(
        &self,
        route: &GatewayRoute,
        text: &str,
        reply_to: Option<&str>,
        components: Option<Value>,
    ) -> Result<Value> {
        let mut body = json!({
            "content": text,
            "allowed_mentions": self.allowed_mentions.to_json(),
        });
        if let Some(reply_to) = reply_to.filter(|value| !value.is_empty()) {
            body["message_reference"] = json!({
                "message_id": reply_to,
                "channel_id": route.key.conversation_id,
                "fail_if_not_exists": false,
            });
        }
        if let Some(components) = components {
            body["components"] = components;
        }
        self.post_json(
            &format!("/channels/{}/messages", route.key.conversation_id),
            &body,
        )
    }

    fn edit_text_message(&self, route: &GatewayRoute, message_id: &str, text: &str) -> Result<()> {
        self.patch_json(
            &format!(
                "/channels/{}/messages/{}",
                route.key.conversation_id, message_id
            ),
            &json!({
                "content": text,
                "allowed_mentions": self.allowed_mentions.to_json(),
            }),
        )?;
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
            return self.send_text_chunk(route, &text, route.key.thread_id.as_deref(), None);
        }
        let path = Path::new(path);
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read Discord upload file {}", path.display()))?;
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("duckagent-upload")
            .to_string();
        let payload = json!({
            "content": caption.unwrap_or_default(),
            "allowed_mentions": self.allowed_mentions.to_json(),
            "attachments": [{"id": 0, "filename": filename}],
        });
        let mut payload = payload;
        if let Some(reply_to) = route
            .key
            .thread_id
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            payload["message_reference"] = json!({
                "message_id": reply_to,
                "channel_id": route.key.conversation_id,
                "fail_if_not_exists": false,
            });
        }
        let part = multipart::Part::bytes(bytes).file_name(filename);
        let form = multipart::Form::new()
            .text("payload_json", serde_json::to_string(&payload)?)
            .part("files[0]", part);
        self.post_multipart(
            &format!("/channels/{}/messages", route.key.conversation_id),
            form,
        )?;
        Ok(())
    }
}

impl ChannelAdapter for DiscordAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        let adapter = self.clone();
        thread::spawn(move || adapter.gateway_loop(inbound));
        Ok(())
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let mut text_sent = false;
        for chunk in discord_text_chunks(&message.text) {
            self.send_text_chunk(route, &chunk, route.key.thread_id.as_deref(), None)?;
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
        let value =
            self.send_text_chunk_with_response(route, text, route.key.thread_id.as_deref(), None)?;
        let message_id = value["id"]
            .as_str()
            .ok_or_else(|| anyhow!("discord stream start did not return message id"))?
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
        self.edit_text_message(route, &handle.message_id, text)
    }

    fn stream_text_limit(&self) -> usize {
        DISCORD_TEXT_LIMIT.saturating_sub(100)
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        if !event.active {
            return Ok(());
        }
        self.post_json(
            &format!("/channels/{}/typing", route.key.conversation_id),
            &json!({}),
        )?;
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        let text = format!("{}\n\nCommand:\n```{}\n```", prompt.message, prompt.command);
        self.send_text_chunk(
            route,
            &text,
            route.key.thread_id.as_deref(),
            Some(discord_approval_components(&prompt.id)),
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

impl DiscordAllowedMentions {
    fn from_config(extra: &std::collections::BTreeMap<String, String>) -> Self {
        Self {
            everyone: config_bool(extra, "allow_mention_everyone", false),
            roles: config_bool(extra, "allow_mention_roles", false),
            users: config_bool(extra, "allow_mention_users", true),
            replied_user: config_bool(extra, "allow_mention_replied_user", false),
        }
    }

    fn to_json(&self) -> Value {
        let mut parse = Vec::new();
        if self.everyone {
            parse.push("everyone");
        }
        if self.roles {
            parse.push("roles");
        }
        if self.users {
            parse.push("users");
        }
        json!({
            "parse": parse,
            "replied_user": self.replied_user,
        })
    }
}

fn read_json_message(socket: &mut ChannelWebSocket) -> Result<Option<Value>> {
    read_ws_json_message(
        socket,
        "discord websocket read failed",
        "discord gateway returned invalid JSON",
    )
}

fn send_json_message(socket: &mut ChannelWebSocket, value: &Value) -> Result<()> {
    send_ws_json_message(socket, value, "discord websocket send failed")
}

fn discord_text_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.len() + ch.len_utf8() > DISCORD_TEXT_LIMIT && !current.is_empty() {
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

fn discord_approval_components(id: &str) -> Value {
    json!([
        {
            "type": 1,
            "components": [
                discord_button("Allow Once", 3, &format!("duckagent:approve:{id}:once")),
                discord_button("Allow Session", 2, &format!("duckagent:approve:{id}:session")),
                discord_button("Always Allow", 1, &format!("duckagent:approve:{id}:always")),
                discord_button("Deny", 4, &format!("duckagent:deny:{id}"))
            ]
        }
    ])
}

fn discord_button(label: &str, style: u8, custom_id: &str) -> Value {
    json!({
        "type": 2,
        "style": style,
        "label": label,
        "custom_id": custom_id,
    })
}

fn discord_custom_id_to_command(value: &str) -> Option<String> {
    let parts = value.split(':').collect::<Vec<_>>();
    match parts.as_slice() {
        ["duckagent", "approve", id, decision] => Some(format!("/approve {id} {decision}")),
        ["duckagent", "deny", id] => Some(format!("/deny {id}")),
        _ => None,
    }
}

fn discord_message_type_allowed(kind: Option<i64>) -> bool {
    matches!(kind, None | Some(0) | Some(19))
}

fn message_mentions_user(message: &Value, user_id: &str) -> bool {
    message["mentions"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|mention| mention["id"].as_str() == Some(user_id))
        || message["content"].as_str().is_some_and(|text| {
            text.contains(&format!("<@{user_id}>")) || text.contains(&format!("<@!{user_id}>"))
        })
}

fn message_mentions_other_bot(message: &Value, bot_id: &str) -> bool {
    message["mentions"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|mention| {
            mention["id"].as_str() != Some(bot_id) && mention["bot"].as_bool().unwrap_or(false)
        })
}

fn strip_discord_bot_mention(text: &str, bot_id: &str) -> String {
    text.replace(&format!("<@{bot_id}>"), "")
        .replace(&format!("<@!{bot_id}>"), "")
        .trim()
        .to_string()
}

fn identity_allowed(allowed: &[String], id: &str) -> bool {
    allowed.is_empty()
        || allowed
            .iter()
            .any(|allowed| allowed == "*" || allowed.trim() == id)
}

fn identity_list_matches(allowed: &[String], id: &str) -> bool {
    !allowed.is_empty() && identity_allowed(allowed, id)
}

fn config_csv(extra: &std::collections::BTreeMap<String, String>, key: &str) -> Vec<String> {
    extra
        .get(key)
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn is_discord_thread_type(kind: i64) -> bool {
    matches!(kind, 10..=12)
}

fn clean_discord_id(value: &str) -> String {
    let mut value = value.trim();
    if let Some(stripped) = value.strip_prefix("user:") {
        value = stripped;
    }
    value
        .trim_start_matches("<@!")
        .trim_start_matches("<@")
        .trim_end_matches('>')
        .trim()
        .to_string()
}

fn clean_discord_channel(value: &str) -> String {
    let mut value = value.trim();
    if let Some(stripped) = value.strip_prefix("channel:") {
        value = stripped;
    }
    value
        .trim_start_matches("<#")
        .trim_end_matches('>')
        .trim()
        .to_string()
}

fn config_bool(
    extra: &std::collections::BTreeMap<String, String>,
    key: &str,
    default: bool,
) -> bool {
    extra
        .get(key)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}

fn now_rfc3339_like() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discord_custom_ids_map_to_approval_commands() {
        assert_eq!(
            discord_custom_id_to_command("duckagent:approve:appr_1:session").as_deref(),
            Some("/approve appr_1 session")
        );
        assert_eq!(
            discord_custom_id_to_command("duckagent:deny:appr_1").as_deref(),
            Some("/deny appr_1")
        );
        assert!(discord_custom_id_to_command("other").is_none());
    }

    #[test]
    fn discord_text_chunks_respect_limit() {
        let text = "x".repeat(DISCORD_TEXT_LIMIT + 1);
        let chunks = discord_text_chunks(&text);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| chunk.len() <= DISCORD_TEXT_LIMIT));
    }

    #[test]
    fn discord_message_maps_to_inbound_when_bot_mentioned() -> Result<()> {
        let adapter = test_adapter()?;
        *adapter.bot_user_id.lock().unwrap() = Some("999".to_string());
        let value = json!({
            "id": "m1",
            "channel_id": "c1",
            "guild_id": "g1",
            "timestamp": "2026-05-14T00:00:00.000000+00:00",
            "content": "<@999> hello",
            "author": {"id": "u1", "bot": false},
            "mentions": [{"id": "999"}],
            "attachments": []
        });
        let inbound = adapter.message_to_inbound(&value)?.expect("inbound");
        assert_eq!(inbound.channel, "discord");
        assert_eq!(inbound.conversation_id, "c1");
        assert_eq!(inbound.sender_id.as_deref(), Some("u1"));
        assert_eq!(inbound.text, "hello");
        Ok(())
    }

    #[test]
    fn discord_message_requires_mention_in_guild() -> Result<()> {
        let adapter = test_adapter()?;
        *adapter.bot_user_id.lock().unwrap() = Some("999".to_string());
        let value = json!({
            "id": "m1",
            "channel_id": "c1",
            "guild_id": "g1",
            "content": "hello",
            "author": {"id": "u1", "bot": false},
            "mentions": [],
            "attachments": []
        });
        assert!(adapter.message_to_inbound(&value)?.is_none());
        Ok(())
    }

    #[test]
    fn discord_free_response_channel_skips_mention_requirement() -> Result<()> {
        let adapter = test_adapter_with_extra(&[("free_response_channels", "c1")])?;
        *adapter.bot_user_id.lock().unwrap() = Some("999".to_string());
        let value = json!({
            "id": "m1",
            "channel_id": "c1",
            "guild_id": "g1",
            "content": "hello",
            "author": {"id": "u1", "bot": false},
            "mentions": [],
            "attachments": []
        });
        let inbound = adapter.message_to_inbound(&value)?.expect("inbound");
        assert_eq!(inbound.text, "hello");
        Ok(())
    }

    #[test]
    fn discord_participated_thread_allows_followup_without_mention() -> Result<()> {
        let adapter = test_adapter()?;
        *adapter.bot_user_id.lock().unwrap() = Some("999".to_string());
        adapter
            .known_thread_channels
            .lock()
            .unwrap()
            .insert("t1".to_string());
        adapter
            .participated_threads
            .lock()
            .unwrap()
            .insert("t1".to_string());
        let value = json!({
            "id": "m1",
            "channel_id": "t1",
            "guild_id": "g1",
            "content": "follow-up",
            "author": {"id": "u1", "bot": false},
            "mentions": [],
            "attachments": []
        });
        let inbound = adapter.message_to_inbound(&value)?.expect("thread inbound");
        assert_eq!(inbound.conversation_id, "t1");
        assert_eq!(inbound.text, "follow-up");
        Ok(())
    }

    #[test]
    fn discord_allowed_mentions_are_safe_by_default() {
        let value = DiscordAllowedMentions::from_config(&Default::default()).to_json();
        assert_eq!(value["parse"], json!(["users"]));
        assert_eq!(value["replied_user"], json!(false));
    }

    fn test_adapter() -> Result<DiscordAdapter> {
        test_adapter_with_extra(&[])
    }

    fn test_adapter_with_extra(extra: &[(&str, &str)]) -> Result<DiscordAdapter> {
        let mut config = GatewayChannelConfig {
            enabled: true,
            ..Default::default()
        };
        for (key, value) in extra {
            config
                .extra
                .insert((*key).to_string(), (*value).to_string());
        }
        DiscordAdapter::new(
            &config,
            &GatewayCredentialEntry {
                channel: "discord".to_string(),
                token: Some("token".to_string()),
                ..Default::default()
            },
        )
    }
}
