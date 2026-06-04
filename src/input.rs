use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone)]
pub(crate) struct InputState {
    pub(crate) text: String,
    pub(crate) cursor: usize,
    pub(crate) selection_anchor: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InputLayout {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl InputState {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            selection_anchor: None,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.selection_anchor = None;
    }

    pub(crate) fn selection_range(&self) -> Option<std::ops::Range<usize>> {
        let anchor = self.selection_anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some(anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    pub(crate) fn insert_char(&mut self, ch: char) {
        let mut buf = [0; 4];
        self.insert_str(ch.encode_utf8(&mut buf));
    }

    pub(crate) fn insert_str(&mut self, text: &str) {
        self.delete_selection();
        let byte_idx = byte_index_for_char(&self.text, self.cursor);
        self.text.insert_str(byte_idx, text);
        self.cursor += text.chars().count();
        self.selection_anchor = None;
    }

    pub(crate) fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let start = byte_index_for_char(&self.text, self.cursor - 1);
        let end = byte_index_for_char(&self.text, self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub(crate) fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let char_count = self.text.chars().count();
        if self.cursor >= char_count {
            return;
        }
        let start = byte_index_for_char(&self.text, self.cursor);
        let end = byte_index_for_char(&self.text, self.cursor + 1);
        self.text.replace_range(start..end, "");
    }

    pub(crate) fn move_left(&mut self, extend_selection: bool) {
        if !extend_selection {
            if let Some(range) = self.selection_range() {
                self.cursor = range.start;
                self.selection_anchor = None;
                return;
            }
            self.selection_anchor = None;
        } else if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        }

        if self.cursor > 0 {
            self.cursor -= 1;
        }
        self.cleanup_selection();
    }

    pub(crate) fn move_right(&mut self, extend_selection: bool) {
        if !extend_selection {
            if let Some(range) = self.selection_range() {
                self.cursor = range.end;
                self.selection_anchor = None;
                return;
            }
            self.selection_anchor = None;
        } else if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        }

        let char_count = self.text.chars().count();
        if self.cursor < char_count {
            self.cursor += 1;
        }
        self.cleanup_selection();
    }

    pub(crate) fn visible_layout(&self, available_width: usize) -> InputLayout {
        let char_count = self.text.chars().count();
        if available_width == 0 || char_count == 0 {
            return InputLayout {
                start: self.cursor.min(char_count),
                end: self.cursor.min(char_count),
            };
        }

        let widths: Vec<usize> = self.text.chars().map(char_display_width).collect();
        let selection = self.selection_range();

        let (mut start, mut end, mut used) = if let Some(range) = selection.clone() {
            let selection_width = widths[range.start..range.end].iter().sum();
            if selection_width <= available_width {
                (range.start, range.end, selection_width)
            } else {
                (self.cursor, self.cursor, 0)
            }
        } else {
            (self.cursor, self.cursor, 0)
        };

        let prefer_right = selection
            .as_ref()
            .is_some_and(|range| self.cursor == range.start);

        if prefer_right {
            while end < char_count && used + widths[end] <= available_width {
                used += widths[end];
                end += 1;
            }
            while start > 0 && used + widths[start - 1] <= available_width {
                used += widths[start - 1];
                start -= 1;
            }
        } else {
            while start > 0 && used + widths[start - 1] <= available_width {
                used += widths[start - 1];
                start -= 1;
            }
            while end < char_count && used + widths[end] <= available_width {
                used += widths[end];
                end += 1;
            }
        }

        InputLayout { start, end }
    }

    fn delete_selection(&mut self) -> bool {
        let Some(range) = self.selection_range() else {
            return false;
        };
        let start = byte_index_for_char(&self.text, range.start);
        let end = byte_index_for_char(&self.text, range.end);
        self.text.replace_range(start..end, "");
        self.cursor = range.start;
        self.selection_anchor = None;
        true
    }

    fn cleanup_selection(&mut self) {
        if self.selection_anchor == Some(self.cursor) {
            self.selection_anchor = None;
        }
    }
}

fn byte_index_for_char(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

fn char_display_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_backspace_handles_multibyte_character() {
        let mut input = InputState::new();
        input.insert_str("abé");
        input.backspace();
        assert_eq!(input.text, "ab");
        assert_eq!(input.cursor, 2);
    }
}
