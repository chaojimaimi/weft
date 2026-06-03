# Weft Project Memory

## 项目概述
- 项目名称：Weft
- 基础项目：warpdotdev/warp @ fe0aee14
- 项目目录：/Users/andylee/McDull/Claude/projects/Weft
- 目标：基于 Warp 重构的纯本地终端工具，移除全部账号/团队/云功能

## 构建命令
```bash
cargo build --bin weft --features weft
cargo run --bin weft --features weft
cargo build --bin weft --features "weft,release_bundle,gui" --release
```

## 构建状态

| 阶段 | 状态 | 说明 |
|------|------|------|
| 入口点和配置 | ✅ | weft.rs, none() 方法, WEFT_FLAGS |
| LLM 客户端 crate | ✅ | crates/local_llm_client/ |
| 云模块声明排除 | ✅ | lib.rs 11 个模块条件编译 |
| Cargo 依赖清理 | ✅ | 根+app Cargo.toml 移除云端 crate 依赖 |
| 批量 use 修复 | ✅ | scripts/fix_weft_imports.py — 534 文件已处理 |
| Workspace exclude | ✅ | 9 个云端 + integration crate 排除 |
| 首次编译 | 🔄 | cargo check 运行中 |

## 关键改造文件
- app/src/bin/weft.rs — 入口点
- app/src/lib.rs — 11 云模块 + 8 use 语句条件编译
- app/Cargo.toml — weft feature + 依赖清理
- Cargo.toml (root) — workspace.exclude + 依赖清理
- crates/local_llm_client/ — LLM 直连客户端
- crates/warp_features/src/lib.rs — WEFT_FLAGS
- crates/warp_core/src/channel/config.rs — none() 方法
- scripts/fix_weft_imports.py — 批量修复工具

## 被排除的云模块
auth, autoupdate, billing, cloud_object, drive, pricing,
referral_theme_status, reward_view, server, workspaces

## 被排除的 crate
firebase, cloud_objects, cloud_object_client, cloud_object_models,
cloud_object_persistence, warp_server_auth, warp_server_client,
onboarding, session_sharing_protocol, integration

## Upstream 同步
- 频率：每月 1 次
- 策略：仅 cherry-pick 核心 crate 变更
- 关注：warp_terminal, editor, warp_completer, warpui*, ai/index, ai/skills, themes
