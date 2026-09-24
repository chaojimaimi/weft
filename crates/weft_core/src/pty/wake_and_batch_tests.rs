//! T8 (PLAN_v11217 §3.4) tests — `read_batch` stop conditions and the
//! flood-phase byte-integrity regression. Split from `pty/tests.rs` to
//! keep both files under the 800-line commit-gate cap; declared as a
//! child module of `tests` so `super::*` reaches the same imports.

use super::*;

// ── T8 (PLAN_v11217 §3.4): read_batch stop conditions ─────────────
// Closure injection, same testability shape as the write_all_nonblocking
// anchors above: a pure aggregation function driven by a fake reader.

/// Production scratch shape (the 256KB read buffer) shared by the tests.
fn read_scratch() -> Vec<u8> {
    vec![0u8; EVENT_CAP]
}

/// Soft-target stop: four 16KB reads cross `READ_BATCH_TARGET`; the batch
/// flushes on the read that reaches the target, having consumed exactly
/// four reads.
#[test]
fn read_batch_stops_at_soft_target() {
    let mut scratch = read_scratch();
    let mut calls = 0;
    let (batch, stop) = read_batch(
        &mut |dst: &mut [u8]| {
            calls += 1;
            let n = dst.len().min(16 * 1024);
            dst[..n].fill(b'a');
            Ok(n)
        },
        &mut scratch,
    );
    assert_eq!(calls, 4, "64KB soft target must stop after four 16KB reads");
    assert_eq!(batch.len(), READ_BATCH_TARGET);
    assert_eq!(batch, vec![b'a'; READ_BATCH_TARGET]);
    assert!(matches!(stop, BatchStop::SoftTarget), "got {stop:?}");
}

/// WouldBlock stop: data dries up mid-batch and the accumulated bytes flush
/// immediately — no timer, no hold (the interactive-echo path must have the
/// same latency as the unbatched loop). Also pins the empty-first-read
/// case: a WouldBlock with nothing read yields an empty batch, which the
/// read loop treats as a spurious wakeup.
#[test]
fn read_batch_stops_on_wouldblock_and_flushes() {
    let mut scratch = read_scratch();
    let mut calls = 0;
    let (batch, stop) = read_batch(
        &mut |dst: &mut [u8]| {
            calls += 1;
            if calls == 1 {
                dst[..12].fill(b'b');
                Ok(12)
            } else {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
        },
        &mut scratch,
    );
    assert_eq!(calls, 2, "one data read, one drained read");
    assert_eq!(batch, vec![b'b'; 12], "partial batch must flush, not wait");
    assert!(matches!(stop, BatchStop::WouldBlock), "got {stop:?}");

    let mut scratch = read_scratch();
    let (batch, stop) = read_batch(
        &mut |_dst: &mut [u8]| Err(io::Error::from(io::ErrorKind::WouldBlock)),
        &mut scratch,
    );
    assert!(batch.is_empty(), "drained first read must yield no batch");
    assert!(matches!(stop, BatchStop::WouldBlock), "got {stop:?}");
}

/// Hard-cap stop (fourth-round review P1): the first read lands 1KB short of
/// the soft target, then the closure fills EVERY requested byte — the
/// continuation request must be capped to `EVENT_CAP − batch.len()`
/// (255KB, not the full 256KB scratch), landing the batch exactly on the
/// cap. A "request full scratch, check after" implementation would produce
/// a 256KB+1KB batch and go red here.
#[test]
fn read_batch_never_exceeds_event_cap() {
    let mut scratch = read_scratch();
    let mut calls = 0;
    let mut requests: Vec<usize> = Vec::new();
    let (batch, stop) = read_batch(
        &mut |dst: &mut [u8]| {
            calls += 1;
            requests.push(dst.len());
            // First read: 1KB short of the 64KB soft target so the loop must
            // continue; afterwards fill the whole request.
            let n = if calls == 1 {
                READ_BATCH_TARGET - 1024
            } else {
                dst.len()
            };
            dst[..n].fill(b'c');
            Ok(n)
        },
        &mut scratch,
    );
    assert_eq!(calls, 2, "the capped continuation read must end the batch");
    assert_eq!(requests[0], EVENT_CAP, "first read uses the full scratch");
    assert_eq!(
        requests[1],
        EVENT_CAP - (READ_BATCH_TARGET - 1024),
        "continuation request must be capped to the batch's remaining room"
    );
    assert_eq!(batch.len(), EVENT_CAP, "batch lands exactly on the cap");
    assert!(batch.len() <= EVENT_CAP, "hard cap holds");
    assert!(matches!(stop, BatchStop::SoftTarget), "got {stop:?}");
}

/// EOF mid-batch: `Ok(0)` stops the loop and the bytes already read are
/// returned WITH the Eof stop, so the caller can flush them before the
/// exit tail (T8: no byte loss on child exit).
#[test]
fn read_batch_returns_batch_with_mid_batch_eof() {
    let mut scratch = read_scratch();
    let mut calls = 0;
    let (batch, stop) = read_batch(
        &mut |dst: &mut [u8]| {
            calls += 1;
            match calls {
                1 | 2 => {
                    dst[..10 * 1024].fill(b'd');
                    Ok(10 * 1024)
                }
                _ => Ok(0),
            }
        },
        &mut scratch,
    );
    assert_eq!(calls, 3, "two data reads, then EOF");
    assert_eq!(
        batch,
        vec![b'd'; 20 * 1024],
        "bytes read before EOF survive"
    );
    assert!(matches!(stop, BatchStop::Eof), "got {stop:?}");
}

/// A terminal read error mid-batch surfaces the same way as EOF: the batch
/// read so far is returned alongside the error (read_loop flushes it before
/// taking the exit tail).
#[test]
fn read_batch_returns_batch_with_terminal_error() {
    let mut scratch = read_scratch();
    let mut calls = 0;
    let (batch, stop) = read_batch(
        &mut |dst: &mut [u8]| {
            calls += 1;
            if calls == 1 {
                dst[..8].fill(b'e');
                Ok(8)
            } else {
                Err(io::Error::other("boom"))
            }
        },
        &mut scratch,
    );
    assert_eq!(calls, 2);
    assert_eq!(batch, vec![b'e'; 8]);
    match stop {
        BatchStop::Error(e) => assert_eq!(e.to_string(), "boom"),
        other => panic!("expected Error stop, got {other:?}"),
    }
}

// ── T8 (PLAN_v11217 §3.4): flood byte-integrity regression ────────
// Planned in PLAN_v1112 §7.2, first implemented here (fourth-round review
// P1: no pre-existing fixture to reuse). NOT in CI (#[ignore], spawns real
// processes). Run manually:
//   cargo test -p weft_core --lib pty::tests::flood -- --ignored --nocapture

/// A child writes a known 8 MiB stream through a REAL PTY (raw mode, so no
/// ONLCR mangling); the receiving side must reassemble it exactly: hash +
/// total-byte-count double assertion, plus the T8 structural observations —
/// no event may exceed `EVENT_CAP` and the channel depth must stay within
/// `PTY_CHANNEL_CAP` (`queued_event_count`, the pty.rs len accessor).
/// Aggregation granularity depends on kernel scheduling: deliberately NOT
/// asserted (flake discipline).
#[tokio::test]
#[ignore]
async fn flood_read_batches_preserve_byte_stream() {
    const TOTAL_BYTES: usize = 8 * 1024 * 1024;
    // `yes <arg>` emits "<arg>\n" forever; head -c truncates to the exact
    // known length. Raw mode keeps the PTY from rewriting \n into \r\n;
    // yes's stderr is silenced because its dying SIGPIPE diagnostic
    // ("yes: stdout: Broken pipe\n", 25 bytes) otherwise rides the same
    // PTY into the stream.
    const PATTERN: &[u8] = b"abcdefgh\n";
    let script = format!(
        "stty raw -echo; /usr/bin/yes abcdefgh 2>/dev/null | /usr/bin/head -c {TOTAL_BYTES}"
    );
    let mut pty = Pty::spawn_with_args(
        "/bin/sh",
        &["-c", script.as_str()],
        (24, 80),
        &[],
        None,
        new_flag(),
        || {},
    )
    .expect("failed to spawn flood fixture");

    // FNV-1a 64-bit — dependency-free, deterministic.
    let fnv_init: u64 = 0xcbf2_9ce4_8422_2325;
    let fnv_prime: u64 = 0x0000_0100_0000_01b3;
    let mut received_hash = fnv_init;
    let mut total = 0usize;
    let mut max_event = 0usize;
    let mut max_queue_depth = 0usize;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "flood fixture did not finish in time"
        );
        let event = tokio::time::timeout(Duration::from_secs(10), pty.recv())
            .await
            .expect("recv stalled for 10s");
        match event {
            Some(PtyEvent::Output(data)) => {
                max_event = max_event.max(data.len());
                max_queue_depth = max_queue_depth.max(pty.queued_event_count());
                for &byte in &data {
                    received_hash ^= byte as u64;
                    received_hash = received_hash.wrapping_mul(fnv_prime);
                }
                total += data.len();
            }
            Some(PtyEvent::Exit(result)) => {
                assert_eq!(result, Ok(0), "flood child must exit cleanly");
                break;
            }
            None => panic!("PTY channel closed before Exit"),
        }
    }

    // Hash the known stream with the same function (no 8MB buffer needed).
    let mut expected_hash = fnv_init;
    let mut remaining = TOTAL_BYTES;
    while remaining > 0 {
        let take = remaining.min(PATTERN.len());
        for &byte in &PATTERN[..take] {
            expected_hash ^= byte as u64;
            expected_hash = expected_hash.wrapping_mul(fnv_prime);
        }
        remaining -= take;
    }

    assert_eq!(total, TOTAL_BYTES, "every byte must arrive exactly once");
    assert_eq!(
        received_hash, expected_hash,
        "received stream must match the known stream byte for byte"
    );
    assert!(
        max_event <= EVENT_CAP,
        "no event may exceed the hard batch cap: {max_event} > {EVENT_CAP}"
    );
    assert!(
        max_queue_depth <= PTY_CHANNEL_CAP,
        "channel peak depth must stay within the bounded cap"
    );
}

// ── T8: wake-throttle decision table (moved from tests.rs) ───────

// ── T8 (PLAN_v11217 §3.4): wake-throttle decision table ──────────
// (Rewritten from v1.11.2 X2's `..._throttles_floods_to_sixty_hz`, which
// pinned the old 16 ms interval; T8 tightened the backlog interval to 2 ms.)

#[test]
fn pty_wake_due_throttles_backlog_wakes_to_two_ms() {
    // Exit always wakes — the throttle never applies.
    assert!(pty_wake_due(true, false, 1_000, 1_000));
    // A caught-up consumer (empty queue) always wakes immediately.
    assert!(pty_wake_due(false, true, 1_000, 1_000));
    // Backlog + inside the 2 ms window: no wake.
    assert!(!pty_wake_due(false, false, 1_000, 1_001));
    // Backlog + at/after the 2 ms boundary: wake.
    assert!(pty_wake_due(false, false, 1_000, 1_002));
    assert!(pty_wake_due(false, false, 1_000, 2_000));
}

#[test]
fn pty_wake_due_wakes_a_caught_up_consumer_immediately() {
    // An empty queue means the consumer is idle — fresh output must be
    // pumped without waiting out the backlog window.
    assert!(pty_wake_due(false, true, 1_000, 1_001));
}

#[test]
fn pty_wake_due_exit_bypasses_the_throttle() {
    assert!(pty_wake_due(true, false, 1_000, 1_001));
}
