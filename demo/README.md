# Demo recording

The animated WebP at the top of the main README is generated, not hand-recorded,
so it can be refreshed whenever the UI changes.

```sh
cargo build --release                 # 1. build the binary
node demo/gen.mjs demo/big.json 1     # 2. generate a ~1 GB sample (deterministic)
brew install vhs ffmpeg webp          # 3. one-time: recorder + ffmpeg + img2webp
JVIEW_NO_ENHANCED_KEYS=1 PATH="$PWD/target/release:$PATH" vhs demo/demo.tape   # 4. -> frames/
node demo/assemble-webp.mjs           # 5. frames/ -> docs/demo.webp
rm -rf frames demo/big.json
```

- **`gen.mjs`** streams a realistically-nested JSON file (`{ users: [...], meta }`)
  to a target size in GiB. Fixed PRNG seed → byte-identical every run. Pass a
  small size like `0.02` (≈20 MB) for a quick check.
- **`demo.tape`** is a [vhs](https://github.com/charmbracelet/vhs) script: it
  types real keystrokes into the real binary and captures the result. Adjust the
  `Sleep`/key lines after the first render to taste.
- **No GIF — lossless PNG frames → animated WebP.** GIF's 256-color palette +
  dithering wreck terminal text; a `.webm` intermediate looks fine but VP9
  quantization perturbs static stretches enough that img2webp can no longer
  coalesce them, blowing the file size up. vhs's PNG-sequence output is
  byte-identical between unchanged frames, so img2webp lossless collapses every
  Sleep to a single stored frame. GitHub renders animated WebP inline.
- **`assemble-webp.mjs`** does the PNG → WebP conversion. vhs writes two
  parallel sequences into `frames/`: `frame-text-*.png` (terminal content, no
  cursor) and `frame-cursor-*.png` (transparent cursor overlay). The script
  composites them with ffmpeg's `overlay` filter, then runs img2webp lossless
  with `-d <ms>` matched to the tape's `Set Framerate`. End result keeps the
  blinking cursor at the shell prompt that the cursor-less path was dropping.
- **`JVIEW_NO_ENHANCED_KEYS=1` is required** on the vhs invocation. vhs records
  through a headless `ttyd` terminal that never answers jsonview's keyboard-
  enhancement probe, so without it jsonview stalls ~2s on a blank screen at startup
  (a real terminal replies instantly — actual users never see this). vhs passes
  its own environment down to the recorded shell, which is how the var reaches
  jsonview.

`demo/big.json` and the intermediate `frames/` are git-ignored (huge,
reproducible). Commit only the rendered `docs/demo.webp`.
