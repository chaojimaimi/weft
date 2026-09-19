#!/usr/bin/env python3
"""import-themes.py 的离线 fixture 测试（不联网）。

覆盖两条解析路径（wezterm TOML / base16-24 YAML）、variant 判定、
输出文件的 TOML 合法性（tomllib 可解析，即 ThemeConfig 可反序列化）、
以及 CREDITS/manifest 产出。

用法：python3 scripts/test_import_themes.py
"""

from __future__ import annotations

import json
import sys
import tempfile
import tomllib
from importlib import util
from pathlib import Path

# 文件名带连字符，无法直接 import —— 按路径加载。
_script = Path(__file__).resolve().parent / "import-themes.py"
_spec = util.spec_from_file_location("import_themes", _script)
assert _spec and _spec.loader
it = util.module_from_spec(_spec)
sys.modules["import_themes"] = it  # dataclass 需要模块已注册
_spec.loader.exec_module(it)

# 上游实测格式（mbadolato/iTerm2-Color-Schemes, wezterm/Dracula.toml）
WEZTERM_DRACULA = """
# Dracula
[colors]
foreground = "#f8f8f2"
background = "#282a36"
cursor_bg = "#f8f8f2"
cursor_border = "#f8f8f2"
cursor_fg = "#282a36"
selection_bg = "#44475a"
selection_fg = "#ffffff"

ansi = ["#21222c","#ff5555","#50fa7b","#f1fa8c","#bd93f9","#ff79c6","#8be9fd","#f8f8f2"]
brights = ["#6272a4","#ff6e6e","#69ff94","#ffffa5","#d6acff","#ff92df","#a4ffff","#ffffff"]
"""

# 上游实测格式（tinted-theming/schemes, base16/dracula.yaml）
BASE16_DRACULA = """
system: "base16"
name: "Dracula"
author: "clach04 (https://github.com/clach04)"
variant: "dark"
palette:
  base00: "#282a36"
  base01: "#21222c"
  base02: "#44475A"
  base03: "#6272a4"
  base04: "#9ea8c7"
  base05: "#f8f8f2"
  base06: "#f8f8f2"
  base07: "#ffffff"
  base08: "#ff5555"
  base09: "#FFB86C"
  base0A: "#f1fa8c"
  base0B: "#50fa7b"
  base0C: "#8be9fd"
  base0D: "#bd93f9"
  base0E: "#ff79c6"
  base0F: "#993333"
"""

# base24：bright 槽位独立（base10-base17）
BASE24_SAMPLE = """
system: "base24"
name: "Sample24"
author: "tester"
variant: "light"
palette:
  base00: "#ffffff"
  base01: "#e0e0e0"
  base02: "#d0d0d0"
  base03: "#808080"
  base04: "#707070"
  base05: "#101010"
  base06: "#000000"
  base07: "#ffffff"
  base08: "#d20f39"
  base09: "#fe640b"
  base0A: "#df8e1d"
  base0B: "#40a02b"
  base0C: "#179299"
  base0D: "#04a5e5"
  base0E: "#8839ef"
  base0F: "#7287fd"
  base10: "#000000"
  base11: "#ff1111"
  base12: "#22ff22"
  base13: "#ffff22"
  base14: "#2222ff"
  base15: "#ff22ff"
  base16: "#22ffff"
  base17: "#ffffff"
"""


def check(results: list[tuple[str, bool, str]], label: str, ok: bool, detail: str = "") -> None:
    results.append((label, ok, detail))


def main() -> int:
    results: list[tuple[str, bool, str]] = []

    # ── wezterm 解析 ──
    w = it.parse_wezterm(WEZTERM_DRACULA, "Dracula")
    check(results, "wezterm/parse", w is not None)
    assert w is not None
    check(results, "wezterm/fg-bg",
          w.foreground == "#f8f8f2" and w.background == "#282a36",
          f"{w.foreground} {w.background}")
    check(results, "wezterm/ansi-len", len(w.ansi) == 16, str(len(w.ansi)))
    check(results, "wezterm/ansi-order",
          w.ansi[0] == "#21222c" and w.ansi[1] == "#ff5555" and w.ansi[8] == "#6272a4",
          str(w.ansi[:2] + [w.ansi[8]]))
    check(results, "wezterm/cursor-selection",
          w.cursor == "#f8f8f2" and w.selection == "#44475a",
          f"{w.cursor} {w.selection}")

    # ── base16 解析 ──
    b = it.parse_base_yaml(BASE16_DRACULA, "dracula")
    check(results, "base16/parse", b is not None)
    assert b is not None
    # 官方 terminal 模板映射：0=base01 1=base08 2=base0B 3=base0A 4=base0D 7=base05
    check(results, "base16/ansi-map",
          [b.ansi[0], b.ansi[1], b.ansi[2], b.ansi[3], b.ansi[4]]
          == ["#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9"],
          str(b.ansi[:5]))
    check(results, "base16/bright-white-is-base07", b.ansi[15] == "#ffffff", b.ansi[15])
    check(results, "base16/variant-metadata", b.variant == "dark", str(b.variant))
    check(results, "base16/author-metadata", (b.author or "").startswith("clach04"), str(b.author))
    check(results, "base16/selection-is-base02", b.selection == "#44475a", str(b.selection))

    # ── base24 bright 覆盖 ──
    b24 = it.parse_base_yaml(BASE24_SAMPLE, "Sample24")
    check(results, "base24/parse", b24 is not None)
    assert b24 is not None
    check(results, "base24/brights",
          b24.ansi[8] == "#000000" and b24.ansi[11] == "#ffff22",
          str(b24.ansi[8:12]))
    check(results, "base24/variant-metadata", b24.variant == "light", str(b24.variant))

    # ── 亮度/variant 判定（与 Rust Theme::is_dark 同阈值）──
    check(results, "luminance/dark", it.relative_luminance("#282a36") < 0.5)
    check(results, "luminance/light", it.relative_luminance("#ffffff") > 0.5)

    # ── 输出渲染可被 TOML 解析（即 ThemeConfig 可反序列化）──
    w.license = "MIT"
    rendered = it.render_scheme(w)
    doc = tomllib.loads(rendered)
    check(results, "render/toml-parseable", True)
    check(results, "render/keys",
          set(doc) >= {"name", "variant", "background", "foreground", "palette"},
          str(sorted(doc)))
    check(results, "render/palette-16", len(doc.get("palette", [])) == 16,
          str(len(doc.get("palette", []))))
    check(results, "render/variant-dark", doc.get("variant") == "dark", str(doc.get("variant")))

    # ── 写盘产物 ──
    with tempfile.TemporaryDirectory() as tmp:
        out = Path(tmp) / "themes"
        it.write_outputs([w, b24], out, "MIT")
        check(results, "write/files", (out / "dracula.toml").exists() and (out / "sample24.toml").exists())
        manifest = json.loads((out / "manifest.json").read_text())
        check(results, "write/manifest-count", len(manifest) == 2, str(len(manifest)))
        credits = (out / "CREDITS.md").read_text()
        check(results, "write/credits-contains-themes",
              "Dracula" in credits and "Sample24" in credits)
        # 每个生成文件都必须能被 TOML 解析
        ok_all = True
        for path in out.glob("*.toml"):
            try:
                tomllib.loads(path.read_text())
            except Exception as error:  # noqa: BLE001
                ok_all = False
                check(results, f"write/parse-{path.name}", False, str(error))
        check(results, "write/all-toml-parseable", ok_all)

    # ── rebuild-index：分批导入后从文件内嵌元数据重建完整署名 ──
    with tempfile.TemporaryDirectory() as tmp:
        out = Path(tmp) / "themes"
        out.mkdir()
        (out / "alpha.toml").write_text(
            'name = "Alpha"\nvariant = "dark"\nauthor = "Someone"\n'
            'background = "#101010"\nforeground = "#eeeeee"\npalette = ["#101010"]\n')
        (out / "beta.toml").write_text(
            'name = "Beta"\nbackground = "#fafafa"\nforeground = "#111111"\n'
            'palette = ["#fafafa"]\n')
        check(results, "rebuild/rc", it.rebuild_index(out) == 0)
        manifest = json.loads((out / "manifest.json").read_text())
        check(results, "rebuild/count", len(manifest) == 2, str(len(manifest)))
        credits = (out / "CREDITS.md").read_text()
        check(results, "rebuild/credits-merged", "Alpha" in credits and "Beta" in credits)
        check(results, "rebuild/author-preserved",
              any(e["name"] == "Alpha" and e["author"] == "Someone" for e in manifest))
        # 缺 variant 时由背景亮度补推（#fafafa → light）
        check(results, "rebuild/variant-from-bg",
              any(e["name"] == "Beta" and e["variant"] == "light" for e in manifest),
              str([(e["name"], e["variant"]) for e in manifest]))

    # ── slug ──
    check(results, "slug", it.slugify("Tokyo Night Storm") == "tokyo-night-storm",
          it.slugify("Tokyo Night Storm"))

    failures = [r for r in results if not r[1]]
    for label, ok, detail in results:
        print(f"  {'PASS' if ok else 'FAIL'}  {label}" + (f"  — {detail}" if detail and not ok else ""))
    print(f"\n{len(results) - len(failures)}/{len(results)} passed")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
