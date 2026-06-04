use crate::profiles;
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Component, Path, PathBuf};

const SKILLS_DIR_NAME: &str = "skills";
const SKILL_FILENAME: &str = "SKILL.md";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillIndexEntry {
    pub name: String,
    pub description: String,
    pub skill_root: PathBuf,
    pub skill_md_path: PathBuf,
}

#[derive(Debug, Clone)]
struct LoadedSkill {
    index: SkillIndexEntry,
    frontmatter: Value,
    content: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct LoadSkillArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ReadSkillFileArgs {
    pub name: String,
    pub path: String,
}

pub fn discover_global_skills() -> Result<Vec<SkillIndexEntry>> {
    discover_skills_in(&global_skills_dir()?)
}

pub fn render_skill_index() -> String {
    let Ok(skills) = discover_global_skills() else {
        return "- (skills unavailable: failed to read active profile skills)".to_string();
    };
    if skills.is_empty() {
        return "- (no skills found in active profile skills)".to_string();
    }
    skills
        .into_iter()
        .map(|skill| format!("- `{}`: {}", skill.name, skill.description))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn load_skill(args: Value) -> Result<String> {
    let input: LoadSkillArgs =
        serde_json::from_value(args).context("failed to parse load_skill args")?;
    let skill = load_skill_by_name(&global_skills_dir()?, &input.name)?;
    serde_json::to_string_pretty(&json!({
        "name": skill.index.name,
        "description": skill.index.description,
        "skill_root": skill.index.skill_root.display().to_string(),
        "skill_md_path": skill.index.skill_md_path.display().to_string(),
        "frontmatter": skill.frontmatter,
        "content": skill.content,
    }))
    .context("failed to serialize load_skill result")
}

pub fn read_skill_file(args: Value) -> Result<String> {
    let input: ReadSkillFileArgs =
        serde_json::from_value(args).context("failed to parse read_skill_file args")?;
    let skill = load_skill_by_name(&global_skills_dir()?, &input.name)?;
    let path = resolve_skill_relative_path(&skill.index.skill_root, &input.path)?;
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read skill file: {}", path.display()))?;
    serde_json::to_string_pretty(&json!({
        "name": skill.index.name,
        "skill_root": skill.index.skill_root.display().to_string(),
        "path": path.display().to_string(),
        "relative_path": input.path,
        "content": content,
    }))
    .context("failed to serialize read_skill_file result")
}

pub fn discover_skills_in(root: &Path) -> Result<Vec<SkillIndexEntry>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    if !root.is_dir() {
        bail!("skills path is not a directory: {}", root.display());
    }

    let mut skills = Vec::new();
    for entry in fs::read_dir(root)
        .with_context(|| format!("failed to read skills directory: {}", root.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }
        let skill_root = entry.path();
        let skill_md_path = skill_root.join(SKILL_FILENAME);
        if !skill_md_path.exists() {
            continue;
        }
        if let Ok(skill) = load_skill_from_path(&skill_root, &skill_md_path) {
            skills.push(skill.index);
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

fn global_skills_dir() -> Result<PathBuf> {
    profiles::active_profile_path(SKILLS_DIR_NAME)
}

fn load_skill_by_name(root: &Path, name: &str) -> Result<LoadedSkill> {
    let name = name.trim();
    if name.is_empty() {
        bail!("load_skill.name must be non-empty");
    }
    let skill_root = root.join(name);
    let skill_md_path = skill_root.join(SKILL_FILENAME);
    if !skill_md_path.exists() {
        bail!("skill `{name}` not found in {}", root.display());
    }
    let skill = load_skill_from_path(&skill_root, &skill_md_path)?;
    if skill.index.name != name {
        bail!(
            "skill directory `{}` contains mismatched name `{}`",
            name,
            skill.index.name
        );
    }
    Ok(skill)
}

fn load_skill_from_path(skill_root: &Path, skill_md_path: &Path) -> Result<LoadedSkill> {
    let content = fs::read_to_string(skill_md_path)
        .with_context(|| format!("failed to read skill file: {}", skill_md_path.display()))?;
    let (frontmatter, name, description) = parse_skill_frontmatter(&content)?;
    let skill_root = skill_root
        .canonicalize()
        .with_context(|| format!("failed to resolve skill root: {}", skill_root.display()))?;
    let skill_md_path = skill_md_path
        .canonicalize()
        .with_context(|| format!("failed to resolve skill file: {}", skill_md_path.display()))?;
    Ok(LoadedSkill {
        index: SkillIndexEntry {
            name,
            description,
            skill_root,
            skill_md_path,
        },
        frontmatter,
        content,
    })
}

fn parse_skill_frontmatter(content: &str) -> Result<(Value, String, String)> {
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        bail!("SKILL.md must start with YAML frontmatter");
    }

    let mut object = Map::new();
    let mut found_end = false;
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            found_end = true;
            break;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            object.insert(key.to_string(), Value::String(unquote_yaml_scalar(value)));
        }
    }
    if !found_end {
        bail!("SKILL.md frontmatter is missing closing ---");
    }

    let name = object
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("SKILL.md frontmatter missing name")?
        .to_string();
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("SKILL.md frontmatter missing description")?
        .to_string();

    Ok((Value::Object(object), name, description))
}

fn unquote_yaml_scalar(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

fn resolve_skill_relative_path(skill_root: &Path, relative_path: &str) -> Result<PathBuf> {
    let relative_path = relative_path.trim();
    if relative_path.is_empty() {
        bail!("read_skill_file.path must be non-empty");
    }
    let raw = PathBuf::from(relative_path);
    if raw.is_absolute()
        || raw.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        bail!("read_skill_file.path must be a relative path inside the skill root");
    }
    let path = skill_root.join(raw);
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve skill file: {}", path.display()))?;
    if !canonical.starts_with(skill_root) {
        bail!(
            "skill file path is outside the skill root: {}",
            canonical.display()
        );
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discovers_skills_sorted_by_name() -> Result<()> {
        let dir = tempdir()?;
        write_skill(dir.path(), "zeta", "Z skill")?;
        write_skill(dir.path(), "alpha", "A skill")?;

        let skills = discover_skills_in(dir.path())?;
        let names = skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["alpha", "zeta"]);
        Ok(())
    }

    #[test]
    fn load_skill_returns_paths_and_full_content() -> Result<()> {
        let dir = tempdir()?;
        let skill_md = write_skill(dir.path(), "business-analysis", "Analyze businesses")?;
        let loaded = load_skill_by_name(dir.path(), "business-analysis")?;
        assert_eq!(loaded.index.skill_md_path, skill_md.canonicalize()?);
        assert!(loaded.content.contains("Use this framework."));
        Ok(())
    }

    #[test]
    fn read_skill_file_rejects_parent_escape() -> Result<()> {
        let dir = tempdir()?;
        write_skill(dir.path(), "demo", "Demo")?;
        let skill = load_skill_by_name(dir.path(), "demo")?;
        let err = resolve_skill_relative_path(&skill.index.skill_root, "../secret.md")
            .expect_err("path escape should fail");
        assert!(
            err.to_string()
                .contains("relative path inside the skill root")
        );
        Ok(())
    }

    fn write_skill(root: &Path, name: &str, description: &str) -> Result<PathBuf> {
        let skill_root = root.join(name);
        fs::create_dir_all(skill_root.join("references"))?;
        let skill_md = skill_root.join(SKILL_FILENAME);
        fs::write(
            &skill_md,
            format!("---\nname: {name}\ndescription: {description}\n---\n\nUse this framework.\n"),
        )?;
        fs::write(skill_root.join("references").join("method.md"), "details")?;
        Ok(skill_md)
    }
}
