use crate::sandbox::backends::windows::dpapi;
use crate::sandbox::backends::windows::winutil::to_wide_str;
use crate::sandbox::backends::windows::{firewall, wfp};
use crate::sandbox::config::NetworkMode;
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use windows_sys::Win32::NetworkManagement::NetManagement::{
    LOCALGROUP_INFO_1, LOCALGROUP_MEMBERS_INFO_3, NERR_Success, NetLocalGroupAdd,
    NetLocalGroupAddMembers, NetUserAdd, NetUserSetInfo, UF_DONT_EXPIRE_PASSWD, UF_SCRIPT,
    USER_INFO_1, USER_INFO_1003, USER_PRIV_USER,
};

pub const OFFLINE_USERNAME: &str = "DuckAgentSandboxOff";
pub const ONLINE_USERNAME: &str = "DuckAgentSandboxOn";
pub const SANDBOX_USERS_GROUP: &str = "DuckAgentSandboxUsers";

const SANDBOX_USERS_GROUP_COMMENT: &str = "DuckAgent sandbox internal group (managed)";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxUserRecord {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxUsersFile {
    pub version: u32,
    pub offline: SandboxUserRecord,
    pub online: SandboxUserRecord,
}

impl SandboxUsersFile {
    pub fn version_matches(&self) -> bool {
        self.version == crate::sandbox::windows_setup::SETUP_VERSION
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCredentials {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WindowsSandboxSetupMarker {
    pub version: u32,
    pub backend: String,
    pub offline_username: String,
    pub online_username: String,
    pub proxy_ports: Vec<u16>,
    pub allow_local_binding: bool,
    pub created_at: String,
}

impl WindowsSandboxSetupMarker {
    pub fn is_current(&self) -> bool {
        self.version == crate::sandbox::windows_setup::SETUP_VERSION
            && self.backend == crate::sandbox::windows_setup::SETUP_BACKEND
            && self.offline_username == OFFLINE_USERNAME
            && self.online_username == ONLINE_USERNAME
    }

    pub fn setup_matches(
        &self,
        proxy_mode: bool,
        proxy_ports: &[u16],
        allow_local_binding: bool,
    ) -> bool {
        if !self.is_current() || self.allow_local_binding != allow_local_binding {
            return false;
        }
        let expected_ports = normalized_ports(proxy_ports);
        if !expected_ports.is_empty() {
            return self.proxy_ports == expected_ports;
        }
        if proxy_mode {
            self.proxy_ports.len() == 1 && self.proxy_ports[0] != 0
        } else {
            self.proxy_ports.is_empty()
        }
    }
}

pub fn provision_sandbox_users(
    duckagent_home: &Path,
    proxy_ports: &[u16],
    allow_local_binding: bool,
) -> Result<()> {
    let mut setup_log = Vec::new();
    setup_stage(&mut setup_log, "starting Windows sandbox user provisioning")?;
    ensure_local_group(SANDBOX_USERS_GROUP, SANDBOX_USERS_GROUP_COMMENT)?;
    setup_stage(&mut setup_log, "local sandbox group is ready")?;
    let offline_password = random_password();
    let online_password = random_password();
    ensure_sandbox_user(OFFLINE_USERNAME, &offline_password)?;
    setup_stage(&mut setup_log, "offline sandbox user is ready")?;
    ensure_sandbox_user(ONLINE_USERNAME, &online_password)?;
    setup_stage(&mut setup_log, "online sandbox user is ready")?;
    write_users_file(duckagent_home, &offline_password, &online_password)?;
    setup_stage(&mut setup_log, "sandbox credential file is ready")?;
    let offline_sid = crate::sandbox::backends::windows::sid::account_sid_string(OFFLINE_USERNAME)?;
    setup_stage(&mut setup_log, "configuring Windows Firewall rules")?;
    firewall::ensure_offline_network_rules(
        &offline_sid,
        proxy_ports,
        allow_local_binding,
        &mut setup_log,
    )?;
    setup_stage(&mut setup_log, "Windows Firewall rules are ready")?;
    setup_stage(&mut setup_log, "configuring WFP defense-in-depth filters")?;
    match wfp::install_wfp_filters_for_account(OFFLINE_USERNAME, proxy_ports) {
        Ok(count) => {
            use std::io::Write;
            let _ = writeln!(
                setup_log,
                "[{}] WFP setup succeeded for {} with {} installed filters",
                Utc::now().to_rfc3339(),
                OFFLINE_USERNAME,
                count
            );
            eprintln!(
                "[duckagent] WFP setup succeeded for {OFFLINE_USERNAME} with {count} installed filters"
            );
        }
        Err(error) => {
            use std::io::Write;
            let _ = writeln!(
                setup_log,
                "[{}] WFP setup failed for {}: {}; failing closed",
                Utc::now().to_rfc3339(),
                OFFLINE_USERNAME,
                error
            );
            eprintln!(
                "[duckagent] WFP setup failed for {OFFLINE_USERNAME}: {error}; failing closed"
            );
            return Err(error).context("failed to install required Windows sandbox WFP filters");
        }
    }
    setup_stage(&mut setup_log, "writing Windows sandbox setup marker")?;
    write_marker_file(duckagent_home, proxy_ports, allow_local_binding, &setup_log)
}

fn setup_stage(log: &mut Vec<u8>, message: &str) -> Result<()> {
    use std::io::Write;

    eprintln!("[duckagent] {message}");
    writeln!(log, "[{}] {message}", Utc::now().to_rfc3339())
        .context("failed to write Windows sandbox setup progress log")
}

pub fn load_users(duckagent_home: &Path) -> Result<SandboxUsersFile> {
    let path = users_path(duckagent_home);
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "failed to read Windows sandbox users file: {}",
            path.display()
        )
    })?;
    let users: SandboxUsersFile = serde_json::from_str(&text).with_context(|| {
        format!(
            "failed to parse Windows sandbox users file: {}",
            path.display()
        )
    })?;
    if !users.version_matches() {
        return Err(anyhow!(
            "Windows sandbox users file version {} does not match setup version {}",
            users.version,
            crate::sandbox::windows_setup::SETUP_VERSION
        ));
    }
    Ok(users)
}

pub fn load_marker(duckagent_home: &Path) -> Result<WindowsSandboxSetupMarker> {
    let path = marker_path(duckagent_home);
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "failed to read Windows sandbox setup marker: {}",
            path.display()
        )
    })?;
    serde_json::from_str(&text).with_context(|| {
        format!(
            "failed to parse Windows sandbox setup marker: {}",
            path.display()
        )
    })
}

pub fn credentials_for_network_mode(
    duckagent_home: &Path,
    mode: NetworkMode,
) -> Result<SandboxCredentials> {
    let users = load_users(duckagent_home)?;
    let record = match mode {
        NetworkMode::Allow => users.online,
        NetworkMode::Deny | NetworkMode::Proxy => users.offline,
    };
    decode_credentials(record)
}

pub fn sandbox_group_sid_string() -> Result<String> {
    crate::sandbox::backends::windows::sid::account_sid_string(SANDBOX_USERS_GROUP)
}

pub fn sandbox_user_sid_strings() -> Result<Vec<String>> {
    Ok(vec![
        crate::sandbox::backends::windows::sid::account_sid_string(OFFLINE_USERNAME)?,
        crate::sandbox::backends::windows::sid::account_sid_string(ONLINE_USERNAME)?,
    ])
}

pub fn users_path(duckagent_home: &Path) -> PathBuf {
    sandbox_dir(duckagent_home).join("sandbox_users.json")
}

pub fn marker_path(duckagent_home: &Path) -> PathBuf {
    sandbox_dir(duckagent_home).join("setup_marker.json")
}

pub fn sandbox_dir(duckagent_home: &Path) -> PathBuf {
    duckagent_home.join("sandbox").join("windows")
}

fn decode_credentials(record: SandboxUserRecord) -> Result<SandboxCredentials> {
    let blob = BASE64
        .decode(record.password.as_bytes())
        .context("failed to base64-decode Windows sandbox password blob")?;
    let password = String::from_utf8(dpapi::unprotect(&blob)?)
        .context("Windows sandbox password blob is not UTF-8")?;
    Ok(SandboxCredentials {
        username: record.username,
        password,
    })
}

fn ensure_sandbox_dir(duckagent_home: &Path) -> Result<PathBuf> {
    let dir = sandbox_dir(duckagent_home);
    std::fs::create_dir_all(&dir).with_context(|| {
        format!(
            "failed to create Windows sandbox directory: {}",
            dir.display()
        )
    })?;
    Ok(dir)
}

fn write_users_file(
    duckagent_home: &Path,
    offline_password: &str,
    online_password: &str,
) -> Result<()> {
    ensure_sandbox_dir(duckagent_home)?;
    let offline_blob = dpapi::protect(offline_password.as_bytes())
        .context("failed to DPAPI-protect offline sandbox password")?;
    let online_blob = dpapi::protect(online_password.as_bytes())
        .context("failed to DPAPI-protect online sandbox password")?;
    let users = SandboxUsersFile {
        version: crate::sandbox::windows_setup::SETUP_VERSION,
        offline: SandboxUserRecord {
            username: OFFLINE_USERNAME.to_string(),
            password: BASE64.encode(offline_blob),
        },
        online: SandboxUserRecord {
            username: ONLINE_USERNAME.to_string(),
            password: BASE64.encode(online_blob),
        },
    };

    let users_path = users_path(duckagent_home);
    std::fs::write(&users_path, serde_json::to_vec_pretty(&users)?).with_context(|| {
        format!(
            "failed to write Windows sandbox users file: {}",
            users_path.display()
        )
    })?;
    Ok(())
}

fn write_marker_file(
    duckagent_home: &Path,
    proxy_ports: &[u16],
    allow_local_binding: bool,
    setup_log: &[u8],
) -> Result<()> {
    ensure_sandbox_dir(duckagent_home)?;
    let marker = WindowsSandboxSetupMarker {
        version: crate::sandbox::windows_setup::SETUP_VERSION,
        backend: crate::sandbox::windows_setup::SETUP_BACKEND.to_string(),
        offline_username: OFFLINE_USERNAME.to_string(),
        online_username: ONLINE_USERNAME.to_string(),
        proxy_ports: normalized_ports(proxy_ports),
        allow_local_binding,
        created_at: Utc::now().to_rfc3339(),
    };

    let marker_path = marker_path(duckagent_home);
    let log_path = sandbox_dir(duckagent_home).join("setup.log");
    std::fs::write(&marker_path, serde_json::to_vec_pretty(&marker)?).with_context(|| {
        format!(
            "failed to write Windows sandbox setup marker: {}",
            marker_path.display()
        )
    })?;
    std::fs::write(&log_path, setup_log).with_context(|| {
        format!(
            "failed to write Windows sandbox setup log: {}",
            log_path.display()
        )
    })?;
    Ok(())
}

fn normalized_ports(proxy_ports: &[u16]) -> Vec<u16> {
    let mut ports = proxy_ports
        .iter()
        .copied()
        .filter(|port| *port != 0)
        .collect::<Vec<_>>();
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn ensure_sandbox_user(username: &str, password: &str) -> Result<()> {
    ensure_local_user(username, password)?;
    ensure_local_group_member(SANDBOX_USERS_GROUP, username)
}

fn ensure_local_user(name: &str, password: &str) -> Result<()> {
    let name_w = to_wide_str(name);
    let password_w = to_wide_str(password);
    unsafe {
        let info = USER_INFO_1 {
            usri1_name: name_w.as_ptr() as *mut u16,
            usri1_password: password_w.as_ptr() as *mut u16,
            usri1_password_age: 0,
            usri1_priv: USER_PRIV_USER,
            usri1_home_dir: std::ptr::null_mut(),
            usri1_comment: std::ptr::null_mut(),
            usri1_flags: UF_SCRIPT | UF_DONT_EXPIRE_PASSWD,
            usri1_script_path: std::ptr::null_mut(),
        };
        let status = NetUserAdd(
            std::ptr::null(),
            1,
            &info as *const _ as *mut u8,
            std::ptr::null_mut(),
        );
        if status == NERR_Success {
            return Ok(());
        }

        let password_info = USER_INFO_1003 {
            usri1003_password: password_w.as_ptr() as *mut u16,
        };
        let update = NetUserSetInfo(
            std::ptr::null(),
            name_w.as_ptr(),
            1003,
            &password_info as *const _ as *mut u8,
            std::ptr::null_mut(),
        );
        if update != NERR_Success {
            return Err(anyhow!(
                "failed to create or update Windows sandbox user `{name}`: NetUserAdd={status}, NetUserSetInfo={update}"
            ));
        }
        Ok(())
    }
}

fn ensure_local_group(name: &str, comment: &str) -> Result<()> {
    const ERROR_ALIAS_EXISTS: u32 = 1379;
    const NERR_GROUP_EXISTS: u32 = 2223;

    let name_w = to_wide_str(name);
    let comment_w = to_wide_str(comment);
    unsafe {
        let info = LOCALGROUP_INFO_1 {
            lgrpi1_name: name_w.as_ptr() as *mut u16,
            lgrpi1_comment: comment_w.as_ptr() as *mut u16,
        };
        let mut parm_err = 0u32;
        let status = NetLocalGroupAdd(
            std::ptr::null(),
            1,
            &info as *const _ as *mut u8,
            &mut parm_err,
        );
        if status != NERR_Success && status != ERROR_ALIAS_EXISTS && status != NERR_GROUP_EXISTS {
            return Err(anyhow!(
                "failed to create Windows sandbox group `{name}`: code {status}, parm_err={parm_err}"
            ));
        }
        Ok(())
    }
}

fn ensure_local_group_member(group_name: &str, member_name: &str) -> Result<()> {
    const ERROR_MEMBER_IN_ALIAS: u32 = 1378;

    let group_w = to_wide_str(group_name);
    let member_w = to_wide_str(member_name);
    unsafe {
        let member = LOCALGROUP_MEMBERS_INFO_3 {
            lgrmi3_domainandname: member_w.as_ptr() as *mut u16,
        };
        let status = NetLocalGroupAddMembers(
            std::ptr::null(),
            group_w.as_ptr(),
            3,
            &member as *const _ as *mut u8,
            1,
        );
        if status != NERR_Success && status != ERROR_MEMBER_IN_ALIAS {
            return Err(anyhow!(
                "failed to add Windows sandbox user `{member_name}` to group `{group_name}`: code {status}"
            ));
        }
        Ok(())
    }
}

fn random_password() -> String {
    const CHARS: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*()-_=+";
    let bytes = rand::random::<[u8; 32]>();
    bytes
        .iter()
        .map(|byte| CHARS[*byte as usize % CHARS.len()] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOWS_LOCAL_ACCOUNT_NAME_MAX_CHARS: usize = 20;

    #[test]
    fn sandbox_account_names_fit_windows_local_user_limit() {
        assert!(OFFLINE_USERNAME.chars().count() <= WINDOWS_LOCAL_ACCOUNT_NAME_MAX_CHARS);
        assert!(ONLINE_USERNAME.chars().count() <= WINDOWS_LOCAL_ACCOUNT_NAME_MAX_CHARS);
    }
}
