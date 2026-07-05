# End-to-end TUI tests

`tui_e2e.py` drives the real `jview` binary inside a pseudo-terminal and asserts
on what it actually paints, using [pyte](https://github.com/selectel/pyte) as a
headless terminal emulator. It covers the things unit tests can't reach — live
key handling, the `?` help overlay, the trimmed footer, jump/climb resolution,
and `J`/`K` sibling navigation.

```sh
cargo build --release
pip install pyte
python tests/e2e/tui_e2e.py            # uses target/release/jview
python tests/e2e/tui_e2e.py path/to/jview   # or point at a specific binary
```

Exits non-zero (and dumps the offending screen) if any scenario fails. Unix only
— it uses `pty.fork`. Run automatically in CI by `.github/workflows/ci.yml`.
