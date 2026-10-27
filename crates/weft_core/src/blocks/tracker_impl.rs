//! `BlockTracker` impl blocks (`Default` + the main inherent impl),
//! moved verbatim out of `blocks.rs` (v1.15.2 governance cart). The
//! struct itself STAYS in `blocks.rs`: ~35 module-private fields are
//! accessed directly by six sibling impls (continuation.rs,
//! live_styled_publish.rs, retention.rs, …) — moving the struct would
//! blind them (PLAN_v1152 §4 B8).

use super::*;

impl Default for BlockTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockTracker {
    pub fn new() -> Self {
        Self {
            phase: ShellPhase::NotIntegrated,
            bootstrap_ready: false,
            blocks: Vec::new(),
            unpersisted: Vec::new(),
            ids: crate::block_id_sequence::BlockIdSequence::new(),
            pending_command: None,
            pending_started: None,
            pending_cwd: None,
            current_cwd: None,
            output: OutputCapture::default(),
            styled_output: None,
            live_styled_snapshot: None,
            last_live_styled_publish: None,
            batch_clock: Instant::now(),
            live_output_version: 0,
            command_generation: 0,
            screen_rewrite_epoch: 0,
            screen_document_start: None,
            screen_owned_blocks: HashSet::new(),
            continuation_candidate: None,
            continuation_base: None,
            session_start: 0,
            loaded_ids: HashSet::new(),
            load_older_ids: HashSet::new(),
            dirty_blocks: HashSet::new(),
            retained_limit: retention::DEFAULT_BLOCKS_RETAINED_LIMIT,
            output_cap: DEFAULT_OUTPUT_CAP,
            evicted_ids: HashSet::new(),
            user_pinned_ids: HashSet::new(),
            settling: false,
            title_generation: 0,
            footer_exhausted: false,
        }
    }

    /// Current working directory (updated from OSC 7). Snapshotted into the
    /// next block at `133;B` so each block carries the dir it ran in.
    /// v1.14.2 (B1): bumps [`title_generation`](Self::title_generation) —
    /// the tab title renders this cwd, so an OSC 7 landing must invalidate
    /// the app's cached title (the `cd` freeze regression).
    pub fn set_cwd(&mut self, cwd: Option<String>) {
        self.current_cwd = cwd;
        self.title_generation = self.title_generation.wrapping_add(1);
    }

    /// v1.14.2 (B1): the tab-title generation (see the field doc). Cheap
    /// equality probe for the app-side title cache.
    pub fn title_generation(&self) -> u64 {
        self.title_generation
    }

    /// v1.12.19 (PLAN_v11217 §3.8 T13b): the configured in-memory retention
    /// cap (0 = retention disabled). Read-side twin of
    /// `set_retained_limit` — the all-tab walk test asserts the config
    /// value reached every pane's tracker.
    pub fn retained_limit(&self) -> usize {
        self.retained_limit
    }

    /// PLAN_v11217 §3.5 (T4): the configured retained-output cap in bytes.
    /// Every bounding site of user-visible retained text reads this — never a
    /// global constant — so `[blocks] output_cap_mib` takes effect per tab.
    pub fn output_cap(&self) -> usize {
        self.output_cap
    }

    /// PLAN_v11217 §3.5 (T4): configure the retained-output cap. `cap` is in
    /// BYTES and is clamped into
    /// `[OUTPUT_CAP_MIN_MIB * MIB, OUTPUT_CAP_MAX_MIB * MIB]` here (the entry
    /// point is reachable from live-reload and profile switches, not only the
    /// normalized config-load path — review P2a double-clamp). Also syncs the
    /// capture's metadata field so the finalize truncation marker reports the
    /// configured value instead of a stale default.
    pub fn set_output_cap(&mut self, cap: usize) {
        self.output_cap = cap.clamp(OUTPUT_CAP_MIN_MIB * MIB, OUTPUT_CAP_MAX_MIB * MIB);
        self.output.set_cap_bytes(self.output_cap);
    }

    /// v1.13.5 T16b: public read of the live-view content version for the
    /// renderer's background block-pane cache fingerprint. Consumers outside
    /// this crate previously reached it only via
    /// [`Self::in_flight`](Self::in_flight)`().version`, which is `None` at
    /// the prompt — exactly when the finished-block list is the thing that
    /// changed. Structural sites (finalize / clear_pending_capture) bump the
    /// same counter, so "version unchanged" means "nothing about the
    /// rendered block view changed". Extra bumps are always conservative:
    /// the renderer-side cache at worst does one extra rebuild, never
    /// serves stale bytes.
    pub fn live_output_version(&self) -> u64 {
        self.live_output_version
    }

    /// v1.13.5 T16a: O(1) tail-line index of the live capture — byte-equal
    /// to `in_flight().output.lines().count().saturating_sub(1)` (the
    /// renderer caret fallback's formula) via the maintained newline
    /// ledger. The formula scan used to cost O(total captured text) on
    /// EVERY rendered frame of a streaming plain command (no cursor
    /// snapshot exists off the screen-exit path): measured 2.1ms/frame at
    /// 750k lines, the seq bench's top hotspot.
    pub fn live_cursor_tail_line(&self) -> usize {
        self.output
            .text_line_count()
            .saturating_sub(usize::from(self.output.as_str().ends_with('\n')))
    }

    // ── ShellPhase accessors ─────────────────────────────────────────────

    pub fn phase(&self) -> ShellPhase {
        self.phase
    }

    pub fn bootstrap_ready(&self) -> bool {
        self.bootstrap_ready
    }

    /// True while a command is executing and output should be captured.
    /// The `Terminal` print path gates on this (plus an alt-screen check).
    pub fn is_capturing(&self) -> bool {
        self.phase == ShellPhase::CommandExecuting && self.screen_document_start.is_none()
    }

    pub fn begin_screen_owned_output(&mut self, document_start: u64) {
        if self.phase == ShellPhase::CommandExecuting && self.screen_document_start.is_none() {
            self.output.clear();
            // FIX_LIVE_STYLED_OUTPUT review m2: a plain-phase snapshot must not
            // survive the screen takeover — the cleared capture no longer
            // matches its line indices, and finalize's screen-snapshot fallback
            // would otherwise persist it.
            self.live_styled_snapshot = None;
            self.last_live_styled_publish = None;
            self.live_output_version = self.live_output_version.wrapping_add(1);
            self.screen_document_start = Some(document_start);
            self.activate_screen_continuation();
        }
    }

    pub fn screen_document_start(&self) -> Option<u64> {
        self.screen_document_start
    }

    /// All finished blocks (newest last). The history panel / search read this
    /// (includes blocks loaded from SQLite on startup).
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// All blocks visible in the main Warp-style block view. v1.7.5: this now
    /// includes blocks loaded from SQLite on startup/Restore (so users see
    /// their previous session's history), plus blocks created this session.
    /// `screen_owned_blocks` distinguishes the two for continuation logic;
    /// the panel / search use [`blocks`] directly.
    pub fn session_blocks(&self) -> &[Block] {
        let start = self.session_start.min(self.blocks.len());
        &self.blocks[start..]
    }

    /// Take the finished-but-unpersisted blocks. The app calls this after each
    /// PTY batch and inserts them into SQLite; the blocks remain in [`blocks`].
    pub fn drain_unpersisted(&mut self) -> Vec<Block> {
        std::mem::take(&mut self.unpersisted)
    }

    /// Toggle the fold (collapse/expand) state of a block by id. Session-only
    /// (not persisted to SQLite in the MVP — the insert-time `collapsed` is).
    pub fn toggle_collapse(&mut self, id: BlockId) {
        if let Some(b) = self.blocks.iter_mut().find(|b| b.id == id) {
            b.collapsed = !b.collapsed;
            self.dirty_blocks.insert(id.0);
        }
    }

    /// The currently-running command (between `133;B` and `133;D`), for the
    /// renderer's live block during CommandExecuting (e.g. an interactive
    /// `sudo su`). `None` when nothing is in flight.
    ///
    /// v1.11.7 (PLAN_v1117_SHADOW_BLOCK_VIEW §三 M1.4, P0-2): also `Some`
    /// during the deferred-exit settle window (`settling`), so the live block
    /// never vanishes for the 200ms between `defer_screen_command_end` and
    /// `finish_deferred_screen_command` — the phase already flipped to
    /// AtPrompt at defer time, and without the widened gate the renderer
    /// would flash a naked grid exactly when the handle is closing.
    /// v1.13.11 P1-1: the current command generation (see the field doc) —
    /// the snapshot identity the renderer keys the in-flight warm watermark
    /// on. Monotonic per `133;B`, never reused within a session.
    pub fn command_generation(&self) -> u64 {
        self.command_generation
    }

    /// v1.13.13 FIX-D: current screen-rewrite epoch (see the field doc).
    /// Folds into the renderer's in-flight warm watermark identity together
    /// with [`command_generation`](Self::command_generation) so a wholesale
    /// snapshot rewrite invalidates the append-only live watermark.
    pub fn screen_rewrite_epoch(&self) -> u64 {
        self.screen_rewrite_epoch
    }

    pub fn in_flight(&self) -> Option<InFlightBlock<'_>> {
        if self.phase != ShellPhase::CommandExecuting && !self.settling {
            return None;
        }
        let command = self
            .continuation_base
            .as_ref()
            .map_or(self.pending_command.as_deref()?, |block| {
                block.command.as_str()
            });
        Some(InFlightBlock {
            command,
            cwd: self
                .continuation_base
                .as_ref()
                .and_then(|block| block.cwd.as_deref())
                .or(self.pending_cwd.as_deref()),
            // Base-first, mirroring `command`/`cwd` above: during the settle
            // window `pending_started` was NOT taken (the base's `started_at`
            // is authoritative), so a continuation session's live header shows
            // the session start. `now` fallback matches continuation.rs's
            // finalize (unreachable under the normal state machine).
            started_at: self
                .continuation_base
                .as_ref()
                .map(|block| block.started_at)
                .unwrap_or_else(|| self.pending_started.unwrap_or_else(SystemTime::now)),
            output: self.output.as_str(),
            // FIX_LIVE_STYLED_OUTPUT: plain-path commands read the throttled
            // streaming snapshot; screen-owned commands keep reading the
            // caller-supplied screen-snapshot field (publishes never fire
            // while `is_capturing()` is false, so neither can clobber the
            // other).
            styled_output: self
                .live_styled_snapshot
                .as_ref()
                .or(self.styled_output.as_ref()),
            version: self.live_output_version,
            screen_origin: self.screen_document_start.is_some(),
            min_write_offset: self.output.min_write_offset_handle(),
        })
    }

    /// Load previously-persisted blocks (e.g. on startup from SQLite). They go
    /// straight into the history list (already persisted, so NOT into
    /// `unpersisted`), and `next_id` is advanced past the highest loaded id.
    ///
    /// v1.7.5: 加载的历史 block 现在也通过 `session_blocks()` 暴露给主视图，
    /// 这样启动/Restore 后用户能看到上次的命令记录（与 Warp 行为一致）。
    /// `session_start` 保持为 0，让 `session_blocks()` 返回全部 blocks。
    /// `screen_owned_blocks` 集合保证 continuation 逻辑只匹配本会话产生的 block，
    /// 不会误判加载的历史 block。
    pub fn load_blocks(&mut self, blocks: Vec<Block>) {
        for b in &blocks {
            self.ids.observe(b.id.0);
            self.dirty_blocks.insert(b.id.0);
            self.loaded_ids.insert(b.id.0);
        }
        self.blocks.extend(blocks);
        // session_start 保持为初始值 0，让 session_blocks() 包含加载的历史。
        // 之前这里设为 self.blocks.len() 是为了"避免显示 phantom pre-session
        // history"，但用户期望 Restore 后能看到历史命令记录。
    }

    /// Share persisted IDs across tabs; isolated trackers remain local.
    ///
    /// T14 (PLAN_v11217 §3.9): the shared allocator is now a hi/lo
    /// [`BlockIdPool`](crate::block_id_sequence::BlockIdPool) backed by the
    /// persistent `meta.next_id` counter, so pruned ids are never reused.
    pub fn use_shared_id_allocator(&mut self, next_id: Arc<crate::block_id_sequence::BlockIdPool>) {
        self.ids.share(next_id);
    }

    // ── Marker-driven state transitions ──────────────────────────────────

    /// `133;A` — prompt start. Marks integration ready. If a command was
    /// mid-flight without a closing `133;D` (interrupted, e.g. Ctrl+C),
    /// finalize it with no exit code so it still appears in history.
    pub fn on_prompt_start(&mut self) {
        self.bootstrap_ready = true;
        if self.phase == ShellPhase::CommandExecuting {
            self.finalize(None);
        }
        self.phase = ShellPhase::AtPrompt;
    }

    /// `133;B` — command start (preexec). `command` is the prompt-row text the
    /// caller extracted from the grid. Begins output capture.
    pub fn on_command_start(&mut self, command: String) {
        self.prepare_screen_continuation(&command);
        self.pending_command = Some(command);
        self.pending_started = Some(SystemTime::now());
        self.pending_cwd = self.current_cwd.clone();
        self.output.clear();
        self.styled_output = None;
        self.live_styled_snapshot = None;
        // FIX_LIVE_STYLED_OUTPUT: the throttle must not leak across commands —
        // the next command's first printed byte publishes immediately.
        self.last_live_styled_publish = None;
        self.screen_document_start = None;
        self.live_output_version = self.live_output_version.wrapping_add(1);
        // v1.13.11 P1-1: a new command invalidates the renderer's in-flight
        // sentinel warm watermark (see the field doc — chained commands can
        // cross the D→A/B boundary inside one batch with no intervening
        // frame, so the forget-on-idle path alone is not enough).
        self.command_generation = self.command_generation.wrapping_add(1);
        // v1.14.2 (B1): the in-flight command IS a tab-title input — a new
        // command swaps the title's "▶ … • cmd" suffix.
        self.title_generation = self.title_generation.wrapping_add(1);
        self.phase = ShellPhase::CommandExecuting;
    }

    /// `133;C` — output start. Capture already began at `133;B`; this is a
    /// no-op for v0.4 (kept for symmetry and future marker-aware splitting).
    pub fn on_command_output_start(&mut self) {
        // intentionally empty
    }

    /// `133;D;<exit>` — command end. Finalizes the in-flight block.
    pub fn on_command_end(&mut self, exit_code: i32) {
        self.finalize(Some(exit_code));
        self.phase = ShellPhase::AtPrompt;
    }

    // ── Output capture (called from Terminal print / LF paths) ───────────

    /// Append a printed character to the in-flight block's output. No-op unless
    /// a command is executing. v1.7.0-A: `style` captures the current VT SGR
    /// attributes so the block preserves program-emitted colors after the live
    /// grid scrolls away.
    pub fn on_print(&mut self, c: char, style: CapturedStyle) {
        if self.is_capturing() {
            self.output.print(c, style, self.output_cap);
            self.live_output_version = self.live_output_version.wrapping_add(1);
            self.maybe_publish_live_styled(Instant::now());
        }
    }

    /// v1.0 P1.5-C2: Batch-append a run of printable ASCII bytes to the
    /// in-flight block's output. Avoids per-char `push_capped` method call
    /// overhead — one truncation check + one `extend_from_slice` instead of
    /// N individual `push` calls. No-op unless a command is executing.
    /// v1.7.0-A: `style` captures the current VT SGR attributes for the whole
    /// ASCII run — all bytes share one style since the fast path only fires
    /// when no SGR change occurred mid-run.
    /// F3 (PLAN_v1137): one clock sample per `Terminal::process` batch —
    /// the live-styled publish throttle needs only ~ms precision, and
    /// per-line `Instant::now()` was 2 clock_gettime per line over 2M-line
    /// floods (~3% of parse). Stale between batches by design (throttle
    /// decisions only).
    pub(crate) fn begin_batch(&mut self, now: Instant) {
        self.batch_clock = now;
    }

    pub fn on_print_ascii_run(&mut self, bytes: &[u8], style: CapturedStyle) {
        if !self.is_capturing() || bytes.is_empty() {
            return;
        }
        self.output.print_ascii(bytes, style, self.output_cap);
        self.live_output_version = self.live_output_version.wrapping_add(1);
        self.maybe_publish_live_styled(self.batch_clock);
    }

    /// Append a newline to the in-flight block's output. No-op unless a command
    /// is executing.
    pub fn on_newline(&mut self) {
        if self.is_capturing() {
            self.output.newline(self.output_cap);
            self.live_output_version = self.live_output_version.wrapping_add(1);
            // Line terminators are rewrite boundaries (spinner frame → "done"
            // line): a snapshot-category flip here publishes immediately, so
            // a de-styled rewrite can't end the stream with stale colors
            // still on the live view.
            self.boundary_publish_live_styled();
        }
    }

    pub fn on_carriage_return(&mut self) {
        if self.is_capturing() {
            self.output.carriage_return();
            self.live_output_version = self.live_output_version.wrapping_add(1);
            self.boundary_publish_live_styled();
        }
    }

    pub fn on_backspace(&mut self) {
        if self.is_capturing() {
            self.output.backspace();
            self.live_output_version = self.live_output_version.wrapping_add(1);
            self.boundary_publish_live_styled();
        }
    }

    pub fn on_erase_line(&mut self, mode: u16) {
        if self.is_capturing() {
            self.output.erase_line(mode);
            self.live_output_version = self.live_output_version.wrapping_add(1);
            self.boundary_publish_live_styled();
        }
    }

    /// Mirror CSI A/B/E/F (cursor up/down) into the capture buffer so
    /// multi-line progress bars (`ollama pull`, `brew upgrade`) repaint
    /// in place instead of appending new rows on every update.
    pub fn on_move_cursor_rows(&mut self, delta: isize) {
        if self.is_capturing() {
            self.output.move_cursor_rows(delta);
            self.live_output_version = self.live_output_version.wrapping_add(1);
            self.maybe_publish_live_styled(Instant::now());
        }
    }
}
