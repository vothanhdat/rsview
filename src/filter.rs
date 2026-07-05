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
//! A program is a pipeline of `|`-separated **stages**; each stage is a
//! comma-separated list of **terms** whose outputs are unioned. Supported terms:
//!   - `.`                 identity (the current value)
//!   - `.foo` / `.foo.bar` object field access
//!   - `["key"]`           bracketed field access (keys with dots/spaces)
//!   - `.[3]` / `[3]`      array index; negatives count from the end (`.[-1]`)
//!   - `.[1:3]` / `.[-2:]` array slice — streams the elements in the range
//!   - `.[]` / `[]`        iterate an array's elements or an object's values
//!   - `..`                recursive descent — this value and every descendant
//!   - `a, b`              comma — union of the outputs of `a` and `b`
//!   - `select(<cond>)`    keep the value when `<cond>` holds. `<cond>` is built
//!     from comparisons (`<operand> <op> <operand>`, `op` one of `== != < <= > >=`),
//!     string matches (`<path> ~ "pattern"` / `!~`), bare operands (kept when
//!     truthy), `and` / `or`, and `( … )` grouping. An operand is a path (`.a.b`)
//!     or a literal (number / "string" / true / false / null). The `~` pattern
//!     speaks the same dialect as `/` search — plain text is a case-insensitive
//!     substring, `re:` a regex, `g:` a `*`/`?` glob — and matches only string
//!     values (anything else is a non-match, never an error).
//!
//! Everything is lenient (jq's `?`): a path that doesn't exist on a value simply
//! produces no output rather than erroring, so `.a.b` over a mixed array yields
//! results only for the elements that have `a.b`. Inside `select`, a missing path
//! reads as `null` (so `select(.x == null)` matches values lacking `x`).
//!
//! Note the two intentional divergences from jq, both in service of staying
//! zero-copy: a slice `.[1:3]` yields the elements as a *stream* (like
//! `.[1:3][]`) rather than a new sub-array, and cross-type comparisons are simply
//! unequal-and-unordered rather than following jq's total type order.

use crate::scanner::{decode_str, Cursor, Kind, MAX_DEPTH};
use crate::search::Pattern;
use crate::source::Source;
use std::cmp::Ordering;
use std::sync::{
    atomic::{AtomicBool, Ordering as AtomicOrdering},
    mpsc::{self, Receiver, Sender},
    Arc,
};
use std::thread::{self, JoinHandle};

/// Cap on collected results — bounds memory and keeps a pathological `.[]` over a
/// huge array (every element selected) from filling the pane without end.
const MAX_HITS: usize = 5000;

/// A parsed program: a pipeline of stages run left to right.
#[derive(Clone, Debug)]
pub struct Program {
    stages: Vec<Stage>,
}

/// One `|`-separated stage: a union of comma-separated terms.
#[derive(Clone, Debug)]
struct Stage {
    terms: Vec<Term>,
}

/// One comma-separated term within a stage.
#[derive(Clone, Debug)]
enum Term {
    /// A path expression: a sequence of navigation steps applied left to right.
    Path(Vec<PathOp>),
    /// `select(<cond>)` — passes the current value through iff `<cond>` holds.
    Select(Cond),
}

/// One navigation step in a path term.
#[derive(Clone, Debug)]
enum PathOp {
    /// `.foo` / `["foo"]` — the value at object key `foo` (nothing if absent).
    Field(String),
    /// `.[3]` / `[3]` — the element at array index (negatives count from the end).
    Index(i64),
    /// `.[lo:hi]` — the elements of an array in the half-open range, as a stream.
    Slice(Option<i64>, Option<i64>),
    /// `.[]` — every element of an array, or every value of an object.
    Iterate,
    /// `..` — this value followed by every descendant, depth-first.
    Recurse,
}

/// A `select(...)` condition tree.
#[derive(Clone, Debug)]
enum Cond {
    /// `<operand>` alone — kept when truthy.
    Truthy(Operand),
    /// `<operand> <op> <operand>`.
    Cmp(Operand, CmpOp, Operand),
    /// `<operand> ~ "pattern"` (or `!~`) — string match against a compiled
    /// pattern. Holds iff the operand resolves to a string that matches
    /// (XORed with `negated`); a non-string operand never matches.
    Match {
        lhs: Operand,
        pattern: Pattern,
        negated: bool,
    },
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
}

/// One side of a comparison (or a bare truthy test).
#[derive(Clone, Debug)]
enum Operand {
    /// A field/index path from the current value (empty = the value itself).
    Path(Vec<PathSeg>),
    Lit(Literal),
}

#[derive(Clone, Debug)]
enum PathSeg {
    Field(String),
    Index(i64),
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

/// Compile a typed program into stages/terms. Returns a footer-ready error string
/// on a malformed expression so the prompt can show it without losing the text.
pub fn parse_pipeline(src: &str) -> Result<Program, String> {
    let mut stages = Vec::new();
    for stage_src in split_top(src, '|') {
        let mut terms = Vec::new();
        for term_src in split_top(&stage_src, ',') {
            let term_src = term_src.trim();
            if term_src.is_empty() || term_src == "." {
                terms.push(Term::Path(Vec::new())); // identity
            } else if let Some(inner) = strip_call(term_src, "select") {
                terms.push(Term::Select(parse_cond(inner)?));
            } else {
                terms.push(Term::Path(parse_path_ops(term_src)?));
            }
        }
        if terms.is_empty() {
            terms.push(Term::Path(Vec::new()));
        }
        stages.push(Stage { terms });
    }
    Ok(Program { stages })
}

/// Split `src` on top-level occurrences of `sep`, ignoring separators inside
/// `"…"` strings or `(…)` / `[…]` nesting.
fn split_top(src: &str, sep: char) -> Vec<String> {
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
            _ if c == sep && depth == 0 => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// If `s` is `name( … )`, return the inside of the parens.
fn strip_call<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(name)?.trim_start();
    rest.strip_prefix('(')?.strip_suffix(')')
}

/// Parse a path term (`.a.b[0][]`, `["k"]`, `.[1:3]`, `..`) into navigation ops.
fn parse_path_ops(s: &str) -> Result<Vec<PathOp>, String> {
    let cs: Vec<char> = s.chars().collect();
    let n = cs.len();
    let mut i = 0;
    let mut ops = Vec::new();
    skip_ws(&cs, &mut i);
    while i < n {
        match cs[i] {
            c if c.is_whitespace() => i += 1,
            // `..` recursive descent must be checked before a plain `.field`.
            '.' if i + 1 < n && cs[i + 1] == '.' => {
                ops.push(PathOp::Recurse);
                i += 2;
            }
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
                ops.push(PathOp::Field(cs[start..i].iter().collect()));
                opt_question(&cs, &mut i);
            }
            '[' => {
                ops.push(parse_bracket(&cs, &mut i)?);
                opt_question(&cs, &mut i);
            }
            c => return Err(format!("unexpected '{c}' in path")),
        }
    }
    Ok(ops)
}

/// Parse one `[ … ]` accessor: `[]`, `["key"]`, `[N]`, or a slice `[lo:hi]`.
/// `i` points at `[`; on return it is just past the matching `]`.
fn parse_bracket(cs: &[char], i: &mut usize) -> Result<PathOp, String> {
    let n = cs.len();
    *i += 1; // past '['
    skip_ws(cs, i);
    if *i < n && cs[*i] == ']' {
        *i += 1;
        return Ok(PathOp::Iterate);
    }
    if *i < n && cs[*i] == '"' {
        let (key, next) = parse_quoted(cs, *i)?;
        *i = next;
        skip_ws(cs, i);
        if *i >= n || cs[*i] != ']' {
            return Err("expected ']' after key".into());
        }
        *i += 1;
        return Ok(PathOp::Field(key));
    }
    // A number, or a slice with optional bounds around a ':'.
    let lo = parse_opt_int(cs, i)?;
    skip_ws(cs, i);
    if *i < n && cs[*i] == ':' {
        *i += 1;
        skip_ws(cs, i);
        let hi = parse_opt_int(cs, i)?;
        skip_ws(cs, i);
        if *i >= n || cs[*i] != ']' {
            return Err("expected ']' after slice".into());
        }
        *i += 1;
        Ok(PathOp::Slice(lo, hi))
    } else {
        let idx = lo.ok_or("expected an index, \"key\", slice, or [] here")?;
        if *i >= n || cs[*i] != ']' {
            return Err("expected ']' after index".into());
        }
        *i += 1;
        Ok(PathOp::Index(idx))
    }
}

fn opt_question(cs: &[char], i: &mut usize) {
    if *i < cs.len() && cs[*i] == '?' {
        *i += 1; // `?` — leniency is automatic, so this is accepted and ignored
    }
}

fn skip_ws(cs: &[char], i: &mut usize) {
    while *i < cs.len() && cs[*i].is_whitespace() {
        *i += 1;
    }
}

/// Parse an optional signed integer at `*i`, advancing past it. `None` if there's
/// no digit there (so a bare `:` slice bound is allowed).
fn parse_opt_int(cs: &[char], i: &mut usize) -> Result<Option<i64>, String> {
    let start = *i;
    if *i < cs.len() && cs[*i] == '-' {
        *i += 1;
    }
    let digits_start = *i;
    while *i < cs.len() && cs[*i].is_ascii_digit() {
        *i += 1;
    }
    if *i == digits_start {
        *i = start; // no digits — rewind past any sign we consumed
        return Ok(None);
    }
    cs[start..*i]
        .iter()
        .collect::<String>()
        .parse()
        .map(Some)
        .map_err(|_| "number out of range".to_string())
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

// --- select() condition parser (recursive descent: or > and > primary) -------

fn parse_cond(inner: &str) -> Result<Cond, String> {
    let cs: Vec<char> = inner.chars().collect();
    let mut i = 0;
    let cond = parse_or(&cs, &mut i)?;
    skip_ws(&cs, &mut i);
    if i != cs.len() {
        return Err(format!("unexpected '{}' in condition", cs[i]));
    }
    Ok(cond)
}

fn parse_or(cs: &[char], i: &mut usize) -> Result<Cond, String> {
    let mut left = parse_and(cs, i)?;
    while match_keyword(cs, i, "or") {
        let right = parse_and(cs, i)?;
        left = Cond::Or(Box::new(left), Box::new(right));
    }
    Ok(left)
}

fn parse_and(cs: &[char], i: &mut usize) -> Result<Cond, String> {
    let mut left = parse_primary(cs, i)?;
    while match_keyword(cs, i, "and") {
        let right = parse_primary(cs, i)?;
        left = Cond::And(Box::new(left), Box::new(right));
    }
    Ok(left)
}

fn parse_primary(cs: &[char], i: &mut usize) -> Result<Cond, String> {
    skip_ws(cs, i);
    if *i < cs.len() && cs[*i] == '(' {
        *i += 1;
        let inner = parse_or(cs, i)?;
        skip_ws(cs, i);
        if *i >= cs.len() || cs[*i] != ')' {
            return Err("expected ')' in condition".into());
        }
        *i += 1;
        return Ok(inner);
    }
    let left = parse_operand(cs, i)?;
    skip_ws(cs, i);
    if let Some(negated) = match_tilde(cs, i) {
        skip_ws(cs, i);
        let pattern = match parse_operand(cs, i)? {
            Operand::Lit(Literal::Str(s)) => Pattern::parse(&s)?,
            _ => return Err("~ needs a \"pattern\" string on its right".into()),
        };
        Ok(Cond::Match {
            lhs: left,
            pattern,
            negated,
        })
    } else if let Some(op) = match_cmp_op(cs, i) {
        let right = parse_operand(cs, i)?;
        Ok(Cond::Cmp(left, op, right))
    } else {
        Ok(Cond::Truthy(left))
    }
}

/// Match a `~` (regex/substring/glob match) or `!~` (negated), advancing past it.
/// Returns `Some(negated)` on success. `!=` is handled by [`match_cmp_op`] and
/// won't be mistaken for `!~` since its second char differs.
fn match_tilde(cs: &[char], i: &mut usize) -> Option<bool> {
    if *i + 1 < cs.len() && cs[*i] == '!' && cs[*i + 1] == '~' {
        *i += 2;
        Some(true)
    } else if *i < cs.len() && cs[*i] == '~' {
        *i += 1;
        Some(false)
    } else {
        None
    }
}

/// Match a whole-word keyword (`and`/`or`), advancing past it on success.
fn match_keyword(cs: &[char], i: &mut usize, kw: &str) -> bool {
    let save = *i;
    skip_ws(cs, i);
    let start = *i;
    for kc in kw.chars() {
        if *i >= cs.len() || cs[*i] != kc {
            *i = save;
            return false;
        }
        *i += 1;
    }
    // Must be a standalone word, not the prefix of an identifier.
    if *i < cs.len() && (cs[*i].is_alphanumeric() || cs[*i] == '_') {
        *i = save;
        return false;
    }
    let _ = start;
    true
}

fn match_cmp_op(cs: &[char], i: &mut usize) -> Option<CmpOp> {
    let two = |a: char, b: char, i: &mut usize| -> bool {
        if *i + 1 < cs.len() && cs[*i] == a && cs[*i + 1] == b {
            *i += 2;
            true
        } else {
            false
        }
    };
    if two('=', '=', i) {
        Some(CmpOp::Eq)
    } else if two('!', '=', i) {
        Some(CmpOp::Ne)
    } else if two('<', '=', i) {
        Some(CmpOp::Le)
    } else if two('>', '=', i) {
        Some(CmpOp::Ge)
    } else if *i < cs.len() && cs[*i] == '<' {
        *i += 1;
        Some(CmpOp::Lt)
    } else if *i < cs.len() && cs[*i] == '>' {
        *i += 1;
        Some(CmpOp::Gt)
    } else {
        None
    }
}

fn parse_operand(cs: &[char], i: &mut usize) -> Result<Operand, String> {
    skip_ws(cs, i);
    if *i >= cs.len() {
        return Err("expected a value or path in condition".into());
    }
    match cs[*i] {
        '.' | '[' => Ok(Operand::Path(parse_cond_path(cs, i)?)),
        '"' => {
            let (s, next) = parse_quoted(cs, *i)?;
            *i = next;
            Ok(Operand::Lit(Literal::Str(s)))
        }
        _ => {
            let start = *i;
            while *i < cs.len()
                && !cs[*i].is_whitespace()
                && !matches!(cs[*i], '(' | ')' | ',' | '<' | '>' | '=' | '!' | '~')
            {
                *i += 1;
            }
            let tok: String = cs[start..*i].iter().collect();
            parse_bareword(&tok)
        }
    }
}

/// A condition path: field/index steps only (must resolve to a single value). A
/// lone `.` is the identity (the value under test itself).
fn parse_cond_path(cs: &[char], i: &mut usize) -> Result<Vec<PathSeg>, String> {
    let n = cs.len();
    let mut segs = Vec::new();
    loop {
        if *i < n && cs[*i] == '.' {
            *i += 1;
            if *i < n && cs[*i] == '[' {
                continue; // `.[` handled below
            }
            let start = *i;
            while *i < n && (cs[*i].is_alphanumeric() || cs[*i] == '_') {
                *i += 1;
            }
            if *i > start {
                segs.push(PathSeg::Field(cs[start..*i].iter().collect()));
            }
            // else: a lone/trailing `.` — identity so far; stop.
            if *i == start {
                break;
            }
        } else if *i < n && cs[*i] == '[' {
            match parse_bracket(cs, i)? {
                PathOp::Field(k) => segs.push(PathSeg::Field(k)),
                PathOp::Index(idx) => segs.push(PathSeg::Index(idx)),
                _ => return Err("only keys and indices are allowed in a select() path".into()),
            }
        } else {
            break;
        }
    }
    Ok(segs)
}

fn parse_bareword(tok: &str) -> Result<Operand, String> {
    match tok {
        "true" => Ok(Operand::Lit(Literal::Bool(true))),
        "false" => Ok(Operand::Lit(Literal::Bool(false))),
        "null" => Ok(Operand::Lit(Literal::Null)),
        "" => Err("expected a value in condition".into()),
        _ => tok
            .parse::<f64>()
            .map(|n| Operand::Lit(Literal::Num(n)))
            .map_err(|_| format!("not a value to compare against: {tok}")),
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

/// Per-run mutable evaluation state, threaded through the recursion.
struct Ctx<'a> {
    cancel: &'a AtomicBool,
    tx: &'a Sender<FilterHit>,
    counter: u64,
    found: usize,
}

impl Ctx<'_> {
    /// Poll the cancel flag every 4096 nodes; `true` means unwind the whole walk.
    fn cancelled(&mut self) -> bool {
        self.counter += 1;
        self.counter & 0xFFF == 0 && self.cancel.load(AtomicOrdering::Relaxed)
    }
}

impl Filter {
    /// Spawn a worker that evaluates `program` over the value(s) in `[start, end)`
    /// and streams the byte range of every selected node. For NDJSON each document
    /// is fed through the pipeline in turn (like jq's stream of inputs).
    pub fn spawn(
        mmap: Arc<Source>,
        program: Program,
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

        // A roomy stack: `..` recurses one frame per nesting level (capped at
        // MAX_DEPTH), matching the search worker's caution.
        let handle = thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(move || {
                let b: &[u8] = &mmap;
                let mut ctx = Ctx {
                    cancel: &cancel_w,
                    tx: &tx,
                    counter: 0,
                    found: 0,
                };
                if jsonl {
                    let mut cur = Cursor::lines(start, end);
                    let mut i = 0usize;
                    while let Some(rc) = cur.next(b) {
                        if cancel_w.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        let label = format!("[{i}]");
                        if eval_stages(
                            b,
                            rc.start,
                            rc.end,
                            rc.kind,
                            &program.stages,
                            &label,
                            &mut ctx,
                        ) {
                            break;
                        }
                        i += 1;
                    }
                } else if start < end {
                    eval_stages(b, start, end, kind, &program.stages, "", &mut ctx);
                }
                done_w.store(true, AtomicOrdering::Relaxed);
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
        if self.done.load(AtomicOrdering::Relaxed) {
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
        self.cancel.store(true, AtomicOrdering::Relaxed);
    }
}

impl Drop for Filter {
    fn drop(&mut self) {
        // Dropping a filter (pane closed / superseded) bails the worker at its next
        // poll instead of walking the rest of the document.
        self.cancel.store(true, AtomicOrdering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Run the value `[start, end)` through the remaining pipeline stages. With no
/// stages left the value is a result. Returns `true` if the walk should bail
/// (cancelled or at the result cap).
fn eval_stages(
    b: &[u8],
    start: usize,
    end: usize,
    kind: Kind,
    stages: &[Stage],
    label: &str,
    ctx: &mut Ctx,
) -> bool {
    if ctx.cancelled() {
        return true;
    }
    let Some((stage, rest)) = stages.split_first() else {
        return emit(ctx, label, start, end, kind);
    };
    // Union of the stage's comma-separated terms, each fed the same input.
    for term in &stage.terms {
        let bail = match term {
            Term::Select(cond) => {
                if cond_holds(b, start, end, kind, cond) {
                    eval_stages(b, start, end, kind, rest, label, ctx)
                } else {
                    false
                }
            }
            Term::Path(ops) => eval_path(b, start, end, kind, ops, rest, label, ctx),
        };
        if bail {
            return true;
        }
    }
    false
}

/// Apply the navigation `ops` of one term, then continue into `rest` stages.
#[allow(clippy::too_many_arguments)]
fn eval_path(
    b: &[u8],
    start: usize,
    end: usize,
    kind: Kind,
    ops: &[PathOp],
    rest: &[Stage],
    label: &str,
    ctx: &mut Ctx,
) -> bool {
    let Some((op, more)) = ops.split_first() else {
        return eval_stages(b, start, end, kind, rest, label, ctx);
    };
    match op {
        PathOp::Field(key) => {
            if kind != Kind::Object {
                return false; // `.key` on a non-object yields nothing (lenient)
            }
            let mut cur = Cursor::new(start, end, false);
            while let Some(rc) = cur.next(b) {
                if &rc.label == key {
                    let cl = extend_field(label, key);
                    return eval_path(b, rc.start, rc.end, rc.kind, more, rest, &cl, ctx);
                }
            }
            false
        }
        PathOp::Index(idx) => {
            if kind != Kind::Array {
                return false;
            }
            let Some(abs) = abs_index(b, start, end, *idx) else {
                return false;
            };
            match nth_child(b, start, end, abs) {
                Some(rc) => {
                    let cl = format!("{label}[{abs}]");
                    eval_path(b, rc.start, rc.end, rc.kind, more, rest, &cl, ctx)
                }
                None => false,
            }
        }
        PathOp::Slice(lo, hi) => {
            if kind != Kind::Array {
                return false;
            }
            let len = count_children(b, start, end, true);
            let l = resolve_bound(*lo, len, 0);
            let h = resolve_bound(*hi, len, len);
            if h <= l {
                return false;
            }
            let mut cur = Cursor::new(start, end, true);
            let mut i = 0;
            while let Some(rc) = cur.next(b) {
                if i >= h {
                    break;
                }
                if i >= l {
                    let cl = format!("{label}[{i}]");
                    if eval_path(b, rc.start, rc.end, rc.kind, more, rest, &cl, ctx) {
                        return true;
                    }
                }
                i += 1;
            }
            false
        }
        PathOp::Iterate => {
            if !matches!(kind, Kind::Object | Kind::Array) {
                return false;
            }
            let is_arr = kind == Kind::Array;
            let mut cur = Cursor::new(start, end, is_arr);
            let mut i = 0;
            while let Some(rc) = cur.next(b) {
                if ctx.cancelled() {
                    return true;
                }
                let cl = if is_arr {
                    format!("{label}[{i}]")
                } else {
                    extend_field(label, &rc.label)
                };
                if eval_path(b, rc.start, rc.end, rc.kind, more, rest, &cl, ctx) {
                    return true;
                }
                i += 1;
            }
            false
        }
        PathOp::Recurse => recurse(b, start, end, kind, 0, more, rest, label, ctx),
    }
}

/// `..` — feed this node into the continuation, then every descendant depth-first.
#[allow(clippy::too_many_arguments)]
fn recurse(
    b: &[u8],
    start: usize,
    end: usize,
    kind: Kind,
    depth: usize,
    more: &[PathOp],
    rest: &[Stage],
    label: &str,
    ctx: &mut Ctx,
) -> bool {
    if eval_path(b, start, end, kind, more, rest, label, ctx) {
        return true;
    }
    if depth >= MAX_DEPTH || !matches!(kind, Kind::Object | Kind::Array) {
        return false;
    }
    let is_arr = kind == Kind::Array;
    let mut cur = Cursor::new(start, end, is_arr);
    let mut i = 0;
    while let Some(rc) = cur.next(b) {
        if ctx.cancelled() {
            return true;
        }
        let cl = if is_arr {
            format!("{label}[{i}]")
        } else {
            extend_field(label, &rc.label)
        };
        if recurse(
            b,
            rc.start,
            rc.end,
            rc.kind,
            depth + 1,
            more,
            rest,
            &cl,
            ctx,
        ) {
            return true;
        }
        i += 1;
    }
    false
}

/// Append a `.key` step to a display path, avoiding a leading dot at the root.
fn extend_field(label: &str, key: &str) -> String {
    if label.is_empty() {
        key.to_string()
    } else {
        format!("{label}.{key}")
    }
}

/// Resolve a (possibly negative) array index to an absolute one, or `None` if it
/// falls outside the array.
fn abs_index(b: &[u8], start: usize, end: usize, idx: i64) -> Option<usize> {
    if idx >= 0 {
        return Some(idx as usize);
    }
    let len = count_children(b, start, end, true) as i64;
    let a = len + idx;
    if a < 0 {
        None
    } else {
        Some(a as usize)
    }
}

/// Clamp a slice bound (default when absent) into `[0, len]`.
fn resolve_bound(bound: Option<i64>, len: usize, default: usize) -> usize {
    match bound {
        None => default,
        Some(v) if v < 0 => (len as i64 + v).max(0) as usize,
        Some(v) => (v as usize).min(len),
    }
}

/// The `n`th immediate child of a container, or `None` if there are fewer.
fn nth_child(b: &[u8], start: usize, end: usize, n: usize) -> Option<crate::scanner::RawChild> {
    let mut cur = Cursor::new(start, end, true);
    let mut i = 0;
    while let Some(rc) = cur.next(b) {
        if i == n {
            return Some(rc);
        }
        i += 1;
    }
    None
}

/// Count a container's immediate children (O(1) memory, one scan).
fn count_children(b: &[u8], start: usize, end: usize, is_array: bool) -> usize {
    let mut cur = Cursor::new(start, end, is_array);
    let mut n = 0;
    while cur.next(b).is_some() {
        n += 1;
    }
    n
}

/// Send one result. Returns `true` (bail) if the receiver is gone or the cap is
/// reached, so the walk unwinds instead of scanning the rest of the document.
fn emit(ctx: &mut Ctx, label: &str, start: usize, end: usize, kind: Kind) -> bool {
    if ctx.found >= MAX_HITS {
        return true;
    }
    // A container end from the scanner may be provisional (running to the parent's
    // bound); scalars are exact. The viewer resolves provisional ends lazily.
    let end_exact = !matches!(kind, Kind::Object | Kind::Array);
    if ctx
        .tx
        .send(FilterHit {
            label: label.to_string(),
            start,
            end,
            kind,
            end_exact,
        })
        .is_err()
    {
        return true; // receiver dropped — filter superseded
    }
    ctx.found += 1;
    ctx.found >= MAX_HITS
}

// --- select() evaluation -----------------------------------------------------

/// A value pulled out for comparison. Containers are `Other` (truthy, but never
/// equal or ordered against a scalar).
enum Val {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    Other,
}

fn cond_holds(b: &[u8], s: usize, e: usize, k: Kind, cond: &Cond) -> bool {
    match cond {
        Cond::Truthy(op) => truthy(&operand_val(b, s, e, k, op)),
        Cond::Cmp(a, op, c) => compare(
            &operand_val(b, s, e, k, a),
            *op,
            &operand_val(b, s, e, k, c),
        ),
        Cond::Match {
            lhs,
            pattern,
            negated,
        } => {
            // Only a string value can match; anything else (number, container,
            // missing → null) is a non-match, never an error.
            let matched = match operand_val(b, s, e, k, lhs) {
                Val::Str(hay) => pattern.is_match(&hay),
                _ => false,
            };
            matched ^ negated
        }
        Cond::And(x, y) => cond_holds(b, s, e, k, x) && cond_holds(b, s, e, k, y),
        Cond::Or(x, y) => cond_holds(b, s, e, k, x) || cond_holds(b, s, e, k, y),
    }
}

fn operand_val(b: &[u8], s: usize, e: usize, k: Kind, op: &Operand) -> Val {
    match op {
        Operand::Lit(l) => match l {
            Literal::Num(n) => Val::Num(*n),
            Literal::Str(s) => Val::Str(s.clone()),
            Literal::Bool(v) => Val::Bool(*v),
            Literal::Null => Val::Null,
        },
        // A missing path reads as null (jq semantics), so `.x == null` matches
        // absent fields.
        Operand::Path(segs) => match resolve(b, s, e, k, segs) {
            None => Val::Null,
            Some((s2, e2, k2)) => node_val(b, s2, e2, k2),
        },
    }
}

fn node_val(b: &[u8], s: usize, e: usize, k: Kind) -> Val {
    match k {
        Kind::Number => std::str::from_utf8(&b[s..e])
            .ok()
            .and_then(|t| t.trim().parse::<f64>().ok())
            .map_or(Val::Other, Val::Num),
        Kind::Str => Val::Str(decode_str(b, s, e)),
        Kind::Bool => Val::Bool(b.get(s) != Some(&b'f')),
        Kind::Null => Val::Null,
        Kind::Object | Kind::Array => Val::Other,
    }
}

/// jq truthiness: everything is true except `false` and `null`.
fn truthy(v: &Val) -> bool {
    !matches!(v, Val::Null | Val::Bool(false))
}

/// Compare two values. Mismatched types (and containers) are unequal and
/// unordered — so `==` is false, `!=` is true, and orderings are false.
fn compare(a: &Val, op: CmpOp, b: &Val) -> bool {
    let ord = match (a, b) {
        (Val::Num(x), Val::Num(y)) => x.partial_cmp(y),
        (Val::Str(x), Val::Str(y)) => Some(x.cmp(y)),
        (Val::Bool(x), Val::Bool(y)) => Some(x.cmp(y)),
        (Val::Null, Val::Null) => Some(Ordering::Equal),
        _ => None,
    };
    match op {
        CmpOp::Eq => ord == Some(Ordering::Equal),
        CmpOp::Ne => ord != Some(Ordering::Equal),
        CmpOp::Lt => ord == Some(Ordering::Less),
        CmpOp::Le => matches!(ord, Some(Ordering::Less | Ordering::Equal)),
        CmpOp::Gt => ord == Some(Ordering::Greater),
        CmpOp::Ge => matches!(ord, Some(Ordering::Greater | Ordering::Equal)),
    }
}

/// Follow a field/index path from a value to the value it points at. An empty
/// path is the identity. Negative indices count from the end.
fn resolve(
    b: &[u8],
    mut s: usize,
    mut e: usize,
    mut k: Kind,
    segs: &[PathSeg],
) -> Option<(usize, usize, Kind)> {
    for seg in segs {
        let rc = match seg {
            PathSeg::Field(key) => {
                if k != Kind::Object {
                    return None;
                }
                let mut cur = Cursor::new(s, e, false);
                loop {
                    let rc = cur.next(b)?;
                    if &rc.label == key {
                        break rc;
                    }
                }
            }
            PathSeg::Index(idx) => {
                if k != Kind::Array {
                    return None;
                }
                let abs = abs_index(b, s, e, *idx)?;
                nth_child(b, s, e, abs)?
            }
        };
        s = rc.start;
        e = rc.end;
        k = rc.kind;
    }
    Some((s, e, k))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &[u8], expr: &str, jsonl: bool) -> Vec<String> {
        let program = parse_pipeline(expr).expect("parse");
        let end = src.len();
        let (start, kind) = if jsonl {
            (0, Kind::Array)
        } else {
            let s = crate::scanner::skip_ws(src, 0, end);
            (s, crate::scanner::value_kind(src, s))
        };
        let source = Arc::new(Source::Buffered(src.to_vec()));
        let mut f = Filter::spawn(source, program, jsonl, start, end, kind);
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
                // Container hits carry a provisional end; the viewer resolves the
                // real closer lazily, so do the same here.
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
        assert_eq!(
            run(b, ".[] | select(.age > 30) | .age", false),
            vec!["40", "60"]
        );
    }

    #[test]
    fn select_string_equality() {
        let b = br#"[{"t":"x"},{"t":"y"},{"t":"x"}]"#;
        let hits = run(b, ".[] | select(.t == \"x\")", false);
        assert_eq!(hits, vec![r#"{"t":"x"}"#, r#"{"t":"x"}"#]);
    }

    #[test]
    fn select_truthy() {
        let b = br#"[{"ok":true,"n":1},{"ok":false,"n":2},{"n":3}]"#;
        assert_eq!(run(b, ".[] | select(.ok) | .n", false), vec!["1"]);
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

    // --- new: recursive descent, slices, negatives, comma, boolean select ----

    #[test]
    fn recursive_descent_visits_every_descendant() {
        let b = br#"{"x":[1,2],"y":{"z":3}}"#;
        // `.. | numbers`-style: keep only the number nodes anywhere in the tree.
        assert_eq!(run(b, ".. | select(. >= 2)", false), vec!["2", "3"]);
    }

    #[test]
    fn recursive_descent_finds_a_key_at_any_depth() {
        let b = br#"{"a":{"id":1},"b":[{"id":2},{"id":3}]}"#;
        assert_eq!(run(b, ".. | .id", false), vec!["1", "2", "3"]);
    }

    #[test]
    fn negative_index_counts_from_the_end() {
        let b = br#"[10,20,30,40]"#;
        assert_eq!(run(b, ".[-1]", false), vec!["40"]);
        assert_eq!(run(b, ".[-2]", false), vec!["30"]);
    }

    #[test]
    fn slices_stream_the_elements_in_range() {
        let b = br#"[10,20,30,40,50]"#;
        assert_eq!(run(b, ".[1:3]", false), vec!["20", "30"]);
        assert_eq!(run(b, ".[:2]", false), vec!["10", "20"]);
        assert_eq!(run(b, ".[3:]", false), vec!["40", "50"]);
        assert_eq!(run(b, ".[-2:]", false), vec!["40", "50"]);
    }

    #[test]
    fn comma_unions_outputs() {
        let b = br#"{"a":1,"b":2,"c":3}"#;
        assert_eq!(run(b, ".a, .c", false), vec!["1", "3"]);
        // comma binds tighter than pipe: (.a, .b) each piped to the next stage
        let d = br#"{"a":{"n":1},"b":{"n":2}}"#;
        assert_eq!(run(d, ".a, .b | .n", false), vec!["1", "2"]);
    }

    #[test]
    fn select_boolean_and_or_and_grouping() {
        let b = br#"[{"age":25,"vip":false},{"age":70,"vip":false},{"age":18,"vip":true}]"#;
        // over 65 OR vip
        assert_eq!(
            run(b, ".[] | select(.age > 65 or .vip) | .age", false),
            vec!["70", "18"]
        );
        // between 20 and 30
        assert_eq!(
            run(b, ".[] | select(.age >= 20 and .age <= 30) | .age", false),
            vec!["25"]
        );
        // grouping: (young or vip) and not-70 — expressed with and/or + parens
        assert_eq!(
            run(b, ".[] | select((.vip or .age < 20)) | .age", false),
            vec!["18"]
        );
    }

    #[test]
    fn select_path_vs_path_comparison() {
        let b = br#"[{"lo":1,"hi":5},{"lo":9,"hi":2}]"#;
        assert_eq!(run(b, ".[] | select(.lo < .hi) | .lo", false), vec!["1"]);
    }

    #[test]
    fn select_missing_is_null() {
        let b = br#"[{"x":1},{"y":2}]"#;
        // The element lacking `x` reads as x==null and is kept.
        assert_eq!(
            run(b, ".[] | select(.x == null)", false),
            vec![r#"{"y":2}"#]
        );
    }

    #[test]
    fn select_regex_match() {
        let b = br#"[{"name":"amy"},{"name":"bob"},{"name":"al"}]"#;
        // `re:` anchors a real regex: names starting with 'a'.
        assert_eq!(
            run(b, r#".[] | select(.name ~ "re:^a") | .name"#, false),
            vec!["\"amy\"", "\"al\""]
        );
    }

    #[test]
    fn select_substring_and_glob_match() {
        let b = br#"[{"f":"report.pdf"},{"f":"notes.txt"},{"f":"scan.pdf"}]"#;
        // Plain text is a case-insensitive substring (same as `/`).
        assert_eq!(
            run(b, r#".[] | select(.f ~ "PDF") | .f"#, false),
            vec!["\"report.pdf\"", "\"scan.pdf\""]
        );
        // `g:` is a shell-style glob, so the literal dot must match.
        assert_eq!(
            run(b, r#".[] | select(.f ~ "g:*.txt") | .f"#, false),
            vec!["\"notes.txt\""]
        );
    }

    #[test]
    fn select_negated_match_and_non_strings() {
        let b = br#"[{"t":"aa"},{"t":"bb"},{"t":7},{"x":1}]"#;
        // `!~` keeps strings that don't match; the number and the missing field
        // are non-strings, so they never match and `!~` keeps them too.
        assert_eq!(
            run(b, r#".[] | select(.t !~ "aa")"#, false),
            vec![r#"{"t":"bb"}"#, r#"{"t":7}"#, r#"{"x":1}"#]
        );
    }

    #[test]
    fn select_match_composes_with_boolean_ops() {
        let b = br#"[{"name":"amy","age":40},{"name":"al","age":10},{"name":"bo","age":50}]"#;
        assert_eq!(
            run(
                b,
                r#".[] | select(.name ~ "re:^a" and .age > 30) | .name"#,
                false
            ),
            vec!["\"amy\""]
        );
    }

    #[test]
    fn parse_errors_are_reported() {
        assert!(parse_pipeline(".foo[").is_err());
        assert!(parse_pipeline("select(.a >)").is_err());
        assert!(parse_pipeline("select(.a and)").is_err());
        assert!(parse_pipeline("select((.a)").is_err());
        // `~` needs a string pattern, and a bad regex is reported at parse time.
        assert!(parse_pipeline("select(.a ~ 3)").is_err());
        assert!(parse_pipeline(r#"select(.a ~ "re:(")"#).is_err());
    }
}
