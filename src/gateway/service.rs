#[cfg(target_os = "macos")]
use super::default_gateway_logs_dir;
use super::default_gateway_run_dir;
#[cfg(target_os = "windows")]
use super::default_gateway_service_dir;
use anyhow::{Context, Result, bail};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const GATEWAY_SERVICE_LABEL: &str = "com.duckagent.gateway";
#[cfg(target_os = "windows")]
const GATEWAY_TASK_NAME: &str = "DuckAgent Gateway";
const GATEWAY_LOCK_FILE: &str = "gateway.pid";
const SERVICE_ENV_KEYS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

#[derive(Debug, Clone)]
struct ServiceDefinition {
    path: PathBuf,
}

#[derive(Debug)]
pub(crate) struct GatewayInstanceGuard {
    path: PathBuf,
    _file: File,
}

impl GatewayInstanceGuard {
    pub(crate) fn acquire() -> Result<Self> {
        let path = gateway_pid_path()?;
        acquire_pid_guard(path)
    }
}

impl Drop for GatewayInstanceGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(crate) fn install_gateway_service() -> Result<()> {
    install_gateway_service_platform()?;
    Ok(())
}

pub(crate) fn start_gateway_service() -> Result<()> {
    if !gateway_service_installed()? {
        bail!("duckagent gateway service is not installed");
    }
    if let Some(pid) = running_gateway_pid()? {
        bail!("{}", describe_running_gateway_for_service_start(pid));
    }
    start_gateway_service_platform()?;
    Ok(())
}

pub(crate) fn gateway_service_installed() -> Result<bool> {
    service_definition_installed()
}

pub(crate) fn stop_gateway_service() -> Result<()> {
    if !gateway_service_installed()? {
        if let Some(pid) = running_gateway_pid()? {
            bail!("{}", describe_running_gateway_for_stop_without_service(pid));
        } else {
            println!("duckagent gateway service is already stopped.");
            return Ok(());
        }
    }
    stop_gateway_service_platform()?;
    Ok(())
}

pub(crate) fn uninstall_gateway_service() -> Result<()> {
    uninstall_gateway_service_platform()?;
    Ok(())
}

pub(crate) fn running_gateway_pid() -> Result<Option<u32>> {
    let path = gateway_pid_path()?;
    let Some(pid) = running_pid_from_file(&path)? else {
        return Ok(None);
    };
    if pid == std::process::id()
        || matches!(gateway_process_kind(pid), GatewayProcessKind::Other(_))
    {
        let _ = fs::remove_file(&path);
        return Ok(None);
    }
    Ok(Some(pid))
}

pub(crate) fn running_gateway_is_background_service(pid: u32) -> bool {
    gateway_process_kind(pid) == GatewayProcessKind::Service
}

#[cfg(target_os = "windows")]
fn service_definition_installed() -> Result<bool> {
    windows_task_exists()
}

#[cfg(not(target_os = "windows"))]
fn service_definition_installed() -> Result<bool> {
    Ok(service_definition()?.is_some_and(|definition| definition.path.exists()))
}

fn service_definition() -> Result<Option<ServiceDefinition>> {
    service_definition_for_label(GATEWAY_SERVICE_LABEL)
}

#[cfg(target_os = "macos")]
fn service_definition_for_label(label: &str) -> Result<Option<ServiceDefinition>> {
    let home = dirs::home_dir().context("failed to resolve home directory")?;
    Ok(Some(ServiceDefinition {
        path: home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{label}.plist")),
    }))
}

#[cfg(target_os = "linux")]
fn service_definition_for_label(label: &str) -> Result<Option<ServiceDefinition>> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
        .context("failed to resolve XDG_CONFIG_HOME or home directory")?;
    Ok(Some(ServiceDefinition {
        path: config_home
            .join("systemd")
            .join("user")
            .join(format!("{label}.service")),
    }))
}

#[cfg(target_os = "windows")]
fn service_definition_for_label(_label: &str) -> Result<Option<ServiceDefinition>> {
    Ok(Some(ServiceDefinition {
        path: windows_launcher_path()?,
    }))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn service_definition_for_label(_label: &str) -> Result<Option<ServiceDefinition>> {
    Ok(None)
}

#[cfg(target_os = "macos")]
fn install_gateway_service_platform() -> Result<()> {
    write_launchd_plist(true)
}

#[cfg(target_os = "macos")]
fn start_gateway_service_platform() -> Result<()> {
    write_launchd_plist(false)?;
    let definition =
        service_definition()?.context("macOS launchd service definition path is unavailable")?;
    let _ = stop_launchd_service(&definition.path, GATEWAY_SERVICE_LABEL);
    run_launchctl_required(&[
        "bootstrap",
        &launchd_user_domain(),
        path_arg(&definition.path).as_str(),
    ])
    .or_else(|_| run_launchctl_required(&["load", "-w", path_arg(&definition.path).as_str()]))
}

#[cfg(target_os = "macos")]
fn stop_gateway_service_platform() -> Result<()> {
    write_launchd_plist(true)?;
    let definition =
        service_definition()?.context("macOS launchd service definition path is unavailable")?;
    stop_launchd_service(&definition.path, GATEWAY_SERVICE_LABEL)
}

#[cfg(target_os = "macos")]
fn uninstall_gateway_service_platform() -> Result<()> {
    let definition =
        service_definition()?.context("macOS launchd service definition path is unavailable")?;
    let _ = stop_launchd_service(&definition.path, GATEWAY_SERVICE_LABEL);
    let _ = fs::remove_file(&definition.path);
    Ok(())
}

#[cfg(target_os = "macos")]
fn write_launchd_plist(disabled: bool) -> Result<()> {
    let definition =
        service_definition()?.context("macOS launchd service definition path is unavailable")?;
    if let Some(parent) = definition.path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create launchd plist dir: {}", parent.display()))?;
    }
    let logs_dir = default_gateway_logs_dir()?;
    fs::create_dir_all(&logs_dir)
        .with_context(|| format!("failed to create gateway log dir: {}", logs_dir.display()))?;
    let exe = std::env::current_exe().context("failed to resolve current duck executable")?;
    let cwd = std::env::current_dir().context("failed to resolve current working directory")?;
    let environment_block = launchd_environment_block(&captured_service_environment());
    let disabled_block = if disabled {
        "\n  <key>Disabled</key>\n  <true/>"
    } else {
        ""
    };
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>gateway</string>
    <string>__service-run</string>
  </array>
  <key>WorkingDirectory</key>
  <string>{cwd}</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>{environment_block}
  <key>StandardOutPath</key>
  <string>{stdout}</string>
  <key>StandardErrorPath</key>
  <string>{stderr}</string>{disabled_block}
</dict>
</plist>
"#,
        label = xml_escape(GATEWAY_SERVICE_LABEL),
        exe = xml_escape(&exe.to_string_lossy()),
        cwd = xml_escape(&cwd.to_string_lossy()),
        environment_block = environment_block,
        stdout = xml_escape(&logs_dir.join("service.stdout.log").to_string_lossy()),
        stderr = xml_escape(&logs_dir.join("service.stderr.log").to_string_lossy()),
    );
    fs::write(&definition.path, plist).with_context(|| {
        format!(
            "failed to write launchd plist: {}",
            definition.path.display()
        )
    })
}

#[cfg(target_os = "linux")]
fn install_gateway_service_platform() -> Result<()> {
    write_systemd_unit()?;
    run_systemctl_required(&["daemon-reload"])?;
    run_systemctl_required(&["enable", "com.duckagent.gateway.service"])?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn start_gateway_service_platform() -> Result<()> {
    run_systemctl_required(&["start", "com.duckagent.gateway.service"])
}

#[cfg(target_os = "linux")]
fn stop_gateway_service_platform() -> Result<()> {
    run_systemctl_required(&["stop", "com.duckagent.gateway.service"])
}

#[cfg(target_os = "linux")]
fn uninstall_gateway_service_platform() -> Result<()> {
    let _ = run_systemctl(&["stop", "com.duckagent.gateway.service"]);
    let _ = run_systemctl(&["disable", "com.duckagent.gateway.service"]);
    if let Some(definition) = service_definition()? {
        let _ = fs::remove_file(definition.path);
    }
    let _ = run_systemctl(&["daemon-reload"]);
    Ok(())
}

#[cfg(target_os = "linux")]
fn write_systemd_unit() -> Result<()> {
    let definition = service_definition()?
        .context("Linux systemd user service definition path is unavailable")?;
    if let Some(parent) = definition.path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create systemd user dir: {}", parent.display()))?;
    }
    let exe = std::env::current_exe().context("failed to resolve current duck executable")?;
    let cwd = std::env::current_dir().context("failed to resolve current working directory")?;
    let environment_lines = systemd_environment_lines(&captured_service_environment());
    let unit = format!(
        "[Unit]\nDescription=DuckAgent Gateway\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={} gateway __service-run\nRestart=on-failure\nRestartSec=5\nWorkingDirectory={}\n{}\n[Install]\nWantedBy=default.target\n",
        systemd_escape(&exe.to_string_lossy()),
        systemd_escape(&cwd.to_string_lossy()),
        environment_lines
    );
    fs::write(&definition.path, unit).with_context(|| {
        format!(
            "failed to write systemd user unit: {}",
            definition.path.display()
        )
    })
}

#[cfg(target_os = "macos")]
fn launchd_user_domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

#[cfg(target_os = "macos")]
fn stop_launchd_service(path: &Path, label: &str) -> Result<()> {
    let domain_target = format!("{}/{label}", launchd_user_domain());
    let bootout = run_launchctl(&["bootout", domain_target.as_str()]);
    if matches!(bootout, Ok(true)) {
        return Ok(());
    }

    let path_arg = path.to_string_lossy();
    let unload = run_launchctl(&["unload", path_arg.as_ref()]);
    if matches!(unload, Ok(true)) {
        return Ok(());
    }

    let remove = run_launchctl(&["remove", label]);
    if matches!(remove, Ok(true)) {
        return Ok(());
    }

    match (bootout, unload, remove) {
        (Err(error), _, _) => Err(error).context("failed to run launchctl bootout"),
        (_, Err(error), _) => Err(error).context("failed to run launchctl unload"),
        (_, _, Err(error)) => Err(error).context("failed to run launchctl remove"),
        (Ok(false), Ok(false), Ok(false)) => {
            bail!("launchctl could not stop duckagent gateway service")
        }
        _ => Ok(()),
    }
}

#[cfg(target_os = "macos")]
fn run_launchctl(args: &[&str]) -> Result<bool> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .with_context(|| format!("failed to execute launchctl {}", args.join(" ")))?;
    if output.status.success() {
        return Ok(true);
    }
    Ok(false)
}

#[cfg(target_os = "macos")]
fn run_launchctl_required(args: &[&str]) -> Result<()> {
    run_command_required("launchctl", args)
}

#[cfg(target_os = "linux")]
fn run_systemctl(args: &[&str]) -> Result<bool> {
    let mut full_args = vec!["--user"];
    full_args.extend_from_slice(args);
    run_command("systemctl", &full_args)
}

#[cfg(target_os = "linux")]
fn run_systemctl_required(args: &[&str]) -> Result<()> {
    let mut full_args = vec!["--user"];
    full_args.extend_from_slice(args);
    run_command_required("systemctl", &full_args)
}

#[cfg(target_os = "windows")]
fn install_gateway_service_platform() -> Result<()> {
    write_windows_launcher()?;
    let launcher = windows_launcher_path()?;
    let task = quote_windows_arg(&launcher.to_string_lossy());
    run_schtasks_required(&[
        "/Create",
        "/F",
        "/SC",
        "ONLOGON",
        "/RL",
        "LIMITED",
        "/TN",
        GATEWAY_TASK_NAME,
        "/TR",
        task.as_str(),
    ])
}

#[cfg(target_os = "windows")]
fn start_gateway_service_platform() -> Result<()> {
    run_schtasks_required(&["/Run", "/TN", GATEWAY_TASK_NAME])
}

#[cfg(target_os = "windows")]
fn stop_gateway_service_platform() -> Result<()> {
    let _ = run_schtasks(&["/End", "/TN", GATEWAY_TASK_NAME]);
    Ok(())
}

#[cfg(target_os = "windows")]
fn uninstall_gateway_service_platform() -> Result<()> {
    let _ = run_schtasks(&["/End", "/TN", GATEWAY_TASK_NAME]);
    let _ = run_schtasks(&["/Delete", "/F", "/TN", GATEWAY_TASK_NAME]);
    let _ = fs::remove_file(windows_launcher_path()?);
    Ok(())
}

#[cfg(target_os = "windows")]
fn windows_task_exists() -> Result<bool> {
    run_schtasks(&["/Query", "/TN", GATEWAY_TASK_NAME])
}

#[cfg(target_os = "windows")]
fn write_windows_launcher() -> Result<()> {
    let launcher = windows_launcher_path()?;
    if let Some(parent) = launcher.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create gateway service dir: {}", parent.display())
        })?;
    }
    let exe = std::env::current_exe().context("failed to resolve current duck executable")?;
    let cwd = std::env::current_dir().context("failed to resolve current working directory")?;
    let environment_lines = windows_environment_lines(&captured_service_environment());
    let content = format!(
        "@echo off\r\n{}cd /d {}\r\n{} gateway __service-run\r\n",
        environment_lines,
        quote_windows_arg(&cwd.to_string_lossy()),
        quote_windows_arg(&exe.to_string_lossy())
    );
    fs::write(&launcher, content)
        .with_context(|| format!("failed to write gateway launcher: {}", launcher.display()))
}

#[cfg(target_os = "windows")]
fn windows_launcher_path() -> Result<PathBuf> {
    Ok(default_gateway_service_dir()?.join("duckagent-gateway.cmd"))
}

#[cfg(target_os = "windows")]
fn run_schtasks(args: &[&str]) -> Result<bool> {
    run_command("schtasks", args)
}

#[cfg(target_os = "windows")]
fn run_schtasks_required(args: &[&str]) -> Result<()> {
    run_command_required("schtasks", args)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn install_gateway_service_platform() -> Result<()> {
    bail!("gateway user service is not supported on this platform")
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn start_gateway_service_platform() -> Result<()> {
    bail!("gateway user service is not supported on this platform")
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn stop_gateway_service_platform() -> Result<()> {
    bail!("gateway user service is not supported on this platform")
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn uninstall_gateway_service_platform() -> Result<()> {
    bail!("gateway user service is not supported on this platform")
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn run_command(command: &str, args: &[&str]) -> Result<bool> {
    let output = Command::new(command)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute {command} {}", args.join(" ")))?;
    if output.status.success() {
        return Ok(true);
    }
    Ok(false)
}

fn run_command_required(command: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(command)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute {command} {}", args.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    bail!(
        "{command} {} failed: {}{}",
        args.join(" "),
        stderr.trim(),
        stdout.trim()
    )
}

fn captured_service_environment() -> Vec<(String, String)> {
    SERVICE_ENV_KEYS
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|value| ((*key).to_string(), value))
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn launchd_environment_block(environment: &[(String, String)]) -> String {
    if environment.is_empty() {
        return String::new();
    }
    let mut block = String::from("\n  <key>EnvironmentVariables</key>\n  <dict>");
    for (key, value) in environment {
        block.push_str(&format!(
            "\n    <key>{}</key>\n    <string>{}</string>",
            xml_escape(key),
            xml_escape(value)
        ));
    }
    block.push_str("\n  </dict>");
    block
}

#[cfg(target_os = "linux")]
fn systemd_environment_lines(environment: &[(String, String)]) -> String {
    environment
        .iter()
        .map(|(key, value)| {
            format!(
                "Environment=\"{}={}\"\n",
                systemd_env_escape(key),
                systemd_env_escape(value)
            )
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn systemd_env_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
}

#[cfg(target_os = "windows")]
fn windows_environment_lines(environment: &[(String, String)]) -> String {
    environment
        .iter()
        .map(|(key, value)| {
            format!(
                "set \"{}={}\"\r\n",
                windows_set_escape(key),
                windows_set_escape(value)
            )
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn windows_set_escape(value: &str) -> String {
    value
        .replace('^', "^^")
        .replace('%', "%%")
        .replace('&', "^&")
        .replace('|', "^|")
        .replace('<', "^<")
        .replace('>', "^>")
        .replace('"', "^\"")
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(target_os = "linux")]
fn systemd_escape(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(target_os = "windows")]
fn quote_windows_arg(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn gateway_pid_path() -> Result<PathBuf> {
    Ok(default_gateway_run_dir()?.join(GATEWAY_LOCK_FILE))
}

fn acquire_pid_guard(path: PathBuf) -> Result<GatewayInstanceGuard> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create gateway run dir: {}", parent.display()))?;
    }
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())
                    .with_context(|| format!("failed to write gateway pid: {}", path.display()))?;
                return Ok(GatewayInstanceGuard { path, _file: file });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Some(pid) = running_pid_from_file(&path)? {
                    bail!("{}", describe_running_gateway_for_foreground_start(pid));
                }
                let _ = fs::remove_file(&path);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create gateway pid: {}", path.display()));
            }
        }
    }
}

fn describe_running_gateway_for_foreground_start(pid: u32) -> String {
    match gateway_process_kind(pid) {
        GatewayProcessKind::Service => format!(
            "gateway background service is already running as PID {pid}; run `duck gateway service stop` before starting another gateway"
        ),
        GatewayProcessKind::Foreground => format!(
            "gateway foreground gateway is already running as PID {pid}; stop that terminal before starting another gateway"
        ),
        GatewayProcessKind::Other(command) => format!(
            "gateway is already running as PID {pid} ({command}); stop it before starting another gateway"
        ),
        GatewayProcessKind::Unknown => {
            format!(
                "gateway is already running as PID {pid}; stop it before starting another gateway"
            )
        }
    }
}

pub(crate) fn describe_running_gateway_for_service_start(pid: u32) -> String {
    match gateway_process_kind(pid) {
        GatewayProcessKind::Service => format!(
            "gateway background service is already running as PID {pid}; it will be restarted by `duck gateway service start`"
        ),
        GatewayProcessKind::Foreground => format!(
            "gateway foreground gateway is already running as PID {pid}; stop that terminal before starting the service"
        ),
        GatewayProcessKind::Other(command) => format!(
            "gateway is already running as PID {pid} ({command}); stop it before starting the service"
        ),
        GatewayProcessKind::Unknown => {
            format!("gateway is already running as PID {pid}; stop it before starting the service")
        }
    }
}

fn describe_running_gateway_for_stop_without_service(pid: u32) -> String {
    match gateway_process_kind(pid) {
        GatewayProcessKind::Service => format!(
            "gateway background service is running as PID {pid}, but its service definition is missing; stop the process manually"
        ),
        GatewayProcessKind::Foreground => format!(
            "gateway foreground gateway is running as PID {pid}, but the background service is not installed; stop that terminal manually"
        ),
        GatewayProcessKind::Other(command) => format!(
            "gateway is running as PID {pid} ({command}), but the background service is not installed; stop it manually"
        ),
        GatewayProcessKind::Unknown => format!(
            "gateway is running as PID {pid}, but the background service is not installed; stop it manually"
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GatewayProcessKind {
    Service,
    Foreground,
    Other(String),
    Unknown,
}

fn gateway_process_kind(pid: u32) -> GatewayProcessKind {
    let Some(command) = gateway_process_command(pid) else {
        return GatewayProcessKind::Unknown;
    };
    let normalized = command.replace('\\', "/");
    if normalized.contains("duckagent") && normalized.contains("gateway __service-run") {
        GatewayProcessKind::Service
    } else if normalized.contains("duckagent") && normalized.contains("gateway") {
        GatewayProcessKind::Foreground
    } else if normalized.contains("duckagent") {
        GatewayProcessKind::Other(command)
    } else {
        GatewayProcessKind::Unknown
    }
}

#[cfg(unix)]
fn gateway_process_command(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let path = PathBuf::from(format!("/proc/{pid}/cmdline"));
        if let Ok(bytes) = fs::read(path) {
            let command = bytes
                .split(|byte| *byte == 0)
                .filter_map(|part| std::str::from_utf8(part).ok())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if !command.trim().is_empty() {
                return Some(command);
            }
        }
    }
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!command.is_empty()).then_some(command)
}

#[cfg(windows)]
fn gateway_process_command(pid: u32) -> Option<String> {
    let filter = format!("ProcessId={pid}");
    let output = Command::new("wmic")
        .args(["process", "where", &filter, "get", "CommandLine", "/value"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| line.strip_prefix("CommandLine="))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(not(any(unix, windows)))]
fn gateway_process_command(_pid: u32) -> Option<String> {
    None
}

fn running_pid_from_file(path: &Path) -> Result<Option<u32>> {
    let mut text = String::new();
    match File::open(path) {
        Ok(mut file) => {
            file.read_to_string(&mut text)
                .with_context(|| format!("failed to read gateway pid: {}", path.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to open gateway pid: {}", path.display()));
        }
    }
    let Some(pid) = text.trim().parse::<u32>().ok().filter(|pid| *pid != 0) else {
        let _ = fs::remove_file(path);
        return Ok(None);
    };
    if pid_is_alive(pid) {
        Ok(Some(pid))
    } else {
        let _ = fs::remove_file(path);
        Ok(None)
    }
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn pid_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle == 0 {
        return false;
    }
    unsafe {
        CloseHandle(handle);
    }
    true
}

#[cfg(not(any(unix, windows)))]
fn pid_is_alive(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn pid_guard_blocks_second_instance_and_cleans_up() -> Result<()> {
        let dir = TempDir::new()?;
        let path = dir.path().join("gateway.pid");
        let guard = acquire_pid_guard(path.clone())?;
        let error = acquire_pid_guard(path.clone()).unwrap_err();
        assert!(error.to_string().contains("gateway is already running"));
        drop(guard);
        let _guard = acquire_pid_guard(path)?;
        Ok(())
    }

    #[test]
    fn stale_pid_file_is_replaced() -> Result<()> {
        let dir = TempDir::new()?;
        let path = dir.path().join("gateway.pid");
        fs::write(&path, "999999999")?;
        let _guard = acquire_pid_guard(path.clone())?;
        let text = fs::read_to_string(path)?;
        assert_eq!(text.trim(), std::process::id().to_string());
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_service_definition_uses_launch_agents() -> Result<()> {
        let definition = service_definition_for_label("com.example.agent")?.unwrap();
        assert!(
            definition
                .path
                .ends_with("Library/LaunchAgents/com.example.agent.plist")
        );
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_launchd_environment_block_writes_proxy_vars() {
        let block = launchd_environment_block(&[(
            "https_proxy".to_string(),
            "http://127.0.0.1:7890?a=1&b=2".to_string(),
        )]);
        assert!(block.contains("<key>EnvironmentVariables</key>"));
        assert!(block.contains("<key>https_proxy</key>"));
        assert!(block.contains("http://127.0.0.1:7890?a=1&amp;b=2"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_service_definition_uses_systemd_user_dir() -> Result<()> {
        let definition = service_definition_for_label("com.example.agent")?.unwrap();
        assert!(
            definition
                .path
                .ends_with("systemd/user/com.example.agent.service")
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_systemd_escape_quotes_exec_paths() {
        assert_eq!(
            systemd_escape(r#"/tmp/path with spaces/"quoted"/bin"#),
            r#""/tmp/path with spaces/\"quoted\"/bin""#
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_systemd_environment_lines_write_proxy_vars() {
        let lines = systemd_environment_lines(&[(
            "https_proxy".to_string(),
            r#"http://127.0.0.1:7890/"x"%done"#.to_string(),
        )]);
        assert!(lines.contains(r#"Environment="https_proxy=http://127.0.0.1:7890/\"x\"%%done""#));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_service_definition_uses_scheduled_task_launcher() -> Result<()> {
        let definition = service_definition_for_label("com.example.agent")?.unwrap();
        assert!(definition.path.ends_with("duckagent-gateway.cmd"));
        Ok(())
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_quote_cmd_argument_doubles_quotes() {
        assert_eq!(
            quote_windows_arg(r#"C:\Mark Agent\gateway "dev".cmd"#),
            r#""C:\Mark Agent\gateway ""dev"".cmd""#
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_launcher_environment_lines_write_proxy_vars() {
        let lines = windows_environment_lines(&[(
            "https_proxy".to_string(),
            r#"http://127.0.0.1:7890/a&b"#.to_string(),
        )]);
        assert!(lines.contains(r#"set "https_proxy=http://127.0.0.1:7890/a^&b""#));
    }
}
