use super::{format_tool_path_scope_block, resolve_existing_sandbox_path};
use crate::sandbox::config::resolve_sandbox;
use crate::sandbox::permissions::{AccessKind, ensure_path_allowed};
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct VisionAnalyzeArgs {
    pub path: String,
    pub question: String,
}

#[derive(Debug, Clone)]
pub struct VisionAnalyzeRequest {
    pub path: PathBuf,
    pub question: String,
    pub mime: String,
    pub size_bytes: u64,
}

pub fn prepare_request(args: Value) -> Result<VisionAnalyzeRequest> {
    let input: VisionAnalyzeArgs =
        serde_json::from_value(args).context("failed to parse vision_analyze args")?;
    if input.question.trim().is_empty() {
        bail!("vision_analyze.question must be non-empty");
    }
    let path = resolve_existing_sandbox_path(&input.path).map_err(|error| {
        anyhow::anyhow!(format_tool_path_scope_block(
            "vision_analyze",
            "read",
            &input.path,
            error
        ))
    })?;
    let workspace = std::env::current_dir()?;
    let sandbox = resolve_sandbox()?;
    ensure_path_allowed(
        &sandbox,
        AccessKind::Read,
        &path,
        &workspace,
        "vision_analyze",
    )?;
    let metadata =
        fs::metadata(&path).with_context(|| format!("failed to stat image: {}", path.display()))?;
    let mime = image_mime(&path).unwrap_or("application/octet-stream");
    if !mime.starts_with("image/") {
        bail!(
            "Tool error: vision_analyze supports local image files only.\npath: {}\nmime: {mime}\nsize_bytes: {}",
            path.display(),
            metadata.len()
        );
    }

    Ok(VisionAnalyzeRequest {
        path,
        question: input.question.trim().to_string(),
        mime: mime.to_string(),
        size_bytes: metadata.len(),
    })
}

fn image_mime(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "tif" | "tiff" => Some("image/tiff"),
        "svg" => Some("image/svg+xml"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn vision_prepare_request_does_not_load_base64() -> Result<()> {
        let dir = std::env::current_dir()?
            .join("target")
            .join("duckagent-vision-tests")
            .join(Uuid::now_v7().to_string());
        fs::create_dir_all(&dir)?;
        let path = dir.join("image.png");
        fs::write(&path, [137, 80, 78, 71])?;
        let request = prepare_request(serde_json::json!({
            "path": path.to_string_lossy(),
            "question": "what is this?"
        }))?;
        assert_eq!(request.mime, "image/png");
        assert_eq!(request.size_bytes, 4);
        assert_eq!(request.question, "what is this?");
        Ok(())
    }
}
