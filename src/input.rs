//! The single-line text editor behind the `/`, `:`, and `|` prompts.
//!
//! [`TextInput`] tracks the typed text plus a caret so the prompts support real
//! in-place editing (arrow keys, Home/End, delete-forward, word/line kill)
//! instead of an append-only "type at the end, backspace off the end". It's a
//! plain model — [`TextInput::edit`] folds one key into the buffer and reports
//! whether the text changed; the caller owns focus, submit, and cancel.

use ratatui::crossterm::event::KeyCode;

/// A single-line editable text field — the model behind the `/`, `:`, and `|`
/// prompts. It tracks a caret so the prompts support real in-place editing
/// (arrow keys, Home/End, delete-forward, word/line kill) instead of the old
/// append-only "type at the end, backspace off the end".
#[derive(Default)]
pub struct TextInput {
    pub text: String,
    /// Caret as a byte offset into `text`, always on a char boundary and in
    /// `0..=text.len()`; equal to `text.len()` at the end of the line.
    pub caret: usize,
}

impl TextInput {
    pub fn as_str(&self) -> &str {
        &self.text
    }
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
    pub fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
    }
    /// Insert a char at the caret and step over it.
    fn insert(&mut self, c: char) {
        self.text.insert(self.caret, c);
        self.caret += c.len_utf8();
    }
    /// Delete the char before the caret (Backspace); no-op at the start.
    fn backspace(&mut self) {
        if let Some(c) = self.text[..self.caret].chars().next_back() {
            self.caret -= c.len_utf8();
            self.text.remove(self.caret);
        }
    }
    /// Delete the char at the caret (Delete); no-op at the end.
    fn delete(&mut self) {
        if self.caret < self.text.len() {
            self.text.remove(self.caret);
        }
    }
    fn left(&mut self) {
        if let Some(c) = self.text[..self.caret].chars().next_back() {
            self.caret -= c.len_utf8();
        }
    }
    fn right(&mut self) {
        if let Some(c) = self.text[self.caret..].chars().next() {
            self.caret += c.len_utf8();
        }
    }
    fn home(&mut self) {
        self.caret = 0;
    }
    fn end(&mut self) {
        self.caret = self.text.len();
    }
    /// Delete the word before the caret (Ctrl-W): trailing spaces, then the run
    /// of non-spaces up to the previous boundary.
    fn delete_word(&mut self) {
        let head = self.text[..self.caret].trim_end_matches(' ');
        let cut = head.rfind(' ').map_or(0, |i| i + 1);
        self.text.replace_range(cut..self.caret, "");
        self.caret = cut;
    }
    /// Delete everything before the caret (Ctrl-U).
    fn clear_to_start(&mut self) {
        self.text.replace_range(..self.caret, "");
        self.caret = 0;
    }
    /// Delete everything from the caret to the end (Ctrl-K).
    fn kill_to_end(&mut self) {
        self.text.truncate(self.caret);
    }
    /// Apply one line-editing key. Returns `Some(changed)` when `code` is an
    /// editing/navigation key (`changed` = whether the text was modified; caret
    /// moves count as handled-but-unchanged), or `None` when the key isn't ours
    /// (Enter/Esc/Up/Down — the caller decides those).
    pub fn edit(&mut self, code: KeyCode, ctrl: bool) -> Option<bool> {
        match code {
            KeyCode::Char(c) if ctrl => match c {
                'a' => Some(self.moved(Self::home)),
                'e' => Some(self.moved(Self::end)),
                'b' => Some(self.moved(Self::left)),
                'f' => Some(self.moved(Self::right)),
                'h' => Some(self.changed(Self::backspace)),
                'd' => Some(self.changed(Self::delete)),
                'w' => Some(self.changed(Self::delete_word)),
                'u' => Some(self.changed(Self::clear_to_start)),
                'k' => Some(self.changed(Self::kill_to_end)),
                _ => None,
            },
            KeyCode::Char(c) => {
                self.insert(c);
                Some(true)
            }
            KeyCode::Backspace => Some(self.changed(Self::backspace)),
            KeyCode::Delete => Some(self.changed(Self::delete)),
            KeyCode::Left => Some(self.moved(Self::left)),
            KeyCode::Right => Some(self.moved(Self::right)),
            KeyCode::Home => Some(self.moved(Self::home)),
            KeyCode::End => Some(self.moved(Self::end)),
            _ => None,
        }
    }
    /// Run a caret move; always reports "unchanged" (text is untouched).
    fn moved(&mut self, f: impl FnOnce(&mut Self)) -> bool {
        f(self);
        false
    }
    /// Run an edit and report whether it actually changed the text.
    fn changed(&mut self, f: impl FnOnce(&mut Self)) -> bool {
        let before = self.text.len();
        f(self);
        self.text.len() != before
    }
}
