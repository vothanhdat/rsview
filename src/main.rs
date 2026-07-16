//! jview — proof-of-concept lazy JSON viewer in Rust.
//!
//! Demonstrates the core of react-obj-view's CLI in native Rust: a file is
//! memory-mapped, parsed on expand (subtrees are byte ranges, not materialized
//! values), and a level is flattened only as far as the viewport scrolls
//! (windowing). Opening a multi-GB file stays near-constant memory.

mod filter;
mod input;
mod scanner;
mod schema;
mod search;
mod source;
mod stream;
mod tree;
mod ui;
use filter::{parse_pipeline, Filter, Program};
use input::TextInput;
use scanner::{container_empty, decode_str, skip_value, skip_ws, Kind};
use search::{Pattern, Search};
use source::Source;
use stream::{spawn_reader, take_pipe_reader, StreamStore, STREAM_REBUILD_MS};
use tree::{
    breadcrumb_segments, collect_expanded, count_children, expand_to, flatten, get, get_mut,
    join_path, make_root, make_subroot, parse_path, resolve_with_climb, set_expanded, truncate,
    Node, Row,
};

use memmap2::Mmap;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind},
    layout::Rect,
};
use std::{
    collections::HashSet,
    fs::File,
    io::{IsTerminal, Write},
    sync::{
        mpsc::{Receiver, TryRecvError},
        Arc, Weak,
    },
    time::{Duration, Instant},
};

/// Upper bound on the bytes the peek overlay decodes for a single scalar. Peek is
/// on-demand (one keypress, not per-frame), so this is far more generous than the
/// row cap — enough to read a big embedded blob — while still bounding the decode
/// and wrap of a pathological multi-hundred-MB string.
const PEEK_MAX_BYTES: usize = 8 << 20; // 8 MiB

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

/// Wall-clock budget for one cooperative `flatten` pass. When reaching the next
/// on-screen row means skipping over a huge value (e.g. a 1 GB array to find the
/// sibling after it), `flatten` yields after this long so the loop can paint what
/// it has and poll input; the skip resumes next frame. Keeps the first paint and
/// every later frame responsive instead of blocking on one giant `skip_value`.
const FLATTEN_BUDGET: Duration = Duration::from_millis(8);

#[derive(PartialEq)]
pub(crate) enum Mode {
    Normal,
    Search,
    /// Typing a path to jump to (`:` prompt, e.g. `data.users[3].city`).
    Goto,
    /// Typing a jq-style filter (`|` prompt, e.g. `.users[] | select(.age > 30)`).
    Filter,
    /// The bookmark picker overlay (`'`): pick a saved node to jump to.
    Marks,
    /// The keyboard-shortcut cheatsheet overlay (`?`): any key closes it.
    Help,
    /// The value-peek overlay (`Enter`/`Space` on a scalar leaf): the focused
    /// value decoded in full and scrollable. See [`View::peek`].
    Peek,
    /// The schema/shape overlay (`t` on a container): a sampled field→type
    /// summary of a container's children. See [`View::schema`].
    Schema,
}

/// The state behind a [`Mode::Peek`] overlay: one scalar's full value, decoded
/// once when the overlay opens (with JSON escapes rendered — `\n` becomes a real
/// line break) so it can be read wrapped and scrolled without touching the source
/// again. `scroll` is the top wrapped-line offset.
pub(crate) struct Peek {
    /// The focused node's label, shown in the card title.
    pub(crate) title: String,
    /// The decoded value; may be capped at `PEEK_MAX_BYTES` of source.
    pub(crate) text: String,
    /// True when the decode hit the cap, so the title can flag it.
    pub(crate) truncated: bool,
    /// Top visible wrapped line (advanced by the scroll keys).
    pub(crate) scroll: usize,
}

/// The `t` schema overlay: the focused node's inferred structural type (see
/// [`schema`]), rendered TypeScript-style and scrollable, plus its copyable source.
pub(crate) struct SchemaView {
    /// The focused node's label, for the card title.
    pub(crate) title: String,
    /// The rendered type as display lines.
    pub(crate) lines: Vec<String>,
    /// The same type as copyable text (`y` yanks it).
    pub(crate) source: String,
    /// Top visible line (advanced by the scroll keys).
    pub(crate) scroll: usize,
}

/// What a key press asks the run loop to do that it can't do itself: quit, or
/// (re)launch the search — which needs a byte `Source` the caller supplies (a
/// fixed mmap for files, a fresh snapshot for streams).
enum KeyOutcome {
    Continue,
    Quit,
    Relaunch,
    /// Run the pipeline stashed in `App::pending_filter`, opening a result pane.
    /// Deferred to the run loop because spawning the worker needs an owned byte
    /// `Source` (a fixed mmap for files, a fresh snapshot for streams).
    LaunchFilter,
}

/// A search scope: the focused container's subtree, captured when the `/` prompt
/// opens. `Tab` toggles between searching this subtree and the whole document.
/// `path` is the container's absolute path from the pane root, so subtree-local
/// match paths (the worker indexes children from 0) can be lifted back to
/// absolute paths that line up with the rows.
#[derive(Clone)]
pub(crate) struct Scope {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) kind: Kind,
    pub(crate) jsonl: bool,
    pub(crate) path: Vec<usize>,
    pub(crate) label: String,
}

/// One pane: an independent lazy tree + viewport over a byte range of the shared
/// `Source`. The main pane is the whole document; a split pane (`derived`) is
/// rooted at another pane's focused node. Each keeps its own focus, scroll,
/// expansion, and search.
pub(crate) struct View {
    pub(crate) root: Node,
    pub(crate) name: String,
    /// True for a pane spun off by `s` (rooted at a sub-range), false for the
    /// original document pane. Drives the `↳` title marker and which pane the
    /// streaming re-parse feeds.
    pub(crate) derived: bool,
    /// Relative size in the workspace layout (a `Fill` weight); `+`/`-` adjust it.
    pub(crate) weight: u16,
    /// Stable identity for parent/child links (Vec indices shift when a pane is
    /// closed, so links can't be indices).
    pub(crate) id: u64,
    /// The pane this was split from. Closing a pane closes its descendants too.
    pub(crate) parent: Option<u64>,
    /// The child reused by `o` (open-or-replace): re-rooted in place rather than
    /// opening yet another pane.
    pub(crate) preview_child: Option<u64>,
    pub(crate) focus: usize,
    pub(crate) scroll: usize,
    pub(crate) rows: Vec<Row>,
    pub(crate) mode: Mode,
    /// Live search-input buffer (typed while in `Mode::Search`).
    pub(crate) query: TextInput,
    /// Set when the typed `re:`/`g:` query failed to compile, so the footer can
    /// surface "(bad pattern)" without re-parsing. Cleared on a clean compile.
    pub(crate) query_error: Option<String>,
    /// The running search, if any. `None` once cleared/cancelled.
    pub(crate) search: Option<Search>,
    /// The focused container captured when `/` opened, or `None` if the focus
    /// wasn't a container. `Tab` in the prompt flips `scoped` to search just this
    /// subtree (faster and quieter in a huge document) vs. the whole pane.
    pub(crate) search_scope: Option<Scope>,
    pub(crate) scoped: bool,
    /// Which match the cursor is currently on.
    pub(crate) match_idx: usize,
    /// Whether match-cycling has landed on a result yet (so the first
    /// next/prev press goes to the first/last match, not the second).
    pub(crate) landed: bool,
    /// Set of match paths, for O(1) row highlighting. Grown incrementally.
    pub(crate) match_set: HashSet<Vec<usize>>,
    /// How many of `search.matches` are already in `match_set`.
    pub(crate) indexed: usize,
    /// A pending jump target: the next frame flattens far enough to land on it.
    pub(crate) want_path: Option<Vec<usize>>,
    /// Set when the last cooperative `flatten_window` yielded mid-skip (a huge
    /// value still being stepped over). The run loop keeps flattening — without
    /// blocking on input — until it clears. Always false after a jump.
    pub(crate) flatten_incomplete: bool,
    /// Live path-input buffer (typed while in `Mode::Goto`).
    pub(crate) goto: TextInput,
    /// Saved node paths (`m` toggles), jumped to via the `'` picker overlay.
    pub(crate) bookmarks: Vec<Vec<usize>>,
    /// Selected row in the bookmark picker.
    pub(crate) mark_idx: usize,
    /// Live filter-input buffer (typed while in `Mode::Filter`).
    pub(crate) filter_query: TextInput,
    /// Set when the typed filter failed to parse, so the footer can surface why
    /// without closing the prompt. Cleared on the next edit.
    pub(crate) filter_error: Option<String>,
    /// The running filter for a *result* pane: its streamed hits become this
    /// pane's synthetic-array children. `None` for ordinary panes.
    pub(crate) filter: Option<Filter>,
    /// How many of `filter.hits` are already materialized as root children.
    pub(crate) filter_added: usize,
    /// The open value-peek overlay, if any (`Mode::Peek`).
    pub(crate) peek: Option<Peek>,
    /// The open schema overlay, if any (`Mode::Schema`).
    pub(crate) schema: Option<SchemaView>,
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
            query: TextInput::default(),
            query_error: None,
            search: None,
            search_scope: None,
            scoped: false,
            match_idx: 0,
            match_set: HashSet::new(),
            indexed: 0,
            want_path: None,
            landed: false,
            goto: TextInput::default(),
            bookmarks: Vec::new(),
            mark_idx: 0,
            flatten_incomplete: false,
            filter_query: TextInput::default(),
            filter_error: None,
            filter: None,
            filter_added: 0,
            peek: None,
            schema: None,
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
        self.peek = None;
        self.schema = None;
        self.goto.clear();
        // Bookmarked paths refer to the old tree; drop them on re-root.
        self.bookmarks.clear();
        self.mark_idx = 0;
        // A filter's hits point into the old root; drop the worker and its state.
        if let Some(f) = self.filter.take() {
            f.cancel();
        }
        self.filter_added = 0;
        self.filter_query.clear();
        self.filter_error = None;
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

    /// If the focused row is a scalar leaf, open the value-peek overlay on it and
    /// return `true`; containers return `false` so the caller expands/collapses
    /// instead. The full value is decoded once here (bounded by `PEEK_MAX_BYTES`,
    /// escapes rendered) so the overlay reads and scrolls without re-touching the
    /// source.
    fn peek_focused(&mut self, b: &[u8]) -> bool {
        let Some(row) = self.rows.get(self.focus) else {
            return false;
        };
        if matches!(row.kind, Kind::Object | Kind::Array) {
            return false; // a container — let Enter toggle it
        }
        let node = get(&self.root, &row.path);
        let cap = node.start.saturating_add(PEEK_MAX_BYTES);
        let e = node.end.min(cap);
        let truncated = node.end > cap;
        let text = match node.kind {
            Kind::Str => decode_str(b, node.start, e),
            // Other scalars are their literal bytes (a long bignum, say).
            _ => String::from_utf8_lossy(&b[node.start..e]).into_owned(),
        };
        self.peek = Some(Peek {
            title: row.label.clone(),
            text,
            truncated,
            scroll: 0,
        });
        self.mode = Mode::Peek;
        true
    }

    /// (Re)launch the live search for the current `query`. Dropping the previous
    /// `Search` cancels its worker thread; an empty query just clears results.
    /// A `re:`/`g:` query that fails to compile is treated as zero-matches —
    /// the footer surfaces the parse error so the user can fix the expression
    /// without losing what they've typed.
    /// The focused container's subtree as a search scope, or `None` if the focus
    /// isn't a container (nothing to scope into).
    fn scope_of_focus(&self) -> Option<Scope> {
        let row = self.rows.get(self.focus)?;
        let node = get(&self.root, &row.path);
        if node.jsonl || matches!(node.kind, Kind::Object | Kind::Array) {
            Some(Scope {
                start: node.start,
                end: node.end,
                kind: node.kind,
                jsonl: node.jsonl,
                path: row.path.clone(),
                label: row.label.clone(),
            })
        } else {
            None
        }
    }

    /// Lift a worker match path to absolute. A scoped search scans a subtree and
    /// indexes its children from 0, so those paths need the scope's own path
    /// prepended to line up with the pane's rows; an unscoped search already
    /// yields absolute paths.
    fn abs_match(&self, p: &[usize]) -> Vec<usize> {
        match (self.scoped, &self.search_scope) {
            (true, Some(s)) => {
                let mut full = s.path.clone();
                full.extend_from_slice(p);
                full
            }
            _ => p.to_vec(),
        }
    }

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
        let pattern = match Pattern::parse(self.query.as_str()) {
            Ok(p) => p,
            Err(e) => {
                self.query_error = Some(e);
                return;
            }
        };
        // Scan just the focused subtree when scoped, else the whole pane root.
        let (start, end, kind, jsonl) = match (self.scoped, &self.search_scope) {
            (true, Some(s)) => (s.start, s.end, s.kind, s.jsonl),
            _ => (
                self.root.start,
                self.root.end,
                self.root.kind,
                self.root.jsonl,
            ),
        };
        self.search = Some(Search::spawn(
            Arc::clone(mmap),
            pattern,
            jsonl,
            start,
            end,
            kind,
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
            self.match_set.insert(self.abs_match(&p));
            self.indexed += 1;
        }
    }

    /// Fold any newly-selected filter hits into the result pane's synthetic array
    /// root as fresh children. Appending (never rebuilding) keeps already-expanded
    /// children — and their indices — stable as more results stream in.
    fn pump_filter(&mut self, b: &[u8]) {
        if self.filter.is_none() {
            return;
        }
        self.filter.as_mut().unwrap().drain();
        let total = self.filter.as_ref().unwrap().hits.len();
        while self.filter_added < total {
            // Snapshot the hit's fields, ending the borrow of `self.filter` before
            // mutating `self.root`.
            let (mut label, start, end, kind, end_exact) = {
                let h = &self.filter.as_ref().unwrap().hits[self.filter_added];
                (h.label.clone(), h.start, h.end, h.kind, h.end_exact)
            };
            if label.is_empty() {
                label = self.filter_added.to_string();
            }
            let is_cont = matches!(kind, Kind::Object | Kind::Array);
            let has = is_cont && !container_empty(b, start, end);
            self.root.children.push(Node {
                label,
                start,
                end,
                end_exact,
                kind,
                is_index: false,
                jsonl: false,
                has_children: has,
                expanded: false,
                done: false,
                children: Vec::new(),
                cursor: None,
            });
            self.filter_added += 1;
        }
        if self.filter_added > 0 {
            self.root.has_children = true;
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
        let path = self.abs_match(&path);
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
        let parsed = parse_path(self.goto.as_str());
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
            None => format!("path not found: {}", self.goto.as_str().trim()),
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
        self.search_scope = None;
        self.scoped = false;
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
pub(crate) struct App {
    pub(crate) views: Vec<View>,
    pub(crate) active: usize,
    /// Pane orientation: false = side by side (columns), true = stacked (rows).
    /// Toggled with `\`.
    pub(crate) stacked: bool,
    /// Monotonic source of pane ids (never reused, so links stay unambiguous).
    pub(crate) next_id: u64,
    /// Transient status line (e.g. "copied 42 B") shown in the footer until the
    /// next key press. `None` = show the normal key hint.
    pub(crate) flash: Option<String>,
    /// True when stdout was redirected (a pipe or file), so `p` can extract the
    /// focused node into it. Set by `run_file`/`run_stdin` after reserving
    /// stdout; into a terminal there's nowhere to pipe, so `p` is a hint instead.
    pub(crate) can_extract: bool,
    /// Set by `p` to the focused node's byte range; the viewer quits and the
    /// caller writes that slice to the reserved stdout on the way out.
    pub(crate) extract: Option<(usize, usize)>,
    /// Set by `p` on a filter-result pane: the byte range of *every* hit, emitted
    /// as NDJSON (one per line) on the way out — batch-carving many subtrees at
    /// once, the same zero-copy way `extract` carves one.
    pub(crate) extract_batch: Option<Vec<(usize, usize)>>,
    /// A filter awaiting launch: stashed by the `|` prompt, consumed by the run
    /// loop (which has the owned byte `Source` the worker needs).
    pub(crate) pending_filter: Option<PendingFilter>,
}

/// A parsed, ready-to-run filter plus the pane range it should evaluate over.
pub(crate) struct PendingFilter {
    pub(crate) program: Program,
    /// The raw expression, kept for the result pane's title.
    pub(crate) expr: String,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) kind: Kind,
    pub(crate) jsonl: bool,
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
            extract_batch: None,
            pending_filter: None,
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

    pub(crate) fn active_view(&self) -> &View {
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
    /// [`ui::pane_layout`].
    pub(crate) fn weights(&self) -> Vec<u16> {
        self.views.iter().map(|v| v.weight).collect()
    }

    /// The active pane's content height (rows) for a given screen area — used to
    /// size paging jumps, since each pane reserves a title + footer row.
    fn active_height(&self, area: Rect) -> usize {
        let rects = ui::pane_layout(ui::panes_area(area), &self.weights(), self.stacked);
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

    /// Launch the pending filter: spawn its worker over `src` and open a new
    /// child pane whose synthetic array root collects the selected nodes as they
    /// stream in. A no-op if nothing is pending. `src` is the owned byte source
    /// (the session mmap, or a stream snapshot) the worker reads.
    fn launch_filter(&mut self, src: &Arc<Source>) {
        let Some(pf) = self.pending_filter.take() else {
            return;
        };
        let filter = Filter::spawn(
            Arc::clone(src),
            pf.program,
            pf.jsonl,
            pf.start,
            pf.end,
            pf.kind,
        );
        // Keep the title compact so the live `N hits` count stays visible even in
        // a narrow split pane; the full expression is what the user just typed.
        let name = format!("| {}", truncate(&pf.expr, 28));
        // A synthetic, pre-expanded array whose children are appended by
        // `View::pump_filter`. It owns no cursor: the worker, not a byte range,
        // is its source of children (`start`/`end` are the searched range, so a
        // whole-root copy still slices something sane).
        let root = Node {
            label: name.clone(),
            start: pf.start,
            end: pf.end,
            end_exact: false,
            kind: Kind::Array,
            is_index: false,
            jsonl: false,
            has_children: false,
            expanded: true,
            done: false,
            children: Vec::new(),
            cursor: None,
        };
        let parent = self.views[self.active].id;
        let id = self.alloc_id();
        let mut v = View::with_root(root, name, true);
        v.id = id;
        v.parent = Some(parent);
        v.filter = Some(filter);
        self.views.push(v);
        self.active = self.views.len() - 1;
    }
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
    let rects = ui::pane_layout(ui::panes_area(term_area()?), &app.weights(), app.stacked);
    for (i, v) in app.views.iter_mut().enumerate() {
        let h = (rects[i * 2].height as usize).saturating_sub(1).max(1);
        v.flatten_window(b, h);
    }
    term.draw(|f| ui::draw(f, app, streaming))?;
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
            // Tab scopes the search to the focused container (captured at open)
            // and back — a no-op if the focus wasn't a container.
            KeyCode::Tab | KeyCode::BackTab if v.search_scope.is_some() => {
                v.scoped = !v.scoped;
                return KeyOutcome::Relaunch;
            }
            // Editing/caret keys go to the input; only a text change relaunches
            // the live search (a caret move leaves the query — and results — put).
            other => {
                if let Some(changed) = v.query.edit(other, ctrl) {
                    if changed {
                        return KeyOutcome::Relaunch;
                    }
                }
            }
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
            other => {
                app.active_mut().goto.edit(other, ctrl);
            }
        }
        return KeyOutcome::Continue;
    }

    // Filter mode: a `|` jq-style prompt. Enter parses the pipeline and (on
    // success) hands it to the run loop to open a result pane; Esc cancels.
    if app.active_view().mode == Mode::Filter {
        match k.code {
            KeyCode::Esc => {
                let v = app.active_mut();
                v.mode = Mode::Normal;
                v.filter_query.clear();
                v.filter_error = None;
            }
            KeyCode::Enter => {
                let expr = app.active_view().filter_query.as_str().trim().to_string();
                if expr.is_empty() {
                    let v = app.active_mut();
                    v.mode = Mode::Normal;
                    v.filter_error = None;
                    return KeyOutcome::Continue;
                }
                match parse_pipeline(&expr) {
                    Ok(program) => {
                        let (start, end, kind, jsonl) = {
                            let r = &app.active_view().root;
                            (r.start, r.end, r.kind, r.jsonl)
                        };
                        app.pending_filter = Some(PendingFilter {
                            program,
                            expr,
                            start,
                            end,
                            kind,
                            jsonl,
                        });
                        let v = app.active_mut();
                        v.mode = Mode::Normal;
                        v.filter_query.clear();
                        v.filter_error = None;
                        return KeyOutcome::LaunchFilter;
                    }
                    Err(e) => app.active_mut().filter_error = Some(e),
                }
            }
            other => {
                let v = app.active_mut();
                if v.filter_query.edit(other, ctrl).is_some() {
                    v.filter_error = None;
                }
            }
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

    // Peek mode: the full-value overlay. Scroll keys move within it; Esc/q/Enter
    // close. Clamp against the same wrap the renderer uses so `k` after the bottom
    // reacts immediately instead of eating dead presses.
    if app.active_view().mode == Mode::Peek {
        let (_, inner_w, inner_h) = ui::peek_layout(term_area().unwrap_or(Rect::new(0, 0, 80, 24)));
        let v = app.active_mut();
        if let Some(pk) = v.peek.as_mut() {
            let max = ui::wrap_for_peek(&pk.text, inner_w)
                .len()
                .saturating_sub(inner_h);
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                    v.mode = Mode::Normal;
                    v.peek = None;
                }
                KeyCode::Down | KeyCode::Char('j') => pk.scroll = (pk.scroll + 1).min(max),
                KeyCode::Up | KeyCode::Char('k') => pk.scroll = pk.scroll.saturating_sub(1),
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    pk.scroll = (pk.scroll + inner_h).min(max)
                }
                KeyCode::PageUp => pk.scroll = pk.scroll.saturating_sub(inner_h),
                KeyCode::Char('f') if ctrl => pk.scroll = (pk.scroll + inner_h).min(max),
                KeyCode::Char('b') if ctrl => pk.scroll = pk.scroll.saturating_sub(inner_h),
                KeyCode::Home | KeyCode::Char('g') => pk.scroll = 0,
                KeyCode::End | KeyCode::Char('G') => pk.scroll = max,
                _ => {}
            }
        } else {
            v.mode = Mode::Normal; // defensive: no state → nothing to show
        }
        return KeyOutcome::Continue;
    }

    // Schema mode: the inferred-type overlay. Scroll the type; `y` copies it;
    // Esc/q/t/Enter close.
    if app.active_view().mode == Mode::Schema {
        let (_, _, inner_h) = ui::peek_layout(term_area().unwrap_or(Rect::new(0, 0, 80, 24)));
        // `y` copies the whole type to the clipboard (handled before the mutable
        // borrow so it can set the app-level flash).
        if matches!(k.code, KeyCode::Char('y')) {
            if let Some(src) = app.active_view().schema.as_ref().map(|s| s.source.clone()) {
                copy_to_clipboard(src.as_bytes());
                app.flash = Some(format!("copied type ({} B)", src.len()));
            }
            return KeyOutcome::Continue;
        }
        let v = app.active_mut();
        if let Some(sc) = v.schema.as_mut() {
            let max = sc.lines.len().saturating_sub(inner_h);
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('t') | KeyCode::Enter => {
                    v.mode = Mode::Normal;
                    v.schema = None;
                }
                KeyCode::Down | KeyCode::Char('j') => sc.scroll = (sc.scroll + 1).min(max),
                KeyCode::Up | KeyCode::Char('k') => sc.scroll = sc.scroll.saturating_sub(1),
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    sc.scroll = (sc.scroll + inner_h).min(max)
                }
                KeyCode::PageUp => sc.scroll = sc.scroll.saturating_sub(inner_h),
                KeyCode::Char('f') if ctrl => sc.scroll = (sc.scroll + inner_h).min(max),
                KeyCode::Char('b') if ctrl => sc.scroll = sc.scroll.saturating_sub(inner_h),
                KeyCode::Home | KeyCode::Char('g') => sc.scroll = 0,
                KeyCode::End | KeyCode::Char('G') => sc.scroll = max,
                _ => {}
            }
        } else {
            v.mode = Mode::Normal;
        }
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
            // On a filter-result pane, `p` carves out *all* the hits as NDJSON;
            // elsewhere it carves the one focused node.
            if let Some(f) = &app.active_view().filter {
                let ranges: Vec<(usize, usize)> = f.hits.iter().map(|h| (h.start, h.end)).collect();
                if ranges.is_empty() {
                    app.flash = Some("no filter hits to extract".to_string());
                    return KeyOutcome::Continue;
                }
                app.extract_batch = Some(ranges);
                return KeyOutcome::Quit;
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
        // `c` counts the focused container's direct children — the size of a
        // collapsed level without expanding it (a full scan, but on demand).
        KeyCode::Char('c') => {
            let v = app.active_view();
            let msg = v.rows.get(v.focus).map(|row| {
                let node = get(&v.root, &row.path);
                match count_children(node, b) {
                    Some(n) => {
                        let what = if matches!(node.kind, Kind::Array) || node.jsonl {
                            "elements"
                        } else {
                            "entries"
                        };
                        format!("{}: {} {what}", row.label, group(n))
                    }
                    None => format!("{}: not a container", row.label),
                }
            });
            if let Some(m) = msg {
                app.flash = Some(m);
            }
            return KeyOutcome::Continue;
        }
        // `t` shows the focused container's shape: a sampled field→type summary.
        // `t` infers the focused node's structural type (JSON→TypeScript-style).
        KeyCode::Char('t') => {
            let v = app.active_view();
            let built = v.rows.get(v.focus).and_then(|row| {
                let node = get(&v.root, &row.path);
                if !node.jsonl && !matches!(node.kind, Kind::Object | Kind::Array) {
                    return None;
                }
                let ty = schema::infer(b, node.start, node.end, node.kind, node.jsonl);
                let lines = schema::render(&ty);
                Some(SchemaView {
                    title: row.label.clone(),
                    source: schema::to_source(&ty),
                    lines,
                    scroll: 0,
                })
            });
            match built {
                Some(s) => {
                    let v = app.active_mut();
                    v.schema = Some(s);
                    v.mode = Mode::Schema;
                }
                None => app.flash = Some("schema: focus an array or object".to_string()),
            }
            return KeyOutcome::Continue;
        }
        // `:` opens the path-jump prompt; `m` bookmarks the focused node; `'`
        // opens the bookmark picker.
        KeyCode::Char(':') => {
            let v = app.active_mut();
            v.mode = Mode::Goto;
            v.goto.clear();
            return KeyOutcome::Continue;
        }
        // `|` opens the jq-style filter prompt; Enter opens a result pane.
        KeyCode::Char('|') => {
            let v = app.active_mut();
            v.mode = Mode::Filter;
            v.filter_query.clear();
            v.filter_error = None;
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
            // Capture the focused container so `Tab` can scope the search into it.
            v.search_scope = v.scope_of_focus();
            v.scoped = false;
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
        // Enter/Space/→ expands a container; on a scalar leaf it opens the
        // value-peek overlay (there's nothing to expand).
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right => {
            if !v.peek_focused(b) {
                v.toggle_focus();
            }
        }
        KeyCode::Left => v.collapse_or_parent(),
        _ => {}
    }
    KeyOutcome::Continue
}

/// Group an integer with thousands separators: `1234567` → `"1,234,567"`.
fn group(n: usize) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, &c) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c as char);
    }
    out
}

fn term_area() -> std::io::Result<Rect> {
    let (w, h) = ratatui::crossterm::terminal::size()?;
    Ok(Rect::new(0, 0, w, h))
}

/// Which pane (index) the screen cell `(col, row)` falls in, if any — for
/// routing a mouse-wheel scroll to the pane under the cursor.
fn pane_at(app: &App, col: u16, row: u16) -> std::io::Result<Option<usize>> {
    let rects = ui::pane_layout(ui::panes_area(term_area()?), &app.weights(), app.stacked);
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
                KeyOutcome::LaunchFilter => outcome = KeyOutcome::LaunchFilter,
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
        // Fold in any matches / filter hits the worker threads produced last frame.
        for v in &mut app.views {
            v.pump_search();
            v.pump_filter(b);
        }
        render_frame(term, app, b, false)?;
        let h = app.active_height(term_area()?);
        // While a flatten is mid-skip, don't block on input: poll at 0 ms and loop
        // so the next frame steps the skip another `FLATTEN_BUDGET` (still
        // dispatching any key pressed meanwhile). Otherwise a short poll so
        // streaming match/filter counts keep ticking even when idle.
        let poll_ms = if app.flatten_pending() { 0 } else { 100 };
        match pump_input(app, b, h, poll_ms)? {
            KeyOutcome::Quit => return Ok(()),
            KeyOutcome::Relaunch => app.active_mut().relaunch(mmap),
            KeyOutcome::LaunchFilter => app.launch_filter(mmap),
            KeyOutcome::Continue => {}
        }
    }
}

/// The streaming loop: bytes arrive on `rx` from the reader thread, the buffer
/// grows, and the tree is re-parsed on a throttle (preserving cursor/expansion).
/// Search snapshots the buffer at launch (so it covers the bytes parsed so far).
fn run_stream(
    term: &mut ratatui::DefaultTerminal,
    app: &mut App,
    store: &mut StreamStore,
    jsonl: &mut bool,
    rx: Receiver<Vec<u8>>,
) -> std::io::Result<()> {
    let mut dirty = false; // bytes arrived that aren't in the tree yet
    let mut done = false; // reader hit EOF
    let mut last_build = Instant::now();
    // The most recent buffer snapshot handed to a search/filter worker, held
    // weakly. Search relaunches once per keystroke, so typing a query would
    // otherwise clone the whole (multi-GB) buffer on every character. Because a
    // stream only appends, a snapshot stays a valid prefix until the buffer
    // grows: consecutive launches at the same length reuse the one `Arc`
    // (the live worker keeps it alive), and once every worker drops it — search
    // closed, filter pane closed — the copy is freed. Zero steady-state cost.
    let mut snapshot: Weak<Source> = Weak::new();
    loop {
        // Drain whatever the reader thread has produced into the store (spilled
        // to the temp file, or the RAM fallback). The chunk is freed right after,
        // so nothing accumulates in memory beyond the store itself.
        loop {
            match rx.try_recv() {
                Ok(chunk) => {
                    store.append(&chunk);
                    dirty = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    done = true;
                    break;
                }
            }
        }
        // Refresh the mapping to cover the newly-arrived bytes *before* anything
        // reads them — the sniff and the rebuild both need the grown view (unlike
        // the RAM fallback, a spill's `bytes()` lags until it's re-mapped).
        if dirty {
            store.sync();
        }
        // NDJSON detection is sticky: once multi-doc, stay multi-doc.
        if !*jsonl && sniff_multi(store.bytes()) {
            *jsonl = true;
        }
        // Re-parse on a throttle, or immediately once the stream is complete.
        // Only the document pane tracks the growing buffer; split panes are
        // snapshots of the bytes that had arrived when they were spun off.
        if dirty && (done || last_build.elapsed().as_millis() >= STREAM_REBUILD_MS) {
            if let Some(main) = app.views.iter_mut().find(|v| !v.derived) {
                main.rebuild(store.bytes(), *jsonl);
            }
            dirty = false;
            last_build = Instant::now();
        }

        for v in &mut app.views {
            v.pump_search();
        }
        // Borrow the store's bytes just for this frame's render + input, then
        // release the borrow so a search relaunch can snapshot the store below.
        let outcome = {
            let b: &[u8] = store.bytes();
            // Fold in filter hits here, where the live buffer is borrowed (a stream
            // only appends, so a hit's offsets stay valid against the grown buffer).
            for v in &mut app.views {
                v.pump_filter(b);
            }
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
                let snap = store.snapshot(&mut snapshot);
                app.active_mut().relaunch(&snap);
            }
            KeyOutcome::LaunchFilter => {
                // Filter over a snapshot of the bytes arrived so far, mirroring
                // search: the result pane reflects the document at launch time.
                let snap = store.snapshot(&mut snapshot);
                app.launch_filter(&snap);
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
    fn into_writer(self) -> Box<dyn Write> {
        match self.file {
            Some(f) => Box::new(f),
            None => Box::new(std::io::stdout()),
        }
    }

    fn emit(self, bytes: &[u8]) -> std::io::Result<()> {
        let mut w = self.into_writer();
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

/// Write every filter hit to the reserved stdout as NDJSON — one trimmed value
/// per line — streaming each slice straight from the source (no concatenation).
fn write_extract_all(b: &[u8], ranges: &[(usize, usize)], sink: Payload) -> std::io::Result<()> {
    let mut w = sink.into_writer();
    for &(start, prov_end) in ranges {
        let end = skip_value(b, start, prov_end);
        let slice = &b[start..end];
        let cut = slice
            .iter()
            .rposition(|c| !c.is_ascii_whitespace())
            .map_or(0, |p| p + 1);
        w.write_all(&slice[..cut])?;
        w.write_all(b"\n")?;
    }
    w.flush()
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
    // After restoring the terminal, hand the chosen node(s) to the reserved stdout.
    if let Some(ranges) = app.extract_batch.take() {
        write_extract_all(b, &ranges, payload)?;
    } else if let Some(range) = app.extract {
        write_extract(b, range, payload)?;
    }
    res
}

/// Stream piped stdin: the JSON renders progressively as it arrives. A pipe
/// can't be mmap'd, so it's spilled to an (unlinked) temp file we mmap — RSS
/// stays ~flat like the file path — or buffered in RAM where that's not
/// available, and re-parsed on a throttle.
fn run_stdin() -> std::io::Result<()> {
    let rx = spawn_reader(take_pipe_reader());
    // stdin was the pipe; fd 0 is now the terminal (reattached above). Reserve
    // stdout so `… | jview | jq` can extract a node into the downstream pipe.
    let payload = reserve_stdout_for_payload();
    let mut store = StreamStore::new();
    let mut jsonl = false;
    let mut app = App::single(View::new(store.bytes(), "stdin", jsonl));
    app.can_extract = payload.enabled;
    if app.can_extract {
        app.flash = Some(EXTRACT_HINT.to_string());
    }
    let mut term = ratatui::init();
    // Same as run_file: paint before the up-to-2s keyboard-enhancement probe so
    // the (initially empty) streaming pane shows immediately, not a blank screen.
    let _ = render_frame(&mut term, &mut app, store.bytes(), true);
    let enhanced = enable_enhanced_keys();
    enable_mouse();
    let res = run_stream(&mut term, &mut app, &mut store, &mut jsonl, rx);
    disable_mouse();
    if enhanced {
        disable_enhanced_keys();
    }
    ratatui::restore();
    if let Some(ranges) = app.extract_batch.take() {
        write_extract_all(store.bytes(), &ranges, payload)?;
    } else if let Some(range) = app.extract {
        write_extract(store.bytes(), range, payload)?;
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
    use crate::tree::{resolve_path, ParsedPath, Seg};

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
    fn write_extract_all_emits_one_ndjson_line_per_hit() {
        // Batch extract (`p` on a filter pane) writes each hit's real slice on its
        // own line — provisional ends resolved, so hits never bleed into siblings.
        let b = br#"[{"n":"a"},{"n":"b"},{"n":"c"}]"#;
        // Ranges for the three objects, each with an over-long provisional end to
        // prove skip_value re-resolves the closer per hit.
        let ranges: Vec<(usize, usize)> = b
            .iter()
            .enumerate()
            .filter(|(_, &c)| c == b'{')
            .map(|(i, _)| (i, b.len()))
            .collect();
        assert_eq!(ranges.len(), 3);
        let path = std::env::temp_dir().join(format!("jview-batch-{}.json", std::process::id()));
        let sink = Payload {
            enabled: true,
            file: Some(File::create(&path).expect("temp file")),
        };
        write_extract_all(b, &ranges, sink).expect("write");
        let got = std::fs::read(&path).expect("read back");
        let _ = std::fs::remove_file(&path);
        assert_eq!(got, b"{\"n\":\"a\"}\n{\"n\":\"b\"}\n{\"n\":\"c\"}\n");
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
