//! v1.14.2 (B1, PLAN_v1142 §3): the tab-title snapshot cache.
//!
//! `tab_bar_state()` used to take every tab's FairMutex twice per frame
//! (in-flight command + `launch_cwd`) and build two Strings per tab — per
//! frame, forever, contending with the parse workers during output storms
//! (tab 风暴债①). The cache keys each entry on the tab's `session_id` plus
//! the tracker's B1 [`title_generation`](weft_core::blocks::BlockTracker::
//! title_generation): a frame only re-takes a tab's lock when that tab's
//! generation changed (in-flight set/clear, OSC 7 `set_cwd`) or the tab
//! identity at that index changed (open/close/reorder — `session_id` makes
//! the post-close gen-coincidence case impossible to false-reuse). Grid
//! dims are refreshed in the SAME probe read so the A4 fingerprint always
//! sees current dims without a second lock.
//!
//! Main-thread-only title inputs that live OUTSIDE the tracker (the B2
//! spawn-cwd fallback, the restore snapshot attach flipping
//! `restored_cwd_fallback`) invalidate entries explicitly —
//! [`App::invalidate_tab_title`] / [`App::invalidate_tab_title_cache`].
//!
//! Cache storage is a `RefCell` on `TabBarState`: both consumers
//! (`tab_bar_state`, `AccessibilityKey::collect`) run on the main thread
//! with `&self`, and the draw path's borrow structure (redraw_controller
//! captures the draw state before `active_mut()`) forbids switching the
//! signature to `&mut self`.

use std::cell::Ref;

use crate::accessibility_model::stable_id;
use crate::paint::tab_bar::{tab_title, TabTitle};
use crate::tab::Tab;
use crate::App;

/// One cached tab-title snapshot (`tab_bar_state` + A4 fingerprint inputs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TabTitleCached {
    /// Tab-level session id — the identity half of the reuse key. `0` is
    /// the "invalidated" sentinel (`NEXT_SESSION_ID` starts at 1), written
    /// by [`App::invalidate_tab_title`] to force a rebuild while keeping
    /// the Vec length (and every other entry) intact.
    pub(crate) session_id: u64,
    /// [`BlockTracker::title_generation`] at build time — the content half.
    pub(crate) gen: u64,
    /// The rendered title (compact strip label + hover tooltip).
    pub(crate) title: TabTitle,
    /// `stable_id` of the in-flight command text, if any (A4 fingerprint).
    pub(crate) in_flight_cmd: Option<u64>,
    /// `stable_id` of `launch_cwd()` (live OSC 7 cwd or restore/spawn
    /// fallback), if any (A4 fingerprint).
    pub(crate) cwd: Option<u64>,
    pub(crate) grid_rows: usize,
    pub(crate) grid_cols: usize,
    /// Bumped on every REBUILD. Test-only observability: a refresh that
    /// reuses everything leaves this value untouched, which is how the
    /// reuse tests prove no rebuild happened.
    pub(crate) rebuild_epoch: u64,
}

impl TabTitleCached {
    /// Placeholder for a slot whose tab has no terminal (PTY-less pane) or
    /// that has not been built yet. `gen 0` can equal a fresh tracker's
    /// generation, so the `session_id` sentinel/identity check is what
    /// actually gates reuse — a placeholder never carries the real
    /// session id, so it always rebuilds on first sight.
    fn placeholder() -> Self {
        Self {
            session_id: 0,
            gen: 0,
            title: TabTitle {
                compact: String::new(),
                tooltip: String::new(),
            },
            in_flight_cmd: None,
            cwd: None,
            grid_rows: 0,
            grid_cols: 0,
            rebuild_epoch: 0,
        }
    }
}

/// Refresh `cache` against `tabs` (the B1 kernel). Per tab: ONE short lock
/// probe (generation + grid dims); a matching `(session_id, gen)` entry is
/// reused verbatim (dims stay frame-fresh — a resize is not a gen bump),
/// anything else rebuilds under that same take. Pure over `(tabs, cache)`
/// so the reuse/rebuild truth table is testable without an event loop.
pub(crate) fn refresh_title_cache(tabs: &[Tab], cache: &mut Vec<TabTitleCached>) {
    // Length mismatch (open/close/restore) — entry positions are meaningless
    // past the new length; rebuild every slot cleanly.
    if cache.len() != tabs.len() {
        cache.clear();
        cache.resize(tabs.len(), TabTitleCached::placeholder());
    }
    for (i, tab) in tabs.iter().enumerate() {
        let session_id = tab.session_id;
        let prev_epoch = cache[i].rebuild_epoch;
        let probed = tab.with_terminal(|t| {
            let gen = t.block_tracker().title_generation();
            let cached = &mut cache[i];
            if cached.session_id == session_id && cached.gen == gen {
                // Reuse: only the dims stay frame-fresh.
                cached.grid_rows = t.grid().num_rows;
                cached.grid_cols = t.grid().num_cols;
                return;
            }
            // Rebuild — ONE lock take for every title input. The cwd
            // fallback read below is a plain pane field (no lock; D9 rule 2
            // stands — no re-lock inside the guard).
            let command = t.block_tracker().in_flight().map(|f| f.command.to_string());
            let cwd = t
                .cwd()
                .map(str::to_owned)
                .or_else(|| tab.restored_cwd_fallback().map(str::to_owned));
            let (grid_rows, grid_cols) = (t.grid().num_rows, t.grid().num_cols);
            *cached = TabTitleCached {
                session_id,
                gen,
                title: tab_title(i, cwd.as_deref(), command.as_deref()),
                in_flight_cmd: command.as_deref().map(stable_id),
                cwd: cwd.as_deref().map(stable_id),
                grid_rows,
                grid_cols,
                rebuild_epoch: prev_epoch.wrapping_add(1),
            };
        });
        if probed.is_none() {
            // PTY-less pane (spawn placeholder / test pane): park the
            // identity so the slot still compares against the right tab.
            cache[i] = TabTitleCached {
                session_id,
                ..TabTitleCached::placeholder()
            };
        }
    }
}

impl App {
    /// Refresh the title cache (cheap per-tab probe) and hand back the
    /// snapshot. Consumers: `tab_bar_state` (labels/tooltips) and the A4
    /// accessibility fingerprint (per-tab key fields) — one refresh serves
    /// both, so neither takes a terminal lock beyond the probe.
    pub(crate) fn tab_titles(&self) -> Ref<'_, Vec<TabTitleCached>> {
        {
            let mut cache = self.tab_bar.title_cache.borrow_mut();
            refresh_title_cache(self.sessions.tabs(), &mut cache);
        }
        self.tab_bar.title_cache.borrow()
    }

    /// Invalidate ONE tab's cached entry (the B2 spawn-cwd backfill: a
    /// main-thread fallback write the tracker generation cannot see).
    /// Session id `0` is the forced-rebuild sentinel.
    pub(crate) fn invalidate_tab_title(&self, session_id: u64) {
        for entry in self.tab_bar.title_cache.borrow_mut().iter_mut() {
            if entry.session_id == session_id {
                entry.session_id = 0;
            }
        }
    }

    /// Invalidate the WHOLE cache — the restore-attach completion point
    /// (R2 review P3: covers the main-thread `restored_cwd_fallback` flip;
    /// next frame rebuilds every entry once).
    pub(crate) fn invalidate_tab_title_cache(&self) {
        self.tab_bar.title_cache.borrow_mut().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::Pane;

    /// PTY-less test tab (`with_terminal_only` = live Terminal, no PTY —
    /// the event loop cannot exist in tests, so no real spawn happens).
    fn test_tab() -> Tab {
        Tab::with_single_pane(Pane::with_terminal_only(1000))
    }

    fn process(tab: &Tab, bytes: &[u8]) {
        tab.with_terminal(|t| t.process(bytes));
    }

    /// 真值表 1: gen 不变 → 复用（rebuild_epoch 不动）；gen bump → 重建。
    #[test]
    fn unchanged_generation_reuses_and_bump_rebuilds() {
        let tabs = vec![test_tab()];
        let mut cache = Vec::new();
        refresh_title_cache(&tabs, &mut cache);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache[0].session_id, tabs[0].session_id);
        assert_eq!(cache[0].rebuild_epoch, 1);

        refresh_title_cache(&tabs, &mut cache);
        assert_eq!(cache[0].rebuild_epoch, 1, "unchanged gen must reuse");

        // OSC 7 → set_cwd → the tracker's title generation bumps.
        process(&tabs[0], b"\x1b]7;file://localhost/Users/me/Claude\x07");
        refresh_title_cache(&tabs, &mut cache);
        assert_eq!(cache[0].rebuild_epoch, 2, "gen bump must rebuild");
    }

    /// 真值表 2 (set_cwd 冻结回归): the title FOLLOWS the cwd after OSC 7 —
    /// the exact regression a missing `set_cwd` bump would cause.
    #[test]
    fn cwd_change_updates_the_title() {
        let tabs = vec![test_tab()];
        let mut cache = Vec::new();
        refresh_title_cache(&tabs, &mut cache);
        assert_eq!(cache[0].title.compact, "Tab 1", "no cwd yet → fallback");

        process(&tabs[0], b"\x1b]7;file://localhost/Users/me/Claude\x07");
        refresh_title_cache(&tabs, &mut cache);
        // ≤20 chars → compact keeps the full path (tab_title contract).
        assert_eq!(cache[0].title.compact, "/Users/me/Claude");
        assert_eq!(cache[0].title.tooltip, "/Users/me/Claude");
        assert_eq!(
            cache[0].cwd,
            Some(stable_id("/Users/me/Claude")),
            "A4 fingerprint sees the new cwd"
        );
    }

    /// 真值表 3: in-flight set (133;B) and clear (133;D) each rebuild and
    /// swap the "▶ … • cmd" suffix on and off.
    #[test]
    fn in_flight_set_and_clear_rebuild() {
        let tabs = vec![test_tab()];
        process(&tabs[0], b"\x1b]7;file://localhost/tmp/proj\x07");
        let mut cache = Vec::new();
        refresh_title_cache(&tabs, &mut cache);
        assert_eq!(cache[0].title.compact, "/tmp/proj");

        process(&tabs[0], b"\x1b]133;A\x07cargo test\x1b]133;B\x07");
        refresh_title_cache(&tabs, &mut cache);
        assert_eq!(cache[0].title.compact, "▶ /tmp/proj • cargo test");
        assert_eq!(
            cache[0].in_flight_cmd,
            Some(stable_id("cargo test")),
            "A4 fingerprint sees the in-flight command"
        );

        process(&tabs[0], b"\x1b]133;D;0\x07");
        refresh_title_cache(&tabs, &mut cache);
        assert_eq!(
            cache[0].title.compact, "/tmp/proj",
            "finalize clears the suffix"
        );
        assert_eq!(cache[0].in_flight_cmd, None);
    }

    /// 真值表 4 (close-shift): after closing tab 0 the surviving tab lands
    /// at index 0. Its own gen may coincidentally equal the dead entry's —
    /// the session_id half of the key must force the rebuild.
    #[test]
    fn close_shift_rebuilds_by_session_identity_not_gen_coincidence() {
        let a = test_tab();
        process(&a, b"\x1b]7;file://localhost/tmp/aaa\x07");
        let b = test_tab();
        process(&b, b"\x1b]7;file://localhost/tmp/bbb\x07");
        let (id_b, cwd_b) = (b.session_id, b.launch_cwd());
        let mut tabs = vec![a, b];
        let mut cache = Vec::new();
        refresh_title_cache(&tabs, &mut cache);
        assert_eq!(cache[0].title.tooltip, "/tmp/aaa");

        // Close tab 0 → B shifts into slot 0 with gen still 0-ish; the
        // identity check (not the gen) must rebuild the entry.
        tabs.remove(0);
        refresh_title_cache(&tabs, &mut cache);
        assert_eq!(cache[0].session_id, id_b);
        assert_eq!(cache[0].cwd, cwd_b.as_deref().map(stable_id));
        assert_eq!(cache[0].title.tooltip, "/tmp/bbb");
    }

    /// 真值表 5: a PTY-less pane parks an identity-only placeholder and a
    /// later refresh stays stable (no panic, no phantom rebuild churn).
    #[test]
    fn pty_less_pane_parks_a_placeholder() {
        let tabs = vec![Tab::empty()];
        let mut cache = Vec::new();
        refresh_title_cache(&tabs, &mut cache);
        assert_eq!(cache[0].session_id, tabs[0].session_id);
        assert_eq!(cache[0].title.compact, "");
        refresh_title_cache(&tabs, &mut cache);
        assert_eq!(cache[0].rebuild_epoch, 0, "placeholder stays parked");
    }

    /// B2 (PLAN_v1142 §3) 兜底链: all-None → "Tab N"; None + spawn-cwd
    /// fallback → titled from the fallback; live OSC 7 cwd wins last. The
    /// fallback write is main-thread-only (invisible to the generation), so
    /// each production backfill is paired with `App::invalidate_tab_title`
    /// — mirrored here by dropping the entry before the refresh.
    #[test]
    fn title_fallback_chain_live_cwd_then_spawn_fallback_then_tab_n() {
        let mut tabs = vec![test_tab()];
        let mut cache = Vec::new();
        refresh_title_cache(&tabs, &mut cache);
        assert_eq!(cache[0].title.compact, "Tab 1", "all None → fallback name");

        // B2 backfill: the adopt step writes the spawn request's cwd into
        // the pane fallback, then invalidates the (now stale) entry.
        tabs[0]
            .active_mut()
            .set_restored_cwd_fallback(Some("/fallback/dir".into()));
        cache.clear(); // = App::invalidate_tab_title(session_id)
        refresh_title_cache(&tabs, &mut cache);
        assert_eq!(cache[0].title.compact, "/fallback/dir");

        // The shell's first OSC 7 lands (generation bump — no manual
        // invalidation needed): the live cwd wins.
        process(&tabs[0], b"\x1b]7;file://localhost/live/dir\x07");
        refresh_title_cache(&tabs, &mut cache);
        assert_eq!(cache[0].title.compact, "/live/dir");
        assert_eq!(cache[0].cwd, Some(stable_id("/live/dir")));
    }
}
