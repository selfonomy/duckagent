use anyhow::{Context, Result, anyhow};
use std::io::Write;

use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::NetworkManagement::WindowsFirewall::{
    INetFwPolicy2, INetFwRule3, INetFwRules, NET_FW_ACTION_BLOCK, NET_FW_IP_PROTOCOL_ANY,
    NET_FW_IP_PROTOCOL_TCP, NET_FW_IP_PROTOCOL_UDP, NET_FW_PROFILE2_ALL, NET_FW_RULE_DIR_OUT,
    NetFwPolicy2, NetFwRule,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::core::{BSTR, Interface};

const OFFLINE_BLOCK_RULE_NAME: &str = "duckagent_sandbox_offline_block_outbound";
const OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME: &str = "duckagent_sandbox_offline_block_loopback_tcp";
const OFFLINE_BLOCK_LOOPBACK_UDP_RULE_NAME: &str = "duckagent_sandbox_offline_block_loopback_udp";

const OFFLINE_BLOCK_RULE_FRIENDLY: &str = "DuckAgent Sandbox Offline - Block Non-Loopback Outbound";
const OFFLINE_BLOCK_LOOPBACK_TCP_RULE_FRIENDLY: &str =
    "DuckAgent Sandbox Offline - Block Loopback TCP (Except Proxy)";
const OFFLINE_BLOCK_LOOPBACK_UDP_RULE_FRIENDLY: &str =
    "DuckAgent Sandbox Offline - Block Loopback UDP";

const LOOPBACK_REMOTE_ADDRESSES: &str = "127.0.0.0/8,::/127";
const NON_LOOPBACK_REMOTE_ADDRESSES: &str = "0.0.0.0-126.255.255.255,128.0.0.0-255.255.255.255,::,::2-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff";

struct BlockRuleSpec<'a> {
    internal_name: &'a str,
    friendly_desc: &'a str,
    protocol: i32,
    local_user_spec: &'a str,
    offline_sid: &'a str,
    remote_addresses: Option<&'a str>,
    remote_ports: Option<&'a str>,
}

pub fn ensure_offline_network_rules(
    offline_sid: &str,
    proxy_ports: &[u16],
    allow_local_binding: bool,
    log: &mut dyn Write,
) -> Result<()> {
    ensure_offline_outbound_block(offline_sid, log)?;
    ensure_offline_loopback_policy(offline_sid, proxy_ports, allow_local_binding, log)
}

fn ensure_offline_loopback_policy(
    offline_sid: &str,
    proxy_ports: &[u16],
    allow_local_binding: bool,
    log: &mut dyn Write,
) -> Result<()> {
    with_firewall_rules(|rules| {
        if allow_local_binding {
            remove_rule_if_present(rules, OFFLINE_BLOCK_LOOPBACK_UDP_RULE_NAME, log)?;
            remove_rule_if_present(rules, OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME, log)?;
            return Ok(());
        }

        let local_user_spec = local_user_spec(offline_sid);
        ensure_block_rule(
            rules,
            &BlockRuleSpec {
                internal_name: OFFLINE_BLOCK_LOOPBACK_UDP_RULE_NAME,
                friendly_desc: OFFLINE_BLOCK_LOOPBACK_UDP_RULE_FRIENDLY,
                protocol: NET_FW_IP_PROTOCOL_UDP.0,
                local_user_spec: &local_user_spec,
                offline_sid,
                remote_addresses: Some(LOOPBACK_REMOTE_ADDRESSES),
                remote_ports: None,
            },
            log,
        )?;

        ensure_block_rule(
            rules,
            &BlockRuleSpec {
                internal_name: OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME,
                friendly_desc: OFFLINE_BLOCK_LOOPBACK_TCP_RULE_FRIENDLY,
                protocol: NET_FW_IP_PROTOCOL_TCP.0,
                local_user_spec: &local_user_spec,
                offline_sid,
                remote_addresses: Some(LOOPBACK_REMOTE_ADDRESSES),
                remote_ports: None,
            },
            log,
        )?;

        if let Some(blocked_ports) = blocked_loopback_tcp_remote_ports(proxy_ports) {
            ensure_block_rule(
                rules,
                &BlockRuleSpec {
                    internal_name: OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME,
                    friendly_desc: OFFLINE_BLOCK_LOOPBACK_TCP_RULE_FRIENDLY,
                    protocol: NET_FW_IP_PROTOCOL_TCP.0,
                    local_user_spec: &local_user_spec,
                    offline_sid,
                    remote_addresses: Some(LOOPBACK_REMOTE_ADDRESSES),
                    remote_ports: Some(&blocked_ports),
                },
                log,
            )?;
        }
        Ok(())
    })
}

fn ensure_offline_outbound_block(offline_sid: &str, log: &mut dyn Write) -> Result<()> {
    with_firewall_rules(|rules| {
        let local_user_spec = local_user_spec(offline_sid);
        ensure_block_rule(
            rules,
            &BlockRuleSpec {
                internal_name: OFFLINE_BLOCK_RULE_NAME,
                friendly_desc: OFFLINE_BLOCK_RULE_FRIENDLY,
                protocol: NET_FW_IP_PROTOCOL_ANY.0,
                local_user_spec: &local_user_spec,
                offline_sid,
                remote_addresses: Some(NON_LOOPBACK_REMOTE_ADDRESSES),
                remote_ports: None,
            },
            log,
        )
    })
}

fn with_firewall_rules<T>(f: impl FnOnce(&INetFwRules) -> Result<T>) -> Result<T> {
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        return Err(anyhow!(
            "CoInitializeEx failed for Windows firewall setup: {hr:?}"
        ));
    }

    let result = unsafe {
        (|| -> Result<T> {
            let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)
                .map_err(|err| anyhow!("CoCreateInstance(NetFwPolicy2) failed: {err:?}"))?;
            let rules = policy
                .Rules()
                .map_err(|err| anyhow!("INetFwPolicy2::Rules failed: {err:?}"))?;
            f(&rules)
        })()
    };

    unsafe {
        CoUninitialize();
    }
    result
}

fn remove_rule_if_present(
    rules: &INetFwRules,
    internal_name: &str,
    log: &mut dyn Write,
) -> Result<()> {
    let name = BSTR::from(internal_name);
    if unsafe { rules.Item(&name) }.is_ok() {
        unsafe { rules.Remove(&name) }
            .map_err(|err| anyhow!("Rules::Remove failed for {internal_name}: {err:?}"))?;
        log_line(log, &format!("firewall rule removed name={internal_name}"))?;
    }
    Ok(())
}

fn ensure_block_rule(
    rules: &INetFwRules,
    spec: &BlockRuleSpec<'_>,
    log: &mut dyn Write,
) -> Result<()> {
    let name = BSTR::from(spec.internal_name);
    let rule: INetFwRule3 = match unsafe { rules.Item(&name) } {
        Ok(existing) => existing
            .cast()
            .map_err(|err| anyhow!("cast existing firewall rule to INetFwRule3 failed: {err:?}"))?,
        Err(_) => {
            let new_rule: INetFwRule3 =
                unsafe { CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER) }
                    .map_err(|err| anyhow!("CoCreateInstance(NetFwRule) failed: {err:?}"))?;
            unsafe { new_rule.SetName(&name) }
                .map_err(|err| anyhow!("INetFwRule::SetName failed: {err:?}"))?;
            configure_rule(&new_rule, spec)?;
            unsafe { rules.Add(&new_rule) }
                .map_err(|err| anyhow!("INetFwRules::Add failed: {err:?}"))?;
            new_rule
        }
    };

    configure_rule(&rule, spec)?;
    log_line(
        log,
        &format!(
            "firewall rule configured name={} protocol={} remote_addresses={} remote_ports={} local_user={}",
            spec.internal_name,
            spec.protocol,
            spec.remote_addresses.unwrap_or("*"),
            spec.remote_ports.unwrap_or("*"),
            spec.local_user_spec
        ),
    )?;
    Ok(())
}

fn configure_rule(rule: &INetFwRule3, spec: &BlockRuleSpec<'_>) -> Result<()> {
    unsafe {
        rule.SetDescription(&BSTR::from(spec.friendly_desc))
            .map_err(|err| anyhow!("INetFwRule::SetDescription failed: {err:?}"))?;
        rule.SetDirection(NET_FW_RULE_DIR_OUT)
            .map_err(|err| anyhow!("INetFwRule::SetDirection failed: {err:?}"))?;
        rule.SetAction(NET_FW_ACTION_BLOCK)
            .map_err(|err| anyhow!("INetFwRule::SetAction failed: {err:?}"))?;
        rule.SetEnabled(VARIANT_TRUE)
            .map_err(|err| anyhow!("INetFwRule::SetEnabled failed: {err:?}"))?;
        rule.SetProfiles(NET_FW_PROFILE2_ALL.0)
            .map_err(|err| anyhow!("INetFwRule::SetProfiles failed: {err:?}"))?;
        rule.SetProtocol(spec.protocol)
            .map_err(|err| anyhow!("INetFwRule::SetProtocol failed: {err:?}"))?;
        rule.SetRemoteAddresses(&BSTR::from(spec.remote_addresses.unwrap_or("*")))
            .map_err(|err| anyhow!("INetFwRule::SetRemoteAddresses failed: {err:?}"))?;
        if let Some(remote_ports) = spec.remote_ports {
            rule.SetRemotePorts(&BSTR::from(remote_ports))
                .map_err(|err| anyhow!("INetFwRule::SetRemotePorts failed: {err:?}"))?;
        } else if spec.protocol == NET_FW_IP_PROTOCOL_TCP.0 {
            rule.SetRemotePorts(&BSTR::from("*"))
                .map_err(|err| anyhow!("INetFwRule::SetRemotePorts failed: {err:?}"))?;
        }
        rule.SetLocalUserAuthorizedList(&BSTR::from(spec.local_user_spec))
            .map_err(|err| anyhow!("INetFwRule::SetLocalUserAuthorizedList failed: {err:?}"))?;
    }

    let actual = unsafe { rule.LocalUserAuthorizedList() }
        .map_err(|err| anyhow!("INetFwRule::LocalUserAuthorizedList read-back failed: {err:?}"))?;
    let actual = actual.to_string();
    if !actual.contains(spec.offline_sid) {
        return Err(anyhow!(
            "offline firewall rule user scope mismatch: expected SID {}, got {}",
            spec.offline_sid,
            actual
        ));
    }
    Ok(())
}

fn local_user_spec(offline_sid: &str) -> String {
    format!("O:LSD:(A;;CC;;;{offline_sid})")
}

pub(crate) fn blocked_loopback_tcp_remote_ports(proxy_ports: &[u16]) -> Option<String> {
    let mut allowed_ports = proxy_ports
        .iter()
        .copied()
        .filter(|port| *port != 0)
        .collect::<Vec<_>>();
    allowed_ports.sort_unstable();
    allowed_ports.dedup();

    let mut blocked_ranges = Vec::new();
    let mut start = 1_u32;
    for port in allowed_ports {
        let port = u32::from(port);
        if port < start {
            continue;
        }
        if port > start {
            blocked_ranges.push(port_range_string(start, port - 1));
        }
        start = port + 1;
    }
    if start <= u32::from(u16::MAX) {
        blocked_ranges.push(port_range_string(start, u32::from(u16::MAX)));
    }

    (!blocked_ranges.is_empty()).then(|| blocked_ranges.join(","))
}

fn port_range_string(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

fn log_line(log: &mut dyn Write, message: &str) -> Result<()> {
    writeln!(log, "[{}] {}", chrono::Utc::now().to_rfc3339(), message)
        .context("failed to write Windows firewall setup log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_port_complement_blocks_everything_except_allowed_ports() {
        assert_eq!(
            blocked_loopback_tcp_remote_ports(&[8080]).as_deref(),
            Some("1-8079,8081-65535")
        );
        assert_eq!(
            blocked_loopback_tcp_remote_ports(&[1, 65535]).as_deref(),
            Some("2-65534")
        );
        assert_eq!(
            blocked_loopback_tcp_remote_ports(&[]).as_deref(),
            Some("1-65535")
        );
    }
}
