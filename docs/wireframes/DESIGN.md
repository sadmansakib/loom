# Loom TUI design language

One thread per task. Amber is live or needs you; settled is quiet.

## Palette
- ivory #E9E3D7 text, user input
- dim #9A9285 metadata, settled
- amber #E3A857 live, attention, brand
- sage #86C19F success
- coral #EE8272 error
- violet #A99BD6 reasoning, italic
- blue #8DB4E8 tool names, paths
- teal #6FC4C4 cache, savings
- thread #8C6B33 committed thread (decor)
- dotted #5A4A2C provisional (decor)
- ground #12100E background

## Glyphs (LOOM_ASCII=1 fallback)
- ● turn node
- ✓ succeeded
- ✗ failed
- ⚠ interrupted
- ○ queued
- ⊘ cancelled
- ↻ retrying
- ⠇ working
- ▸ ▾ reasoning fold
- ├─ └─ tool batch
- ❯ prompt
- ◆ brand
- ◇ needs you (new)

## Layout
Monospace 13/20. Wide 120+: thread + 38-col sidebar. Medium 90-119: Tab opens sidebar. Narrow <90: one status line.
Chrome: header, thread, sidebar, composer (amber border), key hints. Modals amber-bordered over dim.
Meaning is never color alone: glyph + word + color.

## Regenerate
`python3 loom_wireframes.py` writes project/*.dc.html and canvas.json, then publish.

## Commands
Slash-first. Idle: the composer owns the keyboard, so actions are commands (/model /effort /account /sessions /new /context /tools /copy /export /reasoning /sidebar /help /quit; context-scoped: /retry /cancel /export /delete /add /test /remove /compact /drop /pin /permission /baseline /mark /save /edit). Inline list above the composer, filters as you type, Tab completes.
Live keys kept: Esc, ^C, ^O, up/down, up (edit last task). Prompts that take no text keep single keys (y n a e at the approval prompt).

## Compact notice
Posted to the status bar; a modal (/context) may cover it. One slot, latest wins; timer starts when the status bar is uncovered; stays until next keypress or 8 s.
