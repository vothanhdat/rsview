# jview demo recording — drives the real binary to a lossless PNG sequence,
# then assembles a full-color animated WebP for the README. (No GIF: 256-color
# palette + dithering wreck the text. No video intermediate either: VP9
# quantization perturbs identical-looking frames just enough that img2webp
# can't coalesce static stretches.)
#
# Regenerate:
#   cargo build --release                         # build the binary
#   node demo/gen.mjs demo/big.json 1             # ~1 GB sample (the whole pitch)
#   JVIEW_NO_ENHANCED_KEYS=1 PATH="$PWD/target/release:$PATH" vhs demo/demo.tape
#   node demo/assemble-webp.mjs                   # frames/ -> docs/demo.webp
#   rm -rf frames demo/big.json
#
# Tooling: brew install vhs ffmpeg webp
# Notes:
#   - Output is a PNG sequence in frames/. vhs writes frame-text-*.png (the
#     terminal content, no cursor) plus a parallel frame-cursor-*.png
#     transparent overlay; assemble-webp.mjs composites them with ffmpeg
#     overlay then runs img2webp lossless.
#   - Tweak Sleep/key lines after the first render — timing is best-effort.
#   - If Framerate changes here, mirror PER_FRAME_MS in assemble-webp.mjs.

Output frames/

Require jview

Set Shell bash
# Grid is ~2/3 the cols/rows (bigger, more readable text), rendered at 2x pixel
# density (FontSize, Width, Height, Padding all 2x) for crispness on hi-DPI.
Set FontSize 30
Set Width 1600
Set Height 900
Set Padding 54
Set Framerate 24
Set TypingSpeed 40ms
Set Theme "Dracula"

# 1 GB on disk...
Type "ls -lh demo/big.json"
Sleep 500ms
Enter
Sleep 600ms

# ...opens instantly. The open is captured on-camera here (no Hide). The headless
# ttyd recorder takes ~2s to first-paint a fresh process — real terminals are
# instant — so the leading frames are blank; trim them in post if needed.
Type "jview demo/big.json"
Sleep 1s
Enter
Sleep 1s

# Expand the users array and scroll through it.
Down@90ms 1
Sleep 1s
Enter
Sleep 1s
Down@90ms 12
Sleep 1.2s

# Search jumps straight to a match (cities are a known set in the data).
Type "/"
Type "Berlin"
Sleep 1200ms
Enter
Sleep 1s
Enter
Sleep 1s
Enter
Sleep 900ms
Escape
Sleep 600ms

# Goto a deep path directly.
Type ":"
Type "users[2000].address.city"
Sleep 700ms
Enter
Sleep 1.4s

# Full keymap is one keystroke away.
Type "?"
Sleep 2s
Type "q"
Sleep 1000ms

# Quit.
Type "q"
Sleep 800ms
