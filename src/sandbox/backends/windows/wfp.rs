#![cfg(target_os = "windows")]

#[path = "wfp_filter_specs.rs"]
mod filter_specs;

use crate::sandbox::backends::windows::winutil::to_wide_str;
use anyhow::Result;
use std::mem::zeroed;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    FWP_E_ALREADY_EXISTS, FWP_E_FILTER_NOT_FOUND, FWP_E_NOT_FOUND, HANDLE, HLOCAL, LocalFree,
};
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::{
    FWP_ACTION_BLOCK, FWP_ACTRL_MATCH_FILTER, FWP_BYTE_BLOB, FWP_CONDITION_VALUE0,
    FWP_CONDITION_VALUE0_0, FWP_EMPTY, FWP_MATCH_EQUAL, FWP_MATCH_PREFIX, FWP_MATCH_RANGE,
    FWP_RANGE_TYPE, FWP_RANGE0, FWP_SECURITY_DESCRIPTOR_TYPE, FWP_UINT8, FWP_UINT16,
    FWP_V4_ADDR_AND_MASK, FWP_V4_ADDR_MASK, FWP_V6_ADDR_AND_MASK, FWP_V6_ADDR_MASK, FWP_VALUE0,
    FWP_VALUE0_0, FWPM_ACTION0, FWPM_ACTION0_0, FWPM_CONDITION_ALE_USER_ID,
    FWPM_CONDITION_IP_PROTOCOL, FWPM_CONDITION_IP_REMOTE_ADDRESS, FWPM_CONDITION_IP_REMOTE_PORT,
    FWPM_DISPLAY_DATA0, FWPM_FILTER_CONDITION0, FWPM_FILTER_FLAG_PERSISTENT, FWPM_FILTER0,
    FWPM_FILTER0_0, FWPM_LAYER_ALE_AUTH_CONNECT_V4, FWPM_LAYER_ALE_AUTH_CONNECT_V6,
    FWPM_PROVIDER_FLAG_PERSISTENT, FWPM_PROVIDER0, FWPM_SESSION0, FWPM_SUBLAYER_FLAG_PERSISTENT,
    FWPM_SUBLAYER0, FwpmEngineClose0, FwpmEngineOpen0, FwpmFilterAdd0, FwpmFilterDeleteByKey0,
    FwpmProviderAdd0, FwpmSubLayerAdd0, FwpmTransactionAbort0, FwpmTransactionBegin0,
    FwpmTransactionCommit0,
};
use windows_sys::Win32::Networking::WinSock::{IPPROTO_TCP, IPPROTO_UDP};
use windows_sys::Win32::Security::Authorization::{
    BuildExplicitAccessWithNameW, BuildSecurityDescriptorW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
};
use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
use windows_sys::Win32::System::Rpc::RPC_C_AUTHN_DEFAULT;
use windows_sys::core::GUID;

use filter_specs::{ConditionSpec, FILTER_SPECS, StaticFilterSpec};

const SESSION_NAME: &str = "DuckAgent Windows Sandbox WFP";
const PROVIDER_NAME: &str = "DuckAgent Windows Sandbox WFP";
const PROVIDER_DESCRIPTION: &str = "Persistent WFP provider for DuckAgent Windows sandbox filters";
const SUBLAYER_NAME: &str = "DuckAgent Windows Sandbox WFP";
const SUBLAYER_DESCRIPTION: &str = "Persistent WFP sublayer for DuckAgent Windows sandbox filters";

const PROVIDER_KEY: GUID = GUID::from_u128(0x9bec5a6d_18c0_4d15_9995_e87f1875b390);
const SUBLAYER_KEY: GUID = GUID::from_u128(0xb5bf1bab_5bb6_42ea_98fc_27a4dd9d223d);
const WFP_TRANSACTION_WAIT_TIMEOUT_MS: u32 = 15_000;

const LOOPBACK_V4_ADDR: u32 = 0x7f00_0000;
const LOOPBACK_V4_MASK: u32 = 0xff00_0000;
const LOOPBACK_V6_ADDR: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

const DYNAMIC_FILTER_KEYS: &[GUID] = &[
    GUID::from_u128(0xf27127a8_536d_45e0_b284_937174e77801),
    GUID::from_u128(0xbbe22206_41ee_4a84_a517_a04edfa6dc6e),
    GUID::from_u128(0x61428554_8d7b_4f53_b5f0_6533377c209d),
    GUID::from_u128(0xaa64db2e_025b_48d4_99f0_7ab78a70f703),
    GUID::from_u128(0x24ee1b5e_9184_4a7d_8f1d_36ac9d60fceb),
    GUID::from_u128(0x887d00ea_2b44_4cb8_a017_722977576728),
    GUID::from_u128(0xb44bde63_9dec_4766_a547_a70243cdb12b),
    GUID::from_u128(0x0f020257_a8df_4912_a992_e46f92398efc),
];

pub fn install_wfp_filters_for_account(account: &str, proxy_ports: &[u16]) -> Result<usize> {
    let engine = Engine::open()?;
    let mut transaction = engine.begin_transaction()?;
    ensure_provider(engine.handle)?;
    ensure_sublayer(engine.handle)?;

    let user_condition = UserMatchCondition::for_account(account)?;
    let mut installed_filter_count = 0;
    for spec in FILTER_SPECS {
        delete_filter_if_present(engine.handle, &spec.key)?;
        add_filter(engine.handle, spec, &user_condition)?;
        installed_filter_count += 1;
    }
    for key in DYNAMIC_FILTER_KEYS {
        delete_filter_if_present(engine.handle, key)?;
    }
    for spec in loopback_filter_specs(proxy_ports)? {
        add_filter(engine.handle, &spec, &user_condition)?;
        installed_filter_count += 1;
    }

    transaction.commit()?;
    Ok(installed_filter_count)
}

struct DynamicFilterSpec {
    key: GUID,
    name: String,
    description: String,
    layer_key: GUID,
    conditions: Vec<ConditionSpec>,
}

trait FilterSpecLike {
    fn key(&self) -> GUID;
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn layer_key(&self) -> GUID;
    fn conditions(&self) -> &[ConditionSpec];
}

impl FilterSpecLike for StaticFilterSpec {
    fn key(&self) -> GUID {
        self.key
    }

    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn layer_key(&self) -> GUID {
        self.layer_key
    }

    fn conditions(&self) -> &[ConditionSpec] {
        self.conditions
    }
}

impl FilterSpecLike for DynamicFilterSpec {
    fn key(&self) -> GUID {
        self.key
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn layer_key(&self) -> GUID {
        self.layer_key
    }

    fn conditions(&self) -> &[ConditionSpec] {
        &self.conditions
    }
}

fn loopback_filter_specs(proxy_ports: &[u16]) -> Result<Vec<DynamicFilterSpec>> {
    let mut specs = vec![
        loopback_filter(
            DYNAMIC_FILTER_KEYS[0],
            "duckagent_wfp_loopback_udp_v4",
            "Block DuckAgent sandbox-account UDP loopback connect v4",
            FWPM_LAYER_ALE_AUTH_CONNECT_V4,
            vec![
                ConditionSpec::Protocol(IPPROTO_UDP as u8),
                ConditionSpec::RemoteV4AddrMask(LOOPBACK_V4_ADDR, LOOPBACK_V4_MASK),
            ],
        ),
        loopback_filter(
            DYNAMIC_FILTER_KEYS[1],
            "duckagent_wfp_loopback_udp_v6",
            "Block DuckAgent sandbox-account UDP loopback connect v6",
            FWPM_LAYER_ALE_AUTH_CONNECT_V6,
            vec![
                ConditionSpec::Protocol(IPPROTO_UDP as u8),
                ConditionSpec::RemoteV6AddrMask(LOOPBACK_V6_ADDR, 128),
            ],
        ),
    ];

    let ranges = loopback_tcp_blocked_port_ranges(proxy_ports);
    if ranges.len() > 3 {
        anyhow::bail!(
            "Windows WFP loopback filter setup supports at most two managed proxy ports, got {}",
            proxy_ports.len()
        );
    }
    for (index, (start, end)) in ranges.into_iter().take(3).enumerate() {
        let v4_key = DYNAMIC_FILTER_KEYS[2 + (index * 2)];
        let v6_key = DYNAMIC_FILTER_KEYS[3 + (index * 2)];
        specs.push(loopback_filter(
            v4_key,
            &format!("duckagent_wfp_loopback_tcp_v4_range_{index}"),
            "Block DuckAgent sandbox-account TCP loopback connect v4 except managed proxy",
            FWPM_LAYER_ALE_AUTH_CONNECT_V4,
            vec![
                ConditionSpec::Protocol(IPPROTO_TCP as u8),
                ConditionSpec::RemoteV4AddrMask(LOOPBACK_V4_ADDR, LOOPBACK_V4_MASK),
                ConditionSpec::RemotePortRange(start, end),
            ],
        ));
        specs.push(loopback_filter(
            v6_key,
            &format!("duckagent_wfp_loopback_tcp_v6_range_{index}"),
            "Block DuckAgent sandbox-account TCP loopback connect v6 except managed proxy",
            FWPM_LAYER_ALE_AUTH_CONNECT_V6,
            vec![
                ConditionSpec::Protocol(IPPROTO_TCP as u8),
                ConditionSpec::RemoteV6AddrMask(LOOPBACK_V6_ADDR, 128),
                ConditionSpec::RemotePortRange(start, end),
            ],
        ));
    }
    Ok(specs)
}

fn loopback_filter(
    key: GUID,
    name: &str,
    description: &str,
    layer_key: GUID,
    conditions: Vec<ConditionSpec>,
) -> DynamicFilterSpec {
    DynamicFilterSpec {
        key,
        name: name.to_string(),
        description: description.to_string(),
        layer_key,
        conditions,
    }
}

fn loopback_tcp_blocked_port_ranges(proxy_ports: &[u16]) -> Vec<(u16, u16)> {
    let mut allowed_ports = proxy_ports
        .iter()
        .copied()
        .filter(|port| *port != 0)
        .collect::<Vec<_>>();
    allowed_ports.sort_unstable();
    allowed_ports.dedup();

    let mut ranges = Vec::new();
    let mut start = 1_u32;
    for port in allowed_ports {
        let port = u32::from(port);
        if port < start {
            continue;
        }
        if port > start {
            ranges.push((start as u16, (port - 1) as u16));
        }
        start = port + 1;
    }
    if start <= u32::from(u16::MAX) {
        ranges.push((start as u16, u16::MAX));
    }
    ranges
}

struct Engine {
    handle: HANDLE,
}

impl Engine {
    fn open() -> Result<Self> {
        let session_name = to_wide_str(SESSION_NAME);
        let mut session: FWPM_SESSION0 = unsafe { zeroed() };
        session.displayData = FWPM_DISPLAY_DATA0 {
            name: session_name.as_ptr() as *mut _,
            description: null_mut(),
        };
        session.txnWaitTimeoutInMSec = WFP_TRANSACTION_WAIT_TIMEOUT_MS;

        let mut handle = HANDLE::default();
        let result = unsafe {
            FwpmEngineOpen0(
                null(),
                RPC_C_AUTHN_DEFAULT as u32,
                null(),
                &session,
                &mut handle,
            )
        };
        ensure_success(result, "FwpmEngineOpen0")?;
        Ok(Self { handle })
    }

    fn begin_transaction(&self) -> Result<Transaction<'_>> {
        let result = unsafe { FwpmTransactionBegin0(self.handle, 0) };
        ensure_success(result, "FwpmTransactionBegin0")?;
        Ok(Transaction {
            engine: self,
            committed: false,
        })
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            FwpmEngineClose0(self.handle);
        }
    }
}

struct Transaction<'a> {
    engine: &'a Engine,
    committed: bool,
}

impl Transaction<'_> {
    fn commit(&mut self) -> Result<()> {
        let result = unsafe { FwpmTransactionCommit0(self.engine.handle) };
        ensure_success(result, "FwpmTransactionCommit0")?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            unsafe {
                FwpmTransactionAbort0(self.engine.handle);
            }
        }
    }
}

struct UserMatchCondition {
    security_descriptor: PSECURITY_DESCRIPTOR,
    blob: FWP_BYTE_BLOB,
}

impl UserMatchCondition {
    fn for_account(account: &str) -> Result<Self> {
        let account_w = to_wide_str(account);
        let mut access: EXPLICIT_ACCESS_W = unsafe { zeroed() };
        unsafe {
            BuildExplicitAccessWithNameW(
                &mut access,
                account_w.as_ptr(),
                FWP_ACTRL_MATCH_FILTER,
                GRANT_ACCESS,
                0,
            );
        }

        let mut security_descriptor: PSECURITY_DESCRIPTOR = null_mut();
        let mut security_descriptor_len = 0;
        let result = unsafe {
            BuildSecurityDescriptorW(
                null(),
                null(),
                1,
                &access,
                0,
                null(),
                null_mut(),
                &mut security_descriptor_len,
                &mut security_descriptor,
            )
        };
        ensure_success(result, "BuildSecurityDescriptorW")?;

        Ok(Self {
            security_descriptor,
            blob: FWP_BYTE_BLOB {
                size: security_descriptor_len,
                data: security_descriptor as *mut u8,
            },
        })
    }
}

impl Drop for UserMatchCondition {
    fn drop(&mut self) {
        if !self.security_descriptor.is_null() {
            unsafe {
                LocalFree(self.security_descriptor as HLOCAL);
            }
        }
    }
}

fn ensure_provider(engine: HANDLE) -> Result<()> {
    let provider_name = to_wide_str(PROVIDER_NAME);
    let provider_description = to_wide_str(PROVIDER_DESCRIPTION);
    let provider = FWPM_PROVIDER0 {
        providerKey: PROVIDER_KEY,
        displayData: FWPM_DISPLAY_DATA0 {
            name: provider_name.as_ptr() as *mut _,
            description: provider_description.as_ptr() as *mut _,
        },
        flags: FWPM_PROVIDER_FLAG_PERSISTENT,
        providerData: empty_blob(),
        serviceName: null_mut(),
    };

    let result = unsafe { FwpmProviderAdd0(engine, &provider, null_mut()) };
    ensure_success_or(result, "FwpmProviderAdd0", &[FWP_E_ALREADY_EXISTS as u32])
}

fn ensure_sublayer(engine: HANDLE) -> Result<()> {
    let sublayer_name = to_wide_str(SUBLAYER_NAME);
    let sublayer_description = to_wide_str(SUBLAYER_DESCRIPTION);
    let provider_key = PROVIDER_KEY;
    let sublayer = FWPM_SUBLAYER0 {
        subLayerKey: SUBLAYER_KEY,
        displayData: FWPM_DISPLAY_DATA0 {
            name: sublayer_name.as_ptr() as *mut _,
            description: sublayer_description.as_ptr() as *mut _,
        },
        flags: FWPM_SUBLAYER_FLAG_PERSISTENT,
        providerKey: &provider_key as *const _ as *mut _,
        providerData: empty_blob(),
        weight: 0x8000,
    };

    let result = unsafe { FwpmSubLayerAdd0(engine, &sublayer, null_mut()) };
    ensure_success_or(result, "FwpmSubLayerAdd0", &[FWP_E_ALREADY_EXISTS as u32])
}

fn add_filter(
    engine: HANDLE,
    spec: &impl FilterSpecLike,
    user_condition: &UserMatchCondition,
) -> Result<()> {
    let filter_name = to_wide_str(spec.name());
    let filter_description = to_wide_str(spec.description());
    let mut filter_conditions = build_conditions(spec.conditions(), user_condition);
    let provider_key = PROVIDER_KEY;
    let filter = FWPM_FILTER0 {
        filterKey: spec.key(),
        displayData: FWPM_DISPLAY_DATA0 {
            name: filter_name.as_ptr() as *mut _,
            description: filter_description.as_ptr() as *mut _,
        },
        flags: FWPM_FILTER_FLAG_PERSISTENT,
        providerKey: &provider_key as *const _ as *mut _,
        providerData: empty_blob(),
        layerKey: spec.layer_key(),
        subLayerKey: SUBLAYER_KEY,
        weight: empty_value(),
        numFilterConditions: filter_conditions.len() as u32,
        filterCondition: filter_conditions.as_mut_ptr(),
        action: FWPM_ACTION0 {
            r#type: FWP_ACTION_BLOCK,
            Anonymous: FWPM_ACTION0_0 {
                filterType: zero_guid(),
            },
        },
        Anonymous: FWPM_FILTER0_0 { rawContext: 0 },
        reserved: null_mut(),
        filterId: 0,
        effectiveWeight: empty_value(),
    };

    let mut filter_id = 0_u64;
    let result = unsafe { FwpmFilterAdd0(engine, &filter, null_mut(), &mut filter_id) };
    ensure_success(result, &format!("FwpmFilterAdd0({})", spec.name()))
}

struct BuiltConditions {
    conditions: Vec<FWPM_FILTER_CONDITION0>,
    _v4_masks: Vec<Box<FWP_V4_ADDR_AND_MASK>>,
    _v6_masks: Vec<Box<FWP_V6_ADDR_AND_MASK>>,
    _port_ranges: Vec<Box<FWP_RANGE0>>,
}

impl std::ops::Deref for BuiltConditions {
    type Target = [FWPM_FILTER_CONDITION0];

    fn deref(&self) -> &Self::Target {
        &self.conditions
    }
}

impl BuiltConditions {
    fn len(&self) -> usize {
        self.conditions.len()
    }

    fn as_mut_ptr(&mut self) -> *mut FWPM_FILTER_CONDITION0 {
        self.conditions.as_mut_ptr()
    }
}

fn build_conditions(
    specs: &[ConditionSpec],
    user_condition: &UserMatchCondition,
) -> BuiltConditions {
    let mut built = BuiltConditions {
        conditions: Vec::with_capacity(specs.len()),
        _v4_masks: Vec::new(),
        _v6_masks: Vec::new(),
        _port_ranges: Vec::new(),
    };
    for spec in specs {
        let condition = match spec {
            ConditionSpec::User => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_ALE_USER_ID,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_SECURITY_DESCRIPTOR_TYPE,
                    Anonymous: FWP_CONDITION_VALUE0_0 {
                        sd: &user_condition.blob as *const _ as *mut _,
                    },
                },
            },
            ConditionSpec::Protocol(protocol) => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_IP_PROTOCOL,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT8,
                    Anonymous: FWP_CONDITION_VALUE0_0 { uint8: *protocol },
                },
            },
            ConditionSpec::RemotePort(port) => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_IP_REMOTE_PORT,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT16,
                    Anonymous: FWP_CONDITION_VALUE0_0 { uint16: *port },
                },
            },
            ConditionSpec::RemotePortRange(low, high) => {
                built._port_ranges.push(Box::new(FWP_RANGE0 {
                    valueLow: FWP_VALUE0 {
                        r#type: FWP_UINT16,
                        Anonymous: FWP_VALUE0_0 { uint16: *low },
                    },
                    valueHigh: FWP_VALUE0 {
                        r#type: FWP_UINT16,
                        Anonymous: FWP_VALUE0_0 { uint16: *high },
                    },
                }));
                let range = built
                    ._port_ranges
                    .last_mut()
                    .expect("range was pushed above");
                FWPM_FILTER_CONDITION0 {
                    fieldKey: FWPM_CONDITION_IP_REMOTE_PORT,
                    matchType: FWP_MATCH_RANGE,
                    conditionValue: FWP_CONDITION_VALUE0 {
                        r#type: FWP_RANGE_TYPE,
                        Anonymous: FWP_CONDITION_VALUE0_0 {
                            rangeValue: range.as_mut() as *mut _,
                        },
                    },
                }
            }
            ConditionSpec::RemoteV4AddrMask(addr, mask) => {
                built._v4_masks.push(Box::new(FWP_V4_ADDR_AND_MASK {
                    addr: *addr,
                    mask: *mask,
                }));
                let mask = built
                    ._v4_masks
                    .last_mut()
                    .expect("IPv4 mask was pushed above");
                FWPM_FILTER_CONDITION0 {
                    fieldKey: FWPM_CONDITION_IP_REMOTE_ADDRESS,
                    matchType: FWP_MATCH_PREFIX,
                    conditionValue: FWP_CONDITION_VALUE0 {
                        r#type: FWP_V4_ADDR_MASK,
                        Anonymous: FWP_CONDITION_VALUE0_0 {
                            v4AddrMask: mask.as_mut() as *mut _,
                        },
                    },
                }
            }
            ConditionSpec::RemoteV6AddrMask(addr, prefix) => {
                built._v6_masks.push(Box::new(FWP_V6_ADDR_AND_MASK {
                    addr: *addr,
                    prefixLength: *prefix,
                }));
                let mask = built
                    ._v6_masks
                    .last_mut()
                    .expect("IPv6 mask was pushed above");
                FWPM_FILTER_CONDITION0 {
                    fieldKey: FWPM_CONDITION_IP_REMOTE_ADDRESS,
                    matchType: FWP_MATCH_PREFIX,
                    conditionValue: FWP_CONDITION_VALUE0 {
                        r#type: FWP_V6_ADDR_MASK,
                        Anonymous: FWP_CONDITION_VALUE0_0 {
                            v6AddrMask: mask.as_mut() as *mut _,
                        },
                    },
                }
            }
        };
        built.conditions.push(condition);
    }
    built
}

fn delete_filter_if_present(engine: HANDLE, key: &GUID) -> Result<()> {
    let result = unsafe { FwpmFilterDeleteByKey0(engine, key) };
    ensure_success_or(
        result,
        "FwpmFilterDeleteByKey0",
        &[FWP_E_FILTER_NOT_FOUND as u32, FWP_E_NOT_FOUND as u32],
    )
}

fn ensure_success(result: u32, operation: &str) -> Result<()> {
    ensure_success_or(result, operation, &[])
}

fn ensure_success_or(result: u32, operation: &str, allowed: &[u32]) -> Result<()> {
    if result == 0 || allowed.contains(&result) {
        Ok(())
    } else {
        Err(anyhow::anyhow!("{operation} failed: 0x{result:08X}"))
    }
}

fn empty_blob() -> FWP_BYTE_BLOB {
    FWP_BYTE_BLOB {
        size: 0,
        data: null_mut(),
    }
}

fn empty_value() -> FWP_VALUE0 {
    FWP_VALUE0 {
        r#type: FWP_EMPTY,
        Anonymous: unsafe { zeroed() },
    }
}

fn zero_guid() -> GUID {
    GUID::from_u128(0)
}

#[cfg(test)]
mod tests {
    use super::{FILTER_SPECS, loopback_tcp_blocked_port_ranges};
    use std::collections::BTreeSet;

    #[test]
    fn filter_keys_are_unique() {
        let keys = FILTER_SPECS
            .iter()
            .map(|spec| {
                (
                    spec.key.data1,
                    spec.key.data2,
                    spec.key.data3,
                    spec.key.data4,
                )
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(keys.len(), FILTER_SPECS.len());
    }

    #[test]
    fn filter_names_are_unique() {
        let names = FILTER_SPECS
            .iter()
            .map(|spec| spec.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), FILTER_SPECS.len());
    }

    #[test]
    fn loopback_tcp_block_ranges_exclude_proxy_port() {
        assert_eq!(
            loopback_tcp_blocked_port_ranges(&[8080]),
            vec![(1, 8079), (8081, u16::MAX)]
        );
        assert_eq!(
            loopback_tcp_blocked_port_ranges(&[1, u16::MAX]),
            vec![(2, 65534)]
        );
        assert_eq!(loopback_tcp_blocked_port_ranges(&[]), vec![(1, u16::MAX)]);
    }
}
