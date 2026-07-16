//! JSON → structural type inference for the `t` schema overlay.
//!
//! Instead of a flat table of paths, this samples a value's bytes and *merges*
//! them into one recursive type — the way a "JSON to TypeScript" tool does:
//! object shapes are unified (a key missing from some samples becomes optional),
//! array elements and map values are unified into a single element type, mixed
//! scalars become unions, and a data-keyed object becomes `Record<string, T>`.
//! It's rendered TypeScript-style and is copyable.
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

/// Render the type as indented lines. `note` is appended to the *first* line
/// (used for a field's `// %` fill comment).
fn write_type(
    ty: &Type,
    indent: usize,
    prefix: &str,
    suffix: &str,
    note: &str,
    out: &mut Vec<String>,
) {
    let pad = "  ".repeat(indent);
    if let Some(w) = word(ty) {
        out.push(format!("{pad}{prefix}{w}{suffix}{note}"));
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
                out.push(format!("{pad}{prefix}{{}}{suffix}{note}"));
                return;
            }
            out.push(format!("{pad}{prefix}{{{note}"));
            for f in fields {
                let opt = if f.optional() { "?" } else { "" };
                let fnote = if f.optional() && f.total > 0 {
                    format!("   // {}%", f.present * 100 / f.total)
                } else {
                    String::new()
                };
                let fpfx = format!("{}{opt}: ", f.name);
                write_type(&f.ty, indent + 1, &fpfx, "", &fnote, out);
            }
            out.push(format!("{pad}}}{suffix}"));
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
    out: &mut Vec<String>,
) {
    let scalars: Vec<&str> = vs.iter().filter_map(word).collect();
    let composites: Vec<&Type> = vs.iter().filter(|v| word(v).is_none()).collect();
    if composites.is_empty() {
        let joined = scalars.join(" | ");
        let pad = "  ".repeat(indent);
        out.push(format!("{pad}{prefix}{joined}{suffix}{note}"));
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

/// The type as scrollable display lines for the overlay.
pub fn render(ty: &Type) -> Vec<String> {
    let mut out = Vec::new();
    write_type(ty, 0, "", "", "", &mut out);
    if out.is_empty() {
        out.push("any".into());
    }
    out
}

/// The type as copyable source (what `y` yanks).
pub fn to_source(ty: &Type) -> String {
    render(ty).join("\n")
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
