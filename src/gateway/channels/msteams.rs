use super::super::{
    ChannelAdapter, ChannelCapabilities, ChannelHttpRequest, ChannelHttpResponse,
    GatewayApprovalPrompt, GatewayInboundDispatch, GatewayRoute, InboundAttachmentInput,
    InboundMessageInput, OutboundMessage, StreamMessageHandle, TypingEvent,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TEAMS_TEXT_LIMIT: usize = 8_000;
const TEAMS_TOKEN_TTL_SECONDS: u64 = 3_600;
const BOT_FRAMEWORK_SCOPE: &str = "https://api.botframework.com/.default";

#[derive(Clone)]
pub(in crate::gateway) struct MsTeamsAdapter {
    channel: String,
    bot_app_id: Option<String>,
    client_secret: Option<String>,
    tenant_id: Option<String>,
    token: Option<String>,
    service_url: Option<String>,
    allowed_service_hosts: HashSet<String>,
    allowed_users: HashSet<String>,
    allowed_chats: HashSet<String>,
    max_download_bytes: u64,
    client: Client,
    conversations: Arc<Mutex<HashMap<String, TeamsConversationRef>>>,
    seen_message_ids: Arc<Mutex<VecDeque<String>>>,
    access_token: Arc<Mutex<Option<CachedTeamsToken>>>,
}

#[derive(Debug, Clone)]
struct TeamsConversationRef {
    service_url: Option<String>,
    conversation_id: String,
}

#[derive(Debug, Clone)]
struct CachedTeamsToken {
    token: String,
    expires_at: Instant,
}

impl MsTeamsAdapter {
    pub(in crate::gateway) fn new(
        channel: &str,
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build Teams HTTP client")?;
        let allowed_service_hosts = teams_allowed_service_hosts(config);
        let service_url = config
            .extra
            .get("service_url")
            .or_else(|| credentials.extra.get("service_url"))
            .and_then(|value| normalize_teams_service_url(value))
            .filter(|url| teams_service_url_allowed(url, &allowed_service_hosts));
        Ok(Self {
            channel: channel.to_string(),
            bot_app_id: credentials
                .app_id
                .clone()
                .or_else(|| credentials.extra.get("client_id").cloned())
                .or_else(|| config.extra.get("client_id").cloned()),
            client_secret: credentials
                .client_secret
                .clone()
                .or_else(|| credentials.app_secret.clone())
                .or_else(|| credentials.password.clone())
                .or_else(|| credentials.extra.get("client_secret").cloned())
                .or_else(|| config.extra.get("client_secret").cloned()),
            tenant_id: credentials
                .extra
                .get("tenant_id")
                .cloned()
                .or_else(|| config.extra.get("tenant_id").cloned()),
            token: credentials.token.clone().or(credentials.api_key.clone()),
            service_url,
            allowed_service_hosts,
            allowed_users: config.allowed_users.iter().cloned().collect(),
            allowed_chats: config.allowed_chats.iter().cloned().collect(),
            max_download_bytes: config.media.max_download_bytes,
            client,
            conversations: Arc::new(Mutex::new(HashMap::new())),
            seen_message_ids: Arc::new(Mutex::new(VecDeque::new())),
            access_token: Arc::new(Mutex::new(None)),
        })
    }

    fn handle_activity(
        &self,
        request: ChannelHttpRequest,
        inbound: GatewayInboundDispatch,
    ) -> Result<ChannelHttpResponse> {
        let value: Value =
            serde_json::from_slice(&request.body).context("failed to parse Teams activity JSON")?;
        let activity_type = value["type"].as_str().unwrap_or_default();
        if !matches!(activity_type, "message" | "invoke") {
            return Ok(json_response(200, json!({})));
        }
        let conversation_id = value["conversation"]["id"]
            .as_str()
            .ok_or_else(|| anyhow!("Teams activity missing conversation.id"))?;
        let message_id = teams_activity_id(&value);
        if message_id
            .as_deref()
            .is_some_and(|id| self.is_duplicate(id))
        {
            return Ok(json_response(200, json!({})));
        }
        let sender_ids = teams_sender_ids(&value);
        if self
            .bot_app_id
            .as_deref()
            .is_some_and(|bot_id| sender_ids.iter().any(|sender| sender == bot_id))
            || value["recipient"]["id"]
                .as_str()
                .is_some_and(|recipient| sender_ids.iter().any(|sender| sender == recipient))
        {
            return Ok(json_response(200, json!({})));
        }
        if !self.allowed_chats.is_empty() && !self.allowed_chats.contains(conversation_id) {
            return Ok(json_response(200, json!({})));
        }
        if !self.allowed_users.is_empty()
            && !sender_ids
                .iter()
                .any(|sender| self.allowed_users.contains(sender))
        {
            return Ok(json_response(200, json!({})));
        }
        self.conversations
            .lock()
            .expect("teams conversations mutex poisoned")
            .insert(
                conversation_id.to_string(),
                TeamsConversationRef {
                    service_url: value["serviceUrl"]
                        .as_str()
                        .and_then(|url| self.accept_service_url(url)),
                    conversation_id: conversation_id.to_string(),
                },
            );
        let text = teams_text(&value);
        let attachments = self.parse_attachments(&value);
        if text.trim().is_empty() && attachments.is_empty() && activity_type == "invoke" {
            return Ok(json_response(200, json!({})));
        }
        let chat_type = match value["conversation"]["conversationType"]
            .as_str()
            .unwrap_or_default()
        {
            "personal" => "dm",
            "groupChat" => "group",
            "channel" => "channel",
            other if !other.is_empty() => other,
            _ => "dm",
        };
        inbound.submit(InboundMessageInput {
            channel: self.channel.clone(),
            conversation_id: conversation_id.to_string(),
            thread_id: value["replyToId"].as_str().map(str::to_string),
            chat_type: Some(chat_type.to_string()),
            sender_id: sender_ids.first().cloned(),
            message_id,
            text: if text.trim().is_empty() && !attachments.is_empty() {
                "[Teams attachment]".to_string()
            } else {
                text
            },
            attachments,
            timestamp: value["timestamp"].as_str().map(str::to_string),
        })?;
        Ok(json_response(200, json!({})))
    }

    fn parse_attachments(&self, value: &Value) -> Vec<InboundAttachmentInput> {
        let mut out = Vec::new();
        for attachment in value["attachments"].as_array().into_iter().flatten() {
            let Some(content_url) = teams_attachment_str(
                attachment,
                &[
                    "contentUrl",
                    "content_url",
                    "downloadUrl",
                    "content.downloadUrl",
                    "content.download_url",
                    "content.fileDownloadUrl",
                    "content.thumbnailUrl",
                ],
            ) else {
                continue;
            };
            if !content_url.starts_with("http://") && !content_url.starts_with("https://") {
                continue;
            }
            match self.download_attachment(content_url, attachment) {
                Ok(input) => out.push(input),
                Err(error) => eprintln!("Teams attachment skipped: {error:#}"),
            }
        }
        out
    }

    fn download_attachment(&self, url: &str, attachment: &Value) -> Result<InboundAttachmentInput> {
        let mut request = self.client.get(url);
        if let Some(token) = self.auth_token()? {
            request = request.bearer_auth(token);
        }
        let response = request.send().context("Teams attachment download failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("Teams attachment download failed with status {status}");
        }
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).to_string())
            .or_else(|| {
                teams_attachment_str(attachment, &["contentType", "content_type"])
                    .map(str::to_string)
            });
        let bytes = response
            .bytes()
            .context("Teams attachment body unreadable")?;
        if bytes.len() as u64 > self.max_download_bytes {
            bail!("Teams attachment exceeds configured max_download_bytes");
        }
        Ok(InboundAttachmentInput {
            bytes: Some(bytes.to_vec()),
            path: None,
            filename: teams_attachment_str(attachment, &["name", "filename", "fileName"])
                .map(str::to_string)
                .or_else(|| Some("teams-attachment.bin".to_string())),
            mime,
        })
    }

    fn is_duplicate(&self, message_id: &str) -> bool {
        if message_id.is_empty() {
            return false;
        }
        let mut seen = self
            .seen_message_ids
            .lock()
            .expect("teams seen message mutex poisoned");
        if seen.iter().any(|seen| seen == message_id) {
            return true;
        }
        seen.push_back(message_id.to_string());
        while seen.len() > 1_000 {
            seen.pop_front();
        }
        false
    }

    fn conversation_ref(&self, route: &GatewayRoute) -> Result<TeamsConversationRef> {
        let reference = self
            .conversations
            .lock()
            .expect("teams conversations mutex poisoned")
            .get(&route.key.conversation_id)
            .cloned()
            .unwrap_or(TeamsConversationRef {
                service_url: self.service_url.clone(),
                conversation_id: route.key.conversation_id.clone(),
            });
        if reference.service_url.is_none() {
            bail!("Teams proactive send requires a cached serviceUrl from inbound activity");
        }
        Ok(reference)
    }

    fn accept_service_url(&self, raw: &str) -> Option<String> {
        let normalized = normalize_teams_service_url(raw)?;
        let host = url::Url::parse(&normalized)
            .ok()
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase))?;
        self.allowed_service_hosts
            .contains(&host)
            .then_some(normalized)
    }

    fn auth_token(&self) -> Result<Option<String>> {
        if let Some(token) = self
            .token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Ok(Some(token.to_string()));
        }
        let (Some(client_id), Some(client_secret), Some(tenant_id)) = (
            self.bot_app_id.as_deref(),
            self.client_secret.as_deref(),
            self.tenant_id.as_deref(),
        ) else {
            return Ok(None);
        };
        {
            let guard = self
                .access_token
                .lock()
                .expect("teams access token mutex poisoned");
            if let Some(token) = guard.as_ref() {
                if token.expires_at > Instant::now() + Duration::from_secs(60) {
                    return Ok(Some(token.token.clone()));
                }
            }
        }
        let token_url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            encode_component(tenant_id)
        );
        let response = self
            .client
            .post(token_url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("scope", BOT_FRAMEWORK_SCOPE),
            ])
            .send()
            .context("Teams Bot Framework token request failed")?;
        let status = response.status();
        let value: Value = response
            .json()
            .unwrap_or_else(|_| json!({"error": "non-json token response"}));
        if !status.is_success() {
            bail!("Teams Bot Framework token request failed with status {status}: {value}");
        }
        let token = value["access_token"]
            .as_str()
            .ok_or_else(|| anyhow!("Teams Bot Framework token response missing access_token"))?
            .to_string();
        let expires = value["expires_in"]
            .as_u64()
            .unwrap_or(TEAMS_TOKEN_TTL_SECONDS);
        *self
            .access_token
            .lock()
            .expect("teams access token mutex poisoned") = Some(CachedTeamsToken {
            token: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires),
        });
        Ok(Some(token))
    }

    fn send_activity(&self, route: &GatewayRoute, payload: Value) -> Result<Value> {
        let reference = self.conversation_ref(route)?;
        if !valid_teams_conversation_id(&reference.conversation_id) {
            bail!("Teams conversation id contains unsupported characters");
        }
        let service_url = reference.service_url.as_deref().unwrap_or_default();
        let url = format!(
            "{}/v3/conversations/{}/activities",
            service_url.trim_end_matches('/'),
            encode_component(&reference.conversation_id)
        );
        let mut request = self.client.post(url).json(&payload);
        if let Some(token) = self.auth_token()? {
            request = request.bearer_auth(token);
        }
        let response = request.send().context("Teams proactive send failed")?;
        let status = response.status();
        let value = response.json::<Value>().unwrap_or_else(|_| Value::Null);
        if !status.is_success() {
            bail!("Teams proactive send failed with status {status}: {value}");
        }
        Ok(value)
    }

    fn update_activity(
        &self,
        route: &GatewayRoute,
        activity_id: &str,
        payload: Value,
    ) -> Result<()> {
        let reference = self.conversation_ref(route)?;
        if !valid_teams_conversation_id(&reference.conversation_id) {
            bail!("Teams conversation id contains unsupported characters");
        }
        let service_url = reference.service_url.as_deref().unwrap_or_default();
        let url = format!(
            "{}/v3/conversations/{}/activities/{}",
            service_url.trim_end_matches('/'),
            encode_component(&reference.conversation_id),
            encode_component(activity_id)
        );
        let mut request = self.client.put(url).json(&payload);
        if let Some(token) = self.auth_token()? {
            request = request.bearer_auth(token);
        }
        let response = request.send().context("Teams proactive update failed")?;
        if !response.status().is_success() {
            bail!(
                "Teams proactive update failed with status {}",
                response.status()
            );
        }
        Ok(())
    }
}

impl ChannelAdapter for MsTeamsAdapter {
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
        if matches!(
            request.path.as_str(),
            "/api/messages" | "/msteams/events" | "/teams/events"
        ) {
            return self.handle_activity(request, inbound).map(Some);
        }
        Ok(None)
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let mut text = message.text;
        for media in message.media_paths {
            if media.starts_with("http://") || media.starts_with("https://") {
                text.push_str("\n");
                text.push_str(&media);
            } else {
                bail!("Teams local MEDIA requires SharePoint/Graph upload support: {media}");
            }
        }
        let reply_to = message
            .reply_to
            .as_deref()
            .or(route.key.thread_id.as_deref());
        for chunk in teams_chunks(&text) {
            let mut payload = json!({"type": "message", "textFormat": "markdown", "text": chunk});
            if let Some(reply_to) = reply_to {
                payload["replyToId"] = json!(reply_to);
            }
            self.send_activity(route, payload)?;
        }
        Ok(())
    }

    fn send_stream_start(
        &self,
        route: &GatewayRoute,
        text: &str,
    ) -> Result<Option<StreamMessageHandle>> {
        let value = self.send_activity(
            route,
            json!({"type": "message", "textFormat": "markdown", "text": text}),
        )?;
        let message_id = value["id"]
            .as_str()
            .ok_or_else(|| anyhow!("Teams stream start did not return activity id"))?
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
        self.update_activity(
            route,
            &handle.message_id,
            json!({"type": "message", "textFormat": "markdown", "text": text}),
        )
    }

    fn stream_text_limit(&self) -> usize {
        TEAMS_TEXT_LIMIT
    }

    fn send_typing(&self, route: &GatewayRoute, event: TypingEvent) -> Result<()> {
        if event.active {
            let _ = self.send_activity(route, json!({"type": "typing"}));
        }
        Ok(())
    }

    fn send_approval_prompt(
        &self,
        route: &GatewayRoute,
        prompt: GatewayApprovalPrompt,
    ) -> Result<()> {
        self.send_activity(route, json!({
            "type": "message",
            "textFormat": "markdown",
            "text": format!(
                "{}\n\nCommands:\n/approve {} once\n/approve {} session\n/approve {} always\n/deny {}",
                prompt.message, prompt.id, prompt.id, prompt.id, prompt.id
            ),
            "attachments": [{
                "contentType": "application/vnd.microsoft.card.adaptive",
                "content": {
                    "type": "AdaptiveCard",
                    "version": "1.4",
                    "body": [{"type": "TextBlock", "text": prompt.message, "wrap": true}],
                    "actions": [
                        {"type": "Action.Submit", "title": "Once", "data": {"text": format!("/approve {} once", prompt.id)}},
                        {"type": "Action.Submit", "title": "Session", "data": {"text": format!("/approve {} session", prompt.id)}},
                        {"type": "Action.Submit", "title": "Always", "data": {"text": format!("/approve {} always", prompt.id)}},
                        {"type": "Action.Submit", "title": "Deny", "data": {"text": format!("/deny {}", prompt.id)}}
                    ]
                }
            }]
        }))?;
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

fn teams_text(value: &Value) -> String {
    let text = [
        "text",
        "value.text",
        "value.data.text",
        "value.msteams.text",
        "value.action.data.text",
        "value.action.data.msteams.text",
        "channelData.postBack",
    ]
    .into_iter()
    .find_map(|path| teams_nested_str(value, path))
    .map(str::to_string)
    .or_else(|| teams_action_command(value))
    .unwrap_or_default();
    strip_teams_mentions(&decode_basic_html_entities(&text))
}

fn teams_sender_ids(value: &Value) -> Vec<String> {
    [
        value["from"]["aadObjectId"].as_str(),
        value["from"]["aad_object_id"].as_str(),
        value["from"]["userPrincipalName"].as_str(),
        value["from"]["email"].as_str(),
        value["from"]["id"].as_str(),
        value["channelData"]["from"]["aadObjectId"].as_str(),
        value["channelData"]["from"]["aad_object_id"].as_str(),
        value["channelData"]["from"]["userPrincipalName"].as_str(),
        value["channelData"]["from"]["id"].as_str(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .map(str::to_string)
    .collect()
}

fn teams_attachment_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| teams_nested_str(value, key))
}

fn teams_nested_str<'a>(value: &'a Value, path: &str) -> Option<&'a str> {
    let mut current = value;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    current
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn teams_activity_id(value: &Value) -> Option<String> {
    ["id", "channelData.clientActivityId", "value.id"]
        .into_iter()
        .find_map(|path| teams_nested_str(value, path))
        .map(str::to_string)
}

fn teams_action_command(value: &Value) -> Option<String> {
    let approval_id = [
        "value.action.data.approval_id",
        "value.action.data.approvalId",
        "value.data.approval_id",
        "value.data.approvalId",
        "value.approval_id",
        "value.approvalId",
        "value.id",
    ]
    .into_iter()
    .find_map(|path| teams_nested_str(value, path))?;
    let action = [
        "value.action.data.action",
        "value.action.data.decision",
        "value.action.data.choice",
        "value.action.verb",
        "value.verb",
        "value.action",
    ]
    .into_iter()
    .find_map(|path| teams_nested_str(value, path))?
    .to_ascii_lowercase();
    match action.as_str() {
        "approve_once" | "allow_once" | "once" => Some(format!("/approve {approval_id} once")),
        "approve_session" | "allow_session" | "session" => {
            Some(format!("/approve {approval_id} session"))
        }
        "approve_always" | "allow_always" | "always" => {
            Some(format!("/approve {approval_id} always"))
        }
        "deny" | "reject" => Some(format!("/deny {approval_id}")),
        _ => None,
    }
}

fn strip_teams_mentions(text: &str) -> String {
    regex::Regex::new(r"(?is)<at(?:\s+[^>]*)?>.*?</at>\s*")
        .expect("valid Teams mention regex")
        .replace_all(text, "")
        .trim()
        .to_string()
}

fn decode_basic_html_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn teams_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if !current.is_empty() && current.len() + ch.len_utf8() > TEAMS_TEXT_LIMIT {
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

fn valid_teams_conversation_id(value: &str) -> bool {
    !value.trim().is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '@' | '-' | '_' | '.'))
}

fn teams_allowed_service_hosts(config: &GatewayChannelConfig) -> HashSet<String> {
    let mut hosts = HashSet::new();
    hosts.insert("smba.trafficmanager.net".to_string());
    hosts.insert("smba.infra.gov.teams.microsoft.us".to_string());
    for value in split_csv(
        config
            .extra
            .get("allowed_service_hosts")
            .map(String::as_str),
    ) {
        hosts.insert(value.to_ascii_lowercase());
    }
    hosts
}

fn normalize_teams_service_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed = url::Url::parse(trimmed).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }
    parsed.host_str()?;
    Some(if trimmed.ends_with('/') {
        trimmed.to_string()
    } else {
        format!("{trimmed}/")
    })
}

fn teams_service_url_allowed(url: &str, allowed_hosts: &HashSet<String>) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| allowed_hosts.contains(&host))
}

fn split_csv(value: Option<&str>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn encode_component(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
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
    fn teams_text_accepts_adaptive_submit_text() {
        let value = json!({"value": {"text": "/approve a once"}});
        assert_eq!(teams_text(&value), "/approve a once");
    }

    #[test]
    fn teams_attachment_str_accepts_content_url() {
        let value = json!({"contentUrl": "https://example.com/file.png"});
        assert_eq!(
            teams_attachment_str(&value, &["contentUrl"]).unwrap(),
            "https://example.com/file.png"
        );
    }

    #[test]
    fn teams_chunks_split_long_text() {
        assert_eq!(teams_chunks(&"x".repeat(TEAMS_TEXT_LIMIT + 1)).len(), 2);
    }

    #[test]
    fn teams_adapter_keeps_alias_channel() -> Result<()> {
        let adapter = MsTeamsAdapter::new(
            "teams",
            &GatewayChannelConfig::default(),
            &GatewayCredentialEntry {
                channel: "teams".to_string(),
                ..Default::default()
            },
        )?;
        assert_eq!(adapter.channel, "teams");
        Ok(())
    }
}
