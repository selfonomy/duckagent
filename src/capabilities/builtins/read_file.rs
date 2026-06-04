use super::{format_tool_path_scope_block, resolve_existing_sandbox_path};
use crate::sandbox::config::resolve_sandbox;
use crate::sandbox::permissions::{AccessKind, ensure_path_allowed};
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

const DEFAULT_LIMIT: usize = 500;
const MAX_LIMIT: usize = 2_000;
const MAX_RETURN_CHARS: usize = 100_000;

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    pub path: String,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

pub fn execute(args: Value) -> Result<String> {
    let input: ReadFileArgs =
        serde_json::from_value(args).context("failed to parse read_file args")?;
    let path = resolve_existing_sandbox_path(&input.path).map_err(|error| {
        anyhow::anyhow!(format_tool_path_scope_block(
            "read_file",
            "read",
            &input.path,
            error
        ))
    })?;
    let workspace = std::env::current_dir()?;
    let sandbox = resolve_sandbox()?;
    ensure_path_allowed(&sandbox, AccessKind::Read, &path, &workspace, "read_file")?;
    let metadata =
        fs::metadata(&path).with_context(|| format!("failed to stat file: {}", path.display()))?;
    let size_bytes = metadata.len();
    let file_type = classify_path(&path);
    if file_type != "text" {
        return Ok(format_non_text_result(&path, file_type, size_bytes));
    }

    let bytes =
        fs::read(&path).with_context(|| format!("failed to read file: {}", path.display()))?;
    if is_probably_binary(&bytes) {
        return Ok(format_non_text_result(&path, "binary", size_bytes));
    }
    let sha256 = sha256_hex(&bytes);
    let text = String::from_utf8(bytes)
        .with_context(|| format!("file is not valid UTF-8 text: {}", path.display()))?;
    format_text_result(&path, size_bytes, &sha256, &text, input.offset, input.limit)
}

fn format_text_result(
    path: &Path,
    size_bytes: u64,
    sha256: &str,
    text: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<String> {
    let offset = offset.unwrap_or(1);
    if offset == 0 {
        bail!("read_file.offset must be >= 1");
    }
    let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    if limit == 0 {
        bail!("read_file.limit must be >= 1");
    }
    let lines = text.lines().collect::<Vec<_>>();
    let total_lines = lines.len();
    let total_chars = text.chars().count();
    let start = offset.saturating_sub(1).min(total_lines);
    let end = start.saturating_add(limit).min(total_lines);
    let returned = &lines[start..end];
    let truncated = end < total_lines;
    let next_offset = if truncated { Some(end + 1) } else { None };
    let mut content = String::new();
    for (idx, line) in returned.iter().enumerate() {
        let line_no = start + idx + 1;
        content.push_str(&format!("{line_no:>6}|{line}\n"));
        if content.chars().count() > MAX_RETURN_CHARS {
            bail!("read_file output is too large; use a smaller limit or later offset");
        }
    }

    let mut out = vec![
        format!("<path>{}</path>", path.display()),
        format!("<sha256>{sha256}</sha256>"),
        "<type>text</type>".to_string(),
        format!("<size_bytes>{size_bytes}</size_bytes>"),
        format!("<total_chars>{total_chars}</total_chars>"),
        format!("<total_lines>{total_lines}</total_lines>"),
        format!("<offset>{offset}</offset>"),
        format!("<limit>{limit}</limit>"),
        format!("<returned_lines>{}</returned_lines>", returned.len()),
        format!("<truncated>{truncated}</truncated>"),
    ];
    if let Some(next_offset) = next_offset {
        out.push(format!("<next_offset>{next_offset}</next_offset>"));
        out.push(format!(
            "<hint>Use offset={next_offset} to continue reading.</hint>"
        ));
    }
    out.push("<content>".to_string());
    out.push(content.trim_end().to_string());
    out.push("</content>".to_string());
    Ok(out.join("\n"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn format_non_text_result(path: &Path, file_type: &str, size_bytes: u64) -> String {
    let error = match file_type {
        "image" => "Image file detected. Use vision_analyze with this path.",
        "pdf" => "PDF file detected. read_file only reads text files.",
        _ => "Binary file - cannot display as text.",
    };
    format!(
        "<path>{}</path>\n<type>{file_type}</type>\n<size_bytes>{size_bytes}</size_bytes>\n<readable_as_text>false</readable_as_text>\n<error>{error}</error>",
        path.display()
    )
}

fn classify_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "tif" | "svg" => "image",
        "pdf" => "pdf",
        _ => "text",
    }
}

fn is_probably_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|byte| *byte == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use uuid::Uuid;

    #[test]
    fn reads_text_with_line_numbers_and_next_offset() -> Result<()> {
        let path = test_workspace_file("sample.txt")?;
        fs::write(&path, "a\nb\nc\n")?;
        let output = execute(serde_json::json!({
            "path": path.to_string_lossy(),
            "offset": 2,
            "limit": 1
        }))?;
        assert!(output.contains("<sha256>"));
        assert!(output.contains("<total_lines>3</total_lines>"));
        assert!(output.contains("<next_offset>3</next_offset>"));
        assert!(output.contains("     2|b"));
        Ok(())
    }

    #[test]
    fn binary_file_returns_metadata() -> Result<()> {
        let path = test_workspace_file("bin.dat")?;
        let mut file = fs::File::create(&path)?;
        file.write_all(&[0, 1, 2])?;
        let output = execute(serde_json::json!({"path": path.to_string_lossy()}))?;
        assert!(output.contains("<type>binary</type>"));
        assert!(output.contains("<size_bytes>3</size_bytes>"));
        Ok(())
    }

    fn test_workspace_file(name: &str) -> Result<std::path::PathBuf> {
        let dir = std::env::current_dir()?
            .join("target")
            .join("duckagent-read-file-tests")
            .join(Uuid::now_v7().to_string());
        fs::create_dir_all(&dir)?;
        Ok(dir.join(name))
    }
}
