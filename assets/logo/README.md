# Weft Logo 资产

> 设计概念："Woven Grid" — 深色底 + 霓虹青 W + 编织网格点阵
> 设计日期：2026-07-04
> 状态：备用（未接入代码）

## 色调说明

本批 Logo 为**冷色青调**：

| 角色 | 色值 | 说明 |
|------|------|------|
| 背景 | `#0b0e14` | 冷蓝黑底 |
| W 主色 | `#20f0e0` | 霓虹青 |
| 网格线 | `#28c4b4` | 低透明度青 |
| 网格点 | `#40e0d0` | 交叉点亮点 |

weft v0.8 视觉方向为 **"Quiet × Warm"**（暖棕底 `#221c18` + 琥珀 accent `#d4a574`），
两者色调不一致。当前作为备用资产保存，发布前需评估是否：

- **(a)** 直接使用（Logo 与终端内界面可风格独立，冷色图标在 Dock 里更显科技感）
- **(b)** 基于本 SVG 重做暖色版本（统一品牌色调）

## 文件清单

```
assets/logo/
├── weft-icon.svg              # 源矢量（viewBox 1024×1024，无限缩放）
├── png/                       # 各尺寸栅格化（app 图标发布用）
│   ├── weft-icon-32.png       # 小尺寸：W 剪影可辨，网格细节丢失
│   ├── weft-icon-64.png
│   ├── weft-icon-128.png
│   ├── weft-icon-256.png
│   ├── weft-icon-512.png
│   ├── weft-icon-1024.png
│   └── weft-icon-2048.png
├── jpg/                       # 设计预览对比用（preview.html 引用）
└── preview/                   # 设计师自验页面
    ├── preview.html           # 原始概念 vs 矢量版对比
    └── verify.html            # SVG 在浏览器各尺寸下的实时缩放验证
```

> **注**：原始交付包含 7 个尺寸副本的 SVG，但它们与 `weft-icon.svg` 字节完全相同
>（矢量无尺寸概念），已去重为单一源文件。

## SVG 结构

- **viewBox**: `0 0 1024 1024`（圆角方形 `rx=224`，macOS 标准 app icon 形态）
- **背景**: `#0b0e14` 实色圆角矩形
- **网格**: 14×14 线（`stroke #28c4b4` opacity 0.4）+ 196 个交叉点圆（`r=3 #40e0d0`）
- **W**: 单条 polyline 路径，6 层叠加模拟霓虹灯管（外发光 → 管体 → 高光）
- **滤镜**: 6 级 `feGaussianBlur` 发光效果，纯标准 SVG，无外部依赖

## 发布时接入路径（尚未实现）

项目当前无打包基础设施（裸 CLI 二进制，无 `.app` bundle）。接入图标有两条路径：

### 1. 运行时窗口图标（简单）

```rust
// crates/weft_app/src/main.rs — resumed() 中
use winit::window::Icon;
let icon_bytes = include_bytes!("../../assets/logo/png/weft-icon-256.png");
let img = image::load_from_memory(icon_bytes)?.to_rgba8();
let icon = Icon::from_rgba(img.into_raw(), 256)?;
window_attrs = window_attrs.with_window_icon(Some(icon));
```

影响：标题栏 / 任务切换器图标。非 macOS Dock `.app` 图标。

### 2. macOS .app bundle 图标（需打包基础设施）

```bash
# 从 PNG 生成 .icns
mkdir assets/logo/icon.iconset
# 需 16/32/64/128/256/512/1024 各尺寸 + @2x 变体
iconutil -c iconset assets/logo/icon.iconset -o assets/logo/weft.icns
# 然后在 Info.plist 里指定 CFBundleIconFile
```

依赖 cargo-bundle 或手工 `.app` 结构，属 v1.0 ROADMAP V9「macOS .app 打包」范围。

## 32px 可读性评估

W 的青色剪影在深底上仍可辨认（强对比度光斑），但：
- 网格线和交叉点细节全部坍缩为均匀青色底纹（不可辨识）
- 霓虹灯管多层发光质感退化为模糊光晕
- 内部暗色空心线消失

作为 Dock / 任务栏图标可用；若追求 32px 极致清晰度，可考虑做一个无网格、单粗笔 W 的简化变体。
