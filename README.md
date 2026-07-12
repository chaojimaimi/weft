# Weft

A modern macOS terminal emulator with block-based command history, Metal GPU rendering, and a Warp-inspired UI.

## Features

- **Metal GPU rendering** — instance-based cell rendering with dirty-row tracking and GPU scroll blit
- **Block view** — commands and their output are grouped into navigable blocks (à la Warp)
- **Shell integration** — OSC 133 markers from zsh integration split commands into blocks
- **Find** — async regex/case-sensitive search across full scrollback (FindWorker background thread)
- **Multi-tab** — Cmd+T new tab, Cmd+W close, Cmd+Shift+[/] cycle
- **Themes** — 11 built-in themes (Tokyo Night, Solarized, One Dark, Gruvbox, Catppuccin, Nord, ...) + custom theme files
- **System theme follow** — auto-switch light/dark with macOS appearance
- **Settings UI** — Cmd+, opens panel with Appearance/Font/Keybindings/Window/Logo tabs
- **Command palette** — Cmd+P fuzzy command runner + workflow execution
- **History sidebar** — Cmd+Shift+B searchable, click-to-scroll command history
- **Context menu** — F7 for Copy Command/Toggle Fold/Send to Input
- **Logo variants** — 4 Dock icon variants (Cool/Warm/Light/Transparent) switchable in Settings
- **IME + CJK** — full Unicode-width-aware glyph layout
- **Session restore** — tab metadata + editor drafts persisted across restarts

## Performance

Benchmarks (3-run avg, vs Warp reference):

| Scenario | Weft | Warp |
|----------|------|------|
| `seq 1 10000` | ~17ms | ~10ms |
| `seq 1 100000` | ~150ms | ~50ms |
| `ls -la /usr/bin` | ~7.6ms | ~5ms |

Optimizations: CPU VT parser fast-path (ASCII batch), GPU instance rendering with triple-buffered vertex ring, dirty-rect culling, glyph atlas ASCII direct-index.

## Build

```bash
cargo build --release
```

The binary is `target/release/weft`.

## Install

### From source (development / personal use)

```bash
./scripts/build-app.sh          # builds Weft.app (Ad-hoc signed)
open target/release/osx/Weft.app
```

First launch: right-click → Open to bypass Gatekeeper (Ad-hoc signature is not trusted by default).

### From release

Download the latest `Weft-*.zip` from [Releases](https://github.com/chaojimaimi/weft/releases), unzip, drag `Weft.app` to `/Applications`.

> **Note**: Public distribution requires an Apple Developer certificate for notarization. Until then, the app ships Ad-hoc signed and users must right-click → Open on first launch.

## Configuration

Config file: `~/.config/weft/config.toml`

```toml
[font]
family = "Menlo"
size = 13.0
line_height = 1.2

[theme]
name = "weft-warm"           # 11 built-in themes + custom files
follow_system = false        # auto-switch with macOS appearance

[window]
opacity = 0.92
padding_x = 2
padding_y = 1

[logo]
variant = "Cool"             # Cool | Warm | Light | Transparent
```

Settings UI (Cmd+,) writes changes back to disk, preserving comments.

## Keyboard Shortcuts

| Action | Shortcut |
|--------|----------|
| New tab | Cmd+T |
| Close tab | Cmd+W |
| Next/prev tab | Cmd+Shift+] / Cmd+Shift+[ |
| Command palette | Cmd+P |
| Find | Cmd+F |
| Toggle regex mode | Cmd+R |
| History sidebar | Cmd+Shift+B |
| Settings | Cmd+, |
| Toggle theme | Cmd+Shift+T |
| Context menu | F7 |
| Copy/Paste | Cmd+C / Cmd+V |

## Shell Integration

Add to `~/.zshrc`:
```zsh
source ~/.config/weft/shell/weft.zsh
```

Enables OSC 133 markers that split commands into blocks (visible in block view + history sidebar).

## Development

```bash
cargo test --workspace          # 654 tests
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

## License

MIT
