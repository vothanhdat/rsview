//! jq-style **selection** filter — a background-thread evaluator that walks the
//! byte-range tree and streams the byte ranges of the nodes a pipeline selects.
//!
//! This is deliberately a *selection* subset of jq, not a transform language: it
//! only ever picks out sub-values that already exist in the document, so every
//! result is a `[start, end)` slice of the mmap — never a materialized value.
//! That keeps the constant-memory promise (a filter over a 1 GB array collects
//! byte offsets, not copies) and lets the result be displayed by the same lazy
//! `Node` tree as everything else.
//!
//! Supported grammar (a pipeline of `|`-separated stages):
//!   - `.`                 identity (the current value)
//!   - `.foo` / `.foo.bar` object field access
//!   - `["key"]`           bracketed field access (keys with dots/spaces)
//!   - `.[3]` / `[3]`      array index
//!   - `.[]` / `[]`        iterate an array's elements or an object's values
//!   - `select(<cond>)`    keep the value when `<cond>` holds. `<cond>` is either
//!     `<path>` (kept when truthy) or `<path> <op> <literal>`, where `op` is one
//!     of `== != < <= > >=` and the literal is a number / "string" / true / false
//!     / null.
//!
//! Everything is lenient (jq's `?`): a path that doesn't exist on a value simply
//! produces no output rather than erroring, so `.a.b` over a mixed array yields
//! results only for the elements that have `a.b`.

use crate::scanner::{decode_str, Cursor, Kind};
use crate::source::Source;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc,
};
use std::thread::{self, JoinHandle};

/// Cap on collected results — bounds memory and keeps a pathological `.[]` over a
/// huge array (every element selected) from filling the pane without end.
const MAX_HITS: usize = 5000;

/// One stage of a selection pipeline. A pipeline is a `Vec<Op>`; evaluation feeds
/// each value through the ops left to right, and a stage may fan out (`Iterate`)
/// or drop the value (`Field`/`Index` miss, or a failing `Select`).
#[derive(Clone, Debug)]
pub enum Op {
    /// `.foo` / `["foo"]` — the value at object key `foo` (nothing if absent).
    Field(String),
    /// `.[3]` / `[3]` — the element at array index `3` (nothing if out of range).
    Index(usize),
    /// `.[]` — every element of an array, or every value of an object.
    Iterate,
    /// `select(<cond>)` — keep the value unchanged iff `<cond>` holds.
    Select(Predicate),
}

/// A `select(...)` condition: navigate `path` from the current value, then either
/// test it for truthiness (`cmp` is `None`) or compare it against `cmp`'s literal.
#[derive(Clone, Debug)]
pub struct Predicate {
    path: Vec<PathSeg>,
    cmp: Option<(CmpOp, Literal)>,
}

#[derive(Clone, Debug)]
enum PathSeg {
    Field(String),
    Index(usize),
}

#[derive(Clone, Copy, Debug)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug)]
enum Literal {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
}

/// One selected node: a byte range into the source plus a display label built
/// from the path that reached it (e.g. `users[3].name`). `end_exact` mirrors the
/// scanner's provisional-end flag so the viewer knows whether the range's closer
/// has actually been scanned yet.
pub struct FilterHit {
    pub label: String,
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
    pub end_exact: bool,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Compile a typed pipeline into ops. Returns a footer-ready error string on a
/// malformed expression so the prompt can show it without losing what was typed.
pub fn parse_pipeline(src: &str) -> Result<Vec<Op>, String> {
    let mut ops = Vec::new();
    for stage in split_pipes(src) {
        let stage = stage.trim();
        if stage.is_empty() || stage == "." {
            continue; // identity — passes the value through unchanged
        }
        if let Some(inner) = strip_call(stage, "select") {
            ops.push(Op::Select(parse_predicate(inner)?));
        } else {
            ops.extend(parse_path(stage)?);
        }
    }
    Ok(ops)
}

/// Split on top-level `|`, ignoring pipes inside `"…"` strings or `(…)`/`[…]`.
fn split_pipes(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut esc = false;
    let mut depth = 0i32;
    for c in src.chars() {
        if in_str {
            cur.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                cur.push(c);
            }
            '(' | '[' => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' => {
                depth -= 1;
                cur.push(c);
            }
            '|' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// If `s` is `name( … )`, return the inside of the parens.
fn strip_call<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(name)?.trim_start();
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
    Some(inner)
}

/// Parse a path expression (`.a.b[0][]`, `["k"]`, …) into field/index/iterate ops.
fn parse_path(s: &str) -> Result<Vec<Op>, String> {
    let cs: Vec<char> = s.chars().collect();
    let n = cs.len();
    let mut i = 0;
    let mut ops = Vec::new();
    let skip_ws = |i: &mut usize| {
        while *i < n && cs[*i].is_whitespace() {
            *i += 1;
        }
    };
    skip_ws(&mut i);
    while i < n {
        match cs[i] {
            c if c.is_whitespace() => i += 1,
            '.' => {
                i += 1;
                if i < n && cs[i] == '[' {
                    continue; // `.[` — the bracket branch handles it
                }
                let start = i;
                while i < n && (cs[i].is_alphanumeric() || cs[i] == '_') {
                    i += 1;
                }
                if i == start {
                    return Err("expected a field name after '.'".into());
                }
                ops.push(Op::Field(cs[start..i].iter().collect()));
                if i < n && cs[i] == '?' {
                    i += 1;
                }
            }
            '[' => {
                i += 1;
                skip_ws(&mut i);
                if i < n && cs[i] == ']' {
                    i += 1;
                    ops.push(Op::Iterate);
                } else if i < n && cs[i] == '"' {
                    let (key, next) = parse_quoted(&cs, i)?;
                    i = next;
                    skip_ws(&mut i);
                    if i >= n || cs[i] != ']' {
                        return Err("expected ']' after key".into());
                    }
                    i += 1;
                    ops.push(Op::Field(key));
                } else {
                    let start = i;
                    while i < n && cs[i].is_ascii_digit() {
                        i += 1;
                    }
                    if i == start {
                        return Err("expected an index or \"key\" in [ ]".into());
                    }
                    let idx: usize = cs[start..i]
                        .iter()
                        .collect::<String>()
                        .parse()
                        .map_err(|_| "index out of range".to_string())?;
                    skip_ws(&mut i);
                    if i >= n || cs[i] != ']' {
                        return Err("expected ']' after index".into());
                    }
                    i += 1;
                    ops.push(Op::Index(idx));
                }
                if i < n && cs[i] == '?' {
                    i += 1;
                }
            }
            c => return Err(format!("unexpected '{c}' in path")),
        }
    }
    Ok(ops)
}

/// Read a `"…"` string starting at `cs[i] == '"'`, returning the decoded contents
/// and the index just past the closing quote.
fn parse_quoted(cs: &[char], mut i: usize) -> Result<(String, usize), String> {
    i += 1; // opening quote
    let mut s = String::new();
    while i < cs.len() {
        match cs[i] {
            '\\' if i + 1 < cs.len() => {
                s.push(cs[i + 1]);
                i += 2;
            }
            '"' => return Ok((s, i + 1)),
            c => {
                s.push(c);
                i += 1;
            }
        }
    }
    Err("unterminated string".into())
}

/// Parse a `select(...)` condition: a path, optionally followed by a comparison.
fn parse_predicate(inner: &str) -> Result<Predicate, String> {
    let cs: Vec<char> = inner.chars().collect();
    // Find the comparison operator at the top level (skipping string literals).
    let mut in_str = false;
    let mut split = None;
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if in_str {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => in_str = true,
            '=' if cs.get(i + 1) == Some(&'=') => {
                split = Some((i, 2, CmpOp::Eq));
                break;
            }
            '!' if cs.get(i + 1) == Some(&'=') => {
                split = Some((i, 2, CmpOp::Ne));
                break;
            }
            '<' if cs.get(i + 1) == Some(&'=') => {
                split = Some((i, 2, CmpOp::Le));
                break;
            }
            '>' if cs.get(i + 1) == Some(&'=') => {
                split = Some((i, 2, CmpOp::Ge));
                break;
            }
            '<' => {
                split = Some((i, 1, CmpOp::Lt));
                break;
            }
            '>' => {
                split = Some((i, 1, CmpOp::Gt));
                break;
            }
            _ => {}
        }
        i += 1;
    }

    match split {
        None => Ok(Predicate {
            path: parse_pred_path(inner)?,
            cmp: None,
        }),
        Some((pos, len, op)) => {
            let path_src: String = cs[..pos].iter().collect();
            let lit_src: String = cs[pos + len..].iter().collect();
            let lit = parse_literal(lit_src.trim())?;
            Ok(Predicate {
                path: parse_pred_path(&path_src)?,
                cmp: Some((op, lit)),
            })
        }
    }
}

/// A predicate path allows only field/index steps (no iteration): it must resolve
/// to a single value to test.
fn parse_pred_path(s: &str) -> Result<Vec<PathSeg>, String> {
    let mut segs = Vec::new();
    for op in parse_path(s)? {
        match op {
            Op::Field(k) => segs.push(PathSeg::Field(k)),
            Op::Index(i) => segs.push(PathSeg::Index(i)),
            Op::Iterate => return Err("[] is not allowed inside select()".into()),
            Op::Select(_) => return Err("nested select() is not supported".into()),
        }
    }
    Ok(segs)
}

fn parse_literal(s: &str) -> Result<Literal, String> {
    if s == "true" {
        Ok(Literal::Bool(true))
    } else if s == "false" {
        Ok(Literal::Bool(false))
    } else if s == "null" {
        Ok(Literal::Null)
    } else if s.starts_with('"') {
        let cs: Vec<char> = s.chars().collect();
        let (val, next) = parse_quoted(&cs, 0)?;
        if next != cs.len() {
            return Err("trailing text after string literal".into());
        }
        Ok(Literal::Str(val))
    } else {
        s.parse::<f64>()
            .map(Literal::Num)
            .map_err(|_| format!("not a value to compare against: {s}"))
    }
}

// ---------------------------------------------------------------------------
// The running filter (worker thread + streamed results)
// ---------------------------------------------------------------------------

/// One running (or finished) filter. Owns the worker's cancel flag and the
/// receiving end of its result stream. Mirrors [`crate::search::Search`].
pub struct Filter {
    cancel: Arc<AtomicBool>,
    rx: Receiver<FilterHit>,
    done: Arc<AtomicBool>,
    _handle: JoinHandle<()>,
    /// Results drained from `rx` so far.
    pub hits: Vec<FilterHit>,
    /// True once the worker finished and `rx` is fully drained.
    pub finished: bool,
}

impl Filter {
    /// Spawn a worker that evaluates `ops` over the value(s) in `[start, end)` and
    /// streams the byte range of every selected node. For NDJSON each document is
    /// fed through the pipeline in turn (like jq's stream-of-inputs).
    pub fn spawn(
        mmap: Arc<Source>,
        ops: Vec<Op>,
        jsonl: bool,
        start: usize,
        end: usize,
        kind: Kind,
    ) -> Filter {
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let cancel_w = cancel.clone();
        let done_w = done.clone();

        // A roomy stack: `eval` recurses one frame per remaining op (a handful),
        // so this is pure margin, matching the search worker's caution.
        let handle = thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(move || {
                let b: &[u8] = &mmap;
                let mut counter = 0u64;
                let mut found = 0usize;
                if jsonl {
                    let mut cur = Cursor::lines(start, end);
                    let mut i = 0usize;
                    while let Some(rc) = cur.next(b) {
                        if cancel_w.load(Ordering::Relaxed) {
                            break;
                        }
                        let label = format!("[{i}]");
                        let bailed = eval(
                            b,
                            rc.start,
                            rc.end,
                            rc.kind,
                            &ops,
                            label,
                            &cancel_w,
                            &tx,
                            &mut counter,
                            &mut found,
                        );
                        if bailed {
                            break;
                        }
                        i += 1;
                    }
                } else if start < end {
                    eval(
                        b,
                        start,
                        end,
                        kind,
                        &ops,
                        String::new(),
                        &cancel_w,
                        &tx,
                        &mut counter,
                        &mut found,
                    );
                }
                done_w.store(true, Ordering::Relaxed);
            })
            .expect("spawn filter worker thread");

        Filter {
            cancel,
            rx,
            done,
            _handle: handle,
            hits: Vec::new(),
            finished: false,
        }
    }

    /// Pull newly-selected results into `hits`. Returns the count of new ones.
    pub fn drain(&mut self) -> usize {
        let before = self.hits.len();
        while let Ok(h) = self.rx.try_recv() {
            self.hits.push(h);
        }
        if self.done.load(Ordering::Relaxed) {
            // The worker set `done` after its last send; one more sweep guarantees
            // we've seen everything before declaring finished.
            while let Ok(h) = self.rx.try_recv() {
                self.hits.push(h);
            }
            self.finished = true;
        }
        self.hits.len() - before
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for Filter {
    fn drop(&mut self) {
        // Dropping a filter (pane closed / superseded) bails the worker at its next
        // poll instead of walking the rest of the document.
        self.cancel.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Feed the value `[start, end)` through the remaining `ops`, emitting the byte
/// range of each value that survives to the end of the pipeline. `label` is the
/// path taken to reach this value, used only for display. Returns `true` if it
/// bailed (cancelled or hit the result cap) so callers stop iterating.
#[allow(clippy::too_many_arguments)]
fn eval(
    b: &[u8],
    start: usize,
    end: usize,
    kind: Kind,
    ops: &[Op],
    label: String,
    cancel: &AtomicBool,
    tx: &Sender<FilterHit>,
    counter: &mut u64,
    found: &mut usize,
) -> bool {
    *counter += 1;
    if *counter & 0xFFF == 0 && cancel.load(Ordering::Relaxed) {
        return true;
    }
    let Some((op, rest)) = ops.split_first() else {
        // Pipeline exhausted — this value is a result.
        return emit(tx, found, label, start, end, kind);
    };
    match op {
        Op::Field(key) => {
            if kind != Kind::Object {
                return false; // `.key` on a non-object yields nothing (lenient)
            }
            let mut cur = Cursor::new(start, end, false);
            while let Some(rc) = cur.next(b) {
                if &rc.label == key {
                    let child = extend_field(&label, key);
                    return eval(
                        b, rc.start, rc.end, rc.kind, rest, child, cancel, tx, counter, found,
                    );
                }
            }
            false
        }
        Op::Index(idx) => {
            if kind != Kind::Array {
                return false;
            }
            let mut cur = Cursor::new(start, end, true);
            let mut i = 0;
            while let Some(rc) = cur.next(b) {
                if i == *idx {
                    let child = format!("{label}[{idx}]");
                    return eval(
                        b, rc.start, rc.end, rc.kind, rest, child, cancel, tx, counter, found,
                    );
                }
                i += 1;
            }
            false
        }
        Op::Iterate => {
            if !matches!(kind, Kind::Object | Kind::Array) {
                return false;
            }
            let is_arr = kind == Kind::Array;
            let mut cur = Cursor::new(start, end, is_arr);
            let mut i = 0;
            while let Some(rc) = cur.next(b) {
                let child = if is_arr {
                    format!("{label}[{i}]")
                } else {
                    extend_field(&label, &rc.label)
                };
                if eval(
                    b, rc.start, rc.end, rc.kind, rest, child, cancel, tx, counter, found,
                ) {
                    return true;
                }
                i += 1;
            }
            false
        }
        Op::Select(pred) => {
            if pred_holds(b, start, end, kind, pred) {
                eval(b, start, end, kind, rest, label, cancel, tx, counter, found)
            } else {
                false
            }
        }
    }
}

/// Append a `.key` step to a display path, avoiding a leading dot at the root.
fn extend_field(label: &str, key: &str) -> String {
    if label.is_empty() {
        key.to_string()
    } else {
        format!("{label}.{key}")
    }
}

/// Send one result. Returns `true` (bail) if the receiver is gone or the cap is
/// reached, so the walk unwinds instead of scanning the rest of the document.
fn emit(
    tx: &Sender<FilterHit>,
    found: &mut usize,
    label: String,
    start: usize,
    end: usize,
    kind: Kind,
) -> bool {
    if *found >= MAX_HITS {
        return true;
    }
    // A container end from the scanner may be provisional (running to the parent's
    // bound); scalars are exact. The viewer resolves provisional ends lazily.
    let end_exact = !matches!(kind, Kind::Object | Kind::Array);
    if tx
        .send(FilterHit {
            label,
            start,
            end,
            kind,
            end_exact,
        })
        .is_err()
    {
        return true; // receiver dropped — filter superseded
    }
    *found += 1;
    *found >= MAX_HITS
}

/// Evaluate a `select(...)` condition against a value.
fn pred_holds(b: &[u8], start: usize, end: usize, kind: Kind, pred: &Predicate) -> bool {
    let Some((s, e, k)) = resolve(b, start, end, kind, &pred.path) else {
        return false; // the path doesn't exist here — condition fails (lenient)
    };
    match &pred.cmp {
        None => truthy(b, s, k),
        Some((op, lit)) => compare(b, s, e, k, *op, lit),
    }
}

/// Follow a field/index path from a value to the value it points at.
fn resolve(
    b: &[u8],
    mut start: usize,
    mut end: usize,
    mut kind: Kind,
    segs: &[PathSeg],
) -> Option<(usize, usize, Kind)> {
    for seg in segs {
        match seg {
            PathSeg::Field(key) => {
                if kind != Kind::Object {
                    return None;
                }
                let mut cur = Cursor::new(start, end, false);
                let rc = loop {
                    let rc = cur.next(b)?;
                    if &rc.label == key {
                        break rc;
                    }
                };
                start = rc.start;
                end = rc.end;
                kind = rc.kind;
            }
            PathSeg::Index(idx) => {
                if kind != Kind::Array {
                    return None;
                }
                let mut cur = Cursor::new(start, end, true);
                let mut i = 0;
                let rc = loop {
                    let rc = cur.next(b)?;
                    if i == *idx {
                        break rc;
                    }
                    i += 1;
                };
                start = rc.start;
                end = rc.end;
                kind = rc.kind;
            }
        }
    }
    Some((start, end, kind))
}

/// jq truthiness: everything is true except `false` and `null`.
fn truthy(b: &[u8], start: usize, kind: Kind) -> bool {
    match kind {
        Kind::Null => false,
        Kind::Bool => b.get(start) != Some(&b'f'),
        _ => true,
    }
}

/// Compare a value against a literal. Mismatched types are never ordered and are
/// unequal (so `==` is false and `!=` is true), matching jq's cross-type rules
/// closely enough for filtering.
fn compare(b: &[u8], start: usize, end: usize, kind: Kind, op: CmpOp, lit: &Literal) -> bool {
    match lit {
        Literal::Num(x) => {
            if kind != Kind::Number {
                return matches!(op, CmpOp::Ne);
            }
            let Some(v) = std::str::from_utf8(&b[start..end])
                .ok()
                .and_then(|t| t.trim().parse::<f64>().ok())
            else {
                return matches!(op, CmpOp::Ne);
            };
            match op {
                CmpOp::Eq => v == *x,
                CmpOp::Ne => v != *x,
                CmpOp::Lt => v < *x,
                CmpOp::Le => v <= *x,
                CmpOp::Gt => v > *x,
                CmpOp::Ge => v >= *x,
            }
        }
        Literal::Str(sv) => {
            if kind != Kind::Str {
                return matches!(op, CmpOp::Ne);
            }
            let v = decode_str(b, start, end);
            match op {
                CmpOp::Eq => &v == sv,
                CmpOp::Ne => &v != sv,
                CmpOp::Lt => &v < sv,
                CmpOp::Le => &v <= sv,
                CmpOp::Gt => &v > sv,
                CmpOp::Ge => &v >= sv,
            }
        }
        Literal::Bool(bv) => {
            if kind != Kind::Bool {
                return matches!(op, CmpOp::Ne);
            }
            let v = b.get(start) != Some(&b'f');
            match op {
                CmpOp::Eq => v == *bv,
                CmpOp::Ne => v != *bv,
                _ => false,
            }
        }
        Literal::Null => {
            let is_null = kind == Kind::Null;
            match op {
                CmpOp::Eq => is_null,
                CmpOp::Ne => !is_null,
                _ => false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &[u8], expr: &str, jsonl: bool) -> Vec<String> {
        let ops = parse_pipeline(expr).expect("parse");
        let end = src.len();
        let kind = if jsonl {
            Kind::Array
        } else {
            crate::scanner::value_kind(src, crate::scanner::skip_ws(src, 0, end))
        };
        let start = if jsonl {
            0
        } else {
            crate::scanner::skip_ws(src, 0, end)
        };
        let source = Arc::new(Source::Buffered(src.to_vec()));
        let mut f = Filter::spawn(source, ops, jsonl, start, end, kind);
        for _ in 0..10_000 {
            f.drain();
            if f.finished {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(f.finished, "filter did not finish");
        f.hits
            .iter()
            .map(|h| {
                // A container hit's `end` is provisional (the parent's bound); the
                // viewer resolves the real closer lazily, so do the same here.
                let end = crate::scanner::skip_value(src, h.start, src.len());
                String::from_utf8_lossy(&src[h.start..end]).into_owned()
            })
            .collect()
    }

    #[test]
    fn field_access() {
        let b = br#"{"a":{"b":42},"c":7}"#;
        assert_eq!(run(b, ".a.b", false), vec!["42"]);
        assert_eq!(run(b, ".c", false), vec!["7"]);
    }

    #[test]
    fn iterate_and_project() {
        let b = br#"{"users":[{"name":"amy"},{"name":"bob"}]}"#;
        assert_eq!(
            run(b, ".users[] | .name", false),
            vec!["\"amy\"", "\"bob\""]
        );
    }

    #[test]
    fn iterate_object_values() {
        let b = br#"{"x":1,"y":2,"z":3}"#;
        assert_eq!(run(b, ".[]", false), vec!["1", "2", "3"]);
    }

    #[test]
    fn select_numeric_comparison() {
        let b = br#"[{"age":20},{"age":40},{"age":60}]"#;
        let hits = run(b, ".[] | select(.age > 30) | .age", false);
        assert_eq!(hits, vec!["40", "60"]);
    }

    #[test]
    fn select_string_equality() {
        let b = br#"[{"t":"x"},{"t":"y"},{"t":"x"}]"#;
        let hits = run(b, ".[] | select(.t == \"x\")", false);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0], r#"{"t":"x"}"#);
    }

    #[test]
    fn select_truthy() {
        let b = br#"[{"ok":true,"n":1},{"ok":false,"n":2},{"n":3}]"#;
        let hits = run(b, ".[] | select(.ok) | .n", false);
        assert_eq!(hits, vec!["1"]);
    }

    #[test]
    fn index_and_bracket_key() {
        let b = br#"{"a.b":[10,20,30]}"#;
        assert_eq!(run(b, r#"["a.b"][1]"#, false), vec!["20"]);
        assert_eq!(run(b, r#"["a.b"] | .[2]"#, false), vec!["30"]);
    }

    #[test]
    fn missing_paths_are_lenient() {
        let b = br#"[{"a":1},{"b":2},{"a":3}]"#;
        // Only the elements that actually have `.a` produce output.
        assert_eq!(run(b, ".[] | .a", false), vec!["1", "3"]);
    }

    #[test]
    fn ndjson_feeds_each_document() {
        let b = b"{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n";
        assert_eq!(run(b, ".n", true), vec!["1", "2", "3"]);
        assert_eq!(run(b, "select(.n >= 2) | .n", true), vec!["2", "3"]);
    }

    #[test]
    fn identity_returns_the_whole_value() {
        let b = br#"{"a":1}"#;
        assert_eq!(run(b, ".", false), vec![r#"{"a":1}"#]);
    }

    #[test]
    fn parse_errors_are_reported() {
        assert!(parse_pipeline(".foo[").is_err());
        assert!(parse_pipeline("select(.a >)").is_err());
        assert!(parse_pipeline("select(.[] > 1)").is_err());
    }
}
