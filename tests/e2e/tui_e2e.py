#!/usr/bin/env python3
"""End-to-end TUI tests for jview.

Drives the real binary inside a pseudo-terminal and asserts on what it paints,
using pyte as a headless terminal emulator. This is the kind of thing unit tests
can't reach — actual key handling, sticky headers, overlays, and the footer.

Usage:
    python tests/e2e/tui_e2e.py [path-to-jview]      # default: target/release/jview

Requires: pyte  (pip install pyte).  Unix only (uses pty.fork).
Exits 0 if every scenario passes, 1 otherwise.
"""

import fcntl
import json
import os
import pty
import struct
import sys
import termios
import time

try:
    import pyte
except ImportError:
    sys.exit("this harness needs pyte:  pip install pyte")

ROWS, COLS = 32, 100
FAILS = []


class Tui:
    """A running jview in a pty, with a pyte screen mirroring its output."""

    def __init__(self, binary, doc, pipe=False):
        # File mode passes a path; pipe mode feeds the doc to jview's stdin (the
        # spill-to-tempfile streaming path) and passes no arguments.
        self.pipe = pipe
        self.path = f"/tmp/jview_e2e_{os.getpid()}.json"
        raw = doc if isinstance(doc, (str, bytes)) else json.dumps(doc)
        if isinstance(raw, str):
            raw = raw.encode()
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.ByteStream(self.screen)
        if not pipe:
            with open(self.path, "wb") as f:
                f.write(raw)
        self.pid, self.fd = pty.fork()
        if self.pid == 0:  # child
            if pipe:
                r, w = os.pipe()
                if os.fork() == 0:  # grandchild feeds the pipe, then EOFs
                    os.close(r)
                    os.write(w, raw)
                    os.close(w)
                    os._exit(0)
                os.close(w)
                os.dup2(r, 0)
                os.execv(binary, [binary])
            else:
                os.execv(binary, [binary, self.path])
            os._exit(127)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.pump(0.8 if pipe else 0.6)  # initial render (streaming needs a beat)

    def pump(self, secs=0.3):
        end = time.time() + secs
        while time.time() < end:
            try:
                os.set_blocking(self.fd, False)
                data = os.read(self.fd, 65536)
            except (BlockingIOError, OSError):
                data = b""
            if data:
                self.stream.feed(data)
                # Answer terminal capability queries so crossterm proceeds.
                if b"\x1b[?u" in data:
                    os.write(self.fd, b"\x1b[?1u")
                if b"\x1b[c" in data:
                    os.write(self.fd, b"\x1b[?62c")
            else:
                time.sleep(0.01)

    def send(self, keys):
        os.write(self.fd, keys.encode() if isinstance(keys, str) else keys)
        self.pump(0.3)

    def line(self, y):
        return "".join(self.screen.buffer[y][x].data for x in range(COLS)).rstrip()

    def title(self):
        return self.line(0)

    def footer(self):
        for y in range(ROWS - 1, -1, -1):
            if self.line(y).strip():
                return self.line(y)
        return ""

    def dump(self):
        return "\n".join(self.line(y) for y in range(ROWS))

    def card(self):
        """Text inside the floating overlay box only (between its border rows).

        Overlays shrink to fit their content, so the surrounding tree stays
        visible around a small card — use this, not dump(), when an assertion
        means "…is (not) in the overlay" rather than "…anywhere on screen".
        """
        rows = [self.line(y) for y in range(ROWS)]
        top = next((i for i, r in enumerate(rows) if "┌" in r), None)
        bot = next((i for i in range(ROWS - 1, -1, -1) if "└" in rows[i]), None)
        if top is None or bot is None or bot < top:
            return self.dump()
        return "\n".join(rows[top : bot + 1])

    def close(self):
        try:
            os.write(self.fd, b"q")
            self.pump(0.2)
            os.close(self.fd)
        except OSError:
            pass
        try:
            os.unlink(self.path)
        except OSError:
            pass


def check(name, cond, tui):
    status = "ok  " if cond else "FAIL"
    print(f"  [{status}] {name}")
    if not cond:
        FAILS.append(name)
        print("    --- screen ---")
        print("\n".join("    " + ln for ln in tui.dump().splitlines()))


def scenario_jump_relative_and_climb(binary):
    print("jump: absolute, relative, climb-on-miss")
    t = Tui(binary, {"users": [
        {"name": "alice", "address": {"city": "NYC", "zip": "10001"}},
        {"name": "bob", "address": {"city": "LA", "zip": "90001"}},
    ]})
    try:
        t.send(":"); t.send("users[1].address.zip"); t.send("\r")
        check("absolute path focuses users[1].address.zip",
              "users › [1] › address › zip" in t.title(), t)
        # `:city` is absent at root → climbs the cursor's ancestry to address.city.
        t.send(":"); t.send("city"); t.send("\r")
        check("bare :city climbs to users[1].address.city",
              "users[1].address.city" in t.footer(), t)
        # relative parent hop
        t.send(":"); t.send(".."); t.send("\r")
        check("relative .. climbs to users[1].address",
              t.footer().strip().endswith("users[1].address"), t)
    finally:
        t.close()


def scenario_line_editing(binary):
    print("prompts: caret editing (arrows, Home/End, mid-string insert)")
    t = Tui(binary, {"alpha": {"beta": 1}, "gamma": 2})
    try:
        # Mid-string insertion: type a path missing a letter, move the caret back
        # into it with ←, and insert — something an append-only prompt can't do.
        t.send(":"); t.send("alpha.eta")
        t.send("\x1b[D\x1b[D\x1b[D")     # ← ← ←  → caret before "eta"
        t.send("b")                       # → "alpha.beta"
        t.send("\r")
        check("← then insert fixes the path mid-string (alpha.beta)",
              "alpha › beta" in t.title(), t)

        # The filter footer echoes the typed text verbatim, so it's a clean mirror
        # of the edit buffer. Exercise insert-at-caret, Home/End, and Backspace.
        t.send("|"); t.send("hello")
        t.send("\x1b[D\x1b[D")            # ← ←  → caret between "hel|lo"
        t.send("XY")                      # → "helXYlo"
        check("insert at an interior caret",
              "helXYlo" in t.footer(), t)
        t.send("\x01"); t.send(">")       # Ctrl-A (home) then prepend
        check("Ctrl-A homes the caret; text prepends",
              ">helXYlo" in t.footer(), t)
        t.send("\x05"); t.send("<")       # Ctrl-E (end) then append
        check("Ctrl-E ends the caret; text appends",
              ">helXYlo<" in t.footer(), t)
        t.send("\x7f")                    # Backspace at end
        check("Backspace deletes before the caret",
              ">helXYlo<" not in t.footer() and ">helXYlo" in t.footer(), t)
        t.send("\x1b")                    # cancel the prompt
    finally:
        t.close()


def scenario_aggregate_and_history(binary):
    print("aggregate (#) + prompt history recall (:/| ↑↓)")
    t = Tui(binary, {"prices": [10, 20, 30], "gamma": 2})
    try:
        # `#` summarizes a container's direct numeric children in the footer.
        t.send(":"); t.send("prices"); t.send("\r")   # focus the array (records it)
        t.send("#")
        foot = t.footer()
        check("# reports count/sum/min/max/avg",
              "3 numbers" in foot and "60" in foot and "min 10" in foot
              and "max 30" in foot and "avg 20" in foot, t)

        # Reopen `:` and press ↑ — the submitted "prices" comes back.
        t.send(":")
        t.send("\x1b[A")                              # ↑  → recall newest
        check("↑ in : recalls the last jump",
              "prices" in t.footer(), t)
        # Type a fresh draft, ↑ (stash it + show history), ↓ (restore the draft).
        t.send("\x1b"); t.send(":")
        t.send("draft")
        t.send("\x1b[A")                              # ↑  → "prices", draft stashed
        check("↑ replaces the draft with history",
              "prices" in t.footer() and "draft" not in t.footer(), t)
        t.send("\x1b[B")                              # ↓  → back past newest = draft
        check("↓ past the newest restores the draft",
              "draft" in t.footer(), t)
        t.send("\x1b")                                # cancel

        # The `|` filter prompt has its own recall ring.
        t.send("|"); t.send(".gamma"); t.send("\r")   # run a filter (records it)
        t.send("|"); t.send("\x1b[A")                 # reopen, ↑ recalls ".gamma"
        check("↑ in | recalls the last filter",
              ".gamma" in t.footer(), t)
        t.send("\x1b")
    finally:
        t.close()


def scenario_sibling_nav(binary):
    print("siblings: J / K step over subtrees")
    t = Tui(binary, {"events": [
        {"id": 1, "actor": {"login": "alice"}},
        {"id": 2, "actor": {"login": "bob"}},
        {"id": 3, "actor": {"login": "carol"}},
    ]})
    try:
        t.send(":"); t.send("events[0]"); t.send("\r")
        t.send("\r")  # expand events[0] so children sit between siblings
        t.send("J")
        check("J jumps over the subtree to events[1]", t.title().endswith("[1]"), t)
        t.send("J")
        check("J again to events[2]", t.title().endswith("[2]"), t)
        t.send("J")
        check("J at last sibling is a no-op", t.title().endswith("[2]"), t)
        t.send("K")
        check("K walks back to events[1]", t.title().endswith("[1]"), t)
    finally:
        t.close()


def scenario_filter(binary):
    print("filter: | jq-style selection opens a result pane")
    t = Tui(binary, {"users": [
        {"name": "amy", "age": 20},
        {"name": "bob", "age": 40},
        {"name": "cara", "age": 60},
    ]})
    try:
        # A bad expression reports the reason (parenthesized) and keeps the prompt
        # open with the typed text intact.
        t.send("|"); t.send("select(.a >)"); t.send("\r")
        foot = t.footer()
        check("malformed filter shows the parse error in the footer",
              "select(.a >)" in foot and foot.rstrip().endswith(")"), t)
        t.send("\x1b")  # esc out of the prompt
        # A real selection pipeline opens a result pane of the picked nodes.
        t.send("|"); t.send(".users[] | select(.age > 30) | .name"); t.send("\r")
        t.pump(0.8)  # let the worker stream its hits in
        scr = t.dump()
        check("result pane lists the two matching names",
              '"bob"' in scr and '"cara"' in scr, t)
        check("filtered-out value is absent from the result pane",
              '"amy"' not in scr, t)
        check("result pane title shows a hit count",
              "2 hit" in t.title(), t)
        check("result labels carry the origin path",
              "users[1].name" in scr, t)
    finally:
        t.close()

    # Boolean 'or' inside select.
    t = Tui(binary, {"team": [
        {"name": "amy", "age": 20, "vip": False},
        {"name": "bob", "age": 70, "vip": False},
        {"name": "cara", "age": 18, "vip": True},
    ]})
    try:
        t.send("|"); t.send(".team[] | select(.age > 65 or .vip) | .name"); t.send("\r")
        t.pump(0.8)
        scr = t.dump()
        check("boolean 'or' in select keeps both matches",
              '"bob"' in scr and '"cara"' in scr and '"amy"' not in scr, t)
    finally:
        t.close()

    # Recursive descent reaches a key at any depth.
    t = Tui(binary, {"nested": {"a": {"id": 1}, "b": [{"id": 2}, {"id": 3}]}})
    try:
        t.send("|"); t.send(".. | .id"); t.send("\r")
        t.pump(0.8)
        scr = t.dump()
        check("recursive descent finds .id at every depth",
              all(f": {n}" in scr for n in (1, 2, 3)), t)
    finally:
        t.close()

    # Regex string match with the ~ operator inside select.
    t = Tui(binary, {"files": [
        {"name": "report.pdf"},
        {"name": "notes.txt"},
        {"name": "scan.pdf"},
    ]})
    try:
        t.send("|"); t.send('.files[] | select(.name ~ "re:\\.pdf$") | .name'); t.send("\r")
        t.pump(0.8)
        scr = t.dump()
        check("~ regex match keeps only the .pdf names",
              '"report.pdf"' in scr and '"scan.pdf"' in scr
              and '"notes.txt"' not in scr, t)
    finally:
        t.close()


def scenario_peek(binary):
    print("peek: Enter on a leaf opens a scrollable full-value overlay")
    # A string far longer than the ~70-char inline truncation, with a marker only
    # the peek overlay can reveal.
    long = "A" * 60 + "-MID-" + "B" * 400 + "-ENDMARK-"
    t = Tui(binary, {"bio": long})
    try:
        t.send(":"); t.send("bio"); t.send("\r")   # focus the string leaf
        check("inline row truncates the long value (no end marker)",
              "-ENDMARK-" not in t.dump(), t)
        t.send("\r")  # Enter on a scalar → peek
        scr = t.dump()
        check("peek overlay opens with a titled card",
              "peek" in scr and "bio" in scr, t)
        check("peek reveals content past the inline truncation",
              "-ENDMARK-" in scr, t)
        t.send("\x1b")  # esc
        check("esc closes the peek overlay",
              "j/k scroll" not in t.dump(), t)
    finally:
        t.close()

    # A multi-line value taller than the box: newlines render as separate lines
    # and G/g scroll to the end and back.
    doc = "\n".join(f"row{n:02d}" for n in range(40))
    t = Tui(binary, {"log": doc})
    try:
        t.send(":"); t.send("log"); t.send("\r")
        t.send("\r")  # peek
        scr = t.dump()
        check("embedded newlines render as separate lines",
              "row00" in scr and "row01" in scr, t)
        check("a value taller than the box isn't all shown at once",
              "row39" not in scr, t)
        t.send("G")
        check("G scrolls to the end of a long value", "row39" in t.dump(), t)
        t.send("g")
        check("g scrolls back to the top", "row00" in t.dump(), t)
    finally:
        t.close()


def scenario_stream_pipe(binary):
    print("stream: piped stdin renders, searches, and cleans up its spill file")
    import glob
    pre = set(glob.glob("/tmp/jview-stream-*"))
    # NDJSON piped in (not a file) exercises the spill-to-tempfile path.
    doc = "\n".join(json.dumps({"id": i, "tag": "NEEDLE" if i == 40 else "x"})
                    for i in range(60))
    t = Tui(binary, doc, pipe=True)
    try:
        scr = t.dump()
        check("piped stream renders as it arrives",
              "id" in scr and "stdin" in t.title(), t)
        # While running, the spill file is unlinked (no visible name) yet held open.
        named = set(glob.glob("/tmp/jview-stream-*")) - pre
        check("spill temp file is unlinked (no lingering name)", not named, t)
        # Search runs over the spilled mmap and finds a row deep in the stream.
        t.send("/"); t.send("NEEDLE"); t.pump(0.8)
        foot = t.footer()
        check("search over the spilled stream finds the needle",
              "match" in foot and "0 match" not in foot, t)
        t.send("\x1b")  # esc out of search
    finally:
        t.close()
        t.pump(0.2)
    leftover = set(glob.glob("/tmp/jview-stream-*")) - pre
    check("no spill temp files leak after exit", not leftover, t)


def scenario_explore(binary):
    print("explore: c count · t type · Tab-scoped search")
    t = Tui(binary, {
        "users": [
            {"id": 1, "name": "amy", "contact": {"email": "a@x.com"}, "tags": ["x"]},
            {"id": 2, "name": "bob"},
            {"id": 3, "name": "cara", "contact": {"email": "c@x.com"}},
        ],
        "audit": [{"name": "zzz"}],
        # A data-keyed map: keys are ids, values all share {ccy, amount}.
        "prices": {f"AAA{n:03d}": {"ccy": "USD", "amount": n * 1.5}
                   for n in range(20)},
        # A record with disjoint-object fields (not a map) that *contains* a map.
        "book": {
            "meta": {"title": "t", "pages": 3},
            "author": {"name": "n", "born": 1970},
            "quotes": {f"Q{n:02d}": {"bid": n, "ask": n + 1} for n in range(12)},
        },
    })
    try:
        # `c` counts a container's children without expanding it.
        t.send(":"); t.send("users"); t.send("\r")
        t.send("c")
        check("c reports the element count", "3 elements" in t.footer(), t)

        # `t` infers the node's structural type (JSON→TypeScript style).
        t.send("t")
        scr = t.dump()
        check("t opens a type card", "type" in scr and "y copy" in scr, t)
        check("array of objects → { … }[] with scalar field types",
              "id: number" in scr and "name: string" in scr and "}[]" in scr, t)
        check("a field missing from some records is optional (?)",
              "contact?: {" in scr and "email: string" in scr, t)
        check("arrays render as T[]", "tags?: string[]" in scr, t)
        # `y` copies the whole type to the clipboard.
        t.send("y")
        check("y copies the inferred type", "copied type" in t.footer(), t)
        t.send("\x1b")  # close

        # A data-keyed object becomes Record<string, V> — values, not keys.
        t.send(":"); t.send("prices"); t.send("\r")
        t.send("t")
        m = t.card()
        check("a data-keyed object infers as Record<string, …>",
              "Record<string, {" in m, t)
        check("the map's value fields are inferred, not its data keys",
              "ccy: string" in m and "amount: number" in m and "AAA000" not in m, t)
        t.send("\x1b")

        # A record whose object fields have disjoint keys stays a record, and a map
        # nested inside a record is inferred as a nested Record<>.
        t.send(":"); t.send("book"); t.send("\r")
        t.send("t")
        bk = t.dump()
        check("disjoint-object record stays a record (not a Record<>)",
              "meta: {" in bk and "title: string" in bk, t)
        check("a nested map infers as a nested Record<string, …>",
              "quotes: Record<string, {" in bk and "bid: number" in bk, t)
        t.send("\x1b")

        # Scoped search: 'name' appears under both users and audit; scope to users.
        t.send(":"); t.send("users"); t.send("\r")
        t.send("/"); t.send("name"); t.pump(0.6)
        whole = t.footer()
        check("unscoped search sees matches across the doc",
              "match" in whole and "0 match" not in whole, t)
        t.send("\t"); t.pump(0.6)  # scope to users
        scoped = t.footer()
        check("Tab scopes the search to the focused container",
              "in users" in scoped, t)
        # users has 3 names; audit has 1 — scoping must drop the audit hit.
        import re
        def count(s):
            m = re.search(r"(\d+)\s+match", s)
            return int(m.group(1)) if m else -1
        check("scoping narrows the match count",
              0 < count(scoped) < count(whole), t)
        t.send("\x1b")
    finally:
        t.close()


def scenario_help_overlay(binary):
    print("help: ? overlay + trimmed footer")
    t = Tui(binary, {"a": {"b": 1}, "c": [1, 2, 3]})
    try:
        foot = t.footer()
        check("footer is trimmed to core keys + ? help",
              "? help" in foot and "split" not in foot and "marks" not in foot, t)
        t.send("?")
        scr = t.dump()
        check("? opens the cheatsheet with the full key list",
              "keyboard shortcuts" in scr and "next / prev sibling" in scr
              and "split pane at node" in scr, t)
        check("cheatsheet shows worked filter/search examples",
              "examples" in scr and "select(.age > 30)" in scr
              and "regex search" in scr, t)
        t.send("j")  # any key closes it
        check("any key closes the overlay", "keyboard shortcuts" not in t.dump(), t)
    finally:
        t.close()


def scenario_bookmark_indicator(binary):
    print("bookmarks: m toggles a ▎ gutter marker on the row")
    t = Tui(binary, {"alpha": 1, "beta": 2, "gamma": 3})
    try:
        check("no ▎ before anything is bookmarked", "▎" not in t.dump(), t)
        check("footer has no bookmark status when there are none",
              "bookmark" not in t.footer(), t)
        t.send("j")  # step off the root onto the alpha child
        t.send("m")  # bookmark the focused row (alpha)
        rows = [t.line(y) for y in range(ROWS)]
        starred = next((r for r in rows if "▎" in r), "")
        check("▎ appears in the gutter of the bookmarked row",
              "▎" in starred and "alpha" in starred, t)
        check("only the bookmarked row is marked",
              sum("▎" in r for r in rows) == 1, t)
        # `m` leaves a flash ("bookmarked alpha") in the footer; the next key
        # dismisses it, revealing the persistent bookmark-count status.
        t.send("j")  # move focus to beta (clears the flash); alpha keeps its bar
        check("▎ persists on alpha after focus moves away", "▎" in t.dump(), t)
        check("footer shows the bookmark count once one exists",
              "▎ 1 bookmark" in t.footer(), t)
        t.send("m")  # bookmark beta too
        t.send("k")  # move up (clears flash) → count pluralizes
        check("footer pluralizes with two bookmarks",
              "2 bookmarks" in t.footer(), t)
        t.send("m")  # toggle alpha (current focus) off
        t.send("j")  # move to beta (clears flash) → back to one
        check("▎ still shown and footer back to one after removing one",
              "▎" in t.dump() and "1 bookmark" in t.footer(), t)
        t.send("m")  # remove the last bookmark (beta)
        t.send("k")  # move up (clears flash) → status clears entirely
        check("footer bookmark status clears when none remain",
              "▎" not in t.dump() and "bookmark" not in t.footer(), t)
    finally:
        t.close()


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else "target/release/jview"
    if not os.path.exists(binary):
        sys.exit(f"binary not found: {binary} (build it: cargo build --release)")
    binary = os.path.abspath(binary)

    scenario_jump_relative_and_climb(binary)
    scenario_line_editing(binary)
    scenario_aggregate_and_history(binary)
    scenario_sibling_nav(binary)
    scenario_filter(binary)
    scenario_peek(binary)
    scenario_stream_pipe(binary)
    scenario_explore(binary)
    scenario_help_overlay(binary)
    scenario_bookmark_indicator(binary)

    print()
    if FAILS:
        print(f"FAILED ({len(FAILS)}): {', '.join(FAILS)}")
        sys.exit(1)
    print("all e2e scenarios passed")


if __name__ == "__main__":
    main()
