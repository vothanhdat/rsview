//! rsview — proof-of-concept lazy JSON viewer in Rust.
//!
//! Demonstrates the core of react-obj-view's CLI in native Rust: a file is
//! memory-mapped, parsed on expand (subtrees are byte ranges, not materialized
//! values), and a level is flattened only as far as the viewport scrolls
//! (windowing). Opening a multi-GB file stays near-constant memory.

mod scanner;
mod search;
mod source;
use scanner::{
    container_empty, decode_str, skip_value, skip_ws, value_kind, Cursor, Kind, RawChild, MAX_DEPTH,
};
use search::Search;
use source::Source;

use memmap2::Mmap;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
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
        let has = is_cont && !container_empty(b, rc.start, rc.end);
        Node {
            label: rc.label,
            start: rc.start,
            end: rc.end,
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

    /// The value text shown after the label. Collapsed containers get a real
    /// one-line preview of their first few children; expanded ones get the
    /// opening brace; scalars get their (truncated) literal.
    fn preview(&self, b: &[u8]) -> String {
        match self.kind {
            Kind::Object | Kind::Array => {
                let arr = matches!(self.kind, Kind::Array);
                if !self.has_children {
                    if arr { "[]".into() } else { "{}".into() }
                } else if self.expanded {
                    if arr { "[".into() } else { "{".into() }
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
    fn collapsed_preview(&self, b: &[u8]) -> String {
        let arr = matches!(self.kind, Kind::Array);
        let (open, close) = if arr { ("[", "]") } else { ("{", "}") };
        let mut cur = self.make_cursor();
        let mut parts: Vec<String> = Vec::new();
        let mut more = false;
        loop {
            if parts.len() == PREVIEW_ITEMS {
                more = cur.next(b).is_some(); // is there at least one more?
                break;
            }
            match cur.next(b) {
                Some(rc) => {
                    let v = brief(b, &rc);
                    parts.push(if arr { v } else { format!("{}: {}", rc.label, v) });
                }
                None => break,
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
            if container_empty(b, rc.start, rc.end) { "{}".into() } else { "{…}".into() }
        }
        Kind::Array => {
            if container_empty(b, rc.start, rc.end) { "[]".into() } else { "[…]".into() }
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
}

/// Windowed flatten: walk the expanded tree DFS, scanning children on demand,
/// and stop once `budget` rows exist. A pathologically flat level (millions of
/// keys) therefore only flattens ~a screenful, not the whole thing.
fn flatten(node: &mut Node, b: &[u8], depth: usize, budget: usize, out: &mut Vec<Row>, path: &mut Vec<usize>) {
    if out.len() >= budget {
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
    });
    // Stop descending past the depth cap so a deeply-nested (possibly hostile)
    // document can't recurse the stack to a fault. Real data never reaches it.
    if depth < MAX_DEPTH && node.expanded && node.is_container() {
        let mut i = 0;
        while out.len() < budget {
            node.ensure_child(b, i);
            if i >= node.children.len() {
                break; // level fully enumerated
            }
            path.push(i);
            flatten(&mut node.children[i], b, depth + 1, budget, out, path);
            path.pop();
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
            // Empty / whitespace-only input (e.g. `rsview </dev/null`, or a stream
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
        end: b.len(), // root spans to EOF; the scanner stops at the real closer
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
            search: None,
            match_idx: 0,
            match_set: HashSet::new(),
            indexed: 0,
            want_path: None,
            landed: false,
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
    fn relaunch(&mut self, mmap: &Arc<Source>) {
        if let Some(old) = self.search.take() {
            old.cancel(); // belt-and-suspenders; Drop also flips the flag
        }
        self.match_set.clear();
        self.indexed = 0;
        self.match_idx = 0;
        self.landed = false;
        self.want_path = None;
        if self.query.is_empty() {
            return;
        }
        self.search = Some(Search::spawn(
            Arc::clone(mmap),
            self.query.clone(),
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
            if dir >= 0 { 0 } else { n - 1 }
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

    fn clear_search(&mut self) {
        if let Some(s) = self.search.take() {
            s.cancel();
        }
        self.query.clear();
        self.match_set.clear();
        self.indexed = 0;
        self.match_idx = 0;
        self.landed = false;
        self.want_path = None;
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
            let mut budget = target.iter().sum::<usize>() + target.len() + h + 64;
            loop {
                self.rows.clear();
                let mut path = Vec::new();
                flatten(&mut self.root, b, 0, budget, &mut self.rows, &mut path);
                let walked_all = self.rows.len() < budget;
                if self.rows.iter().any(|r| r.path == target) || walked_all {
                    break;
                }
                budget = budget.saturating_mul(2);
            }
            if let Some(idx) = self.rows.iter().position(|r| r.path == target) {
                self.focus = idx;
            }
        } else {
            // No jump pending: flatten only as far as the viewport needs.
            let budget = (self.scroll + h + 64).max(self.focus + 64);
            self.rows.clear();
            let mut path = Vec::new();
            flatten(&mut self.root, b, 0, budget, &mut self.rows, &mut path);
        }

        if self.focus >= self.rows.len() {
            self.focus = self.rows.len().saturating_sub(1);
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
}

impl App {
    fn single(mut view: View) -> App {
        view.id = 0;
        App { views: vec![view], active: 0, stacked: false, next_id: 1 }
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
        (rects[self.active * 2].height as usize).saturating_sub(1).max(1)
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
        let base = if src.derived { src.name.clone() } else { String::new() };
        let origin = join_path(&base, &breadcrumb_segments(&src.root, &path));
        let root = make_subroot(b, node.label.clone(), node.start, node.end, node.kind);
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
    Rect { height: area.height.saturating_sub(1), ..area }
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
        render_pane(f, rects[i * 2], view, i == app.active, streaming && !view.derived);
    }
    // One global footer at the very bottom, reflecting the active pane's mode.
    let footer_row = Rect { y: area.y + area.height.saturating_sub(1), height: 1, ..area };
    render_footer(f, footer_row, app.active_view());
}

/// The single global key/search-status bar, reflecting the active pane.
fn render_footer(f: &mut Frame, area: Rect, view: &View) {
    let footer = if view.mode == Mode::Search {
        let count = view.search.as_ref().map_or(0, |s| s.matches.len());
        let more = match &view.search {
            Some(s) if !s.finished => "+",
            _ => "",
        };
        // Show position once we've landed on a match, else the running total.
        let pos = if view.landed && count > 0 {
            format!("{}/{}{}", view.match_idx + 1, count, more)
        } else {
            format!("{}{} matches", count, more)
        };
        Span::styled(
            format!(" /{}   {} · ↵/↓ next · ⇧↵/↑ prev · esc close", view.query, pos),
            Style::default().fg(Color::Yellow),
        )
    } else {
        Span::styled(
            " ↑/↓ move · enter expand · / search · s split · o preview · \\ layout · +/- size · tab pane · x close · q quit",
            Style::default().fg(Color::DarkGray),
        )
    };
    f.render_widget(Paragraph::new(Line::from(footer)), area);
}

/// Draw one pane (title + breadcrumb, then content rows) into `rect`. The active
/// pane gets a bright title and is the only one to show its cursor bar; the
/// key/search footer is global (see [`render_footer`]), not per-pane.
fn render_pane(f: &mut Frame, rect: Rect, view: &View, active: bool, streaming: bool) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(rect);

    // Title: `↳ origin   focus/rows+` plus the focus breadcrumb.
    let marker = if view.derived { "↳ " } else { "" };
    let prefix = if streaming {
        format!(" {marker}{}   {}/{}+   ⟳ streaming", view.name, view.focus + 1, view.rows.len())
    } else {
        format!(" {marker}{}   {}/{}+", view.name, view.focus + 1, view.rows.len())
    };
    let prefix_w = prefix.chars().count();
    let title_style = if active {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
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

    let cur_match = view.search.as_ref().and_then(|s| s.matches.get(view.match_idx));

    let h = chunks[1].height as usize;
    let mut lines = Vec::new();
    let end = (view.scroll + h).min(view.rows.len());
    for i in view.scroll..end {
        let r = &view.rows[i];
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
            Line::from(Span::styled(text, Style::default().add_modifier(Modifier::REVERSED)))
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
fn process_key(app: &mut App, k: ratatui::crossterm::event::KeyEvent, b: &[u8], h: usize) -> KeyOutcome {
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

    // Normal mode: workspace-level keys (pane management) first.
    match k.code {
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
        // Short poll so streaming match counts keep ticking even without input.
        match pump_input(app, b, h, 100)? {
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
            pump_input(app, b, h, 100)?
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
    let mut term = ratatui::init();
    let enhanced = enable_enhanced_keys();
    enable_mouse();
    let res = run(&mut term, &mut app, b, &mmap);
    disable_mouse();
    if enhanced {
        disable_enhanced_keys();
    }
    ratatui::restore();
    res
}

/// Stream piped stdin: the JSON renders progressively as it arrives (a pipe
/// can't be mmap'd, so it's buffered in RAM and re-parsed on a throttle).
fn run_stdin() -> std::io::Result<()> {
    let rx = spawn_reader(take_pipe_reader());
    let mut buf: Vec<u8> = Vec::new();
    let mut jsonl = false;
    let mut app = App::single(View::new(&buf, "stdin", jsonl));
    let mut term = ratatui::init();
    let enhanced = enable_enhanced_keys();
    enable_mouse();
    let res = run_stream(&mut term, &mut app, &mut buf, &mut jsonl, rx);
    disable_mouse();
    if enhanced {
        disable_enhanced_keys();
    }
    ratatui::restore();
    res
}

fn main() -> std::io::Result<()> {
    match std::env::args().nth(1) {
        Some(path) => run_file(path),
        None => {
            if std::io::stdin().is_terminal() {
                eprintln!("usage: rsview <file.json>   (or pipe JSON: cat file.json | rsview)");
                std::process::exit(2);
            }
            run_stdin()
        }
    }
}
