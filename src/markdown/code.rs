use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static DARK_THEME: OnceLock<Theme> = OnceLock::new();
static LIGHT_THEME: OnceLock<Theme> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme(code_is_light: bool) -> &'static Theme {
    let load = |preferred: &str| {
        let themes = ThemeSet::load_defaults();
        themes
            .themes
            .get(preferred)
            .cloned()
            .or_else(|| themes.themes.values().next().cloned())
            .unwrap_or_default()
    };
    if code_is_light {
        LIGHT_THEME.get_or_init(|| load("base16-ocean.light"))
    } else {
        DARK_THEME.get_or_init(|| load("base16-ocean.dark"))
    }
}

fn find_syntax(lang: &str) -> Option<&'static SyntaxReference> {
    if lang.trim().is_empty() {
        return None;
    }
    let ss = syntax_set();
    ss.find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_name(lang))
        .or_else(|| ss.find_syntax_by_extension(lang))
}

fn style_prefix(style: syntect::highlighting::Style, bg_sgr: &str) -> String {
    let mut parts = Vec::new();
    parts.push(bg_sgr.to_string());
    parts.push(format!(
        "38;2;{};{};{}",
        style.foreground.r, style.foreground.g, style.foreground.b
    ));
    if style.font_style.contains(FontStyle::BOLD) {
        parts.push("1".to_string());
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        parts.push("3".to_string());
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        parts.push("4".to_string());
    }
    format!("\x1b[{}m", parts.join(";"))
}

fn style_suffix(style: syntect::highlighting::Style, bg_reset_sgr: &str) -> String {
    if style.font_style == FontStyle::empty() {
        format!("\x1b[39;{bg_reset_sgr}m")
    } else {
        format!("\x1b[22;23;24;39;{bg_reset_sgr}m")
    }
}

pub(crate) fn highlight_code_block(
    code: &str,
    lang: Option<&str>,
    code_is_light: bool,
    code_bg_rgb: (u8, u8, u8),
) -> Vec<String> {
    let theme = theme(code_is_light);
    let bg_sgr = format!("48;2;{};{};{}", code_bg_rgb.0, code_bg_rgb.1, code_bg_rgb.2);
    let Some(lang) = lang.and_then(find_syntax) else {
        return code.lines().map(ToString::to_string).collect();
    };

    let mut highlighter = HighlightLines::new(lang, theme);
    let mut out = Vec::new();

    for line in LinesWithEndings::from(code) {
        let mut rendered = String::new();
        if let Ok(ranges) = highlighter.highlight_line(line, syntax_set()) {
            for (style, text) in ranges {
                let text = text.trim_end_matches(['\n', '\r']);
                if text.is_empty() {
                    continue;
                }
                rendered.push_str(&style_prefix(style, &bg_sgr));
                rendered.push_str(text);
                rendered.push_str(&style_suffix(style, &bg_sgr));
            }
        } else {
            rendered.push_str(line.trim_end_matches(['\n', '\r']));
        }
        out.push(rendered);
    }

    if out.is_empty() {
        out.push(String::new());
    }
    out
}

pub(crate) fn decorate_code_lines(
    lines: Vec<String>,
    available_width: Option<usize>,
    code_is_light: bool,
    code_bg_rgb: (u8, u8, u8),
) -> Vec<String> {
    if lines.is_empty() {
        return vec![decorate_preview_code_line(
            "",
            1,
            code_is_light,
            code_bg_rgb,
        )];
    }
    let wrapped = wrap_highlighted_lines(lines, available_width);
    let code_width = wrapped
        .iter()
        .map(|line| display_width_ansi(line))
        .max()
        .unwrap_or(0);
    wrapped
        .into_iter()
        .map(|line| decorate_code_line(&line, Some(code_width), code_is_light, code_bg_rgb))
        .collect()
}

pub(crate) fn decorate_preview_code_line(
    line: &str,
    _line_no: usize,
    code_is_light: bool,
    code_bg_rgb: (u8, u8, u8),
) -> String {
    decorate_code_line(line, None, code_is_light, code_bg_rgb)
}

fn decorate_code_line(
    line: &str,
    fixed_code_width: Option<usize>,
    _code_is_light: bool,
    code_bg_rgb: (u8, u8, u8),
) -> String {
    let visible_width = display_width_ansi(line);
    let target_width = fixed_code_width.unwrap_or(visible_width);
    let fill = target_width.saturating_sub(visible_width);
    format!(
        "\x1b[48;2;{};{};{}m{}{padding}\x1b[49m",
        code_bg_rgb.0,
        code_bg_rgb.1,
        code_bg_rgb.2,
        line,
        padding = " ".repeat(fill)
    )
}

fn wrap_highlighted_lines(lines: Vec<String>, available_width: Option<usize>) -> Vec<String> {
    let Some(available_width) = available_width else {
        return lines;
    };
    let code_width = available_width.max(1);
    let mut out = Vec::new();
    for line in lines {
        out.extend(wrap_ansi_line_by_width(&line, code_width));
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn wrap_ansi_line_by_width(line: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![line.to_string()];
    }
    if display_width_ansi(line) <= max_width {
        return vec![line.to_string()];
    }

    let mut out = Vec::new();
    let mut chunk = String::new();
    let mut chunk_width = 0usize;
    let mut i = 0usize;
    let mut active_sgr = String::from("\x1b[39m");

    while i < line.len() {
        if let Some(end) = take_sgr_escape_end(line, i) {
            let esc = &line[i..end];
            chunk.push_str(esc);
            if esc.ends_with('m') {
                active_sgr.clear();
                active_sgr.push_str(esc);
            }
            i = end;
            continue;
        }

        let Some(ch) = line[i..].chars().next() else {
            break;
        };
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if chunk_width + w > max_width && chunk_width > 0 {
            chunk.push_str("\x1b[39m");
            out.push(std::mem::take(&mut chunk));
            chunk_width = 0;
            if !active_sgr.is_empty() {
                chunk.push_str(&active_sgr);
            }
        }
        chunk.push(ch);
        chunk_width = chunk_width.saturating_add(w);
        i += ch.len_utf8();
    }

    if !chunk.is_empty() {
        chunk.push_str("\x1b[39m");
        out.push(chunk);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn take_sgr_escape_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if start + 2 >= bytes.len() || bytes[start] != 0x1B || bytes[start + 1] != b'[' {
        return None;
    }
    let mut i = start + 2;
    while i < bytes.len() {
        if bytes[i] == b'm' {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

fn display_width_ansi(s: &str) -> usize {
    UnicodeWidthStr::width(strip_ansi_sgr(s).as_str())
}

fn strip_ansi_sgr(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            let _ = chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }

    out
}

pub(crate) fn supported_languages() -> Vec<String> {
    let mut langs: Vec<String> = syntax_set()
        .syntaxes()
        .iter()
        .map(|s| s.name.clone())
        .collect();
    langs.sort();
    langs.dedup();
    langs
}
