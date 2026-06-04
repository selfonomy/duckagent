use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundMessageInput,
    OutboundMessage, TypingEvent,
};
use super::feishu_ws::{FeishuWsConfig, spawn_feishu_ws_loop};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use aes::Aes256;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use regex::Regex;
use reqwest::blocking::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::form_urlencoded;

const DEFAULT_FEISHU_API_BASE: &str = "https://open.feishu.cn";
const COMMENT_REPLY_LIMIT: usize = 4_000;
const COMMENT_QUERY_RETRY_LIMIT: usize = 6;
const ALLOWED_NOTICE_TYPES: &[&str] = &["add_comment", "add_reply"];
const NO_REPLY_SENTINEL: &str = "NO_REPLY";

type Aes256CbcDec = cbc::Decryptor<Aes256>;

#[derive(Clone)]
pub(in crate::gateway) struct FeishuCommentAdapter {
    channel: String,
    app_id: String,
    app_secret: String,
    verification_token: Option<String>,
    signing_secret: Option<String>,
    self_open_id: Option<String>,
    api_base: String,
    transport: String,
    allowed_users: HashSet<String>,
    allowed_docs: HashSet<String>,
    policy: CommentPolicy,
    require_mention: bool,
    client: Client,
    token: Arc<Mutex<Option<CachedFeishuToken>>>,
    routes: Arc<Mutex<HashMap<String, CommentDeliveryTarget>>>,
}

#[derive(Debug, Clone)]
struct CachedFeishuToken {
    value: String,
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentPolicy {
    Open,
    Allowlist,
    Disabled,
}

#[derive(Debug, Clone)]
struct CommentEvent {
    event_id: String,
    comment_id: String,
    reply_id: String,
    is_mentioned: bool,
    timestamp: Option<String>,
    file_token: String,
    file_type: String,
    notice_type: String,
    from_open_id: String,
    to_open_id: String,
}

#[derive(Debug, Clone)]
struct CommentDeliveryTarget {
    file_token: String,
    file_type: String,
    comment_id: String,
    reply_id: Option<String>,
    is_whole: bool,
}

impl FeishuCommentAdapter {
    pub(in crate::gateway) fn new(
        channel: &str,
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let app_id = credentials
            .app_id
            .as_deref()
            .or(credentials.api_key.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("feishu_comment gateway credential requires app_id"))?
            .to_string();
        let app_secret = credentials
            .app_secret
            .as_deref()
            .or(credentials.client_secret.as_deref())
            .or(credentials.token.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("feishu_comment gateway credential requires app_secret"))?
            .to_string();
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build Feishu comment HTTP client")?;
        Ok(Self {
            channel: channel.to_string(),
            app_id,
            app_secret,
            verification_token: credentials
                .webhook_secret
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            signing_secret: credentials
                .signing_secret
                .as_deref()
                .or_else(|| credentials.extra.get("encrypt_key").map(String::as_str))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            self_open_id: credentials
                .username
                .as_deref()
                .or_else(|| credentials.extra.get("self_open_id").map(String::as_str))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            api_base: config
                .api_base
                .clone()
                .unwrap_or_else(|| DEFAULT_FEISHU_API_BASE.to_string()),
            transport: config
                .transport
                .clone()
                .unwrap_or_else(|| "websocket".to_string())
                .to_ascii_lowercase(),
            allowed_users: config.allowed_users.iter().cloned().collect(),
            allowed_docs: config.allowed_chats.iter().cloned().collect(),
            policy: parse_policy(config.extra.get("policy").map(String::as_str))?,
            require_mention: config
                .extra
                .get("require_mention")
                .is_none_or(|value| value.trim() != "false"),
            client,
            token: Arc::new(Mutex::new(None)),
            routes: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn events_path(&self) -> String {
        format!("/{}/events", self.channel)
    }

    fn webhook_path(&self) -> String {
        format!("/{}/webhook", self.channel)
    }

    fn api_url(&self, path: &str, query: &[(&str, String)]) -> String {
        let mut url = format!("{}{}", self.api_base.trim_end_matches('/'), path);
        if !query.is_empty() {
            let mut serializer = form_urlencoded::Serializer::new(String::new());
            for (key, value) in query {
                serializer.append_pair(key, value);
            }
            url.push('?');
            url.push_str(&serializer.finish());
        }
        url
    }

    fn tenant_access_token(&self) -> Result<String> {
        {
            let guard = self
                .token
                .lock()
                .expect("feishu comment token mutex poisoned");
            if let Some(token) = guard.as_ref() {
                if token.expires_at > Instant::now() + Duration::from_secs(60) {
                    return Ok(token.value.clone());
                }
            }
        }

        let response = self
            .client
            .post(self.api_url("/open-apis/auth/v3/tenant_access_token/internal", &[]))
            .json(&json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .context("feishu comment tenant access token request failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .context("feishu comment tenant access token returned invalid JSON")?;
        if !status.is_success() || value["code"].as_i64().unwrap_or(-1) != 0 {
            bail!("feishu comment tenant access token failed with status {status}: {value}");
        }
        let token = value["tenant_access_token"]
            .as_str()
            .ok_or_else(|| anyhow!("feishu comment tenant_access_token missing"))?
            .to_string();
        let expire = value["expire"].as_u64().unwrap_or(7_200);
        *self
            .token
            .lock()
            .expect("feishu comment token mutex poisoned") = Some(CachedFeishuToken {
            value: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expire),
        });
        Ok(token)
    }

    fn api_get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let token = self.tenant_access_token()?;
        let response = self
            .client
            .get(self.api_url(path, query))
            .bearer_auth(token)
            .send()
            .with_context(|| format!("feishu comment GET {path} failed"))?;
        self.parse_feishu_response(response, path)
    }

    fn api_post<T: serde::Serialize>(
        &self,
        path: &str,
        query: &[(&str, String)],
        body: &T,
    ) -> Result<Value> {
        let token = self.tenant_access_token()?;
        let response = self
            .client
            .post(self.api_url(path, query))
            .bearer_auth(token)
            .json(body)
            .send()
            .with_context(|| format!("feishu comment POST {path} failed"))?;
        self.parse_feishu_response(response, path)
    }

    fn parse_feishu_response(
        &self,
        response: reqwest::blocking::Response,
        path: &str,
    ) -> Result<Value> {
        let status = response.status();
        let value: Value = response
            .json()
            .with_context(|| format!("feishu comment {path} returned invalid JSON"))?;
        if !status.is_success() || value["code"].as_i64().unwrap_or(-1) != 0 {
            bail!("feishu comment {path} failed with status {status}: {value}");
        }
        Ok(value["data"].clone())
    }

    fn handle_events(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        self.verify_signature(&request)?;
        let value = self.event_value_from_body(&request.body)?;
        self.verify_token(&value)?;
        if value["type"].as_str() == Some("url_verification") {
            let challenge = value["challenge"]
                .as_str()
                .ok_or_else(|| anyhow!("Feishu comment url_verification missing challenge"))?;
            return Ok(json_response(200, json!({"challenge": challenge})));
        }

        self.dispatch_event_value(&value, &inbound)?;
        Ok(json_response(200, json!({"code": 0, "msg": "ok"})))
    }

    fn event_value_from_body(&self, body: &[u8]) -> Result<Value> {
        let value: Value =
            serde_json::from_slice(body).context("failed to parse Feishu comment event JSON")?;
        let Some(encrypt) = value["encrypt"].as_str() else {
            return Ok(value);
        };
        let secret = self
            .signing_secret
            .as_deref()
            .ok_or_else(|| anyhow!("encrypted Feishu comment callback requires encrypt key"))?;
        let key = Sha256::digest(secret.as_bytes());
        let key_bytes: &[u8] = key.as_ref();
        let iv = &key_bytes[..16];
        let mut ciphertext = base64::engine::general_purpose::STANDARD
            .decode(encrypt)
            .context("Feishu comment encrypted callback is invalid base64")?;
        let decrypted = Aes256CbcDec::new_from_slices(key_bytes, iv)
            .context("failed to initialize Feishu comment decryptor")?
            .decrypt_padded_mut::<Pkcs7>(&mut ciphertext)
            .map_err(|_| anyhow!("Feishu comment encrypted callback decrypt failed"))?;
        serde_json::from_slice(decrypted).context("Feishu comment decrypted callback is not JSON")
    }

    fn dispatch_event_value(&self, value: &Value, inbound: &GatewayInboundDispatch) -> Result<()> {
        if feishu_event_type(value).as_deref() != Some("drive.notice.comment_add_v1") {
            return Ok(());
        }
        if let Some(event) = parse_comment_event(value) {
            if let Some(input) = self.comment_event_to_inbound(&event)? {
                inbound.submit(input)?;
            }
        }
        Ok(())
    }

    fn comment_event_to_inbound(
        &self,
        event: &CommentEvent,
    ) -> Result<Option<InboundMessageInput>> {
        if self.should_skip_event(event) {
            return Ok(None);
        }
        if !self.document_allowed(&event.file_type, &event.file_token) {
            return Ok(None);
        }
        if !self.user_allowed(&event.from_open_id) {
            return Ok(None);
        }

        if !event.reply_id.is_empty() {
            let _ = self.set_comment_reaction(event, "add");
        }
        let (prompt, is_whole) = self.build_comment_prompt(event)?;
        let conversation_id = comment_conversation_id(&event.file_type, &event.file_token);
        let route_key = delivery_route_key(&conversation_id, Some(&event.comment_id));
        self.routes
            .lock()
            .expect("feishu comment routes mutex poisoned")
            .insert(
                route_key,
                CommentDeliveryTarget {
                    file_token: event.file_token.clone(),
                    file_type: event.file_type.clone(),
                    comment_id: event.comment_id.clone(),
                    reply_id: (!event.reply_id.is_empty()).then(|| event.reply_id.clone()),
                    is_whole,
                },
            );
        Ok(Some(InboundMessageInput {
            channel: self.channel.clone(),
            conversation_id,
            thread_id: Some(event.comment_id.clone()),
            chat_type: Some("comment".to_string()),
            sender_id: Some(event.from_open_id.clone()),
            message_id: (!event.event_id.is_empty()).then(|| event.event_id.clone()),
            text: prompt,
            attachments: Vec::new(),
            timestamp: event.timestamp.clone(),
        }))
    }

    fn should_skip_event(&self, event: &CommentEvent) -> bool {
        if event.from_open_id.is_empty() || event.to_open_id.is_empty() {
            return true;
        }
        if self
            .self_open_id
            .as_deref()
            .is_some_and(|self_id| event.from_open_id == self_id || event.to_open_id != self_id)
        {
            return true;
        }
        if !ALLOWED_NOTICE_TYPES.contains(&event.notice_type.as_str()) {
            return true;
        }
        self.require_mention && !event.is_mentioned
    }

    fn document_allowed(&self, file_type: &str, file_token: &str) -> bool {
        if self.allowed_docs.is_empty() {
            return true;
        }
        let exact = format!("{file_type}:{file_token}");
        let type_wildcard = format!("{file_type}:*");
        self.allowed_docs.contains("*")
            || self.allowed_docs.contains(&exact)
            || self.allowed_docs.contains(&type_wildcard)
            || self.allowed_docs.contains(file_token)
    }

    fn user_allowed(&self, open_id: &str) -> bool {
        match self.policy {
            CommentPolicy::Open => true,
            CommentPolicy::Disabled => false,
            CommentPolicy::Allowlist => {
                self.allowed_users.contains("*") || self.allowed_users.contains(open_id)
            }
        }
    }

    fn build_comment_prompt(&self, event: &CommentEvent) -> Result<(String, bool)> {
        let meta = self.query_document_meta(&event.file_token, &event.file_type)?;
        let detail =
            self.batch_query_comment(&event.file_token, &event.file_type, &event.comment_id)?;
        let is_whole = detail["is_whole"].as_bool().unwrap_or(false);
        let doc_title = meta["title"].as_str().unwrap_or("Untitled");
        let doc_url = meta["url"].as_str().unwrap_or_default();
        let prompt = if is_whole {
            let comments = self.list_whole_comments(&event.file_token, &event.file_type)?;
            build_whole_prompt(
                event,
                comment_channel_label(&self.channel),
                doc_title,
                doc_url,
                &comments,
                self.self_open_id.as_deref(),
            )
        } else {
            let replies = self.list_comment_replies(
                &event.file_token,
                &event.file_type,
                &event.comment_id,
                &event.reply_id,
            )?;
            build_local_prompt(
                event,
                comment_channel_label(&self.channel),
                doc_title,
                doc_url,
                detail["quote"].as_str().unwrap_or_default(),
                &replies,
                self.self_open_id.as_deref(),
            )
        };
        Ok((prompt, is_whole))
    }

    fn query_document_meta(&self, file_token: &str, file_type: &str) -> Result<Value> {
        let data = self.api_post(
            "/open-apis/drive/v1/metas/batch_query",
            &[],
            &json!({
                "request_docs": [{"doc_token": file_token, "doc_type": file_type}],
                "with_url": true,
            }),
        )?;
        if let Some(items) = data["metas"].as_array() {
            return Ok(items.first().cloned().unwrap_or_else(|| json!({})));
        }
        Ok(data["metas"][file_token].clone())
    }

    fn batch_query_comment(
        &self,
        file_token: &str,
        file_type: &str,
        comment_id: &str,
    ) -> Result<Value> {
        let path = format!(
            "/open-apis/drive/v1/files/{}/comments/batch_query",
            encode_component(file_token)
        );
        for attempt in 0..COMMENT_QUERY_RETRY_LIMIT {
            match self.api_post(
                &path,
                &[
                    ("file_type", file_type.to_string()),
                    ("user_id_type", "open_id".to_string()),
                ],
                &json!({"comment_ids": [comment_id]}),
            ) {
                Ok(data) => {
                    if let Some(item) = data["items"].as_array().and_then(|items| items.first()) {
                        return Ok(item.clone());
                    }
                    if attempt + 1 == COMMENT_QUERY_RETRY_LIMIT {
                        return Ok(json!({}));
                    }
                }
                Err(error) => {
                    if attempt + 1 == COMMENT_QUERY_RETRY_LIMIT {
                        return Err(error);
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        Ok(json!({}))
    }

    fn list_whole_comments(&self, file_token: &str, file_type: &str) -> Result<Vec<Value>> {
        let path = format!(
            "/open-apis/drive/v1/files/{}/comments",
            encode_component(file_token)
        );
        let mut all = Vec::new();
        let mut page_token = String::new();
        for _ in 0..5 {
            let mut query = vec![
                ("file_type", file_type.to_string()),
                ("is_whole", "true".to_string()),
                ("page_size", "100".to_string()),
                ("user_id_type", "open_id".to_string()),
            ];
            if !page_token.is_empty() {
                query.push(("page_token", page_token.clone()));
            }
            let data = self.api_get(&path, &query)?;
            all.extend(data["items"].as_array().cloned().unwrap_or_default());
            if !data["has_more"].as_bool().unwrap_or(false) {
                break;
            }
            page_token = data["page_token"]
                .as_str()
                .or_else(|| data["next_page_token"].as_str())
                .unwrap_or_default()
                .to_string();
            if page_token.is_empty() {
                break;
            }
        }
        Ok(all)
    }

    fn list_comment_replies(
        &self,
        file_token: &str,
        file_type: &str,
        comment_id: &str,
        expect_reply_id: &str,
    ) -> Result<Vec<Value>> {
        let path = format!(
            "/open-apis/drive/v1/files/{}/comments/{}/replies",
            encode_component(file_token),
            encode_component(comment_id)
        );
        for attempt in 0..COMMENT_QUERY_RETRY_LIMIT {
            let mut all = Vec::new();
            let mut page_token = String::new();
            for _ in 0..5 {
                let mut query = vec![
                    ("file_type", file_type.to_string()),
                    ("page_size", "100".to_string()),
                    ("user_id_type", "open_id".to_string()),
                ];
                if !page_token.is_empty() {
                    query.push(("page_token", page_token.clone()));
                }
                let data = self.api_get(&path, &query)?;
                all.extend(data["items"].as_array().cloned().unwrap_or_default());
                if !data["has_more"].as_bool().unwrap_or(false) {
                    break;
                }
                page_token = data["page_token"]
                    .as_str()
                    .or_else(|| data["next_page_token"].as_str())
                    .unwrap_or_default()
                    .to_string();
                if page_token.is_empty() {
                    break;
                }
            }
            if expect_reply_id.is_empty()
                || all
                    .iter()
                    .any(|reply| reply["reply_id"].as_str() == Some(expect_reply_id))
                || attempt + 1 == COMMENT_QUERY_RETRY_LIMIT
            {
                return Ok(all);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        Ok(Vec::new())
    }

    fn set_comment_reaction(&self, event: &CommentEvent, action: &str) -> Result<()> {
        let path = format!(
            "/open-apis/drive/v2/files/{}/comments/reaction",
            encode_component(&event.file_token)
        );
        self.api_post(
            &path,
            &[("file_type", event.file_type.clone())],
            &json!({
                "action": action,
                "reply_id": event.reply_id,
                "reaction_type": "OK",
            }),
        )?;
        Ok(())
    }

    fn deliver_reply(&self, target: &CommentDeliveryTarget, text: &str) -> Result<()> {
        if text.trim().is_empty() || text.contains(NO_REPLY_SENTINEL) {
            self.cleanup_reaction(target);
            return Ok(());
        }
        let mut as_whole = target.is_whole;
        for chunk in comment_chunks(text) {
            if as_whole {
                self.add_whole_comment(target, &chunk)?;
            } else {
                match self.reply_to_comment(target, &chunk) {
                    Ok(()) => {}
                    Err(error) if error.to_string().contains("1069302") => {
                        as_whole = true;
                        self.add_whole_comment(target, &chunk)?;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        self.cleanup_reaction(target);
        Ok(())
    }

    fn reply_to_comment(&self, target: &CommentDeliveryTarget, text: &str) -> Result<()> {
        let path = format!(
            "/open-apis/drive/v1/files/{}/comments/{}/replies",
            encode_component(&target.file_token),
            encode_component(&target.comment_id)
        );
        self.api_post(
            &path,
            &[("file_type", target.file_type.clone())],
            &json!({
                "content": {
                    "elements": [
                        {"type": "text_run", "text_run": {"text": sanitize_comment_text(text)}}
                    ]
                }
            }),
        )?;
        Ok(())
    }

    fn add_whole_comment(&self, target: &CommentDeliveryTarget, text: &str) -> Result<()> {
        let path = format!(
            "/open-apis/drive/v1/files/{}/new_comments",
            encode_component(&target.file_token)
        );
        self.api_post(
            &path,
            &[],
            &json!({
                "file_type": target.file_type,
                "reply_elements": [
                    {"type": "text", "text": sanitize_comment_text(text)}
                ],
            }),
        )?;
        Ok(())
    }

    fn cleanup_reaction(&self, target: &CommentDeliveryTarget) {
        let Some(reply_id) = target.reply_id.as_deref() else {
            return;
        };
        let event = CommentEvent {
            event_id: String::new(),
            comment_id: target.comment_id.clone(),
            reply_id: reply_id.to_string(),
            is_mentioned: true,
            timestamp: None,
            file_token: target.file_token.clone(),
            file_type: target.file_type.clone(),
            notice_type: "add_reply".to_string(),
            from_open_id: String::new(),
            to_open_id: String::new(),
        };
        let _ = self.set_comment_reaction(&event, "delete");
    }

    fn verify_token(&self, value: &Value) -> Result<()> {
        let Some(expected) = self.verification_token.as_deref() else {
            return Ok(());
        };
        let actual = value["token"]
            .as_str()
            .or_else(|| value["header"]["token"].as_str());
        match actual {
            Some(actual) if constant_time_eq(actual.as_bytes(), expected.as_bytes()) => Ok(()),
            Some(_) => bail!("Feishu comment verification token mismatch"),
            None => bail!("Feishu comment verification token missing"),
        }
    }

    fn verify_signature(&self, request: &ChannelHttpRequest) -> Result<()> {
        let Some(secret) = self.signing_secret.as_deref() else {
            return Ok(());
        };
        let timestamp = request
            .header("x-lark-request-timestamp")
            .ok_or_else(|| anyhow!("Feishu comment request missing timestamp"))?;
        let nonce = request
            .header("x-lark-request-nonce")
            .ok_or_else(|| anyhow!("Feishu comment request missing nonce"))?;
        let signature = request
            .header("x-lark-signature")
            .ok_or_else(|| anyhow!("Feishu comment request missing signature"))?;
        let timestamp_seconds = timestamp
            .parse::<i64>()
            .context("Feishu comment timestamp is not an integer")?;
        let now = chrono::Utc::now().timestamp();
        if (now - timestamp_seconds).abs() > 60 * 5 {
            bail!("Feishu comment request timestamp is outside tolerance");
        }
        let mut hasher = Sha256::new();
        hasher.update(timestamp.as_bytes());
        hasher.update(nonce.as_bytes());
        hasher.update(secret.as_bytes());
        hasher.update(&request.body);
        let expected = to_hex(&hasher.finalize());
        if !constant_time_eq(
            signature.to_ascii_lowercase().as_bytes(),
            expected.as_bytes(),
        ) {
            bail!("Feishu comment request signature mismatch");
        }
        Ok(())
    }
}

impl ChannelAdapter for FeishuCommentAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        if self.transport == "websocket" {
            let adapter = self.clone();
            spawn_feishu_ws_loop(
                FeishuWsConfig {
                    channel: self.channel.clone(),
                    api_base: self.api_base.clone(),
                    app_id: self.app_id.clone(),
                    app_secret: self.app_secret.clone(),
                },
                inbound,
                move |value, inbound| adapter.dispatch_event_value(&value, inbound),
            )?;
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
        if request.path == self.events_path() || request.path == self.webhook_path() {
            return self.handle_events(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let route_key =
            delivery_route_key(&route.key.conversation_id, route.key.thread_id.as_deref());
        let target = self
            .routes
            .lock()
            .expect("feishu comment routes mutex poisoned")
            .get(&route_key)
            .cloned()
            .ok_or_else(|| anyhow!("feishu_comment route target missing for {route_key}"))?;
        if !message.media_paths.is_empty() {
            bail!("Feishu comment does not support native MEDIA delivery; include links in text");
        }
        self.deliver_reply(&target, &message.text)
    }

    fn send_typing(&self, _route: &GatewayRoute, _event: TypingEvent) -> Result<()> {
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        self.send_message(
            route,
            OutboundMessage {
                text: prompt.message.clone(),
                media_paths: Vec::new(),
                reply_to: None,
                approval_prompt: Some(prompt),
                typing_event: None,
            },
        )
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            media: false,
            typing: false,
            approval_prompt: true,
        }
    }
}

fn parse_comment_event(value: &Value) -> Option<CommentEvent> {
    let event = &value["event"];
    let notice_meta = &event["notice_meta"];
    let from_user = &notice_meta["from_user_id"];
    let to_user = &notice_meta["to_user_id"];
    let timestamp = event["timestamp"]
        .as_str()
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map(|value| value.to_rfc3339());
    Some(CommentEvent {
        event_id: event["event_id"].as_str().unwrap_or_default().to_string(),
        comment_id: event["comment_id"].as_str()?.to_string(),
        reply_id: event["reply_id"].as_str().unwrap_or_default().to_string(),
        is_mentioned: event["is_mentioned"].as_bool().unwrap_or(false),
        timestamp,
        file_token: notice_meta["file_token"].as_str()?.to_string(),
        file_type: notice_meta["file_type"].as_str()?.to_string(),
        notice_type: notice_meta["notice_type"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        from_open_id: from_user["open_id"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        to_open_id: to_user["open_id"].as_str().unwrap_or_default().to_string(),
    })
}

fn build_local_prompt(
    event: &CommentEvent,
    channel_label: &str,
    doc_title: &str,
    doc_url: &str,
    quote_text: &str,
    replies: &[Value],
    self_open_id: Option<&str>,
) -> String {
    let timeline = replies
        .iter()
        .map(|reply| {
            let uid = reply_user_id(reply);
            let text = extract_reply_text(reply);
            let is_self = self_open_id.is_some_and(|self_id| uid == self_id);
            (
                uid,
                text,
                is_self,
                reply["reply_id"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect::<Vec<_>>();
    let root_text = timeline
        .first()
        .map(|(_, text, _, _)| text.as_str())
        .unwrap_or("");
    let target_text = timeline
        .iter()
        .find(|(_, _, _, reply_id)| reply_id == &event.reply_id)
        .map(|(_, text, _, _)| text.as_str())
        .or_else(|| {
            timeline
                .iter()
                .rev()
                .find(|(uid, _, _, _)| uid == &event.from_open_id)
                .map(|(_, text, _, _)| text.as_str())
        })
        .unwrap_or("");
    let referenced_docs = referenced_docs_text(replies, &event.file_token);
    let mut lines = vec![
        format!("[{channel_label}]"),
        format!("This is a {channel_label} document comment thread, not an IM chat."),
        "Your final assistant reply will be posted automatically as the comment reply.".to_string(),
        format!("Document title: {doc_title}"),
        format!("Document url: {doc_url}"),
        format!("file_type: {}", event.file_type),
        format!("file_token: {}", event.file_token),
        format!("comment_id: {}", event.comment_id),
        format!("Current user: {}", event.from_open_id),
        format!(
            "Current user comment text: \"{}\"",
            truncate_text(target_text, 240)
        ),
        format!(
            "Original comment text: \"{}\"",
            truncate_text(root_text, 240)
        ),
        format!("Quoted content: \"{}\"", truncate_text(quote_text, 600)),
        String::new(),
        format!("Comment timeline ({} entries):", timeline.len()),
    ];
    for (uid, text, is_self, _) in timeline {
        let marker = if is_self { " <-- YOU" } else { "" };
        lines.push(format!("[{uid}] {}{marker}", truncate_text(&text, 260)));
    }
    append_common_comment_instructions(&mut lines, referenced_docs);
    lines.join("\n")
}

fn build_whole_prompt(
    event: &CommentEvent,
    channel_label: &str,
    doc_title: &str,
    doc_url: &str,
    comments: &[Value],
    self_open_id: Option<&str>,
) -> String {
    let replies = comments
        .iter()
        .flat_map(comment_replies)
        .collect::<Vec<_>>();
    let timeline = replies
        .iter()
        .map(|reply| {
            let uid = reply_user_id(reply);
            let text = extract_reply_text(reply);
            let is_self = self_open_id.is_some_and(|self_id| uid == self_id);
            (uid, text, is_self)
        })
        .collect::<Vec<_>>();
    let current_text = timeline
        .iter()
        .rev()
        .find(|(uid, _, is_self)| uid == &event.from_open_id && !is_self)
        .map(|(_, text, _)| text.as_str())
        .or_else(|| {
            timeline
                .iter()
                .rev()
                .find(|(_, _, is_self)| !is_self)
                .map(|(_, text, _)| text.as_str())
        })
        .unwrap_or("");
    let referenced_docs = referenced_docs_text(&replies, &event.file_token);
    let mut lines = vec![
        format!("[{channel_label}]"),
        format!("This is a whole-document {channel_label} comment, not an IM chat."),
        "Your final assistant reply will be posted automatically as a document comment."
            .to_string(),
        format!("Document title: {doc_title}"),
        format!("Document url: {doc_url}"),
        format!("file_type: {}", event.file_type),
        format!("file_token: {}", event.file_token),
        format!("Current user: {}", event.from_open_id),
        format!(
            "Current user comment text: \"{}\"",
            truncate_text(current_text, 240)
        ),
        String::new(),
        format!(
            "Whole-document comment timeline ({} entries):",
            timeline.len()
        ),
    ];
    for (uid, text, is_self) in timeline {
        let marker = if is_self { " <-- YOU" } else { "" };
        lines.push(format!("[{uid}] {}{marker}", truncate_text(&text, 260)));
    }
    append_common_comment_instructions(&mut lines, referenced_docs);
    lines.join("\n")
}

fn append_common_comment_instructions(lines: &mut Vec<String>, referenced_docs: String) {
    if !referenced_docs.is_empty() {
        lines.push(String::new());
        lines.push(referenced_docs);
    }
    lines.push(String::new());
    lines.push("Instructions:".to_string());
    lines.push("Use the thread timeline above as the main context.".to_string());
    lines.push("If no reply is needed, output exactly NO_REPLY.".to_string());
    lines.push(
        "Reply in the same language as the user's comment unless they request otherwise."
            .to_string(),
    );
    lines.push("Use plain text only; do not use Markdown tables or code fences unless the user explicitly asks for code.".to_string());
    lines.push("Do not show reasoning. Output only the final user-facing reply.".to_string());
}

fn comment_replies(comment: &Value) -> Vec<Value> {
    let reply_list = if comment["reply_list"].is_string() {
        serde_json::from_str(comment["reply_list"].as_str().unwrap_or("{}"))
            .unwrap_or_else(|_| json!({}))
    } else {
        comment["reply_list"].clone()
    };
    reply_list["replies"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn reply_user_id(reply: &Value) -> String {
    reply["user_id"]["open_id"]
        .as_str()
        .or_else(|| reply["user_id"]["user_id"].as_str())
        .or_else(|| reply["user_id"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn extract_reply_text(reply: &Value) -> String {
    let content = if reply["content"].is_string() {
        serde_json::from_str(reply["content"].as_str().unwrap_or("{}"))
            .unwrap_or_else(|_| json!({}))
    } else {
        reply["content"].clone()
    };
    let mut parts = Vec::new();
    for element in content["elements"].as_array().into_iter().flatten() {
        match element["type"].as_str().unwrap_or_default() {
            "text_run" => {
                if let Some(text) = element["text_run"]["text"].as_str() {
                    parts.push(text.to_string());
                }
            }
            "docs_link" => {
                if let Some(url) = element["docs_link"]["url"].as_str() {
                    parts.push(url.to_string());
                }
            }
            "link" => {
                if let Some(url) = element["link"]["url"].as_str() {
                    parts.push(url.to_string());
                }
            }
            "person" => {
                let user = element["person"]["user_id"].as_str().unwrap_or("unknown");
                parts.push(format!("@{user}"));
            }
            _ => {}
        }
    }
    parts.join("")
}

fn comment_channel_label(channel: &str) -> &'static str {
    if channel.eq_ignore_ascii_case("lark_comment") {
        "Lark Comment"
    } else {
        "Feishu Comment"
    }
}

fn referenced_docs_text(replies: &[Value], current_file_token: &str) -> String {
    let re = Regex::new(
        r"(?:feishu\.cn|larkoffice\.com|larksuite\.com|lark\.suite\.com)/(wiki|doc|docx|sheet|sheets|slides|mindnote|bitable|base|file)/([A-Za-z0-9_-]{10,80})",
    )
    .expect("valid Feishu doc link regex");
    let mut seen = HashSet::new();
    let mut lines = Vec::new();
    for reply in replies {
        let text = extract_reply_text(reply);
        for capture in re.captures_iter(&text) {
            let doc_type = capture.get(1).map(|value| value.as_str()).unwrap_or("");
            let token = capture.get(2).map(|value| value.as_str()).unwrap_or("");
            if token.is_empty() || !seen.insert(token.to_string()) {
                continue;
            }
            let suffix = if token == current_file_token {
                " (same as current document)"
            } else {
                ""
            };
            lines.push(format!("- {doc_type}:{token}{suffix}"));
        }
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("Referenced documents in comments:\n{}", lines.join("\n"))
    }
}

fn feishu_event_type(value: &Value) -> Option<String> {
    value["header"]["event_type"]
        .as_str()
        .or_else(|| value["type"].as_str())
        .map(str::to_string)
}

fn parse_policy(raw: Option<&str>) -> Result<CommentPolicy> {
    match raw
        .unwrap_or("allowlist")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "" | "allowlist" | "allow-list" => Ok(CommentPolicy::Allowlist),
        "open" => Ok(CommentPolicy::Open),
        "disabled" | "off" => Ok(CommentPolicy::Disabled),
        "pairing" => Ok(CommentPolicy::Allowlist),
        other => bail!("invalid Feishu comment policy `{other}`"),
    }
}

fn comment_conversation_id(file_type: &str, file_token: &str) -> String {
    format!("comment-doc:{file_type}:{file_token}")
}

fn delivery_route_key(conversation_id: &str, thread_id: Option<&str>) -> String {
    format!("{}\n{}", conversation_id, thread_id.unwrap_or_default())
}

fn comment_chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest = text.trim();
    while rest.chars().count() > COMMENT_REPLY_LIMIT {
        let mut cut = 0;
        for (idx, _) in rest.char_indices().take(COMMENT_REPLY_LIMIT) {
            cut = idx;
        }
        if let Some(newline) = rest[..cut].rfind('\n') {
            cut = newline;
        }
        chunks.push(rest[..cut].trim().to_string());
        rest = rest[cut..].trim();
    }
    if !rest.is_empty() {
        chunks.push(rest.to_string());
    }
    chunks
}

fn sanitize_comment_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out = text.chars().take(limit).collect::<String>();
    out.push_str("...");
    out
}

fn encode_component(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"code\":1}".to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feishu_comment_event_parses_drive_notice() {
        let parsed = parse_comment_event(&comment_event_json()).expect("event");
        assert_eq!(parsed.comment_id, "c1");
        assert_eq!(parsed.reply_id, "r1");
        assert_eq!(parsed.file_type, "docx");
        assert_eq!(parsed.from_open_id, "ou_user");
        assert!(parsed.is_mentioned);
    }

    #[test]
    fn feishu_comment_filters_self_and_unmentioned() -> Result<()> {
        let adapter = test_adapter()?;
        let mut event = parse_comment_event(&comment_event_json()).expect("event");
        event.from_open_id = "ou_bot".to_string();
        assert!(adapter.should_skip_event(&event));
        event.from_open_id = "ou_user".to_string();
        event.is_mentioned = false;
        assert!(adapter.should_skip_event(&event));
        Ok(())
    }

    #[test]
    fn feishu_comment_doc_allowlist_accepts_exact_and_wildcard() -> Result<()> {
        let adapter = FeishuCommentAdapter::new(
            "feishu_comment",
            &GatewayChannelConfig {
                allowed_chats: vec!["docx:abc".to_string(), "sheet:*".to_string()],
                ..Default::default()
            },
            &test_credentials(),
        )?;
        assert!(adapter.document_allowed("docx", "abc"));
        assert!(adapter.document_allowed("sheet", "anything"));
        assert!(!adapter.document_allowed("docx", "other"));
        Ok(())
    }

    #[test]
    fn feishu_comment_sanitizes_text() {
        assert_eq!(
            sanitize_comment_text("a < b & c > d"),
            "a &lt; b &amp; c &gt; d"
        );
    }

    #[test]
    fn feishu_comment_prompt_contains_thread_context() {
        let event = parse_comment_event(&comment_event_json()).expect("event");
        let replies = vec![
            json!({"reply_id": "root", "user_id": {"open_id": "ou_owner"}, "content": {"elements": [{"type": "text_run", "text_run": {"text": "root text"}}]}}),
            json!({"reply_id": "r1", "user_id": {"open_id": "ou_user"}, "content": {"elements": [{"type": "person", "person": {"user_id": "ou_bot"}}, {"type": "text_run", "text_run": {"text": " please help"}}]}}),
        ];
        let prompt = build_local_prompt(
            &event,
            "Feishu Comment",
            "Doc",
            "https://example.com/doc",
            "quoted",
            &replies,
            Some("ou_bot"),
        );
        assert!(prompt.contains("[Feishu Comment]"));
        assert!(prompt.contains("quoted"));
        assert!(prompt.contains("please help"));
    }

    #[test]
    fn feishu_comment_signature_matches_lark_formula() -> Result<()> {
        let adapter = test_adapter()?;
        let body = br#"{"token":"verify"}"#.to_vec();
        let now = chrono::Utc::now().timestamp().to_string();
        let nonce = "nonce";
        let mut hasher = Sha256::new();
        hasher.update(now.as_bytes());
        hasher.update(nonce.as_bytes());
        hasher.update(b"sign");
        hasher.update(&body);
        let signature = to_hex(&hasher.finalize());
        adapter.verify_signature(&ChannelHttpRequest {
            method: "POST".to_string(),
            path: "/feishu_comment/events".to_string(),
            query: Default::default(),
            headers: vec![
                ("x-lark-request-timestamp".to_string(), now),
                ("x-lark-request-nonce".to_string(), nonce.to_string()),
                ("x-lark-signature".to_string(), signature),
            ],
            body,
        })?;
        Ok(())
    }

    fn comment_event_json() -> Value {
        json!({
            "schema": "2.0",
            "header": {
                "event_type": "drive.notice.comment_add_v1",
                "token": "verify"
            },
            "event": {
                "event_id": "evt_1",
                "comment_id": "c1",
                "reply_id": "r1",
                "is_mentioned": true,
                "timestamp": "1713200000",
                "notice_meta": {
                    "file_token": "abc",
                    "file_type": "docx",
                    "notice_type": "add_reply",
                    "from_user_id": {"open_id": "ou_user"},
                    "to_user_id": {"open_id": "ou_bot"}
                }
            }
        })
    }

    fn test_adapter() -> Result<FeishuCommentAdapter> {
        FeishuCommentAdapter::new(
            "feishu_comment",
            &GatewayChannelConfig {
                allowed_users: vec!["ou_user".to_string()],
                ..Default::default()
            },
            &test_credentials(),
        )
    }

    fn test_credentials() -> GatewayCredentialEntry {
        GatewayCredentialEntry {
            channel: "feishu_comment".to_string(),
            app_id: Some("cli_xxx".to_string()),
            app_secret: Some("secret".to_string()),
            webhook_secret: Some("verify".to_string()),
            signing_secret: Some("sign".to_string()),
            username: Some("ou_bot".to_string()),
            ..Default::default()
        }
    }
}
