# rsview demo recording — drives the real binary, renders a GIF, then we
# convert it to an animated WebP (smaller, sharper) for the README.
#
# Regenerate:
#   cargo build --release                         # build the binary
#   node demo/gen.mjs demo/big.json 1             # ~1 GB sample (the whole pitch)
#   RSVIEW_NO_ENHANCED_KEYS=1 PATH="target/release:$PATH" vhs demo/demo.tape
#   gif2webp -q 70 -lossy docs/demo.gif -o docs/demo.webp && rm docs/demo.gif
#
# vhs: https://github.com/charmbracelet/vhs  (brew install vhs)
# gif2webp: brew install webp
# Tweak Sleep/keys after the first render — timing is best-effort.

Output docs/demo.gif

Require rsview

Set Shell bash
Set FontSize 15
Set Width 1180
Set Height 660
Set Padding 18
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
