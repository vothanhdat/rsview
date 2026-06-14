# rsview — Rust proof-of-concept

A native port of [react-obj-view](https://github.com/vothanhdat/react-obj-view)'s CLI core: memory-map a JSON file, parse
**on expand** (subtrees are byte ranges, never materialized), flatten a level
only as far as the viewport scrolls, and search on a background thread. Opening a
multi-GB file stays near-constant memory. Rows are syntax-colored, and collapsed
containers show an inline preview of their first few children.

This is a **proof-of-concept**, not the product: a single file argument or piped
stdin (`.jsonl`/`.ndjson` shown as an array of documents), no themes, no copy.
Open / navigate / expand / search / split into panes. Piped input **streams** — it renders
progressively as bytes arrive, so `curl -s … | rsview` shows the document *before
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
| mouse wheel | scroll the pane under the pointer (≈3 rows a notch) |
| `g`, `Home` | top |
| `Enter`/`→`/`Space` | expand / collapse focused node |
| `←` | collapse, or jump to parent if already collapsed |
| `/` | search (live — results stream as you type) |
| `Enter` / `↓` (in search) | next match |
| `Shift-Enter` / `↑` (in search) | previous match |
| `Esc` (in search) | close search (keeps cursor on the match) |
| `:` | jump to a path — type `data.users[3].city`, `Enter` to go |
| `m` | bookmark the focused node (press again to remove) |
| `'` | open the bookmark picker (`↵` jump · `d` delete · `Esc` close) |
| `y` | copy the focused value — a scalar, or the whole subtree — to the clipboard |
| `Y` | copy the path to the focused node (`data.users[3].city`) |
| `s` | split: open a new pane rooted at the focused node (and switch to it) |
| `o` | open/re-root a single preview pane at the focused node (stay on parent) |
| `\` | toggle pane layout (side by side ↔ stacked) |
| `+` / `-` | grow / shrink the active pane |
| `Tab` / `Shift-Tab` | switch the active pane |
| `x` | close the active pane (and any panes split from it) |
| `q`, `Esc` | close the active pane and its children (quit on the last) |

The search box stays open so you can cycle matches in place, then `Esc` to
explore the tree at the match. `Shift-Enter` needs a terminal that speaks the
[Kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
(Kitty, WezTerm, foot, Ghostty, recent iTerm2/Konsole/VTE); elsewhere use `↑`.

Input is coalesced per frame, so holding a key or spinning the wheel stays
snappy instead of lagging behind. Capturing the mouse suppresses the terminal's
own text selection — hold `Shift` to select/copy as usual.

**Jump & bookmarks.** `:` opens a path prompt — type `data.users[3].city` (a
leading `$`/`.` is fine, and bracketed keys like `["odd.key"]` work) and `Enter`
jumps straight there, expanding what it needs on the way. Resolution is lazy like
everything else, so object keys are matched within the first ~100k siblings of a
level. `m` bookmarks the focused node; `'` opens a picker listing your bookmarks
by path, where `↵` jumps, `d` deletes, and `Esc` closes. Bookmarks are per-pane
and live for the session.

**Copy.** `y` copies the focused node's raw JSON — a scalar literal, or an entire
subtree — and `Y` copies its path. Copy goes through the terminal itself via the
[OSC 52](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h3-Operating-System-Commands)
escape, so it needs no system-clipboard library and **works over SSH**. A single
copy is capped (~1 MiB) since the payload rides the terminal escape; to pull a
large subtree out wholesale, that's export's job, not the clipboard. Inside tmux,
enable `set -g set-clipboard on`.

**Panes.** Press `s` on any container to split off a new pane rooted at that
node — the same document seen from a different path — and switch to it. `o`
instead opens (or, if one already exists, **re-roots**) a single *preview* pane
and keeps you on the parent, so you can move the cursor and watch a detail pane
follow along — master/detail browsing. Panes form a tree: each child links to
the pane it was split from, and closing a pane (`x`, or `q`/`Esc`) closes
everything split from it too. They share the one memory-mapped file, so a split
costs nothing; each keeps its own focus, expansion, breadcrumb, and search
(scoped to that pane's subtree). Keys go to the active pane, which alone shows
the highlighted title and cursor bar. `Tab` switches panes, `\` toggles the
workspace between side-by-side (columns) and stacked (rows), and `+`/`-` grow and
shrink the active pane.

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

## How it compares

Interactive terminal JSON viewers — [jless](https://jless.io/), [fx](https://fx.wtf/),
jnv — read and parse the **whole document into memory** at startup. That's ideal up
to tens or hundreds of MB; a multi-GB file means multi-GB of RAM (or it simply won't
open). Streaming and query engines — `jq --stream`, Miller, DuckDB's `read_json_auto`,
[simdjson](https://github.com/simdjson/simdjson) — stay at near-constant memory but
aren't *browsers*: you pipe data in and get data out, you don't navigate and expand it.

rsview goes for the overlap those two camps leave open: **browse and search a multi-GB
file interactively while memory stays near-constant.** It memory-maps the file and
decodes byte-ranges only as you expand and scroll, so opening a 1 GB document sits
around 2.6 MB RSS instead of loading the whole thing.

It is deliberately **not** a transform/query tool — no `jq`-style reshaping, and not
by oversight: constructing new values means materializing them, which would forfeit
exactly the property that makes rsview worth using on a huge file. Reach for `jq` or
DuckDB when you need to *produce* data; reach for rsview when you need to *read* a file
too big to open comfortably anywhere else.

## Layout

| File | Role |
| --- | --- |
| [src/scanner.rs](src/scanner.rs) | byte-range JSON scan + resumable child `Cursor` |
| [src/main.rs](src/main.rs) | lazy `Node` tree, windowed flatten, multi-pane ratatui viewer, stdin streaming |
| [src/search.rs](src/search.rs) | background-thread search + cancel + result stream |
| [src/source.rs](src/source.rs) | byte source: memory-mapped file vs. buffered stream |
