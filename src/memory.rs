use crate::instructions::DynamicContextBlock;
use crate::profiles;
use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

const MEMORIES_DIR_NAME: &str = "memories";
const MEMORY_FILE_NAME: &str = "memories.jsonl";
const WORKSPACE_META_FILE_NAME: &str = "meta.json";
const ACTIVE_MEMORY_CATALOG_TEMPLATE: &str = include_str!("prompts/active-memory-catalog.md");
const ACTIVE_MEMORY_ITEM_TEMPLATE: &str = include_str!("prompts/active-memory-item.md");
const ACTIVE_MEMORY_EMPTY_TEMPLATE: &str = include_str!("prompts/active-memory-empty.md");

#[derive(Debug, Clone)]
pub struct MemoryStore {
    memories_dir: PathBuf,
    workspace_id: String,
    workspace_root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Global,
    Workspace,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Fact,
    Preference,
    Procedure,
    Episode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Active,
    Forgotten,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryItem {
    pub scope: MemoryScope,
    pub title: String,
    pub kind: MemoryKind,
    pub summary: String,
    pub content: String,
    pub updated_at: String,
    pub status: MemoryStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryLine {
    pub timestamp: String,
    #[serde(flatten)]
    pub event: MemoryEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum MemoryEvent {
    Add {
        item: MemoryItem,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_session_id: Option<String>,
    },
    Patch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<MemoryScope>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        previous: MemoryItem,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        patch: Option<String>,
        item: MemoryItem,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_session_id: Option<String>,
    },
    Forget {
        scope: MemoryScope,
        title: String,
        previous: MemoryItem,
        reason: String,
        updated_at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_session_id: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceMemoryMeta {
    workspace_id: String,
    root: String,
    updated_at: String,
}

#[derive(Debug, Clone)]
pub struct PatchMemoryRequest {
    pub scope: MemoryScope,
    pub title: String,
    pub kind: Option<MemoryKind>,
    pub summary: Option<String>,
    pub patch: Option<String>,
    pub reason: Option<String>,
    pub source_session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AddMemoryRequest {
    pub scope: MemoryScope,
    pub title: String,
    pub kind: MemoryKind,
    pub summary: String,
    pub content: String,
    pub reason: Option<String>,
    pub source_session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ForgetMemoryRequest {
    pub scope: MemoryScope,
    pub title: String,
    pub reason: String,
    pub source_session_id: Option<String>,
}

impl MemoryStore {
    pub fn new_default() -> Result<Self> {
        let workspace_root =
            std::env::current_dir().context("failed to resolve current workspace directory")?;
        Self::new(
            profiles::active_profile_path(MEMORIES_DIR_NAME)?,
            workspace_root,
        )
    }

    pub fn new(memories_dir: PathBuf, workspace_root: PathBuf) -> Result<Self> {
        let workspace_root = workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.clone());
        let workspace_id = workspace_id_for_root(&workspace_root);
        Ok(Self {
            memories_dir,
            workspace_id,
            workspace_root,
        })
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn active_memory_block(&self) -> Result<DynamicContextBlock> {
        Ok(DynamicContextBlock {
            id: "duckagent://memory/active".to_string(),
            label: "ACTIVE MEMORY".to_string(),
            content: self.render_active_catalog()?,
        })
    }

    pub fn render_active_catalog(&self) -> Result<String> {
        let global = self.active_items_for_scope(MemoryScope::Global)?;
        let workspace = self.active_items_for_scope(MemoryScope::Workspace)?;
        Ok(render_prompt_template(
            ACTIVE_MEMORY_CATALOG_TEMPLATE,
            &[
                ("workspace_root", self.workspace_root.display().to_string()),
                ("global_items", render_catalog_items(&global)),
                ("workspace_items", render_catalog_items(&workspace)),
            ],
        ))
    }

    pub fn get_memory(&self, scope: MemoryScope, title: &str) -> Result<Option<MemoryItem>> {
        let title = normalize_required("title", title)?;
        let state = self.replay_scope(scope)?;
        Ok(state.get(&title).cloned())
    }

    pub fn add_memory(&self, request: AddMemoryRequest) -> Result<MemoryItem> {
        let title = normalize_required("title", &request.title)?;
        let summary = normalize_required("summary", &request.summary)?;
        let content = normalize_required("content", &request.content)?;
        if let Some(existing) = self.get_memory(request.scope.clone(), &title)? {
            if existing.status == MemoryStatus::Active {
                bail!("active memory already exists for scope/title; use patch_memory: {title}");
            }
        }
        let now = now_rfc3339();
        let item = MemoryItem {
            scope: request.scope,
            title,
            kind: request.kind,
            summary,
            content,
            updated_at: now,
            status: MemoryStatus::Active,
        };
        self.append_event(
            &item.scope,
            MemoryEvent::Add {
                item: item.clone(),
                reason: normalize_optional(request.reason),
                source_session_id: normalize_optional(request.source_session_id),
            },
        )?;
        Ok(item)
    }

    pub fn patch_memory(&self, request: PatchMemoryRequest) -> Result<MemoryItem> {
        let title = normalize_required("title", &request.title)?;
        let previous = self
            .get_memory(request.scope.clone(), &title)?
            .filter(|item| item.status == MemoryStatus::Active)
            .with_context(|| format!("active memory not found for scope/title: {title}"))?;

        let kind = request.kind.unwrap_or_else(|| previous.kind.clone());
        let summary =
            normalize_optional(request.summary).unwrap_or_else(|| previous.summary.clone());
        let patch = normalize_optional_patch(request.patch);
        let content = match patch.as_deref() {
            Some(patch) => apply_unified_content_patch(&previous.content, patch)?,
            None => previous.content.clone(),
        };
        if kind == previous.kind && summary == previous.summary && content == previous.content {
            bail!("patch_memory made no changes for scope/title: {title}");
        }

        let item = MemoryItem {
            scope: request.scope,
            title,
            kind,
            summary,
            content,
            updated_at: now_rfc3339(),
            status: MemoryStatus::Active,
        };
        self.append_event(
            &item.scope,
            MemoryEvent::Patch {
                scope: Some(item.scope.clone()),
                title: Some(item.title.clone()),
                previous,
                patch,
                item: item.clone(),
                reason: normalize_optional(request.reason),
                source_session_id: normalize_optional(request.source_session_id),
            },
        )?;
        Ok(item)
    }

    pub fn forget_memory(&self, request: ForgetMemoryRequest) -> Result<MemoryItem> {
        let title = normalize_required("title", &request.title)?;
        let reason = normalize_required("reason", &request.reason)?;
        let previous = self
            .get_memory(request.scope.clone(), &title)?
            .filter(|item| item.status == MemoryStatus::Active)
            .with_context(|| format!("active memory not found for scope/title: {title}"))?;
        let updated_at = now_rfc3339();
        self.append_event(
            &request.scope,
            MemoryEvent::Forget {
                scope: request.scope.clone(),
                title: title.clone(),
                previous: previous.clone(),
                reason,
                updated_at: updated_at.clone(),
                source_session_id: normalize_optional(request.source_session_id),
            },
        )?;

        Ok(MemoryItem {
            status: MemoryStatus::Forgotten,
            updated_at,
            ..previous
        })
    }

    fn active_items_for_scope(&self, scope: MemoryScope) -> Result<Vec<MemoryItem>> {
        let mut items = self
            .replay_scope(scope)?
            .into_values()
            .filter(|item| item.status == MemoryStatus::Active)
            .collect::<Vec<_>>();
        items.sort_by(|a, b| {
            a.kind
                .cmp(&b.kind)
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });
        Ok(items)
    }

    fn replay_scope(&self, scope: MemoryScope) -> Result<HashMap<String, MemoryItem>> {
        let path = self.path_for_scope(&scope);
        if !path.exists() {
            return Ok(HashMap::new());
        }

        let file = fs::File::open(&path)
            .with_context(|| format!("failed to open memory file: {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut state = HashMap::<String, MemoryItem>::new();

        for (line_no, line) in reader.lines().enumerate() {
            let line = line.with_context(|| {
                format!(
                    "failed to read memory file line {}: {}",
                    line_no + 1,
                    path.display()
                )
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let parsed: MemoryLine = serde_json::from_str(&line).with_context(|| {
                format!(
                    "failed to parse memory file line {}: {}",
                    line_no + 1,
                    path.display()
                )
            })?;
            apply_event(&mut state, parsed.event);
        }

        Ok(state)
    }

    fn append_event(&self, scope: &MemoryScope, event: MemoryEvent) -> Result<()> {
        let path = self.path_for_scope(scope);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create memory directory: {}", parent.display())
            })?;
        }
        if *scope == MemoryScope::Workspace {
            self.write_workspace_meta()?;
        }
        let mut writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open memory append writer: {}", path.display()))?;
        let line = MemoryLine {
            timestamp: now_rfc3339(),
            event,
        };
        let serialized =
            serde_json::to_string(&line).context("failed to serialize memory event")?;
        writer
            .write_all(serialized.as_bytes())
            .context("failed to write memory event")?;
        writer
            .write_all(b"\n")
            .context("failed to write memory newline")?;
        Ok(())
    }

    fn write_workspace_meta(&self) -> Result<()> {
        let dir = self.workspace_dir();
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create workspace memory dir: {}", dir.display()))?;
        let meta = WorkspaceMemoryMeta {
            workspace_id: self.workspace_id.clone(),
            root: self.workspace_root.display().to_string(),
            updated_at: now_rfc3339(),
        };
        let serialized =
            serde_json::to_string_pretty(&meta).context("failed to serialize workspace meta")?;
        fs::write(dir.join(WORKSPACE_META_FILE_NAME), serialized.as_bytes())
            .context("failed to write workspace memory meta")
    }

    fn path_for_scope(&self, scope: &MemoryScope) -> PathBuf {
        match scope {
            MemoryScope::Global => self.memories_dir.join("global").join(MEMORY_FILE_NAME),
            MemoryScope::Workspace => self.workspace_dir().join(MEMORY_FILE_NAME),
        }
    }

    fn workspace_dir(&self) -> PathBuf {
        self.memories_dir
            .join("workspaces")
            .join(&self.workspace_id)
    }
}

fn apply_event(state: &mut HashMap<String, MemoryItem>, event: MemoryEvent) {
    match event {
        MemoryEvent::Add { item, .. } | MemoryEvent::Patch { item, .. } => {
            state.insert(item.title.clone(), item);
        }
        MemoryEvent::Forget {
            title,
            previous,
            updated_at,
            ..
        } => {
            state.insert(
                title,
                MemoryItem {
                    status: MemoryStatus::Forgotten,
                    updated_at,
                    ..previous
                },
            );
        }
    }
}

fn render_catalog_items(items: &[MemoryItem]) -> String {
    if items.is_empty() {
        return ACTIVE_MEMORY_EMPTY_TEMPLATE.trim_end().to_string();
    }
    items
        .iter()
        .map(|item| {
            render_prompt_template(
                ACTIVE_MEMORY_ITEM_TEMPLATE,
                &[
                    ("title", escape_inline(&item.title)),
                    ("kind", memory_kind_name(&item.kind).to_string()),
                    ("summary", escape_inline(&item.summary)),
                ],
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_prompt_template(template: &str, values: &[(&str, String)]) -> String {
    let mut rendered = template.to_string();
    for (key, value) in values {
        rendered = rendered.replace(&format!("{{{{{key}}}}}"), value);
    }
    rendered.trim_end().to_string()
}

fn memory_kind_name(kind: &MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Fact => "fact",
        MemoryKind::Preference => "preference",
        MemoryKind::Procedure => "procedure",
        MemoryKind::Episode => "episode",
    }
}

fn normalize_required(field: &str, value: &str) -> Result<String> {
    let normalized = value.trim();
    if normalized.is_empty() {
        bail!("{field} must be a non-empty string");
    }
    Ok(normalized.to_string())
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn normalize_optional_patch(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

fn apply_unified_content_patch(original: &str, patch: &str) -> Result<String> {
    if patch.trim().is_empty() {
        bail!("patch_memory.patch must be a non-empty unified diff");
    }

    let original_lines = original.split('\n').collect::<Vec<_>>();
    let patch_lines = patch.lines().collect::<Vec<_>>();
    let mut result = Vec::<String>::new();
    let mut original_index = 0usize;
    let mut patch_index = 0usize;
    let mut saw_hunk = false;

    while patch_index < patch_lines.len() {
        let line = patch_lines[patch_index];
        if is_unified_diff_preamble_line(line) || line.trim().is_empty() {
            patch_index += 1;
            continue;
        }
        if !line.starts_with("@@ ") {
            bail!("patch_memory.patch must be a unified diff containing @@ hunk headers");
        }

        saw_hunk = true;
        let target_index = parse_unified_hunk_old_start(line)?.saturating_sub(1);
        if target_index < original_index {
            bail!("patch_memory.patch hunks overlap or move backwards");
        }
        if target_index > original_lines.len() {
            bail!("patch_memory.patch hunk starts beyond existing memory content");
        }
        for original_line in &original_lines[original_index..target_index] {
            result.push((*original_line).to_string());
        }
        original_index = target_index;
        patch_index += 1;

        while patch_index < patch_lines.len() {
            let hunk_line = patch_lines[patch_index];
            if hunk_line.starts_with("@@ ") {
                break;
            }
            if hunk_line.starts_with("\\ ") {
                patch_index += 1;
                continue;
            }
            let Some(prefix) = hunk_line.chars().next() else {
                bail!("patch_memory.patch contains an empty hunk line");
            };
            let body = &hunk_line[prefix.len_utf8()..];
            match prefix {
                ' ' => {
                    expect_original_line(&original_lines, original_index, body, "context")?;
                    result.push(body.to_string());
                    original_index += 1;
                }
                '-' => {
                    expect_original_line(&original_lines, original_index, body, "deletion")?;
                    original_index += 1;
                }
                '+' => result.push(body.to_string()),
                _ => bail!(
                    "patch_memory.patch contains invalid unified diff hunk line: {hunk_line:?}"
                ),
            }
            patch_index += 1;
        }
    }

    if !saw_hunk {
        bail!("patch_memory.patch must contain at least one unified diff hunk");
    }
    for original_line in &original_lines[original_index..] {
        result.push((*original_line).to_string());
    }
    Ok(result.join("\n"))
}

fn is_unified_diff_preamble_line(line: &str) -> bool {
    line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("diff --git ")
        || line.starts_with("index ")
}

fn parse_unified_hunk_old_start(header: &str) -> Result<usize> {
    let after_old_marker = header
        .strip_prefix("@@ -")
        .with_context(|| format!("invalid unified diff hunk header: {header}"))?;
    let old_range_end = after_old_marker
        .find(' ')
        .with_context(|| format!("invalid unified diff hunk header: {header}"))?;
    let old_range = &after_old_marker[..old_range_end];
    old_range
        .split(',')
        .next()
        .unwrap_or_default()
        .parse::<usize>()
        .with_context(|| format!("invalid unified diff hunk old range: {header}"))
}

fn expect_original_line(
    original_lines: &[&str],
    original_index: usize,
    expected: &str,
    line_kind: &str,
) -> Result<()> {
    let actual = original_lines.get(original_index).with_context(|| {
        format!("patch_memory.patch {line_kind} line extends beyond existing memory content")
    })?;
    if *actual != expected {
        bail!(
            "patch_memory.patch {line_kind} mismatch at content line {}",
            original_index + 1
        );
    }
    Ok(())
}

fn escape_inline(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn workspace_id_for_root(root: &Path) -> String {
    let normalized = root.display().to_string();
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let digest = hasher.finalize();
    format!("cwd_{}", bytes_to_hex_prefix(&digest, 16))
}

fn bytes_to_hex_prefix(bytes: &[u8], len: usize) -> String {
    bytes
        .iter()
        .take(len)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{TempDir, tempdir};

    struct MemoryFixture {
        _dir: TempDir,
        memories_dir: PathBuf,
        workspace_root: PathBuf,
        store: MemoryStore,
    }

    fn fixture() -> Result<MemoryFixture> {
        let dir = tempdir()?;
        let workspace_root = dir.path().join("repo");
        fs::create_dir_all(&workspace_root)?;
        let memories_dir = dir.path().join("memories");
        let store = MemoryStore::new(memories_dir.clone(), workspace_root.clone())?;
        Ok(MemoryFixture {
            _dir: dir,
            memories_dir,
            workspace_root,
            store,
        })
    }

    fn global_memory_file(fixture: &MemoryFixture) -> PathBuf {
        fixture.memories_dir.join("global").join(MEMORY_FILE_NAME)
    }

    fn workspace_memory_file(fixture: &MemoryFixture) -> PathBuf {
        fixture
            .memories_dir
            .join("workspaces")
            .join(&fixture.store.workspace_id)
            .join(MEMORY_FILE_NAME)
    }

    fn workspace_meta_file(fixture: &MemoryFixture) -> PathBuf {
        fixture
            .memories_dir
            .join("workspaces")
            .join(&fixture.store.workspace_id)
            .join(WORKSPACE_META_FILE_NAME)
    }

    fn read_memory_lines(path: &Path) -> Result<Vec<MemoryLine>> {
        let text = fs::read_to_string(path)?;
        text.lines()
            .map(|line| serde_json::from_str(line).context("memory line should parse"))
            .collect()
    }

    #[test]
    fn memory_store_appends_and_replays_active_items() -> Result<()> {
        let fixture = fixture()?;
        let store = &fixture.store;
        let added = store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Global,
            title: "User response language preference".to_string(),
            kind: MemoryKind::Preference,
            summary: "User wants responses in Chinese by default.".to_string(),
            content: "User explicitly said they want responses in Chinese by default.".to_string(),
            reason: Some("explicit user preference".to_string()),
            source_session_id: Some("session".to_string()),
        })?;
        assert_eq!(added.status, MemoryStatus::Active);

        let loaded = store
            .get_memory(MemoryScope::Global, "User response language preference")?
            .expect("memory should exist");
        assert_eq!(
            loaded.summary,
            "User wants responses in Chinese by default."
        );
        assert!(
            store
                .render_active_catalog()?
                .contains("User response language preference")
        );
        Ok(())
    }

    #[test]
    fn memory_store_rejects_legacy_name_description_jsonl() -> Result<()> {
        let fixture = fixture()?;
        let path = global_memory_file(&fixture);
        fs::create_dir_all(path.parent().expect("memory file should have parent"))?;
        fs::write(
            &path,
            concat!(
                r#"{"timestamp":"2026-05-09T00:00:00Z","op":"add","item":{"scope":"global","name":"User preferred name","kind":"preference","description":"User wants to be called n.","content":"User wants to be called n.","updated_at":"2026-05-09T00:00:00Z","status":"active"}}"#,
                "\n"
            ),
        )?;

        let err = fixture
            .store
            .get_memory(MemoryScope::Global, "User preferred name")
            .expect_err("legacy name/description memory JSONL should not replay");

        assert!(format!("{err:#}").contains("failed to parse memory file line 1"));

        Ok(())
    }

    #[test]
    fn patch_memory_updates_existing_item_with_unified_diff() -> Result<()> {
        let fixture = fixture()?;
        let store = &fixture.store;
        store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Workspace,
            title: "duckagent memory path".to_string(),
            kind: MemoryKind::Fact,
            summary: "Old path".to_string(),
            content: "Old content".to_string(),
            reason: None,
            source_session_id: None,
        })?;

        let patched = store.patch_memory(PatchMemoryRequest {
            scope: MemoryScope::Workspace,
            title: "duckagent memory path".to_string(),
            kind: None,
            summary: Some("Memory uses memories.jsonl.".to_string()),
            patch: Some(
                "--- memory\n+++ memory\n@@ -1 +1 @@\n-Old content\n+Memory filename is fixed to memories.jsonl."
                    .to_string(),
            ),
            reason: Some("user decision".to_string()),
            source_session_id: None,
        })?;

        assert_eq!(patched.summary, "Memory uses memories.jsonl.");
        assert_eq!(
            patched.content,
            "Memory filename is fixed to memories.jsonl."
        );
        assert_eq!(patched.kind, MemoryKind::Fact);

        let lines = read_memory_lines(&workspace_memory_file(&fixture))?;
        match &lines[1].event {
            MemoryEvent::Patch {
                scope,
                title,
                previous,
                patch,
                item,
                ..
            } => {
                assert_eq!(scope.as_ref(), Some(&MemoryScope::Workspace));
                assert_eq!(title.as_deref(), Some("duckagent memory path"));
                assert_eq!(previous.content, "Old content");
                assert_eq!(item.content, "Memory filename is fixed to memories.jsonl.");
                assert_eq!(
                    patch.as_deref(),
                    Some(
                        "--- memory\n+++ memory\n@@ -1 +1 @@\n-Old content\n+Memory filename is fixed to memories.jsonl."
                    )
                );
            }
            other => panic!("expected patch event, got {other:?}"),
        }

        let jsonl = fs::read_to_string(workspace_memory_file(&fixture))?;
        let patch_line = jsonl.lines().nth(1).expect("patch line should exist");
        let scope_pos = patch_line
            .find(r#""scope":"workspace""#)
            .expect("patch event should expose top-level scope");
        let title_pos = patch_line
            .find(r#""title":"duckagent memory path""#)
            .expect("patch event should expose top-level title");
        let previous_pos = patch_line
            .find(r#""previous":"#)
            .expect("patch event should include previous before patch");
        let patch_pos = patch_line
            .find(r#""patch":"#)
            .expect("patch event should include patch");
        let item_pos = patch_line
            .find(r#""item":"#)
            .expect("patch event should include item after patch");
        assert!(scope_pos < title_pos);
        assert!(title_pos < previous_pos);
        assert!(previous_pos < patch_pos);
        assert!(patch_pos < item_pos);
        Ok(())
    }

    #[test]
    fn patch_memory_rejects_unmatched_diff_without_appending_jsonl() -> Result<()> {
        let fixture = fixture()?;
        let store = &fixture.store;
        store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Global,
            title: "User preferred name".to_string(),
            kind: MemoryKind::Preference,
            summary: "User wants to be called x.".to_string(),
            content: "User wants to be called x.".to_string(),
            reason: None,
            source_session_id: None,
        })?;

        let err = store
            .patch_memory(PatchMemoryRequest {
                scope: MemoryScope::Global,
                title: "User preferred name".to_string(),
                kind: None,
                summary: Some("User wants to be called n instead of x.".to_string()),
                patch: Some(
                    "--- memory\n+++ memory\n@@ -1 +1 @@\n-User wants to be called y.\n+User wants to be called n."
                        .to_string(),
                ),
                reason: Some("user correction".to_string()),
                source_session_id: None,
            })
            .expect_err("mismatched patch should fail");

        assert!(err.to_string().contains("deletion mismatch"));
        assert_eq!(read_memory_lines(&global_memory_file(&fixture))?.len(), 1);
        let loaded = store
            .get_memory(MemoryScope::Global, "User preferred name")?
            .expect("original memory should remain active");
        assert_eq!(loaded.summary, "User wants to be called x.");
        assert_eq!(loaded.content, "User wants to be called x.");

        Ok(())
    }

    #[test]
    fn forgotten_items_leave_active_catalog() -> Result<()> {
        let fixture = fixture()?;
        let store = &fixture.store;
        store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Global,
            title: "Temporary preference".to_string(),
            kind: MemoryKind::Preference,
            summary: "Should be forgotten".to_string(),
            content: "Should be forgotten".to_string(),
            reason: None,
            source_session_id: None,
        })?;
        let forgotten = store.forget_memory(ForgetMemoryRequest {
            scope: MemoryScope::Global,
            title: "Temporary preference".to_string(),
            reason: "user asked to forget".to_string(),
            source_session_id: None,
        })?;

        assert_eq!(forgotten.status, MemoryStatus::Forgotten);
        assert!(
            !store
                .render_active_catalog()?
                .contains("Temporary preference")
        );
        Ok(())
    }

    #[test]
    fn workspace_memory_writes_workspace_file_and_meta() -> Result<()> {
        let fixture = fixture()?;
        let store = &fixture.store;

        store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Workspace,
            title: "Project convention".to_string(),
            kind: MemoryKind::Procedure,
            summary: "Workspace memory is written under the current cwd-derived directory."
                .to_string(),
            content: "Workspace memory uses the cwd hash as its internal storage directory."
                .to_string(),
            reason: Some("workspace decision".to_string()),
            source_session_id: Some("session_workspace".to_string()),
        })?;

        assert!(!global_memory_file(&fixture).exists());
        assert!(workspace_memory_file(&fixture).exists());
        let meta: WorkspaceMemoryMeta =
            serde_json::from_slice(&fs::read(workspace_meta_file(&fixture))?)?;
        assert_eq!(meta.workspace_id, store.workspace_id);
        assert_eq!(
            meta.root,
            fixture.workspace_root.canonicalize()?.display().to_string()
        );

        let lines = read_memory_lines(&workspace_memory_file(&fixture))?;
        assert_eq!(lines.len(), 1);
        match &lines[0].event {
            MemoryEvent::Add {
                item,
                reason,
                source_session_id,
            } => {
                assert_eq!(item.title, "Project convention");
                assert_eq!(reason.as_deref(), Some("workspace decision"));
                assert_eq!(source_session_id.as_deref(), Some("session_workspace"));
            }
            other => panic!("expected add event, got {other:?}"),
        }

        Ok(())
    }

    #[test]
    fn replay_applies_append_only_add_patch_and_forget_events() -> Result<()> {
        let fixture = fixture()?;
        let store = &fixture.store;

        store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Global,
            title: "Response language".to_string(),
            kind: MemoryKind::Preference,
            summary: "Old description".to_string(),
            content: "Old content".to_string(),
            reason: Some("initial".to_string()),
            source_session_id: Some("s1".to_string()),
        })?;
        store.patch_memory(PatchMemoryRequest {
            scope: MemoryScope::Global,
            title: "Response language".to_string(),
            kind: Some(MemoryKind::Fact),
            summary: Some("New description".to_string()),
            patch: Some(
                "--- memory\n+++ memory\n@@ -1 +1 @@\n-Old content\n+New content".to_string(),
            ),
            reason: Some("corrected".to_string()),
            source_session_id: Some("s2".to_string()),
        })?;

        let reopened =
            MemoryStore::new(fixture.memories_dir.clone(), fixture.workspace_root.clone())?;
        let active = reopened
            .get_memory(MemoryScope::Global, "Response language")?
            .expect("patched memory should replay");
        assert_eq!(active.kind, MemoryKind::Fact);
        assert_eq!(active.summary, "New description");
        assert_eq!(active.content, "New content");
        assert_eq!(active.status, MemoryStatus::Active);

        reopened.forget_memory(ForgetMemoryRequest {
            scope: MemoryScope::Global,
            title: "Response language".to_string(),
            reason: "user correction".to_string(),
            source_session_id: Some("s3".to_string()),
        })?;

        let reopened_again =
            MemoryStore::new(fixture.memories_dir.clone(), fixture.workspace_root.clone())?;
        let forgotten = reopened_again
            .get_memory(MemoryScope::Global, "Response language")?
            .expect("forgotten tombstone should replay");
        assert_eq!(forgotten.status, MemoryStatus::Forgotten);
        assert!(
            !reopened_again
                .render_active_catalog()?
                .contains("Response language")
        );

        let lines = read_memory_lines(&global_memory_file(&fixture))?;
        assert_eq!(lines.len(), 3);
        assert!(matches!(lines[0].event, MemoryEvent::Add { .. }));
        assert!(matches!(lines[1].event, MemoryEvent::Patch { .. }));
        assert!(matches!(lines[2].event, MemoryEvent::Forget { .. }));

        Ok(())
    }

    #[test]
    fn active_catalog_renders_replayed_jsonl_title_summary_without_content() -> Result<()> {
        let fixture = fixture()?;
        let store = &fixture.store;

        store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Global,
            title: "User preferred name".to_string(),
            kind: MemoryKind::Preference,
            summary: "User wants to be called x.".to_string(),
            content: "User explicitly said future conversations should call them x. This is full content and should not enter the catalog."
                .to_string(),
            reason: Some("initial preference".to_string()),
            source_session_id: Some("s1".to_string()),
        })?;
        store.patch_memory(PatchMemoryRequest {
            scope: MemoryScope::Global,
            title: "User preferred name".to_string(),
            kind: None,
            summary: Some("User wants to be called n instead of x.".to_string()),
            patch: Some(
                "--- memory\n+++ memory\n@@ -1 +1 @@\n-User explicitly said future conversations should call them x. This is full content and should not enter the catalog.\n+User changed the preferred name from x to n. Call the user n by default unless they change it again."
                    .to_string(),
            ),
            reason: Some("user corrected preferred name".to_string()),
            source_session_id: Some("s2".to_string()),
        })?;

        let reopened =
            MemoryStore::new(fixture.memories_dir.clone(), fixture.workspace_root.clone())?;
        let catalog = reopened.render_active_catalog()?;

        assert!(catalog.contains(r#"title="User preferred name""#));
        assert!(catalog.contains(r#"summary="User wants to be called n instead of x.""#));
        assert!(!catalog.contains("User changed the preferred name from x to n"));
        assert!(!catalog.contains("full content"));

        Ok(())
    }

    #[test]
    fn add_memory_rejects_duplicate_active_title() -> Result<()> {
        let fixture = fixture()?;
        let store = &fixture.store;
        store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Global,
            title: "Duplicate item".to_string(),
            kind: MemoryKind::Fact,
            summary: "Description".to_string(),
            content: "Content".to_string(),
            reason: None,
            source_session_id: None,
        })?;

        let err = store
            .add_memory(AddMemoryRequest {
                scope: MemoryScope::Global,
                title: "Duplicate item".to_string(),
                kind: MemoryKind::Fact,
                summary: "New description".to_string(),
                content: "New content".to_string(),
                reason: None,
                source_session_id: None,
            })
            .expect_err("duplicate active title should fail");

        assert!(err.to_string().contains("use patch_memory"));
        assert_eq!(read_memory_lines(&global_memory_file(&fixture))?.len(), 1);
        Ok(())
    }

    #[test]
    fn patch_memory_rejects_missing_noop_and_trims_updates() -> Result<()> {
        let fixture = fixture()?;
        let store = &fixture.store;

        let missing = store
            .patch_memory(PatchMemoryRequest {
                scope: MemoryScope::Global,
                title: "Missing item".to_string(),
                kind: None,
                summary: Some("Description".to_string()),
                patch: None,
                reason: None,
                source_session_id: None,
            })
            .expect_err("missing active memory should fail");
        assert!(missing.to_string().contains("active memory not found"));

        store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Global,
            title: "Updatable item".to_string(),
            kind: MemoryKind::Fact,
            summary: "Old description".to_string(),
            content: "Old content".to_string(),
            reason: None,
            source_session_id: None,
        })?;

        let noop = store
            .patch_memory(PatchMemoryRequest {
                scope: MemoryScope::Global,
                title: "Updatable item".to_string(),
                kind: None,
                summary: Some("Old description".to_string()),
                patch: None,
                reason: None,
                source_session_id: None,
            })
            .expect_err("no-op patch should fail");
        assert!(noop.to_string().contains("made no changes"));

        let patched = store.patch_memory(PatchMemoryRequest {
            scope: MemoryScope::Global,
            title: " Updatable item ".to_string(),
            kind: None,
            summary: Some("  New description  ".to_string()),
            patch: Some(
                "--- memory\n+++ memory\n@@ -1 +1 @@\n-Old content\n+New content".to_string(),
            ),
            reason: Some("  trim test  ".to_string()),
            source_session_id: Some("  s4  ".to_string()),
        })?;
        assert_eq!(patched.title, "Updatable item");
        assert_eq!(patched.summary, "New description");
        assert_eq!(patched.content, "New content");

        let lines = read_memory_lines(&global_memory_file(&fixture))?;
        assert_eq!(lines.len(), 2);
        match &lines[1].event {
            MemoryEvent::Patch {
                reason,
                source_session_id,
                ..
            } => {
                assert_eq!(reason.as_deref(), Some("trim test"));
                assert_eq!(source_session_id.as_deref(), Some("s4"));
            }
            other => panic!("expected patch event, got {other:?}"),
        }

        Ok(())
    }

    #[test]
    fn forget_memory_rejects_missing_empty_reason_and_inactive_items() -> Result<()> {
        let fixture = fixture()?;
        let store = &fixture.store;

        let missing = store
            .forget_memory(ForgetMemoryRequest {
                scope: MemoryScope::Global,
                title: "Missing item".to_string(),
                reason: "user asked".to_string(),
                source_session_id: None,
            })
            .expect_err("missing memory should fail");
        assert!(missing.to_string().contains("active memory not found"));

        store.add_memory(AddMemoryRequest {
            scope: MemoryScope::Global,
            title: "Forgettable item".to_string(),
            kind: MemoryKind::Preference,
            summary: "Description".to_string(),
            content: "Content".to_string(),
            reason: None,
            source_session_id: None,
        })?;

        let empty_reason = store
            .forget_memory(ForgetMemoryRequest {
                scope: MemoryScope::Global,
                title: "Forgettable item".to_string(),
                reason: "   ".to_string(),
                source_session_id: None,
            })
            .expect_err("empty reason should fail");
        assert!(
            empty_reason
                .to_string()
                .contains("reason must be a non-empty string")
        );

        store.forget_memory(ForgetMemoryRequest {
            scope: MemoryScope::Global,
            title: "Forgettable item".to_string(),
            reason: "user asked".to_string(),
            source_session_id: None,
        })?;

        let inactive = store
            .forget_memory(ForgetMemoryRequest {
                scope: MemoryScope::Global,
                title: "Forgettable item".to_string(),
                reason: "again".to_string(),
                source_session_id: None,
            })
            .expect_err("forgotten memory should not be forgotten twice");
        assert!(inactive.to_string().contains("active memory not found"));
        assert_eq!(read_memory_lines(&global_memory_file(&fixture))?.len(), 2);

        Ok(())
    }

    #[test]
    fn active_catalog_sorts_by_kind_title_and_escapes_inline_values() -> Result<()> {
        let fixture = fixture()?;
        let store = &fixture.store;

        for (title, kind, summary) in [
            ("Runbook", MemoryKind::Procedure, "Deployment steps"),
            ("z fact", MemoryKind::Fact, "Later fact"),
            (
                "A \"quoted\" \\ fact",
                MemoryKind::Fact,
                "desc \"quoted\" \\ path",
            ),
        ] {
            store.add_memory(AddMemoryRequest {
                scope: MemoryScope::Global,
                title: title.to_string(),
                kind,
                summary: summary.to_string(),
                content: summary.to_string(),
                reason: None,
                source_session_id: None,
            })?;
        }

        let catalog = store.render_active_catalog()?;
        let a_pos = catalog
            .find("A \\\"quoted\\\" \\\\ fact")
            .expect("escaped A fact");
        let z_pos = catalog.find("z fact").expect("z fact");
        let runbook_pos = catalog.find("Runbook").expect("runbook");
        assert!(a_pos < z_pos);
        assert!(z_pos < runbook_pos);
        assert!(catalog.contains("desc \\\"quoted\\\" \\\\ path"));

        Ok(())
    }
}
