use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const DEFAULT_PROFILE_NAME: &str = "default";
const ACTIVE_PROFILE_FIELD: &str = "active_profile";
const CONFIG_FILE_NAME: &str = "config.json";
const PROFILES_DIR_NAME: &str = "profiles";
const SOUL_FILE_NAME: &str = "SOUL.md";
const USER_FILE_NAME: &str = "USER.md";
const DEFAULT_AVATAR_FILE_NAME: &str = "avatar.png";
const AVATAR_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif"];
const DEFAULT_SOUL: &str = include_str!("default/SOUL.md");
const DEFAULT_USER: &str = include_str!("default/USER.md");
const DEFAULT_AVATAR_PNG: &[u8] = include_bytes!("default/avatar.png");

static CLI_PROFILE_OVERRIDE: Mutex<Option<String>> = Mutex::new(None);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileInfo {
    pub name: String,
    pub path: PathBuf,
    pub active: bool,
}

pub fn set_cli_profile_override(profile: Option<String>) -> Result<()> {
    let profile = profile
        .map(|value| validate_profile_name(&value).map(str::to_string))
        .transpose()?;
    *CLI_PROFILE_OVERRIDE
        .lock()
        .expect("profile override mutex poisoned") = profile;
    Ok(())
}

pub fn active_profile_name() -> Result<String> {
    if let Some(profile) = CLI_PROFILE_OVERRIDE
        .lock()
        .expect("profile override mutex poisoned")
        .clone()
    {
        return Ok(profile);
    }
    let raw = read_json_object_if_exists(&root_config_path()?)?;
    let name = raw
        .as_ref()
        .and_then(|object| object.get(ACTIVE_PROFILE_FIELD))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROFILE_NAME);
    Ok(validate_profile_name(name)?.to_string())
}

pub fn set_active_profile_name(name: &str) -> Result<()> {
    let name = validate_profile_name(name)?.to_string();
    ensure_profile(&name)?;
    let path = root_config_path()?;
    let mut raw = read_json_object_if_exists(&path)?.unwrap_or_default();
    raw.insert(ACTIVE_PROFILE_FIELD.to_string(), Value::String(name));
    write_json_object(&path, &raw)
}

pub fn ensure_profile(name: &str) -> Result<PathBuf> {
    let name = validate_profile_name(name)?;
    let dir = profile_dir_for_name(name)?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create profile directory: {}", dir.display()))?;
    for dir_name in [
        "sessions", "memories", "skills", "cache", "audit", "gateway", "cron",
    ] {
        let path = dir.join(dir_name);
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create profile directory: {}", path.display()))?;
    }
    let config_path = dir.join(CONFIG_FILE_NAME);
    if !config_path.exists() {
        write_json_object(&config_path, &Map::new())?;
    }
    ensure_default_profile_files(&dir)?;
    Ok(dir)
}

pub fn list_profiles() -> Result<Vec<ProfileInfo>> {
    let active = active_profile_name()?;
    let root = profiles_root_dir()?;
    let mut profiles = Vec::new();
    if root.exists() {
        for entry in fs::read_dir(&root)
            .with_context(|| format!("failed to read profiles directory: {}", root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if validate_profile_name(&name).is_err() {
                continue;
            }
            profiles.push(ProfileInfo {
                active: name == active,
                name,
                path: entry.path(),
            });
        }
    }
    if !profiles.iter().any(|profile| profile.name == active) {
        profiles.push(ProfileInfo {
            name: active.clone(),
            path: profile_dir_for_name(&active)?,
            active: true,
        });
    }
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(profiles)
}

pub fn duckagent_home_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|home| home.join(".duckagent"))
        .context("failed to resolve home directory")
}

pub fn root_config_path() -> Result<PathBuf> {
    Ok(duckagent_home_dir()?.join(CONFIG_FILE_NAME))
}

pub fn profiles_root_dir() -> Result<PathBuf> {
    Ok(duckagent_home_dir()?.join(PROFILES_DIR_NAME))
}

pub fn active_profile_dir() -> Result<PathBuf> {
    ensure_profile(&active_profile_name()?)
}

pub fn profile_dir_for_name(name: &str) -> Result<PathBuf> {
    let name = validate_profile_name(name)?;
    Ok(profiles_root_dir()?.join(name))
}

pub fn active_profile_config_path() -> Result<PathBuf> {
    Ok(active_profile_dir()?.join(CONFIG_FILE_NAME))
}

pub fn active_profile_path(relative: impl AsRef<Path>) -> Result<PathBuf> {
    Ok(active_profile_dir()?.join(relative))
}

pub fn load_active_profile_config() -> Result<Map<String, Value>> {
    Ok(read_json_object_if_exists(&active_profile_config_path()?)?.unwrap_or_default())
}

pub fn save_active_profile_config(raw: &Map<String, Value>) -> Result<()> {
    ensure_profile(&active_profile_name()?)?;
    let mut profile_raw = raw.clone();
    profile_raw.remove(ACTIVE_PROFILE_FIELD);
    profile_raw.remove("sandbox");
    write_json_object(&active_profile_config_path()?, &profile_raw)
}

pub fn validate_profile_name(name: &str) -> Result<&str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("profile name must be non-empty");
    }
    if matches!(trimmed, "." | "..") {
        bail!("profile name must not be `.` or `..`");
    }
    if trimmed
        .chars()
        .any(|ch| ch == '/' || ch == '\\' || ch.is_control())
    {
        bail!("profile name must not contain path separators or control characters");
    }
    Ok(trimmed)
}

fn read_json_object_if_exists(path: &Path) -> Result<Option<Map<String, Value>>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read JSON file: {}", path.display()));
        }
    };
    if text.trim().is_empty() {
        return Ok(Some(Map::new()));
    }
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse JSON file: {}", path.display()))?;
    match value {
        Value::Object(object) => Ok(Some(object)),
        _ => bail!("JSON file must contain an object: {}", path.display()),
    }
}

fn write_json_object(path: &Path, raw: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory: {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(&Value::Object(raw.clone()))
        .context("failed to serialize JSON file")?;
    fs::write(path, format!("{text}\n"))
        .with_context(|| format!("failed to write JSON file: {}", path.display()))
}

fn ensure_default_profile_files(dir: &Path) -> Result<()> {
    write_text_file_if_missing_or_empty(&dir.join(SOUL_FILE_NAME), DEFAULT_SOUL)?;
    write_text_file_if_missing(&dir.join(USER_FILE_NAME), DEFAULT_USER)?;
    if !profile_has_avatar_file(dir) {
        write_bytes_file_if_missing(&dir.join(DEFAULT_AVATAR_FILE_NAME), DEFAULT_AVATAR_PNG)?;
    }
    Ok(())
}

fn profile_has_avatar_file(dir: &Path) -> bool {
    AVATAR_EXTENSIONS
        .iter()
        .any(|extension| dir.join(format!("avatar.{extension}")).exists())
}

fn write_text_file_if_missing(path: &Path, text: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    fs::write(path, text)
        .with_context(|| format!("failed to create profile file: {}", path.display()))
}

fn write_text_file_if_missing_or_empty(path: &Path, text: &str) -> Result<()> {
    if path.exists() {
        let current = fs::read_to_string(path)
            .with_context(|| format!("failed to read profile file: {}", path.display()))?;
        if !current.trim().is_empty() {
            return Ok(());
        }
    }
    fs::write(path, text)
        .with_context(|| format!("failed to create profile file: {}", path.display()))
}

fn write_bytes_file_if_missing(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    fs::write(path, bytes)
        .with_context(|| format!("failed to create profile file: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn validates_profile_names_without_restricting_unicode() {
        assert!(validate_profile_name("default").is_ok());
        assert!(validate_profile_name("role").is_ok());
        assert!(validate_profile_name("").is_err());
        assert!(validate_profile_name("../x").is_err());
        assert!(validate_profile_name("a/b").is_err());
        assert!(validate_profile_name("a\\b").is_err());
    }

    #[test]
    fn active_profile_config_path_lives_under_profiles() -> Result<()> {
        assert!(profile_dir_for_name("default")?.ends_with("profiles/default"));
        Ok(())
    }

    #[test]
    fn default_profile_files_are_created_from_templates() -> Result<()> {
        let dir = tempdir()?;
        ensure_default_profile_files(dir.path())?;

        let soul = fs::read_to_string(dir.path().join("SOUL.md"))?;
        let user = fs::read_to_string(dir.path().join("USER.md"))?;
        let avatar = fs::read(dir.path().join("avatar.png"))?;

        assert!(soul.contains("cute white duck"));
        assert!(user.trim().is_empty());
        assert_eq!(avatar, DEFAULT_AVATAR_PNG);
        Ok(())
    }

    #[test]
    fn default_profile_files_do_not_overwrite_user_files() -> Result<()> {
        let dir = tempdir()?;
        fs::write(dir.path().join("SOUL.md"), "custom soul\n")?;
        fs::write(dir.path().join("USER.md"), "custom user\n")?;
        fs::write(dir.path().join("avatar.jpg"), b"custom-avatar")?;

        ensure_default_profile_files(dir.path())?;

        assert_eq!(
            fs::read_to_string(dir.path().join("SOUL.md"))?,
            "custom soul\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("USER.md"))?,
            "custom user\n"
        );
        assert_eq!(fs::read(dir.path().join("avatar.jpg"))?, b"custom-avatar");
        assert!(!dir.path().join("avatar.png").exists());
        Ok(())
    }

    #[test]
    fn empty_soul_file_is_initialized_from_default_template() -> Result<()> {
        let dir = tempdir()?;
        fs::write(dir.path().join("SOUL.md"), "\n")?;

        ensure_default_profile_files(dir.path())?;

        let soul = fs::read_to_string(dir.path().join("SOUL.md"))?;
        assert!(soul.contains("cute white duck"));
        Ok(())
    }
}
