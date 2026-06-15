//! Byte-range JSON scanner — the Rust port of `src/object-tree/json/scanner.ts`.
//!
//! Operates on raw UTF-8 bytes (`&[u8]`), never on a decoded string. Structural
//! tokens are all ASCII single bytes and every byte of a multi-byte UTF-8
//! sequence is >= 0x80, so the scan never decodes — only `decode_str`
//! (materializing an actual key/value) decodes, and only the range it touches.
//! That's what lets a memory-mapped 1 GB file be browsed without paging it all
//! in or building a UTF-16 copy.

/// Recursion-depth cap for the (recursive) search scan and flatten/expand walks.
/// JSON has no nesting limit, so a hostile `[[[…]]]` document could otherwise
/// drive those recursions past the stack guard page and abort the process. Real
/// data is rarely deeper than a few dozen levels; 1000 is far beyond that and
/// far under any stack we run on (the search worker also gets a 32 MiB stack).
pub const MAX_DEPTH: usize = 1000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Object,
    Array,
    Str,
    Number,
    Bool,
    Null,
}

#[inline]
pub fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

#[inline]
pub fn skip_ws(b: &[u8], mut i: usize, end: usize) -> usize {
    while i < end && is_ws(b[i]) {
        i += 1;
    }
    i
}

/// `i` points at the opening quote; returns the index just past the closing
/// quote. Never returns past `end`: a trailing backslash (`"\` at the buffer
/// edge) would otherwise make the `+= 2` overshoot, and callers treat the result
/// as a byte range — an end past the buffer reads out of bounds later.
pub fn skip_string(b: &[u8], mut i: usize, end: usize) -> usize {
    i += 1; // past opening quote
    while i < end {
        match b[i] {
            b'\\' => i += 2, // escaped char (incl. \" and \uXXXX) — never a closer
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    i.min(end) // clamp a trailing-backslash overshoot back to the buffer end
}

/// `i` points at `{` or `[`; returns the index just past the matching closer,
/// honoring strings (so brackets inside strings don't count).
pub fn skip_container(b: &[u8], mut i: usize, end: usize) -> usize {
    let mut depth = 0i32;
    while i < end {
        match b[i] {
            b'"' => {
                i = skip_string(b, i, end);
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    i
}

/// Skip a whole value starting at/after `i`; returns the index just past it.
pub fn skip_value(b: &[u8], i: usize, end: usize) -> usize {
    let i = skip_ws(b, i, end);
    if i >= end {
        return i;
    }
    match b[i] {
        b'"' => skip_string(b, i, end),
        b'{' | b'[' => skip_container(b, i, end),
        _ => {
            // primitive: scan to the next structural delimiter or whitespace
            let mut j = i;
            while j < end && !matches!(b[j], b',' | b'}' | b']') && !is_ws(b[j]) {
                j += 1;
            }
            j
        }
    }
}

pub fn value_kind(b: &[u8], i: usize) -> Kind {
    match b[i] {
        b'{' => Kind::Object,
        b'[' => Kind::Array,
        b'"' => Kind::Str,
        b't' | b'f' => Kind::Bool,
        b'n' => Kind::Null,
        _ => Kind::Number,
    }
}

/// True if a container `{`/`[` at `start` has no children (so it's not expandable).
pub fn container_empty(b: &[u8], start: usize, end: usize) -> bool {
    let i = skip_ws(b, start + 1, end);
    i >= end || b[i] == b'}' || b[i] == b']'
}

/// Decode a JSON string whose byte range `[start, end)` includes the quotes.
/// Multi-byte UTF-8 is preserved verbatim; only backslash escapes are expanded.
pub fn decode_str(b: &[u8], start: usize, end: usize) -> String {
    if end <= start + 1 {
        return String::new();
    }
    let inner = &b[start + 1..end - 1];
    if !inner.contains(&b'\\') {
        return String::from_utf8_lossy(inner).into_owned();
    }
    let mut out: Vec<u8> = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        let c = inner[i];
        if c == b'\\' && i + 1 < inner.len() {
            match inner[i + 1] {
                b'"' => out.push(b'"'),
                b'\\' => out.push(b'\\'),
                b'/' => out.push(b'/'),
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'r' => out.push(b'\r'),
                b'b' => out.push(8),
                b'f' => out.push(12),
                b'u' => {
                    if i + 6 <= inner.len() {
                        if let Ok(h) = std::str::from_utf8(&inner[i + 2..i + 6]) {
                            if let Ok(cp) = u32::from_str_radix(h, 16) {
                                if let Some(ch) = char::from_u32(cp) {
                                    let mut buf = [0u8; 4];
                                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                                }
                            }
                        }
                        i += 6;
                        continue;
                    }
                    i += 2;
                    continue;
                }
                other => out.push(other),
            }
            i += 2;
        } else {
            out.push(c);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One immediate child of a container: its display label + value byte range.
pub struct RawChild {
    pub label: String,
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
    /// True when the label is an array index (vs. an object key) — used for coloring.
    pub is_index: bool,
    /// False when `end` is *provisional* — a container child whose closer hasn't
    /// been scanned yet (`end` is the parent's bound). The child still renders and
    /// expands correctly (its own cursor stops at the real closer); only raw-range
    /// uses (copy, split) must resolve the true end first. See `Cursor::emit`.
    pub end_exact: bool,
}

/// One outcome of a budgeted [`Cursor::step`]: the next child, a `Yield` (a
/// deferred end-skip is mid-flight — `budget` bytes were consumed, call again to
/// resume), or `Done` (level fully enumerated).
pub enum Step {
    Child(RawChild),
    Yield,
    Done,
}

/// An in-progress skip over a container child's value, kept so a huge value can
/// be stepped over a chunk at a time (resumable) rather than blocking the UI.
/// `i` is the current byte; `depth` the open-bracket nesting (back to 0 = closed).
struct SkipProgress {
    i: usize,
    depth: i32,
}

/// Resumable cursor over a container's immediate children — the port of
/// `scanContainerGen`. Each `next()` scans exactly one more child, so a level is
/// enumerated incrementally (only as far as the viewport scrolls).
pub struct Cursor {
    pos: usize,
    end: usize,
    is_array: bool,
    index: usize,
    done: bool,
    /// NDJSON / JSON-Lines mode: values are whitespace-separated with no
    /// enclosing bracket and no commas (see [`Cursor::lines`]).
    lines: bool,
    /// A deferred container end-skip: the previously emitted child was a
    /// container whose closer we haven't scanned yet. Resolved (eagerly by
    /// `next`, or in chunks by `step`) before the *next* child is produced.
    pending: Option<SkipProgress>,
}

impl Cursor {
    /// `start` points at the container's opening `{`/`[`.
    pub fn new(start: usize, end: usize, is_array: bool) -> Self {
        Cursor {
            pos: start + 1,
            end,
            is_array,
            index: 0,
            done: false,
            lines: false,
            pending: None,
        }
    }

    /// A cursor over a whitespace-separated stream of JSON values — NDJSON /
    /// JSON Lines / concatenated JSON. There is no enclosing bracket; each value
    /// is treated as an array-style element (`is_index`), one per `next()`.
    pub fn lines(start: usize, end: usize) -> Self {
        Cursor {
            pos: start,
            end,
            is_array: true,
            index: 0,
            done: false,
            lines: true,
            pending: None,
        }
    }

    /// Eager enumeration (preview, goto, search): resolve any deferred end-skip
    /// in one shot, then produce the next child. Same observable behaviour as
    /// before lazy ends existed — callers that don't drive a budget see no change.
    pub fn next(&mut self, b: &[u8]) -> Option<RawChild> {
        self.drive_pending(b, usize::MAX);
        self.emit(b)
    }

    /// Cooperative enumeration (windowed flatten): resolve a deferred end-skip in
    /// `budget`-byte chunks. Returns `Yield` while the skip is still in flight so
    /// the caller can repaint and poll input between chunks (the resumable port of
    /// "repaint with throttle"); otherwise the next child, or `Done`.
    pub fn step(&mut self, b: &[u8], budget: usize) -> Step {
        if !self.drive_pending(b, budget) {
            return Step::Yield;
        }
        match self.emit(b) {
            Some(c) => Step::Child(c),
            None => Step::Done,
        }
    }

    /// Finish (or advance by `budget` bytes) a deferred container end-skip.
    /// Returns true once `pos` sits past the value (or there was nothing pending);
    /// false if the budget ran out before the closer (resume on the next call).
    fn drive_pending(&mut self, b: &[u8], budget: usize) -> bool {
        let Some(mut sp) = self.pending.take() else {
            return true;
        };
        let stop = sp.i.saturating_add(budget);
        while sp.i < self.end {
            match b[sp.i] {
                b'"' => {
                    sp.i = skip_string(b, sp.i, self.end);
                    continue; // a string can't hold a structural byte
                }
                b'{' | b'[' => sp.depth += 1,
                b'}' | b']' => {
                    sp.depth -= 1;
                    if sp.depth == 0 {
                        self.pos = sp.i + 1; // past the closer
                        return true;
                    }
                }
                _ => {}
            }
            sp.i += 1;
            if sp.i >= stop {
                self.pending = Some(sp); // budget exhausted mid-value — resume later
                return false;
            }
        }
        // Ran out of buffer without a closer (truncated / malformed): the value
        // is everything that's left. Finish so enumeration can't deadlock.
        self.pos = sp.i;
        true
    }

    /// Produce the next child. A *container* child's end is left **provisional**:
    /// rather than `skip_value` to its closer here (O(value size) — the cost that
    /// makes opening a 1 GB array slow), record a `pending` skip and hand back the
    /// parent's bound as `end`. `drive_pending` resolves it before the next child.
    /// Assumes any prior pending skip is already resolved (see `next`/`step`).
    fn emit(&mut self, b: &[u8]) -> Option<RawChild> {
        if self.done {
            return None;
        }
        let mut i = skip_ws(b, self.pos, self.end);
        if self.lines {
            // No closer to stop at — EOF ends the stream. Each value is one record.
            // Records are line-sized, so skip them eagerly (no deferral here).
            if i >= self.end {
                self.done = true;
                return None;
            }
            let label = self.index.to_string();
            // `.max(i + 1)`: a stray byte that isn't the start of a value (e.g. a
            // `,`/`]` in malformed NDJSON) wouldn't advance `skip_value`, so step
            // over it — the stream must always progress or it loops forever.
            let vend = skip_value(b, i, self.end).max(i + 1);
            let kind = value_kind(b, i);
            self.pos = vend;
            self.index += 1;
            return Some(RawChild {
                label,
                start: i,
                end: vend,
                kind,
                is_index: true,
                end_exact: true,
            });
        }
        if i >= self.end || b[i] == b'}' || b[i] == b']' {
            self.done = true;
            return None;
        }
        if self.index > 0 {
            if b[i] == b',' {
                i = skip_ws(b, i + 1, self.end);
            }
            if i >= self.end || b[i] == b'}' || b[i] == b']' {
                self.done = true;
                return None;
            }
        }

        let label;
        let vstart;
        if self.is_array {
            label = self.index.to_string();
            vstart = i;
        } else {
            let kstart = i;
            let kend = skip_string(b, i, self.end);
            label = decode_str(b, kstart, kend);
            let mut j = skip_ws(b, kend, self.end);
            if j < self.end && b[j] == b':' {
                j += 1;
            }
            vstart = skip_ws(b, j, self.end);
        }
        // The value isn't in the buffer yet (truncated mid-key/colon while
        // streaming an incomplete document). Don't emit a half child — stop here;
        // a later re-scan with more bytes will pick it up.
        if vstart >= self.end {
            self.done = true;
            return None;
        }
        let kind = value_kind(b, vstart);
        self.index += 1;
        if matches!(kind, Kind::Object | Kind::Array) {
            // Defer the end-skip. `pos` stays at the value start; the next
            // `next`/`step` resolves `pending` first. The provisional `end` is the
            // parent's bound — fine for display/expand (the child's own cursor
            // stops at its real closer), flagged `end_exact: false` for copy/split.
            self.pending = Some(SkipProgress {
                i: vstart,
                depth: 0,
            });
            self.pos = vstart;
            Some(RawChild {
                label,
                start: vstart,
                end: self.end,
                kind,
                is_index: self.is_array,
                end_exact: false,
            })
        } else {
            // Scalars are cheap to skip and small — resolve eagerly.
            let vend = skip_value(b, vstart, self.end);
            self.pos = vend;
            Some(RawChild {
                label,
                start: vstart,
                end: vend,
                kind,
                is_index: self.is_array,
                end_exact: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Escapes are expanded: `\"` → `"`, `\\` → `\`, `\n` → newline, `\uXXXX` → char.
    #[test]
    fn decode_str_expands_escapes() {
        let b = br#""a\"b\\c\nA""#; // JSON text for: a"b\c<LF>A
        assert_eq!(decode_str(b, 0, b.len()), "a\"b\\c\nA");
    }

    /// A trailing backslash at the very end of a string must not read past the
    /// buffer (the `i + 1 < len` guard) — it's emitted literally, no panic.
    #[test]
    fn decode_str_trailing_backslash_is_safe() {
        let b = br#""ab\"#; // unterminated: quote, a, b, backslash
        let _ = decode_str(b, 0, b.len()); // must not panic
    }

    /// Structural bytes (`]`, `"`) *inside* a string must not fool the scanner:
    /// the array has exactly two children and the string's real boundary is found.
    #[test]
    fn string_with_structural_chars_does_not_break_enumeration() {
        let b = br#"["x]y\"z", 5]"#;
        let mut cur = Cursor::new(0, b.len(), true);

        let c0 = cur.next(b).expect("first child");
        assert_eq!(c0.kind, Kind::Str);
        assert_eq!(decode_str(b, c0.start, c0.end), "x]y\"z");

        let c1 = cur.next(b).expect("second child");
        assert_eq!(c1.kind, Kind::Number);
        assert_eq!(&b[c1.start..c1.end], b"5");

        assert!(cur.next(b).is_none(), "array has exactly two children");
    }

    /// An unterminated/garbage container must terminate (no infinite loop) and not
    /// index out of bounds.
    #[test]
    fn malformed_input_terminates() {
        for src in [&b"[,,,,"[..], &b"{\"a\":"[..], &b"["[..], &b"{"[..]] {
            let mut cur = Cursor::new(0, src.len(), src[0] == b'[');
            let mut n = 0;
            while cur.next(src).is_some() {
                n += 1;
                assert!(n < 1000, "cursor failed to terminate on {src:?}");
            }
        }
    }

    /// A tiny deterministic PRNG (xorshift64*) so the fuzz tests are reproducible
    /// and need no `rand` dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn byte(&mut self) -> u8 {
            (self.next() & 0xFF) as u8
        }
    }

    /// Throw pseudo-random bytes — biased toward JSON's structural characters so
    /// the scanner actually exercises its branches — at every byte-level entry
    /// point. None may panic, index out of bounds, or fail to terminate. This is
    /// the property the whole multi-GB promise rests on: no input crashes us.
    #[test]
    fn fuzz_scanner_never_panics_or_hangs() {
        // The alphabet the scanner cares about, plus a couple of multi-byte
        // UTF-8 lead/continuation bytes to prove the >= 0x80 assumption holds.
        const ALPHABET: &[u8] = b"{}[]\":,\\ \t\n0123456789.-+eEtfnaul\xc3\xa9\xe2\x82";
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..4000 {
            let len = (rng.next() % 80) as usize;
            let buf: Vec<u8> = (0..len)
                .map(|_| {
                    if rng.byte() < 200 {
                        ALPHABET[(rng.next() as usize) % ALPHABET.len()]
                    } else {
                        rng.byte()
                    }
                })
                .collect();
            let b = &buf[..];

            // Bracketed-container, array, and NDJSON-line cursors all over the
            // whole buffer; each `next()` must make progress and eventually stop.
            for mut cur in [
                Cursor::new(0, b.len(), false),
                Cursor::new(0, b.len(), true),
            ] {
                let mut steps = 0;
                while cur.next(b).is_some() {
                    steps += 1;
                    assert!(steps <= b.len() + 2, "cursor did not terminate");
                }
            }
            let mut lines = Cursor::lines(0, b.len());
            let mut steps = 0;
            while lines.next(b).is_some() {
                steps += 1;
                assert!(steps <= b.len() + 2, "line cursor did not terminate");
            }

            // Range-taking helpers over random sub-ranges (incl. empty / reversed-
            // looking / past-end starts, which callers must never produce but the
            // primitives should still survive).
            for _ in 0..8 {
                let i = (rng.next() as usize) % (b.len() + 1);
                let j = (rng.next() as usize) % (b.len() + 1);
                let (lo, hi) = (i.min(j), i.max(j));
                let _ = skip_ws(b, lo, hi);
                let _ = skip_value(b, lo, hi);
                let _ = skip_string(b, lo, hi);
                let _ = skip_container(b, lo, hi);
                let _ = container_empty(b, lo, hi);
                let _ = decode_str(b, lo, hi);
            }
        }
    }
}
