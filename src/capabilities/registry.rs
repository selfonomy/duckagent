use crate::approval::{
    ApprovalDecision, ApprovalPolicy, ApprovalProvider, ApprovalResponse, RuleHit,
};
use crate::capabilities::builtins::{
    BuiltinExecutionContext, BuiltinToolSpec, builtin_tool_specs, execute_builtin, load_mcp,
};
use crate::mcp::config::{McpServerConfig, McpTransportKind, load_mcp_servers};
use crate::mcp::result::map_mcp_tool_result;
use crate::mcp::transport_stdio::McpToolDefinition;
use crate::sandbox::config::ResolvedSandbox;
use crate::sandbox::permissions::{PermissionMatch, tool_permission};
use crate::tools::RuntimeToolInput;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct RuntimeToolRegistry {
    entries: Vec<RuntimeToolEntry>,
    by_name: HashMap<String, usize>,
}

#[derive(Debug, Clone)]
pub struct RuntimeToolEntry {
    pub exposed_name: String,
    pub original_name: String,
    pub description: String,
    pub input_schema: Value,
    pub source: RuntimeToolSource,
}

#[derive(Debug, Clone)]
pub enum RuntimeToolSource {
    Builtin,
    LoadMcp,
    Mcp {
        server_name: String,
        server_config: McpServerConfig,
    },
}

pub struct RuntimeToolExecutionContext<'a> {
    pub builtin: BuiltinExecutionContext<'a>,
    pub permission: PermissionContext,
}

#[derive(Clone)]
pub struct PermissionContext {
    pub approval_policy: Arc<Mutex<ApprovalPolicy>>,
    pub approval_provider: Arc<dyn ApprovalProvider>,
    pub sandbox: ResolvedSandbox,
}

enum ApprovalPersistence {
    Tool { name: String },
    Shell { normalized_command: String },
}

impl RuntimeToolRegistry {
    pub fn load_best_effort() -> Self {
        Self::load_best_effort_with_approval(None)
    }

    pub fn load_best_effort_for_session(approval_provider: Arc<dyn ApprovalProvider>) -> Self {
        Self::load_best_effort_with_approval(Some(approval_provider))
    }

    fn load_best_effort_with_approval(
        approval_provider: Option<Arc<dyn ApprovalProvider>>,
    ) -> Self {
        let mut registry = Self::new();
        if let Ok(servers) = load_mcp_servers() {
            registry.register_external_servers(servers, approval_provider);
        }
        registry
    }

    pub fn new() -> Self {
        let mut registry = Self {
            entries: Vec::new(),
            by_name: HashMap::new(),
        };
        for spec in builtin_tool_specs() {
            registry.register_builtin(spec);
        }
        registry.register_load_mcp();
        registry
    }

    pub fn entries(&self) -> &[RuntimeToolEntry] {
        &self.entries
    }

    pub fn execute(
        &self,
        input: RuntimeToolInput,
        context: &RuntimeToolExecutionContext<'_>,
    ) -> Result<String> {
        let capability = input.capability.trim();
        if capability.is_empty() {
            return Ok("Tool error: call_capability.capability must be non-empty.".to_string());
        }
        let Some(entry) = self
            .by_name
            .get(capability)
            .and_then(|idx| self.entries.get(*idx))
        else {
            return Ok(format!(
                "Tool error: unknown call_capability capability `{capability}`."
            ));
        };
        if let Err(error) = check_runtime_permission(&input, &context.permission) {
            audit_runtime_tool_permission_error(entry, context, &error);
            return Err(error);
        }
        let result = match &entry.source {
            RuntimeToolSource::Builtin => execute_builtin(input, &context.builtin),
            RuntimeToolSource::LoadMcp => self.load_mcp_tool(input.args),
            RuntimeToolSource::Mcp {
                server_name,
                server_config,
            } => call_mcp_tool(
                server_name,
                server_config,
                &entry.original_name,
                input.args,
                context.permission.approval_provider.clone(),
            ),
        };
        audit_runtime_tool(entry, context, &result);
        result
    }

    pub fn render_runtime_tools(&self) -> String {
        self.entries
            .iter()
            .map(render_runtime_tool)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn register_builtin(&mut self, spec: BuiltinToolSpec) {
        self.push_entry(RuntimeToolEntry {
            exposed_name: spec.name.to_string(),
            original_name: spec.name.to_string(),
            description: spec.description.to_string(),
            input_schema: spec.input_schema,
            source: RuntimeToolSource::Builtin,
        });
    }

    fn register_load_mcp(&mut self) {
        let spec = load_mcp::spec();
        self.push_entry(RuntimeToolEntry {
            exposed_name: spec.name.to_string(),
            original_name: spec.name.to_string(),
            description: spec.description.to_string(),
            input_schema: spec.input_schema,
            source: RuntimeToolSource::LoadMcp,
        });
    }

    fn register_external_servers(
        &mut self,
        servers: BTreeMap<String, McpServerConfig>,
        approval_provider: Option<Arc<dyn ApprovalProvider>>,
    ) {
        for (server_name, config) in servers {
            if !config.is_enabled() {
                continue;
            }
            let Ok(tools) = discover_server_tools(&server_name, &config, approval_provider.clone())
            else {
                continue;
            };
            for tool in tools {
                self.register_external_tool(server_name.clone(), config.clone(), tool);
            }
        }
    }

    fn register_external_tool(
        &mut self,
        server_name: String,
        server_config: McpServerConfig,
        tool: McpToolDefinition,
    ) {
        let description = tool
            .description
            .clone()
            .or(tool.title.clone())
            .unwrap_or_else(|| format!("MCP tool `{}` from server `{server_name}`", tool.name));
        let exposed_name = format!(
            "{}_{}",
            normalize_capability_segment(&server_name),
            normalize_capability_segment(&tool.name)
        );
        self.push_entry(RuntimeToolEntry {
            exposed_name,
            original_name: tool.name,
            description,
            input_schema: if tool.input_schema.is_null() {
                serde_json::json!({"type": "object"})
            } else {
                tool.input_schema
            },
            source: RuntimeToolSource::Mcp {
                server_name,
                server_config,
            },
        });
    }

    fn push_entry(&mut self, mut entry: RuntimeToolEntry) {
        entry.exposed_name = self.unique_name(&entry.exposed_name);
        self.by_name
            .insert(entry.exposed_name.clone(), self.entries.len());
        self.entries.push(entry);
    }

    fn unique_name(&self, base: &str) -> String {
        if !self.by_name.contains_key(base) {
            return base.to_string();
        }
        let mut index = 2;
        loop {
            let candidate = format!("{base}{index}");
            if !self.by_name.contains_key(&candidate) {
                return candidate;
            }
            index += 1;
        }
    }

    fn load_mcp_tool(&self, args: Value) -> Result<String> {
        let name = load_mcp::parse_requested_name(args)?;
        if name.is_empty() {
            return Ok("Tool error: load_mcp.name must be non-empty.".to_string());
        }
        let Some(entry) = self
            .by_name
            .get(&name)
            .and_then(|idx| self.entries.get(*idx))
        else {
            return Ok(format!("Tool error: unknown MCP tool `{name}`."));
        };
        let RuntimeToolSource::Mcp { server_name, .. } = &entry.source else {
            return Ok(format!(
                "Tool error: `{name}` is not an MCP tool. Use load_mcp only for MCP tools."
            ));
        };
        load_mcp::render_tool_metadata(load_mcp::LoadMcpToolMetadata {
            name: entry.exposed_name.clone(),
            server: server_name.clone(),
            original_name: entry.original_name.clone(),
            description: entry.description.clone(),
            input_schema: entry.input_schema.clone(),
        })
    }
}

fn audit_runtime_tool(
    entry: &RuntimeToolEntry,
    context: &RuntimeToolExecutionContext<'_>,
    result: &Result<String>,
) {
    let mut event = crate::audit::AuditEvent::new("tool", "call");
    event.session_id = Some(context.builtin.session_id.to_string());
    event.sandbox = Some(context.permission.sandbox.name.clone());
    event.target = Some(entry.exposed_name.clone());
    event.outcome = if result.is_ok() { "ok" } else { "error" }.to_string();
    event.fields = serde_json::json!({
        "source": match &entry.source {
            RuntimeToolSource::Builtin => "builtin",
            RuntimeToolSource::LoadMcp => "load_mcp",
            RuntimeToolSource::Mcp { .. } => "mcp",
        },
        "original_name": entry.original_name,
        "result_bytes": result.as_ref().map(|text| text.len()).unwrap_or(0),
        "error": result.as_ref().err().map(|error| format!("{error:#}")),
    });
    crate::audit::record(event);
}

fn audit_runtime_tool_permission_error(
    entry: &RuntimeToolEntry,
    context: &RuntimeToolExecutionContext<'_>,
    error: &anyhow::Error,
) {
    let mut event = crate::audit::AuditEvent::new("tool", "call");
    event.session_id = Some(context.builtin.session_id.to_string());
    event.sandbox = Some(context.permission.sandbox.name.clone());
    event.target = Some(entry.exposed_name.clone());
    event.outcome = "blocked".to_string();
    event.fields = serde_json::json!({
        "source": match &entry.source {
            RuntimeToolSource::Builtin => "builtin",
            RuntimeToolSource::LoadMcp => "load_mcp",
            RuntimeToolSource::Mcp { .. } => "mcp",
        },
        "original_name": entry.original_name,
        "error": format!("{error:#}"),
    });
    crate::audit::record(event);
}

fn check_runtime_permission(input: &RuntimeToolInput, context: &PermissionContext) -> Result<()> {
    check_tool_permission(input, context)?;
    match input.capability.trim() {
        "process_start" => check_process_start_permission(input, context),
        _ => Ok(()),
    }
}

fn check_tool_permission(input: &RuntimeToolInput, context: &PermissionContext) -> Result<()> {
    match tool_permission(&context.sandbox, input.capability.trim()) {
        PermissionMatch::Deny => bail!(
            "Tool error: sandbox `{}` permissions deny tool `{}`",
            context.sandbox.name,
            input.capability.trim()
        ),
        PermissionMatch::Ask => {
            let rule_hits = vec![RuleHit {
                rule_id: "permissions.tools.ask".to_string(),
                description: format!(
                    "tool `{}` matched sandbox ask rule",
                    input.capability.trim()
                ),
            }];
            request_policy_approval(
                &format!("tool {}", input.capability.trim()),
                &rule_hits,
                context,
                ApprovalPersistence::Tool {
                    name: input.capability.trim().to_string(),
                },
            )
        }
        PermissionMatch::Allow | PermissionMatch::Unspecified => Ok(()),
    }
}

fn check_process_start_permission(
    input: &RuntimeToolInput,
    context: &PermissionContext,
) -> Result<()> {
    let command = input
        .args
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    if command.is_empty() {
        return Ok(());
    }
    let evaluation =
        crate::sandbox::shell_permissions::evaluate_shell_permission(&context.sandbox, command);
    match evaluation.action {
        PermissionMatch::Deny => bail!(
            "Tool error: process_start blocked by sandbox shell deny rule.\nCommand: {}",
            evaluation.normalized_command
        ),
        PermissionMatch::Ask => {
            let normalized_command = evaluation.normalized_command.clone();
            let rule_hits = vec![RuleHit {
                rule_id: "permissions.shell.ask".to_string(),
                description: format!(
                    "process_start matched sandbox shell ask rule or syntax: {:?}",
                    evaluation.reason
                ),
            }];
            request_policy_approval(
                &normalized_command,
                &rule_hits,
                context,
                ApprovalPersistence::Shell {
                    normalized_command: normalized_command.clone(),
                },
            )
        }
        PermissionMatch::Allow | PermissionMatch::Unspecified => Ok(()),
    }
}

fn request_policy_approval(
    command: &str,
    rule_hits: &[RuleHit],
    context: &PermissionContext,
    persistence: ApprovalPersistence,
) -> Result<()> {
    let (approved_by_policy, forbidden_by_policy) = {
        let guard = context
            .approval_policy
            .lock()
            .expect("approval policy mutex poisoned");
        (guard.is_allowed(command), guard.is_forbidden(command))
    };
    if forbidden_by_policy {
        bail!(
            "Tool error: blocked by user decision (forbidden).\nCommand: {}\nRules: {}",
            command,
            format_rule_hits(rule_hits)
        );
    }
    if approved_by_policy {
        return Ok(());
    }

    let decision = context
        .approval_provider
        .request_approval(command, rule_hits, ApprovalDecision::options())
        .unwrap_or(ApprovalResponse {
            decision: ApprovalDecision::Forbidden,
        })
        .decision;

    {
        let mut guard = context
            .approval_policy
            .lock()
            .expect("approval policy mutex poisoned");
        guard.apply_decision(decision, command);
    }

    if matches!(decision, ApprovalDecision::Always) {
        match persistence {
            ApprovalPersistence::Tool { name } => {
                crate::sandbox::config::append_tool_action_to_current_preset(
                    &name,
                    crate::sandbox::config::PermissionAction::Allow,
                )
                .with_context(|| format!("failed to persist tool allow rule for `{name}`"))?;
            }
            ApprovalPersistence::Shell { normalized_command } => {
                crate::sandbox::config::append_shell_allow_to_current_preset(&normalized_command)
                    .with_context(|| {
                    format!("failed to persist shell allow rule for `{normalized_command}`")
                })?;
            }
        }
    }

    if decision.approved() {
        Ok(())
    } else {
        bail!(
            "Tool error: blocked by user decision (forbidden).\nCommand: {}\nRules: {}",
            command,
            format_rule_hits(rule_hits)
        )
    }
}

fn format_rule_hits(rule_hits: &[RuleHit]) -> String {
    rule_hits
        .iter()
        .map(|hit| format!("{} ({})", hit.rule_id, hit.description))
        .collect::<Vec<_>>()
        .join(", ")
}

fn normalize_capability_segment(value: &str) -> String {
    let mut out = String::new();
    let mut last_was_underscore = false;
    for ch in value.chars() {
        let normalized = if ch.is_ascii_alphanumeric() { ch } else { '_' };
        if normalized == '_' {
            if !last_was_underscore && !out.is_empty() {
                out.push('_');
            }
            last_was_underscore = true;
        } else {
            out.push(normalized.to_ascii_lowercase());
            last_was_underscore = false;
        }
    }
    out.trim_matches('_').to_string()
}

fn discover_server_tools(
    server_name: &str,
    config: &McpServerConfig,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
) -> Result<Vec<McpToolDefinition>> {
    match config.effective_transport()? {
        McpTransportKind::Stdio => {
            crate::mcp::transport_stdio::list_tools(config, approval_provider).with_context(|| {
                format!("failed to list tools from MCP stdio server `{server_name}`")
            })
        }
        McpTransportKind::Http | McpTransportKind::Sse => {
            crate::mcp::transport_http::list_tools(server_name, config, approval_provider)
                .with_context(|| {
                    format!("failed to list tools from MCP remote server `{server_name}`")
                })
        }
    }
}

fn call_mcp_tool(
    server_name: &str,
    config: &McpServerConfig,
    original_name: &str,
    args: Value,
    approval_provider: Arc<dyn ApprovalProvider>,
) -> Result<String> {
    let result = match config.effective_transport()? {
        McpTransportKind::Stdio => crate::mcp::transport_stdio::call_tool(
            config,
            original_name,
            args,
            Some(approval_provider),
        ),
        McpTransportKind::Http | McpTransportKind::Sse => crate::mcp::transport_http::call_tool(
            server_name,
            config,
            original_name,
            args,
            Some(approval_provider),
        ),
    }?;
    Ok(map_mcp_tool_result(result))
}

fn render_runtime_tool(entry: &RuntimeToolEntry) -> String {
    let args = serde_json::to_string(&entry.input_schema)
        .unwrap_or_else(|_| "{\"type\":\"object\"}".to_string());
    match &entry.source {
        RuntimeToolSource::Builtin => format!(
            "- `{}`: args = {}. {}",
            entry.exposed_name, args, entry.description
        ),
        RuntimeToolSource::LoadMcp => format!(
            "- `{}`: args = {}. {}",
            entry.exposed_name, args, entry.description
        ),
        RuntimeToolSource::Mcp { server_name, .. } => format!(
            "- `{}`: MCP server `{}` tool `{}`; args = {}. {}",
            entry.exposed_name, server_name, entry.original_name, args, entry.description
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::config::{PermissionAction, SandboxConfig};
    use serde_json::json;

    struct RecordingApprovalProvider {
        decision: ApprovalDecision,
        prompts: Mutex<Vec<String>>,
    }

    impl RecordingApprovalProvider {
        fn new(decision: ApprovalDecision) -> Self {
            Self {
                decision,
                prompts: Mutex::new(Vec::new()),
            }
        }
    }

    impl ApprovalProvider for RecordingApprovalProvider {
        fn request_approval(
            &self,
            command: &str,
            _rule_hits: &[RuleHit],
            _options: [ApprovalDecision; 4],
        ) -> Option<ApprovalResponse> {
            self.prompts
                .lock()
                .expect("approval prompt recorder poisoned")
                .push(command.to_string());
            Some(ApprovalResponse {
                decision: self.decision,
            })
        }
    }

    fn permission_context(
        sandbox: ResolvedSandbox,
        provider: Arc<RecordingApprovalProvider>,
    ) -> PermissionContext {
        PermissionContext {
            approval_policy: Arc::new(Mutex::new(ApprovalPolicy::default())),
            approval_provider: provider,
            sandbox,
        }
    }

    fn runtime_input(capability: &str, args: serde_json::Value) -> RuntimeToolInput {
        RuntimeToolInput {
            capability: capability.to_string(),
            args,
            purpose: "test".to_string(),
        }
    }

    #[test]
    fn builtins_are_first_and_stable() {
        let registry = RuntimeToolRegistry::new();
        let names = registry
            .entries()
            .iter()
            .map(|entry| entry.exposed_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "read_file",
                "search_files",
                "search_content",
                "write_file",
                "edit",
                "apply_patch",
                "process_start",
                "process_read",
                "process_list",
                "process_stop",
                "process_write",
                "process_watch",
                "vision_analyze",
                "web_search",
                "web_extract",
                "load_skill",
                "read_skill_file",
                "cron_create",
                "cron_list",
                "cron_get",
                "cron_update",
                "cron_delete",
                "cron_pause",
                "cron_resume",
                "request_filesystem_access",
                "load_mcp"
            ]
        );
    }

    #[test]
    fn external_mcp_names_are_prefixed_by_server_name() {
        let mut registry = RuntimeToolRegistry::new();
        registry.register_external_tool(
            "mock".to_string(),
            McpServerConfig::default(),
            McpToolDefinition {
                name: "read_file".to_string(),
                title: None,
                description: Some("mock read".to_string()),
                input_schema: json!({"type":"object"}),
            },
        );
        registry.register_external_tool(
            "mock".to_string(),
            McpServerConfig::default(),
            McpToolDefinition {
                name: "read_file".to_string(),
                title: None,
                description: Some("mock read".to_string()),
                input_schema: json!({"type":"object"}),
            },
        );
        let names = registry
            .entries()
            .iter()
            .map(|entry| entry.exposed_name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"mock_read_file"));
        assert!(names.contains(&"mock_read_file2"));
    }

    #[test]
    fn rendered_tools_are_deterministic() {
        let registry = RuntimeToolRegistry::new();
        let first = registry.render_runtime_tools();
        let second = registry.render_runtime_tools();
        assert_eq!(first, second);
        assert!(first.contains("`read_file`"));
        assert!(first.contains("`vision_analyze`"));
        assert!(first.contains("`request_filesystem_access`"));
        assert!(first.contains("`load_mcp`"));
    }

    #[test]
    fn load_mcp_returns_only_external_mcp_tool_schema() -> Result<()> {
        let mut registry = RuntimeToolRegistry::new();
        registry.register_external_tool(
            "docs".to_string(),
            McpServerConfig::default(),
            McpToolDefinition {
                name: "search_docs".to_string(),
                title: None,
                description: Some("search docs".to_string()),
                input_schema: json!({"type":"object","properties":{"query":{"type":"string"}}}),
            },
        );

        let output = registry.load_mcp_tool(json!({"name": "docs_search_docs"}))?;
        assert!(output.contains("\"server\": \"docs\""));
        assert!(output.contains("\"original_name\": \"search_docs\""));

        let builtin = registry.load_mcp_tool(json!({"name": "read_file"}))?;
        assert!(builtin.contains("is not an MCP tool"));
        Ok(())
    }

    #[test]
    fn process_start_shell_deny_blocks_before_approval() {
        let mut sandbox = SandboxConfig::default().resolve(Some("danger")).unwrap();
        sandbox
            .preset
            .permissions
            .shell
            .rules
            .insert("rm -rf".to_string(), PermissionAction::Deny);
        let provider = Arc::new(RecordingApprovalProvider::new(ApprovalDecision::Once));
        let context = permission_context(sandbox, provider.clone());

        let error = check_runtime_permission(
            &runtime_input("process_start", json!({"command": "rm -rf tmp"})),
            &context,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("process_start blocked by sandbox shell deny rule"));
        assert!(
            provider
                .prompts
                .lock()
                .expect("approval prompt recorder poisoned")
                .is_empty()
        );
    }

    #[test]
    fn process_start_shell_ask_requests_approval() {
        let mut sandbox = SandboxConfig::default().resolve(Some("danger")).unwrap();
        sandbox
            .preset
            .permissions
            .shell
            .rules
            .insert("cargo test".to_string(), PermissionAction::Ask);
        let provider = Arc::new(RecordingApprovalProvider::new(ApprovalDecision::Once));
        let context = permission_context(sandbox, provider.clone());

        check_runtime_permission(
            &runtime_input("process_start", json!({"command": "cargo  \"test\""})),
            &context,
        )
        .unwrap();

        let prompts = provider
            .prompts
            .lock()
            .expect("approval prompt recorder poisoned");
        assert_eq!(prompts.as_slice(), ["cargo test"]);
    }

    #[test]
    fn tool_ask_requests_approval_without_shell_persistence() {
        let mut sandbox = SandboxConfig::default().resolve(Some("danger")).unwrap();
        sandbox
            .preset
            .permissions
            .tools
            .insert("dangerous_*".to_string(), PermissionAction::Ask);
        let provider = Arc::new(RecordingApprovalProvider::new(ApprovalDecision::Once));
        let context = permission_context(sandbox, provider.clone());

        check_runtime_permission(&runtime_input("dangerous_tool", json!({})), &context).unwrap();

        let prompts = provider
            .prompts
            .lock()
            .expect("approval prompt recorder poisoned");
        assert_eq!(prompts.as_slice(), ["tool dangerous_tool"]);
    }

    #[test]
    fn request_filesystem_access_obeys_tool_permission_rules() {
        let mut sandbox = SandboxConfig::default().resolve(Some("danger")).unwrap();
        sandbox.preset.permissions.tools.insert(
            "request_filesystem_access".to_string(),
            PermissionAction::Ask,
        );
        let provider = Arc::new(RecordingApprovalProvider::new(ApprovalDecision::Once));
        let context = permission_context(sandbox, provider.clone());

        check_runtime_permission(
            &runtime_input(
                "request_filesystem_access",
                json!({"path": "/tmp/docs", "access": "ro"}),
            ),
            &context,
        )
        .unwrap();

        let prompts = provider
            .prompts
            .lock()
            .expect("approval prompt recorder poisoned");
        assert_eq!(prompts.as_slice(), ["tool request_filesystem_access"]);
    }
}
