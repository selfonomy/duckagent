use pulldown_cmark::{CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd};

use super::BLOCKQUOTE_MARKER;
use super::code;
use super::table;

#[derive(Default)]
struct InlineStyle {
    bold: usize,
    italic: usize,
    strike: usize,
    link: usize,
}

impl InlineStyle {
    fn open_codes(&self) -> Vec<&'static str> {
        let mut codes = Vec::new();
        if self.bold > 0 {
            codes.push("1");
        }
        if self.italic > 0 {
            codes.push("3");
        }
        if self.strike > 0 {
            codes.push("9");
        }
        if self.link > 0 {
            codes.push("4");
            codes.push("36");
        }
        codes
    }

    fn close_seq(&self, base_fg_rgb: (u8, u8, u8)) -> String {
        if self.bold == 0 && self.italic == 0 && self.strike == 0 && self.link == 0 {
            String::new()
        } else {
            format!(
                "\x1b[22;23;24;29;38;2;{};{};{}m",
                base_fg_rgb.0, base_fg_rgb.1, base_fg_rgb.2
            )
        }
    }

    fn apply(&self, text: &str, base_fg_rgb: (u8, u8, u8)) -> String {
        let codes = self.open_codes();
        if codes.is_empty() {
            return text.to_string();
        }
        format!(
            "\x1b[{}m{}{}",
            codes.join(";"),
            text,
            self.close_seq(base_fg_rgb)
        )
    }
}

#[derive(Clone, Copy)]
struct ListState {
    ordered: bool,
    next_index: u64,
}

#[derive(Default)]
pub(crate) struct MarkdownRenderer {
    width: usize,
    code_is_light: bool,
    code_bg_rgb: (u8, u8, u8),
    base_fg_rgb: (u8, u8, u8),
    table_palette: table::TablePalette,
}

impl MarkdownRenderer {
    pub(crate) fn new(
        width: usize,
        code_is_light: bool,
        code_bg_rgb: (u8, u8, u8),
        base_fg_rgb: (u8, u8, u8),
    ) -> Self {
        Self {
            width: width.max(1),
            code_is_light,
            code_bg_rgb,
            base_fg_rgb,
            table_palette: table::TablePalette::from_bubble_bg(code_bg_rgb, code_is_light),
        }
    }

    pub(crate) fn render_lines(&self, input: &str) -> Vec<String> {
        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_FOOTNOTES);
        let parser = Parser::new_ext(input, options);

        let mut out = Vec::<String>::new();
        let mut line = String::new();

        let mut inline = InlineStyle::default();
        let mut list_stack = Vec::<ListState>::new();
        let mut blockquote_depth = 0usize;
        let mut link_stack = Vec::<String>::new();
        let mut current_footnote: Option<String> = None;
        let mut footnote_first_line = false;

        let mut in_code_block = false;
        let mut code_lang: Option<String> = None;
        let mut code_buffer = String::new();

        let mut in_table = false;
        let mut table_rows = Vec::<Vec<String>>::new();
        let mut table_row = Vec::<String>::new();
        let mut table_cell = String::new();
        let mut table_inline = InlineStyle::default();

        let flush_line = |line: &mut String, out: &mut Vec<String>| {
            out.push(std::mem::take(line));
        };

        let line_prefix = |list_stack: &[ListState], blockquote_depth: usize| -> String {
            let mut prefix = String::new();
            if blockquote_depth > 0 {
                prefix.push_str(BLOCKQUOTE_MARKER);
            }
            if list_stack.len() > 1 {
                prefix.push_str(&"  ".repeat(list_stack.len() - 1));
            }
            prefix
        };

        let ensure_line_prefix = |line: &mut String,
                                  list_stack: &[ListState],
                                  blockquote_depth: usize,
                                  current_footnote: Option<&str>,
                                  footnote_first_line: &mut bool| {
            if !line.is_empty() {
                return;
            }
            line.push_str(&line_prefix(list_stack, blockquote_depth));
            if let Some(label) = current_footnote {
                if *footnote_first_line {
                    line.push_str(&format!("[^{label}]: "));
                    *footnote_first_line = false;
                } else {
                    line.push_str("    ");
                }
            }
        };

        let push_text = |text: &str,
                         line: &mut String,
                         out: &mut Vec<String>,
                         inline: &InlineStyle,
                         list_stack: &[ListState],
                         blockquote_depth: usize,
                         active_link_url: Option<&str>,
                         current_footnote: Option<&str>,
                         footnote_first_line: &mut bool| {
            for (idx, part) in text.split('\n').enumerate() {
                ensure_line_prefix(
                    line,
                    list_stack,
                    blockquote_depth,
                    current_footnote,
                    footnote_first_line,
                );
                if !part.is_empty() {
                    let rendered = inline.apply(part, self.base_fg_rgb);
                    if let Some(url) = active_link_url {
                        line.push_str(&format!("\x1b]8;;{url}\x07{rendered}\x1b]8;;\x07"));
                    } else {
                        line.push_str(&rendered);
                    }
                }
                if idx + 1 < text.split('\n').count() {
                    flush_line(line, out);
                }
            }
        };

        for ev in parser {
            if in_table {
                match ev {
                    Event::Start(Tag::Emphasis) => {
                        table_inline.italic = table_inline.italic.saturating_add(1)
                    }
                    Event::End(TagEnd::Emphasis) => {
                        table_inline.italic = table_inline.italic.saturating_sub(1)
                    }
                    Event::Start(Tag::Strong) => {
                        table_inline.bold = table_inline.bold.saturating_add(1)
                    }
                    Event::End(TagEnd::Strong) => {
                        table_inline.bold = table_inline.bold.saturating_sub(1)
                    }
                    Event::Start(Tag::Strikethrough) => {
                        table_inline.strike = table_inline.strike.saturating_add(1)
                    }
                    Event::End(TagEnd::Strikethrough) => {
                        table_inline.strike = table_inline.strike.saturating_sub(1)
                    }
                    Event::Start(Tag::Link { .. }) => {
                        table_inline.link = table_inline.link.saturating_add(1)
                    }
                    Event::End(TagEnd::Link) => {
                        table_inline.link = table_inline.link.saturating_sub(1)
                    }
                    Event::End(TagEnd::TableCell) => {
                        table_row.push(std::mem::take(&mut table_cell));
                        table_inline = InlineStyle::default();
                    }
                    Event::End(TagEnd::TableHead) => {
                        if !table_row.is_empty() {
                            table_rows.push(std::mem::take(&mut table_row));
                        }
                    }
                    Event::End(TagEnd::TableRow) => {
                        if !table_row.is_empty() {
                            table_rows.push(std::mem::take(&mut table_row));
                        }
                    }
                    Event::End(TagEnd::Table) => {
                        in_table = false;
                        if !line.is_empty() {
                            flush_line(&mut line, &mut out);
                        }
                        out.extend(table::render_table(
                            &table_rows,
                            self.width,
                            self.table_palette,
                        ));
                        if out.last().is_some_and(|last| !last.is_empty()) {
                            out.push(String::new());
                        }
                        table_rows.clear();
                    }
                    Event::Text(text) => {
                        if !table_cell.is_empty() {
                            table_cell.push(' ');
                        }
                        let rendered = table_inline.apply(&text, self.base_fg_rgb);
                        table_cell.push_str(&rendered);
                    }
                    Event::Code(text) => {
                        if !table_cell.is_empty() {
                            table_cell.push(' ');
                        }
                        let rendered = format!(
                            "\x1b[38;5;215m{}\x1b[38;2;{};{};{}m",
                            text, self.base_fg_rgb.0, self.base_fg_rgb.1, self.base_fg_rgb.2
                        );
                        table_cell.push_str(&rendered);
                    }
                    _ => {}
                }
                continue;
            }

            if in_code_block {
                match ev {
                    Event::End(TagEnd::CodeBlock) => {
                        in_code_block = false;
                        if !line.is_empty() {
                            flush_line(&mut line, &mut out);
                        }
                        let highlighted = code::highlight_code_block(
                            &code_buffer,
                            code_lang.as_deref(),
                            self.code_is_light,
                            self.code_bg_rgb,
                        );
                        out.extend(code::decorate_code_lines(
                            highlighted,
                            Some(self.width),
                            self.code_is_light,
                            self.code_bg_rgb,
                        ));
                        code_buffer.clear();
                        code_lang = None;
                    }
                    Event::Text(text) => code_buffer.push_str(&text),
                    Event::SoftBreak | Event::HardBreak => code_buffer.push('\n'),
                    _ => {}
                }
                continue;
            }

            match ev {
                Event::Start(Tag::Paragraph) => {
                    if !line.is_empty() {
                        flush_line(&mut line, &mut out);
                    }
                }
                Event::End(TagEnd::Paragraph) => {
                    if !line.is_empty() {
                        flush_line(&mut line, &mut out);
                    }
                    if blockquote_depth == 0
                        && current_footnote.is_none()
                        && out.last().is_some_and(|last| !last.is_empty())
                    {
                        out.push(String::new());
                    }
                }
                Event::Start(Tag::Heading { level, .. }) => {
                    if !line.is_empty() {
                        flush_line(&mut line, &mut out);
                    }
                    let _ = level;
                    line.push_str("\x1b[1m");
                }
                Event::End(TagEnd::Heading(_)) => {
                    line.push_str("\x1b[22m");
                    if !line.is_empty() {
                        flush_line(&mut line, &mut out);
                    }
                }
                Event::Start(Tag::BlockQuote(_)) => {
                    blockquote_depth = blockquote_depth.saturating_add(1);
                }
                Event::End(TagEnd::BlockQuote(_)) => {
                    blockquote_depth = blockquote_depth.saturating_sub(1);
                    if !line.is_empty() {
                        flush_line(&mut line, &mut out);
                    }
                }
                Event::Start(Tag::List(start)) => {
                    list_stack.push(ListState {
                        ordered: start.is_some(),
                        next_index: start.unwrap_or(1),
                    });
                }
                Event::Start(Tag::FootnoteDefinition(label)) => {
                    if !line.is_empty() {
                        flush_line(&mut line, &mut out);
                    }
                    current_footnote = Some(label.to_string());
                    footnote_first_line = true;
                }
                Event::End(TagEnd::List(_)) => {
                    list_stack.pop();
                    if !line.is_empty() {
                        flush_line(&mut line, &mut out);
                    }
                }
                Event::End(TagEnd::FootnoteDefinition) => {
                    if !line.is_empty() {
                        flush_line(&mut line, &mut out);
                    }
                    current_footnote = None;
                    footnote_first_line = false;
                    if out.last().is_some_and(|last| !last.is_empty()) {
                        out.push(String::new());
                    }
                }
                Event::Start(Tag::Item) => {
                    if !line.is_empty() {
                        flush_line(&mut line, &mut out);
                    }
                    line.push_str(&line_prefix(&list_stack, blockquote_depth));
                    if let Some(last) = list_stack.last_mut() {
                        if last.ordered {
                            line.push_str(&format!("{}. ", last.next_index));
                            last.next_index = last.next_index.saturating_add(1);
                        } else {
                            line.push_str("• ");
                        }
                    }
                }
                Event::End(TagEnd::Item) => {
                    if !line.is_empty() {
                        flush_line(&mut line, &mut out);
                    }
                }
                Event::Start(Tag::CodeBlock(kind)) => {
                    in_code_block = true;
                    code_lang = match kind {
                        CodeBlockKind::Fenced(lang) => {
                            let l = lang.split([',', ' ', '\t']).next().unwrap_or("").trim();
                            if l.is_empty() {
                                None
                            } else {
                                Some(l.to_string())
                            }
                        }
                        CodeBlockKind::Indented => None,
                    };
                    code_buffer.clear();
                }
                Event::Start(Tag::Table(_)) => {
                    in_table = true;
                    table_rows.clear();
                    table_row.clear();
                    table_cell.clear();
                }
                Event::Start(Tag::Emphasis) => inline.italic = inline.italic.saturating_add(1),
                Event::End(TagEnd::Emphasis) => inline.italic = inline.italic.saturating_sub(1),
                Event::Start(Tag::Strong) => inline.bold = inline.bold.saturating_add(1),
                Event::End(TagEnd::Strong) => inline.bold = inline.bold.saturating_sub(1),
                Event::Start(Tag::Strikethrough) => inline.strike = inline.strike.saturating_add(1),
                Event::End(TagEnd::Strikethrough) => {
                    inline.strike = inline.strike.saturating_sub(1)
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    inline.link = inline.link.saturating_add(1);
                    link_stack.push(dest_url.to_string());
                }
                Event::End(TagEnd::Link) => {
                    inline.link = inline.link.saturating_sub(1);
                    let _ = link_stack.pop();
                }
                Event::Code(code) => {
                    ensure_line_prefix(
                        &mut line,
                        &list_stack,
                        blockquote_depth,
                        current_footnote.as_deref(),
                        &mut footnote_first_line,
                    );
                    let rendered = format!(
                        "\x1b[38;5;215m{}\x1b[38;2;{};{};{}m",
                        code, self.base_fg_rgb.0, self.base_fg_rgb.1, self.base_fg_rgb.2
                    );
                    if let Some(url) = link_stack.last() {
                        line.push_str(&format!("\x1b]8;;{url}\x07{rendered}\x1b]8;;\x07"));
                    } else {
                        line.push_str(&rendered);
                    }
                }
                Event::Text(text) => {
                    push_text(
                        &text,
                        &mut line,
                        &mut out,
                        &inline,
                        &list_stack,
                        blockquote_depth,
                        link_stack.last().map(String::as_str),
                        current_footnote.as_deref(),
                        &mut footnote_first_line,
                    );
                }
                Event::SoftBreak | Event::HardBreak => {
                    if !line.is_empty() {
                        flush_line(&mut line, &mut out);
                    }
                }
                Event::Rule => {
                    if !line.is_empty() {
                        flush_line(&mut line, &mut out);
                    }
                    out.push("─".repeat(self.width.min(80).max(3)));
                }
                Event::Html(html) | Event::InlineHtml(html) => {
                    push_text(
                        &html,
                        &mut line,
                        &mut out,
                        &inline,
                        &list_stack,
                        blockquote_depth,
                        link_stack.last().map(String::as_str),
                        current_footnote.as_deref(),
                        &mut footnote_first_line,
                    );
                }
                Event::TaskListMarker(checked) => {
                    ensure_line_prefix(
                        &mut line,
                        &list_stack,
                        blockquote_depth,
                        current_footnote.as_deref(),
                        &mut footnote_first_line,
                    );
                    line.push_str(if checked { "[x] " } else { "[ ] " });
                }
                Event::FootnoteReference(CowStr::Borrowed(label)) => {
                    ensure_line_prefix(
                        &mut line,
                        &list_stack,
                        blockquote_depth,
                        current_footnote.as_deref(),
                        &mut footnote_first_line,
                    );
                    line.push_str(&format!("[^{label}]"));
                }
                Event::FootnoteReference(CowStr::Boxed(label)) => {
                    ensure_line_prefix(
                        &mut line,
                        &list_stack,
                        blockquote_depth,
                        current_footnote.as_deref(),
                        &mut footnote_first_line,
                    );
                    line.push_str(&format!("[^{label}]"));
                }
                Event::FootnoteReference(CowStr::Inlined(label)) => {
                    ensure_line_prefix(
                        &mut line,
                        &list_stack,
                        blockquote_depth,
                        current_footnote.as_deref(),
                        &mut footnote_first_line,
                    );
                    line.push_str(&format!("[^{}]", label.as_ref()));
                }
                _ => {}
            }
        }

        if !line.is_empty() {
            out.push(line);
        }

        if out.is_empty() {
            out.push(String::new());
        }
        out
    }
}
