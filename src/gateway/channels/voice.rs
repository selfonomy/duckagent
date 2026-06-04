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
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TALK_VOICE_CHANNEL: &str = "talk-voice";
const DEFAULT_SEND_ENDPOINT: &str = "/send";
const DEFAULT_TYPING_ENDPOINT: &str = "/typing";
const TEXT_LIMIT: usize = 4_000;

#[derive(Clone, Copy)]
enum VoiceMode {
    VoiceCall,
    TalkVoice,
}

#[derive(Clone)]
pub(in crate::gateway) struct VoiceAdapter {
    mode: VoiceMode,
    channel: &'static str,
    platform: &'static str,
    bridge_base: Option<String>,
    token: Option<String>,
    webhook_secret: Option<String>,
    provider: Option<String>,
    allowed_sessions: HashSet<String>,
    allowed_participants: HashSet<String>,
    max_download_bytes: u64,
    send_endpoint: String,
    typing_endpoint: Option<String>,
    client: Client,
    seen_event_ids: Arc<Mutex<VecDeque<String>>>,
}

impl VoiceAdapter {
    pub(in crate::gateway) fn new(
        channel: &'static str,
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let mode = if channel == TALK_VOICE_CHANNEL {
            VoiceMode::TalkVoice
        } else {
            VoiceMode::VoiceCall
        };
        let client = Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .context("failed to build voice bridge HTTP client")?;
        Ok(Self {
            mode,
            channel,
            platform: match mode {
                VoiceMode::VoiceCall => "voice_call",
                VoiceMode::TalkVoice => "talk_voice",
            },
            bridge_base: config.api_base.clone(),
            token: credentials.token.clone().or(credentials.api_key.clone()),
            webhook_secret: credentials
                .webhook_secret
                .clone()
                .or_else(|| credentials.signing_secret.clone()),
            provider: config
                .extra
                .get("provider")
                .cloned()
                .or_else(|| credentials.extra.get("provider").cloned()),
            allowed_sessions: config
                .allowed_chats
                .iter()
                .map(|value| normalize_id(value))
                .collect(),
            allowed_participants: config
                .allowed_users
                .iter()
                .map(|value| normalize_id(value))
                .collect(),
            max_download_bytes: config.media.max_download_bytes,
            send_endpoint: config
                .extra
                .get("send_endpoint")
                .cloned()
                .unwrap_or_else(|| DEFAULT_SEND_ENDPOINT.to_string()),
            typing_endpoint: config
                .extra
                .get("typing_endpoint")
                .cloned()
                .or_else(|| Some(DEFAULT_TYPING_ENDPOINT.to_string())),
            client,
            seen_event_ids: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    fn handle_voice_event(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        if !self.verify_webhook(&request) {
            return Ok(json_response(401, json!({"error": "unauthorized"})));
        }
        let value: Value =
            serde_json::from_slice(&request.body).context("failed to parse voice bridge JSON")?;
        let value = normalize_voice_event(value);
        let events = voice_events_from_value(&value);
        for event in events {
            let event = normalize_voice_event(event);
            if let Some(input) = self.event_to_inbound(&event)? {
                inbound.submit(input)?;
            }
        }
        Ok(json_response(200, json!({"ok": true})))
    }

    fn event_to_inbound(&self, event: &Value) -> Result<Option<InboundMessageInput>> {
        let body = voice_event_body(event);
        let conversation_id = first_voice_str(
            body,
            event,
            &[
                "conversation_id",
                "call_id",
                "session_id",
                "room_id",
                "channel_id",
                "chat_id",
                "call.id",
                "session.id",
                "room.id",
                "conversation.id",
                "meeting.id",
            ],
        )
        .or_else(|| compose_voice_session_id(body, event))
        .ok_or_else(|| anyhow!("{} event missing call/session id", self.channel))?;
        if !allowlist_matches(&self.allowed_sessions, &conversation_id) {
            return Ok(None);
        }

        let sender_id = first_voice_str(
            body,
            event,
            &[
                "sender_id",
                "participant_id",
                "speaker_id",
                "caller_id",
                "from",
                "user_id",
                "phone_number",
                "sip_uri",
                "sender.id",
                "participant.id",
                "speaker.id",
                "caller.id",
                "caller.number",
                "from.id",
                "from.number",
                "user.id",
            ],
        );
        if let Some(sender_id) = sender_id.as_deref() {
            if !allowlist_matches(&self.allowed_participants, sender_id) {
                return Ok(None);
            }
        }

        if !is_final_voice_event(body, event) {
            return Ok(None);
        }

        let message_id = first_voice_str(
            body,
            event,
            &[
                "message_id",
                "event_id",
                "id",
                "call_event_id",
                "transcript_id",
                "recording_id",
                "segment_id",
                "message.id",
                "event.id",
                "transcript.id",
                "utterance.id",
                "recording.id",
                "sequence",
                "seq",
            ],
        );
        if let Some(message_id) = message_id.as_deref() {
            if self.is_duplicate(&format!("id:{message_id}")) {
                return Ok(None);
            }
        }

        let text = voice_text(body, event);
        if let Some(fingerprint) =
            transcript_fingerprint(body, event, &conversation_id, sender_id.as_deref(), &text)
        {
            if self.is_duplicate(&fingerprint) {
                return Ok(None);
            }
        }
        let attachments = self.parse_attachments(body, event);
        if text.trim().is_empty() && attachments.is_empty() {
            return Ok(None);
        }

        Ok(Some(InboundMessageInput {
            channel: self.channel.to_string(),
            conversation_id,
            thread_id: first_voice_str(
                body,
                event,
                &[
                    "thread_id",
                    "reply_to",
                    "parent_id",
                    "message.thread_id",
                    "transcript.thread_id",
                ],
            ),
            chat_type: Some(voice_chat_type(self.mode, body)),
            sender_id,
            message_id,
            text: if text.trim().is_empty() {
                format!("[{} media]", self.channel)
            } else {
                text
            },
            attachments,
            timestamp: first_voice_str(
                body,
                event,
                &[
                    "timestamp",
                    "created_at",
                    "createdAt",
                    "time",
                    "event_time",
                    "transcript.start_time",
                ],
            ),
        }))
    }

    fn parse_attachments(&self, body: &Value, event: &Value) -> Vec<InboundAttachmentInput> {
        let mut out = Vec::new();
        let mut seen_urls = HashSet::new();
        for source in [body, event] {
            for attachment in attachment_values(source) {
                if let Some(input) = attachment_from_value(attachment) {
                    out.push(input);
                    continue;
                }
                if let Some(url) = attachment
                    .as_str()
                    .or_else(|| {
                        first_str(
                            attachment,
                            &[
                                "url",
                                "download_url",
                                "media_url",
                                "audio_url",
                                "recording_url",
                                "file_url",
                                "voice_url",
                                "playback_url",
                            ],
                        )
                    })
                    .filter(|url| seen_urls.insert((*url).to_string()))
                {
                    match self.download_attachment(url, attachment) {
                        Ok(input) => out.push(input),
                        Err(error) => eprintln!("{} attachment skipped: {error:#}", self.channel),
                    }
                }
            }
            for key in [
                "recording_url",
                "audio_url",
                "media_url",
                "download_url",
                "file_url",
                "voice_url",
                "playback_url",
            ] {
                if let Some(url) = source[key]
                    .as_str()
                    .filter(|url| seen_urls.insert((*url).to_string()))
                {
                    match self.download_attachment(url, source) {
                        Ok(input) => out.push(input),
                        Err(error) => eprintln!("{} media skipped: {error:#}", self.channel),
                    }
                }
            }
        }
        out
    }

    fn download_attachment(&self, url: &str, attachment: &Value) -> Result<InboundAttachmentInput> {
        let mut request = self.client.get(url);
        if let Some(token) = self.token.as_deref() {
            request = request.bearer_auth(token);
        }
        let response = request.send().context("voice attachment download failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("voice attachment download failed with status {status}");
        }
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).to_string())
            .or_else(|| {
                first_str(attachment, &["mime", "mime_type", "content_type"]).map(str::to_string)
            })
            .or_else(|| Some("audio/mpeg".to_string()));
        let bytes = response
            .bytes()
            .context("voice attachment body unreadable")?;
        if self.max_download_bytes > 0 && bytes.len() as u64 > self.max_download_bytes {
            bail!(
                "{} attachment exceeds max_download_bytes ({})",
                self.channel,
                self.max_download_bytes
            );
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: first_str(attachment, &["filename", "name", "file_name"])
                .map(str::to_string)
                .or_else(|| Some(format!("{}-media.bin", self.channel))),
            mime,
        })
    }

    fn verify_webhook(&self, request: &ChannelHttpRequest) -> bool {
        let Some(secret) = self.webhook_secret.as_deref() else {
            return true;
        };
        let channel_header = if self.channel == TALK_VOICE_CHANNEL {
            "x-talk-voice-secret"
        } else {
            "x-voice-call-secret"
        };
        let candidate = request
            .header("x-duckagent-gateway-secret")
            .or_else(|| request.header(channel_header))
            .or_else(|| request.header("x-voice-secret"))
            .or_else(|| request.query.get("secret").map(String::as_str));
        candidate.is_some_and(|value| constant_time_eq(value.as_bytes(), secret.as_bytes()))
    }

    fn is_duplicate(&self, event_id: &str) -> bool {
        if event_id.trim().is_empty() {
            return false;
        }
        let mut seen = self
            .seen_event_ids
            .lock()
            .expect("voice seen event ids mutex poisoned");
        if seen.iter().any(|existing| existing == event_id) {
            return true;
        }
        seen.push_back(event_id.to_string());
        while seen.len() > 1000 {
            seen.pop_front();
        }
        false
    }

    fn post_bridge(&self, endpoint: &str, body: Value) -> Result<()> {
        let bridge_base = self
            .bridge_base
            .as_deref()
            .ok_or_else(|| anyhow!("{} channel requires bridge API URL", self.channel))?;
        let mut request = self
            .client
            .post(format!(
                "{}{}",
                bridge_base.trim_end_matches('/'),
                endpoint_path(endpoint)
            ))
            .json(&body);
        if let Some(token) = self.token.as_deref() {
            request = request.bearer_auth(token);
        }
        let response = request.send().context("voice bridge POST failed")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            bail!("voice bridge POST failed with status {status}: {text}");
        }
        Ok(())
    }
}

impl ChannelAdapter for VoiceAdapter {
    fn start(&self, _inbound: GatewayInboundDispatch) -> Result<()> {
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        let expected_paths = if self.channel == TALK_VOICE_CHANNEL {
            ["/talk-voice/events", "/talk-voice/webhook"]
        } else {
            ["/voice-call/events", "/voice-call/webhook"]
        };
        if request.method == "POST" && expected_paths.iter().any(|path| request.path == *path) {
            return self.handle_voice_event(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let conversation_id = route.key.conversation_id.as_str();
        let thread_id = route.key.thread_id.as_deref();
        let reply_to = message.reply_to.as_deref();
        for chunk in text_chunks(&message.text) {
            self.post_bridge(
                &self.send_endpoint,
                json!({
                    "channel": self.channel,
                    "platform": self.platform,
                    "provider": self.provider.as_deref(),
                    "conversation_id": conversation_id,
                    "call_id": conversation_id,
                    "session_id": conversation_id,
                    "thread_id": thread_id,
                    "reply_to": reply_to,
                    "action": "message",
                    "text": chunk,
                    "transcript_reply": chunk,
                    "media_paths": [],
                }),
            )?;
        }
        if !message.media_paths.is_empty() {
            self.post_bridge(
                &self.send_endpoint,
                json!({
                    "channel": self.channel,
                    "platform": self.platform,
                    "provider": self.provider.as_deref(),
                    "conversation_id": conversation_id,
                    "call_id": conversation_id,
                    "session_id": conversation_id,
                    "thread_id": thread_id,
                    "reply_to": reply_to,
                    "action": "media",
                    "text": "",
                    "transcript_reply": "",
                    "media_paths": &message.media_paths,
                    "media_mode": "voice_play_or_link",
                }),
            )?;
        }
        Ok(())
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        let Some(endpoint) = self.typing_endpoint.as_deref() else {
            return Ok(());
        };
        self.post_bridge(
            endpoint,
            json!({
                "channel": self.channel,
                "platform": self.platform,
                "provider": self.provider.as_deref(),
                "conversation_id": route.key.conversation_id.as_str(),
                "call_id": route.key.conversation_id.as_str(),
                "session_id": route.key.conversation_id.as_str(),
                "thread_id": route.key.thread_id.as_deref(),
                "active": event.active,
                "reason": event.reason,
                "status": if event.active { "agent_speaking" } else { "idle" },
            }),
        )
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
        self.post_bridge(
            &self.send_endpoint,
            json!({
                "channel": self.channel,
                "platform": self.platform,
                "provider": self.provider.as_deref(),
                "conversation_id": route.key.conversation_id.as_str(),
                "call_id": route.key.conversation_id.as_str(),
                "session_id": route.key.conversation_id.as_str(),
                "thread_id": route.key.thread_id.as_deref(),
                "action": "approval",
                "text": approval_text,
                "transcript_reply": approval_text,
                "approval": {
                    "id": approval_id.as_str(),
                    "commands": [
                        format!("/approve {} once", approval_id.as_str()),
                        format!("/approve {} session", approval_id.as_str()),
                        format!("/approve {} always", approval_id.as_str()),
                        format!("/deny {}", approval_id.as_str())
                    ]
                }
            }),
        )
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: true,
            typing: self.typing_endpoint.is_some(),
            approval_prompt: true,
        }
    }
}

pub(in crate::gateway::channels) fn new_adapter(
    channel: &'static str,
    config: &GatewayChannelConfig,
    credentials: &GatewayCredentialEntry,
) -> Result<VoiceAdapter> {
    VoiceAdapter::new(channel, config, credentials)
}

fn normalize_voice_event(event: Value) -> Value {
    let Some(payload_key) = ["payload", "body", "data"]
        .iter()
        .find(|key| event.get(*key).and_then(Value::as_str).is_some())
        .copied()
    else {
        return event;
    };
    let Some(payload) = event.get(payload_key).and_then(Value::as_str) else {
        return event;
    };
    let Ok(parsed_payload) = serde_json::from_str::<Value>(payload) else {
        return event;
    };
    let (mut outer, mut inner) = match (event, parsed_payload) {
        (Value::Object(outer), Value::Object(inner)) => (outer, inner),
        (_, parsed_payload) => return parsed_payload,
    };
    outer.remove(payload_key);
    for (key, value) in outer {
        inner.entry(key).or_insert(value);
    }
    Value::Object(inner)
}

fn voice_events_from_value(value: &Value) -> Vec<Value> {
    let mut events = Vec::new();
    for source in [
        Some(value),
        value.get("data").filter(|value| value.is_object()),
        value.get("payload").filter(|value| value.is_object()),
        value.get("event").filter(|value| value.is_object()),
        value.get("message").filter(|value| value.is_object()),
    ]
    .into_iter()
    .flatten()
    {
        for key in [
            "events",
            "messages",
            "segments",
            "transcripts",
            "utterances",
            "recordings",
        ] {
            if let Some(items) = source.get(key).and_then(Value::as_array) {
                events.extend(items.iter().cloned());
            }
        }
    }
    if events.is_empty() {
        vec![value.clone()]
    } else {
        events
    }
}

fn voice_event_body(event: &Value) -> &Value {
    for key in [
        "event",
        "message",
        "payload",
        "data",
        "transcript",
        "utterance",
        "recording",
        "call",
    ] {
        if let Some(value) = event.get(key).filter(|value| value.is_object()) {
            return value;
        }
    }
    event
}

fn compose_voice_session_id(body: &Value, event: &Value) -> Option<String> {
    let from = first_voice_str(
        body,
        event,
        &[
            "caller_id",
            "from",
            "phone_number",
            "sip_uri",
            "caller.id",
            "caller.number",
            "from.id",
            "from.number",
        ],
    )?;
    let to = first_voice_str(
        body,
        event,
        &[
            "callee_id",
            "to",
            "agent_id",
            "callee.id",
            "to.id",
            "agent.id",
        ],
    )?;
    Some(format!("{from}->{to}"))
}

fn voice_text(body: &Value, event: &Value) -> String {
    let text = first_voice_str(
        body,
        event,
        &[
            "text",
            "transcript",
            "message",
            "body",
            "content",
            "summary",
            "caption",
            "message.text",
            "message.content",
            "transcript.text",
            "utterance.text",
            "speech.text",
        ],
    )
    .or_else(|| nested_str(body, "transcript", "text").map(str::to_string))
    .or_else(|| nested_str(body, "utterance", "text").map(str::to_string))
    .or_else(|| nested_str(event, "transcript", "text").map(str::to_string))
    .or_else(|| nested_str(event, "utterance", "text").map(str::to_string))
    .unwrap_or_default();
    normalize_transcript_text(&text)
}

fn voice_chat_type(mode: VoiceMode, body: &Value) -> String {
    first_str(body, &["call_type", "chat_type", "type"])
        .map(str::to_string)
        .unwrap_or_else(|| match mode {
            VoiceMode::VoiceCall => "voice_call".to_string(),
            VoiceMode::TalkVoice => "talk_voice".to_string(),
        })
}

fn first_voice_str(body: &Value, event: &Value, keys: &[&str]) -> Option<String> {
    first_str(body, keys)
        .or_else(|| first_str(event, keys))
        .map(str::to_string)
}

fn first_voice_bool(body: &Value, event: &Value, keys: &[&str]) -> Option<bool> {
    first_bool(body, keys).or_else(|| first_bool(event, keys))
}

fn is_final_voice_event(body: &Value, event: &Value) -> bool {
    if let Some(false) = first_voice_bool(body, event, &["is_final", "final", "finalized"]) {
        return false;
    }
    if let Some(true) = first_voice_bool(body, event, &["partial", "interim", "is_partial"]) {
        return false;
    }
    let status = first_voice_str(
        body,
        event,
        &[
            "status",
            "state",
            "transcript_status",
            "speech_status",
            "event_type",
            "type",
            "kind",
        ],
    )
    .unwrap_or_default()
    .to_ascii_lowercase();
    !matches!(
        status.as_str(),
        "partial"
            | "interim"
            | "recognizing"
            | "recognised_partial"
            | "speech_start"
            | "speech_started"
            | "speaking"
            | "ringing"
            | "connecting"
    )
}

fn transcript_fingerprint(
    body: &Value,
    event: &Value,
    conversation_id: &str,
    sender_id: Option<&str>,
    text: &str,
) -> Option<String> {
    let normalized_text = normalize_transcript_text(text);
    if normalized_text.is_empty() {
        return None;
    }
    let offset = first_voice_str(
        body,
        event,
        &[
            "sequence",
            "seq",
            "index",
            "start_ms",
            "end_ms",
            "start_time",
            "end_time",
            "transcript.start_ms",
            "transcript.end_ms",
            "transcript.start_time",
            "transcript.end_time",
            "timestamp",
        ],
    )
    .unwrap_or_default();
    Some(format!(
        "transcript:{}:{}:{}:{}",
        normalize_id(conversation_id),
        sender_id.map(normalize_id).unwrap_or_default(),
        offset,
        normalized_text.to_ascii_lowercase()
    ))
}

fn normalize_transcript_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn nested_str<'a>(value: &'a Value, object_key: &str, field_key: &str) -> Option<&'a str> {
    value
        .get(object_key)
        .and_then(Value::as_object)
        .and_then(|object| object.get(field_key))
        .and_then(Value::as_str)
}

fn attachment_values(value: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    for key in [
        "attachments",
        "attachment",
        "media",
        "files",
        "file",
        "recordings",
        "recording",
        "audio",
        "voice",
    ] {
        match value.get(key) {
            Some(Value::Array(items)) => out.extend(items.iter()),
            Some(Value::Object(_)) | Some(Value::String(_)) => out.push(&value[key]),
            _ => {}
        }
    }
    out
}

fn attachment_from_value(value: &Value) -> Option<InboundAttachmentInput> {
    if let Some(path) = first_str(value, &["path", "file_path", "local_path"]) {
        return Some(InboundAttachmentInput {
            bytes: None,
            path: Some(path.to_string()),
            filename: first_str(value, &["filename", "name", "file_name"]).map(str::to_string),
            mime: first_str(value, &["mime", "mime_type", "content_type"]).map(str::to_string),
        });
    }
    if let Some(bytes) = first_str(value, &["bytes_base64", "base64"]) {
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(bytes) {
            return Some(InboundAttachmentInput {
                bytes: Some(decoded),
                path: None,
                filename: first_str(value, &["filename", "name", "file_name"]).map(str::to_string),
                mime: first_str(value, &["mime", "mime_type", "content_type"]).map(str::to_string),
            });
        }
    }
    None
}

fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        if key.contains('.') {
            dotted_value(value, key).and_then(Value::as_str)
        } else {
            value.get(*key).and_then(Value::as_str)
        }
    })
}

fn first_bool(value: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|key| {
        if key.contains('.') {
            dotted_value(value, key).and_then(Value::as_bool)
        } else {
            value.get(*key).and_then(Value::as_bool)
        }
    })
}

fn dotted_value<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

fn allowlist_matches(allowlist: &HashSet<String>, value: &str) -> bool {
    allowlist.is_empty() || allowlist.contains("*") || allowlist.contains(&normalize_id(value))
}

fn normalize_id(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn endpoint_path(endpoint: &str) -> String {
    if endpoint.starts_with('/') {
        endpoint.to_string()
    } else {
        format!("/{endpoint}")
    }
}

fn text_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if current.len() + character.len_utf8() > TEXT_LIMIT && !current.is_empty() {
            chunks.push(current);
            current = String::new();
        }
        current.push(character);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
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
    fn voice_session_can_be_composed_from_call_parties() {
        let value = json!({"from": "+15550001", "to": "+15550002", "text": "hi"});
        assert_eq!(
            compose_voice_session_id(&value, &value),
            Some("+15550001->+15550002".to_string())
        );
    }

    #[test]
    fn nested_transcript_text_is_supported() {
        let value = json!({"transcript": {"text": "hello"}});
        assert_eq!(voice_text(&value, &value), "hello");
    }
}
