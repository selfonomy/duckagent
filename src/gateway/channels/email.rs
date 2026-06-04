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
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::Duration;
use url::form_urlencoded;
use uuid::Uuid;

const EMAIL_TEXT_LIMIT: usize = 50_000;
const DEFAULT_EMAIL_EVENTS_PATH: &str = "/email/events";
const EMAIL_DEDUPE_LIMIT: usize = 2_000;
const DEFAULT_IMAP_PORT: u16 = 993;
const DEFAULT_SMTP_PORT: u16 = 587;
const DEFAULT_EMAIL_POLL_SECONDS: u64 = 30;

#[derive(Clone)]
pub(in crate::gateway) struct EmailAdapter {
    address: String,
    api_base: Option<String>,
    api_token: Option<String>,
    allowed_users: HashSet<String>,
    skip_attachments: bool,
    max_download_bytes: u64,
    send_endpoint: String,
    webhook_secret: Option<String>,
    client: Client,
    thread_context: std::sync::Arc<std::sync::Mutex<HashMap<String, EmailThreadContext>>>,
    seen_message_ids: std::sync::Arc<std::sync::Mutex<VecDeque<String>>>,
    password: Option<String>,
    imap_host: Option<String>,
    imap_port: u16,
    smtp_host: Option<String>,
    smtp_port: u16,
    poll_interval: Duration,
    direct_imap_smtp: bool,
}

#[derive(Debug, Clone)]
struct EmailThreadContext {
    subject: String,
    message_id: Option<String>,
}

#[derive(Debug, Clone)]
struct ParsedEmail {
    from: String,
    from_name: Option<String>,
    subject: String,
    message_id: Option<String>,
    in_reply_to: Option<String>,
    text: String,
    attachments: Vec<InboundAttachmentInput>,
    automated: bool,
}

impl EmailAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let address = credentials
            .username
            .as_deref()
            .or(credentials.extra.get("address").map(String::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("email gateway credentials require username/address"))?
            .to_ascii_lowercase();
        let client = Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .context("failed to build email provider HTTP client")?;
        let transport = config.transport.as_deref().unwrap_or("provider_http");
        let direct_imap_smtp = matches!(transport, "direct_imap_smtp" | "imap_smtp");
        let imap_host = config.extra.get("imap_host").cloned();
        let imap_port = config
            .extra
            .get("imap_port")
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(DEFAULT_IMAP_PORT);
        let smtp_host = config.extra.get("smtp_host").cloned();
        let smtp_port = config
            .extra
            .get("smtp_port")
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(DEFAULT_SMTP_PORT);
        let poll_interval = Duration::from_secs(
            config
                .extra
                .get("poll_seconds")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(DEFAULT_EMAIL_POLL_SECONDS)
                .clamp(10, 3600),
        );
        Ok(Self {
            address,
            api_base: config.api_base.clone(),
            api_token: credentials
                .token
                .as_deref()
                .or(credentials.api_key.as_deref())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            allowed_users: config
                .allowed_users
                .iter()
                .map(|value| value.to_ascii_lowercase())
                .collect(),
            skip_attachments: config
                .extra
                .get("skip_attachments")
                .is_some_and(|value| value.trim() == "true"),
            max_download_bytes: config.media.max_download_bytes,
            send_endpoint: config
                .extra
                .get("send_endpoint")
                .cloned()
                .unwrap_or_else(|| "/send".to_string()),
            webhook_secret: credentials.webhook_secret.clone(),
            client,
            thread_context: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            seen_message_ids: std::sync::Arc::new(std::sync::Mutex::new(VecDeque::new())),
            password: credentials.password.clone(),
            imap_host,
            imap_port,
            smtp_host,
            smtp_port,
            poll_interval,
            direct_imap_smtp,
        })
    }

    fn start_direct_imap_polling(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        self.password
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("email direct_imap_smtp requires mailbox password"))?;
        self.imap_host
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("email direct_imap_smtp requires imap_host"))?;
        self.smtp_host
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("email direct_imap_smtp requires smtp_host"))?;
        let adapter = self.clone();
        thread::Builder::new()
            .name("gateway-email-imap".to_string())
            .spawn(move || adapter.run_direct_imap_loop(inbound))
            .context("failed to start email IMAP polling thread")?;
        Ok(())
    }

    fn run_direct_imap_loop(self, inbound: GatewayInboundDispatch) {
        loop {
            if let Err(error) = self.poll_imap_once(&inbound) {
                eprintln!("Email IMAP poll failed: {error:#}");
            }
            thread::sleep(self.poll_interval);
        }
    }

    fn poll_imap_once(&self, inbound: &GatewayInboundDispatch) -> Result<()> {
        let host = self
            .imap_host
            .as_deref()
            .ok_or_else(|| anyhow!("email direct_imap_smtp requires imap_host"))?;
        let password = self
            .password
            .as_deref()
            .ok_or_else(|| anyhow!("email direct_imap_smtp requires mailbox password"))?;
        let tcp = TcpStream::connect((host, self.imap_port))
            .with_context(|| format!("failed to connect IMAP {host}:{}", self.imap_port))?;
        tcp.set_read_timeout(Some(Duration::from_secs(30))).ok();
        tcp.set_write_timeout(Some(Duration::from_secs(30))).ok();
        let mut stream = tls_client_stream(host, tcp)?;
        let _ = read_protocol_line(&mut stream)?;
        imap_command(
            &mut stream,
            "a001",
            &format!(
                "LOGIN {} {}",
                imap_quote(&self.address),
                imap_quote(password)
            ),
        )?;
        imap_command(&mut stream, "a002", "SELECT INBOX")?;
        let search_lines = imap_command(&mut stream, "a003", "UID SEARCH UNSEEN")?;
        let mut uids = Vec::new();
        for line in search_lines {
            if let Some(rest) = line.strip_prefix("* SEARCH") {
                uids.extend(rest.split_whitespace().map(str::to_string));
            }
        }
        for uid in uids.into_iter().take(20) {
            let raw = imap_fetch_rfc822(&mut stream, &uid)?;
            let parsed = parse_raw_email(
                &String::from_utf8_lossy(&raw),
                self.skip_attachments,
                self.max_download_bytes,
            )?;
            self.accept_direct_email(parsed, inbound)?;
            let _ = imap_command(
                &mut stream,
                "a005",
                &format!("UID STORE {uid} +FLAGS.SILENT (\\Seen)"),
            );
        }
        let _ = imap_command(&mut stream, "a999", "LOGOUT");
        Ok(())
    }

    fn accept_direct_email(
        &self,
        email: ParsedEmail,
        inbound: &GatewayInboundDispatch,
    ) -> Result<()> {
        if email.from == self.address || email.automated {
            return Ok(());
        }
        if email
            .message_id
            .as_deref()
            .is_some_and(|message_id| self.is_duplicate(message_id))
        {
            return Ok(());
        }
        if !self.allowed_users.is_empty() && !self.allowed_users.contains(&email.from) {
            return Ok(());
        }
        self.thread_context
            .lock()
            .expect("email thread context mutex poisoned")
            .insert(
                email.from.clone(),
                EmailThreadContext {
                    subject: email.subject.clone(),
                    message_id: email.message_id.clone(),
                },
            );
        inbound.submit(InboundMessageInput {
            channel: "email".to_string(),
            conversation_id: email.from.clone(),
            thread_id: email.in_reply_to.clone(),
            chat_type: Some("dm".to_string()),
            sender_id: Some(email.from.clone()),
            message_id: email.message_id.clone(),
            text: email_message_text(&email),
            attachments: email.attachments,
            timestamp: None,
        })
    }

    fn handle_email_webhook(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        if !self.verify_webhook(&request) {
            return Ok(json_response(401, json!({"error": "unauthorized"})));
        }
        let emails = parse_email_payloads(
            &request.body,
            self.skip_attachments,
            self.max_download_bytes,
        )?;
        let mut accepted = 0usize;
        let mut ignored = 0usize;
        for email in emails {
            if email.from == self.address || email.automated {
                ignored += 1;
                continue;
            }
            if email
                .message_id
                .as_deref()
                .is_some_and(|message_id| self.is_duplicate(message_id))
            {
                ignored += 1;
                continue;
            }
            if !self.allowed_users.is_empty() && !self.allowed_users.contains(&email.from) {
                ignored += 1;
                continue;
            }
            self.thread_context
                .lock()
                .expect("email thread context mutex poisoned")
                .insert(
                    email.from.clone(),
                    EmailThreadContext {
                        subject: email.subject.clone(),
                        message_id: email.message_id.clone(),
                    },
                );
            let text = email_message_text(&email);
            inbound.submit(InboundMessageInput {
                channel: "email".to_string(),
                conversation_id: email.from.clone(),
                thread_id: email.in_reply_to.clone(),
                chat_type: Some("dm".to_string()),
                sender_id: Some(email.from.clone()),
                message_id: email.message_id,
                text,
                attachments: email.attachments,
                timestamp: None,
            })?;
            accepted += 1;
        }
        Ok(json_response(
            200,
            json!({"ok": true, "accepted": accepted, "ignored": ignored}),
        ))
    }

    fn verify_webhook(&self, request: &ChannelHttpRequest) -> bool {
        let Some(secret) = self.webhook_secret.as_deref() else {
            return true;
        };
        let candidate = request
            .header("x-duckagent-email-secret")
            .or_else(|| request.header("x-email-secret"))
            .or_else(|| request.query.get("secret").map(String::as_str));
        candidate.is_some_and(|value| constant_time_eq(value.as_bytes(), secret.as_bytes()))
    }

    fn send_email(&self, route: &GatewayRoute, text: &str, media_paths: &[String]) -> Result<()> {
        if self.direct_imap_smtp {
            return self.send_smtp_email(route, text, media_paths);
        }
        let api_base = self
            .api_base
            .as_deref()
            .ok_or_else(|| anyhow!("email channel requires api_base provider send endpoint"))?;
        let to = &route.key.conversation_id;
        let ctx = self
            .thread_context
            .lock()
            .expect("email thread context mutex poisoned")
            .get(to)
            .cloned();
        let subject = ctx
            .as_ref()
            .map(|ctx| reply_subject(&ctx.subject))
            .unwrap_or_else(|| "Re: DuckAgent".to_string());
        let mut body = text.to_string();
        let mut media_links = Vec::new();
        let mut local_files = Vec::new();
        for path in media_paths {
            if path.starts_with("http://") || path.starts_with("https://") {
                media_links.push(path.clone());
            } else {
                local_files.push(path.clone());
            }
        }
        if !media_links.is_empty() {
            body.push_str("\n\n");
            body.push_str(&media_links.join("\n"));
        }
        let mut request = self.client.post(format!(
            "{}{}",
            api_base.trim_end_matches('/'),
            ensure_leading_slash(&self.send_endpoint)
        ));
        if let Some(token) = self.api_token.as_deref() {
            request = request.bearer_auth(token);
        }
        let response = request
            .json(&json!({
                "from": self.address,
                "to": to,
                "subject": subject,
                "text": body,
                "in_reply_to": route.key.thread_id.as_deref().or_else(|| ctx.as_ref().and_then(|ctx| ctx.message_id.as_deref())),
                "references": ctx.as_ref().and_then(|ctx| ctx.message_id.as_deref()),
                "attachments": local_files,
                "message_id": format!("<duckagent-{}@local>", Uuid::now_v7().simple()),
            }))
            .send()
            .context("email provider send request failed")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            bail!("email provider send failed with status {status}: {body}");
        }
        Ok(())
    }

    fn send_smtp_email(
        &self,
        route: &GatewayRoute,
        text: &str,
        media_paths: &[String],
    ) -> Result<()> {
        let host = self
            .smtp_host
            .as_deref()
            .ok_or_else(|| anyhow!("email direct_imap_smtp requires smtp_host"))?;
        let password = self
            .password
            .as_deref()
            .ok_or_else(|| anyhow!("email direct_imap_smtp requires mailbox password"))?;
        let to = &route.key.conversation_id;
        let ctx = self
            .thread_context
            .lock()
            .expect("email thread context mutex poisoned")
            .get(to)
            .cloned();
        let subject = ctx
            .as_ref()
            .map(|ctx| reply_subject(&ctx.subject))
            .unwrap_or_else(|| "Re: DuckAgent".to_string());
        let mut body = text.to_string();
        if !media_paths.is_empty() {
            body.push_str("\n\nAttachments/links:\n");
            body.push_str(&media_paths.join("\n"));
        }
        smtp_send(
            host,
            self.smtp_port,
            &self.address,
            password,
            to,
            &subject,
            &body,
            route
                .key
                .thread_id
                .as_deref()
                .or_else(|| ctx.as_ref().and_then(|ctx| ctx.message_id.as_deref())),
            ctx.as_ref().and_then(|ctx| ctx.message_id.as_deref()),
        )
    }

    fn is_duplicate(&self, message_id: &str) -> bool {
        let trimmed = message_id.trim();
        if trimmed.is_empty() {
            return false;
        }
        let mut seen = self
            .seen_message_ids
            .lock()
            .expect("email seen message ids mutex poisoned");
        if seen.iter().any(|value| value == trimmed) {
            return true;
        }
        seen.push_back(trimmed.to_string());
        while seen.len() > EMAIL_DEDUPE_LIMIT {
            seen.pop_front();
        }
        false
    }
}

impl ChannelAdapter for EmailAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        if self.direct_imap_smtp {
            self.start_direct_imap_polling(inbound)?;
        }
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
        if request.path == DEFAULT_EMAIL_EVENTS_PATH || request.path == "/email/webhook" {
            return self.handle_email_webhook(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let mut chunks = email_chunks(&message.text);
        if message.media_paths.is_empty() {
            for chunk in chunks {
                self.send_email(route, &chunk, &[])?;
            }
            return Ok(());
        }

        if chunks.is_empty() {
            self.send_email(route, "", &message.media_paths)?;
            return Ok(());
        }

        let first = chunks.remove(0);
        self.send_email(route, &first, &message.media_paths)?;
        for chunk in chunks {
            self.send_email(route, &chunk, &[])?;
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
        self.send_email(
            route,
            &format!(
                "{}\n\nCommands:\n/approve {} once\n/approve {} session\n/approve {} always\n/deny {}",
                prompt.message, prompt.id, prompt.id, prompt.id, prompt.id
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

fn parse_email_payload(
    body: &[u8],
    skip_attachments: bool,
    max_download_bytes: u64,
) -> Result<ParsedEmail> {
    parse_email_payloads(body, skip_attachments, max_download_bytes)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("email payload did not contain an email event"))
}

fn parse_email_payloads(
    body: &[u8],
    skip_attachments: bool,
    max_download_bytes: u64,
) -> Result<Vec<ParsedEmail>> {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        let candidates = email_event_values(&value);
        let mut parsed = Vec::new();
        let mut first_error = None;
        for candidate in candidates {
            match parse_email_value(candidate, skip_attachments, max_download_bytes) {
                Ok(email) => parsed.push(email),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        if !parsed.is_empty() {
            return Ok(parsed);
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        return Ok(Vec::new());
    }
    let body_text = String::from_utf8_lossy(body);
    let pairs = form_urlencoded::parse(body_text.as_bytes()).collect::<Vec<_>>();
    if pairs.iter().any(|(key, _)| is_email_payload_key(key)) {
        let mut map = serde_json::Map::new();
        for (key, value) in pairs {
            map.insert(key.to_string(), Value::String(value.to_string()));
        }
        return parse_email_value(&Value::Object(map), skip_attachments, max_download_bytes)
            .map(|email| vec![email]);
    }
    parse_raw_email(&body_text, skip_attachments, max_download_bytes).map(|email| vec![email])
}

fn parse_email_value(
    value: &Value,
    skip_attachments: bool,
    max_download_bytes: u64,
) -> Result<ParsedEmail> {
    let headers = headers_from_value(value.get("headers").or_else(|| value.get("Headers")));
    let from_raw = string_value(&[
        &value["from"],
        &value["sender"],
        &value["from_email"],
        &value["sender_email"],
        &value["From"],
    ])
    .or_else(|| header_value(&headers, "from"))
    .ok_or_else(|| anyhow!("email payload missing from"))?;
    let from = extract_email_address(&from_raw);
    let text = string_value(&[
        &value["text"],
        &value["body"],
        &value["plain"],
        &value["body-plain"],
        &value["body_plain"],
        &value["stripped-text"],
        &value["stripped_text"],
        &value["TextBody"],
        &value["text_body"],
        &value["html"],
        &value["body-html"],
        &value["body_html"],
        &value["HtmlBody"],
        &value["html_body"],
    ])
    .map(|text| strip_html(&text))
    .unwrap_or_default();
    let mut attachments = Vec::new();
    if !skip_attachments {
        for item in attachment_values(value) {
            let bytes = string_value(&[
                &item["bytes"],
                &item["base64"],
                &item["content"],
                &item["content_base64"],
                &item["data"],
            ])
            .and_then(|encoded| {
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .ok()
            });
            if bytes
                .as_ref()
                .is_some_and(|bytes| bytes.len() as u64 > max_download_bytes)
            {
                continue;
            }
            if item.is_string() {
                attachments.push(InboundAttachmentInput {
                    bytes: None,
                    path: item.as_str().map(str::to_string),
                    filename: None,
                    mime: None,
                });
                continue;
            }
            attachments.push(InboundAttachmentInput {
                bytes,
                path: string_value(&[
                    &item["path"],
                    &item["url"],
                    &item["download_url"],
                    &item["downloadUrl"],
                    &item["content_url"],
                    &item["contentUrl"],
                ]),
                filename: string_value(&[
                    &item["filename"],
                    &item["Filename"],
                    &item["name"],
                    &item["Name"],
                    &item["file_name"],
                    &item["fileName"],
                ])
                .map(|value| decode_rfc2047_header(&value)),
                mime: string_value(&[
                    &item["mime"],
                    &item["mimetype"],
                    &item["mime_type"],
                    &item["content_type"],
                    &item["contentType"],
                    &item["ContentType"],
                ]),
            });
        }
    }
    let automated = automated_sender(&from) || automated_headers(&headers);
    Ok(ParsedEmail {
        from,
        from_name: string_value(&[&value["from_name"], &value["sender_name"]]),
        subject: string_value(&[&value["subject"], &value["Subject"]])
            .or_else(|| header_value(&headers, "subject"))
            .map(|value| decode_rfc2047_header(&value))
            .unwrap_or_default(),
        message_id: string_value(&[
            &value["message_id"],
            &value["message-id"],
            &value["Message-ID"],
            &value["MessageId"],
            &value["MessageID"],
        ])
        .or_else(|| header_value(&headers, "message-id")),
        in_reply_to: string_value(&[
            &value["in_reply_to"],
            &value["in-reply-to"],
            &value["In-Reply-To"],
            &value["references"],
            &value["References"],
        ])
        .or_else(|| header_value(&headers, "in-reply-to"))
        .or_else(|| header_value(&headers, "references")),
        text,
        attachments,
        automated,
    })
}

fn parse_raw_email(
    raw: &str,
    skip_attachments: bool,
    max_download_bytes: u64,
) -> Result<ParsedEmail> {
    let (headers_text, body) = split_raw_message(raw);
    let headers = parse_headers(headers_text);
    let from_raw =
        header_value(&headers, "from").ok_or_else(|| anyhow!("email payload missing from"))?;
    let from = extract_email_address(&from_raw);
    let subject = header_value(&headers, "subject")
        .map(|value| decode_rfc2047_header(&value))
        .unwrap_or_default();
    let message_id = header_value(&headers, "message-id");
    let in_reply_to = header_value(&headers, "in-reply-to");
    let content_type =
        header_value(&headers, "content-type").unwrap_or_else(|| "text/plain".to_string());
    let transfer_encoding = header_value(&headers, "content-transfer-encoding").unwrap_or_default();
    let mut text = String::new();
    let mut html_fallback = String::new();
    let mut attachments = Vec::new();
    if content_type.to_ascii_lowercase().contains("multipart/") {
        if let Some(boundary) = header_param(&content_type, "boundary") {
            for part in multipart_parts(body, &boundary) {
                let (part_headers_text, part_body) = split_raw_message(part);
                let part_headers = parse_headers(part_headers_text);
                let part_content_type = header_value(&part_headers, "content-type")
                    .unwrap_or_else(|| "text/plain".to_string());
                let part_disposition =
                    header_value(&part_headers, "content-disposition").unwrap_or_default();
                let part_transfer_encoding =
                    header_value(&part_headers, "content-transfer-encoding").unwrap_or_default();
                let is_attachment = part_disposition.to_ascii_lowercase().contains("attachment")
                    || part_disposition.to_ascii_lowercase().contains("inline")
                    || header_param(&part_disposition, "filename").is_some()
                    || header_param(&part_content_type, "name").is_some();
                let decoded = decode_transfer(part_body, &part_transfer_encoding);
                let content_type_lower = part_content_type.to_ascii_lowercase();
                if !is_attachment && content_type_lower.starts_with("text/plain") && text.is_empty()
                {
                    text = String::from_utf8_lossy(&decoded).trim().to_string();
                } else if !is_attachment
                    && content_type_lower.starts_with("text/html")
                    && html_fallback.is_empty()
                {
                    html_fallback = strip_html(&String::from_utf8_lossy(&decoded));
                } else if !skip_attachments {
                    push_raw_attachment(
                        &mut attachments,
                        decoded,
                        &part_content_type,
                        &part_disposition,
                        max_download_bytes,
                    );
                }
            }
        }
    } else {
        let decoded = decode_transfer(body, &transfer_encoding);
        if content_type.to_ascii_lowercase().starts_with("text/html") {
            text = strip_html(&String::from_utf8_lossy(&decoded));
        } else {
            text = String::from_utf8_lossy(&decoded).trim().to_string();
        }
    }
    if text.is_empty() {
        text = html_fallback;
    }
    let automated = automated_sender(&from) || automated_headers(&headers);
    Ok(ParsedEmail {
        from,
        from_name: header_value(&headers, "from").and_then(|value| {
            let value = decode_rfc2047_header(&value);
            value
                .split_once('<')
                .map(|(name, _)| name.trim().trim_matches('"').to_string())
                .filter(|name| !name.is_empty())
        }),
        subject,
        message_id,
        in_reply_to,
        text,
        attachments,
        automated,
    })
}

fn extract_email_address(raw: &str) -> String {
    RegexEmail::extract(raw).unwrap_or_else(|| raw.trim().to_ascii_lowercase())
}

struct RegexEmail;

impl RegexEmail {
    fn extract(raw: &str) -> Option<String> {
        let lower = raw.trim().to_ascii_lowercase();
        if let (Some(start), Some(end)) = (lower.find('<'), lower.find('>')) {
            if start < end {
                return Some(lower[start + 1..end].trim().to_string());
            }
        }
        Some(lower).filter(|value| value.contains('@'))
    }
}

fn automated_sender(address: &str) -> bool {
    let lower = address.to_ascii_lowercase();
    [
        "noreply",
        "no-reply",
        "no_reply",
        "donotreply",
        "do-not-reply",
        "mailer-daemon",
        "postmaster",
        "bounce",
        "notifications@",
        "automated@",
        "auto-confirm",
        "auto-reply",
        "automailer",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
}

fn automated_headers(headers: &HashMap<String, String>) -> bool {
    header_value(headers, "auto-submitted").is_some_and(|value| !value.eq_ignore_ascii_case("no"))
        || header_value(headers, "precedence")
            .map(|value| value.to_ascii_lowercase())
            .is_some_and(|value| matches!(value.as_str(), "bulk" | "list" | "junk"))
        || header_value(headers, "x-auto-response-suppress").is_some()
        || header_value(headers, "list-unsubscribe").is_some()
}

fn email_message_text(email: &ParsedEmail) -> String {
    let mut text = email.text.trim().to_string();
    if text.is_empty() {
        text = if email.attachments.is_empty() {
            "(empty email)".to_string()
        } else {
            "(email attachment)".to_string()
        };
    }
    if !email.subject.trim().is_empty() && !is_reply_subject(&email.subject) {
        text = format!("[Subject: {}]\n\n{}", email.subject.trim(), text);
    }
    text
}

fn is_reply_subject(subject: &str) -> bool {
    subject.trim_start().to_ascii_lowercase().starts_with("re:")
}

fn strip_html(value: &str) -> String {
    let text = value
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    let text = regex::Regex::new(r"(?i)</?p[^>]*>")
        .expect("valid paragraph html regex")
        .replace_all(&text, "\n")
        .to_string();
    regex::Regex::new(r"<[^>]+>")
        .expect("valid html regex")
        .replace_all(&text, "")
        .trim()
        .to_string()
}

fn reply_subject(subject: &str) -> String {
    if is_reply_subject(subject) {
        subject.trim().to_string()
    } else if subject.trim().is_empty() {
        "Re: DuckAgent".to_string()
    } else {
        format!("Re: {}", subject.trim())
    }
}

trait ReadWrite: Read + Write {}

impl<T: Read + Write> ReadWrite for T {}

type TlsTcpStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

fn tls_client_stream(host: &str, tcp: TcpStream) -> Result<TlsTcpStream> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = rustls_pki_types::ServerName::try_from(host.to_string())
        .with_context(|| format!("invalid TLS server name {host}"))?;
    let connection = rustls::ClientConnection::new(std::sync::Arc::new(config), server_name)
        .context("failed to create TLS client connection")?;
    Ok(rustls::StreamOwned::new(connection, tcp))
}

fn read_protocol_line(stream: &mut dyn ReadWrite) -> Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream
            .read(&mut byte)
            .context("failed to read protocol line")?;
        if read == 0 {
            bail!("connection closed while reading protocol line");
        }
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        if bytes.len() > 16 * 1024 {
            bail!("protocol line too long");
        }
    }
    Ok(String::from_utf8_lossy(&bytes)
        .trim_end_matches(['\r', '\n'])
        .to_string())
}

fn write_protocol_line(stream: &mut dyn ReadWrite, line: &str) -> Result<()> {
    stream
        .write_all(line.as_bytes())
        .context("failed to write protocol line")?;
    stream
        .write_all(b"\r\n")
        .context("failed to write protocol line ending")?;
    stream.flush().context("failed to flush protocol line")
}

fn imap_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn imap_command(stream: &mut dyn ReadWrite, tag: &str, command: &str) -> Result<Vec<String>> {
    write_protocol_line(stream, &format!("{tag} {command}"))?;
    let mut lines = Vec::new();
    loop {
        let line = read_protocol_line(stream)?;
        let done = line.starts_with(tag);
        if done && !line.contains(" OK") {
            bail!("IMAP command failed: {line}");
        }
        lines.push(line);
        if done {
            break;
        }
    }
    Ok(lines)
}

fn imap_fetch_rfc822(stream: &mut dyn ReadWrite, uid: &str) -> Result<Vec<u8>> {
    let tag = "a004";
    write_protocol_line(stream, &format!("{tag} UID FETCH {uid} (RFC822)"))?;
    let mut raw = Vec::new();
    loop {
        let line = read_protocol_line(stream)?;
        if let Some(length) = imap_literal_length(&line) {
            raw.resize(length, 0);
            stream
                .read_exact(&mut raw)
                .context("failed to read IMAP RFC822 literal")?;
            continue;
        }
        if line.starts_with(tag) {
            if !line.contains(" OK") {
                bail!("IMAP fetch failed: {line}");
            }
            break;
        }
    }
    if raw.is_empty() {
        bail!("IMAP fetch returned no RFC822 payload");
    }
    Ok(raw)
}

fn imap_literal_length(line: &str) -> Option<usize> {
    let start = line.rfind('{')?;
    let end = line[start + 1..].find('}')? + start + 1;
    line[start + 1..end].parse().ok()
}

fn smtp_send(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    to: &str,
    subject: &str,
    body: &str,
    in_reply_to: Option<&str>,
    references: Option<&str>,
) -> Result<()> {
    let tcp = TcpStream::connect((host, port))
        .with_context(|| format!("failed to connect SMTP {host}:{port}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(30))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(30))).ok();
    if port == 465 {
        let mut stream = tls_client_stream(host, tcp)?;
        smtp_expect(&mut stream, 220)?;
        smtp_session(
            &mut stream,
            host,
            username,
            password,
            to,
            subject,
            body,
            in_reply_to,
            references,
        )
    } else {
        let mut stream = tcp;
        smtp_expect(&mut stream, 220)?;
        write_protocol_line(&mut stream, &format!("EHLO {host}"))?;
        smtp_expect_multiline(&mut stream, 250)?;
        write_protocol_line(&mut stream, "STARTTLS")?;
        smtp_expect(&mut stream, 220)?;
        let mut tls = tls_client_stream(host, stream)?;
        smtp_session(
            &mut tls,
            host,
            username,
            password,
            to,
            subject,
            body,
            in_reply_to,
            references,
        )
    }
}

fn smtp_session(
    stream: &mut dyn ReadWrite,
    host: &str,
    username: &str,
    password: &str,
    to: &str,
    subject: &str,
    body: &str,
    in_reply_to: Option<&str>,
    references: Option<&str>,
) -> Result<()> {
    write_protocol_line(stream, &format!("EHLO {host}"))?;
    smtp_expect_multiline(stream, 250)?;
    write_protocol_line(stream, "AUTH LOGIN")?;
    smtp_expect(stream, 334)?;
    write_protocol_line(
        stream,
        &base64::engine::general_purpose::STANDARD.encode(username),
    )?;
    smtp_expect(stream, 334)?;
    write_protocol_line(
        stream,
        &base64::engine::general_purpose::STANDARD.encode(password),
    )?;
    smtp_expect(stream, 235)?;
    write_protocol_line(stream, &format!("MAIL FROM:<{username}>"))?;
    smtp_expect(stream, 250)?;
    write_protocol_line(stream, &format!("RCPT TO:<{to}>"))?;
    smtp_expect_multiline(stream, 250)?;
    write_protocol_line(stream, "DATA")?;
    smtp_expect(stream, 354)?;
    let message = smtp_message(username, to, subject, body, in_reply_to, references);
    stream
        .write_all(dot_stuff(&message).as_bytes())
        .context("failed to write SMTP DATA")?;
    stream
        .write_all(b"\r\n.\r\n")
        .context("failed to finish SMTP DATA")?;
    stream.flush().context("failed to flush SMTP DATA")?;
    smtp_expect(stream, 250)?;
    let _ = write_protocol_line(stream, "QUIT");
    Ok(())
}

fn smtp_expect(stream: &mut dyn ReadWrite, code: u16) -> Result<String> {
    let line = read_protocol_line(stream)?;
    if !line.starts_with(&code.to_string()) {
        bail!("SMTP expected {code}, got {line}");
    }
    Ok(line)
}

fn smtp_expect_multiline(stream: &mut dyn ReadWrite, code: u16) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    loop {
        let line = smtp_expect(stream, code)?;
        let done = line
            .as_bytes()
            .get(3)
            .is_some_and(|separator| *separator == b' ');
        lines.push(line);
        if done {
            break;
        }
    }
    Ok(lines)
}

fn smtp_message(
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
    in_reply_to: Option<&str>,
    references: Option<&str>,
) -> String {
    let message_id = format!("<duckagent-{}@local>", Uuid::now_v7().simple());
    let mut headers = vec![
        format!("From: <{from}>"),
        format!("To: <{to}>"),
        format!("Subject: {}", sanitize_header(subject)),
        format!("Date: {}", chrono::Utc::now().to_rfc2822()),
        format!("Message-ID: {message_id}"),
        "MIME-Version: 1.0".to_string(),
        "Content-Type: text/plain; charset=utf-8".to_string(),
        "Content-Transfer-Encoding: 8bit".to_string(),
    ];
    if let Some(in_reply_to) = in_reply_to.filter(|value| !value.trim().is_empty()) {
        headers.push(format!("In-Reply-To: {}", sanitize_header(in_reply_to)));
    }
    if let Some(references) = references.filter(|value| !value.trim().is_empty()) {
        headers.push(format!("References: {}", sanitize_header(references)));
    }
    format!("{}\r\n\r\n{}", headers.join("\r\n"), normalize_crlf(body))
}

fn sanitize_header(value: &str) -> String {
    value.replace(['\r', '\n'], " ").trim().to_string()
}

fn normalize_crlf(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r\n")
}

fn dot_stuff(value: &str) -> String {
    normalize_crlf(value)
        .lines()
        .map(|line| {
            if line.starts_with('.') {
                format!(".{line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

fn email_chunks(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed.len() <= EMAIL_TEXT_LIMIT {
        return vec![trimmed.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in trimmed.chars() {
        if current.len() + character.len_utf8() > EMAIL_TEXT_LIMIT && !current.is_empty() {
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

fn is_email_payload_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "from"
            | "sender"
            | "headers"
            | "from_email"
            | "sender_email"
            | "subject"
            | "text"
            | "body"
            | "plain"
            | "html"
            | "body-plain"
            | "body_plain"
            | "body-html"
            | "body_html"
            | "stripped-text"
            | "stripped_text"
            | "textbody"
            | "htmlbody"
            | "text_body"
            | "html_body"
            | "message_id"
            | "message-id"
            | "in_reply_to"
            | "in-reply-to"
    )
}

fn email_event_values(value: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    collect_email_event_values(value, &mut out, 0);
    if out.is_empty() && !value.is_array() {
        out.push(value);
    }
    out
}

fn collect_email_event_values<'a>(value: &'a Value, out: &mut Vec<&'a Value>, depth: usize) {
    if depth > 6 {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                collect_email_event_values(item, out, depth + 1);
            }
        }
        Value::Object(object) => {
            if object.keys().any(|key| is_email_payload_key(key)) {
                out.push(value);
                return;
            }
            for key in [
                "email", "mail", "message", "event", "payload", "data", "record", "item",
            ] {
                if let Some(next) = object.get(key) {
                    collect_email_event_values(next, out, depth + 1);
                }
            }
            for key in [
                "emails",
                "mail",
                "messages",
                "events",
                "records",
                "items",
                "notifications",
            ] {
                if let Some(next) = object.get(key) {
                    collect_email_event_values(next, out, depth + 1);
                }
            }
        }
        _ => {}
    }
}

fn split_raw_message(raw: &str) -> (&str, &str) {
    raw.split_once("\r\n\r\n")
        .or_else(|| raw.split_once("\n\n"))
        .unwrap_or((raw, ""))
}

fn parse_headers(raw: &str) -> HashMap<String, String> {
    let mut headers: HashMap<String, String> = HashMap::new();
    let mut current_key = String::new();
    for line in raw.lines() {
        if line.starts_with(char::is_whitespace) && !current_key.is_empty() {
            if let Some(value) = headers.get_mut(&current_key) {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        current_key = key.trim().to_ascii_lowercase();
        headers.insert(current_key.clone(), value.trim().to_string());
    }
    headers
}

fn headers_from_value(value: Option<&Value>) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    match value {
        Some(Value::Object(object)) => {
            for (key, value) in object {
                if let Some(value) = string_value(&[value]) {
                    insert_header(&mut headers, key, &value);
                }
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                match item {
                    Value::Object(_) => {
                        let key = string_value(&[&item["name"], &item["key"], &item["header"]]);
                        let value = string_value(&[&item["value"], &item["Value"]]);
                        if let (Some(key), Some(value)) = (key, value) {
                            insert_header(&mut headers, &key, &value);
                        }
                    }
                    Value::Array(pair) if pair.len() >= 2 => {
                        let key = string_value(&[&pair[0]]);
                        let value = string_value(&[&pair[1]]);
                        if let (Some(key), Some(value)) = (key, value) {
                            insert_header(&mut headers, &key, &value);
                        }
                    }
                    _ => {}
                }
            }
        }
        Some(Value::String(raw)) => headers.extend(parse_headers(raw)),
        _ => {}
    }
    headers
}

fn insert_header(headers: &mut HashMap<String, String>, key: &str, value: &str) {
    let key = key.trim().to_ascii_lowercase();
    let value = value.trim();
    if !key.is_empty() && !value.is_empty() {
        headers.insert(key, value.to_string());
    }
}

fn header_value(headers: &HashMap<String, String>, key: &str) -> Option<String> {
    headers.get(&key.to_ascii_lowercase()).cloned()
}

fn header_param(header: &str, name: &str) -> Option<String> {
    let target = name.to_ascii_lowercase();
    for part in header.split(';').skip(1) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(&target) {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn multipart_parts<'a>(body: &'a str, boundary: &str) -> Vec<&'a str> {
    let marker = format!("--{boundary}");
    body.split(&marker)
        .skip(1)
        .filter_map(|part| {
            let trimmed = part.trim_start_matches(|ch| ch == '\r' || ch == '\n');
            if trimmed.starts_with("--") {
                return None;
            }
            Some(trimmed.trim_end_matches(|ch| ch == '\r' || ch == '\n'))
        })
        .collect()
}

fn decode_transfer(body: &str, transfer_encoding: &str) -> Vec<u8> {
    match transfer_encoding.trim().to_ascii_lowercase().as_str() {
        "base64" => {
            let compact = body
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>();
            base64::engine::general_purpose::STANDARD
                .decode(compact)
                .unwrap_or_else(|_| body.as_bytes().to_vec())
        }
        "quoted-printable" => decode_quoted_printable(body.as_bytes()),
        _ => body.as_bytes().to_vec(),
    }
}

fn decode_quoted_printable(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'=' {
            if input.get(index + 1) == Some(&b'\r') && input.get(index + 2) == Some(&b'\n') {
                index += 3;
                continue;
            }
            if input.get(index + 1) == Some(&b'\n') {
                index += 2;
                continue;
            }
            if let (Some(high), Some(low)) = (input.get(index + 1), input.get(index + 2)) {
                if let (Some(high), Some(low)) = (hex_value(*high), hex_value(*low)) {
                    out.push((high << 4) | low);
                    index += 3;
                    continue;
                }
            }
        }
        out.push(input[index]);
        index += 1;
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn push_raw_attachment(
    attachments: &mut Vec<InboundAttachmentInput>,
    bytes: Vec<u8>,
    content_type: &str,
    disposition: &str,
    max_download_bytes: u64,
) {
    if bytes.is_empty() || bytes.len() as u64 > max_download_bytes {
        return;
    }
    let filename = header_param(disposition, "filename")
        .or_else(|| header_param(content_type, "name"))
        .map(|value| decode_rfc2047_header(&value))
        .unwrap_or_else(|| "email-attachment.bin".to_string());
    let mime = content_type
        .split(';')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("application/octet-stream")
        .to_string();
    attachments.push(InboundAttachmentInput {
        bytes: Some(bytes),
        path: None,
        filename: Some(filename),
        mime: Some(mime),
    });
}

fn ensure_leading_slash(value: &str) -> String {
    if value.starts_with('/') {
        value.to_string()
    } else {
        format!("/{value}")
    }
}

fn string_value(values: &[&Value]) -> Option<String> {
    values.iter().find_map(|value| match value {
        Value::String(value) => {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Number(value) => Some(value.to_string()),
        Value::Object(object) => ["email", "address", "value", "name"]
            .iter()
            .find_map(|key| object.get(*key))
            .and_then(|value| string_value(&[value])),
        _ => None,
    })
}

fn attachment_values(value: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    for key in ["attachments", "Attachments", "attachment", "files", "Files"] {
        match value.get(key) {
            Some(Value::Array(items)) => out.extend(items.iter()),
            Some(Value::Object(_)) => out.push(&value[key]),
            _ => {}
        }
    }
    out
}

fn decode_rfc2047_header(raw: &str) -> String {
    let re =
        regex::Regex::new(r"=\?([^?]+)\?([bBqQ])\?([^?]*)\?=").expect("valid rfc2047 header regex");
    let decoded = re.replace_all(raw, |captures: &regex::Captures<'_>| {
        let encoding = captures
            .get(2)
            .map(|value| value.as_str().to_ascii_lowercase())
            .unwrap_or_default();
        let payload = captures.get(3).map(|value| value.as_str()).unwrap_or("");
        let bytes = if encoding == "b" {
            base64::engine::general_purpose::STANDARD
                .decode(payload)
                .unwrap_or_else(|_| payload.as_bytes().to_vec())
        } else {
            decode_quoted_printable(payload.replace('_', " ").as_bytes())
        };
        String::from_utf8_lossy(&bytes).to_string()
    });
    decoded.trim().to_string()
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
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"ok\":false}".to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_payload_parses_json_and_attachment() -> Result<()> {
        let parsed = parse_email_payload(
            br#"{"from":"Alice <Alice@Example.COM>","subject":"Hi","text":"hello","message_id":"m1","attachments":[{"filename":"a.txt","mime":"text/plain","bytes":"aGk="}]}"#,
            false,
            u64::MAX,
        )?;
        assert_eq!(parsed.from, "alice@example.com");
        assert_eq!(parsed.subject, "Hi");
        assert_eq!(parsed.attachments.len(), 1);
        Ok(())
    }

    #[test]
    fn email_strips_html() -> Result<()> {
        let parsed = parse_email_payload(
            b"from=bob%40example.com&subject=Hi&html=%3Cp%3EHello%26nbsp%3Bthere%3C%2Fp%3E",
            false,
            u64::MAX,
        )?;
        assert_eq!(parsed.text, "Hello there");
        Ok(())
    }

    #[test]
    fn email_reply_subject_is_stable() {
        assert_eq!(reply_subject("Hello"), "Re: Hello");
        assert_eq!(reply_subject("Re: Hello"), "Re: Hello");
    }
}
