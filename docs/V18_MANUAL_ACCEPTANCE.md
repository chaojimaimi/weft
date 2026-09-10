# Weft v1.8 "Spindle" Release App 与真实 Ollama 验收

> 目标版本：1.8.10
> 状态：自动化硬化与真实 Ollama backend 验收完成；Release App GUI 人工验收待执行
> 原则：未勾选项目不得用于宣称 v1.8 已完成人工验收或网络隔离验收。

## 自动化前置

- [x] `cargo fmt --all --check`
- [x] `cargo test --workspace`：1975 passed / 0 failed / 13 ignored。
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
- [x] `scripts/architecture_gate.sh`
- [x] `scripts/performance_gate.sh`：idle wake/redraw 1.797Hz，CPU frame p95 0.832ms；Metal 四组 p95 1.076-1.477ms。
- [x] `scripts/acceptance_preflight.sh`
- [x] mock `/api/tags`、`/api/chat`、取消、错误与流式边界测试实际进入 test binary：79 passed / 1 real-Ollama ignored。
- [x] `ai::real_tests::real_ollama_lists_models_and_completes` 使用 `gemma4:e4b` 通过：1 passed / 0 failed，9.98s。
- [x] 生产 client 的系统代理隔离、禁止 redirect 与流式 idle read-timeout 回归：4 passed / 0 failed。

真实 Ollama 测试命令：

```bash
WEFT_OLLAMA_MODEL=gemma4:e4b cargo test -p weft_app --bin weft \
  ai::real_tests::real_ollama_lists_models_and_completes -- --ignored --nocapture
```

可用 `WEFT_OLLAMA_BASE_URL=http://localhost:11434` 覆盖地址；测试会拒绝 HTTPS、LAN、
带 userinfo 的 URL 和任何非 loopback 主机。

## 真实 Ollama 工程验收结果（2026-08-01）

- Ollama `0.32.5` 仅监听 `127.0.0.1:11434`（`lsof -nP -iTCP:11434`）。
- `/api/tags` 成功发现 `gemma4:e4b`、`gemma4:12b` 与 embedding 模型；生产
  `OllamaBackend::list_models` + 流式 `complete` 集成测试使用 `gemma4:e4b` 通过。
- 对支持 thinking 的 `gemma4:e4b` 发送 `think: false` 的真实 `/api/chat` 请求返回可见内容
  `READY`，无 reasoning trace，总耗时约 0.545s。
- 生产 URL 校验的五项拒绝测试通过：非 Ollama provider、非 loopback、HTTPS、userinfo 与 LAN host
  均在请求前拒绝。
- 本节只证明 backend、真实模型和监听边界；下方 Settings、Palette、Block、进程级出站观察及恢复场景
  仍须在 Release App 中逐项人工勾选。

## 准备

1. 用 `scripts/build-app.sh` 构建 1.8.10 Release App，不使用 `cargo run` 代替 GUI 验收。
2. 启动本地 Ollama，并准备一个普通模型和一个支持 thinking 的模型（若可用）。
3. 打开网络观察工具，记录 Weft 进程的所有出站连接。
4. 准备包含 CJK、长输出、Bearer token、URL credential、`password=` 和环境变量密钥的失败 Block。

## Settings 与模型发现

- [ ] **V18-SETTINGS-1**：Local AI Disabled 时保存并重启，Palette 不显示 AI 入口，Block 不显示 Diagnose。
- [ ] **V18-SETTINGS-2**：Enabled 后 Test Connection 能列出 `/api/tags` 返回的本机模型并选择、保存、重载。
- [ ] **V18-SETTINGS-3**：Ollama 未启动、没有模型和未知模型分别显示可操作错误，Settings 保持可编辑。
- [ ] **V18-SETTINGS-4**：Max Tokens、Timeout、Command Generation、Error Diagnosis 保存后重启值一致。
- [ ] **V18-SETTINGS-5**：非 loopback、HTTPS、带用户名密码的 URL 在发请求前被拒绝。

## Palette 命令生成

- [ ] **V18-COMMAND-1**：中文自然语言请求能生成单条可编辑 shell 命令。
- [ ] **V18-COMMAND-2**：结果只插入 editor，任何情况下都不自动提交或执行。
- [ ] **V18-COMMAND-3**：危险命令显示风险等级；用户仍需显式确认插入和执行。
- [ ] **V18-COMMAND-4**：生成期间第一次 Esc 取消请求并恢复输入，第二次 Esc 关闭 Palette。
- [ ] **V18-COMMAND-5**：中文 IME preedit、候选窗位置、Cmd+V 和长输入横向可见性正常。

## Block 诊断

- [ ] **V18-DIAGNOSE-1**：失败 Block 显示 Diagnose，成功 Block 不显示；诊断面板紧邻对应输出。
- [ ] **V18-DIAGNOSE-2**：CJK 诊断无缺字、碎片、旧 atlas glyph 或错误折行。
- [ ] **V18-DIAGNOSE-3**：关闭面板或切换请求不会把旧结果写入另一个 Block。
- [ ] **V18-DIAGNOSE-4**：thinking 模型返回可见诊断而非空响应；输出不包含 reasoning trace。
- [ ] **V18-DIAGNOSE-5**：长输出按预算截断，UI 保持响应，完整终端/Block 内容不被修改。

## 安全、网络与恢复

- [ ] **V18-SAFETY-1**：发送给 Ollama 的命令、历史和失败输出不含准备好的明文密钥。
- [ ] **V18-SAFETY-2**：网络观察只出现所配置的 loopback Ollama 连接，无 DNS、LAN 或公网请求。
- [ ] **V18-RECOVERY-1**：停止 Ollama、流中断、重新启动后可重试成功，无需重启 Weft。
- [ ] **V18-RECOVERY-2**：切换模型后新请求使用新模型，旧请求被取消且不污染 metrics。
- [ ] **V18-RECOVERY-3**：崩溃 Restore 后历史 Block、命令、CWD 和 AI 开关状态正确，历史命令不会自动执行。

## 性能与可观测性

- [ ] **V18-PERF-1**：Local AI Disabled 时 idle wake/redraw/CPU/内存与 v1.7 基线等价。
- [ ] **V18-PERF-2**：生成期间终端输入、滚动、切 tab/pane 无明显阻塞。
- [ ] **V18-METRICS-1**：requests/success/errors/cancellations/truncations 和 p95 随真实请求正确变化。
- [ ] **V18-METRICS-2**：Debug 日志只含模型、时延、长度和错误类别，不含 prompt、输出或密钥正文。

## 验收结论

- 执行人：Codex（工程验收部分）
- 日期：2026-08-01
- macOS / 机器 / scale：待填写
- Ollama / 模型：Ollama 0.32.5 / `gemma4:e4b`
- Commit：`ebc7069`（v1.8.10）；统一 Release Candidate 由 v1.9.0 验收承接
- 网络观察工具：`lsof` 已确认服务只监听 `127.0.0.1:11434`；Weft 进程级出站观察待 GUI 人工验收
- 结论：自动化门禁、真实 backend 与 Release App 构建通过；GUI 人工矩阵尚未完成
- 备注：v1.8.10 App/DMG 已由当前工作树重建；DMG 完整性通过，SHA-256
  `3bc6b975e1ed4ac8bd19c4286aa642b2ca60e0385cb4e07f76340c2b1f834a56`。
