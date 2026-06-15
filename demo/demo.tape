# rsview demo recording — drives the real binary and renders docs/demo.gif.
#
# Regenerate:
#   cargo build --release                 # build the binary
#   node demo/gen.mjs demo/big.json 1     # ~1 GB sample (the whole pitch)
#   PATH="target/release:$PATH" vhs demo/demo.tape
#
# vhs: https://github.com/charmbracelet/vhs  (brew install vhs)
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

# 1 GB on disk...
Type "ls -lh demo/big.json"
Enter
Sleep 1.5s

# ...opens instantly.
Type "rsview demo/big.json"
Enter
Sleep 2.5s

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
