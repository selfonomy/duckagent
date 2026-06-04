use super::super::GatewayInboundDispatch;
use super::websocket::{ChannelWebSocket, is_transient_read_error, set_read_timeout};
use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::{Message as WsMessage, connect};
use url::Url;

const ENDPOINT_PATH: &str = "/callback/ws/endpoint";
const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(120);
const RECONNECT_BACKOFF: &[u64] = &[2, 4, 8, 16, 30, 60];
const FRAGMENT_TTL: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(in crate::gateway::channels) struct FeishuWsConfig {
    pub channel: String,
    pub api_base: String,
    pub app_id: String,
    pub app_secret: String,
}

pub(in crate::gateway::channels) fn spawn_feishu_ws_loop<F>(
    config: FeishuWsConfig,
    inbound: GatewayInboundDispatch,
    handler: F,
) -> Result<()>
where
    F: Fn(Value, &GatewayInboundDispatch) -> Result<()> + Send + Sync + 'static,
{
    thread::Builder::new()
        .name(format!("gateway-{}-websocket", config.channel))
        .spawn(move || websocket_loop(config, inbound, handler))
        .context("failed to spawn Feishu/Lark websocket thread")?;
    Ok(())
}

fn websocket_loop<F>(config: FeishuWsConfig, inbound: GatewayInboundDispatch, handler: F)
where
    F: Fn(Value, &GatewayInboundDispatch) -> Result<()> + Send + Sync + 'static,
{
    let mut attempt = 0usize;
    loop {
        match consume_websocket_once(&config, &inbound, &handler) {
            Ok(()) => attempt = 0,
            Err(error) => eprintln!("{} websocket disconnected: {error:#}", config.channel),
        }
        let sleep = RECONNECT_BACKOFF
            .get(attempt)
            .copied()
            .unwrap_or(*RECONNECT_BACKOFF.last().unwrap_or(&60));
        attempt = attempt.saturating_add(1);
        thread::sleep(Duration::from_secs(sleep));
    }
}

fn consume_websocket_once<F>(
    config: &FeishuWsConfig,
    inbound: &GatewayInboundDispatch,
    handler: &F,
) -> Result<()>
where
    F: Fn(Value, &GatewayInboundDispatch) -> Result<()> + Send + Sync + 'static,
{
    let endpoint = discover_endpoint(config)?;
    let service_id = service_id_from_url(&endpoint.url)?;
    let (mut socket, _) = connect(endpoint.url.as_str())
        .with_context(|| format!("{} websocket connect failed", config.channel))?;
    eprintln!(
        "{} websocket connected to {}",
        config.channel,
        websocket_host_label(&endpoint.url)
    );
    set_read_timeout(&mut socket, Duration::from_secs(10));

    let mut ping_interval = endpoint.ping_interval.unwrap_or(DEFAULT_PING_INTERVAL);
    let mut next_ping = Instant::now() + ping_interval;
    let mut fragments = FragmentStore::default();

    loop {
        if Instant::now() >= next_ping {
            send_ping(&mut socket, service_id)?;
            next_ping = Instant::now() + ping_interval;
        }
        match read_frame(&mut socket) {
            Ok(Some(frame)) => {
                if let Some(updated) = handle_frame(
                    &config.channel,
                    &mut socket,
                    frame,
                    inbound,
                    handler,
                    &mut fragments,
                )? {
                    ping_interval = updated;
                    next_ping = Instant::now() + ping_interval;
                }
            }
            Ok(None) => {}
            Err(error) if is_transient_read_error(&error) => {}
            Err(error) => return Err(error),
        }
    }
}

#[derive(Debug)]
struct WsEndpoint {
    url: String,
    ping_interval: Option<Duration>,
}

fn discover_endpoint(config: &FeishuWsConfig) -> Result<WsEndpoint> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build Feishu/Lark websocket endpoint client")?;
    let url = format!("{}{}", config.api_base.trim_end_matches('/'), ENDPOINT_PATH);
    let response = client
        .post(&url)
        .header("locale", "zh")
        .header("User-Agent", "duckagent")
        .json(&json!({
            "AppID": config.app_id,
            "AppSecret": config.app_secret,
        }))
        .send()
        .with_context(|| format!("{} websocket endpoint discovery failed", config.channel))?;
    let status = response.status();
    let value: Value = response
        .json()
        .context("Feishu/Lark websocket endpoint discovery returned invalid JSON")?;
    if !status.is_success() || value["code"].as_i64().unwrap_or(-1) != 0 {
        bail!(
            "{} websocket endpoint discovery failed with status {status}: {value}",
            config.channel
        );
    }
    let data = &value["data"];
    let url = data["URL"]
        .as_str()
        .or_else(|| data["url"].as_str())
        .ok_or_else(|| anyhow!("Feishu/Lark websocket endpoint response missing URL"))?
        .to_string();
    let ping_interval = data["ClientConfig"]["PingInterval"]
        .as_u64()
        .or_else(|| data["client_config"]["ping_interval"].as_u64())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs);
    Ok(WsEndpoint { url, ping_interval })
}

fn service_id_from_url(raw: &str) -> Result<i32> {
    let url = Url::parse(raw).context("Feishu/Lark websocket URL is invalid")?;
    let service_id = url
        .query_pairs()
        .find(|(key, _)| key == "service_id")
        .map(|(_, value)| value.to_string())
        .ok_or_else(|| anyhow!("Feishu/Lark websocket URL missing service_id"))?;
    service_id
        .parse::<i32>()
        .context("Feishu/Lark websocket service_id is invalid")
}

fn handle_frame<F>(
    channel: &str,
    socket: &mut ChannelWebSocket,
    mut frame: WsFrame,
    inbound: &GatewayInboundDispatch,
    handler: &F,
    fragments: &mut FragmentStore,
) -> Result<Option<Duration>>
where
    F: Fn(Value, &GatewayInboundDispatch) -> Result<()> + Send + Sync + 'static,
{
    match frame.method {
        0 => handle_control_frame(frame),
        1 => {
            let started_at = Instant::now();
            let payload = match fragments.payload_for(&frame)? {
                Some(payload) => payload,
                None => return Ok(None),
            };
            let payload = decode_payload(&frame, payload)?;
            let mut status = 200u16;
            if matches!(frame.header("type").as_deref(), Some("event" | "card")) {
                let value: Value = serde_json::from_slice(&payload)
                    .context("Feishu/Lark websocket event payload is invalid JSON")?;
                eprintln!(
                    "{channel} websocket event received: {}",
                    websocket_event_type(&value)
                );
                if let Err(error) = handler(value, inbound) {
                    status = 500;
                    eprintln!("Feishu/Lark websocket event handler failed: {error:#}");
                }
            }
            frame.push_header(
                "biz_rt",
                started_at.elapsed().as_millis().to_string().as_str(),
            );
            frame.payload = json!({"code": status}).to_string().into_bytes();
            socket
                .send(WsMessage::Binary(frame.encode()))
                .context("Feishu/Lark websocket ack failed")?;
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn websocket_host_label(raw: &str) -> String {
    Url::parse(raw)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "endpoint".to_string())
}

fn websocket_event_type(value: &Value) -> &str {
    value["header"]["event_type"]
        .as_str()
        .or_else(|| value["event_type"].as_str())
        .or_else(|| value["type"].as_str())
        .unwrap_or("unknown")
}

fn decode_payload(frame: &WsFrame, payload: Vec<u8>) -> Result<Vec<u8>> {
    let Some(encoding) = frame.payload_encoding.as_deref() else {
        return Ok(payload);
    };
    match encoding.trim().to_ascii_lowercase().as_str() {
        "" | "none" | "identity" => Ok(payload),
        "gzip" | "gz" => {
            let mut decoder = GzDecoder::new(payload.as_slice());
            let mut out = Vec::new();
            decoder
                .read_to_end(&mut out)
                .context("Feishu/Lark websocket gzip payload decode failed")?;
            Ok(out)
        }
        other => bail!("unsupported Feishu/Lark websocket payload encoding `{other}`"),
    }
}

fn handle_control_frame(frame: WsFrame) -> Result<Option<Duration>> {
    if frame.header("type").as_deref() != Some("pong") || frame.payload.is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(&frame.payload)
        .context("Feishu/Lark websocket pong payload is invalid JSON")?;
    let interval = value["PingInterval"]
        .as_u64()
        .or_else(|| value["ping_interval"].as_u64())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs);
    Ok(interval)
}

fn send_ping(socket: &mut ChannelWebSocket, service_id: i32) -> Result<()> {
    let frame = WsFrame {
        service: service_id,
        method: 0,
        headers: vec![("type".to_string(), "ping".to_string())],
        ..Default::default()
    };
    socket
        .send(WsMessage::Binary(frame.encode()))
        .context("Feishu/Lark websocket ping failed")
}

fn read_frame(socket: &mut ChannelWebSocket) -> Result<Option<WsFrame>> {
    loop {
        let message = socket.read().context("Feishu/Lark websocket read failed")?;
        match message {
            WsMessage::Binary(bytes) => return WsFrame::decode(&bytes).map(Some),
            WsMessage::Ping(payload) => {
                socket
                    .send(WsMessage::Pong(payload))
                    .context("Feishu/Lark websocket pong failed")?;
            }
            WsMessage::Pong(_) | WsMessage::Text(_) => return Ok(None),
            WsMessage::Close(frame) => bail!("Feishu/Lark websocket closed: {frame:?}"),
            _ => {}
        }
    }
}

#[derive(Default)]
struct FragmentStore {
    fragments: HashMap<String, FragmentEntry>,
}

struct FragmentEntry {
    created_at: Instant,
    parts: Vec<Option<Vec<u8>>>,
}

impl FragmentStore {
    fn payload_for(&mut self, frame: &WsFrame) -> Result<Option<Vec<u8>>> {
        self.prune();
        let total = frame
            .header("sum")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(1);
        if total <= 1 {
            return Ok(Some(frame.payload.clone()));
        }
        let message_id = frame
            .header("message_id")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("Feishu/Lark fragmented websocket frame missing message_id"))?;
        let seq = frame
            .header("seq")
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or_else(|| anyhow!("Feishu/Lark fragmented websocket frame missing seq"))?;
        if seq >= total {
            bail!("Feishu/Lark fragmented websocket frame seq {seq} >= sum {total}");
        }
        let entry = self
            .fragments
            .entry(message_id.clone())
            .or_insert_with(|| FragmentEntry {
                created_at: Instant::now(),
                parts: vec![None; total],
            });
        if entry.parts.len() != total {
            entry.parts = vec![None; total];
            entry.created_at = Instant::now();
        }
        entry.parts[seq] = Some(frame.payload.clone());
        if entry.parts.iter().all(Option::is_some) {
            let entry = self
                .fragments
                .remove(&message_id)
                .expect("fragment entry exists");
            let mut payload = Vec::new();
            for part in entry.parts.into_iter().flatten() {
                payload.extend(part);
            }
            Ok(Some(payload))
        } else {
            Ok(None)
        }
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.fragments
            .retain(|_, entry| now.duration_since(entry.created_at) <= FRAGMENT_TTL);
    }
}

#[derive(Debug, Clone, Default)]
struct WsFrame {
    seq_id: u64,
    log_id: u64,
    service: i32,
    method: i32,
    headers: Vec<(String, String)>,
    payload_encoding: Option<String>,
    payload_type: Option<String>,
    payload: Vec<u8>,
    log_id_new: Option<String>,
}

impl WsFrame {
    fn header(&self, key: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.clone())
    }

    fn push_header(&mut self, key: &str, value: &str) {
        self.headers.push((key.to_string(), value.to_string()));
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut input = ProtoInput::new(bytes);
        let mut frame = WsFrame::default();
        while !input.is_empty() {
            let (field, wire) = input.read_key()?;
            match (field, wire) {
                (1, 0) => frame.seq_id = input.read_varint()?,
                (2, 0) => frame.log_id = input.read_varint()?,
                (3, 0) => frame.service = input.read_varint()? as i32,
                (4, 0) => frame.method = input.read_varint()? as i32,
                (5, 2) => frame.headers.push(decode_header(input.read_bytes()?)?),
                (6, 2) => frame.payload_encoding = Some(input.read_string()?),
                (7, 2) => frame.payload_type = Some(input.read_string()?),
                (8, 2) => frame.payload = input.read_bytes()?.to_vec(),
                (9, 2) => frame.log_id_new = Some(input.read_string()?),
                (_, wire) => input.skip(wire)?,
            }
        }
        Ok(frame)
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_varint_field(&mut out, 1, self.seq_id);
        write_varint_field(&mut out, 2, self.log_id);
        write_varint_field(&mut out, 3, self.service as u64);
        write_varint_field(&mut out, 4, self.method as u64);
        for (key, value) in &self.headers {
            let mut header = Vec::new();
            write_string_field(&mut header, 1, key);
            write_string_field(&mut header, 2, value);
            write_bytes_field(&mut out, 5, &header);
        }
        if let Some(value) = &self.payload_encoding {
            write_string_field(&mut out, 6, value);
        }
        if let Some(value) = &self.payload_type {
            write_string_field(&mut out, 7, value);
        }
        write_bytes_field(&mut out, 8, &self.payload);
        if let Some(value) = &self.log_id_new {
            write_string_field(&mut out, 9, value);
        }
        out
    }
}

fn decode_header(bytes: &[u8]) -> Result<(String, String)> {
    let mut input = ProtoInput::new(bytes);
    let mut key = String::new();
    let mut value = String::new();
    while !input.is_empty() {
        let (field, wire) = input.read_key()?;
        match (field, wire) {
            (1, 2) => key = input.read_string()?,
            (2, 2) => value = input.read_string()?,
            (_, wire) => input.skip(wire)?,
        }
    }
    Ok((key, value))
}

struct ProtoInput<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ProtoInput<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn read_key(&mut self) -> Result<(u64, u8)> {
        let key = self.read_varint()?;
        Ok((key >> 3, (key & 0b111) as u8))
    }

    fn read_varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = *self
                .bytes
                .get(self.pos)
                .ok_or_else(|| anyhow!("protobuf varint truncated"))?;
            self.pos += 1;
            value |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift >= 64 {
                bail!("protobuf varint too large");
            }
        }
    }

    fn read_bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.read_varint()? as usize;
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| anyhow!("protobuf length overflow"))?;
        if end > self.bytes.len() {
            bail!("protobuf length-delimited field truncated");
        }
        let out = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn read_string(&mut self) -> Result<String> {
        let bytes = self.read_bytes()?;
        String::from_utf8(bytes.to_vec()).context("protobuf string field is not UTF-8")
    }

    fn skip(&mut self, wire: u8) -> Result<()> {
        match wire {
            0 => {
                let _ = self.read_varint()?;
            }
            1 => self.pos = self.pos.saturating_add(8),
            2 => {
                let _ = self.read_bytes()?;
            }
            5 => self.pos = self.pos.saturating_add(4),
            _ => bail!("unsupported protobuf wire type {wire}"),
        }
        if self.pos > self.bytes.len() {
            bail!("protobuf field skip passed end of buffer");
        }
        Ok(())
    }
}

fn write_key(out: &mut Vec<u8>, field: u64, wire: u8) {
    write_varint(out, (field << 3) | u64::from(wire));
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn write_varint_field(out: &mut Vec<u8>, field: u64, value: u64) {
    write_key(out, field, 0);
    write_varint(out, value);
}

fn write_bytes_field(out: &mut Vec<u8>, field: u64, value: &[u8]) {
    write_key(out, field, 2);
    write_varint(out, value.len() as u64);
    out.extend(value);
}

fn write_string_field(out: &mut Vec<u8>, field: u64, value: &str) {
    write_bytes_field(out, field, value.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips_required_fields() -> Result<()> {
        let frame = WsFrame {
            seq_id: 7,
            log_id: 9,
            service: 3,
            method: 1,
            headers: vec![("type".to_string(), "event".to_string())],
            payload: br#"{"hello":"world"}"#.to_vec(),
            ..Default::default()
        };
        let decoded = WsFrame::decode(&frame.encode())?;
        assert_eq!(decoded.seq_id, 7);
        assert_eq!(decoded.log_id, 9);
        assert_eq!(decoded.service, 3);
        assert_eq!(decoded.method, 1);
        assert_eq!(decoded.header("type").as_deref(), Some("event"));
        assert_eq!(decoded.payload, br#"{"hello":"world"}"#);
        Ok(())
    }

    #[test]
    fn fragments_combine_payload() -> Result<()> {
        let mut store = FragmentStore::default();
        let mut first = WsFrame {
            payload: b"hel".to_vec(),
            ..Default::default()
        };
        first.headers = vec![
            ("message_id".to_string(), "m1".to_string()),
            ("sum".to_string(), "2".to_string()),
            ("seq".to_string(), "0".to_string()),
        ];
        let mut second = first.clone();
        second.payload = b"lo".to_vec();
        second.headers = vec![
            ("message_id".to_string(), "m1".to_string()),
            ("sum".to_string(), "2".to_string()),
            ("seq".to_string(), "1".to_string()),
        ];
        assert!(store.payload_for(&first)?.is_none());
        assert_eq!(store.payload_for(&second)?, Some(b"hello".to_vec()));
        Ok(())
    }

    #[test]
    fn gzip_payload_decodes_before_event_json_parse() -> Result<()> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(br#"{"hello":"world"}"#)?;
        let payload = encoder.finish()?;
        let frame = WsFrame {
            payload_encoding: Some("gzip".to_string()),
            ..Default::default()
        };
        assert_eq!(decode_payload(&frame, payload)?, br#"{"hello":"world"}"#);
        Ok(())
    }

    #[test]
    fn websocket_event_type_accepts_header_shape() {
        let value = json!({
            "header": {
                "event_type": "im.message.receive_v1"
            }
        });
        assert_eq!(websocket_event_type(&value), "im.message.receive_v1");
    }
}
