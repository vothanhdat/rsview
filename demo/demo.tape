# rsview demo recording — drives the real binary to lossless PNG frames, then
# assembles a full-color animated WebP for the README. (No GIF: its 256-color
# palette + dithering wreck the text. img2webp auto-coalesces static runs and
# stores only changed rectangles, so lossless full-color stays ~700 KB at 3x.)
#
# Regenerate:
#   cargo build --release                         # build the binary
#   node demo/gen.mjs demo/big.json 1             # ~1 GB sample (the whole pitch)
#   RSVIEW_NO_ENHANCED_KEYS=1 PATH="target/release:$PATH" vhs demo/demo.tape
#   img2webp -loop 0 -d 20 -lossless -m 6 frames/frame-text-*.png -o docs/demo.webp
#   rm -rf frames demo/big.json
#
# Tooling: brew install vhs webp
# Notes:
#   - Output is PNG frames (frames/). frame-text-* is the content; frame-cursor-*
#     is a transparent cursor overlay — use frame-text (rsview hides the cursor).
#   - -d 20 = 50fps. Tweak Sleep/keys after the first render — timing is best-effort.

Output frames/

Require rsview

Set Shell bash
# Grid is ~2/3 the cols/rows (bigger, more readable text), rendered at 3x pixel
# density (FontSize, Width, Height, Padding all 3x) for crispness on hi-DPI.
Set FontSize 45
Set Width 2360
Set Height 1320
Set Padding 54
Set Framerate 50
Set TypingSpeed 55ms
Set Theme "Dracula"

# This tape MUST be invoked with RSVIEW_NO_ENHANCED_KEYS=1 (see header). vhs
# passes its own env down to the recorded shell, so rsview skips its keyboard-
# enhancement probe — a headless ttyd terminal never answers that probe, so
# otherwise rsview stalls ~2s on a blank screen at startup (real terminals
# reply instantly, so this never affects actual users).

# 1 GB on disk...
Type "ls -lh demo/big.json"
Enter
Sleep 1.5s

# ...opens instantly.
Type "rsview demo/big.json"
Enter
Sleep 1.5s

# Expand the users array and scroll through it.
Down@200ms 1
Enter
Sleep 1s
Down@90ms 12
Sleep 1.2s

# Search jumps straight to a match (cities are a known set in the data).
Type "/"
Type "Berlin"
Sleep 800ms
Enter
Sleep 1s
Type "n"
Sleep 700ms
Type "n"
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
Sleep 600ms

# Quit.
Type "q"
Sleep 800ms
