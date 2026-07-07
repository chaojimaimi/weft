# Weft

A modern macOS terminal emulator with block-based command history, Metal GPU rendering, and a Warp-inspired UI.

## Features

- **Metal GPU rendering** — instance-based cell rendering with dirty-row tracking and GPU scroll blit
- **Block view** — commands and their output are grouped into navigable blocks (à la Warp)
- **Shell integration** — OSC 133 markers from zsh integration split commands into blocks
- **Find** — async regex/case-sensitive search across full scrollback
- **Multi-tab + splits** — Cmd+T new tab, Cmd+D split, Cmd+[/] cycle
- **Themes** — Tokyo Night, Solarized, One Dark, Gruvbox, Catppuccin, and more
- **System theme follow** — auto-switch light/dark with macOS appearance
- **Command palette** — Cmd+P fuzzy command runner
- **History sidebar** — searchable, click-to-scroll command history
- **IME + CJK** — full Unicode-width-aware glyph layout

## Build

```bash
cargo build --release
```

The binary is `target/release/weft`.

## License

MIT
