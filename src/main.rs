//! jview — proof-of-concept lazy JSON viewer in Rust.
//!
//! Demonstrates the core of react-obj-view's CLI in native Rust: a file is
//! memory-mapped, parsed on expand (subtrees are byte ranges, not materialized
//! values), and a level is flattened only as far as the viewport scrolls
//! (windowing). Opening a multi-GB file stays near-constant memory.

mod scanner;
mod search;
mod source;
use scanner::{
    container_empty, decode_str, skip_value, skip_ws, value_kind, Cursor, Kind, RawChild, Step,
    MAX_DEPTH,
};
use search::{glob_to_regex, Pattern, Search};
use source::Source;

use memmap2::Mmap;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use std::{
    collections::HashSet,
    fs::File,
    io::{IsTerminal, Read},
    sync::{
        mpsc::{self, Receiver, TryRecvError},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

/// How many children to show in a collapsed container's inline preview.
const PREVIEW_ITEMS: usize = 5;
/// Max display width of a collapsed preview before it's truncated with `…`.
const PREVIEW_WIDTH: usize = 64;
/// Bytes the inline preview will step over to reach the *next* sibling before
/// giving up with `…`. A preview must be cheap to recompute every frame, so it
/// must never pay a huge `skip_value`: if the next key sits behind a value bigger
/// than this (e.g. `meta` behind a 1 GB `users`), the preview shows `…` instead
/// of blocking to scan past it. Generous enough that normal values still appear.
const PREVIEW_SKIP_BUDGET: usize = 64 << 10; // 64 KiB
/// When decoding a scalar for display, only touch this many bytes. A preview/row
/// truncates to a few dozen chars anyway, so decoding a whole multi-hundred-MB
/// string (or a bogus run of digits) would just be wasted work and a memory
/// spike. 512 bytes always covers the truncation width, even for 4-byte chars.
const PREVIEW_DECODE_BYTES: usize = 512;

/// Pane size weights (a ratatui `Fill` factor). A new pane starts at
/// `WEIGHT_DEFAULT`; `+`/`-` step it within `[WEIGHT_MIN, WEIGHT_MAX]`. Equal
/// weights divide the space evenly; a larger weight takes a bigger share.
const WEIGHT_DEFAULT: u16 = 4;
const WEIGHT_MIN: u16 = 1;
const WEIGHT_MAX: u16 = 16;

/// Rows moved per mouse-wheel notch — chunky like tmux, not the 1-row arrow step.
const WHEEL_STEP: usize = 3;

/// Upper bound on a single clipboard copy. Copying a whole subtree is a raw
/// byte-range slice (cheap), but OSC 52 carries the payload base64-encoded
/// through the terminal, which caps how much it'll accept — so we clamp and flag
/// truncation rather than emit a megabytes-long escape. (To extract a big
/// subtree wholesale, that's the export-to-file feature's job, not the clipboard.)
const COPY_MAX_BYTES: usize = 1 << 20; // 1 MiB

/// How many children a `:` goto will scan to resolve an object key. A goto is a
/// synchronous, page-faulting walk, so this bounds the UI freeze on a giant
/// level — a key past this many siblings reports "not found" rather than hanging.
const GOTO_SCAN_CAP: usize = 100_000;

/// Wall-clock budget for one cooperative `flatten` pass. When reaching the next
/// on-screen row means skipping over a huge value (e.g. a 1 GB array to find the
/// sibling after it), `flatten` yields after this long so the loop can paint what
/// it has and poll input; the skip resumes next frame. Keeps the first paint and
/// every later frame responsive instead of blocking on one giant `skip_value`.
const FLATTEN_BUDGET: Duration = Duration::from_millis(8);

/// Bytes a single resumable skip-chunk steps over before re-checking the frame
/// deadline. Small enough that the deadline is honoured to within a chunk, large
/// enough that the per-chunk overhead is negligible (~1–2 ms of scanning).
const SKIP_CHUNK_BYTES: usize = 2 << 20; // 2 MiB

// Syntax-highlight palette (ANSI named colors so it adapts to the terminal theme).
const C_KEY: Color = Color::Cyan; // object keys
const C_INDEX: Color = Color::DarkGray; // array indices
const C_STR: Color = Color::Green; // string values
const C_NUM: Color = Color::Yellow; // numbers
const C_BOOL: Color = Color::Magenta; // true / false
const C_PUNCT: Color = Color::DarkGray; // braces, colon, markers, previews

/// The foreground color for a value of the given kind.
fn value_color(kind: Kind) -> Color {
    match kind {
        Kind::Str => C_STR,
        Kind::Number => C_NUM,
        Kind::Bool => C_BOOL,
        Kind::Null | Kind::Object | Kind::Array => C_PUNCT,
    }
}

/// A lazily-expanding tree node. Children are scanned on demand from `cursor`
/// (resumable), so a collapsed node costs O(1) and a huge level only enumerates
/// as far as it's scrolled.
struct Node {
    label: String,
    start: usize,
    end: usize,
    /// False when `end` is provisional — a container whose closer hasn't been
    /// scanned yet (`end` is its parent's bound). Display/expand work regardless
    /// (cursors stop at the real closer); the raw-range consumers (copy `y`, split
    /// `o`/`O`) check this and skip to the true closer before slicing.
    end_exact: bool,
    kind: Kind,
    is_index: bool,
    /// Synthetic NDJSON root: children are the documents, enumerated by a
    /// `Cursor::lines` instead of a bracketed-container cursor.
    jsonl: bool,
    has_children: bool,
    expanded: bool,
    done: bool,
    children: Vec<Node>,
    cursor: Option<Cursor>,
}

impl Node {
    fn is_container(&self) -> bool {
        matches!(self.kind, Kind::Object | Kind::Array)
    }

    /// The right child-cursor for this node: a line stream for the NDJSON root,
    /// otherwise a normal bracketed-container cursor.
    fn make_cursor(&self) -> Cursor {
        if self.jsonl {
            Cursor::lines(self.start, self.end)
        } else {
            Cursor::new(self.start, self.end, matches!(self.kind, Kind::Array))
        }
    }

    fn from_raw(rc: RawChild, b: &[u8]) -> Node {
        let is_cont = matches!(rc.kind, Kind::Object | Kind::Array);
        // `container_empty` only reads just past the opener, so a provisional
        // (over-long) `end` is harmless here.
        let has = is_cont && !container_empty(b, rc.start, rc.end);
        Node {
            label: rc.label,
            start: rc.start,
            end: rc.end,
            end_exact: rc.end_exact,
            kind: rc.kind,
            is_index: rc.is_index,
            jsonl: false,
            has_children: has,
            expanded: false,
            done: false,
            children: Vec::new(),
            cursor: None,
        }
    }

    fn toggle(&mut self) {
        if !self.is_container() || !self.has_children {
            return;
        }
        self.expanded = !self.expanded;
        // Init the child cursor on first expand only; collapse keeps the cursor +
        // already-scanned children so re-expand resumes instead of rescanning.
        if self.expanded && self.cursor.is_none() && self.children.is_empty() && !self.done {
            self.cursor = Some(self.make_cursor());
        }
    }

    /// Ensure `children[i]` exists, scanning more from the cursor if needed.
    /// Eager: a deferred end-skip is resolved in one shot (used by jumps/goto/
    /// search, which must reach a specific node now).
    fn ensure_child(&mut self, b: &[u8], i: usize) {
        while self.children.len() <= i {
            let nx = match self.cursor.as_mut() {
                Some(c) => c.next(b),
                None => None,
            };
            match nx {
                Some(rc) => self.children.push(Node::from_raw(rc, b)),
                None => {
                    self.done = true;
                    break;
                }
            }
        }
    }

    /// Cooperative `ensure_child`: drive the cursor in resumable chunks until
    /// `children[i]` exists or `deadline` passes. Returns `Ready` (available),
    /// `Done` (level ended before `i`), or `Busy` (deadline hit mid skip — call
    /// again next frame; cursor state is preserved). This is what lets a huge
    /// level (or a giant value standing between two on-screen rows) flatten a
    /// frame's worth at a time instead of blocking.
    fn ensure_child_until(&mut self, b: &[u8], i: usize, deadline: Instant) -> EnsureChild {
        while self.children.len() <= i {
            let Some(c) = self.cursor.as_mut() else {
                return EnsureChild::Done;
            };
            match c.step(b, SKIP_CHUNK_BYTES) {
                Step::Child(rc) => self.children.push(Node::from_raw(rc, b)),
                Step::Done => {
                    self.done = true;
                    return EnsureChild::Done;
                }
                Step::Yield => {
                    if Instant::now() >= deadline {
                        return EnsureChild::Busy; // out of frame time, skip in flight
                    }
                    // time left — keep stepping the same skip
                }
            }
        }
        EnsureChild::Ready
    }

    /// The value text shown after the label. Collapsed containers get a real
    /// one-line preview of their first few children; expanded ones get the
    /// opening brace; scalars get their (truncated) literal.
    fn preview(&self, b: &[u8]) -> String {
        match self.kind {
            Kind::Object | Kind::Array => {
                let arr = matches!(self.kind, Kind::Array);
                if !self.has_children {
                    if arr {
                        "[]".into()
                    } else {
                        "{}".into()
                    }
                } else if self.expanded {
                    if arr {
                        "[".into()
                    } else {
                        "{".into()
                    }
                } else {
                    self.collapsed_preview(b)
                }
            }
            Kind::Str => {
                let e = decode_cap(self.start, self.end);
                format!("\"{}\"", truncate(&decode_str(b, self.start, e), 70))
            }
            _ => {
                let e = decode_cap(self.start, self.end);
                truncate(&String::from_utf8_lossy(&b[self.start..e]), 70)
            }
        }
    }

    /// Scan the first few children of a collapsed container and render them
    /// inline, e.g. `{ version: "0.3.2", deps: {…}, … }`. Uses a fresh resumable
    /// `Cursor`, capped at `PREVIEW_ITEMS`, so it's O(few) regardless of size.
    ///
    /// Crucially it scans with a small per-sibling budget (`step`, not the eager
    /// `next`): if reaching the next child would mean skipping a value bigger than
    /// `PREVIEW_SKIP_BUDGET` (e.g. `meta` behind a 1 GB `users`), it stops with `…`
    /// rather than blocking. Without this, the *collapsed* row would re-scan 1 GB
    /// every frame — which is why collapsing a giant container felt slow.
    fn collapsed_preview(&self, b: &[u8]) -> String {
        let arr = matches!(self.kind, Kind::Array);
        let (open, close) = if arr { ("[", "]") } else { ("{", "}") };
        let mut cur = self.make_cursor();
        let mut parts: Vec<String> = Vec::new();
        let mut more = false;
        loop {
            if parts.len() == PREVIEW_ITEMS {
                // Is there at least one more? `Yield` (next child behind a big
                // value) counts as "yes" without paying the skip.
                more = !matches!(cur.step(b, PREVIEW_SKIP_BUDGET), Step::Done);
                break;
            }
            match cur.step(b, PREVIEW_SKIP_BUDGET) {
                Step::Child(rc) => {
                    let v = brief(b, &rc);
                    parts.push(if arr {
                        v
                    } else {
                        format!("{}: {}", rc.label, v)
                    });
                }
                // The next sibling is behind a value too big to scan for a preview.
                Step::Yield => {
                    more = true;
                    break;
                }
                Step::Done => break,
            }
        }
        if parts.is_empty() {
            return format!("{open}{close}");
        }
        let mut body = parts.join(", ");
        if more {
            body.push_str(", …");
        }
        truncate(&format!("{open} {body} {close}"), PREVIEW_WIDTH)
    }
}

/// Upper bound for a display decode: never touch more than `PREVIEW_DECODE_BYTES`
/// past `start`, so a giant string/number value isn't fully decoded just to show
/// a truncated preview of it.
fn decode_cap(start: usize, end: usize) -> usize {
    end.min(start.saturating_add(PREVIEW_DECODE_BYTES))
}

/// A brief one-level rendering of a value for use inside a parent's preview:
/// nested containers collapse to `{…}`/`[…]`, scalars show their literal.
fn brief(b: &[u8], rc: &RawChild) -> String {
    match rc.kind {
        Kind::Object => {
            if container_empty(b, rc.start, rc.end) {
                "{}".into()
            } else {
                "{…}".into()
            }
        }
        Kind::Array => {
            if container_empty(b, rc.start, rc.end) {
                "[]".into()
            } else {
                "[…]".into()
            }
        }
        Kind::Str => {
            let e = decode_cap(rc.start, rc.end);
            format!("\"{}\"", truncate(&decode_str(b, rc.start, e), 24))
        }
        _ => {
            let e = decode_cap(rc.start, rc.end);
            truncate(&String::from_utf8_lossy(&b[rc.start..e]), 24)
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

/// A flattened, on-screen row plus the index path back to its node. Carries the
/// label/value/kind separately so the renderer can syntax-color each segment.
struct Row {
    depth: usize,
    label: String,
    value: String,
    kind: Kind,
    is_index: bool,
    has_children: bool,
    expanded: bool,
    path: Vec<usize>,
    /// A synthetic, non-navigable placeholder rendered while a huge value is being
    /// stepped over (the inline "⠋ loading…" line at the spot where the next
    /// sibling will appear). Always the last row; focus is clamped above it.
    loading: bool,
}

/// Windowed flatten: walk the expanded tree DFS, scanning children on demand,
/// and stop once `budget` rows exist. A pathologically flat level (millions of
/// keys) therefore only flattens ~a screenful, not the whole thing.
/// Outcome of a cooperative child-scan (see [`Node::ensure_child_until`]).
enum EnsureChild {
    /// `children[i]` is available.
    Ready,
    /// The deadline was hit mid-skip — repaint and resume next frame.
    Busy,
    /// The level ended before index `i`.
    Done,
}

/// Windowed flatten: walk the expanded tree DFS, scanning children on demand,
/// and stop once `budget` rows exist. With `deadline = Some(_)` it is also
/// *cooperative*: if reaching the next row means skipping a huge value past the
/// frame's time budget, it sets `*incomplete = true` and returns early so the
/// caller can paint what's flattened and resume next frame (the skip is
/// preserved on the cursor). `deadline = None` drains eagerly (used by jumps,
/// which must reach a target row synchronously).
#[allow(clippy::too_many_arguments)]
fn flatten(
    node: &mut Node,
    b: &[u8],
    depth: usize,
    budget: usize,
    out: &mut Vec<Row>,
    path: &mut Vec<usize>,
    deadline: Option<Instant>,
    incomplete: &mut bool,
) {
    if *incomplete || out.len() >= budget {
        return;
    }
    out.push(Row {
        depth,
        label: node.label.clone(),
        value: node.preview(b),
        kind: node.kind,
        is_index: node.is_index,
        has_children: node.has_children,
        expanded: node.expanded,
        path: path.clone(),
        loading: false,
    });
    // Stop descending past the depth cap so a deeply-nested (possibly hostile)
    // document can't recurse the stack to a fault. Real data never reaches it.
    if depth < MAX_DEPTH && node.expanded && node.is_container() {
        let mut i = 0;
        while out.len() < budget {
            match deadline {
                Some(dl) => match node.ensure_child_until(b, i, dl) {
                    EnsureChild::Ready => {}
                    EnsureChild::Done => break, // level fully enumerated
                    EnsureChild::Busy => {
                        *incomplete = true; // skip in flight — paint now, resume next frame
                                            // Inline placeholder at the spot the next child will fill,
                                            // indented to the child level, so the wait reads as "this
                                            // node is still loading" rather than a frozen screen.
                        if out.len() < budget {
                            out.push(Row {
                                depth: depth + 1,
                                label: String::new(),
                                value: String::new(),
                                kind: Kind::Null,
                                is_index: false,
                                has_children: false,
                                expanded: false,
                                path: Vec::new(),
                                loading: true,
                            });
                        }
                        return;
                    }
                },
                None => {
                    node.ensure_child(b, i);
                    if i >= node.children.len() {
                        break; // level fully enumerated
                    }
                }
            }
            path.push(i);
            flatten(
                &mut node.children[i],
                b,
                depth + 1,
                budget,
                out,
                path,
                deadline,
                incomplete,
            );
            path.pop();
            if *incomplete {
                return;
            }
            i += 1;
        }
    }
}

fn get<'a>(mut n: &'a Node, path: &[usize]) -> &'a Node {
    for &i in path {
        n = &n.children[i];
    }
    n
}

fn get_mut<'a>(mut n: &'a mut Node, path: &[usize]) -> &'a mut Node {
    for &i in path {
        n = &mut n.children[i];
    }
    n
}

/// Expand every ancestor of `path` (scanning children up to each step) so the
/// target node becomes reachable in the flattened rows. The target itself is
/// left as-is — only the chain above it is opened.
fn expand_to(n: &mut Node, b: &[u8], path: &[usize]) {
    // Recursion depth equals the path length; cap it so a deep path can't fault
    // the stack. (Search matches are already capped at MAX_DEPTH, so this only
    // bites pathological input — the deepest reachable rows simply aren't opened.)
    if path.is_empty() || path.len() > MAX_DEPTH {
        return;
    }
    if n.is_container() && n.has_children && !n.expanded {
        n.toggle();
    }
    n.ensure_child(b, path[0]);
    if path[0] < n.children.len() {
        expand_to(&mut n.children[path[0]], b, &path[1..]);
    }
}

#[derive(PartialEq)]
enum Mode {
    Normal,
    Search,
    /// Typing a path to jump to (`:` prompt, e.g. `data.users[3].city`).
    Goto,
    /// The bookmark picker overlay (`'`): pick a saved node to jump to.
    Marks,
    /// The keyboard-shortcut cheatsheet overlay (`?`): any key closes it.
    Help,
}

/// What a key press asks the run loop to do that it can't do itself: quit, or
/// (re)launch the search — which needs a byte `Source` the caller supplies (a
/// fixed mmap for files, a fresh snapshot for streams).
enum KeyOutcome {
    Continue,
    Quit,
    Relaunch,
}

/// One pane: an independent lazy tree + viewport over a byte range of the shared
/// `Source`. The main pane is the whole document; a split pane (`derived`) is
/// rooted at another pane's focused node. Each keeps its own focus, scroll,
/// expansion, and search.
struct View {
    root: Node,
    name: String,
    /// True for a pane spun off by `s` (rooted at a sub-range), false for the
    /// original document pane. Drives the `↳` title marker and which pane the
    /// streaming re-parse feeds.
    derived: bool,
    /// Relative size in the workspace layout (a `Fill` weight); `+`/`-` adjust it.
    weight: u16,
    /// Stable identity for parent/child links (Vec indices shift when a pane is
    /// closed, so links can't be indices).
    id: u64,
    /// The pane this was split from. Closing a pane closes its descendants too.
    parent: Option<u64>,
    /// The child reused by `o` (open-or-replace): re-rooted in place rather than
    /// opening yet another pane.
    preview_child: Option<u64>,
    focus: usize,
    scroll: usize,
    rows: Vec<Row>,
    mode: Mode,
    /// Live search-input buffer (typed while in `Mode::Search`).
    query: String,
    /// Set when the typed `re:`/`g:` query failed to compile, so the footer can
    /// surface "(bad pattern)" without re-parsing. Cleared on a clean compile.
    query_error: Option<String>,
    /// The running search, if any. `None` once cleared/cancelled.
    search: Option<Search>,
    /// Which match the cursor is currently on.
    match_idx: usize,
    /// Whether match-cycling has landed on a result yet (so the first
    /// next/prev press goes to the first/last match, not the second).
    landed: bool,
    /// Set of match paths, for O(1) row highlighting. Grown incrementally.
    match_set: HashSet<Vec<usize>>,
    /// How many of `search.matches` are already in `match_set`.
    indexed: usize,
    /// A pending jump target: the next frame flattens far enough to land on it.
    want_path: Option<Vec<usize>>,
    /// Set when the last cooperative `flatten_window` yielded mid-skip (a huge
    /// value still being stepped over). The run loop keeps flattening — without
    /// blocking on input — until it clears. Always false after a jump.
    flatten_incomplete: bool,
    /// Live path-input buffer (typed while in `Mode::Goto`).
    goto: String,
    /// Saved node paths (`m` toggles), jumped to via the `'` picker overlay.
    bookmarks: Vec<Vec<usize>>,
    /// Selected row in the bookmark picker.
    mark_idx: usize,
}

/// Build the root node for a buffer. Shared by `App::new` and `App::rebuild`
/// (the streaming re-parse), so a growing buffer always produces a consistent
/// root. The root is auto-expanded (like `--depth 1`) when it has children.
fn make_root(b: &[u8], name: &str, jsonl: bool) -> Node {
    // NDJSON: a synthetic array root spanning the whole buffer, with at least one
    // document if there's any non-whitespace. Single-doc: the first value.
    let (start, kind, has) = if jsonl {
        (0, Kind::Array, skip_ws(b, 0, b.len()) < b.len())
    } else {
        let rstart = skip_ws(b, 0, b.len());
        if rstart >= b.len() {
            // Empty / whitespace-only input (e.g. `jview </dev/null`, or a stream
            // with no data yet): show an empty root rather than indexing past the
            // buffer in value_kind.
            (rstart, Kind::Null, false)
        } else {
            let k = value_kind(b, rstart);
            let cont = matches!(k, Kind::Object | Kind::Array);
            (rstart, k, cont && !container_empty(b, rstart, b.len()))
        }
    };
    let mut root = Node {
        label: name.into(),
        start,
        end: b.len(),     // root spans to EOF; the scanner stops at the real closer
        end_exact: false, // provisional (to EOF) — copy re-resolves bounded by its cap
        kind,
        is_index: false,
        jsonl,
        has_children: has,
        expanded: false,
        done: false,
        children: Vec::new(),
        cursor: None,
    };
    if has {
        root.toggle(); // auto-expand the root (like --depth 1)
    }
    root
}

/// Build a pane root over the byte range `[start, end)` of an existing node (the
/// focused node of the pane being split). Unlike `make_root` this is a real
/// bounded container, not the to-EOF document root, so its cursor stops at the
/// node's own closer. Auto-expanded like the document root.
fn make_subroot(b: &[u8], label: String, start: usize, end: usize, kind: Kind) -> Node {
    let cont = matches!(kind, Kind::Object | Kind::Array);
    let has = cont && !container_empty(b, start, end);
    let mut root = Node {
        label,
        start,
        end,
        end_exact: true, // caller passed the node's already-resolved real end
        kind,
        is_index: false,
        jsonl: false,
        has_children: has,
        expanded: false,
        done: false,
        children: Vec::new(),
        cursor: None,
    };
    if has {
        root.toggle();
    }
    root
}

/// Render a node path as a JSON accessor string (`users[1].address`), used as a
/// split pane's title. `base` is the source pane's own origin (empty for the
/// document pane) so chained splits accumulate (`users[1].address.city`).
fn join_path(base: &str, segs: &[(String, bool)]) -> String {
    let mut s = base.to_string();
    for (label, is_idx) in segs {
        if *is_idx {
            s.push_str(&format!("[{label}]"));
        } else {
            if !s.is_empty() {
                s.push('.');
            }
            s.push_str(label);
        }
    }
    if s.is_empty() {
        "root".into()
    } else {
        s
    }
}

/// A parsed `goto` path segment: an object key or an array index.
#[derive(Debug, PartialEq)]
enum Seg {
    Key(String),
    Index(usize),
}

/// A parsed `goto` path: where resolution starts, then the segments to descend.
#[derive(Debug, PartialEq)]
struct ParsedPath {
    /// `None` → **absolute**: resolve from the document root.
    /// `Some(up)` → **relative**: start at the focused node and climb `up` levels
    /// first (0 = the focused node itself, 1 = its parent, …), then descend `segs`.
    up: Option<usize>,
    segs: Vec<Seg>,
}

/// Parse a `goto` path string. Three forms, all lenient (it's an interactive
/// jump, not a validator):
/// - **absolute** — a bare path (`data.users[3].city`) or a `$`-prefixed one
///   (`$.data`, the dot after `$` optional) resolves from the document root.
/// - **relative** — a leading run of dots resolves from the focused node,
///   Python-import style: `.child` (from the focus), `..sibling` (climb to the
///   parent first), `...x` (climb two), and bare `.`/`..` jump to the
///   focus/parent with no descent.
///
/// Bracketed (optionally quoted) keys may hold literal dots: `["x.y"][2]`.
fn parse_path(input: &str) -> ParsedPath {
    let s = input.trim();
    if let Some(rest) = s.strip_prefix('$') {
        // Explicit absolute. Drop one optional separating dot (`$.a` == `$a`).
        let rest = rest.strip_prefix('.').unwrap_or(rest);
        return ParsedPath {
            up: None,
            segs: tokenize_segments(rest),
        };
    }
    if s.starts_with('.') {
        // Relative: the leading dots say how far up to climb (d dots → up d-1).
        // Dots are ASCII, so the count is also a byte offset into `s`.
        let dots = s.chars().take_while(|&c| c == '.').count();
        return ParsedPath {
            up: Some(dots - 1),
            segs: tokenize_segments(&s[dots..]),
        };
    }
    ParsedPath {
        up: None,
        segs: tokenize_segments(s),
    }
}

/// Split a path *body* (after any `$`/leading-dot prefix is stripped) into
/// key/index segments on `.` and `[…]`.
fn tokenize_segments(input: &str) -> Vec<Seg> {
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        match c {
            '.' => {
                if !cur.is_empty() {
                    segs.push(Seg::Key(std::mem::take(&mut cur)));
                }
            }
            '[' => {
                if !cur.is_empty() {
                    segs.push(Seg::Key(std::mem::take(&mut cur)));
                }
                let mut inner = String::new();
                for d in chars.by_ref() {
                    if d == ']' {
                        break;
                    }
                    inner.push(d);
                }
                let inner = inner.trim().trim_matches(|q| q == '"' || q == '\'');
                match inner.parse::<usize>() {
                    Ok(i) => segs.push(Seg::Index(i)),
                    Err(_) => segs.push(Seg::Key(inner.to_string())),
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        segs.push(Seg::Key(cur));
    }
    segs
}

/// Resolve parsed segments to an index-path into `root`, starting from `base`
/// (the empty slice for an absolute path; the focused node's path, already
/// climbed, for a relative one), expanding and scanning each container as it
/// descends (so a collapsed level still resolves). Returns `None` if a segment
/// doesn't exist (missing key, out-of-range index, or a scalar hit mid-path).
/// Object-key lookup scans up to `GOTO_SCAN_CAP` children.
fn resolve_path(root: &mut Node, b: &[u8], base: &[usize], segs: &[Seg]) -> Option<Vec<usize>> {
    let mut out: Vec<usize> = base.to_vec();
    for seg in segs {
        let node = get_mut(root, &out);
        if !node.is_container() || !node.has_children {
            return None;
        }
        if !node.expanded {
            node.toggle(); // create the child cursor so ensure_child can scan
        }
        match seg {
            Seg::Index(i) => {
                node.ensure_child(b, *i);
                if *i >= node.children.len() {
                    return None;
                }
                out.push(*i);
            }
            Seg::Key(k) => {
                // Glob keys (`*`, `?`) widen the lookup to "first child whose
                // label matches the whole pattern", letting the user jump with
                // partial recall (`data.user*` / `data.*name*`). Plain keys
                // keep the exact-match fast path.
                let re = if k.contains('*') || k.contains('?') {
                    Some(
                        regex::RegexBuilder::new(&format!("^{}$", glob_to_regex(k)))
                            .case_insensitive(true)
                            .build()
                            .ok()?,
                    )
                } else {
                    None
                };
                let mut i = 0;
                let mut found = None;
                while i < GOTO_SCAN_CAP {
                    node.ensure_child(b, i);
                    if i >= node.children.len() {
                        break;
                    }
                    let label = &node.children[i].label;
                    let hit = match &re {
                        Some(re) => re.is_match(label),
                        None => label == k,
                    };
                    if hit {
                        found = Some(i);
                        break;
                    }
                    i += 1;
                }
                out.push(found?);
            }
        }
    }
    Some(out)
}

/// Resolve `parsed`, climbing toward the root on a miss so a path that doesn't
/// exist where it was typed still lands if it exists higher up. The candidate
/// bases, tried in order, are:
/// - **absolute / bare** (`up == None`): the root first (the literal reading),
///   then the focused node's ancestry from the cursor up — `.seg`, `..seg`,
///   `...seg`, … — so `:city` falls back to `..city`, then `...city`, and so on.
/// - **relative** (`up == Some(n)`): the cursor climbed `n` levels (the level you
///   asked for), then each ancestor above it up to the root.
///
/// Returns the first base at which every segment resolves, or `None` if none do.
fn resolve_with_climb(
    root: &mut Node,
    b: &[u8],
    focus_path: &[usize],
    parsed: &ParsedPath,
) -> Option<Vec<usize>> {
    let mut bases: Vec<Vec<usize>> = Vec::new();
    if parsed.up.is_none() {
        bases.push(Vec::new()); // literal absolute, from the root
    }
    // Climb the focus ancestry from the requested level up to the root. For an
    // absolute path that's the whole chain (cursor → root); for a relative one it
    // starts `up` levels above the cursor.
    let start = focus_path.len().saturating_sub(parsed.up.unwrap_or(0));
    for keep in (0..=start).rev() {
        bases.push(focus_path[..keep].to_vec());
    }
    let mut tried: Vec<Vec<usize>> = Vec::new();
    for base in bases {
        if tried.contains(&base) {
            continue; // root can appear twice (absolute case); resolve it once
        }
        if let Some(path) = resolve_path(root, b, &base, &parsed.segs) {
            return Some(path);
        }
        tried.push(base);
    }
    None
}

/// Collect the index-paths of every expanded container in the tree, in DFS
/// preorder (parents before children). Used to carry expansion state across a
/// streaming re-parse.
fn collect_expanded(node: &Node, path: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
    if node.expanded {
        out.push(path.clone());
        for (i, ch) in node.children.iter().enumerate() {
            path.push(i);
            collect_expanded(ch, path, out);
            path.pop();
        }
    }
}

/// Re-expand the node at `path`, opening (and scanning) each ancestor as needed.
/// A no-op if the path isn't reachable yet (data hasn't arrived for it).
fn set_expanded(root: &mut Node, b: &[u8], path: &[usize]) {
    let mut n = root;
    for &idx in path {
        if n.is_container() && n.has_children && !n.expanded {
            n.toggle();
        }
        n.ensure_child(b, idx);
        if idx >= n.children.len() {
            return; // child not present in the buffer yet
        }
        n = &mut n.children[idx];
    }
    if n.is_container() && n.has_children && !n.expanded {
        n.toggle();
    }
}

impl View {
    fn new(b: &[u8], path: &str, jsonl: bool) -> View {
        let name = std::path::Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "ROOT".into());
        let root = make_root(b, &name, jsonl);
        View::with_root(root, name, false)
    }

    /// A pane over an already-built root (the document root, or a split's
    /// sub-range root). `name` is the origin path shown in the title.
    fn with_root(root: Node, name: String, derived: bool) -> View {
        View {
            root,
            name,
            derived,
            weight: WEIGHT_DEFAULT,
            id: 0,
            parent: None,
            preview_child: None,
            focus: 0,
            scroll: 0,
            rows: Vec::new(),
            mode: Mode::Normal,
            query: String::new(),
            query_error: None,
            search: None,
            match_idx: 0,
            match_set: HashSet::new(),
            indexed: 0,
            want_path: None,
            landed: false,
            goto: String::new(),
            bookmarks: Vec::new(),
            mark_idx: 0,
            flatten_incomplete: false,
        }
    }

    /// Re-root this pane at a new subtree (used by `o` to reuse a preview pane).
    /// Resets the viewport and drops any search, since the content changed; keeps
    /// the pane's id/links and size weight.
    fn reroot(&mut self, root: Node, name: String) {
        self.root = root;
        self.name = name;
        self.derived = true;
        self.focus = 0;
        self.scroll = 0;
        self.rows.clear();
        self.want_path = None;
        self.mode = Mode::Normal;
        self.goto.clear();
        // Bookmarked paths refer to the old tree; drop them on re-root.
        self.bookmarks.clear();
        self.mark_idx = 0;
        self.clear_search();
    }

    /// Re-parse a grown stream buffer from scratch, preserving the UI cursor and
    /// expansion. Cheap because the parse is lazy: only the expanded/visible
    /// extent is scanned, and byte offsets of already-seen content are stable
    /// (the stream only appends). The focused row's path is queued as the next
    /// jump so the cursor lands back on the same node.
    fn rebuild(&mut self, b: &[u8], jsonl: bool) {
        let mut expanded = Vec::new();
        collect_expanded(&self.root, &mut Vec::new(), &mut expanded);
        let focus_path = self.rows.get(self.focus).map(|r| r.path.clone());

        self.root = make_root(b, &self.name, jsonl);
        for path in &expanded {
            set_expanded(&mut self.root, b, path);
        }
        // Keep the cursor on the same node (unless a jump is already pending).
        if self.want_path.is_none() {
            self.want_path = focus_path;
        }
    }

    fn toggle_focus(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let path = self.rows[self.focus].path.clone();
        get_mut(&mut self.root, &path).toggle();
    }

    /// (Re)launch the live search for the current `query`. Dropping the previous
    /// `Search` cancels its worker thread; an empty query just clears results.
    /// A `re:`/`g:` query that fails to compile is treated as zero-matches —
    /// the footer surfaces the parse error so the user can fix the expression
    /// without losing what they've typed.
    fn relaunch(&mut self, mmap: &Arc<Source>) {
        if let Some(old) = self.search.take() {
            old.cancel(); // belt-and-suspenders; Drop also flips the flag
        }
        self.match_set.clear();
        self.indexed = 0;
        self.match_idx = 0;
        self.landed = false;
        self.want_path = None;
        self.query_error = None;
        if self.query.is_empty() {
            return;
        }
        let pattern = match Pattern::parse(&self.query) {
            Ok(p) => p,
            Err(e) => {
                self.query_error = Some(e);
                return;
            }
        };
        self.search = Some(Search::spawn(
            Arc::clone(mmap),
            pattern,
            self.root.jsonl,
            self.root.start,
            self.root.end,
            self.root.kind,
        ));
    }

    /// Pull newly-found matches into `match_set` so rows can be highlighted.
    fn pump_search(&mut self) {
        if let Some(s) = self.search.as_mut() {
            s.drain();
        }
        let n = self.search.as_ref().map_or(0, |s| s.matches.len());
        while self.indexed < n {
            let p = self.search.as_ref().unwrap().matches[self.indexed].clone();
            self.match_set.insert(p);
            self.indexed += 1;
        }
    }

    /// Jump to the next/previous match (`dir` = +1 / -1), wrapping around. The
    /// first press after a (re)search lands on the first (or last) match; later
    /// presses step from there.
    fn nav_match(&mut self, dir: i32, b: &[u8]) {
        let n = self.search.as_ref().map_or(0, |s| s.matches.len());
        if n == 0 {
            return;
        }
        self.match_idx = if !self.landed {
            self.landed = true;
            if dir >= 0 {
                0
            } else {
                n - 1
            }
        } else if dir >= 0 {
            (self.match_idx + 1) % n
        } else {
            (self.match_idx + n - 1) % n
        };
        let path = self.search.as_ref().unwrap().matches[self.match_idx].clone();
        self.jump_to(&path, b);
    }

    /// Expand the ancestors of `path` and queue the row for focus next frame.
    fn jump_to(&mut self, path: &[usize], b: &[u8]) {
        expand_to(&mut self.root, b, path);
        self.want_path = Some(path.to_vec());
    }

    /// Resolve the typed `goto` path and jump to it. Returns a footer status line.
    /// Absolute paths resolve from the root; relative ones (leading dots) start at
    /// the focused node, climbed `up` levels. On a miss the resolution climbs
    /// toward the root and retries (see [`resolve_with_climb`]), so `:city` falls
    /// back to `..city`, `...city`, … relative to the cursor.
    fn goto_path(&mut self, b: &[u8]) -> String {
        let parsed = parse_path(&self.goto);
        if parsed.up.is_none() && parsed.segs.is_empty() {
            return "empty path".to_string();
        }
        let focus_path = self
            .rows
            .get(self.focus)
            .map(|r| r.path.clone())
            .unwrap_or_default();
        match resolve_with_climb(&mut self.root, b, &focus_path, &parsed) {
            Some(path) => {
                self.jump_to(&path, b);
                format!(
                    "jumped to {}",
                    join_path("", &breadcrumb_segments(&self.root, &path))
                )
            }
            None => format!("path not found: {}", self.goto.trim()),
        }
    }

    /// Toggle a bookmark on the focused node (add if absent, else remove).
    fn toggle_bookmark(&mut self) -> String {
        let Some(row) = self.rows.get(self.focus) else {
            return "nothing to bookmark".to_string();
        };
        let path = row.path.clone();
        if let Some(pos) = self.bookmarks.iter().position(|p| *p == path) {
            self.bookmarks.remove(pos);
            if self.mark_idx >= self.bookmarks.len() {
                self.mark_idx = self.bookmarks.len().saturating_sub(1);
            }
            "bookmark removed".to_string()
        } else {
            let label = join_path("", &breadcrumb_segments(&self.root, &path));
            self.bookmarks.push(path);
            format!("bookmarked {label}")
        }
    }

    fn clear_search(&mut self) {
        if let Some(s) = self.search.take() {
            s.cancel();
        }
        self.query.clear();
        self.query_error = None;
        self.match_set.clear();
        self.indexed = 0;
        self.match_idx = 0;
        self.landed = false;
        self.want_path = None;
    }

    /// Jump to the next (`forward`) or previous sibling at the focused node's
    /// level, stepping over its (possibly expanded) subtree. A no-op at either
    /// end of the level, or on the root (which has no siblings).
    fn nav_sibling(&mut self, b: &[u8], forward: bool) {
        if self.rows.is_empty() {
            return;
        }
        let path = self.rows[self.focus].path.clone();
        let Some((&last, parent)) = path.split_last() else {
            return; // root has no siblings
        };
        let target = if forward {
            last + 1
        } else if last == 0 {
            return; // already the first sibling
        } else {
            last - 1
        };
        // The parent is expanded (the focused row is one of its children), so
        // scan one more sibling lazily; bail if there's none (past the end, or
        // not yet streamed in).
        let parent_node = get_mut(&mut self.root, parent);
        parent_node.ensure_child(b, target);
        if target >= parent_node.children.len() {
            return;
        }
        let mut cand = parent.to_vec();
        cand.push(target);
        self.jump_to(&cand, b);
    }

    fn collapse_or_parent(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let path = self.rows[self.focus].path.clone();
        let (expanded, cont) = {
            let n = get(&self.root, &path);
            (n.expanded, n.is_container())
        };
        if expanded && cont {
            get_mut(&mut self.root, &path).toggle();
        } else if !path.is_empty() {
            let pp = &path[..path.len() - 1];
            if let Some(idx) = self.rows.iter().position(|r| r.path.as_slice() == pp) {
                self.focus = idx;
            }
        }
    }

    /// Flatten this pane's visible window, land any pending jump, and clamp
    /// focus/scroll into range. `h` is the pane's content height.
    fn flatten_window(&mut self, b: &[u8], h: usize) {
        if let Some(target) = self.want_path.take() {
            // Jump: flatten far enough to include the target's row. A static
            // estimate undershoots once sibling subtrees are expanded (e.g.
            // cycling search matches expands each visited node, pushing later
            // rows down), so grow the budget until the target appears — or the
            // tree is fully walked (target unreachable / not arrived yet).
            // Eager (`deadline = None`): a jump must resolve its row now.
            let mut budget = target.iter().sum::<usize>() + target.len() + h + 64;
            let mut ignore = false;
            loop {
                self.rows.clear();
                let mut path = Vec::new();
                flatten(
                    &mut self.root,
                    b,
                    0,
                    budget,
                    &mut self.rows,
                    &mut path,
                    None,
                    &mut ignore,
                );
                let walked_all = self.rows.len() < budget;
                if self.rows.iter().any(|r| r.path == target) || walked_all {
                    break;
                }
                budget = budget.saturating_mul(2);
            }
            self.flatten_incomplete = false;
            if let Some(idx) = self.rows.iter().position(|r| r.path == target) {
                self.focus = idx;
            }
        } else {
            // No jump pending: flatten only as far as the viewport needs, and
            // cooperatively — if a huge value sits between visible rows, paint
            // what we have and resume next frame (see `flatten`/the run loop).
            let budget = (self.scroll + h + 64).max(self.focus + 64);
            self.rows.clear();
            let mut path = Vec::new();
            let mut incomplete = false;
            flatten(
                &mut self.root,
                b,
                0,
                budget,
                &mut self.rows,
                &mut path,
                Some(Instant::now() + FLATTEN_BUDGET),
                &mut incomplete,
            );
            self.flatten_incomplete = incomplete;
        }

        // Clamp focus to the last *real* row — never the trailing loading
        // placeholder (it's not a navigable node, and its path is empty).
        let max_focus = self.rows.iter().rposition(|r| !r.loading).unwrap_or(0);
        if self.focus > max_focus {
            self.focus = max_focus;
        }
        if self.focus < self.scroll {
            self.scroll = self.focus;
        }
        if self.focus >= self.scroll + h {
            self.scroll = self.focus + 1 - h;
        }
    }
}

/// The workspace: one or more panes side by side, with `active` receiving keys.
/// Splitting pushes a new pane rooted at the active pane's focused node; closing
/// removes it. All panes share the same byte `Source`, so a split is O(1).
struct App {
    views: Vec<View>,
    active: usize,
    /// Pane orientation: false = side by side (columns), true = stacked (rows).
    /// Toggled with `\`.
    stacked: bool,
    /// Monotonic source of pane ids (never reused, so links stay unambiguous).
    next_id: u64,
    /// Transient status line (e.g. "copied 42 B") shown in the footer until the
    /// next key press. `None` = show the normal key hint.
    flash: Option<String>,
    /// True when stdout was redirected (a pipe or file), so `p` can extract the
    /// focused node into it. Set by `run_file`/`run_stdin` after reserving
    /// stdout; into a terminal there's nowhere to pipe, so `p` is a hint instead.
    can_extract: bool,
    /// Set by `p` to the focused node's byte range; the viewer quits and the
    /// caller writes that slice to the reserved stdout on the way out.
    extract: Option<(usize, usize)>,
}

impl App {
    fn single(mut view: View) -> App {
        view.id = 0;
        App {
            views: vec![view],
            active: 0,
            stacked: false,
            next_id: 1,
            flash: None,
            can_extract: false,
            extract: None,
        }
    }

    /// Byte range of the active pane's focused node (its raw JSON value/subtree).
    fn focused_range(&self) -> Option<(usize, usize)> {
        let v = self.active_view();
        let row = v.rows.get(v.focus)?;
        let node = get(&v.root, &row.path);
        Some((node.start, node.end))
    }

    /// Copy the focused node's raw JSON (a scalar literal, or a whole subtree) to
    /// the terminal clipboard. Clamped to `COPY_MAX_BYTES`. Returns a status line.
    fn yank_value(&self, b: &[u8]) -> String {
        match self.focused_range() {
            Some((s, prov_end)) => {
                // `prov_end` may be provisional (a container whose closer wasn't
                // scanned — it spans to the parent's bound, so a naive slice would
                // run into siblings). Find the real closer, but never scan past
                // what we'd copy anyway: the copy cap. If the scan stops at the cap
                // rather than a closer, the value is larger than the cap → truncated.
                let cap = s.saturating_add(COPY_MAX_BYTES);
                let scan_to = prov_end.min(cap);
                let e = skip_value(b, s, scan_to);
                let truncated = e >= cap && cap < prov_end;
                let slice = &b[s..e];
                // Trim trailing whitespace — the document root's range runs to EOF,
                // so it would otherwise carry the file's final newline.
                let cut = slice
                    .iter()
                    .rposition(|c| !c.is_ascii_whitespace())
                    .map_or(0, |p| p + 1);
                let slice = &slice[..cut];
                copy_to_clipboard(slice);
                let n = slice.len();
                if truncated {
                    format!(
                        "copied {n} B (truncated at {} KiB cap)",
                        COPY_MAX_BYTES >> 10
                    )
                } else {
                    format!("copied value ({n} B)")
                }
            }
            None => "nothing to copy".to_string(),
        }
    }

    /// Copy the path to the focused node (`data.users[3].city`) to the clipboard.
    fn yank_path(&self) -> String {
        let v = self.active_view();
        let Some(row) = v.rows.get(v.focus) else {
            return "nothing to copy".to_string();
        };
        let segs = breadcrumb_segments(&v.root, &row.path);
        let path = join_path("", &segs);
        copy_to_clipboard(path.as_bytes());
        format!("copied path: {path}")
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.views.iter().position(|v| v.id == id)
    }

    fn active_view(&self) -> &View {
        &self.views[self.active]
    }

    fn active_mut(&mut self) -> &mut View {
        &mut self.views[self.active]
    }

    /// True if any pane's last flatten yielded mid-skip (a huge value still being
    /// stepped over). The run loop then keeps flattening — polling input at 0 ms
    /// instead of blocking — until it clears, so a later sibling behind a giant
    /// value (e.g. `meta` after a 1 GB `users`) still appears on its own, just
    /// without the giant `skip_value` ever blocking a frame.
    fn flatten_pending(&self) -> bool {
        self.views.iter().any(|v| v.flatten_incomplete)
    }

    fn toggle_layout(&mut self) {
        self.stacked = !self.stacked;
    }

    fn grow_active(&mut self) {
        let w = &mut self.active_mut().weight;
        *w = (*w + 1).min(WEIGHT_MAX);
    }

    fn shrink_active(&mut self) {
        let w = &mut self.active_mut().weight;
        *w = (*w - 1).max(WEIGHT_MIN);
    }

    /// Each pane's layout weight, in order — the `Fill` factors handed to
    /// [`pane_layout`].
    fn weights(&self) -> Vec<u16> {
        self.views.iter().map(|v| v.weight).collect()
    }

    /// The active pane's content height (rows) for a given screen area — used to
    /// size paging jumps, since each pane reserves a title + footer row.
    fn active_height(&self, area: Rect) -> usize {
        let rects = pane_layout(panes_area(area), &self.weights(), self.stacked);
        (rects[self.active * 2].height as usize)
            .saturating_sub(1)
            .max(1)
    }

    fn next_pane(&mut self) {
        let n = self.views.len();
        if n > 1 {
            self.active = (self.active + 1) % n;
        }
    }

    fn prev_pane(&mut self) {
        let n = self.views.len();
        if n > 1 {
            self.active = (self.active + n - 1) % n;
        }
    }

    /// Close the active pane and every pane descending from it (split from it,
    /// transitively). Returns false if that would close every pane — the caller
    /// then quits. The closed pane's parent (if any) becomes active.
    fn close_active(&mut self) -> bool {
        if self.views.len() <= 1 {
            return false;
        }
        // Gather the active pane plus everything that descends from it.
        let mut doomed = HashSet::new();
        doomed.insert(self.views[self.active].id);
        loop {
            let mut grew = false;
            for v in &self.views {
                if let Some(p) = v.parent {
                    if doomed.contains(&p) && doomed.insert(v.id) {
                        grew = true;
                    }
                }
            }
            if !grew {
                break;
            }
        }
        if doomed.len() >= self.views.len() {
            return false; // active is the ancestor of all panes — quit instead
        }
        let parent = self.views[self.active].parent;
        self.views.retain(|v| !doomed.contains(&v.id));
        // Drop now-dangling preview links (their target was just closed).
        let alive: HashSet<u64> = self.views.iter().map(|v| v.id).collect();
        for v in &mut self.views {
            if v.preview_child.is_some_and(|c| !alive.contains(&c)) {
                v.preview_child = None;
            }
        }
        // Land on the closed pane's parent if it survived, else clamp.
        self.active = parent
            .and_then(|pid| self.index_of(pid))
            .unwrap_or_else(|| self.active.min(self.views.len() - 1));
        true
    }

    /// Build the (subroot, origin-label) for a child pane rooted at the active
    /// pane's focused node. `None` if that node isn't a non-empty container.
    fn make_child_root(&self, b: &[u8]) -> Option<(Node, String)> {
        let src = self.active_view();
        let row = src.rows.get(src.focus)?;
        let path = row.path.clone();
        let node = get(&src.root, &path);
        if !node.is_container() || !node.has_children {
            return None;
        }
        // Chain the origin label off a derived pane's own name; start fresh
        // (no filename prefix) for the document pane.
        let base = if src.derived {
            src.name.clone()
        } else {
            String::new()
        };
        let origin = join_path(&base, &breadcrumb_segments(&src.root, &path));
        // The subroot is a real bounded container, so it needs the node's *true*
        // end — `node.end` may be provisional (running to the parent's bound). Pay
        // the one-shot skip to the real closer here (a split is a deliberate, rare
        // action); display/scroll never need it.
        let end = if node.end_exact {
            node.end
        } else {
            skip_value(b, node.start, node.end)
        };
        let root = make_subroot(b, node.label.clone(), node.start, end, node.kind);
        Some((root, origin))
    }

    /// Split: open a *new* child pane rooted at the active pane's focused node and
    /// switch to it. The child links back to the parent, so closing the parent
    /// closes it too. No-op on a scalar/empty node.
    fn split_active(&mut self, b: &[u8]) {
        let Some((root, origin)) = self.make_child_root(b) else {
            return;
        };
        let parent = self.views[self.active].id;
        let id = self.alloc_id();
        let mut v = View::with_root(root, origin, true);
        v.id = id;
        v.parent = Some(parent);
        self.views.push(v);
        self.active = self.views.len() - 1;
    }

    /// Open-or-replace: re-root the active pane's preview child at the focused
    /// node (opening one the first time), and *stay on the parent* so you can keep
    /// browsing with a live detail pane. No-op on a scalar/empty node.
    fn preview_active(&mut self, b: &[u8]) {
        let Some((root, origin)) = self.make_child_root(b) else {
            return;
        };
        let parent = self.views[self.active].id;
        let existing = self.views[self.active]
            .preview_child
            .and_then(|cid| self.index_of(cid));
        if let Some(ci) = existing {
            self.views[ci].reroot(root, origin); // reuse the detail pane
        } else {
            let id = self.alloc_id();
            let mut v = View::with_root(root, origin, true);
            v.id = id;
            v.parent = Some(parent);
            self.views.push(v);
            self.views[self.active].preview_child = Some(id);
        }
    }
}

/// Walk the focus `path` from the root, collecting each ancestor's label and a
/// flag for whether it's an array element. The root (the filename) is excluded —
/// it's already shown at the start of the top bar.
fn breadcrumb_segments(root: &Node, path: &[usize]) -> Vec<(String, bool)> {
    let mut out = Vec::with_capacity(path.len());
    let mut n = root;
    for &i in path {
        if i >= n.children.len() {
            break; // path points past what's scanned (shouldn't happen for a row)
        }
        n = &n.children[i];
        out.push((n.label.clone(), n.is_index));
    }
    out
}

/// Copy `data` to the terminal clipboard via OSC 52. This goes through the
/// terminal itself (not a system clipboard library), so it works over SSH and
/// needs no platform-specific dependency. Written straight to stdout and flushed;
/// it doesn't touch the screen grid, so ratatui repaints normally next frame.
/// (Inside tmux this needs `set -g set-clipboard on`.)
fn copy_to_clipboard(data: &[u8]) {
    use std::io::Write;
    let seq = format!("\x1b]52;c;{}\x07", base64_encode(data));
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Minimal standard-base64 encoder (with `=` padding). Inlined to avoid pulling
/// in a crate just for the OSC 52 payload.
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Render the focus breadcrumb (`users › [2] › city`) as styled spans that fit
/// within `avail` columns, **left-truncating** with a leading `…` so the tail
/// nearest the focused node always stays visible. Array elements are bracketed;
/// the last (current) segment is bold.
fn breadcrumb_spans(segs: &[(String, bool)], avail: usize) -> Vec<Span<'static>> {
    if segs.is_empty() || avail == 0 {
        return Vec::new();
    }
    const SEP: &str = " › ";
    const SEP_W: usize = 3;
    let texts: Vec<String> = segs
        .iter()
        .map(|(l, is_idx)| if *is_idx { format!("[{l}]") } else { l.clone() })
        .collect();
    let widths: Vec<usize> = texts.iter().map(|t| t.chars().count()).collect();
    let total: usize = widths.iter().sum::<usize>() + SEP_W * segs.len().saturating_sub(1);

    // Find the first segment to show. 0 = the whole path fits, no ellipsis. Else
    // pick the smallest start ≥ 1 whose suffix (plus a leading `…`) fits.
    let mut start = 0usize;
    if total > avail {
        start = segs.len(); // nothing fits yet
        let mut acc = 0usize; // width of the accepted suffix (excluding the `…`)
        for i in (0..segs.len()).rev() {
            let add = SEP_W + widths[i]; // each shown segment gets a leading separator
            if 1 + acc + add <= avail {
                acc += add;
                start = i;
            } else {
                break;
            }
        }
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    if start > 0 {
        spans.push(Span::styled("…", Style::default().fg(C_PUNCT)));
        if start >= segs.len() {
            return spans; // too narrow for even the last segment — just the `…`
        }
    }
    for i in start..segs.len() {
        if i > 0 || start > 0 {
            spans.push(Span::styled(SEP, Style::default().fg(C_PUNCT)));
        }
        let color = if segs[i].1 { C_INDEX } else { C_KEY };
        let mut st = Style::default().fg(color);
        if i == segs.len() - 1 {
            st = st.add_modifier(Modifier::BOLD); // the focused node
        }
        spans.push(Span::styled(texts[i].clone(), st));
    }
    spans
}

/// Split the screen into pane rects interleaved with 1-cell separator rects:
/// `[pane0, sep, pane1, sep, …]`. Side by side (columns) by default, stacked
/// (rows) when `stacked`. Each pane is a `Fill(weight)`, so the leftover space
/// (after the fixed-size separators) divides in proportion to the weights.
fn pane_layout(area: Rect, weights: &[u16], stacked: bool) -> std::rc::Rc<[Rect]> {
    let n = weights.len();
    let mut constraints = Vec::with_capacity(n * 2 - 1);
    for (i, &w) in weights.iter().enumerate() {
        if i > 0 {
            constraints.push(Constraint::Length(1)); // separator row/column
        }
        constraints.push(Constraint::Fill(w.max(1)));
    }
    if stacked {
        Layout::vertical(constraints).split(area)
    } else {
        Layout::horizontal(constraints).split(area)
    }
}

/// The rule between panes: a vertical `│` column (side by side) or a horizontal
/// `─` row (stacked).
fn render_separator(f: &mut Frame, sep: Rect, stacked: bool) {
    let style = Style::default().fg(C_PUNCT);
    if stacked {
        let rule = "─".repeat(sep.width as usize);
        f.render_widget(Paragraph::new(Line::from(Span::styled(rule, style))), sep);
    } else {
        let rule: Vec<Line> = (0..sep.height)
            .map(|_| Line::from(Span::styled("│", style)))
            .collect();
        f.render_widget(Paragraph::new(rule), sep);
    }
}

/// Draw every pane (side by side or stacked, per `app.stacked`), separated by a
/// rule. `streaming` only affects the (non-derived) document pane's title.
/// The vertical span used for panes: everything above the global footer row.
fn panes_area(area: Rect) -> Rect {
    Rect {
        height: area.height.saturating_sub(1),
        ..area
    }
}

/// Draw every pane (side by side or stacked) above a single global footer that
/// spans the full width. `streaming` only affects the (non-derived) document
/// pane's title.
fn ui(f: &mut Frame, app: &App, streaming: bool) {
    let area = f.area();
    let n = app.views.len();
    let rects = pane_layout(panes_area(area), &app.weights(), app.stacked);
    for i in 0..n {
        if i > 0 {
            render_separator(f, rects[i * 2 - 1], app.stacked);
        }
        let view = &app.views[i];
        render_pane(
            f,
            rects[i * 2],
            view,
            i == app.active,
            streaming && !view.derived,
        );
    }
    // One global footer at the very bottom, reflecting the active pane's mode.
    let footer_row = Rect {
        y: area.y + area.height.saturating_sub(1),
        height: 1,
        ..area
    };
    render_footer(f, footer_row, app.active_view(), app.flash.as_deref());

    // An overlay floats over everything when open (only one at a time).
    match app.active_view().mode {
        Mode::Marks => render_marks(f, area, app.active_view()),
        Mode::Help => render_help(f, area),
        _ => {}
    }
}

/// The single global key/search-status bar, reflecting the active pane. A
/// transient `flash` (e.g. a copy confirmation) takes the bar over the key hint.
fn render_footer(f: &mut Frame, area: Rect, view: &View, flash: Option<&str>) {
    let footer = if view.mode == Mode::Search {
        let count = view.search.as_ref().map_or(0, |s| s.matches.len());
        let more = match &view.search {
            Some(s) if !s.finished => "+",
            _ => "",
        };
        // Show pattern-parse failures verbatim so the user can fix the typed
        // `re:`/`g:` expression. Else show position (after landing) or running
        // total.
        let pos = if let Some(err) = view.query_error.as_deref() {
            format!("({err})")
        } else if view.landed && count > 0 {
            format!("{}/{}{}", view.match_idx + 1, count, more)
        } else {
            format!("{}{} matches", count, more)
        };
        Span::styled(
            format!(
                " /{}   {} · ↵/↓ next · ⇧↵/↑ prev · esc close",
                view.query, pos
            ),
            Style::default().fg(Color::Yellow),
        )
    } else if view.mode == Mode::Goto {
        Span::styled(
            format!(" :{}   ↵ jump · esc cancel", view.goto),
            Style::default().fg(Color::Yellow),
        )
    } else if let Some(msg) = flash {
        Span::styled(format!(" {msg}"), Style::default().fg(Color::Green))
    } else {
        Span::styled(
            " ↑/↓ move · enter expand · / search · : goto · y copy · ? help · q quit",
            Style::default().fg(Color::DarkGray),
        )
    };
    f.render_widget(Paragraph::new(Line::from(footer)), area);
}

/// Draw the bookmark picker as a centered overlay listing each saved node's
/// path; the selected row is highlighted. Jumped/edited via [`process_key`].
fn render_marks(f: &mut Frame, area: Rect, view: &View) {
    let items: Vec<String> = view
        .bookmarks
        .iter()
        .map(|p| join_path("", &breadcrumb_segments(&view.root, p)))
        .collect();
    // Size to the wider of the longest bookmark or the title, so the title can't
    // be truncated to "… d de" on a narrow box.
    let title = " bookmarks · ↵ jump · d delete · esc ";
    let title_w = title.chars().count();
    let longest = items.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    let inner_w = longest
        .max(title_w)
        .min(area.width.saturating_sub(4) as usize);
    let w = inner_w as u16 + 4;
    let h = (items.len() as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(3);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    // Read as a floating card above the dimmed content: a solid dark fill with a
    // bright, bold border/title, and a full-width selection bar (padded out so the
    // highlight spans the row instead of clipping to the label).
    let panel_bg = Color::Indexed(236); // dark gray; degrades gracefully on 16-color terms
    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let pad = " ".repeat(inner_w.saturating_sub(s.chars().count()));
            let text = format!(" {s}{pad} ");
            let st = if i == view.mark_idx {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray).bg(panel_bg)
            };
            Line::from(Span::styled(text, st))
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(panel_bg))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// Draw the keyboard-shortcut cheatsheet as a centered overlay (opened with `?`,
/// closed by any key). Two columns of key → action in the same floating-card
/// style as the bookmark picker — this is where the keys trimmed off the footer
/// live.
fn render_help(f: &mut Frame, area: Rect) {
    const ENTRIES: &[(&str, &str)] = &[
        ("↑/↓  k/j", "move focus"),
        ("J / K", "next / prev sibling"),
        ("PgUp/PgDn", "page up / down"),
        ("Ctrl-D/U", "half page"),
        ("g  Home", "jump to top"),
        ("Enter  →", "expand / collapse"),
        ("←", "collapse / parent"),
        ("wheel", "scroll the pane"),
        ("/", "search · re:rx · g:glob"),
        ("↵  ⇧↵", "next / prev match"),
        (":", "jump to a path · *? in keys"),
        ("m", "toggle bookmark"),
        ("'", "bookmark picker"),
        ("y / Y", "copy value / path"),
        ("p", "pipe node to stdout"),
        ("s", "split pane at node"),
        ("o", "preview pane"),
        ("\\", "toggle pane layout"),
        ("+ / -", "grow / shrink pane"),
        ("Tab/⇧Tab", "switch pane"),
        ("x", "close pane"),
        ("?", "toggle this help"),
        ("q  Esc", "close pane / quit"),
    ];
    let key_w = ENTRIES
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    let desc_w = ENTRIES
        .iter()
        .map(|(_, d)| d.chars().count())
        .max()
        .unwrap_or(0);
    let rows = ENTRIES.len().div_ceil(2);

    let key_st = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let desc_st = Style::default().fg(Color::Gray);
    let panel_bg = Color::Indexed(236);

    // One "key  description" cell, key right-aligned so the columns line up.
    let cell = |k: &str, d: &str| -> Vec<Span<'static>> {
        vec![
            Span::styled(format!(" {k:>key_w$}  "), key_st),
            Span::styled(format!("{d:<desc_w$} "), desc_st),
        ]
    };

    // Pair entry i with entry i+rows, so the list reads top-to-bottom per column.
    let lines: Vec<Line> = (0..rows)
        .map(|r| {
            let (k1, d1) = ENTRIES[r];
            let mut spans = cell(k1, d1);
            if let Some(&(k2, d2)) = ENTRIES.get(r + rows) {
                spans.push(Span::styled("  ", desc_st));
                spans.extend(cell(k2, d2));
            }
            Line::from(spans)
        })
        .collect();

    let title = " keyboard shortcuts · any key to close ";
    let col_w = key_w + desc_w + 4; // " " + key + "  " + desc + " "
    let inner_w = (col_w * 2 + 2)
        .max(title.chars().count())
        .min(area.width.saturating_sub(2) as usize);
    let w = inner_w as u16 + 2;
    let h = (rows as u16 + 2).min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(panel_bg))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// Braille spinner frames for the inline "still scanning a huge value" row.
/// Advanced by wall-clock time so it animates across the drain's repaints without
/// threading a frame counter through the render path.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner_frame() -> &'static str {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    SPINNER[(ms / 80) as usize % SPINNER.len()]
}

/// Draw one pane (title + breadcrumb, then content rows) into `rect`. The active
/// pane gets a bright title and is the only one to show its cursor bar; the
/// key/search footer is global (see [`render_footer`]), not per-pane.
fn render_pane(f: &mut Frame, rect: Rect, view: &View, active: bool, streaming: bool) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(rect);

    // Title: `↳ origin   focus/rows+` plus the focus breadcrumb. Count only real
    // rows (the trailing loading placeholder isn't a node), and clamp the shown
    // position to that count.
    let marker = if view.derived { "↳ " } else { "" };
    let n_rows = view.rows.iter().filter(|r| !r.loading).count();
    let pos = (view.focus + 1).min(n_rows.max(1));
    let mut prefix = format!(" {marker}{}   {}/{}+", view.name, pos, n_rows);
    if streaming {
        prefix.push_str("   ⟳ streaming");
    }
    let prefix_w = prefix.chars().count();
    let title_style = if active {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let mut title = vec![Span::styled(prefix, title_style)];
    // Breadcrumb to the focused row, left-truncated to whatever space is left.
    let segs = view
        .rows
        .get(view.focus)
        .map(|r| breadcrumb_segments(&view.root, &r.path))
        .unwrap_or_default();
    if !segs.is_empty() {
        const GAP: usize = 3;
        let avail = (chunks[0].width as usize).saturating_sub(prefix_w + GAP);
        let crumb = breadcrumb_spans(&segs, avail);
        if !crumb.is_empty() {
            title.push(Span::raw("   "));
            title.extend(crumb);
        }
    }
    f.render_widget(Paragraph::new(Line::from(title)), chunks[0]);

    let cur_match = view
        .search
        .as_ref()
        .and_then(|s| s.matches.get(view.match_idx));

    let h = chunks[1].height as usize;
    let mut lines = Vec::new();
    let end = (view.scroll + h).min(view.rows.len());
    for i in view.scroll..end {
        let r = &view.rows[i];
        if r.loading {
            // Inline drain indicator: a dim, animated "⠋ loading…" at the child
            // indent, where the next sibling will appear once it's scanned in.
            let indent = "  ".repeat(r.depth);
            lines.push(Line::from(Span::styled(
                format!("{indent}{} loading…", spinner_frame()),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            )));
            continue;
        }
        let marker = if r.has_children {
            if r.expanded {
                "▼"
            } else {
                "▶"
            }
        } else {
            " "
        };
        let indent = "  ".repeat(r.depth);

        let line = if active && i == view.focus {
            // Selection bar: only the active pane shows its cursor, so the one
            // highlighted row unambiguously marks which pane is live. Inactive
            // panes keep their focus (it returns on Tab) but draw it as a normal
            // row.
            let text = format!("{indent}{marker} {}: {}", r.label, r.value);
            Line::from(Span::styled(
                text,
                Style::default().add_modifier(Modifier::REVERSED),
            ))
        } else if cur_match == Some(&r.path) || view.match_set.contains(&r.path) {
            // A search hit: whole row yellow (current match also bold).
            let text = format!("{indent}{marker} {}: {}", r.label, r.value);
            let mut st = Style::default().fg(Color::Yellow);
            if cur_match == Some(&r.path) {
                st = st.add_modifier(Modifier::BOLD);
            }
            Line::from(Span::styled(text, st))
        } else {
            // Normal row: syntax-colored segments.
            let key_color = if r.is_index { C_INDEX } else { C_KEY };
            Line::from(vec![
                Span::raw(indent),
                Span::styled(marker, Style::default().fg(C_PUNCT)),
                Span::raw(" "),
                Span::styled(r.label.clone(), Style::default().fg(key_color)),
                Span::styled(": ", Style::default().fg(C_PUNCT)),
                Span::styled(r.value.clone(), Style::default().fg(value_color(r.kind))),
            ])
        };
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), chunks[1]);
}

/// Flatten the visible window, land any pending jump, clamp focus/scroll, and
/// draw one frame. Shared by the file and streaming loops.
fn render_frame(
    term: &mut ratatui::DefaultTerminal,
    app: &mut App,
    b: &[u8],
    streaming: bool,
) -> std::io::Result<()> {
    // Each pane flattens to its own content height (which depends on orientation
    // and pane count), so a stacked pane only walks its shorter window. Each pane
    // reserves one title row; the footer is global, below the panes.
    let rects = pane_layout(panes_area(term_area()?), &app.weights(), app.stacked);
    for (i, v) in app.views.iter_mut().enumerate() {
        let h = (rects[i * 2].height as usize).saturating_sub(1).max(1);
        v.flatten_window(b, h);
    }
    term.draw(|f| ui(f, app, streaming))?;
    Ok(())
}

/// Apply one key press. Everything self-contained happens here; search
/// (re)launch is deferred to the caller via `KeyOutcome::Relaunch` because it
/// needs a byte `Source`.
fn process_key(
    app: &mut App,
    k: ratatui::crossterm::event::KeyEvent,
    b: &[u8],
    h: usize,
) -> KeyOutcome {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);

    // Search mode belongs to the active pane. (Pane-switching keys are normal-mode
    // only, so at most the active pane is ever in search mode.)
    if app.active_view().mode == Mode::Search {
        let v = app.active_mut();
        // Find-box style: the overlay stays open so matches cycle in place —
        // Enter / ↓ next, Shift+Enter / ↑ prev, typing refines live, Esc closes.
        match k.code {
            KeyCode::Esc => {
                v.mode = Mode::Normal;
                v.clear_search();
            }
            KeyCode::Enter if shift => v.nav_match(-1, b),
            KeyCode::Enter | KeyCode::Down => v.nav_match(1, b),
            KeyCode::Up => v.nav_match(-1, b),
            KeyCode::Backspace => {
                v.query.pop();
                return KeyOutcome::Relaunch;
            }
            KeyCode::Char(c) => {
                v.query.push(c);
                return KeyOutcome::Relaunch;
            }
            _ => {}
        }
        return KeyOutcome::Continue;
    }

    // Goto mode: a `:` path prompt. Enter resolves and jumps; Esc cancels.
    if app.active_view().mode == Mode::Goto {
        match k.code {
            KeyCode::Esc => {
                let v = app.active_mut();
                v.mode = Mode::Normal;
                v.goto.clear();
            }
            KeyCode::Enter => {
                let msg = {
                    let v = app.active_mut();
                    let msg = v.goto_path(b);
                    v.mode = Mode::Normal;
                    v.goto.clear();
                    msg
                };
                app.flash = Some(msg);
            }
            KeyCode::Backspace => {
                app.active_mut().goto.pop();
            }
            KeyCode::Char(c) => {
                app.active_mut().goto.push(c);
            }
            _ => {}
        }
        return KeyOutcome::Continue;
    }

    // Marks mode: the bookmark picker overlay. j/k move, Enter jumps, d deletes.
    if app.active_view().mode == Mode::Marks {
        let v = app.active_mut();
        match k.code {
            KeyCode::Esc | KeyCode::Char('\'') | KeyCode::Char('q') => v.mode = Mode::Normal,
            KeyCode::Down | KeyCode::Char('j') => {
                if !v.bookmarks.is_empty() {
                    v.mark_idx = (v.mark_idx + 1) % v.bookmarks.len();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !v.bookmarks.is_empty() {
                    let n = v.bookmarks.len();
                    v.mark_idx = (v.mark_idx + n - 1) % n;
                }
            }
            KeyCode::Char('d') => {
                if v.mark_idx < v.bookmarks.len() {
                    v.bookmarks.remove(v.mark_idx);
                    if v.mark_idx >= v.bookmarks.len() {
                        v.mark_idx = v.bookmarks.len().saturating_sub(1);
                    }
                    if v.bookmarks.is_empty() {
                        v.mode = Mode::Normal;
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(p) = v.bookmarks.get(v.mark_idx).cloned() {
                    v.jump_to(&p, b);
                }
                v.mode = Mode::Normal;
            }
            _ => {}
        }
        return KeyOutcome::Continue;
    }

    // Help mode: the shortcut cheatsheet. Any key dismisses it.
    if app.active_view().mode == Mode::Help {
        app.active_mut().mode = Mode::Normal;
        return KeyOutcome::Continue;
    }

    // Any normal-mode key dismisses the previous flash (copy status, …); copy
    // keys below set a fresh one.
    app.flash = None;

    // Normal mode: workspace-level keys (pane management) first.
    match k.code {
        // Copy the focused value (raw JSON / subtree) or its path to the clipboard.
        KeyCode::Char('y') => {
            app.flash = Some(app.yank_value(b));
            return KeyOutcome::Continue;
        }
        KeyCode::Char('Y') => {
            app.flash = Some(app.yank_path());
            return KeyOutcome::Continue;
        }
        // `p` pipes the focused node's raw JSON out: it records the range and
        // quits, and the caller writes that slice to the reserved stdout. Only
        // meaningful when stdout is redirected (`jview … | jq`) — into a
        // terminal there's nowhere to pipe, so show a hint instead of quitting.
        KeyCode::Char('p') => {
            if !app.can_extract {
                app.flash =
                    Some("pipe jview into a command (e.g. | jq) to extract a node".to_string());
                return KeyOutcome::Continue;
            }
            match app.focused_range() {
                Some(range) => {
                    app.extract = Some(range);
                    return KeyOutcome::Quit;
                }
                None => {
                    app.flash = Some("nothing to extract".to_string());
                    return KeyOutcome::Continue;
                }
            }
        }
        // `:` opens the path-jump prompt; `m` bookmarks the focused node; `'`
        // opens the bookmark picker.
        KeyCode::Char(':') => {
            let v = app.active_mut();
            v.mode = Mode::Goto;
            v.goto.clear();
            return KeyOutcome::Continue;
        }
        KeyCode::Char('m') => {
            app.flash = Some(app.active_mut().toggle_bookmark());
            return KeyOutcome::Continue;
        }
        // `?` opens the full keyboard-shortcut cheatsheet.
        KeyCode::Char('?') => {
            app.active_mut().mode = Mode::Help;
            return KeyOutcome::Continue;
        }
        KeyCode::Char('\'') => {
            if app.active_view().bookmarks.is_empty() {
                app.flash = Some("no bookmarks — press m to add one".to_string());
            } else {
                let v = app.active_mut();
                v.mark_idx = v.mark_idx.min(v.bookmarks.len() - 1);
                v.mode = Mode::Marks;
            }
            return KeyOutcome::Continue;
        }
        // q / Esc close the active pane, quitting only when it's the last one.
        KeyCode::Char('q') | KeyCode::Esc => {
            if !app.close_active() {
                return KeyOutcome::Quit;
            }
            return KeyOutcome::Continue;
        }
        KeyCode::Char('x') => {
            app.close_active();
            return KeyOutcome::Continue;
        }
        KeyCode::Char('s') => {
            app.split_active(b);
            return KeyOutcome::Continue;
        }
        KeyCode::Char('o') => {
            app.preview_active(b);
            return KeyOutcome::Continue;
        }
        KeyCode::Char('\\') => {
            app.toggle_layout();
            return KeyOutcome::Continue;
        }
        // Grow / shrink the active pane. `=`/`_` are the unshifted twins of
        // `+`/`-`, so both forms work whatever the terminal reports.
        KeyCode::Char('+') | KeyCode::Char('=') => {
            app.grow_active();
            return KeyOutcome::Continue;
        }
        KeyCode::Char('-') | KeyCode::Char('_') => {
            app.shrink_active();
            return KeyOutcome::Continue;
        }
        KeyCode::Tab => {
            app.next_pane();
            return KeyOutcome::Continue;
        }
        KeyCode::BackTab => {
            app.prev_pane();
            return KeyOutcome::Continue;
        }
        _ => {}
    }

    // Per-pane navigation goes to the active pane.
    let v = app.active_mut();
    match k.code {
        KeyCode::Char('/') => {
            v.mode = Mode::Search;
            v.query.clear();
            return KeyOutcome::Relaunch;
        }
        KeyCode::Down | KeyCode::Char('j') => v.focus += 1,
        KeyCode::Up | KeyCode::Char('k') => v.focus = v.focus.saturating_sub(1),
        // Same-level navigation: J/K hop to the next/previous sibling, stepping
        // over the focused node's subtree.
        KeyCode::Char('J') => v.nav_sibling(b, true),
        KeyCode::Char('K') => v.nav_sibling(b, false),
        // Paging: PageUp/PageDown plus Ctrl-F/B (full screen) and Ctrl-D/U (half),
        // for keyboards without dedicated Page keys.
        KeyCode::PageDown => v.focus += h,
        KeyCode::PageUp => v.focus = v.focus.saturating_sub(h),
        KeyCode::Char('f') if ctrl => v.focus += h,
        KeyCode::Char('b') if ctrl => v.focus = v.focus.saturating_sub(h),
        KeyCode::Char('d') if ctrl => v.focus += h / 2,
        KeyCode::Char('u') if ctrl => v.focus = v.focus.saturating_sub(h / 2),
        KeyCode::Home | KeyCode::Char('g') => {
            v.focus = 0;
            v.scroll = 0;
        }
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right => v.toggle_focus(),
        KeyCode::Left => v.collapse_or_parent(),
        _ => {}
    }
    KeyOutcome::Continue
}

fn term_area() -> std::io::Result<Rect> {
    let (w, h) = ratatui::crossterm::terminal::size()?;
    Ok(Rect::new(0, 0, w, h))
}

/// Which pane (index) the screen cell `(col, row)` falls in, if any — for
/// routing a mouse-wheel scroll to the pane under the cursor.
fn pane_at(app: &App, col: u16, row: u16) -> std::io::Result<Option<usize>> {
    let rects = pane_layout(panes_area(term_area()?), &app.weights(), app.stacked);
    for i in 0..app.views.len() {
        let r = rects[i * 2];
        if col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

/// Drain the whole pending input burst (coalescing), dispatching each event, so
/// the caller renders once per burst. Holding a key or spinning the wheel then
/// moves many rows per frame instead of one row per redraw — the lag fix. The
/// first poll blocks up to `poll_ms` so streaming match counts keep ticking when
/// idle. The `Relaunch` outcome is coalesced (search relaunches once after the
/// burst) and returned to the caller, which owns the byte `Source`.
fn pump_input(app: &mut App, b: &[u8], h: usize, poll_ms: u64) -> std::io::Result<KeyOutcome> {
    if !event::poll(Duration::from_millis(poll_ms))? {
        return Ok(KeyOutcome::Continue);
    }
    let mut outcome = KeyOutcome::Continue;
    loop {
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => match process_key(app, k, b, h) {
                KeyOutcome::Quit => return Ok(KeyOutcome::Quit),
                KeyOutcome::Relaunch => outcome = KeyOutcome::Relaunch,
                KeyOutcome::Continue => {}
            },
            Event::Mouse(m) => {
                let step = match m.kind {
                    MouseEventKind::ScrollDown => WHEEL_STEP as isize,
                    MouseEventKind::ScrollUp => -(WHEEL_STEP as isize),
                    _ => 0,
                };
                if step != 0 {
                    if let Some(i) = pane_at(app, m.column, m.row)? {
                        let v = &mut app.views[i];
                        v.focus = if step > 0 {
                            v.focus + step as usize
                        } else {
                            v.focus.saturating_sub((-step) as usize)
                        };
                    }
                }
            }
            _ => {}
        }
        // Stop once the input queue is empty, then render the coalesced result.
        if !event::poll(Duration::from_millis(0))? {
            break;
        }
    }
    Ok(outcome)
}

/// The file (mmap) / fully-buffered loop: one fixed byte source for the session.
fn run(
    term: &mut ratatui::DefaultTerminal,
    app: &mut App,
    b: &[u8],
    mmap: &Arc<Source>,
) -> std::io::Result<()> {
    loop {
        // Fold in any matches the worker threads have produced since last frame.
        for v in &mut app.views {
            v.pump_search();
        }
        render_frame(term, app, b, false)?;
        let h = app.active_height(term_area()?);
        // While a flatten is mid-skip, don't block on input: poll at 0 ms and loop
        // so the next frame steps the skip another `FLATTEN_BUDGET` (still
        // dispatching any key pressed meanwhile). Otherwise a short poll so
        // streaming match counts keep ticking even when idle.
        let poll_ms = if app.flatten_pending() { 0 } else { 100 };
        match pump_input(app, b, h, poll_ms)? {
            KeyOutcome::Quit => return Ok(()),
            KeyOutcome::Relaunch => app.active_mut().relaunch(mmap),
            KeyOutcome::Continue => {}
        }
    }
}

/// How often a growing stream is re-parsed into the tree. Short enough to feel
/// live, long enough not to thrash on a fast pipe.
const STREAM_REBUILD_MS: u128 = 100;

/// The streaming loop: bytes arrive on `rx` from the reader thread, the buffer
/// grows, and the tree is re-parsed on a throttle (preserving cursor/expansion).
/// Search snapshots the buffer at launch (so it covers the bytes parsed so far).
fn run_stream(
    term: &mut ratatui::DefaultTerminal,
    app: &mut App,
    buf: &mut Vec<u8>,
    jsonl: &mut bool,
    rx: Receiver<Vec<u8>>,
) -> std::io::Result<()> {
    let mut dirty = false; // bytes arrived that aren't in the tree yet
    let mut done = false; // reader hit EOF
    let mut last_build = Instant::now();
    loop {
        // Drain whatever the reader thread has produced.
        loop {
            match rx.try_recv() {
                Ok(chunk) => {
                    buf.extend_from_slice(&chunk);
                    dirty = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    done = true;
                    break;
                }
            }
        }
        // NDJSON detection is sticky: once multi-doc, stay multi-doc.
        if !*jsonl && sniff_multi(buf) {
            *jsonl = true;
        }
        // Re-parse on a throttle, or immediately once the stream is complete.
        // Only the document pane tracks the growing buffer; split panes are
        // snapshots of the bytes that had arrived when they were spun off.
        if dirty && (done || last_build.elapsed().as_millis() >= STREAM_REBUILD_MS) {
            if let Some(main) = app.views.iter_mut().find(|v| !v.derived) {
                main.rebuild(buf, *jsonl);
            }
            dirty = false;
            last_build = Instant::now();
        }

        for v in &mut app.views {
            v.pump_search();
        }
        // Borrow the (possibly grown) buffer just for this frame's render + input,
        // then release it so a search relaunch can snapshot it below.
        let outcome = {
            let b: &[u8] = buf;
            render_frame(term, app, b, !done)?;
            let h = app.active_height(term_area()?);
            // 0 ms while a flatten is mid-skip (resume the skip next frame); 100 ms
            // otherwise so the buffer-rebuild throttle keeps ticking when idle.
            let poll_ms = if app.flatten_pending() { 0 } else { 100 };
            pump_input(app, b, h, poll_ms)?
        };
        match outcome {
            KeyOutcome::Quit => return Ok(()),
            KeyOutcome::Relaunch => {
                // Search over the bytes parsed so far (the stream keeps growing,
                // but one search covers what's arrived at its launch).
                let snap = Arc::new(Source::Buffered(buf.clone()));
                app.active_mut().relaunch(&snap);
            }
            KeyOutcome::Continue => {}
        }
    }
}

/// Sniff whether a buffer holds more than one top-level JSON value — NDJSON or
/// concatenated JSON — rather than a single document. Used for piped stdin,
/// which has no filename extension to key off. Cheap: it skips exactly one
/// value and checks whether anything non-whitespace follows.
fn sniff_multi(b: &[u8]) -> bool {
    let i = skip_ws(b, 0, b.len());
    if i >= b.len() {
        return false;
    }
    let after = skip_value(b, i, b.len());
    skip_ws(b, after, b.len()) < b.len()
}

/// Point fd 0 back at the real terminal so crossterm reads keys from it (the
/// reader thread keeps its own dup'd handle to the pipe for the JSON).
///
/// crossterm's own fallback — open `/dev/tty` when stdin isn't a tty — is fatal
/// on macOS: a `/dev/tty` fd can't be registered with kqueue (it returns
/// EINVAL), so the key-event reader never initializes and the first keypress
/// errors out. The actual terminal device *can* be registered, so resolve its
/// path via `ttyname()` on stdout/stderr (still ttys here — only stdin was
/// piped), open it read-write, and dup it over stdin. Then `isatty(0)` is true,
/// crossterm uses fd 0 directly, and kqueue accepts it. Linux's `/dev/tty` is
/// pollable so this is redundant there, but it's harmless.
#[cfg(unix)]
fn reattach_terminal_to_stdin() {
    unsafe {
        for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            if libc::isatty(fd) != 1 {
                continue;
            }
            let name = libc::ttyname(fd);
            if name.is_null() {
                continue;
            }
            let f = libc::open(name, libc::O_RDWR);
            if f >= 0 {
                libc::dup2(f, libc::STDIN_FILENO);
                if f != libc::STDIN_FILENO {
                    libc::close(f);
                }
                return;
            }
        }
    }
}

/// Hand back a readable handle to the piped stdin for the background reader.
///
/// On unix the reader needs its own fd because we hand fd 0 back to the
/// terminal (see `reattach_terminal_to_stdin`): dup the pipe first, then
/// reattach. On Windows, crossterm reads keys from the console (not stdin), so
/// the reader can just take stdin directly.
#[cfg(unix)]
fn take_pipe_reader() -> Box<dyn Read + Send> {
    use std::os::fd::FromRawFd;
    unsafe {
        let dup = libc::dup(libc::STDIN_FILENO);
        reattach_terminal_to_stdin();
        Box::new(File::from_raw_fd(dup))
    }
}

#[cfg(not(unix))]
fn take_pipe_reader() -> Box<dyn Read + Send> {
    Box::new(std::io::stdin())
}

/// Read the pipe in chunks on a background thread, streaming each to the UI loop
/// over a channel. The thread ends (and the channel disconnects, signalling EOF)
/// when the pipe closes or the receiver is dropped.
fn spawn_reader(mut reader: Box<dyn Read + Send>) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break; // receiver gone — viewer quit
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    rx
}

/// Ask the terminal to report modifiers on keys it normally wouldn't (so
/// Shift+Enter is distinguishable from Enter). Only terminals implementing the
/// Kitty keyboard protocol support this — most modern ones do (Kitty, WezTerm,
/// foot, Ghostty, recent iTerm2/Konsole/VTE); on others it's a no-op and
/// Shift+Enter falls back to ↑ for previous-match. Returns whether it was
/// enabled (so it can be popped on exit).
fn enable_enhanced_keys() -> bool {
    use ratatui::crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
    use ratatui::crossterm::execute;
    use ratatui::crossterm::terminal::supports_keyboard_enhancement;
    // `supports_keyboard_enhancement()` sends a query and blocks until the
    // terminal replies (or a ~2s timeout). Terminals that never answer — ttyd,
    // some multiplexers/pty wrappers — stall startup for that whole window.
    // JVIEW_NO_ENHANCED_KEYS=1 skips the probe (used by the demo recording).
    if std::env::var_os("JVIEW_NO_ENHANCED_KEYS").is_some() {
        return false;
    }
    if supports_keyboard_enhancement().unwrap_or(false) {
        let _ = execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
        true
    } else {
        false
    }
}

fn disable_enhanced_keys() {
    use ratatui::crossterm::event::PopKeyboardEnhancementFlags;
    use ratatui::crossterm::execute;
    let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
}

/// Capture the mouse so wheel events reach the app (for scrolling). Like tmux,
/// this takes over the terminal's native selection — hold Shift to select text.
fn enable_mouse() {
    use ratatui::crossterm::event::EnableMouseCapture;
    use ratatui::crossterm::execute;
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
}

fn disable_mouse() {
    use ratatui::crossterm::event::DisableMouseCapture;
    use ratatui::crossterm::execute;
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
}

/// First-run footer hint shown when stdout is piped, so the `p` extract key is
/// discoverable; it clears on the first keypress like any other flash.
const EXTRACT_HINT: &str = "output piped — press p to extract the focused node into it";

/// Where an extracted node's JSON goes when the viewer exits after `p`.
///
/// A full-screen TUI and clean piped data can't share fd 1. When stdout is
/// redirected (`jview f.json | jq`, `> out.json`) we dup the real stdout aside
/// here and point fd 1 at the controlling terminal, so every existing
/// `stdout()` render/escape lands on the tty while the pipe stays pristine for
/// the payload. When stdout is already a terminal there's nothing to pipe into,
/// so `enabled` is false and `p` shows a hint instead of extracting.
struct Payload {
    enabled: bool,
    /// The dup'd real stdout (unix, redirected case). `None` means "write to the
    /// process's own stdout" — the terminal case, or non-unix.
    file: Option<File>,
}

impl Payload {
    /// Write a node's raw JSON to the reserved sink, with a trailing newline so
    /// the output is a clean line for `jq` / a shell / a file. Uncapped, unlike
    /// the clipboard copy: piping a large subtree is the whole point, and the
    /// slice is a zero-copy view into the mmap.
    fn emit(self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let mut w: Box<dyn Write> = match self.file {
            Some(f) => Box::new(f),
            None => Box::new(std::io::stdout()),
        };
        w.write_all(bytes)?;
        w.write_all(b"\n")?;
        w.flush()
    }
}

/// Reserve stdout for an extracted node's JSON, repointing the TUI at the tty
/// when stdout is redirected. See [`Payload`].
#[cfg(unix)]
fn reserve_stdout_for_payload() -> Payload {
    use std::os::fd::FromRawFd;
    unsafe {
        // stdout is a terminal → nothing to pipe into; render there as usual.
        if libc::isatty(libc::STDOUT_FILENO) == 1 {
            return Payload {
                enabled: false,
                file: None,
            };
        }
        // stdout is a pipe/file: save it for the payload, then repoint fd 1 at
        // the controlling terminal (found via an fd that still points at it —
        // stdin is the terminal here for a file arg, or reattached for a pipe).
        let saved = libc::dup(libc::STDOUT_FILENO);
        if saved >= 0 {
            for fd in [libc::STDIN_FILENO, libc::STDERR_FILENO] {
                if libc::isatty(fd) != 1 {
                    continue;
                }
                let name = libc::ttyname(fd);
                if name.is_null() {
                    continue;
                }
                let tty = libc::open(name, libc::O_WRONLY);
                if tty >= 0 {
                    libc::dup2(tty, libc::STDOUT_FILENO);
                    if tty != libc::STDOUT_FILENO {
                        libc::close(tty);
                    }
                    return Payload {
                        enabled: true,
                        file: Some(File::from_raw_fd(saved)),
                    };
                }
            }
            // No tty to render to (output redirected *and* no controlling
            // terminal): the viewer can't run usefully anyway. Drop the dup and
            // leave stdout as-is.
            libc::close(saved);
        }
        Payload {
            enabled: false,
            file: None,
        }
    }
}

#[cfg(not(unix))]
fn reserve_stdout_for_payload() -> Payload {
    // On Windows crossterm renders to the console handle, separate from a
    // redirected stdout pipe, so fd 1 can stay put and still carry clean output.
    Payload {
        enabled: !std::io::stdout().is_terminal(),
        file: None,
    }
}

/// Resolve the focused node's real byte range (a collapsed container's end is
/// provisional) and write its whitespace-trimmed JSON to the payload sink.
fn write_extract(
    b: &[u8],
    (start, prov_end): (usize, usize),
    sink: Payload,
) -> std::io::Result<()> {
    let end = skip_value(b, start, prov_end);
    let slice = &b[start..end];
    // The document root's range runs to EOF; trim so a trailing file newline
    // (or container padding) doesn't ride along before our own newline.
    let cut = slice
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map_or(0, |p| p + 1);
    sink.emit(&slice[..cut])
}

/// Open a file via mmap (zero-copy, near-constant memory) and run the viewer.
fn run_file(path: String) -> std::io::Result<()> {
    // NDJSON / JSON Lines detected by extension (cheap — a content sniff would
    // have to scan the whole file and defeat the lazy open).
    let lower = path.to_ascii_lowercase();
    let jsonl = [".jsonl", ".ndjson", ".ldjson", ".jsonlines"]
        .iter()
        .any(|ext| lower.ends_with(ext));
    let file = File::open(&path)?;
    // SAFETY: the file isn't mutated while mapped for the viewer's lifetime.
    let mmap = Arc::new(Source::Mapped(unsafe { Mmap::map(&file)? }));
    let b: &[u8] = &mmap;

    let mut app = App::single(View::new(b, &path, jsonl));
    // Steal fd 1 for the TUI (repointing it at the tty) before ratatui grabs
    // stdout, so a redirected stdout stays clean for a `p` extract on exit.
    let payload = reserve_stdout_for_payload();
    app.can_extract = payload.enabled;
    if app.can_extract {
        // Make `p` discoverable: this first-frame hint clears on any keypress.
        app.flash = Some(EXTRACT_HINT.to_string());
    }
    let mut term = ratatui::init();
    // Paint the first frame *before* probing keyboard-enhancement support.
    // `supports_keyboard_enhancement()` (in enable_enhanced_keys) blocks up to
    // ~2s on terminals that never answer the query, and the alt-screen is blank
    // until the first draw — that's the "blank screen for 2s on open". The tree
    // flattens a windowed screenful instantly (mmap + lazy), so drawing it first
    // makes the file appear immediately and the probe runs behind rendered
    // content (it only changes how *later* keypresses are reported, so the delay
    // is invisible — keys can't arrive before the user has seen the tree).
    let _ = render_frame(&mut term, &mut app, b, false);
    let enhanced = enable_enhanced_keys();
    enable_mouse();
    let res = run(&mut term, &mut app, b, &mmap);
    disable_mouse();
    if enhanced {
        disable_enhanced_keys();
    }
    ratatui::restore();
    // After restoring the terminal, hand the chosen node to the reserved stdout.
    if let Some(range) = app.extract {
        write_extract(b, range, payload)?;
    }
    res
}

/// Stream piped stdin: the JSON renders progressively as it arrives (a pipe
/// can't be mmap'd, so it's buffered in RAM and re-parsed on a throttle).
fn run_stdin() -> std::io::Result<()> {
    let rx = spawn_reader(take_pipe_reader());
    // stdin was the pipe; fd 0 is now the terminal (reattached above). Reserve
    // stdout so `… | jview | jq` can extract a node into the downstream pipe.
    let payload = reserve_stdout_for_payload();
    let mut buf: Vec<u8> = Vec::new();
    let mut jsonl = false;
    let mut app = App::single(View::new(&buf, "stdin", jsonl));
    app.can_extract = payload.enabled;
    if app.can_extract {
        app.flash = Some(EXTRACT_HINT.to_string());
    }
    let mut term = ratatui::init();
    // Same as run_file: paint before the up-to-2s keyboard-enhancement probe so
    // the (initially empty) streaming pane shows immediately, not a blank screen.
    let _ = render_frame(&mut term, &mut app, &buf, true);
    let enhanced = enable_enhanced_keys();
    enable_mouse();
    let res = run_stream(&mut term, &mut app, &mut buf, &mut jsonl, rx);
    disable_mouse();
    if enhanced {
        disable_enhanced_keys();
    }
    ratatui::restore();
    if let Some(range) = app.extract {
        write_extract(&buf, range, payload)?;
    }
    res
}

const USAGE: &str = "\
jview — browse, navigate and search multi-GB JSON in the terminal

USAGE:
    jview <file.json>          open a file
    cat file.json | jview      or pipe JSON in (NDJSON auto-detected)

OPTIONS:
    -h, --help       print this help
    -V, --version    print version

Keys: ↑/↓ move · enter expand · / search · : goto · y copy · ? help · q quit";

fn main() -> std::io::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("-V" | "--version") => {
            println!("jview {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(_) => run_file(std::env::args().nth(1).unwrap()),
        None => {
            if std::io::stdin().is_terminal() {
                eprintln!("usage: jview <file.json>   (or pipe JSON: cat file.json | jview)");
                std::process::exit(2);
            }
            run_stdin()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Diagnostic: how long does *opening* big.json take (mmap → root →
    /// first windowed flatten)? `#[ignore]`d so it never runs in CI and never
    /// depends on the 1 GB fixture existing. Run explicitly:
    ///   cargo test --release -- --ignored --nocapture bench_open_big
    #[test]
    #[ignore]
    fn bench_open_big() {
        let path = "demo/big.json";
        let Ok(file) = File::open(path) else {
            eprintln!("skip: {path} not present");
            return;
        };
        let t0 = std::time::Instant::now();
        let mmap = unsafe { Mmap::map(&file).unwrap() };
        let t_map = t0.elapsed();
        let b: &[u8] = &mmap;
        let t1 = std::time::Instant::now();
        let mut v = View::new(b, path, false); // make_root auto-expands depth 1
        let t_root = t1.elapsed();
        // First frame: cooperative flatten yields within FLATTEN_BUDGET, so it
        // paints the rows it has (root + the giant `users`) without scanning past
        // it — the fix for the open-blank. `meta` sits behind the 1 GB skip.
        let t2 = std::time::Instant::now();
        v.flatten_window(b, 50);
        let t_first = t2.elapsed();
        // Count real rows (a mid-drain flatten also pushes a trailing loading row).
        let first_rows = v.rows.iter().filter(|r| !r.loading).count();
        let first_incomplete = v.flatten_incomplete;
        // Drain: keep flattening (as the run loop does while flatten_pending) until
        // the skip completes and the rest of the level is in. This is where the
        // ~700 ms now lives — *after* the first paint, not blocking it.
        let t3 = std::time::Instant::now();
        let mut frames = 1;
        while v.flatten_incomplete {
            v.flatten_window(b, 50);
            frames += 1;
        }
        let t_drain = t3.elapsed();
        eprintln!("file size        : {} bytes", b.len());
        eprintln!("mmap             : {t_map:?}");
        eprintln!("View::new        : {t_root:?}");
        eprintln!(
            "FIRST PAINT      : {t_first:?}   rows={first_rows}  incomplete={first_incomplete}"
        );
        let final_rows = v.rows.iter().filter(|r| !r.loading).count();
        eprintln!("drain to complete: {t_drain:?}   frames={frames}  rows={final_rows}");
        eprintln!("TOTAL            : {:?}", t0.elapsed());
        for r in v.rows.iter().filter(|r| !r.loading) {
            eprintln!(
                "  row d{} {:?}: {}",
                r.depth,
                r.label,
                truncate(&r.value, 60)
            );
        }
        // Collapse the root: the collapsed preview must NOT re-scan the 1 GB
        // `users` to reach `meta` — it stops at `…` (bounded by PREVIEW_SKIP_BUDGET)
        // and is instant, even though warm-cache pages are already faulted in.
        v.focus = 0;
        v.toggle_focus(); // collapse the root
        let t4 = std::time::Instant::now();
        v.flatten_window(b, 50);
        let t_collapse = t4.elapsed();
        let preview = v.rows.first().map(|r| r.value.clone()).unwrap_or_default();
        eprintln!("collapse         : {t_collapse:?}   root preview = {preview}");

        // Re-expand: the first drain already enumerated `meta` into `root.children`,
        // and collapse keeps that cache, so this must NOT re-scan the 1 GB — it just
        // re-reads the cached child nodes. Instant.
        v.focus = 0;
        v.toggle_focus(); // re-expand the root
        let t5 = std::time::Instant::now();
        v.flatten_window(b, 50);
        let t_reexpand = t5.elapsed();
        let reexpand_rows = v.rows.iter().filter(|r| !r.loading).count();
        eprintln!(
            "re-expand        : {t_reexpand:?}   rows={reexpand_rows}  incomplete={}",
            v.flatten_incomplete
        );

        // The point of the change: the first paint is fast and bounded, not the
        // whole skip; collapsing never pays the giant skip; and re-expanding reuses
        // the cached children instead of re-scanning.
        assert!(
            t_first < Duration::from_millis(100),
            "first paint should be fast"
        );
        assert!(
            t_collapse < Duration::from_millis(50),
            "collapse should be instant"
        );
        assert!(
            t_reexpand < Duration::from_millis(50),
            "re-expand should reuse the cache"
        );
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (input, expected) in cases {
            assert_eq!(base64_encode(input.as_bytes()), expected, "input={input:?}");
        }
    }

    #[test]
    fn base64_handles_binary() {
        assert_eq!(base64_encode(&[0xff, 0xfe, 0xfd]), "//79");
        assert_eq!(base64_encode(&[0x00]), "AA==");
    }

    #[test]
    fn join_path_dots_keys_and_brackets_indices() {
        let segs = vec![
            ("data".to_string(), false),
            ("users".to_string(), false),
            ("3".to_string(), true),
            ("city".to_string(), false),
        ];
        assert_eq!(join_path("", &segs), "data.users[3].city");
        assert_eq!(join_path("", &[("0".to_string(), true)]), "[0]");
        assert_eq!(join_path("", &[]), "root");
    }

    #[test]
    fn parse_path_tokenizes_keys_and_indices() {
        use Seg::*;
        // A bare path is absolute (no climb).
        let p = parse_path("data.users[3].city");
        assert_eq!(p.up, None);
        assert_eq!(
            p.segs,
            vec![
                Key("data".into()),
                Key("users".into()),
                Index(3),
                Key("city".into())
            ]
        );
        // `$` marks absolute; the dot right after `$` is optional.
        assert_eq!(
            parse_path("$.a"),
            ParsedPath {
                up: None,
                segs: vec![Key("a".into())]
            }
        );
        // Bracketed (optionally quoted) key holds a literal dot.
        assert_eq!(
            parse_path(r#"["x.y"][2]"#).segs,
            vec![Key("x.y".into()), Index(2)]
        );
        assert_eq!(
            parse_path(""),
            ParsedPath {
                up: None,
                segs: vec![]
            }
        );
    }

    #[test]
    fn parse_path_relative_leading_dots() {
        use Seg::*;
        // One dot → from the focused node (climb 0).
        assert_eq!(
            parse_path(".actor.login"),
            ParsedPath {
                up: Some(0),
                segs: vec![Key("actor".into()), Key("login".into())]
            }
        );
        // Two dots → climb to the parent first (sibling access); three → climb two.
        assert_eq!(parse_path("..sibling").up, Some(1));
        assert_eq!(parse_path("...x").up, Some(2));
        // Bare `.`/`..` are valid jumps with no descent.
        assert_eq!(
            parse_path("."),
            ParsedPath {
                up: Some(0),
                segs: vec![]
            }
        );
        assert_eq!(
            parse_path(".."),
            ParsedPath {
                up: Some(1),
                segs: vec![]
            }
        );
        // A relative path can lead straight into a bracket: `.[0]`.
        assert_eq!(
            parse_path(".[0]"),
            ParsedPath {
                up: Some(0),
                segs: vec![Index(0)]
            }
        );
    }

    #[test]
    fn resolve_path_walks_the_lazy_tree() {
        let b = br#"{"a":{"b":[10,20,30]}}"#;
        let mut root = make_root(b, "t", false);
        let path = resolve_path(&mut root, b, &[], &parse_path("a.b[1]").segs).expect("resolved");
        let node = get(&root, &path);
        assert_eq!(&b[node.start..node.end], b"20");
        // A missing key resolves to None.
        assert!(resolve_path(&mut root, b, &[], &parse_path("a.nope").segs).is_none());
    }

    #[test]
    fn nav_sibling_steps_over_subtrees() {
        let b = br#"{"a":{"x":1},"b":{"y":2},"c":3}"#;
        let mut v = View::new(b, "t", false);
        v.flatten_window(b, 40);
        // Focus key "a" (path [0]) and expand it, so its child sits between it
        // and the next sibling.
        v.focus = v.rows.iter().position(|r| r.path == vec![0]).unwrap();
        v.toggle_focus();
        v.flatten_window(b, 40);
        // Forward hops over a.x straight to "b" ([1]), then "c" ([2]).
        v.nav_sibling(b, true);
        v.flatten_window(b, 40);
        assert_eq!(v.rows[v.focus].path, vec![1]);
        v.nav_sibling(b, true);
        v.flatten_window(b, 40);
        assert_eq!(v.rows[v.focus].path, vec![2]);
        // At the last sibling, forward is a no-op.
        v.nav_sibling(b, true);
        v.flatten_window(b, 40);
        assert_eq!(v.rows[v.focus].path, vec![2]);
        // Backward walks back to "b".
        v.nav_sibling(b, false);
        v.flatten_window(b, 40);
        assert_eq!(v.rows[v.focus].path, vec![1]);
    }

    #[test]
    fn resolve_with_climb_falls_back_to_ancestors() {
        let b = br#"{"a":{"b":{"city":"X","id":1},"c":{"city":"Y"}}}"#;
        let mut root = make_root(b, "t", false);
        // Focus deep on a.b.id (a scalar leaf).
        let focus = resolve_path(&mut root, b, &[], &parse_path("a.b.id").segs).expect("focus");
        // Bare `city` is absent at the root and a.b.id is a scalar, so it climbs
        // to a.b → a.b.city = "X" (the nearest ancestor that has it).
        let p = resolve_with_climb(&mut root, b, &focus, &parse_path("city")).expect("climbed");
        assert_eq!(&b[get(&root, &p).start..get(&root, &p).end], b"\"X\"");
        // An explicit absolute path still resolves from the root first.
        let p = resolve_with_climb(&mut root, b, &focus, &parse_path("a.c.city")).expect("abs");
        assert_eq!(&b[get(&root, &p).start..get(&root, &p).end], b"\"Y\"");
        // A genuinely-absent key resolves nowhere, even after climbing.
        assert!(resolve_with_climb(&mut root, b, &focus, &parse_path("nope")).is_none());
    }

    #[test]
    fn resolve_path_globs_keys() {
        // `*`/`?` in a key segment match the whole label, case-insensitively,
        // and take the first child that fits. Plain keys still need an exact hit.
        let b = br#"{"users":{"firstName":"Ada","lastName":"L","age":36}}"#;
        let mut root = make_root(b, "t", false);
        let p = resolve_path(&mut root, b, &[], &parse_path("users.first*").segs).expect("glob *");
        assert_eq!(&b[get(&root, &p).start..get(&root, &p).end], b"\"Ada\"");
        let p =
            resolve_path(&mut root, b, &[], &parse_path("users.*Name").segs).expect("glob suffix");
        // *Name matches firstName (the first child that fits the whole pattern).
        assert_eq!(&b[get(&root, &p).start..get(&root, &p).end], b"\"Ada\"");
        let p = resolve_path(&mut root, b, &[], &parse_path("users.a?e").segs).expect("glob ?");
        assert_eq!(&b[get(&root, &p).start..get(&root, &p).end], b"36");
        // Globs are anchored at both ends — `first` (without `*`) is exact, so it misses.
        assert!(resolve_path(&mut root, b, &[], &parse_path("users.first").segs).is_none());
        // A glob with no child matching the whole pattern resolves to None.
        assert!(resolve_path(&mut root, b, &[], &parse_path("users.zz*").segs).is_none());
    }

    #[test]
    fn search_pattern_parses_re_glob_and_literal() {
        // re: compiles a regex (case-insensitive).
        let p = Pattern::parse("re:^foo.*bar$").expect("regex");
        match p {
            Pattern::Regex(_) => {}
            _ => panic!("expected Regex"),
        }
        // Bad regex returns a footer-ready error.
        assert!(Pattern::parse("re:[unclosed").is_err());
        // g: turns into a regex (the `.` is escaped so it matches literal dot).
        let p = Pattern::parse("g:foo.*bar").expect("glob");
        match p {
            Pattern::Regex(_) => {}
            _ => panic!("expected Regex"),
        }
        // Plain query is literal substring (the default).
        let p = Pattern::parse("Hello").expect("literal");
        match p {
            Pattern::Literal(n) => assert_eq!(n, "hello"),
            _ => panic!("expected Literal"),
        }
    }

    #[test]
    fn write_extract_slices_the_subtree_with_trailing_newline() {
        // `p` records a node's (start, end), but end can be provisional — for a
        // collapsed container it runs to the parent's bound. write_extract must
        // re-resolve the real closer so it emits just the node, not its trailing
        // siblings, plus one newline for a clean pipe line.
        let b = br#"{"a":{"b":[1,2,3]},"c":9}"#;
        let inner = b[1..].iter().position(|&c| c == b'{').unwrap() + 1; // second `{`
        assert_eq!(inner, 5, "inner object starts at the second brace");
        // Pass a deliberately over-long provisional end (whole doc) to prove
        // skip_value stops at the inner `}` rather than slicing into `,"c":9}`.
        let path = std::env::temp_dir().join(format!("jview-extract-{}.json", std::process::id()));
        let f = File::create(&path).expect("temp file");
        let sink = Payload {
            enabled: true,
            file: Some(f),
        };
        write_extract(b, (inner, b.len()), sink).expect("write");
        let got = std::fs::read(&path).expect("read back");
        let _ = std::fs::remove_file(&path);
        assert_eq!(got, b"{\"b\":[1,2,3]}\n");
    }

    #[test]
    fn resolve_path_descends_from_a_base() {
        let b = br#"{"a":{"b":[10,20,30],"c":99}}"#;
        let mut root = make_root(b, "t", false);
        // Establish a base path (`a`), then descend relatively from it.
        let base = resolve_path(&mut root, b, &[], &parse_path("a").segs).expect("base");
        let p = resolve_path(&mut root, b, &base, &parse_path(".b[2]").segs).expect("relative");
        let node = get(&root, &p);
        assert_eq!(&b[node.start..node.end], b"30");
        // Empty segments from a base resolve to the base node itself.
        assert_eq!(
            resolve_path(&mut root, b, &base, &[]).expect("base itself"),
            base
        );
    }

    // --- robustness & correctness guards ---

    /// Deterministic xorshift64* PRNG — reproducible, and no `rand` dependency.
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
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A short random string spanning the bytes that stress JSON decoding —
    /// quotes, backslashes, control chars, and multi-byte UTF-8 — all of which
    /// serde_json escapes on the way out and our `decode_str` must restore.
    fn gen_string(rng: &mut Rng) -> String {
        const POOL: &[char] = &[
            'a', 'Z', '0', '"', '\\', '/', '\n', '\t', '\u{1}', ' ', ':', '{', '[', ']', ',', 'é',
            '☃', '🦀',
        ];
        (0..rng.below(8))
            .map(|_| POOL[(rng.next() as usize) % POOL.len()])
            .collect()
    }

    /// Build a random JSON value, depth-limited. Object keys are index-prefixed so
    /// they're unique — otherwise serde_json's last-wins map would disagree with
    /// our tree, which keeps every child.
    fn gen_json(rng: &mut Rng, depth: u32) -> serde_json::Value {
        use serde_json::Value;
        // At depth 0 emit a scalar only (arms 4..=8 below).
        let pick = if depth == 0 {
            rng.below(5) + 4
        } else {
            rng.below(9)
        };
        match pick {
            0 => Value::Array(
                (0..rng.below(5))
                    .map(|_| gen_json(rng, depth - 1))
                    .collect(),
            ),
            1 => {
                let mut m = serde_json::Map::new();
                for i in 0..rng.below(5) {
                    m.insert(
                        format!("k{i}_{}", gen_string(rng)),
                        gen_json(rng, depth - 1),
                    );
                }
                Value::Object(m)
            }
            2 | 3 => Value::Bool(rng.next() & 1 == 0),
            4 => Value::Null,
            5 | 6 => Value::from((rng.next() as i64).wrapping_sub(i64::MAX / 2)),
            7 => {
                // Use the float only if serde_json round-trips it bit-for-bit.
                // (A few extreme denormals don't — that's the oracle's float
                // fidelity, not our scanner, and would make the test flaky.)
                let f = f64::from_bits(rng.next());
                let round_trips = f.is_finite()
                    && serde_json::from_str::<f64>(&serde_json::to_string(&f).unwrap())
                        .is_ok_and(|g| g.to_bits() == f.to_bits());
                if round_trips {
                    Value::from(f)
                } else {
                    Value::from((rng.next() as i64).wrapping_sub(i64::MAX / 2))
                }
            }
            _ => Value::String(gen_string(rng)),
        }
    }

    /// Fully materialize the lazy tree into a serde_json::Value: walk every child
    /// (scanning each container on the way) and re-parse scalar byte ranges with
    /// serde_json. So this exercises *our* structural scan and boundary-finding;
    /// scalar decoding is delegated to the oracle on the exact range we located.
    fn materialize(node: &mut Node, b: &[u8]) -> serde_json::Value {
        use serde_json::Value;
        if !node.is_container() {
            return serde_json::from_slice(&b[node.start..node.end]).expect("scalar slice parses");
        }
        if node.has_children && !node.expanded {
            node.toggle();
        }
        if node.has_children {
            let mut i = 0;
            loop {
                node.ensure_child(b, i);
                if i >= node.children.len() {
                    break;
                }
                i += 1;
            }
        }
        if matches!(node.kind, Kind::Array) {
            Value::Array(
                node.children
                    .iter_mut()
                    .map(|c| materialize(c, b))
                    .collect(),
            )
        } else {
            let mut m = serde_json::Map::new();
            for c in node.children.iter_mut() {
                let k = c.label.clone();
                m.insert(k, materialize(c, b));
            }
            Value::Object(m)
        }
    }

    /// **Oracle test.** For thousands of random documents — in both compact and
    /// pretty (whitespace-heavy) encodings — our lazy byte-range tree must
    /// reconstruct exactly what serde_json parses from the same bytes. This is the
    /// correctness backbone of the lazy parser: if a boundary is off by a byte,
    /// this fails.
    #[test]
    fn lazy_tree_matches_serde_json() {
        let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
        for _ in 0..2000 {
            let expected = gen_json(&mut rng, 4);
            for encoded in [
                serde_json::to_string(&expected).unwrap(),
                serde_json::to_string_pretty(&expected).unwrap(),
            ] {
                let b = encoded.as_bytes();
                let mut root = make_root(b, "t", false);
                assert_eq!(
                    materialize(&mut root, b),
                    expected,
                    "mismatch on: {encoded}"
                );
            }
        }
    }

    /// **Laziness guard.** Opening a level with a million children must flatten
    /// only ~a screenful and scan only that many children — never enumerate the
    /// whole thing. This is the invariant that keeps memory and first-paint
    /// constant on a huge file; a non-windowed flatten would blow it up.
    #[test]
    fn windowed_flatten_stays_bounded_on_a_huge_level() {
        let n = 1_000_000;
        let mut s = String::with_capacity(n * 2 + 2);
        s.push('[');
        for i in 0..n {
            if i > 0 {
                s.push(',');
            }
            s.push('0');
        }
        s.push(']');
        let b = s.as_bytes();

        let mut root = make_root(b, "t", false); // the array, auto-expanded
        let budget = 256;
        let mut rows = Vec::new();
        let mut path = Vec::new();
        let mut incomplete = false;
        flatten(
            &mut root,
            b,
            0,
            budget,
            &mut rows,
            &mut path,
            None,
            &mut incomplete,
        );

        assert!(
            rows.len() <= budget,
            "flatten exceeded its row budget: {}",
            rows.len()
        );
        assert!(
            root.children.len() <= budget,
            "enumerated the level eagerly: scanned {} of {n}",
            root.children.len()
        );
    }

    /// The high-level navigation path must also survive arbitrary bytes: building
    /// a root, flattening a window, and resolving random goto paths against the
    /// result may never panic. (Pairs with the scanner-level fuzz.)
    #[test]
    fn fuzz_make_root_and_navigation_never_panic() {
        const ALPHABET: &[u8] = b"{}[]\":,\\ \t\n0123456789.-eEtfnul";
        const GOTO: &[&str] = &["a", ".a", "..a", "a.b[0]", "$.x", "[3]", "...z", "k0_", ""];
        let mut rng = Rng(0x1234_5678_9ABC_DEF0);
        for _ in 0..3000 {
            let len = (rng.next() % 120) as usize;
            let buf: Vec<u8> = (0..len)
                .map(|_| ALPHABET[(rng.next() as usize) % ALPHABET.len()])
                .collect();
            let b = &buf[..];
            for jsonl in [false, true] {
                let mut root = make_root(b, "x", jsonl);
                let mut rows = Vec::new();
                let mut path = Vec::new();
                let mut incomplete = false;
                flatten(
                    &mut root,
                    b,
                    0,
                    128,
                    &mut rows,
                    &mut path,
                    None,
                    &mut incomplete,
                );
                let parsed = parse_path(GOTO[(rng.next() as usize) % GOTO.len()]);
                let focus = rows.last().map(|r| r.path.clone()).unwrap_or_default();
                let _ = resolve_with_climb(&mut root, b, &focus, &parsed);
            }
        }
    }
}
