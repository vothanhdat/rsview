//! JSON → structural type inference for the `t` schema overlay.
//!
//! Instead of a flat table of paths, this samples a value's bytes and *merges*
//! them into one recursive type — the way a "JSON to TypeScript" tool does:
//! object shapes are unified (a key missing from some samples becomes optional),
//! array elements and map values are unified into a single element type, mixed
//! scalars become unions, and a data-keyed object becomes `Record<string, T>`.
//! It's rendered TypeScript-style as a foldable outline (each object block
//! collapses to `{…}`) and is copyable in full. The same type also drives the
//! `T` table view's columns (see [`columns`]), so the grid and the schema can
//! never disagree about a document's shape.
//!
//! Zero-copy: it reads byte ranges through the scanner's `Cursor` and never
//! materializes a value. Bounded by sampling caps and a global node budget, so it
//! stays cheap on huge, deeply-nested data.

use crate::scanner::{Cursor, Kind};
use std::collections::HashMap;

/// Top-level array/map entries sampled.
const TOP_SAMPLE: usize = 2000;
/// Entries sampled per *nested* array/map.
const NESTED_CAP: usize = 128;
/// Keys read per object.
const KEY_CAP: usize = 512;
/// Total values inferred before the walk stops descending (runaway guard).
const NODE_BUDGET: usize = 300_000;
/// Deepest nesting inferred.
const MAX_DEPTH: usize = 24;

// --- map detection (value-shape + key-shape based) -------------------------

/// Values probed to decide whether an object is a data-keyed map.
const MAP_PROBE: usize = 48;
/// Keys sampled per probed object value.
const MAP_KEY_CAP: usize = 32;
/// Object values needed before shape-similarity can call an object a map.
const MAP_MIN_OBJ: usize = 3;
/// Key-set self-similarity (percent) for an object map.
const MAP_SIMILARITY_PCT: usize = 70;
/// Value-kind homogeneity (percent) required for a map.
const MAP_RATIO_PCT: usize = 80;
/// Share (percent) of keys that must be *non-identifier* (data-like) for the
/// key-shape signal to fire — the strong tell that keys are values, not fields.
const MAP_DATAKEY_PCT: usize = 60;

/// The key type of a [`Type::Map`].
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum MapKey {
    Str,
    Num,
}

/// Whether a key looks like a normal field name (`^[A-Za-z_][A-Za-z0-9_]*$`).
/// Numeric, delimited (`DNSE|STOCK`), or dotted keys are *not* identifiers — a
/// strong sign they're data.
fn is_ident_key(k: &str) -> bool {
    let mut cs = k.chars();
    match cs.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether an object at `[start, end)` reads as a data-keyed map (keys are
/// values, values share a shape) rather than a record. Returns the inferred key
/// type — `Num` when every sampled key parses as a number, else `Str`.
///
/// Two triggers, both gated on homogeneous values: **key shape** (most keys
/// aren't identifiers — catches `{8960: …, 8970: …}` even with a few entries) and
/// **object similarity** (object values with alike key-sets). Sheer entry count
/// is deliberately *not* a trigger: a stats record like `{buys_placed: 22464,
/// tick: …, …}` has many homogeneous numeric fields and would be misread as a map.
pub fn looks_like_map(b: &[u8], start: usize, end: usize) -> Option<MapKey> {
    let mut n = 0usize;
    let mut kind_counts = [0usize; 5]; // obj, arr, str, num, bool+null
    let mut n_obj = 0usize;
    let mut n_nonident = 0usize;
    let mut n_numeric = 0usize;
    let mut key_freq: HashMap<String, usize> = HashMap::new();
    let mut c = Cursor::new(start, end, false);
    while let Some(f) = c.next(b) {
        n += 1;
        if !is_ident_key(&f.label) {
            n_nonident += 1;
        }
        if f.label.parse::<f64>().is_ok() {
            n_numeric += 1;
        }
        match f.kind {
            Kind::Object => {
                kind_counts[0] += 1;
                n_obj += 1;
                let mut seen = std::collections::HashSet::new();
                let mut fc = Cursor::new(f.start, f.end, false);
                let mut kc = 0;
                while let Some(sub) = fc.next(b) {
                    if seen.insert(sub.label.clone()) {
                        *key_freq.entry(sub.label).or_insert(0) += 1;
                    }
                    kc += 1;
                    if kc >= MAP_KEY_CAP {
                        break;
                    }
                }
            }
            Kind::Array => kind_counts[1] += 1,
            Kind::Str => kind_counts[2] += 1,
            Kind::Number => kind_counts[3] += 1,
            Kind::Bool | Kind::Null => kind_counts[4] += 1,
        }
        if n >= MAP_PROBE {
            break;
        }
    }
    if n == 0 {
        return None;
    }
    // Values must be homogeneous in kind — a record's values are typically mixed.
    let dominant = kind_counts.iter().copied().max().unwrap_or(0);
    if dominant * 100 < n * MAP_RATIO_PCT {
        return None;
    }
    // Object similarity: object values whose key-sets look alike. Score
    // (Σf²/Σf)/n_obj ∈ [1/n_obj, 1] — 1 when every value shares every key.
    let obj_similar = n_obj >= MAP_MIN_OBJ && n_obj * 100 >= n * MAP_RATIO_PCT && {
        let sum_f: usize = key_freq.values().sum();
        let sum_f2: usize = key_freq.values().map(|f| f * f).sum();
        sum_f > 0 && sum_f2 * 100 >= sum_f * n_obj * MAP_SIMILARITY_PCT
    };
    // Key shape: most keys aren't identifiers → they're data.
    let data_keys = n_nonident * 100 >= n * MAP_DATAKEY_PCT;
    if obj_similar || data_keys {
        Some(if n_numeric == n {
            MapKey::Num
        } else {
            MapKey::Str
        })
    } else {
        None
    }
}

// --- the inferred type -----------------------------------------------------

/// A structural type inferred from sampled JSON.
#[derive(Debug, PartialEq)]
pub enum Type {
    /// Nothing observed, or the node budget ran out.
    Any,
    Str,
    Num,
    Bool,
    Null,
    Array(Box<Type>),
    /// A record: named fields, each possibly optional.
    Object(Vec<Field>),
    /// A data-keyed map — `Record<K, T>`, where `K` is `string` or `number`.
    Map(MapKey, Box<Type>),
    /// Mixed shapes seen at the same position.
    Union(Vec<Type>),
}

/// One field of an [`Type::Object`], with how often it appeared (for optionality
/// and a fill-rate comment).
#[derive(Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: Type,
    pub present: usize,
    pub total: usize,
}

impl Field {
    pub fn optional(&self) -> bool {
        self.present < self.total
    }
}

// --- inference (accumulate then finish) ------------------------------------

struct Budget(usize);
impl Budget {
    fn take(&mut self) -> bool {
        if self.0 == 0 {
            return false;
        }
        self.0 -= 1;
        true
    }
}

/// Accumulates every value seen at one position, then collapses to a [`Type`].
#[derive(Default)]
struct Acc {
    str: bool,
    num: bool,
    boo: bool,
    null: bool,
    // record variant
    rec_count: usize,
    order: Vec<String>,
    fields: HashMap<String, FieldAcc>,
    // map variant
    map_count: usize,
    map_val: Option<Box<Acc>>,
    /// Set once a map occurrence with non-numeric keys is seen (so the merged
    /// key type is `number` only when *every* occurrence had numeric keys).
    map_key_str: bool,
    // array variant
    arr_count: usize,
    arr_elem: Option<Box<Acc>>,
}

#[derive(Default)]
struct FieldAcc {
    present: usize,
    acc: Acc,
}

impl Acc {
    /// Fold one value into the accumulator.
    fn add(
        &mut self,
        b: &[u8],
        start: usize,
        end: usize,
        kind: Kind,
        depth: usize,
        bud: &mut Budget,
    ) {
        if depth > MAX_DEPTH || !bud.take() {
            return;
        }
        let lim = if depth == 0 { TOP_SAMPLE } else { NESTED_CAP };
        match kind {
            Kind::Str => self.str = true,
            Kind::Number => self.num = true,
            Kind::Bool => self.boo = true,
            Kind::Null => self.null = true,
            Kind::Array => {
                self.arr_count += 1;
                let el = self.arr_elem.get_or_insert_with(Box::default);
                let mut c = Cursor::new(start, end, true);
                let mut i = 0;
                while let Some(e) = c.next(b) {
                    i += 1;
                    if i > lim {
                        break;
                    }
                    el.add(b, e.start, e.end, e.kind, depth + 1, bud);
                }
            }
            Kind::Object => {
                if let Some(key) = looks_like_map(b, start, end) {
                    // A data-keyed map: merge its *values* into one element type.
                    self.map_count += 1;
                    if key == MapKey::Str {
                        self.map_key_str = true;
                    }
                    let mv = self.map_val.get_or_insert_with(Box::default);
                    let mut c = Cursor::new(start, end, false);
                    let mut i = 0;
                    while let Some(v) = c.next(b) {
                        i += 1;
                        if i > lim {
                            break;
                        }
                        mv.add(b, v.start, v.end, v.kind, depth + 1, bud);
                    }
                } else {
                    // A record: merge each key's value type across occurrences.
                    self.rec_count += 1;
                    let mut c = Cursor::new(start, end, false);
                    let mut i = 0;
                    while let Some(f) = c.next(b) {
                        i += 1;
                        if i > KEY_CAP {
                            break;
                        }
                        let fa = match self.fields.get_mut(&f.label) {
                            Some(fa) => fa,
                            None => {
                                self.order.push(f.label.clone());
                                self.fields.entry(f.label.clone()).or_default()
                            }
                        };
                        fa.present += 1;
                        fa.acc.add(b, f.start, f.end, f.kind, depth + 1, bud);
                    }
                }
            }
        }
    }

    /// Collapse the accumulated observations into a single type.
    fn finish(mut self) -> Type {
        let mut variants: Vec<Type> = Vec::new();
        if self.rec_count > 0 {
            let total = self.rec_count;
            let order = std::mem::take(&mut self.order);
            let fields = order
                .into_iter()
                .filter_map(|k| {
                    self.fields.remove(&k).map(|fa| Field {
                        name: k,
                        ty: fa.acc.finish(),
                        present: fa.present,
                        total,
                    })
                })
                .collect();
            variants.push(Type::Object(fields));
        }
        if self.arr_count > 0 {
            let el = self.arr_elem.map_or(Type::Any, |a| a.finish());
            variants.push(Type::Array(Box::new(el)));
        }
        if self.map_count > 0 {
            let key = if self.map_key_str {
                MapKey::Str
            } else {
                MapKey::Num
            };
            let v = self.map_val.map_or(Type::Any, |a| a.finish());
            variants.push(Type::Map(key, Box::new(v)));
        }
        if self.str {
            variants.push(Type::Str);
        }
        if self.num {
            variants.push(Type::Num);
        }
        if self.boo {
            variants.push(Type::Bool);
        }
        if self.null {
            variants.push(Type::Null);
        }
        match variants.len() {
            0 => Type::Any,
            1 => variants.pop().unwrap(),
            _ => Type::Union(variants),
        }
    }
}

/// Infer the structural type of the value at `[start, end)`. `jsonl` treats the
/// range as a stream of documents (an array of them).
pub fn infer(b: &[u8], start: usize, end: usize, kind: Kind, jsonl: bool) -> Type {
    let mut bud = Budget(NODE_BUDGET);
    let mut acc = Acc::default();
    if jsonl {
        let mut el = Acc::default();
        let mut c = Cursor::lines(start, end);
        let mut i = 0;
        while let Some(d) = c.next(b) {
            i += 1;
            if i > TOP_SAMPLE {
                break;
            }
            el.add(b, d.start, d.end, d.kind, 1, &mut bud);
        }
        return Type::Array(Box::new(el.finish()));
    }
    acc.add(b, start, end, kind, 0, &mut bud);
    acc.finish()
}

/// Infer the type of a *synthetic* sequence — values that belong together but
/// aren't contiguous in the file, like a filter pane's hits. Merged the same way
/// an array's elements are, and reported as one: `T[]`.
pub fn infer_seq(b: &[u8], items: impl Iterator<Item = (usize, usize, Kind)>) -> Type {
    let mut bud = Budget(NODE_BUDGET);
    let mut el = Acc::default();
    for (i, (s, e, k)) in items.enumerate() {
        if i >= TOP_SAMPLE {
            break;
        }
        el.add(b, s, e, k, 1, &mut bud);
    }
    Type::Array(Box::new(el.finish()))
}

// --- rendering (TypeScript-ish) --------------------------------------------

/// The scalar keyword for a leaf type, or `None` for a composite.
fn word(ty: &Type) -> Option<&'static str> {
    match ty {
        Type::Any => Some("any"),
        Type::Str => Some("string"),
        Type::Num => Some("number"),
        Type::Bool => Some("boolean"),
        Type::Null => Some("null"),
        _ => None,
    }
}

/// One node of the rendered outline. A node with `children` is a foldable block
/// — an object body: its opening line (`user: {`), the child nodes, then
/// `close_text` (`}` plus any `[]`/`Record<>` suffix that trails the block).
/// Folded, the whole block collapses onto `folded_text` (`user: {…}  // 3 fields`).
/// A leaf carries the same text in both, and an empty `close_text`.
#[derive(Debug, PartialEq)]
pub struct Outline {
    pub open_text: String,
    pub folded_text: String,
    pub close_text: String,
    /// Nesting level, matching the two-space indent already baked into the texts.
    pub depth: usize,
    pub children: Vec<Outline>,
    /// Fold state, flipped by the overlay. Everything starts open.
    pub open: bool,
}

impl Outline {
    fn leaf(text: String, depth: usize) -> Self {
        Outline {
            open_text: text.clone(),
            folded_text: text,
            close_text: String::new(),
            depth,
            children: Vec::new(),
            open: true,
        }
    }

    pub fn foldable(&self) -> bool {
        !self.children.is_empty()
    }
}

/// Build the type's outline. `note` is appended to the *first* line (used for a
/// field's `// %` fill comment).
fn write_type(
    ty: &Type,
    indent: usize,
    prefix: &str,
    suffix: &str,
    note: &str,
    out: &mut Vec<Outline>,
) {
    let pad = "  ".repeat(indent);
    if let Some(w) = word(ty) {
        out.push(Outline::leaf(
            format!("{pad}{prefix}{w}{suffix}{note}"),
            indent,
        ));
        return;
    }
    match ty {
        Type::Array(el) => {
            let sfx = format!("[]{suffix}");
            write_type(el, indent, prefix, &sfx, note, out);
        }
        Type::Map(key, v) => {
            let kw = match key {
                MapKey::Str => "string",
                MapKey::Num => "number",
            };
            let pfx = format!("{prefix}Record<{kw}, ");
            let sfx = format!(">{suffix}");
            write_type(v, indent, &pfx, &sfx, note, out);
        }
        Type::Object(fields) => {
            if fields.is_empty() {
                out.push(Outline::leaf(
                    format!("{pad}{prefix}{{}}{suffix}{note}"),
                    indent,
                ));
                return;
            }
            let mut children = Vec::new();
            for f in fields {
                let opt = if f.optional() { "?" } else { "" };
                let fnote = if f.optional() && f.total > 0 {
                    format!("   // {}%", f.present * 100 / f.total)
                } else {
                    String::new()
                };
                let fpfx = format!("{}{opt}: ", f.name);
                write_type(&f.ty, indent + 1, &fpfx, "", &fnote, &mut children);
            }
            // Folded, the field count stands in for the hidden body — appended to
            // the fill-rate comment when there already is one.
            let n = fields.len();
            let count = if n == 1 {
                "1 field".to_string()
            } else {
                format!("{n} fields")
            };
            let folded_note = if note.is_empty() {
                format!("   // {count}")
            } else {
                format!("{note} · {count}")
            };
            out.push(Outline {
                open_text: format!("{pad}{prefix}{{{note}"),
                folded_text: format!("{pad}{prefix}{{…}}{suffix}{folded_note}"),
                close_text: format!("{pad}}}{suffix}"),
                depth: indent,
                children,
                open: true,
            });
        }
        Type::Union(vs) => write_union(vs, indent, prefix, suffix, note, out),
        _ => {}
    }
}

/// Render a union: scalars inline (`a | b`), a nullable/optional container as
/// `{…} | null`. Falls back to `| …` when several distinct composites collide
/// (rare) rather than exploding.
fn write_union(
    vs: &[Type],
    indent: usize,
    prefix: &str,
    suffix: &str,
    note: &str,
    out: &mut Vec<Outline>,
) {
    let scalars: Vec<&str> = vs.iter().filter_map(word).collect();
    let composites: Vec<&Type> = vs.iter().filter(|v| word(v).is_none()).collect();
    if composites.is_empty() {
        let joined = scalars.join(" | ");
        let pad = "  ".repeat(indent);
        out.push(Outline::leaf(
            format!("{pad}{prefix}{joined}{suffix}{note}"),
            indent,
        ));
        return;
    }
    let mut tail = String::new();
    if !scalars.is_empty() {
        tail.push_str(&format!(" | {}", scalars.join(" | ")));
    }
    if composites.len() > 1 {
        tail.push_str(" | …");
    }
    let sfx = format!("{tail}{suffix}");
    write_type(composites[0], indent, prefix, &sfx, note, out);
}

/// The type as a foldable outline for the overlay — every block open.
pub fn outline(ty: &Type) -> Vec<Outline> {
    let mut out = Vec::new();
    write_type(ty, 0, "", "", "", &mut out);
    if out.is_empty() {
        out.push(Outline::leaf("any".into(), 0));
    }
    out
}

fn push_lines(o: &Outline, out: &mut Vec<String>) {
    out.push(o.open_text.clone());
    if o.foldable() {
        for c in &o.children {
            push_lines(c, out);
        }
        out.push(o.close_text.clone());
    }
}

/// The type as display lines, fully expanded regardless of fold state.
pub fn render(ty: &Type) -> Vec<String> {
    let mut out = Vec::new();
    for o in outline(ty) {
        push_lines(&o, &mut out);
    }
    out
}

/// The type as copyable source (what `y` yanks) — always the whole type, so a
/// folded overlay still copies everything.
pub fn to_source(ty: &Type) -> String {
    render(ty).join("\n")
}

/// One visible line of an [`Outline`], as drawn by the overlay.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub text: String,
    pub depth: usize,
    /// Index path into the outline tree. A block's closing line repeats its
    /// opening line's path, so folding works from either end of the block.
    pub path: Vec<usize>,
    /// True on the opening (or folded) line of a foldable block.
    pub foldable: bool,
    /// Whether that block is currently open.
    pub open: bool,
    /// True on a block's closing line.
    pub close: bool,
}

fn walk(nodes: &[Outline], path: &mut Vec<usize>, out: &mut Vec<Row>) {
    for (i, o) in nodes.iter().enumerate() {
        path.push(i);
        let foldable = o.foldable();
        out.push(Row {
            text: if foldable && !o.open {
                o.folded_text.clone()
            } else {
                o.open_text.clone()
            },
            depth: o.depth,
            path: path.clone(),
            foldable,
            open: o.open,
            close: false,
        });
        if foldable && o.open {
            walk(&o.children, path, out);
            out.push(Row {
                text: o.close_text.clone(),
                depth: o.depth,
                path: path.clone(),
                foldable: false,
                open: true,
                close: true,
            });
        }
        path.pop();
    }
}

/// The outline's currently visible lines, top to bottom.
pub fn rows(nodes: &[Outline]) -> Vec<Row> {
    let mut out = Vec::new();
    walk(nodes, &mut Vec::new(), &mut out);
    out
}

/// The node a [`Row::path`] points at.
pub fn node_mut<'a>(nodes: &'a mut [Outline], path: &[usize]) -> Option<&'a mut Outline> {
    let (&i, rest) = path.split_first()?;
    let n = nodes.get_mut(i)?;
    if rest.is_empty() {
        Some(n)
    } else {
        node_mut(&mut n.children, rest)
    }
}

/// Open or fold every block. Folding leaves the outermost block open — a lone
/// `{…}` would say nothing about the type.
pub fn set_all_open(nodes: &mut [Outline], open: bool) {
    for n in nodes.iter_mut() {
        n.open = open || n.depth == 0;
        set_all_open(&mut n.children, open);
    }
}

// --- table columns ---------------------------------------------------------

/// Deepest object nesting flattened into dotted columns (`user.city`). A field
/// below that shows as a `{…}` cell instead — a table is a grid, not a tree.
const COL_DEPTH: usize = 2;
/// Columns present in fewer than this share of sampled rows are dropped: a key
/// seen twice in 5000 rows is noise, not a column.
const MIN_FILL_PCT: usize = 5;
/// Hard cap on columns, so a pathological record can't build a 4000-wide grid.
const MAX_COLS: usize = 64;

/// One column of the table view: where its cell lives inside a row, and what to
/// call it. Derived from the same inferred [`Type`] the `t` overlay renders, so
/// the table's shape and the schema's never disagree.
#[derive(Debug, PartialEq, Clone)]
pub struct Column {
    /// Field path within a row (`["user","city"]`). Empty when the row *is* the
    /// value (a sequence of scalars).
    pub path: Vec<String>,
    /// Header text — `path` joined with dots, or `value` for a bare sequence.
    pub label: String,
    /// True when every value observed here was a number, so cells right-align.
    pub num: bool,
    /// Percent of sampled rows carrying this field; 100 = always present.
    pub fill: usize,
}

/// The element type of a sequence — what one table row looks like.
fn element_of(ty: &Type) -> Option<&Type> {
    match ty {
        Type::Array(el) => Some(el),
        Type::Map(_, v) => Some(v),
        Type::Union(vs) => vs.iter().find_map(element_of),
        _ => None,
    }
}

/// The record fields of a type, looking through a union (rows of mixed shapes
/// tabulate on the object arm; the others simply leave their cells blank).
fn record_of(ty: &Type) -> Option<&Vec<Field>> {
    match ty {
        Type::Object(f) if !f.is_empty() => Some(f),
        Type::Union(vs) => vs.iter().find_map(record_of),
        _ => None,
    }
}

/// Flatten a record's fields into columns, descending into nested records up to
/// [`COL_DEPTH`]. `fill` is the enclosing field's fill rate, so a nested column's
/// rate compounds (`user` in 50% of rows × `city` in 50% of those = 25%).
fn push_columns(
    fields: &[Field],
    prefix: &mut Vec<String>,
    fill: usize,
    depth: usize,
    out: &mut Vec<Column>,
) {
    for f in fields {
        let own = if f.total == 0 {
            100
        } else {
            f.present * 100 / f.total
        };
        let fill = fill * own / 100;
        prefix.push(f.name.clone());
        match record_of(&f.ty) {
            Some(sub) if depth + 1 < COL_DEPTH => push_columns(sub, prefix, fill, depth + 1, out),
            _ => out.push(Column {
                label: prefix.join("."),
                path: prefix.clone(),
                num: matches!(f.ty, Type::Num),
                fill,
            }),
        }
        prefix.pop();
    }
}

/// Columns for tabulating a value of type `ty`, or `None` when it isn't a
/// sequence — a lone record has nothing to repeat down the page.
///
/// Also returns how many columns the type *has*, before sparse ones are dropped
/// and the rest capped, so the view can say what it isn't showing.
pub fn columns(ty: &Type) -> Option<(Vec<Column>, usize)> {
    let el = element_of(ty)?;
    let mut cols = Vec::new();
    match record_of(el) {
        Some(fields) => push_columns(fields, &mut Vec::new(), 100, 0, &mut cols),
        // A sequence of scalars (or of arrays) is still a table — one column of
        // values, which beats no table at all.
        None => cols.push(Column {
            path: Vec::new(),
            label: "value".into(),
            num: matches!(el, Type::Num),
            fill: 100,
        }),
    }
    let total = cols.len();
    // Drop the near-empty columns — unless that's all of them, in which case a
    // sparse grid still beats an empty one.
    if cols.iter().any(|c| c.fill >= MIN_FILL_PCT) {
        cols.retain(|c| c.fill >= MIN_FILL_PCT);
    }
    cols.truncate(MAX_COLS);
    Some((cols, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(json: &str) -> String {
        let b = json.as_bytes();
        let ty = infer(b, 0, b.len(), crate::scanner::value_kind(b, 0), false);
        to_source(&ty)
    }

    #[test]
    fn merges_object_records_with_optionals() {
        // `b` is missing from the first element → optional; `a` is always present.
        let t = ts(r#"[{"a":1},{"a":2,"b":"x"}]"#);
        assert!(t.contains("a: number"), "{t}");
        assert!(t.contains("b?: string"), "{t}");
        assert!(t.trim_end().ends_with("}[]"), "{t}");
    }

    #[test]
    fn data_keyed_object_is_a_record_map() {
        let t = ts(r#"{"AAA":{"v":1},"BBB":{"v":2},"CCC":{"v":3},"DDD":{"v":4}}"#);
        assert!(t.contains("Record<string, {"), "{t}");
        assert!(t.contains("v: number"), "{t}");
        assert!(!t.contains("AAA"), "keys must not leak: {t}");
    }

    #[test]
    fn nested_map_inside_record() {
        let t = ts(r#"{"owner":"x","positions":{"S1":{"qty":1},"S2":{"qty":2},"S3":{"qty":3}}}"#);
        assert!(t.contains("owner: string"), "{t}");
        assert!(t.contains("positions: Record<string, {"), "{t}");
        assert!(t.contains("qty: number"), "{t}");
    }

    #[test]
    fn mixed_scalars_form_a_union() {
        let t = ts(r#"[1,"x",2,"y"]"#);
        assert!(
            t.contains("number") && t.contains("string") && t.contains('|'),
            "{t}"
        );
    }

    #[test]
    fn disjoint_object_fields_stay_a_record() {
        // address/geo have disjoint keys → a record, not a map.
        let t = ts(r#"{"address":{"street":"x"},"geo":{"lat":1}}"#);
        assert!(t.contains("address: {"), "{t}");
        assert!(!t.contains("Record<"), "should not be a map: {t}");
    }

    #[test]
    fn numeric_keyed_object_is_a_record_number_map() {
        // Numeric keys (a few of them, array values) → Record<number, number[]>.
        let t = ts(r#"{"8960":[2,18],"8970":[2,0],"9000":[1,0],"9120":[10,0]}"#);
        assert_eq!(t, "Record<number, number[]>", "{t}");
    }

    #[test]
    fn numeric_keyed_maps_merge_across_records_instead_of_exploding() {
        // Each record's `lv` map has *different* numeric keys; merged they must
        // collapse to one Record<number, …>, not a union of every key.
        let t = ts(r#"[{"lv":{"10":[1],"20":[2]}},{"lv":{"30":[3],"40":[4]}},{"lv":{"50":[5]}}]"#);
        assert!(t.contains("lv: Record<number, number[]>"), "{t}");
        assert!(!t.contains("10?"), "keys must not explode into fields: {t}");
    }

    #[test]
    fn identifier_keyed_small_object_stays_a_record() {
        // Few entries, identifier keys → a record even though values are uniform.
        let t = ts(r#"{"x":[1],"y":[2],"z":[3]}"#);
        assert!(t.contains("x: number[]"), "{t}");
        assert!(!t.contains("Record<"), "should not be a map: {t}");
    }

    fn ty_of(json: &str) -> Type {
        let b = json.as_bytes();
        infer(b, 0, b.len(), crate::scanner::value_kind(b, 0), false)
    }

    #[test]
    fn outline_starts_fully_open_and_matches_the_flat_render() {
        let t = ty_of(r#"[{"a":1,"user":{"name":"x"}}]"#);
        let o = outline(&t);
        let texts: Vec<String> = rows(&o).into_iter().map(|r| r.text).collect();
        assert_eq!(texts, render(&t), "open outline == the flat render");
    }

    #[test]
    fn folding_a_block_hides_its_children_and_closing_line() {
        let t = ty_of(r#"[{"a":1,"user":{"name":"x","age":2}}]"#);
        let mut o = outline(&t);
        let open = rows(&o);
        // The nested object under `user`.
        let path = open
            .iter()
            .find(|r| r.text.contains("user: {"))
            .expect("a user block")
            .path
            .clone();
        assert!(open.iter().any(|r| r.text.contains("name: string")));

        node_mut(&mut o, &path).unwrap().open = false;
        let folded = rows(&o);
        assert!(folded.len() < open.len(), "folding must drop lines");
        assert!(
            !folded.iter().any(|r| r.text.contains("name: string")),
            "children stay hidden: {folded:?}"
        );
        let line = folded
            .iter()
            .find(|r| r.path == path)
            .expect("the folded line");
        assert!(line.text.contains("user: {…}"), "{}", line.text);
        assert!(line.text.contains("2 fields"), "{}", line.text);
        assert!(line.foldable && !line.open);
    }

    #[test]
    fn folding_keeps_the_type_suffix_on_the_folded_line() {
        // Open, `[]` trails the closing brace; folded, it must trail `{…}`.
        let t = ty_of(r#"[{"a":1}]"#);
        let mut o = outline(&t);
        set_all_open(&mut o, false);
        // The outermost block stays open — folding it away would show nothing.
        assert!(rows(&o).iter().any(|r| r.text.contains("a: number")));
        o[0].open = false;
        let r = rows(&o);
        assert_eq!(r.len(), 1);
        assert!(r[0].text.starts_with("{…}[]"), "{}", r[0].text);
    }

    #[test]
    fn a_closing_line_carries_its_blocks_path() {
        let t = ty_of(r#"{"a":{"b":1}}"#);
        let o = outline(&t);
        let r = rows(&o);
        let close = r.last().unwrap();
        assert!(close.close && close.text == "}");
        assert_eq!(close.path, r[0].path, "closes the block it ends");
    }

    #[test]
    fn collapse_all_folds_nested_blocks_but_keeps_the_root_open() {
        let t = ty_of(r#"{"a":{"b":{"c":1}},"d":2}"#);
        let mut o = outline(&t);
        set_all_open(&mut o, false);
        let r = rows(&o);
        assert!(r.iter().any(|x| x.text.contains("d: number")), "{r:?}");
        assert!(r.iter().any(|x| x.text.contains("a: {…}")), "{r:?}");
        assert!(!r.iter().any(|x| x.text.contains("c: number")), "{r:?}");

        set_all_open(&mut o, true);
        assert_eq!(
            rows(&o).into_iter().map(|x| x.text).collect::<Vec<_>>(),
            render(&t),
            "expand-all restores every line"
        );
    }

    #[test]
    fn scalars_and_empty_objects_are_not_foldable() {
        let t = ty_of(r#"{"a":1,"e":{}}"#);
        let r = rows(&outline(&t));
        for line in r.iter().filter(|x| x.depth > 0) {
            assert!(!line.foldable, "{}", line.text);
        }
    }

    fn cols_of(json: &str) -> (Vec<String>, usize) {
        let (cols, total) = columns(&ty_of(json)).expect("tabular");
        (cols.into_iter().map(|c| c.label).collect(), total)
    }

    #[test]
    fn a_lone_record_has_no_columns() {
        assert!(columns(&ty_of(r#"{"a":1,"b":2}"#)).is_none());
        assert!(columns(&ty_of(r#"42"#)).is_none());
    }

    #[test]
    fn sparse_columns_are_dropped_and_counted() {
        // `rare` shows up in 1 of 100 rows — below the fill floor.
        let rows: Vec<String> = (0..100)
            .map(|i| {
                if i == 0 {
                    r#"{"a":1,"rare":2}"#.to_string()
                } else {
                    r#"{"a":1}"#.to_string()
                }
            })
            .collect();
        let (labels, total) = cols_of(&format!("[{}]", rows.join(",")));
        assert_eq!(labels, ["a"]);
        assert_eq!(total, 2, "the dropped column is still counted");
    }

    #[test]
    fn a_nested_columns_fill_rate_compounds_with_its_parents() {
        // `u` in half the rows, `city` in half of those → 25%.
        let (cols, _) =
            columns(&ty_of(r#"[{"u":{"city":"NY"}},{"u":{}},{"x":1},{"x":1}]"#)).unwrap();
        let city = cols.iter().find(|c| c.label == "u.city").expect("u.city");
        assert_eq!(city.fill, 25);
    }

    #[test]
    fn rows_of_mixed_shapes_tabulate_on_the_record_arm() {
        let (labels, _) = cols_of(r#"[{"a":1},{"a":2},"loose string"]"#);
        assert_eq!(labels, ["a"]);
    }

    #[test]
    fn a_synthetic_sequence_infers_like_an_array() {
        // Two objects that aren't siblings in the file (a filter pane's hits).
        let doc = r#"{"x":{"a":1},"y":{"a":2,"b":"s"}}"#;
        let b = doc.as_bytes();
        let items = [
            (doc.find(r#"{"a":1}"#).unwrap(), doc.len(), Kind::Object),
            (doc.find(r#"{"a":2"#).unwrap(), doc.len(), Kind::Object),
        ];
        let t = to_source(&infer_seq(b, items.into_iter()));
        assert!(t.contains("a: number"), "{t}");
        assert!(t.contains("b?: string"), "{t}");
        assert!(t.trim_end().ends_with("}[]"), "{t}");
    }

    #[test]
    fn stats_record_with_many_numeric_fields_stays_a_record() {
        // A counters/stats object: many identifier keys, mostly numbers, one nested
        // map. It must stay a record (the nested `dict` becomes a Record<>).
        let t = ts(
            r#"{"buys_placed":22464,"cold_book_skips":75173,"dict":{"AAA":{"v":1},"AAM":{"v":2},"AAT":{"v":3}},"last_update":1784169321396,"sells_placed":177,"signals_blocked":12707811,"thin_book_skips":139091,"tick":19481645}"#,
        );
        assert!(t.starts_with('{'), "top must be a record, not a map: {t}");
        assert!(t.contains("buys_placed: number"), "{t}");
        assert!(t.contains("dict: Record<string, {"), "{t}");
    }
}
