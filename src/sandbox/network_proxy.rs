use crate::approval::{ApprovalDecision, ApprovalProvider, ApprovalResponse, RuleHit};
use crate::sandbox::config::{
    NetworkMode, PermissionAction, ResolvedSandbox, SECRET_REVERSE_ROUTE_PREFIX,
    append_network_address_action_to_current_preset, append_network_host_action_to_current_preset,
};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use url::Url;

const MAX_HEADER_BYTES: usize = 64 * 1024;
const PROXY_ENV_KEYS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "WS_PROXY",
    "WSS_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "ws_proxy",
    "wss_proxy",
    "NPM_CONFIG_PROXY",
    "NPM_CONFIG_HTTP_PROXY",
    "NPM_CONFIG_HTTPS_PROXY",
    "YARN_HTTP_PROXY",
    "YARN_HTTPS_PROXY",
];
const NO_PROXY_ENV_KEYS: &[&str] = &[
    "NO_PROXY",
    "no_proxy",
    "NPM_CONFIG_NO_PROXY",
    "NPM_CONFIG_NOPROXY",
    "npm_config_no_proxy",
    "npm_config_noproxy",
    "YARN_NO_PROXY",
    "yarn_no_proxy",
    "GLOBAL_AGENT_NO_PROXY",
    "global_agent_no_proxy",
];
pub const MANAGED_PROXY_ENV_KEY: &str = "DUCKAGENT_MANAGED_NETWORK_PROXY";

pub struct ManagedNetworkProxy {
    addr: SocketAddr,
    env_overrides: BTreeMap<String, String>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ManagedNetworkProxy {
    pub fn env_overrides(&self) -> BTreeMap<String, String> {
        self.env_overrides.clone()
    }
}

impl Drop for ManagedNetworkProxy {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(100));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn start_if_supported(
    sandbox: &ResolvedSandbox,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<Option<ManagedNetworkProxy>> {
    if !matches!(sandbox.preset.network.mode, NetworkMode::Proxy) {
        return Ok(None);
    }

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        return start(sandbox, approval_provider).map(Some);
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = sandbox;
        Ok(None)
    }
}

fn start(
    sandbox: &ResolvedSandbox,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<ManagedNetworkProxy> {
    let listener = bind_managed_proxy_listener(sandbox)?;
    listener
        .set_nonblocking(true)
        .context("failed to configure sandbox proxy listener")?;
    let addr = listener
        .local_addr()
        .context("failed to read sandbox proxy address")?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut env_overrides = proxy_env_overrides(addr);
    env_overrides.extend(sandbox.preset.secrets.proxy_env_overrides(addr));
    let state = Arc::new(ProxyState {
        addr,
        sandbox: sandbox.clone(),
        upstream: UpstreamProxy::from_env(),
        approval_provider,
        session_allowed_hosts: Mutex::new(HashSet::new()),
        session_denied_hosts: Mutex::new(HashSet::new()),
    });
    let thread_shutdown = Arc::clone(&shutdown);
    let handle = thread::Builder::new()
        .name("duckagent-sandbox-network-proxy".to_string())
        .spawn(move || accept_loop(listener, state, thread_shutdown))
        .context("failed to start sandbox proxy thread")?;

    Ok(ManagedNetworkProxy {
        addr,
        env_overrides,
        shutdown,
        handle: Some(handle),
    })
}

#[cfg(target_os = "windows")]
fn bind_managed_proxy_listener(sandbox: &ResolvedSandbox) -> Result<TcpListener> {
    let port = crate::sandbox::windows_setup::prepare_managed_proxy_port(sandbox)?;
    match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        Ok(listener) => Ok(listener),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            let refreshed_port =
                crate::sandbox::windows_setup::refresh_managed_proxy_port_after_bind_failure(
                    sandbox, port,
                )?;
            TcpListener::bind((Ipv4Addr::LOCALHOST, refreshed_port)).with_context(|| {
                format!(
                    "failed to bind Windows managed sandbox proxy on refreshed marker port 127.0.0.1:{refreshed_port} after previous marker port {port} was busy"
                )
            })
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to bind Windows managed sandbox proxy on 127.0.0.1:{}",
                port
            )
        }),
    }
}

#[cfg(not(target_os = "windows"))]
fn bind_managed_proxy_listener(_sandbox: &ResolvedSandbox) -> Result<TcpListener> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("failed to bind sandbox proxy")
}

fn accept_loop(listener: TcpListener, state: Arc<ProxyState>, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let state = Arc::clone(&state);
                let _ = thread::Builder::new()
                    .name("duckagent-sandbox-network-proxy-conn".to_string())
                    .spawn(move || {
                        let _ = handle_client(stream, state);
                    });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
}

fn handle_client(mut client: TcpStream, state: Arc<ProxyState>) -> Result<()> {
    let mut first = [0_u8; 1];
    if client.peek(&mut first)? == 0 {
        return Ok(());
    }
    if first[0] == 0x05 {
        return handle_socks5(client, state);
    }

    let header = read_http_header(&mut client)?;
    if header.is_empty() {
        return Ok(());
    }
    let request = ProxyRequest::parse(&header)?;
    if let Some(reverse) = SecretReverseRequest::from_request(&state, &request)? {
        let endpoint = state.ensure_allowed(&reverse.host, &reverse.target)?;
        return handle_secret_reverse(client, header, request, reverse, endpoint, &state);
    }
    let endpoint = state.ensure_allowed(&request.host, &request.target)?;

    match request.kind {
        ProxyRequestKind::Connect => handle_connect(client, &request.target, &endpoint, &state),
        ProxyRequestKind::Http => handle_http(client, header, request, endpoint, &state),
    }
}

fn handle_socks5(mut client: TcpStream, state: Arc<ProxyState>) -> Result<()> {
    let mut greeting = [0_u8; 2];
    client.read_exact(&mut greeting)?;
    if greeting[0] != 0x05 {
        bail!("unsupported SOCKS version {}", greeting[0]);
    }
    let mut methods = vec![0_u8; greeting[1] as usize];
    client.read_exact(&mut methods)?;
    client.write_all(&[0x05, 0x00])?;

    let request = Socks5Request::read_from(&mut client)?;
    let endpoint = state.ensure_allowed(&request.host, &request.target)?;
    let upstream = match state.connect_target_or_proxy(&request.target, &endpoint) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = write_socks5_reply(&mut client, 0x05);
            return Err(error);
        }
    };
    write_socks5_reply(&mut client, 0x00)?;
    proxy_bidirectional(client, upstream)
}

fn write_socks5_reply(client: &mut TcpStream, status: u8) -> io::Result<()> {
    client.write_all(&[0x05, status, 0x00, 0x01, 127, 0, 0, 1, 0, 0])
}

fn handle_connect(
    mut client: TcpStream,
    target: &str,
    endpoint: &EndpointPolicy,
    state: &ProxyState,
) -> Result<()> {
    let upstream = state.connect_target_or_proxy(target, endpoint)?;
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    proxy_bidirectional(client, upstream)
}

fn handle_http(
    client: TcpStream,
    header: Vec<u8>,
    request: ProxyRequest,
    endpoint: EndpointPolicy,
    state: &ProxyState,
) -> Result<()> {
    let mut upstream = state.connect_http_upstream(&request.target, &endpoint)?;
    if state.upstream_is_http_proxy() {
        upstream.write_all(&header)?;
    } else {
        upstream.write_all(&rewrite_absolute_http_request(&header, &request)?)?;
    }
    proxy_bidirectional(client, upstream)
}

fn proxy_bidirectional(mut left: TcpStream, mut right: TcpStream) -> Result<()> {
    let mut left_reader = left.try_clone()?;
    let mut right_writer = right.try_clone()?;
    let upload = thread::spawn(move || {
        let _ = io::copy(&mut left_reader, &mut right_writer);
        let _ = right_writer.shutdown(Shutdown::Write);
    });

    let _ = io::copy(&mut right, &mut left);
    let _ = left.shutdown(Shutdown::Write);
    let _ = upload.join();
    Ok(())
}

fn read_http_header(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut byte = [0_u8; 1];
    while out.len() < MAX_HEADER_BYTES {
        let read = stream.read(&mut byte)?;
        if read == 0 {
            break;
        }
        out.push(byte[0]);
        if out.ends_with(b"\r\n\r\n") {
            return Ok(out);
        }
    }
    if out.len() >= MAX_HEADER_BYTES {
        bail!("proxy request header exceeded {MAX_HEADER_BYTES} bytes");
    }
    Ok(out)
}

fn rewrite_absolute_http_request(header: &[u8], request: &ProxyRequest) -> Result<Vec<u8>> {
    if !request.absolute_uri {
        return Ok(header.to_vec());
    }
    let text = std::str::from_utf8(header).context("proxy request header is not UTF-8")?;
    let Some((first_line, rest)) = text.split_once("\r\n") else {
        bail!("proxy request header is missing request line terminator");
    };
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let _uri = parts.next().unwrap_or("/");
    let version = parts.next().unwrap_or("HTTP/1.1");
    let path = request
        .url_path
        .as_deref()
        .filter(|path| !path.is_empty())
        .unwrap_or("/");
    Ok(format!("{method} {path} {version}\r\n{rest}").into_bytes())
}

#[derive(Debug, Clone)]
struct SecretReverseRequest {
    secret_name: String,
    target: String,
    host: String,
    upstream_url: Url,
}

impl SecretReverseRequest {
    fn from_request(state: &ProxyState, request: &ProxyRequest) -> Result<Option<Self>> {
        if !request_points_at_proxy(request, state.addr)? {
            return Ok(None);
        }
        let Some(path) = request.url_path.as_deref() else {
            return Ok(None);
        };
        let Some((secret_name, suffix)) = parse_secret_reverse_path(path) else {
            return Ok(None);
        };
        let Some(secret) = state.sandbox.preset.secrets.0.get(&secret_name) else {
            return Ok(None);
        };
        let upstream_url = join_upstream_url(secret.upstream_url()?, &suffix)?;
        let host = upstream_url
            .host_str()
            .ok_or_else(|| anyhow!("secret upstream URL has no host"))?
            .to_string();
        let port = upstream_url.port_or_known_default().unwrap_or(80);
        Ok(Some(Self {
            secret_name,
            target: target_with_port(&host, port),
            host,
            upstream_url,
        }))
    }
}

fn handle_secret_reverse(
    mut client: TcpStream,
    header: Vec<u8>,
    request: ProxyRequest,
    reverse: SecretReverseRequest,
    _endpoint: EndpointPolicy,
    state: &ProxyState,
) -> Result<()> {
    let Some(secret) = state.sandbox.preset.secrets.0.get(&reverse.secret_name) else {
        send_http_error(&mut client, 404, "secret route not found")?;
        bail!("secret route `{}` not found", reverse.secret_name);
    };
    let Some(secret_value) = secret.source_value() else {
        send_http_error(&mut client, 500, "secret source env missing")?;
        bail!("secret source env `{}` is missing", secret.source_env);
    };
    let body = match read_proxy_request_body(&mut client, &header) {
        Ok(body) => body,
        Err(error) => {
            send_http_error(&mut client, 501, "unsupported reverse proxy request body")?;
            return Err(error);
        }
    };
    let response = send_reverse_request(
        &header,
        &request,
        &reverse.upstream_url,
        secret,
        &secret_value,
        body,
    )?;
    write_reverse_response(&mut client, response)?;
    audit_secret_reverse(&state.sandbox, &reverse, &request);
    Ok(())
}

fn send_reverse_request(
    header: &[u8],
    request: &ProxyRequest,
    upstream_url: &Url,
    secret: &crate::sandbox::config::SecretConfig,
    secret_value: &str,
    body: Vec<u8>,
) -> Result<reqwest::blocking::Response> {
    let client = reqwest::blocking::Client::new();
    let method = reqwest::Method::from_bytes(request.method.as_bytes())
        .with_context(|| format!("unsupported HTTP method `{}`", request.method))?;
    let mut builder = client.request(method, upstream_url.clone());
    for (name, value) in parse_forward_headers(header)? {
        if should_skip_reverse_header(&name, &secret.inject.header) {
            continue;
        }
        builder = builder.header(name, value);
    }
    let injected = secret.inject.format.replace("{}", secret_value);
    builder = builder.header(&secret.inject.header, injected);
    if !body.is_empty() {
        builder = builder.body(body);
    }
    builder
        .send()
        .with_context(|| format!("failed reverse proxy request to `{upstream_url}`"))
}

fn write_reverse_response(
    client: &mut TcpStream,
    response: reqwest::blocking::Response,
) -> Result<()> {
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .bytes()
        .context("failed to read reverse proxy response")?;
    write!(
        client,
        "HTTP/1.1 {} {}\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or("OK")
    )?;
    for (name, value) in headers.iter() {
        let name_lower = name.as_str().to_ascii_lowercase();
        if matches!(
            name_lower.as_str(),
            "content-length" | "transfer-encoding" | "connection"
        ) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            write!(client, "{}: {}\r\n", name.as_str(), value)?;
        }
    }
    write!(
        client,
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    client.write_all(&body)?;
    Ok(())
}

fn parse_forward_headers(header: &[u8]) -> Result<Vec<(String, String)>> {
    let text = std::str::from_utf8(header).context("proxy request header is not UTF-8")?;
    Ok(text
        .split("\r\n")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect())
}

fn should_skip_reverse_header(name: &str, injected_header: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let injected = injected_header.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "host" | "proxy-authorization" | "proxy-connection" | "connection"
    ) || name == injected
}

fn read_proxy_request_body(client: &mut TcpStream, header: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(header).context("proxy request header is not UTF-8")?;
    let mut content_length = 0_usize;
    for line in text
        .split("\r\n")
        .skip(1)
        .take_while(|line| !line.is_empty())
    {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            bail!("chunked request bodies are not supported by secret reverse proxy yet");
        }
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value
                .trim()
                .parse()
                .context("invalid reverse proxy content-length")?;
        }
    }
    let mut body = vec![0_u8; content_length];
    if content_length > 0 {
        client.read_exact(&mut body)?;
    }
    Ok(body)
}

fn request_points_at_proxy(request: &ProxyRequest, addr: SocketAddr) -> Result<bool> {
    let (host, port) = split_target_host_port(&request.target)?;
    Ok(port == addr.port() && is_loopback_host(&host))
}

fn is_loopback_host(host: &str) -> bool {
    let normalized = normalize_host(host);
    normalized == "localhost"
        || normalized == "127.0.0.1"
        || normalized == "::1"
        || normalized
            .parse::<IpAddr>()
            .is_ok_and(|addr| addr.is_loopback())
}

fn parse_secret_reverse_path(path: &str) -> Option<(String, String)> {
    let prefix = format!("{SECRET_REVERSE_ROUTE_PREFIX}/");
    let rest = path.strip_prefix(&prefix)?;
    let (name, suffix) = rest.split_once('/').unwrap_or((rest, ""));
    if name.is_empty() {
        return None;
    }
    let suffix = if suffix.is_empty() {
        "/".to_string()
    } else {
        format!("/{suffix}")
    };
    Some((name.to_string(), suffix))
}

fn join_upstream_url(mut upstream: Url, suffix: &str) -> Result<Url> {
    let (suffix, query) = suffix
        .split_once('?')
        .map(|(path, query)| (path, Some(query)))
        .unwrap_or((suffix, None));
    let suffix = suffix.strip_prefix('/').unwrap_or(suffix);
    let base = upstream.path().trim_end_matches('/');
    let joined = if suffix.is_empty() {
        if base.is_empty() { "/" } else { base }.to_string()
    } else if base.is_empty() || base == "/" {
        format!("/{suffix}")
    } else {
        format!("{base}/{suffix}")
    };
    upstream.set_path(&joined);
    upstream.set_query(query);
    Ok(upstream)
}

fn send_http_error(client: &mut TcpStream, status: u16, message: &str) -> io::Result<()> {
    write!(
        client,
        "HTTP/1.1 {status} Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        message.len(),
        message
    )
}

fn audit_secret_reverse(
    sandbox: &ResolvedSandbox,
    reverse: &SecretReverseRequest,
    request: &ProxyRequest,
) {
    let mut event = crate::audit::AuditEvent::new("secrets", "reverse_proxy");
    event.sandbox = Some(sandbox.name.clone());
    event.target = Some(reverse.host.clone());
    event.outcome = "ok".to_string();
    event.fields = serde_json::json!({
        "secret_name": reverse.secret_name,
        "method": request.method,
        "upstream": reverse.upstream_url.as_str(),
    });
    crate::audit::record(event);
}

fn proxy_env_overrides(addr: SocketAddr) -> BTreeMap<String, String> {
    let proxy_url = format!("http://{addr}");
    let mut env: BTreeMap<String, String> = PROXY_ENV_KEYS
        .iter()
        .map(|key| ((*key).to_string(), proxy_url.clone()))
        .collect();
    for key in NO_PROXY_ENV_KEYS {
        env.insert((*key).to_string(), String::new());
    }
    env.insert(MANAGED_PROXY_ENV_KEY.to_string(), "1".to_string());
    env
}

pub fn proxy_env_from_current_environment() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for key in PROXY_ENV_KEYS
        .iter()
        .copied()
        .chain(std::iter::once(MANAGED_PROXY_ENV_KEY))
    {
        if let Ok(value) = std::env::var(key)
            && !value.trim().is_empty()
        {
            env.insert(key.to_string(), value);
        }
    }
    env
}

pub(crate) fn managed_proxy_addr_from_env(
    env: &BTreeMap<String, String>,
) -> Result<Option<SocketAddr>> {
    if !matches!(
        env.get(MANAGED_PROXY_ENV_KEY).map(String::as_str),
        Some("1")
    ) {
        return Ok(None);
    }
    for key in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        let Some(value) = env.get(key).filter(|value| !value.trim().is_empty()) else {
            continue;
        };
        let parsed = Url::parse(value)
            .with_context(|| format!("managed proxy env `{key}` is not a valid proxy URL"))?;
        let host = parsed
            .host_str()
            .with_context(|| format!("managed proxy env `{key}` is missing a host"))?;
        let port = parsed
            .port_or_known_default()
            .with_context(|| format!("managed proxy env `{key}` is missing a port"))?;
        let addr = if host.eq_ignore_ascii_case("localhost") {
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port)
        } else {
            format!("{host}:{port}")
                .parse::<SocketAddr>()
                .with_context(|| format!("managed proxy env `{key}` is not a socket address"))?
        };
        if !addr.ip().is_loopback() {
            bail!("managed proxy env `{key}` must point to a loopback address");
        }
        return Ok(Some(addr));
    }
    bail!("managed proxy mode is missing HTTP_PROXY/ALL_PROXY loopback env");
}

struct ProxyState {
    addr: SocketAddr,
    sandbox: ResolvedSandbox,
    upstream: Option<UpstreamProxy>,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
    session_allowed_hosts: Mutex<HashSet<String>>,
    session_denied_hosts: Mutex<HashSet<String>>,
}

#[derive(Debug, Clone)]
struct EndpointPolicy {
    host: String,
    port: u16,
    addresses: Vec<IpAddr>,
    host_action: PermissionAction,
    address_action: PermissionAction,
    action: PermissionAction,
}

impl EndpointPolicy {
    fn evaluate(sandbox: &ResolvedSandbox, host: &str, target: &str) -> Result<Self> {
        let (_, port) = split_target_host_port(target)?;
        let host_action = sandbox.preset.network.action_for_host(host);
        let skip_address_resolution = matches!(host_action, PermissionAction::Deny);
        let addresses = if skip_address_resolution {
            Vec::new()
        } else {
            resolve_target_addresses(target)?
        };
        let address_action = combine_actions(
            addresses
                .iter()
                .map(|address| sandbox.preset.network.action_for_address(*address)),
        );
        let action = combine_actions([host_action, address_action]);
        Ok(Self {
            host: host.to_string(),
            port,
            addresses,
            host_action,
            address_action,
            action,
        })
    }

    fn cache_key(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    fn session_cache_key(&self) -> String {
        let mut addresses = self
            .addresses
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        addresses.sort();
        format!("{}@{}", self.cache_key(), addresses.join(","))
    }

    fn rule_description(&self) -> String {
        let addresses = if self.addresses.is_empty() {
            "unresolved".to_string()
        } else {
            self.addresses
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "network endpoint `{}` matched sandbox ask rule (host={:?}, addresses=[{}], address_action={:?})",
            self.cache_key(),
            self.host_action,
            addresses,
            self.address_action
        )
    }

    fn ask_rule_hits(&self) -> Vec<RuleHit> {
        let mut hits = Vec::new();
        if self.host_action == PermissionAction::Ask {
            hits.push(RuleHit {
                rule_id: "network.hosts.ask".to_string(),
                description: self.rule_description(),
            });
        }
        if self.address_action == PermissionAction::Ask {
            hits.push(RuleHit {
                rule_id: "network.addresses.ask".to_string(),
                description: self.rule_description(),
            });
        }
        if hits.is_empty() {
            hits.push(RuleHit {
                rule_id: "network.ask".to_string(),
                description: self.rule_description(),
            });
        }
        hits
    }
}

fn persist_endpoint_allow_rules(endpoint: &EndpointPolicy) -> Result<()> {
    if endpoint.host_action == PermissionAction::Ask {
        append_network_host_action_to_current_preset(&endpoint.host, PermissionAction::Allow)
            .with_context(|| {
                format!(
                    "failed to persist network host allow rule for `{}`",
                    endpoint.host
                )
            })?;
    }
    if endpoint.address_action == PermissionAction::Ask {
        for address in &endpoint.addresses {
            append_network_address_action_to_current_preset(
                &address.to_string(),
                PermissionAction::Allow,
            )
            .with_context(|| {
                format!("failed to persist network address allow rule for `{address}`")
            })?;
        }
    }
    Ok(())
}

fn resolve_target_addresses(target: &str) -> Result<Vec<IpAddr>> {
    let addresses = target
        .to_socket_addrs()
        .with_context(|| format!("failed to resolve proxy target `{target}`"))?
        .map(|addr| addr.ip())
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        bail!("proxy target `{target}` resolved to no addresses");
    }
    Ok(addresses)
}

fn combine_actions(actions: impl IntoIterator<Item = PermissionAction>) -> PermissionAction {
    let mut saw_ask = false;
    for action in actions {
        match action {
            PermissionAction::Deny => return PermissionAction::Deny,
            PermissionAction::Ask => saw_ask = true,
            PermissionAction::Allow => {}
        }
    }
    if saw_ask {
        PermissionAction::Ask
    } else {
        PermissionAction::Allow
    }
}

impl ProxyState {
    fn ensure_allowed(&self, host: &str, target: &str) -> Result<EndpointPolicy> {
        let host = normalize_host(host);
        let endpoint = EndpointPolicy::evaluate(&self.sandbox, &host, target)?;
        let cache_key = endpoint.cache_key();
        let session_cache_key = endpoint.session_cache_key();
        if self
            .session_denied_hosts
            .lock()
            .expect("network denied host cache poisoned")
            .contains(&session_cache_key)
        {
            bail!(
                "sandbox `{}` network proxy blocked endpoint `{cache_key}` (session deny)",
                self.sandbox.name
            );
        }

        match endpoint.action {
            PermissionAction::Allow => {
                audit_network_endpoint(&self.sandbox, &endpoint, "allow");
                Ok(endpoint)
            }
            PermissionAction::Deny => {
                audit_network_endpoint(&self.sandbox, &endpoint, "deny");
                bail!(
                    "sandbox `{}` network proxy blocked endpoint `{cache_key}` (deny)",
                    self.sandbox.name
                )
            }
            PermissionAction::Ask => {
                if self
                    .session_allowed_hosts
                    .lock()
                    .expect("network allowed host cache poisoned")
                    .contains(&session_cache_key)
                {
                    audit_network_endpoint(&self.sandbox, &endpoint, "session_allow");
                    return Ok(endpoint);
                }
                self.request_network_approval(&endpoint)?;
                Ok(endpoint)
            }
        }
    }

    fn request_network_approval(&self, endpoint: &EndpointPolicy) -> Result<()> {
        let Some(provider) = self.approval_provider.as_ref() else {
            bail!(
                "sandbox `{}` network proxy blocked endpoint `{}` (ask requires an interactive approval provider)",
                self.sandbox.name,
                endpoint.cache_key()
            );
        };
        let command = format!("network-access {}", endpoint.cache_key());
        let rule_hits = endpoint.ask_rule_hits();
        audit_network_endpoint(&self.sandbox, &endpoint, "ask");
        let response = provider
            .request_approval(&command, &rule_hits, ApprovalDecision::options())
            .unwrap_or(ApprovalResponse {
                decision: ApprovalDecision::Forbidden,
            });
        match response.decision {
            ApprovalDecision::Once => Ok(()),
            ApprovalDecision::Session => {
                self.session_allowed_hosts
                    .lock()
                    .expect("network allowed host cache poisoned")
                    .insert(endpoint.session_cache_key());
                Ok(())
            }
            ApprovalDecision::Always => {
                persist_endpoint_allow_rules(endpoint)?;
                self.session_allowed_hosts
                    .lock()
                    .expect("network allowed host cache poisoned")
                    .insert(endpoint.session_cache_key());
                Ok(())
            }
            ApprovalDecision::Forbidden => {
                self.session_denied_hosts
                    .lock()
                    .expect("network denied host cache poisoned")
                    .insert(endpoint.session_cache_key());
                bail!(
                    "network access to `{}` was rejected by user",
                    endpoint.cache_key()
                )
            }
        }
    }

    fn connect_target_or_proxy(
        &self,
        target: &str,
        endpoint: &EndpointPolicy,
    ) -> Result<TcpStream> {
        if let Some(upstream) = &self.upstream {
            return upstream.connect_tunnel(target);
        }
        connect_tcp_endpoint(endpoint)
    }

    fn connect_http_upstream(&self, target: &str, endpoint: &EndpointPolicy) -> Result<TcpStream> {
        if let Some(upstream) = &self.upstream {
            return upstream.connect_http(target);
        }
        connect_tcp_endpoint(endpoint)
    }

    fn upstream_is_http_proxy(&self) -> bool {
        self.upstream
            .as_ref()
            .is_some_and(|upstream| upstream.kind == UpstreamProxyKind::Http)
    }
}

fn audit_network_endpoint(sandbox: &ResolvedSandbox, endpoint: &EndpointPolicy, outcome: &str) {
    let mut event = crate::audit::AuditEvent::new("network", "proxy_decision");
    event.sandbox = Some(sandbox.name.clone());
    event.target = Some(endpoint.cache_key());
    event.outcome = outcome.to_string();
    event.fields = serde_json::json!({
        "host": endpoint.host,
        "port": endpoint.port,
        "addresses": endpoint.addresses.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "host_action": endpoint.host_action,
        "address_action": endpoint.address_action,
        "decision": endpoint.action,
    });
    crate::audit::record(event);
}

fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

#[derive(Debug)]
struct ProxyRequest {
    kind: ProxyRequestKind,
    method: String,
    host: String,
    target: String,
    absolute_uri: bool,
    url_path: Option<String>,
}

#[derive(Debug)]
enum ProxyRequestKind {
    Connect,
    Http,
}

#[derive(Debug)]
struct Socks5Request {
    host: String,
    target: String,
}

impl Socks5Request {
    fn read_from(client: &mut TcpStream) -> Result<Self> {
        let mut head = [0_u8; 4];
        client.read_exact(&mut head)?;
        if head[0] != 0x05 {
            bail!("unsupported SOCKS request version {}", head[0]);
        }
        if head[1] != 0x01 {
            bail!("unsupported SOCKS command {}", head[1]);
        }
        let host = match head[3] {
            0x01 => {
                let mut addr = [0_u8; 4];
                client.read_exact(&mut addr)?;
                std::net::Ipv4Addr::from(addr).to_string()
            }
            0x03 => {
                let mut len = [0_u8; 1];
                client.read_exact(&mut len)?;
                let mut name = vec![0_u8; len[0] as usize];
                client.read_exact(&mut name)?;
                String::from_utf8(name).context("SOCKS host is not UTF-8")?
            }
            0x04 => {
                let mut addr = [0_u8; 16];
                client.read_exact(&mut addr)?;
                std::net::Ipv6Addr::from(addr).to_string()
            }
            other => bail!("unsupported SOCKS address type {other}"),
        };
        let mut port = [0_u8; 2];
        client.read_exact(&mut port)?;
        let port = u16::from_be_bytes(port);
        Ok(Self {
            target: target_with_port(&host, port),
            host,
        })
    }
}

impl ProxyRequest {
    fn parse(header: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(header).context("proxy request header is not UTF-8")?;
        let mut lines = text.split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| anyhow!("proxy request is missing request line"))?;
        let mut parts = request_line.split_whitespace();
        let method = parts
            .next()
            .ok_or_else(|| anyhow!("proxy request is missing method"))?;
        let uri = parts
            .next()
            .ok_or_else(|| anyhow!("proxy request is missing URI"))?;

        if method.eq_ignore_ascii_case("CONNECT") {
            let host = host_without_port(uri);
            return Ok(Self {
                kind: ProxyRequestKind::Connect,
                method: method.to_string(),
                host,
                target: uri.to_string(),
                absolute_uri: false,
                url_path: None,
            });
        }

        if let Ok(url) = Url::parse(uri) {
            let host = url
                .host_str()
                .ok_or_else(|| anyhow!("proxy absolute URI has no host"))?
                .to_string();
            let port = url.port_or_known_default().unwrap_or(80);
            let path = if let Some(query) = url.query() {
                format!("{}?{query}", url.path())
            } else {
                url.path().to_string()
            };
            return Ok(Self {
                kind: ProxyRequestKind::Http,
                method: method.to_string(),
                host: host.clone(),
                target: format!("{host}:{port}"),
                absolute_uri: true,
                url_path: Some(if path.is_empty() {
                    "/".to_string()
                } else {
                    path
                }),
            });
        }

        let host = parse_host_header(text)?;
        let target = add_default_port(&host, 80);
        let path = if uri.is_empty() {
            "/".to_string()
        } else {
            uri.to_string()
        };
        Ok(Self {
            kind: ProxyRequestKind::Http,
            method: method.to_string(),
            host: host_without_port(&host),
            target,
            absolute_uri: false,
            url_path: Some(path),
        })
    }
}

fn parse_host_header(header: &str) -> Result<String> {
    for line in header.lines() {
        if let Some(value) = line.strip_prefix("Host:") {
            return Ok(value.trim().to_string());
        }
        if let Some(value) = line.strip_prefix("host:") {
            return Ok(value.trim().to_string());
        }
    }
    bail!("proxy request is missing Host header")
}

fn host_without_port(target: &str) -> String {
    if let Some(stripped) = target.strip_prefix('[')
        && let Some((host, _)) = stripped.split_once(']')
    {
        return host.to_string();
    }
    target
        .rsplit_once(':')
        .map(|(host, port)| {
            if port.chars().all(|ch| ch.is_ascii_digit()) {
                host.to_string()
            } else {
                target.to_string()
            }
        })
        .unwrap_or_else(|| target.to_string())
}

fn add_default_port(host: &str, default_port: u16) -> String {
    if host.starts_with('[') || host.rsplit_once(':').is_some() {
        host.to_string()
    } else {
        format!("{host}:{default_port}")
    }
}

fn target_with_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn connect_tcp_target(target: &str) -> Result<TcpStream> {
    let mut addrs = target
        .to_socket_addrs()
        .with_context(|| format!("failed to resolve proxy target `{target}`"))?;
    let addr = addrs
        .next()
        .ok_or_else(|| anyhow!("proxy target `{target}` resolved to no addresses"))?;
    TcpStream::connect_timeout(&addr, Duration::from_secs(15))
        .with_context(|| format!("failed to connect proxy target `{target}`"))
}

fn connect_tcp_endpoint(endpoint: &EndpointPolicy) -> Result<TcpStream> {
    let mut last_error = None;
    for address in &endpoint.addresses {
        let socket = SocketAddr::new(*address, endpoint.port);
        match TcpStream::connect_timeout(&socket, Duration::from_secs(15)) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    let message = last_error
        .map(|error| error.to_string())
        .unwrap_or_else(|| "no resolved addresses".to_string());
    bail!(
        "failed to connect proxy endpoint `{}` using pre-resolved addresses: {message}",
        endpoint.cache_key()
    )
}

#[derive(Debug)]
struct UpstreamProxy {
    target: String,
    kind: UpstreamProxyKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpstreamProxyKind {
    Http,
    Socks5,
}

impl UpstreamProxy {
    fn from_env() -> Option<Self> {
        for key in [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            let Ok(value) = std::env::var(key) else {
                continue;
            };
            let Some((target, kind)) = upstream_proxy_target(&value) else {
                continue;
            };
            return Some(Self { target, kind });
        }
        None
    }

    fn connect_raw(&self) -> Result<TcpStream> {
        connect_tcp_target(&self.target)
    }

    fn connect_tunnel(&self, target: &str) -> Result<TcpStream> {
        if self.kind == UpstreamProxyKind::Socks5 {
            return self.connect_socks5_target(target);
        }
        let mut stream = self.connect_raw()?;
        write!(
            stream,
            "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n"
        )?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut status_line = String::new();
        reader.read_line(&mut status_line)?;
        if !status_line.contains(" 200 ") && !status_line.contains(" 201 ") {
            bail!(
                "upstream proxy refused CONNECT {target}: {}",
                status_line.trim()
            );
        }
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader.read_line(&mut line)?;
            if read == 0 || line == "\r\n" {
                break;
            }
        }
        Ok(stream)
    }

    fn connect_http(&self, target: &str) -> Result<TcpStream> {
        match self.kind {
            UpstreamProxyKind::Http => self.connect_raw(),
            UpstreamProxyKind::Socks5 => self.connect_socks5_target(target),
        }
    }

    fn connect_socks5_target(&self, target: &str) -> Result<TcpStream> {
        let (host, port) = split_target_host_port(target)?;
        let mut stream = self.connect_raw()?;
        stream.write_all(&[0x05, 0x01, 0x00])?;
        let mut greeting = [0_u8; 2];
        stream.read_exact(&mut greeting)?;
        if greeting != [0x05, 0x00] {
            bail!("upstream SOCKS5 proxy refused no-auth negotiation");
        }

        let mut request = vec![0x05, 0x01, 0x00];
        if let Ok(ipv4) = host.parse::<std::net::Ipv4Addr>() {
            request.push(0x01);
            request.extend(ipv4.octets());
        } else if let Ok(ipv6) = host.parse::<std::net::Ipv6Addr>() {
            request.push(0x04);
            request.extend(ipv6.octets());
        } else {
            let host_bytes = host.as_bytes();
            if host_bytes.len() > u8::MAX as usize {
                bail!("upstream SOCKS5 target hostname is too long: {host}");
            }
            request.push(0x03);
            request.push(host_bytes.len() as u8);
            request.extend(host_bytes);
        }
        request.extend(port.to_be_bytes());
        stream.write_all(&request)?;

        let mut head = [0_u8; 4];
        stream.read_exact(&mut head)?;
        if head[0] != 0x05 || head[1] != 0x00 {
            bail!(
                "upstream SOCKS5 proxy refused target `{target}` with status {}",
                head[1]
            );
        }
        skip_socks5_bound_addr(&mut stream, head[3])?;
        Ok(stream)
    }
}

fn upstream_proxy_target(value: &str) -> Option<(String, UpstreamProxyKind)> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed = if trimmed.contains("://") {
        Url::parse(trimmed).ok()?
    } else {
        Url::parse(&format!("http://{trimmed}")).ok()?
    };
    let host = parsed.host_str()?;
    let port = parsed.port_or_known_default().unwrap_or(8080);
    let kind = match parsed.scheme().to_ascii_lowercase().as_str() {
        "socks5" | "socks5h" => UpstreamProxyKind::Socks5,
        _ => UpstreamProxyKind::Http,
    };
    Some((format!("{host}:{port}"), kind))
}

fn split_target_host_port(target: &str) -> Result<(String, u16)> {
    if let Some(stripped) = target.strip_prefix('[') {
        let Some((host, rest)) = stripped.split_once(']') else {
            bail!("invalid IPv6 target `{target}`");
        };
        let Some(port) = rest.strip_prefix(':') else {
            bail!("missing port in target `{target}`");
        };
        return Ok((host.to_string(), port.parse()?));
    }
    if let Some((host, port)) = target.rsplit_once(':') {
        return Ok((host.to_string(), port.parse()?));
    }
    bail!("missing port in target `{target}`")
}

fn skip_socks5_bound_addr(stream: &mut TcpStream, atyp: u8) -> Result<()> {
    match atyp {
        0x01 => {
            let mut buf = [0_u8; 4 + 2];
            stream.read_exact(&mut buf)?;
        }
        0x03 => {
            let mut len = [0_u8; 1];
            stream.read_exact(&mut len)?;
            let mut buf = vec![0_u8; len[0] as usize + 2];
            stream.read_exact(&mut buf)?;
        }
        0x04 => {
            let mut buf = [0_u8; 16 + 2];
            stream.read_exact(&mut buf)?;
        }
        other => bail!("upstream SOCKS5 proxy returned unsupported address type {other}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::config::SandboxConfig;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct FixedApprovalProvider {
        decision: ApprovalDecision,
        calls: AtomicUsize,
    }

    impl FixedApprovalProvider {
        fn new(decision: ApprovalDecision) -> Arc<Self> {
            Arc::new(Self {
                decision,
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(AtomicOrdering::SeqCst)
        }
    }

    impl ApprovalProvider for FixedApprovalProvider {
        fn request_approval(
            &self,
            _command: &str,
            _rule_hits: &[RuleHit],
            _options: [ApprovalDecision; 4],
        ) -> Option<ApprovalResponse> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Some(ApprovalResponse {
                decision: self.decision,
            })
        }
    }

    #[test]
    fn proxy_env_overrides_sets_common_keys() {
        let addr = SocketAddr::from(([127, 0, 0, 1], 43210));
        let env = proxy_env_overrides(addr);
        assert_eq!(env["HTTP_PROXY"], "http://127.0.0.1:43210");
        assert_eq!(env["HTTPS_PROXY"], "http://127.0.0.1:43210");
        assert_eq!(env["ALL_PROXY"], "http://127.0.0.1:43210");
        assert_eq!(env["NPM_CONFIG_HTTPS_PROXY"], "http://127.0.0.1:43210");
        assert_eq!(env["NO_PROXY"], "");
        assert_eq!(env["no_proxy"], "");
        assert_eq!(env["NPM_CONFIG_NO_PROXY"], "");
        assert_eq!(env["YARN_NO_PROXY"], "");
    }

    #[test]
    fn managed_proxy_addr_requires_marker_and_loopback_proxy_url() -> Result<()> {
        let mut env = BTreeMap::new();
        assert!(managed_proxy_addr_from_env(&env)?.is_none());

        env.insert(MANAGED_PROXY_ENV_KEY.to_string(), "1".to_string());
        env.insert(
            "HTTP_PROXY".to_string(),
            "http://127.0.0.1:43210".to_string(),
        );
        assert_eq!(
            managed_proxy_addr_from_env(&env)?,
            Some(SocketAddr::from(([127, 0, 0, 1], 43210)))
        );

        env.insert(
            "HTTP_PROXY".to_string(),
            "http://example.com:3128".to_string(),
        );
        assert!(managed_proxy_addr_from_env(&env).is_err());
        Ok(())
    }

    #[test]
    fn parses_connect_request() -> Result<()> {
        let request = ProxyRequest::parse(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")?;
        assert_eq!(request.host, "example.com");
        assert_eq!(request.target, "example.com:443");
        assert!(matches!(request.kind, ProxyRequestKind::Connect));
        Ok(())
    }

    #[test]
    fn parses_absolute_http_request() -> Result<()> {
        let request = ProxyRequest::parse(
            b"GET http://example.com/path?q=1 HTTP/1.1\r\nHost: example.com\r\n\r\n",
        )?;
        assert_eq!(request.host, "example.com");
        assert_eq!(request.target, "example.com:80");
        assert_eq!(request.url_path.as_deref(), Some("/path?q=1"));
        Ok(())
    }

    #[test]
    fn formats_socks_targets_for_ipv4_hosts_and_ipv6() {
        assert_eq!(target_with_port("example.com", 443), "example.com:443");
        assert_eq!(target_with_port("127.0.0.1", 80), "127.0.0.1:80");
        assert_eq!(target_with_port("::1", 8080), "[::1]:8080");
    }

    #[test]
    fn proxy_policy_asks_for_default_ask_hosts() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        let provider = FixedApprovalProvider::new(ApprovalDecision::Once);
        let state = ProxyState {
            addr: SocketAddr::from(([127, 0, 0, 1], 43210)),
            sandbox,
            upstream: None,
            approval_provider: Some(provider.clone()),
            session_allowed_hosts: Mutex::new(HashSet::new()),
            session_denied_hosts: Mutex::new(HashSet::new()),
        };
        assert!(state.ensure_allowed("example.com", "127.0.0.1:443").is_ok());
        assert_eq!(provider.calls(), 1);
        assert!(state.ensure_allowed("localhost", "127.0.0.1:443").is_ok());
        assert_eq!(provider.calls(), 2);
        Ok(())
    }

    #[test]
    fn proxy_policy_session_approval_is_cached() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        let provider = FixedApprovalProvider::new(ApprovalDecision::Session);
        let state = ProxyState {
            addr: SocketAddr::from(([127, 0, 0, 1], 43210)),
            sandbox,
            upstream: None,
            approval_provider: Some(provider.clone()),
            session_allowed_hosts: Mutex::new(HashSet::new()),
            session_denied_hosts: Mutex::new(HashSet::new()),
        };
        assert!(state.ensure_allowed("example.com", "127.0.0.1:443").is_ok());
        assert!(state.ensure_allowed("example.com", "127.0.0.1:443").is_ok());
        assert_eq!(provider.calls(), 1);
        Ok(())
    }

    #[test]
    fn proxy_policy_forbidden_is_cached_as_session_deny() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        let provider = FixedApprovalProvider::new(ApprovalDecision::Forbidden);
        let state = ProxyState {
            addr: SocketAddr::from(([127, 0, 0, 1], 43210)),
            sandbox,
            upstream: None,
            approval_provider: Some(provider.clone()),
            session_allowed_hosts: Mutex::new(HashSet::new()),
            session_denied_hosts: Mutex::new(HashSet::new()),
        };
        assert!(
            state
                .ensure_allowed("example.com", "127.0.0.1:443")
                .is_err()
        );
        assert!(
            state
                .ensure_allowed("example.com", "127.0.0.1:443")
                .is_err()
        );
        assert_eq!(provider.calls(), 1);
        Ok(())
    }

    #[test]
    fn endpoint_policy_denies_blocked_address_even_when_host_allows() -> Result<()> {
        let mut sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        sandbox
            .preset
            .network
            .hosts
            .insert("metadata.test".to_string(), PermissionAction::Allow);

        let endpoint = EndpointPolicy::evaluate(&sandbox, "metadata.test", "169.254.169.254:443")?;

        assert_eq!(endpoint.host_action, PermissionAction::Allow);
        assert_eq!(endpoint.address_action, PermissionAction::Deny);
        assert_eq!(endpoint.action, PermissionAction::Deny);
        Ok(())
    }

    #[test]
    fn endpoint_policy_address_ask_reports_address_rule_hit() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        let endpoint = EndpointPolicy::evaluate(&sandbox, "localhost", "127.0.0.1:443")?;

        assert_eq!(endpoint.host_action, PermissionAction::Allow);
        assert_eq!(endpoint.address_action, PermissionAction::Ask);
        let rule_ids = endpoint
            .ask_rule_hits()
            .into_iter()
            .map(|hit| hit.rule_id)
            .collect::<Vec<_>>();
        assert_eq!(rule_ids, vec!["network.addresses.ask"]);
        Ok(())
    }

    #[test]
    fn endpoint_policy_skips_dns_when_host_is_already_denied() -> Result<()> {
        let mut sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        sandbox.preset.network.hosts.clear();
        sandbox
            .preset
            .network
            .hosts
            .insert("*".to_string(), PermissionAction::Allow);
        sandbox
            .preset
            .network
            .hosts
            .insert("blocked.invalid".to_string(), PermissionAction::Deny);

        let host_denied =
            EndpointPolicy::evaluate(&sandbox, "blocked.invalid", "blocked.invalid:443")?;
        assert_eq!(host_denied.host_action, PermissionAction::Deny);
        assert_eq!(host_denied.action, PermissionAction::Deny);
        assert!(host_denied.addresses.is_empty());
        Ok(())
    }

    #[test]
    fn session_allow_cache_does_not_bypass_later_address_deny() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        let provider = FixedApprovalProvider::new(ApprovalDecision::Session);
        let state = ProxyState {
            addr: SocketAddr::from(([127, 0, 0, 1], 43210)),
            sandbox,
            upstream: None,
            approval_provider: Some(provider.clone()),
            session_allowed_hosts: Mutex::new(HashSet::new()),
            session_denied_hosts: Mutex::new(HashSet::new()),
        };

        assert!(state.ensure_allowed("example.com", "127.0.0.1:443").is_ok());
        assert_eq!(provider.calls(), 1);
        assert!(
            state
                .ensure_allowed("example.com", "169.254.169.254:443")
                .is_err()
        );
        assert_eq!(provider.calls(), 1);
        Ok(())
    }

    #[test]
    fn env_secret_proxy_overrides_env_and_resolves_reverse_route() -> Result<()> {
        let key = "DUCKAGENT_TEST_SECRET_PROXY_KEY";
        let url = "DUCKAGENT_TEST_SECRET_PROXY_URL";
        unsafe {
            std::env::set_var(key, "real-secret");
            std::env::set_var(url, "https://api.openai.com/base");
        }
        let result = (|| -> Result<()> {
            let value = serde_json::json!({
                "preset": "custom",
                "presets": {
                    "custom": {
                        "filesystem": {"mounts": [{"path": ".", "access": "rw"}], "rules": []},
                        "network": {"mode": "proxy", "hosts": {"*": "ask"}},
                        "env": {
                            "DUCKAGENT_TEST_SECRET_PROXY_KEY": {
                                "type": "secret",
                                "inject": {
                                    "url": "DUCKAGENT_TEST_SECRET_PROXY_URL",
                                    "header": "Authorization",
                                    "format": "Bearer {}"
                                }
                            }
                        }
                    }
                }
            });
            let sandbox = serde_json::from_value::<SandboxConfig>(value)?.resolve(None)?;
            let addr = SocketAddr::from(([127, 0, 0, 1], 43210));
            let env = sandbox.preset.secrets.proxy_env_overrides(addr);
            assert_eq!(
                env.get(key).map(String::as_str),
                Some("duckagent-secret:DUCKAGENT_TEST_SECRET_PROXY_KEY")
            );
            assert_eq!(
                env.get(url).map(String::as_str),
                Some("http://127.0.0.1:43210/__duckagent_secret/DUCKAGENT_TEST_SECRET_PROXY_KEY")
            );
            let state = ProxyState {
                addr,
                sandbox,
                upstream: None,
                approval_provider: None,
                session_allowed_hosts: Mutex::new(HashSet::new()),
                session_denied_hosts: Mutex::new(HashSet::new()),
            };
            let request = ProxyRequest::parse(
                b"POST /__duckagent_secret/DUCKAGENT_TEST_SECRET_PROXY_KEY/v1/responses?q=1 HTTP/1.1\r\nHost: 127.0.0.1:43210\r\n\r\n",
            )?;
            let reverse = SecretReverseRequest::from_request(&state, &request)?
                .expect("request should match secret reverse route");
            assert_eq!(
                reverse.upstream_url.as_str(),
                "https://api.openai.com/base/v1/responses?q=1"
            );
            Ok(())
        })();
        unsafe {
            std::env::remove_var(key);
            std::env::remove_var(url);
        }
        result
    }

    #[test]
    fn preserves_loopback_upstream_proxy_from_parent_environment() {
        assert_eq!(
            upstream_proxy_target("http://127.0.0.1:9999"),
            Some(("127.0.0.1:9999".to_string(), UpstreamProxyKind::Http))
        );
        assert_eq!(
            upstream_proxy_target("http://localhost:9999"),
            Some(("localhost:9999".to_string(), UpstreamProxyKind::Http))
        );
        assert_eq!(
            upstream_proxy_target("http://proxy.example:8080"),
            Some(("proxy.example:8080".to_string(), UpstreamProxyKind::Http))
        );
        assert_eq!(
            upstream_proxy_target("socks5h://127.0.0.1:1080"),
            Some(("127.0.0.1:1080".to_string(), UpstreamProxyKind::Socks5))
        );
    }
}
