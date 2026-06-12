# rsview — Rust proof-of-concept

A native port of [react-obj-view](https://github.com/vothanhdat/react-obj-view)'s CLI core: memory-map a JSON file, parse
**on expand** (subtrees are byte ranges, never materialized), flatten a level
only as far as the viewport scrolls, and search on a background thread. Opening a
multi-GB file stays near-constant memory. Rows are syntax-colored, and collapsed
containers show an inline preview of their first few children.

This is a **proof-of-concept**, not the product: a single file argument
(`.jsonl`/`.ndjson` shown as an array of documents; no stdin streaming yet), no
themes, no copy. Open / navigate / expand / search.

## Install

**Prebuilt binary (no compile)** — Linux (x86_64/arm64), macOS (Intel/Apple
Silicon), and Windows binaries are attached to each
[GitHub Release](https://github.com/vothanhdat/rsview/releases). Download and
drop on your `PATH`, or let [cargo-binstall](https://github.com/cargo-bins/cargo-binstall)
fetch the right one:

```sh
cargo binstall rsview
```

**From crates.io (compiles from source):**

```sh
cargo install rsview
```

Either way you get an `rsview` on your `PATH`. Then: `rsview path/to/file.json`.

## Build & run (from a checkout)

```sh
cargo build --release
./target/release/rsview path/to/file.json
# or
cargo run --release -- path/to/file.json
```

## Keys

| Key | Action |
| --- | --- |
| `↑`/`↓`, `k`/`j` | move focus |
| `PageUp`/`PageDown` | jump a screenful |
| `g`, `Home` | top |
| `Enter`/`→`/`Space` | expand / collapse focused node |
| `←` | collapse, or jump to parent if already collapsed |
| `/` | search (live — results stream as you type) |
| `Enter` (in search) | jump to first match |
| `Esc` (in search) | cancel search |
| `n` / `N` | next / previous match |
| `q`, `Esc` | quit |

Top line is `filename   <focus>/<rows>+` — the `+` means the row count is a
**lower bound**: the level has only been flattened as far as you've scrolled.
Matches render yellow; the current match is brighter.

## Why it's fast (and stays small)

- **mmap, not read.** The file is mapped (`memmap2`), so opening it copies
  nothing — the kernel pages in 4 KB chunks only when a byte is actually touched.
  Browsing a 1 GB file sits around **2.6 MB RSS**.
- **Scan bytes, don't parse.** [scanner.rs](src/scanner.rs) walks raw `&[u8]` to
  find a container's child byte-ranges. Structural tokens (`{ } [ ] " : ,`) are
  ASCII; every byte of a multi-byte UTF-8 sequence is ≥ 0x80, so the scan never
  collides with them and never decodes. A value is decoded (`from_utf8`) only
  when it's drawn — and only that slice.
- **Parse on expand, incrementally.** A collapsed node costs O(1). Expanding uses
  a resumable `Cursor` that scans **one more child per call**, so a level with
  millions of keys only enumerates ~a screenful (`flatten` stops at a row
  `budget`). First paint is ~constant regardless of file size.
- **Search on a real thread.** [search.rs](src/search.rs) scans the mmap on its
  own OS thread and streams match paths over an `mpsc` channel; an `AtomicBool`
  the thread polls is the cancel. Retyping drops the old search instantly. The
  UI never blocks because the scan isn't on the UI's loop. (A full scan does
  fault in the pages it reads — evictable, file-backed page cache — so search
  trades the near-zero-memory property for the bytes it must touch.)

## Layout

| File | Role |
| --- | --- |
| [src/scanner.rs](src/scanner.rs) | byte-range JSON scan + resumable child `Cursor` |
| [src/main.rs](src/main.rs) | lazy `Node` tree, windowed flatten, ratatui viewer |
| [src/search.rs](src/search.rs) | background-thread search + cancel + result stream |
