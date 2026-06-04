use super::zalo::ZaloAdapter;
use crate::auth::GatewayCredentialEntry;
use crate::gateway::config::GatewayChannelConfig;
use anyhow::Result;

pub(in crate::gateway::channels) fn new_adapter(
    config: &GatewayChannelConfig,
    credentials: &GatewayCredentialEntry,
) -> Result<ZaloAdapter> {
    super::zalo::new_user_adapter(config, credentials)
}
