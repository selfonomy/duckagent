use super::code;
use super::render::MarkdownRenderer;
use super::table;

#[derive(Default, Clone)]
struct PredictState {
    in_code_block: bool,
    table_likely: bool,
    code_lang: Option<String>,
    code_line_no: usize,
}

pub(crate) struct MarkdownStream {
    buffer: String,
    committed_lines: usize,
    width: usize,
    code_is_light: bool,
    code_bg_rgb: (u8, u8, u8),
    base_fg_rgb: (u8, u8, u8),
    predict_state: PredictState,
}

impl MarkdownStream {
    pub(crate) fn new(
        width: usize,
        code_is_light: bool,
        code_bg_rgb: (u8, u8, u8),
        base_fg_rgb: (u8, u8, u8),
    ) -> Self {
        Self {
            buffer: String::new(),
            committed_lines: 0,
            width: width.max(1),
            code_is_light,
            code_bg_rgb,
            base_fg_rgb,
            predict_state: PredictState::default(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.buffer.clear();
        self.committed_lines = 0;
        self.predict_state = PredictState::default();
    }

    pub(crate) fn push_delta(&mut self, delta: &str) {
        self.buffer.push_str(delta);
    }

    pub(crate) fn commit_complete_lines(&mut self) -> Vec<String> {
        let Some(last_nl) = self.buffer.rfind('\n') else {
            return Vec::new();
        };
        let source = &self.buffer[..=last_nl];
        let defer_bytes = trailing_deferred_table_bytes(source);
        let committed_end = source.len().saturating_sub(defer_bytes);
        if committed_end == 0 {
            return Vec::new();
        }
        let committed_source = &source[..committed_end];
        self.predict_state = scan_predict_state(committed_source);

        let rendered = MarkdownRenderer::new(
            self.width,
            self.code_is_light,
            self.code_bg_rgb,
            self.base_fg_rgb,
        )
        .render_lines(committed_source);
        let rendered_len = rendered.len();
        let out = rendered;
        self.committed_lines = rendered_len;
        out
    }

    pub(crate) fn preview_incomplete_line(&self) -> Option<String> {
        let tail = self
            .buffer
            .rsplit_once('\n')
            .map_or(self.buffer.as_str(), |(_, t)| t);
        if tail.is_empty() {
            return None;
        }
        if self.predict_state.in_code_block {
            let highlighted = code::highlight_code_block(
                tail,
                self.predict_state.code_lang.as_deref(),
                self.code_is_light,
                self.code_bg_rgb,
            );
            let line = highlighted.into_iter().next().unwrap_or_default();
            return Some(code::decorate_preview_code_line(
                &line,
                self.predict_state.code_line_no.saturating_add(1),
                self.code_is_light,
                self.code_bg_rgb,
            ));
        }
        let trimmed = tail.trim_start();
        let table_candidate = trimmed.starts_with('|')
            || table::looks_like_table_separator_line(trimmed)
            || trimmed.matches('|').count() >= 2
            || (self.predict_state.table_likely && trimmed.contains('|'));
        if table_candidate {
            // For table-like incomplete lines, avoid speculative rendering to prevent
            // transient raw/duplicated rows during stream-to-final reconciliation.
            // We render only after the line is committed.
            return None;
        }
        self.render_incomplete_markdown_preview(tail)
    }

    pub(crate) fn finalize_and_drain(&mut self) -> Vec<String> {
        let mut src = self.buffer.clone();
        if !src.ends_with('\n') {
            src.push('\n');
        }
        let rendered = MarkdownRenderer::new(
            self.width,
            self.code_is_light,
            self.code_bg_rgb,
            self.base_fg_rgb,
        )
        .render_lines(&src);
        let out = if self.committed_lines >= rendered.len() {
            Vec::new()
        } else {
            rendered[self.committed_lines..].to_vec()
        };
        out
    }

    fn render_incomplete_markdown_preview(&self, tail: &str) -> Option<String> {
        let mut fragment = tail.to_string();
        fragment.push('\n');
        let rendered = MarkdownRenderer::new(
            self.width,
            self.code_is_light,
            self.code_bg_rgb,
            self.base_fg_rgb,
        )
        .render_lines(&fragment);

        if let Some(line) = rendered.into_iter().find(|line| !line.is_empty()) {
            return Some(line);
        }

        if tail.trim_start().starts_with('>') {
            // Keep quote area visible for an unfinished bare quote marker.
            return Some(super::BLOCKQUOTE_MARKER.to_string());
        }
        None
    }
}

fn trailing_deferred_table_bytes(source: &str) -> usize {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    if lines.is_empty() {
        return 0;
    }

    let mut block_start = 0usize;
    for (idx, line) in lines.iter().enumerate().rev() {
        if line.trim().is_empty() {
            block_start = idx.saturating_add(1);
            break;
        }
    }
    let block = &lines[block_start..];
    if block.is_empty() {
        return 0;
    }

    let has_confirmed_table = block.windows(2).any(|w| {
        table::looks_like_table_row(w[0].trim())
            && table::looks_like_table_separator_line(w[1].trim())
    });
    if has_confirmed_table {
        return 0;
    }

    let Some(last_non_empty_idx) = block.iter().rposition(|line| !line.trim().is_empty()) else {
        return 0;
    };
    let last_non_empty = block[last_non_empty_idx];
    if table::looks_like_table_row(last_non_empty.trim()) {
        return last_non_empty.len();
    }
    0
}

fn scan_predict_state(source: &str) -> PredictState {
    let mut state = PredictState::default();
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            if !state.in_code_block {
                state.code_lang = parse_code_fence_lang(trimmed);
                state.code_line_no = 0;
            } else {
                state.code_lang = None;
                state.code_line_no = 0;
            }
            state.in_code_block = !state.in_code_block;
            continue;
        }
        if state.in_code_block {
            state.code_line_no = state.code_line_no.saturating_add(1);
            continue;
        }
        state.table_likely = table::looks_like_table_row(trimmed);
    }
    state
}

fn parse_code_fence_lang(fence_line: &str) -> Option<String> {
    let rest = fence_line.trim_start_matches('`').trim();
    let lang = rest.split([',', ' ', '\t']).next().unwrap_or("").trim();
    if lang.is_empty() {
        None
    } else {
        Some(lang.to_string())
    }
}
