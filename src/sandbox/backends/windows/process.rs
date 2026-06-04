use crate::sandbox::backends::windows::users::SandboxCredentials;
use crate::sandbox::backends::windows::winutil::{quote_windows_arg, to_wide_os, to_wide_str};
use anyhow::{Result, anyhow};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Instant;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessWithLogonW, GetExitCodeProcess, INFINITE,
    LOGON_WITH_PROFILE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
    WaitForSingleObject,
};

pub fn run_with_logon(
    credentials: &SandboxCredentials,
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> Result<ExitStatus> {
    let user_w = to_wide_str(&credentials.username);
    let domain_w = to_wide_str(".");
    let password_w = to_wide_str(&credentials.password);
    let mut command_line = to_wide_str(join_command_line(program, args));
    let mut cwd_w = to_wide_os(cwd.as_os_str());
    let mut env_block = make_env_block(env);
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    startup.dwFlags = STARTF_USESTDHANDLES;
    startup.hStdInput = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    startup.hStdOutput = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    startup.hStdError = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let create_start = Instant::now();
    let ok = unsafe {
        CreateProcessWithLogonW(
            user_w.as_ptr(),
            domain_w.as_ptr(),
            password_w.as_ptr(),
            LOGON_WITH_PROFILE,
            std::ptr::null(),
            command_line.as_mut_ptr(),
            CREATE_UNICODE_ENVIRONMENT,
            env_block.as_mut_ptr() as *const c_void,
            cwd_w.as_mut_ptr(),
            &mut startup,
            &mut process_info,
        )
    };
    if ok == 0 {
        return Err(anyhow!(
            "CreateProcessWithLogonW failed for `{}` as `{}` in {}: {}",
            program,
            credentials.username,
            cwd.display(),
            unsafe { GetLastError() }
        ));
    }
    timing_log("CreateProcessWithLogonW create", create_start);

    let wait_start = Instant::now();
    unsafe {
        WaitForSingleObject(process_info.hProcess, INFINITE);
    }
    timing_log("CreateProcessWithLogonW wait child", wait_start);
    let mut exit_code = 1u32;
    let exit_ok = unsafe { GetExitCodeProcess(process_info.hProcess, &mut exit_code) };
    unsafe {
        CloseHandle(process_info.hThread);
        CloseHandle(process_info.hProcess);
    }
    if exit_ok == 0 {
        return Err(anyhow!("GetExitCodeProcess failed: {}", unsafe {
            GetLastError()
        }));
    }
    Ok(ExitStatus::from_raw(exit_code))
}

fn timing_log(label: &str, start: Instant) {
    if !sandbox_timing_enabled() {
        return;
    }
    eprintln!(
        "[duckagent][sandbox][timing] {label}: {} ms",
        start.elapsed().as_millis()
    );
}

fn sandbox_timing_enabled() -> bool {
    std::env::var("DUCKAGENT_SANDBOX_TIMING")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn join_command_line(program: &str, args: &[String]) -> String {
    std::iter::once(program.to_string())
        .chain(args.iter().cloned())
        .map(|arg| quote_windows_arg(&arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn make_env_block(env: &BTreeMap<String, String>) -> Vec<u16> {
    let mut pairs = env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>();
    pairs.sort_by_key(|pair| pair.to_ascii_uppercase());
    let mut block = Vec::new();
    for pair in pairs {
        block.extend(to_wide_str(pair).into_iter());
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    block
}
