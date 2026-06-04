use super::config::{
    API_SERVER_CHANNEL, BLUEBUBBLES_CHANNEL, DEFAULT_GATEWAY_BIND, DEFAULT_GATEWAY_CONFIGURED_BIND,
    DINGTALK_CHANNEL, DISCORD_CHANNEL, EMAIL_CHANNEL, FEISHU_CHANNEL, FEISHU_COMMENT_CHANNEL,
    GOOGLECHAT_CHANNEL, GatewayAccessConfig, GatewayChannelConfig, GatewayConfig,
    HOMEASSISTANT_CHANNEL, IMESSAGE_CHANNEL, IRC_CHANNEL, LARK_CHANNEL, LARK_COMMENT_CHANNEL,
    LINE_CHANNEL, MATRIX_CHANNEL, MATTERMOST_CHANNEL, MSGRAPH_WEBHOOK_CHANNEL, MSTEAMS_CHANNEL,
    NEXTCLOUD_TALK_CHANNEL, NOSTR_CHANNEL, QA_CHANNEL, QQBOT_CHANNEL, SIGNAL_CHANNEL,
    SLACK_CHANNEL, SMS_CHANNEL, SYNOLOGY_CHAT_CHANNEL, TALK_VOICE_CHANNEL, TELEGRAM_CHANNEL,
    TLON_CHANNEL, TWITCH_CHANNEL, VOICE_CALL_CHANNEL, WEBHOOK_CHANNEL, WECOM_CALLBACK_CHANNEL,
    WECOM_CHANNEL, WEIXIN_CHANNEL, WHATSAPP_CHANNEL, YUANBAO_CHANNEL, ZALO_CHANNEL,
    ZALOUSER_CHANNEL, config_needs_stable_bind, load_gateway_config, save_gateway_config,
};
use super::pairing::GatewayPairingStore;
use crate::auth::{GatewayCredentialEntry, remove_gateway_credentials, save_gateway_credentials};
use crate::setup::{
    GATEWAY_SETUP_FLOW, PickerItem, PickerManageAction, SETUP_QR_DENSE_ROW_PREFIX, SetupAction,
    prompt_confirm_with_flow, prompt_text_with_flow, run_edit_picker_with_flow,
    run_picker_with_flow, show_setup_message_with_flow, wait_setup_display_task_with_flow,
    wait_setup_task_with_flow,
};
use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const FEISHU_REGISTRATION_PATH: &str = "/oauth/v1/app/registration";
const FEISHU_ONBOARD_TIMEOUT_SECS: u64 = 600;
const FEISHU_ONBOARD_REQUEST_TIMEOUT_SECS: u64 = 10;
const SLACK_API_BASE: &str = "https://slack.com/api";
const TELEGRAM_API_BASE: &str = "https://api.telegram.org";
const WEIXIN_ILINK_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const WEIXIN_ILINK_QR_TIMEOUT_SECS: u64 = 480;
const WEIXIN_ILINK_REQUEST_TIMEOUT_SECS: u64 = 35;
const WEIXIN_ILINK_APP_ID: &str = "bot";
const WEIXIN_ILINK_APP_CLIENT_VERSION: &str = "131584";
const WEIXIN_ILINK_BOT_TYPE: &str = "3";

struct GatewayChannelSetupDescriptor {
    id: &'static str,
    title: &'static str,
    detail: &'static str,
}

#[derive(Debug, Clone)]
struct WeixinCredentialSetup {
    account_id: Option<String>,
    token: String,
    api_base: String,
    user_id: Option<String>,
}

#[derive(Debug, Clone)]
struct WeixinQrBegin {
    qrcode: String,
    qr_url: Option<String>,
}

struct SlackSetupIdentity {
    user_id: Option<String>,
    team_id: Option<String>,
    user_name: Option<String>,
}

struct DiscordSetupIdentity {
    user_id: Option<String>,
    username: Option<String>,
}

struct MattermostSetupIdentity {
    user_id: Option<String>,
    username: Option<String>,
}

enum GatewayChannelManagerAction {
    Add,
    Configure(String),
    Delete(String),
    CannotDeleteAdd,
}

const CONFIGURABLE_CHANNELS: &[GatewayChannelSetupDescriptor] = &[
    GatewayChannelSetupDescriptor {
        id: TELEGRAM_CHANNEL,
        title: "Telegram",
        detail: "Telegram Bot API polling, media, typing, and inline approvals",
    },
    GatewayChannelSetupDescriptor {
        id: SLACK_CHANNEL,
        title: "Slack",
        detail: "Slack Socket Mode, files, threads, and Block Kit approvals",
    },
    GatewayChannelSetupDescriptor {
        id: SIGNAL_CHANNEL,
        title: "Signal",
        detail: "signal-cli HTTP daemon, SSE inbound, media, typing, and approvals",
    },
    GatewayChannelSetupDescriptor {
        id: MATRIX_CHANNEL,
        title: "Matrix",
        detail: "Matrix sync, rooms, media, typing, and approval fallback",
    },
    GatewayChannelSetupDescriptor {
        id: FEISHU_CHANNEL,
        title: "Feishu",
        detail: "Feishu China WebSocket, media, and card approvals",
    },
    GatewayChannelSetupDescriptor {
        id: LARK_CHANNEL,
        title: "Lark",
        detail: "Lark global WebSocket, media, and card approvals",
    },
    GatewayChannelSetupDescriptor {
        id: FEISHU_COMMENT_CHANNEL,
        title: "Feishu Comment",
        detail: "Feishu Drive comment WebSocket/callback events with mention and allowlist rules",
    },
    GatewayChannelSetupDescriptor {
        id: LARK_COMMENT_CHANNEL,
        title: "Lark Comment",
        detail: "Lark Drive comment WebSocket/callback events with independent namespace and rules",
    },
    GatewayChannelSetupDescriptor {
        id: DISCORD_CHANNEL,
        title: "Discord",
        detail: "Discord Gateway websocket, REST media, typing, and components approvals",
    },
    GatewayChannelSetupDescriptor {
        id: MATTERMOST_CHANNEL,
        title: "Mattermost",
        detail: "Mattermost REST/websocket, files, typing, and approvals",
    },
    GatewayChannelSetupDescriptor {
        id: API_SERVER_CHANNEL,
        title: "API Server",
        detail: "OpenAI-compatible chat completions and models endpoints",
    },
    GatewayChannelSetupDescriptor {
        id: WHATSAPP_CHANNEL,
        title: "WhatsApp",
        detail: "WhatsApp Cloud API webhook/Graph API, media, and approval fallback; Web bridge optional",
    },
    GatewayChannelSetupDescriptor {
        id: DINGTALK_CHANNEL,
        title: "DingTalk",
        detail: "DingTalk Stream Mode WebSocket, session webhook replies, media, and approvals",
    },
    GatewayChannelSetupDescriptor {
        id: WECOM_CHANNEL,
        title: "WeCom",
        detail: "Enterprise WeChat AI Bot WebSocket, media, group routing, and approvals",
    },
    GatewayChannelSetupDescriptor {
        id: WECOM_CALLBACK_CHANNEL,
        title: "WeCom Callback",
        detail: "Enterprise WeChat encrypted callback alias with independent namespace",
    },
    GatewayChannelSetupDescriptor {
        id: WEIXIN_CHANNEL,
        title: "Weixin",
        detail: "WeChat iLink Bot polling, text routing, typing, and media notices",
    },
    GatewayChannelSetupDescriptor {
        id: BLUEBUBBLES_CHANNEL,
        title: "BlueBubbles",
        detail: "BlueBubbles server webhook/REST for iMessage text, media, and typing",
    },
    GatewayChannelSetupDescriptor {
        id: IMESSAGE_CHANNEL,
        title: "iMessage",
        detail: "iMessage alias over BlueBubbles with independent session namespace",
    },
    GatewayChannelSetupDescriptor {
        id: EMAIL_CHANNEL,
        title: "Email",
        detail: "Email IMAP polling, SMTP replies, attachments, and reply threading",
    },
    GatewayChannelSetupDescriptor {
        id: SMS_CHANNEL,
        title: "SMS",
        detail: "Twilio-compatible SMS/MMS webhook and REST sender",
    },
    GatewayChannelSetupDescriptor {
        id: MSGRAPH_WEBHOOK_CHANNEL,
        title: "MS Graph Webhook",
        detail: "Microsoft Graph webhook validation, notification routing, and bridge replies",
    },
    GatewayChannelSetupDescriptor {
        id: MSTEAMS_CHANNEL,
        title: "Teams / MSTeams",
        detail: "Microsoft Teams Bot Framework messages, threads, typing, and Adaptive Card approvals",
    },
    GatewayChannelSetupDescriptor {
        id: GOOGLECHAT_CHANNEL,
        title: "Google Chat",
        detail: "Google Chat Pub/Sub spaces, threads, card approvals, and REST replies",
    },
    GatewayChannelSetupDescriptor {
        id: LINE_CHANNEL,
        title: "LINE",
        detail: "LINE Messaging API reply/push delivery, media intake, typing, and template approvals",
    },
    GatewayChannelSetupDescriptor {
        id: IRC_CHANNEL,
        title: "IRC",
        detail: "IRC server/channel text chat over TCP/TLS with mention gating",
    },
    GatewayChannelSetupDescriptor {
        id: NEXTCLOUD_TALK_CHANNEL,
        title: "Nextcloud Talk",
        detail: "Nextcloud Talk webhook bot with direct OCS replies, rooms, attachments, and approvals",
    },
    GatewayChannelSetupDescriptor {
        id: NOSTR_CHANNEL,
        title: "Nostr",
        detail: "Nostr relay direct messages, media links, and approval fallback",
    },
    GatewayChannelSetupDescriptor {
        id: SYNOLOGY_CHAT_CHANNEL,
        title: "Synology Chat",
        detail: "Synology Chat incoming/outgoing webhooks for channels, media, and replies",
    },
    GatewayChannelSetupDescriptor {
        id: TLON_CHANNEL,
        title: "Tlon",
        detail: "External Tlon/Urbit bridge API plus local inbound endpoint for routing, media, and replies",
    },
    GatewayChannelSetupDescriptor {
        id: TWITCH_CHANNEL,
        title: "Twitch",
        detail: "Twitch chat over IRC with OAuth, channel routing, and mention gating",
    },
    GatewayChannelSetupDescriptor {
        id: ZALO_CHANNEL,
        title: "Zalo",
        detail: "External Zalo OA/business bridge API plus local inbound endpoint for text, media, and replies",
    },
    GatewayChannelSetupDescriptor {
        id: ZALOUSER_CHANNEL,
        title: "Zalo User",
        detail: "External Zalo user bridge API plus independent session namespace",
    },
    GatewayChannelSetupDescriptor {
        id: HOMEASSISTANT_CHANNEL,
        title: "Home Assistant",
        detail: "Home Assistant WebSocket state changes, REST notify, and optional command webhook",
    },
    GatewayChannelSetupDescriptor {
        id: QQBOT_CHANNEL,
        title: "QQ Bot",
        detail: "QQ Bot official Gateway WebSocket, REST replies, policy, and approvals",
    },
    GatewayChannelSetupDescriptor {
        id: YUANBAO_CHANNEL,
        title: "Yuanbao",
        detail: "Yuanbao direct WebSocket/proto, media notes, policy, and replies",
    },
    GatewayChannelSetupDescriptor {
        id: VOICE_CALL_CHANNEL,
        title: "Voice Call",
        detail: "External voice bridge API plus local inbound endpoint for calls, transcripts, audio, and replies",
    },
    GatewayChannelSetupDescriptor {
        id: TALK_VOICE_CHANNEL,
        title: "Talk Voice",
        detail: "External talk-voice bridge API plus local inbound endpoint for audio conversations and replies",
    },
];

fn run_picker(
    title: &str,
    subtitle: &str,
    items: &[PickerItem],
    allow_back: bool,
) -> Result<SetupAction<usize>> {
    run_picker_with_flow(GATEWAY_SETUP_FLOW, title, subtitle, items, allow_back)
}

fn prompt_text(
    title: &str,
    subtitle: &str,
    placeholder: &str,
    initial: Option<&str>,
    required: bool,
    allow_back: bool,
    mask_input: bool,
) -> Result<SetupAction<String>> {
    prompt_text_with_flow(
        GATEWAY_SETUP_FLOW,
        title,
        subtitle,
        placeholder,
        initial,
        required,
        allow_back,
        mask_input,
    )
}

fn prompt_confirm(title: &str, lines: &[String], allow_back: bool) -> Result<SetupAction<()>> {
    prompt_confirm_with_flow(GATEWAY_SETUP_FLOW, title, lines, allow_back)
}

fn show_setup_message(title: &str, subtitle: &str) -> Result<()> {
    show_setup_message_with_flow(GATEWAY_SETUP_FLOW, title, subtitle)
}

pub(in crate::gateway) fn run_gateway_setup() -> Result<bool> {
    let channel = match prompt_gateway_channel(false)? {
        SetupAction::Submit(channel) => channel,
        SetupAction::Back => unreachable!("gateway channel picker does not allow back"),
    };
    run_gateway_setup_for_channel(channel)
}

fn run_gateway_setup_for_channel(channel: &str) -> Result<bool> {
    match channel {
        WEBHOOK_CHANNEL => run_webhook_setup(),
        TELEGRAM_CHANNEL => run_telegram_setup(),
        SLACK_CHANNEL => run_slack_setup(),
        SIGNAL_CHANNEL => run_signal_setup(),
        MATRIX_CHANNEL => run_matrix_setup(),
        FEISHU_CHANNEL => run_feishu_setup(FEISHU_CHANNEL, "https://open.feishu.cn"),
        LARK_CHANNEL => run_feishu_setup(LARK_CHANNEL, "https://open.larksuite.com"),
        FEISHU_COMMENT_CHANNEL => {
            run_feishu_comment_setup(FEISHU_COMMENT_CHANNEL, "https://open.feishu.cn")
        }
        LARK_COMMENT_CHANNEL => {
            run_feishu_comment_setup(LARK_COMMENT_CHANNEL, "https://open.larksuite.com")
        }
        DISCORD_CHANNEL => run_discord_setup(),
        MATTERMOST_CHANNEL => run_mattermost_setup(),
        API_SERVER_CHANNEL => run_api_server_setup(),
        WHATSAPP_CHANNEL => run_whatsapp_setup(),
        DINGTALK_CHANNEL => run_dingtalk_setup(),
        WECOM_CHANNEL => run_wecom_setup(WECOM_CHANNEL),
        WECOM_CALLBACK_CHANNEL => run_wecom_setup(WECOM_CALLBACK_CHANNEL),
        WEIXIN_CHANNEL => run_weixin_setup(),
        BLUEBUBBLES_CHANNEL => run_bluebubbles_setup(BLUEBUBBLES_CHANNEL),
        IMESSAGE_CHANNEL => run_bluebubbles_setup(IMESSAGE_CHANNEL),
        EMAIL_CHANNEL => run_email_setup(),
        SMS_CHANNEL => run_sms_setup(),
        MSGRAPH_WEBHOOK_CHANNEL => run_msgraph_webhook_setup(),
        MSTEAMS_CHANNEL => run_msteams_setup(),
        GOOGLECHAT_CHANNEL => run_googlechat_setup(),
        LINE_CHANNEL => run_line_setup(),
        IRC_CHANNEL => run_irc_setup(),
        NEXTCLOUD_TALK_CHANNEL => run_nextcloud_talk_setup(),
        NOSTR_CHANNEL => run_nostr_setup(),
        SYNOLOGY_CHAT_CHANNEL => run_synology_chat_setup(),
        TLON_CHANNEL => run_tlon_setup(),
        TWITCH_CHANNEL => run_twitch_setup(),
        ZALO_CHANNEL => run_zalo_setup(ZALO_CHANNEL),
        ZALOUSER_CHANNEL => run_zalo_setup(ZALOUSER_CHANNEL),
        HOMEASSISTANT_CHANNEL => run_homeassistant_setup(),
        QQBOT_CHANNEL => run_qqbot_setup(),
        YUANBAO_CHANNEL => run_yuanbao_setup(),
        QA_CHANNEL => run_qa_channel_setup(),
        VOICE_CALL_CHANNEL => run_voice_bridge_setup(VOICE_CALL_CHANNEL),
        TALK_VOICE_CHANNEL => run_voice_bridge_setup(TALK_VOICE_CHANNEL),
        other => Err(anyhow!("unsupported gateway setup channel `{other}`")),
    }
}

pub(in crate::gateway) fn run_gateway_channels_manager() -> Result<bool> {
    let mut changed = false;
    loop {
        let config = load_gateway_config().unwrap_or_default();
        let visible_channels = configured_visible_channel_ids(&config);
        let action = match prompt_gateway_channels_manager_action(&config, &visible_channels)? {
            SetupAction::Submit(action) => action,
            SetupAction::Back => return Ok(changed),
        };
        match action {
            GatewayChannelManagerAction::Add => {
                let channel = match prompt_gateway_channel(true)? {
                    SetupAction::Submit(channel) => channel,
                    SetupAction::Back => continue,
                };
                if run_gateway_setup_for_channel(channel)? {
                    changed = true;
                }
            }
            GatewayChannelManagerAction::Configure(channel) => {
                if run_gateway_setup_for_channel(&channel)? {
                    changed = true;
                }
            }
            GatewayChannelManagerAction::Delete(channel) => {
                if remove_gateway_channel_from_manager(&channel)? {
                    changed = true;
                }
            }
            GatewayChannelManagerAction::CannotDeleteAdd => {
                show_setup_message("Cannot delete", "Select a configured channel first.")?;
            }
        }
    }
}

fn prompt_gateway_channels_manager_action(
    config: &GatewayConfig,
    visible_channels: &[String],
) -> Result<SetupAction<GatewayChannelManagerAction>> {
    let items = gateway_channel_manager_items(config, visible_channels);
    let action = run_edit_picker_with_flow(
        GATEWAY_SETUP_FLOW,
        "Gateway channels",
        "Enter adds or reconfigures a channel. Delete removes a configured channel.",
        &items,
    );
    match action? {
        PickerManageAction::Back => Ok(SetupAction::Back),
        PickerManageAction::Submit(0) => Ok(SetupAction::Submit(GatewayChannelManagerAction::Add)),
        PickerManageAction::Submit(index) => {
            let channel = visible_channels
                .get(index.saturating_sub(1))
                .cloned()
                .ok_or_else(|| anyhow!("invalid gateway channel manager selection"))?;
            Ok(SetupAction::Submit(GatewayChannelManagerAction::Configure(
                channel,
            )))
        }
        PickerManageAction::Delete(0) => Ok(SetupAction::Submit(
            GatewayChannelManagerAction::CannotDeleteAdd,
        )),
        PickerManageAction::Delete(index) => {
            let channel = visible_channels
                .get(index.saturating_sub(1))
                .cloned()
                .ok_or_else(|| anyhow!("invalid gateway channel delete selection"))?;
            Ok(SetupAction::Submit(GatewayChannelManagerAction::Delete(
                channel,
            )))
        }
    }
}

fn gateway_channel_manager_items(
    config: &GatewayConfig,
    visible_channels: &[String],
) -> Vec<PickerItem> {
    let mut items = Vec::with_capacity(visible_channels.len() + 1);
    items.push(PickerItem {
        title: "Add Channel".to_string(),
        detail: "Configure another gateway channel".to_string(),
        model_columns: None,
    });
    items.extend(visible_channels.iter().map(|channel| {
        let descriptor = channel_descriptor(channel);
        let title = descriptor
            .map(|descriptor| descriptor.title.to_string())
            .unwrap_or_else(|| channel.to_string());
        let detail = config
            .channels
            .get(channel)
            .map(format_configured_channel_detail)
            .unwrap_or_else(|| "configured".to_string());
        PickerItem {
            title,
            detail,
            model_columns: None,
        }
    }));
    items
}

fn configured_visible_channel_ids(config: &GatewayConfig) -> Vec<String> {
    config
        .channels
        .keys()
        .filter(|channel| channel_descriptor(channel).is_some())
        .cloned()
        .collect()
}

fn channel_descriptor(channel: &str) -> Option<&'static GatewayChannelSetupDescriptor> {
    CONFIGURABLE_CHANNELS
        .iter()
        .find(|descriptor| descriptor.id == channel)
}

fn format_configured_channel_detail(config: &GatewayChannelConfig) -> String {
    let state = if config.enabled {
        "enabled"
    } else {
        "disabled"
    };
    let transport = config.transport.as_deref().unwrap_or("default");
    let scope = if config.allowed_users.is_empty() && config.allowed_chats.is_empty() {
        "open/default access".to_string()
    } else {
        format!(
            "{} users, {} chats",
            config.allowed_users.len(),
            config.allowed_chats.len()
        )
    };
    format!("{state}; transport: {transport}; {scope}")
}

fn remove_gateway_channel_from_manager(channel: &str) -> Result<bool> {
    let mut config = load_gateway_config().unwrap_or_default();
    let Some(channel_config) = config.channels.get(channel).cloned() else {
        show_setup_message(
            "Channel already removed",
            "The selected gateway channel is no longer configured.",
        )?;
        return Ok(false);
    };
    let label = channel_descriptor(channel)
        .map(|descriptor| descriptor.title)
        .unwrap_or(channel);
    let lines = vec![
        format!("Channel: {label}"),
        format!(
            "Transport: {}",
            channel_config.transport.as_deref().unwrap_or("default")
        ),
        "This deletes the channel configuration and its saved gateway credential.".to_string(),
    ];
    match prompt_confirm("Remove gateway channel", &lines, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return Ok(false),
    }
    let auth_key = channel_config.auth_key_for(channel);
    config.channels.remove(channel);
    if !config_needs_stable_bind(&config) {
        config.bind = None;
    }
    save_gateway_config(&config)?;
    remove_gateway_credentials(&auth_key)?;
    show_setup_message(
        "Gateway channel removed",
        &format!("{label} was removed from gateway channels."),
    )?;
    Ok(true)
}

fn run_webhook_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bind = default_stable_gateway_bind(current_config.bind.as_deref())?;
    match prompt_gateway_review(WEBHOOK_CHANNEL, &bind, "http")? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let mut next = current_config;
    next.bind = Some(bind.clone());
    next.channels.insert(
        WEBHOOK_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("http".to_string()),
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "Gateway configured",
        "Configuration saved. Gateway service startup will continue.",
    )?;
    Ok(true)
}

fn run_telegram_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let token =
        match prompt_gateway_secret("Telegram bot token", "Paste the token from BotFather.")? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return run_gateway_setup(),
        };
    let bot_username = telegram_setup_get_me(&token)?;
    let allowed_users = match prompt_telegram_allowed_users()? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_key = TELEGRAM_CHANNEL.to_string();
    match prompt_telegram_review(bot_username.as_deref(), &allowed_users)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    let mut credential_extra = BTreeMap::new();
    if let Some(username) = bot_username.clone() {
        credential_extra.insert("bot_username".to_string(), username);
    }
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: TELEGRAM_CHANNEL.to_string(),
            token: Some(token),
            username: bot_username,
            extra: credential_extra,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut next = current_config;
    next.channels.insert(
        TELEGRAM_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("polling".to_string()),
            access: telegram_default_access(),
            allowed_users,
            ..Default::default()
        },
    );
    if !config_needs_stable_bind(&next) {
        next.bind = None;
    }
    save_gateway_config(&next)?;
    show_setup_message(
        "Telegram gateway configured",
        "Configuration saved. Telegram DMs are restricted to the allowed user IDs.",
    )?;
    Ok(true)
}

fn telegram_default_access() -> GatewayAccessConfig {
    GatewayAccessConfig {
        dm_policy: "allowlist".to_string(),
        group_policy: "allowlist".to_string(),
        ..Default::default()
    }
}

fn prompt_telegram_allowed_users() -> Result<SetupAction<Vec<String>>> {
    loop {
        let input = match prompt_text(
            "Telegram allowed users",
            "In Telegram, open @userinfobot (not BotFather), send /start, then paste the numeric Id here. Use commas for multiple users.",
            "123456789",
            None,
            true,
            true,
            false,
        )? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return Ok(SetupAction::Back),
        };
        let allowed_users = parse_csv_list(&input);
        if !allowed_users.is_empty() && allowed_users.iter().all(|value| is_telegram_user_id(value))
        {
            return Ok(SetupAction::Submit(allowed_users));
        }
        show_setup_message(
            "Invalid Telegram user id",
            "Use numeric Telegram user IDs from @userinfobot, for example 123456789.",
        )?;
    }
}

fn is_telegram_user_id(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit())
}

fn prompt_telegram_review(
    bot_username: Option<&str>,
    allowed_users: &[String],
) -> Result<SetupAction<()>> {
    let bot = bot_username
        .map(|username| format!("@{username}"))
        .unwrap_or_else(|| "validated".to_string());
    let lines = vec![
        "Channel: telegram".to_string(),
        "Transport: polling".to_string(),
        format!("Bot: {bot}"),
        format!("Allowed users: {}", allowed_users.join(", ")),
        "DM access: allowlist".to_string(),
        "Group access: allowlist; add allowed_chats manually for trusted groups".to_string(),
    ];
    prompt_confirm("Review gateway", &lines, true)
}

fn telegram_setup_get_me(token: &str) -> Result<Option<String>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to build Telegram setup client")?;
    let value = telegram_setup_post_json(&client, token, "getMe", &json!({}))?;
    Ok(value["result"]["username"].as_str().map(str::to_string))
}

fn telegram_setup_post_json<T: serde::Serialize>(
    client: &Client,
    token: &str,
    method: &str,
    body: &T,
) -> Result<Value> {
    let response = client
        .post(format!(
            "{}/bot{}/{}",
            TELEGRAM_API_BASE,
            token.trim(),
            method
        ))
        .json(body)
        .send()
        .map_err(|error| {
            anyhow!(
                "telegram setup {method} request failed: {}",
                redact_telegram_token(&error.to_string(), token)
            )
        })?;
    let status = response.status();
    let value: Value = response
        .json()
        .with_context(|| format!("telegram setup {method} returned invalid JSON"))?;
    if !status.is_success() || value.get("ok") == Some(&Value::Bool(false)) {
        return Err(anyhow!(
            "telegram setup {method} failed with status {status}: {value}"
        ));
    }
    Ok(value)
}

fn redact_telegram_token(text: &str, token: &str) -> String {
    let token = token.trim();
    if token.is_empty() {
        return text.to_string();
    }
    text.replace(token, "<telegram-token>")
}

fn run_slack_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bot_token = match prompt_gateway_secret("Slack bot token", "Paste the xoxb bot token.")? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_string();
            if !value.starts_with("xoxb-") {
                bail!("Slack bot token must start with xoxb-");
            }
            value
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let bot_identity = slack_setup_auth_test(&bot_token).ok().flatten();
    let app_token = match prompt_gateway_secret(
        "Slack app token",
        "Paste the xapp token for Socket Mode. This avoids a public callback URL.",
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_string();
            if !value.starts_with("xapp-") {
                bail!("Slack app token must start with xapp-");
            }
            value
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let signing_secret = match prompt_optional_secret(
        "Slack signing secret",
        "Optional. Only needed for HTTP callbacks.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_chats = match prompt_optional_csv(
        "Allowed Slack channels",
        "Comma-separated channel ids. Empty or * allows any channel.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed Slack users",
        "Comma-separated user ids. Empty or * allows any user.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_key = SLACK_CHANNEL.to_string();
    let allowed_channels_label = if allowed_chats.is_empty() {
        "any channel where the app is installed; channel messages still require @mention"
            .to_string()
    } else {
        allowed_chats.join(", ")
    };
    let allowed_users_label = if allowed_users.is_empty() {
        "all Slack users in allowed conversations".to_string()
    } else {
        allowed_users.join(", ")
    };
    let mut review = vec![
        format!("Channel: {SLACK_CHANNEL}"),
        "Transport: socket_mode (no public callback URL)".to_string(),
        "Required app token: xapp-... with connections:write".to_string(),
        "Required bot events: app_mention, message.im, message.channels, message.groups".to_string(),
        "Required app settings: Socket Mode enabled, Interactivity enabled, App Home Messages tab enabled".to_string(),
        format!("Allowed channels: {allowed_channels_label}"),
        format!("Allowed users: {allowed_users_label}"),
        "Approval delivery: Block Kit buttons with /approve and /deny text fallback".to_string(),
        "Channel policy: DMs respond directly; channels require @mention unless configured as free-response".to_string(),
    ];
    if let Some(identity) = bot_identity.as_ref() {
        if let Some(team_id) = identity.team_id.as_deref() {
            review.push(format!("Workspace team: {team_id}"));
        }
        if let Some(user_id) = identity.user_id.as_deref() {
            review.push(format!("Bot user id: {user_id}"));
        }
    }
    if signing_secret.is_some() {
        review.push("HTTP callbacks/interactions: signing secret saved for optional advanced compatibility; Socket Mode still does not retain a local callback bind".to_string());
    }
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    let mut extra = BTreeMap::new();
    extra.insert("app_token".to_string(), app_token);
    if let Some(identity) = bot_identity {
        if let Some(user_id) = identity.user_id {
            extra.insert("bot_user_id".to_string(), user_id);
        }
        if let Some(team_id) = identity.team_id {
            extra.insert("team_id".to_string(), team_id);
        }
        if let Some(user_name) = identity.user_name {
            extra.insert("bot_name".to_string(), user_name);
        }
    }
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: SLACK_CHANNEL.to_string(),
            token: Some(bot_token),
            signing_secret,
            extra,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut next = current_config;
    let mut channel_extra = BTreeMap::new();
    channel_extra.insert("require_mention".to_string(), "true".to_string());
    next.channels.insert(
        SLACK_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("socket_mode".to_string()),
            allowed_users,
            allowed_chats,
            extra: channel_extra,
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "Slack gateway configured",
        "Socket Mode is configured. Gateway service startup will continue.",
    )?;
    Ok(true)
}

fn slack_setup_auth_test(bot_token: &str) -> Result<Option<SlackSetupIdentity>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to build Slack setup HTTP client")?;
    let response = client
        .post(format!("{SLACK_API_BASE}/auth.test"))
        .bearer_auth(bot_token.trim())
        .send()
        .context("Slack auth.test request failed")?;
    let value: Value = response
        .json()
        .context("Slack auth.test returned invalid JSON")?;
    if value.get("ok") != Some(&Value::Bool(true)) {
        return Ok(None);
    }
    Ok(Some(SlackSetupIdentity {
        user_id: value
            .get("user_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        team_id: value
            .get("team_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        user_name: value
            .get("user")
            .and_then(Value::as_str)
            .map(str::to_string),
    }))
}

fn run_signal_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let http_url = match prompt_text(
        "Signal daemon URL",
        "signal-cli daemon HTTP endpoint. Start with: signal-cli daemon --http 127.0.0.1:8080",
        "http://127.0.0.1:8080",
        Some("http://127.0.0.1:8080"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let http_url = http_url.trim().trim_end_matches('/').to_string();
    if !(http_url.starts_with("http://") || http_url.starts_with("https://")) {
        show_setup_message(
            "Signal daemon URL required",
            "Use a full signal-cli daemon HTTP URL beginning with http:// or https://.",
        )?;
        return run_signal_setup();
    }
    let signal_account = match prompt_text(
        "Signal account",
        "Phone number or account identifier registered with signal-cli.",
        "+15550000000",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let signal_account = signal_account.trim().to_string();
    let dm_policy = match prompt_text(
        "Signal DM policy",
        "pairing, open, allowlist, or disabled. pairing asks unknown DM senders to get owner approval with a one-time code.",
        "pairing",
        Some("pairing"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase().replace('-', "_");
            if !matches!(
                value.as_str(),
                "pairing" | "open" | "allowlist" | "disabled"
            ) {
                bail!("Signal DM policy must be pairing, open, allowlist, or disabled");
            }
            value
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = if dm_policy == "allowlist" {
        match prompt_optional_csv(
            "Allowed Signal DM senders",
            "Comma-separated phone numbers or service ids. Required when DM policy is allowlist.",
        )? {
            SetupAction::Submit(value) => {
                if value.is_empty() {
                    bail!(
                        "Signal DM policy allowlist requires at least one sender phone number or service id"
                    );
                }
                value
            }
            SetupAction::Back => return run_gateway_setup(),
        }
    } else {
        Vec::new()
    };
    let allowed_chats = match prompt_optional_csv(
        "Allowed Signal groups",
        "Optional comma-separated group ids or group:<id>. Empty disables group chats; use * only for trusted all-group mode.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_key = SIGNAL_CHANNEL.to_string();
    let allowed_groups_label = if allowed_chats.is_empty() {
        "disabled; group messages are ignored until group ids are added".to_string()
    } else {
        allowed_chats.join(", ")
    };
    let allowed_dm_label = if allowed_users.is_empty() {
        match dm_policy.as_str() {
            "pairing" => "none; unknown DM senders receive a pairing code".to_string(),
            "open" => "all DM senders that can reach this Signal account".to_string(),
            "disabled" => "DM disabled".to_string(),
            _ => "none".to_string(),
        }
    } else {
        allowed_users.join(", ")
    };
    let group_access_policy = if allowed_chats.is_empty() {
        "disabled"
    } else if allowed_chats.iter().any(|value| value.trim() == "*") {
        "open"
    } else {
        "allowlist"
    };
    let review = vec![
        format!("Channel: {SIGNAL_CHANNEL}"),
        "Transport: signal-cli HTTP daemon SSE + JSON-RPC".to_string(),
        format!("Signal daemon URL: {http_url}"),
        format!("SSE inbound: {http_url}/api/v1/events?account=<Signal account>"),
        format!("JSON-RPC outbound: {http_url}/api/v1/rpc"),
        "Local callback port: not required; DuckAgent does not receive Signal webhooks".to_string(),
        "Signal account setup: signal-cli must already be linked/registered and daemonized for this account".to_string(),
        format!("Signal account: {signal_account}"),
        format!("DM policy: {dm_policy}"),
        format!("Allowed DM senders: {allowed_dm_label}"),
        format!("Allowed groups: {allowed_groups_label}"),
        "DuckAgent pairing: use `duck gateway pairing approve <code>` when DM policy is pairing".to_string(),
        "Approvals: text commands only; Signal has no native approval buttons".to_string(),
    ];
    match prompt_confirm("Review Signal gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: SIGNAL_CHANNEL.to_string(),
            username: Some(signal_account),
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut next = current_config;
    next.channels.insert(
        SIGNAL_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("http_daemon".to_string()),
            api_base: Some(http_url),
            allowed_users,
            allowed_chats,
            access: GatewayAccessConfig {
                dm_policy,
                group_policy: group_access_policy.to_string(),
                require_mention: false,
            },
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "Signal gateway configured",
        "Configuration saved. Ensure signal-cli daemon --http is running before serve.",
    )?;
    Ok(true)
}

fn run_matrix_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let homeserver = match prompt_text(
        "Matrix homeserver",
        "Homeserver base URL, for example https://matrix.example.org.",
        "https://matrix.example.org",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let homeserver = homeserver.trim().trim_end_matches('/').to_string();
    if !(homeserver.starts_with("https://") || homeserver.starts_with("http://")) {
        show_setup_message(
            "Matrix homeserver URL required",
            "Use a full Matrix homeserver URL beginning with https:// or http://.",
        )?;
        return run_matrix_setup();
    }
    let access_token =
        match prompt_gateway_secret("Matrix access token", "Paste a Matrix access token.")? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return run_gateway_setup(),
        };
    let user_id = match matrix_setup_whoami(&homeserver, &access_token)
        .ok()
        .flatten()
    {
        Some(user_id) => user_id,
        None => match prompt_text(
            "Matrix user id",
            "Full bot user id, for example @bot:example.org. Only needed when token validation cannot detect it.",
            "@bot:example.org",
            None,
            true,
            true,
            false,
        )? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return run_gateway_setup(),
        },
    };
    let user_id = user_id.trim().to_string();
    if !user_id.starts_with('@') || !user_id.contains(':') {
        show_setup_message(
            "Matrix user id required",
            "Use a full Matrix user id such as @bot:example.org.",
        )?;
        return run_matrix_setup();
    }
    let allowed_chats = match prompt_optional_csv(
        "Allowed Matrix rooms",
        "Optional comma-separated room ids. Empty allows any invited room, but rooms still require @mention by default.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let free_response_rooms = match prompt_optional_csv(
        "Free-response Matrix rooms",
        "Optional comma-separated room ids where the bot may respond without @mention. Empty keeps @mention gating.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed Matrix users",
        "Optional comma-separated user ids. Empty allows all users.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_key = MATRIX_CHANNEL.to_string();
    let allowed_rooms_label = if allowed_chats.is_empty() {
        "any room the bot account is invited to; non-DM rooms still require @mention".to_string()
    } else {
        allowed_chats.join(", ")
    };
    let allowed_users_label = if allowed_users.is_empty() {
        "all Matrix users in allowed rooms".to_string()
    } else {
        allowed_users.join(", ")
    };
    let free_response_label = if free_response_rooms.is_empty() {
        "none; non-DM rooms require @mention".to_string()
    } else {
        free_response_rooms.join(", ")
    };
    let review = vec![
        format!("Channel: {MATRIX_CHANNEL}"),
        "Transport: Matrix Client-Server /sync long-poll + REST send".to_string(),
        format!("Homeserver: {homeserver}"),
        format!("Whoami/token check: {homeserver}/_matrix/client/v3/account/whoami"),
        format!("Sync inbound: {homeserver}/_matrix/client/v3/sync"),
        "Local callback port: not required; DuckAgent does not receive Matrix webhooks".to_string(),
        format!("Bot user id: {user_id}"),
        format!("Allowed rooms: {allowed_rooms_label}"),
        format!("Allowed users: {allowed_users_label}"),
        format!("Free-response rooms: {free_response_label}"),
        "Room policy: DMs respond directly; normal rooms require @mention unless free-response".to_string(),
        "Thread policy: top-level room mentions start a Matrix thread; participated threads continue".to_string(),
        "Invite policy: room invites are auto-joined only when allowed by the room allowlist".to_string(),
        "E2EE note: encrypted rooms need separate device/key support and are not treated as plain messages".to_string(),
    ];
    match prompt_confirm("Review Matrix gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: MATRIX_CHANNEL.to_string(),
            token: Some(access_token),
            username: Some(user_id),
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut next = current_config;
    let mut extra = BTreeMap::new();
    extra.insert("require_mention".to_string(), "true".to_string());
    extra.insert("auto_thread".to_string(), "true".to_string());
    if !free_response_rooms.is_empty() {
        extra.insert(
            "free_response_rooms".to_string(),
            free_response_rooms.join(","),
        );
    }
    next.channels.insert(
        MATRIX_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("sync".to_string()),
            api_base: Some(homeserver),
            allowed_users,
            allowed_chats,
            extra,
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "Matrix gateway configured",
        "Configuration saved. Starting Matrix sync when the gateway service starts.",
    )?;
    Ok(true)
}

fn matrix_setup_whoami(homeserver: &str, access_token: &str) -> Result<Option<String>> {
    let homeserver = homeserver.trim_end_matches('/');
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to build Matrix setup HTTP client")?;
    let response = client
        .get(format!("{homeserver}/_matrix/client/v3/account/whoami"))
        .bearer_auth(access_token.trim())
        .send()
        .context("Matrix whoami request failed")?;
    let status = response.status();
    let value: Value = response
        .json()
        .context("Matrix whoami returned invalid JSON")?;
    if !status.is_success() {
        return Ok(None);
    }
    Ok(value
        .get("user_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string))
}

fn feishu_family_product_name(channel: &str) -> &'static str {
    match channel {
        LARK_CHANNEL | LARK_COMMENT_CHANNEL => "Lark",
        _ => "Feishu",
    }
}

fn feishu_family_domain(channel: &str) -> &'static str {
    match channel {
        LARK_CHANNEL | LARK_COMMENT_CHANNEL => "lark",
        _ => "feishu",
    }
}

fn feishu_registration_initial_domain(channel: &str) -> &'static str {
    feishu_registration_code_domain(channel)
}

fn feishu_registration_code_domain(_channel: &str) -> &'static str {
    // Feishu/Lark's scan-to-create device code is issued by the Feishu
    // registration backend even for global Lark tenants. The visible launcher
    // URL is rewritten separately so a user who selected Lark opens
    // open.larksuite.com instead of seeing a Feishu-branded URL.
    "feishu"
}

fn feishu_registration_launcher_domain(channel: &str) -> &'static str {
    feishu_family_domain(channel)
}

fn feishu_domain_accounts_url(domain: &str) -> &'static str {
    match domain {
        "lark" => "https://accounts.larksuite.com",
        _ => "https://accounts.feishu.cn",
    }
}

fn feishu_domain_api_base(domain: &str) -> &'static str {
    match domain {
        "lark" => "https://open.larksuite.com",
        _ => "https://open.feishu.cn",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeishuSetupMethod {
    ScanToCreate,
    ExistingApp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FeishuCredentialSetup {
    app_id: String,
    app_secret: String,
    domain: String,
    api_base: String,
    bot_name: Option<String>,
    bot_open_id: Option<String>,
    scanner_user_ids: Vec<String>,
    source: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FeishuBotProbe {
    bot_name: Option<String>,
    bot_open_id: Option<String>,
}

#[derive(Debug, Clone)]
struct FeishuRegistrationBegin {
    device_code: String,
    qr_url: String,
    user_code: String,
    interval: u64,
    expire_in: u64,
    initial_domain: &'static str,
}

#[derive(Debug, Clone)]
struct FeishuRegistrationCredentials {
    app_id: String,
    app_secret: String,
    domain: String,
    api_base: String,
    scanner_user_ids: Vec<String>,
}

fn prompt_feishu_setup_method(product: &str) -> Result<SetupAction<FeishuSetupMethod>> {
    let items = vec![
        PickerItem {
            title: "Scan to create bot app".to_string(),
            detail: format!(
                "Recommended. Scan with {product} mobile app; platform returns app credentials."
            ),
            model_columns: None,
        },
        PickerItem {
            title: "Use existing app".to_string(),
            detail: "Choose this when you already created an app and enabled Bot.".to_string(),
            model_columns: None,
        },
    ];
    match run_picker(
        "Setup method",
        &format!("Create or connect the {product} bot application."),
        &items,
        true,
    )? {
        SetupAction::Submit(0) => Ok(SetupAction::Submit(FeishuSetupMethod::ScanToCreate)),
        SetupAction::Submit(1) => Ok(SetupAction::Submit(FeishuSetupMethod::ExistingApp)),
        SetupAction::Submit(_) => Err(anyhow!("invalid Feishu/Lark setup method selection")),
        SetupAction::Back => Ok(SetupAction::Back),
    }
}

fn prompt_feishu_connection_mode(_product: &str) -> Result<SetupAction<&'static str>> {
    Ok(SetupAction::Submit("websocket"))
}

fn prompt_feishu_existing_app(
    channel: &'static str,
    default_api_base: &'static str,
) -> Result<SetupAction<FeishuCredentialSetup>> {
    let product = feishu_family_product_name(channel);

    loop {
        let app_id = match prompt_text(
            "App ID",
            &format!("Paste the {product} App ID from the developer console."),
            "cli_xxx",
            None,
            true,
            true,
            false,
        )? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return Ok(SetupAction::Back),
        };
        let app_secret = match prompt_gateway_secret(
            "App Secret",
            &format!("Paste the {product} App Secret from the developer console."),
        )? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return Ok(SetupAction::Back),
        };

        let bot = wait_for_feishu_bot_check(product, default_api_base, &app_id, &app_secret)?;
        return Ok(SetupAction::Submit(FeishuCredentialSetup {
            app_id,
            app_secret,
            domain: feishu_family_domain(channel).to_string(),
            api_base: default_api_base.to_string(),
            bot_name: bot.bot_name,
            bot_open_id: bot.bot_open_id,
            scanner_user_ids: Vec::new(),
            source: "manual",
        }));
    }
}

fn prompt_feishu_scan_to_create(
    channel: &'static str,
    default_api_base: &'static str,
) -> Result<SetupAction<Option<FeishuCredentialSetup>>> {
    match feishu_qr_register(channel, default_api_base) {
        Ok(Some(credentials)) => Ok(SetupAction::Submit(Some(credentials))),
        Ok(None) => Ok(SetupAction::Submit(None)),
        Err(error) => {
            let warning = vec![
                "Scan-to-create did not complete.".to_string(),
                format!("Reason: {error}"),
                "Confirm continues with existing app credentials.".to_string(),
            ];
            match prompt_confirm("Scan setup fallback", &warning, false)? {
                SetupAction::Submit(()) => Ok(SetupAction::Submit(None)),
                SetupAction::Back => unreachable!("fallback prompt does not allow back"),
            }
        }
    }
}

fn feishu_qr_register(
    channel: &'static str,
    _default_api_base: &'static str,
) -> Result<Option<FeishuCredentialSetup>> {
    let product = feishu_family_product_name(channel);
    let begin = begin_feishu_registration_with_loading(channel)?;
    let wait_lines = feishu_registration_wait_lines(product, &begin);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = poll_feishu_registration(begin);
        let _ = tx.send(result);
    });
    let Some(credentials) = wait_setup_display_task_with_flow(
        GATEWAY_SETUP_FLOW,
        &format!("{product} authorization"),
        &wait_lines,
        rx,
    )?
    else {
        return Ok(None);
    };
    let bot = wait_for_feishu_bot_check(
        product,
        &credentials.api_base,
        &credentials.app_id,
        &credentials.app_secret,
    )?;
    Ok(Some(FeishuCredentialSetup {
        app_id: credentials.app_id,
        app_secret: credentials.app_secret,
        domain: credentials.domain,
        api_base: credentials.api_base,
        bot_name: bot.bot_name,
        bot_open_id: bot.bot_open_id,
        scanner_user_ids: credentials.scanner_user_ids,
        source: "scan_to_create",
    }))
}

fn begin_feishu_registration_with_loading(
    channel: &'static str,
) -> Result<FeishuRegistrationBegin> {
    let product = feishu_family_product_name(channel);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = begin_feishu_registration(channel);
        let _ = tx.send(result);
    });
    wait_setup_task_with_flow(
        GATEWAY_SETUP_FLOW,
        &format!("Requesting {product} code"),
        "",
        rx,
    )
}

fn begin_feishu_registration(channel: &'static str) -> Result<FeishuRegistrationBegin> {
    let product = feishu_family_product_name(channel);
    let initial_domain = feishu_registration_initial_domain(channel);
    let launcher_domain = feishu_registration_launcher_domain(channel);
    let accounts_base = feishu_domain_accounts_url(initial_domain);
    let client = Client::builder()
        .timeout(Duration::from_secs(FEISHU_ONBOARD_REQUEST_TIMEOUT_SECS))
        .build()
        .context("failed to build Feishu/Lark onboarding client")?;

    let init = post_feishu_registration(&client, accounts_base, &[("action", "init")])?;
    let methods = init["supported_auth_methods"]
        .as_array()
        .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    if !methods.contains(&"client_secret") {
        return Err(anyhow!(
            "{product} registration does not support client_secret auth; supported: {methods:?}"
        ));
    }

    let begin = post_feishu_registration(
        &client,
        accounts_base,
        &[
            ("action", "begin"),
            ("archetype", "PersonalAgent"),
            ("auth_method", "client_secret"),
            ("request_user_info", "open_id"),
        ],
    )?;
    let device_code = begin["device_code"]
        .as_str()
        .ok_or_else(|| anyhow!("{product} registration did not return device_code"))?
        .to_string();
    let user_code = begin["user_code"].as_str().unwrap_or_default();
    let qr_url = duckagent_registration_url_for_domain(
        begin["verification_uri_complete"]
            .as_str()
            .unwrap_or_default(),
        launcher_domain,
        user_code,
    );
    if qr_url.is_empty() {
        return Err(anyhow!(
            "{product} registration did not return verification URL"
        ));
    }
    let interval = begin["interval"].as_u64().unwrap_or(5).max(1);
    let expire_in = begin["expires_in"]
        .as_u64()
        .or_else(|| begin["expire_in"].as_u64())
        .unwrap_or(FEISHU_ONBOARD_TIMEOUT_SECS)
        .min(FEISHU_ONBOARD_TIMEOUT_SECS);

    Ok(FeishuRegistrationBegin {
        device_code,
        qr_url,
        user_code: user_code.to_string(),
        interval,
        expire_in,
        initial_domain,
    })
}

fn poll_feishu_registration(
    begin: FeishuRegistrationBegin,
) -> Result<Option<FeishuRegistrationCredentials>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(FEISHU_ONBOARD_REQUEST_TIMEOUT_SECS))
        .build()
        .context("failed to build Feishu/Lark onboarding client")?;
    let mut current_domain = begin.initial_domain;
    let mut current_accounts_base = feishu_domain_accounts_url(current_domain);
    let mut current_api_base = feishu_domain_api_base(current_domain);

    let deadline = Instant::now() + Duration::from_secs(begin.expire_in);
    while Instant::now() < deadline {
        let poll = post_feishu_registration(
            &client,
            current_accounts_base,
            &[
                ("action", "poll"),
                ("device_code", begin.device_code.as_str()),
            ],
        );
        match poll {
            Ok(value) => {
                if let Some(tenant_brand) = value["user_info"]["tenant_brand"].as_str() {
                    let next_domain = match tenant_brand {
                        "lark" => Some("lark"),
                        "feishu" => Some("feishu"),
                        _ => None,
                    };
                    if let Some(next_domain) = next_domain {
                        if next_domain != current_domain {
                            current_domain = next_domain;
                            current_accounts_base = feishu_domain_accounts_url(next_domain);
                            current_api_base = feishu_domain_api_base(next_domain);
                        }
                    }
                }
                if let (Some(app_id), Some(app_secret)) =
                    (value["client_id"].as_str(), value["client_secret"].as_str())
                {
                    return Ok(Some(FeishuRegistrationCredentials {
                        app_id: app_id.to_string(),
                        app_secret: app_secret.to_string(),
                        domain: current_domain.to_string(),
                        api_base: current_api_base.to_string(),
                        scanner_user_ids: feishu_registration_user_ids(&value["user_info"]),
                    }));
                }
                let error = value["error"].as_str().unwrap_or_default();
                if matches!(error, "access_denied" | "expired_token") {
                    return Ok(None);
                }
            }
            Err(_) => {
                // Transient network failures are common while the user is scanning.
            }
        }
        thread::sleep(Duration::from_secs(begin.interval));
    }
    Ok(None)
}

fn feishu_registration_user_ids(user_info: &Value) -> Vec<String> {
    ["open_id", "user_id", "union_id"]
        .into_iter()
        .filter_map(|key| user_info[key].as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .fold(Vec::<String>::new(), |mut out, id| {
            if !out.iter().any(|existing| existing == &id) {
                out.push(id);
            }
            out
        })
}

fn post_feishu_registration(
    client: &Client,
    accounts_base: &str,
    form: &[(&str, &str)],
) -> Result<Value> {
    let url = format!(
        "{}{}",
        accounts_base.trim_end_matches('/'),
        FEISHU_REGISTRATION_PATH
    );
    let response = client
        .post(url)
        .form(form)
        .send()
        .context("Feishu/Lark registration request failed")?;
    let text = response
        .text()
        .context("Feishu/Lark registration response read failed")?;
    serde_json::from_str(&text).context("Feishu/Lark registration returned invalid JSON")
}

fn probe_feishu_bot(
    api_base: &str,
    app_id: &str,
    app_secret: &str,
) -> Result<Option<FeishuBotProbe>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(FEISHU_ONBOARD_REQUEST_TIMEOUT_SECS))
        .build()
        .context("failed to build Feishu/Lark probe client")?;
    let token_response = client
        .post(format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            api_base.trim_end_matches('/')
        ))
        .json(&json!({
            "app_id": app_id,
            "app_secret": app_secret,
        }))
        .send()
        .context("Feishu/Lark token probe request failed")?;
    let token_status = token_response.status();
    let token_json: Value = token_response
        .json()
        .context("Feishu/Lark token probe returned invalid JSON")?;
    if !token_status.is_success() || token_json["code"].as_i64().unwrap_or(-1) != 0 {
        return Ok(None);
    }
    let token = token_json["tenant_access_token"]
        .as_str()
        .ok_or_else(|| anyhow!("Feishu/Lark token probe missing tenant_access_token"))?;

    let bot_response = client
        .get(format!(
            "{}/open-apis/bot/v3/info",
            api_base.trim_end_matches('/')
        ))
        .bearer_auth(token)
        .send()
        .context("Feishu/Lark bot probe request failed")?;
    let bot_status = bot_response.status();
    let bot_json: Value = bot_response
        .json()
        .context("Feishu/Lark bot probe returned invalid JSON")?;
    if !bot_status.is_success() {
        return Ok(None);
    }
    Ok(parse_feishu_bot_info(&bot_json))
}

fn parse_feishu_bot_info(value: &Value) -> Option<FeishuBotProbe> {
    if value["code"].as_i64()? != 0 {
        return None;
    }
    let bot = value
        .get("bot")
        .or_else(|| value.get("data").and_then(|data| data.get("bot")))?;
    Some(FeishuBotProbe {
        bot_name: bot
            .get("app_name")
            .or_else(|| bot.get("bot_name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        bot_open_id: bot
            .get("open_id")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn wait_for_feishu_bot_check(
    product: &'static str,
    api_base: &str,
    app_id: &str,
    app_secret: &str,
) -> Result<FeishuBotProbe> {
    let api_base = api_base.to_string();
    let app_id = app_id.to_string();
    let app_secret = app_secret.to_string();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            match probe_feishu_bot(&api_base, &app_id, &app_secret) {
                Ok(Some(bot)) => {
                    let _ = tx.send(Ok(bot));
                    return;
                }
                Ok(None) | Err(_) => {
                    thread::sleep(Duration::from_secs(5));
                }
            }
        }
    });
    wait_setup_task_with_flow(
        GATEWAY_SETUP_FLOW,
        &format!("Checking {product} Bot"),
        "Waiting until Bot info is readable.",
        rx,
    )
}

fn feishu_registration_wait_lines(product: &str, begin: &FeishuRegistrationBegin) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("Use {product} to scan the QR code below."));
    lines.push(
        "If the QR code looks garbled, try another terminal or open this URL directly:".to_string(),
    );
    lines.extend(wrap_setup_url(&begin.qr_url, 92));
    match render_terminal_qr(&begin.qr_url) {
        Ok(qr) => lines.extend(qr.lines().map(str::to_string)),
        Err(_) => {
            lines.push("QR unavailable in this terminal; open the URL above directly.".to_string())
        }
    }
    lines
}

fn wrap_setup_url(url: &str, width: usize) -> Vec<String> {
    if url.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let chars: Vec<char> = url.chars().collect();
    let mut start = 0usize;
    while start < chars.len() {
        let end = (start + width.max(1)).min(chars.len());
        out.push(chars[start..end].iter().collect());
        start = end;
    }
    out
}

fn render_terminal_qr(payload: &str) -> Result<String> {
    use qrcode::render::unicode::Dense1x2;
    use qrcode::{EcLevel, QrCode};

    let qr = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::L)
        .map_err(|error| anyhow!("failed to encode gateway setup QR: {error}"))?;
    let qr = qr
        .render::<Dense1x2>()
        .quiet_zone(false)
        .build()
        .lines()
        .map(|line| format!("{SETUP_QR_DENSE_ROW_PREFIX}{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(qr)
}

fn duckagent_registration_url(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let Ok(mut parsed) = url::Url::parse(trimmed) else {
        return trimmed.to_string();
    };
    let existing: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(key, _)| !matches!(key.as_ref(), "from" | "source" | "tp"))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    parsed.set_query(None);
    {
        let mut query = parsed.query_pairs_mut();
        for (key, value) in existing {
            query.append_pair(&key, &value);
        }
        query.append_pair("from", "sdk");
        query.append_pair("source", "node-sdk");
        query.append_pair("tp", "sdk");
    }
    parsed.to_string()
}

fn duckagent_registration_url_for_domain(
    url: &str,
    launcher_domain: &str,
    user_code: &str,
) -> String {
    // Keep the short-lived code from the backend response, but render the URL
    // under the product domain selected in setup. This is important for Lark:
    // the code may come from accounts.feishu.cn, while mobile users expect the
    // launcher page to live on open.larksuite.com.
    let fallback = if user_code.trim().is_empty() {
        String::new()
    } else {
        format!(
            "{}/page/launcher?user_code={}",
            feishu_domain_api_base(launcher_domain),
            user_code.trim()
        )
    };
    let mut rewritten = duckagent_registration_url(if url.trim().is_empty() {
        &fallback
    } else {
        url
    });
    if let Ok(mut parsed) = url::Url::parse(&rewritten) {
        if let Ok(target) = url::Url::parse(feishu_domain_api_base(launcher_domain)) {
            let _ = parsed.set_scheme(target.scheme());
            let _ = parsed.set_host(target.host_str());
            if let Some(port) = target.port() {
                let _ = parsed.set_port(Some(port));
            } else {
                let _ = parsed.set_port(None);
            }
            rewritten = parsed.to_string();
        }
    }
    rewritten
}

fn prompt_feishu_review(
    channel: &str,
    bind: &str,
    transport: &str,
    credentials: &FeishuCredentialSetup,
) -> Result<SetupAction<()>> {
    let bot = credentials
        .bot_name
        .as_deref()
        .unwrap_or("not verified yet");
    let mut lines = vec![
        format!("Channel: {channel}"),
        format!("Transport: {transport}"),
        format!("Bot: {bot}"),
        format!("Open API: {}", credentials.api_base),
    ];
    if transport == "webhook" {
        lines.insert(2, format!("Bind: {bind}"));
    }
    prompt_confirm("Review gateway", &lines, true)
}

fn approve_feishu_scanner_pairing(
    channel: &str,
    scanner_user_ids: &[String],
) -> Result<Vec<String>> {
    let store = GatewayPairingStore::new(super::default_gateway_pairing_dir()?)?;
    let mut approved = Vec::new();
    for user_id in scanner_user_ids
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        store.approve_user(channel, user_id, Some("scan-to-create user".to_string()))?;
        approved.push(user_id.to_string());
    }
    Ok(approved)
}

fn run_feishu_setup(channel: &'static str, default_api_base: &'static str) -> Result<bool> {
    let product = feishu_family_product_name(channel);
    let current_config = load_gateway_config().unwrap_or_default();
    let setup_method = match prompt_feishu_setup_method(product)? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let credentials = match setup_method {
        FeishuSetupMethod::ScanToCreate => {
            match prompt_feishu_scan_to_create(channel, default_api_base)? {
                SetupAction::Submit(Some(value)) => value,
                SetupAction::Submit(None) => {
                    match prompt_feishu_existing_app(channel, default_api_base)? {
                        SetupAction::Submit(value) => value,
                        SetupAction::Back => return run_gateway_setup(),
                    }
                }
                SetupAction::Back => return run_gateway_setup(),
            }
        }
        FeishuSetupMethod::ExistingApp => {
            match prompt_feishu_existing_app(channel, default_api_base)? {
                SetupAction::Submit(value) => value,
                SetupAction::Back => return run_gateway_setup(),
            }
        }
    };
    let transport = match prompt_feishu_connection_mode(product)? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let bind = if transport == "webhook" {
        default_stable_gateway_bind(current_config.bind.as_deref())?
    } else {
        current_config
            .bind
            .clone()
            .unwrap_or_else(|| DEFAULT_GATEWAY_BIND.to_string())
    };
    let (verification_token, encrypt_key) = if transport == "webhook" {
        let verification_token = match prompt_optional_secret(
            "Verification token",
            &format!(
                "Optional. Paste the Verification Token from the {product} Event Subscriptions page."
            ),
        )? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return run_gateway_setup(),
        };
        let encrypt_key = match prompt_optional_secret(
            "Encrypt/sign key",
            "Optional. If set, callback signatures are checked; encrypted bodies are rejected until decrypt support is configured.",
        )? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return run_gateway_setup(),
        };
        (verification_token, encrypt_key)
    } else {
        (None, None)
    };
    let auth_key = channel.to_string();
    match prompt_feishu_review(channel, &bind, transport, &credentials)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    let mut credential_extra = BTreeMap::new();
    credential_extra.insert("domain".to_string(), credentials.domain.clone());
    credential_extra.insert("setup_source".to_string(), credentials.source.to_string());
    if let Some(bot_name) = credentials.bot_name.clone() {
        credential_extra.insert("bot_name".to_string(), bot_name);
    }
    if let Some(bot_open_id) = credentials.bot_open_id.clone() {
        credential_extra.insert("bot_open_id".to_string(), bot_open_id);
    }
    if !credentials.scanner_user_ids.is_empty() {
        credential_extra.insert(
            "scanner_user_ids".to_string(),
            credentials.scanner_user_ids.join(","),
        );
    }
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: channel.to_string(),
            app_id: Some(credentials.app_id.clone()),
            app_secret: Some(credentials.app_secret.clone()),
            webhook_secret: verification_token,
            signing_secret: encrypt_key,
            username: credentials.bot_name.clone(),
            extra: credential_extra,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut next = current_config;
    if transport == "webhook" {
        next.bind = Some(bind.clone());
    }
    next.channels.insert(
        channel.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some(transport.to_string()),
            api_base: Some(credentials.api_base.clone()),
            ..Default::default()
        },
    );
    if transport != "webhook" {
        clear_gateway_bind_if_unused(&mut next);
    }
    save_gateway_config(&next)?;
    if let Err(error) = approve_feishu_scanner_pairing(channel, &credentials.scanner_user_ids) {
        eprintln!("duckagent gateway scanner auto-pairing failed: {error:#}");
    }
    Ok(true)
}

fn run_feishu_comment_setup(channel: &'static str, default_api_base: &'static str) -> Result<bool> {
    let product = feishu_family_product_name(channel);
    let current_config = load_gateway_config().unwrap_or_default();
    let app_id = match prompt_text(
        "App id",
        &format!("{product} app_id with Drive comment event permission."),
        "cli_xxx",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let app_secret = match prompt_gateway_secret(
        "App secret",
        &format!("{product} app_secret used for tenant_access_token."),
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let bot_open_id = match prompt_text(
        "Bot open_id",
        "The bot/user open_id that comment events mention; self-authored comments are ignored.",
        "ou_xxx",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_string();
            if value.is_empty() {
                bail!("{product} Comment bot open_id is required");
            }
            value
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let transport = match prompt_feishu_connection_mode(product)? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let (verification_token, encrypt_key) = if transport == "webhook" {
        let verification_token = match prompt_optional_secret(
            "Verification token",
            &format!(
                "Optional. Paste the Verification Token from the {product} Event Subscriptions page."
            ),
        )? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return run_gateway_setup(),
        };
        let encrypt_key = match prompt_optional_secret(
            "Encrypt/sign key",
            "Optional request signature key. Encrypted callbacks are rejected until decrypt support is configured.",
        )? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return run_gateway_setup(),
        };
        (verification_token, encrypt_key)
    } else {
        (None, None)
    };
    let policy = match prompt_text(
        "Comment policy",
        "allowlist, open, or disabled. allowlist uses Allowed commenters below.",
        "allowlist",
        Some("allowlist"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            if !matches!(
                value.as_str(),
                "allowlist" | "allow-list" | "open" | "disabled" | "off"
            ) {
                bail!("{product} Comment policy must be allowlist, open, or disabled");
            }
            if value == "allow-list" {
                "allowlist".to_string()
            } else if value == "off" {
                "disabled".to_string()
            } else {
                value
            }
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed commenters",
        &format!("Comma-separated {product} open_id values. Required when policy is allowlist."),
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_chats = match prompt_optional_csv(
        "Allowed documents",
        "Optional doc keys: docx:<token>, sheet:<token>, file token, type wildcard like docx:*, or *.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    if policy == "allowlist" && allowed_users.is_empty() {
        bail!("{product} Comment policy allowlist requires at least one allowed commenter");
    }
    let require_mention = match prompt_text(
        "Require mention",
        "true means only mentioned comment events are routed to the Agent.",
        "true",
        Some("true"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            if !matches!(value.as_str(), "true" | "false") {
                bail!("{product} Comment Require mention must be true or false");
            }
            value
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let bind = if transport == "webhook" {
        Some(default_callback_gateway_bind(
            current_config.bind.as_deref(),
        )?)
    } else {
        None
    };
    let callback_path = format!("/{channel}/events");
    let api_base = default_api_base.to_string();
    let auth_key = channel.to_string();
    let allowed_commenters_label = if allowed_users.is_empty() {
        if policy == "allowlist" {
            "none configured".to_string()
        } else {
            "all commenters accepted by policy".to_string()
        }
    } else {
        allowed_users.join(", ")
    };
    let allowed_documents_label = if allowed_chats.is_empty() {
        "all documents".to_string()
    } else {
        allowed_chats.join(", ")
    };
    let mut review = if let Some(bind) = bind.as_deref() {
        vec![
            format!("Channel: {channel}"),
            format!("Transport: {transport}"),
            format!("{product} Comment local tunnel target: http://{bind}{callback_path}"),
            "Webhook auth: verification token optional; encrypted/signature callbacks require the encrypt/sign key".to_string(),
        ]
    } else {
        vec![
            format!("Channel: {channel}"),
            format!("Transport: {transport}"),
            "Public callback: not required for WebSocket mode".to_string(),
        ]
    };
    review.extend([
        format!("API base: {api_base}"),
        format!("Bot open_id: {bot_open_id}"),
        format!("Comment policy: {policy}"),
        format!("Require mention: {require_mention}"),
        format!("Allowed commenters: {allowed_commenters_label}"),
        format!("Allowed documents: {allowed_documents_label}"),
        "Inbound: Drive comment add/reply events only; self-authored comments are ignored"
            .to_string(),
        "Outbound: replies to the comment thread, with whole-document fallback when needed"
            .to_string(),
        "Approvals: explicit text fallback commands through the comment reply path".to_string(),
    ]);
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: channel.to_string(),
            app_id: Some(app_id),
            app_secret: Some(app_secret),
            webhook_secret: verification_token,
            signing_secret: encrypt_key,
            username: Some(bot_open_id),
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("policy".to_string(), policy);
    extra.insert("require_mention".to_string(), require_mention);
    extra.insert("callback_path".to_string(), callback_path.clone());
    let mut next = current_config;
    if let Some(bind) = bind {
        next.bind = Some(bind.clone());
    }
    next.channels.insert(
        channel.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some(transport.to_string()),
            api_base: Some(api_base),
            allowed_users,
            allowed_chats,
            extra,
            ..Default::default()
        },
    );
    if transport != "webhook" {
        clear_gateway_bind_if_unused(&mut next);
    }
    save_gateway_config(&next)?;
    let message = if transport == "webhook" {
        let bind = next
            .bind
            .as_deref()
            .expect("webhook comment setup stores a callback bind");
        format!(
            "Use http://{bind}{callback_path} as the document comment event callback URL, or map that path through your public/reverse-proxy URL."
        )
    } else {
        format!(
            "{product} will receive document comment events over WebSocket. No public callback URL is needed."
        )
    };
    show_setup_message(&format!("{product} Comment gateway configured"), &message)?;
    Ok(true)
}

fn run_discord_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let token = match prompt_gateway_secret("Discord bot token", "Paste the Discord bot token.")? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let bot_identity = discord_setup_me(&token).ok().flatten();
    let allowed_chats = match prompt_optional_csv(
        "Allowed Discord channels",
        "Optional comma-separated channel/thread/guild ids. Empty allows all chats, but guild messages still require @mention.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let free_response_channels = match prompt_optional_csv(
        "Free-response Discord channels",
        "Optional comma-separated channel ids where the bot may respond without @mention. Empty keeps @mention gating.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed Discord users",
        "Optional comma-separated user ids or <@mentions>. Empty allows all users.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_key = DISCORD_CHANNEL.to_string();
    let bot_identity_label = bot_identity
        .as_ref()
        .and_then(|identity| {
            identity.username.as_ref().map(|username| {
                identity
                    .user_id
                    .as_ref()
                    .map(|user_id| format!("{username} ({user_id})"))
                    .unwrap_or_else(|| username.clone())
            })
        })
        .unwrap_or_else(|| "will be learned from READY when the Gateway starts".to_string());
    let allowed_chats_label = if allowed_chats.is_empty() {
        "all DMs/channels/guilds; guild messages still require @mention".to_string()
    } else {
        allowed_chats.join(", ")
    };
    let free_response_label = if free_response_channels.is_empty() {
        "none; guild channels require @mention".to_string()
    } else {
        free_response_channels.join(", ")
    };
    let allowed_users_label = if allowed_users.is_empty() {
        "all Discord users".to_string()
    } else {
        allowed_users.join(", ")
    };
    let review = vec![
        format!("Channel: {DISCORD_CHANNEL}"),
        "Transport: Discord Gateway websocket + REST API".to_string(),
        "API base: https://discord.com/api/v10".to_string(),
        "Gateway: discovered from /gateway/bot, fallback wss://gateway.discord.gg".to_string(),
        "Local callback port: not required; DuckAgent does not receive Discord webhooks".to_string(),
        "Developer Portal: enable Bot, install the bot, and enable Message Content Intent for guild text".to_string(),
        format!("Bot identity: {bot_identity_label}"),
        format!("Allowed chats/guilds: {allowed_chats_label}"),
        format!("Allowed users: {allowed_users_label}"),
        format!("Free-response channels: {free_response_label}"),
        "Room policy: DMs respond directly; guild channels require @bot mention unless free-response".to_string(),
        "Thread policy: participated Discord threads can continue without repeating @mention".to_string(),
        "Approval UI: native components plus explicit text command fallback".to_string(),
        "Allowed mentions: everyone/roles disabled, user mentions enabled, replied_user disabled".to_string(),
        "Other bot messages: ignored by default; advanced allow_bots can opt in".to_string(),
    ];
    match prompt_confirm("Review Discord gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    let mut credential_extra = BTreeMap::new();
    if let Some(identity) = bot_identity {
        if let Some(user_id) = identity.user_id {
            credential_extra.insert("bot_user_id".to_string(), user_id);
        }
        if let Some(username) = identity.username {
            credential_extra.insert("bot_name".to_string(), username);
        }
    }
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: DISCORD_CHANNEL.to_string(),
            token: Some(token),
            extra: credential_extra,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("require_mention".to_string(), "true".to_string());
    extra.insert("allow_mention_everyone".to_string(), "false".to_string());
    extra.insert("allow_mention_roles".to_string(), "false".to_string());
    extra.insert("allow_mention_users".to_string(), "true".to_string());
    extra.insert(
        "allow_mention_replied_user".to_string(),
        "false".to_string(),
    );
    if !free_response_channels.is_empty() {
        extra.insert(
            "free_response_channels".to_string(),
            free_response_channels.join(","),
        );
    }

    let mut next = current_config;
    next.channels.insert(
        DISCORD_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("gateway_websocket".to_string()),
            api_base: Some("https://discord.com/api/v10".to_string()),
            allowed_users,
            allowed_chats,
            extra,
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "Discord gateway configured",
        "Configuration saved. Enable Message Content Intent in the Discord Developer Portal before serve.",
    )?;
    Ok(true)
}

fn discord_setup_me(token: &str) -> Result<Option<DiscordSetupIdentity>> {
    let token = token.trim().trim_start_matches("Bot ").trim();
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to build Discord setup HTTP client")?;
    let response = client
        .get("https://discord.com/api/v10/users/@me")
        .header("Authorization", format!("Bot {token}"))
        .send()
        .context("Discord users/@me request failed")?;
    let status = response.status();
    let value: Value = response
        .json()
        .context("Discord users/@me returned invalid JSON")?;
    if !status.is_success() {
        return Ok(None);
    }
    Ok(Some(DiscordSetupIdentity {
        user_id: value.get("id").and_then(Value::as_str).map(str::to_string),
        username: value
            .get("username")
            .and_then(Value::as_str)
            .map(str::to_string),
    }))
}

fn run_mattermost_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let base_url = match prompt_text(
        "Mattermost URL",
        "Mattermost server base URL, for example https://mattermost.example.com.",
        "https://mattermost.example.com",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let base_url = base_url.trim().trim_end_matches('/').to_string();
    if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
        show_setup_message(
            "Mattermost URL required",
            "Use a full Mattermost base URL beginning with https:// or http://.",
        )?;
        return run_mattermost_setup();
    }
    let token =
        match prompt_gateway_secret("Mattermost token", "Paste a bot or personal access token.")? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return run_gateway_setup(),
        };
    let bot_identity = mattermost_setup_me(&base_url, &token).ok().flatten();
    let allowed_chats = match prompt_optional_csv(
        "Allowed Mattermost channels",
        "Optional comma-separated channel ids. Empty allows all channels, but non-DM messages still require @mention.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed Mattermost users",
        "Optional comma-separated user ids. Empty allows all users.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let free_response_channels = match prompt_optional_csv(
        "Free-response Mattermost channels",
        "Optional comma-separated channel ids where the bot may respond without @mention. Empty keeps @mention gating.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_key = MATTERMOST_CHANNEL.to_string();
    let bot_identity_label = bot_identity
        .as_ref()
        .and_then(|identity| {
            identity.username.as_ref().map(|username| {
                identity
                    .user_id
                    .as_ref()
                    .map(|user_id| format!("@{username} ({user_id})"))
                    .unwrap_or_else(|| format!("@{username}"))
            })
        })
        .unwrap_or_else(|| "will be validated when the gateway starts".to_string());
    let allowed_chats_label = if allowed_chats.is_empty() {
        "all non-DM channels, with @mention required by default".to_string()
    } else {
        allowed_chats.join(", ")
    };
    let allowed_users_label = if allowed_users.is_empty() {
        "all users".to_string()
    } else {
        allowed_users.join(", ")
    };
    let free_response_label = if free_response_channels.is_empty() {
        "none; non-DM messages require @mention".to_string()
    } else {
        free_response_channels.join(", ")
    };
    let review = vec![
        format!("Channel: {MATTERMOST_CHANNEL}"),
        "Transport: REST API + outbound Mattermost websocket".to_string(),
        format!("Mattermost URL: {base_url}"),
        format!("WebSocket: {base_url}/api/v4/websocket"),
        "Local callback port: not required for receiving Mattermost messages".to_string(),
        "Approval UI: text fallback by default; native buttons require a public HTTPS URL mapped to /mattermost/actions".to_string(),
        format!("Bot identity: {bot_identity_label}"),
        format!("Allowed channels: {allowed_chats_label}"),
        format!("Allowed users: {allowed_users_label}"),
        format!("Free-response channels: {free_response_label}"),
        "Reply mode: thread replies enabled".to_string(),
    ];
    match prompt_confirm("Review Mattermost gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    let mut credential_extra = BTreeMap::new();
    if let Some(identity) = bot_identity {
        if let Some(user_id) = identity.user_id {
            credential_extra.insert("bot_user_id".to_string(), user_id);
        }
        if let Some(username) = identity.username {
            credential_extra.insert("bot_name".to_string(), username);
        }
    }
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: MATTERMOST_CHANNEL.to_string(),
            token: Some(token),
            extra: credential_extra,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("require_mention".to_string(), "true".to_string());
    extra.insert("reply_mode".to_string(), "thread".to_string());
    if !free_response_channels.is_empty() {
        extra.insert(
            "free_response_channels".to_string(),
            free_response_channels.join(","),
        );
    }

    let mut next = current_config;
    next.channels.insert(
        MATTERMOST_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("rest_websocket".to_string()),
            api_base: Some(base_url),
            allowed_users,
            allowed_chats,
            extra,
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "Mattermost gateway configured",
        "Configuration saved. Mattermost websocket will start when the gateway service starts.",
    )?;
    Ok(true)
}

fn mattermost_setup_me(base_url: &str, token: &str) -> Result<Option<MattermostSetupIdentity>> {
    let base_url = base_url.trim_end_matches('/');
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to build Mattermost setup HTTP client")?;
    let response = client
        .get(format!("{base_url}/api/v4/users/me"))
        .header("Authorization", format!("Bearer {}", token.trim()))
        .send()
        .context("Mattermost users/me request failed")?;
    let status = response.status();
    let value: Value = response
        .json()
        .context("Mattermost users/me returned invalid JSON")?;
    if !status.is_success() {
        return Ok(None);
    }
    Ok(Some(MattermostSetupIdentity {
        user_id: value.get("id").and_then(Value::as_str).map(str::to_string),
        username: value
            .get("username")
            .and_then(Value::as_str)
            .map(str::to_string),
    }))
}

fn run_api_server_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bind = default_callback_gateway_bind(current_config.bind.as_deref())?;
    let api_key =
        match prompt_optional_secret("API server key", "Optional bearer token for /v1 endpoints.")?
        {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return run_gateway_setup(),
        };
    let auth_key = API_SERVER_CHANNEL.to_string();
    let auth_label = if api_key.is_some() {
        "Bearer token required".to_string()
    } else {
        "disabled; any client that can reach this bind can call /v1".to_string()
    };
    let review = vec![
        format!("Channel: {API_SERVER_CHANNEL}"),
        "Transport: openai_http".to_string(),
        "Surface: OpenAI-compatible HTTP API for local or explicitly exposed clients".to_string(),
        format!("API base URL: http://{bind}/v1"),
        format!("Authentication: {auth_label}"),
        "Endpoints: /v1/models, /v1/capabilities, /v1/chat/completions, /v1/responses".to_string(),
        "Gateway listener: required for this API channel; the bind is allocated automatically unless an existing stable bind is already configured".to_string(),
    ];
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }
    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: API_SERVER_CHANNEL.to_string(),
            token: api_key,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;
    let mut next = current_config;
    next.bind = Some(bind.clone());
    next.channels.insert(
        API_SERVER_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("openai_http".to_string()),
            ..Default::default()
        },
    );
    save_gateway_config(&next)?;
    show_setup_message(
        "API server configured",
        &format!("Use http://{bind}/v1/chat/completions and http://{bind}/v1/models."),
    )?;
    Ok(true)
}

fn run_whatsapp_setup() -> Result<bool> {
    let transport = match prompt_text(
        "WhatsApp transport",
        "cloud-api uses the official WhatsApp Business Cloud API webhook/Graph API. Use bridge only for WhatsApp Web or personal-number QR pairing.",
        "cloud-api",
        Some("cloud-api"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => match value.trim().to_ascii_lowercase().as_str() {
            "" | "cloud" | "cloud-api" | "cloud_api" | "business" | "business-api"
            | "business_api" => "cloud_api",
            "bridge" | "bridge-http" | "bridge_http" | "web" | "whatsapp-web" | "whatsapp_web"
            | "personal" => "bridge_http",
            _ => {
                show_setup_message(
                    "WhatsApp transport required",
                    "Use cloud-api for the official WhatsApp Business Cloud API, or bridge for WhatsApp Web / personal-number QR pairing.",
                )?;
                return run_whatsapp_setup();
            }
        },
        SetupAction::Back => return run_gateway_setup(),
    };
    match transport {
        "cloud_api" => run_whatsapp_cloud_setup(),
        "bridge_http" => run_whatsapp_bridge_setup(),
        _ => unreachable!("validated WhatsApp transport"),
    }
}

fn run_whatsapp_cloud_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bind = default_callback_gateway_bind(current_config.bind.as_deref())?;
    let phone_number_id = match prompt_text(
        "WhatsApp phone number ID",
        "Phone number ID from Meta WhatsApp Business Cloud API settings.",
        "",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => value.trim().to_string(),
        SetupAction::Back => return run_whatsapp_setup(),
    };
    let access_token = match prompt_text(
        "WhatsApp access token",
        "Permanent or system-user access token with WhatsApp Business messaging permissions.",
        "",
        None,
        true,
        true,
        true,
    )? {
        SetupAction::Submit(value) => value.trim().to_string(),
        SetupAction::Back => return run_whatsapp_setup(),
    };
    let verify_token = match prompt_text(
        "Webhook verify token",
        "Secret string you will paste into Meta's webhook verification settings.",
        "",
        None,
        true,
        true,
        true,
    )? {
        SetupAction::Submit(value) => value.trim().to_string(),
        SetupAction::Back => return run_whatsapp_setup(),
    };
    let app_secret = match prompt_text(
        "Meta app secret",
        "Meta app secret used to verify x-hub-signature-256 on incoming WhatsApp webhooks.",
        "",
        None,
        true,
        true,
        true,
    )? {
        SetupAction::Submit(value) => value.trim().to_string(),
        SetupAction::Back => return run_whatsapp_setup(),
    };
    let public_webhook_url = match prompt_text(
        "Public Meta webhook URL",
        "HTTPS URL configured in Meta Webhooks, usually your tunnel/reverse-proxy URL ending in /whatsapp/webhook.",
        "https://your-domain.example/whatsapp/webhook",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().trim_end_matches('/').to_string();
            if !value.starts_with("https://") || !value.ends_with("/whatsapp/webhook") {
                show_setup_message(
                    "Public WhatsApp webhook URL required",
                    "Use an HTTPS URL ending in /whatsapp/webhook. Map it to the local tunnel target shown in review.",
                )?;
                return run_whatsapp_cloud_setup();
            }
            value
        }
        SetupAction::Back => return run_whatsapp_setup(),
    };
    let dm_policy = match prompt_text(
        "DM policy",
        "open, allowlist, or disabled. allowlist uses Allowed WhatsApp numbers below.",
        "open",
        Some("open"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            match value.as_str() {
                "open" => "open".to_string(),
                "allowlist" | "allow-list" => "allowlist".to_string(),
                "disabled" | "off" => "disabled".to_string(),
                _ => {
                    show_setup_message(
                        "WhatsApp DM policy required",
                        "Use open, allowlist, or disabled.",
                    )?;
                    return run_whatsapp_cloud_setup();
                }
            }
        }
        SetupAction::Back => return run_whatsapp_setup(),
    };
    let group_policy = match prompt_text(
        "Group policy",
        "Cloud API is normally 1:1 business messaging; use disabled unless your provider emits group context.",
        "disabled",
        Some("disabled"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            match value.as_str() {
                "open" => "open".to_string(),
                "allowlist" | "allow-list" => "allowlist".to_string(),
                "disabled" | "off" => "disabled".to_string(),
                _ => {
                    show_setup_message(
                        "WhatsApp group policy required",
                        "Use open, allowlist, or disabled.",
                    )?;
                    return run_whatsapp_cloud_setup();
                }
            }
        }
        SetupAction::Back => return run_whatsapp_setup(),
    };
    let require_mention = match prompt_text(
        "Require group wake pattern",
        "true means group-context messages must use a slash command or match a wake pattern.",
        "true",
        Some("true"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            match value.as_str() {
                "true" | "1" | "yes" | "on" => "true".to_string(),
                "false" | "0" | "no" | "off" => "false".to_string(),
                _ => {
                    show_setup_message(
                        "WhatsApp group wake setting required",
                        "Use true to require slash command/wake pattern, or false to allow open group-context messages under the group policy.",
                    )?;
                    return run_whatsapp_cloud_setup();
                }
            }
        }
        SetupAction::Back => return run_whatsapp_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed WhatsApp numbers",
        "Optional comma-separated E.164 numbers. Required when DM policy is allowlist.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_whatsapp_setup(),
    };
    let allowed_chats = match prompt_optional_csv(
        "Allowed WhatsApp group IDs",
        "Optional comma-separated group IDs, only used if your webhook provider emits group context.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_whatsapp_setup(),
    };
    if dm_policy == "allowlist" && allowed_users.is_empty() {
        show_setup_message(
            "Allowed WhatsApp numbers required",
            "DM policy allowlist requires at least one E.164 WhatsApp number.",
        )?;
        return run_whatsapp_cloud_setup();
    }
    if group_policy == "allowlist" && allowed_chats.is_empty() {
        show_setup_message(
            "Allowed WhatsApp group IDs required",
            "Group policy allowlist requires at least one group ID.",
        )?;
        return run_whatsapp_cloud_setup();
    }
    let mention_patterns = match prompt_optional_csv(
        "Wake patterns",
        "Optional comma-separated regex patterns for group-context wake words.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_whatsapp_setup(),
    };
    let inbound_path = "/whatsapp/webhook";
    let review = vec![
        format!("Channel: {WHATSAPP_CHANNEL}"),
        "Transport: WhatsApp Business Cloud API webhook + Graph API".to_string(),
        format!("Phone number ID: {phone_number_id}"),
        format!("Public Meta webhook URL: {public_webhook_url}"),
        format!("Local tunnel target: http://{bind}{inbound_path}"),
        "Webhook verification: verify token configured".to_string(),
        "Webhook signatures: x-hub-signature-256 verified with Meta app secret".to_string(),
        "Pairing: subscribe the WhatsApp Business Account messages webhook in Meta App Dashboard"
            .to_string(),
        format!("DM policy: {dm_policy}"),
        format!(
            "Allowed numbers: {}",
            if allowed_users.is_empty() {
                "none configured; open DM policy allows any reachable sender".to_string()
            } else {
                allowed_users.join(", ")
            }
        ),
        format!("Group policy: {group_policy}"),
        format!(
            "Allowed group IDs: {}",
            if allowed_chats.is_empty() {
                "none".to_string()
            } else {
                allowed_chats.join(", ")
            }
        ),
        format!("Require group wake pattern: {require_mention}"),
        format!(
            "Wake patterns: {}",
            if mention_patterns.is_empty() {
                "none".to_string()
            } else {
                mention_patterns.join(", ")
            }
        ),
        "Approvals: text fallback commands; WhatsApp has no native approval buttons".to_string(),
        "Bridge: not used in Cloud API mode; no Node.js or QR pairing is required".to_string(),
    ];
    match prompt_confirm("Review WhatsApp gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_whatsapp_setup(),
    }

    let auth_key = WHATSAPP_CHANNEL.to_string();
    let now = now_rfc3339_like();
    let mut credential_extra = BTreeMap::new();
    credential_extra.insert("phone_number_id".to_string(), phone_number_id.clone());
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: WHATSAPP_CHANNEL.to_string(),
            token: Some(access_token),
            signing_secret: Some(app_secret),
            webhook_secret: Some(verify_token),
            extra: credential_extra,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("phone_number_id".to_string(), phone_number_id);
    extra.insert("inbound_path".to_string(), inbound_path.to_string());
    extra.insert("public_webhook_url".to_string(), public_webhook_url);
    extra.insert("dm_policy".to_string(), dm_policy);
    extra.insert("group_policy".to_string(), group_policy.clone());
    extra.insert("require_mention".to_string(), require_mention);
    if !mention_patterns.is_empty() {
        extra.insert("mention_patterns".to_string(), mention_patterns.join(","));
    }

    let mut next = current_config;
    next.bind = Some(bind.clone());
    next.channels.insert(
        WHATSAPP_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("cloud_api".to_string()),
            api_base: Some("https://graph.facebook.com/v18.0".to_string()),
            allowed_users,
            allowed_chats,
            extra,
            ..Default::default()
        },
    );
    save_gateway_config(&next)?;
    show_setup_message(
        "WhatsApp gateway configured",
        &format!(
            "Set Meta's WhatsApp webhook callback URL to your public HTTPS URL and map it to http://{bind}{inbound_path}."
        ),
    )?;
    Ok(true)
}

fn run_whatsapp_bridge_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bridge_mode = match prompt_text(
        "WhatsApp bridge mode",
        "managed lets DuckAgent start a local WhatsApp bridge; external connects to an already-running bridge.",
        "managed",
        Some("managed"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            match value.as_str() {
                "managed" | "local" | "auto" | "true" | "yes" | "on" => "managed".to_string(),
                "external" | "manual" | "remote" | "false" | "no" | "off" => "external".to_string(),
                _ => {
                    show_setup_message(
                        "WhatsApp bridge mode required",
                        "Use managed to let DuckAgent start the local bridge, or external to connect to an already-running bridge.",
                    )?;
                    return run_whatsapp_bridge_setup();
                }
            }
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let managed_bridge = bridge_mode == "managed";
    let (bridge_base, bridge_port) = if managed_bridge {
        let port = allocate_local_setup_port()?;
        (format!("http://127.0.0.1:{port}"), Some(port))
    } else {
        let bridge_base = match prompt_text(
            "External bridge URL",
            "Base URL of an already-running WhatsApp bridge, for example http://127.0.0.1:3000.",
            "http://127.0.0.1:3000",
            Some("http://127.0.0.1:3000"),
            true,
            true,
            false,
        )? {
            SetupAction::Submit(value) => value.trim().trim_end_matches('/').to_string(),
            SetupAction::Back => return run_gateway_setup(),
        };
        if !(bridge_base.starts_with("http://") || bridge_base.starts_with("https://")) {
            show_setup_message(
                "WhatsApp bridge URL required",
                "Use a full external bridge URL beginning with http:// or https://.",
            )?;
            return run_whatsapp_bridge_setup();
        }
        (bridge_base, None)
    };
    let bridge_script = if managed_bridge {
        match prompt_text(
            "Bridge script path",
            "Path to a WhatsApp bridge.js script that exposes /messages, /send, /send-media, and /typing.",
            "scripts/whatsapp-bridge/bridge.js",
            Some("scripts/whatsapp-bridge/bridge.js"),
            true,
            true,
            false,
        )? {
            SetupAction::Submit(value) => Some(value.trim().to_string()),
            SetupAction::Back => return run_gateway_setup(),
        }
    } else {
        None
    };
    let session_path = managed_bridge.then(default_whatsapp_session_path_text);
    let mode = match prompt_text(
        "WhatsApp mode",
        "Bridge mode: bot for a dedicated number, self-chat for sending messages to yourself.",
        "bot",
        Some("bot"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            match value.as_str() {
                "bot" | "dedicated" => "bot".to_string(),
                "self-chat" | "self_chat" | "self" => "self-chat".to_string(),
                _ => {
                    show_setup_message(
                        "WhatsApp mode required",
                        "Use bot for a dedicated WhatsApp number, or self-chat to message yourself.",
                    )?;
                    return run_whatsapp_bridge_setup();
                }
            }
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let dm_policy = match prompt_text(
        "DM policy",
        "open, allowlist, or disabled. allowlist uses Allowed WhatsApp users below.",
        "open",
        Some("open"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            match value.as_str() {
                "open" => "open".to_string(),
                "allowlist" | "allow-list" => "allowlist".to_string(),
                "disabled" | "off" => "disabled".to_string(),
                _ => {
                    show_setup_message(
                        "WhatsApp DM policy required",
                        "Use open, allowlist, or disabled.",
                    )?;
                    return run_whatsapp_bridge_setup();
                }
            }
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let group_policy = match prompt_text(
        "Group policy",
        "open, allowlist, or disabled. allowlist uses Allowed WhatsApp groups below.",
        "open",
        Some("open"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            match value.as_str() {
                "open" => "open".to_string(),
                "allowlist" | "allow-list" => "allowlist".to_string(),
                "disabled" | "off" => "disabled".to_string(),
                _ => {
                    show_setup_message(
                        "WhatsApp group policy required",
                        "Use open, allowlist, or disabled.",
                    )?;
                    return run_whatsapp_bridge_setup();
                }
            }
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let require_mention = match prompt_text(
        "Require group mention",
        "true means group messages must mention/reply to the bot, use a slash command, or match a wake pattern.",
        "true",
        Some("true"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            match value.as_str() {
                "true" | "1" | "yes" | "on" => "true".to_string(),
                "false" | "0" | "no" | "off" => "false".to_string(),
                _ => {
                    show_setup_message(
                        "WhatsApp group mention setting required",
                        "Use true to require group mention/reply/wake pattern, or false to allow open group messages under the group policy.",
                    )?;
                    return run_whatsapp_bridge_setup();
                }
            }
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed WhatsApp users",
        "Optional comma-separated WhatsApp JIDs or phone numbers. Used for allowlists and bridge-side filtering.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_chats = match prompt_optional_csv(
        "Allowed WhatsApp groups",
        "Optional comma-separated group JIDs such as 120363001234567890@g.us.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    if dm_policy == "allowlist" && allowed_users.is_empty() {
        show_setup_message(
            "Allowed WhatsApp users required",
            "DM policy allowlist requires at least one WhatsApp user JID or phone number.",
        )?;
        return run_whatsapp_bridge_setup();
    }
    if group_policy == "allowlist" && allowed_chats.is_empty() {
        show_setup_message(
            "Allowed WhatsApp groups required",
            "Group policy allowlist requires at least one WhatsApp group JID.",
        )?;
        return run_whatsapp_bridge_setup();
    }
    let mention_patterns = match prompt_optional_csv(
        "Wake patterns",
        "Optional comma-separated regex patterns for group wake words.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let reply_prefix = match prompt_text(
        "Reply prefix",
        "Optional prefix for managed self-chat bridge replies. Empty keeps bridge default.",
        "",
        None,
        false,
        true,
        false,
    )? {
        SetupAction::Submit(value) => (!value.trim().is_empty()).then(|| value.trim().to_string()),
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_key = WHATSAPP_CHANNEL.to_string();
    let bridge_label = if managed_bridge {
        format!(
            "managed local bridge on {bridge_base} (auto-assigned local port {})",
            bridge_port.unwrap_or_default()
        )
    } else {
        format!("external bridge at {bridge_base}")
    };
    let bridge_script_label = bridge_script
        .as_deref()
        .unwrap_or("not used in external bridge mode");
    let session_path_label = session_path
        .as_deref()
        .unwrap_or("managed session path not used in external bridge mode");
    let allowed_users_label = if allowed_users.is_empty() {
        "none configured; open DM policy allows any reachable DM sender".to_string()
    } else {
        allowed_users.join(", ")
    };
    let allowed_groups_label = if allowed_chats.is_empty() {
        match group_policy.as_str() {
            "open" => {
                "all groups, still gated by mention/reply/wake pattern when required".to_string()
            }
            "disabled" => "group messages disabled".to_string(),
            _ => "none".to_string(),
        }
    } else {
        allowed_chats.join(", ")
    };
    let mention_patterns_label = if mention_patterns.is_empty() {
        "none".to_string()
    } else {
        mention_patterns.join(", ")
    };
    let reply_prefix_label = reply_prefix
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or(if mode == "self-chat" {
            "DuckAgent self-chat default"
        } else {
            "none"
        });
    let review = vec![
        format!("Channel: {WHATSAPP_CHANNEL}"),
        "Transport: WhatsApp Web/Baileys-style HTTP bridge polling".to_string(),
        format!("Bridge: {bridge_label}"),
        "Local callback port: not required; DuckAgent polls the bridge /messages endpoint".to_string(),
        format!("Bridge script: {bridge_script_label}"),
        format!("Session path: {session_path_label}"),
        "Pairing: scan the WhatsApp QR emitted by the bridge on first start; protect the session directory like a password".to_string(),
        "Requirements: Node.js must be available when using managed bridge mode".to_string(),
        format!("WhatsApp mode: {mode}"),
        format!("DM policy: {dm_policy}"),
        format!("Allowed DM users: {allowed_users_label}"),
        format!("Group policy: {group_policy}"),
        format!("Allowed groups: {allowed_groups_label}"),
        format!("Require group mention: {require_mention}"),
        format!("Wake patterns: {mention_patterns_label}"),
        format!("Reply prefix: {reply_prefix_label}"),
        "Approvals: text fallback commands; WhatsApp has no native approval buttons".to_string(),
        "Delivery limits: bridge/provider errors such as session-window failures are surfaced as send errors".to_string(),
    ];
    match prompt_confirm("Review WhatsApp gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: WHATSAPP_CHANNEL.to_string(),
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("mode".to_string(), mode);
    extra.insert("dm_policy".to_string(), dm_policy);
    extra.insert("group_policy".to_string(), group_policy);
    extra.insert("require_mention".to_string(), require_mention);
    extra.insert("managed_bridge".to_string(), managed_bridge.to_string());
    if let Some(port) = bridge_port {
        extra.insert("bridge_port".to_string(), port.to_string());
    }
    if let Some(path) = bridge_script {
        extra.insert("bridge_script".to_string(), path);
    }
    if let Some(path) = session_path {
        extra.insert("session_path".to_string(), path);
    }
    if !mention_patterns.is_empty() {
        extra.insert("mention_patterns".to_string(), mention_patterns.join(","));
    }
    if let Some(prefix) = reply_prefix {
        extra.insert("reply_prefix".to_string(), prefix);
    }

    let mut next = current_config;
    next.channels.insert(
        WHATSAPP_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("bridge_http".to_string()),
            api_base: Some(bridge_base),
            allowed_users,
            allowed_chats,
            extra,
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "WhatsApp gateway configured",
        "Configuration saved. The WhatsApp bridge adapter will start when the gateway service starts.",
    )?;
    Ok(true)
}

fn run_dingtalk_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let app_key = match prompt_text(
        "DingTalk client id / app key",
        "Client ID/App Key from the DingTalk bot app Stream Mode settings. No public callback URL or local port is required.",
        "",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty()
                || trimmed.contains("://")
                || trimmed.chars().any(char::is_whitespace)
            {
                bail!("IRC server must be a hostname such as irc.libera.chat");
            }
            trimmed.to_string()
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let app_secret = match prompt_gateway_secret(
        "DingTalk client secret / app secret",
        "Client Secret/App Secret used to open the DingTalk Stream Mode websocket.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed DingTalk users",
        "Optional comma-separated sender ids or staff ids. Empty allows all users.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_chats = match prompt_optional_csv(
        "Allowed DingTalk chats",
        "Optional comma-separated conversation ids. Empty allows all chats.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let free_response_chats = match prompt_optional_csv(
        "Free-response DingTalk chats",
        "Optional comma-separated conversation ids where the bot may respond without @mention.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let require_mention = match prompt_text(
        "Require group mention",
        "true means group messages need @bot, slash command, free-response chat, or wake pattern.",
        "true",
        Some("true"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            match value.as_str() {
                "true" | "1" | "yes" | "on" => "true".to_string(),
                "false" | "0" | "no" | "off" => "false".to_string(),
                _ => {
                    show_setup_message(
                        "DingTalk group mention setting required",
                        "Use true to require @bot/slash/free-response/wake pattern in groups, or false to allow any allowed group message.",
                    )?;
                    return run_dingtalk_setup();
                }
            }
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let mention_patterns = match prompt_optional_csv(
        "Wake patterns",
        "Optional comma-separated regex patterns for DingTalk group wake words.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_key = DINGTALK_CHANNEL.to_string();
    let allowed_users_label = if allowed_users.is_empty() {
        "all DingTalk senders".to_string()
    } else {
        allowed_users.join(", ")
    };
    let allowed_chats_label = if allowed_chats.is_empty() {
        "all conversations".to_string()
    } else {
        allowed_chats.join(", ")
    };
    let free_response_label = if free_response_chats.is_empty() {
        "none; group messages require @bot/slash/wake pattern".to_string()
    } else {
        free_response_chats.join(", ")
    };
    let mention_patterns_label = if mention_patterns.is_empty() {
        "none".to_string()
    } else {
        mention_patterns.join(", ")
    };
    let review = vec![
        format!("Channel: {DINGTALK_CHANNEL}"),
        "Transport: DingTalk Stream Mode websocket + session webhook replies".to_string(),
        format!("Client id / app key: {app_key}"),
        "Client secret / app secret: configured".to_string(),
        "Inbound: DuckAgent opens a DingTalk Stream Mode websocket; no public callback URL or local port is required".to_string(),
        "Session webhook: learned from each inbound DingTalk message for replies".to_string(),
        format!("Allowed users: {allowed_users_label}"),
        format!("Allowed chats: {allowed_chats_label}"),
        format!("Free-response chats: {free_response_label}"),
        format!("Require group mention: {require_mention}"),
        format!("Wake patterns: {mention_patterns_label}"),
        "Inbound media: URL media is downloaded; download-code-only media is surfaced as a note".to_string(),
        "Outbound media: HTTP(S) images/links can be sent through markdown; local file upload is not available yet".to_string(),
        "Approvals: explicit text fallback commands; DingTalk has no native approval buttons here".to_string(),
        "Typing: not supported by this Stream Mode/session webhook transport".to_string(),
    ];
    match prompt_confirm("Review DingTalk gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: DINGTALK_CHANNEL.to_string(),
            app_id: Some(app_key),
            app_secret: Some(app_secret),
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("require_mention".to_string(), require_mention);
    if !free_response_chats.is_empty() {
        extra.insert(
            "free_response_chats".to_string(),
            free_response_chats.join(","),
        );
    }
    if !mention_patterns.is_empty() {
        extra.insert("mention_patterns".to_string(), mention_patterns.join(","));
    }

    let mut next = current_config;
    next.channels.insert(
        DINGTALK_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("stream".to_string()),
            allowed_users,
            allowed_chats,
            extra,
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "DingTalk gateway configured",
        "Configuration saved. DingTalk Stream Mode will connect when the gateway service starts.",
    )?;
    Ok(true)
}

fn run_wecom_setup(channel: &'static str) -> Result<bool> {
    if channel == WECOM_CHANNEL {
        return run_wecom_bot_setup();
    }
    run_wecom_callback_setup(channel)
}

fn run_wecom_bot_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bot_id = match prompt_text(
        "WeCom Bot ID",
        "Bot ID from the Enterprise WeChat AI Bot API mode. No public callback URL or local port is required.",
        "bot_xxxxxxxxxxxxxxxx",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_string();
            if value.is_empty() || value.contains(char::is_whitespace) {
                bail!("WeCom Bot ID must be a non-empty single token");
            }
            value
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let secret = match prompt_gateway_secret(
        "WeCom Bot Secret",
        "Secret from the Enterprise WeChat AI Bot credentials page.",
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_string();
            if trimmed.chars().any(char::is_whitespace) {
                bail!("WeCom Bot Secret must not contain whitespace");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    if secret.is_empty() {
        bail!("WeCom Bot Secret is required");
    }
    let websocket_url = match prompt_text(
        "WeCom websocket URL",
        "Optional advanced override. Leave empty for the official Enterprise WeChat AI Bot websocket endpoint.",
        "",
        None,
        false,
        true,
        false,
    )? {
        SetupAction::Submit(value) => (!value.trim().is_empty()).then(|| value.trim().to_string()),
        SetupAction::Back => return run_gateway_setup(),
    };
    if let Some(url) = websocket_url.as_deref() {
        if !(url.starts_with("wss://") || url.starts_with("ws://")) {
            show_setup_message(
                "WeCom websocket URL required",
                "Use a full websocket URL beginning with wss:// or ws://, or leave the field empty for the official endpoint.",
            )?;
            return run_wecom_bot_setup();
        }
    }
    let dm_policy = match prompt_text(
        "WeCom DM policy",
        "open, allowlist, or disabled. allowlist uses Allowed WeCom users below.",
        "open",
        Some("open"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            match value.as_str() {
                "open" | "allowlist" | "allow-list" | "disabled" | "off" => {
                    if value == "allow-list" {
                        "allowlist".to_string()
                    } else if value == "off" {
                        "disabled".to_string()
                    } else {
                        value
                    }
                }
                _ => bail!("WeCom DM policy must be open, allowlist, or disabled"),
            }
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let group_policy = match prompt_text(
        "WeCom group policy",
        "open, allowlist, or disabled. allowlist uses Allowed WeCom group chats below.",
        "open",
        Some("open"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            match value.as_str() {
                "open" | "allowlist" | "allow-list" | "disabled" | "off" => {
                    if value == "allow-list" {
                        "allowlist".to_string()
                    } else if value == "off" {
                        "disabled".to_string()
                    } else {
                        value
                    }
                }
                _ => bail!("WeCom group policy must be open, allowlist, or disabled"),
            }
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed WeCom users",
        "Optional comma-separated WeCom user ids. Required when DM policy is allowlist; when set, also filters group senders.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_chats = match prompt_optional_csv(
        "Allowed WeCom group chats",
        "Optional comma-separated WeCom group chat ids. Required when group policy is allowlist.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    if dm_policy == "allowlist" && allowed_users.is_empty() {
        bail!("WeCom DM policy allowlist requires at least one allowed WeCom user");
    }
    if group_policy == "allowlist" && allowed_chats.is_empty() {
        bail!("WeCom group policy allowlist requires at least one allowed WeCom group chat");
    }
    let websocket_label = websocket_url
        .as_deref()
        .unwrap_or("wss://openws.work.weixin.qq.com");
    let allowed_users_label = if allowed_users.is_empty() {
        match dm_policy.as_str() {
            "disabled" => "none; direct messages are disabled".to_string(),
            "allowlist" => "none configured".to_string(),
            _ => "all direct-message users that can reach this bot".to_string(),
        }
    } else {
        allowed_users.join(", ")
    };
    let allowed_chats_label = if allowed_chats.is_empty() {
        match group_policy.as_str() {
            "disabled" => "none; group messages are disabled".to_string(),
            "allowlist" => "none configured".to_string(),
            _ => "all group chats where the bot is present".to_string(),
        }
    } else {
        allowed_chats.join(", ")
    };
    let review = vec![
        format!("Channel: {WECOM_CHANNEL}"),
        "Transport: Enterprise WeChat AI Bot websocket".to_string(),
        "Public callback URL: not required; this mode uses outbound websocket long connection".to_string(),
        format!("Bot ID: {bot_id}"),
        format!("WebSocket endpoint: {websocket_label}"),
        format!("DM policy: {dm_policy}"),
        format!("Allowed users: {allowed_users_label}"),
        format!("Group policy: {group_policy}"),
        format!("Allowed group chats: {allowed_chats_label}"),
        "Inbound: DM/group text, mixed/appmsg/media payloads, and media download when URLs are available".to_string(),
        "Outbound: markdown text plus local media upload over AI Bot websocket".to_string(),
        "Group replies: reuse recent inbound req_id when available, then fall back to proactive send".to_string(),
        "Approvals: explicit text fallback commands; WeCom AI Bot has no native approval buttons here".to_string(),
        "Typing: not supported by this transport".to_string(),
    ];
    match prompt_confirm("Review WeCom gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    let mut credential_extra = BTreeMap::new();
    if let Some(value) = websocket_url.clone() {
        credential_extra.insert("websocket_url".to_string(), value);
    }
    save_gateway_credentials(
        WECOM_CHANNEL,
        GatewayCredentialEntry {
            channel: WECOM_CHANNEL.to_string(),
            app_id: Some(bot_id),
            app_secret: Some(secret),
            extra: credential_extra,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    if let Some(value) = websocket_url {
        extra.insert("websocket_url".to_string(), value);
    }
    extra.insert("dm_policy".to_string(), dm_policy);
    extra.insert("group_policy".to_string(), group_policy);
    let mut next = current_config;
    next.channels.insert(
        WECOM_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("aibot_websocket".to_string()),
            allowed_users,
            allowed_chats,
            extra,
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "WeCom gateway configured",
        "Add the AI Bot to a WeCom chat or message it directly. No callback endpoint is needed.",
    )?;
    Ok(true)
}

fn run_wecom_callback_setup(channel: &'static str) -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bind = default_callback_gateway_bind(current_config.bind.as_deref())?;
    let callback_path = if channel == WECOM_CALLBACK_CHANNEL {
        "/wecom_callback/events"
    } else {
        "/wecom/events"
    };
    let callback_public_url = match prompt_text(
        "WeCom callback URL",
        "Optional public callback base URL. DuckAgent will append the WeCom callback path. Leave empty if you will map the local endpoint yourself.",
        "",
        None,
        false,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            (!value.trim().is_empty()).then(|| gateway_callback_url(value.trim(), callback_path))
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let corp_id = match prompt_text(
        "WeCom corp id",
        "Enterprise WeChat CorpID.",
        "wwxxxxxxxxxxxxxxxx",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim();
            let parsed: u16 = trimmed
                .parse()
                .map_err(|_| anyhow!("IRC port must be a number such as 6697"))?;
            if parsed == 0 {
                bail!("IRC port must be between 1 and 65535");
            }
            parsed.to_string()
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let corp_secret = match prompt_gateway_secret(
        "WeCom corp secret",
        "Self-built app secret for access_token and proactive message/send.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let agent_id = match prompt_text(
        "WeCom agent id",
        "Self-built app AgentId.",
        "1000002",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
                bail!("Yuanbao app id must be non-empty and must not contain whitespace");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let callback_token = match prompt_gateway_secret(
        "WeCom callback token",
        "Token configured in the WeCom callback URL verification settings.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let encoding_aes_key = match prompt_gateway_secret(
        "WeCom EncodingAESKey",
        "43-character EncodingAESKey configured in WeCom callback settings.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    if encoding_aes_key.trim().len() != 43 {
        bail!("WeCom EncodingAESKey must be exactly 43 characters");
    }
    let allowed_users = match prompt_optional_csv(
        "Allowed WeCom users",
        "Optional comma-separated WeCom user ids. Empty allows all users reachable by this app.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    if corp_id.trim().is_empty() {
        bail!("WeCom corp id is required");
    }
    if agent_id.trim().is_empty() {
        bail!("WeCom agent id is required");
    }
    let auth_key = channel.to_string();
    let callback_endpoint = callback_public_url
        .clone()
        .unwrap_or_else(|| format!("http://{bind}{callback_path}"));
    let endpoint_label = if callback_public_url.is_some() {
        "Public callback URL"
    } else {
        "Local tunnel target"
    };
    let local_endpoint = format!("http://{bind}{callback_path}");
    let allowed_users_label = if allowed_users.is_empty() {
        "all users reachable by this self-built app".to_string()
    } else {
        allowed_users.join(", ")
    };
    let review = vec![
        format!("Channel: {channel}"),
        "Transport: Enterprise WeChat encrypted callback + proactive message/send".to_string(),
        format!("{endpoint_label}: {callback_endpoint}"),
        format!("Local tunnel target: {local_endpoint}"),
        format!("Callback path: {callback_path}"),
        format!("Corp ID: {corp_id}"),
        format!("Agent ID: {agent_id}"),
        "Callback verification: Token + EncodingAESKey signature/decrypt enabled".to_string(),
        format!("Allowed users: {allowed_users_label}"),
        "Inbound: encrypted text/event callback; GET verification handshake is supported".to_string(),
        "Outbound: proactive message/send with access_token refresh; media upload is available for local MEDIA paths".to_string(),
        "Session namespace: independent from WeCom AI Bot and scoped to this callback channel".to_string(),
        "Approvals: explicit text fallback commands; callback app has no native approval buttons here".to_string(),
    ];
    match prompt_confirm("Review WeCom Callback gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    let mut credential_extra = BTreeMap::new();
    credential_extra.insert("agent_id".to_string(), agent_id.clone());
    credential_extra.insert("encoding_aes_key".to_string(), encoding_aes_key);
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: channel.to_string(),
            app_id: Some(corp_id),
            app_secret: Some(corp_secret),
            webhook_secret: Some(callback_token),
            extra: credential_extra,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("agent_id".to_string(), agent_id);
    if let Some(callback_url) = callback_public_url {
        extra.insert("callback_url".to_string(), callback_url);
    }
    let mut next = current_config;
    next.bind = Some(bind.clone());
    next.channels.insert(
        channel.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("encrypted_callback".to_string()),
            allowed_users,
            extra,
            ..Default::default()
        },
    );
    save_gateway_config(&next)?;
    show_setup_message(
        "WeCom Callback gateway configured",
        &format!(
            "Set the WeCom callback URL to {callback_endpoint}, or map a public/reverse-proxy URL to that local target."
        ),
    )?;
    Ok(true)
}

fn run_weixin_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let credentials = match prompt_weixin_credentials()? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let api_base = credentials
        .api_base
        .trim()
        .trim_end_matches('/')
        .to_string();
    let auth_key = WEIXIN_CHANNEL.to_string();
    match prompt_gateway_review_without_bind(WEIXIN_CHANNEL, "ilink_polling")? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: WEIXIN_CHANNEL.to_string(),
            token: Some(credentials.token.clone()),
            extra: weixin_credential_extra(&credentials),
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    if let Some(account_id) = credentials.account_id.as_ref() {
        extra.insert("account_id".to_string(), account_id.clone());
        extra.insert("bot_user_id".to_string(), account_id.clone());
    }
    if let Some(user_id) = credentials.user_id.as_ref() {
        extra.insert("owner_user_id".to_string(), user_id.clone());
    }
    let mut next = current_config;
    next.channels.insert(
        WEIXIN_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("ilink_polling".to_string()),
            api_base: Some(api_base),
            extra,
            ..Default::default()
        },
    );
    if !config_needs_stable_bind(&next) {
        next.bind = None;
    }
    save_gateway_config(&next)?;
    show_setup_message(
        "Weixin gateway configured",
        "Configuration saved. The Weixin iLink polling adapter will start on a random local port when the gateway service starts.",
    )?;
    Ok(true)
}

fn prompt_weixin_credentials() -> Result<SetupAction<WeixinCredentialSetup>> {
    match weixin_qr_login_setup()? {
        Some(credentials) => Ok(SetupAction::Submit(credentials)),
        None => Err(anyhow!(
            "Weixin QR login did not complete; no credentials were saved. Run `duck gateway service start` again to retry."
        )),
    }
}

fn weixin_credential_extra(credentials: &WeixinCredentialSetup) -> BTreeMap<String, String> {
    let mut extra = BTreeMap::new();
    if let Some(account_id) = credentials.account_id.as_ref() {
        extra.insert("account_id".to_string(), account_id.clone());
        extra.insert("bot_user_id".to_string(), account_id.clone());
    }
    if let Some(user_id) = credentials.user_id.as_ref() {
        extra.insert("owner_user_id".to_string(), user_id.clone());
    }
    extra
}

fn weixin_qr_login_setup() -> Result<Option<WeixinCredentialSetup>> {
    let begin = begin_weixin_qr_login()?;
    let lines = weixin_qr_wait_lines(&begin);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = poll_weixin_qr_login(begin);
        let _ = tx.send(result);
    });
    wait_setup_display_task_with_flow(GATEWAY_SETUP_FLOW, "Weixin QR login", &lines, rx)
}

fn begin_weixin_qr_login() -> Result<WeixinQrBegin> {
    let client = Client::builder()
        .timeout(Duration::from_secs(WEIXIN_ILINK_REQUEST_TIMEOUT_SECS))
        .build()
        .context("failed to build Weixin iLink setup HTTP client")?;
    let value = weixin_ilink_get(
        &client,
        WEIXIN_ILINK_BASE_URL,
        &format!("ilink/bot/get_bot_qrcode?bot_type={WEIXIN_ILINK_BOT_TYPE}"),
    )?;
    let qrcode = value["qrcode"].as_str().unwrap_or_default().trim();
    if qrcode.is_empty() {
        return Err(anyhow!("Weixin iLink QR response did not include qrcode"));
    }
    Ok(WeixinQrBegin {
        qrcode: qrcode.to_string(),
        qr_url: value["qrcode_img_content"]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    })
}

fn poll_weixin_qr_login(begin: WeixinQrBegin) -> Result<Option<WeixinCredentialSetup>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(WEIXIN_ILINK_REQUEST_TIMEOUT_SECS))
        .build()
        .context("failed to build Weixin iLink setup HTTP client")?;
    let deadline = Instant::now() + Duration::from_secs(WEIXIN_ILINK_QR_TIMEOUT_SECS);
    let mut base_url = WEIXIN_ILINK_BASE_URL.to_string();
    while Instant::now() < deadline {
        let value = weixin_ilink_get(
            &client,
            &base_url,
            &format!(
                "ilink/bot/get_qrcode_status?qrcode={}",
                url_encode_component(&begin.qrcode)
            ),
        )?;
        match value["status"].as_str().unwrap_or("wait") {
            "wait" | "scaned" => {}
            "scaned_but_redirect" => {
                if let Some(host) = value["redirect_host"].as_str().map(str::trim) {
                    if !host.is_empty() {
                        base_url = format!("https://{host}");
                    }
                }
            }
            "expired" => {
                return Err(anyhow!(
                    "Weixin QR code expired; run setup again to request a fresh QR code"
                ));
            }
            "confirmed" => {
                let account_id = value["ilink_bot_id"]
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let token = value["bot_token"].as_str().unwrap_or_default().trim();
                if token.is_empty() {
                    return Err(anyhow!(
                        "Weixin QR was confirmed but iLink did not return bot_token"
                    ));
                }
                let api_base = normalize_weixin_api_base(
                    value["baseurl"]
                        .as_str()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .unwrap_or(base_url.as_str()),
                );
                let user_id = value["ilink_user_id"]
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                return Ok(Some(WeixinCredentialSetup {
                    account_id,
                    token: token.to_string(),
                    api_base,
                    user_id,
                }));
            }
            other => {
                return Err(anyhow!(
                    "Weixin QR login returned unexpected status `{other}`"
                ));
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
    Ok(None)
}

fn weixin_qr_wait_lines(begin: &WeixinQrBegin) -> Vec<String> {
    let qr_payload = begin.qr_url.as_deref().unwrap_or(begin.qrcode.as_str());
    let mut lines = vec![
        "Use WeChat to scan this iLink QR code, then confirm login on your phone.".to_string(),
        "This creates/connects an iLink bot identity, not a normal scriptable personal account."
            .to_string(),
    ];
    if let Some(url) = begin.qr_url.as_deref() {
        lines.push("If the QR code looks garbled, open this URL directly:".to_string());
        lines.extend(wrap_setup_url(url, 92));
    }
    match render_terminal_qr(qr_payload) {
        Ok(qr) => lines.extend(qr.lines().map(str::to_string)),
        Err(_) => {
            lines.push("QR unavailable in this terminal; open the URL above directly.".to_string())
        }
    }
    lines.push(format!(
        "Waiting up to {} minutes.",
        WEIXIN_ILINK_QR_TIMEOUT_SECS / 60
    ));
    lines
}

fn weixin_ilink_get(client: &Client, base_url: &str, endpoint: &str) -> Result<Value> {
    let url = format!("{}/{}", base_url.trim_end_matches('/'), endpoint);
    let response = client
        .get(&url)
        .header("iLink-App-Id", WEIXIN_ILINK_APP_ID)
        .header("iLink-App-ClientVersion", WEIXIN_ILINK_APP_CLIENT_VERSION)
        .send()
        .with_context(|| format!("Weixin iLink GET {endpoint} failed"))?;
    let status = response.status();
    let text = response
        .text()
        .with_context(|| format!("Weixin iLink GET {endpoint} returned unreadable body"))?;
    if !status.is_success() {
        return Err(anyhow!(
            "Weixin iLink GET {endpoint} failed with status {status}: {text}"
        ));
    }
    serde_json::from_str(&text)
        .with_context(|| format!("Weixin iLink GET {endpoint} returned invalid JSON"))
}

fn normalize_weixin_api_base(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return WEIXIN_ILINK_BASE_URL.to_string();
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    }
}

fn url_encode_component(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn run_bluebubbles_setup(channel: &'static str) -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bind = default_callback_gateway_bind(current_config.bind.as_deref())?;
    let server_url = match prompt_text(
        "BlueBubbles server URL",
        "BlueBubbles macOS server URL, for example http://192.168.1.10:1234.",
        "http://127.0.0.1:1234",
        Some("http://127.0.0.1:1234"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().trim_end_matches('/').to_string();
            if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
                bail!("BlueBubbles server URL must start with http:// or https://");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    if !(server_url.starts_with("http://") || server_url.starts_with("https://")) {
        show_setup_message(
            "BlueBubbles server URL required",
            "Use a full BlueBubbles server URL beginning with http:// or https://.",
        )?;
        return run_bluebubbles_setup(channel);
    }
    let password =
        match prompt_gateway_secret("BlueBubbles password", "BlueBubbles server password.")? {
            SetupAction::Submit(value) => value.trim().to_string(),
            SetupAction::Back => return run_gateway_setup(),
        };
    if password.is_empty() {
        bail!("BlueBubbles password is required");
    }
    let default_webhook_path = if channel == IMESSAGE_CHANNEL {
        "/imessage-webhook"
    } else {
        "/bluebubbles-webhook"
    };
    let webhook_path = match prompt_text(
        "Webhook path",
        "HTTP callback path configured in BlueBubbles Server.",
        default_webhook_path,
        Some(default_webhook_path),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim();
            if trimmed.trim_matches('/').is_empty() {
                show_setup_message(
                    "BlueBubbles webhook path required",
                    "Use a non-empty webhook path such as /bluebubbles-webhook or /imessage-webhook.",
                )?;
                return run_bluebubbles_setup(channel);
            }
            if trimmed.starts_with('/') {
                trimmed.to_string()
            } else {
                format!("/{trimmed}")
            }
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let webhook_url = match prompt_text(
        "Reachable BlueBubbles webhook URL",
        "Optional webhook base URL reachable by BlueBubbles Server. DuckAgent appends the callback path. Leave empty to register the automatic local tunnel target.",
        "",
        None,
        false,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            (!value.trim().is_empty()).then(|| gateway_callback_url(value.trim(), &webhook_path))
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let dm_policy = match prompt_text(
        "DM policy",
        "open, allowlist, or disabled. allowlist uses Allowed handles below.",
        "open",
        Some("open"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            match value.as_str() {
                "open" => "open".to_string(),
                "allowlist" | "allow-list" => "allowlist".to_string(),
                "disabled" | "off" => "disabled".to_string(),
                _ => {
                    show_setup_message(
                        "BlueBubbles DM policy required",
                        "Use open, allowlist, or disabled.",
                    )?;
                    return run_bluebubbles_setup(channel);
                }
            }
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let group_policy = match prompt_text(
        "Group policy",
        "allowlist, open, or disabled. allowlist uses Allowed chat GUIDs below.",
        "allowlist",
        Some("allowlist"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            match value.as_str() {
                "open" => "open".to_string(),
                "allowlist" | "allow-list" => "allowlist".to_string(),
                "disabled" | "off" => "disabled".to_string(),
                _ => {
                    show_setup_message(
                        "BlueBubbles group policy required",
                        "Use open, allowlist, or disabled.",
                    )?;
                    return run_bluebubbles_setup(channel);
                }
            }
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed handles",
        "Optional comma-separated phone numbers or emails. Empty allows all DMs when DM policy is open.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_chats = match prompt_optional_csv(
        "Allowed group chats",
        "Optional comma-separated BlueBubbles chat GUIDs. Required when group policy is allowlist.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    if dm_policy == "allowlist" && allowed_users.is_empty() {
        show_setup_message(
            "Allowed BlueBubbles handles required",
            "DM policy allowlist requires at least one phone number or email handle.",
        )?;
        return run_bluebubbles_setup(channel);
    }
    if group_policy == "allowlist" && allowed_chats.is_empty() {
        show_setup_message(
            "Allowed BlueBubbles group chats required",
            "Group policy allowlist requires at least one BlueBubbles chat GUID.",
        )?;
        return run_bluebubbles_setup(channel);
    }
    let auth_key = channel.to_string();
    let webhook_endpoint = webhook_url
        .clone()
        .unwrap_or_else(|| format!("http://{bind}{webhook_path}"));
    let public_or_local_label = if webhook_url.is_some() {
        "Reachable webhook URL"
    } else {
        "Local tunnel target"
    };
    let allowed_users_label = if allowed_users.is_empty() {
        "all DM handles when DM policy is open".to_string()
    } else {
        allowed_users.join(", ")
    };
    let allowed_chats_label = if allowed_chats.is_empty() {
        match group_policy.as_str() {
            "open" => "all group chats".to_string(),
            "disabled" => "group messages disabled".to_string(),
            _ => "none".to_string(),
        }
    } else {
        allowed_chats.join(", ")
    };
    let namespace_note = if channel == IMESSAGE_CHANNEL {
        "Namespace: iMessage alias over BlueBubbles with independent sessions"
    } else {
        "Namespace: BlueBubbles channel sessions"
    };
    let review = vec![
        format!("Channel: {channel}"),
        "Transport: BlueBubbles REST API + password-authenticated webhook".to_string(),
        format!("BlueBubbles server URL: {server_url}"),
        format!("{public_or_local_label}: {webhook_endpoint}?password=<BlueBubbles password>"),
        format!("Local tunnel target: http://{bind}{webhook_path}"),
        format!("Webhook path: {webhook_path}"),
        namespace_note.to_string(),
        format!("DM policy: {dm_policy}"),
        format!("Allowed handles: {allowed_users_label}"),
        format!("Group policy: {group_policy}"),
        format!("Allowed group chats: {allowed_chats_label}"),
        "Webhook registration: adapter registers new-message and updated-message events with BlueBubbles Server".to_string(),
        "Private API: typing indicators and reply metadata are used only when the server reports helper support".to_string(),
        "Media: inbound attachments download through BlueBubbles; local outbound MEDIA paths upload as native iMessage attachments".to_string(),
        "Approvals: explicit text fallback commands; iMessage has no native approval buttons".to_string(),
    ];
    match prompt_confirm("Review BlueBubbles gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: channel.to_string(),
            password: Some(password),
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("dm_policy".to_string(), dm_policy);
    extra.insert("group_policy".to_string(), group_policy);
    extra.insert("webhook_path".to_string(), webhook_path.clone());
    extra.insert("webhook_url".to_string(), webhook_endpoint.clone());
    let mut next = current_config;
    next.bind = Some(bind.clone());
    next.channels.insert(
        channel.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("bluebubbles_rest_webhook".to_string()),
            api_base: Some(server_url),
            allowed_users,
            allowed_chats,
            extra,
            ..Default::default()
        },
    );
    save_gateway_config(&next)?;
    show_setup_message(
        "BlueBubbles gateway configured",
        &format!(
            "BlueBubbles webhook registration will use {webhook_endpoint}?password=<password> when the gateway service starts."
        ),
    )?;
    Ok(true)
}

fn run_email_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let address = match prompt_text(
        "Email address",
        "Agent mailbox address.",
        "agent@example.com",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_ascii_lowercase();
            if !trimmed.contains('@')
                || trimmed.starts_with('@')
                || trimmed.ends_with('@')
                || trimmed.chars().any(char::is_whitespace)
            {
                bail!("Email address must look like agent@example.com");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let password = match prompt_gateway_secret(
        "Email password",
        "Mailbox password or app password. For Gmail, use an app password.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let imap_host = match prompt_text(
        "IMAP host",
        "IMAP server host, for example imap.gmail.com or outlook.office365.com.",
        "imap.gmail.com",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty()
                || trimmed.contains("://")
                || trimmed.chars().any(char::is_whitespace)
            {
                bail!("IMAP host must be a host such as imap.gmail.com");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let smtp_host = match prompt_text(
        "SMTP host",
        "SMTP server host, for example smtp.gmail.com or smtp.office365.com.",
        "smtp.gmail.com",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty()
                || trimmed.contains("://")
                || trimmed.chars().any(char::is_whitespace)
            {
                bail!("SMTP host must be a host such as smtp.gmail.com");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed email senders",
        "Optional comma-separated email addresses. Empty allows all non-automated senders.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let skip_attachments = match prompt_text(
        "Skip attachments",
        "true disables inbound attachment ingestion for this mailbox.",
        "false",
        Some("false"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_ascii_lowercase();
            if !matches!(trimmed.as_str(), "true" | "false") {
                bail!("Skip attachments must be true or false");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let poll_seconds = match prompt_text(
        "Email poll interval seconds",
        "How often to check IMAP for unread mail.",
        "30",
        Some("30"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim();
            let seconds: u64 = trimmed
                .parse()
                .map_err(|_| anyhow!("Email poll interval must be a number of seconds"))?;
            if !(10..=3600).contains(&seconds) {
                bail!("Email poll interval must be between 10 and 3600 seconds");
            }
            seconds.to_string()
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_key = EMAIL_CHANNEL.to_string();
    let allowed_summary = if allowed_users.is_empty() {
        "all non-automated senders".to_string()
    } else {
        allowed_users.join(", ")
    };
    let attachment_summary = if skip_attachments == "true" {
        "disabled".to_string()
    } else {
        "enabled with gateway media limits".to_string()
    };
    let review = vec![
        format!("Channel: {EMAIL_CHANNEL}"),
        "Transport: direct_imap_smtp".to_string(),
        "Connection: IMAP polling + SMTP send; no local callback URL or DuckAgent port".to_string(),
        format!("Mailbox: {address}"),
        format!("IMAP: {imap_host}:993"),
        format!("SMTP: {smtp_host}:587 STARTTLS"),
        format!("Poll interval: {poll_seconds}s"),
        format!("Allowed senders: {allowed_summary}"),
        format!("Attachments: {attachment_summary}"),
        "Reply threading: Message-ID/In-Reply-To/References when mail headers supply them"
            .to_string(),
    ];
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: EMAIL_CHANNEL.to_string(),
            username: Some(address),
            password: Some(password),
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("skip_attachments".to_string(), skip_attachments);
    extra.insert("imap_host".to_string(), imap_host);
    extra.insert("imap_port".to_string(), "993".to_string());
    extra.insert("smtp_host".to_string(), smtp_host);
    extra.insert("smtp_port".to_string(), "587".to_string());
    extra.insert("poll_seconds".to_string(), poll_seconds);
    let mut next = current_config;
    next.channels.insert(
        EMAIL_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("direct_imap_smtp".to_string()),
            allowed_users,
            extra,
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "Email gateway configured",
        "Email will be checked via IMAP and replies will be sent via SMTP when the gateway service starts. No local callback port is required.",
    )?;
    Ok(true)
}

fn run_sms_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bind = default_callback_gateway_bind(current_config.bind.as_deref())?;
    let account_sid = match prompt_text(
        "Twilio account SID",
        "Twilio-compatible account SID.",
        "ACxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_string();
            if !trimmed.starts_with("AC")
                || trimmed.len() < 10
                || trimmed.chars().any(char::is_whitespace)
            {
                bail!("Twilio account SID must start with AC and must not contain whitespace");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_token =
        match prompt_gateway_secret("Twilio auth token", "Twilio-compatible auth token.")? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return run_gateway_setup(),
        };
    let from_number = match prompt_text(
        "From phone number",
        "E.164 phone number used for outbound replies.",
        "+15551234567",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_string();
            let digits = trimmed.strip_prefix('+').unwrap_or_default();
            if digits.len() < 8
                || digits.len() > 15
                || !digits.chars().all(|ch| ch.is_ascii_digit())
            {
                bail!("From phone number must be E.164, for example +15551234567");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let webhook_url = match prompt_text(
        "Public SMS webhook base URL",
        "Public/reachable base URL configured for Twilio-compatible inbound webhooks. DuckAgent derives /sms/twilio unless you paste exact /sms/twilio or /webhooks/twilio.",
        "",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                bail!("Twilio webhook URL is required for signature validation");
            }
            let trimmed = trimmed.trim_end_matches('/');
            if trimmed.ends_with("/sms/twilio") || trimmed.ends_with("/webhooks/twilio") {
                trimmed.to_string()
            } else {
                gateway_callback_url(trimmed, "/sms/twilio")
            }
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let webhook_secret = match prompt_optional_secret(
        "SMS JSON bridge secret",
        "Optional shared secret for /sms/events JSON bridge. Twilio /sms/twilio still uses Twilio signature validation.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed SMS senders",
        "Optional comma-separated E.164 numbers. Empty allows all senders.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_key = SMS_CHANNEL.to_string();
    let allowed_summary = if allowed_users.is_empty() {
        "all senders".to_string()
    } else {
        allowed_users.join(", ")
    };
    let local_twilio_path = if webhook_url.ends_with("/webhooks/twilio") {
        "/webhooks/twilio"
    } else {
        "/sms/twilio"
    };
    let review = vec![
        format!("Channel: {SMS_CHANNEL}"),
        "Transport: twilio_webhook".to_string(),
        format!("Twilio inbound endpoint: {webhook_url}"),
        format!("Local tunnel target: http://{bind}{local_twilio_path}"),
        format!("JSON bridge endpoint: http://{bind}/sms/events"),
        "Twilio signature: validated against the public inbound endpoint".to_string(),
        format!(
            "JSON bridge auth: {}",
            if webhook_secret.is_some() {
                "X-DuckAgent-SMS-Secret or ?secret="
            } else {
                "disabled unless an SMS JSON bridge secret is configured"
            }
        ),
        format!("Allowed senders: {allowed_summary}"),
        "MMS: MediaUrl attachments are downloaded within gateway media limits".to_string(),
        "Outbound: Twilio-compatible Messages.json with 1600-character text chunking".to_string(),
    ];
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    let mut credential_extra = BTreeMap::new();
    credential_extra.insert("from_number".to_string(), from_number.clone());
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: SMS_CHANNEL.to_string(),
            app_id: Some(account_sid),
            token: Some(auth_token),
            webhook_secret,
            extra: credential_extra,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("from_number".to_string(), from_number);
    extra.insert("webhook_url".to_string(), webhook_url.clone());
    let mut next = current_config;
    next.bind = Some(bind.clone());
    next.channels.insert(
        SMS_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("twilio_webhook".to_string()),
            api_base: Some("https://api.twilio.com/2010-04-01/Accounts".to_string()),
            allowed_users,
            extra,
            ..Default::default()
        },
    );
    save_gateway_config(&next)?;
    show_setup_message(
        "SMS gateway configured",
        &format!(
            "Set the Twilio inbound message webhook URL to {webhook_url}. For local development, forward that public URL to http://{bind}{local_twilio_path}."
        ),
    )?;
    Ok(true)
}

fn run_msgraph_webhook_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bind = default_callback_gateway_bind(current_config.bind.as_deref())?;
    let notification_url = match prompt_text(
        "Public Graph notificationUrl",
        "Public/reachable Microsoft Graph subscription notificationUrl. DuckAgent appends /msgraph/webhook.",
        "",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                bail!("Graph notification URL is required for Microsoft Graph subscriptions");
            }
            gateway_callback_url(trimmed, "/msgraph/webhook")
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let bridge_url = match prompt_text(
        "Graph bridge URL",
        "Optional HTTP bridge base URL for outbound Graph replies. Leave empty for notification-only mode.",
        "",
        None,
        false,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            (!value.trim().is_empty()).then(|| value.trim().trim_end_matches('/').to_string())
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let bridge_token = match prompt_optional_secret(
        "Graph bridge token",
        "Optional bearer token for the outbound Graph bridge.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let client_state = match prompt_optional_secret(
        "Graph clientState",
        "Optional clientState secret. If set, webhook notifications must match it.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_resources = match prompt_optional_csv(
        "Allowed Graph resources",
        "Optional comma-separated resource prefixes such as chats/, teams/, users/. Empty allows all verified notifications.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_key = MSGRAPH_WEBHOOK_CHANNEL.to_string();
    let local_endpoint = format!("http://{bind}/msgraph/webhook");
    let client_state_label = if client_state.is_some() {
        "configured; notifications must match clientState".to_string()
    } else {
        "not configured; Graph validation still works but resource filtering is the main guard"
            .to_string()
    };
    let allowed_resources_label = if allowed_resources.is_empty() {
        "all resources that pass clientState verification".to_string()
    } else {
        allowed_resources.join(", ")
    };
    let mut review = vec![
        format!("Channel: {MSGRAPH_WEBHOOK_CHANNEL}"),
        "Transport: graph_webhook".to_string(),
        format!("Graph notificationUrl: {notification_url}"),
        format!("Local tunnel target: {local_endpoint}"),
        format!("ClientState: {client_state_label}"),
        format!("Accepted resources: {allowed_resources_label}"),
        "Gateway access: resource filtering is handled inside the MS Graph adapter, not by chat allowlists".to_string(),
    ];
    if let Some(bridge_url) = bridge_url.as_deref() {
        review.push(format!("Outbound bridge: {bridge_url}/send"));
    } else {
        review.push("Outbound bridge: disabled".to_string());
    }
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: MSGRAPH_WEBHOOK_CHANNEL.to_string(),
            token: bridge_token,
            webhook_secret: client_state,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut next = current_config;
    next.bind = Some(bind.clone());
    let mut extra = BTreeMap::new();
    extra.insert("notification_url".to_string(), notification_url.clone());
    if !allowed_resources.is_empty() {
        extra.insert(
            "accepted_resources".to_string(),
            allowed_resources.join(","),
        );
    }
    next.channels.insert(
        MSGRAPH_WEBHOOK_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("graph_webhook".to_string()),
            api_base: bridge_url,
            extra,
            ..Default::default()
        },
    );
    save_gateway_config(&next)?;
    show_setup_message(
        "MS Graph webhook gateway configured",
        &format!(
            "Set Microsoft Graph subscription notificationUrl to {notification_url}. For local development, forward that public URL to {local_endpoint}."
        ),
    )?;
    Ok(true)
}

fn run_msteams_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bind = default_callback_gateway_bind(current_config.bind.as_deref())?;
    let messaging_endpoint = match prompt_text(
        "Public Teams messaging endpoint",
        "Public HTTPS Bot Framework messaging endpoint configured in Azure/Bot Framework. DuckAgent appends /api/messages.",
        "",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                bail!("Teams messaging endpoint is required for Bot Framework setup");
            }
            let endpoint = gateway_callback_url(trimmed, "/api/messages");
            if !endpoint.starts_with("https://") {
                bail!(
                    "Teams messaging endpoint must be a public HTTPS URL ending in /api/messages"
                );
            }
            endpoint
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let bot_app_id = match prompt_text(
        "Teams bot app id",
        "Microsoft Bot Framework app id.",
        "00000000-0000-0000-0000-000000000000",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => value.trim().to_string(),
        SetupAction::Back => return run_gateway_setup(),
    };
    let client_secret = match prompt_gateway_secret(
        "Teams bot client secret",
        "Microsoft Bot Framework app client secret. DuckAgent exchanges it for short-lived Bot Framework access tokens.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let tenant_id = match prompt_text(
        "Teams tenant id",
        "Microsoft Entra tenant id for the Bot Framework app.",
        "00000000-0000-0000-0000-000000000000",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => value.trim().to_string(),
        SetupAction::Back => return run_gateway_setup(),
    };
    let service_url = match prompt_text(
        "Teams service URL",
        "Optional advanced default Bot Framework serviceUrl for proactive sends without a cached inbound conversation. Leave empty for normal inbound-cached sends.",
        "",
        None,
        false,
        true,
        false,
    )? {
        SetupAction::Submit(value) => (!value.trim().is_empty()).then(|| value.trim().to_string()),
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_chats = match prompt_optional_csv(
        "Allowed Teams conversations",
        "Optional comma-separated conversation ids. Empty allows all conversations.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed Teams users",
        "Optional comma-separated Bot Framework user ids. Empty allows all users.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_key = MSTEAMS_CHANNEL.to_string();
    let teams_conversations_summary = if allowed_chats.is_empty() {
        "all conversations".to_string()
    } else {
        allowed_chats.join(", ")
    };
    let teams_users_summary = if allowed_users.is_empty() {
        "all users".to_string()
    } else {
        allowed_users.join(", ")
    };
    let service_url_summary = service_url
        .as_deref()
        .unwrap_or("inbound conversation cache; no proactive default serviceUrl");
    let review = vec![
        format!("Channel: {MSTEAMS_CHANNEL}"),
        "Transport: bot_framework".to_string(),
        format!("Teams messaging endpoint: {messaging_endpoint}"),
        format!("Local tunnel target: http://{bind}/api/messages"),
        format!("Tenant id: {tenant_id}"),
        format!("Proactive serviceUrl: {service_url_summary}"),
        format!("Allowed conversations: {teams_conversations_summary}"),
        format!("Allowed users: {teams_users_summary}"),
        "Auth: Bot Framework OAuth client credentials".to_string(),
        "HTTP paths: primary /api/messages; /teams/events and /msteams/events stay as compatibility aliases".to_string(),
        "Service URL safety: bearer token is sent only to allowed Bot Framework service hosts".to_string(),
        "Approvals: Adaptive Card buttons plus text fallback commands".to_string(),
    ];
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: MSTEAMS_CHANNEL.to_string(),
            app_id: Some(bot_app_id),
            client_secret: Some(client_secret),
            extra: {
                let mut extra = BTreeMap::new();
                extra.insert("tenant_id".to_string(), tenant_id.clone());
                extra
            },
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("messaging_endpoint".to_string(), messaging_endpoint.clone());
    extra.insert("tenant_id".to_string(), tenant_id);
    if let Some(service_url) = service_url {
        extra.insert("service_url".to_string(), service_url);
    }
    let mut next = current_config;
    next.bind = Some(bind.clone());
    next.channels.insert(
        MSTEAMS_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("bot_framework".to_string()),
            allowed_users,
            allowed_chats,
            extra,
            ..Default::default()
        },
    );
    save_gateway_config(&next)?;
    show_setup_message(
        "Teams gateway configured",
        &format!(
            "Set the Bot Framework messaging endpoint to {messaging_endpoint}. For local development, forward that public URL to http://{bind}/api/messages."
        ),
    )?;
    Ok(true)
}

fn run_googlechat_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let previous_subscription = current_config
        .channels
        .get(GOOGLECHAT_CHANNEL)
        .and_then(|channel| channel.extra.get("pubsub_subscription"))
        .map(String::as_str)
        .unwrap_or_default();
    let pubsub_subscription = match prompt_text(
        "Google Chat Pub/Sub subscription",
        "Pub/Sub subscription path for Google Chat app events, for example projects/my-project/subscriptions/duckagent-chat.",
        previous_subscription,
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                bail!("Google Chat Pub/Sub subscription is required");
            }
            if google_chat_subscription_project(trimmed).is_none() {
                bail!(
                    "Google Chat Pub/Sub subscription must look like projects/<project>/subscriptions/<subscription>"
                );
            }
            trimmed.to_string()
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let project_id = google_chat_subscription_project(&pubsub_subscription)
        .unwrap_or_default()
        .to_string();
    let access_token = match prompt_gateway_secret(
        "Google Chat access token",
        "Bearer access token with Google Chat send and Pub/Sub pull/ack access.",
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_string();
            if value.is_empty() {
                bail!("Google Chat access token is required");
            }
            value
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_spaces = match prompt_optional_csv(
        "Allowed Google Chat spaces",
        "Optional comma-separated space names such as spaces/AAA. Empty allows all spaces.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed Google Chat users",
        "Optional comma-separated sender resource names. Empty allows all users.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_key = GOOGLECHAT_CHANNEL.to_string();
    let allowed_spaces_summary = if allowed_spaces.is_empty() {
        "all spaces".to_string()
    } else {
        allowed_spaces.join(", ")
    };
    let allowed_users_summary = if allowed_users.is_empty() {
        "all users".to_string()
    } else {
        allowed_users.join(", ")
    };
    let review = vec![
        format!("Channel: {GOOGLECHAT_CHANNEL}"),
        "Transport: pubsub_rest (Cloud Pub/Sub pull; no public callback URL)".to_string(),
        format!("GCP project: {project_id}"),
        format!("Pub/Sub subscription: {pubsub_subscription}"),
        "Gateway listener: not used for guided Pub/Sub setup; no local callback bind is retained".to_string(),
        format!("Allowed spaces: {allowed_spaces_summary}"),
        format!("Allowed users: {allowed_users_summary}"),
        "Google Chat API config: Connection settings = Cloud Pub/Sub topic, not HTTP endpoint".to_string(),
        "Topic IAM: chat-api-push@system.gserviceaccount.com needs Pub/Sub Publisher".to_string(),
        "Subscription IAM: the token identity needs Pub/Sub Subscriber on this subscription".to_string(),
        "Credential note: refresh the bearer token outside DuckAgent or update auth.json before expiry".to_string(),
    ];
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: GOOGLECHAT_CHANNEL.to_string(),
            token: Some(access_token),
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut next = current_config;
    let mut extra = BTreeMap::new();
    extra.insert(
        "pubsub_subscription".to_string(),
        pubsub_subscription.clone().into(),
    );
    extra.insert("project_id".to_string(), project_id.into());
    next.channels.insert(
        GOOGLECHAT_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("pubsub_rest".to_string()),
            api_base: Some("https://chat.googleapis.com".to_string()),
            allowed_users,
            allowed_chats: allowed_spaces,
            extra,
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "Google Chat gateway configured",
        "Configuration saved. Google Chat events will be pulled from Pub/Sub when the gateway service starts.",
    )?;
    Ok(true)
}

fn google_chat_subscription_project(subscription: &str) -> Option<&str> {
    let mut parts = subscription.split('/');
    let projects = parts.next()?;
    let project = parts.next()?;
    let subscriptions = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if projects == "projects"
        && !project.trim().is_empty()
        && subscriptions == "subscriptions"
        && !name.trim().is_empty()
    {
        Some(project)
    } else {
        None
    }
}

fn run_line_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bind = default_callback_gateway_bind(current_config.bind.as_deref())?;
    let previous_webhook_url = current_config
        .channels
        .get(LINE_CHANNEL)
        .and_then(|channel| channel.extra.get("webhook_url"))
        .map(String::as_str)
        .unwrap_or_default();
    let webhook_url = match prompt_text(
        "Public LINE webhook URL",
        "Public HTTPS LINE Messaging API webhook URL. DuckAgent appends /line/webhook unless you paste exact /line/webhook or /line/events.",
        previous_webhook_url,
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                bail!("LINE webhook URL is required for Messaging API setup");
            }
            let trimmed = trimmed.trim_end_matches('/');
            let endpoint =
                if trimmed.ends_with("/line/webhook") || trimmed.ends_with("/line/events") {
                    trimmed.to_string()
                } else {
                    gateway_callback_url(trimmed, "/line/webhook")
                };
            if !endpoint.starts_with("https://") {
                bail!("LINE webhook URL must be public HTTPS for Messaging API webhook delivery");
            }
            endpoint
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let channel_access_token = match prompt_gateway_secret(
        "LINE channel access token",
        "Long-lived Messaging API channel access token.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let channel_secret = match prompt_gateway_secret(
        "LINE channel secret",
        "Used to verify x-line-signature on inbound webhooks.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let dm_policy = match prompt_text(
        "LINE DM policy",
        "pairing, open, allowlist, or disabled. pairing asks new DM users to get owner approval with a one-time code.",
        "pairing",
        Some("pairing"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase().replace('-', "_");
            if !matches!(
                value.as_str(),
                "pairing" | "open" | "allowlist" | "disabled"
            ) {
                bail!("LINE DM policy must be pairing, open, allowlist, or disabled");
            }
            value
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = if dm_policy == "allowlist" {
        match prompt_optional_csv(
            "Allowed LINE senders",
            "Comma-separated LINE userId values. Required when DM policy is allowlist; when set, the same sender allowlist also limits group/room messages.",
        )? {
            SetupAction::Submit(value) => {
                if value.is_empty() {
                    bail!("LINE DM policy allowlist requires at least one allowed LINE user");
                }
                value
            }
            SetupAction::Back => return run_gateway_setup(),
        }
    } else {
        Vec::new()
    };
    let group_policy = match prompt_text(
        "LINE group/room policy",
        "mention, open, allowlist, or disabled. mention requires a native @Bot mention in group/room chats.",
        "mention",
        Some("mention"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            if !matches!(
                value.as_str(),
                "mention" | "open" | "allowlist" | "disabled"
            ) {
                bail!("LINE group/room policy must be mention, open, allowlist, or disabled");
            }
            value
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_chats = if group_policy == "allowlist" {
        match prompt_optional_csv(
            "Allowed LINE groups/rooms",
            "Comma-separated LINE groupId/roomId values. Required when group/room policy is allowlist.",
        )? {
            SetupAction::Submit(value) => {
                if value.is_empty() {
                    bail!(
                        "LINE group/room policy allowlist requires at least one allowed LINE chat"
                    );
                }
                value
            }
            SetupAction::Back => return run_gateway_setup(),
        }
    } else {
        Vec::new()
    };
    let auth_key = LINE_CHANNEL.to_string();
    let local_line_path = if webhook_url.ends_with("/line/events") {
        "/line/events"
    } else {
        "/line/webhook"
    };
    let review = vec![
        format!("Channel: {LINE_CHANNEL}"),
        "Transport: messaging_api".to_string(),
        format!("LINE webhook URL: {webhook_url}"),
        format!("Local tunnel target: http://{bind}{local_line_path}"),
        "Signature: x-line-signature verified with channel secret".to_string(),
        format!("DM policy: {dm_policy}"),
        format!(
            "Allowed LINE senders: {}",
            if allowed_users.is_empty() {
                match dm_policy.as_str() {
                    "pairing" => "none; new DM users receive a pairing code".to_string(),
                    "open" => "all DM users".to_string(),
                    "disabled" => "DM disabled".to_string(),
                    _ => "none".to_string(),
                }
            } else {
                allowed_users.join(", ")
            }
        ),
        format!("Group/room policy: {group_policy}"),
        format!(
            "Allowed groups/rooms: {}",
            if allowed_chats.is_empty() {
                match group_policy.as_str() {
                    "mention" => "all groups/rooms, gated by native @Bot mention".to_string(),
                    "open" => "all groups/rooms".to_string(),
                    "disabled" => "groups/rooms disabled".to_string(),
                    _ => "none".to_string(),
                }
            } else {
                allowed_chats.join(", ")
            }
        ),
        "Pairing approval: use `duck gateway pairing approve <code>` when DM policy is pairing"
            .to_string(),
        "Local media: uses the same HTTPS public base for temporary /line/media URLs".to_string(),
    ];
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: LINE_CHANNEL.to_string(),
            token: Some(channel_access_token),
            webhook_secret: Some(channel_secret),
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut next = current_config;
    next.bind = Some(bind.clone());
    let mut extra = BTreeMap::new();
    extra.insert("webhook_url".to_string(), webhook_url.clone().into());
    extra.insert("group_policy".to_string(), group_policy.clone());
    next.channels.insert(
        LINE_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("messaging_api".to_string()),
            api_base: Some("https://api.line.me".to_string()),
            allowed_users,
            allowed_chats,
            extra,
            access: GatewayAccessConfig {
                dm_policy,
                group_policy: match group_policy.as_str() {
                    "disabled" => "disabled".to_string(),
                    "allowlist" => "allowlist".to_string(),
                    _ => "open".to_string(),
                },
                require_mention: group_policy == "mention",
            },
            ..Default::default()
        },
    );
    save_gateway_config(&next)?;
    show_setup_message(
        "LINE gateway configured",
        &format!(
            "Set the LINE Messaging API webhook URL to {webhook_url}. For local development, forward that public URL to http://{bind}{local_line_path}."
        ),
    )?;
    Ok(true)
}

fn run_irc_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let server = match prompt_text(
        "IRC server",
        "IRC server hostname, for example irc.libera.chat.",
        "irc.libera.chat",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => value.trim().to_string(),
        SetupAction::Back => return run_gateway_setup(),
    };
    let port = match prompt_text(
        "IRC port",
        "Use 6697 for TLS or 6667 for plaintext.",
        "6697",
        Some("6697"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => value.trim().to_string(),
        SetupAction::Back => return run_gateway_setup(),
    };
    let tls = match prompt_text(
        "Use TLS",
        "true is recommended for public IRC networks.",
        "true",
        Some("true"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            if !matches!(value.as_str(), "true" | "false") {
                bail!("Use TLS must be true or false");
            }
            value
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let nickname = match prompt_text(
        "IRC nickname",
        "Bot nickname used to connect and for mention gating.",
        "duckagent",
        Some("duckagent"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
                bail!("IRC nickname must be a single nickname without whitespace");
            }
            trimmed.to_string()
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let channels = match prompt_optional_csv(
        "IRC channels",
        "Comma-separated channels to join, for example #duckagent,#ops.",
    )? {
        SetupAction::Submit(value) => {
            let mut normalized = Vec::new();
            for channel in value {
                let trimmed = channel.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed.chars().any(char::is_whitespace) {
                    bail!("IRC channel names must not contain whitespace");
                }
                let bare = trimmed.trim_start_matches(&['#', '&'][..]);
                if bare.is_empty() {
                    bail!("IRC channel names must include text after # or &");
                }
                if trimmed.starts_with('#') || trimmed.starts_with('&') {
                    normalized.push(trimmed.to_string());
                } else {
                    normalized.push(format!("#{trimmed}"));
                }
            }
            normalized
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    if channels.is_empty() {
        return Err(anyhow!("IRC setup requires at least one channel"));
    }
    let allowed_users = match prompt_optional_csv(
        "Allowed IRC senders",
        "Optional comma-separated nicks or full nick!user@host identities. Empty allows all senders.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let server_password = match prompt_optional_secret(
        "IRC server password",
        "Optional PASS value for server or bouncer authentication.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let nickserv_password = match prompt_optional_secret(
        "NickServ password",
        "Optional NickServ IDENTIFY password.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let require_mention = match prompt_text(
        "Require mention",
        "true means channel messages must mention the bot nickname.",
        "true",
        Some("true"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            if !matches!(value.as_str(), "true" | "false") {
                bail!("Require mention must be true or false");
            }
            value
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_key = IRC_CHANNEL.to_string();
    let review = vec![
        format!("Channel: {IRC_CHANNEL}"),
        "Transport: irc_tcp (outbound IRC client; no local callback port)".to_string(),
        format!("Server: {server}:{port}"),
        format!("TLS: {tls}"),
        format!("Nickname: {nickname}"),
        format!("Join channels: {}", channels.join(", ")),
        format!("Require mention in channels: {require_mention}"),
        format!(
            "Allowed senders: {}",
            if allowed_users.is_empty() {
                "all".to_string()
            } else {
                allowed_users.join(", ")
            }
        ),
    ];
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: IRC_CHANNEL.to_string(),
            username: Some(nickname.clone()),
            token: server_password,
            password: nickserv_password,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("server".to_string(), server.clone());
    extra.insert("port".to_string(), port);
    extra.insert("tls".to_string(), tls);
    extra.insert("nickname".to_string(), nickname);
    extra.insert("require_mention".to_string(), require_mention);
    let mut next = current_config;
    next.channels.insert(
        IRC_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("irc_tcp".to_string()),
            api_base: Some(server),
            allowed_users,
            allowed_chats: channels,
            extra,
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "IRC gateway configured",
        "Configuration saved. The IRC adapter will connect when the gateway service starts.",
    )?;
    Ok(true)
}

fn run_twitch_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let username = match prompt_text(
        "Twitch bot username",
        "Bot account username used for Twitch IRC.",
        "duckagent",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => value.trim().to_ascii_lowercase(),
        SetupAction::Back => return run_gateway_setup(),
    };
    let token = match prompt_gateway_secret(
        "Twitch OAuth token",
        "OAuth token with chat:read and chat:write, usually prefixed oauth:.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let channel = match prompt_text(
        "Twitch channel",
        "Twitch channel to join, without or with leading #.",
        "channel_name",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().trim_start_matches('#').to_ascii_lowercase();
            if value.is_empty() || value.contains(char::is_whitespace) {
                bail!("Twitch channel must be a single channel name");
            }
            format!("#{value}")
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed Twitch senders",
        "Optional comma-separated Twitch usernames or user ids. Empty allows all senders.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let require_mention = match prompt_text(
        "Require mention",
        "true means chat messages must mention the bot username.",
        "true",
        Some("true"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let value = value.trim().to_ascii_lowercase();
            if !matches!(value.as_str(), "true" | "false") {
                bail!("Require mention must be true or false");
            }
            value
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let auth_key = TWITCH_CHANNEL.to_string();
    let review = vec![
        format!("Channel: {TWITCH_CHANNEL}"),
        "Transport: twitch_irc (outbound Twitch IRC; no local callback port)".to_string(),
        format!("Bot username: {username}"),
        format!("Twitch channel: {channel}"),
        "Server: irc.chat.twitch.tv:6697 over TLS".to_string(),
        "Capabilities: twitch.tv/tags + twitch.tv/commands".to_string(),
        "OAuth token: saved with oauth: prefix normalization at runtime".to_string(),
        format!("Require mention in chat: {require_mention}"),
        format!(
            "Allowed senders: {}",
            if allowed_users.is_empty() {
                "all".to_string()
            } else {
                allowed_users.join(", ")
            }
        ),
    ];
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: TWITCH_CHANNEL.to_string(),
            username: Some(username.clone()),
            token: Some(token),
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("server".to_string(), "irc.chat.twitch.tv".to_string());
    extra.insert("port".to_string(), "6697".to_string());
    extra.insert("tls".to_string(), "true".to_string());
    extra.insert("nickname".to_string(), username);
    extra.insert("require_mention".to_string(), require_mention);
    extra.insert(
        "capabilities".to_string(),
        "twitch.tv/tags twitch.tv/commands".to_string(),
    );
    let mut next = current_config;
    next.channels.insert(
        TWITCH_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("twitch_irc".to_string()),
            api_base: Some("irc.chat.twitch.tv".to_string()),
            allowed_users,
            allowed_chats: vec![channel],
            extra,
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "Twitch gateway configured",
        "Configuration saved. The Twitch adapter will connect to Twitch IRC when the gateway service starts.",
    )?;
    Ok(true)
}

fn run_homeassistant_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let base_url = match prompt_text(
        "Home Assistant URL",
        "Home Assistant base URL, for example http://homeassistant.local:8123.",
        "http://homeassistant.local:8123",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().trim_end_matches('/').to_string();
            if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
                bail!("Home Assistant URL must start with http:// or https://");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let token = match prompt_gateway_secret("Home Assistant token", "Long-lived access token.")? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let notify_service = match prompt_text(
        "Notify service",
        "Home Assistant notify service, for example notify.mobile_app_phone or notify.notify.",
        "notify.notify",
        Some("notify.notify"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_string();
            if trimmed.chars().any(char::is_whitespace) || trimmed.split_once('.').is_none() {
                bail!("Notify service must look like notify.notify or notify.mobile_app_phone");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let watch_domains = match prompt_optional_csv(
        "Watched HA domains",
        "Optional comma-separated domains such as sensor,binary_sensor,climate. Empty relies on watched entities or watch all.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let watch_entities = match prompt_optional_csv(
        "Watched HA entities",
        "Optional comma-separated entity ids such as binary_sensor.front_door.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let ignore_entities = match prompt_optional_csv(
        "Ignored HA entities",
        "Optional entity ids that should never trigger the agent, such as sensor.uptime.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let watch_all = match prompt_text(
        "Watch all state changes",
        "true forwards every state_changed event. Use carefully; most homes should prefer domains/entities.",
        "false",
        Some("false"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_ascii_lowercase();
            if !matches!(trimmed.as_str(), "true" | "false") {
                bail!("Watch all state changes must be true or false");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let cooldown_seconds = match prompt_text(
        "Event cooldown seconds",
        "Per-entity cooldown to avoid event floods.",
        "30",
        Some("30"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_string();
            if trimmed.parse::<u64>().is_err() {
                bail!("Event cooldown seconds must be a non-negative integer");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let webhook_secret = match prompt_optional_secret(
        "Command webhook secret",
        "Optional secret for /homeassistant/events and /homeassistant/webhook. Leave empty to keep HTTP command webhooks disabled.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let callback_bind = if webhook_secret.is_some() {
        Some(default_callback_gateway_bind(
            current_config.bind.as_deref(),
        )?)
    } else {
        None
    };
    let command_webhook_enabled = webhook_secret.is_some();
    let auth_key = HOMEASSISTANT_CHANNEL.to_string();
    let websocket_url = if base_url.starts_with("https://") {
        base_url.replacen("https://", "wss://", 1)
    } else {
        base_url.replacen("http://", "ws://", 1)
    } + "/api/websocket";
    let state_intake = if watch_all == "true" {
        "all state_changed events".to_string()
    } else if !watch_domains.is_empty() || !watch_entities.is_empty() {
        "filtered state_changed events".to_string()
    } else {
        "disabled until watched domains/entities or watch_all are configured".to_string()
    };
    let domain_summary = if watch_domains.is_empty() {
        "none".to_string()
    } else {
        watch_domains.join(", ")
    };
    let entity_summary = if watch_entities.is_empty() {
        "none".to_string()
    } else {
        watch_entities.join(", ")
    };
    let ignored_summary = if ignore_entities.is_empty() {
        "none".to_string()
    } else {
        ignore_entities.join(", ")
    };
    let command_webhook_summary = if let Some(bind) = callback_bind.as_deref() {
        format!(
            "enabled at /homeassistant/events and /homeassistant/webhook; local tunnel target: http://{bind}/homeassistant/events"
        )
    } else {
        "disabled; no HTTP command webhook secret configured".to_string()
    };
    let review = vec![
        format!("Channel: {HOMEASSISTANT_CHANNEL}"),
        "Transport: homeassistant_ws_rest".to_string(),
        format!("Home Assistant URL: {base_url}"),
        format!("WebSocket subscription: {websocket_url}"),
        format!("State_changed intake: {state_intake}"),
        format!("Watched domains: {domain_summary}"),
        format!("Watched entities: {entity_summary}"),
        format!("Ignored entities: {ignored_summary}"),
        format!("Cooldown seconds: {cooldown_seconds}"),
        format!("Notify service: {notify_service}"),
        "REST delivery: /api/services/<domain>/<service>".to_string(),
        format!("Command webhook: {command_webhook_summary}"),
        format!(
            "Command webhook auth: {}",
            if command_webhook_enabled {
                "X-DuckAgent-Gateway-Secret, X-HomeAssistant-Secret, or ?secret="
            } else {
                "not enabled"
            }
        ),
        "Typing: no-op; approvals use /approve and /deny notification fallback".to_string(),
    ];
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: HOMEASSISTANT_CHANNEL.to_string(),
            token: Some(token),
            webhook_secret,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("notify_service".to_string(), notify_service);
    if !watch_domains.is_empty() {
        extra.insert("watch_domains".to_string(), watch_domains.join(","));
    }
    if !watch_entities.is_empty() {
        extra.insert("watch_entities".to_string(), watch_entities.join(","));
    }
    if !ignore_entities.is_empty() {
        extra.insert("ignore_entities".to_string(), ignore_entities.join(","));
    }
    extra.insert("watch_all".to_string(), watch_all);
    extra.insert("cooldown_seconds".to_string(), cooldown_seconds);
    if command_webhook_enabled {
        extra.insert("command_webhook_enabled".to_string(), "true".to_string());
    }
    let mut next = current_config;
    if let Some(bind) = callback_bind.as_ref() {
        next.bind = Some(bind.clone());
    }
    next.channels.insert(
        HOMEASSISTANT_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("homeassistant_ws_rest".to_string()),
            api_base: Some(base_url),
            extra,
            ..Default::default()
        },
    );
    if callback_bind.is_none() {
        clear_gateway_bind_if_unused(&mut next);
    }
    save_gateway_config(&next)?;
    show_setup_message(
        "Home Assistant gateway configured",
        "DuckAgent will connect to Home Assistant over WebSocket for state changes and use REST notify for replies.",
    )?;
    Ok(true)
}

fn run_qa_channel_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bind = default_stable_gateway_bind(current_config.bind.as_deref())?;
    let auth_key = QA_CHANNEL.to_string();
    match prompt_gateway_review(QA_CHANNEL, &bind, "fixture_http")? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        &auth_key,
        GatewayCredentialEntry {
            channel: QA_CHANNEL.to_string(),
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut next = current_config;
    next.bind = Some(bind.clone());
    next.channels.insert(
        QA_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("fixture_http".to_string()),
            ..Default::default()
        },
    );
    save_gateway_config(&next)?;
    show_setup_message(
        "QA gateway configured",
        "Use /qa-channel/inbound for deterministic local gateway contract tests.",
    )?;
    Ok(true)
}

struct BridgeSetupSpec {
    channel: &'static str,
    transport: &'static str,
    bridge_title: &'static str,
    bridge_subtitle: &'static str,
    default_bridge: &'static str,
    allowed_chats_title: &'static str,
    allowed_chats_subtitle: &'static str,
    allowed_users_title: &'static str,
    allowed_users_subtitle: &'static str,
    configured_title: &'static str,
    inbound_path: &'static str,
    send_endpoint: &'static str,
    typing_endpoint: Option<&'static str>,
    extra: BTreeMap<String, String>,
    review_extra: Vec<String>,
}

fn run_synology_chat_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bind = default_callback_gateway_bind(current_config.bind.as_deref())?;
    let incoming_webhook_url = match prompt_text(
        "Synology incoming webhook URL",
        "Incoming webhook URL from Synology Chat integration. DuckAgent uses it directly to send replies.",
        "https://chat.example.com/webapi/entry.cgi?...",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_string();
            if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
                bail!("Synology incoming webhook URL must start with http:// or https://");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let bot_username = match prompt_optional_bridge_text(
        "Synology Chat bot username",
        "Optional Synology Chat bot username/account used for self-message filtering.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let webhook_secret = match prompt_optional_secret(
        "Webhook secret",
        "Optional secret expected in X-DuckAgent-Gateway-Secret, X-Synology-Chat-Secret, X-Synology-Secret, or ?secret=.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_chats = match prompt_optional_csv(
        "Allowed Synology channels",
        "Optional Synology Chat channel ids, conversation ids, channel names, or *. Empty allows all incoming channels.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed Synology users",
        "Optional Synology account/user ids or *. Empty allows all senders.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let mut extra = BTreeMap::new();
    let mut review_extra = Vec::new();
    if let Some(bot_username) = bot_username {
        review_extra.push(format!("Bot username: {bot_username}"));
        extra.insert("bot_username".to_string(), bot_username);
    }
    let inbound_path = "/synology-chat/events".to_string();
    let channel_summary = if allowed_chats.is_empty() {
        "all incoming channels".to_string()
    } else {
        allowed_chats.join(", ")
    };
    let user_summary = if allowed_users.is_empty() {
        "all incoming senders".to_string()
    } else {
        allowed_users.join(", ")
    };
    review_extra.extend([
        format!("Channel: {SYNOLOGY_CHAT_CHANNEL}"),
        "Transport: Synology Chat incoming/outgoing webhooks".to_string(),
        "Incoming webhook URL: configured".to_string(),
        format!("Outgoing webhook target: http://{bind}{inbound_path}"),
        "Inbound endpoints: /synology-chat/events and /synology-chat/webhook".to_string(),
        format!(
            "Webhook auth: {}",
            if webhook_secret.is_some() {
                "X-DuckAgent-Gateway-Secret, X-Synology-Chat-Secret, X-Synology-Secret, or ?secret="
            } else {
                "not required"
            }
        ),
        format!("Allowed Synology channels: {channel_summary}"),
        format!("Allowed Synology users: {user_summary}"),
        "Replies: direct Synology incoming webhook payload".to_string(),
        "Typing: no-op by default; approvals use text fallback".to_string(),
    ]);
    match prompt_confirm("Review gateway", &review_extra, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let mut credential_extra = BTreeMap::new();
    credential_extra.insert("incoming_webhook_url".to_string(), incoming_webhook_url);
    let now = now_rfc3339_like();
    save_gateway_credentials(
        SYNOLOGY_CHAT_CHANNEL,
        GatewayCredentialEntry {
            channel: SYNOLOGY_CHAT_CHANNEL.to_string(),
            webhook_secret,
            extra: credential_extra,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    extra.insert("inbound_path".to_string(), inbound_path.clone());
    let mut next = current_config;
    next.bind = Some(bind.clone());
    next.channels.insert(
        SYNOLOGY_CHAT_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("synology_chat_webhook".to_string()),
            allowed_chats,
            allowed_users,
            extra,
            ..Default::default()
        },
    );
    save_gateway_config(&next)?;
    show_setup_message(
        "Synology Chat gateway configured",
        &format!(
            "Set the Synology Chat outgoing webhook URL to http://{bind}{inbound_path}, or map that path through your public/reverse-proxy URL."
        ),
    )?;
    Ok(true)
}

fn run_tlon_setup() -> Result<bool> {
    let ship = match prompt_optional_bridge_text(
        "Tlon ship",
        "Optional default Urbit ship such as ~zod. Empty lets the bridge decide.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let default_channel = match prompt_optional_bridge_text(
        "Tlon default channel",
        "Optional default Tlon/Urbit graph, channel, or desk path used by the bridge.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let mut extra = BTreeMap::new();
    let mut review_extra = Vec::new();
    if let Some(ship) = ship {
        review_extra.push(format!("Ship: {ship}"));
        extra.insert("ship".to_string(), ship);
    }
    if let Some(default_channel) = default_channel {
        review_extra.push(format!("Default channel: {default_channel}"));
        extra.insert("default_channel".to_string(), default_channel);
    }
    review_extra.push("Inbound endpoints: /tlon/events and /tlon/webhook".to_string());
    review_extra.push(
        "Webhook auth: X-DuckAgent-Gateway-Secret, X-Tlon-Secret, X-Urbit-Secret, or ?secret= when configured"
            .to_string(),
    );
    review_extra.push(
        "Bridge responsibilities: Urbit auth, graph/channel/thread mapping, and media upload policy"
            .to_string(),
    );
    review_extra.push(
        "Typing: no-op by default; approvals use text fallback plus approval.commands".to_string(),
    );
    run_bridge_setup(BridgeSetupSpec {
        channel: TLON_CHANNEL,
        transport: "tlon_bridge",
        bridge_title: "Tlon bridge API URL",
        bridge_subtitle: "External HTTP bridge base URL for Tlon/Urbit chat events, threads, media, and replies; this is not the DuckAgent callback bind.",
        default_bridge: "http://127.0.0.1:8095",
        allowed_chats_title: "Allowed Tlon conversations",
        allowed_chats_subtitle: "Optional ship/channel/thread ids or *. Empty allows all conversations accepted by the bridge.",
        allowed_users_title: "Allowed Tlon senders",
        allowed_users_subtitle: "Optional Urbit ship/user ids or *. Empty allows all bridge-accepted senders.",
        configured_title: "Tlon gateway configured",
        inbound_path: "/tlon/events",
        send_endpoint: "/send",
        typing_endpoint: None,
        extra,
        review_extra,
    })
}

fn run_zalo_setup(channel: &'static str) -> Result<bool> {
    let is_oa = channel == ZALO_CHANNEL;
    let app_id = match prompt_optional_bridge_text(
        if is_oa {
            "Zalo app ID"
        } else {
            "Zalo user app ID"
        },
        "Optional Zalo app id used by the bridge. Empty leaves app identity in the bridge.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let account_id = match prompt_optional_bridge_text(
        if is_oa {
            "Zalo OA ID"
        } else {
            "Zalo user/session ID"
        },
        if is_oa {
            "Optional Official Account id used for OA/business messaging."
        } else {
            "Optional Zalo user/session id used for personal/session messaging."
        },
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let mut extra = BTreeMap::new();
    let mut review_extra = Vec::new();
    if let Some(app_id) = app_id {
        review_extra.push(format!("App ID: {app_id}"));
        extra.insert("app_id".to_string(), app_id);
    }
    if let Some(account_id) = account_id {
        review_extra.push(format!(
            "{}: {account_id}",
            if is_oa { "OA ID" } else { "Session ID" }
        ));
        extra.insert(
            if is_oa { "oa_id" } else { "session_id" }.to_string(),
            account_id,
        );
    }
    review_extra.push(format!(
        "Inbound endpoints: {} and {}",
        if is_oa {
            "/zalo/events"
        } else {
            "/zalouser/events"
        },
        if is_oa {
            "/zalo/webhook"
        } else {
            "/zalouser/webhook"
        }
    ));
    review_extra.push(format!(
        "Webhook auth: X-DuckAgent-Gateway-Secret, {}, or ?secret= when configured",
        if is_oa {
            "X-Zalo-Secret"
        } else {
            "X-Zalouser-Secret"
        }
    ));
    review_extra.push(
        "Bridge responsibilities: Zalo auth/session lifecycle, media upload policy, and callback normalization"
            .to_string(),
    );
    review_extra.push(
        "Typing: /typing when the bridge supports it; approvals use text fallback plus approval.commands"
            .to_string(),
    );
    run_bridge_setup(BridgeSetupSpec {
        channel,
        transport: if is_oa {
            "zalo_bridge"
        } else {
            "zalouser_bridge"
        },
        bridge_title: if is_oa {
            "Zalo OA bridge API URL"
        } else {
            "Zalo user bridge API URL"
        },
        bridge_subtitle: if is_oa {
            "External HTTP bridge base URL for Zalo Official Account/business callbacks, media, and replies; this is not the DuckAgent callback bind."
        } else {
            "External HTTP bridge base URL for Zalo personal/session callbacks, media, typing, and replies; this is not the DuckAgent callback bind."
        },
        default_bridge: if is_oa {
            "http://127.0.0.1:8096"
        } else {
            "http://127.0.0.1:8097"
        },
        allowed_chats_title: if is_oa {
            "Allowed Zalo OA chats"
        } else {
            "Allowed Zalo user chats"
        },
        allowed_chats_subtitle: "Optional Zalo user ids, conversation ids, group ids, or *. Empty allows all bridge-accepted chats.",
        allowed_users_title: "Allowed Zalo senders",
        allowed_users_subtitle: "Optional Zalo sender ids or *. Empty allows all bridge-accepted senders.",
        configured_title: if is_oa {
            "Zalo OA gateway configured"
        } else {
            "Zalo user gateway configured"
        },
        inbound_path: if is_oa {
            "/zalo/events"
        } else {
            "/zalouser/events"
        },
        send_endpoint: "/send",
        typing_endpoint: Some("/typing"),
        extra,
        review_extra,
    })
}

fn run_voice_bridge_setup(channel: &'static str) -> Result<bool> {
    let is_talk_voice = channel == TALK_VOICE_CHANNEL;
    let provider = match prompt_optional_bridge_text(
        if is_talk_voice {
            "Talk voice provider"
        } else {
            "Voice call provider"
        },
        "Optional provider label such as sip, livekit, twilio, or custom. Empty leaves provider selection in the bridge.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let mut extra = BTreeMap::new();
    let mut review_extra = Vec::new();
    if let Some(provider) = provider {
        review_extra.push(format!("Provider: {provider}"));
        extra.insert("provider".to_string(), provider);
    }
    review_extra.push(format!(
        "Inbound endpoints: {} and {}",
        if is_talk_voice {
            "/talk-voice/events"
        } else {
            "/voice-call/events"
        },
        if is_talk_voice {
            "/talk-voice/webhook"
        } else {
            "/voice-call/webhook"
        }
    ));
    review_extra.push(format!(
        "Webhook auth: X-DuckAgent-Gateway-Secret, {}, X-Voice-Secret, or ?secret= when configured",
        if is_talk_voice {
            "X-Talk-Voice-Secret"
        } else {
            "X-Voice-Call-Secret"
        }
    ));
    review_extra.push(
        "Bridge responsibilities: call/session lifecycle, STT/TTS, recording/media upload policy, and provider callbacks"
            .to_string(),
    );
    review_extra
        .push("Status: /typing maps to speaking/idle or provider-specific call status".to_string());
    review_extra.push(
        "Approvals: voice UI through the bridge or /approve and /deny text fallback".to_string(),
    );
    run_bridge_setup(BridgeSetupSpec {
        channel,
        transport: if is_talk_voice {
            "talk_voice_bridge"
        } else {
            "voice_call_bridge"
        },
        bridge_title: if is_talk_voice {
            "Talk voice bridge API URL"
        } else {
            "Voice call bridge API URL"
        },
        bridge_subtitle: if is_talk_voice {
            "External HTTP bridge base URL for talk-voice transcripts, recordings, audio routing, and replies; this is not the DuckAgent callback bind."
        } else {
            "External HTTP bridge base URL for call events, transcripts, recordings, call control, and replies; this is not the DuckAgent callback bind."
        },
        default_bridge: if is_talk_voice {
            "http://127.0.0.1:8101"
        } else {
            "http://127.0.0.1:8100"
        },
        allowed_chats_title: if is_talk_voice {
            "Allowed talk voice sessions"
        } else {
            "Allowed call sessions"
        },
        allowed_chats_subtitle: "Optional call ids, room ids, conversation ids, or *. Empty allows all bridge-accepted sessions.",
        allowed_users_title: if is_talk_voice {
            "Allowed talk voice participants"
        } else {
            "Allowed callers"
        },
        allowed_users_subtitle: "Optional caller/participant ids or *. Empty allows all bridge-accepted participants.",
        configured_title: if is_talk_voice {
            "Talk voice gateway configured"
        } else {
            "Voice call gateway configured"
        },
        inbound_path: if is_talk_voice {
            "/talk-voice/events"
        } else {
            "/voice-call/events"
        },
        send_endpoint: "/send",
        typing_endpoint: Some("/typing"),
        extra,
        review_extra,
    })
}

fn prompt_optional_bridge_text(title: &str, subtitle: &str) -> Result<SetupAction<Option<String>>> {
    match prompt_text(title, subtitle, "", None, false, true, false)? {
        SetupAction::Submit(value) => Ok(SetupAction::Submit(
            (!value.trim().is_empty()).then(|| value.trim().to_string()),
        )),
        SetupAction::Back => Ok(SetupAction::Back),
    }
}

fn run_bridge_setup(spec: BridgeSetupSpec) -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bind = default_callback_gateway_bind(current_config.bind.as_deref())?;
    let bridge_url = match prompt_text(
        spec.bridge_title,
        spec.bridge_subtitle,
        spec.default_bridge,
        Some(spec.default_bridge),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().trim_end_matches('/').to_string();
            if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
                bail!("{} must start with http:// or https://", spec.bridge_title);
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let bridge_token = match prompt_optional_secret(
        "Bridge bearer token",
        "Optional bearer token used for outbound bridge calls and protected media downloads.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let webhook_secret = match prompt_optional_secret(
        "Webhook secret",
        "Optional secret expected in X-DuckAgent-Gateway-Secret, channel secret header, or ?secret=.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_chats =
        match prompt_optional_csv(spec.allowed_chats_title, spec.allowed_chats_subtitle)? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return run_gateway_setup(),
        };
    let allowed_users =
        match prompt_optional_csv(spec.allowed_users_title, spec.allowed_users_subtitle)? {
            SetupAction::Submit(value) => value,
            SetupAction::Back => return run_gateway_setup(),
        };
    let mut review = vec![
        format!("Channel: {}", spec.channel),
        format!("Transport: {}", spec.transport),
        format!("Bridge API URL: {bridge_url}"),
        "Bridge API URL is the external platform bridge base URL, not the DuckAgent inbound callback bind".to_string(),
        format!("Local tunnel target: http://{bind}{}", spec.inbound_path),
    ];
    review.extend(spec.review_extra.iter().cloned());
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        spec.channel,
        GatewayCredentialEntry {
            channel: spec.channel.to_string(),
            token: bridge_token,
            webhook_secret,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = spec.extra;
    extra.insert("inbound_path".to_string(), spec.inbound_path.to_string());
    extra.insert("send_endpoint".to_string(), spec.send_endpoint.to_string());
    if let Some(typing_endpoint) = spec.typing_endpoint {
        extra.insert("typing_endpoint".to_string(), typing_endpoint.to_string());
    }
    let mut next = current_config;
    next.bind = Some(bind.clone());
    next.channels.insert(
        spec.channel.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some(spec.transport.to_string()),
            api_base: Some(bridge_url),
            allowed_users,
            allowed_chats,
            extra,
            ..Default::default()
        },
    );
    save_gateway_config(&next)?;
    show_setup_message(
        spec.configured_title,
        &format!(
            "Configure the bridge to POST inbound events to http://{bind}{}, or map that path through your public/reverse-proxy URL.",
            spec.inbound_path
        ),
    )?;
    Ok(true)
}

fn run_yuanbao_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let default_ws_url = "wss://bot-wss.yuanbao.tencent.com/wss/connection";
    let default_api_domain = "https://bot.yuanbao.tencent.com";
    let app_id = match prompt_text(
        "Yuanbao app id",
        "Yuanbao bot APP_ID from Yuanbao PAI / My Bot.",
        "app_xxx",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => value.trim().to_string(),
        SetupAction::Back => return run_gateway_setup(),
    };
    let app_secret = match prompt_gateway_secret(
        "Yuanbao app secret",
        "Yuanbao APP_SECRET used for sign-token and WebSocket auth.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let bot_accounts = match prompt_optional_csv(
        "Yuanbao bot accounts",
        "Optional bot account ids or mention names used to detect @Bot in groups. Empty uses Yuanbao metadata.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let dm_policy = match prompt_text(
        "Yuanbao DM policy",
        "open, allowlist, or disabled for direct messages.",
        "open",
        Some("open"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_ascii_lowercase();
            if !matches!(trimmed.as_str(), "open" | "allowlist" | "disabled") {
                bail!("Yuanbao DM policy must be open, allowlist, or disabled");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let group_policy = match prompt_text(
        "Yuanbao group policy",
        "mention, open, allowlist, or disabled. mention requires @Bot in group chats.",
        "mention",
        Some("mention"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_ascii_lowercase();
            if !matches!(
                trimmed.as_str(),
                "mention" | "open" | "allowlist" | "disabled"
            ) {
                bail!("Yuanbao group policy must be mention, open, allowlist, or disabled");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_chats = match prompt_optional_csv(
        "Allowed Yuanbao conversations",
        "Optional direct:<account> or group:<group_code> allowlist. Empty allows all conversations.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed Yuanbao users",
        "Optional sender account allowlist. Empty allows all senders.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    if (dm_policy == "allowlist" || group_policy == "allowlist")
        && allowed_chats.is_empty()
        && allowed_users.is_empty()
    {
        bail!("Yuanbao allowlist policy requires at least one allowed conversation or user");
    }
    let chat_summary = if allowed_chats.is_empty() {
        "all policy-accepted conversations".to_string()
    } else {
        allowed_chats.join(", ")
    };
    let user_summary = if allowed_users.is_empty() {
        "all policy-accepted senders".to_string()
    } else {
        allowed_users.join(", ")
    };
    let bot_summary = if bot_accounts.is_empty() {
        "Yuanbao metadata and returned bot id".to_string()
    } else {
        bot_accounts.join(", ")
    };
    let review = vec![
        format!("Channel: {YUANBAO_CHANNEL}"),
        "Transport: direct_websocket".to_string(),
        "Connection: persistent Yuanbao WebSocket; no local callback URL or DuckAgent port"
            .to_string(),
        format!("Yuanbao WebSocket URL: {default_ws_url}"),
        format!("Yuanbao API domain: {default_api_domain}"),
        format!("DM policy: {dm_policy}"),
        format!("Group policy: {group_policy}"),
        format!("Allowed conversations: {chat_summary}"),
        format!("Allowed users: {user_summary}"),
        format!("Bot mention/self-filter accounts: {bot_summary}"),
        "Delivery: text replies over Yuanbao WebSocket; approvals use /approve and /deny text fallback"
            .to_string(),
    ];
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let mut credential_extra = BTreeMap::new();
    credential_extra.insert("ws_url".to_string(), default_ws_url.to_string());
    credential_extra.insert("api_domain".to_string(), default_api_domain.to_string());
    let now = now_rfc3339_like();
    save_gateway_credentials(
        YUANBAO_CHANNEL,
        GatewayCredentialEntry {
            channel: YUANBAO_CHANNEL.to_string(),
            app_id: Some(app_id),
            app_secret: Some(app_secret),
            extra: credential_extra,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("ws_url".to_string(), default_ws_url.to_string());
    extra.insert("api_domain".to_string(), default_api_domain.to_string());
    extra.insert("bot_accounts".to_string(), bot_accounts.join(","));
    extra.insert("dm_policy".to_string(), dm_policy);
    extra.insert("group_policy".to_string(), group_policy);
    let mut next = current_config;
    next.channels.insert(
        YUANBAO_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("direct_websocket".to_string()),
            api_base: Some(default_api_domain.to_string()),
            allowed_users,
            allowed_chats,
            extra,
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "Yuanbao gateway configured",
        "Yuanbao will connect directly over WebSocket when the gateway service starts. No local callback port is required.",
    )?;
    Ok(true)
}

fn run_qqbot_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let default_api_base = "https://api.sgroup.qq.com";
    let app_id = match prompt_text(
        "QQ Bot app id",
        "QQ Official Bot App ID from q.qq.com.",
        "app_id",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
                bail!("QQ Bot app id must be non-empty and must not contain whitespace");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let app_secret = match prompt_gateway_secret(
        "QQ Bot app secret",
        "QQ Official Bot App Secret used for token and gateway auth.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let markdown_support = match prompt_text(
        "QQ markdown support",
        "true keeps markdown for legacy bridge/native template mode; direct Gateway sends safe plain text.",
        "false",
        Some("false"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_ascii_lowercase();
            if !matches!(trimmed.as_str(), "true" | "false") {
                bail!("QQ markdown support must be true or false");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let dm_policy = match prompt_text(
        "QQ DM policy",
        "open, allowlist, or disabled for C2C/guild DM messages.",
        "open",
        Some("open"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_ascii_lowercase();
            if !matches!(trimmed.as_str(), "open" | "allowlist" | "disabled") {
                bail!("QQ DM policy must be open, allowlist, or disabled");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let group_policy = match prompt_text(
        "QQ group policy",
        "open, allowlist, or disabled for group/guild channel messages.",
        "open",
        Some("open"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_ascii_lowercase();
            if !matches!(trimmed.as_str(), "open" | "allowlist" | "disabled") {
                bail!("QQ group policy must be open, allowlist, or disabled");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_chats = match prompt_optional_csv(
        "Allowed QQ chats",
        "Optional c2c:<openid>, group:<group_openid>, guild:<channel_id>, or dm:<guild_id> allowlist.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed QQ users",
        "Optional QQ user/member OpenID allowlist. Empty allows all senders unless policy is allowlist.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    if (dm_policy == "allowlist" || group_policy == "allowlist")
        && allowed_chats.is_empty()
        && allowed_users.is_empty()
    {
        bail!("QQ allowlist policy requires at least one allowed chat or user");
    }
    let chat_summary = if allowed_chats.is_empty() {
        "all policy-accepted chats".to_string()
    } else {
        allowed_chats.join(", ")
    };
    let user_summary = if allowed_users.is_empty() {
        "all policy-accepted senders".to_string()
    } else {
        allowed_users.join(", ")
    };
    let review = vec![
        format!("Channel: {QQBOT_CHANNEL}"),
        "Transport: direct_gateway".to_string(),
        "Connection: QQ official Gateway WebSocket + REST; no local callback URL or DuckAgent port"
            .to_string(),
        format!("QQ API base: {default_api_base}"),
        format!("Legacy markdown setting: {markdown_support}"),
        format!("DM policy: {dm_policy}"),
        format!("Group policy: {group_policy}"),
        format!("Allowed chats: {chat_summary}"),
        format!("Allowed users: {user_summary}"),
        "Delivery: text replies over QQ REST; approvals use /approve and /deny text fallback"
            .to_string(),
    ];
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        QQBOT_CHANNEL,
        GatewayCredentialEntry {
            channel: QQBOT_CHANNEL.to_string(),
            app_id: Some(app_id),
            app_secret: Some(app_secret),
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("api_base".to_string(), default_api_base.to_string());
    extra.insert("markdown_support".to_string(), markdown_support);
    extra.insert("dm_policy".to_string(), dm_policy);
    extra.insert("group_policy".to_string(), group_policy);
    let mut next = current_config;
    next.channels.insert(
        QQBOT_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("direct_gateway".to_string()),
            api_base: Some(default_api_base.to_string()),
            allowed_users,
            allowed_chats,
            extra,
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "QQ Bot gateway configured",
        "QQ Bot will connect directly through the official Gateway when the gateway service starts. No local callback port is required.",
    )?;
    Ok(true)
}

fn run_nextcloud_talk_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bind = default_callback_gateway_bind(current_config.bind.as_deref())?;
    let server_url = match prompt_text(
        "Nextcloud URL",
        "Nextcloud server base URL, for example https://cloud.example.com.",
        "https://cloud.example.com",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().trim_end_matches('/').to_string();
            if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
                bail!("Nextcloud URL must start with http:// or https://");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let username = match prompt_text(
        "Nextcloud bot username",
        "Bot or app account username used for Talk OCS API calls and self-message filtering.",
        "duckagent",
        Some("duckagent"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
                bail!("Nextcloud bot username must be a non-empty account name without whitespace");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let app_password = match prompt_gateway_secret(
        "Nextcloud app password",
        "App password used directly by DuckAgent for Talk OCS API replies and protected attachment downloads.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let webhook_secret = match prompt_optional_secret(
        "Webhook secret",
        "Optional secret expected in X-DuckAgent-Gateway-Secret, X-Nextcloud-Talk-Secret, or ?secret=.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_chats = match prompt_optional_csv(
        "Allowed Talk rooms",
        "Optional room tokens. Empty allows all rooms.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_users = match prompt_optional_csv(
        "Allowed Talk actors",
        "Optional actor ids/user ids. Empty allows all senders.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let inbound_path = "/nextcloud-talk/events".to_string();
    let room_summary = if allowed_chats.is_empty() {
        "all Talk rooms accepted by the webhook".to_string()
    } else {
        allowed_chats.join(", ")
    };
    let actor_summary = if allowed_users.is_empty() {
        "all Talk actors accepted by the webhook".to_string()
    } else {
        allowed_users.join(", ")
    };
    let review = vec![
        format!("Channel: {NEXTCLOUD_TALK_CHANNEL}"),
        "Transport: Nextcloud Talk webhook + direct OCS API".to_string(),
        format!("Nextcloud URL: {server_url}"),
        format!(
            "Inbound endpoints: {inbound_path}, /nextcloud-talk/webhook, /nextcloud-talk-webhook"
        ),
        format!("Webhook target: http://{bind}{inbound_path}"),
        "Nextcloud app password: configured for direct OCS API calls".to_string(),
        format!(
            "Webhook auth: {}",
            if webhook_secret.is_some() {
                "X-DuckAgent-Gateway-Secret, X-Nextcloud-Talk-Secret, or ?secret="
            } else {
                "not required"
            }
        ),
        format!("Allowed Talk rooms: {room_summary}"),
        format!("Allowed Talk actors: {actor_summary}"),
        "Replies: direct Talk OCS chat API".to_string(),
        "Approvals: /approve and /deny text fallback in the Talk room".to_string(),
    ];
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let mut credential_extra = BTreeMap::new();
    credential_extra.insert("server_url".to_string(), server_url.clone());
    let now = now_rfc3339_like();
    save_gateway_credentials(
        NEXTCLOUD_TALK_CHANNEL,
        GatewayCredentialEntry {
            channel: NEXTCLOUD_TALK_CHANNEL.to_string(),
            username: Some(username),
            password: Some(app_password),
            webhook_secret,
            extra: credential_extra,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("server_url".to_string(), server_url.clone());
    extra.insert("inbound_path".to_string(), inbound_path.clone());
    let mut next = current_config;
    next.bind = Some(bind.clone());
    next.channels.insert(
        NEXTCLOUD_TALK_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("nextcloud_talk_ocs".to_string()),
            api_base: Some(server_url),
            allowed_users,
            allowed_chats,
            extra,
            ..Default::default()
        },
    );
    save_gateway_config(&next)?;
    show_setup_message(
        "Nextcloud Talk gateway configured",
        &format!(
            "Configure the Nextcloud Talk bot webhook to POST inbound events to http://{bind}{inbound_path}, or map that path through your public/reverse-proxy URL."
        ),
    )?;
    Ok(true)
}

fn run_nostr_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let private_key = match prompt_gateway_secret(
        "Nostr private key / nsec",
        "Private key used by DuckAgent to connect to relays and decrypt NIP-04/NIP-17 direct messages. No bridge URL or callback port is required.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let relays = match prompt_text(
        "Nostr relays",
        "Comma-separated relay URLs such as wss://relay.damus.io,wss://nos.lol.",
        "wss://relay.damus.io",
        None,
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => parse_csv_list(&value),
        SetupAction::Back => return run_gateway_setup(),
    };
    if relays.is_empty() {
        bail!("Nostr relay transport requires at least one relay URL");
    }
    for relay in &relays {
        if !(relay.starts_with("wss://") || relay.starts_with("ws://")) {
            bail!("Nostr relay URLs must start with wss:// or ws://");
        }
    }
    let allowed_peers = match prompt_optional_csv(
        "Allowed Nostr pubkeys",
        "Optional sender pubkeys/npubs. Empty allows all Nostr DMs received by this key.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let review = vec![
        format!("Channel: {NOSTR_CHANNEL}"),
        "Transport: Nostr relay direct messages (NIP-04/NIP-17)".to_string(),
        format!("Relays: {}", relays.join(", ")),
        "Private key: configured in auth.json".to_string(),
        "Inbound: DuckAgent subscribes to Nostr relays directly; no public callback URL or local port is required".to_string(),
        "Outbound: replies are encrypted direct messages to the sender pubkey".to_string(),
        format!(
            "Allowed Nostr pubkeys: {}",
            if allowed_peers.is_empty() {
                "all received DM senders".to_string()
            } else {
                allowed_peers.join(", ")
            }
        ),
        "Media: outbound media paths are sent as text links; inbound media URLs are surfaced as attachments/links".to_string(),
        "Typing: no-op; approvals use command fallback".to_string(),
    ];
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let now = now_rfc3339_like();
    save_gateway_credentials(
        NOSTR_CHANNEL,
        GatewayCredentialEntry {
            channel: NOSTR_CHANNEL.to_string(),
            password: Some(private_key),
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("relay_urls".to_string(), relays.join(","));
    let mut next = current_config;
    next.channels.insert(
        NOSTR_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("relay".to_string()),
            allowed_users: allowed_peers,
            extra,
            ..Default::default()
        },
    );
    clear_gateway_bind_if_unused(&mut next);
    save_gateway_config(&next)?;
    show_setup_message(
        "Nostr gateway configured",
        "Configuration saved. The Nostr relay listener will connect when the gateway service starts.",
    )?;
    Ok(true)
}

#[allow(dead_code)]
fn run_nostr_bridge_setup() -> Result<bool> {
    let current_config = load_gateway_config().unwrap_or_default();
    let bind = default_callback_gateway_bind(current_config.bind.as_deref())?;
    let bridge_url = match prompt_text(
        "Nostr bridge API URL",
        "HTTP bridge base URL that owns Nostr relay connections, signing, and encryption.",
        "http://127.0.0.1:8093",
        Some("http://127.0.0.1:8093"),
        true,
        true,
        false,
    )? {
        SetupAction::Submit(value) => {
            let trimmed = value.trim().trim_end_matches('/').to_string();
            if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
                bail!("Nostr bridge API URL must start with http:// or https://");
            }
            trimmed
        }
        SetupAction::Back => return run_gateway_setup(),
    };
    let relays = match prompt_optional_csv(
        "Nostr relays",
        "Optional comma-separated relay URLs such as wss://relay.damus.io,wss://nos.lol.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    for relay in &relays {
        if !(relay.starts_with("wss://") || relay.starts_with("ws://")) {
            bail!("Nostr relay URLs must start with wss:// or ws://");
        }
    }
    let bot_pubkey = match prompt_text(
        "Nostr bot pubkey",
        "Optional bot public key/npub used by the bridge; empty if bridge discovers it.",
        "",
        None,
        false,
        true,
        false,
    )? {
        SetupAction::Submit(value) => (!value.trim().is_empty()).then(|| value.trim().to_string()),
        SetupAction::Back => return run_gateway_setup(),
    };
    let signer_secret = match prompt_optional_secret(
        "Nostr signer secret",
        "Optional nsec/private key for a local bridge. Leave empty when using an external signer/NIP-46.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let bridge_token = match prompt_optional_secret(
        "Bridge bearer token",
        "Optional bearer token used for outbound bridge calls and protected media downloads.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let webhook_secret = match prompt_optional_secret(
        "Webhook secret",
        "Optional secret expected in X-DuckAgent-Gateway-Secret, X-Nostr-Secret, or ?secret=.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_peers = match prompt_optional_csv(
        "Allowed Nostr pubkeys",
        "Optional sender pubkeys/npubs. Empty allows all senders accepted by the bridge.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let allowed_conversations = match prompt_optional_csv(
        "Allowed Nostr conversations",
        "Optional dm:<pubkey>, event:<event_id>, note id, or * allowlist. Empty allows all conversations.",
    )? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return run_gateway_setup(),
    };
    let inbound_path = "/nostr/events".to_string();
    let pubkey_summary = if allowed_peers.is_empty() {
        "all bridge-accepted pubkeys".to_string()
    } else {
        allowed_peers.join(", ")
    };
    let conversation_summary = if allowed_conversations.is_empty() {
        "all bridge-accepted conversations".to_string()
    } else {
        allowed_conversations.join(", ")
    };
    let review = vec![
        format!("Channel: {NOSTR_CHANNEL}"),
        "Transport: nostr_bridge".to_string(),
        format!("Nostr bridge API URL: {bridge_url}"),
        format!("Inbound endpoints: {inbound_path}, /nostr/webhook"),
        format!(
            "Relays: {}",
            if relays.is_empty() {
                "bridge default".to_string()
            } else {
                relays.join(", ")
            }
        ),
        format!(
            "Bot pubkey: {}",
            bot_pubkey.as_deref().unwrap_or("bridge discovers it")
        ),
        format!(
            "Signer: {}",
            if signer_secret.is_some() {
                "local signer secret configured"
            } else {
                "external signer or bridge-managed key"
            }
        ),
        format!(
            "Webhook auth: {}",
            if webhook_secret.is_some() {
                "X-DuckAgent-Gateway-Secret, X-Nostr-Secret, or ?secret="
            } else {
                "not required"
            }
        ),
        format!("Allowed Nostr pubkeys: {pubkey_summary}"),
        format!("Allowed Nostr conversations: {conversation_summary}"),
        format!("Local tunnel target: http://{bind}{inbound_path}"),
        "Bridge responsibilities: relay connection, signing, encryption, and media upload policy"
            .to_string(),
        "Typing: no-op; approvals use command fallback unless the bridge maps a native UI"
            .to_string(),
    ];
    match prompt_confirm("Review gateway", &review, true)? {
        SetupAction::Submit(()) => {}
        SetupAction::Back => return run_gateway_setup(),
    }

    let mut credential_extra = BTreeMap::new();
    if let Some(bot_pubkey) = bot_pubkey {
        credential_extra.insert("bot_pubkey".to_string(), bot_pubkey);
    }
    if let Some(signer_secret) = signer_secret {
        credential_extra.insert("signer_secret".to_string(), signer_secret);
    }
    let now = now_rfc3339_like();
    save_gateway_credentials(
        NOSTR_CHANNEL,
        GatewayCredentialEntry {
            channel: NOSTR_CHANNEL.to_string(),
            token: bridge_token,
            webhook_secret,
            extra: credential_extra,
            created_at: Some(now.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    )?;

    let mut extra = BTreeMap::new();
    extra.insert("relay_urls".to_string(), relays.join(","));
    extra.insert("inbound_path".to_string(), inbound_path.clone());
    extra.insert("send_endpoint".to_string(), "/send".to_string());
    let mut next = current_config;
    next.bind = Some(bind.clone());
    next.channels.insert(
        NOSTR_CHANNEL.to_string(),
        GatewayChannelConfig {
            enabled: true,
            transport: Some("nostr_bridge".to_string()),
            api_base: Some(bridge_url),
            allowed_users: allowed_peers,
            allowed_chats: allowed_conversations,
            extra,
            ..Default::default()
        },
    );
    save_gateway_config(&next)?;
    show_setup_message(
        "Nostr gateway configured",
        &format!(
            "Configure the Nostr bridge to POST inbound events to http://{bind}{inbound_path}, or map that path through your public/reverse-proxy URL."
        ),
    )?;
    Ok(true)
}

fn prompt_gateway_channel(allow_back: bool) -> Result<SetupAction<&'static str>> {
    let items = CONFIGURABLE_CHANNELS
        .iter()
        .map(|channel| PickerItem {
            title: channel.title.to_string(),
            detail: channel.detail.to_string(),
            model_columns: None,
        })
        .collect::<Vec<_>>();
    let selected = match run_picker(
        "Select gateway channel",
        "Only fully usable channels are shown here.",
        &items,
        allow_back,
    )? {
        SetupAction::Submit(index) => index,
        SetupAction::Back => return Ok(SetupAction::Back),
    };
    let channel = CONFIGURABLE_CHANNELS
        .get(selected)
        .map(|channel| channel.id)
        .ok_or_else(|| anyhow!("invalid gateway channel selection"))?;
    Ok(SetupAction::Submit(channel))
}

fn default_stable_gateway_bind(initial: Option<&str>) -> Result<String> {
    default_callback_gateway_bind(initial)
}

fn default_callback_gateway_bind(initial: Option<&str>) -> Result<String> {
    if let Some(value) = initial.map(str::trim).filter(|value| {
        !value.is_empty() && !value.ends_with(":0") && *value != DEFAULT_GATEWAY_CONFIGURED_BIND
    }) {
        return Ok(value.to_string());
    }
    Ok(format!("127.0.0.1:{}", allocate_local_setup_port()?))
}

fn gateway_callback_url(base: &str, path: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    let path = path.trim();
    if base.ends_with(path) {
        base.to_string()
    } else {
        format!("{}/{}", base, path.trim_start_matches('/'))
    }
}

fn prompt_gateway_secret(title: &str, subtitle: &str) -> Result<SetupAction<String>> {
    prompt_text(title, subtitle, "", None, true, true, true)
}

fn prompt_optional_secret(title: &str, subtitle: &str) -> Result<SetupAction<Option<String>>> {
    let input = match prompt_text(title, subtitle, "", None, false, true, true)? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return Ok(SetupAction::Back),
    };
    Ok(SetupAction::Submit(
        (!input.trim().is_empty()).then(|| input.trim().to_string()),
    ))
}

fn prompt_optional_csv(title: &str, subtitle: &str) -> Result<SetupAction<Vec<String>>> {
    let input = match prompt_text(title, subtitle, "", None, false, true, false)? {
        SetupAction::Submit(value) => value,
        SetupAction::Back => return Ok(SetupAction::Back),
    };
    Ok(SetupAction::Submit(parse_csv_list(&input)))
}

fn prompt_gateway_review(channel: &str, _bind: &str, transport: &str) -> Result<SetupAction<()>> {
    let lines = vec![
        format!("Channel: {channel}"),
        format!("Transport: {transport}"),
    ];
    prompt_confirm("Review gateway", &lines, true)
}

fn prompt_gateway_review_maybe_bind(
    channel: &str,
    bind: Option<&str>,
    transport: &str,
) -> Result<SetupAction<()>> {
    match bind {
        Some(bind) => prompt_gateway_review(channel, bind, transport),
        None => prompt_gateway_review_without_bind(channel, transport),
    }
}

fn prompt_gateway_review_without_bind(channel: &str, transport: &str) -> Result<SetupAction<()>> {
    let lines = vec![
        format!("Channel: {channel}"),
        format!("Transport: {transport}"),
    ];
    prompt_confirm("Review gateway", &lines, true)
}

fn clear_gateway_bind_if_unused(config: &mut super::config::GatewayConfig) {
    if !config_needs_stable_bind(config) {
        config.bind = None;
    }
}

fn parse_csv_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn now_rfc3339_like() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn default_whatsapp_session_path_text() -> String {
    super::default_gateway_channel_state_dir("whatsapp")
        .unwrap_or_else(|_| std::env::temp_dir().join("duckagent").join("gateway"))
        .join("session")
        .to_string_lossy()
        .to_string()
}

fn allocate_local_setup_port() -> Result<u16> {
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).context("failed to allocate a local port for setup")?;
    Ok(listener.local_addr()?.port())
}

#[cfg(test)]
mod tests {
    use super::super::config::GatewayConfig;
    use super::*;

    #[test]
    fn setup_lists_only_fully_usable_channels() {
        let ids = CONFIGURABLE_CHANNELS
            .iter()
            .map(|channel| channel.id)
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                TELEGRAM_CHANNEL,
                SLACK_CHANNEL,
                SIGNAL_CHANNEL,
                MATRIX_CHANNEL,
                FEISHU_CHANNEL,
                LARK_CHANNEL,
                FEISHU_COMMENT_CHANNEL,
                LARK_COMMENT_CHANNEL,
                DISCORD_CHANNEL,
                MATTERMOST_CHANNEL,
                API_SERVER_CHANNEL,
                WHATSAPP_CHANNEL,
                DINGTALK_CHANNEL,
                WECOM_CHANNEL,
                WECOM_CALLBACK_CHANNEL,
                WEIXIN_CHANNEL,
                BLUEBUBBLES_CHANNEL,
                IMESSAGE_CHANNEL,
                EMAIL_CHANNEL,
                SMS_CHANNEL,
                MSGRAPH_WEBHOOK_CHANNEL,
                MSTEAMS_CHANNEL,
                GOOGLECHAT_CHANNEL,
                LINE_CHANNEL,
                IRC_CHANNEL,
                NEXTCLOUD_TALK_CHANNEL,
                NOSTR_CHANNEL,
                SYNOLOGY_CHAT_CHANNEL,
                TLON_CHANNEL,
                TWITCH_CHANNEL,
                ZALO_CHANNEL,
                ZALOUSER_CHANNEL,
                HOMEASSISTANT_CHANNEL,
                QQBOT_CHANNEL,
                YUANBAO_CHANNEL,
                VOICE_CALL_CHANNEL,
                TALK_VOICE_CHANNEL,
            ]
        );
    }

    #[test]
    fn setup_descriptor_contains_no_planned_channels() {
        assert!(
            !CONFIGURABLE_CHANNELS
                .iter()
                .any(|channel| channel.detail.to_ascii_lowercase().contains("planned"))
        );
    }

    #[test]
    fn gateway_channel_manager_lists_add_remove_and_configured_channels() {
        let mut config = GatewayConfig::default();
        config.channels.insert(
            TELEGRAM_CHANNEL.to_string(),
            GatewayChannelConfig {
                enabled: true,
                transport: Some("polling".to_string()),
                allowed_users: vec!["123456789".to_string()],
                ..Default::default()
            },
        );
        config.channels.insert(
            WEBHOOK_CHANNEL.to_string(),
            GatewayChannelConfig {
                enabled: true,
                transport: Some("http".to_string()),
                ..Default::default()
            },
        );

        let visible_channels = configured_visible_channel_ids(&config);
        assert_eq!(visible_channels, vec![TELEGRAM_CHANNEL.to_string()]);

        let items = gateway_channel_manager_items(&config, &visible_channels);
        assert_eq!(items[0].title, "Add Channel");
        assert_eq!(items[1].title, "Telegram");
        assert!(items[1].detail.contains("enabled"));
        assert!(items[1].detail.contains("transport: polling"));
        assert!(items[1].detail.contains("1 users, 0 chats"));
    }

    #[test]
    fn gateway_channel_manager_filters_internal_channels() {
        let mut config = GatewayConfig::default();
        for channel in [WEBHOOK_CHANNEL, QA_CHANNEL, TELEGRAM_CHANNEL] {
            config.channels.insert(
                channel.to_string(),
                GatewayChannelConfig {
                    enabled: true,
                    ..Default::default()
                },
            );
        }

        assert_eq!(
            configured_visible_channel_ids(&config),
            vec![TELEGRAM_CHANNEL.to_string()]
        );
    }

    #[test]
    fn default_setup_config_is_launchable_webhook() {
        let mut config = GatewayConfig {
            bind: Some(DEFAULT_GATEWAY_BIND.to_string()),
            ..Default::default()
        };
        config.channels.insert(
            WEBHOOK_CHANNEL.to_string(),
            GatewayChannelConfig {
                enabled: true,
                transport: Some("http".to_string()),
                ..Default::default()
            },
        );
        assert!(config.enabled_channels().next().is_some());
    }

    #[test]
    fn parse_csv_list_trims_empty_values() {
        assert_eq!(
            parse_csv_list("123, @alice, ,bob"),
            vec!["123", "@alice", "bob"]
        );
    }

    #[test]
    fn telegram_user_id_validation_requires_numeric_ids() {
        assert!(is_telegram_user_id("123456789"));
        assert!(!is_telegram_user_id("@alice"));
        assert!(!is_telegram_user_id("123 456"));
        assert!(!is_telegram_user_id(""));
    }

    #[test]
    fn telegram_setup_error_redaction_hides_bot_token() {
        let token = "123456:ABC-secret";
        let text =
            "error sending request for url (https://api.telegram.org/bot123456:ABC-secret/getMe)";
        let redacted = redact_telegram_token(text, token);
        assert!(!redacted.contains(token));
        assert!(redacted.contains("bot<telegram-token>/getMe"));
    }

    #[test]
    fn telegram_setup_uses_numeric_allowlist_access() {
        let access = telegram_default_access();
        assert_eq!(access.dm_policy, "allowlist");
        assert_eq!(access.group_policy, "allowlist");
        assert!(access.require_mention);
    }

    #[test]
    fn telegram_polling_does_not_require_stable_bind() {
        let mut config = GatewayConfig {
            bind: Some("127.0.0.1:8788".to_string()),
            ..Default::default()
        };
        config.channels.insert(
            TELEGRAM_CHANNEL.to_string(),
            GatewayChannelConfig {
                enabled: true,
                transport: Some("polling".to_string()),
                access: telegram_default_access(),
                ..Default::default()
            },
        );
        if !config_needs_stable_bind(&config) {
            config.bind = None;
        }
        assert!(config.bind.is_none());

        config.channels.insert(
            WEBHOOK_CHANNEL.to_string(),
            GatewayChannelConfig {
                enabled: true,
                transport: Some("http".to_string()),
                ..Default::default()
            },
        );
        config.bind = Some("127.0.0.1:8788".to_string());
        if !config_needs_stable_bind(&config) {
            config.bind = None;
        }
        assert_eq!(config.bind.as_deref(), Some("127.0.0.1:8788"));
    }

    #[test]
    fn feishu_bot_info_accepts_current_and_legacy_shapes() {
        let current = json!({
            "code": 0,
            "bot": {
                "app_name": "DuckAgent Lark",
                "open_id": "ou_bot"
            }
        });
        assert_eq!(
            parse_feishu_bot_info(&current),
            Some(FeishuBotProbe {
                bot_name: Some("DuckAgent Lark".to_string()),
                bot_open_id: Some("ou_bot".to_string())
            })
        );

        let legacy = json!({
            "code": 0,
            "data": {
                "bot": {
                    "bot_name": "DuckAgent Feishu",
                    "open_id": "ou_legacy"
                }
            }
        });
        assert_eq!(
            parse_feishu_bot_info(&legacy),
            Some(FeishuBotProbe {
                bot_name: Some("DuckAgent Feishu".to_string()),
                bot_open_id: Some("ou_legacy".to_string())
            })
        );
    }

    #[test]
    fn feishu_registration_url_uses_official_sdk_launcher_params() {
        assert_eq!(
            duckagent_registration_url("https://example.test/scan"),
            "https://example.test/scan?from=sdk&source=node-sdk&tp=sdk"
        );
        assert_eq!(
            duckagent_registration_url("https://example.test/scan?code=1&from=old&tp=old"),
            "https://example.test/scan?code=1&from=sdk&source=node-sdk&tp=sdk"
        );
    }

    #[test]
    fn feishu_registration_uses_feishu_code_backend_and_selected_launcher_domain() {
        // The code endpoint and the visible launcher are intentionally split:
        // Lark registration still begins on Feishu's registration backend, but
        // the URL shown to the user must stay on the Lark launcher domain.
        assert_eq!(feishu_registration_initial_domain(FEISHU_CHANNEL), "feishu");
        assert_eq!(feishu_registration_initial_domain(LARK_CHANNEL), "feishu");
        assert_eq!(
            feishu_registration_launcher_domain(FEISHU_CHANNEL),
            "feishu"
        );
        assert_eq!(feishu_registration_launcher_domain(LARK_CHANNEL), "lark");
    }

    #[test]
    fn feishu_registration_url_rewrites_visible_launcher_domain() {
        // Preserve the backend-issued user_code while showing the selected
        // product host. This prevents Lark setup from displaying a Feishu URL.
        let rewritten = duckagent_registration_url_for_domain(
            "https://open.feishu.cn/page/launcher?user_code=ZE8T-G8AN",
            "lark",
            "ZE8T-G8AN",
        );
        assert!(rewritten.starts_with("https://open.larksuite.com/page/launcher?"));
        assert!(rewritten.contains("user_code=ZE8T-G8AN"));
        assert!(rewritten.contains("from=sdk"));
        assert!(rewritten.contains("source=node-sdk"));
        assert!(rewritten.contains("tp=sdk"));
    }

    #[test]
    fn setup_url_wrapping_preserves_full_launcher_url() {
        let url = "https://open.larksuite.com/page/launcher?user_code=ZE8T-G8AN&from=sdk&source=node-sdk&tp=sdk";
        let wrapped = wrap_setup_url(url, 24);
        assert!(wrapped.len() > 1);
        assert_eq!(wrapped.concat(), url);
    }

    #[test]
    fn terminal_qr_uses_dense_half_block_mapping() {
        let qr = render_terminal_qr("https://open.larksuite.com/page/launcher?user_code=ZE8T-G8AN&from=sdk&source=node-sdk&tp=sdk")
            .expect("qr should render");
        let first_line = qr.lines().next().expect("qr should not be empty");
        assert!(first_line.starts_with(SETUP_QR_DENSE_ROW_PREFIX));
        assert!(
            first_line[SETUP_QR_DENSE_ROW_PREFIX.len()..]
                .chars()
                .any(|ch| ch != ' ')
        );
        assert!(qr.chars().any(|ch| matches!(ch, '█' | '▀' | '▄')));
        assert!(qr.lines().all(|line| {
            line.strip_prefix(SETUP_QR_DENSE_ROW_PREFIX)
                .is_some_and(|cells| cells.chars().all(|ch| matches!(ch, ' ' | '█' | '▀' | '▄')))
        }));
        assert!(qr.lines().count() <= 20);
    }

    #[test]
    fn feishu_registration_wait_copy_is_product_specific() {
        let lines = feishu_registration_wait_lines(
            "Lark",
            &FeishuRegistrationBegin {
                device_code: "device".to_string(),
                user_code: "code".to_string(),
                qr_url: "https://open.larksuite.com/page/launcher?user_code=ZE8T-G8AN&from=sdk&source=node-sdk&tp=sdk".to_string(),
                interval: 5,
                expire_in: 600,
                initial_domain: "feishu",
            },
        );
        assert_eq!(lines[0], "Use Lark to scan the QR code below.");
        assert!(lines[1].contains("If the QR code looks garbled"));
        assert!(lines.iter().any(|line| line.contains("open.larksuite.com")));
    }

    #[test]
    fn feishu_registration_collects_scanner_identity_aliases() {
        let ids = feishu_registration_user_ids(&json!({
            "open_id": "ou_scanner",
            "user_id": "user_scanner",
            "union_id": "on_scanner"
        }));
        assert_eq!(ids, vec!["ou_scanner", "user_scanner", "on_scanner"]);
    }
}
