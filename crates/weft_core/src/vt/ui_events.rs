//! App-facing events discovered during VT parsing (v1.11.5, PLAN_v1115 §M1).
//!
//! The VT parser is pure: grid mutations stay in the grid, and anything the
//! *application* must act on (clipboard access, notifications, Dock badge)
//! is collected here and drained by `Terminal::take_ui_events()` at the same
//! drain point as `take_response()`. `reset()` replaces the whole `Terminal`
//! object, so RIS automatically clears the queue (no manual clear needed).

/// Event emitted by an OSC sequence, drained by the app layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    /// OSC 9;message / OSC 777;notify;title;body — the program asks for a
    /// notification. Subject to the app-side focus gate + rate limiter.
    RemoteNotify { title: String, body: String },
    /// OSC 52;c;<b64> — the program writes the system clipboard. `truncated`
    /// is set when the decoded payload exceeded [`OSC52_MAX_BYTES`] and was
    /// cut (the toast about it is app-side).
    ClipboardWrite { data: Vec<u8>, truncated: bool },
    /// OSC 52;c;? — the program asks to READ the system clipboard. The app
    /// decides (permission prompt); the answer is written straight to the
    /// PTY, never routed through `pending_output` (see PLAN_v1115 F3).
    ClipboardReadRequest,
    /// OSC 9;4;state[;progress] — Dock icon badge update (see [`DockProgress`]).
    DockProgress(DockProgress),
}

/// Dock badge state derived from OSC 9;4 (iTerm2-family semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockProgress {
    /// state 0 — clear the badge immediately.
    Clear,
    /// state 1 — indeterminate spinner (badge text `…`).
    Indeterminate,
    /// state 2 — failed (badge text `!`).
    Failed,
    /// state 3 — percentage, clamped 0..=100 (badge text `42%`).
    Percent(u8),
}

// ── Shared constants (PLAN_v1115 D-j) ────────────────────────────────────

/// OSC 52 payload cap (bytes, after base64 decode). Aligned with
/// `osc_guard::OSC_GUARD_PAYLOAD_CAP` so the guard's byte-level cut and this
/// business cap never create a second hidden truncation layer at ordinary
/// sizes: the guard already swallowed anything ≥ 1MiB raw, so this cap only
/// fires on edge payloads (~750 KiB decoded from 1MiB raw).
pub const OSC52_MAX_BYTES: usize = 1024 * 1024;

/// OSC 52 read-reply cap (bytes of clipboard content). A clipboard larger
/// than this is answered as deny — echoing half a base64 blob into the PTY
/// would time the peer out silently (50ms write budget), which is worse.
pub const OSC52_READ_REPLY_MAX: usize = OSC52_MAX_BYTES;

/// Remote-notification title cap (chars).
pub const NOTIFY_TITLE_MAX: usize = 100;

/// Remote-notification body cap (chars).
pub const NOTIFY_BODY_MAX: usize = 256;

#[cfg(test)]
mod tests {
    use super::*;

    // Sanity: the reply cap must not exceed what the guard would ever
    // deliver (raw payload ≤ 1MiB → decoded ≤ ~750KiB < 1MiB cap), so
    // `OSC52_READ_REPLY_MAX` can never be hit by an in-guard payload.
    const _: () = {
        assert!(OSC52_MAX_BYTES == 1024 * 1024);
        assert!(OSC52_READ_REPLY_MAX <= OSC52_MAX_BYTES);
        assert!(NOTIFY_TITLE_MAX < NOTIFY_BODY_MAX);
    };
}
