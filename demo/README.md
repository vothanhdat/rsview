# Demo recording

The animated WebP at the top of the main README is generated, not hand-recorded,
so it can be refreshed whenever the UI changes.

```sh
cargo build --release                 # 1. build the binary
node demo/gen.mjs demo/big.json 1     # 2. generate a ~1 GB sample (deterministic)
brew install vhs webp                 # 3. one-time: the recorder + gif2webp
RSVIEW_NO_ENHANCED_KEYS=1 PATH="target/release:$PATH" vhs demo/demo.tape   # 4. render -> docs/demo.gif
gif2webp -q 90 -m 6 docs/demo.gif -o docs/demo.webp && rm docs/demo.gif    # 5. -> docs/demo.webp
```

- **`gen.mjs`** streams a realistically-nested JSON file (`{ users: [...], meta }`)
  to a target size in GiB. Fixed PRNG seed → byte-identical every run. Pass a
  small size like `0.02` (≈20 MB) for a quick check.
- **`demo.tape`** is a [vhs](https://github.com/charmbracelet/vhs) script: it
  types real keystrokes into the real binary and captures the result. Adjust the
  `Sleep`/key lines after the first render to taste.
- **`RSVIEW_NO_ENHANCED_KEYS=1` is required** on the vhs invocation. vhs records
  through a headless `ttyd` terminal that never answers rsview's keyboard-
  enhancement probe, so without it rsview stalls ~2s on a blank screen at startup
  (a real terminal replies instantly — actual users never see this). vhs passes
  its own environment down to the recorded shell, which is how the var reaches
  rsview.
- **WebP, not GIF.** Lossless `gif2webp` keeps terminal text crisp (no palette
  dithering) and is smaller. GitHub renders animated WebP inline.

`demo/big.json` is git-ignored (huge, reproducible) and so is the intermediate
`docs/demo.gif`. Commit only the rendered `docs/demo.webp`.
