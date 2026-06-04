use super::config::{
    API_SERVER_CHANNEL, GatewayChannelConfig, MSGRAPH_WEBHOOK_CHANNEL, QA_CHANNEL, WEBHOOK_CHANNEL,
    normalize_channel_name,
};
use super::pairing::{GatewayPairingStore, PairingNotice, format_pairing_time};
use super::types::{GatewaySessionKey, InboundMessageInput, OutboundMessage};
use super::{ChannelAdapter, GatewayRoute};
use anyhow::{Result, anyhow};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GatewayAccessDecision {
    Allowed,
    Blocked { reason: String },
    PairingRequired { notice: String },
}

pub(crate) fn evaluate_gateway_access(
    input: &InboundMessageInput,
    config: &GatewayChannelConfig,
    pairing: &GatewayPairingStore,
) -> Result<GatewayAccessDecision> {
    let channel = normalize_channel_name(&input.channel);
    if access_exempt_channel(&channel) {
        return Ok(GatewayAccessDecision::Allowed);
    }
    let Some(sender_id) = input
        .sender_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(GatewayAccessDecision::Blocked {
            reason: "incoming message has no sender_id and this channel is not trusted-local"
                .to_string(),
        });
    };
    let kind = conversation_kind(input);
    match kind {
        ConversationKind::Dm => evaluate_dm_access(input, config, pairing, &channel, sender_id),
        ConversationKind::Group => evaluate_group_access(input, config, sender_id),
    }
}

pub(crate) fn send_access_notice(
    adapter: Arc<dyn ChannelAdapter>,
    input: &InboundMessageInput,
    text: String,
) -> Result<()> {
    let route = GatewayRoute {
        session_id: String::new(),
        key: GatewaySessionKey {
            channel: normalize_channel_name(&input.channel),
            conversation_id: input.conversation_id.clone(),
            thread_id: input.thread_id.clone(),
        },
    };
    adapter.send_message(
        &route,
        OutboundMessage {
            text,
            media_paths: Vec::new(),
            reply_to: input.message_id.clone(),
            approval_prompt: None,
            typing_event: None,
        },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConversationKind {
    Dm,
    Group,
}

fn evaluate_dm_access(
    input: &InboundMessageInput,
    config: &GatewayChannelConfig,
    pairing: &GatewayPairingStore,
    channel: &str,
    sender_id: &str,
) -> Result<GatewayAccessDecision> {
    if explicit_list_allows(&config.allowed_users, sender_id, None) {
        return Ok(GatewayAccessDecision::Allowed);
    }
    if has_list_entries(&config.allowed_users) {
        return Ok(GatewayAccessDecision::Blocked {
            reason: format!("sender `{sender_id}` is not in allowed_users"),
        });
    }
    match policy(&config.access.dm_policy).as_str() {
        "open" => Ok(GatewayAccessDecision::Allowed),
        "disabled" => Ok(GatewayAccessDecision::Blocked {
            reason: "dm access is disabled for this channel".to_string(),
        }),
        "allowlist" => Ok(GatewayAccessDecision::Blocked {
            reason: format!("sender `{sender_id}` is not in allowed_users"),
        }),
        "pairing" => {
            if pairing.is_approved(channel, sender_id) {
                return Ok(GatewayAccessDecision::Allowed);
            }
            let notice =
                pairing.ensure_pending_code(channel, sender_id, input.sender_id.clone())?;
            Ok(GatewayAccessDecision::PairingRequired {
                notice: render_pairing_notice(&notice),
            })
        }
        other => Err(anyhow!("unknown gateway dm access policy `{other}`")),
    }
}

fn evaluate_group_access(
    input: &InboundMessageInput,
    config: &GatewayChannelConfig,
    sender_id: &str,
) -> Result<GatewayAccessDecision> {
    if !list_allows(&config.allowed_users, sender_id, None) {
        return Ok(GatewayAccessDecision::Blocked {
            reason: format!("sender `{sender_id}` is not in allowed_users"),
        });
    }
    match policy(&config.access.group_policy).as_str() {
        "open" => {
            if list_allows(&config.allowed_chats, &input.conversation_id, None) {
                Ok(GatewayAccessDecision::Allowed)
            } else {
                Ok(GatewayAccessDecision::Blocked {
                    reason: format!(
                        "conversation `{}` is not in allowed_chats",
                        input.conversation_id
                    ),
                })
            }
        }
        "disabled" => Ok(GatewayAccessDecision::Blocked {
            reason: "group access is disabled for this channel".to_string(),
        }),
        "allowlist" => {
            if explicit_list_allows(&config.allowed_chats, &input.conversation_id, None) {
                Ok(GatewayAccessDecision::Allowed)
            } else {
                Ok(GatewayAccessDecision::Blocked {
                    reason: format!(
                        "conversation `{}` is not in allowed_chats",
                        input.conversation_id
                    ),
                })
            }
        }
        other => Err(anyhow!("unknown gateway group access policy `{other}`")),
    }
}

fn render_pairing_notice(notice: &PairingNotice) -> String {
    let reused = if notice.reused_existing {
        "Existing pairing code"
    } else {
        "Pairing code"
    };
    format!(
        "DuckAgent needs to pair this chat before I can answer here.\n\n{reused}: `{}`\nExpires: {}\n\nAsk the DuckAgent owner to approve this pairing code.",
        notice.code,
        format_pairing_time(notice.expires_at)
    )
}

fn conversation_kind(input: &InboundMessageInput) -> ConversationKind {
    if let Some(chat_type) = input.chat_type.as_deref() {
        let value = chat_type.trim().to_ascii_lowercase();
        if matches!(
            value.as_str(),
            "dm" | "direct" | "private" | "p2p" | "single" | "user"
        ) {
            return ConversationKind::Dm;
        }
        if matches!(
            value.as_str(),
            "group" | "channel" | "room" | "guild" | "supergroup" | "space"
        ) {
            return ConversationKind::Group;
        }
    }
    match normalize_channel_name(&input.channel).as_str() {
        "telegram" => {
            if input.conversation_id.starts_with('-') {
                ConversationKind::Group
            } else {
                ConversationKind::Dm
            }
        }
        "slack" => {
            if input.conversation_id.starts_with('D') {
                ConversationKind::Dm
            } else {
                ConversationKind::Group
            }
        }
        "signal" => {
            if input.conversation_id.starts_with("group:") {
                ConversationKind::Group
            } else {
                ConversationKind::Dm
            }
        }
        "whatsapp" => {
            if input.conversation_id.ends_with("@g.us") {
                ConversationKind::Group
            } else {
                ConversationKind::Dm
            }
        }
        "email" | "sms" => ConversationKind::Dm,
        _ => ConversationKind::Group,
    }
}

fn access_exempt_channel(channel: &str) -> bool {
    matches!(
        channel,
        WEBHOOK_CHANNEL | API_SERVER_CHANNEL | QA_CHANNEL | MSGRAPH_WEBHOOK_CHANNEL
    )
}

fn policy(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('-', "_")
}

fn list_allows(list: &[String], primary: &str, alias: Option<&str>) -> bool {
    list.is_empty() || explicit_list_allows(list, primary, alias)
}

fn explicit_list_allows(list: &[String], primary: &str, alias: Option<&str>) -> bool {
    list.iter().any(|item| {
        let value = item.trim();
        value == "*"
            || value == primary
            || alias.is_some_and(|alias| value == alias || value == format!("@{alias}"))
    })
}

fn has_list_entries(list: &[String]) -> bool {
    list.iter().any(|item| !item.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::config::GatewayAccessConfig;
    use crate::gateway::pairing::GatewayPairingStore;
    use tempfile::TempDir;

    #[test]
    fn dm_is_open_by_default() -> Result<()> {
        let dir = TempDir::new()?;
        let store = GatewayPairingStore::new(dir.path().join("pairing"))?;
        let input = InboundMessageInput {
            channel: "telegram".to_string(),
            conversation_id: "123".to_string(),
            thread_id: None,
            chat_type: Some("dm".to_string()),
            sender_id: Some("u1".to_string()),
            message_id: None,
            text: "hi".to_string(),
            attachments: Vec::new(),
            timestamp: None,
        };

        let decision = evaluate_gateway_access(&input, &GatewayChannelConfig::default(), &store)?;
        assert_eq!(decision, GatewayAccessDecision::Allowed);
        Ok(())
    }

    #[test]
    fn dm_pairing_can_be_enabled() -> Result<()> {
        let dir = TempDir::new()?;
        let store = GatewayPairingStore::new(dir.path().join("pairing"))?;
        let config = GatewayChannelConfig {
            access: GatewayAccessConfig {
                dm_policy: "pairing".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let input = InboundMessageInput {
            channel: "telegram".to_string(),
            conversation_id: "123".to_string(),
            thread_id: None,
            chat_type: Some("dm".to_string()),
            sender_id: Some("u1".to_string()),
            message_id: None,
            text: "hi".to_string(),
            attachments: Vec::new(),
            timestamp: None,
        };

        let decision = evaluate_gateway_access(&input, &config, &store)?;
        assert!(matches!(
            decision,
            GatewayAccessDecision::PairingRequired { .. }
        ));
        Ok(())
    }

    #[test]
    fn dm_pairing_allows_preapproved_sender() -> Result<()> {
        let dir = TempDir::new()?;
        let store = GatewayPairingStore::new(dir.path().join("pairing"))?;
        store.approve_user("telegram", "u1", Some("owner".to_string()))?;
        let config = GatewayChannelConfig {
            access: GatewayAccessConfig {
                dm_policy: "pairing".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let input = InboundMessageInput {
            channel: "telegram".to_string(),
            conversation_id: "123".to_string(),
            thread_id: None,
            chat_type: Some("dm".to_string()),
            sender_id: Some("u1".to_string()),
            message_id: None,
            text: "hi".to_string(),
            attachments: Vec::new(),
            timestamp: None,
        };

        assert_eq!(
            evaluate_gateway_access(&input, &config, &store)?,
            GatewayAccessDecision::Allowed
        );
        Ok(())
    }

    #[test]
    fn dm_allowed_users_restricts_default_open_access() -> Result<()> {
        let dir = TempDir::new()?;
        let store = GatewayPairingStore::new(dir.path().join("pairing"))?;
        let config = GatewayChannelConfig {
            allowed_users: vec!["u2".to_string()],
            ..Default::default()
        };
        let input = InboundMessageInput {
            channel: "telegram".to_string(),
            conversation_id: "123".to_string(),
            thread_id: None,
            chat_type: Some("dm".to_string()),
            sender_id: Some("u1".to_string()),
            message_id: None,
            text: "hi".to_string(),
            attachments: Vec::new(),
            timestamp: None,
        };

        assert_eq!(
            evaluate_gateway_access(&input, &config, &store)?,
            GatewayAccessDecision::Blocked {
                reason: "sender `u1` is not in allowed_users".to_string()
            }
        );
        Ok(())
    }

    #[test]
    fn allowlisted_group_is_allowed() -> Result<()> {
        let dir = TempDir::new()?;
        let store = GatewayPairingStore::new(dir.path().join("pairing"))?;
        let config = GatewayChannelConfig {
            allowed_chats: vec!["C1".to_string()],
            ..Default::default()
        };
        let input = InboundMessageInput {
            channel: "slack".to_string(),
            conversation_id: "C1".to_string(),
            thread_id: None,
            chat_type: Some("channel".to_string()),
            sender_id: Some("U1".to_string()),
            message_id: None,
            text: "hi".to_string(),
            attachments: Vec::new(),
            timestamp: None,
        };

        assert_eq!(
            evaluate_gateway_access(&input, &config, &store)?,
            GatewayAccessDecision::Allowed
        );
        Ok(())
    }
}
