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
    /// Replace the whole line and park the caret at its end — used when a
    /// recalled [`History`] entry is loaded into the prompt.
    pub fn set(&mut self, s: String) {
        self.text = s;
        self.caret = self.text.len();
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

/// A session-scoped ring of submitted prompt entries (`:` paths, `|` filters)
/// with an up/down browse cursor, so a long path or filter can be recalled and
/// tweaked instead of retyped. Newest entries sit last; blank lines and an
/// immediate repeat of the newest entry are dropped, and the ring is capped so a
/// long session can't grow it without bound.
#[derive(Default)]
pub struct History {
    entries: Vec<String>,
    /// Browse position while arrowing: `Some(i)` sits on `entries[i]`; `None`
    /// means "not browsing" — parked on the fresh line the user was typing.
    cursor: Option<usize>,
    /// The half-typed line stashed when browsing begins, restored when the user
    /// arrows back down past the newest entry.
    draft: String,
}

impl History {
    const CAP: usize = 200;

    /// Record a just-submitted entry and leave browsing. Blank lines and an
    /// immediate repeat of the newest entry are ignored.
    pub fn record(&mut self, entry: &str) {
        self.cursor = None;
        let entry = entry.trim();
        if entry.is_empty() || self.entries.last().map(String::as_str) == Some(entry) {
            return;
        }
        self.entries.push(entry.to_string());
        if self.entries.len() > Self::CAP {
            self.entries.remove(0);
        }
    }

    /// Leave browsing — call when the prompt (re)opens so the next `↑` starts
    /// from the newest entry again rather than mid-ring.
    pub fn reset(&mut self) {
        self.cursor = None;
    }

    /// Step to the previous (older) entry, stashing `current` as the draft on the
    /// first step. Returns the text to load, or `None` at the oldest entry (or an
    /// empty ring) so the caller leaves the line untouched.
    pub fn prev(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let i = match self.cursor {
            None => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(0) => return None,
            Some(i) => i - 1,
        };
        self.cursor = Some(i);
        Some(self.entries[i].clone())
    }

    /// Step to the next (newer) entry. Past the newest, restore the stashed draft
    /// and stop browsing. `None` when not currently browsing.
    pub fn next(&mut self) -> Option<String> {
        match self.cursor {
            None => None,
            Some(i) if i + 1 < self.entries.len() => {
                self.cursor = Some(i + 1);
                Some(self.entries[i + 1].clone())
            }
            Some(_) => {
                self.cursor = None;
                Some(std::mem::take(&mut self.draft))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::History;

    #[test]
    fn record_skips_blanks_and_consecutive_dupes() {
        let mut h = History::default();
        h.record("a");
        h.record("  ");
        h.record("a"); // dupe of newest — dropped
        h.record("b");
        h.record("a"); // not consecutive — kept
                       // Oldest→newest: a, b, a
        assert_eq!(h.prev(""), Some("a".to_string()));
        assert_eq!(h.prev("a"), Some("b".to_string()));
        assert_eq!(h.prev("b"), Some("a".to_string()));
        assert_eq!(h.prev("a"), None); // at the oldest
    }

    #[test]
    fn browse_up_then_down_restores_draft() {
        let mut h = History::default();
        h.record("first");
        h.record("second");
        assert_eq!(h.prev("draft"), Some("second".to_string()));
        assert_eq!(h.prev("second"), Some("first".to_string()));
        // Down past the newest brings the stashed draft back and stops browsing.
        assert_eq!(h.next(), Some("second".to_string()));
        assert_eq!(h.next(), Some("draft".to_string()));
        assert_eq!(h.next(), None);
    }

    #[test]
    fn empty_ring_and_reset() {
        let mut h = History::default();
        assert_eq!(h.prev("x"), None);
        assert_eq!(h.next(), None);
        h.record("only");
        assert_eq!(h.prev("y"), Some("only".to_string()));
        h.reset(); // reopening the prompt
        assert_eq!(h.prev("z"), Some("only".to_string())); // starts from newest again
    }

    #[test]
    fn cap_bounds_the_ring() {
        let mut h = History::default();
        for i in 0..(History::CAP + 50) {
            h.record(&i.to_string());
        }
        // Newest is CAP+49; walking all the way back reaches the retained oldest.
        let mut steps = 1;
        let mut last = h.prev("").unwrap();
        while let Some(v) = h.prev(&last) {
            last = v;
            steps += 1;
        }
        assert_eq!(steps, History::CAP);
    }
}
