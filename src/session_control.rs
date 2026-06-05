use crate::session::{RewindListItem, SessionMeta};

pub const SESSION_LIST_PAGE_SIZE: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionControlCommand {
    New,
    Resume { index: Option<usize> },
    Rewind { index: Option<usize> },
    Invalid { message: String },
}

#[derive(Debug, Clone)]
pub struct SessionListItem {
    pub session_id: String,
    pub title: String,
    pub source: String,
    pub updated_at: String,
}

pub fn parse_session_control_command(text: &str) -> Option<SessionControlCommand> {
    let trimmed = text.trim();
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return None;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let command = parts.next()?.trim();
    let args = parts.next().unwrap_or("").trim();
    match command {
        "/new" => Some(SessionControlCommand::New),
        "/resume" => {
            if args.is_empty() {
                Some(SessionControlCommand::Resume { index: None })
            } else {
                match args.parse::<usize>().ok().filter(|index| *index > 0) {
                    Some(index) => Some(SessionControlCommand::Resume { index: Some(index) }),
                    None => Some(SessionControlCommand::Invalid {
                        message: "Usage: /resume [number]".to_string(),
                    }),
                }
            }
        }
        "/rewind" => {
            if args.is_empty() {
                Some(SessionControlCommand::Rewind { index: None })
            } else {
                match args.parse::<usize>().ok().filter(|index| *index > 0) {
                    Some(index) => Some(SessionControlCommand::Rewind { index: Some(index) }),
                    None => Some(SessionControlCommand::Invalid {
                        message: "Usage: /rewind [number]".to_string(),
                    }),
                }
            }
        }
        _ => None,
    }
}

pub fn session_meta_to_list_item(meta: &SessionMeta, source: impl Into<String>) -> SessionListItem {
    SessionListItem {
        session_id: meta.id.clone(),
        title: meta.title.clone(),
        source: source.into(),
        updated_at: meta.updated_at.clone(),
    }
}

pub fn filter_session_items(
    mut items: Vec<SessionListItem>,
    query: Option<&str>,
) -> Vec<SessionListItem> {
    if let Some(query) = query.map(str::trim).filter(|value| !value.is_empty()) {
        let needle = query.to_lowercase();
        items.retain(|item| {
            item.title.to_lowercase().contains(&needle)
                || item.source.to_lowercase().contains(&needle)
        });
    }
    items.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.session_id.cmp(&a.session_id))
    });
    items
}

pub fn paginate_session_items(
    items: &[SessionListItem],
    page: usize,
) -> (&[SessionListItem], usize, usize) {
    let page = page.max(1);
    let total_pages = items.len().div_ceil(SESSION_LIST_PAGE_SIZE).max(1);
    let page = page.min(total_pages);
    let start = (page - 1) * SESSION_LIST_PAGE_SIZE;
    let end = (start + SESSION_LIST_PAGE_SIZE).min(items.len());
    (&items[start..end], page, total_pages)
}

pub fn format_session_list(items: &[SessionListItem], page: usize, total_pages: usize) -> String {
    if items.is_empty() {
        return "No sessions found for this chat yet.\n\nUse `/new` to start one.".to_string();
    }
    let mut out = String::new();
    let _ = (page, total_pages);
    out.push_str("Recent sessions:\n\n");
    for (idx, item) in items.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", idx + 1, item.title));
        out.push_str(&format!("   source: {}\n", item.source));
        out.push_str(&format!(
            "   updated: {}\n\n",
            compact_timestamp(&item.updated_at)
        ));
    }
    out.push_str("Reply with:\n");
    for idx in 1..=items.len().min(3) {
        out.push_str(&format!("/resume {idx}\n"));
    }
    out.trim_end().to_string()
}

pub fn format_rewind_list(items: &[RewindListItem]) -> String {
    if items.is_empty() {
        return "No rewind points found in this session yet.".to_string();
    }
    let mut out = String::from("Rewind points:\n\n");
    for item in items {
        out.push_str(&format!("{}. {}\n", item.index, item.preview));
    }
    out.push_str("\nReply with:\n");
    for item in items.iter().take(3) {
        out.push_str(&format!("/rewind {}\n", item.index));
    }
    out.trim_end().to_string()
}

fn compact_timestamp(value: &str) -> String {
    value
        .strip_suffix("+00:00")
        .or_else(|| value.strip_suffix('Z'))
        .unwrap_or(value)
        .replace('T', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_control_commands() {
        assert_eq!(
            parse_session_control_command("/new"),
            Some(SessionControlCommand::New)
        );
        assert_eq!(
            parse_session_control_command("/new Travel plan"),
            Some(SessionControlCommand::New)
        );
        assert_eq!(
            parse_session_control_command("/resume"),
            Some(SessionControlCommand::Resume { index: None })
        );
        assert_eq!(
            parse_session_control_command("/resume 3"),
            Some(SessionControlCommand::Resume { index: Some(3) })
        );
        assert_eq!(parse_session_control_command("/sessions"), None);
        assert_eq!(
            parse_session_control_command("/rewind"),
            Some(SessionControlCommand::Rewind { index: None })
        );
        assert_eq!(
            parse_session_control_command("/rewind 2"),
            Some(SessionControlCommand::Rewind { index: Some(2) })
        );
    }

    #[test]
    fn formats_numbered_list_without_session_ids() {
        let text = format_session_list(
            &[SessionListItem {
                session_id: "019e".to_string(),
                title: "Travel plan".to_string(),
                source: "current chat".to_string(),
                updated_at: "2026-05-16T07:33:54+00:00".to_string(),
            }],
            1,
            1,
        );
        assert!(text.contains("1. Travel plan"));
        assert!(text.contains("/resume 1"));
        assert!(!text.contains("019e"));
    }
}
