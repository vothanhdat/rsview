//! rsview — proof-of-concept lazy JSON viewer in Rust.
//!
//! Demonstrates the core of react-obj-view's CLI in native Rust: a file is
//! memory-mapped, parsed on expand (subtrees are byte ranges, not materialized
//! values), and a level is flattened only as far as the viewport scrolls
//! (windowing). Opening a multi-GB file stays near-constant memory.

mod scanner;
use scanner::{container_empty, decode_str, skip_ws, value_kind, Cursor, Kind, RawChild};

use memmap2::Mmap;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use std::{fs::File, time::Duration};

/// A lazily-expanding tree node. Children are scanned on demand from `cursor`
/// (resumable), so a collapsed node costs O(1) and a huge level only enumerates
/// as far as it's scrolled.
struct Node {
    label: String,
    start: usize,
    end: usize,
    kind: Kind,
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

    fn from_raw(rc: RawChild, b: &[u8]) -> Node {
        let is_cont = matches!(rc.kind, Kind::Object | Kind::Array);
        let has = is_cont && !container_empty(b, rc.start, rc.end);
        Node {
            label: rc.label,
            start: rc.start,
            end: rc.end,
            kind: rc.kind,
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
            self.cursor = Some(Cursor::new(self.start, self.end, matches!(self.kind, Kind::Array)));
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

    fn preview(&self, b: &[u8]) -> String {
        match self.kind {
            Kind::Object => {
                if !self.has_children {
                    "{}".into()
                } else if self.expanded {
                    "{".into()
                } else {
                    "{…}".into()
                }
            }
            Kind::Array => {
                if !self.has_children {
                    "[]".into()
                } else if self.expanded {
                    "[".into()
                } else {
                    "[…]".into()
                }
            }
            Kind::Str => format!("\"{}\"", truncate(&decode_str(b, self.start, self.end), 70)),
            _ => truncate(&String::from_utf8_lossy(&b[self.start..self.end]), 70),
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

/// A flattened, on-screen row plus the index path back to its node.
struct Row {
    depth: usize,
    text: String,
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
        text: format!("{}: {}", node.label, node.preview(b)),
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

struct App {
    root: Node,
    name: String,
    focus: usize,
    scroll: usize,
    rows: Vec<Row>,
}

impl App {
    fn new(b: &[u8], path: &str) -> App {
        let rstart = skip_ws(b, 0, b.len());
        let kind = value_kind(b, rstart);
        let is_cont = matches!(kind, Kind::Object | Kind::Array);
        let has = is_cont && !container_empty(b, rstart, b.len());
        let name = std::path::Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "ROOT".into());
        let mut root = Node {
            label: name.clone(),
            start: rstart,
            end: b.len(), // root spans to EOF; the scanner stops at the real closer
            kind,
            has_children: has,
            expanded: false,
            done: false,
            children: Vec::new(),
            cursor: None,
        };
        if has {
            root.toggle(); // auto-expand the root (like --depth 1)
        }
        App {
            root,
            name,
            focus: 0,
            scroll: 0,
            rows: Vec::new(),
        }
    }

    fn toggle_focus(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let path = self.rows[self.focus].path.clone();
        get_mut(&mut self.root, &path).toggle();
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

fn ui(f: &mut Frame, app: &App, h: usize) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(f.area());

    let title = format!(" {}   {}/{}+", app.name, app.focus + 1, app.rows.len());
    f.render_widget(
        Paragraph::new(title).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        chunks[0],
    );

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
        let text = format!("{}{} {}", "  ".repeat(r.depth), marker, r.text);
        let mut st = Style::default();
        if i == app.focus {
            st = st.add_modifier(Modifier::REVERSED);
        }
        lines.push(Line::from(Span::styled(text, st)));
    }
    f.render_widget(Paragraph::new(lines), chunks[1]);

    f.render_widget(
        Paragraph::new(" ↑/↓ move · enter/→ expand · ← collapse · g top · q quit")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn run(term: &mut ratatui::DefaultTerminal, app: &mut App, b: &[u8]) -> std::io::Result<()> {
    loop {
        let (_, th) = ratatui::crossterm::terminal::size()?;
        let h = (th.saturating_sub(2) as usize).max(1);
        // Flatten only as far as the viewport (plus a small lookahead) needs.
        let budget = (app.scroll + h + 64).max(app.focus + 64);
        app.rows.clear();
        let mut path = Vec::new();
        flatten(&mut app.root, b, 0, budget, &mut app.rows, &mut path);

        if app.focus >= app.rows.len() {
            app.focus = app.rows.len().saturating_sub(1);
        }
        if app.focus < app.scroll {
            app.scroll = app.focus;
        }
        if app.focus >= app.scroll + h {
            app.scroll = app.focus + 1 - h;
        }

        term.draw(|f| ui(f, app, h))?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
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
                }
            }
        }
    }
}

fn main() -> std::io::Result<()> {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: rsview <file.json>");
            std::process::exit(2);
        }
    };
    let file = File::open(&path)?;
    // SAFETY: the file isn't mutated while mapped for the lifetime of the viewer.
    let mmap = unsafe { Mmap::map(&file)? };
    let b: &[u8] = &mmap;

    let mut app = App::new(b, &path);
    let mut term = ratatui::init();
    let res = run(&mut term, &mut app, b);
    ratatui::restore();
    res
}
