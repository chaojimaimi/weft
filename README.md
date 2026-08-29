# Weft

A modern macOS terminal emulator with block-based command history, Metal GPU rendering, and a Warp-inspired UI.

## Features

- **Metal GPU rendering** — instance-based cell rendering with dirty-row tracking and GPU scroll blit
- **Block view** — commands and their output are grouped into navigable blocks (à la Warp)
- **Shell integration** — OSC 133 markers from zsh integration split commands into blocks
- **Find** — async regex/case-sensitive search across full scrollback (FindWorker background thread)
- **Multi-tab** — Cmd+T new tab, Cmd+W close, Cmd+Shift+[/] cycle
- **Split panes + workspaces** — nested pane layouts, save/open workspace, crash recovery
- **Themes** — 11 built-in themes (Tokyo Night, Solarized, One Dark, Gruvbox, Catppuccin, Nord, ...) + custom theme files
- **System theme follow** — auto-switch light/dark with macOS appearance
- **Settings UI** — Cmd+, opens Appearance/Font/Keybindings/Window/Local AI/Advanced settings with profiles
- **Command palette** — Cmd+P fuzzy command runner + workflow execution
- **History sidebar** — Cmd+Shift+B searchable, click-to-scroll command history
- **Context menu** — right-click a block for Copy Command / Copy Output / **Copy Block** (cwd + `$ command` + output in one paste) / Toggle Fold / Send to Input / Bookmark / Note / Export / AI Diagnose; keyboard navigation and mouse hover both track selection
- **Logo variants** — 4 Dock icon variants (Cool/Warm/Light/Transparent) switchable in Settings
- **IME + CJK** — full Unicode-width-aware glyph layout
- **Smart Select** — Cmd+Shift+Click selects URLs, paths, locations, hosts and hashes consistently in Grid/Block view; Cmd+Option+Click explicitly opens safe http(s) URLs or reveals existing local paths
- **Session restore** — tab metadata + editor drafts persisted across restarts
- **Local AI** — loopback-only Ollama command generation and failed-block diagnosis; suggestions are inserted, never auto-executed

## Performance

Interactive throughput (2026-08-17 manual 3-run avg; both columns same methodology, single-session measurement — see `docs/perf/warp-comparison/2026-08-18-baseline.md` for caveats):

| Scenario | Weft | Warp |
|----------|------|------|
| `seq 1 10000` | ~17ms | ~10ms |
| `seq 1 100000` | ~150ms | ~50ms |
| `ls -la /usr/bin` | ~7.6ms | ~5ms |

Automated baselines (2026-08-29, Apple M2 Max, macOS 26.6.2, release build — full data in `docs/perf/v1.11.9-baseline.md`): VT parse 14.6–20.6 MB/s (100k/1M lines), block visible-window render p95 ≤0.15ms at 100k lines, 10k history filter 15.3ms, offscreen Metal frame p95 ≤0.68ms, GUI idle CPU frame p95 0.48ms. Cold start ~0.39s to first frame (bundled, warm; gate protocol p95 456ms on the bare binary) — exceeded the legacy 0.30s budget set against the v1.10.3-era codebase; waiver and attribution recorded in the baseline doc, optimization queued for v1.12.

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
minimum_contrast = 7.0        # 1.0 preserves exact ANSI/truecolor RGB
follow_system = false        # auto-switch with macOS appearance

[window]
opacity = 0.92
padding_x = 2
padding_y = 1

[logo]
variant = "Cool"             # Cool | Warm | Light | Transparent

[editor]
smart_select = true           # disable semantic pointer gestures if desired
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
| Block context menu | Right-click a block |
| Copy/Paste | Cmd+C / Cmd+V |
| Smart Select | Cmd+Shift+Click |
| Open/reveal Smart target | Cmd+Option+Click |

## Shell Integration

Add to `~/.zshrc`:
```zsh
source ~/.config/weft/shell/weft.zsh
```

Enables OSC 133 markers that split commands into blocks (visible in block view + history sidebar).

## Development

```bash
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

## License

MIT
