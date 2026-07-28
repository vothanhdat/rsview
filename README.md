# jview

**Browse, navigate, and search multi-GB JSON in the terminal at near-constant memory.**

![jview opening a 1 GB JSON file instantly](docs/demo.webp)

jview memory-maps a JSON file and parses it *on expand* — subtrees are byte
ranges into the mmap, never materialized — so opening a 1 GB document sits around
**2.6 MB RSS** and first paint is near-instant whatever the file size. Rows are
syntax-colored and collapsed containers show an inline preview of their first few
children. It's a native Rust port of
[react-obj-view](https://github.com/vothanhdat/react-obj-view)'s CLI core (a
proof-of-concept, not a finished product).

Point it at a file, or pipe into it. Piped input **streams** — `curl -s … |
jview` renders the document as bytes arrive, and your cursor and expanded nodes
stay put as it fills in. A stream can't be memory-mapped directly, so its bytes
are spilled to a temp file that jview mmaps and re-parses on a throttle — so a
pipe stays at near-constant memory too, like the file path. The temp file is
unlinked the moment it's opened, so it never appears on disk and the OS reclaims
it when jview exits, even on a crash. (Where no writable temp dir is available,
or off unix, it falls back to buffering in RAM.)

## Install

No Rust toolchain needed — grab the prebuilt binary for your platform:

```sh
curl -fsSL https://raw.githubusercontent.com/vothanhdat/rsview/stable/install.sh | sh
```

It picks the right build (Linux x86_64/arm64, macOS Intel/Apple Silicon) and
drops it in `~/.local/bin`. Or, if you'd rather:

```sh
cargo binstall jsonview   # prebuilt binary via cargo-binstall (installs `jview`)
cargo install  jsonview   # compile from crates.io (installs `jview`)
```

Windows binaries (and every release archive) are attached to each
[release](https://github.com/vothanhdat/rsview/releases).

Then:

```sh
jview path/to/file.json
cat file.json | jview                            # pipe it (NDJSON auto-detected)
curl -s https://raw.githubusercontent.com/json-iterator/test-data/refs/heads/master/large-file.json | jview # streams as it downloads
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
| `Enter`/`→`/`Space` | expand / collapse a container — or peek a leaf's full value |
| `←` | collapse, or jump to parent if already collapsed |
| `t` · `c` · `#` | infer the value's type (TypeScript-style; `y` copies it) · count a container's children · aggregate its numeric children (count/sum/min/max/mean) |
| `/` | search (live — results stream as you type; `Tab` scopes it to the focused subtree) |
| `Enter`/`↓` · `Shift-Enter`/`↑` (in search) | next / previous match |
| `:` | jump to a path — absolute or relative to the cursor |
| `\|` | jq-style filter — opens a result pane of the selected nodes |
| `m` · `'` | bookmark the focused node · open the bookmark picker |
| `y` · `Y` | copy the focused value/subtree · copy its path |
| `p` | pipe the focused node's JSON to stdout — or every hit on a filter pane (when output is redirected) |
| `s` · `o` | split a new pane at the node · open a preview pane |
| `\` · `+`/`-` · `Tab` | toggle layout · resize · switch pane |
| `x` · `q`/`Esc` | close the active pane · close (quit on the last) |
| `?` | show the full keyboard cheatsheet (any key closes it) |

Press `?` in-app for the full list; the footer shows only the core keys. Notes on
the richer ones:

- **Editing the prompts** — the `/`, `:`, and `|` inputs are real single-line
  fields with a visible caret: `←`/`→` move it, `Home`/`End` (or `Ctrl-A`/`Ctrl-E`)
  jump to the ends, `Backspace`/`Delete` remove either side of it, and
  `Ctrl-W`/`Ctrl-U`/`Ctrl-K` delete the previous word / to the start / to the end.
  Editing anywhere in a `/` query re-runs the live search; moving the caret alone
  doesn't. In the `:` and `|` prompts, `↑`/`↓` walk a **history** of the paths and
  filters you've submitted this session, so a long one can be recalled and tweaked
  instead of retyped (a half-typed line is stashed and comes back when you arrow
  past the newest entry).
- **Jump (`:`)** — type a path, **absolute** (`data.users[3].city`; leading `$`
  optional, `["odd.key"]` brackets allowed) or **relative** to the cursor,
  Python-import style: `.actor` descends, `..sibling` climbs to the parent, `...x`
  two levels. If a path isn't found where you typed it, resolution climbs toward
  the root and retries — `:city` falls back to `..city`, `...city`, … — landing on
  the nearest ancestor that has it (the footer shows where it reached). Object key
  segments accept `*`/`?` wildcards (`data.user*`, `data.*name*`) when you only
  remember part of the key — the first child whose label matches the whole
  pattern wins.
- **Type (`t`)** — infer the focused value's **structural type**, rendered
  TypeScript-style, the way a "JSON to TypeScript" tool does. It samples the
  bytes and *merges* them into one recursive type: object shapes are unified (a
  key missing from some records becomes `field?`, with a `// 66%` fill comment),
  array elements and map values collapse into a single element type, mixed scalars
  become unions (`number | string`), and a **data-keyed object** becomes
  `Record<K, T>`. Map detection uses both the values (do they share a shape?) and
  the **keys** (do they look like data rather than field names?) — so
  `{"AAPL": {…}, "MSFT": {…}}` and `{8960: […], 8970: […]}` are both recognized
  (the latter as `Record<number, number[]>`), even with only a handful of entries,
  while a small record with real field names stays a record. Because detection
  runs per-node, a map nested in thousands of records collapses to one
  `Record<…>` instead of exploding every key into an optional field. It recurses
  to full depth automatically, so
  ```ts
  {
    users: { id: number; name: string; email?: string; contact?: { email: string } }[]
    cumulativeStats: Record<string, { buyCount: number; buyValCum: number }>
  }
  ```
  falls out in one shot. Scroll it with `j`/`k`, and **`y` copies the whole type**
  to your clipboard to paste straight into your code. Sampling is bounded (2000
  records, a node budget), so it stays instant on huge, deeply-nested data. The
  inference lives in its own module, [src/schema.rs](src/schema.rs).
- **Count (`c`)** — a container's exact child count (`1,234,567 elements`), a full
  scan but on demand, so you can size a collapsed level without opening it.
- **Aggregate (`#`)** — the numeric companion to `c`: a one-line **count · sum ·
  min · max · mean** of the focused container's *direct* numeric children, in the
  same streaming pass (it accumulates into an `f64`, never materializing the
  array, so it stays constant-memory on a huge one). Non-numbers are skipped and
  the footer notes when only some children counted (`12 of 20 numeric`). Pairs
  naturally with the `|` filter: `.[].price` into a result pane, then `#` to total
  it.
- **Search (`/`)** — plain queries are case-insensitive substring matches (the
  default). Prefix with `re:` for a full regex (`re:^id_\w+$`) or `g:` for a
  glob (`g:user*`); a bad pattern shows `(bad pattern: …)` in the footer so you
  can fix it without losing what you typed. `Tab` **scopes** the search to the
  container you were on when you opened it (the footer shows `in <label>`) — press
  it again for the whole document; scoping is faster and quieter in a huge file.
- **Filter (`|`)** — a jq-style **selection** pipeline that opens a new pane
  listing the nodes it picks out (`.users[] | select(.age > 30) | .name`).
  Supported:
  - **paths** — field access (`.foo.bar`, `["odd.key"]`), indexing with
    negatives (`.[3]`, `.[-1]`), array **slices** streamed as elements
    (`.[1:3]`, `.[-2:]`), and iteration (`.[]` over an array's elements or an
    object's values);
  - **recursive descent** — `..` yields the value and every descendant, so
    `.. | .id` pulls every `id` at any depth;
  - **`select(…)`** — comparisons (`== != < <= > >=`) between two operands, where
    an operand is a path (`.a.b`) or a literal (number / `"string"` / `true` /
    `false` / `null`); string **matches** `~` / `!~` against a path
    (`select(.name ~ "re:^a")`), where the pattern speaks the same dialect as `/`
    search — plain text is a case-insensitive substring, `re:` a regex, `g:` a
    `*`/`?` glob, and only string values ever match; all combined with `and` /
    `or` and `( … )` grouping, or a bare path as a truthiness test
    (`select(.active)`);
  - **`,` and `|`** — the comma unions outputs (`.name, .email`, binding tighter
    than the pipe) and `|` chains stages.

  Because it only *selects* sub-values that already exist — it never builds new
  ones — each result is a zero-copy byte range into the file, so filtering a
  multi-GB document collects offsets rather than materializing values, and
  results stream into the pane as the worker scans (the title shows a live
  `N hits` count, capped at 5000). Missing paths are skipped rather than
  erroring (jq's `?`; inside `select` a missing path reads as `null`); a
  malformed expression shows the reason in the footer without closing the
  prompt. NDJSON feeds each document through the pipeline in turn. Two
  intentional divergences from jq keep it zero-copy: a slice yields its elements
  as a stream rather than a new sub-array, and cross-type comparisons are simply
  unequal/unordered.
- **Peek (`Enter` on a leaf)** — rows truncate a value to keep one line each, so a
  long string, a URL, an embedded-JSON blob, or a multi-line log line gets cut off
  with `…`. Press `Enter`/`Space` on a scalar (there's nothing to expand) to open a
  near-full-screen overlay with the **whole value**, word-wrapped, with `\n`/`\t`
  and `\uXXXX` escapes rendered as real characters so multi-line text is readable.
  Scroll it with `j`/`k`, `PageUp`/`PageDown` (or `Ctrl-F`/`Ctrl-B`), `g`/`G` for
  top/bottom; `esc`/`q` closes. The decode is bounded (8 MiB) and the title flags
  `⚠ capped` if the value was longer; to get an uncapped copy use `y`, or `p` to
  pipe the raw bytes out.
- **Bookmarks (`m`/`'`)** — `m` toggles one on the focused node; `'` opens a picker
  (`↵` jump, `d` delete). Per-pane, session-lived.
- **Copy (`y`/`Y`)** — goes through the terminal via
  [OSC 52](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h3-Operating-System-Commands),
  so it needs no clipboard library and **works over SSH** (capped ~1 MiB; in tmux
  set `set -g set-clipboard on`).
- **Pipe out (`p`)** — when you redirect jview's output
  (`jview big.json | jq …`, or `> node.json`), the UI renders on your terminal
  and `p` writes the focused node's raw JSON to that pipe/file, then quits. The
  payload is a zero-copy slice of the mmap and **uncapped**, so it's the way to
  carve one subtree out of a file too big for `jq` to open and hand just that
  piece downstream. On a **filter-result pane** `p` instead streams *every* hit as
  NDJSON (one value per line), so `select(…)` becomes a batch extractor — carve
  all the matching subtrees out at once. Into a plain terminal (no redirect)
  there's nowhere to pipe, so `p` shows a hint instead.
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

jview takes the overlap those two camps leave open: **browse and search a
multi-GB file interactively, at near-constant memory.** It is deliberately *not* a
transform tool — constructing new values means materializing them, forfeiting the
very property that makes it useful on a huge file. Reach for `jq` or DuckDB to
*produce* data; reach for jview to *read* a file too big to open comfortably
anywhere else — then `p` hands the one subtree you navigated to straight to `jq`,
which could never have opened the whole file itself.

## Layout

| File | Role |
| --- | --- |
| [src/scanner.rs](src/scanner.rs) | byte-range JSON scan + resumable child `Cursor` |
| [src/main.rs](src/main.rs) | lazy `Node` tree, windowed flatten, multi-pane ratatui viewer, stdin streaming |
| [src/search.rs](src/search.rs) | background-thread search + cancel + result stream |
| [src/schema.rs](src/schema.rs) | JSON → structural type inference (the `t` overlay) |
| [src/source.rs](src/source.rs) | byte source: memory-mapped file or spilled stream, vs. RAM-buffered fallback |
