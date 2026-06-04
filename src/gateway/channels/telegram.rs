use super::super::{
    ChannelAdapter, ChannelCapabilities, GatewayApprovalPrompt, GatewayInboundDispatch,
    GatewayRoute, InboundAttachmentInput, InboundMessageInput, OutboundMessage,
    StreamMessageHandle, TypingEvent,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use chrono::TimeZone;
use reqwest::blocking::{Client, multipart};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::thread;
use std::time::Duration;

const DEFAULT_TELEGRAM_API_BASE: &str = "https://api.telegram.org";
const TELEGRAM_TEXT_LIMIT_UTF16: usize = 4096;

#[derive(Clone)]
pub(in crate::gateway) struct TelegramAdapter {
    bot_token: String,
    api_base: String,
    transport: String,
    allowed_users: Vec<String>,
    allowed_chats: Vec<String>,
    max_download_bytes: u64,
    client: Client,
}

impl TelegramAdapter {
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
            .ok_or_else(|| anyhow::anyhow!("telegram gateway credential requires token"))?
            .to_string();
        let api_base = config
            .api_base
            .clone()
            .unwrap_or_else(|| DEFAULT_TELEGRAM_API_BASE.to_string());
        let transport = config
            .transport
            .as_deref()
            .unwrap_or("polling")
            .trim()
            .to_ascii_lowercase();
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build Telegram HTTP client")?;
        Ok(Self {
            bot_token,
            api_base,
            transport,
            allowed_users: config.allowed_users.clone(),
            allowed_chats: config.allowed_chats.clone(),
            max_download_bytes: config.media.max_download_bytes,
            client,
        })
    }

    fn api_url(&self, method: &str) -> String {
        format!(
            "{}/bot{}/{}",
            self.api_base.trim_end_matches('/'),
            self.bot_token,
            method
        )
    }

    fn post_json<T: Serialize>(&self, method: &str, body: &T) -> Result<Value> {
        let response = self
            .client
            .post(self.api_url(method))
            .json(body)
            .send()
            .map_err(|error| {
                anyhow!(
                    "telegram {method} request failed: {}",
                    redact_telegram_token(&error.to_string(), &self.bot_token)
                )
            })?;
        let status = response.status();
        let value: Value = response
            .json()
            .with_context(|| format!("telegram {method} returned invalid JSON"))?;
        if !status.is_success() || value.get("ok") == Some(&Value::Bool(false)) {
            bail!("telegram {method} failed with status {status}: {value}");
        }
        Ok(value)
    }

    fn post_multipart(&self, method: &str, form: multipart::Form) -> Result<Value> {
        let response = self
            .client
            .post(self.api_url(method))
            .multipart(form)
            .send()
            .map_err(|error| {
                anyhow!(
                    "telegram {method} upload failed: {}",
                    redact_telegram_token(&error.to_string(), &self.bot_token)
                )
            })?;
        let status = response.status();
        let value: Value = response
            .json()
            .with_context(|| format!("telegram {method} returned invalid JSON"))?;
        if !status.is_success() || value.get("ok") == Some(&Value::Bool(false)) {
            bail!("telegram {method} failed with status {status}: {value}");
        }
        Ok(value)
    }

    fn send_text_chunk(
        &self,
        route: &GatewayRoute,
        chunk: &str,
        reply_to: Option<&str>,
        reply_markup: Option<Value>,
    ) -> Result<()> {
        self.send_text_chunk_with_response(route, chunk, reply_to, reply_markup)
            .map(|_| ())
    }

    fn send_text_chunk_with_response(
        &self,
        route: &GatewayRoute,
        chunk: &str,
        reply_to: Option<&str>,
        reply_markup: Option<Value>,
    ) -> Result<Value> {
        let mut body = json!({
            "chat_id": route.key.conversation_id,
            "text": chunk,
            "disable_web_page_preview": true,
        });
        if let Some(thread_id) = route.key.thread_id.as_deref() {
            body["message_thread_id"] = json!(thread_id);
        }
        if let Some(reply_to) = reply_to.and_then(|value| value.parse::<i64>().ok()) {
            body["reply_parameters"] = json!({ "message_id": reply_to });
        }
        if let Some(reply_markup) = reply_markup {
            body["reply_markup"] = reply_markup;
        }
        self.post_json("sendMessage", &body)
    }

    fn edit_text_message(&self, route: &GatewayRoute, message_id: &str, text: &str) -> Result<()> {
        let mut body = json!({
            "chat_id": route.key.conversation_id,
            "message_id": message_id,
            "text": text,
            "disable_web_page_preview": true,
        });
        if let Some(thread_id) = route.key.thread_id.as_deref() {
            body["message_thread_id"] = json!(thread_id);
        }
        self.post_json("editMessageText", &body)?;
        Ok(())
    }

    fn send_media_path(
        &self,
        route: &GatewayRoute,
        path: &Path,
        caption: Option<&str>,
    ) -> Result<()> {
        let (method, field) = telegram_media_method(path);
        let file = multipart::Part::file(path)
            .with_context(|| format!("failed to open media file {}", path.display()))?;
        let mut form = multipart::Form::new()
            .text("chat_id", route.key.conversation_id.clone())
            .part(field, file);
        if let Some(thread_id) = route.key.thread_id.as_deref() {
            form = form.text("message_thread_id", thread_id.to_string());
        }
        if let Some(caption) = caption.filter(|value| !value.trim().is_empty()) {
            form = form.text("caption", caption.to_string());
        }
        self.post_multipart(method, form)?;
        Ok(())
    }

    fn poll_loop(self, inbound: GatewayInboundDispatch) {
        let mut offset: Option<i64> = None;
        loop {
            match self.fetch_updates(offset) {
                Ok(updates) => {
                    for update in updates {
                        offset = Some(update.update_id + 1);
                        match self.update_to_inbound(update) {
                            Ok(Some(message)) => {
                                if let Err(error) = inbound.submit(message) {
                                    eprintln!("telegram inbound dispatch failed: {error:#}");
                                }
                            }
                            Ok(None) => {}
                            Err(error) => eprintln!(
                                "telegram update ignored: {}",
                                redact_telegram_token(&format!("{error:#}"), &self.bot_token)
                            ),
                        }
                    }
                }
                Err(error) => {
                    eprintln!(
                        "telegram polling failed: {}",
                        redact_telegram_token(&format!("{error:#}"), &self.bot_token)
                    );
                    thread::sleep(Duration::from_secs(5));
                }
            }
        }
    }

    fn fetch_updates(&self, offset: Option<i64>) -> Result<Vec<TelegramUpdate>> {
        let mut body = json!({
            "timeout": 25,
            "allowed_updates": ["message", "edited_message", "callback_query"],
        });
        if let Some(offset) = offset {
            body["offset"] = json!(offset);
        }
        let value = self.post_json("getUpdates", &body)?;
        serde_json::from_value(value["result"].clone()).context("telegram getUpdates result shape")
    }

    fn update_to_inbound(&self, update: TelegramUpdate) -> Result<Option<InboundMessageInput>> {
        if let Some(callback) = update.callback_query {
            return self.callback_to_inbound(callback);
        }
        let Some(message) = update.message.or(update.edited_message) else {
            return Ok(None);
        };
        self.message_to_inbound(message).map(Some)
    }

    fn callback_to_inbound(
        &self,
        callback: TelegramCallbackQuery,
    ) -> Result<Option<InboundMessageInput>> {
        let Some(data) = callback.data.as_deref() else {
            return Ok(None);
        };
        let Some(command) = telegram_callback_to_command(data) else {
            return Ok(None);
        };
        let Some(message) = callback.message else {
            return Ok(None);
        };
        let _ = self.post_json(
            "answerCallbackQuery",
            &json!({
                "callback_query_id": callback.id,
                "text": "Recorded",
            }),
        );
        Ok(Some(InboundMessageInput {
            channel: "telegram".to_string(),
            conversation_id: message.chat.id.to_string(),
            thread_id: message.message_thread_id.map(|value| value.to_string()),
            chat_type: telegram_chat_type(&message.chat),
            sender_id: Some(callback.from.id.to_string()),
            message_id: Some(message.message_id.to_string()),
            text: command,
            attachments: Vec::new(),
            timestamp: Some(now_rfc3339_like()),
        }))
    }

    fn message_to_inbound(&self, message: TelegramMessage) -> Result<InboundMessageInput> {
        let chat_id = message.chat.id.to_string();
        if !identity_allowed(
            &self.allowed_chats,
            &chat_id,
            message.chat.username.as_deref(),
        ) {
            bail!("chat {chat_id} is not in telegram allowed_chats");
        }
        let sender_id = message.from.as_ref().map(|from| from.id.to_string());
        if let Some(from) = message.from.as_ref() {
            if !identity_allowed(
                &self.allowed_users,
                &from.id.to_string(),
                from.username.as_deref(),
            ) {
                bail!("sender {} is not in telegram allowed_users", from.id);
            }
        }
        let mut text = message
            .text
            .clone()
            .or_else(|| message.caption.clone())
            .unwrap_or_default();
        let (attachments, skipped) = self.collect_message_attachments(&message);
        for note in skipped {
            if !text.trim().is_empty() {
                text.push('\n');
            }
            text.push_str(&note);
        }
        Ok(InboundMessageInput {
            channel: "telegram".to_string(),
            conversation_id: chat_id,
            thread_id: message.message_thread_id.map(|value| value.to_string()),
            chat_type: telegram_chat_type(&message.chat),
            sender_id,
            message_id: Some(message.message_id.to_string()),
            text,
            attachments,
            timestamp: message.date.and_then(unix_to_rfc3339),
        })
    }

    fn collect_message_attachments(
        &self,
        message: &TelegramMessage,
    ) -> (Vec<InboundAttachmentInput>, Vec<String>) {
        let mut attachments = Vec::new();
        let mut skipped = Vec::new();
        if let Some(photo) = message
            .photo
            .iter()
            .max_by_key(|photo| photo.file_size.unwrap_or(photo.width * photo.height))
        {
            self.push_downloaded_attachment(
                &mut attachments,
                &mut skipped,
                &photo.file_id,
                Some(format!("telegram-photo-{}.jpg", message.message_id)),
                Some("image/jpeg".to_string()),
            );
        }
        if let Some(document) = message.document.as_ref() {
            self.push_downloaded_attachment(
                &mut attachments,
                &mut skipped,
                &document.file_id,
                document.file_name.clone(),
                document.mime_type.clone(),
            );
        }
        if let Some(voice) = message.voice.as_ref() {
            self.push_downloaded_attachment(
                &mut attachments,
                &mut skipped,
                &voice.file_id,
                Some(format!("telegram-voice-{}.ogg", message.message_id)),
                Some(
                    voice
                        .mime_type
                        .clone()
                        .unwrap_or_else(|| "audio/ogg".to_string()),
                ),
            );
        }
        if let Some(audio) = message.audio.as_ref() {
            self.push_downloaded_attachment(
                &mut attachments,
                &mut skipped,
                &audio.file_id,
                audio.file_name.clone(),
                audio
                    .mime_type
                    .clone()
                    .or_else(|| Some("audio/mpeg".to_string())),
            );
        }
        if let Some(video) = message.video.as_ref() {
            self.push_downloaded_attachment(
                &mut attachments,
                &mut skipped,
                &video.file_id,
                video
                    .file_name
                    .clone()
                    .or_else(|| Some(format!("telegram-video-{}.mp4", message.message_id))),
                video
                    .mime_type
                    .clone()
                    .or_else(|| Some("video/mp4".to_string())),
            );
        }
        if let Some(animation) = message.animation.as_ref() {
            self.push_downloaded_attachment(
                &mut attachments,
                &mut skipped,
                &animation.file_id,
                animation
                    .file_name
                    .clone()
                    .or_else(|| Some(format!("telegram-animation-{}.mp4", message.message_id))),
                animation
                    .mime_type
                    .clone()
                    .or_else(|| Some("video/mp4".to_string())),
            );
        }
        if let Some(sticker) = message.sticker.as_ref() {
            self.push_downloaded_attachment(
                &mut attachments,
                &mut skipped,
                &sticker.file_id,
                Some(format!("telegram-sticker-{}.webp", message.message_id)),
                Some(if sticker.is_video.unwrap_or(false) {
                    "video/webm".to_string()
                } else {
                    "image/webp".to_string()
                }),
            );
        }
        (attachments, skipped)
    }

    fn push_downloaded_attachment(
        &self,
        attachments: &mut Vec<InboundAttachmentInput>,
        skipped: &mut Vec<String>,
        file_id: &str,
        filename: Option<String>,
        mime: Option<String>,
    ) {
        match self.download_attachment(file_id, filename, mime) {
            Ok(attachment) => attachments.push(attachment),
            Err(error) => skipped.push(format!(
                "[Telegram attachment skipped: file_id={file_id}, reason={error:#}]"
            )),
        }
    }

    fn download_attachment(
        &self,
        file_id: &str,
        filename: Option<String>,
        mime: Option<String>,
    ) -> Result<InboundAttachmentInput> {
        let value = self.post_json("getFile", &json!({ "file_id": file_id }))?;
        let file = value
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("telegram getFile missing result"))?;
        let file_path = file
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("telegram getFile missing file_path"))?;
        if let Some(size) = file.get("file_size").and_then(Value::as_u64) {
            if size > self.max_download_bytes {
                bail!(
                    "telegram file is {size} bytes, over max_download_bytes {}",
                    self.max_download_bytes
                );
            }
        }
        let url = format!(
            "{}/file/bot{}/{}",
            self.api_base.trim_end_matches('/'),
            self.bot_token,
            file_path
        );
        let response = self
            .client
            .get(url)
            .send()
            .with_context(|| format!("telegram download failed for file_id {file_id}"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("telegram download failed with status {status}");
        }
        if let Some(length) = response.content_length() {
            if length > self.max_download_bytes {
                bail!(
                    "telegram download is {length} bytes, over max_download_bytes {}",
                    self.max_download_bytes
                );
            }
        }
        let bytes = response
            .bytes()
            .context("telegram download returned unreadable body")?;
        if bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "telegram download is {} bytes, over max_download_bytes {}",
                bytes.len(),
                self.max_download_bytes
            );
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: filename.or_else(|| filename_from_telegram_path(file_path)),
            mime,
        })
    }
}

impl ChannelAdapter for TelegramAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        match self.transport.as_str() {
            "polling" => {
                let adapter = self.clone();
                thread::spawn(move || adapter.poll_loop(inbound));
                Ok(())
            }
            other => bail!("telegram transport `{other}` is not supported yet; use `polling`"),
        }
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let chunks = telegram_text_chunks(&message.text);
        let reply_to = message.reply_to.as_deref();
        for chunk in chunks {
            self.send_text_chunk(route, &chunk, reply_to, None)?;
        }
        for media_path in message.media_paths {
            self.send_media_path(route, Path::new(&media_path), None)?;
        }
        Ok(())
    }

    fn send_stream_start(
        &self,
        route: &GatewayRoute,
        text: &str,
    ) -> Result<Option<StreamMessageHandle>> {
        let value = self.send_text_chunk_with_response(route, text, None, None)?;
        let message_id = value["result"]["message_id"]
            .as_i64()
            .map(|value| value.to_string())
            .or_else(|| value["result"]["message_id"].as_str().map(str::to_string))
            .ok_or_else(|| anyhow!("telegram stream start did not return message_id"))?;
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
        TELEGRAM_TEXT_LIMIT_UTF16.saturating_sub(64)
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        if !event.active {
            return Ok(());
        }
        let mut body = json!({
            "chat_id": route.key.conversation_id,
            "action": "typing",
        });
        if let Some(thread_id) = route.key.thread_id.as_deref() {
            body["message_thread_id"] = json!(thread_id);
        }
        self.post_json("sendChatAction", &body)?;
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        let text = prompt.message.clone();
        let mut chunks = telegram_text_chunks(&text);
        let Some(last) = chunks.pop() else {
            return Ok(());
        };
        for chunk in chunks {
            self.send_text_chunk(route, &chunk, None, None)?;
        }
        self.send_text_chunk(
            route,
            &last,
            None,
            Some(telegram_approval_keyboard(&prompt.id)),
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

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    #[serde(default)]
    message: Option<TelegramMessage>,
    #[serde(default)]
    edited_message: Option<TelegramMessage>,
    #[serde(default)]
    callback_query: Option<TelegramCallbackQuery>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramCallbackQuery {
    id: String,
    from: TelegramUser,
    #[serde(default)]
    message: Option<TelegramMessage>,
    #[serde(default)]
    data: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramMessage {
    message_id: i64,
    #[serde(default)]
    date: Option<i64>,
    chat: TelegramChat,
    #[serde(default)]
    from: Option<TelegramUser>,
    #[serde(default)]
    message_thread_id: Option<i64>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    photo: Vec<TelegramPhotoSize>,
    #[serde(default)]
    document: Option<TelegramFileAttachment>,
    #[serde(default)]
    voice: Option<TelegramFileAttachment>,
    #[serde(default)]
    audio: Option<TelegramFileAttachment>,
    #[serde(default)]
    video: Option<TelegramFileAttachment>,
    #[serde(default)]
    animation: Option<TelegramFileAttachment>,
    #[serde(default)]
    sticker: Option<TelegramSticker>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramChat {
    id: i64,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramUser {
    id: i64,
    #[serde(default)]
    username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramPhotoSize {
    file_id: String,
    #[serde(default)]
    file_size: Option<i64>,
    #[serde(default)]
    width: i64,
    #[serde(default)]
    height: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramFileAttachment {
    file_id: String,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramSticker {
    file_id: String,
    #[serde(default)]
    is_video: Option<bool>,
}

fn telegram_approval_keyboard(id: &str) -> Value {
    json!({
        "inline_keyboard": [[
            {"text": "Once", "callback_data": format!("duckagent:approve:{id}:once")},
            {"text": "Session", "callback_data": format!("duckagent:approve:{id}:session")},
            {"text": "Always", "callback_data": format!("duckagent:approve:{id}:always")},
            {"text": "Deny", "callback_data": format!("duckagent:deny:{id}")},
        ]]
    })
}

fn telegram_callback_to_command(data: &str) -> Option<String> {
    let parts = data.split(':').collect::<Vec<_>>();
    match parts.as_slice() {
        ["duckagent", "approve", id, decision] => Some(format!("/approve {id} {decision}")),
        ["duckagent", "deny", id] => Some(format!("/deny {id}")),
        _ => None,
    }
}

fn identity_allowed(allowed: &[String], id: &str, username: Option<&str>) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let username = username.map(|value| value.trim_start_matches('@').to_ascii_lowercase());
    allowed.iter().any(|allowed| {
        let allowed = allowed.trim();
        if allowed == id {
            return true;
        }
        username
            .as_deref()
            .is_some_and(|name| allowed.trim_start_matches('@').eq_ignore_ascii_case(name))
    })
}

fn filename_from_telegram_path(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

fn unix_to_rfc3339(seconds: i64) -> Option<String> {
    chrono::Utc
        .timestamp_opt(seconds, 0)
        .single()
        .map(|value| value.to_rfc3339())
}

fn now_rfc3339_like() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn redact_telegram_token(text: &str, token: &str) -> String {
    let token = token.trim();
    if token.is_empty() {
        return text.to_string();
    }
    text.replace(token, "<telegram-token>")
}

fn telegram_chat_type(chat: &TelegramChat) -> Option<String> {
    chat.kind
        .clone()
        .or_else(|| Some(if chat.id < 0 { "group" } else { "dm" }.to_string()))
}

fn telegram_text_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut units = 0usize;
    for ch in text.chars() {
        let next_units = ch.len_utf16();
        if units + next_units > TELEGRAM_TEXT_LIMIT_UTF16 && !current.is_empty() {
            chunks.push(current);
            current = String::new();
            units = 0;
        }
        current.push(ch);
        units += next_units;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn telegram_media_method(path: &Path) -> (&'static str, &'static str) {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" | "png" | "webp" => ("sendPhoto", "photo"),
        "gif" => ("sendAnimation", "animation"),
        "mp4" | "mov" | "m4v" | "webm" => ("sendVideo", "video"),
        "mp3" | "m4a" => ("sendAudio", "audio"),
        "ogg" | "opus" => ("sendVoice", "voice"),
        _ => ("sendDocument", "document"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_text_chunks_respect_utf16_limit() {
        let text = "😀".repeat(TELEGRAM_TEXT_LIMIT_UTF16);
        let chunks = telegram_text_chunks(&text);
        assert_eq!(chunks.len(), 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.encode_utf16().count() <= TELEGRAM_TEXT_LIMIT_UTF16)
        );
    }

    #[test]
    fn telegram_media_method_routes_common_types() {
        assert_eq!(
            telegram_media_method(Path::new("/tmp/a.png")),
            ("sendPhoto", "photo")
        );
        assert_eq!(
            telegram_media_method(Path::new("/tmp/a.pdf")),
            ("sendDocument", "document")
        );
        assert_eq!(
            telegram_media_method(Path::new("/tmp/a.ogg")),
            ("sendVoice", "voice")
        );
    }

    #[test]
    fn telegram_update_maps_text_message_to_inbound() -> Result<()> {
        let adapter = test_adapter()?;
        let update: TelegramUpdate = serde_json::from_value(json!({
            "update_id": 1,
            "message": {
                "message_id": 42,
                "date": 1710000000,
                "chat": {"id": -100123, "username": "team"},
                "from": {"id": 7, "username": "alice"},
                "message_thread_id": 99,
                "text": "hello"
            }
        }))?;

        let inbound = adapter.update_to_inbound(update)?.expect("inbound");

        assert_eq!(inbound.channel, "telegram");
        assert_eq!(inbound.conversation_id, "-100123");
        assert_eq!(inbound.thread_id.as_deref(), Some("99"));
        assert_eq!(inbound.sender_id.as_deref(), Some("7"));
        assert_eq!(inbound.message_id.as_deref(), Some("42"));
        assert_eq!(inbound.text, "hello");
        Ok(())
    }

    #[test]
    fn telegram_callback_data_maps_to_approval_commands() {
        assert_eq!(
            telegram_callback_to_command("duckagent:approve:appr_123:session").as_deref(),
            Some("/approve appr_123 session")
        );
        assert_eq!(
            telegram_callback_to_command("duckagent:deny:appr_123").as_deref(),
            Some("/deny appr_123")
        );
        assert!(telegram_callback_to_command("other").is_none());
    }

    #[test]
    fn telegram_identity_allowlist_accepts_ids_and_usernames() {
        let allowed = vec!["123".to_string(), "@alice".to_string()];
        assert!(identity_allowed(&allowed, "123", None));
        assert!(identity_allowed(&allowed, "999", Some("Alice")));
        assert!(!identity_allowed(&allowed, "999", Some("bob")));
        assert!(identity_allowed(&[], "999", None));
    }

    #[test]
    fn telegram_error_redaction_hides_bot_token() {
        let token = "123456:ABC-secret";
        let text = "error sending request for url (https://api.telegram.org/bot123456:ABC-secret/getUpdates)";
        let redacted = redact_telegram_token(text, token);
        assert!(!redacted.contains(token));
        assert!(redacted.contains("bot<telegram-token>/getUpdates"));
    }

    fn test_adapter() -> Result<TelegramAdapter> {
        TelegramAdapter::new(
            &GatewayChannelConfig {
                enabled: true,
                transport: Some("polling".to_string()),
                ..Default::default()
            },
            &GatewayCredentialEntry {
                channel: "telegram".to_string(),
                token: Some("123:token".to_string()),
                ..Default::default()
            },
        )
    }
}
