# rsview

**Browse, navigate, and search multi-GB JSON in the terminal at near-constant memory.**

![rsview opening a 1 GB JSON file instantly](docs/demo.webp)

rsview memory-maps a JSON file and parses it *on expand* — subtrees are byte
ranges into the mmap, never materialized — so opening a 1 GB document sits around
**2.6 MB RSS** and first paint is near-instant whatever the file size. Rows are
syntax-colored and collapsed containers show an inline preview of their first few
children. It's a native Rust port of
[react-obj-view](https://github.com/vothanhdat/react-obj-view)'s CLI core (a
proof-of-concept, not a finished product).

Point it at a file, or pipe into it. Piped input **streams** — `curl -s … |
rsview` renders the document as bytes arrive, and your cursor and expanded nodes
stay put as it fills in. (A stream can't be memory-mapped, so it's buffered and
re-parsed on a throttle; the constant-memory property is the file path's.)

## Install

No Rust toolchain needed — grab the prebuilt binary for your platform:

```sh
curl -fsSL https://raw.githubusercontent.com/vothanhdat/rsview/stable/install.sh | sh
```

It picks the right build (Linux x86_64/arm64, macOS Intel/Apple Silicon) and
drops it in `~/.local/bin`. Or, if you'd rather:

```sh
cargo binstall rsview      # prebuilt binary via cargo-binstall
cargo install  rsview      # compile from crates.io
```

Windows binaries (and every release archive) are attached to each
[release](https://github.com/vothanhdat/rsview/releases).

Then:

```sh
rsview path/to/file.json
cat file.json | rsview                            # pipe it (NDJSON auto-detected)
curl -s https://raw.githubusercontent.com/json-iterator/test-data/refs/heads/master/large-file.json | rsview # streams as it downloads
```

From a checkout: `cargo run --release -- file.json`.

## Keys

| Key | Action |
| --- | --- |
| `↑`/`↓`, `k`/`j` | move focus |
| `J`/`K` | next / previous sibling (same level, stepping over the subtree) |
| `PageUp`/`PageDown`, `Ctrl-F`/`Ctrl-B` | jump a screenful |
| `Ctrl-D`/`Ctrl-U` | jump half a screenful |
| mouse wheel | scroll the pane under the pointer |
| `g`, `Home` | top |
| `Enter`/`→`/`Space` | expand / collapse focused node |
| `←` | collapse, or jump to parent if already collapsed |
| `/` | search (live — results stream as you type) |
| `Enter`/`↓` · `Shift-Enter`/`↑` (in search) | next / previous match |
| `:` | jump to a path — absolute or relative to the cursor |
| `m` · `'` | bookmark the focused node · open the bookmark picker |
| `y` · `Y` | copy the focused value/subtree · copy its path |
| `s` · `o` | split a new pane at the node · open a preview pane |
| `\` · `+`/`-` · `Tab` | toggle layout · resize · switch pane |
| `x` · `q`/`Esc` | close the active pane · close (quit on the last) |
| `?` | show the full keyboard cheatsheet (any key closes it) |

Press `?` in-app for the full list; the footer shows only the core keys. Notes on
the richer ones:

- **Jump (`:`)** — type a path, **absolute** (`data.users[3].city`; leading `$`
  optional, `["odd.key"]` brackets allowed) or **relative** to the cursor,
  Python-import style: `.actor` descends, `..sibling` climbs to the parent, `...x`
  two levels. If a path isn't found where you typed it, resolution climbs toward
  the root and retries — `:city` falls back to `..city`, `...city`, … — landing on
  the nearest ancestor that has it (the footer shows where it reached). Object key
  segments accept `*`/`?` wildcards (`data.user*`, `data.*name*`) when you only
  remember part of the key — the first child whose label matches the whole
  pattern wins.
- **Search (`/`)** — plain queries are case-insensitive substring matches (the
  default). Prefix with `re:` for a full regex (`re:^id_\w+$`) or `g:` for a
  glob (`g:user*`); a bad pattern shows `(bad pattern: …)` in the footer so you
  can fix it without losing what you typed.
- **Bookmarks (`m`/`'`)** — `m` toggles one on the focused node; `'` opens a picker
  (`↵` jump, `d` delete). Per-pane, session-lived.
- **Copy (`y`/`Y`)** — goes through the terminal via
  [OSC 52](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h3-Operating-System-Commands),
  so it needs no clipboard library and **works over SSH** (capped ~1 MiB; in tmux
  set `set -g set-clipboard on`).
- **Panes (`s`/`o`)** — `s` splits a new pane rooted at the focused node; `o` opens
  one *preview* pane that follows your cursor (master/detail). Panes share the one
  mmap (a split costs nothing), each keeps its own focus/expansion/search, and
  closing a pane closes everything split from it.

The top line is `filename   <focus>/<rows>+   <breadcrumb>`; the `+` means the row
count is a lower bound — the level is flattened only as far as you've scrolled.
`Shift-Enter` (previous match) needs a terminal speaking the
[Kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
(Kitty, WezTerm, foot, Ghostty, recent iTerm2/Konsole/VTE); elsewhere use `↑`.
Capturing the mouse suppresses the terminal's own selection — hold `Shift` to
select/copy text as usual.

## Why it's fast (and stays small)

- **mmap, not read.** Mapping the file (`memmap2`) copies nothing — the kernel
  pages in 4 KB chunks only when a byte is touched. A 1 GB file browses at
  ~2.6 MB RSS.
- **Scan bytes, don't parse.** [scanner.rs](src/scanner.rs) walks raw `&[u8]` for
  child byte-ranges. Structural tokens are ASCII and every byte of a multi-byte
  UTF-8 sequence is ≥ 0x80, so the scan never decodes — a value is decoded
  (`from_utf8`) only when drawn, and only that slice.
- **Parse on expand, incrementally.** A collapsed node is O(1); expanding scans
  one more child per call (a resumable `Cursor`), so a level with millions of keys
  flattens ~a screenful at a time. First paint is ~constant regardless of size.
- **Search off the UI thread.** [search.rs](src/search.rs) scans the mmap on its
  own OS thread, streaming match paths over an `mpsc` channel with an `AtomicBool`
  cancel — retyping drops the old search instantly and the UI never blocks. (A
  scan faults in the pages it reads — evictable, file-backed page cache.)

## How it compares

Interactive TUI viewers — [jless](https://jless.io/), [fx](https://fx.wtf/), jnv —
parse the **whole document into memory** at startup. That's fine up to hundreds of
MB, but a multi-GB file means multi-GB of RAM (or it won't open). Streaming and
query engines — `jq --stream`, DuckDB's `read_json_auto`,
[simdjson](https://github.com/simdjson/simdjson) — stay near-constant memory but
aren't *browsers*: you pipe data through them, you don't navigate it.

rsview takes the overlap those two camps leave open: **browse and search a
multi-GB file interactively, at near-constant memory.** It is deliberately *not* a
transform tool — constructing new values means materializing them, forfeiting the
very property that makes it useful on a huge file. Reach for `jq` or DuckDB to
*produce* data; reach for rsview to *read* a file too big to open comfortably
anywhere else.

## Layout

| File | Role |
| --- | --- |
| [src/scanner.rs](src/scanner.rs) | byte-range JSON scan + resumable child `Cursor` |
| [src/main.rs](src/main.rs) | lazy `Node` tree, windowed flatten, multi-pane ratatui viewer, stdin streaming |
| [src/search.rs](src/search.rs) | background-thread search + cancel + result stream |
| [src/source.rs](src/source.rs) | byte source: memory-mapped file vs. buffered stream |
