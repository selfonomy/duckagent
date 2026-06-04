use serde_json::Value;

pub fn map_mcp_tool_result(value: Value) -> String {
    if let Some(error) = value.get("isError").and_then(Value::as_bool).filter(|v| *v) {
        let prefix = if error { "Tool error: " } else { "" };
        return format!("{prefix}{}", mcp_content_to_text(&value));
    }
    mcp_content_to_text(&value)
}

fn mcp_content_to_text(value: &Value) -> String {
    let Some(content) = value.get("content").and_then(Value::as_array) else {
        return value.to_string();
    };
    let mut parts = Vec::new();
    for item in content {
        match item.get("type").and_then(Value::as_str).unwrap_or_default() {
            "text" => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                }
            }
            "image" | "audio" | "blob" => parts.push(item.to_string()),
            other => parts.push(format!(
                "<mcp_content type=\"{other}\">{item}</mcp_content>"
            )),
        }
    }
    if parts.is_empty() {
        value.to_string()
    } else {
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_text_content() {
        let out = map_mcp_tool_result(json!({
            "content": [{ "type": "text", "text": "hello" }]
        }));
        assert_eq!(out, "hello");
    }

    #[test]
    fn binary_content_remains_visible_to_current_tool_loop() {
        let out = map_mcp_tool_result(json!({
            "content": [{ "type": "image", "mimeType": "image/png", "data": "AAAA" }]
        }));
        assert!(out.contains("image/png"));
        assert!(out.contains("AAAA"));
    }
}
