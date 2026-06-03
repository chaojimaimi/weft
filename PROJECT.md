# Weft 项目介绍与当前进展

> **项目代号**: Weft  
> **基础项目**: [warpdotdev/warp](https://github.com/warpdotdev/warp) @ fe0aee14  
> **创建日期**: 2026-06-03  
> **许可**: AGPL v3 (主体) + MIT (WarpUI 框架)  

---

## 一、项目定位

**Weft** 是基于 Warp 开源终端项目的纯本地化分支，目标是打造一个**完全本地的现代化终端工具**。

### 与 Warp 的核心区别

| 维度 | Warp | Weft |
|------|------|------|
| 账号系统 | Firebase/OAuth 登录 | **已移除** |
| 云同步 | Warp Drive 云端存储 | **纯本地存储** |
| AI 调用 | 通过 Warp 服务器中转 | **客户端直连 LLM** |
| 团队协作 | 团队空间、会话共享 | **已移除** |
| 自动更新 | Warp 更新服务器 | **手动构建** |
| 遥测 | Sentry + Rudderstack | **已移除** |
| 自定义大模型 | 需经服务器中转 | **直连 (GLM/DeepSeek/Ollama)** |

### 保留的核心功能

终端模拟、命令补全 (Fish 风格)、块式编辑器、22 内置主题 + 自定义、Vim 模式、代码库索引、AI Skills、Markdown/Mermaid 渲染、命令纠正、Kitty/iTerm 图片协议、多标签页、分屏、SSH 连接。

---

## 二、项目结构

```
Weft/
├── app/                          # 主应用
│   ├── src/
│   │   ├── bin/weft.rs           # ★ Weft 入口点
│   │   └── lib.rs                # 条件编译排除云模块
│   └── Cargo.toml                # weft feature
├── crates/
│   ├── local_llm_client/         # ★ 新增：LLM 直连客户端
│   │   ├── src/
│   │   │   ├── client.rs         # HTTP 客户端
│   │   │   ├── protocol.rs       # OpenAI 兼容协议类型
│   │   │   ├── streaming.rs      # SSE 流解析
│   │   │   └── presets.rs        # GLM/DeepSeek/OpenAI 预设
│   │   └── Cargo.toml
│   ├── warp_core/                # 核心库
│   │   └── src/channel/
│   │       └── config.rs         # 新增 WarpServerConfig::none() / OzConfig::none()
│   ├── warp_features/            # Feature Flags
│   │   └── src/lib.rs            # 新增 WEFT_FLAGS 常量集合
│   └── ...                       # 其余 60+ crate 基本保持原始状态
├── scripts/
│   └── fix_weft_imports.py       # 批量修复云模块 use 引用的 Python 脚本
├── WEFT.md                       # 工程指南 (构建、同步 SOP)
├── PROJECT.md                    # 本文件
└── .workbuddy/memory/MEMORY.md   # 项目记忆
```

---

## 三、当前完成度

```
████████████░░░░░░░░░░  ~65%

✅ 架构设计与分析   (100%) — 3 份报告 + 详细设计文档
✅ 项目基础设施     (100%) — 入口点、Feature Flags、LLM 客户端、Cargo 配置
✅ 云模块排除       (100%) — 11 个模块 #[cfg] 包裹 + 10 个 crate 从 workspace 排除
✅ 批量引用修复     (90%)  — 534 文件 use 语句已包裹
⬜ 首次编译         (0%)   — 待终端手动运行
⬜ 编译错误修复     (0%)   — 取决于首次编译输出
⬜ LLM 直连集成     (0%)   — 需编译通过后实施
⬜ 存储本地化       (0%)   — 需编译通过后实施
```

---

## 四、已完成的改造清单

### 4.1 入口点
| 文件 | 改造 | 
|------|------|
| `app/src/bin/weft.rs` | 新入口，使用 `Channel::Local` + `WarpServerConfig::none()` + `WEFT_FLAGS` |
| `crates/warp_core/src/channel/config.rs` | 新增 `WarpServerConfig::none()` 和 `OzConfig::none()` 方法 |

### 4.2 Feature Flags
| 文件 | 改造 |
|------|------|
| `crates/warp_features/src/lib.rs` | 新增 `WEFT_FLAGS` (约 100 个纯本地 FeatureFlag) |
| `app/Cargo.toml` | 新增 `weft` feature，imply `skip_login` + `custom_inference_endpoints` + `solo_user_byok` + `api_key_authentication` + `api_key_management` + `dep:local_llm_client` |

### 4.3 Cargo 依赖清理
| 文件 | 改造 |
|------|------|
| `Cargo.toml` (root) | `workspace.exclude` 添加 10 个云端+测试 crate；移除 workspace.dependencies 中的云端 crate |
| `app/Cargo.toml` | 移除 `cloud_object_*`, `warp_server_*`, `firebase`, `onboarding`, `session_sharing_protocol` 共 9 个依赖；修复 `skip_login`, `test-util`, `integration_tests`, `agent_mode_evals` feature flags |

### 4.4 核心代码
| 文件 | 改造 |
|------|------|
| `app/src/lib.rs` | 11 个云模块 `#[cfg(not(feature = "weft"))]` 包裹；8 个顶层 `use` 语句包裹 |
| `app/src/workspace/mod.rs` | `use crate::server::telemetry` 条件编译 |
| `app/src/` 下 534 个 Rust 文件 | `use crate::auth/cloud_object/server/drive/workspaces::` 等语句添加 `#[cfg(not(feature = "weft"))]` 前缀 |

### 4.5 新增模块
| 文件 | 说明 |
|------|------|
| `crates/local_llm_client/` | 完整 5 文件 crate：OpenAI 兼容直连客户端 |
| `scripts/fix_weft_imports.py` | 批量修复工具 |
| `WEFT.md` | 工程指南 |

### 4.6 被排除的模块
**App 层 (条件编译排除):** `auth`, `autoupdate`, `billing`, `cloud_object`, `drive`, `pricing`, `referral_theme_status`, `reward_view`, `server`, `workspaces`

**Crate 层 (workspace 排除):** `firebase`, `cloud_objects`, `cloud_object_client`, `cloud_object_models`, `cloud_object_persistence`, `warp_server_auth`, `warp_server_client`, `onboarding`, `session_sharing_protocol`, `integration`

---

## 五、剩余工作

### P0 — 首次编译通过

**状态：待执行** (需在终端手动运行，编译时间 ~30-60 分钟)

```bash
cd /Users/andylee/McDull/Claude/projects/Weft
cargo check --bin weft --features weft 2>&1 | tee check-output.log
```

预期产生两类错误：
1. **use 语句遗漏** — 534 文件外的剩余文件仍有引用 → 继续用 `scripts/fix_weft_imports.py` 或手动补充
2. **代码体内直接引用** — 如 `UserWorkspaces::as_ref(ctx).is_xxx()` 等运行时引用 → 需手动 `#[cfg]` 包裹

### P1 — LLM 直连集成

| 文件 | 改造内容 |
|------|---------|
| `app/src/ai/agent/api/impl.rs` | 新增 `generate_local()` 直连路径，使用 `local_llm_client::LocalLlmClient` |
| `app/src/settings_view/custom_inference_modal.rs` | 解除 URL 限制 (允许 HTTP/localhost) |

### P2 — 存储本地化

| 模块 | 改造内容 |
|------|---------|
| Workflows | `cloud_objects` 存储 → 本地 YAML 文件 |
| Notebooks | `cloud_objects` 存储 → 本地 SQLite |
| Settings | `SyncToCloud::Globally(...)` → `SyncToCloud::Never` |
| History | 去除云同步 |
| MCP 配置 | 本地 TOML 文件 |

### P3 — AI 模块云依赖清理

`app/src/ai/` 下 ~130 个文件引用排除模块，包括 `ai/cloud_environments/`, `ai/cloud_agent_config/`, `ai/onboarding.rs` 等需条件编译排除或重写。

---

## 六、构建命令

```bash
# 开发构建
cargo build --bin weft --features weft

# 运行
cargo run --bin weft --features weft

# Release 构建
cargo build --bin weft --features "weft,release_bundle,gui" --release
```

---

## 七、Upstream 同步

- **频率**: 每月 1 次
- **策略**: 仅 cherry-pick 核心 crate 变更，不 merge 整个 upstream
- **关注**: `warp_terminal`, `editor`, `warp_completer`, `warpui*`, `ai/index`, `ai/skills`, `themes`

```bash
git remote add upstream git@github.com:warpdotdev/warp.git
git fetch upstream
git log upstream/master --oneline --since="1 month ago" -- crates/warp_terminal/ crates/editor/ ...
git cherry-pick <hash>
cargo build --bin weft --features weft
```

---

## 八、相关文档

| 文档 | 位置 |
|------|------|
| 初版分析报告 | `/Users/andylee/McDull/WorkBuddy/2026-06-03-10-47-37/Warp重构分析报告.md` |
| 深入审查报告 | `/Users/andylee/McDull/WorkBuddy/2026-06-03-10-47-37/Warp纯本地实施方案-深入审查报告.md` |
| 详细设计文档 | `/Users/andylee/McDull/WorkBuddy/2026-06-03-10-47-37/Weft开发设计文档.md` |
| 工程指南 | `./WEFT.md` |
| 项目记忆 | `.workbuddy/memory/MEMORY.md` |
