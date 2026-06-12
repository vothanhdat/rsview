# rsview — Rust proof-of-concept

A native port of [react-obj-view](https://github.com/vothanhdat/react-obj-view)'s CLI core: memory-map a JSON file, parse
**on expand** (subtrees are byte ranges, never materialized), flatten a level
only as far as the viewport scrolls, and search on a background thread. Opening a
multi-GB file stays near-constant memory. Rows are syntax-colored, and collapsed
containers show an inline preview of their first few children.

This is a **proof-of-concept**, not the product: a single file argument or piped
stdin (`.jsonl`/`.ndjson` shown as an array of documents), no themes, no copy.
Open / navigate / expand / search. Piped input **streams** — it renders
progressively as bytes arrive, so `curl … | rsview` shows the document *before
it's complete*, and your cursor and expanded nodes stay put as new data fills in.
(A stream can't be memory-mapped, so it's buffered in RAM and re-parsed on a
throttle; the near-constant-memory property is the file path's.) Keys come from
`/dev/tty` while JSON arrives on stdin.

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

Either way you get an `rsview` on your `PATH`. Then:

```sh
rsview path/to/file.json
cat path/to/file.json | rsview                       # pipe it (NDJSON auto-detected)
curl -s https://api.example.com/big.json | rsview    # streams as it downloads
```

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
| `PageUp`/`PageDown`, `Ctrl-F`/`Ctrl-B` | jump a screenful |
| `Ctrl-D`/`Ctrl-U` | jump half a screenful |
| `g`, `Home` | top |
| `Enter`/`→`/`Space` | expand / collapse focused node |
| `←` | collapse, or jump to parent if already collapsed |
| `/` | search (live — results stream as you type) |
| `Enter` / `↓` (in search) | next match |
| `Shift-Enter` / `↑` (in search) | previous match |
| `Esc` (in search) | close search (keeps cursor on the match) |
| `q`, `Esc` | quit |

The search box stays open so you can cycle matches in place, then `Esc` to
explore the tree at the match. `Shift-Enter` needs a terminal that speaks the
[Kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
(Kitty, WezTerm, foot, Ghostty, recent iTerm2/Konsole/VTE); elsewhere use `↑`.

Top line is `filename   <focus>/<rows>+   <breadcrumb>` — the `+` means the row
count is a **lower bound**: the level has only been flattened as far as you've
scrolled. The breadcrumb is the path to the focused node (`data › users › [1] ›
city`, array elements bracketed); it left-truncates with a leading `…` on deep
paths so the tail nearest your cursor stays visible. Matches render yellow; the
current match is brighter.

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
| [src/main.rs](src/main.rs) | lazy `Node` tree, windowed flatten, ratatui viewer, stdin streaming |
| [src/search.rs](src/search.rs) | background-thread search + cancel + result stream |
| [src/source.rs](src/source.rs) | byte source: memory-mapped file vs. buffered stream |
