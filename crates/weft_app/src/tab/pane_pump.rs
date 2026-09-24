//! FIX_background_pane_pump (docs/FIX_background_pane_pump.md v2.1): the
//! per-pane PTY pump + consume chain.
//!
//! Root cause being fixed: both halves of the PTY pipeline only served the
//! ACTIVE pane — `Tab::pump_pty` pumped `active_mut()` only, and
//! `Tab::process_messages` consumed only the active pane's channel
//! (`self.msg_rx` through `Deref`). A background pane's output stalled in
//! its own bounded channel with zero consumers, so its picture froze until
//! it gained focus. Both halves now walk EVERY pane; the tab-level frame
//! budget (flood-aware clock/byte cap — base 8ms/256KB, flood 16ms/1MiB
//! when the consumer falls behind, `frame_budget` below) bounds the worst
//! case so N panes cannot eat N× the old frame cost (spec §2.1–§2.3).
//!
//! The per-pane dispatch lives here so `tab.rs` stays at its architecture
//! allowlist ceiling: this module owns the multi-pane orchestration, the
//! per-pane PtyExit semantics (§2.2), and the shared frame budget (§2.3).

use std::collections::HashSet;
use std::time::Duration;

use super::Tab;
use crate::AppMsg;
use weft_core::pane_layout::PaneId;

/// Split threshold for a single oversized PTY message. T8 (PLAN_v11217 §3.4):
/// single source of truth `weft_core::pty::EVENT_CAP` (the hard cap read_batch
/// enforces; value unchanged) — a production message can never exceed this,
/// so the split path stays test-injection-only. Oversize split: the head is
/// processed now, the tail is re-queued ON THE PANE ahead of every later
/// AppMsg (esp. PtyExit) — PTY byte order never inverts. T1 (R2): FROZEN.
const MAX_BYTES_PER_MESSAGE: usize = weft_core::pty::EVENT_CAP;
/// Time-check granularity: the clock is consulted at most once per this
/// many bytes so clock reads cannot dominate tiny messages. T1: unchanged.
const MIN_BYTES_FOR_TIME_CHECK: usize = 32 * 1024;
/// T1 (PLAN_v11217 §3.2): the tab-level frame budget is FLOOD-AWARE —
/// chosen ONCE per frame by [`frame_budget`] from a backlog snapshot.
/// Base = 8ms/256KB (the exact v1.0 + FIX_background_pane_pump budget;
/// clock stays TAB-level — N panes share ONE window per frame, R3; bytes
/// counted include oversized heads). Flood = 16ms/1MiB while the consumer
/// is visibly behind, raising pump duty cycle from ~48% toward ~100%;
/// deferred panes resume next frame (bounded channel, round-robin rotation).
const FLOOD_BACKLOG_MSGS: usize = 16;
const BASE_TIME_BUDGET: Duration = Duration::from_millis(8);
const FLOOD_TIME_BUDGET: Duration = Duration::from_millis(16);
const BASE_BYTES_PER_FRAME: usize = 256 * 1024;
const FLOOD_BYTES_PER_FRAME: usize = 1024 * 1024;

/// One frame's drain limits, picked by [`frame_budget`].
struct FrameBudget {
    time: Duration,
    bytes: usize,
}

/// 洪水判定：tab 内所有窗格 msg 通道积压之和 + 超长消息尾存在性。
/// crossbeam bounded(1024)；pump 高水位 768，洪水期积压可达数百，
/// 阈值 16 只在"消费端明显落后"时触发。
///
/// [评审修订 P2] 生产事实：PTY 读缓冲 256KB（pty.rs BUF_SIZE）恰好等于
/// MAX_BYTES_PER_MESSAGE，生产单条 PtyOutput 永不超限、永不切分——
/// `has_pending_tail` 在生产中恒 false，是防御/测试注入路径的信号；
/// 生产唯一真实洪水信号是 Σ msg_rx.len() >= 16。保留 tail 信号无害
/// （对注入式测试有用），但维护者不得按"尾=洪水"推理生产行为。
///
/// [误触发无害性 · 天花板不是地板] 预算/字节上限只在被绑定时才生效：
/// TUI 全屏重绘（vim 刷新等）瞬间产生 ≥16 条小消息误入洪水模式时，
/// 若实际工作量在 8ms/256KB 内即消化完毕，行为与基础模式逐位一致；
/// 上限放宽不改变任何排水前的处理顺序或内容。
///
/// [洪水期最坏交互代价] 非洪水场景行为逐位不变；洪水期键入回显上界
/// ≈ FLOOD_TIME_BUDGET(16ms) + 唤醒间隔(≤16ms) + vsync(≤16.6ms) ≈ 48ms
/// （评审 P3 修正：原 32ms 低估了上屏 vsync 项），实测 Warp 同场景零
/// 退化，可接受。
fn frame_budget(total_backlog_msgs: usize, has_pending_tail: bool) -> FrameBudget {
    let flood = total_backlog_msgs >= FLOOD_BACKLOG_MSGS || has_pending_tail;
    if flood {
        FrameBudget {
            time: FLOOD_TIME_BUDGET,
            bytes: FLOOD_BYTES_PER_FRAME,
        }
    } else {
        FrameBudget {
            time: BASE_TIME_BUDGET,
            bytes: BASE_BYTES_PER_FRAME,
        }
    }
}

impl Tab {
    /// Process queued messages into the terminals and drain finished blocks.
    /// v1.0 P1.5-C3: time-bounded processing — the loop drains until the
    /// channels are empty OR the frame budget is exhausted, keeping the
    /// render thread responsive during huge PTY bursts (`cat huge.log`).
    ///
    /// FIX_background_pane_pump (docs/FIX_background_pane_pump.md v2.1):
    /// this dispatches PER PANE — every pane's channel is `try_recv`-ed and
    /// its output feeds THAT pane's terminal — replacing the old
    /// active-pane-only consume that froze background panes' pictures.
    /// Multi-source semantics (spec §2.4; supersedes the v1.11.5 F18 note):
    /// - drained ui events are tagged with their source [`PaneId`], so
    ///   OSC 52 read replies route back to the pane that asked;
    /// - the OSC 52 authorization gate itself is UNCHANGED (config-driven;
    ///   same terminal semantics regardless of which pane produced the
    ///   event);
    /// - `pending_alt_rescale` stays a single merged tab flag (one main
    ///   thread consumes all panes in order; the resize commit itself is
    ///   per-pane via `resize_all_panes_for_rect`);
    /// - the alt flip history is per pane (`Tab::alt_flip_history` map), so
    ///   one pane's isolated flip cannot reset another pane's storm record.
    ///
    /// The v1.11.5 F18 warning — "only the ACTIVE pane is pumped … would-be
    /// producers in background panes simply don't run … Do not 'fix' this by
    /// pumping every pane here" — described the active-only design this fix
    /// retires; its hidden premise, that multi-source dispatch needs no
    /// source disambiguation, is resolved by the tagging above.
    ///
    /// PtyExit per pane (spec §2.2): a non-last pane's shell exit walks the
    /// existing pane-close infrastructure (`SplitTree::close_pane`) and does
    /// NOT touch `alive`; only the LAST pane's exit keeps the old
    /// `alive = false` whole-tab semantics. An exit never interrupts the
    /// same pass's consumption of the other panes.
    ///
    /// Returns `(alive, drained_blocks, need_redraw, ui_events)` where
    /// `ui_events` carries `(source_pane, event)` pairs.
    pub fn process_messages(
        &mut self,
    ) -> (
        bool,
        Vec<weft_core::blocks::Block>,
        bool,
        Vec<(PaneId, weft_core::vt::UiEvent)>,
    ) {
        self.process_messages_with_clock(std::time::Instant::now)
    }

    /// Testable seam for [`Self::process_messages`] (T1, PLAN_v11217 §3.2
    /// 改动点 2): `now` is the frame clock; the production entry passes
    /// `Instant::now` (path bit-identical to pre-T1). HARD RULE: budget
    /// checks read the clock ONLY via this closure — `frame_start.elapsed()`
    /// implicitly consults the real clock and bypasses the seam. Real-clock
    /// touch points per frame: EXACTLY three — frame-start init, the
    /// between-panes check, the per-message check; the post-drain
    /// `Instant::now()` (settle/idle) stays on the REAL clock, not in the seam.
    fn process_messages_with_clock(
        &mut self,
        now: impl Fn() -> std::time::Instant,
    ) -> (
        bool,
        Vec<weft_core::blocks::Block>,
        bool,
        Vec<(PaneId, weft_core::vt::UiEvent)>,
    ) {
        let mut need_redraw = false;
        let mut alive = true;
        let mut processed_panes: HashSet<PaneId> = HashSet::new();
        let mut exited_panes: HashSet<PaneId> = HashSet::new();
        let mut drained: Vec<weft_core::blocks::Block> = Vec::new();
        let mut ui_events: Vec<(PaneId, weft_core::vt::UiEvent)> = Vec::new();
        // §2.3: ONE shared clock per frame — the first pane processed starts
        // it; `frame_bytes` accumulates across all panes. Seam touch 1/3.
        let frame_start = now();
        let mut frame_bytes = 0usize;
        // T1 (§3.2 改动点 2): ONE backlog snapshot per frame — the sum spans
        // EVERY pane's channel (the FIX_background_pane_pump active-only bug
        // must not resurrect in the flood signal); entries are serial per thread.
        let total_backlog_msgs: usize = self.panes.values().map(|pane| pane.msg_rx.len()).sum();
        let has_pending_tail = self
            .panes
            .values()
            .any(|pane| pane.pending_pty_output.is_some());
        let budget = frame_budget(total_backlog_msgs, has_pending_tail);

        // Deterministic order (HashMap keys() is process-randomly shuffled,
        // which both flaked the byte-cap test and pinned the deferral on one
        // arbitrary pane). When last frame's budget ran out on a pane, THAT
        // pane goes first now: it was deferred, and a `pos + 1` rotation
        // would be an identity rotation in the common two-pane case (the
        // saturating pane always re-spends the cap before its sibling runs),
        // keeping the deferral pinned forever.
        let mut pane_ids: Vec<PaneId> = self.panes.keys().copied().collect();
        pane_ids.sort_unstable();
        if let Some(last_deferred) = self.pump_rotation.take() {
            if let Some(pos) = pane_ids.iter().position(|id| *id == last_deferred) {
                pane_ids.rotate_left(pos);
            }
        }
        let mut rotation_tail: Option<PaneId> = None;
        for pane_id in pane_ids {
            // Shared frame budget: once the tab's window has lapsed (and
            // enough bytes have flowed to make the check meaningful) or the
            // total byte cap is spent, leave every remaining pane for the
            // next frame — bounded channel, no loss; the rotation above
            // makes the deferral round-robin across frames, not a fixed
            // priority. T1: dynamic budget; seam touch 2/3 via `now()`.
            if frame_bytes >= budget.bytes
                || (frame_bytes >= MIN_BYTES_FOR_TIME_CHECK
                    && now().saturating_duration_since(frame_start) >= budget.time)
            {
                // Remember the FIRST deferred pane so the NEXT frame starts
                // with it (see the rotation rationale above).
                rotation_tail = Some(pane_id);
                break;
            }
            let pane_exited = self.drain_pane_channel(
                pane_id,
                &mut frame_bytes,
                frame_start,
                &budget,
                &now,
                &mut need_redraw,
                &mut processed_panes,
            );
            if !pane_exited {
                continue;
            }
            // v1.11.4: kitty negotiated flags die with the shell (both exit
            // paths below).
            if let Some(t) = self
                .panes
                .get_mut(&pane_id)
                .and_then(|pane| pane.terminal.as_mut())
            {
                t.kitty_reset();
            }
            if self.panes.len() == 1 {
                // LAST pane: keep the current whole-tab semantics — the app
                // layer (`remove_dead` in main.rs) drops the tab; the
                // post-drain pass below force-settles this pane.
                exited_panes.insert(pane_id);
                alive = false;
            } else {
                self.close_exited_pane(pane_id, &mut drained, &mut ui_events);
            }
        }

        // Post-drain pass — per pane (was active-pane-only). Each pane's
        // sequence matches the original single-pane post-loop exactly:
        // keypress-bypass refresh → settle → block drain → ui events →
        // split-head anchor compensation → block-completion snap.
        // T1 seam rule: REAL clock, NOT the injected frame clock — settle/
        // idle timing must keep ticking under a frozen test clock (不入缝).
        let now = std::time::Instant::now();
        // Sorted to match the consume pass above — drained blocks / ui
        // events then append in a stable pane order (no correctness impact,
        // just deterministic output assembly).
        let mut survivor_ids: Vec<PaneId> = self.panes.keys().copied().collect();
        survivor_ids.sort_unstable();
        for pane_id in survivor_ids {
            let pane_processed = processed_panes.contains(&pane_id);
            let pane_exited = exited_panes.contains(&pane_id);
            let mut pane_drained: Vec<weft_core::blocks::Block> = Vec::new();
            let mut split_heads = 0usize;
            let mut reset_scroll = false;
            let mut terminal_gone = false;
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                // v1.10.4: keypress bypass window — publish now (gated).
                need_redraw |= if pane_processed && pane.primary_history_refresh.take_force() {
                    pane.terminal
                        .as_mut()
                        .is_some_and(weft_core::vt::Terminal::refresh_primary_history_snapshot_now)
                } else {
                    pane.refresh_primary_history_snapshot(pane_processed)
                };
                if let Some(terminal) = pane.terminal.as_mut() {
                    let settled = if !pane_exited {
                        terminal.settle_primary_screen_exit_if_idle(now)
                    } else {
                        terminal.settle_primary_screen_exit()
                    };
                    if settled {
                        need_redraw = true;
                    }
                    pane_drained = terminal.block_tracker_mut().drain_unpersisted();
                    // v1.11.5 (PLAN_v1115 §M2): drain app-facing ui events at
                    // the response drain point, now source-tagged (see this
                    // fn's doc for the F18 rewrite).
                    ui_events.extend(terminal.take_ui_events().into_iter().map(|e| (pane_id, e)));
                    // v1.10.26 Batch D (D-3): 1MiB history-split heads settled
                    // this frame; the anchor compensation below runs after the
                    // terminal borrow ends (disjoint-pane field).
                    split_heads = terminal.take_pending_screen_split_heads().unwrap_or(0);
                    // v1.10.21: don't yank THIS pane's active history peek.
                    reset_scroll =
                        crate::tab::scroll::block_completion_should_snap(terminal, &pane_drained);
                } else {
                    terminal_gone = true;
                }
            }
            drained.extend(pane_drained);
            if terminal_gone {
                continue;
            }
            if split_heads > 0 {
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    pane.compensate_anchor_for_split(split_heads);
                }
            }
            if reset_scroll {
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    pane.snap_to_bottom();
                }
            }
        }
        // v1.11.10: DEC 2026 synchronized output suppresses presents. Every
        // pane's terminal feeds the same frame now, so any synchronized pane
        // suppresses — identical to the old active-pane-only check when only
        // one pane exists.
        if self.any_synchronized_output() {
            need_redraw = false;
        }

        // Persist the round-robin point: None when every pane got its turn
        // this frame, so the next frame starts from the top of the order.
        self.pump_rotation = rotation_tail;

        (alive, drained, need_redraw, ui_events)
    }

    /// Drain ONE pane's channel for this frame. Returns `true` when the
    /// pane's shell exited (PtyExit consumed — PtyExit is always the last
    /// message the pump ever enqueues for a session).
    ///
    /// Budget accounting is tab-level (`frame_bytes` accumulates across
    /// panes; `frame_start` + injected `now` form the shared frame clock —
    /// seam touch point 3/3; `budget` is this frame's base-or-flood
    /// FrameBudget, chosen once at frame start). When a budget trips, THIS
    /// pane's drain ends; the caller's top-of-loop check defers the
    /// remaining panes to the next frame.
    // T1: budget + injected clock must reach the per-message check —
    // explicit params, mirroring paint/'s why-commented allowances.
    #[allow(clippy::too_many_arguments)]
    fn drain_pane_channel(
        &mut self,
        pane_id: PaneId,
        frame_bytes: &mut usize,
        frame_start: std::time::Instant,
        budget: &FrameBudget,
        now: &impl Fn() -> std::time::Instant,
        need_redraw: &mut bool,
        processed_panes: &mut HashSet<PaneId>,
    ) -> bool {
        // A tail left by an earlier frame's oversize split is consumed
        // FIRST, ahead of every later AppMsg (byte-order invariant).
        let mut carried = self
            .panes
            .get_mut(&pane_id)
            .and_then(|pane| pane.pending_pty_output.take());
        loop {
            let msg = match carried.take() {
                // The carried tail is raw unprocessed output bytes — it IS
                // PtyOutput payload, just split off an oversize message.
                Some(bytes) => AppMsg::PtyOutput(bytes),
                None => match self.panes.get(&pane_id).map(|p| p.msg_rx.try_recv()) {
                    Some(Ok(msg)) => msg,
                    _ => return false, // pane gone or channel drained
                },
            };
            match msg {
                AppMsg::PtyOutput(mut data) => {
                    if data.len() > MAX_BYTES_PER_MESSAGE {
                        // Keep the remainder ahead of every later AppMsg,
                        // especially PtyExit. Re-queueing it at the channel
                        // tail would invert the original PTY byte order.
                        let tail = data.split_off(MAX_BYTES_PER_MESSAGE);
                        if let Some(pane) = self.panes.get_mut(&pane_id) {
                            pane.pending_pty_output = Some(tail);
                        }
                        *need_redraw |= self.process_pty_output_for_pane(pane_id, &data);
                        processed_panes.insert(pane_id);
                        // The head is real processing cost: it counts toward
                        // the tab cap, and this pane's drain ends for this
                        // frame (the tail resumes next frame, in order).
                        *frame_bytes = frame_bytes.saturating_add(MAX_BYTES_PER_MESSAGE);
                        return false;
                    }
                    *frame_bytes = frame_bytes.saturating_add(data.len());
                    *need_redraw |= self.process_pty_output_for_pane(pane_id, &data);
                    processed_panes.insert(pane_id);
                    // Cooperative yield (§2.3): over the shared budget, stop.
                    // Seam touch 3/3 — the delta goes through `now()`.
                    if *frame_bytes >= MIN_BYTES_FOR_TIME_CHECK
                        && now().saturating_duration_since(frame_start) >= budget.time
                    {
                        return false;
                    }
                    if *frame_bytes >= budget.bytes {
                        return false;
                    }
                }
                AppMsg::PtyExit(code) => {
                    tracing::info!(pane = ?pane_id, "Shell exited: {:?}", code);
                    return true;
                }
            }
        }
    }

    /// §2.2: a NON-last pane's shell exit. Mirrors the user pane-close path:
    /// force-settle the dying shell's primary-screen state and drain its
    /// finished blocks + ui events first (they must persist like any other
    /// pane's — `finish_pending_blocks` parity), then shrink the tree with
    /// the existing [`weft_core::pane_layout::SplitTree::close_pane`]
    /// infrastructure and drop the pane. NEVER touches `alive` — surviving
    /// panes keep the tab open. (`kitty_reset` already ran at exit
    /// detection.)
    fn close_exited_pane(
        &mut self,
        pane_id: PaneId,
        drained: &mut Vec<weft_core::blocks::Block>,
        ui_events: &mut Vec<(PaneId, weft_core::vt::UiEvent)>,
    ) {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            if let Some(terminal) = pane.terminal.as_mut() {
                // A shell that died mid-command still finalizes its in-flight
                // block before the pane drops.
                terminal.settle_primary_screen_exit();
                drained.extend(terminal.block_tracker_mut().drain_unpersisted());
                ui_events.extend(terminal.take_ui_events().into_iter().map(|e| (pane_id, e)));
            }
        }
        let new_active = match self.split_tree.close_pane(pane_id) {
            Ok(new_active) => new_active,
            Err(error) => {
                // Only reachable if the tree and the pane map desynced (caller
                // bug). Keep the tab usable: drop just the pane's own state.
                tracing::error!(?pane_id, %error, "PtyExit for a pane missing from the split tree; dropping the pane entry only");
                self.panes.remove(&pane_id);
                self.alt_flip_history.remove(&pane_id);
                return;
            }
        };
        let was_active = self.active_pane == pane_id;
        // Focus: an exiting ACTIVE pane hands focus to the absorbing sibling
        // (user-close parity); an exiting BACKGROUND pane must not move the
        // user's focus — re-sync the tree's focus to the kept active pane
        // (`close_pane` otherwise parks tree focus on the survivor).
        let desired = if was_active {
            new_active
        } else {
            Some(self.active_pane)
        };
        if let Some(id) = desired {
            self.active_pane = id;
            // `set_active` is a deliberate no-op while zoomed (the zoomed
            // pane stays focused) and only errors on stale ids — `id` is a
            // survivor leaf.
            let _ = self.split_tree.set_active(id);
        }
        self.panes.remove(&pane_id);
        // §2.5: the per-pane storm record dies with the pane.
        self.alt_flip_history.remove(&pane_id);
    }

    /// Per-pane body of the PTY output processor (moved from
    /// `tab/lifecycle.rs::process_pty_output`, which is now the active-pane
    /// delegate). `pane_id` is the pane whose terminal consumes `data` — the
    /// per-pane consume pass routes each channel's bytes here.
    ///
    /// v1.10.4: detect alt-screen (DEC 1049) enter/exit. When a TUI toggles
    /// between alt-screen and primary screen, the PTY cols must switch
    /// between full-width (alt-screen: TUI needs every column to paint
    /// borders/layout) and gutter-subtracted (primary screen: BlockView
    /// reserves breathing room).
    ///
    /// v1.10.26 Batch D (D-2): alt toggles are detected by the terminal's
    /// u64 flip-counter diff across the `process()` batch, not by comparing
    /// the `alt_active` boolean before/after — a batch that contains an h→l
    /// pair nets the boolean to zero yet still performed two real flips, and
    /// those must refresh the flip history / debounce window or the burst
    /// lock expires early (v1.10.19 loop loophole). FIX §2.5: the flips are
    /// recorded in the SOURCE pane's history slot.
    ///
    /// v1.10.21: same capture-before pattern for the alt-screen history peek
    /// — the VT core clears the flag itself on CSI ?1049l (deep in the
    /// parser, unreachable from the app layer), and the entry gate's
    /// re-entry lockout must arm on that exit too. `note_exit` runs after
    /// the terminal borrow ends because the gate is a sibling Pane field.
    pub(super) fn process_pty_output_for_pane(&mut self, pane_id: PaneId, data: &[u8]) -> bool {
        let was_peeking = self
            .panes
            .get(&pane_id)
            .and_then(|pane| pane.terminal.as_ref())
            .is_some_and(weft_core::vt::Terminal::is_alt_screen_history_peek);
        let (response, alt_flips, still_peeking) = {
            let Some(pane) = self.panes.get_mut(&pane_id) else {
                return false;
            };
            let Some(terminal) = pane.terminal.as_mut() else {
                return false;
            };
            let before = terminal.alt_flip_count();
            terminal.process(data);
            let flips = terminal.alt_flip_count().saturating_sub(before);
            let peeking = terminal.is_alt_screen_history_peek();
            (terminal.take_response(), flips, peeking)
        };
        // v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING) DEBUG probe
        // (stage 3/4): first PTY output after a committed resize — measures
        // when omp starts repainting (the ioctl-to-repaint gap). Fires once
        // per resize, then disarms. Tab-level single slot: with multiple
        // panes the first pane to produce output after any resize commit is
        // the one that logs (debug instrumentation only).
        if let Some(since_ioctl) = self.take_resize_output_probe() {
            tracing::debug!(
                since_ioctl_ms = since_ioctl.as_millis(),
                bytes = data.len(),
                "RESIZE_PROBE first_pty_output",
            );
        }
        if alt_flips > 0 {
            self.pending_alt_rescale = true;
            // v1.10.19: arm the debounce window — take_pending_alt_rescale
            // holds the recompute while toggles repeat inside it so a burst
            // coalesces into one recompute (see tab/resize.rs).
            // v1.10.26 (D-1/D-2) + v1.10.27 (FIX_RESIZE_DOUBLE_REDRAW): the
            // flip history (last two instants per source pane) is driven off
            // the counter diff — the burst-storm signature for the cols
            // mirror freeze (`burst_locked_cols`). FIX §2.5: per-pane slot.
            self.record_alt_flip_instants(pane_id, alt_flips);
        }
        if was_peeking && !still_peeking {
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.alt_peek_gate.note_exit();
            }
        }
        if !response.is_empty() {
            if let Some(pty) = self.panes.get(&pane_id).and_then(|pane| pane.pty.as_ref()) {
                if let Err(error) = pty.write_sync(&response) {
                    tracing::warn!(%error, "failed to write terminal response");
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::Tab;
    use crate::pane::Pane;
    use crate::AppMsg;
    use weft_core::pane_layout::{PaneId, SplitDirection};

    /// Two-pane test tab built from `with_terminal_only` panes (no real
    /// PTY — messages are injected straight into each pane's channel).
    /// Returns `(tab, background_id, active_id)`: `split_active_pane_test`
    /// focuses the NEW pane, so `first` (the original root) ends up in the
    /// background — exactly the frozen-pane scenario under repair.
    fn two_pane_tab() -> (Tab, PaneId, PaneId) {
        let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
        let first = tab.active_pane_id();
        let second = tab
            .split_active_pane_test(SplitDirection::Vertical, 0.5, 1000)
            .expect("test split is infallible for a single-leaf tree");
        (tab, first, second)
    }

    fn inject(pane_id: PaneId, msg: AppMsg) -> impl FnOnce(&mut Tab) {
        move |tab: &mut Tab| {
            tab.pane(pane_id)
                .unwrap_or_else(|| panic!("pane {pane_id:?} vanished"))
                .msg_tx
                .send(msg)
                .expect("test channel is empty and bounded(1024)");
        }
    }

    fn row_text(tab: &Tab, pane_id: PaneId, row: usize) -> String {
        tab.pane(pane_id)
            .and_then(|pane| pane.terminal.as_ref())
            .map(|t| t.grid().row_text(row))
            .unwrap_or_default()
    }

    /// §三 background-pane end to end: output injected into a BACKGROUND
    /// pane's PTY channel must advance THAT pane's terminal after one
    /// pump+process pass. Red before the fix: the background channel had
    /// zero consumers, so its grid never moved.
    #[test]
    fn background_pane_output_advances_its_terminal() {
        let (mut tab, bg, active) = two_pane_tab();
        inject(bg, AppMsg::PtyOutput(b"hello".to_vec()))(&mut tab);
        inject(active, AppMsg::PtyOutput(b"WORLD".to_vec()))(&mut tab);

        tab.pump_pty(); // no real PTY here — a no-op, kept for the pump+process shape
        let (_, _, need_redraw, _) = tab.process_messages();

        assert_eq!(
            row_text(&tab, bg, 0),
            "hello",
            "the background pane's terminal must advance (the freeze under repair)"
        );
        assert_eq!(
            row_text(&tab, active, 0),
            "WORLD",
            "the active pane's consumption must keep working unchanged"
        );
        assert!(need_redraw, "processed PTY output flags a redraw");
    }

    /// §二.2 PtyExit semantics (1/3): a NON-LAST pane's shell exit shrinks
    /// the split tree via the user-close-pane infrastructure and must NOT
    /// set `alive = false` — surviving panes keep the tab open.
    #[test]
    fn background_pane_exit_shrinks_tree_and_keeps_tab_alive() {
        let (mut tab, bg, active) = two_pane_tab();
        inject(bg, AppMsg::PtyExit(Ok(0)))(&mut tab);

        let (alive, _, _, _) = tab.process_messages();

        assert!(alive, "a background pane's exit must not close the tab");
        assert_eq!(tab.pane_count(), 1, "the exited pane's leaf is removed");
        assert!(tab.pane(bg).is_none(), "the exited pane's state is dropped");
        assert!(tab.pane(active).is_some(), "the survivor stays");
        assert_eq!(
            tab.active_pane_id(),
            active,
            "focus must stay on the pane the user was using"
        );
    }

    /// §二.2 PtyExit semantics (2/3): the LAST pane's exit keeps the current
    /// `alive = false` semantics — the app layer removes the whole tab
    /// (main.rs `remove_dead`). Pin of the existing behavior.
    #[test]
    fn last_pane_exit_still_closes_the_tab() {
        let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
        let only = tab.active_pane_id();
        inject(only, AppMsg::PtyExit(Ok(0)))(&mut tab);

        let (alive, _, _, _) = tab.process_messages();

        assert!(!alive, "the last pane's exit closes the tab as before");
        assert_eq!(
            tab.pane_count(),
            1,
            "the pane itself is left for remove_dead"
        );
    }

    /// §二.2 PtyExit semantics (3/3): a PtyExit in one pane's channel must
    /// not interrupt the SAME frame's consumption of the other panes.
    #[test]
    fn pane_exit_does_not_interrupt_other_panes_consumption() {
        let (mut tab, bg, active) = two_pane_tab();
        inject(active, AppMsg::PtyOutput(b"LIVE".to_vec()))(&mut tab);
        inject(bg, AppMsg::PtyExit(Ok(7)))(&mut tab);

        let (alive, _, _, _) = tab.process_messages();

        assert!(alive);
        assert_eq!(
            row_text(&tab, active, 0),
            "LIVE",
            "the surviving pane is consumed in the same pass as the exit"
        );
        assert_eq!(tab.pane_count(), 1);
    }

    /// §二.3 tab-level per-frame total byte cap (256KB): when one pane's
    /// traffic exhausts the frame budget, the OTHER pane's channel is
    /// deferred to the next frame — not lost (the channel is bounded and
    /// every pane gets its turn again).
    #[test]
    fn tab_byte_cap_defers_remaining_panes_to_next_frame() {
        let (mut tab, bg, active) = two_pane_tab();
        // Deterministic sorted order is [bg, active] (the root pane's id is
        // smaller than the split pane's), so the oversized message goes on
        // the FIRST pane: its head exactly hits the per-message split
        // threshold (= the tab frame cap), the tail re-queues, and the
        // SECOND pane's marker is deferred — the cap semantics must not
        // depend on which role a pane plays.
        let mut big = vec![b'x'; 256 * 1024 + 1];
        big.extend_from_slice(b"TAIL");
        inject(bg, AppMsg::PtyOutput(big))(&mut tab);
        inject(active, AppMsg::PtyOutput(b"\x1b[2J\x1b[HMARKER".to_vec()))(&mut tab);

        let _ = tab.process_messages();
        assert!(
            !row_text(&tab, active, 0).contains("MARKER"),
            "frame 1: the second pane is deferred once the tab byte cap is spent"
        );
        assert_eq!(
            tab.pane(active).map(|p| p.msg_rx.len()),
            Some(1),
            "the deferred message stays queued (bounded channel, no loss)"
        );

        // Subsequent frames drain the remainder (the rotation start makes
        // the deferred pane go first, then the re-queued tail) — bounded
        // loop so a regression fails instead of hanging.
        for _ in 0..10 {
            if tab.pane(active).map(|p| p.msg_rx.len()) != Some(1) {
                break;
            }
            let _ = tab.process_messages();
        }
        assert_eq!(
            row_text(&tab, active, 0),
            "MARKER",
            "the deferred pane resumes on a later frame — 超限续传，通道不丢"
        );
    }

    /// §二.3 轮转公平（rust-reviewer H-1 回归钉）：低 id 窗格每帧都灌满
    /// tab 字节上限时，高 id 窗格必须在第 2 帧被消费。`rotate_left(pos+1)`
    /// 的 off-by-one 在双窗格下是恒等旋转——饱和窗格每次都重新吃满上限、
    /// 被推迟窗格无限饥饿，正是本测试钉死的回归形态。
    #[test]
    fn saturating_sibling_rotates_deferred_pane_in_on_the_next_frame() {
        let (mut tab, bg, active) = two_pane_tab();
        // Sorted order is [bg, active]; bg streams an oversize message per
        // frame (two queued up front), active waits with one marker.
        for _ in 0..2 {
            let mut big = vec![b'x'; 256 * 1024 + 1];
            big.extend_from_slice(b"TAIL");
            inject(bg, AppMsg::PtyOutput(big))(&mut tab);
        }
        inject(active, AppMsg::PtyOutput(b"\x1b[2J\x1b[HMARKER".to_vec()))(&mut tab);

        // Frame 1: bg's head spends the whole cap before active runs.
        let _ = tab.process_messages();
        assert!(
            !row_text(&tab, active, 0).contains("MARKER"),
            "frame 1: active is deferred behind the saturating pane"
        );

        // Frame 2: the rotation must START at active. With the off-by-one
        // (rotate_left(pos+1)) this order is the identity [bg, active] again,
        // bg re-spends the cap first and this assertion goes red.
        let _ = tab.process_messages();
        assert!(
            row_text(&tab, active, 0).contains("MARKER"),
            "frame 2: the deferred pane is consumed first (round-robin)"
        );
    }

    /// §三 single-pane byte-identity: budget slicing (shared clock or the
    /// new tab byte cap) may split consumption across frames, but the
    /// terminal must observe the byte stream in exactly the original order
    /// and without loss. 300 × 1KB chunks (> the 256KB cap) each print one
    /// repeating letter; the grid tail must match the exact stream tail.
    #[test]
    fn single_pane_byte_order_is_preserved_across_budget_slices() {
        let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
        let only = tab.active_pane_id();
        let chunk_chars: Vec<u8> = (0..300).map(|i| b'A' + (i % 26) as u8).collect();
        for &c in &chunk_chars {
            inject(only, AppMsg::PtyOutput(vec![c; 1024]))(&mut tab);
        }

        // Drain across as many frames as the budgets need.
        for _ in 0..1000 {
            let drained = tab
                .pane(only)
                .map(|p| p.msg_rx.is_empty() && p.pending_pty_output.is_none())
                .unwrap_or(true);
            if drained {
                break;
            }
            let _ = tab.process_messages();
        }

        // The last chunk is 300 - 1 = 299 → 299 % 26 = 13 → 'N'. The 24×80
        // grid's bottom rows hold only the final chunk's bytes.
        let tail = "N".repeat(80);
        for row in 21..=23 {
            assert_eq!(
                row_text(&tab, only, row),
                tail,
                "row {row}: the stream tail must be byte-identical (no reorder/loss)"
            );
        }
    }

    /// §二.4 (2): a BACKGROUND pane's alt-screen flip arms the tab-level
    /// `pending_alt_rescale` — the single-value merge is intentional
    /// (single main thread, resize commit is per-pane).
    #[test]
    fn background_pane_alt_flip_arms_pending_alt_rescale() {
        let (mut tab, bg, _active) = two_pane_tab();
        assert!(!tab.pending_alt_rescale);
        inject(bg, AppMsg::PtyOutput(b"\x1b[?1049h".to_vec()))(&mut tab);

        let _ = tab.process_messages();

        assert!(
            tab.pending_alt_rescale,
            "any pane's flip must arm the pending rescale, not just the active pane's"
        );
    }
}

/// T1 flood-aware frame budget tests (PLAN_v11217 §3.2). A CHILD module (via
/// `#[path]`, file `src/tab/pane_pump_budget_tests.rs`) so the strategy fn
/// [`frame_budget`], its constants, and the [`Tab::process_messages_with_clock`]
/// seam stay private while being pinned end to end. Split out of `mod tests`
/// to keep this file under the commit-gate 800-line ceiling.
#[cfg(test)]
#[path = "pane_pump_budget_tests.rs"]
mod budget_tests;
