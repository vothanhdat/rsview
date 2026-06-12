//! Background-thread search — the part that's genuinely simpler in Rust than in
//! the TS port. There, search and the windowed walk share one event loop, so a
//! runaway drain starves the next keystroke (hence pauseWalk + filter-fn
//! cancellation). Here the scan runs on its own OS thread over the same mmap;
//! cancellation is a single `AtomicBool` the thread polls, and matches stream
//! back over an `mpsc` channel. Retyping just drops the old `Search` (its `Drop`
//! flips the flag) and spawns a new one — no starvation, no shared loop.

use crate::scanner::{decode_str, skip_ws, value_kind, Cursor, Kind};
use crate::source::Source;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc,
};
use std::thread::{self, JoinHandle};

/// Cap on collected matches — bounds memory and keeps a pathological "every key
/// matches" query from running forever.
const MAX_MATCHES: usize = 5000;

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
    /// Spawn a worker that scans the whole document for `term` (case-insensitive)
    /// and streams the path of every matching node back over a channel.
    pub fn spawn(mmap: Arc<Source>, term: String, jsonl: bool) -> Search {
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let cancel_w = cancel.clone();
        let done_w = done.clone();
        let needle = term.to_lowercase();

        let handle = thread::spawn(move || {
            let b: &[u8] = &mmap;
            let mut path = Vec::new();
            let mut counter = 0u64;
            let mut found = 0usize;
            if jsonl {
                // Each document is element [i]; recurse into it like an array child
                // so match paths line up with the synthetic NDJSON array root.
                let mut cur = Cursor::lines(0, b.len());
                let mut i = 0usize;
                while let Some(rc) = cur.next(b) {
                    if cancel_w.load(Ordering::Relaxed) {
                        break;
                    }
                    path.push(i);
                    let bailed = scan(
                        b, rc.start, rc.end, rc.kind, &rc.label, &needle, &mut path, &cancel_w,
                        &tx, &mut counter, &mut found,
                    );
                    path.pop();
                    if bailed {
                        break;
                    }
                    i += 1;
                }
            } else {
                let rstart = skip_ws(b, 0, b.len());
                if rstart < b.len() {
                    let kind = value_kind(b, rstart);
                    scan(
                        b, rstart, b.len(), kind, "", &needle, &mut path, &cancel_w, &tx,
                        &mut counter, &mut found,
                    );
                }
            }
            done_w.store(true, Ordering::Relaxed);
        });

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
    needle: &str,
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

    // A node matches if its key contains the needle, or (for a scalar) its value
    // does. Containers match only via their key.
    let mut hit = !label.is_empty() && label.to_lowercase().contains(needle);
    if !hit {
        match kind {
            Kind::Object | Kind::Array => {}
            Kind::Str => hit = decode_str(b, start, end).to_lowercase().contains(needle),
            _ => {
                if let Ok(s) = std::str::from_utf8(&b[start..end]) {
                    hit = s.to_lowercase().contains(needle);
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

    if matches!(kind, Kind::Object | Kind::Array) {
        let mut cur = Cursor::new(start, end, matches!(kind, Kind::Array));
        let mut i = 0;
        while let Some(rc) = cur.next(b) {
            path.push(i);
            let bailed = scan(
                b, rc.start, rc.end, rc.kind, &rc.label, needle, path, cancel, tx, counter, found,
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
