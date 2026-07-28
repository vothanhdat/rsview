//! The lazy JSON tree model and path navigation.
//!
//! A [`Node`] is a byte range plus a resumable child [`Cursor`]: a collapsed
//! node costs O(1), and a huge level only enumerates as far as it's scrolled
//! ([`flatten`] windows the DFS to a row budget, cooperatively yielding when a
//! single value is too big to skip in one frame). On top of the tree sit the
//! `goto` path parser ([`parse_path`]/[`resolve_with_climb`]) and the helpers
//! that carry expansion state across a streaming re-parse.
//!
//! This layer depends only on the [`scanner`](crate::scanner); the panes, app
//! state, and rendering are built above it.

use crate::scanner::{
    container_empty, decode_str, skip_ws, value_kind, Cursor, Kind, RawChild, Step, MAX_DEPTH,
};
use crate::search::glob_to_regex;
use std::time::Instant;

/// How many children to show in a collapsed container's inline preview.
const PREVIEW_ITEMS: usize = 5;
/// Max display width of a collapsed preview before it's truncated with `…`.
const PREVIEW_WIDTH: usize = 64;
/// Max display width of one value inside a preview — also a table cell's width
/// cap, since a cell is exactly that: one value, briefly.
pub const BRIEF_WIDTH: usize = 24;
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

/// Bytes a single resumable skip-chunk steps over before re-checking the frame
/// deadline. Small enough that the deadline is honoured to within a chunk, large
/// enough that the per-chunk overhead is negligible (~1–2 ms of scanning).
const SKIP_CHUNK_BYTES: usize = 2 << 20; // 2 MiB

/// How many children a `:` goto will scan to resolve an object key. A goto is a
/// synchronous, page-faulting walk, so this bounds the UI freeze on a giant
/// level — a key past this many siblings reports "not found" rather than hanging.
const GOTO_SCAN_CAP: usize = 100_000;

/// A lazily-expanding tree node. Children are scanned on demand from `cursor`
/// (resumable), so a collapsed node costs O(1) and a huge level only enumerates
/// as far as it's scrolled.
pub struct Node {
    pub label: String,
    pub start: usize,
    pub end: usize,
    /// False when `end` is provisional — a container whose closer hasn't been
    /// scanned yet (`end` is its parent's bound). Display/expand work regardless
    /// (cursors stop at the real closer); the raw-range consumers (copy `y`, split
    /// `o`/`O`) check this and skip to the true closer before slicing.
    pub end_exact: bool,
    pub kind: Kind,
    pub is_index: bool,
    /// Synthetic NDJSON root: children are the documents, enumerated by a
    /// `Cursor::lines` instead of a bracketed-container cursor.
    pub jsonl: bool,
    pub has_children: bool,
    pub expanded: bool,
    pub done: bool,
    pub children: Vec<Node>,
    pub cursor: Option<Cursor>,
}

impl Node {
    pub fn is_container(&self) -> bool {
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

    pub fn toggle(&mut self) {
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
    pub fn ensure_child(&mut self, b: &[u8], i: usize) {
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
    pub fn preview(&self, b: &[u8]) -> String {
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
pub fn decode_cap(start: usize, end: usize) -> usize {
    end.min(start.saturating_add(PREVIEW_DECODE_BYTES))
}

/// A brief one-level rendering of a value for use inside a parent's preview:
/// nested containers collapse to `{…}`/`[…]`, scalars show their literal.
fn brief(b: &[u8], rc: &RawChild) -> String {
    brief_value(b, rc.start, rc.end, rc.kind)
}

/// [`brief`] over a bare byte range — what a table cell shows. Bounded like
/// every other preview: at most `PREVIEW_DECODE_BYTES` decoded, truncated to
/// `BRIEF_WIDTH`, and containers never descend.
pub fn brief_value(b: &[u8], start: usize, end: usize, kind: Kind) -> String {
    match kind {
        Kind::Object => {
            if container_empty(b, start, end) {
                "{}".into()
            } else {
                "{…}".into()
            }
        }
        Kind::Array => {
            if container_empty(b, start, end) {
                "[]".into()
            } else {
                "[…]".into()
            }
        }
        Kind::Str => {
            let e = decode_cap(start, end);
            format!("\"{}\"", truncate(&decode_str(b, start, e), BRIEF_WIDTH))
        }
        _ => {
            let e = decode_cap(start, end);
            truncate(&String::from_utf8_lossy(&b[start..e]), BRIEF_WIDTH)
        }
    }
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

/// A flattened, on-screen row plus the index path back to its node. Carries the
/// label/value/kind separately so the renderer can syntax-color each segment.
pub struct Row {
    pub depth: usize,
    pub label: String,
    pub value: String,
    pub kind: Kind,
    pub is_index: bool,
    pub has_children: bool,
    pub expanded: bool,
    pub path: Vec<usize>,
    /// A synthetic, non-navigable placeholder rendered while a huge value is being
    /// stepped over (the inline "⠋ loading…" line at the spot where the next
    /// sibling will appear). Always the last row; focus is clamped above it.
    pub loading: bool,
}

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
pub fn flatten(
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

pub fn get<'a>(mut n: &'a Node, path: &[usize]) -> &'a Node {
    for &i in path {
        n = &n.children[i];
    }
    n
}

pub fn get_mut<'a>(mut n: &'a mut Node, path: &[usize]) -> &'a mut Node {
    for &i in path {
        n = &mut n.children[i];
    }
    n
}

/// Expand every ancestor of `path` (scanning children up to each step) so the
/// target node becomes reachable in the flattened rows. The target itself is
/// left as-is — only the chain above it is opened.
pub fn expand_to(n: &mut Node, b: &[u8], path: &[usize]) {
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

/// Build the root node for a buffer. Shared by `App::new` and `App::rebuild`
/// (the streaming re-parse), so a growing buffer always produces a consistent
/// root. The root is auto-expanded (like `--depth 1`) when it has children.
pub fn make_root(b: &[u8], name: &str, jsonl: bool) -> Node {
    // NDJSON: a synthetic array root spanning the whole buffer, with at least one
    // document if there's any non-whitespace. Single-doc: the first value.
    let (start, kind, has) = if jsonl {
        (0, Kind::Array, skip_ws(b, 0, b.len()) < b.len())
    } else {
        let rstart = skip_ws(b, 0, b.len());
        if rstart >= b.len() {
            // Empty / whitespace-only input (e.g. `jsonview </dev/null`, or a stream
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
pub fn make_subroot(b: &[u8], label: String, start: usize, end: usize, kind: Kind) -> Node {
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
pub fn join_path(base: &str, segs: &[(String, bool)]) -> String {
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
pub enum Seg {
    Key(String),
    Index(usize),
}

/// A parsed `goto` path: where resolution starts, then the segments to descend.
#[derive(Debug, PartialEq)]
pub struct ParsedPath {
    /// `None` → **absolute**: resolve from the document root.
    /// `Some(up)` → **relative**: start at the focused node and climb `up` levels
    /// first (0 = the focused node itself, 1 = its parent, …), then descend `segs`.
    pub up: Option<usize>,
    pub segs: Vec<Seg>,
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
pub fn parse_path(input: &str) -> ParsedPath {
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
pub fn resolve_path(root: &mut Node, b: &[u8], base: &[usize], segs: &[Seg]) -> Option<Vec<usize>> {
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
pub fn resolve_with_climb(
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
pub fn collect_expanded(node: &Node, path: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
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
pub fn set_expanded(root: &mut Node, b: &[u8], path: &[usize]) {
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

/// Count a container's direct children by scanning them (bounded only by the
/// data — a resumable cursor, so it's the same walk `expand` does, just to the
/// end). `None` for a scalar. Backs the `c` key: "how big is this?" without
/// expanding a possibly-huge level.
pub fn count_children(node: &Node, b: &[u8]) -> Option<usize> {
    if !node.jsonl && !matches!(node.kind, Kind::Object | Kind::Array) {
        return None;
    }
    let mut cur = node.make_cursor();
    let mut n = 0usize;
    while cur.next(b).is_some() {
        n += 1;
    }
    Some(n)
}

/// Count / sum / min / max / mean of a container's **direct** numeric children.
/// The numeric companion to [`count_children`] (`c`) and `t` (type): "what's in
/// this array of numbers?" without materializing it. See [`aggregate_numbers`].
pub struct NumStats {
    /// Direct children that parsed as a JSON number.
    pub count: usize,
    /// All direct children scanned (so the caller can say "12 of 20 numeric").
    pub total: usize,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}

impl NumStats {
    pub fn mean(&self) -> f64 {
        self.sum / self.count as f64
    }
}

/// Summarize a container's **direct** numeric children in one streaming pass —
/// the same resumable walk `c` (count) does, accumulating the values that parse
/// as JSON numbers into an `f64` running total/min/max instead of copying
/// anything (so it stays constant-memory over a huge array). Non-numbers are
/// counted in `total` but skipped; `None` for a scalar (not a container), and
/// `count == 0` when a container holds no numbers.
pub fn aggregate_numbers(node: &Node, b: &[u8]) -> Option<NumStats> {
    if !node.jsonl && !matches!(node.kind, Kind::Object | Kind::Array) {
        return None;
    }
    let mut cur = node.make_cursor();
    let mut s = NumStats {
        count: 0,
        total: 0,
        sum: 0.0,
        min: f64::INFINITY,
        max: f64::NEG_INFINITY,
    };
    while let Some(rc) = cur.next(b) {
        s.total += 1;
        if rc.kind == Kind::Number {
            if let Some(x) = std::str::from_utf8(&b[rc.start..rc.end])
                .ok()
                .and_then(|t| t.trim().parse::<f64>().ok())
            {
                s.count += 1;
                s.sum += x;
                s.min = s.min.min(x);
                s.max = s.max.max(x);
            }
        }
    }
    Some(s)
}

/// The label/is-index pairs from the root down to `path`, for the focus
/// breadcrumb and split-pane titles.
pub fn breadcrumb_segments(root: &Node, path: &[usize]) -> Vec<(String, bool)> {
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

#[cfg(test)]
mod agg_tests {
    use super::{aggregate_numbers, make_root};

    fn stats(json: &str) -> Option<(usize, usize, f64, f64, f64, f64)> {
        let root = make_root(json.as_bytes(), "root", false);
        aggregate_numbers(&root, json.as_bytes())
            .map(|s| (s.count, s.total, s.sum, s.min, s.max, s.mean()))
    }

    #[test]
    fn sums_a_plain_number_array() {
        let (count, total, sum, min, max, mean) = stats("[1, 2, 3, 4]").unwrap();
        assert_eq!((count, total), (4, 4));
        assert_eq!((sum, min, max, mean), (10.0, 1.0, 4.0, 2.5));
    }

    #[test]
    fn skips_non_numeric_children_but_counts_them_in_total() {
        // Two numbers, two non-numbers among four direct children.
        let (count, total, sum, min, max, _) = stats(r#"[1, "x", true, 5]"#).unwrap();
        assert_eq!((count, total), (2, 4));
        assert_eq!((sum, min, max), (6.0, 1.0, 5.0));
    }

    #[test]
    fn aggregates_object_values_and_handles_negatives_and_floats() {
        let (count, sum, min, max) = stats(r#"{"a": -1.5, "b": 2.5, "c": 10}"#)
            .map(|(c, _, s, mn, mx, _)| (c, s, mn, mx))
            .unwrap();
        assert_eq!(count, 3);
        assert_eq!((sum, min, max), (11.0, -1.5, 10.0));
    }

    #[test]
    fn no_numbers_reports_zero_count() {
        let (count, total, ..) = stats(r#"["a", "b"]"#).unwrap();
        assert_eq!((count, total), (0, 2));
    }

    #[test]
    fn scalar_is_not_a_container() {
        assert!(stats("42").is_none());
    }
}
