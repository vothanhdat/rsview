# Demo recording

The GIF at the top of the main README is generated, not hand-recorded, so it can
be refreshed whenever the UI changes.

```sh
cargo build --release                 # 1. build the binary
node demo/gen.mjs demo/big.json 1     # 2. generate a ~1 GB sample (deterministic)
brew install vhs                      # 3. one-time: the recorder
PATH="target/release:$PATH" vhs demo/demo.tape   # 4. render -> docs/demo.gif
```

- **`gen.mjs`** streams a realistically-nested JSON file (`{ users: [...], meta }`)
  to a target size in GiB. Fixed PRNG seed → byte-identical every run. Pass a
  small size like `0.02` (≈20 MB) for a quick check.
- **`demo.tape`** is a [vhs](https://github.com/charmbracelet/vhs) script: it
  types real keystrokes into the real binary and captures the result. Adjust the
  `Sleep`/key lines after the first render to taste.

`demo/big.json` is git-ignored (it's huge and reproducible). Commit only the
rendered `docs/demo.gif`.
