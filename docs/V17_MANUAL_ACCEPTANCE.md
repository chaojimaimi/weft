# Weft v1.7 Release App 验收矩阵

> 状态：自动化门禁完成；关键 GUI 路径已实测；完整兼容矩阵待用户确认
> App：`target/release/osx/Weft.app`（1.7.8，无签名开发构建）
> 日期：2026-07-30

## v1.7.8 Semantic 输出测试

1. Settings → Appearance 选中 `Semantic`；左键必须稳定设为 Off，右键必须稳定设为 On，
   连续按同一方向不得反复翻转。
2. 分别在 On/Off 下执行 `printf 'Status: ok\nPath: /tmp/weft.log\nVersion: v1.7.8\n'`；
   On 时 label、状态、路径和版本应有稳定角色色，Off 时恢复普通输出色。
3. 执行 `printf '\033[31mStatus: ok /tmp/red.log\033[0m\n'`；Semantic On/Off 均必须保留
   程序原始红色，不得被语义色覆盖。
4. Off → 右键调为 On 后分别用 Enter 和 Cmd+Enter 保存；重新打开 Settings、切换 profile 并重启
   Weft，都应保持 On，已存历史 Block 与新输出应同步重绘。

## 已完成证据

- [x] Release App 可启动，窗口、tab、Block 和 editor 正常渲染。
- [x] 普通命令完成后形成 Block，输出与 exit 状态可见。
- [x] 新完成 Block 无需重启即可通过 Command Palette 搜索命中。
- [x] Tab 补全由后台 provider 返回候选，输入线程无可见卡顿。
- [x] Release App 长单行命令按 pane 宽度软换行，光标保持可见且未自动执行。
- [x] Open Runbook 原生面板默认打开 `/tmp`，真实验收文件可加载为两条 Runbook 候选。
- [x] Runbook parser、文件上限、未知语言/恶意内容和“只填入不执行”有自动化测试。
- [x] Block export command/output secret 脱敏与 Unicode 预览边界有自动化测试。
- [x] zsh shell integration、vim/nano/less/tmux TUI integration 全部通过。
- [x] Dark/Light x 1x/2x Metal offscreen gate 全部通过。
- [x] GUI idle、architecture、fmt、clippy、workspace tests、rust-reviewer 全部通过。

## 待人工确认

- [ ] **V17-BLOCKVIEW-1**：执行带 `CSI 1G` spinner/progress 的命令，确认动态状态始终原位覆盖。
- [ ] 使用触控板快速/慢速滚动 1,000+ 历史块，确认细小位移连续、快速手势不丢行。
- [ ] 检查成功、失败、中断、hover、click-selected 五种 Block 表面在 Dark/Light 下均可区分。
- [ ] 分别 hover Copy/Fold，确认只有当前按钮出现明确底色/描边，点击区域不与滚动条冲突。
- [ ] 执行中的前台命令关闭窗口/tab/pane 时显示原生确认；空闲 shell 不误报，取消不终止任务。

### v1.7.7 活动进程关闭保护测试方法

1. 在新标签执行 `sleep 30`，立即点击窗口红色关闭按钮；应显示 `Close window?`，命令
   列表包含 `sleep 30`。点击 `Cancel` 后窗口保持打开，任务继续；再次关闭并选择
   `Close Anyway` 才退出。
2. 打开两个标签，在第二个执行
   `for i in {1..20}; do printf 'downloading %s/20\r' "$i"; sleep 1; done`；分别点击该
   标签关闭按钮和使用 Close Tab 快捷键，均应提示。第一个空闲标签单独关闭不应提示。
3. 创建两个 pane，在其中一个执行 `ssh localhost`、`vim` 或 `sleep 30`；关闭活动 pane
   应提示且只列出该 pane 的任务。关闭整个窗口应汇总所有 tab/pane 的活动命令。
4. 在 prompt 空闲状态连续新建并关闭标签、pane 和窗口，均不应出现误报；执行
   `true` 等已完成命令后关闭也不应提示。
5. 对确认框按 Esc、点击 `Cancel`，确认 PTY 输出仍继续、Block 仍保持运行态；选择
   `Close Anyway` 后才允许结算并销毁对应会话。
6. 执行 `sleep 30` 后按 `Cmd+Q` 或选择菜单 `Weft > Quit Weft`；行为应与窗口关闭一致，
   `Cancel` 后应用继续运行，`Close Anyway` 只退出一次。
7. 执行 `sleep 30` 后通过命令面板打开另一个 Workspace；确认提示标题为
   `Replace workspace?`。点击 `Cancel` 后原 profile、窗口尺寸、tab/pane、PTY 和输入草稿
   均不改变；确认后才替换工作区。搜索结果中的 Workspace 入口执行相同检查。
8. 非集成 shell 的外部任务由前台进程组识别；shell builtin/函数与空闲 shell 共用进程组，
   只有安装 Shell Integration 时才能通过 `CommandExecuting` 精确识别，测试此边界时不要
   把无集成的 `read` 作为“必须提示”用例。

### v1.7.7 Block 间距与彩色 Emoji 测试方法

1. 依次执行 `true`、`printf 'result\n'`、`openclaw gateway status`。无输出的 `true`
   Block 应保持紧凑；后两条命令与首行结果之间应各有一行稳定留白，运行中与完成后间距
   不应跳变。折叠/展开后滚动条范围、鼠标选择和 Find 跳转仍应落在正确行。
2. 在 12pt、14pt、18pt 分别执行 `printf '🦞 🫠 🪄 👩‍🔬 🇨🇳\n'`。彩色 emoji 应随字号同比例
   放大，视觉高度接近同一行文字且基线不应明显上浮或下沉；🦞、🫠、🪄 等非 BMP
   单字符周围不得出现方框、缺字框或重影。
3. 在 Retina 1x/2x 或内外接显示器间移动窗口后重复第 2 项，确认 atlas 重建后尺寸一致，
   不出现裁切、空白 slot 或双宽字符挤压后续文本。

### v1.7.7 Terminal 最低对比度测试方法

1. 打开 Settings → Terminal，将 `Contrast` 在 1.0、4.5、7.0、12.0 间切换；无需保存，
   当前 shell/TUI 和历史 Block 应立即同步变化，不能只有一处变亮。
2. 执行 `printf '\033[38;2;120;55;20mtruecolor orange\033[0m \033[32mANSI green\033[0m plain\n'`。
   阈值升高时三段文字只沿背景的高对比方向改变明度，橙/绿/正文的颜色身份不能互换，
   已达到阈值的颜色不得继续变化；浅色主题下应改为加深而非继续加亮。
3. 将 `Contrast` 设为 1.0，显示应恢复程序原始 RGB；复制该 Block 并导出 Markdown，内容
   与控制序列处理不得因对比度设置变化。关闭 Settings 不保存应恢复旧值，保存并重启后
   应保留新值；切换主题/profile 后当前 Grid 与历史 Block 仍应使用同一阈值。
4. 在 Grid、历史 Block 和底部输入框分别拖选橙色/红色文字，选中文字不能因半透明选区
   底色重新变暗；再测试 `printf '\033[7;31mreverse red\033[0m\n'`，REVERSE 的显式背景
   应保留且文字可读，选择与取消选择时不得闪回旧缓存颜色。普通、失败、Block 选中以及
   文本选区叠加在失败且选中的 Block 上时，都应保持同一阈值的清晰度。

### v1.7.5 详细测试方法

1. **动态输出覆盖**
   - 在 Release App 中执行：
     `for s in 'Upgrading.' 'Upgrading..' 'Upgrading...'; do printf '\033[1G%s' "$s"; sleep 0.4; done; printf '\nDone\n'`
   - 运行时应始终只有一行 `Upgrading` 原位变化；完成后的 Block 只能保留
     `Upgrading...` 和 `Done`，不能出现横向或纵向重复帧。
   - 再执行真实 `opencode upgrade`；若当前已是最新版，也应确认其 spinner 阶段不累加。

2. **滚动连续性与历史规模**
   - 使用已有 1,000+ Block 的历史；若历史不足，可先连续执行一批短命令，再执行
     `seq 1 10000` 生成长输出压力块。
   - 触控板分别做极慢双指位移、快速甩动、方向反转；滚轮一次滚动多格。
   - 慢速手势应能产生小于一整行的连续移动，快速手势不能被截成每次一行；滚动中
     不应出现明显停顿、跳回底部或错位，停止后文字和分隔面应稳定对齐。

3. **Block 状态和选择**
   - 依次执行 `printf 'success\n'`、`sh -c 'printf "failed\\n"; exit 7'`，再执行
     `sleep 30` 并按 `Ctrl+C`，形成成功、失败和中断 Block。
   - 失败块应有错误底色与红色左竖条；中断块应有警告色；相邻成功块应以轻微交替
     表面和分隔线区分，但不能压低正文对比度。
   - 鼠标依次悬停和单击每个块：hover 只跟随当前块，click-selected 在移出鼠标后仍
     保持 accent 背景，但不得出现边框或选中专用左竖条；单击空白处取消选择，切换
     tab 后旧选择不得串到新 tab。
   - 分别在 Dark/Light 主题下重复，正文、错误输出和选中边界均需清晰可读。

4. **Copy/Fold 按钮**
   - 将鼠标移到一个已完成 Block 的右上角，分别停在 Copy 和 Fold 图标上。
   - 只有当前按钮应显示较强底色与描边，另一个按钮保持较弱表面；两个点击区域不能
     重叠，也不能抢占右侧滚动条。
   - Copy 后粘贴到临时编辑器，内容应与该 Block 一致；Fold 应只折叠该 Block，再次
     点击恢复，折叠前后的滚动位置不应突跳。

5. **峰值内存回落**
   - 启动 Release App 并等待 60 秒，用 `pgrep -fl '/Weft.app/Contents/MacOS/weft'`
     获取 PID，执行 `vmmap -summary <PID>` 记录基线 footprint。
   - 运行大输出并在 BlockView 中滚动，记录 peak；回到底部 prompt 后保持窗口可见并
     静置 90 秒，再采集一次 `vmmap`。
   - 大输出结束后 Metal/MALLOC_LARGE 不应永久停留在峰值；重复三轮后稳定 footprint
   不应阶梯式持续增长。完整对比协议见 `V175_MEMORY_ANALYSIS.md`。

### v1.7.6 布局详细测试方法

1. **内边距和提示符对齐**
   - 将 Window / Horizontal Padding 设为 `0`，分别用 12pt、14pt、18pt 字号执行
     `pwd`、`printf 'hello\n'`。
   - CWD、折叠三角、命令和输出左侧应仍有约 1.5 个字符宽的稳定留白；
     失败块红色状态条应仍贴 Block 外侧，分隔线应贯穿整个 pane。
   - 在空 prompt 和输入 `echo hello` 时，`>` 与首行文字应共用同一基线；
     中文 IME 候选窗应跟随光标，不应回到窗口左边。

2. **`ls` 窄窗排版**
   - 先将窗口拉宽至 1200px 以上执行 `ls`，记住其多列对齐；再将窗口
     缩小到 600px 左右，然后恢复原宽度。
   - 窄窗中所有文件名都必须通过续行完整显示，不得裁掉右侧列，也不应在
     下一行出现带大段前导空格的 `projects` 等假缩进；恢复宽度后原列式输出
     应重新完整显示。
   - 再执行 `printf '%s\n' 'ordinary terminal prose that is deliberately longer than the window'`，
     确认普通长文本仍会软换行，而不是一律裁切。

3. **退出状态层级**
   - 依次执行 `true`、`sh -c 'exit 7'`、`sleep 30` 后按 `Ctrl+C`。
   - `true` 的成功 Block 只显示 CWD/耗时，不再显示 `exit 0`；失败 Block 必须
     显示 `exit 7` 且有红色状态条；中断 Block 必须显示 `interrupted` 和警告色。

4. **运行中输出与 CLI truecolor**
   - 执行 `opencode upgrade`，在输出尚未完成时观察 Block 底部；最新一行必须完整位于
     可见区内。随着程序继续输出，旧行应向上移动；截图时尚未产生的行不会提前出现，
     命令完成后也不能突然恢复此前被底边裁掉的半行。
   - 执行 `openclaw gateway status`。`Service:` 等标签应为弱化灰色，路径、端口和主要值
     应保留 OpenClaw 的橙色，`loaded`、`running`、`ok` 等成功状态应为绿色；不能只给
     OpenClaw 标题着色而让全部正文变白。
   - 再执行：
     `printf '\033[38;2;139;127;119mLabel:\033[0m \033[38;2;255;90;45mvalue\033[0m (\033[32mok\033[0m)\r\n'`
     完成后的历史 Block 必须仍显示灰/橙/绿三段颜色；复制结果不得包含 ANSI 控制字节。

- [ ] **V17-ANSI-1**：Dark/Light 各执行一条 truecolor + bold/italic/underline/reverse 命令，完成前后颜色和属性一致。
- [ ] 1x/2x Retina 下复测长 CJK、selection、find highlight、hyperlink hover 无互相覆盖；
  重点确认中文查找命中精确字符位置，且跨视觉行可逐项定位。
- [x] 输入超过 pane 宽度的单行命令，确认自动软换行、光标持续可见，执行内容未被插入换行。
- [ ] **V17-RUNBOOK-1**：从 Palette 打开真实 Markdown Runbook，选择多条命令，确认只填 editor、不自动执行。
- [ ] 导出含 quoted/escaped secret 的 Block，检查预览和最终 Markdown 均已遮蔽。
- [ ] **V17-SEARCH-1**：创建/编辑/删除 Workflow，保存/打开 Workspace，重启后均能在统一搜索中命中。
- [ ] 多 tab/pane 中搜索旧 Block，确认自动切换到正确 tab/pane 并定位高亮。
- [ ] **V17-COMPLETION-1**：在慢速或网络目录连续触发 Tab 补全，确认输入保持响应且无持续线程增长。
- [ ] **V17-RELEASE-1**：手工覆盖 bash/fish、ssh、neovim、top/btop、fzf；确认 resize、scroll、退出和恢复正常。

人工项必须使用 Release App；`cargo run`、单元测试或 offscreen snapshot 不替代最终观感结论。
