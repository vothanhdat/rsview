//! Background-thread search — the part that's genuinely simpler in Rust than in
//! the TS port. There, search and the windowed walk share one event loop, so a
//! runaway drain starves the next keystroke (hence pauseWalk + filter-fn
//! cancellation). Here the scan runs on its own OS thread over the same mmap;
//! cancellation is a single `AtomicBool` the thread polls, and matches stream
//! back over an `mpsc` channel. Retyping just drops the old `Search` (its `Drop`
//! flips the flag) and spawns a new one — no starvation, no shared loop.

use crate::scanner::{decode_str, Cursor, Kind, MAX_DEPTH};
use crate::source::Source;
use regex::{Regex, RegexBuilder};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc,
};
use std::thread::{self, JoinHandle};

/// Cap on collected matches — bounds memory and keeps a pathological "every key
/// matches" query from running forever.
const MAX_MATCHES: usize = 5000;

/// What the user typed in the `/` prompt, compiled into something the worker
/// can match against. Three flavours:
/// - `re:<regex>` → full Rust regex, case-insensitive
/// - `g:<glob>`  → `*` (any chars), `?` (one char); everything else literal
/// - anything else → case-insensitive substring (the default since v0.1.0)
pub enum Pattern {
    /// Lowercased needle; match via `hay.to_lowercase().contains(needle)`.
    Literal(String),
    /// Compiled regex (already case-insensitive).
    Regex(Regex),
}

impl Pattern {
    /// Compile the typed query. Returns the parsed pattern, or a footer-ready
    /// error string when `re:`/`g:` was used and the inner expression is bad.
    pub fn parse(s: &str) -> Result<Pattern, String> {
        if let Some(inner) = s.strip_prefix("re:") {
            build_regex(inner).map(Pattern::Regex)
        } else if let Some(inner) = s.strip_prefix("g:") {
            build_regex(&glob_to_regex(inner)).map(Pattern::Regex)
        } else {
            Ok(Pattern::Literal(s.to_lowercase()))
        }
    }

    fn is_match(&self, hay: &str) -> bool {
        match self {
            Pattern::Literal(n) => hay.to_lowercase().contains(n.as_str()),
            Pattern::Regex(r) => r.is_match(hay),
        }
    }
}

fn build_regex(src: &str) -> Result<Regex, String> {
    RegexBuilder::new(src)
        .case_insensitive(true)
        .build()
        .map_err(|e| format!("bad pattern: {e}"))
}

/// Translate a shell-style glob (`*` = any run, `?` = one char) into an
/// unanchored regex. All other regex metacharacters are escaped, so a glob
/// behaves intuitively (`foo.bar` matches the literal dot).
pub fn glob_to_regex(g: &str) -> String {
    let mut out = String::with_capacity(g.len() + 4);
    for c in g.chars() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '\\' | '{' | '}' | '[' | ']' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// One running (or finished) search. Owns the worker thread's cancel flag and
/// the receiving end of its match stream.
pub struct Search {
    cancel: Arc<AtomicBool>,
    rx: Receiver<Vec<usize>>,
    done: Arc<AtomicBool>,
    _handle: JoinHandle<()>,
    /// Match paths drained from `rx` so far (each is an index-path into the tree).
    pub matches: Vec<Vec<usize>>,
    /// True once the worker finished and `rx` is fully drained.
    pub finished: bool,
}

impl Search {
    /// Spawn a worker that scans the subtree rooted at byte range `[start, end)`
    /// for `pattern` (case-insensitive) and streams the path of every matching
    /// node back over a channel. Paths are relative to that root, so a pane
    /// viewing a sub-range gets matches that line up with its own rows. For the
    /// whole document pass the document root's range; for NDJSON, the whole
    /// buffer.
    pub fn spawn(
        mmap: Arc<Source>,
        pattern: Pattern,
        jsonl: bool,
        start: usize,
        end: usize,
        kind: Kind,
    ) -> Search {
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let cancel_w = cancel.clone();
        let done_w = done.clone();

        // A generous stack for the worker: `scan` recurses per nesting level, and
        // while MAX_DEPTH caps that, a roomy stack keeps margin regardless of how
        // big a frame the compiler picks. (Default thread stacks are ~2 MiB.)
        let handle = thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(move || {
                let b: &[u8] = &mmap;
                let mut path = Vec::new();
                let mut counter = 0u64;
                let mut found = 0usize;
                if jsonl {
                    // Each document is element [i]; recurse into it like an array child
                    // so match paths line up with the synthetic NDJSON array root.
                    let mut cur = Cursor::lines(start, end);
                    let mut i = 0usize;
                    while let Some(rc) = cur.next(b) {
                        if cancel_w.load(Ordering::Relaxed) {
                            break;
                        }
                        path.push(i);
                        let bailed = scan(
                            b,
                            rc.start,
                            rc.end,
                            rc.kind,
                            &rc.label,
                            &pattern,
                            1,
                            &mut path,
                            &cancel_w,
                            &tx,
                            &mut counter,
                            &mut found,
                        );
                        path.pop();
                        if bailed {
                            break;
                        }
                        i += 1;
                    }
                } else if start < end {
                    scan(
                        b,
                        start,
                        end,
                        kind,
                        "",
                        &pattern,
                        0,
                        &mut path,
                        &cancel_w,
                        &tx,
                        &mut counter,
                        &mut found,
                    );
                }
                done_w.store(true, Ordering::Relaxed);
            })
            .expect("spawn search worker thread");

        Search {
            cancel,
            rx,
            done,
            _handle: handle,
            matches: Vec::new(),
            finished: false,
        }
    }

    /// Pull newly-found match paths into `matches`. Returns the count of new ones.
    pub fn drain(&mut self) -> usize {
        let before = self.matches.len();
        while let Ok(p) = self.rx.try_recv() {
            self.matches.push(p);
        }
        if self.done.load(Ordering::Relaxed) {
            // The worker set `done` *after* its last send; one more non-blocking
            // sweep guarantees we've seen everything before declaring finished.
            while let Ok(p) = self.rx.try_recv() {
                self.matches.push(p);
            }
            self.finished = true;
        }
        self.matches.len() - before
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        // Dropping a search (e.g. on retype) detaches the thread; the flag makes
        // it bail at its next poll instead of scanning the rest of the file.
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Recursive byte-range scan. Returns `true` if it bailed (cancelled, receiver
/// gone, or match cap hit) so callers stop descending.
#[allow(clippy::too_many_arguments)]
fn scan(
    b: &[u8],
    start: usize,
    end: usize,
    kind: Kind,
    label: &str,
    pattern: &Pattern,
    depth: usize,
    path: &mut Vec<usize>,
    cancel: &AtomicBool,
    tx: &Sender<Vec<usize>>,
    counter: &mut u64,
    found: &mut usize,
) -> bool {
    // Poll the cancel flag every 4096 nodes — cheap, and bounds how long a
    // dropped search keeps faulting pages before it notices.
    *counter += 1;
    if *counter & 0xFFF == 0 && cancel.load(Ordering::Relaxed) {
        return true;
    }
    if *found >= MAX_MATCHES {
        return true;
    }

    // A node matches if its key matches the pattern, or (for a scalar) its
    // value does. Containers match only via their key.
    let mut hit = !label.is_empty() && pattern.is_match(label);
    if !hit {
        match kind {
            Kind::Object | Kind::Array => {}
            Kind::Str => hit = pattern.is_match(&decode_str(b, start, end)),
            _ => {
                if let Ok(s) = std::str::from_utf8(&b[start..end]) {
                    hit = pattern.is_match(s);
                }
            }
        }
    }
    if hit {
        if tx.send(path.clone()).is_err() {
            return true; // receiver dropped — the search was superseded
        }
        *found += 1;
    }

    // Stop descending past the depth cap — a pathologically deep document would
    // otherwise overflow the stack. We still emitted this node's own match above;
    // we just don't recurse into its children. (See MAX_DEPTH.)
    if depth < MAX_DEPTH && matches!(kind, Kind::Object | Kind::Array) {
        let mut cur = Cursor::new(start, end, matches!(kind, Kind::Array));
        let mut i = 0;
        while let Some(rc) = cur.next(b) {
            path.push(i);
            let bailed = scan(
                b,
                rc.start,
                rc.end,
                rc.kind,
                &rc.label,
                pattern,
                depth + 1,
                path,
                cancel,
                tx,
                counter,
                found,
            );
            path.pop();
            if bailed {
                return true;
            }
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pathologically deep document (`[[[…"needle"…]]]`) must not overflow the
    /// stack when searched — before the depth cap this aborted the process. The
    /// match sits below MAX_DEPTH so it won't be found; the point is that the
    /// search *finishes* instead of crashing.
    #[test]
    fn deep_nesting_search_does_not_overflow() {
        let n = 50_000;
        let mut s = String::with_capacity(n * 2 + 16);
        for _ in 0..n {
            s.push('[');
        }
        s.push_str("\"needle\"");
        for _ in 0..n {
            s.push(']');
        }
        let bytes = s.into_bytes();
        let end = bytes.len();
        let src = Arc::new(Source::Buffered(bytes));

        let pat = Pattern::parse("needle").unwrap();
        let mut search = Search::spawn(src, pat, false, 0, end, Kind::Array);
        // Drain until the worker reports done (bounded spin so a hang can't wedge CI).
        for _ in 0..10_000 {
            search.drain();
            if search.finished {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(search.finished, "search did not finish (possible hang)");
    }

    /// End-to-end: a `re:` pattern actually filters the worker's emitted paths.
    /// `^id_\d+$` matches the two `id_42`/`id_99` string values but not the
    /// numeric `id` keys, so we should see exactly the two matches in `meta`.
    #[test]
    fn regex_pattern_filters_string_values() {
        let bytes =
            br#"{"items":[{"id":1,"tag":"id_42"},{"id":2,"tag":"id_99"},{"id":3,"tag":"x"}]}"#
                .to_vec();
        let end = bytes.len();
        let src = Arc::new(Source::Buffered(bytes));
        let pat = Pattern::parse(r"re:^id_\d+$").expect("regex");
        let mut search = Search::spawn(src, pat, false, 0, end, Kind::Object);
        for _ in 0..10_000 {
            search.drain();
            if search.finished {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(search.finished);
        // Each match's last index is the `tag` field (index 1 inside its object).
        assert_eq!(search.matches.len(), 2, "matches: {:?}", search.matches);
        for m in &search.matches {
            assert_eq!(m.last(), Some(&1));
        }
    }
}
