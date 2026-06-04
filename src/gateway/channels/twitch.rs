use super::irc::IrcAdapter;
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::{Result, anyhow};

pub(in crate::gateway) fn new_adapter(
    config: &GatewayChannelConfig,
    credentials: &GatewayCredentialEntry,
) -> Result<IrcAdapter> {
    if credentials
        .token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        return Err(anyhow!(
            "twitch gateway credentials require OAuth access token"
        ));
    }
    if credentials
        .username
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        return Err(anyhow!("twitch gateway credentials require bot username"));
    }
    IrcAdapter::new_with_defaults(
        "twitch",
        Some("irc.chat.twitch.tv"),
        6697,
        true,
        config,
        credentials,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::ChannelAdapter;

    #[test]
    fn twitch_requires_token_and_username() {
        let config = GatewayChannelConfig {
            allowed_chats: vec!["#duckagent".to_string()],
            ..Default::default()
        };
        let missing = new_adapter(
            &config,
            &GatewayCredentialEntry {
                channel: "twitch".to_string(),
                ..Default::default()
            },
        );
        assert!(missing.is_err());
    }

    #[test]
    fn twitch_uses_irc_capabilities() -> Result<()> {
        let config = GatewayChannelConfig {
            allowed_chats: vec!["#duckagent".to_string()],
            ..Default::default()
        };
        let adapter = new_adapter(
            &config,
            &GatewayCredentialEntry {
                channel: "twitch".to_string(),
                username: Some("duckagent".to_string()),
                token: Some("oauth:test".to_string()),
                ..Default::default()
            },
        )?;
        let capabilities = adapter.capabilities();
        assert!(!capabilities.media);
        assert!(!capabilities.typing);
        Ok(())
    }
}
