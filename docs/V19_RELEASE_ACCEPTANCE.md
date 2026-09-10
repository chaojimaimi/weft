# Weft v1.9 发布验收

> 目标版本：1.9.0
> 定位：v1.6-v1.8 工程成果的统一 Release Candidate 与可分发制品验收，不新增产品功能。
> 原则：本机结构验收、Release App 人工验收、Developer ID/公证验收分别记录；后两者未完成时不得宣称正式发布完成。

## 发布合同

- [x] **V19-VERSION-1**：workspace、bundle metadata、App Info.plist、ZIP/DMG 文件名与 Cask 版本一致。
- [x] release workflow 对 tag/手动输入与 Cargo workspace 版本做 fail-closed 校验。
- [x] workflow 固定 `MACOSX_DEPLOYMENT_TARGET=12.0`，产出 ZIP、DMG 与 `SHA256SUMS`。
- [ ] 最终签名 ZIP 生成后运行 `scripts/update-cask.sh 1.9.0 <zip>`，移除 `sha256 :no_check`。

## 自动化与包结构

- [x] `cargo fmt --all --check`
- [x] `cargo test --workspace`：1975 passed / 0 failed / 13 ignored。
- [x] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [x] `scripts/architecture_gate.sh`
- [x] `scripts/performance_gate.sh`：resize 71.56ms，seq 10k 21.42ms，seq 100k 242.70ms，
  10k history 21.37ms，Metal p95 1.21-2.38ms，idle wake/redraw 1.998Hz，CPU p95 1.897ms。
- [x] 真实 Ollama production backend：`gemma4:e4b`，1 passed / 0 failed，11.77s。
- [x] **V19-PACKAGE-1**：`scripts/v19-release-acceptance.sh` 本机模式通过，ZIP/DMG 完整性与 arm64 App 结构正确。

## Release App 人工矩阵

- [x] **V19-GUI-1**：从打包 App 启动；窗口、tab、垂直 split pane、Settings、Palette、命令 Block
  与 Retina 2x Dark 渲染通过真实 GUI smoke test。
- [ ] Release App 的 Light 主题人工视觉确认；Dark/Light 1x/2x offscreen Metal golden 已自动通过。
- [ ] 系统拼音 IME：editor 与 TUI preedit/commit、候选窗锚点、CJK 宽度正常。
- [ ] vim、nano、less、tmux、top/btop、fzf：输入、鼠标、滚动、resize、退出与 Block 恢复正常。
- [ ] v1.8 Local AI：模型发现、中文命令生成只插入不执行、失败 Block CJK 诊断、取消与重试正常。
- [ ] 进程级网络观察：Local AI 仅连接配置的 loopback Ollama，无代理、DNS、LAN 或公网连接。
- [x] 内建 Retina 2x 显示通过。
- [ ] 物理 1x 与跨屏 scale-factor 迁移另机验收。

## 分发信任链

- [ ] **V19-SIGN-1**：Developer ID Application 签名，`codesign --verify --deep --strict` 通过。
- [ ] Apple notarytool 返回 Accepted，ticket 已 staple 且 `stapler validate` 通过。
- [ ] 在隔离下载属性下 `spctl --assess --type execute` 通过，首次启动无 Gatekeeper 绕行。
- [ ] `scripts/v19-release-acceptance.sh --distribution` 全绿，Cask SHA-256 等于最终签名 ZIP。

## 当前验收环境与限制

- 日期：2026-08-01
- 环境：macOS 26.6 (25G72)，Apple M2 Max，arm64，32 GB，内建 3456×2234 Retina 2x。
- 输入法：系统 ABC 与拼音可用；Ollama 本机运行。
- 当前钥匙串：`0 valid identities found`，无 Developer ID Application；签名、公证、Gatekeeper 下载制品验收必须在配置了发布 secrets 的 GitHub Actions 或发布机完成。
- 显示器：当前只有内建 Retina，无法在本机完成物理 1x 与跨显示器迁移验收。

## 本轮发现与修复

- fresh XDG 配置启动截图显示欢迎横幅把 ANSI ESC 渲染为字面量 `{1b}`，且版本固定为 v1.0。
  源码证据确认 `format!("{:?}")` 在写 PTY 前把 ESC 编为 `\\u{1b}`，排除 Metal/VT 渲染根因。
- 改为 printable `\\033` + shell `printf '%b'`，版本取 `CARGO_PKG_VERSION`；新增真实 `/bin/sh`
  输出回归测试并经 rust-reviewer PASS。重建 App 后 fresh 配置截图确认横幅为 v1.9.0 且 ANSI dim 正常。

## 制品与结论

- `Weft-v1.9.0.zip`：6,200,172 bytes，SHA-256
  `fcc261b4d0a4e3a003452cd4c50364232ba491c852d9f35941642cc9d3b24bd5`。
- `Weft-1.9.0.dmg`：6,886,801 bytes，SHA-256
  `dd8118d4f36ce2753f285ca7e52a545e9a46de12e4d0044bb158fb8b6489136c`。
- 本机 RC 结论：自动化、真实 Ollama、包结构、Retina 2x GUI smoke 与 TUI integration 通过。
- 正式分发结论：**尚未通过**。唯一强制外部阻塞为本机无 Developer ID identity，因此签名、公证、
  Gatekeeper 与最终签名 ZIP 的 Cask SHA 尚待 signed CI；物理 1x/跨屏及扩展人工兼容矩阵仍明确未勾选。
