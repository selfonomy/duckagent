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
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::form_urlencoded;

const TWILIO_API_BASE: &str = "https://api.twilio.com/2010-04-01/Accounts";
const SMS_TEXT_LIMIT: usize = 1_600;
const SMS_TWILIO_PATH: &str = "/sms/twilio";
const SMS_EVENTS_PATH: &str = "/sms/events";
const EMPTY_TWIML: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Response></Response>"#;

#[derive(Clone)]
pub(in crate::gateway) struct SmsAdapter {
    account_sid: String,
    auth_token: String,
    from_number: String,
    api_base: String,
    webhook_url: Option<String>,
    webhook_secret: Option<String>,
    insecure_no_signature: bool,
    allowed_users: HashSet<String>,
    max_download_bytes: u64,
    client: Client,
    seen_message_ids: Arc<Mutex<VecDeque<String>>>,
}

#[derive(Debug, Clone)]
struct SmsInbound {
    from: String,
    body: String,
    message_sid: Option<String>,
    attachments: Vec<InboundAttachmentInput>,
}

impl SmsAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let account_sid = credentials
            .app_id
            .as_deref()
            .or(credentials.username.as_deref())
            .or(credentials.api_key.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("sms gateway credential requires Twilio account SID"))?
            .to_string();
        let auth_token = credentials
            .token
            .as_deref()
            .or(credentials.password.as_deref())
            .or(credentials.client_secret.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("sms gateway credential requires Twilio auth token"))?
            .to_string();
        let from_number = credentials
            .extra
            .get("from_number")
            .map(String::as_str)
            .or(config.extra.get("from_number").map(String::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("sms gateway config requires from_number"))?
            .to_string();
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build SMS HTTP client")?;
        Ok(Self {
            account_sid,
            auth_token,
            from_number: normalize_sms_number(&from_number),
            api_base: config
                .api_base
                .clone()
                .unwrap_or_else(|| TWILIO_API_BASE.to_string()),
            webhook_url: config.extra.get("webhook_url").cloned(),
            webhook_secret: credentials.webhook_secret.clone(),
            insecure_no_signature: config
                .extra
                .get("insecure_no_signature")
                .is_some_and(|value| value == "true"),
            allowed_users: config
                .allowed_users
                .iter()
                .map(|value| normalize_sms_number(value))
                .collect(),
            max_download_bytes: config.media.max_download_bytes,
            client,
            seen_message_ids: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    fn handle_sms_webhook(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        if request.path == SMS_EVENTS_PATH {
            if let Some(messages) = self.parse_json_inbounds(&request.body)? {
                if !self.verify_bridge_secret(&request) {
                    return Ok(xml_response(403));
                }
                for sms in messages {
                    self.submit_inbound(sms, &inbound)?;
                }
                return Ok(xml_response(200));
            }
        }

        let form = parse_form(&request.body);
        if !self.verify_twilio_signature(&request, &form) {
            return Ok(xml_response(403));
        }
        if is_twilio_status_callback(&form) {
            return Ok(xml_response(200));
        }
        let sms = self.parse_inbound(&form)?;
        self.submit_inbound(sms, &inbound)?;
        Ok(xml_response(200))
    }

    fn submit_inbound(&self, sms: SmsInbound, inbound: &GatewayInboundDispatch) -> Result<()> {
        if sms
            .message_sid
            .as_deref()
            .is_some_and(|message_sid| self.is_duplicate(message_sid))
        {
            return Ok(());
        }
        if sms.from == self.from_number {
            return Ok(());
        }
        if !self.allowed_users.is_empty() && !self.allowed_users.contains(&sms.from) {
            return Ok(());
        }
        inbound.submit(InboundMessageInput {
            channel: "sms".to_string(),
            conversation_id: sms.from.clone(),
            thread_id: None,
            chat_type: Some("dm".to_string()),
            sender_id: Some(sms.from),
            message_id: sms.message_sid,
            text: sms.body,
            attachments: sms.attachments,
            timestamp: None,
        })?;
        Ok(())
    }

    fn parse_inbound(&self, form: &HashMap<String, String>) -> Result<SmsInbound> {
        let from = normalize_sms_number(
            &form_value(form, &["From", "from", "from_number", "fromNumber"]).unwrap_or_default(),
        );
        let body = form_value(
            form,
            &["Body", "body", "Text", "text", "Message", "message"],
        )
        .unwrap_or_default();
        let media_count = media_count(form);
        if from.trim().is_empty() || (body.trim().is_empty() && media_count == 0) {
            bail!("sms inbound missing From or Body");
        }
        let mut attachments = Vec::new();
        for index in 0..media_count {
            let Some(url) = form_value(
                form,
                &[
                    &format!("MediaUrl{index}"),
                    &format!("mediaUrl{index}"),
                    &format!("media_url_{index}"),
                ],
            ) else {
                continue;
            };
            let mime = form_value(
                form,
                &[
                    &format!("MediaContentType{index}"),
                    &format!("mediaContentType{index}"),
                    &format!("media_content_type_{index}"),
                ],
            )
            .unwrap_or_else(|| "application/octet-stream".to_string());
            match self.download_media(&url, &mime) {
                Ok(attachment) => attachments.push(attachment),
                Err(error) => eprintln!("sms gateway media skipped: {error:#}"),
            }
        }
        Ok(SmsInbound {
            from,
            body: if body.trim().is_empty() {
                "(sms media)".to_string()
            } else {
                body
            },
            message_sid: form_value(
                form,
                &[
                    "MessageSid",
                    "SmsMessageSid",
                    "message_sid",
                    "messageSid",
                    "smsMessageSid",
                    "sid",
                    "id",
                ],
            ),
            attachments,
        })
    }

    fn parse_json_inbounds(&self, body: &[u8]) -> Result<Option<Vec<SmsInbound>>> {
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return Ok(None);
        };
        let candidates = sms_event_values(&value);
        let mut messages = Vec::new();
        let mut first_error = None;
        for candidate in candidates {
            if is_sms_status_value(candidate) {
                continue;
            }
            match self.parse_json_inbound(candidate) {
                Ok(message) => messages.push(message),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        if messages.is_empty() {
            if let Some(error) = first_error {
                return Err(error);
            }
        }
        Ok(Some(messages))
    }

    fn parse_json_inbound(&self, value: &Value) -> Result<SmsInbound> {
        let from = normalize_sms_number(
            &json_string_value(&[
                &value["From"],
                &value["from"],
                &value["from_number"],
                &value["fromNumber"],
                &value["sender"],
                &value["sender_number"],
                &value["phone"],
            ])
            .unwrap_or_default(),
        );
        let body = json_string_value(&[
            &value["Body"],
            &value["body"],
            &value["Text"],
            &value["text"],
            &value["Message"],
            &value["message"],
            &value["content"],
        ])
        .unwrap_or_default();
        let attachments = self.json_attachments(value)?;
        if from.trim().is_empty() || (body.trim().is_empty() && attachments.is_empty()) {
            bail!("sms inbound missing From or Body");
        }
        Ok(SmsInbound {
            from,
            body: if body.trim().is_empty() {
                "(sms media)".to_string()
            } else {
                body
            },
            message_sid: json_string_value(&[
                &value["MessageSid"],
                &value["SmsMessageSid"],
                &value["message_sid"],
                &value["messageSid"],
                &value["smsMessageSid"],
                &value["sid"],
                &value["id"],
            ]),
            attachments,
        })
    }

    fn json_attachments(&self, value: &Value) -> Result<Vec<InboundAttachmentInput>> {
        let mut attachments = Vec::new();
        let media_count =
            json_string_value(&[&value["NumMedia"], &value["numMedia"], &value["num_media"]])
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
        for index in 0..media_count {
            let url_keys = [
                format!("MediaUrl{index}"),
                format!("mediaUrl{index}"),
                format!("media_url_{index}"),
            ];
            if let Some(url) = url_keys
                .iter()
                .find_map(|key| json_object_string(value, key))
            {
                let mime_keys = [
                    format!("MediaContentType{index}"),
                    format!("mediaContentType{index}"),
                    format!("media_content_type_{index}"),
                ];
                let mime = mime_keys
                    .iter()
                    .find_map(|key| json_object_string(value, key))
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                match self.download_media_auto_auth(&url, &mime) {
                    Ok(attachment) => attachments.push(attachment),
                    Err(error) => eprintln!("sms gateway media skipped: {error:#}"),
                }
            }
        }

        for item in sms_attachment_values(value) {
            if let Some(attachment) = self.json_attachment(item)? {
                attachments.push(attachment);
            }
        }
        Ok(attachments)
    }

    fn json_attachment(&self, value: &Value) -> Result<Option<InboundAttachmentInput>> {
        if let Some(url) = value.as_str().map(str::trim).filter(|url| !url.is_empty()) {
            return self
                .download_media_auto_auth(url, "application/octet-stream")
                .map(Some);
        }
        let bytes = json_string_value(&[
            &value["bytes"],
            &value["base64"],
            &value["content"],
            &value["content_base64"],
            &value["data"],
        ])
        .and_then(|encoded| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
        });
        if bytes
            .as_ref()
            .is_some_and(|bytes| bytes.len() as u64 > self.max_download_bytes)
        {
            return Ok(None);
        }
        if let Some(bytes) = bytes {
            return Ok(Some(InboundAttachmentInput {
                bytes: Some(bytes),
                path: None,
                filename: json_string_value(&[
                    &value["filename"],
                    &value["name"],
                    &value["file_name"],
                    &value["fileName"],
                ]),
                mime: json_string_value(&[
                    &value["mime"],
                    &value["mimetype"],
                    &value["mime_type"],
                    &value["content_type"],
                    &value["contentType"],
                ]),
            }));
        }
        if let Some(url) = json_string_value(&[
            &value["url"],
            &value["media_url"],
            &value["mediaUrl"],
            &value["download_url"],
            &value["downloadUrl"],
        ]) {
            return self
                .download_media_auto_auth(
                    &url,
                    &json_string_value(&[
                        &value["mime"],
                        &value["mimetype"],
                        &value["mime_type"],
                        &value["content_type"],
                        &value["contentType"],
                    ])
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                )
                .map(Some);
        }
        if let Some(path) =
            json_string_value(&[&value["path"], &value["file_path"], &value["filePath"]])
        {
            return Ok(Some(InboundAttachmentInput {
                bytes: None,
                path: Some(path),
                filename: json_string_value(&[
                    &value["filename"],
                    &value["name"],
                    &value["file_name"],
                    &value["fileName"],
                ]),
                mime: json_string_value(&[
                    &value["mime"],
                    &value["mimetype"],
                    &value["mime_type"],
                    &value["content_type"],
                    &value["contentType"],
                ]),
            }));
        }
        Ok(None)
    }

    fn download_media(&self, url: &str, mime: &str) -> Result<InboundAttachmentInput> {
        self.download_media_with_auth(url, mime, true)
    }

    fn download_media_auto_auth(&self, url: &str, mime: &str) -> Result<InboundAttachmentInput> {
        self.download_media_with_auth(url, mime, url.contains("twilio.com"))
    }

    fn download_media_with_auth(
        &self,
        url: &str,
        mime: &str,
        use_auth: bool,
    ) -> Result<InboundAttachmentInput> {
        let mut request = self.client.get(url);
        if use_auth {
            request = request.basic_auth(&self.account_sid, Some(&self.auth_token));
        }
        let response = request
            .send()
            .with_context(|| format!("sms media download failed for {url}"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("sms media download failed with status {status}");
        }
        let bytes = response.bytes().context("sms media body unreadable")?;
        if bytes.len() as u64 > self.max_download_bytes {
            bail!("sms media exceeds configured max_download_bytes: {url}");
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: Some(format!("sms-media{}", extension_for_mime(mime))),
            mime: Some(mime.to_string()),
        })
    }

    fn verify_twilio_signature(
        &self,
        request: &ChannelHttpRequest,
        form: &HashMap<String, String>,
    ) -> bool {
        let Some(webhook_url) = self.webhook_url.as_deref() else {
            return self.insecure_no_signature;
        };
        let Some(signature) = request
            .header("x-twilio-signature")
            .or_else(|| request.header("X-Twilio-Signature"))
        else {
            return false;
        };
        check_twilio_signature(webhook_url, form, signature, &self.auth_token)
            || port_variant_url(webhook_url)
                .is_some_and(|url| check_twilio_signature(&url, form, signature, &self.auth_token))
    }

    fn verify_bridge_secret(&self, request: &ChannelHttpRequest) -> bool {
        let Some(secret) = self.webhook_secret.as_deref() else {
            return self.insecure_no_signature;
        };
        let candidate = request
            .header("x-duckagent-sms-secret")
            .or_else(|| request.header("x-sms-secret"))
            .or_else(|| request.query.get("secret").map(String::as_str));
        candidate.is_some_and(|value| constant_time_eq(value.as_bytes(), secret.as_bytes()))
    }

    fn send_sms(&self, to: &str, body: &str, media_paths: &[String]) -> Result<()> {
        let url = format!(
            "{}/{}/Messages.json",
            self.api_base.trim_end_matches('/'),
            self.account_sid
        );
        let mut params = vec![
            ("From".to_string(), self.from_number.clone()),
            ("To".to_string(), normalize_sms_number(to)),
            ("Body".to_string(), body.to_string()),
        ];
        for media_path in media_paths {
            if media_path.starts_with("http://") || media_path.starts_with("https://") {
                params.push(("MediaUrl".to_string(), media_path.clone()));
            } else {
                bail!("SMS/MMS local MEDIA requires a public media URL: {media_path}");
            }
        }
        let response = self
            .client
            .post(url)
            .basic_auth(&self.account_sid, Some(&self.auth_token))
            .form(&params)
            .send()
            .context("Twilio SMS send failed")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            bail!("Twilio SMS send failed with status {status}: {body}");
        }
        Ok(())
    }

    fn is_duplicate(&self, message_id: &str) -> bool {
        let trimmed = message_id.trim();
        if trimmed.is_empty() {
            return false;
        }
        let mut seen = self
            .seen_message_ids
            .lock()
            .expect("sms seen message ids mutex poisoned");
        if seen.iter().any(|value| value == trimmed) {
            return true;
        }
        seen.push_back(trimmed.to_string());
        while seen.len() > 2_000 {
            seen.pop_front();
        }
        false
    }
}

impl ChannelAdapter for SmsAdapter {
    fn start(&self, _inbound: GatewayInboundDispatch) -> Result<()> {
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
        if request.path == SMS_TWILIO_PATH
            || request.path == SMS_EVENTS_PATH
            || request.path == "/webhooks/twilio"
        {
            return self.handle_sms_webhook(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let formatted = strip_sms_markdown(&message.text);
        let chunks = sms_chunks(&formatted);
        if chunks.is_empty() && !message.media_paths.is_empty() {
            self.send_sms(&route.key.conversation_id, "", &message.media_paths)?;
            return Ok(());
        }
        for (idx, chunk) in chunks.iter().enumerate() {
            let media = if idx == 0 {
                message.media_paths.as_slice()
            } else {
                &[]
            };
            self.send_sms(&route.key.conversation_id, chunk, media)?;
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
        self.send_sms(
            &route.key.conversation_id,
            &format!(
                "{}\n\nCommands:\n/approve {} once\n/approve {} session\n/approve {} always\n/deny {}",
                strip_sms_markdown(&prompt.message),
                prompt.id,
                prompt.id,
                prompt.id,
                prompt.id
            ),
            &[],
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

fn parse_form(body: &[u8]) -> HashMap<String, String> {
    form_urlencoded::parse(body)
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn sms_event_values(value: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    collect_sms_event_values(value, &mut out, 0);
    if out.is_empty() && !value.is_array() {
        out.push(value);
    }
    out
}

fn collect_sms_event_values<'a>(value: &'a Value, out: &mut Vec<&'a Value>, depth: usize) {
    if depth > 6 {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                collect_sms_event_values(item, out, depth + 1);
            }
        }
        Value::Object(object) => {
            if object.keys().any(|key| is_sms_payload_key(key)) {
                out.push(value);
                return;
            }
            for key in [
                "sms", "message", "event", "payload", "data", "record", "item",
            ] {
                if let Some(next) = object.get(key) {
                    collect_sms_event_values(next, out, depth + 1);
                }
            }
            for key in [
                "messages",
                "events",
                "records",
                "items",
                "notifications",
                "sms_messages",
            ] {
                if let Some(next) = object.get(key) {
                    collect_sms_event_values(next, out, depth + 1);
                }
            }
        }
        _ => {}
    }
}

fn is_sms_payload_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "from"
            | "from_number"
            | "fromnumber"
            | "sender"
            | "sender_number"
            | "phone"
            | "body"
            | "text"
            | "content"
            | "messagesid"
            | "smsmessagesid"
            | "message_sid"
            | "sid"
            | "nummedia"
            | "num_media"
            | "attachments"
            | "files"
            | "media"
    )
}

fn is_sms_status_value(value: &Value) -> bool {
    json_string_value(&[
        &value["MessageStatus"],
        &value["SmsStatus"],
        &value["message_status"],
        &value["status"],
    ])
    .is_some()
        && json_string_value(&[
            &value["Body"],
            &value["body"],
            &value["Text"],
            &value["text"],
            &value["Message"],
            &value["message"],
            &value["content"],
        ])
        .is_none()
        && sms_attachment_values(value).is_empty()
        && json_string_value(&[&value["NumMedia"], &value["numMedia"], &value["num_media"]])
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0)
            == 0
}

fn sms_attachment_values(value: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    for key in [
        "attachments",
        "Attachments",
        "attachment",
        "files",
        "Files",
        "media",
        "Media",
        "mms",
        "images",
    ] {
        match value.get(key) {
            Some(Value::Array(items)) => out.extend(items.iter()),
            Some(Value::Object(_)) | Some(Value::String(_)) => out.push(&value[key]),
            _ => {}
        }
    }
    out
}

fn json_object_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(|value| json_string_value(&[value]))
}

fn json_string_value(values: &[&Value]) -> Option<String> {
    values.iter().find_map(|value| match value {
        Value::String(value) => {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Object(object) => [
            "value", "text", "body", "content", "url", "path", "phone", "number", "id", "sid",
        ]
        .iter()
        .find_map(|key| object.get(*key))
        .and_then(|value| json_string_value(&[value])),
        _ => None,
    })
}

fn form_value(form: &HashMap<String, String>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| form.get(*key))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn media_count(form: &HashMap<String, String>) -> usize {
    form_value(form, &["NumMedia", "numMedia", "num_media"])
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| {
            (0..20)
                .take_while(|index| {
                    form.contains_key(&format!("MediaUrl{index}"))
                        || form.contains_key(&format!("mediaUrl{index}"))
                        || form.contains_key(&format!("media_url_{index}"))
                })
                .count()
        })
}

fn is_twilio_status_callback(form: &HashMap<String, String>) -> bool {
    form_value(
        form,
        &["MessageStatus", "SmsStatus", "message_status", "status"],
    )
    .is_some()
        && form_value(form, &["Body", "body"]).is_none()
        && media_count(form) == 0
}

fn check_twilio_signature(
    url: &str,
    form: &HashMap<String, String>,
    signature: &str,
    auth_token: &str,
) -> bool {
    let mut data = url.to_string();
    let mut keys = form.keys().collect::<Vec<_>>();
    keys.sort();
    for key in keys {
        data.push_str(key);
        if let Some(value) = form.get(key) {
            data.push_str(value);
        }
    }
    let computed = base64::engine::general_purpose::STANDARD
        .encode(hmac_sha1(auth_token.as_bytes(), data.as_bytes()));
    constant_time_eq(computed.as_bytes(), signature.as_bytes())
}

fn hmac_sha1(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut key_block = [0u8; 64];
    if key.len() > 64 {
        let digest = Sha1::digest(key);
        key_block[..20].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut outer = [0x5c_u8; 64];
    let mut inner = [0x36_u8; 64];
    for index in 0..64 {
        outer[index] ^= key_block[index];
        inner[index] ^= key_block[index];
    }
    let mut inner_hash = Sha1::new();
    inner_hash.update(inner);
    inner_hash.update(data);
    let inner_digest = inner_hash.finalize();
    let mut outer_hash = Sha1::new();
    outer_hash.update(outer);
    outer_hash.update(inner_digest);
    outer_hash.finalize().to_vec()
}

fn port_variant_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let default_port = match parsed.scheme() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let mut next = parsed.clone();
    if parsed.port() == Some(default_port) {
        next.set_port(None).ok()?;
    } else if parsed.port().is_none() {
        next.set_port(Some(default_port)).ok()?;
    } else {
        return None;
    }
    Some(next.to_string())
}

fn sms_chunks(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in trimmed.chars() {
        if current.chars().count() >= SMS_TEXT_LIMIT {
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

fn normalize_sms_number(value: &str) -> String {
    let trimmed = value.trim();
    let mut out = String::new();
    for (index, ch) in trimmed.chars().enumerate() {
        if ch == '+' && index == 0 {
            out.push(ch);
        } else if ch.is_ascii_digit() {
            out.push(ch);
        }
    }
    if out.is_empty() {
        trimmed.to_string()
    } else {
        out
    }
}

fn strip_sms_markdown(value: &str) -> String {
    let mut text = value.to_string();
    text = regex::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)")
        .expect("valid sms markdown link regex")
        .replace_all(&text, "$1 ($2)")
        .to_string();
    text = regex::Regex::new(r"[*_`#>]+")
        .expect("valid sms markdown marker regex")
        .replace_all(&text, "")
        .to_string();
    text = regex::Regex::new(r"\n{3,}")
        .expect("valid sms newline regex")
        .replace_all(&text, "\n\n")
        .to_string();
    text.trim().to_string()
}

fn extension_for_mime(mime: &str) -> &'static str {
    match mime.to_ascii_lowercase().as_str() {
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/heic" => ".heic",
        "image/heif" => ".heic",
        "audio/mpeg" => ".mp3",
        "audio/mp3" => ".mp3",
        "audio/mp4" => ".m4a",
        "audio/aac" => ".m4a",
        "audio/ogg" => ".ogg",
        "audio/wav" => ".wav",
        "video/mp4" => ".mp4",
        "video/quicktime" => ".mov",
        "application/pdf" => ".pdf",
        _ => ".bin",
    }
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

fn xml_response(status: u16) -> ChannelHttpResponse {
    ChannelHttpResponse {
        status,
        content_type: "application/xml",
        body: EMPTY_TWIML.as_bytes().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sms_form_parses() {
        let form = parse_form(b"From=%2B15551234567&To=%2B15557654321&Body=hello&MessageSid=SM1");
        assert_eq!(form.get("From").map(String::as_str), Some("+15551234567"));
        assert_eq!(form.get("Body").map(String::as_str), Some("hello"));
    }

    #[test]
    fn sms_signature_matches_twilio_formula() {
        let mut form = HashMap::new();
        form.insert("From".to_string(), "+15551234567".to_string());
        form.insert("Body".to_string(), "hello".to_string());
        let url = "https://example.com/sms/twilio";
        let sig = base64::engine::general_purpose::STANDARD.encode(hmac_sha1(
            b"token",
            b"https://example.com/sms/twilioBodyhelloFrom+15551234567",
        ));
        assert!(check_twilio_signature(url, &form, &sig, "token"));
    }

    #[test]
    fn sms_chunks_long_text() {
        let chunks = sms_chunks(&"x".repeat(SMS_TEXT_LIMIT + 1));
        assert_eq!(chunks.len(), 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.chars().count() <= SMS_TEXT_LIMIT)
        );
    }

    #[test]
    fn sms_adapter_requires_from_number() {
        let result = SmsAdapter::new(
            &GatewayChannelConfig {
                ..Default::default()
            },
            &GatewayCredentialEntry {
                channel: "sms".to_string(),
                app_id: Some("sid".to_string()),
                token: Some("token".to_string()),
                ..Default::default()
            },
        );
        assert!(result.is_err());
    }
}
