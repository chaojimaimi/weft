//! v1.15.1 (T3, PLAN_v1151): PTY 耗尽诊断页一键重试 —— 裸 `R` 在
//! spawn-dead 的诊断 pane 上原地重发 spawn。
//!
//! 落点说明（评审 P1）：`pane_spawn.rs` 797 行仅 3 行余量、`pane.rs` 798
//! 行是行数禁区，所以重试动作独立成模块（app/* 子模块先例）。两段式：
//!
//! 1. **terminal 重置（必选）** —— adopt（`install_spawned_pty`）复用
//!    pane 既有 terminal Arc、不替换，诊断文本不清就会残留到重试成功的
//!    会话里。`Pane::terminal` 是 D9 rule 1 私有字段，唯一合法的写路径是
//!    `with_terminal` 的 `&mut Terminal`：新 `Terminal::with_scrollback`
//!    经 `*t = fresh` 原位装回（Arc 不换，锁身份不变），并显式
//!    `set_mouse_suppress_flag` 恢复 pane.rs:269-270 的「读循环↔解析器
//!    共享一个旗标」不变量（`Terminal::reset` 的 RIS 路径会换掉旗标，
//!    不能用）。
//! 2. **重发 spawn** —— `SpawnRequest::for_new_pane(cwd=None)` + 主线程
//!    `dispatch_spawn_request` + `pane.pending_spawn = Some(request)`
//!    （`can_adopt_spawn` 需要它成立）。再次失败走
//!    `handle_spawn_failed` resource 分支（keep 清 pending_spawn）→
//!    诊断页重现、可再 R —— 循环安全。

use std::sync::Arc;

use weft_core::input::{KeyCode, Modifiers};
use weft_core::persistence::BlockStore;
use weft_core::vt::Terminal;

use crate::pane::Pane;

/// v1.15.1 (T3): 重试键判定（纯决策，真值表可测）—— spawn-dead 的 pane
/// 上，无 SUPER/CTRL/ALT 修饰的裸 `r`/`R`（SHIFT 允许，即大写 R）命中。
/// 正常 pane / 修饰键组合 / 其他键一律不命中（照常走编码路径）。
pub(crate) fn retry_key_hit(
    key: KeyCode,
    mods: Modifiers,
    pane_is_dead: bool,
    is_repeat: bool,
) -> bool {
    // is_repeat gate (rust-reviewer P3-2): holding R would otherwise re-fire
    // the fork/reset cycle at the OS repeat rate; a retry is a discrete press.
    !is_repeat
        && pane_is_dead
        && !mods.intersects(Modifiers::SUPER | Modifiers::CONTROL | Modifiers::ALT)
        && matches!(key, KeyCode::Char('r') | KeyCode::Char('R'))
}

/// v1.15.1 (T3): spawn-dead 纯判定核（真值表可测，无需构造 Pane）——
/// 三字段全空才算：PTY 从未装上、pending spawn 已清、不在关闭流程。
/// `Tab::is_persist_excluded`（tab/snapshot.rs）的 per-pane 判定复用本
/// 谓词，两处判定永不漂移。
pub(crate) fn spawn_dead_kernel(pty_installed: bool, spawn_pending: bool, closing: bool) -> bool {
    !pty_installed && !spawn_pending && !closing
}

impl Pane {
    /// v1.15.1 (T3): this pane is a spawn-failed diagnostic placeholder —
    /// the R-retry gate. See [`spawn_dead_kernel`] for the truth table and
    /// the three-field contract.
    pub(crate) fn is_spawn_dead(&self) -> bool {
        spawn_dead_kernel(
            self.pty.is_some(),
            self.pending_spawn.is_some(),
            self.close_deadline.is_some(),
        )
    }
}

impl crate::App {
    /// v1.15.1 (T3): retry the spawn on the ACTIVE pane's diagnostic
    /// placeholder. Returns `false` (no-op) when the pane is not
    /// spawn-dead — the keyboard gate pre-checks, this re-check is the
    /// defensive contract for direct callers.
    pub(crate) fn retry_spawn_active_pane(&mut self) -> bool {
        // 镜像 new_tab 的取法：共享块 id 分配器在 sessions 层。
        let block_id_allocator = self
            .sessions
            .block_store()
            .map(BlockStore::block_id_allocator);
        let tab_idx = self.sessions.active_idx();
        let Some(tab) = self.sessions.tab_mut(tab_idx) else {
            return false;
        };
        let pane = tab.active_mut();
        if !pane.is_spawn_dead() {
            return false;
        }
        // 创建站点同款配置注入值（与 self.sessions 字段不相交，可同持）。
        let scrollback = self.config_state.config.scrollback.lines.clamp(
            weft_core::config::SCROLLBACK_MIN_LINES,
            weft_core::config::SCROLLBACK_MAX_LINES,
        );
        let retained = self.config_state.config.blocks.retained_limit;
        let output_cap = crate::config_controller::output_cap_bytes(
            self.config_state.config.blocks.output_cap_mib,
        );
        let tui_mode = self.config_state.config.experimental.tui_render_mode;
        let theme = self
            .renderer
            .as_ref()
            .map(|renderer| (renderer.theme().palette, renderer.theme().background));

        // ① terminal 重置（必选，见模块 doc）：诊断文本清除 + 旗标共享
        // 不变量恢复 + 创建站点注入镜像（palette/背景/保留上限/输出上限/
        // tui 层/共享块 id 分配器），重试成功后的会话与首建同配置态。
        let Some((rows, cols)) = pane.with_terminal(|t| (t.grid().num_rows, t.grid().num_cols))
        else {
            return false;
        };
        pane.with_terminal(|t| {
            let mut fresh = Terminal::with_scrollback(rows, cols, scrollback);
            fresh.set_mouse_suppress_flag(pane.mouse_suppress.clone());
            fresh.set_blocks_retained_limit(retained);
            fresh.set_block_output_cap(output_cap);
            fresh.set_tui_render_mode(tui_mode);
            if let Some((palette, background)) = theme {
                fresh.set_palette(palette);
                fresh.set_background_color(background);
            }
            if let Some(allocator) = &block_id_allocator {
                fresh
                    .block_tracker_mut()
                    .use_shared_id_allocator(Arc::clone(allocator));
            }
            *t = fresh;
        });

        // ② 原地重发 spawn：cwd=None 继承进程 cwd（首次请求的 cwd 已随
        // spawn_failure_resource_keep 清 pending_spawn，不可恢复 —— 保真度
        // 损失备案 PLAN_v1151 评审 P3）；env 同首建源（for_new_pane 内建）。
        let request = crate::app::pane_spawn::SpawnRequest::for_new_pane(
            pane.pane_session_id,
            rows,
            cols,
            None,
            pane.mouse_suppress.clone(),
        );
        crate::app::pane_spawn::dispatch_spawn_request(request.clone(), &self.proxy);
        pane.pending_spawn = Some(request);
        let pane_session_id = pane.pane_session_id;
        tracing::info!(
            pane_session_id,
            tab_idx,
            rows,
            cols,
            "PTY spawn retry dispatched"
        );
        self.request_redraw();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── spawn_dead_kernel 真值表（三字段 8 组合）────────────────────────

    #[test]
    fn spawn_dead_kernel_truth_table() {
        // 仅「无 PTY + 无 pending spawn + 不在关闭流程」为真。
        for (pty_installed, spawn_pending, closing, expected) in [
            (false, false, false, true),
            (true, false, false, false),
            (false, true, false, false),
            (false, false, true, false),
            (true, true, false, false),
            (true, false, true, false),
            (false, true, true, false),
            (true, true, true, false),
        ] {
            assert_eq!(
                spawn_dead_kernel(pty_installed, spawn_pending, closing),
                expected,
                "pty={pty_installed} pending={spawn_pending} closing={closing}"
            );
        }
    }

    #[test]
    fn pane_is_spawn_dead_reachable_states() {
        // 占位诊断 pane（三字段全空）→ dead。
        let mut pane = Pane::placeholder(24, 80, 1000);
        assert!(pane.is_spawn_dead());
        // fork 在途 placeholder（pending_spawn 未清）→ 非 dead。
        pane.pending_spawn = Some(crate::app::pane_spawn::SpawnRequest::for_new_pane(
            pane.pane_session_id,
            24,
            80,
            None,
            pane.mouse_suppress.clone(),
        ));
        assert!(!pane.is_spawn_dead());
        // 关闭中（close_deadline 已设）→ 非 dead。
        let mut pane = Pane::placeholder(24, 80, 1000);
        pane.close_deadline = Some(std::time::Instant::now());
        assert!(!pane.is_spawn_dead());
    }

    // ── retry_key_hit 真值表 ───────────────────────────────────────────

    #[test]
    fn retry_key_hit_truth_table() {
        let bare = Modifiers::empty();
        let shift = Modifiers::SHIFT;
        // 裸 r / R（SHIFT 允许）+ dead → 命中。
        assert!(retry_key_hit(KeyCode::Char('r'), bare, true, false));
        assert!(retry_key_hit(KeyCode::Char('R'), shift, true, false));
        // 活 pane 不命中（正常 r/R 照常编码给 PTY）。
        assert!(!retry_key_hit(KeyCode::Char('r'), bare, false, false));
        assert!(!retry_key_hit(KeyCode::Char('R'), shift, false, false));
        // 其他键不命中。
        assert!(!retry_key_hit(KeyCode::Char('x'), bare, true, false));
        assert!(!retry_key_hit(KeyCode::Enter, bare, true, false));
        // 修饰键组合不命中（Cmd+R / Ctrl+R / Alt+R 各归其主）。
        assert!(!retry_key_hit(
            KeyCode::Char('r'),
            Modifiers::SUPER,
            true,
            false
        ));
        assert!(!retry_key_hit(
            KeyCode::Char('r'),
            Modifiers::CONTROL,
            true,
            false
        ));
        assert!(!retry_key_hit(
            KeyCode::Char('r'),
            Modifiers::ALT,
            true,
            false
        ));
        // P3-2: OS auto-repeat must not re-fire the fork/reset cycle.
        assert!(!retry_key_hit(KeyCode::Char('r'), bare, true, true));
        assert!(!retry_key_hit(KeyCode::Char('R'), shift, true, true));
    }
}
