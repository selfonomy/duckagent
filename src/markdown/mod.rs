mod code;
mod render;
mod stream;
mod table;

pub(crate) const BLOCKQUOTE_MARKER: &str = "<@BQ@>";
pub(crate) const TABLE_MARKER: &str = "<@TB@>";

pub(crate) fn render_assistant_markdown(markdown: &str, width: usize) -> String {
    render_assistant_markdown_with_mode(
        markdown,
        width,
        /*code_is_light*/ false,
        (50, 48, 47),
        (235, 219, 178),
    )
}

pub(crate) fn render_assistant_markdown_with_mode(
    markdown: &str,
    width: usize,
    code_is_light: bool,
    code_bg_rgb: (u8, u8, u8),
    base_fg_rgb: (u8, u8, u8),
) -> String {
    let lines = render::MarkdownRenderer::new(width, code_is_light, code_bg_rgb, base_fg_rgb)
        .render_lines(markdown);
    lines.join("\n")
}

#[allow(dead_code)]
pub(crate) fn supported_highlight_languages() -> Vec<String> {
    code::supported_languages()
}

#[allow(dead_code)]
pub(crate) struct StreamRenderer {
    inner: stream::MarkdownStream,
}

#[allow(dead_code)]
impl StreamRenderer {
    pub(crate) fn new(
        width: usize,
        code_is_light: bool,
        code_bg_rgb: (u8, u8, u8),
        base_fg_rgb: (u8, u8, u8),
    ) -> Self {
        Self {
            inner: stream::MarkdownStream::new(width, code_is_light, code_bg_rgb, base_fg_rgb),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.inner.clear();
    }

    pub(crate) fn push_delta(&mut self, delta: &str) {
        self.inner.push_delta(delta);
    }

    pub(crate) fn commit_complete_lines(&mut self) -> Vec<String> {
        self.inner.commit_complete_lines()
    }

    pub(crate) fn preview_incomplete_line(&self) -> Option<String> {
        self.inner.preview_incomplete_line()
    }

    pub(crate) fn finalize_and_drain(&mut self) -> Vec<String> {
        self.inner.finalize_and_drain()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_renders_heading_and_bold() {
        let out = render_assistant_markdown("# hello\n**world**", 80);
        assert!(out.contains("hello"));
        assert!(!out.contains("# hello"));
        assert!(out.contains("world"));
    }

    #[test]
    fn markdown_renders_table() {
        let out = render_assistant_markdown("| A | B |\n|---|---|\n| 1 | 2 |", 80);
        assert!(out.contains("\x1b[48;2;"));
        assert!(!out.contains("┌"));
        assert!(!out.contains("│"));
    }

    #[test]
    fn markdown_table_header_not_duplicated() {
        let out = render_assistant_markdown(
            "| Col 1 | Col 2 | Col 3 |\n|-------|-------|-------|\n| Data 1 | Data 2 | Data 3 |\n| Data 4 | Data 5 | Data 6 |",
            100,
        );
        assert_eq!(out.matches("Col 1").count(), 1);
    }

    #[test]
    fn markdown_table_cell_inline_bold_is_rendered() {
        let out = render_assistant_markdown("| A |\n|---|\n| **Total** |", 80);
        assert!(out.contains("\x1b[1mTotal"));
    }

    #[test]
    fn markdown_table_header_text_is_bold() {
        let out = render_assistant_markdown("| Name |\n|---|\n| Alice |", 80);
        assert!(out.contains("\x1b[1mName"));
    }

    #[test]
    fn markdown_table_body_cells_have_vertical_padding() {
        let out = render_assistant_markdown("| A |\n|---|\n| 1 |", 80);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.len() >= 6);
    }

    #[test]
    fn markdown_inserts_blank_line_between_table_and_following_quote() {
        let out = render_assistant_markdown("| A | B |\n|---|---|\n| 1 | 2 |\n\n> quoted", 80);
        let lines: Vec<&str> = out.lines().collect();
        let quote_idx = lines
            .iter()
            .position(|line| line.starts_with(BLOCKQUOTE_MARKER))
            .expect("quote line exists");
        assert!(quote_idx > 0);
        assert!(lines[quote_idx - 1].is_empty());
    }

    #[test]
    fn markdown_renders_code_without_box() {
        let out = render_assistant_markdown("```rust\nlet x = 1;\n```", 80);
        assert!(out.contains("\x1b[48;2;"));
        assert!(!out.contains("  1   "));
        assert!(!out.contains("┌─ code"));
        assert!(!out.contains("└─"));
    }

    #[test]
    fn markdown_wraps_long_code_line_when_width_is_narrow() {
        let out = render_assistant_markdown(
            "```js\nconst veryLongIdentifierName = anotherVeryLongIdentifierName + 12345;\n```",
            28,
        );
        assert!(out.lines().count() >= 2);
    }

    #[test]
    fn markdown_renders_blockquote_without_visible_bar() {
        let out = render_assistant_markdown("> quote", 80);
        assert!(out.contains(BLOCKQUOTE_MARKER));
        assert!(!out.contains("▌ "));
    }

    #[test]
    fn markdown_renders_multiline_blockquote_without_dropping_tail() {
        let md = "> First quoted line\n>\n> Second quoted line (blank line separates paragraphs)\n>\n>   Third indented quoted line\n> Fourth line continues";
        let out = render_assistant_markdown(md, 80);
        assert!(out.contains("First quoted line"));
        assert!(out.contains("Second quoted line (blank line separates paragraphs)"));
        assert!(out.contains("Third indented quoted line"));
        assert!(out.contains("Fourth line continues"));
        assert!(!out.contains("\n>\n"));
    }

    #[test]
    fn markdown_renders_footnote_references_and_definitions() {
        let md = "> Quote[^1]\n\n[^1]: Footnote content";
        let out = render_assistant_markdown(md, 80);
        assert!(out.contains(&format!("{BLOCKQUOTE_MARKER}Quote[^1]")));
        assert!(out.contains("[^1]: Footnote content"));
    }

    #[test]
    fn markdown_complex_table_quote_footnotes_do_not_duplicate_lines() {
        let md = "Here's a table example with citations below it:\n\n| Name     | Type     | Version | Notes              |\n|----------|----------|---------|--------------------|\n| markdown | markup   | 1.0     | lightweight syntax |\n| HTML     | markup   | 5       | hypertext markup   |\n| JSON     | data     | 2020    | lightweight format |\n| YAML     | data     | 1.2     | readable format    |\n\n> **Example Quote 1**: Markdown is a lightweight markup language created by John Gruber in 2004 for readable plain text. [^1]\n\n> **Example Quote 2**: Markdown tables use pipes `|` and hyphens `-`, and alignment markers control column layout. [^2]\n\n> **Example Quote 3**: Blockquotes use the `>` marker and can be nested or combined with other Markdown syntax. [^3]\n\n[^1]: Gruber, J. (2004). *Markdown: Syntax*. https://daringfireball.net/projects/markdown/syntax\n\n[^2]: This table example demonstrates headers, alignment, and cell separation.\n\n[^3]: Blockquotes support multiline content, nesting, code blocks, lists, and other combinations.";
        let out = render_assistant_markdown(md, 88);
        assert_eq!(out.matches("Example Quote 1").count(), 1);
        assert_eq!(out.matches("Example Quote 2").count(), 1);
        assert_eq!(out.matches("Example Quote 3").count(), 1);
        assert_eq!(out.matches("[^1]:").count(), 1);
        assert_eq!(out.matches("[^2]:").count(), 1);
        assert_eq!(out.matches("[^3]:").count(), 1);
    }

    #[test]
    fn markdown_multiline_footnote_definition_indents_continuation() {
        let md = "[^1]: First line\n    Second line continues the explanation";
        let out = render_assistant_markdown(md, 80);
        assert!(out.contains("[^1]: First line"));
        assert!(out.contains("    Second line continues the explanation"));
    }

    #[test]
    fn markdown_links_emit_clickable_osc8_sequence() {
        let out = render_assistant_markdown("[Google](https://google.com)", 80);
        assert!(out.contains("\x1b]8;;https://google.com\x07"));
        assert!(out.contains("\x1b]8;;\x07"));
    }

    #[test]
    fn stream_blockquote_preview_uses_hidden_marker_instead_of_raw_marker() {
        let mut s = StreamRenderer::new(80, false, (50, 48, 47), (235, 219, 178));
        s.push_delta(">");
        assert_eq!(
            s.preview_incomplete_line().as_deref(),
            Some(BLOCKQUOTE_MARKER)
        );
    }

    #[test]
    fn stream_blockquote_preview_renders_nested_inline_styles() {
        let mut s = StreamRenderer::new(80, false, (50, 48, 47), (235, 219, 178));
        s.push_delta("> **Note:** All figures are in USD millions.");
        let preview = s.preview_incomplete_line().unwrap_or_default();
        assert!(preview.starts_with(BLOCKQUOTE_MARKER));
        assert!(preview.contains("Note:"));
        assert!(preview.contains("\x1b[1m"));
        assert!(!preview.contains("**Note:**"));
    }

    #[test]
    fn stream_blockquote_preview_preserves_footnote_reference_text() {
        let mut s = StreamRenderer::new(80, false, (50, 48, 47), (235, 219, 178));
        s.push_delta("> Quote example[^2]");
        let preview = s.preview_incomplete_line().unwrap_or_default();
        assert!(preview.starts_with(BLOCKQUOTE_MARKER));
        assert!(preview.contains("[^2]"));
    }

    #[test]
    fn stream_list_preview_renders_nested_inline_styles() {
        let mut s = StreamRenderer::new(80, false, (50, 48, 47), (235, 219, 178));
        s.push_delta("- **bold** and `code`");
        let preview = s.preview_incomplete_line().unwrap_or_default();
        assert!(preview.contains("• "));
        assert!(preview.contains("\x1b[1m"));
        assert!(preview.contains("\x1b[38;5;215mcode"));
    }

    #[test]
    fn stream_commits_only_on_newline() {
        let mut s = StreamRenderer::new(80, false, (50, 48, 47), (235, 219, 178));
        s.push_delta("hello");
        assert!(s.commit_complete_lines().is_empty());
        s.push_delta("\n");
        assert_eq!(s.commit_complete_lines().len(), 2);
    }

    #[test]
    fn stream_defers_unconfirmed_table_header_line() {
        let mut s = StreamRenderer::new(80, false, (50, 48, 47), (235, 219, 178));
        s.push_delta("| A | B |\n");
        let committed = s.commit_complete_lines();
        assert!(committed.is_empty());
    }

    #[test]
    fn stream_commits_table_after_separator_arrives() {
        let mut s = StreamRenderer::new(80, false, (50, 48, 47), (235, 219, 178));
        s.push_delta("| A | B |\n");
        let _ = s.commit_complete_lines();
        s.push_delta("|---|---|\n| 1 | 2 |\n");
        let committed = s.commit_complete_lines();
        assert!(committed.iter().any(|line| line.contains(TABLE_MARKER)));
        assert!(!committed.iter().any(|line| line.contains("| A | B |")));
    }

    #[test]
    fn stream_table_like_incomplete_line_is_not_previewed() {
        let mut s = StreamRenderer::new(80, false, (50, 48, 47), (235, 219, 178));
        s.push_delta("A | B | C");
        assert!(s.preview_incomplete_line().is_none());
    }

    #[test]
    fn stream_table_separator_prefix_is_not_previewed() {
        let mut s = StreamRenderer::new(80, false, (50, 48, 47), (235, 219, 178));
        s.push_delta("|---");
        assert!(s.preview_incomplete_line().is_none());
    }
}
