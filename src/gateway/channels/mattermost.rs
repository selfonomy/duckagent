use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundAttachmentInput,
    InboundMessageInput, OutboundMessage, StreamMessageHandle, TypingEvent,
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
use std::time::Duration;
use tungstenite::connect;

const MATTERMOST_ACTIONS_PATH: &str = "/mattermost/actions";
const MATTERMOST_TEXT_LIMIT: usize = 4_000;
const MATTERMOST_RECONNECT_BACKOFF: &[u64] = &[2, 5, 10, 30, 60];

#[derive(Clone)]
pub(in crate::gateway) struct MattermostAdapter {
    token: String,
    base_url: String,
    allowed_users: Vec<String>,
    allowed_chats: Vec<String>,
    require_mention: bool,
    free_response_chats: Vec<String>,
    reply_mode: MattermostReplyMode,
    callback_url: Option<String>,
    callback_token: Option<String>,
    max_download_bytes: u64,
    client: Client,
    bot_identity: Arc<Mutex<Option<MattermostBotIdentity>>>,
    seen_posts: Arc<Mutex<HashSet<String>>>,
}

#[derive(Debug, Clone)]
struct MattermostBotIdentity {
    user_id: String,
    username: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MattermostReplyMode {
    Off,
    Thread,
}

impl MattermostAdapter {
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
            .ok_or_else(|| anyhow!("mattermost gateway credential requires token"))?
            .to_string();
        let base_url = config
            .api_base
            .as_deref()
            .or_else(|| credentials.extra.get("url").map(String::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("mattermost gateway config requires api_base server URL"))?
            .trim_end_matches('/')
            .to_string();
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build Mattermost HTTP client")?;
        Ok(Self {
            token,
            base_url,
            allowed_users: config.allowed_users.clone(),
            allowed_chats: config.allowed_chats.clone(),
            require_mention: config_bool(&config.extra, "require_mention", true),
            free_response_chats: csv_from_extra(&config.extra, "free_response_channels"),
            reply_mode: match config.extra.get("reply_mode").map(String::as_str) {
                Some("thread") => MattermostReplyMode::Thread,
                _ => MattermostReplyMode::Off,
            },
            callback_url: config
                .extra
                .get("approval_callback_url")
                .or_else(|| config.extra.get("public_url"))
                .map(|value| value.trim().trim_end_matches('/').to_string())
                .filter(|value| !value.is_empty()),
            callback_token: credentials
                .webhook_secret
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            max_download_bytes: config.media.max_download_bytes,
            client,
            bot_identity: Arc::new(Mutex::new(mattermost_identity_from_extra(
                &credentials.extra,
            ))),
            seen_posts: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}/api/v4/{}", self.base_url, path.trim_start_matches('/'))
    }

    fn ws_url(&self) -> String {
        let ws_base = if let Some(rest) = self.base_url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.base_url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            format!("wss://{}", self.base_url)
        };
        format!("{}/api/v4/websocket", ws_base.trim_end_matches('/'))
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }

    fn authenticate(&self) -> Result<MattermostBotIdentity> {
        let response = self
            .client
            .get(self.api_url("users/me"))
            .header("Authorization", self.bearer())
            .send()
            .context("mattermost users/me failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .context("mattermost users/me returned invalid JSON")?;
        if !status.is_success() {
            bail!("mattermost users/me failed with status {status}: {value}");
        }
        Ok(MattermostBotIdentity {
            user_id: value["id"]
                .as_str()
                .ok_or_else(|| anyhow!("mattermost users/me missing id"))?
                .to_string(),
            username: value["username"].as_str().unwrap_or_default().to_string(),
        })
    }

    fn websocket_loop(self, inbound: GatewayInboundDispatch) {
        let mut attempt = 0usize;
        loop {
            match self.consume_websocket_once(&inbound) {
                Ok(()) => attempt = 0,
                Err(error) => eprintln!("mattermost websocket disconnected: {error:#}"),
            }
            let sleep = MATTERMOST_RECONNECT_BACKOFF
                .get(attempt)
                .copied()
                .unwrap_or(*MATTERMOST_RECONNECT_BACKOFF.last().unwrap_or(&60));
            attempt = attempt.saturating_add(1);
            thread::sleep(Duration::from_secs(sleep));
        }
    }

    fn consume_websocket_once(&self, inbound: &GatewayInboundDispatch) -> Result<()> {
        let (mut socket, _) = connect(self.ws_url().as_str())
            .with_context(|| format!("mattermost websocket connect: {}", self.ws_url()))?;
        set_read_timeout(&mut socket, Duration::from_secs(45));
        send_json_message(
            &mut socket,
            &json!({
                "seq": 1,
                "action": "authentication_challenge",
                "data": {"token": self.token}
            }),
        )?;
        loop {
            match read_json_message(&mut socket) {
                Ok(Some(event)) => {
                    if let Some(input) = self.event_to_inbound(&event)? {
                        inbound.submit(input)?;
                    }
                }
                Ok(None) => {}
                Err(error) if is_transient_read_error(&error) => {
                    send_json_message(&mut socket, &json!({"seq": 0, "action": "ping"}))?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn event_to_inbound(&self, event: &Value) -> Result<Option<InboundMessageInput>> {
        if event["event"].as_str() != Some("posted") {
            return Ok(None);
        }
        let data = &event["data"];
        let raw_post = data["post"]
            .as_str()
            .ok_or_else(|| anyhow!("mattermost posted event missing data.post"))?;
        let post: Value =
            serde_json::from_str(raw_post).context("mattermost post JSON is invalid")?;
        if let Some(post_id) = post.get("id").and_then(Value::as_str) {
            if !self.remember_post_once(post_id) {
                return Ok(None);
            }
        }
        let identity = self
            .bot_identity
            .lock()
            .expect("mattermost identity mutex poisoned")
            .clone();
        if identity
            .as_ref()
            .is_some_and(|identity| post["user_id"].as_str() == Some(identity.user_id.as_str()))
        {
            return Ok(None);
        }
        if post["type"].as_str().is_some_and(|value| !value.is_empty()) {
            return Ok(None);
        }
        let channel_id = post["channel_id"]
            .as_str()
            .ok_or_else(|| anyhow!("mattermost post missing channel_id"))?;
        let channel_type = data["channel_type"].as_str().unwrap_or("O");
        if channel_type != "D" && !identity_allowed(&self.allowed_chats, channel_id) {
            return Ok(None);
        }
        let sender_id = post["user_id"].as_str().unwrap_or_default();
        if !self.user_allowed(sender_id) {
            return Ok(None);
        }
        let mut text = post["message"].as_str().unwrap_or_default().to_string();
        if channel_type != "D"
            && self.require_mention
            && !list_contains(&self.free_response_chats, channel_id)
        {
            let Some(identity) = identity.as_ref() else {
                return Ok(None);
            };
            if !self.post_mentions_bot(&post, &text, identity) {
                return Ok(None);
            }
            text = strip_mattermost_bot_mention(&text, identity);
        }
        let attachments = self.collect_attachments(&post);
        if text.trim().is_empty() && attachments.is_empty() {
            text = "[Mattermost message]".to_string();
        }
        Ok(Some(InboundMessageInput {
            channel: "mattermost".to_string(),
            conversation_id: channel_id.to_string(),
            thread_id: mattermost_thread_id_for_post(&post, self.reply_mode),
            chat_type: Some(mattermost_chat_type(channel_type).to_string()),
            sender_id: Some(sender_id.to_string()),
            message_id: post["id"].as_str().map(str::to_string),
            text,
            attachments,
            timestamp: mattermost_create_at_to_rfc3339(post["create_at"].as_i64()),
        }))
    }

    fn remember_post_once(&self, post_id: &str) -> bool {
        let mut seen = self
            .seen_posts
            .lock()
            .expect("mattermost seen posts mutex poisoned");
        if !seen.insert(post_id.to_string()) {
            return false;
        }
        if seen.len() > 4096 {
            seen.clear();
            seen.insert(post_id.to_string());
        }
        true
    }

    fn post_mentions_bot(
        &self,
        post: &Value,
        text: &str,
        identity: &MattermostBotIdentity,
    ) -> bool {
        mattermost_mentions_bot(text, identity)
            || post
                .get("metadata")
                .and_then(|metadata| metadata.get("mentions"))
                .and_then(Value::as_array)
                .is_some_and(|mentions| {
                    mentions
                        .iter()
                        .any(|mention| mention.as_str() == Some(identity.user_id.as_str()))
                })
    }

    fn user_allowed(&self, user_id: &str) -> bool {
        self.allowed_users.iter().any(|value| value.trim() == "*")
            || identity_allowed(&self.allowed_users, user_id)
    }

    fn collect_attachments(&self, post: &Value) -> Vec<InboundAttachmentInput> {
        post["file_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str())
            .filter_map(|id| match self.download_file(id) {
                Ok(attachment) => Some(attachment),
                Err(error) => {
                    eprintln!("mattermost file skipped: {error:#}");
                    None
                }
            })
            .collect()
    }

    fn download_file(&self, file_id: &str) -> Result<InboundAttachmentInput> {
        let info = self.get_json(&format!("files/{file_id}/info"))?;
        let name = info["name"].as_str().unwrap_or(file_id).to_string();
        let mime = info["mime_type"]
            .as_str()
            .unwrap_or("application/octet-stream")
            .to_string();
        let response = self
            .client
            .get(self.api_url(&format!("files/{file_id}")))
            .header("Authorization", self.bearer())
            .send()
            .context("mattermost file download failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("mattermost file download failed with status {status}");
        }
        if let Some(length) = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            if length > self.max_download_bytes {
                bail!(
                    "mattermost file is {length} bytes, over max_download_bytes {}",
                    self.max_download_bytes
                );
            }
        }
        let bytes = response
            .bytes()
            .context("mattermost file body unreadable")?;
        if bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "mattermost file is {} bytes, over max_download_bytes {}",
                bytes.len(),
                self.max_download_bytes
            );
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: Some(name),
            mime: Some(mime),
        })
    }

    fn get_json(&self, path: &str) -> Result<Value> {
        let response = self
            .client
            .get(self.api_url(path))
            .header("Authorization", self.bearer())
            .send()
            .with_context(|| format!("mattermost GET {path} failed"))?;
        self.parse_response("GET", path, response)
    }

    fn post_json<T: Serialize>(&self, path: &str, body: &T) -> Result<Value> {
        let response = self
            .client
            .post(self.api_url(path))
            .header("Authorization", self.bearer())
            .json(body)
            .send()
            .with_context(|| format!("mattermost POST {path} failed"))?;
        self.parse_response("POST", path, response)
    }

    fn put_json<T: Serialize>(&self, path: &str, body: &T) -> Result<Value> {
        let response = self
            .client
            .put(self.api_url(path))
            .header("Authorization", self.bearer())
            .json(body)
            .send()
            .with_context(|| format!("mattermost PUT {path} failed"))?;
        self.parse_response("PUT", path, response)
    }

    fn post_multipart(&self, path: &str, form: multipart::Form) -> Result<Value> {
        let response = self
            .client
            .post(self.api_url(path))
            .header("Authorization", self.bearer())
            .multipart(form)
            .send()
            .with_context(|| format!("mattermost multipart POST {path} failed"))?;
        self.parse_response("POST", path, response)
    }

    fn parse_response(
        &self,
        method: &str,
        path: &str,
        response: reqwest::blocking::Response,
    ) -> Result<Value> {
        let status = response.status();
        let value = response
            .json::<Value>()
            .unwrap_or_else(|_| json!({"message": "non-json response"}));
        if !status.is_success() {
            bail!("mattermost {method} {path} failed with status {status}: {value}");
        }
        Ok(value)
    }

    fn send_text_chunk(&self, route: &GatewayRoute, text: &str) -> Result<()> {
        self.send_text_chunk_with_response(route, text).map(|_| ())
    }

    fn send_text_chunk_with_response(&self, route: &GatewayRoute, text: &str) -> Result<Value> {
        let mut body = json!({
            "channel_id": route.key.conversation_id,
            "message": format_mattermost_message(text),
        });
        if self.reply_mode == MattermostReplyMode::Thread {
            if let Some(root_id) = route.key.thread_id.as_deref() {
                body["root_id"] = json!(root_id);
            }
        }
        self.post_json("posts", &body)
    }

    fn update_text_message(&self, post_id: &str, text: &str) -> Result<()> {
        self.put_json(
            &format!("posts/{post_id}/patch"),
            &json!({"message": format_mattermost_message(text)}),
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
            return self.send_text_chunk(route, &text);
        }
        let path = Path::new(path);
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read Mattermost upload file {}", path.display()))?;
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("duckagent-upload")
            .to_string();
        let form = multipart::Form::new()
            .text("channel_id", route.key.conversation_id.clone())
            .part("files", multipart::Part::bytes(bytes).file_name(filename));
        let uploaded = self.post_multipart("files", form)?;
        let file_id = uploaded["file_infos"][0]["id"]
            .as_str()
            .ok_or_else(|| anyhow!("mattermost file upload missing file id"))?;
        let mut body = json!({
            "channel_id": route.key.conversation_id,
            "message": caption.unwrap_or_default(),
            "file_ids": [file_id],
        });
        if self.reply_mode == MattermostReplyMode::Thread {
            if let Some(root_id) = route.key.thread_id.as_deref() {
                body["root_id"] = json!(root_id);
            }
        }
        self.post_json("posts", &body)?;
        Ok(())
    }

    fn handle_action(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        let value: Value = serde_json::from_slice(&request.body)
            .context("failed to parse Mattermost action JSON")?;
        if let Some(expected) = self.callback_token.as_deref() {
            let actual = value["context"]["token"]
                .as_str()
                .or_else(|| value["token"].as_str())
                .unwrap_or_default();
            if actual != expected {
                bail!("Mattermost action callback token mismatch");
            }
        }
        let command = value["context"]["command"]
            .as_str()
            .ok_or_else(|| anyhow!("Mattermost action context missing command"))?;
        let channel_id = value["channel_id"]
            .as_str()
            .ok_or_else(|| anyhow!("Mattermost action missing channel_id"))?;
        inbound.submit(InboundMessageInput {
            channel: "mattermost".to_string(),
            conversation_id: channel_id.to_string(),
            thread_id: value["post_id"].as_str().map(str::to_string),
            chat_type: Some("channel".to_string()),
            sender_id: value["user_id"].as_str().map(str::to_string),
            message_id: value["trigger_id"].as_str().map(str::to_string),
            text: command.to_string(),
            attachments: Vec::new(),
            timestamp: Some(now_rfc3339_like()),
        })?;
        Ok(json_response(
            200,
            json!({"update": {"message": "Recorded."}}),
        ))
    }
}

fn mattermost_thread_id_for_post(post: &Value, reply_mode: MattermostReplyMode) -> Option<String> {
    if let Some(root_id) = post["root_id"].as_str().filter(|value| !value.is_empty()) {
        return Some(root_id.to_string());
    }
    if reply_mode == MattermostReplyMode::Thread {
        return post["id"]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_string);
    }
    None
}

impl ChannelAdapter for MattermostAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        let identity = self.authenticate()?;
        *self
            .bot_identity
            .lock()
            .expect("mattermost identity mutex poisoned") = Some(identity);
        let adapter = self.clone();
        thread::spawn(move || adapter.websocket_loop(inbound));
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        if request.method == "POST" && request.path == MATTERMOST_ACTIONS_PATH {
            return self.handle_action(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let mut text_sent = false;
        for chunk in mattermost_text_chunks(&message.text) {
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
        let value = self.send_text_chunk_with_response(route, text)?;
        let message_id = value["id"]
            .as_str()
            .ok_or_else(|| anyhow!("mattermost stream start did not return post id"))?
            .to_string();
        Ok(Some(StreamMessageHandle { message_id }))
    }

    fn update_stream(
        &self,
        _route: &GatewayRoute,
        handle: &StreamMessageHandle,
        text: &str,
        _final_update: bool,
    ) -> Result<()> {
        self.update_text_message(&handle.message_id, text)
    }

    fn stream_text_limit(&self) -> usize {
        MATTERMOST_TEXT_LIMIT
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        if !event.active {
            return Ok(());
        }
        let identity = self
            .bot_identity
            .lock()
            .expect("mattermost identity mutex poisoned")
            .clone()
            .ok_or_else(|| anyhow!("mattermost bot identity not initialized"))?;
        let mut body = json!({"channel_id": route.key.conversation_id});
        if self.reply_mode == MattermostReplyMode::Thread {
            if let Some(parent_id) = route.key.thread_id.as_deref() {
                body["parent_id"] = json!(parent_id);
            }
        }
        self.post_json(&format!("users/{}/typing", identity.user_id), &body)?;
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        let mut body = json!({
            "channel_id": route.key.conversation_id,
            "message": prompt.message.clone(),
        });
        if self.reply_mode == MattermostReplyMode::Thread {
            if let Some(root_id) = route.key.thread_id.as_deref() {
                body["root_id"] = json!(root_id);
            }
        }
        if let Some(callback_url) = self.callback_url.as_deref() {
            body["props"] = json!({
                "attachments": [{
                    "actions": mattermost_approval_actions(
                        callback_url,
                        self.callback_token.as_deref(),
                        &prompt.id
                    )
                }]
            });
        } else {
            body["message"] = json!(format!(
                "{}\n\nCommands:\n`/approve {} once`\n`/approve {} session`\n`/approve {} always`\n`/deny {}`",
                prompt.message, prompt.id, prompt.id, prompt.id, prompt.id
            ));
        }
        self.post_json("posts", &body)?;
        Ok(())
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: true,
            typing: true,
            approval_prompt: true,
        }
    }
}

fn mattermost_approval_actions(
    callback_url: &str,
    callback_token: Option<&str>,
    approval_id: &str,
) -> Value {
    json!([
        mattermost_action(
            callback_url,
            callback_token,
            "Once",
            format!("/approve {approval_id} once")
        ),
        mattermost_action(
            callback_url,
            callback_token,
            "Session",
            format!("/approve {approval_id} session")
        ),
        mattermost_action(
            callback_url,
            callback_token,
            "Always",
            format!("/approve {approval_id} always")
        ),
        mattermost_action(
            callback_url,
            callback_token,
            "Deny",
            format!("/deny {approval_id}")
        ),
    ])
}

fn mattermost_action(
    callback_url: &str,
    callback_token: Option<&str>,
    name: &str,
    command: String,
) -> Value {
    json!({
        "name": name,
        "integration": {
            "url": format!("{}/mattermost/actions", callback_url.trim_end_matches('/')),
            "context": {
                "command": command,
                "token": callback_token.unwrap_or_default(),
            }
        }
    })
}

fn read_json_message(socket: &mut ChannelWebSocket) -> Result<Option<Value>> {
    read_ws_json_message(
        socket,
        "mattermost websocket read failed",
        "mattermost websocket returned invalid JSON",
    )
}

fn send_json_message(socket: &mut ChannelWebSocket, value: &Value) -> Result<()> {
    send_ws_json_message(socket, value, "mattermost websocket send failed")
}

fn mattermost_mentions_bot(text: &str, identity: &MattermostBotIdentity) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains(&format!("@{}", identity.username.to_ascii_lowercase()))
        || lower.contains(&format!("@{}", identity.user_id.to_ascii_lowercase()))
}

fn strip_mattermost_bot_mention(text: &str, identity: &MattermostBotIdentity) -> String {
    let mut text = text.to_string();
    for pattern in [
        format!("@{}", identity.username),
        format!("@{}", identity.user_id),
    ] {
        let regex = regex::RegexBuilder::new(&regex::escape(&pattern))
            .case_insensitive(true)
            .build()
            .expect("valid mattermost mention regex");
        text = regex.replace_all(&text, "").to_string();
    }
    text.trim().to_string()
}

fn mattermost_chat_type(channel_type: &str) -> &'static str {
    match channel_type {
        "D" => "dm",
        "G" => "group",
        "P" => "group",
        _ => "channel",
    }
}

fn mattermost_identity_from_extra(
    extra: &std::collections::BTreeMap<String, String>,
) -> Option<MattermostBotIdentity> {
    let user_id = extra
        .get("bot_user_id")
        .map(String::as_str)
        .unwrap_or_default()
        .trim();
    let username = extra
        .get("bot_name")
        .or_else(|| extra.get("bot_username"))
        .map(String::as_str)
        .unwrap_or_default()
        .trim();
    if user_id.is_empty() || username.is_empty() {
        return None;
    }
    Some(MattermostBotIdentity {
        user_id: user_id.to_string(),
        username: username.to_string(),
    })
}

fn format_mattermost_message(text: &str) -> String {
    let re = regex::Regex::new(r"!\[[^\]]*\]\(([^)]+)\)").expect("valid mattermost image regex");
    re.replace_all(text, "$1").to_string()
}

fn mattermost_text_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.len() + ch.len_utf8() > MATTERMOST_TEXT_LIMIT && !current.is_empty() {
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

fn identity_allowed(allowed: &[String], id: &str) -> bool {
    allowed.is_empty()
        || allowed
            .iter()
            .any(|allowed| allowed.trim() == "*" || allowed.trim() == id)
}

fn list_contains(values: &[String], id: &str) -> bool {
    values
        .iter()
        .any(|value| value.trim() == "*" || value.trim() == id)
}

fn csv_from_extra(extra: &std::collections::BTreeMap<String, String>, key: &str) -> Vec<String> {
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

fn mattermost_create_at_to_rfc3339(millis: Option<i64>) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(millis?).map(|value| value.to_rfc3339())
}

fn now_rfc3339_like() -> String {
    chrono::Utc::now().to_rfc3339()
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
    fn mattermost_posted_event_maps_to_inbound() -> Result<()> {
        let adapter = test_adapter()?;
        *adapter.bot_identity.lock().unwrap() = Some(MattermostBotIdentity {
            user_id: "bot".to_string(),
            username: "duckagent".to_string(),
        });
        let event = json!({
            "event": "posted",
            "data": {
                "channel_type": "O",
                "sender_name": "alice",
                "post": "{\"id\":\"p1\",\"channel_id\":\"c1\",\"user_id\":\"u1\",\"message\":\"@duckagent hello\",\"create_at\":1710000000000,\"file_ids\":[]}"
            }
        });
        let inbound = adapter.event_to_inbound(&event)?.expect("inbound");
        assert_eq!(inbound.channel, "mattermost");
        assert_eq!(inbound.conversation_id, "c1");
        assert_eq!(inbound.sender_id.as_deref(), Some("u1"));
        assert_eq!(inbound.message_id.as_deref(), Some("p1"));
        assert_eq!(inbound.text, "hello");
        Ok(())
    }

    #[test]
    fn mattermost_non_dm_requires_mention() -> Result<()> {
        let adapter = test_adapter()?;
        *adapter.bot_identity.lock().unwrap() = Some(MattermostBotIdentity {
            user_id: "bot".to_string(),
            username: "duckagent".to_string(),
        });
        let event = json!({
            "event": "posted",
            "data": {
                "channel_type": "O",
                "post": "{\"id\":\"p1\",\"channel_id\":\"c1\",\"user_id\":\"u1\",\"message\":\"hello\",\"file_ids\":[]}"
            }
        });
        assert!(adapter.event_to_inbound(&event)?.is_none());
        Ok(())
    }

    #[test]
    fn mattermost_dm_does_not_require_mention() -> Result<()> {
        let adapter = test_adapter()?;
        let event = json!({
            "event": "posted",
            "data": {
                "channel_type": "D",
                "post": "{\"id\":\"p1\",\"channel_id\":\"c1\",\"user_id\":\"u1\",\"message\":\"hello\",\"file_ids\":[]}"
            }
        });
        assert!(adapter.event_to_inbound(&event)?.is_some());
        Ok(())
    }

    #[test]
    fn mattermost_text_chunks_respect_limit() {
        let text = "x".repeat(MATTERMOST_TEXT_LIMIT + 1);
        let chunks = mattermost_text_chunks(&text);
        assert_eq!(chunks.len(), 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= MATTERMOST_TEXT_LIMIT)
        );
    }

    #[test]
    fn mattermost_image_markdown_becomes_url() {
        assert_eq!(
            format_mattermost_message("see ![cat](https://x/cat.png)"),
            "see https://x/cat.png"
        );
    }

    fn test_adapter() -> Result<MattermostAdapter> {
        MattermostAdapter::new(
            &GatewayChannelConfig {
                enabled: true,
                api_base: Some("https://mm.example.com".to_string()),
                ..Default::default()
            },
            &GatewayCredentialEntry {
                channel: "mattermost".to_string(),
                token: Some("token".to_string()),
                ..Default::default()
            },
        )
    }
}
