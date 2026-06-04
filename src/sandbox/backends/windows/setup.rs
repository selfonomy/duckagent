use crate::sandbox::backends::windows::users;
use crate::sandbox::backends::windows::winutil::{quote_windows_arg, to_wide_os, to_wide_str};
use anyhow::{Context, Result, anyhow};
use std::net::{Ipv4Addr, TcpListener};
use std::path::Path;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
use windows_sys::Win32::Security::{
    AllocateAndInitializeSid, CheckTokenMembership, FreeSid, SECURITY_NT_AUTHORITY,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};

const ERROR_CANCELLED: u32 = 1223;
const SECURITY_BUILTIN_DOMAIN_RID: u32 = 0x0000_0020;
const DOMAIN_ALIAS_RID_ADMINS: u32 = 0x0000_0220;

pub fn run_elevated_setup(
    duckagent_home: &Path,
    proxy_mode: bool,
    proxy_ports: &[u16],
    allow_local_binding: bool,
) -> Result<()> {
    if is_elevated()? {
        return run_setup_helper(duckagent_home, proxy_mode, proxy_ports, allow_local_binding);
    }

    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let mut params = format!(
        "__sandbox-windows-setup-helper --duckagent-home {}",
        quote_windows_arg(&duckagent_home.display().to_string())
    );
    if proxy_mode {
        params.push_str(" --proxy");
    }
    for port in proxy_ports {
        params.push_str(" --proxy-port ");
        params.push_str(&port.to_string());
    }
    if allow_local_binding {
        params.push_str(" --allow-local-binding");
    }
    let exe_w = to_wide_os(exe.as_os_str());
    let params_w = to_wide_str(params);
    let verb_w = to_wide_str("runas");
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb_w.as_ptr();
    info.lpFile = exe_w.as_ptr();
    info.lpParameters = params_w.as_ptr();
    info.nShow = 0;

    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 || info.hProcess == 0 {
        let error = unsafe { GetLastError() };
        if error == ERROR_CANCELLED {
            return Err(anyhow!("Windows sandbox setup was cancelled by the user"));
        }
        return Err(anyhow!(
            "ShellExecuteExW failed to launch elevated Windows sandbox setup helper: {error}"
        ));
    }

    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        if code != 0 {
            return Err(anyhow!(
                "elevated Windows sandbox setup helper exited with code {code}"
            ));
        }
    }
    Ok(())
}

pub fn run_setup_helper(
    duckagent_home: &Path,
    proxy_mode: bool,
    proxy_ports: &[u16],
    allow_local_binding: bool,
) -> Result<()> {
    if !is_elevated()? {
        return Err(anyhow!(
            "Windows sandbox setup helper must run with Administrator permissions"
        ));
    }
    let selected_proxy_ports = select_proxy_ports(proxy_mode, proxy_ports)?;
    users::provision_sandbox_users(duckagent_home, &selected_proxy_ports, allow_local_binding)
}

pub fn setup_matches(
    duckagent_home: &Path,
    proxy_mode: bool,
    proxy_ports: &[u16],
    allow_local_binding: bool,
) -> bool {
    let marker_matches = users::load_marker(duckagent_home)
        .map(|marker| marker.setup_matches(proxy_mode, proxy_ports, allow_local_binding))
        .unwrap_or(false);
    marker_matches && users::load_users(duckagent_home).is_ok()
}

fn select_proxy_ports(proxy_mode: bool, proxy_ports: &[u16]) -> Result<Vec<u16>> {
    let explicit_ports = normalize_proxy_ports(proxy_ports);
    if !proxy_mode && explicit_ports.is_empty() {
        return Ok(Vec::new());
    }
    if !explicit_ports.is_empty() {
        return Ok(explicit_ports);
    }
    Ok(vec![pick_available_loopback_port()?])
}

fn pick_available_loopback_port() -> Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .context("failed to probe an available Windows managed proxy port")?;
    let port = listener
        .local_addr()
        .context("failed to read probed Windows managed proxy port")?
        .port();
    if port == 0 {
        return Err(anyhow!(
            "Windows managed proxy port probe returned invalid port 0"
        ));
    }
    Ok(port)
}

fn normalize_proxy_ports(proxy_ports: &[u16]) -> Vec<u16> {
    let mut ports = proxy_ports
        .iter()
        .copied()
        .filter(|port| *port != 0)
        .collect::<Vec<_>>();
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn is_elevated() -> Result<bool> {
    unsafe {
        let mut administrators_group = std::ptr::null_mut();
        let ok = AllocateAndInitializeSid(
            &SECURITY_NT_AUTHORITY,
            2,
            SECURITY_BUILTIN_DOMAIN_RID,
            DOMAIN_ALIAS_RID_ADMINS,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut administrators_group,
        );
        if ok == 0 {
            return Err(anyhow!(
                "AllocateAndInitializeSid failed: {}",
                GetLastError()
            ));
        }
        let mut is_member = 0i32;
        let checked = CheckTokenMembership(0, administrators_group, &mut is_member);
        FreeSid(administrators_group as *mut _);
        if checked == 0 {
            return Err(anyhow!("CheckTokenMembership failed: {}", GetLastError()));
        }
        Ok(is_member != 0)
    }
}
