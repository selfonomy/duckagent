use crate::instructions::DynamicContextBlock;
use crate::profiles;
use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use chrono::Utc;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const AVATAR_PNG_FILE: &str = "avatar.png";
const AVATAR_JSON_FILE: &str = "avatar.json";
const SOUL_FILE: &str = "SOUL.md";
const USER_FILE: &str = "USER.md";
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

pub fn active_profile_context_blocks() -> Result<Vec<DynamicContextBlock>> {
    let profile_name = profiles::active_profile_name()?;
    let profile_dir = profiles::active_profile_dir()?;
    profile_context_blocks_for_dir(&profile_name, &profile_dir)
}

pub fn profile_context_blocks_for_dir(
    profile_name: &str,
    profile_dir: &Path,
) -> Result<Vec<DynamicContextBlock>> {
    let mut blocks = Vec::new();
    if let Some(card) = load_avatar_card(profile_dir)? {
        if let Some(content) = render_avatar_card_context(&card) {
            blocks.push(DynamicContextBlock {
                id: format!("duckagent://profile/{profile_name}/avatar"),
                label: "AVATAR CARD".to_string(),
                content,
            });
        }
    }
    if let Some(content) = read_trimmed(profile_dir.join(SOUL_FILE))? {
        blocks.push(DynamicContextBlock {
            id: format!("duckagent://profile/{profile_name}/SOUL.md"),
            label: "SOUL".to_string(),
            content,
        });
    }
    if let Some(content) = read_trimmed(profile_dir.join(USER_FILE))? {
        blocks.push(DynamicContextBlock {
            id: format!("duckagent://profile/{profile_name}/USER.md"),
            label: "USER".to_string(),
            content,
        });
    }
    Ok(blocks)
}

pub fn load_avatar_card(profile_dir: &Path) -> Result<Option<Value>> {
    let avatar_png = profile_dir.join(AVATAR_PNG_FILE);
    let avatar_json = profile_dir.join(AVATAR_JSON_FILE);
    if avatar_png.exists() {
        if avatar_json_is_current(&avatar_png, &avatar_json)? {
            if let Ok(Some(card)) = read_avatar_json(&avatar_json) {
                return Ok(Some(card));
            }
        }
        let Ok(bytes) = fs::read(&avatar_png) else {
            return Ok(None);
        };
        if let Some(card) = extract_sillytavern_card_from_png_bytes(&bytes)? {
            let _ = write_avatar_json(&avatar_json, &card);
            return Ok(Some(card));
        }
        return Ok(None);
    }
    if avatar_json.exists() {
        return Ok(read_avatar_json(&avatar_json).ok().flatten());
    }
    Ok(None)
}

pub fn extract_sillytavern_card_from_png_bytes(bytes: &[u8]) -> Result<Option<Value>> {
    if bytes.len() < PNG_SIGNATURE.len() || &bytes[..PNG_SIGNATURE.len()] != PNG_SIGNATURE {
        return Ok(None);
    }
    let mut offset = PNG_SIGNATURE.len();
    let mut best_payload: Option<(u8, String)> = None;
    while offset + 12 <= bytes.len() {
        let length = u32::from_be_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("slice length checked"),
        ) as usize;
        let chunk_type = &bytes[offset + 4..offset + 8];
        let data_start = offset + 8;
        let data_end = data_start.saturating_add(length);
        let next_offset = data_end.saturating_add(4);
        if data_end > bytes.len() || next_offset > bytes.len() {
            return Ok(None);
        }
        if chunk_type == b"tEXt" {
            if let Some((keyword, text)) = parse_text_chunk(&bytes[data_start..data_end]) {
                remember_card_payload(&mut best_payload, &keyword, text);
            }
        } else if chunk_type == b"IEND" {
            break;
        }
        offset = next_offset;
    }
    Ok(best_payload.and_then(|(_, text)| parse_card_payload(&text)))
}

fn parse_text_chunk(data: &[u8]) -> Option<(String, String)> {
    let separator = data.iter().position(|byte| *byte == 0)?;
    let keyword = String::from_utf8_lossy(&data[..separator]).to_string();
    let text = String::from_utf8_lossy(&data[separator + 1..]).to_string();
    Some((keyword, text))
}

fn remember_card_payload(best_payload: &mut Option<(u8, String)>, keyword: &str, text: String) {
    let Some(rank) = card_keyword_rank(keyword) else {
        return;
    };
    let should_replace = best_payload
        .as_ref()
        .map(|(best_rank, _)| rank < *best_rank)
        .unwrap_or(true);
    if should_replace {
        *best_payload = Some((rank, text));
    }
}

fn card_keyword_rank(keyword: &str) -> Option<u8> {
    match keyword.trim().to_ascii_lowercase().as_str() {
        "ccv3" => Some(0),
        "chara" => Some(1),
        _ => None,
    }
}

fn parse_card_payload(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    for engine in [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD] {
        if let Ok(decoded) = engine.decode(trimmed.as_bytes()) {
            if let Ok(decoded_text) = String::from_utf8(decoded) {
                if let Ok(value) = serde_json::from_str(decoded_text.trim()) {
                    return Some(value);
                }
            }
        }
    }
    None
}

fn avatar_json_is_current(avatar_png: &Path, avatar_json: &Path) -> Result<bool> {
    if !avatar_json.exists() {
        return Ok(false);
    }
    if let Some(source_hash) = read_avatar_json_source_hash(avatar_json) {
        return Ok(source_hash == file_sha256(avatar_png).ok());
    }
    let png_modified = avatar_png
        .metadata()
        .and_then(|metadata| metadata.modified())
        .with_context(|| format!("failed to inspect avatar PNG: {}", avatar_png.display()))?;
    let json_modified = avatar_json
        .metadata()
        .and_then(|metadata| metadata.modified())
        .with_context(|| format!("failed to inspect avatar JSON: {}", avatar_json.display()))?;
    Ok(json_modified >= png_modified)
}

fn read_avatar_json_source_hash(avatar_json: &Path) -> Option<Option<String>> {
    let value = read_avatar_json(avatar_json).ok().flatten()?;
    let hash = value
        .get("_duckagent")
        .and_then(|metadata| metadata.get("source_sha256"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(hash)
}

fn read_avatar_json(path: &Path) -> Result<Option<Value>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read avatar JSON: {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| format!("failed to parse avatar JSON: {}", path.display()))
}

fn write_avatar_json(path: &Path, card: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create profile directory: {}", parent.display()))?;
    }
    let mut normalized = card.clone();
    if let Value::Object(object) = &mut normalized {
        object.insert(
            "_duckagent".to_string(),
            json!({
                "derived_from": AVATAR_PNG_FILE,
                "parsed_at": Utc::now().to_rfc3339(),
                "source_sha256": source_hash_for_sibling_png(path).ok(),
            }),
        );
    }
    let text =
        serde_json::to_string_pretty(&normalized).context("failed to serialize avatar JSON")?;
    fs::write(path, format!("{text}\n"))
        .with_context(|| format!("failed to write avatar JSON: {}", path.display()))
}

fn source_hash_for_sibling_png(avatar_json: &Path) -> Result<String> {
    let avatar_png = avatar_json
        .parent()
        .context("avatar JSON path has no parent")?
        .join(AVATAR_PNG_FILE);
    file_sha256(&avatar_png)
}

fn file_sha256(path: &Path) -> Result<String> {
    let bytes =
        fs::read(path).with_context(|| format!("failed to read file: {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn render_avatar_card_context(card: &Value) -> Option<String> {
    let data = card_data(card)?;
    let mut sections = Vec::new();
    push_field(&mut sections, "name", lookup_string(data, "name"));
    push_field(
        &mut sections,
        "description",
        lookup_string(data, "description"),
    );
    push_field(
        &mut sections,
        "personality",
        lookup_string(data, "personality"),
    );
    push_field(&mut sections, "scenario", lookup_string(data, "scenario"));
    push_field(
        &mut sections,
        "system_prompt",
        lookup_string(data, "system_prompt"),
    );
    push_field(
        &mut sections,
        "post_history_instructions",
        lookup_string(data, "post_history_instructions"),
    );
    push_field(
        &mut sections,
        "first_message",
        lookup_string(data, "first_mes"),
    );
    push_field(
        &mut sections,
        "example_dialogue",
        lookup_string(data, "mes_example"),
    );
    for entry in constant_character_book_entries(data) {
        push_field(&mut sections, "character_book", Some(entry));
    }
    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

fn card_data(card: &Value) -> Option<&Map<String, Value>> {
    card.get("data")
        .and_then(Value::as_object)
        .or_else(|| card.as_object())
}

fn lookup_string<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn push_field(sections: &mut Vec<String>, label: &str, value: Option<impl AsRef<str>>) {
    let Some(value) = value else {
        return;
    };
    let value = value.as_ref().trim();
    if value.is_empty() {
        return;
    }
    sections.push(format!("{label}:\n{value}"));
}

fn constant_character_book_entries(data: &Map<String, Value>) -> Vec<String> {
    let Some(entries) = data
        .get("character_book")
        .and_then(|book| book.get("entries"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let object = entry.as_object()?;
            if object.get("enabled").and_then(Value::as_bool) == Some(false) {
                return None;
            }
            if object.get("constant").and_then(Value::as_bool) != Some(true) {
                return None;
            }
            let content = lookup_string(object, "content")?;
            let title = lookup_string(object, "comment")
                .or_else(|| lookup_string(object, "name"))
                .unwrap_or("constant entry");
            Some(format!("{title}\n{content}"))
        })
        .collect()
}

fn read_trimmed(path: PathBuf) -> Result<Option<String>> {
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read profile file: {}", path.display()));
        }
    };
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn extracts_base64_card_from_png_text_chunk() -> Result<()> {
        let card = json!({
            "spec": "chara_card_v2",
            "data": {
                "name": "Ada",
                "description": "Careful engineer"
            }
        });
        let encoded = STANDARD.encode(serde_json::to_string(&card)?);
        let png = test_png_with_text_chunk("chara", &encoded);
        let parsed = extract_sillytavern_card_from_png_bytes(&png)?.expect("card");
        assert_eq!(parsed["data"]["name"], json!("Ada"));
        Ok(())
    }

    #[test]
    fn reads_uppercase_text_chunk_keyword_like_sillytavern() -> Result<()> {
        let card = json!({"spec": "chara_card_v2", "data": {"name": "Casey"}});
        let png =
            test_png_with_text_chunk("CHARA", &STANDARD.encode(serde_json::to_string(&card)?));
        let parsed = extract_sillytavern_card_from_png_bytes(&png)?.expect("card");
        assert_eq!(parsed["data"]["name"], json!("Casey"));
        Ok(())
    }

    #[test]
    fn prefers_ccv3_over_legacy_chara_chunk() -> Result<()> {
        let legacy = json!({"spec": "chara_card_v2", "data": {"name": "Legacy"}});
        let v3 = json!({"spec": "chara_card_v3", "data": {"name": "Current"}});
        let mut png = Vec::new();
        png.extend_from_slice(PNG_SIGNATURE);
        write_text_chunk(
            &mut png,
            "chara",
            &STANDARD.encode(serde_json::to_string(&legacy)?),
        );
        write_text_chunk(
            &mut png,
            "ccv3",
            &STANDARD.encode(serde_json::to_string(&v3)?),
        );
        write_chunk(&mut png, b"IEND", &[]);

        let parsed = extract_sillytavern_card_from_png_bytes(&png)?.expect("card");
        assert_eq!(parsed["spec"], json!("chara_card_v3"));
        assert_eq!(parsed["data"]["name"], json!("Current"));
        Ok(())
    }

    #[test]
    fn ignores_non_text_chunks_even_when_they_look_like_cards() -> Result<()> {
        let card = json!({"spec": "chara_card_v3", "data": {"name": "Ignored"}});
        let mut png = Vec::new();
        png.extend_from_slice(PNG_SIGNATURE);
        write_chunk(
            &mut png,
            b"iTXt",
            format!(
                "ccv3\0\0\0\0\0{}",
                STANDARD.encode(serde_json::to_string(&card)?)
            )
            .as_bytes(),
        );
        write_chunk(&mut png, b"IEND", &[]);

        assert!(extract_sillytavern_card_from_png_bytes(&png)?.is_none());
        Ok(())
    }

    #[test]
    fn direct_json_text_chunk_is_plain_avatar_not_sillytavern_card() -> Result<()> {
        let card = json!({"spec": "chara_card_v2", "data": {"name": "Direct"}});
        let png = test_png_with_text_chunk("chara", &serde_json::to_string(&card)?);
        assert!(extract_sillytavern_card_from_png_bytes(&png)?.is_none());
        Ok(())
    }

    #[test]
    fn png_without_text_chunks_is_plain_avatar() -> Result<()> {
        let mut png = Vec::new();
        png.extend_from_slice(PNG_SIGNATURE);
        write_chunk(&mut png, b"IHDR", &[0; 13]);
        write_chunk(&mut png, b"IEND", &[]);

        assert!(extract_sillytavern_card_from_png_bytes(&png)?.is_none());
        Ok(())
    }

    #[test]
    fn invalid_png_card_metadata_is_treated_as_plain_avatar() -> Result<()> {
        let legacy = json!({"spec": "chara_card_v2", "data": {"name": "Legacy"}});
        let mut png = Vec::new();
        png.extend_from_slice(PNG_SIGNATURE);
        write_text_chunk(
            &mut png,
            "chara",
            &STANDARD.encode(serde_json::to_string(&legacy)?),
        );
        write_text_chunk(&mut png, "ccv3", "not-base64-json");
        write_chunk(&mut png, b"IEND", &[]);

        assert!(extract_sillytavern_card_from_png_bytes(&png)?.is_none());
        Ok(())
    }

    #[test]
    fn stale_avatar_json_hash_does_not_override_plain_png() -> Result<()> {
        let dir = tempdir()?;
        let old_png = test_png_with_text_chunk("chara", &STANDARD.encode(r#"{"name":"Old Card"}"#));
        let old_hash = format!("{:x}", Sha256::digest(&old_png));
        fs::write(dir.path().join(AVATAR_PNG_FILE), test_png_without_card())?;
        fs::write(
            dir.path().join(AVATAR_JSON_FILE),
            serde_json::to_string(&json!({
                "name": "Old Card",
                "_duckagent": {
                    "derived_from": AVATAR_PNG_FILE,
                    "source_sha256": old_hash
                }
            }))?,
        )?;

        assert!(load_avatar_card(dir.path())?.is_none());
        Ok(())
    }

    #[test]
    fn empty_current_avatar_json_falls_back_to_png_parse() -> Result<()> {
        let dir = tempdir()?;
        let card = json!({"spec": "chara_card_v2", "data": {"name": "From Png"}});
        let png =
            test_png_with_text_chunk("chara", &STANDARD.encode(serde_json::to_string(&card)?));
        fs::write(dir.path().join(AVATAR_PNG_FILE), png)?;
        fs::write(dir.path().join(AVATAR_JSON_FILE), "")?;

        let loaded = load_avatar_card(dir.path())?.expect("card from png");
        assert_eq!(loaded["data"]["name"], json!("From Png"));
        Ok(())
    }

    #[test]
    fn invalid_avatar_json_is_ignored_when_no_png_exists() -> Result<()> {
        let dir = tempdir()?;
        fs::write(dir.path().join(AVATAR_JSON_FILE), "not json")?;
        assert!(load_avatar_card(dir.path())?.is_none());
        Ok(())
    }

    #[test]
    fn malformed_png_is_treated_as_plain_avatar() -> Result<()> {
        let mut png = Vec::new();
        png.extend_from_slice(PNG_SIGNATURE);
        png.extend_from_slice(&999_u32.to_be_bytes());
        png.extend_from_slice(b"tEXt");
        png.extend_from_slice(b"chara\0broken");

        assert!(extract_sillytavern_card_from_png_bytes(&png)?.is_none());
        Ok(())
    }

    #[test]
    fn profile_blocks_order_avatar_soul_user() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(AVATAR_JSON_FILE),
            serde_json::to_string(&json!({"data": {"name": "Ada"}}))?,
        )?;
        fs::write(dir.path().join(SOUL_FILE), "warm but precise")?;
        fs::write(dir.path().join(USER_FILE), "user likes direct answers")?;
        let blocks = profile_context_blocks_for_dir("demo", dir.path())?;
        let labels = blocks
            .into_iter()
            .map(|block| block.label)
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["AVATAR CARD", "SOUL", "USER"]);
        Ok(())
    }

    #[test]
    fn renders_v1_card_fields_without_data_wrapper() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(AVATAR_JSON_FILE),
            serde_json::to_string(&json!({
                "name": "V1 Name",
                "description": "V1 description",
                "first_mes": "hello"
            }))?,
        )?;

        let blocks = profile_context_blocks_for_dir("demo", dir.path())?;
        let content = blocks
            .iter()
            .find(|block| block.label == "AVATAR CARD")
            .map(|block| block.content.as_str())
            .expect("avatar block");
        assert!(content.contains("name:\nV1 Name"));
        assert!(content.contains("description:\nV1 description"));
        assert!(content.contains("first_message:\nhello"));
        Ok(())
    }

    #[test]
    fn renders_only_constant_enabled_character_book_entries() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(AVATAR_JSON_FILE),
            serde_json::to_string(&json!({
                "data": {
                    "name": "Booky",
                    "character_book": {
                        "entries": [
                            {"comment": "always", "content": "always present", "constant": true},
                            {"comment": "selective", "content": "keyword only", "constant": false},
                            {"comment": "disabled", "content": "never", "constant": true, "enabled": false}
                        ]
                    }
                }
            }))?,
        )?;

        let blocks = profile_context_blocks_for_dir("demo", dir.path())?;
        let content = blocks
            .iter()
            .find(|block| block.label == "AVATAR CARD")
            .map(|block| block.content.as_str())
            .expect("avatar block");
        assert!(content.contains("character_book:\nalways\nalways present"));
        assert!(!content.contains("keyword only"));
        assert!(!content.contains("never"));
        Ok(())
    }

    #[test]
    fn load_avatar_card_derives_rebuildable_json_from_png() -> Result<()> {
        let dir = tempdir()?;
        let card = json!({
            "spec": "chara_card_v3",
            "data": {
                "name": "Cache Test"
            }
        });
        let png = test_png_with_text_chunk("ccv3", &STANDARD.encode(serde_json::to_string(&card)?));
        fs::write(dir.path().join(AVATAR_PNG_FILE), png)?;

        let loaded = load_avatar_card(dir.path())?.expect("avatar card");
        assert_eq!(loaded["data"]["name"], json!("Cache Test"));

        let cached_text = fs::read_to_string(dir.path().join(AVATAR_JSON_FILE))?;
        let cached: Value = serde_json::from_str(&cached_text)?;
        assert_eq!(cached["data"]["name"], json!("Cache Test"));
        assert_eq!(cached["_duckagent"]["derived_from"], json!(AVATAR_PNG_FILE));
        assert!(cached["_duckagent"]["source_sha256"].as_str().is_some());
        Ok(())
    }

    fn test_png_with_text_chunk(keyword: &str, text: &str) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(PNG_SIGNATURE);
        write_text_chunk(&mut data, keyword, text);
        write_chunk(&mut data, b"IEND", &[]);
        data
    }

    fn test_png_without_card() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(PNG_SIGNATURE);
        write_chunk(&mut data, b"IEND", &[]);
        data
    }

    fn write_text_chunk(out: &mut Vec<u8>, keyword: &str, text: &str) {
        let mut chunk = Vec::new();
        chunk.extend_from_slice(keyword.as_bytes());
        chunk.push(0);
        chunk.extend_from_slice(text.as_bytes());
        write_chunk(out, b"tEXt", &chunk);
    }

    fn write_chunk(out: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(chunk_type);
        out.extend_from_slice(data);
        out.extend_from_slice(&[0, 0, 0, 0]);
    }
}
