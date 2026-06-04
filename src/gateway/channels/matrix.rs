use super::super::{
    ChannelAdapter, ChannelCapabilities, GatewayApprovalPrompt, GatewayInboundDispatch,
    GatewayRoute, InboundAttachmentInput, InboundMessageInput, OutboundMessage,
    StreamMessageHandle, TypingEvent,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use url::form_urlencoded;
use uuid::Uuid;

const MATRIX_TEXT_LIMIT: usize = 4_000;
const MATRIX_SYNC_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone)]
pub(in crate::gateway) struct MatrixAdapter {
    homeserver: String,
    access_token: String,
    user_id: String,
    require_mention: bool,
    free_response_rooms: Vec<String>,
    auto_thread: bool,
    auto_join_invites: bool,
    allowed_users: Vec<String>,
    allowed_chats: Vec<String>,
    max_download_bytes: u64,
    sync_token: Arc<Mutex<Option<String>>>,
    dm_rooms: Arc<Mutex<HashSet<String>>>,
    participated_threads: Arc<Mutex<HashSet<String>>>,
    client: Client,
}

impl MatrixAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let homeserver = config
            .api_base
            .clone()
            .or_else(|| credentials.extra.get("homeserver").cloned())
            .map(|value| value.trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("matrix gateway config requires api_base homeserver URL"))?;
        let access_token = credentials
            .token
            .as_deref()
            .or(credentials.api_key.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("matrix gateway credential requires access token"))?
            .to_string();
        let user_id = credentials
            .username
            .as_deref()
            .or_else(|| credentials.extra.get("user_id").map(String::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("matrix gateway credential requires user_id"))?
            .to_string();
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("failed to build Matrix HTTP client")?;
        let require_mention = extra_bool(&config.extra, "require_mention", true);
        let free_response_rooms = extra_csv(&config.extra, "free_response_rooms");
        let auto_thread = extra_bool(&config.extra, "auto_thread", true);
        let auto_join_invites = extra_bool(&config.extra, "auto_join_invites", true);
        Ok(Self {
            homeserver,
            access_token,
            user_id,
            require_mention,
            free_response_rooms,
            auto_thread,
            auto_join_invites,
            allowed_users: config.allowed_users.clone(),
            allowed_chats: config.allowed_chats.clone(),
            max_download_bytes: config.media.max_download_bytes,
            sync_token: Arc::new(Mutex::new(None)),
            dm_rooms: Arc::new(Mutex::new(HashSet::new())),
            participated_threads: Arc::new(Mutex::new(HashSet::new())),
            client,
        })
    }

    fn check_homeserver(&self) -> Result<()> {
        let response = self
            .client
            .get(self.client_url("/account/whoami"))
            .bearer_auth(&self.access_token)
            .timeout(Duration::from_secs(10))
            .send()
            .context("failed to reach Matrix homeserver")?;
        if !response.status().is_success() {
            bail!("Matrix whoami failed: {}", response.status());
        }
        Ok(())
    }

    fn sync_loop(self, inbound: GatewayInboundDispatch) {
        loop {
            if let Err(error) = self.sync_once(&inbound) {
                eprintln!("matrix sync failed: {error:#}");
                thread::sleep(Duration::from_secs(5));
            }
        }
    }

    fn sync_once(&self, inbound: &GatewayInboundDispatch) -> Result<()> {
        let since = self
            .sync_token
            .lock()
            .expect("matrix sync token mutex poisoned")
            .clone();
        let url = match since.as_deref() {
            Some(since) => format!(
                "{}?timeout={}&since={}",
                self.client_url("/sync"),
                MATRIX_SYNC_TIMEOUT_MS,
                encode_path(since)
            ),
            None => format!("{}?timeout=1000", self.client_url("/sync")),
        };
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.access_token)
            .timeout(Duration::from_millis(MATRIX_SYNC_TIMEOUT_MS + 10_000))
            .send()
            .context("Matrix sync request failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .context("Matrix sync returned invalid JSON")?;
        if !status.is_success() {
            bail!("Matrix sync failed with status {status}: {value}");
        }
        if let Some(next_batch) = value.get("next_batch").and_then(Value::as_str) {
            *self
                .sync_token
                .lock()
                .expect("matrix sync token mutex poisoned") = Some(next_batch.to_string());
        }
        self.update_direct_rooms(&value);
        self.join_pending_invites(&value);
        if since.is_some() {
            self.dispatch_sync(&value, inbound);
        }
        Ok(())
    }

    fn join_pending_invites(&self, value: &Value) {
        if !self.auto_join_invites {
            return;
        }
        let Some(invites) = value
            .get("rooms")
            .and_then(|rooms| rooms.get("invite"))
            .and_then(Value::as_object)
        else {
            return;
        };
        for room_id in invites.keys() {
            if !identity_allowed(&self.allowed_chats, room_id) {
                continue;
            }
            if let Err(error) = self.join_room(room_id) {
                eprintln!("matrix invite join failed for {room_id}: {error:#}");
            }
        }
    }

    fn join_room(&self, room_id: &str) -> Result<()> {
        let response = self
            .client
            .post(format!(
                "{}/rooms/{}/join",
                self.client_url(""),
                encode_path(room_id)
            ))
            .bearer_auth(&self.access_token)
            .json(&json!({}))
            .timeout(Duration::from_secs(30))
            .send()
            .context("Matrix room join request failed")?;
        let status = response.status();
        let value: Value = response.json().unwrap_or_else(|_| Value::Null);
        if !status.is_success() {
            bail!("Matrix room join failed with status {status}: {value}");
        }
        Ok(())
    }

    fn update_direct_rooms(&self, value: &Value) {
        let Some(events) = value
            .get("account_data")
            .and_then(|account_data| account_data.get("events"))
            .and_then(Value::as_array)
        else {
            return;
        };
        let mut next = HashSet::new();
        let mut found_direct_event = false;
        for event in events {
            if event.get("type").and_then(Value::as_str) != Some("m.direct") {
                continue;
            }
            found_direct_event = true;
            let Some(content) = event.get("content").and_then(Value::as_object) else {
                continue;
            };
            for rooms in content.values() {
                if let Some(rooms) = rooms.as_array() {
                    for room_id in rooms.iter().filter_map(Value::as_str) {
                        next.insert(room_id.to_string());
                    }
                }
            }
        }
        if found_direct_event {
            if let Ok(mut dm_rooms) = self.dm_rooms.lock() {
                *dm_rooms = next;
            }
        }
    }

    fn dispatch_sync(&self, value: &Value, inbound: &GatewayInboundDispatch) {
        let Some(joined_rooms) = value
            .get("rooms")
            .and_then(|rooms| rooms.get("join"))
            .and_then(Value::as_object)
        else {
            return;
        };
        for (room_id, room) in joined_rooms {
            let events = room
                .get("timeline")
                .and_then(|timeline| timeline.get("events"))
                .and_then(Value::as_array);
            let Some(events) = events else {
                continue;
            };
            for event in events {
                match self.event_to_inbound(room_id, room, event) {
                    Ok(Some(input)) => {
                        if let Err(error) = inbound.submit(input) {
                            eprintln!("matrix inbound dispatch failed: {error:#}");
                        }
                    }
                    Ok(None) => {}
                    Err(error) => eprintln!("matrix event ignored: {error:#}"),
                }
            }
        }
    }

    fn event_to_inbound(
        &self,
        room_id: &str,
        room: &Value,
        event: &Value,
    ) -> Result<Option<InboundMessageInput>> {
        if event.get("type").and_then(Value::as_str) != Some("m.room.message") {
            return Ok(None);
        }
        let sender = value_str(event, "sender").unwrap_or_default();
        if sender.is_empty() || sender == self.user_id {
            return Ok(None);
        }
        if !identity_allowed(&self.allowed_users, &sender) {
            return Ok(None);
        }
        if !identity_allowed(&self.allowed_chats, room_id) {
            return Ok(None);
        }
        let Some(content) = event.get("content") else {
            return Ok(None);
        };
        let msgtype = content
            .get("msgtype")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if msgtype == "m.notice" || matrix_is_edit(content) {
            return Ok(None);
        }
        let mut text = value_str(content, "body").unwrap_or_default();
        if matrix_has_reply_fallback(content) {
            text = strip_matrix_reply_fallback(&text);
        }
        if msgtype == "m.image" && looks_like_matrix_image_filename(&text) {
            text.clear();
        }
        let mut attachments = Vec::new();
        if matches!(msgtype, "m.image" | "m.file" | "m.audio" | "m.video") {
            if let Some(url) = content.get("url").and_then(Value::as_str) {
                match self.download_mxc(url, content) {
                    Ok(attachment) => attachments.push(attachment),
                    Err(error) => {
                        if !text.trim().is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&format!(
                            "[Matrix attachment skipped: url={url}, reason={error:#}]"
                        ));
                    }
                }
            }
        }
        if text.trim().is_empty() && attachments.is_empty() {
            return Ok(None);
        }
        let event_id = value_str(event, "event_id");
        let existing_thread_id = matrix_thread_id(content);
        let is_dm = self.is_dm_room(room_id, room);
        if !self.accepts_event(
            room_id,
            content,
            &text,
            is_dm,
            existing_thread_id.as_deref(),
        ) {
            return Ok(None);
        }
        text = strip_matrix_bot_mention(&text, &self.user_id);
        let thread_id = if is_dm {
            existing_thread_id
        } else {
            existing_thread_id.or_else(|| self.auto_thread.then(|| event_id.clone()).flatten())
        };
        if let Some(thread_id) = thread_id.as_deref() {
            self.remember_thread(room_id, thread_id);
        }
        Ok(Some(InboundMessageInput {
            channel: "matrix".to_string(),
            conversation_id: room_id.to_string(),
            thread_id,
            chat_type: Some(if is_dm { "dm" } else { "room" }.to_string()),
            sender_id: Some(sender),
            message_id: event_id,
            text,
            attachments,
            timestamp: event
                .get("origin_server_ts")
                .and_then(Value::as_i64)
                .and_then(matrix_millis_to_rfc3339),
        }))
    }

    fn is_dm_room(&self, room_id: &str, room: &Value) -> bool {
        if self
            .dm_rooms
            .lock()
            .map(|rooms| rooms.contains(room_id))
            .unwrap_or(false)
        {
            return true;
        }
        let joined = room
            .get("summary")
            .and_then(|summary| summary.get("m.joined_member_count"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let invited = room
            .get("summary")
            .and_then(|summary| summary.get("m.invited_member_count"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        joined > 0 && joined + invited <= 2
    }

    fn accepts_event(
        &self,
        room_id: &str,
        content: &Value,
        text: &str,
        is_dm: bool,
        existing_thread_id: Option<&str>,
    ) -> bool {
        if is_dm
            || !self.require_mention
            || identity_list_matches(&self.free_response_rooms, room_id)
        {
            return true;
        }
        if existing_thread_id.is_some_and(|thread_id| self.thread_participated(room_id, thread_id))
        {
            return true;
        }
        matrix_mentions_user(content, text, &self.user_id)
    }

    fn thread_participated(&self, room_id: &str, thread_id: &str) -> bool {
        self.participated_threads
            .lock()
            .map(|threads| threads.contains(&matrix_thread_key(room_id, thread_id)))
            .unwrap_or(false)
    }

    fn remember_thread(&self, room_id: &str, thread_id: &str) {
        if let Ok(mut threads) = self.participated_threads.lock() {
            if threads.len() > 10_000 {
                threads.clear();
            }
            threads.insert(matrix_thread_key(room_id, thread_id));
        }
    }

    fn download_mxc(&self, mxc_url: &str, content: &Value) -> Result<InboundAttachmentInput> {
        let (server, media_id) = parse_mxc(mxc_url)?;
        let response = self
            .client
            .get(self.media_download_url(&server, &media_id))
            .bearer_auth(&self.access_token)
            .timeout(Duration::from_secs(60))
            .send()
            .context("Matrix media download failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("Matrix media download failed with status {status}");
        }
        if let Some(length) = response.content_length() {
            if length > self.max_download_bytes {
                bail!(
                    "Matrix media is {length} bytes, over max_download_bytes {}",
                    self.max_download_bytes
                );
            }
        }
        let bytes = response.bytes().context("Matrix media body unreadable")?;
        if bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "Matrix media is {} bytes, over max_download_bytes {}",
                bytes.len(),
                self.max_download_bytes
            );
        }
        let mime = content
            .get("info")
            .and_then(|info| value_str(info, "mimetype"))
            .or_else(|| {
                Some(infer_mime_from_name(
                    content.get("body").and_then(Value::as_str).unwrap_or(""),
                ))
            });
        let filename = value_str(content, "filename")
            .or_else(|| value_str(content, "body"))
            .unwrap_or_else(|| media_id.clone());
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: Some(filename),
            mime,
        })
    }

    fn send_room_message(&self, route: &GatewayRoute, content: Value) -> Result<Value> {
        let txn = Uuid::now_v7().simple().to_string();
        let url = format!(
            "{}/rooms/{}/send/m.room.message/{}",
            self.client_url(""),
            encode_path(&route.key.conversation_id),
            txn
        );
        let response = self
            .client
            .put(url)
            .bearer_auth(&self.access_token)
            .json(&content)
            .timeout(Duration::from_secs(30))
            .send()
            .context("Matrix send request failed")?;
        let status = response.status();
        let value: Value = response.json().unwrap_or_else(|_| Value::Null);
        if !status.is_success() {
            bail!("Matrix send failed with status {status}: {value}");
        }
        Ok(value)
    }

    fn send_text_chunks(&self, route: &GatewayRoute, text: &str) -> Result<bool> {
        let mut sent = false;
        for chunk in matrix_text_chunks(text) {
            let mut content = matrix_text_content(&chunk);
            apply_thread(&mut content, route.key.thread_id.as_deref());
            self.send_room_message(route, content)?;
            sent = true;
        }
        Ok(sent)
    }

    fn send_text_message(&self, route: &GatewayRoute, text: &str) -> Result<Value> {
        let mut content = matrix_text_content(text);
        apply_thread(&mut content, route.key.thread_id.as_deref());
        self.send_room_message(route, content)
    }

    fn edit_text_message(&self, route: &GatewayRoute, event_id: &str, text: &str) -> Result<()> {
        let mut content = json!({
            "msgtype": "m.text",
            "body": format!("* {text}"),
            "m.new_content": {
                "msgtype": "m.text",
                "body": text,
                "m.mentions": matrix_outbound_mentions(text),
            },
            "m.relates_to": {
                "rel_type": "m.replace",
                "event_id": event_id,
            },
        });
        apply_thread(&mut content, route.key.thread_id.as_deref());
        self.send_room_message(route, content)?;
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
            self.send_text_chunks(route, &text)?;
            return Ok(());
        }
        let path = Path::new(path);
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read Matrix upload file {}", path.display()))?;
        if bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "Matrix outbound media is {} bytes, over max_download_bytes {}",
                bytes.len(),
                self.max_download_bytes
            );
        }
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("duckagent-upload")
            .to_string();
        let mime = infer_mime_from_name(&filename);
        let upload_response = self
            .client
            .post(format!(
                "{}/_matrix/media/v3/upload?filename={}",
                self.homeserver,
                encode_path(&filename)
            ))
            .bearer_auth(&self.access_token)
            .header("Content-Type", mime.as_str())
            .body(bytes.clone())
            .timeout(Duration::from_secs(60))
            .send()
            .context("Matrix media upload failed")?;
        let status = upload_response.status();
        let upload: Value = upload_response
            .json()
            .context("Matrix upload returned invalid JSON")?;
        if !status.is_success() {
            bail!("Matrix media upload failed with status {status}: {upload}");
        }
        let content_uri = upload
            .get("content_uri")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Matrix upload missing content_uri"))?;
        let mut content = json!({
            "msgtype": matrix_msgtype_for_mime(&mime),
            "body": caption.filter(|value| !value.trim().is_empty()).unwrap_or(&filename),
            "filename": filename,
            "url": content_uri,
            "info": {
                "mimetype": mime,
                "size": bytes.len(),
            }
        });
        apply_thread(&mut content, route.key.thread_id.as_deref());
        self.send_room_message(route, content)?;
        Ok(())
    }

    fn client_url(&self, path: &str) -> String {
        format!(
            "{}/_matrix/client/v3{}",
            self.homeserver,
            if path.starts_with('/') {
                path.to_string()
            } else if path.is_empty() {
                String::new()
            } else {
                format!("/{path}")
            }
        )
    }

    fn media_download_url(&self, server: &str, media_id: &str) -> String {
        format!(
            "{}/_matrix/client/v1/media/download/{}/{}",
            self.homeserver,
            encode_path(server),
            encode_path(media_id)
        )
    }
}

impl ChannelAdapter for MatrixAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        self.check_homeserver()?;
        let adapter = self.clone();
        thread::spawn(move || adapter.sync_loop(inbound));
        Ok(())
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let text_sent = self.send_text_chunks(route, &message.text)?;
        let mut caption = (!text_sent)
            .then_some(message.text.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        for media_path in message.media_paths {
            self.send_media_path(route, &media_path, caption.as_deref())?;
            caption = None;
        }
        Ok(())
    }

    fn send_stream_start(
        &self,
        route: &GatewayRoute,
        text: &str,
    ) -> Result<Option<StreamMessageHandle>> {
        let value = self.send_text_message(route, text)?;
        let message_id = value["event_id"]
            .as_str()
            .ok_or_else(|| anyhow!("Matrix stream start did not return event_id"))?
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
        MATRIX_TEXT_LIMIT
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        let url = format!(
            "{}/rooms/{}/typing/{}",
            self.client_url(""),
            encode_path(&route.key.conversation_id),
            encode_path(&self.user_id)
        );
        let body = if event.active {
            json!({"typing": true, "timeout": 5000})
        } else {
            json!({"typing": false, "timeout": 0})
        };
        let response = self
            .client
            .put(url)
            .bearer_auth(&self.access_token)
            .json(&body)
            .timeout(Duration::from_secs(10))
            .send()
            .context("Matrix typing request failed")?;
        if !response.status().is_success() {
            bail!("Matrix typing failed with status {}", response.status());
        }
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        self.send_text_chunks(
            route,
            &format!(
                "{}\n\nCommands:\n/approve {} once\n/approve {} session\n/approve {} always\n/deny {}",
                prompt.message, prompt.id, prompt.id, prompt.id, prompt.id
            ),
        )?;
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

fn apply_thread(content: &mut Value, thread_id: Option<&str>) {
    if let Some(thread_id) = thread_id.filter(|value| !value.trim().is_empty()) {
        content["m.relates_to"] = json!({
            "rel_type": "m.thread",
            "event_id": thread_id,
            "is_falling_back": false,
        });
    }
}

fn matrix_text_content(text: &str) -> Value {
    let mut content = json!({
        "msgtype": "m.text",
        "body": text,
        "m.mentions": matrix_outbound_mentions(text),
    });
    let html = matrix_plain_markdown_to_html(text);
    if html != html_escape(text) {
        content["format"] = json!("org.matrix.custom.html");
        content["formatted_body"] = json!(html);
    }
    content
}

fn matrix_outbound_mentions(text: &str) -> Value {
    let mut users = Vec::new();
    let mut seen = HashSet::new();
    for token in text.split_whitespace() {
        let token = token.trim_matches(|ch: char| ",;:)([]\"'".contains(ch));
        if token.starts_with('@')
            && token.contains(':')
            && token.len() > 3
            && seen.insert(token.to_string())
        {
            users.push(Value::String(token.to_string()));
        }
    }
    json!({"user_ids": users})
}

fn matrix_plain_markdown_to_html(text: &str) -> String {
    text.lines()
        .map(|line| {
            let escaped = html_escape(line);
            if let Some(stripped) = escaped.strip_prefix("### ") {
                format!("<strong>{stripped}</strong>")
            } else if let Some(stripped) = escaped.strip_prefix("## ") {
                format!("<strong>{stripped}</strong>")
            } else if let Some(stripped) = escaped.strip_prefix("# ") {
                format!("<strong>{stripped}</strong>")
            } else if let Some(stripped) = escaped.strip_prefix("&gt; ") {
                format!("<blockquote>{stripped}</blockquote>")
            } else {
                escaped
            }
        })
        .collect::<Vec<_>>()
        .join("<br>")
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn matrix_thread_id(content: &Value) -> Option<String> {
    let relates = content.get("m.relates_to")?;
    if relates.get("rel_type").and_then(Value::as_str) == Some("m.thread") {
        return value_str(relates, "event_id");
    }
    relates
        .get("m.in_reply_to")
        .and_then(|reply| value_str(reply, "event_id"))
}

fn matrix_is_edit(content: &Value) -> bool {
    content
        .get("m.relates_to")
        .and_then(|relates| relates.get("rel_type"))
        .and_then(Value::as_str)
        == Some("m.replace")
}

fn matrix_has_reply_fallback(content: &Value) -> bool {
    content
        .get("m.relates_to")
        .and_then(|relates| relates.get("m.in_reply_to"))
        .and_then(|reply| reply.get("event_id"))
        .and_then(Value::as_str)
        .is_some()
}

fn strip_matrix_reply_fallback(text: &str) -> String {
    if !text.starts_with("> ") && text != ">" {
        return text.to_string();
    }
    let mut stripped = Vec::new();
    let mut past_fallback = false;
    for line in text.lines() {
        if !past_fallback {
            if line.starts_with("> ") || line == ">" {
                continue;
            }
            if line.is_empty() {
                past_fallback = true;
                continue;
            }
            past_fallback = true;
        }
        stripped.push(line);
    }
    stripped.join("\n").trim().to_string()
}

fn looks_like_matrix_image_filename(text: &str) -> bool {
    let candidate = text.trim();
    if candidate.is_empty()
        || candidate.contains('\n')
        || candidate.ends_with('/')
        || candidate.contains('/')
        || candidate.contains('\\')
    {
        return false;
    }
    let lower = candidate.to_ascii_lowercase();
    [
        ".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".svg", ".heic", ".heif", ".avif",
    ]
    .iter()
    .any(|suffix| lower.ends_with(suffix))
}

fn parse_mxc(value: &str) -> Result<(String, String)> {
    let Some(rest) = value.strip_prefix("mxc://") else {
        bail!("not an mxc URI: {value}");
    };
    let Some((server, media_id)) = rest.split_once('/') else {
        bail!("invalid mxc URI: {value}");
    };
    if server.is_empty() || media_id.is_empty() {
        bail!("invalid mxc URI: {value}");
    }
    Ok((server.to_string(), media_id.to_string()))
}

fn value_str(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn identity_allowed(allowed: &[String], id: &str) -> bool {
    if allowed.is_empty() {
        return true;
    }
    allowed
        .iter()
        .any(|allowed| allowed.trim() == "*" || allowed.trim() == id)
}

fn identity_list_matches(allowed: &[String], id: &str) -> bool {
    !allowed.is_empty() && identity_allowed(allowed, id)
}

fn extra_bool(extra: &BTreeMap<String, String>, key: &str, default: bool) -> bool {
    extra
        .get(key)
        .map(|value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "false" | "0" | "no"
            )
        })
        .unwrap_or(default)
}

fn extra_csv(extra: &BTreeMap<String, String>, key: &str) -> Vec<String> {
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

fn matrix_mentions_user(content: &Value, text: &str, user_id: &str) -> bool {
    content
        .get("m.mentions")
        .and_then(|mentions| mentions.get("user_ids"))
        .and_then(Value::as_array)
        .is_some_and(|users| users.iter().any(|user| user.as_str() == Some(user_id)))
        || text.contains(user_id)
        || content
            .get("formatted_body")
            .and_then(Value::as_str)
            .is_some_and(|formatted| formatted.contains(user_id))
}

fn strip_matrix_bot_mention(text: &str, user_id: &str) -> String {
    text.replace(user_id, "").trim().to_string()
}

fn matrix_thread_key(room_id: &str, thread_id: &str) -> String {
    format!("{room_id}:{thread_id}")
}

fn matrix_millis_to_rfc3339(millis: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(millis).map(|value| value.to_rfc3339())
}

fn matrix_text_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.len() + ch.len_utf8() > MATRIX_TEXT_LIMIT && !current.is_empty() {
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

fn matrix_msgtype_for_mime(mime: &str) -> &'static str {
    if mime.starts_with("image/") {
        "m.image"
    } else if mime.starts_with("audio/") {
        "m.audio"
    } else if mime.starts_with("video/") {
        "m.video"
    } else {
        "m.file"
    }
}

fn infer_mime_from_name(filename: &str) -> String {
    match Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn encode_path(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_text_event_maps_to_inbound() -> Result<()> {
        let adapter = test_adapter()?;
        let event = json!({
            "type": "m.room.message",
            "sender": "@alice:example.org",
            "event_id": "$event1",
            "origin_server_ts": 1710000000123_i64,
            "content": {
                "msgtype": "m.text",
                "body": "hello",
                "m.mentions": {"user_ids": ["@bot:example.org"]},
                "m.relates_to": {
                    "rel_type": "m.thread",
                    "event_id": "$thread"
                }
            }
        });

        let inbound = adapter
            .event_to_inbound("!room:example.org", &json!({}), &event)?
            .expect("inbound");

        assert_eq!(inbound.channel, "matrix");
        assert_eq!(inbound.conversation_id, "!room:example.org");
        assert_eq!(inbound.thread_id.as_deref(), Some("$thread"));
        assert_eq!(inbound.sender_id.as_deref(), Some("@alice:example.org"));
        assert_eq!(inbound.message_id.as_deref(), Some("$event1"));
        assert_eq!(inbound.text, "hello");
        Ok(())
    }

    #[test]
    fn matrix_room_message_requires_mention_by_default() -> Result<()> {
        let adapter = test_adapter()?;
        let event = json!({
            "type": "m.room.message",
            "sender": "@alice:example.org",
            "event_id": "$event1",
            "content": {
                "msgtype": "m.text",
                "body": "hello"
            }
        });

        assert!(
            adapter
                .event_to_inbound("!room:example.org", &json!({}), &event)?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn matrix_mention_auto_threads_room_message() -> Result<()> {
        let adapter = test_adapter()?;
        let event = json!({
            "type": "m.room.message",
            "sender": "@alice:example.org",
            "event_id": "$event1",
            "content": {
                "msgtype": "m.text",
                "body": "@bot:example.org hello",
                "m.mentions": {"user_ids": ["@bot:example.org"]}
            }
        });

        let inbound = adapter
            .event_to_inbound("!room:example.org", &json!({}), &event)?
            .expect("inbound");

        assert_eq!(inbound.thread_id.as_deref(), Some("$event1"));
        assert_eq!(inbound.text, "hello");
        Ok(())
    }

    #[test]
    fn matrix_thread_reply_follows_participated_thread() -> Result<()> {
        let adapter = test_adapter()?;
        let mention = json!({
            "type": "m.room.message",
            "sender": "@alice:example.org",
            "event_id": "$root",
            "content": {
                "msgtype": "m.text",
                "body": "@bot:example.org start",
                "m.mentions": {"user_ids": ["@bot:example.org"]}
            }
        });
        assert!(
            adapter
                .event_to_inbound("!room:example.org", &json!({}), &mention)?
                .is_some()
        );

        let reply = json!({
            "type": "m.room.message",
            "sender": "@alice:example.org",
            "event_id": "$reply",
            "content": {
                "msgtype": "m.text",
                "body": "follow-up",
                "m.relates_to": {
                    "rel_type": "m.thread",
                    "event_id": "$root"
                }
            }
        });

        let inbound = adapter
            .event_to_inbound("!room:example.org", &json!({}), &reply)?
            .expect("thread reply");
        assert_eq!(inbound.thread_id.as_deref(), Some("$root"));
        assert_eq!(inbound.text, "follow-up");
        Ok(())
    }

    #[test]
    fn matrix_direct_room_does_not_require_mention() -> Result<()> {
        let adapter = test_adapter()?;
        adapter.update_direct_rooms(&json!({
            "account_data": {
                "events": [{
                    "type": "m.direct",
                    "content": {"@alice:example.org": ["!dm:example.org"]}
                }]
            }
        }));
        let event = json!({
            "type": "m.room.message",
            "sender": "@alice:example.org",
            "event_id": "$dm1",
            "content": {
                "msgtype": "m.text",
                "body": "hello"
            }
        });

        let inbound = adapter
            .event_to_inbound("!dm:example.org", &json!({}), &event)?
            .expect("dm inbound");
        assert_eq!(inbound.thread_id, None);
        assert_eq!(inbound.text, "hello");
        Ok(())
    }

    #[test]
    fn matrix_parse_mxc_uri() -> Result<()> {
        assert_eq!(
            parse_mxc("mxc://example.org/abc123")?,
            ("example.org".to_string(), "abc123".to_string())
        );
        assert!(parse_mxc("https://example.org").is_err());
        Ok(())
    }

    #[test]
    fn matrix_text_chunks_respect_limit() {
        let text = "a".repeat(MATRIX_TEXT_LIMIT + 1);
        let chunks = matrix_text_chunks(&text);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| chunk.len() <= MATRIX_TEXT_LIMIT));
    }

    fn test_adapter() -> Result<MatrixAdapter> {
        MatrixAdapter::new(
            &GatewayChannelConfig {
                enabled: true,
                api_base: Some("https://matrix.example.org".to_string()),
                ..Default::default()
            },
            &GatewayCredentialEntry {
                channel: "matrix".to_string(),
                token: Some("token".to_string()),
                username: Some("@bot:example.org".to_string()),
                ..Default::default()
            },
        )
    }
}
