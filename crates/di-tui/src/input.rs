//! A single-line text editor (search box, label and notes prompts).

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineEdit {
    text: String,
    /// Cursor position in chars.
    cursor: usize,
}

impl LineEdit {
    pub fn with(text: &str) -> LineEdit {
        LineEdit { text: text.to_string(), cursor: text.chars().count() }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = text.chars().count();
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.text.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(self.text.len())
    }

    /// Apply an editing key. Returns true if the text changed.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        let len = self.text.chars().count();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char('u') if ctrl => {
                let b = self.byte_at(self.cursor);
                self.text.replace_range(..b, "");
                self.cursor = 0;
                return true;
            }
            KeyCode::Char('k') if ctrl => {
                let b = self.byte_at(self.cursor);
                self.text.truncate(b);
                return true;
            }
            KeyCode::Char('w') if ctrl => {
                // Delete the word before the cursor.
                let chars: Vec<char> = self.text.chars().collect();
                let mut start = self.cursor;
                while start > 0 && chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && chars[start - 1] != ' ' {
                    start -= 1;
                }
                let (a, b) = (self.byte_at(start), self.byte_at(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor = start;
                return true;
            }
            KeyCode::Char(c) if !ctrl => {
                let b = self.byte_at(self.cursor);
                self.text.insert(b, c);
                self.cursor += 1;
                return true;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let (a, b) = (self.byte_at(self.cursor - 1), self.byte_at(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor -= 1;
                return true;
            }
            KeyCode::Delete if self.cursor < len => {
                let (a, b) = (self.byte_at(self.cursor), self.byte_at(self.cursor + 1));
                self.text.replace_range(a..b, "");
                return true;
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            _ => {}
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn editing() {
        let mut e = LineEdit::default();
        for c in "ext:cr2 iceland".chars() {
            e.handle(key(KeyCode::Char(c)));
        }
        assert_eq!(e.text(), "ext:cr2 iceland");
        e.handle(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(e.text(), "ext:cr2 ");
        e.handle(key(KeyCode::Home));
        e.handle(key(KeyCode::Delete));
        assert_eq!(e.text(), "xt:cr2 ");
        e.handle(key(KeyCode::Char('é')));
        e.handle(key(KeyCode::End));
        e.handle(key(KeyCode::Backspace));
        assert_eq!(e.text(), "éxt:cr2");
        assert_eq!(e.cursor(), 7);
    }
}
