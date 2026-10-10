//! `osc_dispatch` body for the `vte::Perform` trait impl. perform.rs keeps
//! the single trait block and delegates here (v1.13.8 S2 zero-behavior
//! file-budget split; body moved verbatim, `impl Terminal` cross-file block
//! per the kitty_keyboard.rs / screen_exit precedent).

use super::attrs::ShellMarker;
use super::osc::{
    cap_osc_payload, parse_osc52, parse_osc7_cwd, parse_osc_notify, parse_osc_progress,
    parse_x11_color, Osc52Result,
};
use super::ui_events::UiEvent;
use super::{Terminal, PRIMARY_SCREEN_EXIT_SETTLE_DELAY};
use crate::blocks::ShellPhase;

impl Terminal {
    pub(super) fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.suppress_joined_scalar = false;
        if params.is_empty() {
            return;
        }

        let code = std::str::from_utf8(params[0]).unwrap_or("");

        match code {
            "0" | "2" => {
                if params.len() > 1 {
                    // VULN-009: cap before retaining — the osc_guard 1MiB raw
                    // cap is the only other bound, and a near-1MiB title would
                    // otherwise reach the NSWindow title verbatim.
                    self.title = cap_osc_payload(params[1]);
                }
            }
            "4" => {
                if params.len() >= 3 {
                    if let Ok(idx) = std::str::from_utf8(params[1])
                        .unwrap_or("")
                        .parse::<usize>()
                    {
                        if idx < 256 {
                            if let Some(color) = parse_x11_color(params[2]) {
                                self.palette[idx] = color;
                            }
                        }
                    }
                }
            }
            "11" => {
                // OSC 11 background query — answer the app theme's
                // background in xterm rgb:RR/GG/BB form so TUIs detect
                // dark vs light. Only queries (`OSC 11` / `OSC 11;?`)
                // are answered; a set request (`OSC 11;rgb:..`) is not.
                let is_query =
                    params.len() == 1 || params.get(1).is_some_and(|p| p.is_empty() || p == b"?");
                if is_query {
                    let [r, g, b] = [
                        self.background_color.r,
                        self.background_color.g,
                        self.background_color.b,
                    ];
                    self.respond(format!("\x1b]11;rgb:{r:02x}/{g:02x}/{b:02x}\x1b\\").as_bytes());
                }
            }
            "7" => {
                if params.len() > 1 {
                    // VULN-009: same payload cap as the title. A truncated
                    // file:// URL yields a bounded wrong path — acceptable,
                    // cwd is display/git-probe only.
                    if let Some(path) = parse_osc7_cwd(cap_osc_payload(params[1]).as_bytes()) {
                        self.cwd = Some(path.clone());
                        // Mirror into the block tracker so each block is stamped
                        // with the dir it ran in (for the block-view header).
                        self.block_tracker.set_cwd(Some(path));
                    }
                }
            }
            "9" => {
                // OSC 9 has three faces:
                //   1. `9;4;state[;progress]` — Dock progress (iTerm2 family,
                //      PLAN_v1115 §M1; state semantics in osc.rs).
                //   2. `9;git=<branch>` — Weft shell-hook branch report
                //      (legacy, kept; the git fixture must stay green).
                //   3. `9;message` — remote notification request (iTerm2).
                if params.get(1).is_some_and(|p| *p == b"4") {
                    if let Some(progress) = parse_osc_progress(
                        params.get(2).copied().unwrap_or(b""),
                        params.get(3).copied().unwrap_or(b""),
                    ) {
                        self.push_ui_event(UiEvent::DockProgress(progress));
                    }
                } else if let Some(payload) = params.get(1) {
                    if let Ok(s) = std::str::from_utf8(payload) {
                        if let Some(branch) = s.strip_prefix("git=") {
                            self.git_branch = Some(branch.to_string());
                        } else if let Some(text) = parse_osc_notify(params) {
                            self.push_ui_event(UiEvent::RemoteNotify {
                                title: text.title,
                                body: text.body,
                            });
                        }
                    }
                }
            }
            "52" => {
                // OSC 52 — clipboard exchange (PLAN_v1115 §M1). Parsed pure:
                // the app layer gates + answers in its own feature modules.
                match parse_osc52(params) {
                    Osc52Result::Read => self.push_ui_event(UiEvent::ClipboardReadRequest),
                    Osc52Result::Write { data, truncated } => {
                        self.push_ui_event(UiEvent::ClipboardWrite { data, truncated });
                    }
                    Osc52Result::Ignore => {
                        tracing::trace!("OSC 52 ignored (non-clipboard selection or malformed)")
                    }
                }
            }
            "777" => {
                // OSC 777;notify;title;body — remote notification
                // (rxvt/wezterm family; non-notify subcommands → no event).
                if let Some(text) = parse_osc_notify(params) {
                    self.push_ui_event(UiEvent::RemoteNotify {
                        title: text.title,
                        body: text.body,
                    });
                }
            }
            "8" => {
                // OSC 8 ; params ; URI ST — hyperlink.
                //   `OSC 8 ; ; URI ST`     → start hyperlink to URI (no id).
                //   `OSC 8 ; id=K ; URI ST`→ start hyperlink with id K (kitty
                //                            extension; we dedup by URL anyway).
                //   `OSC 8 ; ; ST`         → end hyperlink (empty URI).
                // We dedup URLs in the registry and remember the active id;
                // `print()` stamps cells with HYPERLINK while this is Some.
                // v1.6.1: register() returns 0 when URL is too long or the
                // registry is full — treat 0 as "no link" so the cell doesn't
                // get tagged with an unresolvable id.
                if params.len() >= 3 {
                    let uri = std::str::from_utf8(params[2]).unwrap_or("");
                    if uri.is_empty() {
                        self.active_hyperlink_id = None;
                    } else {
                        let id = self.hyperlinks.register(uri.to_string());
                        self.active_hyperlink_id = (id != 0).then_some(id);
                    }
                } else {
                    // OSC 8 ;; ST (no URI field) — clear.
                    self.active_hyperlink_id = None;
                }
            }
            "133" => {
                if params.len() > 1 {
                    let tagged = params.iter().skip(2).any(|p| *p == b"weft-shell");
                    if !self.capabilities.accepts_shell_marker(tagged) {
                        tracing::trace!("ignored untagged application OSC 133 zone");
                        return;
                    }
                    match params[1] {
                        b"A" => {
                            self.snapshot_primary_screen_output();
                            // v1.10.7: a screen-owned TUI session whose shell
                            // emits 133;A (precmd) while still running — e.g. pi
                            // spawns an interactive zsh that inherits
                            // WEFT_SHELL_INTEGRATION and re-emits the markers per
                            // internal command. If a pending exit ALREADY exists
                            // (the precmd pair `133;D → 133;A` arrived together,
                            // or `133;A` follows `133;D` of the previous marker
                            // run), deferring again would reset `phase` to
                            // AtPrompt and split the session block per internal
                            // command. Only defer when no exit is pending.
                            let defer_screen_exit =
                                self.block_tracker.screen_document_start().is_some()
                                    && self.block_tracker.phase() == ShellPhase::CommandExecuting
                                    && self.capabilities.primary_screen_exit.is_none();
                            self.capabilities.primary_screen_cursor_ops = 0;
                            self.capabilities.primary_screen_relative_addressing_seen = false;
                            self.reset_primary_screen_synchronized_frame();
                            self.attrs = Default::default();
                            self.shell_markers.push(ShellMarker::PromptStart);
                            if defer_screen_exit {
                                self.defer_primary_screen_exit(None);
                                // v1.10.7: do NOT unlock the render-mode lock
                                // here. This branch is also reachable by the
                                // FIRST marker of a nested run (pi's inner
                                // zsh prompt before any internal command
                                // completed → no pending exit yet), and
                                // unlocking would let the next CUP repaint
                                // flip the session to the live grid. The lock
                                // is released in `settle_primary_screen_exit`
                                // (the real finalize path) and at a real
                                // 133;B command start.
                            } else if self.capabilities.primary_screen_exit.is_some()
                                && self.block_tracker.screen_document_start().is_some()
                            {
                                // v1.10.7: this 133;A is the second marker of a
                                // nested precmd pair (`133;D → 133;A`) from a
                                // shell running INSIDE the screen-owned TUI — the
                                // pending exit belongs to the first marker of the
                                // run and the session is still executing. Keep it
                                // executing (do NOT let `on_prompt_start` finalize
                                // the in-flight block). The render-mode lock must
                                // SURVIVE nested markers.
                                self.block_tracker.resume_screen_command();
                            } else {
                                self.block_tracker.on_prompt_start();
                                // v1.10.7: real prompt boundary (or shell
                                // outside a screen session).
                            }
                            // Clear the git branch: the precmd hook re-emits
                            // OSC 9;git= if (and only if) the cwd is still a git
                            // repo. Without this, leaving a repo keeps the stale
                            // branch label forever (the hook sends nothing in a
                            // non-git dir, so git_branch was never cleared).
                            self.git_branch = None;
                            // Clear any stale submit flag so a missing 133;B
                            // (crashed / non-integrated sub-shell) can't pin the
                            // editor in passthrough forever.
                            self.command_from_editor = None;
                            // FIX_ORPHAN_PARSE_ERROR_OUTPUT: same staleness
                            // guard for the staging buffer — a D-less sequence
                            // (interrupted / crashed shell) must not leak its
                            // staged bytes into a LATER command's block.
                            let stale = self.drop_preexec_staging();
                            if stale > 0 {
                                tracing::warn!(
                                    bytes = stale,
                                    "133;A without 133;B/D: dropped stale pre-exec staging"
                                );
                            }
                        }
                        b"B" => {
                            // FIX_ORPHAN_PARSE_ERROR_OUTPUT (normal path): the
                            // ZLE repaint of the accepted line staged between
                            // editor submit and preexec must never enter a
                            // block — discard it here. This is also the timing
                            // mutex with the orphan-D synthesis below: B both
                            // clears the staging and consumes
                            // `command_from_editor`, so a later D can only
                            // synthesize when THIS branch did not run.
                            let staged = self.drop_preexec_staging();
                            if staged > 0 {
                                tracing::trace!(
                                    bytes = staged,
                                    "133;B discarded pre-exec staging (ZLE echo)"
                                );
                            }
                            // v1.10.7: nested-shell guard. A screen-owned TUI
                            // session (pi/openclaw) whose shell integration runs
                            // inside the app (pi spawns interactive zsh) emits
                            // `133;B` <200ms after the `133;A`/`133;D` precmd
                            // pair — that is a NEW INTERNAL COMMAND of the still
                            // running TUI, not a command typed at the shell after
                            // the app exited. Settling here would finalize the
                            // session block per internal command (one prompt
                            // split into many blocks). A real app exit leaves the
                            // user time to type the next command, so the pending
                            // exit would already have been settled by the idle
                            // timer. When the pending exit is still younger than
                            // the settle window, cancel it and keep the in-flight
                            // block: the nested command's output still flows into
                            // the screen snapshot (screen-owned capture), so the
                            // session block grows without splitting.
                            let nested_marker =
                                self.capabilities.primary_screen_exit.as_ref().is_some_and(
                                    |pending| {
                                        pending.last_activity.elapsed()
                                            < PRIMARY_SCREEN_EXIT_SETTLE_DELAY
                                    },
                                ) && self.block_tracker.screen_document_start().is_some();
                            if nested_marker {
                                self.capabilities.primary_screen_exit = None;
                                self.block_tracker.resume_screen_command();
                                self.capabilities.primary_history_view = false;
                                self.capabilities.primary_screen_cursor_ops = 0;
                                self.capabilities.primary_screen_relative_addressing_seen = false;
                                self.reset_primary_screen_synchronized_frame();
                                self.shell_markers.push(ShellMarker::CommandStart);
                            } else {
                                self.settle_primary_screen_exit();
                                self.capabilities.primary_history_view = false;
                                self.capabilities.primary_screen_cursor_ops = 0;
                                // v1.10.7: real command start — a new
                                // command re-detects its render mode at
                                // first screen ownership.
                                self.reset_primary_screen_synchronized_frame();
                                self.shell_markers.push(ShellMarker::CommandStart);
                                // 133;B (preexec): if the editor submitted the
                                // command, record that; otherwise snapshot the grid
                                // row (real shells emit a newline first, so the
                                // cursor may sit below the command —
                                // `snapshot_command_line` walks up).
                                let command = if let Some(c) = self.command_from_editor.take() {
                                    c
                                } else {
                                    self.snapshot_command_line()
                                };
                                self.freeze_primary_screen_document_candidate();
                                self.block_tracker.on_command_start(command);
                                // v1.11.7 (PLAN_v1117_SHADOW_BLOCK_VIEW §三
                                // M1.1, P1-2): real command boundary — clear
                                // the interactive-stdin exemption. Deliberately
                                // NOT at 133;A/D: a nested marker inside a
                                // still-running screen-owned TUI must not
                                // clear the flag mid-interaction (the nested
                                // branch above never reaches this line).
                                self.capabilities.interactive_stdin_seen = false;
                                // v1.11.7: same boundary hygiene for the
                                // caret anchor (see
                                // `settle_primary_screen_exit`) — a stale
                                // snapshot line must not anchor the caret
                                // past the new live block.
                                self.capabilities.primary_screen_cursor_snapshot_line = None;
                                self.capabilities.primary_screen_cursor_segment_len = None;
                                self.capabilities.last_caret_snapshot_cursor = None;
                            }
                        }
                        b"C" => {
                            self.shell_markers.push(ShellMarker::CommandOutputStart);
                            self.block_tracker.on_command_output_start();
                        }
                        b"D" => {
                            // v1.11.4 (PLAN_v1114 §1.3): back at the prompt —
                            // SIGKILL'd TUIs emit no PtyExit, so this marker
                            // clears negotiated kitty flags; the exit paths
                            // (tab.rs / lifecycle.rs) hook the shell-death case.
                            self.kitty_reset();
                            // v1.10.7: see `133;A` — a `133;D` arriving while an
                            // exit is already pending is the shell integration's
                            // precmd pair (`133;D;rc → 133;A`) or the closing of
                            // a nested command marker run; the FIRST marker of
                            // the run already deferred. Deferring again would
                            // overwrite the exit code and reset `phase`, churning
                            // the session block.
                            let defer_screen_exit =
                                self.block_tracker.screen_document_start().is_some()
                                    && self.capabilities.primary_screen_exit.is_none();
                            self.snapshot_primary_screen_output();
                            self.capabilities.primary_screen_cursor_ops = 0;
                            self.capabilities.primary_screen_relative_addressing_seen = false;
                            self.reset_primary_screen_synchronized_frame();
                            self.attrs = Default::default();
                            let exit_code = if params.len() > 2 {
                                std::str::from_utf8(params[2])
                                    .ok()
                                    .and_then(|s| s.parse::<i32>().ok())
                                    .unwrap_or(0)
                            } else {
                                0
                            };
                            self.shell_markers
                                .push(ShellMarker::CommandEnd { exit_code });
                            if defer_screen_exit {
                                self.defer_primary_screen_exit(Some(exit_code));
                            } else if self.capabilities.primary_screen_exit.is_some()
                                && self.block_tracker.screen_document_start().is_some()
                            {
                                // v1.10.7: nested precmd pair — the first marker
                                // of this run already deferred. Do NOT let
                                // `on_command_end` finalize the in-flight
                                // session block mid-session.
                                // v1.10.7 (reviewer LOW): if the pending exit
                                // came from a `133;A` (exit_code None — the
                                // nested run's first marker), this REAL `133;D`
                                // carries the authoritative exit code of the
                                // TUI session; upgrade it so the finalized
                                // block shows the session's code, not None.
                                if let Some(pending) = &mut self.capabilities.primary_screen_exit {
                                    if pending.exit_code.is_none() {
                                        pending.exit_code = Some(exit_code);
                                    }
                                }
                            } else if let Some((command, staged)) = self.take_orphan_staging() {
                                // FIX_ORPHAN_PARSE_ERROR_OUTPUT (parse-error
                                // path): zsh rejected the line before preexec,
                                // so no `133;B` ever opened a capture window.
                                // Build the block retroactively from the
                                // staged error output instead of dropping it
                                // on the floor (previously this arm was a bare
                                // noop — blocks.db had no record at all).
                                tracing::info!(
                                    rc = exit_code,
                                    bytes = staged.as_str().len(),
                                    command = %command,
                                    "orphan 133;D: synthesizing block from pre-exec staging"
                                );
                                self.block_tracker
                                    .on_orphan_command_end(exit_code, command, staged);
                            } else {
                                self.block_tracker.on_command_end(exit_code);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                tracing::trace!(code, "unhandled OSC");
            }
        }
    }
}
