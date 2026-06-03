# WEFT.md — Weft 工程指南

> Weft 是基于 [warpdotdev/warp](https://github.com/warpdotdev/warp) 的纯本地终端工具分支。

## 项目概述

Weft 是 Warp 终端开源项目的本地化分支（fork），**移除全部账号管理、团队协作和云服务功能**，保留核心终端体验并新增自定义大模型直连能力。

### 与 Warp 的区别

| 维度 | Warp | Weft |
|------|------|------|
| 许可 | AGPL v3 + MIT (warpui) | 同上游 |
| 账号系统 | Firebase/OAuth 登录 | **已移除** |
| 云同步 | Warp Drive 云存储 | **已移除（纯本地）** |
| AI 调用 | 通过 Warp 服务器中转 | **直连 LLM（OpenAI 兼容）** |
| 团队协作 | 团队工作区/会话共享 | **已移除** |
| 自动更新 | Warp 更新服务器 | **已移除（手动构建）** |
| 遥测 | Sentry + Rudderstack | **已移除** |
| 自定义 LLM | 需 CustomInferenceEndpoints 且经服务器中转 | **直连，支持 GLM/DeepSeek/Ollama** |
| 主题 | 22 内置 + 自定义 YAML + 图片生成 | **同上游** |

## 构建

### 前置条件

```bash
# macOS
./script/bootstrap
```

### 构建 Weft

```bash
cargo build --bin weft --features weft
```

### 运行 Weft

```bash
cargo run --bin weft --features weft
```

### Release 构建

```bash
cargo build --bin weft --features "weft,release_bundle,gui" --release
```

## 项目结构

```
Weft/
├── app/                        # 主应用（基于 warp app）
│   ├── src/
│   │   ├── bin/weft.rs         # ★ Weft 入口点
│   │   └── lib.rs              # 条件编译排除云模块
│   └── Cargo.toml              # 新增 weft feature
├── crates/
│   ├── local_llm_client/       # ★ 新增：LLM 直连客户端
│   │   ├── src/
│   │   │   ├── client.rs       # HTTP 客户端
│   │   │   ├── protocol.rs     # OpenAI 兼容协议类型
│   │   │   ├── streaming.rs    # SSE 流解析
│   │   │   └── presets.rs      # GLM/DeepSeek/OpenAI 预设
│   │   └── Cargo.toml
│   ├── warp_core/              # 核心库（轻微修改）
│   │   └── src/channel/
│   │       └── config.rs       # 新增 none() 静态方法
│   ├── warp_features/          # Feature flags（新增 WEFT_FLAGS）
│   │   └── src/lib.rs
│   └── ...                     # 其余 crate 基本保持原始状态
└── WEFT.md                     # 本文件
```

## Feature Flags

### `weft` feature

启用 `weft` feature 后：

1. **编译时排除**：所有云/团队模块通过 `#[cfg(not(feature = "weft"))]` 条件编译排除
2. **运行时启用**：`WEFT_FLAGS` 常量集合在入口点加载，启用核心本地功能
3. **自动 imply**：`skip_login`、`custom_inference_endpoints`、`solo_user_byok`、`api_key_authentication`、`api_key_management`
4. **依赖注入**：`local_llm_client` crate 编译并链接

### `WEFT_FLAGS` 包含的核心功能

- 终端体验：补全、命令纠正、Kitty/iTerm 图片、粘贴、Shell 选择
- AI 功能：自定义 LLM 端点、BYOK、Skills、Agent Mode
- 编辑器：Tabbed Editor、File Tree、Vim Mode
- Markdown：表格、Mermaid、图片渲染
- 主题：内置主题 + YAML 自定义 + 图片生成
- 搜索：Command Palette、Global Search、Web Search
- 代码审查：Inline Review、Diff、Checkpoints

## Upstream 同步

### 同步原则

- **仅 cherry-pick** 核心 crate 层的变更
- **不 merge** 整个 upstream
- 同步频率：**每月 1 次**

### 同步流程

```bash
# 1. Fetch upstream
git remote add upstream git@github.com:warpdotdev/warp.git  # 首次
git fetch upstream

# 2. 查看核心 crate 变更
git log upstream/master --oneline --since="1 month ago" -- \
  crates/warp_terminal/ crates/editor/ crates/warp_completer/ \
  crates/warpui_core/ crates/warpui/ crates/warp_search_core/ \
  crates/ai/src/index/ crates/ai/src/skills/ crates/ai/src/project_context/ \
  app/src/themes/

# 3. Cherry-pick 核心变更
git cherry-pick <commit-hash> ...

# 4. 编译验证
cargo build --bin weft --features weft

# 5. 测试
cargo test --bin weft --features weft
```

### 冲突解决优先级

| 区域 | 策略 |
|------|------|
| 核心 crate | 优先合入 upstream 变更 |
| app/src/lib.rs | 保留 Weft 的 `#[cfg]` 包裹 |
| 云服务模块 | 直接忽略（不合入） |

## 许可

- `warpui_core` / `warpui`: MIT
- 其余全部代码：AGPL v3
- `crates/local_llm_client/`: AGPL v3（新增，同主体）
- Weft 是一个纯本地工具，**无服务端组件** — AGPL 的"网络使用触发开源"条款不构成负担
