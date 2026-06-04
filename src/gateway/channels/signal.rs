use super::super::{
    ChannelAdapter, ChannelCapabilities, GatewayApprovalPrompt, GatewayInboundDispatch,
    GatewayRoute, InboundAttachmentInput, InboundMessageInput, OutboundMessage, TypingEvent,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use regex::Regex;
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use url::form_urlencoded;

const DEFAULT_SIGNAL_HTTP_URL: &str = "http://127.0.0.1:8080";
const SIGNAL_TEXT_LIMIT: usize = 8_000;
const SIGNAL_SSE_RETRY_INITIAL_SECONDS: u64 = 2;
const SIGNAL_SSE_RETRY_MAX_SECONDS: u64 = 60;
const SIGNAL_HEALTH_CHECK_INTERVAL_SECONDS: u64 = 30;
const SIGNAL_HEALTH_STALE_THRESHOLD_SECONDS: i64 = 120;

#[derive(Clone)]
pub(in crate::gateway) struct SignalAdapter {
    http_url: String,
    account: String,
    allowed_users: Vec<String>,
    allowed_chats: Vec<String>,
    max_download_bytes: u64,
    recent_sent_timestamps: Arc<Mutex<HashSet<i64>>>,
    recipient_uuid_by_number: Arc<Mutex<HashMap<String, String>>>,
    recipient_number_by_uuid: Arc<Mutex<HashMap<String, String>>>,
    last_sse_activity_millis: Arc<AtomicI64>,
    typing_failures: Arc<Mutex<HashMap<String, u32>>>,
    typing_skip_until_millis: Arc<Mutex<HashMap<String, i64>>>,
    client: Client,
}

impl SignalAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let account = credentials
            .username
            .as_deref()
            .or_else(|| credentials.extra.get("account").map(String::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("signal gateway credential requires account"))?
            .to_string();
        let http_url = config
            .api_base
            .clone()
            .or_else(|| credentials.extra.get("http_url").cloned())
            .unwrap_or_else(|| DEFAULT_SIGNAL_HTTP_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("failed to build Signal HTTP client")?;
        Ok(Self {
            http_url,
            account,
            allowed_users: config.allowed_users.clone(),
            allowed_chats: config.allowed_chats.clone(),
            max_download_bytes: config.media.max_download_bytes,
            recent_sent_timestamps: Arc::new(Mutex::new(HashSet::new())),
            recipient_uuid_by_number: Arc::new(Mutex::new(HashMap::new())),
            recipient_number_by_uuid: Arc::new(Mutex::new(HashMap::new())),
            last_sse_activity_millis: Arc::new(AtomicI64::new(
                chrono::Utc::now().timestamp_millis(),
            )),
            typing_failures: Arc::new(Mutex::new(HashMap::new())),
            typing_skip_until_millis: Arc::new(Mutex::new(HashMap::new())),
            client,
        })
    }

    fn check_daemon(&self) -> Result<()> {
        let response = self
            .client
            .get(format!("{}/api/v1/check", self.http_url))
            .timeout(Duration::from_secs(10))
            .send()
            .context("failed to reach signal-cli daemon")?;
        if !response.status().is_success() {
            bail!(
                "signal-cli daemon health check failed: {}",
                response.status()
            );
        }
        Ok(())
    }

    fn sse_loop(self, inbound: GatewayInboundDispatch) {
        let mut backoff = SIGNAL_SSE_RETRY_INITIAL_SECONDS;
        loop {
            match self.consume_sse_once(inbound.clone()) {
                Ok(()) => backoff = SIGNAL_SSE_RETRY_INITIAL_SECONDS,
                Err(error) => {
                    eprintln!("signal SSE disconnected: {error:#}");
                    thread::sleep(Duration::from_secs(backoff));
                    backoff = (backoff * 2).min(SIGNAL_SSE_RETRY_MAX_SECONDS);
                }
            }
        }
    }

    fn health_monitor_loop(self) {
        loop {
            thread::sleep(Duration::from_secs(SIGNAL_HEALTH_CHECK_INTERVAL_SECONDS));
            let elapsed_millis = chrono::Utc::now().timestamp_millis()
                - self.last_sse_activity_millis.load(Ordering::Relaxed);
            if elapsed_millis < SIGNAL_HEALTH_STALE_THRESHOLD_SECONDS * 1_000 {
                continue;
            }
            match self.check_daemon() {
                Ok(()) => {
                    self.record_sse_activity();
                }
                Err(error) => {
                    eprintln!("signal SSE idle and daemon health check failed: {error:#}");
                }
            }
        }
    }

    fn record_sse_activity(&self) {
        self.last_sse_activity_millis
            .store(chrono::Utc::now().timestamp_millis(), Ordering::Relaxed);
    }

    fn consume_sse_once(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        let account = form_urlencoded::byte_serialize(self.account.as_bytes()).collect::<String>();
        let response = self
            .client
            .get(format!("{}/api/v1/events?account={account}", self.http_url))
            .header("Accept", "text/event-stream")
            .send()
            .context("failed to connect Signal SSE")?;
        if !response.status().is_success() {
            bail!("Signal SSE returned {}", response.status());
        }

        let reader = BufReader::new(response);
        let mut data_lines = Vec::new();
        for line in reader.lines() {
            let line = line.context("failed to read Signal SSE line")?;
            self.record_sse_activity();
            let line = line.trim_end_matches('\r').trim();
            if line.is_empty() {
                self.dispatch_sse_data(&data_lines, &inbound);
                data_lines.clear();
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            if let Some(data) = line.strip_prefix("data:") {
                data_lines.push(data.trim().to_string());
            }
        }
        if !data_lines.is_empty() {
            self.dispatch_sse_data(&data_lines, &inbound);
        }
        Ok(())
    }

    fn dispatch_sse_data(&self, data_lines: &[String], inbound: &GatewayInboundDispatch) {
        if data_lines.is_empty() {
            return;
        }
        let payload = data_lines.join("\n");
        match serde_json::from_str::<Value>(&payload)
            .context("failed to parse Signal SSE JSON")
            .and_then(|value| self.envelope_to_inbound(&value))
        {
            Ok(Some(message)) => {
                if let Err(error) = inbound.submit(message) {
                    eprintln!("signal inbound dispatch failed: {error:#}");
                }
            }
            Ok(None) => {}
            Err(error) => eprintln!("signal event ignored: {error:#}"),
        }
    }

    fn envelope_to_inbound(&self, raw: &Value) -> Result<Option<InboundMessageInput>> {
        let mut envelope = raw.get("envelope").unwrap_or(raw).clone();
        let mut note_to_self = false;

        if let Some(sync) = envelope.get("syncMessage").and_then(Value::as_object) {
            let sent = sync.get("sentMessage").and_then(Value::as_object);
            if let Some(sent) = sent {
                let destination = sent
                    .get("destinationNumber")
                    .and_then(Value::as_str)
                    .or_else(|| sent.get("destination").and_then(Value::as_str))
                    .map(str::to_string);
                let sent_timestamp = sent.get("timestamp").and_then(Value::as_i64);
                let group_id = sent
                    .get("groupInfo")
                    .and_then(|group| value_str_obj(group, "groupId"));
                if destination.as_deref() == Some(self.account.as_str()) || group_id.is_some() {
                    if sent_timestamp.is_some_and(|timestamp| self.recent_sent_contains(timestamp))
                    {
                        return Ok(None);
                    }
                    note_to_self = true;
                    let mut promoted = envelope
                        .as_object()
                        .cloned()
                        .ok_or_else(|| anyhow!("Signal envelope must be an object"))?;
                    promoted.insert("dataMessage".to_string(), Value::Object(sent.clone()));
                    envelope = Value::Object(promoted);
                }
            }
            if !note_to_self {
                return Ok(None);
            }
        }

        if envelope.get("storyMessage").is_some() {
            return Ok(None);
        }

        let sender = value_str(&envelope, "sourceNumber")
            .or_else(|| value_str(&envelope, "sourceUuid"))
            .or_else(|| value_str(&envelope, "source"));
        let Some(sender) = sender else {
            return Ok(None);
        };
        let sender_uuid = value_str(&envelope, "sourceUuid");
        self.remember_recipient_identifiers(sender.as_str(), sender_uuid.as_deref());
        if sender == self.account && !note_to_self {
            return Ok(None);
        }
        if !identity_allowed_any(
            &self.allowed_users,
            &[Some(sender.as_str()), sender_uuid.as_deref()],
        ) {
            return Ok(None);
        }

        let data_message = envelope
            .get("dataMessage")
            .or_else(|| envelope.get("editMessage")?.get("dataMessage"));
        let Some(data_message) = data_message else {
            return Ok(None);
        };

        let group_info = data_message.get("groupInfo");
        let group_id = group_info.and_then(|group| value_str(group, "groupId"));
        let conversation_id = group_id
            .as_ref()
            .map(|group_id| format!("group:{group_id}"))
            .unwrap_or_else(|| sender.clone());
        if let Some(group_id) = group_id.as_deref() {
            if !signal_group_allowed(&self.allowed_chats, group_id, &conversation_id) {
                return Ok(None);
            }
        }

        let mut text = value_str(data_message, "message").unwrap_or_default();
        if let Some(mentions) = data_message.get("mentions").and_then(Value::as_array) {
            text = render_signal_mentions(&text, mentions);
        }
        if let Some(quote_text) = signal_quote_text(data_message) {
            if !quote_text.trim().is_empty() && !text.trim().is_empty() {
                text = format!(
                    "[Quoted Signal message]\n{}\n\n{}",
                    truncate_text(&quote_text, 800),
                    text
                );
            }
        }

        let attachments = self.collect_attachments(data_message);
        let (attachments, skipped) = attachments?;
        for note in skipped {
            if !text.trim().is_empty() {
                text.push('\n');
            }
            text.push_str(&note);
        }
        if text.trim().is_empty() && attachments.is_empty() {
            return Ok(None);
        }

        let timestamp = envelope
            .get("timestamp")
            .and_then(Value::as_i64)
            .and_then(signal_millis_to_rfc3339);
        let message_id = envelope
            .get("timestamp")
            .and_then(Value::as_i64)
            .map(|value| value.to_string());

        Ok(Some(InboundMessageInput {
            channel: "signal".to_string(),
            conversation_id,
            thread_id: None,
            chat_type: Some(if group_id.is_some() { "group" } else { "dm" }.to_string()),
            sender_id: Some(sender),
            message_id,
            text,
            attachments,
            timestamp,
        }))
    }

    fn collect_attachments(
        &self,
        data_message: &Value,
    ) -> Result<(Vec<InboundAttachmentInput>, Vec<String>)> {
        let mut attachments = Vec::new();
        let mut skipped = Vec::new();
        let Some(values) = data_message.get("attachments").and_then(Value::as_array) else {
            return Ok((attachments, skipped));
        };
        for attachment in values {
            let Some(id) = value_str(attachment, "id") else {
                continue;
            };
            if let Some(size) = attachment.get("size").and_then(Value::as_u64) {
                if size > self.max_download_bytes {
                    skipped.push(format!(
                        "[Signal attachment skipped: id={id}, reason=file is {size} bytes, over max_download_bytes {}]",
                        self.max_download_bytes
                    ));
                    continue;
                }
            }
            match self.fetch_attachment(&id, attachment) {
                Ok(attachment) => attachments.push(attachment),
                Err(error) => skipped.push(format!(
                    "[Signal attachment skipped: id={id}, reason={error:#}]"
                )),
            }
        }
        Ok((attachments, skipped))
    }

    fn fetch_attachment(
        &self,
        attachment_id: &str,
        attachment: &Value,
    ) -> Result<InboundAttachmentInput> {
        let result = self.rpc(
            "getAttachment",
            json!({
                "account": self.account,
                "id": attachment_id,
            }),
        )?;
        let data = result
            .get("data")
            .and_then(Value::as_str)
            .or_else(|| result.as_str())
            .ok_or_else(|| anyhow!("Signal getAttachment returned no base64 data"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("Signal attachment is not valid base64")?;
        if bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "Signal attachment is {} bytes, over max_download_bytes {}",
                bytes.len(),
                self.max_download_bytes
            );
        }
        let mime = value_str(attachment, "contentType").unwrap_or_else(|| infer_mime(&bytes));
        let filename = value_str(attachment, "filename")
            .or_else(|| value_str(attachment, "name"))
            .unwrap_or_else(|| format!("signal-{attachment_id}{}", guess_extension(&bytes)));
        Ok(InboundAttachmentInput {
            bytes: Some(bytes),
            path: None,
            filename: Some(filename),
            mime: Some(mime),
        })
    }

    fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let response = self
            .client
            .post(format!("{}/api/v1/rpc", self.http_url))
            .timeout(Duration::from_secs(30))
            .json(&json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
                "id": format!("{method}_{}", chrono::Utc::now().timestamp_millis()),
            }))
            .send()
            .with_context(|| format!("Signal RPC {method} request failed"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .unwrap_or_else(|error| format!("<failed to read response body: {error}>"));
            bail!("Signal RPC {method} failed with status {status}: {body}");
        }
        let body = response
            .text()
            .with_context(|| format!("Signal RPC {method} response body unreadable"))?;
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        let value: Value = serde_json::from_str(&body)
            .with_context(|| format!("Signal RPC {method} returned invalid JSON"))?;
        if value.is_null() {
            return Ok(Value::Null);
        }
        if let Some(error) = value.get("error") {
            bail!("Signal RPC {method} returned error: {error}");
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    fn recent_sent_contains(&self, timestamp: i64) -> bool {
        self.recent_sent_timestamps
            .lock()
            .map(|mut timestamps| timestamps.remove(&timestamp))
            .unwrap_or(false)
    }

    fn remember_sent_timestamps(&self, value: &Value) {
        let mut timestamps = Vec::new();
        collect_signal_timestamps(value, &mut timestamps);
        if timestamps.is_empty() {
            return;
        }
        if let Ok(mut recent) = self.recent_sent_timestamps.lock() {
            for timestamp in timestamps {
                recent.insert(timestamp);
            }
            if recent.len() > 100 {
                let keep = recent.iter().copied().take(50).collect::<HashSet<_>>();
                *recent = keep;
            }
        }
    }

    fn remember_recipient_identifiers(&self, number: &str, service_id: Option<&str>) {
        let Some(service_id) = service_id
            .map(str::trim)
            .filter(|value| is_signal_service_id(value))
        else {
            return;
        };
        if !looks_like_e164_number(number) {
            return;
        }
        if let Ok(mut by_number) = self.recipient_uuid_by_number.lock() {
            by_number.insert(number.to_string(), service_id.to_string());
        }
        if let Ok(mut by_uuid) = self.recipient_number_by_uuid.lock() {
            by_uuid.insert(service_id.to_string(), number.to_string());
        }
    }

    fn resolve_recipient(&self, recipient: &str) -> String {
        if recipient.starts_with("group:")
            || is_signal_service_id(recipient)
            || !looks_like_e164_number(recipient)
        {
            return recipient.to_string();
        }
        if let Ok(by_number) = self.recipient_uuid_by_number.lock() {
            if let Some(service_id) = by_number.get(recipient) {
                return service_id.clone();
            }
        }
        if let Ok(Value::Array(contacts)) = self.rpc(
            "listContacts",
            json!({
                "account": self.account,
                "allRecipients": true,
            }),
        ) {
            for contact in contacts {
                let Some(number) = value_str(&contact, "number") else {
                    continue;
                };
                let service_id = signal_contact_service_id(&contact);
                self.remember_recipient_identifiers(&number, service_id.as_deref());
            }
        }
        self.recipient_uuid_by_number
            .lock()
            .ok()
            .and_then(|by_number| by_number.get(recipient).cloned())
            .unwrap_or_else(|| recipient.to_string())
    }

    fn send_signal(
        &self,
        route: &GatewayRoute,
        message: &str,
        attachments: &[String],
    ) -> Result<()> {
        let message = signal_plain_text(message);
        let mut params = json!({
            "account": self.account,
            "message": message,
        });
        if let Some(group_id) = route.key.conversation_id.strip_prefix("group:") {
            params["groupId"] = json!(group_id);
        } else {
            params["recipient"] = json!([self.resolve_recipient(&route.key.conversation_id)]);
        }
        if !attachments.is_empty() {
            params["attachments"] = json!(attachments);
        }
        let result = self.rpc("send", params)?;
        self.remember_sent_timestamps(&result);
        Ok(())
    }

    fn send_text_chunks(&self, route: &GatewayRoute, text: &str) -> Result<bool> {
        let chunks = signal_text_chunks(text);
        let mut sent = false;
        for chunk in chunks {
            self.send_signal(route, &chunk, &[])?;
            sent = true;
        }
        Ok(sent)
    }
}

impl ChannelAdapter for SignalAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        self.check_daemon()?;
        let adapter = self.clone();
        thread::spawn(move || adapter.sse_loop(inbound));
        let monitor = self.clone();
        thread::spawn(move || monitor.health_monitor_loop());
        Ok(())
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let text_sent = self.send_text_chunks(route, &message.text)?;
        let mut caption = (!text_sent)
            .then_some(message.text.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        for media_path in message.media_paths {
            if media_path.starts_with("http://") || media_path.starts_with("https://") {
                let link_text = caption
                    .take()
                    .map(|text| format!("{text}\n{media_path}"))
                    .unwrap_or(media_path);
                self.send_text_chunks(route, &link_text)?;
                continue;
            }
            if !Path::new(&media_path).exists() {
                bail!("Signal outbound media path does not exist: {media_path}");
            }
            self.send_signal(
                route,
                caption.take().as_deref().unwrap_or(""),
                &[media_path],
            )?;
        }
        Ok(())
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        if !event.active {
            return Ok(());
        }
        let chat_key = route.key.conversation_id.clone();
        if self.typing_backoff_active(&chat_key) {
            return Ok(());
        }
        let mut params = json!({
            "account": self.account,
        });
        if let Some(group_id) = route.key.conversation_id.strip_prefix("group:") {
            params["groupId"] = json!(group_id);
        } else {
            params["recipient"] = json!([self.resolve_recipient(&route.key.conversation_id)]);
        }
        let result = self.rpc("sendTyping", params);
        self.record_typing_result(&chat_key, result.is_ok());
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

impl SignalAdapter {
    fn typing_backoff_active(&self, chat_key: &str) -> bool {
        let now = chrono::Utc::now().timestamp_millis();
        self.typing_skip_until_millis
            .lock()
            .map(|skips| skips.get(chat_key).is_some_and(|until| *until > now))
            .unwrap_or(false)
    }

    fn record_typing_result(&self, chat_key: &str, success: bool) {
        if success {
            if let Ok(mut failures) = self.typing_failures.lock() {
                failures.remove(chat_key);
            }
            if let Ok(mut skips) = self.typing_skip_until_millis.lock() {
                skips.remove(chat_key);
            }
            return;
        }
        let mut failure_count = 0;
        if let Ok(mut failures) = self.typing_failures.lock() {
            failure_count = failures
                .entry(chat_key.to_string())
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1)
                .to_owned();
        }
        if failure_count >= 3 {
            let exponent = failure_count.saturating_sub(3).min(3);
            let backoff_millis = (16_000_i64 * 2_i64.pow(exponent)).min(60_000);
            if let Ok(mut skips) = self.typing_skip_until_millis.lock() {
                skips.insert(
                    chat_key.to_string(),
                    chrono::Utc::now().timestamp_millis() + backoff_millis,
                );
            }
        }
    }
}

fn value_str(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn value_str_obj(value: &Value, key: &str) -> Option<String> {
    value
        .as_object()
        .and_then(|object| object.get(key))
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

fn identity_allowed_any(allowed: &[String], ids: &[Option<&str>]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    ids.iter().flatten().any(|id| identity_allowed(allowed, id))
}

fn signal_group_allowed(allowed_chats: &[String], group_id: &str, conversation_id: &str) -> bool {
    if allowed_chats.is_empty() {
        return false;
    }
    identity_allowed(allowed_chats, "*")
        || identity_allowed(allowed_chats, group_id)
        || identity_allowed(allowed_chats, conversation_id)
}

fn looks_like_e164_number(value: &str) -> bool {
    let Some(digits) = value.strip_prefix('+') else {
        return false;
    };
    (7..=15).contains(&digits.len()) && digits.chars().all(|ch| ch.is_ascii_digit())
}

fn is_signal_service_id(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    value.starts_with("PNI:") || value.starts_with("u:") || looks_like_uuid(value)
}

fn looks_like_uuid(value: &str) -> bool {
    let parts = value.split('-').collect::<Vec<_>>();
    if parts.len() != 5 {
        return false;
    }
    let expected = [8, 4, 4, 4, 12];
    parts
        .iter()
        .zip(expected.iter())
        .all(|(part, len)| part.len() == *len && part.chars().all(|ch| ch.is_ascii_hexdigit()))
}

fn signal_contact_service_id(contact: &Value) -> Option<String> {
    for key in [
        "uuid",
        "aci",
        "pni",
        "serviceId",
        "service_id",
        "recipientId",
    ] {
        if let Some(value) = value_str(contact, key).filter(|value| is_signal_service_id(value)) {
            return Some(value);
        }
        if let Some(value) = contact
            .get(key)
            .and_then(|value| value.get("uuid"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| is_signal_service_id(value))
        {
            return Some(value.to_string());
        }
    }
    None
}

fn signal_quote_text(data_message: &Value) -> Option<String> {
    let quote = data_message.get("quote")?;
    value_str(quote, "text")
        .or_else(|| value_str(quote, "message"))
        .or_else(|| {
            quote
                .get("dataMessage")
                .and_then(|message| value_str(message, "message"))
        })
}

fn collect_signal_timestamps(value: &Value, timestamps: &mut Vec<i64>) {
    match value {
        Value::Object(object) => {
            for key in ["timestamp", "timestampMs", "timestamp_ms"] {
                if let Some(timestamp) = object.get(key).and_then(Value::as_i64) {
                    timestamps.push(timestamp);
                }
            }
            for value in object.values() {
                collect_signal_timestamps(value, timestamps);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_signal_timestamps(value, timestamps);
            }
        }
        _ => {}
    }
}

fn render_signal_mentions(text: &str, mentions: &[Value]) -> String {
    if mentions.is_empty() || !text.contains('\u{fffc}') {
        return text.to_string();
    }
    let mut out = text.to_string();
    let mut replacements = mentions
        .iter()
        .filter_map(|mention| {
            let start = mention.get("start")?.as_u64()? as usize;
            let length = mention.get("length").and_then(Value::as_u64).unwrap_or(1) as usize;
            let label = value_str(mention, "number")
                .or_else(|| value_str(mention, "uuid"))
                .unwrap_or_else(|| "user".to_string());
            Some((start, length, format!("@{label}")))
        })
        .collect::<Vec<_>>();
    replacements.sort_by(|left, right| right.0.cmp(&left.0));
    for (char_start, char_length, replacement) in replacements {
        let byte_start = char_to_byte_index(&out, char_start);
        let byte_end = char_to_byte_index(&out, char_start.saturating_add(char_length));
        if byte_start <= out.len() && byte_end <= out.len() && byte_start <= byte_end {
            out.replace_range(byte_start..byte_end, &replacement);
        }
    }
    out
}

fn char_to_byte_index(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map(|(idx, _)| idx)
        .unwrap_or(value.len())
}

fn signal_millis_to_rfc3339(millis: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(millis).map(|value| value.to_rfc3339())
}

fn signal_text_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.len() + ch.len_utf8() > SIGNAL_TEXT_LIMIT && !current.is_empty() {
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

fn signal_plain_text(text: &str) -> String {
    let mut out = text.trim().to_string();
    if out.is_empty() {
        return out;
    }
    out = Regex::new(r"\n{3,}")
        .expect("valid Signal whitespace regex")
        .replace_all(&out, "\n\n")
        .to_string();
    out = Regex::new(r"(?s)```[a-zA-Z0-9_+-]*\n?(.*?)```")
        .expect("valid Signal code fence regex")
        .replace_all(&out, "$1")
        .to_string();
    out = Regex::new(r"(?m)^#{1,6}\s+")
        .expect("valid Signal heading regex")
        .replace_all(&out, "")
        .to_string();
    out = Regex::new(r"!\[([^\]]*)\]\(([^)\s]+)\)")
        .expect("valid Signal image link regex")
        .replace_all(&out, "$1 ($2)")
        .to_string();
    out = Regex::new(r"\[([^\]]+)\]\(([^)\s]+)\)")
        .expect("valid Signal markdown link regex")
        .replace_all(&out, "$1 ($2)")
        .to_string();
    for pattern in [r"\*\*(.+?)\*\*", r"__(.+?)__", r"~~(.+?)~~", r"`(.+?)`"] {
        out = Regex::new(pattern)
            .expect("valid Signal markdown regex")
            .replace_all(&out, "$1")
            .to_string();
    }
    out = strip_signal_marker_pairs(&out, '*');
    out = strip_signal_marker_pairs(&out, '_');
    out.trim().to_string()
}

fn strip_signal_marker_pairs(text: &str, marker: char) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    let mut remove = vec![false; chars.len()];
    let mut open: Option<usize> = None;
    for idx in 0..chars.len() {
        if chars[idx] != marker {
            continue;
        }
        if idx > 0 && chars[idx - 1] == marker {
            continue;
        }
        if chars.get(idx + 1).is_some_and(|next| *next == marker) {
            continue;
        }
        if let Some(start) = open {
            if idx <= start + 1 || chars[idx - 1].is_whitespace() {
                continue;
            }
            if marker == '_'
                && chars
                    .get(idx + 1)
                    .is_some_and(|next| next.is_alphanumeric())
            {
                continue;
            }
            remove[start] = true;
            remove[idx] = true;
            open = None;
            continue;
        }
        if chars.get(idx + 1).is_none_or(|next| next.is_whitespace()) {
            continue;
        }
        if marker == '_' && idx > 0 && chars[idx - 1].is_alphanumeric() {
            continue;
        }
        open = Some(idx);
    }
    chars
        .into_iter()
        .enumerate()
        .filter_map(|(idx, ch)| (!remove[idx]).then_some(ch))
        .collect()
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out = text.chars().take(limit).collect::<String>();
    out.push_str("...");
    out
}

fn guess_extension(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG") {
        ".png"
    } else if bytes.starts_with(b"\xff\xd8") {
        ".jpg"
    } else if bytes.starts_with(b"GIF8") {
        ".gif"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        ".webp"
    } else if bytes.starts_with(b"%PDF") {
        ".pdf"
    } else if bytes.len() >= 8 && &bytes[4..8] == b"ftyp" {
        ".mp4"
    } else if bytes.starts_with(b"OggS") {
        ".ogg"
    } else if bytes.starts_with(b"PK") {
        ".zip"
    } else {
        ".bin"
    }
}

fn infer_mime(bytes: &[u8]) -> String {
    match guess_extension(bytes) {
        ".png" => "image/png",
        ".jpg" => "image/jpeg",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".pdf" => "application/pdf",
        ".mp4" => "video/mp4",
        ".ogg" => "audio/ogg",
        ".zip" => "application/zip",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_dm_envelope_maps_to_inbound() -> Result<()> {
        let adapter = test_adapter()?;
        let event = json!({
            "envelope": {
                "sourceNumber": "+15550001111",
                "sourceName": "Alice",
                "timestamp": 1710000000123_i64,
                "dataMessage": {
                    "message": "hello",
                    "attachments": []
                }
            }
        });

        let inbound = adapter.envelope_to_inbound(&event)?.expect("inbound");

        assert_eq!(inbound.channel, "signal");
        assert_eq!(inbound.conversation_id, "+15550001111");
        assert_eq!(inbound.sender_id.as_deref(), Some("+15550001111"));
        assert_eq!(inbound.message_id.as_deref(), Some("1710000000123"));
        assert_eq!(inbound.text, "hello");
        Ok(())
    }

    #[test]
    fn signal_group_envelope_uses_group_session() -> Result<()> {
        let adapter = test_adapter_with_groups(&["g1"])?;
        let event = json!({
            "envelope": {
                "sourceNumber": "+15550001111",
                "timestamp": 1710000000123_i64,
                "dataMessage": {
                    "message": "group hello",
                    "groupInfo": {"groupId": "g1", "groupName": "Ops"},
                    "attachments": []
                }
            }
        });

        let inbound = adapter.envelope_to_inbound(&event)?.expect("inbound");

        assert_eq!(inbound.conversation_id, "group:g1");
        assert_eq!(inbound.text, "group hello");
        Ok(())
    }

    #[test]
    fn signal_groups_are_disabled_by_default() -> Result<()> {
        let adapter = test_adapter()?;
        let event = json!({
            "envelope": {
                "sourceNumber": "+15550001111",
                "timestamp": 1710000000123_i64,
                "dataMessage": {
                    "message": "group hello",
                    "groupInfo": {"groupId": "g1", "groupName": "Ops"},
                    "attachments": []
                }
            }
        });

        assert!(adapter.envelope_to_inbound(&event)?.is_none());
        Ok(())
    }

    #[test]
    fn signal_allowed_users_accept_source_uuid() -> Result<()> {
        let mut config = GatewayChannelConfig {
            enabled: true,
            api_base: Some(DEFAULT_SIGNAL_HTTP_URL.to_string()),
            allowed_users: vec!["uuid-1".to_string()],
            ..Default::default()
        };
        config.media.max_download_bytes = 1024 * 1024;
        let adapter = SignalAdapter::new(
            &config,
            &GatewayCredentialEntry {
                channel: "signal".to_string(),
                username: Some("+15550000000".to_string()),
                ..Default::default()
            },
        )?;
        let event = json!({
            "envelope": {
                "sourceNumber": "+15550001111",
                "sourceUuid": "uuid-1",
                "timestamp": 1710000000123_i64,
                "dataMessage": {
                    "message": "hello",
                    "attachments": []
                }
            }
        });

        assert!(adapter.envelope_to_inbound(&event)?.is_some());
        Ok(())
    }

    #[test]
    fn signal_sync_echo_from_recent_send_is_ignored() -> Result<()> {
        let adapter = test_adapter_with_groups(&["g1"])?;
        adapter.remember_sent_timestamps(&json!({"timestamp": 1710000000123_i64}));
        let event = json!({
            "envelope": {
                "sourceNumber": "+15550000000",
                "syncMessage": {
                    "sentMessage": {
                        "timestamp": 1710000000123_i64,
                        "message": "bot echo",
                        "groupInfo": {"groupId": "g1"}
                    }
                }
            }
        });

        assert!(adapter.envelope_to_inbound(&event)?.is_none());
        Ok(())
    }

    #[test]
    fn signal_text_chunks_respect_limit() {
        let text = "a".repeat(SIGNAL_TEXT_LIMIT + 1);
        let chunks = signal_text_chunks(&text);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| chunk.len() <= SIGNAL_TEXT_LIMIT));
    }

    #[test]
    fn signal_magic_bytes_infer_extension() {
        assert_eq!(guess_extension(b"%PDF-1.7"), ".pdf");
        assert_eq!(infer_mime(b"OggSxxxx"), "audio/ogg");
    }

    fn test_adapter() -> Result<SignalAdapter> {
        SignalAdapter::new(
            &GatewayChannelConfig {
                enabled: true,
                api_base: Some(DEFAULT_SIGNAL_HTTP_URL.to_string()),
                ..Default::default()
            },
            &GatewayCredentialEntry {
                channel: "signal".to_string(),
                username: Some("+15550000000".to_string()),
                ..Default::default()
            },
        )
    }

    fn test_adapter_with_groups(groups: &[&str]) -> Result<SignalAdapter> {
        SignalAdapter::new(
            &GatewayChannelConfig {
                enabled: true,
                api_base: Some(DEFAULT_SIGNAL_HTTP_URL.to_string()),
                allowed_chats: groups.iter().map(|value| value.to_string()).collect(),
                ..Default::default()
            },
            &GatewayCredentialEntry {
                channel: "signal".to_string(),
                username: Some("+15550000000".to_string()),
                ..Default::default()
            },
        )
    }
}
