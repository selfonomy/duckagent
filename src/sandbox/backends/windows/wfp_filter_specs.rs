#![cfg(target_os = "windows")]

use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::{
    FWPM_LAYER_ALE_AUTH_CONNECT_V4, FWPM_LAYER_ALE_AUTH_CONNECT_V6,
    FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4, FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6,
};
use windows_sys::Win32::Networking::WinSock::{IPPROTO_ICMP, IPPROTO_ICMPV6};
use windows_sys::core::GUID;

#[derive(Clone, Copy)]
pub(super) enum ConditionSpec {
    User,
    Protocol(u8),
    RemotePort(u16),
    RemotePortRange(u16, u16),
    RemoteV4AddrMask(u32, u32),
    RemoteV6AddrMask([u8; 16], u8),
}

#[derive(Clone, Copy)]
pub(super) struct StaticFilterSpec {
    pub(super) key: GUID,
    pub(super) name: &'static str,
    pub(super) description: &'static str,
    pub(super) layer_key: GUID,
    pub(super) conditions: &'static [ConditionSpec],
}

pub(super) const FILTER_SPECS: &[StaticFilterSpec] = &[
    StaticFilterSpec {
        key: GUID::from_u128(0xb67a7535_7087_4b8f_a708_d30f6854f8b8),
        name: "duckagent_wfp_icmp_connect_v4",
        description: "Block DuckAgent sandbox-account ICMP connect v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMP as u8),
        ],
    },
    StaticFilterSpec {
        key: GUID::from_u128(0xd1ed1284_8fea_438f_8b99_36a0c2ea22ec),
        name: "duckagent_wfp_icmp_connect_v6",
        description: "Block DuckAgent sandbox-account ICMP connect v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMPV6 as u8),
        ],
    },
    StaticFilterSpec {
        key: GUID::from_u128(0xb2cc06f9_6e81_4068_8fb6_5b391e9277fd),
        name: "duckagent_wfp_icmp_assign_v4",
        description: "Block DuckAgent sandbox-account ICMP resource assignment v4",
        layer_key: FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMP as u8),
        ],
    },
    StaticFilterSpec {
        key: GUID::from_u128(0x530d87ef_9755_4044_9b3f_f55b5d178f39),
        name: "duckagent_wfp_icmp_assign_v6",
        description: "Block DuckAgent sandbox-account ICMP resource assignment v6",
        layer_key: FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMPV6 as u8),
        ],
    },
    StaticFilterSpec {
        key: GUID::from_u128(0x35740f22_bc44_495c_b120_a42e9cc42beb),
        name: "duckagent_wfp_dns_53_v4",
        description: "Block DuckAgent sandbox-account DNS TCP or UDP port 53 v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(53)],
    },
    StaticFilterSpec {
        key: GUID::from_u128(0xb5403dd0_141a_4ca2_8d1a_e0b6b7b0b8bc),
        name: "duckagent_wfp_dns_53_v6",
        description: "Block DuckAgent sandbox-account DNS TCP or UDP port 53 v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(53)],
    },
    StaticFilterSpec {
        key: GUID::from_u128(0x1dc7efe2_180a_4689_b8f0_2a09633b9462),
        name: "duckagent_wfp_dns_853_v4",
        description: "Block DuckAgent sandbox-account DNS-over-TLS port 853 v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(853)],
    },
    StaticFilterSpec {
        key: GUID::from_u128(0x7bbdcc02_60e0_4d25_af78_a7515f52ced7),
        name: "duckagent_wfp_dns_853_v6",
        description: "Block DuckAgent sandbox-account DNS-over-TLS port 853 v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(853)],
    },
    StaticFilterSpec {
        key: GUID::from_u128(0x3edeb4d7_6562_47e6_b3e2_ba77a9fb9038),
        name: "duckagent_wfp_smb_445_v4",
        description: "Block DuckAgent sandbox-account SMB port 445 v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(445)],
    },
    StaticFilterSpec {
        key: GUID::from_u128(0xe2079117_8d0a_40f2_903b_f4f0cff77317),
        name: "duckagent_wfp_smb_445_v6",
        description: "Block DuckAgent sandbox-account SMB port 445 v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(445)],
    },
    StaticFilterSpec {
        key: GUID::from_u128(0xf0eb3b03_6592_4b09_aac2_bb269471ba93),
        name: "duckagent_wfp_smb_139_v4",
        description: "Block DuckAgent sandbox-account SMB port 139 v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(139)],
    },
    StaticFilterSpec {
        key: GUID::from_u128(0x34f27788_82bb_4414_8eab_becd56240aad),
        name: "duckagent_wfp_smb_139_v6",
        description: "Block DuckAgent sandbox-account SMB port 139 v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(139)],
    },
];
