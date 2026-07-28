//! The `T` table view: a sequence read as rows × columns instead of an outline.
//!
//! Columns come from the same inference that backs the `t` overlay
//! ([`schema::columns`]), and rows are the container's own lazily-enumerated
//! children — so tabulating a 1 GB array costs what scrolling it costs. Only the
//! visible window is enumerated, and a cell is a byte range resolved on the fly
//! (a short `Cursor` walk inside one row), never a materialized value.
//!
//! The table owns no data: it is a cursor (`row`, `col`), a scroll pair, and the
//! window of cells last painted. The rows themselves stay in the pane's [`Node`]
//! tree, which is why stepping out of the table lands on the row you were
//! reading.

use crate::scanner::{Cursor, Kind};
use crate::schema::Column;
use crate::tree::{brief_value, Node, BRIEF_WIDTH};

/// Narrowest a column may be drawn — enough for `…` plus a character or two.
const MIN_COL_W: usize = 3;
/// Blank cells between columns.
pub const COL_GAP: usize = 2;
/// Object keys scanned looking for one cell's field before giving up. A row with
/// more keys than this is pathological; the cell reads as missing.
const CELL_KEY_CAP: usize = 512;
/// Rows enumerated past the bottom of the window. Without slack the cursor could
/// only ever step one row past what's been scanned, so a page-down would crawl a
/// row at a time; the tree's flatten keeps the same 64 rows of headroom.
pub const ROW_LOOKAHEAD: usize = 64;
/// Widest the row-label gutter (index or map key) may grow.
const MAX_LABEL_W: usize = 16;

/// One cell of the visible window.
pub struct Cell {
    /// The value, briefly (`"alice"`, `42`, `{…}`) — empty when the row has no
    /// such field, which is how a sparse column reads as a gap.
    pub text: String,
    /// The value's actual kind, for coloring. `None` when the field is missing.
    pub kind: Option<Kind>,
}

/// One visible row: the child's label (array index or map key) plus its cells,
/// one per column, in column order.
pub struct Row {
    pub label: String,
    pub cells: Vec<Cell>,
}

/// A pane's table view. Attached to the pane by `T` and dropped when it's
/// pressed again; the pane's tree keeps running underneath.
pub struct Table {
    /// Index path to the tabulated container in the pane's tree.
    pub root: Vec<usize>,
    /// The container's label, for the pane title.
    pub title: String,
    pub cols: Vec<Column>,
    /// How many columns the inferred type had before sparse ones were dropped —
    /// so the title can admit to hiding some.
    pub total_cols: usize,
    /// Cursor: which row (child index) and column are selected.
    pub row: usize,
    pub col: usize,
    /// Top visible row, and the leftmost visible column.
    pub scroll: usize,
    pub hscroll: usize,
    /// The rows currently on screen, rebuilt every frame by [`Table::refresh`].
    pub window: Vec<Row>,
    /// Children enumerated so far, and whether that's all of them. Like the tree,
    /// a huge level is only walked as far as it's scrolled.
    pub known: usize,
    pub done: bool,
    /// Drawn width of each column, and of the row-label gutter.
    pub widths: Vec<usize>,
    pub label_w: usize,
}

impl Table {
    pub fn new(root: Vec<usize>, title: String, cols: Vec<Column>, total_cols: usize) -> Table {
        let widths = vec![MIN_COL_W; cols.len()];
        Table {
            root,
            title,
            cols,
            total_cols,
            row: 0,
            col: 0,
            scroll: 0,
            hscroll: 0,
            window: Vec::new(),
            known: 0,
            done: false,
            widths,
            label_w: 1,
        }
    }

    /// Enumerate the visible rows, resolve their cells, and re-measure the
    /// columns. `vis` is how many data rows fit (the header is already
    /// subtracted); `width` is the pane's, for the horizontal scroll.
    pub fn refresh(&mut self, node: &mut Node, b: &[u8], vis: usize, width: usize) {
        let vis = vis.max(1);
        if self.cols.is_empty() {
            return;
        }
        self.col = self.col.min(self.cols.len() - 1);
        // Follow the cursor vertically before enumerating, so a jump only scans
        // as far as the rows it actually lands on.
        if self.row < self.scroll {
            self.scroll = self.row;
        } else if self.row >= self.scroll + vis {
            self.scroll = self.row + 1 - vis;
        }
        // One eager scan for the window plus its lookahead. Unlike the tree's
        // cooperative flatten this can stall a frame on a single enormous row, but
        // a table row is by nature a record, not a gigabyte.
        node.ensure_child(b, self.scroll + vis + ROW_LOOKAHEAD);
        self.known = node.children.len();
        self.done = node.done;
        if self.known == 0 {
            // An empty container still draws its header, so the columns still
            // need measuring — off the labels alone.
            self.window.clear();
            self.row = 0;
            self.scroll = 0;
            self.measure();
            return;
        }
        self.row = self.row.min(self.known - 1);
        self.scroll = self.scroll.min(self.row).min(self.known.saturating_sub(1));
        if self.row >= self.scroll + vis {
            self.scroll = self.row + 1 - vis;
        }

        let end = (self.scroll + vis).min(self.known);
        self.window.clear();
        for child in &node.children[self.scroll..end] {
            let cells = self
                .cols
                .iter()
                .map(
                    |c| match resolve(b, child.start, child.end, child.kind, &c.path) {
                        Some((s, e, k)) => Cell {
                            text: brief_value(b, s, e, k),
                            kind: Some(k),
                        },
                        None => Cell {
                            text: String::new(),
                            kind: None,
                        },
                    },
                )
                .collect();
            self.window.push(Row {
                label: child.label.clone(),
                cells,
            });
        }
        self.measure();
        self.follow_col(width);
    }

    /// Size each column to the widest thing in it — header or visible cell —
    /// within the cell-preview cap. Widths are per-window, so the grid settles as
    /// you scroll rather than reserving room for values you can't see.
    fn measure(&mut self) {
        self.label_w = self
            .window
            .iter()
            .map(|r| r.label.chars().count())
            .max()
            .unwrap_or(1)
            .clamp(1, MAX_LABEL_W);
        self.widths = self
            .cols
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let cells = self
                    .window
                    .iter()
                    .filter_map(|r| r.cells.get(i))
                    .map(|c| c.text.chars().count())
                    .max()
                    .unwrap_or(0);
                cells
                    .max(c.label.chars().count())
                    .clamp(MIN_COL_W, BRIEF_WIDTH.max(MIN_COL_W))
            })
            .collect();
    }

    /// Scroll horizontally just enough to keep the cursor column on screen.
    fn follow_col(&mut self, width: usize) {
        if self.col < self.hscroll {
            self.hscroll = self.col;
            return;
        }
        while self.hscroll < self.col && !self.layout(width).iter().any(|&(i, _, _)| i == self.col)
        {
            self.hscroll += 1;
        }
    }

    /// The columns that fit in a pane `width` starting at `hscroll`, as
    /// `(column index, x offset, drawn width)`. The row-label gutter is always
    /// drawn first, so it acts as a pinned key column. At least one column is
    /// always returned, even in a pane too narrow for it.
    pub fn layout(&self, width: usize) -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        let mut x = self.label_w + COL_GAP;
        for i in self.hscroll..self.cols.len() {
            let w = self.widths.get(i).copied().unwrap_or(MIN_COL_W);
            if x + w > width && !out.is_empty() {
                break;
            }
            out.push((i, x, w));
            x += w + COL_GAP;
        }
        out
    }

    /// The window row under the cursor, if it's on screen.
    pub fn cursor_row(&self) -> Option<&Row> {
        self.window.get(self.row.checked_sub(self.scroll)?)
    }

    /// The byte range of the cell under the cursor, resolved against the tree —
    /// what Enter peeks and `y` copies. `None` when the row lacks the field.
    pub fn cursor_range(&self, node: &Node, b: &[u8]) -> Option<(usize, usize, Kind)> {
        let child = node.children.get(self.row)?;
        let col = self.cols.get(self.col)?;
        resolve(b, child.start, child.end, child.kind, &col.path)
    }

    /// The label of the cursor's column, for a peek/copy status line.
    pub fn cursor_label(&self) -> String {
        match (self.cursor_row(), self.cols.get(self.col)) {
            (Some(r), Some(c)) => format!("{}.{}", r.label, c.label),
            (_, Some(c)) => c.label.clone(),
            _ => self.title.clone(),
        }
    }
}

/// Walk `path` inside one row's bytes, returning the field's range and kind.
/// Lenient like the filter language: a row missing the field simply has no cell.
pub fn resolve(
    b: &[u8],
    start: usize,
    end: usize,
    kind: Kind,
    path: &[String],
) -> Option<(usize, usize, Kind)> {
    let (mut s, mut e, mut k) = (start, end, kind);
    for seg in path {
        if !matches!(k, Kind::Object) {
            return None;
        }
        let mut c = Cursor::new(s, e, false);
        let mut n = 0;
        let mut hit = None;
        while let Some(f) = c.next(b) {
            if &f.label == seg {
                hit = Some((f.start, f.end, f.kind));
                break;
            }
            n += 1;
            if n >= CELL_KEY_CAP {
                break;
            }
        }
        let (hs, he, hk) = hit?;
        s = hs;
        e = he;
        k = hk;
    }
    Some((s, e, k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::value_kind;
    use crate::schema;
    use crate::tree::make_root;

    /// Build a table over the whole document, refreshed once at `vis` rows.
    fn table_of(json: &str, vis: usize, width: usize) -> (Table, Node) {
        let b = json.as_bytes();
        let ty = schema::infer(b, 0, b.len(), value_kind(b, 0), false);
        let (cols, total) = schema::columns(&ty).expect("tabular");
        let mut root = make_root(b, "ROOT", false);
        root.toggle();
        let mut t = Table::new(Vec::new(), "ROOT".into(), cols, total);
        t.refresh(&mut root, b, vis, width);
        (t, root)
    }

    fn texts(t: &Table, row: usize) -> Vec<&str> {
        t.window[row]
            .cells
            .iter()
            .map(|c| c.text.as_str())
            .collect()
    }

    #[test]
    fn rows_are_elements_and_columns_are_fields() {
        let (t, _) = table_of(r#"[{"id":1,"name":"ann"},{"id":2,"name":"bo"}]"#, 10, 80);
        let labels: Vec<&str> = t.cols.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["id", "name"]);
        assert_eq!(t.window.len(), 2);
        assert_eq!(t.window[0].label, "0");
        assert_eq!(texts(&t, 0), ["1", "\"ann\""]);
        assert_eq!(texts(&t, 1), ["2", "\"bo\""]);
    }

    #[test]
    fn a_missing_field_leaves_an_empty_cell() {
        let (t, _) = table_of(r#"[{"a":1,"b":2},{"a":3}]"#, 10, 80);
        assert_eq!(texts(&t, 1), ["3", ""]);
        assert!(t.window[1].cells[1].kind.is_none());
    }

    #[test]
    fn nested_records_flatten_into_dotted_columns() {
        let (t, _) = table_of(
            r#"[{"u":{"city":"NY","zip":1}},{"u":{"city":"LA","zip":2}}]"#,
            10,
            80,
        );
        let labels: Vec<&str> = t.cols.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["u.city", "u.zip"]);
        assert_eq!(texts(&t, 0), ["\"NY\"", "1"]);
    }

    #[test]
    fn a_column_deeper_than_the_flatten_limit_shows_as_a_brief_value() {
        let (t, _) = table_of(r#"[{"a":{"b":{"c":1}}},{"a":{"b":{"c":2}}}]"#, 10, 80);
        let labels: Vec<&str> = t.cols.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["a.b"]);
        assert_eq!(texts(&t, 0), ["{…}"]);
    }

    #[test]
    fn a_map_tabulates_with_its_keys_as_row_labels() {
        let (t, _) = table_of(
            r#"{"AAA":{"v":1},"BBB":{"v":2},"CCC":{"v":3},"DDD":{"v":4}}"#,
            10,
            80,
        );
        assert_eq!(t.cols.len(), 1);
        assert_eq!(t.window[0].label, "AAA");
        assert_eq!(texts(&t, 3), ["4"]);
    }

    #[test]
    fn a_sequence_of_scalars_gets_one_value_column() {
        let (t, _) = table_of(r#"[1,2,3]"#, 10, 80);
        assert_eq!(t.cols[0].label, "value");
        assert_eq!(texts(&t, 2), ["3"]);
    }

    #[test]
    fn only_the_visible_window_is_enumerated() {
        let json: String = format!(
            "[{}]",
            (0..5000)
                .map(|i| format!(r#"{{"i":{i}}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let (t, _) = table_of(&json, 10, 80);
        assert_eq!(t.window.len(), 10);
        assert!(
            t.known <= 10 + ROW_LOOKAHEAD + 2,
            "enumerated {} of 5000 rows for a 10-row window",
            t.known
        );
        assert!(!t.done);
    }

    #[test]
    fn paging_moves_a_whole_page_at_a_time() {
        // The cursor may only land on an enumerated row, so the lookahead is what
        // keeps a page-down from crawling one row per press.
        let json: String = format!(
            "[{}]",
            (0..5000)
                .map(|i| format!(r#"{{"i":{i}}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let b = json.as_bytes();
        let (mut t, mut root) = table_of(&json, 30, 80);
        for page in 1..=5 {
            t.row = (t.row + 30).min(t.known - 1);
            t.refresh(&mut root, b, 30, 80);
            assert_eq!(t.row, page * 30, "page {page} fell short");
        }
    }

    #[test]
    fn scrolling_follows_the_cursor_and_keeps_the_window_full() {
        let json: String = format!(
            "[{}]",
            (0..100)
                .map(|i| format!(r#"{{"i":{i}}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let b = json.as_bytes();
        let (mut t, mut root) = table_of(&json, 10, 80);
        t.row = 42;
        t.refresh(&mut root, b, 10, 80);
        assert_eq!(t.scroll, 33);
        assert_eq!(t.window.len(), 10);
        assert_eq!(t.window[9].label, "42");
    }

    #[test]
    fn the_layout_pins_the_label_gutter_and_pages_columns_sideways() {
        let (mut t, _) = table_of(
            r#"[{"aaaaaaaa":"xxxxxxxxxx","bbbbbbbb":"yyyyyyyyyy","cccccccc":"zzzzzzzzzz"}]"#,
            10,
            30,
        );
        let first = t.layout(30);
        assert_eq!(first[0].0, 0);
        assert!(first.len() < 3, "a 30-col pane can't fit every column");
        // Every visible column starts past the pinned gutter.
        assert!(first.iter().all(|&(_, x, _)| x >= t.label_w + COL_GAP));
        t.hscroll = 2;
        assert_eq!(t.layout(30)[0].0, 2);
    }

    #[test]
    fn a_narrow_pane_still_shows_one_column() {
        let (t, _) = table_of(r#"[{"aaaaaaaaaaaaaaaaaaaa":1}]"#, 10, 4);
        assert_eq!(t.layout(4).len(), 1);
    }
}
