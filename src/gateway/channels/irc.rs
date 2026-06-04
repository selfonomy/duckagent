use super::super::{
    ChannelAdapter, ChannelCapabilities, GatewayApprovalPrompt, GatewayInboundDispatch,
    GatewayRoute, InboundMessageInput, OutboundMessage, TypingEvent,
};
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Context, Result, anyhow, bail};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use rustls_pki_types::ServerName;
use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const IRC_TEXT_LIMIT: usize = 420;

#[derive(Clone)]
pub(in crate::gateway) struct IrcAdapter {
    gateway_channel: String,
    server: String,
    port: u16,
    use_tls: bool,
    nickname: String,
    channels: Vec<String>,
    server_password: Option<String>,
    nickserv_password: Option<String>,
    capabilities: Vec<String>,
    allowed_users: HashSet<String>,
    allowed_chats: HashSet<String>,
    require_mention: bool,
    outbound: Arc<Mutex<Option<Sender<String>>>>,
    current_nickname: Arc<Mutex<String>>,
}

trait IrcReadWrite: Read + Write {}
impl<T: Read + Write> IrcReadWrite for T {}

#[derive(Debug, Clone)]
struct ParsedIrcLine {
    tags: HashMap<String, String>,
    prefix: Option<String>,
    command: String,
    params: Vec<String>,
}

impl IrcAdapter {
    pub(in crate::gateway) fn new(
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        Self::new_with_defaults("irc", None, 6697, true, config, credentials)
    }

    pub(in crate::gateway::channels) fn new_with_defaults(
        gateway_channel: &str,
        default_server: Option<&str>,
        default_port: u16,
        default_tls: bool,
        config: &GatewayChannelConfig,
        credentials: &GatewayCredentialEntry,
    ) -> Result<Self> {
        let server = config
            .api_base
            .as_deref()
            .or_else(|| config.extra.get("server").map(String::as_str))
            .or(default_server)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("{gateway_channel} gateway config requires server/api_base"))?
            .trim_start_matches("irc://")
            .trim_start_matches("ircs://")
            .to_string();
        let port = config
            .extra
            .get("port")
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(default_port);
        let use_tls = config
            .extra
            .get("tls")
            .map(|value| value == "true")
            .unwrap_or(default_tls);
        let nickname = credentials
            .username
            .as_deref()
            .or_else(|| config.extra.get("nickname").map(String::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("duckagent")
            .to_string();
        let mut capabilities = config
            .extra
            .get("capabilities")
            .map(|value| {
                value
                    .split([',', ' '])
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if capabilities.is_empty() && gateway_channel == "twitch" {
            capabilities.extend(
                ["twitch.tv/tags", "twitch.tv/commands"]
                    .into_iter()
                    .map(str::to_string),
            );
        }
        let mut channels = config.allowed_chats.clone();
        if channels.is_empty() {
            if let Some(channel) = config.extra.get("channel") {
                channels.push(channel.clone());
            }
        }
        if channels.is_empty() {
            bail!(
                "{gateway_channel} gateway config requires at least one channel in allowed_chats or extra.channel"
            );
        }
        Ok(Self {
            gateway_channel: gateway_channel.to_string(),
            server,
            port,
            use_tls,
            nickname: nickname.clone(),
            channels,
            server_password: credentials.token.clone().map(|token| {
                if gateway_channel == "twitch"
                    && !token.trim().to_ascii_lowercase().starts_with("oauth:")
                {
                    format!("oauth:{}", token.trim())
                } else {
                    token.trim().to_string()
                }
            }),
            nickserv_password: credentials.password.clone(),
            capabilities,
            allowed_users: config
                .allowed_users
                .iter()
                .map(|value| value.to_ascii_lowercase())
                .collect(),
            allowed_chats: config.allowed_chats.iter().cloned().collect(),
            require_mention: config
                .extra
                .get("require_mention")
                .map(|value| value != "false")
                .unwrap_or(true),
            outbound: Arc::new(Mutex::new(None)),
            current_nickname: Arc::new(Mutex::new(nickname)),
        })
    }

    fn send_raw(&self, raw: String) -> Result<()> {
        let sender = self
            .outbound
            .lock()
            .expect("irc outbound mutex poisoned")
            .clone()
            .ok_or_else(|| anyhow!("IRC adapter is not connected yet"))?;
        sender
            .send(raw)
            .context("failed to queue IRC outbound line")
    }

    fn inbound_from_privmsg(&self, line: &ParsedIrcLine) -> Option<InboundMessageInput> {
        if line.command != "PRIVMSG" || line.params.len() < 2 {
            return None;
        }
        let target = &line.params[0];
        let raw_text = &line.params[1];
        let prefix = line.prefix.as_deref().unwrap_or_default();
        let nick = irc_nick(prefix);
        let current_nick = self.current_nickname();
        if nick.eq_ignore_ascii_case(&current_nick) {
            return None;
        }
        let mut raw_text = raw_text.to_string();
        if raw_text.starts_with("\x01ACTION ") && raw_text.ends_with('\x01') {
            raw_text = format!("* {nick} {}", &raw_text[8..raw_text.len() - 1]);
        } else if raw_text.starts_with('\x01') {
            return None;
        }
        let is_channel = target.starts_with('#') || target.starts_with('&');
        let conversation_id = if is_channel { target.as_str() } else { nick };
        if is_channel
            && !self.allowed_chats.is_empty()
            && !self
                .allowed_chats
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(target))
        {
            return None;
        }
        if !self.allowed_users.is_empty() && !self.allowed_users.contains("*") {
            let sender_allowed = [
                Some(nick.to_ascii_lowercase()),
                Some(prefix.to_ascii_lowercase()),
                line.tags
                    .get("user-id")
                    .map(|value| value.to_ascii_lowercase()),
                line.tags
                    .get("display-name")
                    .map(|value| value.to_ascii_lowercase()),
                line.tags
                    .get("login")
                    .map(|value| value.to_ascii_lowercase()),
            ]
            .into_iter()
            .flatten()
            .any(|candidate| self.allowed_users.contains(&candidate));
            if !sender_allowed {
                return None;
            }
        }
        let text = if is_channel && self.require_mention {
            strip_mention(&raw_text, &current_nick)?
        } else {
            raw_text
        };
        Some(InboundMessageInput {
            channel: self.gateway_channel.clone(),
            conversation_id: conversation_id.to_string(),
            thread_id: None,
            chat_type: Some(if is_channel { "channel" } else { "dm" }.to_string()),
            sender_id: Some(
                line.tags
                    .get("user-id")
                    .or_else(|| line.tags.get("login"))
                    .or_else(|| line.tags.get("display-name"))
                    .map(String::as_str)
                    .unwrap_or(prefix)
                    .to_string(),
            ),
            message_id: line.tags.get("id").cloned(),
            text,
            attachments: Vec::new(),
            timestamp: line
                .tags
                .get("tmi-sent-ts")
                .and_then(|value| value.parse::<i64>().ok())
                .and_then(|millis| chrono::DateTime::from_timestamp_millis(millis))
                .map(|value| value.to_rfc3339())
                .or_else(|| Some(chrono::Utc::now().to_rfc3339())),
        })
    }

    fn current_nickname(&self) -> String {
        self.current_nickname
            .lock()
            .expect("irc current nickname mutex poisoned")
            .clone()
    }

    fn set_current_nickname(&self, nickname: String) {
        *self
            .current_nickname
            .lock()
            .expect("irc current nickname mutex poisoned") = nickname;
    }
}

impl ChannelAdapter for IrcAdapter {
    fn start(&self, inbound: GatewayInboundDispatch) -> Result<()> {
        let (sender, receiver) = mpsc::channel::<String>();
        *self.outbound.lock().expect("irc outbound mutex poisoned") = Some(sender);
        let adapter = self.clone();
        thread::Builder::new()
            .name(format!("gateway-irc-{}", self.gateway_channel))
            .spawn(move || {
                loop {
                    if let Err(error) = run_irc_loop(adapter.clone(), &inbound, &receiver) {
                        eprintln!("irc gateway loop disconnected: {error:#}");
                    }
                    thread::sleep(Duration::from_secs(5));
                }
            })
            .context("failed to spawn IRC gateway thread")?;
        Ok(())
    }

    fn send_message(&self, route: &GatewayRoute, message: OutboundMessage) -> Result<()> {
        let target = route.key.conversation_id.trim();
        if !valid_irc_target(target) {
            bail!("invalid IRC target `{}`", route.key.conversation_id);
        }
        let mut text = strip_irc_markdown(&message.text);
        for media in message.media_paths {
            text.push('\n');
            if media.starts_with("http://") || media.starts_with("https://") {
                text.push_str(&media);
            } else {
                text.push_str("[local media requires a public URL for IRC]");
            }
        }
        for chunk in irc_chunks_for_target(&text, target, IRC_TEXT_LIMIT) {
            self.send_raw(format!(
                "PRIVMSG {} :{}\r\n",
                target,
                sanitize_irc_text(&chunk)
            ))?;
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
        let approval_id = prompt.id.clone();
        let approval_message = prompt.message.clone();
        self.send_message(
            route,
            OutboundMessage {
                text: format!(
                    "{}\nCommands: /approve {} once | /approve {} session | /approve {} always | /deny {}",
                    approval_message,
                    approval_id.as_str(),
                    approval_id.as_str(),
                    approval_id.as_str(),
                    approval_id.as_str()
                ),
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

fn run_irc_loop(
    adapter: IrcAdapter,
    inbound: &GatewayInboundDispatch,
    receiver: &mpsc::Receiver<String>,
) -> Result<()> {
    let mut stream = connect_irc(&adapter.server, adapter.port, adapter.use_tls)?;
    let mut current_nick = adapter.nickname.clone();
    adapter.set_current_nickname(current_nick.clone());
    write_irc_line(
        &mut stream,
        adapter
            .server_password
            .as_ref()
            .map(|password| format!("PASS {password}\r\n")),
    )?;
    write_irc_line(&mut stream, Some(format!("NICK {}\r\n", adapter.nickname)))?;
    write_irc_line(
        &mut stream,
        Some(format!(
            "USER {} 0 * :DuckAgent Gateway\r\n",
            adapter.nickname
        )),
    )?;
    if !adapter.capabilities.is_empty() {
        write_irc_line(
            &mut stream,
            Some(format!("CAP REQ :{}\r\n", adapter.capabilities.join(" "))),
        )?;
        write_irc_line(&mut stream, Some("CAP END\r\n".to_string()))?;
    }
    let mut registered = false;
    let mut buffer = Vec::new();
    loop {
        while let Ok(line) = receiver.try_recv() {
            stream.write_all(line.as_bytes())?;
            stream.flush()?;
        }
        let mut byte = [0_u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => bail!("IRC connection closed"),
            Ok(_) => {
                buffer.push(byte[0]);
                if byte[0] != b'\n' {
                    continue;
                }
                let raw = String::from_utf8_lossy(&buffer).trim().to_string();
                buffer.clear();
                if raw.is_empty() {
                    continue;
                }
                let parsed = parse_irc_line(&raw);
                if parsed.command == "PING" {
                    let token = parsed.params.first().cloned().unwrap_or_default();
                    stream.write_all(format!("PONG :{token}\r\n").as_bytes())?;
                    stream.flush()?;
                    continue;
                }
                if matches!(parsed.command.as_str(), "432" | "433") && !registered {
                    current_nick = next_irc_nick(&adapter.nickname, &current_nick);
                    adapter.set_current_nickname(current_nick.clone());
                    stream.write_all(format!("NICK {current_nick}\r\n").as_bytes())?;
                    stream.flush()?;
                    continue;
                }
                if parsed.command == "001" && !registered {
                    registered = true;
                    if let Some(nick) = parsed.params.first().filter(|nick| !nick.is_empty()) {
                        current_nick = nick.to_string();
                        adapter.set_current_nickname(current_nick.clone());
                    }
                    if let Some(password) = adapter.nickserv_password.as_deref() {
                        stream.write_all(
                            format!("PRIVMSG NickServ :IDENTIFY {password}\r\n").as_bytes(),
                        )?;
                    }
                    for channel in &adapter.channels {
                        stream.write_all(format!("JOIN {channel}\r\n").as_bytes())?;
                    }
                    stream.flush()?;
                    continue;
                }
                if parsed.command == "NICK" {
                    let is_current_nick = parsed
                        .prefix
                        .as_deref()
                        .map(irc_nick)
                        .is_some_and(|nick| nick.eq_ignore_ascii_case(&current_nick));
                    if is_current_nick {
                        if let Some(nick) = parsed.params.first().filter(|nick| !nick.is_empty()) {
                            current_nick = nick.trim_start_matches(':').to_string();
                            adapter.set_current_nickname(current_nick.clone());
                            continue;
                        }
                    }
                }
                if let Some(input) = adapter.inbound_from_privmsg(&parsed) {
                    inbound.submit(input)?;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error).context("IRC read failed"),
        }
    }
}

fn connect_irc(server: &str, port: u16, use_tls: bool) -> Result<Box<dyn IrcReadWrite + Send>> {
    let tcp = TcpStream::connect((server, port))
        .with_context(|| format!("failed to connect to IRC server {server}:{port}"))?;
    tcp.set_read_timeout(Some(Duration::from_millis(250)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
    if !use_tls {
        return Ok(Box::new(tcp));
    }
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = ServerName::try_from(server.to_string())
        .map_err(|_| anyhow!("invalid IRC TLS server name `{server}`"))?;
    let conn = ClientConnection::new(Arc::new(config), server_name)
        .context("failed to create IRC TLS client connection")?;
    Ok(Box::new(StreamOwned::new(conn, tcp)))
}

fn write_irc_line(stream: &mut Box<dyn IrcReadWrite + Send>, line: Option<String>) -> Result<()> {
    if let Some(line) = line {
        stream.write_all(line.as_bytes())?;
        stream.flush()?;
    }
    Ok(())
}

fn parse_irc_line(raw: &str) -> ParsedIrcLine {
    let mut rest = raw.trim().to_string();
    let mut tags = HashMap::new();
    if rest.starts_with('@') {
        if let Some((raw_tags, after_tags)) = rest.split_once(' ') {
            for tag in raw_tags.trim_start_matches('@').split(';') {
                let (key, value) = tag.split_once('=').unwrap_or((tag, ""));
                if !key.is_empty() {
                    tags.insert(key.to_string(), unescape_irc_tag(value));
                }
            }
            rest = after_tags.to_string();
        }
    }
    let prefix = if rest.starts_with(':') {
        let stripped = rest[1..].to_string();
        if let Some((prefix, after)) = stripped.split_once(' ') {
            let prefix = prefix.to_string();
            rest = after.to_string();
            Some(prefix)
        } else {
            return ParsedIrcLine {
                tags,
                prefix: Some(stripped),
                command: String::new(),
                params: Vec::new(),
            };
        }
    } else {
        None
    };
    let mut trailing = None;
    if let Some((before, after)) = rest.split_once(" :") {
        trailing = Some(after.to_string());
        rest = before.to_string();
    }
    let mut parts = rest
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let command = parts.first().cloned().unwrap_or_default();
    if !parts.is_empty() {
        parts.remove(0);
    }
    if let Some(trailing) = trailing {
        parts.push(trailing);
    }
    ParsedIrcLine {
        tags,
        prefix,
        command,
        params: parts,
    }
}

fn unescape_irc_tag(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            output.push(match character {
                ':' => ';',
                's' => ' ',
                'r' => '\r',
                'n' => '\n',
                '\\' => '\\',
                other => other,
            });
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            output.push(character);
        }
    }
    output
}

fn irc_nick(prefix: &str) -> &str {
    prefix.split('!').next().unwrap_or(prefix)
}

fn strip_mention(text: &str, nick: &str) -> Option<String> {
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    let nick_lower = nick.to_ascii_lowercase();
    for prefix in [
        format!("{nick_lower}:"),
        format!("{nick_lower},"),
        format!("{nick_lower} "),
    ] {
        if lower.starts_with(&prefix) {
            return Some(trimmed[prefix.len()..].trim().to_string());
        }
    }
    let at_nick = format!("@{nick_lower}");
    if lower.starts_with(&at_nick) {
        let rest = trimmed[at_nick.len()..]
            .trim_start_matches(|ch: char| ch == ':' || ch == ',' || ch.is_whitespace())
            .trim();
        return Some(rest.to_string());
    }
    if lower.contains(&at_nick) {
        return Some(trimmed.to_string());
    }
    None
}

fn sanitize_irc_text(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
        .replace('\0', "")
        .trim()
        .to_string()
}

fn irc_chunks(text: &str) -> Vec<String> {
    irc_chunks_for_target(text, "#channel", IRC_TEXT_LIMIT)
}

fn irc_chunks_for_target(text: &str, target: &str, user_limit: usize) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let overhead = format!("PRIVMSG {target} :\r\n").len();
    let limit = user_limit.min(510_usize.saturating_sub(overhead)).max(1);
    let mut chunks = Vec::new();
    for paragraph in strip_irc_markdown(text).lines() {
        let mut remaining = sanitize_irc_text(paragraph);
        while !remaining.is_empty() {
            if remaining.len() <= limit {
                chunks.push(remaining);
                break;
            }
            let mut split_at = remaining
                .char_indices()
                .take_while(|(index, character)| index + character.len_utf8() <= limit)
                .map(|(index, character)| index + character.len_utf8())
                .last()
                .unwrap_or(remaining.len().min(limit));
            if let Some(space) = remaining[..split_at].rfind(' ') {
                if space > split_at / 3 {
                    split_at = space;
                }
            }
            let chunk = remaining[..split_at].trim_end().to_string();
            if !chunk.is_empty() {
                chunks.push(chunk);
            }
            remaining = remaining[split_at..].trim_start().to_string();
        }
    }
    chunks
}

fn valid_irc_target(target: &str) -> bool {
    !target.is_empty()
        && !target
            .chars()
            .any(|character| character.is_whitespace() || character == '\0')
}

fn strip_irc_markdown(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '!' if chars.peek() == Some(&'[') => {
                chars.next();
                while let Some(next) = chars.next() {
                    if next == ']' {
                        break;
                    }
                }
                if chars.peek() == Some(&'(') {
                    chars.next();
                    let mut url = String::new();
                    while let Some(next) = chars.next() {
                        if next == ')' {
                            break;
                        }
                        url.push(next);
                    }
                    output.push_str(&url);
                }
            }
            '[' => {
                let mut label = String::new();
                while let Some(next) = chars.next() {
                    if next == ']' {
                        break;
                    }
                    label.push(next);
                }
                if chars.peek() == Some(&'(') {
                    chars.next();
                    let mut url = String::new();
                    while let Some(next) = chars.next() {
                        if next == ')' {
                            break;
                        }
                        url.push(next);
                    }
                    if !label.is_empty() && !url.is_empty() {
                        output.push_str(&label);
                        output.push_str(" (");
                        output.push_str(&url);
                        output.push(')');
                    } else {
                        output.push_str(&label);
                        output.push_str(&url);
                    }
                } else {
                    output.push_str(&label);
                }
            }
            '`' | '*' | '_' => {}
            _ => output.push(character),
        }
    }
    output
}

fn next_irc_nick(base: &str, current: &str) -> String {
    let base =
        base.trim_end_matches(|character: char| character == '_' || character.is_ascii_digit());
    if current == base {
        return format!("{base}_");
    }
    let Some((prefix, suffix)) = current.rsplit_once('_') else {
        return format!("{base}_1");
    };
    match suffix.parse::<usize>() {
        Ok(value) => format!("{prefix}_{}", value + 1),
        Err(_) => format!("{base}_1"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_privmsg_with_trailing_text() {
        let parsed = parse_irc_line(":alice!u@h PRIVMSG #room :bot: hello world");
        assert_eq!(parsed.prefix.as_deref(), Some("alice!u@h"));
        assert_eq!(parsed.command, "PRIVMSG");
        assert_eq!(parsed.params, vec!["#room", "bot: hello world"]);
    }

    #[test]
    fn parses_ircv3_tags() {
        let parsed = parse_irc_line(
            "@badge-info=;display-name=Alice\\sA;user-id=123 :alice!u@h PRIVMSG #room :hello",
        );
        assert_eq!(
            parsed.tags.get("display-name").map(String::as_str),
            Some("Alice A")
        );
        assert_eq!(parsed.tags.get("user-id").map(String::as_str), Some("123"));
        assert_eq!(parsed.prefix.as_deref(), Some("alice!u@h"));
    }

    #[test]
    fn mention_gate_strips_prefix() {
        assert_eq!(
            strip_mention("duckagent: hello", "duckagent").as_deref(),
            Some("hello")
        );
        assert!(strip_mention("hello", "duckagent").is_none());
    }

    #[test]
    fn irc_chunks_split_long_text() {
        assert_eq!(irc_chunks(&"x".repeat(IRC_TEXT_LIMIT + 1)).len(), 2);
    }
}
