//! rsview — proof-of-concept lazy JSON viewer in Rust.
//!
//! Demonstrates the core of react-obj-view's CLI in native Rust: a file is
//! memory-mapped, parsed on expand (subtrees are byte ranges, not materialized
//! values), and a level is flattened only as far as the viewport scrolls
//! (windowing). Opening a multi-GB file stays near-constant memory.

mod scanner;
mod search;
mod source;
use scanner::{container_empty, decode_str, skip_value, skip_ws, value_kind, Cursor, Kind, RawChild};
use search::Search;
use source::Source;

use memmap2::Mmap;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    layout::{Constraint, Layout},
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
            Kind::Str => format!("\"{}\"", truncate(&decode_str(b, self.start, self.end), 70)),
            _ => truncate(&String::from_utf8_lossy(&b[self.start..self.end]), 70),
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
        Kind::Str => format!("\"{}\"", truncate(&decode_str(b, rc.start, rc.end), 24)),
        _ => truncate(&String::from_utf8_lossy(&b[rc.start..rc.end]), 24),
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
    if node.expanded && node.is_container() {
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
    if path.is_empty() {
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

struct App {
    root: Node,
    name: String,
    focus: usize,
    scroll: usize,
    rows: Vec<Row>,
    mode: Mode,
    /// Live search-input buffer (typed while in `Mode::Search`).
    query: String,
    /// The running search, if any. `None` once cleared/cancelled.
    search: Option<Search>,
    /// Which match `n`/`N` is currently on.
    match_idx: usize,
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

impl App {
    fn new(b: &[u8], path: &str, jsonl: bool) -> App {
        let name = std::path::Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "ROOT".into());
        let root = make_root(b, &name, jsonl);
        App {
            root,
            name,
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
        }
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
        self.want_path = None;
        if self.query.is_empty() {
            return;
        }
        self.search = Some(Search::spawn(Arc::clone(mmap), self.query.clone(), self.root.jsonl));
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

    /// Move to the next/previous match (`dir` = +1 / -1) and queue a jump to it.
    fn step_match(&mut self, dir: i32, b: &[u8]) {
        let n = self.search.as_ref().map_or(0, |s| s.matches.len());
        if n == 0 {
            return;
        }
        self.match_idx = if dir >= 0 {
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
}

fn ui(f: &mut Frame, app: &App, h: usize, streaming: bool) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(f.area());

    let title = if streaming {
        format!(" {}   {}/{}+   ⟳ streaming", app.name, app.focus + 1, app.rows.len())
    } else {
        format!(" {}   {}/{}+", app.name, app.focus + 1, app.rows.len())
    };
    f.render_widget(
        Paragraph::new(title).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    let cur_match = app
        .search
        .as_ref()
        .and_then(|s| s.matches.get(app.match_idx));

    let mut lines = Vec::new();
    let end = (app.scroll + h).min(app.rows.len());
    for i in app.scroll..end {
        let r = &app.rows[i];
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

        let line = if i == app.focus {
            // Selection bar: plain reversed, colors dropped for maximum contrast.
            let text = format!("{indent}{marker} {}: {}", r.label, r.value);
            Line::from(Span::styled(text, Style::default().add_modifier(Modifier::REVERSED)))
        } else if cur_match == Some(&r.path) || app.match_set.contains(&r.path) {
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

    let footer = if app.mode == Mode::Search {
        let count = app.search.as_ref().map_or(0, |s| s.matches.len());
        let more = match &app.search {
            Some(s) if !s.finished => "+",
            _ => "",
        };
        Span::styled(
            format!(" /{}    {}{} matches · enter go · esc cancel", app.query, count, more),
            Style::default().fg(Color::Yellow),
        )
    } else if app.search.is_some() {
        let count = app.search.as_ref().map_or(0, |s| s.matches.len());
        Span::styled(
            format!(
                " /{}  {}/{}  · n/N next/prev · / search · q quit",
                app.query,
                app.match_idx + 1,
                count
            ),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::styled(
            " ↑/↓ move · enter/→ expand · ← collapse · / search · g top · q quit",
            Style::default().fg(Color::DarkGray),
        )
    };
    f.render_widget(Paragraph::new(Line::from(footer)), chunks[2]);
}

/// Flatten the visible window, land any pending jump, clamp focus/scroll, and
/// draw one frame. Shared by the file and streaming loops.
fn render_frame(
    term: &mut ratatui::DefaultTerminal,
    app: &mut App,
    b: &[u8],
    h: usize,
    streaming: bool,
) -> std::io::Result<()> {
    // Normally flatten only as far as the viewport needs. When a jump is
    // pending, flatten just far enough to include the target's row.
    let budget = match &app.want_path {
        Some(p) => p.iter().sum::<usize>() + p.len() + h + 64,
        None => (app.scroll + h + 64).max(app.focus + 64),
    };
    app.rows.clear();
    let mut path = Vec::new();
    flatten(&mut app.root, b, 0, budget, &mut app.rows, &mut path);

    // Land a queued jump: find the target's row and focus it.
    if let Some(p) = app.want_path.take() {
        if let Some(idx) = app.rows.iter().position(|r| r.path == p) {
            app.focus = idx;
        }
    }

    if app.focus >= app.rows.len() {
        app.focus = app.rows.len().saturating_sub(1);
    }
    if app.focus < app.scroll {
        app.scroll = app.focus;
    }
    if app.focus >= app.scroll + h {
        app.scroll = app.focus + 1 - h;
    }

    term.draw(|f| ui(f, app, h, streaming))?;
    Ok(())
}

/// Apply one key press. Everything self-contained happens here; search
/// (re)launch is deferred to the caller via `KeyOutcome::Relaunch` because it
/// needs a byte `Source`.
fn process_key(app: &mut App, k: ratatui::crossterm::event::KeyEvent, b: &[u8], h: usize) -> KeyOutcome {
    if app.mode == Mode::Search {
        // Live search input: each edit drops the old worker and starts fresh.
        match k.code {
            KeyCode::Esc => {
                app.mode = Mode::Normal;
                app.clear_search();
            }
            KeyCode::Enter => {
                app.mode = Mode::Normal;
                if app.search.as_ref().is_some_and(|s| !s.matches.is_empty()) {
                    let first = app.search.as_ref().unwrap().matches[0].clone();
                    app.match_idx = 0;
                    app.jump_to(&first, b);
                }
            }
            KeyCode::Backspace => {
                app.query.pop();
                return KeyOutcome::Relaunch;
            }
            KeyCode::Char(c) => {
                app.query.push(c);
                return KeyOutcome::Relaunch;
            }
            _ => {}
        }
        return KeyOutcome::Continue;
    }

    // Normal mode.
    match k.code {
        KeyCode::Char('q') | KeyCode::Esc => return KeyOutcome::Quit,
        KeyCode::Char('/') => {
            app.mode = Mode::Search;
            app.query.clear();
            return KeyOutcome::Relaunch;
        }
        KeyCode::Char('n') => app.step_match(1, b),
        KeyCode::Char('N') => app.step_match(-1, b),
        KeyCode::Down | KeyCode::Char('j') => app.focus += 1,
        KeyCode::Up | KeyCode::Char('k') => app.focus = app.focus.saturating_sub(1),
        KeyCode::PageDown => app.focus += h,
        KeyCode::PageUp => app.focus = app.focus.saturating_sub(h),
        KeyCode::Home | KeyCode::Char('g') => {
            app.focus = 0;
            app.scroll = 0;
        }
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right => app.toggle_focus(),
        KeyCode::Left => app.collapse_or_parent(),
        _ => {}
    }
    KeyOutcome::Continue
}

fn term_rows() -> std::io::Result<usize> {
    let (_, th) = ratatui::crossterm::terminal::size()?;
    Ok((th.saturating_sub(2) as usize).max(1))
}

/// Poll for a key (up to `poll_ms`) and dispatch it. The `Relaunch` outcome is
/// returned to the caller, which owns the byte `Source` the search needs.
fn pump_input(app: &mut App, b: &[u8], h: usize, poll_ms: u64) -> std::io::Result<KeyOutcome> {
    if !event::poll(Duration::from_millis(poll_ms))? {
        return Ok(KeyOutcome::Continue);
    }
    let Event::Key(k) = event::read()? else {
        return Ok(KeyOutcome::Continue);
    };
    if k.kind != KeyEventKind::Press {
        return Ok(KeyOutcome::Continue);
    }
    Ok(process_key(app, k, b, h))
}

/// The file (mmap) / fully-buffered loop: one fixed byte source for the session.
fn run(
    term: &mut ratatui::DefaultTerminal,
    app: &mut App,
    b: &[u8],
    mmap: &Arc<Source>,
) -> std::io::Result<()> {
    loop {
        // Fold in any matches the worker thread has produced since last frame.
        app.pump_search();
        let h = term_rows()?;
        render_frame(term, app, b, h, false)?;
        // Short poll so streaming match counts keep ticking even without input.
        match pump_input(app, b, h, 100)? {
            KeyOutcome::Quit => return Ok(()),
            KeyOutcome::Relaunch => app.relaunch(mmap),
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
        if dirty && (done || last_build.elapsed().as_millis() >= STREAM_REBUILD_MS) {
            app.rebuild(buf, *jsonl);
            dirty = false;
            last_build = Instant::now();
        }

        app.pump_search();
        let h = term_rows()?;
        // Borrow the (possibly grown) buffer just for this frame's render + input,
        // then release it so a search relaunch can snapshot it below.
        let outcome = {
            let b: &[u8] = buf;
            render_frame(term, app, b, h, !done)?;
            pump_input(app, b, h, 100)?
        };
        match outcome {
            KeyOutcome::Quit => return Ok(()),
            KeyOutcome::Relaunch => {
                // Search over the bytes parsed so far (the stream keeps growing,
                // but one search covers what's arrived at its launch).
                let snap = Arc::new(Source::Buffered(buf.clone()));
                app.relaunch(&snap);
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

    let mut app = App::new(b, &path, jsonl);
    let mut term = ratatui::init();
    let res = run(&mut term, &mut app, b, &mmap);
    ratatui::restore();
    res
}

/// Stream piped stdin: the JSON renders progressively as it arrives (a pipe
/// can't be mmap'd, so it's buffered in RAM and re-parsed on a throttle).
fn run_stdin() -> std::io::Result<()> {
    let rx = spawn_reader(take_pipe_reader());
    let mut buf: Vec<u8> = Vec::new();
    let mut jsonl = false;
    let mut app = App::new(&buf, "stdin", jsonl);
    let mut term = ratatui::init();
    let res = run_stream(&mut term, &mut app, &mut buf, &mut jsonl, rx);
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
