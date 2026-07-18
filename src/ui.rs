//! All terminal rendering: the pane tree, footer, prompts, and the floating
//! overlays (bookmarks, help, peek, schema). These functions only *read* the
//! [`App`]/[`View`] state and draw it to a ratatui [`Frame`]; the frame
//! orchestration that mutates state (flatten, jump, draw) stays in the run loop
//! and calls [`draw`].

use crate::input::TextInput;
use crate::scanner::Kind;
use crate::tree::{breadcrumb_segments, join_path};
use crate::{App, Mode, View};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

// Syntax-highlight palette (ANSI named colors so it adapts to the terminal theme).
const C_KEY: Color = Color::Cyan; // object keys
const C_INDEX: Color = Color::DarkGray; // array indices
const C_STR: Color = Color::Green; // string values
const C_NUM: Color = Color::Yellow; // numbers
const C_BOOL: Color = Color::Magenta; // true / false
const C_PUNCT: Color = Color::DarkGray; // braces, colon, markers, previews
const C_BOOKMARK: Color = Color::LightYellow; // the `★` gutter marker on bookmarked rows

/// The foreground color for a value of the given kind.
pub(crate) fn value_color(kind: Kind) -> Color {
    match kind {
        Kind::Str => C_STR,
        Kind::Number => C_NUM,
        Kind::Bool => C_BOOL,
        Kind::Null | Kind::Object | Kind::Array => C_PUNCT,
    }
}

/// Render the focus breadcrumb (`users › [2] › city`) as styled spans that fit
/// within `avail` columns, **left-truncating** with a leading `…` so the tail
/// nearest the focused node always stays visible. Array elements are bracketed;
/// the last (current) segment is bold.
pub(crate) fn breadcrumb_spans(segs: &[(String, bool)], avail: usize) -> Vec<Span<'static>> {
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
pub(crate) fn pane_layout(area: Rect, weights: &[u16], stacked: bool) -> std::rc::Rc<[Rect]> {
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
pub(crate) fn render_separator(f: &mut Frame, sep: Rect, stacked: bool) {
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
pub(crate) fn panes_area(area: Rect) -> Rect {
    Rect {
        height: area.height.saturating_sub(1),
        ..area
    }
}

/// Draw every pane (side by side or stacked) above a single global footer that
/// spans the full width. `streaming` only affects the (non-derived) document
/// pane's title.
pub(crate) fn draw(f: &mut Frame, app: &App, streaming: bool) {
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
        Mode::Peek => render_peek(f, area, app.active_view()),
        Mode::Schema => render_schema(f, area, app.active_view()),
        _ => {}
    }
}

/// The single global key/search-status bar, reflecting the active pane. A
/// transient `flash` (e.g. a copy confirmation) takes the bar over the key hint.
pub(crate) fn render_footer(f: &mut Frame, area: Rect, view: &View, flash: Option<&str>) {
    let line = if view.mode == Mode::Search {
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
        // Scope hint: show where a scoped search is aimed, or that Tab can scope.
        let scope = match (&view.search_scope, view.scoped) {
            (Some(s), true) => format!(" · in {} · ⇥ all", s.label),
            (Some(_), false) => " · ⇥ scope".to_string(),
            (None, _) => String::new(),
        };
        prompt_line(
            '/',
            &view.query,
            &format!("{pos}{scope} · ↵/↓ next · esc close"),
        )
    } else if view.mode == Mode::Goto {
        prompt_line(':', &view.goto, "↵ jump · esc cancel")
    } else if view.mode == Mode::Filter {
        let hint = match view.filter_error.as_deref() {
            Some(e) => format!("({e})"),
            None => "↵ run → new pane · esc cancel".to_string(),
        };
        prompt_line('|', &view.filter_query, &hint)
    } else if let Some(msg) = flash {
        Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().fg(Color::Green),
        ))
    } else {
        Line::from(Span::styled(
            " ↑/↓ move · enter expand · / search · : goto · | filter · y copy · ? help · q quit",
            Style::default().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

/// Build a prompt footer line with a visible block caret at the edit position.
/// The character under the caret is drawn reverse-video (a blank cell when the
/// caret sits at the end of the line), so the `/`, `:`, and `|` prompts read
/// like real input fields — the split at `input.caret` is always on a char
/// boundary, so it never slices a multi-byte character.
pub(crate) fn prompt_line(prefix: char, input: &TextInput, hint: &str) -> Line<'static> {
    let base = Style::default().fg(Color::Yellow);
    let (before, rest) = input.as_str().split_at(input.caret);
    let under = rest.chars().next();
    let after = &rest[under.map_or(0, char::len_utf8)..];
    Line::from(vec![
        Span::styled(format!(" {prefix}{before}"), base),
        Span::styled(
            under.unwrap_or(' ').to_string(),
            base.add_modifier(Modifier::REVERSED),
        ),
        Span::styled(format!("{after}   {hint}"), base),
    ])
}

/// Draw the bookmark picker as a centered overlay listing each saved node's
/// path; the selected row is highlighted. Jumped/edited via [`process_key`].
pub(crate) fn render_marks(f: &mut Frame, area: Rect, view: &View) {
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
pub(crate) fn render_help(f: &mut Frame, area: Rect) {
    const ENTRIES: &[(&str, &str)] = &[
        ("↑/↓  k/j", "move focus"),
        ("J / K", "next / prev sibling"),
        ("PgUp/PgDn", "page up / down"),
        ("Ctrl-D/U", "half page"),
        ("g  Home", "jump to top"),
        ("Enter  →", "expand · peek a leaf"),
        ("←", "collapse / parent"),
        ("wheel", "scroll the pane"),
        ("t", "infer type (TS) · y copy"),
        ("c", "count children"),
        ("#", "aggregate numbers (Σ min/max/avg)"),
        ("/", "search · ⇥ scope subtree"),
        ("↵  ⇧↵", "next / prev match"),
        (":", "jump to a path · *? in keys"),
        ("|", "jq-style filter → new pane"),
        ("m", "toggle bookmark"),
        ("'", "bookmark picker"),
        ("y / Y", "copy value / path"),
        ("p", "pipe node · all hits on a filter pane"),
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

    // Worked examples for the two syntaxes that don't fit a one-line key hint:
    // the `/` search prefixes and the `|` jq-style filter. Shown full-width
    // below the key grid, `prefix code — description`.
    const EXAMPLES: &[(&str, &str, &str)] = &[
        ("/", "re:^id_\\d+$", "regex search"),
        ("/", "g:log-*", "glob search (*, ? wildcards)"),
        (
            "|",
            ".users[] | select(.age > 30) | .name",
            "names of users over 30",
        ),
        ("|", ".. | .id", "every .id at any depth"),
        (
            "|",
            "select(.name ~ \"re:^a\")",
            "field value matches a regex",
        ),
        (
            "|",
            ".items[] | select(.f ~ \"g:*.log\")",
            "field value matches a glob",
        ),
    ];
    let ex_code_w = EXAMPLES
        .iter()
        .map(|(_, c, _)| c.chars().count())
        .max()
        .unwrap_or(0);

    let key_st = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let desc_st = Style::default().fg(Color::Gray);
    let code_st = Style::default().fg(Color::Yellow);
    let panel_bg = Color::Indexed(236);

    // One "key  description" cell, key right-aligned so the columns line up.
    let cell = |k: &str, d: &str| -> Vec<Span<'static>> {
        vec![
            Span::styled(format!(" {k:>key_w$}  "), key_st),
            Span::styled(format!("{d:<desc_w$} "), desc_st),
        ]
    };

    // Pair entry i with entry i+rows, so the list reads top-to-bottom per column.
    let mut lines: Vec<Line> = (0..rows)
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

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " examples · / search · | filter",
        key_st,
    )));
    for &(p, c, d) in EXAMPLES {
        lines.push(Line::from(vec![
            Span::styled(format!(" {p} "), key_st),
            Span::styled(format!("{c:<ex_code_w$}"), code_st),
            Span::styled(format!("  {d}"), desc_st),
        ]));
    }

    let title = " keyboard shortcuts · any key to close ";
    let col_w = key_w + desc_w + 4; // " " + key + "  " + desc + " "
    let ex_w = 3
        + ex_code_w
        + 2
        + EXAMPLES
            .iter()
            .map(|(_, _, d)| d.chars().count())
            .max()
            .unwrap_or(0);
    let inner_w = (col_w * 2 + 2)
        .max(ex_w)
        .max(title.chars().count())
        .min(area.width.saturating_sub(2) as usize);
    let w = inner_w as u16 + 2;
    let h = (lines.len() as u16 + 2).min(area.height);
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

/// The centered peek card and its inner (text) dimensions `(rect, inner_w,
/// inner_h)`. A near-full-screen box so a long value has room to breathe. Shared
/// by [`render_peek`] and the peek scroll handler so the scroll clamp matches what
/// is actually drawn.
pub(crate) fn peek_layout(area: Rect) -> (Rect, usize, usize) {
    let w = area.width.saturating_sub(6).clamp(8, area.width.max(8));
    let h = area.height.saturating_sub(4).clamp(3, area.height.max(3));
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };
    (
        rect,
        w.saturating_sub(2) as usize,
        h.saturating_sub(2) as usize,
    )
}

/// Lay a decoded value out for the peek overlay: hard-break on `\n`, expand tabs,
/// drop other control chars, and char-wrap each logical line to `width` columns.
/// Char counts (not display columns) mirror the width accounting the rest of the
/// UI uses (`truncate`, breadcrumbs), so wide glyphs are treated the same here.
pub(crate) fn wrap_for_peek(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    for logical in text.split('\n') {
        // Sanitize: tabs to spaces, strip other control chars (a raw \r or NUL
        // would garble the terminal).
        let mut clean = String::with_capacity(logical.len());
        for c in logical.chars() {
            match c {
                '\t' => clean.push_str("    "),
                c if c.is_control() => {}
                c => clean.push(c),
            }
        }
        if clean.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut n = 0;
        for c in clean.chars() {
            cur.push(c);
            n += 1;
            if n == width {
                out.push(std::mem::take(&mut cur));
                n = 0;
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Draw the value-peek overlay: the focused scalar's full value, wrapped and
/// vertically scrolled to `peek.scroll`, in the same floating-card style as the
/// other overlays. Opened by `Enter`/`Space` on a leaf; closed by `esc`/`q`.
pub(crate) fn render_peek(f: &mut Frame, area: Rect, view: &View) {
    let Some(pk) = view.peek.as_ref() else {
        return;
    };
    let (rect, inner_w, inner_h) = peek_layout(area);
    let all = wrap_for_peek(&pk.text, inner_w);
    let total = all.len();
    let top = pk.scroll.min(total.saturating_sub(inner_h));
    let bottom = (top + inner_h).min(total);

    let panel_bg = Color::Indexed(236);
    let lines: Vec<Line> = all[top..bottom]
        .iter()
        .map(|s| Line::from(Span::styled(s.clone(), Style::default().fg(Color::Gray))))
        .collect();

    // Title carries the label and a position readout so a long value shows how far
    // down you are; the `⚠ capped` flag marks a value clipped at PEEK_MAX_BYTES.
    let cap = if pk.truncated { " · ⚠ capped" } else { "" };
    let title = format!(
        " peek · {} · lines {}–{}/{}{cap} · j/k scroll · esc ",
        pk.title,
        top + 1,
        bottom,
        total,
    );
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

/// Syntax-highlight one line of a rendered type: type keywords green, field
/// names cyan, punctuation dim, and a trailing `// %` comment dimmer. Word runs
/// are classified by a fixed keyword set, so a field name colours as a key while
/// `string`/`number`/`Record`/… colour as types.
pub(crate) fn highlight_type_line(s: &str) -> Line<'static> {
    const KEYWORDS: &[&str] = &["string", "number", "boolean", "null", "any", "Record"];
    let (code, comment) = match s.split_once("//") {
        Some((c, n)) => (c, Some(format!("//{n}"))),
        None => (s, None),
    };
    let mut spans: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_word = false;
    let flush = |run: &mut String, is_word: bool, spans: &mut Vec<Span>| {
        if run.is_empty() {
            return;
        }
        let color = if !is_word {
            C_PUNCT
        } else if KEYWORDS.contains(&run.as_str()) {
            C_STR // type keywords reuse the string-value green
        } else {
            C_KEY // field names
        };
        spans.push(Span::styled(
            std::mem::take(run),
            Style::default().fg(color),
        ));
    };
    for ch in code.chars() {
        let is_word = ch.is_ascii_alphanumeric() || ch == '_';
        if !run.is_empty() && is_word != run_word {
            flush(&mut run, run_word, &mut spans);
        }
        run_word = is_word;
        run.push(ch);
    }
    flush(&mut run, run_word, &mut spans);
    if let Some(c) = comment {
        spans.push(Span::styled(c, Style::default().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

/// Draw the schema overlay: the focused node's inferred type (TypeScript-style),
/// scrolled to `schema.scroll`, in the same floating-card style as the other
/// overlays. Opened by `t`; `y` copies it; closed by `esc`/`q`/`t`.
pub(crate) fn render_schema(f: &mut Frame, area: Rect, view: &View) {
    let Some(sc) = view.schema.as_ref() else {
        return;
    };
    // The peek card is near-full-screen, right for a long scalar but oversized for
    // a schema — usually a few short type lines. Use the peek box only as an upper
    // bound and shrink the card to fit its content, so a small type gets a small
    // card and only a large one grows toward full-screen.
    let (max_rect, max_inner_w, max_inner_h) = peek_layout(area);
    let panel_bg = Color::Indexed(236);

    let total = sc.lines.len();
    let content_w = sc
        .lines
        .iter()
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(0);
    // Floor the width so the title chrome (" type · … · esc ") mostly fits, cap it
    // at the peek bound. Height fits every line up to the same bound.
    let title_w = sc.title.chars().count() + 38;
    let inner_w = content_w.max(title_w.min(62)).clamp(1, max_inner_w);
    let inner_h = total.clamp(1, max_inner_h);
    let w = (inner_w as u16 + 2).min(max_rect.width);
    let h = (inner_h as u16 + 2).min(max_rect.height);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };
    let inner_h = h.saturating_sub(2) as usize;

    let top = sc.scroll.min(total.saturating_sub(inner_h));
    let bottom = (top + inner_h).min(total);
    let lines: Vec<Line> = sc.lines[top..bottom]
        .iter()
        .map(|s| highlight_type_line(s))
        .collect();

    let title = format!(
        " type · {} · lines {}–{}/{} · y copy · esc ",
        sc.title,
        top + 1,
        bottom,
        total,
    );
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

pub(crate) fn spinner_frame() -> &'static str {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    SPINNER[(ms / 80) as usize % SPINNER.len()]
}

/// Draw one pane (title + breadcrumb, then content rows) into `rect`. The active
/// pane gets a bright title and is the only one to show its cursor bar; the
/// key/search footer is global (see [`render_footer`]), not per-pane.
pub(crate) fn render_pane(f: &mut Frame, rect: Rect, view: &View, active: bool, streaming: bool) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(rect);

    // Title: `↳ origin   focus/rows+` plus the focus breadcrumb. Count only real
    // rows (the trailing loading placeholder isn't a node), and clamp the shown
    // position to that count.
    let marker = if view.derived { "↳ " } else { "" };
    let n_rows = view.rows.iter().filter(|r| !r.loading).count();
    let pos = (view.focus + 1).min(n_rows.max(1));
    let mut prefix = format!(" {marker}{}   {}/{}+", view.name, pos, n_rows);
    if let Some(f) = view.filter.as_ref() {
        // A filter result pane: show how many nodes matched, with a `+` while the
        // worker is still scanning.
        let more = if f.finished { "" } else { "+" };
        prefix.push_str(&format!("   {} hits{more}", f.hits.len()));
    }
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
            // The leading space keeps it aligned with the bookmark gutter below.
            let indent = "  ".repeat(r.depth);
            lines.push(Line::from(Span::styled(
                format!(" {indent}{} loading…", spinner_frame()),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            )));
            continue;
        }
        // A one-column gutter carries the `★` bookmark marker (a space when the
        // row isn't bookmarked) so bookmarked nodes are visible at a glance and
        // every row stays aligned whether or not it's marked.
        let bookmarked = view.bookmarks.contains(&r.path);
        let gutter = if bookmarked { "★" } else { " " };
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
            let text = format!("{gutter}{indent}{marker} {}: {}", r.label, r.value);
            Line::from(Span::styled(
                text,
                Style::default().add_modifier(Modifier::REVERSED),
            ))
        } else if cur_match == Some(&r.path) || view.match_set.contains(&r.path) {
            // A search hit: whole row yellow (current match also bold).
            let text = format!("{gutter}{indent}{marker} {}: {}", r.label, r.value);
            let mut st = Style::default().fg(Color::Yellow);
            if cur_match == Some(&r.path) {
                st = st.add_modifier(Modifier::BOLD);
            }
            Line::from(Span::styled(text, st))
        } else {
            // Normal row: syntax-colored segments.
            let key_color = if r.is_index { C_INDEX } else { C_KEY };
            Line::from(vec![
                Span::styled(gutter, Style::default().fg(C_BOOKMARK)),
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
