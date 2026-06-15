# Demo recording

The animated WebP at the top of the main README is generated, not hand-recorded,
so it can be refreshed whenever the UI changes.

```sh
cargo build --release                 # 1. build the binary
node demo/gen.mjs demo/big.json 1     # 2. generate a ~1 GB sample (deterministic)
brew install vhs webp                 # 3. one-time: recorder (vhs) + img2webp (webp)
RSVIEW_NO_ENHANCED_KEYS=1 PATH="target/release:$PATH" vhs demo/demo.tape   # 4. -> frames/
img2webp -loop 0 -d 20 -lossless -m 6 frames/frame-text-*.png -o docs/demo.webp   # 5. -> webp
rm -rf frames demo/big.json
```

- **`gen.mjs`** streams a realistically-nested JSON file (`{ users: [...], meta }`)
  to a target size in GiB. Fixed PRNG seed → byte-identical every run. Pass a
  small size like `0.02` (≈20 MB) for a quick check.
- **`demo.tape`** is a [vhs](https://github.com/charmbracelet/vhs) script: it
  types real keystrokes into the real binary and captures the result. Adjust the
  `Sleep`/key lines after the first render to taste.
- **No GIF — lossless PNG frames → WebP.** A GIF's 256-color palette + dithering
  wreck terminal text. We render to full-color PNG frames (`frames/`) and assemble
  a lossless animated WebP with `img2webp`. It auto-coalesces static runs and
  stores only changed rectangles, so full-color lossless still lands ~260 KB —
  smaller *and* sharper than the GIF route. `-d 20` = 50fps. GitHub renders
  animated WebP inline. Use `frame-text-*` (content); `frame-cursor-*` is just a
  transparent cursor overlay (rsview hides the cursor anyway).
- **`RSVIEW_NO_ENHANCED_KEYS=1` is required** on the vhs invocation. vhs records
  through a headless `ttyd` terminal that never answers rsview's keyboard-
  enhancement probe, so without it rsview stalls ~2s on a blank screen at startup
  (a real terminal replies instantly — actual users never see this). vhs passes
  its own environment down to the recorded shell, which is how the var reaches
  rsview.

`demo/big.json` and the intermediate `frames/` are git-ignored (huge,
reproducible). Commit only the rendered `docs/demo.webp`.
