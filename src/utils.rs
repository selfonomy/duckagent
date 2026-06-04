use anyhow::{Context, Result};
use serde_json::Value;
use std::sync::OnceLock;
use tiktoken_rs::{CoreBPE, cl100k_base};

static TOKENIZER: OnceLock<CoreBPE> = OnceLock::new();

const TRUNCATION_MARKER: &str = "\n[...]\n";

fn tokenizer() -> Result<&'static CoreBPE> {
    if let Some(bpe) = TOKENIZER.get() {
        return Ok(bpe);
    }

    let bpe = cl100k_base().context("failed to initialize tokenizer")?;
    let _ = TOKENIZER.set(bpe);
    TOKENIZER
        .get()
        .context("failed to read tokenizer after initialization")
}

pub fn count_tool_tokens_cl100k(text: &str) -> Result<usize> {
    Ok(tokenizer()?.encode_ordinary(text).len())
}

pub fn estimate_tokens_rough(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    (text.len() + 3) / 4
}

pub fn estimate_messages_tokens_rough(messages: &[Value]) -> usize {
    let total_chars: usize = messages.iter().map(|msg| msg.to_string().len()).sum();
    (total_chars + 3) / 4
}

pub fn truncate_head_middle_tail_by_tokens(text: &str, max_tokens: usize) -> Result<String> {
    if max_tokens == 0 || text.is_empty() {
        return Ok(String::new());
    }

    if estimate_tokens_rough(text) <= max_tokens {
        return Ok(text.to_string());
    }

    let char_budget = max_tokens.saturating_mul(4);
    if char_budget == 0 {
        return Ok(String::new());
    }

    let marker_chars = TRUNCATION_MARKER.chars().count();
    if char_budget <= marker_chars * 2 + 3 {
        return Ok(text.chars().take(char_budget).collect());
    }

    let keep_budget = char_budget.saturating_sub(marker_chars * 2);
    let head_chars = keep_budget / 3;
    let middle_chars = keep_budget / 3;
    let tail_chars = keep_budget.saturating_sub(head_chars + middle_chars);

    let all_chars: Vec<char> = text.chars().collect();
    let total = all_chars.len();
    let middle_start = ((total.saturating_sub(middle_chars)) / 2).min(total);
    let middle_end = (middle_start + middle_chars).min(total);
    let head = all_chars[..head_chars.min(total)]
        .iter()
        .collect::<String>();
    let middle = all_chars[middle_start..middle_end]
        .iter()
        .collect::<String>();
    let tail_start = total.saturating_sub(tail_chars);
    let tail = all_chars[tail_start..].iter().collect::<String>();

    let mut out = String::new();
    out.push_str(&head);
    out.push_str(TRUNCATION_MARKER);
    out.push_str(&middle);
    out.push_str(TRUNCATION_MARKER);
    out.push_str(&tail);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_token_count_returns_positive_for_text() -> Result<()> {
        assert!(count_tool_tokens_cl100k("hello world")? > 0);
        Ok(())
    }

    #[test]
    fn rough_token_estimate_matches_len_div_4_rule() {
        assert_eq!(estimate_tokens_rough(""), 0);
        assert_eq!(estimate_tokens_rough("a"), 1);
        assert_eq!(estimate_tokens_rough("abcd"), 1);
        assert_eq!(estimate_tokens_rough("abcde"), 2);
    }

    #[test]
    fn rough_message_token_estimate_matches_len_div_4_rule() {
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "world"}),
        ];
        let total_chars: usize = messages.iter().map(|msg| msg.to_string().len()).sum();
        assert_eq!(
            estimate_messages_tokens_rough(&messages),
            (total_chars + 3) / 4
        );
    }

    #[test]
    fn truncate_head_middle_tail_keeps_short_text_intact() -> Result<()> {
        let text = "hello world";
        assert_eq!(truncate_head_middle_tail_by_tokens(text, 100)?, text);
        Ok(())
    }

    #[test]
    fn truncate_head_middle_tail_truncates_long_text() -> Result<()> {
        let text = (0..200)
            .map(|i| format!("token-{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let truncated = truncate_head_middle_tail_by_tokens(&text, 40)?;
        assert!(truncated.contains(TRUNCATION_MARKER));
        assert!(estimate_tokens_rough(&truncated) <= 40 + 4);
        Ok(())
    }

    #[test]
    fn truncate_head_middle_tail_handles_zero_budget() -> Result<()> {
        let truncated = truncate_head_middle_tail_by_tokens("hello", 0)?;
        assert!(truncated.is_empty());
        Ok(())
    }
}
