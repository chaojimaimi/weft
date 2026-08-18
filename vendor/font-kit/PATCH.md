# font-kit vendored patch — 消除 CoreText 后端字体文件整体读入（slurp）

上游：<https://github.com/servo/font-kit> `v0.14.3`（2023 后无维护，0.14.3 即最新）。
本目录 = 上游 `~/.cargo/registry/.../font-kit-0.14.3` 的逐字拷贝 + 下列差异（仅 CoreText 后端）。

Weft 取证（分配栈、字节级命中）：`docs/perf/warp-comparison/2026-08-18-baseline.md` §2.3。
方案全文：`docs/FIX_FONT_KIT_SLURP.md`。

## 动机

font-kit 0.14.3 CoreText 后端会把整个字体文件读入内存并**终身持有**（`font_data: Arc<Vec<u8>>`）：
家族搜索时为集合每个成员 slurp 整文件（一次恢复预热 92 次 × 78,222,888 B ≈ 7.2GB 分配抖动），
且每个 Font 对象留存整文件（PingFang.ttc 74.6MiB ×2 + Apple Color Emoji.ttc 192MiB）。
weft 对该数据**零消费**（`grep copy_font_data / .handle() / load_font_data crates/` 无命中）—
所有用到的 API（metrics/glyph_for_char/advance/rasterize_glyph/native_font）全部走 CoreText
（CTFont/CGContext），CoreText 自己 mmap 字体文件。因此数据装载改为"零读入"，匹配/选择/栅格化
逻辑一行不动，渲染字节级等价。

## 差异清单（对照 upstream v0.14.3）

| 编号 | 文件 | 内容 |
|---|---|---|
| P1 | `src/loaders/core_text.rs::from_core_text_font` | 删除 url→`File::open`→`slurp_file` 块；`font_data` 恒 `FontData::Unavailable`（保留函数签名；对应"无 URL 时原本也允许 Unavailable"的行为） |
| P2 | `src/sources/core_text.rs::create_handles_from_core_text_collection` + `create_handle_from_descriptor` | 不再 slurp + `analyze_bytes` + postscript 逐成员匹配；改为产出 `Handle::Path { path, font_index }`。`font_index` 用 `CTFontManagerCreateFontDescriptorsFromURL` 数组成员按 PostScript 名对位（单字体文件恒 0） |
| P3 | `src/loaders/core_text.rs::from_path` + `impl Loader for Font::from_path` | 覆写默认实现（不再 `from_file`→slurp）：URL→descriptors→`get(font_index)`→`new_from_descriptor(desc, 0.0)`→`Font{Unavailable}`；index 越界返回 `FontLoadingError::NoSuchFontInCollection`（与上游 from_bytes 越界错误类别一致） |
| P4 | `src/loaders/core_text.rs::from_file` | 保留暂态 slurp（只有 `File` 无 URL，构造 CTFont 需要数据），构造完成后 `font_data` 置 `Unavailable`（数据函数返回即释放，不留存）。`from_bytes` 不动 |
| P5 | `src/loaders/core_text.rs` 模块注释 | 声明 vendored 版 `copy_font_data()` 恒 `None`、`Handle` 均为 `Path`；依赖 font data 的消费方需回退上游版 |

共享新增：`src/loaders/core_text.rs::core_text_descriptors_from_url(path) -> Option<CFArray<CTFontDescriptor>>`
——`core-text` 20.1 只声明了 `CTFontManagerCreateFontDescriptorsFromURL` 的 extern、无安全包装，
于是在 vendored crate 内补必需的最小 `unsafe` 包装（CFURL 构建 + `wrap_under_create_rule`，带
SAFETY 注释说明 Create Rule 内存语义）。`src/sources/core_text.rs::font_index_for_descriptor` 复用它。

## 已知限制与行为分歧（审查定案）

- **PS 名对位失败/缺失时回退 index 0，不跳过字体**（P2）：上游集合路径遇成员异常是
  `continue` 跳过、descriptor 路径返回 `Err(NotFound)`；本补丁选择回退 0（宁可用
  错误成员也不用丢字体，两风险取其轻）。实测 Menlo/PingFang/Emoji 均不触发（两侧
  名字同源 `kCTFontNameAttribute`）。
- **PS 名读取 None 容忍**（审查 should-fix 1）：`CTFontDescriptor::font_name()` 内部
  `expect` 会在缺名属性时 panic（启动路径）；补丁用
  `descriptor_postscript_name()`（`CTFontDescriptorCopyAttribute` 直调，None 安全）替代。
- **P4 的"释放"只覆盖 Rust 侧**：`from_file` 的 `Arc<Vec<u8>>` 返回即释放，但
  CTFont 终身持有字节自己的 `CFData` **副本**（`CFDataCreate` 拷贝语义）——weft 不走
  `from_file`，影响为零；仅注释表述准确性在此修正。
- **`from_path` 对非字体文件**返回 `Io(NotFound)`（上游为 `Parse`），均为 Err，weft
  仅 debug 记录。vendored crate 自带测试套件不在 workspace 测试范围，覆盖方式 =
  weft workspace 测试（含 Metal goldens 渲染等价门禁）+ alloc 探针数据级验收。
- **性能**：`font_index_for_descriptor` 对家族每个成员各枚举一次 URL（O(N²) 名字
  分配）；实测 PingFang SC 家族搜索 ~3.7ms（冷 14ms），可接受，profiling 出问题再优化。

补丁点代码内均带 `Weft vendored patch (Px): ... See PATCH.md.` 注释，便于 diff 检索。

## 明确不动

匹配/选择（`select_family_by_name`/`pick_best_match`/properties 读取）、栅格化、metrics、advance、
glyph_for_char、`from_bytes`、DirectWrite/freetype/fontconfig 后端。`pick_best_match` 对每个
handle 的 `Font::from_handle` 仍存在，只是从 N 次 slurp 变为 N 次 CTFont 轻量构造（CoreText mmap）。

## 回滚

删除根 `Cargo.toml` 的 `[patch.crates-io]` 段（含注释）即回到上游 font-kit 0.14.3；
`vendor/font-kit/` 目录可直接删除。当前差异到此文件为止，未来升级 font-kit 时按上表逐条重审。

## 验证记录

- `cargo tree -p weft_app | grep font-kit` → `font-kit v0.14.3 (vendor/font-kit)`。
- `cargo test --workspace`：全绿（glyph 系列 / offscreen Metal goldens / cluster / emoji 栅格化）。
- 探针实测：`WEFT_ALLOC_PROBE=1 WEFT_ALLOC_PROBE_MIN=8388608` + CJK/emoji 负载 →
  该时段 `LARGE_ALLOC` **0 条**（修复前同负载 92 × 78,222,888B）；`vmmap --summary` MALLOC_LARGE
  仅剩 8224K 图形区。
