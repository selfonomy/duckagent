pub(in crate::gateway) mod api_server;
pub(in crate::gateway) mod bluebubbles;
pub(in crate::gateway) mod dingtalk;
pub(in crate::gateway) mod discord;
pub(in crate::gateway) mod email;
pub(in crate::gateway) mod feishu;
pub(in crate::gateway) mod feishu_comment;
pub(in crate::gateway::channels) mod feishu_ws;
pub(in crate::gateway) mod google_chat;
pub(in crate::gateway) mod homeassistant;
pub(in crate::gateway) mod irc;
pub(in crate::gateway) mod line;
pub(in crate::gateway) mod matrix;
pub(in crate::gateway) mod mattermost;
pub(in crate::gateway) mod msgraph_webhook;
pub(in crate::gateway) mod msteams;
pub(in crate::gateway) mod nextcloud_talk;
pub(in crate::gateway) mod nostr;
pub(in crate::gateway) mod qa_channel;
pub(in crate::gateway) mod qqbot;
pub(in crate::gateway) mod signal;
pub(in crate::gateway) mod slack;
pub(in crate::gateway) mod sms;
pub(in crate::gateway) mod synology_chat;
pub(in crate::gateway) mod telegram;
pub(in crate::gateway) mod tlon;
pub(in crate::gateway) mod twitch;
pub(in crate::gateway) mod voice;
pub(in crate::gateway) mod webhook;
pub(in crate::gateway::channels) mod websocket;
pub(in crate::gateway) mod wecom;
pub(in crate::gateway) mod weixin;
pub(in crate::gateway) mod whatsapp;
pub(in crate::gateway) mod yuanbao;
pub(in crate::gateway) mod zalo;
pub(in crate::gateway) mod zalouser;

use super::config::GatewayChannelConfig;
use super::{ChannelAdapter, GatewayOutbox};
use crate::auth::GatewayCredentialEntry;
use anyhow::{Result, bail};
use std::sync::Arc;

pub(in crate::gateway) fn create_adapter(
    name: &str,
    config: &GatewayChannelConfig,
    credentials: Option<&GatewayCredentialEntry>,
    outbox: GatewayOutbox,
) -> Result<Arc<dyn ChannelAdapter>> {
    match normalize_channel_name(name).as_str() {
        "webhook" => Ok(Arc::new(webhook::WebhookAdapter::new(outbox))),
        "telegram" => Ok(Arc::new(telegram::TelegramAdapter::new(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("telegram gateway credentials missing"))?,
        )?)),
        "slack" => Ok(Arc::new(slack::SlackAdapter::new(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("slack gateway credentials missing"))?,
        )?)),
        "signal" => Ok(Arc::new(signal::SignalAdapter::new(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("signal gateway credentials missing"))?,
        )?)),
        "matrix" => Ok(Arc::new(matrix::MatrixAdapter::new(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("matrix gateway credentials missing"))?,
        )?)),
        "feishu" | "lark" => {
            let channel = normalize_channel_name(name);
            Ok(Arc::new(feishu::FeishuAdapter::new(
                &channel,
                config,
                credentials
                    .ok_or_else(|| anyhow::anyhow!("{channel} gateway credentials missing"))?,
            )?))
        }
        "feishu_comment" | "lark_comment" => {
            let channel = normalize_channel_name(name);
            Ok(Arc::new(feishu_comment::FeishuCommentAdapter::new(
                &channel,
                config,
                credentials
                    .ok_or_else(|| anyhow::anyhow!("{channel} gateway credentials missing"))?,
            )?))
        }
        "discord" => Ok(Arc::new(discord::DiscordAdapter::new(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("discord gateway credentials missing"))?,
        )?)),
        "mattermost" => Ok(Arc::new(mattermost::MattermostAdapter::new(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("mattermost gateway credentials missing"))?,
        )?)),
        "api_server" => Ok(Arc::new(api_server::ApiServerAdapter::new(outbox))),
        "whatsapp" => Ok(Arc::new(whatsapp::WhatsAppAdapter::new(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("whatsapp gateway credentials missing"))?,
        )?)),
        "dingtalk" => Ok(Arc::new(dingtalk::DingTalkAdapter::new(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("dingtalk gateway credentials missing"))?,
        )?)),
        "wecom" | "wecom_callback" => {
            let channel = normalize_channel_name(name);
            Ok(Arc::new(wecom::WeComAdapter::new(
                &channel,
                config,
                credentials.ok_or_else(|| anyhow::anyhow!("wecom gateway credentials missing"))?,
            )?))
        }
        "weixin" => Ok(Arc::new(weixin::WeixinAdapter::new(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("weixin gateway credentials missing"))?,
        )?)),
        "bluebubbles" | "imessage" => {
            let channel = normalize_channel_name(name);
            Ok(Arc::new(bluebubbles::BlueBubblesAdapter::new(
                &channel,
                config,
                credentials
                    .ok_or_else(|| anyhow::anyhow!("{channel} gateway credentials missing"))?,
            )?))
        }
        "email" => Ok(Arc::new(email::EmailAdapter::new(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("email gateway credentials missing"))?,
        )?)),
        "sms" => Ok(Arc::new(sms::SmsAdapter::new(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("sms gateway credentials missing"))?,
        )?)),
        "msgraph_webhook" => Ok(Arc::new(msgraph_webhook::MsGraphWebhookAdapter::new(
            config,
            credentials
                .ok_or_else(|| anyhow::anyhow!("msgraph_webhook gateway credentials missing"))?,
        )?)),
        "msteams" => Ok(Arc::new(msteams::MsTeamsAdapter::new(
            "msteams",
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("msteams gateway credentials missing"))?,
        )?)),
        "googlechat" => Ok(Arc::new(google_chat::GoogleChatAdapter::new(
            "googlechat",
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("googlechat gateway credentials missing"))?,
        )?)),
        "line" => Ok(Arc::new(line::LineAdapter::new(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("line gateway credentials missing"))?,
        )?)),
        "irc" => Ok(Arc::new(irc::IrcAdapter::new(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("irc gateway credentials missing"))?,
        )?)),
        "nextcloud-talk" => Ok(Arc::new(nextcloud_talk::new_adapter(
            config,
            credentials
                .ok_or_else(|| anyhow::anyhow!("nextcloud-talk gateway credentials missing"))?,
        )?)),
        "nostr" => Ok(Arc::new(nostr::new_adapter(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("nostr gateway credentials missing"))?,
        )?)),
        "synology-chat" => Ok(Arc::new(synology_chat::new_adapter(
            config,
            credentials
                .ok_or_else(|| anyhow::anyhow!("synology-chat gateway credentials missing"))?,
        )?)),
        "tlon" => Ok(Arc::new(tlon::new_adapter(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("tlon gateway credentials missing"))?,
        )?)),
        "twitch" => Ok(Arc::new(twitch::new_adapter(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("twitch gateway credentials missing"))?,
        )?)),
        "zalo" => Ok(Arc::new(zalo::new_adapter(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("zalo gateway credentials missing"))?,
        )?)),
        "zalouser" => Ok(Arc::new(zalouser::new_adapter(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("zalouser gateway credentials missing"))?,
        )?)),
        "homeassistant" => Ok(Arc::new(homeassistant::HomeAssistantAdapter::new(
            config,
            credentials
                .ok_or_else(|| anyhow::anyhow!("homeassistant gateway credentials missing"))?,
        )?)),
        "qqbot" => Ok(Arc::new(qqbot::new_adapter(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("qqbot gateway credentials missing"))?,
        )?)),
        "yuanbao" => Ok(Arc::new(yuanbao::new_adapter(
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("yuanbao gateway credentials missing"))?,
        )?)),
        "qa-channel" => Ok(Arc::new(qa_channel::QaChannelAdapter::new(outbox))),
        "voice-call" => Ok(Arc::new(voice::new_adapter(
            "voice-call",
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("voice-call gateway credentials missing"))?,
        )?)),
        "talk-voice" => Ok(Arc::new(voice::new_adapter(
            "talk-voice",
            config,
            credentials.ok_or_else(|| anyhow::anyhow!("talk-voice gateway credentials missing"))?,
        )?)),
        other => bail!(
            "unsupported gateway channel `{other}`; supported channels: {}",
            user_facing_supported_channels().join(", ")
        ),
    }
}

pub(in crate::gateway) fn user_facing_supported_channels() -> &'static [&'static str] {
    &[
        "telegram",
        "slack",
        "signal",
        "matrix",
        "feishu",
        "lark",
        "feishu_comment",
        "lark_comment",
        "discord",
        "mattermost",
        "api_server",
        "whatsapp",
        "dingtalk",
        "wecom",
        "wecom_callback",
        "weixin",
        "bluebubbles",
        "imessage",
        "email",
        "sms",
        "msgraph_webhook",
        "msteams",
        "teams",
        "googlechat",
        "google_chat",
        "line",
        "irc",
        "nextcloud-talk",
        "nextcloud_talk",
        "nostr",
        "synology-chat",
        "synology_chat",
        "tlon",
        "twitch",
        "zalo",
        "zalouser",
        "homeassistant",
        "home_assistant",
        "qqbot",
        "yuanbao",
        "voice-call",
        "voice_call",
        "talk-voice",
        "talk_voice",
    ]
}

fn normalize_channel_name(name: &str) -> String {
    match name.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "api_server" => "api_server".to_string(),
        "feishu_comment" => "feishu_comment".to_string(),
        "lark_comment" => "lark_comment".to_string(),
        "wecom_callback" => "wecom_callback".to_string(),
        "msgraph_webhook" => "msgraph_webhook".to_string(),
        "imsg" => "imessage".to_string(),
        "blue_bubbles" => "bluebubbles".to_string(),
        "google_chat" => "googlechat".to_string(),
        "teams" | "microsoft_teams" => "msteams".to_string(),
        "nextcloud_talk" => "nextcloud-talk".to_string(),
        "synology_chat" => "synology-chat".to_string(),
        "qa_channel" => "qa-channel".to_string(),
        "voice_call" => "voice-call".to_string(),
        "talk_voice" => "talk-voice".to_string(),
        "home_assistant" => "homeassistant".to_string(),
        other => other.replace('_', "-"),
    }
}
