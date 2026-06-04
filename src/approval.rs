use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Once,
    Session,
    Always,
    Forbidden,
}

impl ApprovalDecision {
    pub fn approved(self) -> bool {
        !matches!(self, ApprovalDecision::Forbidden)
    }

    pub fn options() -> [ApprovalDecision; 4] {
        [
            ApprovalDecision::Once,
            ApprovalDecision::Session,
            ApprovalDecision::Always,
            ApprovalDecision::Forbidden,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleHit {
    pub rule_id: String,
    pub description: String,
}

#[derive(Debug)]
pub struct ApprovalPrompt {
    pub command: String,
    pub options: [ApprovalDecision; 4],
    pub response_tx: std::sync::mpsc::Sender<ApprovalResponse>,
}

#[derive(Debug, Clone)]
pub struct ApprovalResponse {
    pub decision: ApprovalDecision,
}

pub trait ApprovalProvider: Send + Sync {
    fn request_approval(
        &self,
        command: &str,
        rule_hits: &[RuleHit],
        options: [ApprovalDecision; 4],
    ) -> Option<ApprovalResponse>;
}

#[cfg(test)]
pub struct DenyApprovalProvider;

#[cfg(test)]
impl ApprovalProvider for DenyApprovalProvider {
    fn request_approval(
        &self,
        _command: &str,
        _rule_hits: &[RuleHit],
        _options: [ApprovalDecision; 4],
    ) -> Option<ApprovalResponse> {
        Some(ApprovalResponse {
            decision: ApprovalDecision::Forbidden,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct ApprovalPolicy {
    session_allow_commands: HashSet<String>,
    loop_forbidden_commands: HashSet<String>,
}

impl ApprovalPolicy {
    pub fn load_default() -> Result<Self> {
        Ok(Self::default())
    }

    pub fn is_allowed(&self, command: &str) -> bool {
        self.session_allow_commands
            .contains(&normalize_command_key(command))
    }

    pub fn is_forbidden(&self, command: &str) -> bool {
        self.loop_forbidden_commands
            .contains(&normalize_command_key(command))
    }

    pub fn clear_loop_forbidden(&mut self) {
        self.loop_forbidden_commands.clear();
    }

    pub fn apply_decision(&mut self, decision: ApprovalDecision, command: &str) {
        let key = normalize_command_key(command);
        match decision {
            ApprovalDecision::Once => {}
            ApprovalDecision::Forbidden => {
                self.loop_forbidden_commands.insert(key);
            }
            ApprovalDecision::Session | ApprovalDecision::Always => {
                self.loop_forbidden_commands.remove(&key);
                self.session_allow_commands.insert(key);
            }
        }
    }
}

fn normalize_command_key(command: &str) -> String {
    crate::sandbox::shell_permissions::normalized_shell_command(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn once_does_not_persist_state() {
        let mut policy = ApprovalPolicy::default();
        policy.apply_decision(ApprovalDecision::Once, "chmod 777 a.txt");
        assert!(!policy.is_allowed("chmod 777 a.txt"));
    }

    #[test]
    fn session_allows_in_memory() {
        let mut policy = ApprovalPolicy::default();
        policy.apply_decision(ApprovalDecision::Session, "chmod 777 a.txt");
        assert!(policy.is_allowed("chmod 777 a.txt"));
        assert!(!policy.is_allowed("chmod 777 b.txt"));
    }

    #[test]
    fn forbidden_does_not_persist_state() {
        let mut policy = ApprovalPolicy::default();
        policy.apply_decision(ApprovalDecision::Forbidden, "chmod 777 a.txt");
        assert!(!policy.is_allowed("chmod 777 a.txt"));
        assert!(policy.is_forbidden("chmod 777 a.txt"));
    }
}
