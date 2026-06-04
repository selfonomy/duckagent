use crate::memory::{
    AddMemoryRequest, ForgetMemoryRequest, MemoryKind, MemoryScope, MemoryStore, PatchMemoryRequest,
};
use crate::tools::CallCapabilityInput;
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub const CAPABILITIES: &[&str] = &["get_memory", "add_memory", "patch_memory", "forget_memory"];

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScopeInput {
    Global,
    Workspace,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKindInput {
    Fact,
    Preference,
    Procedure,
    Episode,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct GetMemoryInput {
    /// Memory scope. Use global for durable user-level preferences and workspace for current-project knowledge.
    pub scope: MemoryScopeInput,
    /// Exact memory title from ACTIVE MEMORY, or the intended canonical title when checking a candidate.
    pub title: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct AddMemoryInput {
    /// Memory scope. Use global for user-level preferences and workspace for current-project knowledge.
    pub scope: MemoryScopeInput,
    /// Canonical short memory point title.
    pub title: String,
    /// Memory kind.
    pub kind: MemoryKindInput,
    /// One-sentence actionable catalog summary shown in ACTIVE MEMORY.
    pub summary: String,
    /// Full durable memory content.
    pub content: String,
    /// Why this memory is being added.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct PatchMemoryInput {
    /// Memory scope.
    pub scope: MemoryScopeInput,
    /// Exact existing memory title from ACTIVE MEMORY.
    pub title: String,
    /// Optional replacement kind.
    #[serde(default)]
    pub kind: Option<MemoryKindInput>,
    /// Optional replacement actionable catalog summary.
    #[serde(default)]
    pub summary: Option<String>,
    /// Optional strict unified diff to apply to the existing full durable content.
    #[serde(default)]
    pub patch: Option<String>,
    /// Why this memory is being changed.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct ForgetMemoryInput {
    /// Memory scope.
    pub scope: MemoryScopeInput,
    /// Exact existing memory title from ACTIVE MEMORY.
    pub title: String,
    /// Why this memory should be forgotten.
    pub reason: String,
}

pub struct MemoryBuiltinContext<'a> {
    pub memory_store: &'a MemoryStore,
    pub changed: Arc<AtomicBool>,
    pub source_session_id: &'a str,
}

pub fn is_memory_capability(capability: &str) -> bool {
    CAPABILITIES.contains(&capability)
}

pub fn execute_memory_builtin(
    input: CallCapabilityInput,
    context: &MemoryBuiltinContext<'_>,
) -> Result<String> {
    match input.capability.trim() {
        "get_memory" => {
            let args = serde_json::from_value::<GetMemoryInput>(input.args)
                .context("failed to parse get_memory args")?;
            let scope = memory_scope_from_input(args.scope);
            let result = context.memory_store.get_memory(scope, &args.title)?;
            Ok(serde_json::to_string(&result).context("failed to serialize get_memory output")?)
        }
        "add_memory" => {
            let args = serde_json::from_value::<AddMemoryInput>(input.args)
                .context("failed to parse add_memory args")?;
            let item = context.memory_store.add_memory(AddMemoryRequest {
                scope: memory_scope_from_input(args.scope),
                title: args.title,
                kind: memory_kind_from_input(args.kind),
                summary: args.summary,
                content: args.content,
                reason: args.reason,
                source_session_id: Some(context.source_session_id.to_string()),
            })?;
            context.changed.store(true, Ordering::SeqCst);
            Ok(serde_json::to_string(&item).context("failed to serialize add_memory output")?)
        }
        "patch_memory" => {
            let args = serde_json::from_value::<PatchMemoryInput>(input.args)
                .context("failed to parse patch_memory args")?;
            let item = context.memory_store.patch_memory(PatchMemoryRequest {
                scope: memory_scope_from_input(args.scope),
                title: args.title,
                kind: args.kind.map(memory_kind_from_input),
                summary: args.summary,
                patch: args.patch,
                reason: args.reason,
                source_session_id: Some(context.source_session_id.to_string()),
            })?;
            context.changed.store(true, Ordering::SeqCst);
            Ok(serde_json::to_string(&item).context("failed to serialize patch_memory output")?)
        }
        "forget_memory" => {
            let args = serde_json::from_value::<ForgetMemoryInput>(input.args)
                .context("failed to parse forget_memory args")?;
            let item = context.memory_store.forget_memory(ForgetMemoryRequest {
                scope: memory_scope_from_input(args.scope),
                title: args.title,
                reason: args.reason,
                source_session_id: Some(context.source_session_id.to_string()),
            })?;
            context.changed.store(true, Ordering::SeqCst);
            Ok(serde_json::to_string(&item).context("failed to serialize forget_memory output")?)
        }
        "" => Ok(super::unavailable_capability_result(
            "MemoryAgent",
            "(missing capability)",
            CAPABILITIES,
        )),
        other => Ok(super::unavailable_capability_result(
            "MemoryAgent",
            other,
            CAPABILITIES,
        )),
    }
}

fn memory_scope_from_input(scope: MemoryScopeInput) -> MemoryScope {
    match scope {
        MemoryScopeInput::Global => MemoryScope::Global,
        MemoryScopeInput::Workspace => MemoryScope::Workspace,
    }
}

fn memory_kind_from_input(kind: MemoryKindInput) -> MemoryKind {
    match kind {
        MemoryKindInput::Fact => MemoryKind::Fact,
        MemoryKindInput::Preference => MemoryKind::Preference,
        MemoryKindInput::Procedure => MemoryKind::Procedure,
        MemoryKindInput::Episode => MemoryKind::Episode,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryItem, MemoryStatus};
    use serde_json::{Value, json};
    use std::fs;
    use tempfile::{TempDir, tempdir};

    struct MemoryCapabilityFixture {
        _dir: TempDir,
        memories_dir: std::path::PathBuf,
        store: MemoryStore,
        changed: Arc<AtomicBool>,
    }

    impl MemoryCapabilityFixture {
        fn new() -> Result<Self> {
            let dir = tempdir()?;
            let workspace_root = dir.path().join("repo");
            fs::create_dir_all(&workspace_root)?;
            let memories_dir = dir.path().join("memories");
            let store = MemoryStore::new(memories_dir.clone(), workspace_root)?;
            Ok(Self {
                _dir: dir,
                memories_dir,
                store,
                changed: Arc::new(AtomicBool::new(false)),
            })
        }

        fn context(&self) -> MemoryBuiltinContext<'_> {
            MemoryBuiltinContext {
                memory_store: &self.store,
                changed: self.changed.clone(),
                source_session_id: "source_session",
            }
        }

        fn execute(&self, capability: &str, args: Value) -> Result<String> {
            execute_memory_builtin(
                CallCapabilityInput {
                    capability: capability.to_string(),
                    args,
                    purpose: "test memory capability".to_string(),
                },
                &self.context(),
            )
        }

        fn changed(&self) -> bool {
            self.changed.load(Ordering::SeqCst)
        }

        fn global_memory_file(&self) -> std::path::PathBuf {
            self.memories_dir.join("global").join("memories.jsonl")
        }
    }

    #[test]
    fn add_memory_capability_writes_item_sets_changed_and_records_source_session() -> Result<()> {
        let fixture = MemoryCapabilityFixture::new()?;

        let output = fixture.execute(
            "add_memory",
            json!({
                "scope": "global",
                "title": "User response language preference",
                "kind": "preference",
                "summary": "User wants responses in Chinese by default.",
                "content": "User explicitly requested responses in Chinese by default.",
                "reason": "explicit user preference"
            }),
        )?;

        let item: MemoryItem = serde_json::from_str(&output)?;
        assert_eq!(item.title, "User response language preference");
        assert_eq!(item.kind, MemoryKind::Preference);
        assert_eq!(item.status, MemoryStatus::Active);
        assert!(fixture.changed());

        let loaded = fixture
            .store
            .get_memory(MemoryScope::Global, "User response language preference")?
            .expect("memory should be stored");
        assert_eq!(
            loaded.content,
            "User explicitly requested responses in Chinese by default."
        );

        let jsonl = fs::read_to_string(fixture.global_memory_file())?;
        assert!(jsonl.contains(r#""source_session_id":"source_session""#));
        assert!(jsonl.contains(r#""reason":"explicit user preference""#));

        Ok(())
    }

    #[test]
    fn add_memory_capability_rejects_legacy_name_description_fields() -> Result<()> {
        let fixture = MemoryCapabilityFixture::new()?;

        let err = fixture
            .execute(
                "add_memory",
                json!({
                    "scope": "global",
                    "name": "User preferred name",
                    "kind": "preference",
                    "description": "User wants to be called n.",
                    "content": "User wants to be called n.",
                    "reason": "legacy invalid contract"
                }),
            )
            .expect_err("model-facing memory capabilities should reject legacy keys");

        let err = format!("{err:#}");
        assert!(err.contains("failed to parse add_memory args"));
        assert!(err.contains("missing field") || err.contains("unknown field"));
        assert!(!fixture.changed());

        Ok(())
    }

    #[test]
    fn get_memory_capability_returns_option_without_setting_changed() -> Result<()> {
        let fixture = MemoryCapabilityFixture::new()?;
        fixture.store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Global,
            title: "Default language".to_string(),
            kind: MemoryKind::Preference,
            summary: "Default Chinese".to_string(),
            content: "User prefers Chinese.".to_string(),
            reason: None,
            source_session_id: None,
        })?;
        fixture.changed.store(false, Ordering::SeqCst);

        let output = fixture.execute(
            "get_memory",
            json!({
                "scope": "global",
                "title": "Default language"
            }),
        )?;

        let item: Option<MemoryItem> = serde_json::from_str(&output)?;
        assert_eq!(
            item.expect("memory should exist").content,
            "User prefers Chinese."
        );
        assert!(!fixture.changed());

        let missing = fixture.execute(
            "get_memory",
            json!({
                "scope": "global",
                "title": "Missing item"
            }),
        )?;
        let missing: Option<MemoryItem> = serde_json::from_str(&missing)?;
        assert!(missing.is_none());
        assert!(!fixture.changed());

        Ok(())
    }

    #[test]
    fn patch_memory_capability_updates_existing_item_with_unified_diff() -> Result<()> {
        let fixture = MemoryCapabilityFixture::new()?;
        fixture.store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Workspace,
            title: "Project workflow".to_string(),
            kind: MemoryKind::Procedure,
            summary: "Old workflow".to_string(),
            content: "Old content".to_string(),
            reason: None,
            source_session_id: None,
        })?;

        let output = fixture.execute(
            "patch_memory",
            json!({
                "scope": "workspace",
                "title": "Project workflow",
                "summary": "New workflow",
                "patch": "--- memory\n+++ memory\n@@ -1 +1 @@\n-Old content\n+New content",
                "reason": "workflow corrected"
            }),
        )?;

        let item: MemoryItem = serde_json::from_str(&output)?;
        assert_eq!(item.kind, MemoryKind::Procedure);
        assert_eq!(item.summary, "New workflow");
        assert_eq!(item.content, "New content");
        assert_eq!(item.status, MemoryStatus::Active);
        assert!(fixture.changed());

        Ok(())
    }

    #[test]
    fn patch_memory_capability_rejects_content_replacement_contract() -> Result<()> {
        let fixture = MemoryCapabilityFixture::new()?;
        fixture.store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Global,
            title: "User preferred name".to_string(),
            kind: MemoryKind::Preference,
            summary: "User wants to be called x.".to_string(),
            content: "User wants to be called x.".to_string(),
            reason: None,
            source_session_id: None,
        })?;

        let err = fixture
            .execute(
                "patch_memory",
                json!({
                    "scope": "global",
                    "title": "User preferred name",
                    "summary": "User wants to be called n instead of x.",
                    "content": "User wants to be called n.",
                    "reason": "old invalid contract"
                }),
            )
            .expect_err("patch_memory must reject direct content replacement");

        assert!(format!("{err:#}").contains("failed to parse patch_memory args"));
        assert!(format!("{err:#}").contains("unknown field `content`"));
        assert!(!fixture.changed());
        let loaded = fixture
            .store
            .get_memory(MemoryScope::Global, "User preferred name")?
            .expect("memory should remain unchanged");
        assert_eq!(loaded.summary, "User wants to be called x.");
        assert_eq!(loaded.content, "User wants to be called x.");

        Ok(())
    }

    #[test]
    fn patch_memory_capability_rejects_mismatched_patch_without_changed_flag() -> Result<()> {
        let fixture = MemoryCapabilityFixture::new()?;
        fixture.store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Global,
            title: "User preferred name".to_string(),
            kind: MemoryKind::Preference,
            summary: "User wants to be called x.".to_string(),
            content: "User wants to be called x.".to_string(),
            reason: None,
            source_session_id: None,
        })?;

        let err = fixture
            .execute(
                "patch_memory",
                json!({
                    "scope": "global",
                    "title": "User preferred name",
                    "summary": "User wants to be called n instead of x.",
                    "patch": "--- memory\n+++ memory\n@@ -1 +1 @@\n-User wants to be called y.\n+User wants to be called n.",
                    "reason": "mismatched patch"
                }),
            )
            .expect_err("mismatched patch should fail");

        assert!(err.to_string().contains("deletion mismatch"));
        assert!(!fixture.changed());
        let jsonl = fs::read_to_string(fixture.global_memory_file())?;
        assert_eq!(jsonl.lines().count(), 1);

        Ok(())
    }

    #[test]
    fn forget_memory_capability_marks_item_forgotten_and_removes_active_catalog_entry() -> Result<()>
    {
        let fixture = MemoryCapabilityFixture::new()?;
        fixture.store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Global,
            title: "Temporary preference".to_string(),
            kind: MemoryKind::Preference,
            summary: "Temporary".to_string(),
            content: "Temporary content".to_string(),
            reason: None,
            source_session_id: None,
        })?;

        let output = fixture.execute(
            "forget_memory",
            json!({
                "scope": "global",
                "title": "Temporary preference",
                "reason": "user asked to forget"
            }),
        )?;

        let item: MemoryItem = serde_json::from_str(&output)?;
        assert_eq!(item.status, MemoryStatus::Forgotten);
        assert!(fixture.changed());
        let replayed = fixture
            .store
            .get_memory(MemoryScope::Global, "Temporary preference")?
            .expect("forgotten item should leave tombstone");
        assert_eq!(replayed.status, MemoryStatus::Forgotten);
        assert!(
            !fixture
                .store
                .render_active_catalog()?
                .contains("Temporary preference")
        );

        Ok(())
    }

    #[test]
    fn unavailable_memory_capability_returns_structured_result_without_changed() -> Result<()> {
        let fixture = MemoryCapabilityFixture::new()?;

        let output = fixture.execute("read_file", json!({"path": "src/main.rs"}))?;
        let value: Value = serde_json::from_str(&output)?;
        assert_eq!(value["status"], "unavailable");
        assert_eq!(value["agent_mode"], "MemoryAgent");
        assert_eq!(value["capability"], "read_file");
        assert!(
            value["allowed_capabilities"]
                .as_array()
                .expect("allowed capabilities should be an array")
                .iter()
                .any(|item| item == "patch_memory")
        );
        assert!(!fixture.changed());

        let missing = fixture.execute("", json!({}))?;
        let missing: Value = serde_json::from_str(&missing)?;
        assert_eq!(missing["capability"], "(missing capability)");
        assert!(!fixture.changed());

        Ok(())
    }

    #[test]
    fn memory_capability_errors_do_not_set_changed() -> Result<()> {
        let fixture = MemoryCapabilityFixture::new()?;

        let parse_err = fixture
            .execute(
                "add_memory",
                json!({
                    "scope": "global",
                    "title": "Missing kind",
                    "summary": "Description",
                    "content": "Content"
                }),
            )
            .expect_err("missing required kind should fail parsing");
        assert!(format!("{parse_err:#}").contains("failed to parse add_memory args"));
        assert!(!fixture.changed());

        fixture.store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Global,
            title: "Existing item".to_string(),
            kind: MemoryKind::Fact,
            summary: "Description".to_string(),
            content: "Content".to_string(),
            reason: None,
            source_session_id: None,
        })?;

        let duplicate = fixture
            .execute(
                "add_memory",
                json!({
                    "scope": "global",
                    "title": "Existing item",
                    "kind": "fact",
                    "summary": "New description",
                    "content": "New content"
                }),
            )
            .expect_err("duplicate active add should fail");
        assert!(duplicate.to_string().contains("use patch_memory"));
        assert!(!fixture.changed());

        let no_op = fixture
            .execute(
                "patch_memory",
                json!({
                    "scope": "global",
                    "title": "Existing item",
                    "summary": "Description"
                }),
            )
            .expect_err("no-op patch should fail");
        assert!(no_op.to_string().contains("made no changes"));
        assert!(!fixture.changed());

        let missing_forget = fixture
            .execute(
                "forget_memory",
                json!({
                    "scope": "global",
                    "title": "Missing item",
                    "reason": "user asked"
                }),
            )
            .expect_err("forget missing should fail");
        assert!(
            missing_forget
                .to_string()
                .contains("active memory not found")
        );
        assert!(!fixture.changed());

        Ok(())
    }
}
