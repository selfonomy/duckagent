use crate::auth::{GatewayCredentialEntry, load_auth_store};
use crate::mcp::config::DuckAgentConfig;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::net::SocketAddr;

pub const DEFAULT_GATEWAY_BIND: &str = "127.0.0.1:0";
pub const DEFAULT_GATEWAY_CONFIGURED_BIND: &str = "127.0.0.1:8788";
pub const WEBHOOK_CHANNEL: &str = "webhook";
pub const TELEGRAM_CHANNEL: &str = "telegram";
pub const SLACK_CHANNEL: &str = "slack";
pub const SIGNAL_CHANNEL: &str = "signal";
pub const MATRIX_CHANNEL: &str = "matrix";
pub const FEISHU_CHANNEL: &str = "feishu";
pub const LARK_CHANNEL: &str = "lark";
pub const FEISHU_COMMENT_CHANNEL: &str = "feishu_comment";
pub const LARK_COMMENT_CHANNEL: &str = "lark_comment";
pub const DISCORD_CHANNEL: &str = "discord";
pub const MATTERMOST_CHANNEL: &str = "mattermost";
pub const API_SERVER_CHANNEL: &str = "api_server";
pub const WHATSAPP_CHANNEL: &str = "whatsapp";
pub const DINGTALK_CHANNEL: &str = "dingtalk";
pub const WECOM_CHANNEL: &str = "wecom";
pub const WECOM_CALLBACK_CHANNEL: &str = "wecom_callback";
pub const WEIXIN_CHANNEL: &str = "weixin";
pub const BLUEBUBBLES_CHANNEL: &str = "bluebubbles";
pub const IMESSAGE_CHANNEL: &str = "imessage";
pub const EMAIL_CHANNEL: &str = "email";
pub const SMS_CHANNEL: &str = "sms";
pub const MSGRAPH_WEBHOOK_CHANNEL: &str = "msgraph_webhook";
pub const MSTEAMS_CHANNEL: &str = "msteams";
pub const GOOGLECHAT_CHANNEL: &str = "googlechat";
pub const LINE_CHANNEL: &str = "line";
pub const IRC_CHANNEL: &str = "irc";
pub const NEXTCLOUD_TALK_CHANNEL: &str = "nextcloud-talk";
pub const NOSTR_CHANNEL: &str = "nostr";
pub const SYNOLOGY_CHAT_CHANNEL: &str = "synology-chat";
pub const TLON_CHANNEL: &str = "tlon";
pub const TWITCH_CHANNEL: &str = "twitch";
pub const ZALO_CHANNEL: &str = "zalo";
pub const ZALOUSER_CHANNEL: &str = "zalouser";
pub const HOMEASSISTANT_CHANNEL: &str = "homeassistant";
pub const QQBOT_CHANNEL: &str = "qqbot";
pub const YUANBAO_CHANNEL: &str = "yuanbao";
pub const QA_CHANNEL: &str = "qa-channel";
pub const VOICE_CALL_CHANNEL: &str = "voice-call";
pub const TALK_VOICE_CHANNEL: &str = "talk-voice";

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct GatewayConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub channels: BTreeMap<String, GatewayChannelConfig>,
}

impl GatewayConfig {
    pub fn bind_addr(&self) -> Result<SocketAddr> {
        self.bind
            .as_deref()
            .unwrap_or(DEFAULT_GATEWAY_BIND)
            .parse()
            .with_context(|| {
                format!(
                    "gateway.bind must be a socket address like {}",
                    DEFAULT_GATEWAY_BIND
                )
            })
    }

    pub fn enabled_channels(&self) -> impl Iterator<Item = (&String, &GatewayChannelConfig)> {
        self.channels.iter().filter(|(_, config)| config.enabled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GatewayChannelConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_users: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_chats: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<GatewayHomeTarget>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "GatewayTypingConfig::is_default")]
    pub typing: GatewayTypingConfig,
    #[serde(default, skip_serializing_if = "GatewayMediaConfig::is_default")]
    pub media: GatewayMediaConfig,
    #[serde(default, skip_serializing_if = "GatewayApprovalConfig::is_default")]
    pub approval: GatewayApprovalConfig,
    #[serde(default, skip_serializing_if = "GatewayAccessConfig::is_default")]
    pub access: GatewayAccessConfig,
}

impl Default for GatewayChannelConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            transport: None,
            api_base: None,
            allowed_users: Vec::new(),
            allowed_chats: Vec::new(),
            home: None,
            extra: BTreeMap::new(),
            typing: GatewayTypingConfig::default(),
            media: GatewayMediaConfig::default(),
            approval: GatewayApprovalConfig::default(),
            access: GatewayAccessConfig::default(),
        }
    }
}

impl GatewayChannelConfig {
    pub fn auth_key_for(&self, channel: &str) -> String {
        channel.to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayHomeTarget {
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayTypingConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_typing_refresh_seconds")]
    pub refresh_seconds: u64,
}

impl Default for GatewayTypingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh_seconds: default_typing_refresh_seconds(),
        }
    }
}

impl GatewayTypingConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayMediaConfig {
    #[serde(default = "default_media_max_download_bytes")]
    pub max_download_bytes: u64,
    #[serde(default = "default_true")]
    pub allow_voice: bool,
}

impl Default for GatewayMediaConfig {
    fn default() -> Self {
        Self {
            max_download_bytes: default_media_max_download_bytes(),
            allow_voice: true,
        }
    }
}

impl GatewayMediaConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayApprovalConfig {
    #[serde(default = "default_approval_mode")]
    pub mode: String,
}

impl Default for GatewayApprovalConfig {
    fn default() -> Self {
        Self {
            mode: default_approval_mode(),
        }
    }
}

impl GatewayApprovalConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayAccessConfig {
    #[serde(default = "default_dm_access_policy")]
    pub dm_policy: String,
    #[serde(default = "default_group_access_policy")]
    pub group_policy: String,
    #[serde(default = "default_true")]
    pub require_mention: bool,
}

impl Default for GatewayAccessConfig {
    fn default() -> Self {
        Self {
            dm_policy: default_dm_access_policy(),
            group_policy: default_group_access_policy(),
            require_mention: true,
        }
    }
}

impl GatewayAccessConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone)]
pub struct GatewayLaunchChannel {
    pub channel: String,
    pub config: GatewayChannelConfig,
    pub credentials: Option<GatewayCredentialEntry>,
}

pub fn load_gateway_config() -> Result<GatewayConfig> {
    let config = DuckAgentConfig::load_active_profile()?;
    match config.raw().get("gateway") {
        None | Some(Value::Null) => Ok(GatewayConfig::default()),
        Some(value) => serde_json::from_value(value.clone())
            .context("failed to parse active profile gateway config"),
    }
}

pub fn save_gateway_config(gateway: &GatewayConfig) -> Result<()> {
    let mut config = DuckAgentConfig::load_active_profile()?;
    set_gateway_config(&mut config, gateway)?;
    config.save_active_profile()
}

pub fn set_gateway_config(config: &mut DuckAgentConfig, gateway: &GatewayConfig) -> Result<()> {
    config.raw_mut().insert(
        "gateway".to_string(),
        serde_json::to_value(gateway).context("failed to serialize gateway config")?,
    );
    Ok(())
}

pub fn load_gateway_credentials(id: &str) -> Result<Option<GatewayCredentialEntry>> {
    let store = load_auth_store().unwrap_or_default();
    Ok(store.gateway.get(id).cloned())
}

pub fn resolve_launch_channels(config: &GatewayConfig) -> Result<Vec<GatewayLaunchChannel>> {
    let mut out = Vec::new();
    for (raw_channel, channel_config) in config.enabled_channels() {
        let channel = normalize_channel_name(raw_channel);
        let credentials = if channel == WEBHOOK_CHANNEL {
            None
        } else {
            let auth_key = channel_config.auth_key_for(&channel);
            let credentials = load_gateway_credentials(&auth_key)?;
            Some(credentials.ok_or_else(|| {
                anyhow::anyhow!(
                    "gateway channel `{channel}` is enabled but auth entry `{auth_key}` is missing; run `duck gateway service start` to reconfigure it"
                )
            })?)
        };
        out.push(GatewayLaunchChannel {
            channel,
            config: channel_config.clone(),
            credentials,
        });
    }
    Ok(out)
}

pub fn launch_channels_need_stable_bind(channels: &[GatewayLaunchChannel]) -> bool {
    channels
        .iter()
        .any(|channel| channel_needs_stable_bind(&channel.channel, &channel.config))
}

pub fn config_needs_stable_bind(config: &GatewayConfig) -> bool {
    config
        .enabled_channels()
        .any(|(channel, channel_config)| channel_needs_stable_bind(channel, channel_config))
}

fn channel_needs_stable_bind(channel: &str, config: &GatewayChannelConfig) -> bool {
    match normalize_channel_name(channel).as_str() {
        WEBHOOK_CHANNEL
        | API_SERVER_CHANNEL
        | WECOM_CALLBACK_CHANNEL
        | BLUEBUBBLES_CHANNEL
        | IMESSAGE_CHANNEL
        | SMS_CHANNEL
        | MSGRAPH_WEBHOOK_CHANNEL
        | MSTEAMS_CHANNEL
        | LINE_CHANNEL
        | NEXTCLOUD_TALK_CHANNEL
        | QA_CHANNEL
        | VOICE_CALL_CHANNEL
        | TALK_VOICE_CHANNEL
        | SYNOLOGY_CHAT_CHANNEL
        | TLON_CHANNEL
        | ZALO_CHANNEL
        | ZALOUSER_CHANNEL => true,
        SLACK_CHANNEL => config
            .transport
            .as_deref()
            .is_some_and(|transport| matches!(transport, "events_api" | "http" | "webhook")),
        MATTERMOST_CHANNEL => config
            .extra
            .get("public_url")
            .or_else(|| config.extra.get("approval_callback_url"))
            .is_some_and(|value| !value.trim().is_empty()),
        DINGTALK_CHANNEL => {
            let transport = config.transport.as_deref().unwrap_or("stream");
            matches!(
                transport,
                "event_callback" | "callback" | "webhook" | "http" | "http_callback"
            )
        }
        NOSTR_CHANNEL => {
            let transport = config.transport.as_deref().unwrap_or("relay");
            matches!(
                transport,
                "nostr_bridge" | "bridge" | "webhook" | "http" | "http_callback" | "callback"
            )
        }
        WHATSAPP_CHANNEL => {
            let transport = config.transport.as_deref().unwrap_or("cloud_api");
            !matches!(
                transport,
                "bridge"
                    | "bridge_http"
                    | "whatsapp_bridge"
                    | "managed_bridge"
                    | "external_bridge"
                    | "baileys"
                    | "whatsapp_web"
                    | "web"
            )
        }
        EMAIL_CHANNEL => {
            let transport = config.transport.as_deref().unwrap_or("direct_imap_smtp");
            matches!(
                transport,
                "provider_http" | "webhook" | "http" | "http_callback" | "callback"
            )
        }
        WECOM_CHANNEL => config.transport.as_deref().is_some_and(|transport| {
            matches!(
                transport,
                "encrypted_callback" | "event_callback" | "callback"
            )
        }),
        GOOGLECHAT_CHANNEL => {
            let transport_needs_callback = config.transport.as_deref().is_some_and(|transport| {
                matches!(
                    transport,
                    "webhook" | "http" | "http_callback" | "app_event_callback" | "callback"
                )
            });
            let extra_needs_callback = config
                .extra
                .get("app_event_callback_enabled")
                .or_else(|| config.extra.get("webhook_enabled"))
                .or_else(|| config.extra.get("http_callback_enabled"))
                .is_some_and(|value| matches!(value.as_str(), "true" | "1" | "yes"));
            transport_needs_callback || extra_needs_callback
        }
        YUANBAO_CHANNEL => {
            let transport = config.transport.as_deref().unwrap_or("direct_websocket");
            matches!(
                transport,
                "yuanbao_bridge" | "bridge" | "webhook" | "http" | "http_callback" | "callback"
            )
        }
        QQBOT_CHANNEL => {
            let transport = config.transport.as_deref().unwrap_or("direct_gateway");
            matches!(
                transport,
                "qqbot_bridge" | "bridge" | "webhook" | "http" | "http_callback" | "callback"
            )
        }
        HOMEASSISTANT_CHANNEL => config
            .extra
            .get("command_webhook_enabled")
            .or_else(|| config.extra.get("insecure_webhook"))
            .is_some_and(|value| matches!(value.as_str(), "true" | "1" | "yes")),
        FEISHU_CHANNEL | LARK_CHANNEL | FEISHU_COMMENT_CHANNEL | LARK_COMMENT_CHANNEL => config
            .transport
            .as_deref()
            .is_some_and(|transport| transport == "webhook" || transport == "event_callback"),
        _ => false,
    }
}

pub fn normalize_channel_name(name: &str) -> String {
    match name.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "api_server" => "api_server".to_string(),
        "feishu_comment" => "feishu_comment".to_string(),
        "lark_comment" => "lark_comment".to_string(),
        "wecom_callback" => "wecom_callback".to_string(),
        "msgraph_webhook" => "msgraph_webhook".to_string(),
        "imsg" => "imessage".to_string(),
        "blue_bubbles" => "bluebubbles".to_string(),
        "google_chat" => "googlechat".to_string(),
        "teams" => "msteams".to_string(),
        "microsoft_teams" => "msteams".to_string(),
        "nextcloud_talk" => "nextcloud-talk".to_string(),
        "synology_chat" => "synology-chat".to_string(),
        "qa_channel" => "qa-channel".to_string(),
        "voice_call" => "voice-call".to_string(),
        "talk_voice" => "talk-voice".to_string(),
        "home_assistant" => "homeassistant".to_string(),
        other => other.replace('_', "-"),
    }
}

fn default_true() -> bool {
    true
}

fn default_typing_refresh_seconds() -> u64 {
    4
}

fn default_media_max_download_bytes() -> u64 {
    25 * 1024 * 1024
}

fn default_approval_mode() -> String {
    "native_with_command_fallback".to_string()
}

fn default_dm_access_policy() -> String {
    "open".to_string()
}

fn default_group_access_policy() -> String {
    "open".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::config::DuckAgentConfig;
    use serde_json::json;

    #[test]
    fn set_gateway_config_preserves_unknown_fields() -> Result<()> {
        let mut config = DuckAgentConfig::from_str(
            r#"{
              "provider": "openai",
              "mcpServers": {"docs": {"url": "https://example.com/mcp"}}
            }"#,
        )?;
        let mut gateway = GatewayConfig {
            bind: Some(DEFAULT_GATEWAY_BIND.to_string()),
            ..Default::default()
        };
        gateway.channels.insert(
            "webhook".to_string(),
            GatewayChannelConfig {
                enabled: true,
                ..Default::default()
            },
        );

        set_gateway_config(&mut config, &gateway)?;

        assert_eq!(config.raw()["provider"], json!("openai"));
        assert!(config.raw().contains_key("mcpServers"));
        assert_eq!(config.raw()["gateway"]["bind"], json!(DEFAULT_GATEWAY_BIND));
        assert_eq!(
            config.raw()["gateway"]["channels"]["webhook"]["enabled"],
            json!(true)
        );
        Ok(())
    }

    #[test]
    fn parses_gateway_config_defaults() -> Result<()> {
        let config: GatewayConfig = serde_json::from_value(json!({
            "channels": {
                "telegram": {}
            }
        }))?;
        let telegram = &config.channels["telegram"];
        assert!(telegram.enabled);
        assert_eq!(telegram.auth_key_for("telegram"), "telegram");
        assert_eq!(telegram.typing.refresh_seconds, 4);
        assert_eq!(telegram.media.max_download_bytes, 25 * 1024 * 1024);
        Ok(())
    }

    #[test]
    fn stable_bind_is_only_required_for_http_callback_channels() {
        let telegram = GatewayLaunchChannel {
            channel: TELEGRAM_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("polling".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(!launch_channels_need_stable_bind(&[telegram]));

        let lark_ws = GatewayLaunchChannel {
            channel: LARK_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("websocket".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(!launch_channels_need_stable_bind(&[lark_ws]));

        let lark_webhook = GatewayLaunchChannel {
            channel: LARK_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("webhook".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(launch_channels_need_stable_bind(&[lark_webhook]));

        let api_server = GatewayLaunchChannel {
            channel: API_SERVER_CHANNEL.to_string(),
            config: GatewayChannelConfig::default(),
            credentials: None,
        };
        assert!(launch_channels_need_stable_bind(&[api_server]));

        let slack_socket = GatewayLaunchChannel {
            channel: SLACK_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("socket_mode".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(!launch_channels_need_stable_bind(&[slack_socket]));

        let slack_http = GatewayLaunchChannel {
            channel: SLACK_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("events_api".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(launch_channels_need_stable_bind(&[slack_http]));

        let mattermost_with_actions = GatewayLaunchChannel {
            channel: MATTERMOST_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                extra: BTreeMap::from([(
                    "public_url".to_string(),
                    "https://gateway.example.com".to_string(),
                )]),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(launch_channels_need_stable_bind(&[mattermost_with_actions]));

        let bluebubbles = GatewayLaunchChannel {
            channel: BLUEBUBBLES_CHANNEL.to_string(),
            config: GatewayChannelConfig::default(),
            credentials: None,
        };
        assert!(launch_channels_need_stable_bind(&[bluebubbles]));

        let nextcloud_bridge = GatewayLaunchChannel {
            channel: NEXTCLOUD_TALK_CHANNEL.to_string(),
            config: GatewayChannelConfig::default(),
            credentials: None,
        };
        assert!(launch_channels_need_stable_bind(&[nextcloud_bridge]));

        let wecom = GatewayLaunchChannel {
            channel: WECOM_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("aibot_websocket".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(!launch_channels_need_stable_bind(&[wecom]));

        let wecom_callback = GatewayLaunchChannel {
            channel: WECOM_CALLBACK_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("encrypted_callback".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(launch_channels_need_stable_bind(&[wecom_callback]));

        let googlechat_pubsub = GatewayLaunchChannel {
            channel: GOOGLECHAT_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("pubsub_rest".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(!launch_channels_need_stable_bind(&[googlechat_pubsub]));

        let googlechat_callback = GatewayLaunchChannel {
            channel: GOOGLECHAT_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("app_event_callback".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(launch_channels_need_stable_bind(&[googlechat_callback]));

        let whatsapp_default = GatewayLaunchChannel {
            channel: WHATSAPP_CHANNEL.to_string(),
            config: GatewayChannelConfig::default(),
            credentials: None,
        };
        assert!(launch_channels_need_stable_bind(&[whatsapp_default]));

        let whatsapp_bridge = GatewayLaunchChannel {
            channel: WHATSAPP_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("bridge_http".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(!launch_channels_need_stable_bind(&[whatsapp_bridge]));

        let yuanbao_default = GatewayLaunchChannel {
            channel: YUANBAO_CHANNEL.to_string(),
            config: GatewayChannelConfig::default(),
            credentials: None,
        };
        assert!(!launch_channels_need_stable_bind(&[yuanbao_default]));

        let yuanbao_direct = GatewayLaunchChannel {
            channel: YUANBAO_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("direct_websocket".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(!launch_channels_need_stable_bind(&[yuanbao_direct]));

        let yuanbao_bridge = GatewayLaunchChannel {
            channel: YUANBAO_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("yuanbao_bridge".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(launch_channels_need_stable_bind(&[yuanbao_bridge]));

        let qqbot_default = GatewayLaunchChannel {
            channel: QQBOT_CHANNEL.to_string(),
            config: GatewayChannelConfig::default(),
            credentials: None,
        };
        assert!(!launch_channels_need_stable_bind(&[qqbot_default]));

        let qqbot_direct = GatewayLaunchChannel {
            channel: QQBOT_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("direct_gateway".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(!launch_channels_need_stable_bind(&[qqbot_direct]));

        let qqbot_bridge = GatewayLaunchChannel {
            channel: QQBOT_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("qqbot_bridge".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(launch_channels_need_stable_bind(&[qqbot_bridge]));

        let email_default = GatewayLaunchChannel {
            channel: EMAIL_CHANNEL.to_string(),
            config: GatewayChannelConfig::default(),
            credentials: None,
        };
        assert!(!launch_channels_need_stable_bind(&[email_default]));

        let email_direct = GatewayLaunchChannel {
            channel: EMAIL_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("direct_imap_smtp".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(!launch_channels_need_stable_bind(&[email_direct]));

        let email_provider = GatewayLaunchChannel {
            channel: EMAIL_CHANNEL.to_string(),
            config: GatewayChannelConfig {
                transport: Some("provider_http".to_string()),
                ..Default::default()
            },
            credentials: None,
        };
        assert!(launch_channels_need_stable_bind(&[email_provider]));
    }

    #[test]
    fn config_stable_bind_ignores_weixin_polling() {
        let mut config = GatewayConfig::default();
        config.channels.insert(
            WEIXIN_CHANNEL.to_string(),
            GatewayChannelConfig {
                enabled: true,
                transport: Some("ilink_polling".to_string()),
                ..Default::default()
            },
        );
        assert!(!config_needs_stable_bind(&config));

        config.channels.insert(
            API_SERVER_CHANNEL.to_string(),
            GatewayChannelConfig {
                enabled: true,
                ..Default::default()
            },
        );
        assert!(config_needs_stable_bind(&config));
    }

    #[test]
    fn gateway_channel_config_serializes_without_instance_fields() -> Result<()> {
        let value = serde_json::to_value(GatewayChannelConfig {
            transport: Some("polling".to_string()),
            ..Default::default()
        })?;
        assert_eq!(value["transport"], json!("polling"));
        assert!(value.get("account_id").is_none());
        assert!(value.get("credential_id").is_none());
        Ok(())
    }

    #[test]
    fn canonicalizes_common_aliases() {
        assert_eq!(normalize_channel_name("api-server"), "api_server");
        assert_eq!(normalize_channel_name("api_server"), "api_server");
        assert_eq!(normalize_channel_name("lark_comment"), "lark_comment");
        assert_eq!(normalize_channel_name("google_chat"), "googlechat");
        assert_eq!(normalize_channel_name("teams"), "msteams");
        assert_eq!(normalize_channel_name("msgraph_webhook"), "msgraph_webhook");
        assert_eq!(normalize_channel_name("microsoft-teams"), "msteams");
        assert_eq!(normalize_channel_name("nextcloud_talk"), "nextcloud-talk");
        assert_eq!(normalize_channel_name("synology_chat"), "synology-chat");
        assert_eq!(normalize_channel_name("qa_channel"), "qa-channel");
        assert_eq!(normalize_channel_name("voice_call"), "voice-call");
        assert_eq!(normalize_channel_name("home_assistant"), "homeassistant");
        assert_eq!(normalize_channel_name("telegram"), "telegram");
    }
}
