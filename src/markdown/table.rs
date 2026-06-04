use unicode_width::UnicodeWidthStr;

use super::TABLE_MARKER;

const TABLE_BG_RESET_SGR: &str = "\x1b[49m";
const TABLE_TEXT_BOLD_ON_SGR: &str = "\x1b[1m";
const TABLE_TEXT_BOLD_OFF_SGR: &str = "\x1b[22m";
const CELL_VERTICAL_PADDING: usize = 1;

#[derive(Clone, Copy, Default)]
pub(crate) struct TablePalette {
    pub(crate) header_bg_rgb: (u8, u8, u8),
    pub(crate) body_bg_rgb: (u8, u8, u8),
}

impl TablePalette {
    pub(crate) fn from_bubble_bg(ai_bg_rgb: (u8, u8, u8), is_light: bool) -> Self {
        let header_delta = if is_light { -18 } else { 24 };
        let body_delta = if is_light { -10 } else { 14 };
        Self {
            header_bg_rgb: adjust_rgb(ai_bg_rgb, header_delta),
            body_bg_rgb: adjust_rgb(ai_bg_rgb, body_delta),
        }
    }
}

fn adjust_rgb((r, g, b): (u8, u8, u8), delta: i16) -> (u8, u8, u8) {
    let adj = |v: u8| -> u8 { ((v as i16) + delta).clamp(0, 255) as u8 };
    (adj(r), adj(g), adj(b))
}

fn bg_sgr((r, g, b): (u8, u8, u8)) -> String {
    format!("\x1b[48;2;{r};{g};{b}m")
}

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(strip_ansi(s).as_str())
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

fn take_osc_escape_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if start + 2 >= bytes.len() || bytes[start] != 0x1B || bytes[start + 1] != b']' {
        return None;
    }
    let mut i = start + 2;
    while i < bytes.len() {
        if bytes[i] == 0x07 {
            return Some(i + 1);
        }
        if i + 1 < bytes.len() && bytes[i] == 0x1B && bytes[i + 1] == b'\\' {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

fn take_ansi_escape_end(text: &str, start: usize) -> Option<usize> {
    take_sgr_escape_end(text, start).or_else(|| take_osc_escape_end(text, start))
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        if let Some(end) = take_ansi_escape_end(text, i) {
            i = end;
            continue;
        }
        let Some(ch) = text[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn char_display_width(c: char) -> usize {
    UnicodeWidthStr::width(c.encode_utf8(&mut [0; 4]))
}

fn wrap_ansi_hard_by_display_width(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    if display_width(text) <= width {
        return vec![text.to_string()];
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    let mut i = 0usize;
    while i < text.len() {
        if let Some(end) = take_ansi_escape_end(text, i) {
            current.push_str(&text[i..end]);
            i = end;
            continue;
        }
        let Some(ch) = text[i..].chars().next() else {
            break;
        };
        let ch_width = char_display_width(ch);
        if current_width + ch_width > width && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width = current_width.saturating_add(ch_width);
        i += ch.len_utf8();
    }

    if current.is_empty() {
        out.push(String::new());
    } else {
        out.push(current);
    }
    out
}

pub(crate) fn looks_like_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('>') || trimmed.starts_with("```") {
        return false;
    }
    trimmed.matches('|').count() >= 2
}

pub(crate) fn looks_like_table_separator_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let cells: Vec<String> = trimmed
        .trim_matches('|')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect();
    if cells.is_empty() {
        return false;
    }
    is_separator_row(&cells)
}

fn is_separator_row(cells: &[String]) -> bool {
    cells.iter().all(|cell| {
        let t = cell.trim();
        !t.is_empty() && t.chars().all(|c| c == '-' || c == ':' || c == ' ')
    })
}

pub(crate) fn render_table(
    rows: &[Vec<String>],
    max_width: usize,
    palette: TablePalette,
) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let col_count = rows.iter().map(Vec::len).max().unwrap_or(0);
    if col_count == 0 {
        return Vec::new();
    }

    let mut normalized = Vec::new();
    for row in rows {
        let mut r = row.clone();
        while r.len() < col_count {
            r.push(String::new());
        }
        if is_separator_row(&r) {
            continue;
        }
        normalized.push(r);
    }
    if normalized.is_empty() {
        return Vec::new();
    }

    let mut widths = vec![0usize; col_count];
    for row in &normalized {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(display_width(cell));
        }
    }

    let mut table_width = widths.iter().sum::<usize>() + col_count.saturating_mul(2);
    let max_width = max_width.max(1);
    if table_width > max_width {
        let min_col = 3usize;
        while table_width > max_width {
            let Some((idx, _)) = widths
                .iter()
                .enumerate()
                .filter(|(_, w)| **w > min_col)
                .max_by_key(|(_, w)| **w)
            else {
                break;
            };
            widths[idx] = widths[idx].saturating_sub(1);
            table_width = widths.iter().sum::<usize>() + col_count.saturating_mul(2);
        }
    }

    let header_bg = bg_sgr(palette.header_bg_rgb);
    let body_bg = bg_sgr(palette.body_bg_rgb);

    let render_band_row = |cell_bg: &str| -> String {
        widths
            .iter()
            .map(|w| {
                format!(
                    "{cell_bg}{}{TABLE_BG_RESET_SGR}",
                    " ".repeat(w.saturating_add(2))
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };

    let render_data_row = |row: &[String],
                           cell_bg: &str,
                           bold_text: bool,
                           with_vertical_padding: bool|
     -> Vec<String> {
        let wrapped_cells: Vec<Vec<String>> = widths
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let cell = row.get(i).cloned().unwrap_or_default();
                wrap_ansi_hard_by_display_width(&cell, *w)
            })
            .collect();
        let row_height = wrapped_cells.iter().map(Vec::len).max().unwrap_or(1);
        let mut lines = Vec::with_capacity(
            row_height
                + if with_vertical_padding {
                    CELL_VERTICAL_PADDING * 2
                } else {
                    0
                },
        );
        let empty_cells = widths
            .iter()
            .map(|w| {
                format!(
                    "{cell_bg}{}{TABLE_BG_RESET_SGR}",
                    " ".repeat(w.saturating_add(2))
                )
            })
            .collect::<Vec<_>>()
            .join("");
        if with_vertical_padding {
            for _ in 0..CELL_VERTICAL_PADDING {
                lines.push(empty_cells.clone());
            }
        }
        for line_idx in 0..row_height {
            let rendered = widths
                .iter()
                .enumerate()
                .map(|(i, w)| {
                    let cell_line = wrapped_cells
                        .get(i)
                        .and_then(|c| c.get(line_idx))
                        .cloned()
                        .unwrap_or_default();
                    let fill = w.saturating_sub(display_width(&cell_line));
                    if bold_text {
                        format!(
                            "{cell_bg} {TABLE_TEXT_BOLD_ON_SGR}{}{} {TABLE_TEXT_BOLD_OFF_SGR}{TABLE_BG_RESET_SGR}",
                            cell_line,
                            " ".repeat(fill)
                        )
                    } else {
                        format!("{cell_bg} {}{} {TABLE_BG_RESET_SGR}", cell_line, " ".repeat(fill))
                    }
                })
                .collect::<Vec<_>>()
                .join("");
            lines.push(rendered);
        }
        if with_vertical_padding {
            for _ in 0..CELL_VERTICAL_PADDING {
                lines.push(empty_cells.clone());
            }
        }
        lines
    };

    let mut out = Vec::new();
    out.push(render_band_row(&header_bg));
    out.extend(render_data_row(&normalized[0], &header_bg, true, false));
    if normalized.len() > 1 {
        out.push(render_band_row(&header_bg));
    }
    for row in normalized.iter().skip(1) {
        out.extend(render_data_row(row, &body_bg, false, true));
    }
    for line in &mut out {
        line.insert_str(0, TABLE_MARKER);
    }
    out
}
