#!/usr/bin/env python3
"""End-to-end TUI tests for rsview.

Drives the real binary inside a pseudo-terminal and asserts on what it paints,
using pyte as a headless terminal emulator. This is the kind of thing unit tests
can't reach — actual key handling, sticky headers, overlays, and the footer.

Usage:
    python tests/e2e/tui_e2e.py [path-to-rsview]      # default: target/release/rsview

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
    """A running rsview in a pty, with a pyte screen mirroring its output."""

    def __init__(self, binary, doc):
        self.path = f"/tmp/rsview_e2e_{os.getpid()}.json"
        with open(self.path, "w") as f:
            json.dump(doc, f)
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.ByteStream(self.screen)
        self.pid, self.fd = pty.fork()
        if self.pid == 0:  # child
            os.execv(binary, [binary, self.path])
            os._exit(127)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.pump(0.6)  # initial render

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
        t.send("j")  # any key closes it
        check("any key closes the overlay", "keyboard shortcuts" not in t.dump(), t)
    finally:
        t.close()


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else "target/release/rsview"
    if not os.path.exists(binary):
        sys.exit(f"binary not found: {binary} (build it: cargo build --release)")
    binary = os.path.abspath(binary)

    scenario_jump_relative_and_climb(binary)
    scenario_sibling_nav(binary)
    scenario_help_overlay(binary)

    print()
    if FAILS:
        print(f"FAILED ({len(FAILS)}): {', '.join(FAILS)}")
        sys.exit(1)
    print("all e2e scenarios passed")


if __name__ == "__main__":
    main()
