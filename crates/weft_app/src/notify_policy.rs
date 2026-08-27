//! Pure decision logic for the v1.11.5 notification system (PLAN_v1115 §M5).
//!
//! `block_complete_should_notify` decides whether a completed command block
//! earns a system notification; `notification_text` shapes the payload;
//! `badge_text` maps the OSC 9;4 Dock state to a badge string; `RateLimiter`
//! and `DenyCooldown` throttle notify events / OSC 52 read-denies. All are
//! pure — injectable clocks, no I/O, no App/AppState dependency — so every
//! branch is unit-testable (test-first requirement).

use std::time::{Duration, Instant, SystemTime};

use weft_core::vt::DockProgress;
use weft_core::vt::NOTIFY_TITLE_MAX;

/// Minimum gap between two notifications (PLAN_v1115 D-j). Anti-flood: a
/// script that finishes many commands back-to-back only ever posts one
/// notification per window.
pub const NOTIFY_MIN_GAP: Duration = Duration::from_secs(1);

/// OSC 52 read-deny cooldown (PLAN_v1115 D-c). After a deny, further read
/// requests within this window are treated as deny without prompting —
/// otherwise a hostile server could re-open the modal dialog every cycle.
pub const OSC52_DENY_COOLDOWN: Duration = Duration::from_secs(30);

/// User-facing notification policy (config `[notifications]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotifyPolicy {
    /// Master switch (`notifications.enabled`). False → no notifications at
    /// all, zero surprises (安全默认).
    pub enabled: bool,
    /// Minimum command runtime before a completion notifies (`threshold_secs`).
    pub threshold: Duration,
    /// Play the system sound alongside the banner (`notifications.sound`).
    pub sound: bool,
}

/// Notify throttle: at most one notification per `min_gap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimiter {
    last: Option<Instant>,
    min_gap: Duration,
}

impl RateLimiter {
    pub fn new(min_gap: Duration) -> Self {
        Self {
            last: None,
            min_gap,
        }
    }

    /// True when the gap since the last grant is `>= min_gap`; grants
    /// (records `now`) only on success, so a denied caller does not push
    /// its own denial window forward.
    pub fn try_acquire(&mut self, now: Instant) -> bool {
        let admitted = match self.last {
            None => true,
            Some(last) => now.duration_since(last) >= self.min_gap,
        };
        if admitted {
            self.last = Some(now);
        }
        admitted
    }
}

/// OSC 52 read-deny cooldown: after `mark_deny`, `in_cooldown` stays true
/// for `cooldown` (30 s; plan D-c prevents modal-bombing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DenyCooldown {
    last_deny: Option<Instant>,
    cooldown: Duration,
}

impl DenyCooldown {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            last_deny: None,
            cooldown,
        }
    }

    pub fn mark_deny(&mut self, now: Instant) {
        self.last_deny = Some(now);
    }

    pub fn in_cooldown(&self, now: Instant) -> bool {
        match self.last_deny {
            None => false,
            Some(t) => now.duration_since(t) < self.cooldown,
        }
    }
}

/// Decide whether a just-finished command block should post a notification.
///
/// Gates, in order: master switch → focus (window must be OUT of focus) →
/// elapsed `>= threshold` (exactly-equal hits) → rate limiter. The elapsed
/// span is `finished_at - started_at`; if the system clock was adjusted
/// backwards mid-command (`duration_since` errs — SystemTime is wall clock),
/// fall back to `now_sys - started_at` as the best available estimate.
pub fn block_complete_should_notify(
    started_at: SystemTime,
    finished_at: SystemTime,
    now_sys: SystemTime,
    window_focused: bool,
    policy: &NotifyPolicy,
    limiter: &mut RateLimiter,
) -> bool {
    if !policy.enabled {
        return false;
    }
    if window_focused {
        return false;
    }
    let elapsed = finished_at
        .duration_since(started_at)
        .unwrap_or_else(|_| now_sys.duration_since(started_at).unwrap_or_default());
    if elapsed < policy.threshold {
        return false;
    }
    limiter.try_acquire(Instant::now())
}

/// Shape the notification payload for a finished command block.
///
/// Title: the command, truncated at the share NOTIFY_TITLE_MAX char cap.
/// Body: one line — `Finished in <s>` / `Failed after <s>` (`failed` = the
/// block's exit code != 0). The body template is bounded well under
/// NOTIFY_BODY_MAX (256) for any realistic elapsed rendering.
pub fn notification_text(command: &str, elapsed: Duration, failed: bool) -> (String, String) {
    let title: String = command.chars().take(NOTIFY_TITLE_MAX).collect();
    let secs = elapsed.as_secs_f64();
    let body = if failed {
        format!("Failed after {secs:.1}s")
    } else {
        format!("Finished in {secs:.1}s")
    };
    (title, body)
}

/// Map an OSC 9;4 Dock state to the badge text (v1.11.5 D-e): `42%` /
/// `…` (indeterminate) / `!` (failed) / `None` (clear).
pub fn badge_text(progress: DockProgress) -> Option<String> {
    match progress {
        DockProgress::Clear => None,
        DockProgress::Indeterminate => Some("…".to_string()),
        DockProgress::Failed => Some("!".to_string()),
        DockProgress::Percent(p) => Some(format!("{p}%")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::vt::DockProgress;

    fn policy(enabled: bool, threshold_secs: u64) -> NotifyPolicy {
        NotifyPolicy {
            enabled,
            threshold: Duration::from_secs(threshold_secs),
            sound: false,
        }
    }

    fn sys(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    // ── block_complete_should_notify ──────────────────────────────────

    #[test]
    fn threshold_boundary_29_9_vs_30_0() {
        // exactly 30.0s hits ("恰等算命中"); 29.9s does not.
        for (elapsed, expect) in [
            (Duration::from_millis(29_900), false),
            (Duration::from_millis(30_000), true),
            (Duration::from_secs(31), true),
        ] {
            let mut limiter = RateLimiter::new(NOTIFY_MIN_GAP);
            let start = sys(1_000_000);
            let ok = block_complete_should_notify(
                start,
                start + elapsed,
                sys(1_000_000),
                false,
                &policy(true, 30),
                &mut limiter,
            );
            assert_eq!(ok, expect, "elapsed {elapsed:?}");
        }
    }

    #[test]
    fn focus_gates_notifications() {
        // focused → never notify, regardless of elapsed
        let mut limiter = RateLimiter::new(NOTIFY_MIN_GAP);
        let start = sys(100);
        let ok = block_complete_should_notify(
            start,
            start + Duration::from_secs(120),
            sys(100),
            true,
            &policy(true, 30),
            &mut limiter,
        );
        assert!(!ok, "focused window must not notify");
        // unfocused → notifies
        let ok = block_complete_should_notify(
            start,
            start + Duration::from_secs(120),
            sys(100),
            false,
            &policy(true, 30),
            &mut limiter,
        );
        assert!(ok, "unfocused + long command → notify");
    }

    #[test]
    fn master_switch_always_gates() {
        let mut limiter = RateLimiter::new(NOTIFY_MIN_GAP);
        let start = sys(100);
        let ok = block_complete_should_notify(
            start,
            start + Duration::from_secs(3600),
            sys(100),
            false,
            &policy(false, 30),
            &mut limiter,
        );
        assert!(!ok, "enabled=false must silence everything");
    }

    #[test]
    fn rate_limiter_gap_respects_min_gap() {
        let mut limiter = RateLimiter::new(NOTIFY_MIN_GAP);
        let t0 = Instant::now();
        assert!(limiter.try_acquire(t0), "first grant always passes");
        assert!(
            !limiter.try_acquire(t0 + Duration::from_millis(999)),
            "under gap"
        );
        assert!(
            limiter.try_acquire(t0 + Duration::from_secs(1)),
            "exactly at gap"
        );
        assert!(
            limiter.try_acquire(t0 + Duration::from_secs(10)),
            "after gap"
        );
        // a denied / past call must not push the last-grant time forward:
        // t0+2s < the t0+10s grant → still denied
        assert!(
            !limiter.try_acquire(t0 + Duration::from_secs(2)),
            "still gated by the real grant"
        );
    }

    #[test]
    fn limiter_blocks_second_block_within_gap() {
        // two blocks finishing <1s apart → only the first notifies
        let mut limiter = RateLimiter::new(NOTIFY_MIN_GAP);
        let start = sys(1000);
        let first = block_complete_should_notify(
            start,
            start + Duration::from_secs(60),
            sys(1000),
            false,
            &policy(true, 30),
            &mut limiter,
        );
        assert!(first);
        let second = block_complete_should_notify(
            sys(1001),
            sys(1001) + Duration::from_secs(60),
            sys(1001),
            false,
            &policy(true, 30),
            &mut limiter,
        );
        assert!(!second, "second completion inside the 1s gap is dropped");
    }

    #[test]
    fn backwards_clock_falls_back_to_now_sys() {
        // finished_at before started_at (wall clock adjusted): no panic, and
        // the now_sys fallback still measures the true elapsed span.
        let mut limiter = RateLimiter::new(NOTIFY_MIN_GAP);
        let start = sys(10_000);
        let ok = block_complete_should_notify(
            start,
            sys(9_000), // backward! duration_since errs
            sys(10_000) + Duration::from_secs(45),
            false,
            &policy(true, 30),
            &mut limiter,
        );
        assert!(ok, "fallback elapsed (45s) ≥ threshold → notify");
    }

    // ── notification_text ─────────────────────────────────────────────

    #[test]
    fn text_truncates_command_to_title_cap() {
        let long = "x".repeat(NOTIFY_TITLE_MAX + 40);
        let (title, _) = notification_text(&long, Duration::from_secs(5), false);
        assert_eq!(title.chars().count(), NOTIFY_TITLE_MAX);
        // CJK must not be split mid-sequence
        let (title, _) =
            notification_text(&"汉".repeat(NOTIFY_TITLE_MAX + 3), Duration::ZERO, false);
        assert_eq!(title.chars().count(), NOTIFY_TITLE_MAX);
        assert!(title.is_char_boundary(title.len()));
    }

    #[test]
    fn text_short_command_kept_whole() {
        let (title, body) = notification_text("cargo build", Duration::from_secs(5), false);
        assert_eq!(title, "cargo build");
        assert_eq!(body, "Finished in 5.0s");
        assert!(body.len() <= 256);
    }

    #[test]
    fn text_failed_variant_reports_error() {
        let (_, body) = notification_text("make", Duration::from_millis(2500), true);
        assert_eq!(body, "Failed after 2.5s");
        assert!(body.len() <= 256);
    }

    // ── badge_text (iTerm2 semantics, v1.11.5 D-e) ─────────────────────

    #[test]
    fn badge_text_maps_all_four_progress_states() {
        assert_eq!(badge_text(DockProgress::Clear), None);
        assert_eq!(
            badge_text(DockProgress::Indeterminate).as_deref(),
            Some("…")
        );
        assert_eq!(badge_text(DockProgress::Failed).as_deref(), Some("!"));
        assert_eq!(
            badge_text(DockProgress::Percent(47)).as_deref(),
            Some("47%")
        );
        assert_eq!(badge_text(DockProgress::Percent(0)).as_deref(), Some("0%"));
        assert_eq!(
            badge_text(DockProgress::Percent(100)).as_deref(),
            Some("100%")
        );
    }

    // ── DenyCooldown (OSC 52 read-deny 30s) ────────────────────────────

    #[test]
    fn deny_cooldown_boundary_29_30_31() {
        let mut cd = DenyCooldown::new(OSC52_DENY_COOLDOWN);
        let t0 = Instant::now();
        assert!(!cd.in_cooldown(t0), "fresh: no cooldown");
        cd.mark_deny(t0);
        assert!(cd.in_cooldown(t0), "denied: immediately in cooldown");
        assert!(cd.in_cooldown(t0 + Duration::from_secs(29)), "29s in");
        assert!(
            !cd.in_cooldown(t0 + Duration::from_secs(30)),
            "30s → window expired (strictly-less comparison)"
        );
        assert!(!cd.in_cooldown(t0 + Duration::from_secs(31)));
        // re-marking after expiry re-arms the window
        cd.mark_deny(t0 + Duration::from_secs(31));
        assert!(cd.in_cooldown(t0 + Duration::from_secs(40)));
    }
}
