use crate::profiles::{self, ProfileInfo};
use crate::setup::{
    PROFILE_MANAGER_FLOW, PickerItem, SetupAction, is_runtime_setup_cancelled,
    prompt_confirm_with_flow, prompt_text_with_flow, run_picker_with_flow,
};
use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::redirect::Policy;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

const ADD_PROFILE_INDEX: usize = 0;
const AVATAR_MAX_BYTES: u64 = 20 * 1024 * 1024;
const SUPPORTED_AVATAR_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif"];

struct NewProfile {
    name: String,
    avatar: Option<AvatarImport>,
    soul: String,
    user: String,
}

struct AvatarImport {
    extension: &'static str,
    bytes: Vec<u8>,
}

pub(crate) fn run_profile_manager() -> Result<()> {
    let picker_profiles = picker_profile_order(profiles::list_profiles()?);
    let items = profile_picker_items(&picker_profiles);
    let action = match run_picker_with_flow(
        PROFILE_MANAGER_FLOW,
        "Profiles",
        "Enter switches the active profile or starts Add Profile. Type to filter. Esc goes back.",
        &items,
        true,
    ) {
        Ok(action) => action,
        Err(error) if is_runtime_setup_cancelled(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    let SetupAction::Submit(index) = action else {
        return Ok(());
    };
    if index == ADD_PROFILE_INDEX {
        return run_add_profile_wizard();
    }
    let selected = picker_profiles
        .get(index - 1)
        .ok_or_else(|| anyhow!("invalid profile selection"))?;
    profiles::set_active_profile_name(&selected.name)?;
    println!("Using profile `{}`", selected.name);
    Ok(())
}

fn run_add_profile_wizard() -> Result<()> {
    let Some(name) = prompt_new_profile_name()? else {
        return Ok(());
    };
    let Some(avatar) = prompt_avatar_import()? else {
        return Ok(());
    };
    let Some(soul) = prompt_optional_line(
        "SOUL.md",
        "Optional one-line persona, tone, or boundaries. You can edit SOUL.md later.",
        "Helpful, concise, careful.",
    )?
    else {
        return Ok(());
    };
    let Some(user) = prompt_optional_line(
        "USER.md",
        "Optional one-line user background or preferences. You can edit USER.md later.",
        "Prefers direct technical answers.",
    )?
    else {
        return Ok(());
    };

    let path = create_profile(NewProfile {
        name: name.clone(),
        avatar,
        soul,
        user,
    })?;
    profiles::set_active_profile_name(&name)?;
    println!("Created and using profile `{name}` at {}", path.display());
    Ok(())
}

fn prompt_new_profile_name() -> Result<Option<String>> {
    loop {
        let action = match prompt_text_with_flow(
            PROFILE_MANAGER_FLOW,
            "Profile name",
            "Required. No path separators or control characters.",
            "work",
            None,
            true,
            true,
            false,
        ) {
            Ok(action) => action,
            Err(error) if is_runtime_setup_cancelled(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let SetupAction::Submit(name) = action else {
            return Ok(None);
        };
        match validate_new_profile_name(&name) {
            Ok(name) => return Ok(Some(name.to_string())),
            Err(error) => show_profile_error("Cannot add profile", &error.to_string())?,
        }
    }
}

fn prompt_avatar_import() -> Result<Option<Option<AvatarImport>>> {
    loop {
        let action = match prompt_text_with_flow(
            PROFILE_MANAGER_FLOW,
            "Avatar image",
            "Optional local path or http(s) URL. Leave empty to use the bundled default avatar.",
            "~/avatar.png",
            None,
            false,
            true,
            false,
        ) {
            Ok(action) => action,
            Err(error) if is_runtime_setup_cancelled(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let SetupAction::Submit(source) = action else {
            return Ok(None);
        };
        if source.trim().is_empty() {
            return Ok(Some(None));
        }
        match import_avatar(&source) {
            Ok(avatar) => return Ok(Some(Some(avatar))),
            Err(error) => show_profile_error("Cannot import avatar", &error.to_string())?,
        }
    }
}

fn prompt_optional_line(title: &str, subtitle: &str, placeholder: &str) -> Result<Option<String>> {
    let action = match prompt_text_with_flow(
        PROFILE_MANAGER_FLOW,
        title,
        subtitle,
        placeholder,
        None,
        false,
        true,
        false,
    ) {
        Ok(action) => action,
        Err(error) if is_runtime_setup_cancelled(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    match action {
        SetupAction::Submit(line) => Ok(Some(line)),
        SetupAction::Back => Ok(None),
    }
}

fn create_profile(profile: NewProfile) -> Result<PathBuf> {
    let name = validate_new_profile_name(&profile.name)?;
    let dir = profiles::ensure_profile(name)?;
    apply_new_profile_inputs(&dir, profile)?;
    Ok(dir)
}

fn apply_new_profile_inputs(dir: &Path, profile: NewProfile) -> Result<()> {
    if let Some(avatar) = profile.avatar {
        write_avatar(dir, avatar)?;
    }
    write_optional_markdown_line(&dir.join("SOUL.md"), &profile.soul)?;
    write_optional_markdown_line(&dir.join("USER.md"), &profile.user)
}

fn validate_new_profile_name(name: &str) -> Result<&str> {
    let name = profiles::validate_profile_name(name)?;
    let dir = profiles::profile_dir_for_name(name)?;
    if dir.exists() {
        bail!("profile `{name}` already exists");
    }
    Ok(name)
}

fn picker_profile_order(mut profiles: Vec<ProfileInfo>) -> Vec<ProfileInfo> {
    profiles.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| a.name.cmp(&b.name)));
    profiles
}

fn profile_picker_items(profiles: &[ProfileInfo]) -> Vec<PickerItem> {
    let mut items = Vec::with_capacity(profiles.len() + 1);
    items.push(PickerItem {
        title: "+ Add Profile".to_string(),
        detail: "Create a profile with optional avatar, SOUL.md, and USER.md".to_string(),
        model_columns: None,
    });
    items.extend(profiles.iter().map(|profile| PickerItem {
        title: format_profile_title(profile),
        detail: profile.path.display().to_string(),
        model_columns: None,
    }));
    items
}

fn format_profile_title(profile: &ProfileInfo) -> String {
    let marker = if profile.active { "*" } else { " " };
    format!("{marker} {}", profile.name)
}

fn import_avatar(source: &str) -> Result<AvatarImport> {
    if let Ok(url) = Url::parse(source.trim())
        && matches!(url.scheme(), "http" | "https")
    {
        return download_avatar(&url);
    }
    read_local_avatar(source)
}

fn download_avatar(url: &Url) -> Result<AvatarImport> {
    let response = Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(Policy::limited(5))
        .build()
        .context("failed to create avatar download client")?
        .get(url.clone())
        .send()
        .with_context(|| format!("failed to download avatar: {url}"))?
        .error_for_status()
        .with_context(|| format!("avatar URL returned an error status: {url}"))?;

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if let Some(length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        && length > AVATAR_MAX_BYTES
    {
        bail!(
            "avatar is larger than {} MiB",
            AVATAR_MAX_BYTES / 1024 / 1024
        );
    }
    let mut bytes = Vec::new();
    response
        .take(AVATAR_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read avatar response: {url}"))?;
    if bytes.len() as u64 > AVATAR_MAX_BYTES {
        bail!(
            "avatar is larger than {} MiB",
            AVATAR_MAX_BYTES / 1024 / 1024
        );
    }
    let extension = avatar_extension(Some(url.path()), content_type.as_deref(), &bytes)?;
    Ok(AvatarImport { extension, bytes })
}

fn read_local_avatar(source: &str) -> Result<AvatarImport> {
    let path = expand_local_path(source.trim())?;
    let bytes = fs::read(&path)
        .with_context(|| format!("failed to read avatar image: {}", path.display()))?;
    if bytes.len() as u64 > AVATAR_MAX_BYTES {
        bail!(
            "avatar is larger than {} MiB",
            AVATAR_MAX_BYTES / 1024 / 1024
        );
    }
    let extension = avatar_extension(path.to_str(), None, &bytes)?;
    Ok(AvatarImport { extension, bytes })
}

fn avatar_extension(
    source_hint: Option<&str>,
    content_type: Option<&str>,
    bytes: &[u8],
) -> Result<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok("png");
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Ok("jpg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Ok("gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Ok("webp");
    }
    if let Some(extension) = extension_from_content_type(content_type) {
        return Ok(extension);
    }
    if let Some(extension) = source_hint.and_then(extension_from_path_hint) {
        return Ok(extension);
    }
    bail!("avatar must be png, jpg, jpeg, webp, or gif")
}

fn extension_from_content_type(content_type: Option<&str>) -> Option<&'static str> {
    let content_type = content_type?.split(';').next()?.trim().to_ascii_lowercase();
    match content_type.as_str() {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

fn extension_from_path_hint(path: &str) -> Option<&'static str> {
    let extension = Path::new(path)
        .extension()?
        .to_str()?
        .trim_start_matches('.')
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some("png"),
        "jpg" => Some("jpg"),
        "jpeg" => Some("jpeg"),
        "webp" => Some("webp"),
        "gif" => Some("gif"),
        _ => None,
    }
}

fn expand_local_path(input: &str) -> Result<PathBuf> {
    if input == "~" {
        return dirs::home_dir().context("failed to resolve home directory");
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| home.join(rest))
            .context("failed to resolve home directory");
    }
    if let Some(rest) = input.strip_prefix("~\\") {
        return dirs::home_dir()
            .map(|home| home.join(rest))
            .context("failed to resolve home directory");
    }
    let path = PathBuf::from(input);
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .context("failed to resolve current directory")
    }
}

fn write_avatar(dir: &Path, avatar: AvatarImport) -> Result<()> {
    for extension in SUPPORTED_AVATAR_EXTENSIONS {
        let path = dir.join(format!("avatar.{extension}"));
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to replace avatar image: {}", path.display()))?;
        }
    }
    let path = dir.join(format!("avatar.{}", avatar.extension));
    fs::write(&path, avatar.bytes)
        .with_context(|| format!("failed to write avatar image: {}", path.display()))
}

fn write_optional_markdown_line(path: &Path, line: &str) -> Result<()> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }
    fs::write(path, format!("{line}\n"))
        .with_context(|| format!("failed to write profile file: {}", path.display()))
}

fn show_profile_error(title: &str, message: &str) -> Result<()> {
    let lines = vec![message.to_string(), "Press Enter to retry.".to_string()];
    let _ = prompt_confirm_with_flow(PROFILE_MANAGER_FLOW, title, &lines, true)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn active_profile_is_first_in_picker_order() {
        let profiles = picker_profile_order(vec![
            profile("work", false),
            profile("default", true),
            profile("demo", false),
        ]);
        assert_eq!(profiles[0].name, "default");
        assert!(profiles[0].active);
        assert_eq!(profiles[1].name, "demo");
        assert_eq!(profiles[2].name, "work");
    }

    #[test]
    fn picker_items_mark_current_profile() {
        let items = profile_picker_items(&[profile("default", true), profile("work", false)]);
        assert_eq!(items[0].title, "+ Add Profile");
        assert_eq!(items[1].title, "* default");
        assert_eq!(items[2].title, "  work");
        assert!(items[1].detail.ends_with("default"));
    }

    #[test]
    fn detects_avatar_extension_from_magic_bytes_first() -> Result<()> {
        assert_eq!(
            avatar_extension(Some("avatar.gif"), Some("image/gif"), b"\x89PNG\r\n\x1a\nx")?,
            "png"
        );
        assert_eq!(
            avatar_extension(Some("avatar.png"), None, b"\xff\xd8\xff\xdb")?,
            "jpg"
        );
        assert_eq!(avatar_extension(None, None, b"GIF89a...")?, "gif");
        assert_eq!(avatar_extension(None, None, b"RIFFxxxxWEBPvp8 ")?, "webp");
        Ok(())
    }

    #[test]
    fn detects_avatar_extension_from_content_type_or_hint() -> Result<()> {
        assert_eq!(
            avatar_extension(None, Some("image/jpeg; charset=binary"), b"not-magic")?,
            "jpg"
        );
        assert_eq!(
            avatar_extension(Some("/tmp/avatar.jpeg"), None, b"not-magic")?,
            "jpeg"
        );
        assert!(avatar_extension(Some("/tmp/avatar.bmp"), None, b"not-magic").is_err());
        Ok(())
    }

    #[test]
    fn add_profile_inputs_override_default_templates() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("avatar.png"), b"default-avatar")?;
        fs::write(dir.path().join("SOUL.md"), "default soul\n")?;
        fs::write(dir.path().join("USER.md"), "")?;

        apply_new_profile_inputs(
            dir.path(),
            NewProfile {
                name: "custom".to_string(),
                avatar: Some(AvatarImport {
                    extension: "jpg",
                    bytes: b"custom-avatar".to_vec(),
                }),
                soul: "custom soul".to_string(),
                user: "custom user".to_string(),
            },
        )?;

        assert!(!dir.path().join("avatar.png").exists());
        assert_eq!(fs::read(dir.path().join("avatar.jpg"))?, b"custom-avatar");
        assert_eq!(
            fs::read_to_string(dir.path().join("SOUL.md"))?,
            "custom soul\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("USER.md"))?,
            "custom user\n"
        );
        Ok(())
    }

    fn profile(name: &str, active: bool) -> ProfileInfo {
        ProfileInfo {
            name: name.to_string(),
            path: PathBuf::from(format!("/tmp/{name}")),
            active,
        }
    }
}
