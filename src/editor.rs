//! Minimal multi-line text editor with soft wrapping, used by the comment composer.

use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone)]
pub struct Editor {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    /// First visual row shown (kept in sync by `layout`).
    pub scroll: usize,
}

/// One visual (wrapped) row: which logical line it belongs to and its char range.
#[derive(Debug, Clone, Copy)]
pub struct VisualRow {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

impl Default for Editor {
    fn default() -> Self {
        Self { lines: vec![Vec::new()], row: 0, col: 0, scroll: 0 }
    }
}

impl Editor {
    pub fn text(&self) -> String {
        self.lines.iter().map(|l| l.iter().collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    pub fn is_blank(&self) -> bool {
        self.lines.iter().all(|l| l.iter().all(|c| c.is_whitespace()))
    }

    pub fn insert_char(&mut self, c: char) {
        if c == '\n' {
            return self.newline();
        }
        self.lines[self.row].insert(self.col, c);
        self.col += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars().filter(|c| *c != '\r') {
            self.insert_char(c);
        }
    }

    pub fn newline(&mut self) {
        let rest = self.lines[self.row].split_off(self.col);
        self.row += 1;
        self.lines.insert(self.row, rest);
        self.col = 0;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            self.lines[self.row].remove(self.col);
        } else if self.row > 0 {
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.lines[self.row].extend(cur);
        }
    }

    pub fn delete(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.lines[self.row].remove(self.col);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].extend(next);
        }
    }

    /// Delete the word before the cursor (Ctrl+W).
    pub fn delete_word(&mut self) {
        let line = &self.lines[self.row];
        let mut start = self.col;
        while start > 0 && line[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !line[start - 1].is_whitespace() {
            start -= 1;
        }
        if start == self.col {
            return self.backspace();
        }
        self.lines[self.row].drain(start..self.col);
        self.col = start;
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
    }

    pub fn right(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].len());
        }
    }

    pub fn down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].len());
        }
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.lines[self.row].len();
    }

    /// If the cursor sits right after `@partial`, return (start col of '@', partial).
    pub fn mention_prefix(&self) -> Option<(usize, String)> {
        let line = &self.lines[self.row];
        let mut i = self.col;
        while i > 0 && is_login_char(line[i - 1]) {
            i -= 1;
        }
        if i == 0 || line[i - 1] != '@' {
            return None;
        }
        // '@' must start a word, so emails like a@b don't trigger it.
        if i >= 2 && !line[i - 2].is_whitespace() && !"([{".contains(line[i - 2]) {
            return None;
        }
        Some((i - 1, line[i..self.col].iter().collect()))
    }

    /// Replace the `@partial` at `start` (up to the cursor) with `@login `.
    pub fn complete_mention(&mut self, start: usize, login: &str) {
        let replacement: Vec<char> = format!("@{login} ").chars().collect();
        let len = replacement.len();
        self.lines[self.row].splice(start..self.col, replacement);
        self.col = start + len;
    }

    /// Compute wrapped visual rows for the given width.
    pub fn visual_rows(&self, width: usize) -> Vec<VisualRow> {
        let width = width.max(1);
        let mut rows = Vec::new();
        for (li, line) in self.lines.iter().enumerate() {
            let mut start = 0;
            let mut w = 0;
            for (ci, c) in line.iter().enumerate() {
                let cw = c.width().unwrap_or(0);
                if w + cw > width && ci > start {
                    rows.push(VisualRow { line: li, start, end: ci });
                    start = ci;
                    w = 0;
                }
                w += cw;
            }
            rows.push(VisualRow { line: li, start, end: line.len() });
        }
        rows
    }

    /// Visual (row, x) of the cursor for the given rows.
    pub fn cursor_visual(&self, rows: &[VisualRow]) -> (usize, usize) {
        for (i, r) in rows.iter().enumerate() {
            let last_of_line = rows.get(i + 1).is_none_or(|n| n.line != r.line);
            if r.line == self.row && self.col >= r.start && (self.col < r.end || last_of_line) {
                let x = self.lines[r.line][r.start..self.col].iter().map(|c| c.width().unwrap_or(0)).sum();
                return (i, x);
            }
        }
        (0, 0)
    }

    /// Adjust scroll so the cursor is visible in a viewport of `height` rows.
    pub fn ensure_visible(&mut self, rows: &[VisualRow], height: usize) {
        let (cy, _) = self.cursor_visual(rows);
        if cy < self.scroll {
            self.scroll = cy;
        } else if height > 0 && cy >= self.scroll + height {
            self.scroll = cy + 1 - height;
        }
    }

    pub fn row_text(&self, r: &VisualRow) -> String {
        self.lines[r.line][r.start..r.end].iter().collect()
    }

    /// Move the cursor to a clicked visual position.
    pub fn click(&mut self, rows: &[VisualRow], vrow: usize, x: usize) {
        let Some(r) = rows.get(vrow.min(rows.len().saturating_sub(1))) else { return };
        let mut col = r.start;
        let mut w = 0;
        while col < r.end {
            let cw = self.lines[r.line][col].width().unwrap_or(0);
            if w + cw > x {
                break;
            }
            w += cw;
            col += 1;
        }
        self.row = r.line;
        self.col = col;
    }
}

pub fn is_login_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '/' || c == '[' || c == ']'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mention_detection_and_completion() {
        let mut e = Editor::default();
        e.insert_str("hey @cla");
        let (start, p) = e.mention_prefix().unwrap();
        assert_eq!(p, "cla");
        e.complete_mention(start, "claude");
        assert_eq!(e.text(), "hey @claude ");
        assert!(e.mention_prefix().is_none());
    }

    #[test]
    fn email_is_not_a_mention() {
        let mut e = Editor::default();
        e.insert_str("a@b");
        assert!(e.mention_prefix().is_none());
    }

    #[test]
    fn wrap_and_cursor() {
        let mut e = Editor::default();
        e.insert_str("abcdefgh\nxy");
        let rows = e.visual_rows(3);
        assert_eq!(rows.len(), 4);
        assert_eq!(e.cursor_visual(&rows), (3, 2));
        e.click(&rows, 1, 1);
        e.insert_char('Z');
        assert_eq!(e.text(), "abcdZefgh\nxy");
    }

    #[test]
    fn backspace_joins_lines() {
        let mut e = Editor::default();
        e.insert_str("ab\ncd");
        e.home();
        e.backspace();
        assert_eq!(e.text(), "abcd");
    }
}
