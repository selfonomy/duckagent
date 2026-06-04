use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundAttachmentInput,
    InboundMessageInput, OutboundMessage, StreamMessageHandle, TypingEvent,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::{thread, time::Duration};

const GOOGLE_CHAT_API_BASE: &str = "https://chat.googleapis.com";
const GOOGLE_PUBSUB_API_BASE: &str = "https://pubsub.googleapis.com";
const GOOGLE_CHAT_TEXT_LIMIT: usize = 4_000;
const GOOGLE_ATTACHMENT_DOWNLOAD_LIMIT: u64 = 25 * 1024 * 1024;
const GOOGLE_ATTACHMENT_HOSTS: &[&str] = &[
    "googleapis.com",
    "chat.google.com",
    "drive.google.com",
    "docs.google.com",
    "lh3.googleusercontent.com",
    "lh4.googleusercontent.com",
    "lh5.googleusercontent.com",
    "lh6.googleusercontent.com",
];

#[derive(Clone)]
pub(in crate::gateway) struct GoogleChatAdapter {
    channel: String,
    token: Option<String>,
    api_base: String,
    pubsub_api_base: String,
    pubsub_subscription: Option<String>,
    allowed_users: HashSet<String>,
    allowed_spaces: HashSet<String>,
    client: Client,
    threads: Arc<Mutex<HashMap<String, String>>>,
    seen_message_ids: Arc<Mutex<VecDeque<String>>>,
}

impl GoogleChatAdapter {
    pub(in crate::gateway) fn new(
        channel: &str,
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build Google Chat HTTP client")?;
        Ok(Self {
            channel: channel.to_string(),
            token: credentials.token.clone().or(credentials.api_key.clone()),
            api_base: config
                .api_base
                .clone()
                .unwrap_or_else(|| GOOGLE_CHAT_API_BASE.to_string()),
            pubsub_api_base: config
                .extra
                .get("pubsub_api_base")
                .cloned()
                .unwrap_or_else(|| GOOGLE_PUBSUB_API_BASE.to_string()),
            pubsub_subscription: config
                .extra
                .get("pubsub_subscription")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            allowed_users: config
                .allowed_users
                .iter()
                .flat_map(|value| {
                    let trimmed = value.trim().to_string();
                    [trimmed.clone(), trimmed.to_ascii_lowercase()]
                })
                .filter(|value| !value.is_empty())
                .collect(),
            allowed_spaces: config
                .allowed_chats
                .iter()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect(),
            client,
            threads: Arc::new(Mutex::new(HashMap::new())),
            seen_message_ids: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    fn is_duplicate_message(&self, message_id: &str) -> bool {
        let mut seen = self
            .seen_message_ids
            .lock()
            .expect("google chat seen message ids mutex poisoned");
        if seen.iter().any(|existing| existing == message_id) {
            return true;
        }
        seen.push_back(message_id.to_string());
        while seen.len() > 1000 {
            seen.pop_front();
        }
        false
    }

    fn sender_ids(value: &Value, message: &Value) -> Vec<String> {
        let mut ids = Vec::new();
        for (source, pointer) in [
            (message, "/sender/name"),
            (message, "/sender/email"),
            (message, "/sender/displayName"),
            (value, "/user/name"),
            (value, "/user/email"),
            (value, "/user/displayName"),
            (value, "/sender_name"),
            (value, "/sender_email"),
            (value, "/sender_display_name"),
        ] {
            let Some(id) = source
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            let id = id.to_string();
            if !ids.contains(&id) {
                ids.push(id.clone());
            }
            let lower = id.to_ascii_lowercase();
            if lower != id && !ids.contains(&lower) {
                ids.push(lower);
            }
        }
        ids
    }

    fn event_text(value: &Value, message: &Value) -> Option<String> {
        if let Some(argument_text) = message["argumentText"]
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            return Some(argument_text.to_string());
        }
        if let Some(text) = message["text"]
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            return Some(text.to_string());
        }
        for pointer in [
            "/common/parameters/text",
            "/common/parameters/command",
            "/action/parameters/text",
            "/action/parameters/command",
            "/text",
            "/command",
        ] {
            if let Some(text) = value
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
            {
                return Some(text.to_string());
            }
        }
        for pointer in ["/common/parameters", "/action/parameters"] {
            if let Some(parameters) = value.pointer(pointer).and_then(Value::as_array) {
                for parameter in parameters {
                    let key = parameter
                        .get("key")
                        .or_else(|| parameter.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if matches!(key, "text" | "command") {
                        if let Some(text) = parameter
                            .get("value")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|text| !text.is_empty())
                        {
                            return Some(text.to_string());
                        }
                    }
                }
            }
        }
        None
    }

    fn event_type(value: &Value) -> String {
        if let Some(event_type) = value["type"]
            .as_str()
            .or_else(|| value["event_type"].as_str())
            .or_else(|| value.pointer("/chat/type").and_then(Value::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return event_type.to_string();
        }
        if value.pointer("/chat/messagePayload/message").is_some()
            || value.pointer("/messagePayload/message").is_some()
        {
            return "MESSAGE".to_string();
        }
        if value
            .pointer("/chat/buttonClickedPayload/message")
            .is_some()
            || value.pointer("/buttonClickedPayload/message").is_some()
        {
            return "CARD_CLICKED".to_string();
        }
        if value.pointer("/chat/membershipPayload").is_some()
            || value.pointer("/membershipPayload").is_some()
        {
            return "MEMBERSHIP".to_string();
        }
        String::new()
    }

    fn event_message(value: &Value) -> Option<&Value> {
        value
            .get("message")
            .filter(|message| message.is_object())
            .or_else(|| value.pointer("/chat/messagePayload/message"))
            .or_else(|| value.pointer("/messagePayload/message"))
            .or_else(|| value.pointer("/chat/buttonClickedPayload/message"))
            .or_else(|| value.pointer("/buttonClickedPayload/message"))
    }

    fn event_space<'a>(value: &'a Value, message: Option<&'a Value>) -> Option<&'a str> {
        message
            .and_then(|message| message.pointer("/space/name").and_then(Value::as_str))
            .or_else(|| value.pointer("/space/name").and_then(Value::as_str))
            .or_else(|| {
                value
                    .pointer("/chat/messagePayload/space/name")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                value
                    .pointer("/messagePayload/space/name")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                value
                    .pointer("/chat/buttonClickedPayload/space/name")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                value
                    .pointer("/buttonClickedPayload/space/name")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                value
                    .pointer("/chat/membershipPayload/space/name")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                value
                    .pointer("/membershipPayload/space/name")
                    .and_then(Value::as_str)
            })
            .or_else(|| value["space_name"].as_str())
    }

    fn sender_is_bot(value: &Value, message: &Value) -> bool {
        [
            message.pointer("/sender/type"),
            value.pointer("/user/type"),
            value.pointer("/sender_type"),
        ]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|value| value.eq_ignore_ascii_case("BOT"))
    }

    fn chat_type(value: &Value, message: &Value) -> String {
        match message["space"]["type"]
            .as_str()
            .or_else(|| message["space"]["spaceType"].as_str())
            .or_else(|| value["space"]["type"].as_str())
            .or_else(|| value["space"]["spaceType"].as_str())
            .or_else(|| {
                value
                    .pointer("/chat/messagePayload/space/type")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                value
                    .pointer("/chat/messagePayload/space/spaceType")
                    .and_then(Value::as_str)
            })
            .unwrap_or_default()
        {
            "DM" | "DIRECT_MESSAGE" => "dm".to_string(),
            _ => "space".to_string(),
        }
    }

    fn thread_for_route(&self, route: &GatewayRoute) -> Option<String> {
        if let Some(thread_id) = route.key.thread_id.as_ref() {
            return Some(thread_id.clone());
        }
        self.threads
            .lock()
            .expect("google chat threads mutex poisoned")
            .get(&route.key.conversation_id)
            .cloned()
    }

    fn handle_event(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        let value: Value = serde_json::from_slice(&request.body)
            .context("failed to parse Google Chat event JSON")?;
        self.submit_event_value(value, &inbound)?;
        Ok(json_response(200, json!({"text": ""})))
    }

    fn submit_event_value(&self, value: Value, inbound: &GatewayInboundDispatch) -> Result<()> {
        let event_type = Self::event_type(&value);
        if let Some(flat_input) = self.flat_event_to_inbound(&value, &event_type)? {
            inbound.submit(flat_input)?;
            return Ok(());
        }
        let message = Self::event_message(&value);
        let Some(space) = Self::event_space(&value, message) else {
            if matches!(
                event_type.as_str(),
                "ADDED_TO_SPACE" | "REMOVED_FROM_SPACE" | "MEMBERSHIP"
            ) {
                return Ok(());
            }
            bail!("Google Chat event missing space.name");
        };
        if !self.allowed_spaces.is_empty() && !self.allowed_spaces.contains(space) {
            return Ok(());
        }
        let Some(message) = message else {
            return Ok(());
        };
        if Self::sender_is_bot(&value, message) {
            return Ok(());
        }
        let sender_ids = Self::sender_ids(&value, message);
        if !self.allowed_users.is_empty()
            && !sender_ids
                .iter()
                .any(|sender_id| self.allowed_users.contains(sender_id))
        {
            return Ok(());
        }
        let sender = sender_ids.first().map(String::as_str).unwrap_or_default();
        if !matches!(event_type.as_str(), "MESSAGE" | "CARD_CLICKED") {
            return Ok(());
        }
        if event_type == "MESSAGE" {
            if let Some(message_id) = message["name"].as_str() {
                if self.is_duplicate_message(message_id) {
                    return Ok(());
                }
            }
        }
        let thread = message["thread"]["name"].as_str().map(str::to_string);
        if let Some(thread) = thread.as_deref() {
            self.threads
                .lock()
                .expect("google chat threads mutex poisoned")
                .insert(space.to_string(), thread.to_string());
        }
        let text = Self::event_text(&value, message).unwrap_or_default();
        let attachments = self.message_attachments(message);
        inbound.submit(InboundMessageInput {
            channel: self.channel.clone(),
            conversation_id: space.to_string(),
            thread_id: thread,
            chat_type: Some(Self::chat_type(&value, message)),
            sender_id: (!sender.is_empty()).then(|| sender.to_string()),
            message_id: message["name"].as_str().map(str::to_string),
            text,
            attachments,
            timestamp: message["createTime"].as_str().map(str::to_string),
        })?;
        Ok(())
    }

    fn flat_event_to_inbound(
        &self,
        value: &Value,
        event_type: &str,
    ) -> Result<Option<InboundMessageInput>> {
        let Some(space) = value["space_name"].as_str() else {
            return Ok(None);
        };
        if !matches!(event_type, "MESSAGE" | "") {
            return Ok(None);
        }
        if value["sender_type"]
            .as_str()
            .is_some_and(|sender_type| sender_type.eq_ignore_ascii_case("BOT"))
        {
            return Ok(None);
        }
        if !self.allowed_spaces.is_empty() && !self.allowed_spaces.contains(space) {
            return Ok(None);
        }
        let sender_ids = Self::sender_ids(value, &Value::Null);
        if !self.allowed_users.is_empty()
            && !sender_ids
                .iter()
                .any(|sender_id| self.allowed_users.contains(sender_id))
        {
            return Ok(None);
        }
        let message_id = value["message_name"]
            .as_str()
            .or_else(|| value["message_id"].as_str())
            .map(str::to_string);
        if let Some(message_id) = message_id.as_deref() {
            if self.is_duplicate_message(message_id) {
                return Ok(None);
            }
        }
        Ok(Some(InboundMessageInput {
            channel: self.channel.clone(),
            conversation_id: space.to_string(),
            thread_id: value["thread_name"]
                .as_str()
                .or_else(|| value["thread_id"].as_str())
                .map(str::to_string),
            chat_type: Some(
                value["space_type"]
                    .as_str()
                    .map(|value| {
                        if matches!(value, "DM" | "DIRECT_MESSAGE" | "dm") {
                            "dm"
                        } else {
                            "space"
                        }
                    })
                    .unwrap_or("space")
                    .to_string(),
            ),
            sender_id: sender_ids.first().cloned(),
            message_id,
            text: value["text"].as_str().unwrap_or_default().to_string(),
            attachments: Vec::new(),
            timestamp: value["create_time"]
                .as_str()
                .or_else(|| value["timestamp"].as_str())
                .map(str::to_string),
        }))
    }

    fn message_attachments(&self, message: &Value) -> Vec<InboundAttachmentInput> {
        let mut attachments = Vec::new();
        for attachment in message["attachment"].as_array().into_iter().flatten() {
            match self.download_attachment(attachment) {
                Ok(attachment) => attachments.push(attachment),
                Err(error) => eprintln!("Google Chat attachment skipped: {error:#}"),
            }
        }
        attachments
    }

    fn download_attachment(&self, attachment: &Value) -> Result<InboundAttachmentInput> {
        let token = self
            .token
            .as_deref()
            .ok_or_else(|| anyhow!("Google Chat attachment download requires access token"))?;
        let content_name = attachment["contentName"]
            .as_str()
            .or_else(|| attachment["content_name"].as_str())
            .unwrap_or("google-chat-attachment");
        let mime = attachment["contentType"]
            .as_str()
            .or_else(|| attachment["content_type"].as_str())
            .unwrap_or("application/octet-stream")
            .to_string();
        let url = if let Some(uri) = attachment["downloadUri"]
            .as_str()
            .or_else(|| attachment["download_uri"].as_str())
            .filter(|uri| trusted_google_attachment_url(uri))
        {
            uri.to_string()
        } else if let Some(resource_name) = attachment
            .pointer("/attachmentDataRef/resourceName")
            .and_then(Value::as_str)
            .or_else(|| {
                attachment
                    .pointer("/attachment_data_ref/resource_name")
                    .and_then(Value::as_str)
            })
        {
            format!(
                "{}/v1/media/{}?alt=media",
                self.api_base.trim_end_matches('/'),
                resource_name.trim_start_matches('/')
            )
        } else {
            bail!("Google Chat attachment missing downloadUri or attachmentDataRef.resourceName");
        };
        let response = self
            .client
            .get(url)
            .bearer_auth(token)
            .send()
            .context("Google Chat attachment download failed")?;
        if !response.status().is_success() {
            bail!(
                "Google Chat attachment download failed with status {}",
                response.status()
            );
        }
        let bytes = response
            .bytes()
            .context("Google Chat attachment body unreadable")?;
        if bytes.len() as u64 > GOOGLE_ATTACHMENT_DOWNLOAD_LIMIT {
            bail!("Google Chat attachment exceeds download limit");
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: Some(content_name.to_string()),
            mime: Some(mime),
        })
    }

    fn poll_pubsub_loop(&self, subscription: String, inbound: GatewayInboundDispatch) {
        loop {
            match self.poll_pubsub_once(&subscription, &inbound) {
                Ok(0) => thread::sleep(Duration::from_secs(2)),
                Ok(_) => {}
                Err(error) => {
                    eprintln!("Google Chat Pub/Sub polling failed: {error:#}");
                    thread::sleep(Duration::from_secs(5));
                }
            }
        }
    }

    fn poll_pubsub_once(
        &self,
        subscription: &str,
        inbound: &GatewayInboundDispatch,
    ) -> Result<usize> {
        let token = self
            .token
            .as_deref()
            .ok_or_else(|| anyhow!("Google Chat Pub/Sub polling requires an access token"))?;
        let pull_url = format!(
            "{}/v1/{}:pull",
            self.pubsub_api_base.trim_end_matches('/'),
            subscription
        );
        let response = self
            .client
            .post(pull_url)
            .bearer_auth(token)
            .json(&json!({"maxMessages": 10}))
            .send()
            .context("Google Chat Pub/Sub pull failed")?;
        let status = response.status();
        let value = response.json::<Value>().unwrap_or_else(|_| Value::Null);
        if !status.is_success() {
            bail!("Google Chat Pub/Sub pull failed with status {status}: {value}");
        }
        let Some(messages) = value["receivedMessages"].as_array() else {
            return Ok(0);
        };
        let mut ack_ids = Vec::new();
        for received in messages {
            let Some(ack_id) = received["ackId"].as_str().map(str::to_string) else {
                continue;
            };
            let Some(data) = received["message"]["data"].as_str() else {
                ack_ids.push(ack_id);
                continue;
            };
            match decode_pubsub_event(data)
                .and_then(|event| self.submit_event_value(event, inbound))
            {
                Ok(()) => ack_ids.push(ack_id),
                Err(error) => eprintln!("Google Chat Pub/Sub message skipped: {error:#}"),
            }
        }
        let processed = ack_ids.len();
        if !ack_ids.is_empty() {
            self.ack_pubsub(subscription, token, ack_ids)?;
        }
        Ok(processed)
    }

    fn ack_pubsub(&self, subscription: &str, token: &str, ack_ids: Vec<String>) -> Result<()> {
        let ack_url = format!(
            "{}/v1/{}:acknowledge",
            self.pubsub_api_base.trim_end_matches('/'),
            subscription
        );
        let response = self
            .client
            .post(ack_url)
            .bearer_auth(token)
            .json(&json!({"ackIds": ack_ids}))
            .send()
            .context("Google Chat Pub/Sub ack failed")?;
        let status = response.status();
        if !status.is_success() {
            let value = response.json::<Value>().unwrap_or_else(|_| Value::Null);
            bail!("Google Chat Pub/Sub ack failed with status {status}: {value}");
        }
        Ok(())
    }

    fn send_google_message(&self, route: &GatewayRoute, body: Value) -> Result<Value> {
        let space = &route.key.conversation_id;
        let url = format!(
            "{}/v1/{}/messages",
            self.api_base.trim_end_matches('/'),
            space
        );
        let mut request = self.client.post(url).json(&body);
        if let Some(token) = self.token.as_deref() {
            request = request.bearer_auth(token);
        }
        let response = request.send().context("Google Chat send failed")?;
        let status = response.status();
        let value = response.json::<Value>().unwrap_or_else(|_| Value::Null);
        if !status.is_success() {
            bail!("Google Chat send failed with status {status}: {value}");
        }
        Ok(value)
    }

    fn update_google_message(&self, message_name: &str, text: &str) -> Result<()> {
        let url = format!(
            "{}/v1/{}?updateMask=text",
            self.api_base.trim_end_matches('/'),
            message_name
        );
        let mut request = self.client.patch(url).json(&json!({"text": text}));
        if let Some(token) = self.token.as_deref() {
            request = request.bearer_auth(token);
        }
        let response = request.send().context("Google Chat update failed")?;
        let status = response.status();
        let value = response.json::<Value>().unwrap_or_else(|_| Value::Null);
        if !status.is_success() {
            bail!("Google Chat update failed with status {status}: {value}");
        }
        Ok(())
    }
}

impl ChannelAdapter for GoogleChatAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        let Some(subscription) = self.pubsub_subscription.clone() else {
            return Ok(());
        };
        let adapter = self.clone();
        thread::Builder::new()
            .name(format!("gateway-googlechat-pubsub-{subscription}"))
            .spawn(move || adapter.poll_pubsub_loop(subscription, inbound))
            .context("failed to spawn Google Chat Pub/Sub polling thread")?;
        Ok(())
    }

    fn handle_http(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<Option<ChannelHttpResponse>> {
        if request.method == "POST"
            && matches!(
                request.path.as_str(),
                "/google_chat/events" | "/googlechat/events"
            )
        {
            return self.handle_event(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let mut text = message.text;
        for media in message.media_paths {
            text.push('\n');
            text.push_str(&media);
        }
        for chunk in google_chunks(&text) {
            let mut body = json!({"text": chunk});
            if let Some(thread) = self.thread_for_route(route) {
                body["thread"] = json!({"name": thread});
            }
            self.send_google_message(route, body)?;
        }
        Ok(())
    }

    fn send_stream_start(
        &self,
        route: &GatewayRoute,
        text: &str,
    ) -> Result<Option<StreamMessageHandle>> {
        let mut body = json!({"text": text});
        if let Some(thread) = self.thread_for_route(route) {
            body["thread"] = json!({"name": thread});
        }
        let value = self.send_google_message(route, body)?;
        let message_id = value["name"]
            .as_str()
            .ok_or_else(|| anyhow!("Google Chat stream start did not return message name"))?
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
        self.update_google_message(&handle.message_id, text)
    }

    fn stream_text_limit(&self) -> usize {
        GOOGLE_CHAT_TEXT_LIMIT
    }

    fn send_typing(&self, _route: &GatewayRoute, _event: TypingEvent) -> Result<()> {
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        let approval_id = prompt.id.clone();
        let approval_message = prompt.message.clone();
        let approval_text = format!(
            "{}\n\nCommands:\n/approve {} once\n/approve {} session\n/approve {} always\n/deny {}",
            approval_message,
            approval_id.as_str(),
            approval_id.as_str(),
            approval_id.as_str(),
            approval_id.as_str()
        );
        let mut body = json!({
            "text": approval_text.clone(),
            "cardsV2": [{
                "cardId": "duckagent_approval",
                "card": {
                    "header": {"title": "DuckAgent approval"},
                    "sections": [{
                        "widgets": [
                            {"textParagraph": {"text": approval_text}},
                            {"buttonList": {"buttons": [
                                {"text": "Once", "onClick": {"action": {"function": "approval", "parameters": [{"key": "text", "value": format!("/approve {} once", approval_id.as_str())}]}}},
                                {"text": "Session", "onClick": {"action": {"function": "approval", "parameters": [{"key": "text", "value": format!("/approve {} session", approval_id.as_str())}]}}},
                                {"text": "Always", "onClick": {"action": {"function": "approval", "parameters": [{"key": "text", "value": format!("/approve {} always", approval_id.as_str())}]}}},
                                {"text": "Deny", "onClick": {"action": {"function": "approval", "parameters": [{"key": "text", "value": format!("/deny {}", approval_id.as_str())}]}}}
                            ]}}
                        ]
                    }]
                }
            }]
        });
        if let Some(thread) = self.thread_for_route(route) {
            body["thread"] = json!({"name": thread});
        }
        self.send_google_message(route, body)?;
        Ok(())
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: true,
            typing: false,
            approval_prompt: true,
        }
    }
}

fn google_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    text.as_bytes()
        .chunks(GOOGLE_CHAT_TEXT_LIMIT)
        .map(|chunk| String::from_utf8_lossy(chunk).to_string())
        .collect()
}

fn json_response(status: u16, value: Value) -> ChannelHttpResponse {
    ChannelHttpResponse {
        status,
        content_type: "application/json",
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
    }
}

fn decode_pubsub_event(data: &str) -> Result<Value> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .context("failed to decode Google Chat Pub/Sub message data")?;
    serde_json::from_slice(&bytes).context("failed to parse Google Chat Pub/Sub event JSON")
}

fn trusted_google_attachment_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let Some(host) = parsed.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    GOOGLE_ATTACHMENT_HOSTS
        .iter()
        .any(|trusted| host == *trusted || host.ends_with(&format!(".{trusted}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_chunks_split() {
        assert_eq!(
            google_chunks(&"x".repeat(GOOGLE_CHAT_TEXT_LIMIT + 1)).len(),
            2
        );
    }

    #[test]
    fn google_alias_keeps_namespace() -> Result<()> {
        let adapter = GoogleChatAdapter::new(
            "googlechat",
            &GatewayChannelConfig::default(),
            &GatewayCredentialEntry {
                channel: "googlechat".to_string(),
                ..Default::default()
            },
        )?;
        assert_eq!(adapter.channel, "googlechat");
        Ok(())
    }
}
