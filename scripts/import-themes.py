#!/usr/bin/env python3
"""把开源终端主题转档为 Weft 主题文件（TOML，schema = [theme] 段）。

数据源
  iterm2 : mbadolato/iTerm2-Color-Schemes 的 wezterm/*.toml（606 套，MIT 集合许可，
           单主题版权归各自作者 —— 见仓库 CREDITS.md）
  tinted : tinted-theming/schemes 的 base16/base24 YAML（含 variant/author 元数据）

输出
  <out>/<slug>.toml   只写"终端兼容层"字段（fg/bg/cursor/selection/palette 0-15）+
                      元数据（variant/author/source/license）。Weft 语义层 21 个角色
                      由 Rust 侧 Theme::from_import 推导（config/theme_import.rs），
                      脚本不落任何推导色值 —— 单一事实来源。
  <out>/CREDITS.md    署名清单（MIT 集合许可要求保留上游署名）
  <out>/manifest.json 导入清单（含每套主题的 variant，便于人工复核）

用法
  # 联网抓取（默认 iTerm2 的 wezterm 目录）
  python3 scripts/import-themes.py --limit 20
  # 离线：指向已 clone 的仓库
  python3 scripts/import-themes.py --from-dir /path/to/iTerm2-Color-Schemes --source iterm2
  python3 scripts/import-themes.py --from-dir /path/to/schemes --source tinted

依赖：仅标准库（tomllib 解析 wezterm TOML；base16 YAML 用极简解析器）。
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import time
import unicodedata
import urllib.request
from urllib.parse import quote
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

ITerm2Repo = "mbadolato/iTerm2-Color-Schemes"
TintedRepo = "tinted-theming/schemes"
UserAgent = {"User-Agent": "weft-theme-import/1.0"}

# base16/base24 → xterm ANSI 0-15（tinted-theming 官方 terminal 模板映射）
Base16ToAnsi = [
    "base01", "base08", "base0B", "base0A", "base0D", "base0E", "base0C", "base05",
    "base03", "base08", "base0B", "base0A", "base0D", "base0E", "base0C", "base07",
]
Base24Brights = [f"base{i:02X}" for i in range(0x10, 0x18)]


@dataclass
class Scheme:
    name: str
    foreground: str
    background: str
    cursor: str | None = None
    selection: str | None = None
    ansi: list[str] = field(default_factory=list)
    variant: str | None = None
    author: str | None = None
    source: str | None = None
    license: str | None = None


# ── 颜色工具（与 Rust config::theme_import 保持一致）───────────────────

def _srgb_to_linear(channel: float) -> float:
    return channel / 12.92 if channel <= 0.03928 else ((channel + 0.055) / 1.055) ** 2.4


def relative_luminance(hex_color: str) -> float:
    h = hex_color.lstrip("#")
    r, g, b = (int(h[i:i + 2], 16) / 255.0 for i in (0, 2, 4))
    return 0.2126 * _srgb_to_linear(r) + 0.7152 * _srgb_to_linear(g) + 0.0722 * _srgb_to_linear(b)


def normalize_hex(value: str) -> str | None:
    v = (value or "").strip().strip("'\"")
    if not v.startswith("#"):
        return None
    v = v[1:]
    if len(v) == 3:
        v = "".join(c * 2 for c in v)
    if len(v) != 6 or any(c not in "0123456789abcdefABCDEF" for c in v):
        return None
    return "#" + v.lower()


def slugify(name: str) -> str:
    """文件名 slug：先做 NFKD 去重音（Rosé → rose），再转小写连字符。"""
    s = unicodedata.normalize("NFKD", name.strip())
    s = "".join(c for c in s if not unicodedata.combining(c)).lower()
    s = re.sub(r"[^a-z0-9]+", "-", s)
    return s.strip("-") or "theme"


# ── 解析：wezterm TOML ─────────────────────────────────────────────────

def parse_wezterm(text: str, fallback_name: str) -> Scheme | None:
    import tomllib

    data = tomllib.loads(text)
    colors = data.get("colors") or {}
    fg = normalize_hex(colors.get("foreground", ""))
    bg = normalize_hex(colors.get("background", ""))
    if not fg or not bg:
        return None
    ansi = [normalize_hex(c) for c in colors.get("ansi", [])]
    brights = [normalize_hex(c) for c in colors.get("brights", [])]
    if len(ansi) != 8:
        return None
    # 缺 brights 时退化：用 normal 复制一份（终端常见做法）
    if len(brights) != 8:
        brights = list(ansi)
    palette = [c for c in ansi + brights if c]
    if len(palette) != 16:
        return None
    return Scheme(
        name=str(data.get("name") or fallback_name),
        foreground=fg,
        background=bg,
        cursor=normalize_hex(colors.get("cursor_bg", "")),
        selection=normalize_hex(colors.get("selection_bg", "")),
        ansi=palette,
    )


# ── 解析：base16/base24 YAML（极简，只认顶层标量 + palette 块）─────────

def parse_base_yaml(text: str, fallback_name: str) -> Scheme | None:
    top: dict[str, str] = {}
    palette: dict[str, str] = {}
    current: dict[str, str] = top
    for raw in text.splitlines():
        # 只在 " #" 处剥离注释——色值 `#282a36` 里的 # 必须保留。
        line = re.sub(r"\s#.*$", "", raw).rstrip()
        if not line.strip() or "#" == line.strip()[:1]:
            continue
        indented = line[:1].isspace()
        stripped = line.strip()
        if ":" not in stripped:
            continue
        key, _, value = stripped.partition(":")
        key = key.strip()
        value = value.strip()
        if indented and current is palette:
            palette[key] = value
        elif key == "palette" and not value:
            current = palette
        else:
            top[key] = value
    if not palette:
        return None
    bg = normalize_hex(palette.get("base00", ""))
    fg = normalize_hex(palette.get("base05", ""))
    if not bg or not fg:
        return None

    ansi: list[str] = []
    for slot in Base16ToAnsi:
        color = normalize_hex(palette.get(slot, ""))
        ansi.append(color or fg)
    if "base10" in palette:  # base24：bright 槽位独立
        for index, slot in enumerate(Base24Brights):
            color = normalize_hex(palette.get(slot, ""))
            if color:
                ansi[8 + index] = color
    cursor = normalize_hex(palette.get("base05", "")) or fg
    selection = normalize_hex(palette.get("base02", "")) or bg

    variant = (top.get("variant") or "").strip().strip("\"'") or None
    if variant not in ("dark", "light"):
        variant = None
    return Scheme(
        name=(top.get("name") or fallback_name).strip().strip("\"'"),
        foreground=fg,
        background=bg,
        cursor=cursor,
        selection=selection,
        ansi=ansi,
        variant=variant,
        author=(top.get("author") or "").strip().strip("\"'") or None,
    )


# ── 数据获取 ───────────────────────────────────────────────────────────

def _get(url: str, retries: int = 3) -> bytes:
    """带重试的 GET —— 经过代理时大响应可能被截断（IncompleteRead）。"""
    last_error: Exception | None = None
    for attempt in range(retries):
        try:
            request = urllib.request.Request(url, headers=UserAgent)
            with urllib.request.urlopen(request, timeout=60) as response:
                return response.read()
        except Exception as error:  # 网络抖动/截断，退避后重试
            last_error = error
            if attempt + 1 < retries:
                time.sleep(0.5 * (attempt + 1))
    raise RuntimeError(f"GET {url} failed: {last_error}")


def fetch_tree(repo: str, path: str, branch: str) -> list[str]:
    """用 git trees API 列目录（比 contents API 小得多，不易被截断）。"""
    data = json.loads(_get(f"https://api.github.com/repos/{repo}/git/trees/{branch}:{path}"))
    if not isinstance(data.get("tree"), list):
        return []
    return [entry["path"] for entry in data["tree"]
            if entry.get("type") == "blob"
            and entry["path"].endswith((".toml", ".yml", ".yaml"))]


def fetch_github(repo: str, paths: list[str], branch: str, parser, source_prefix: str,
                 limit: int | None) -> list[Scheme]:
    schemes: list[Scheme] = []
    targets: list[tuple[str, str]] = []
    for directory in paths:
        for name in fetch_tree(repo, directory, branch):
            # 上游文件名大量含空格（"3024 Day.toml"），必须百分号编码。
            url = f"https://raw.githubusercontent.com/{repo}/{branch}/{directory}/{quote(name)}"
            targets.append((f"{source_prefix}/{directory}/{name}", url))
    if limit:
        targets = targets[:limit]

    def load(item: tuple[str, str]) -> Scheme | None:
        label, url = item
        try:
            text = _get(url).decode("utf-8", errors="replace")
            scheme = parser(text, Path(label).stem)
            if scheme:
                scheme.source = label
            return scheme
        except Exception as error:  # 单文件失败不应中断批量导入
            print(f"  ! skip {label}: {error}", file=sys.stderr)
            return None

    with ThreadPoolExecutor(max_workers=4) as pool:
        for scheme in pool.map(load, targets):
            if scheme:
                schemes.append(scheme)
    return schemes


def load_local(root: Path, directories: list[str], parser, source_prefix: str,
               limit: int | None) -> list[Scheme]:
    schemes: list[Scheme] = []
    files: list[Path] = []
    for directory in directories:
        base = root / directory
        if not base.is_dir():
            continue
        files.extend(sorted(p for p in base.iterdir()
                            if p.suffix in (".toml", ".yml", ".yaml")))
    if limit:
        files = files[:limit]
    for path in files:
        try:
            scheme = parser(path.read_text(encoding="utf-8", errors="replace"), path.stem)
            if scheme:
                scheme.source = f"{source_prefix}/{path.parent.name}/{path.name}"
                schemes.append(scheme)
        except Exception as error:
            print(f"  ! skip {path}: {error}", file=sys.stderr)
    return schemes


# ── 输出 ───────────────────────────────────────────────────────────────

def toml_escape(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"')


def render_scheme(scheme: Scheme) -> str:
    lines = [f'name = "{toml_escape(scheme.name)}"']
    variant = scheme.variant or ("light" if relative_luminance(scheme.background) > 0.5 else "dark")
    lines.append(f'variant = "{variant}"')
    for key, value in (("author", scheme.author), ("source", scheme.source),
                       ("license", scheme.license)):
        if value:
            lines.append(f'{key} = "{toml_escape(value)}"')
    lines.append("")
    lines.append(f'background = "{scheme.background}"')
    lines.append(f'foreground = "{scheme.foreground}"')
    if scheme.cursor:
        lines.append(f'cursor = "{scheme.cursor}"')
    if scheme.selection:
        lines.append(f'selection = "{scheme.selection}"')
    palette = ", ".join(f'"{color}"' for color in scheme.ansi)
    lines.append(f"palette = [{palette}]")
    return "\n".join(lines) + "\n"


def write_index(entries: list[dict], out_dir: Path, collection_license: str | None) -> None:
    """写 manifest.json + CREDITS.md。元数据来自各主题文件内嵌字段。"""
    (out_dir / "manifest.json").write_text(
        json.dumps(entries, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")

    license_note = (
        f"集合许可：{collection_license}；**单个主题的版权归各自作者**（见下表）。"
        if collection_license
        else "**单个主题的版权归各自作者**（见下表）。"
    )
    credits = ["# 主题署名", "",
               "本目录下的主题由 `scripts/import-themes.py` 从上游转档而来。",
               license_note, ""]
    credits.append("| 主题 | 变体 | 作者 | 来源 | 许可 |")
    credits.append("| --- | --- | --- | --- | --- |")
    for entry in sorted(entries, key=lambda e: (e["name"] or "").lower()):
        credits.append("| {name} | {variant} | {author} | {source} | {license} |".format(
            name=entry["name"], variant=entry["variant"] or "—", author=entry["author"] or "—",
            source=entry["source"] or "—", license=entry["license"] or "—"))
    (out_dir / "CREDITS.md").write_text("\n".join(credits) + "\n", encoding="utf-8")


def write_outputs(schemes: list[Scheme], out_dir: Path, collection_license: str) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    manifest = []
    for scheme in schemes:
        slug = slugify(scheme.name)
        target = out_dir / f"{slug}.toml"
        # 同名冲突（不同上游的同名主题）加后缀，避免静默覆盖
        suffix = 2
        while target.exists():
            target = out_dir / f"{slug}-{suffix}.toml"
            suffix += 1
        if scheme.license is None:
            scheme.license = collection_license
        target.write_text(render_scheme(scheme), encoding="utf-8")
        manifest.append({
            "name": scheme.name,
            "file": target.name,
            "variant": scheme.variant
            or ("light" if relative_luminance(scheme.background) > 0.5 else "dark"),
            "author": scheme.author,
            "source": scheme.source,
            "license": scheme.license,
        })
    write_index(manifest, out_dir, collection_license)


def rebuild_index(out_dir: Path) -> int:
    """不导入、只根据目录内主题文件的内嵌元数据重建 manifest.json + CREDITS.md。

    多次分批导入（例如先 tinted 再 iTerm2）后调用一次，署名清单才是完整的 ——
    否则后一次导入会把它自己的清单覆盖到整个目录上。
    """
    import tomllib

    entries: list[dict] = []
    for path in sorted(out_dir.glob("*.toml")):
        try:
            doc = tomllib.loads(path.read_text(encoding="utf-8"))
        except Exception as error:  # noqa: BLE001
            print(f"  ! skip {path.name}: {error}", file=sys.stderr)
            continue
        background = doc.get("background", "#000000")
        entries.append({
            "name": doc.get("name", path.stem),
            "file": path.name,
            "variant": doc.get("variant")
            or ("light" if relative_luminance(background) > 0.5 else "dark"),
            "author": doc.get("author"),
            "source": doc.get("source"),
            "license": doc.get("license"),
        })
    if not entries:
        print("目录里没有可索引的 .toml 主题。", file=sys.stderr)
        return 1
    write_index(entries, out_dir, None)
    print(f"✓ 重建索引：{len(entries)} 套主题 → {out_dir}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--source", choices=("iterm2", "tinted"), default="iterm2")
    parser.add_argument("--from-dir", type=Path, help="已 clone 的上游仓库（离线模式）")
    parser.add_argument("--out", type=Path, default=Path("assets/themes"))
    parser.add_argument("--limit", type=int, help="只处理前 N 个（冒烟用）")
    parser.add_argument("--rebuild-index", action="store_true",
                        help="不导入，仅按目录内主题文件的内嵌元数据重建 CREDITS.md/manifest.json")
    args = parser.parse_args()

    if args.rebuild_index:
        return rebuild_index(args.out)

    if args.source == "iterm2":
        directories = ["wezterm"]
        parser_fn = parse_wezterm
        prefix, repo, branch, lic = "iTerm2-Color-Schemes", ITerm2Repo, "master", "MIT"
    else:
        directories = ["base16", "base24"]
        parser_fn = parse_base_yaml
        prefix, repo, branch, lic = "tinted-theming/schemes", TintedRepo, "spec-0.11", "MIT"

    print(f"→ source={args.source} ({prefix}) limit={args.limit or 'all'}")
    if args.from_dir:
        schemes = load_local(args.from_dir, directories, parser_fn, prefix, args.limit)
    else:
        schemes = fetch_github(repo, directories, branch, parser_fn, prefix, args.limit)

    if not schemes:
        print("没有解析到任何主题，检查数据源或 --from-dir 路径。", file=sys.stderr)
        return 1

    write_outputs(schemes, args.out, lic)
    dark = sum(1 for s in schemes
               if (s.variant or ("light" if relative_luminance(s.background) > 0.5 else "dark")) == "dark")
    print(f"✓ 写出 {len(schemes)} 套主题 → {args.out}")
    print(f"  dark={dark} light={len(schemes) - dark}")
    print(f"  署名清单：{args.out / 'CREDITS.md'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
