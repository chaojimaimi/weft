//! The PTY write half (v1.13.6 T10 P2, PLAN_v1136 §1 D4/D5).
//!
//! Split out of `pty.rs` with the standard `pty.rs` + `pty/` layout so the
//! production files stay under the architecture gate. The split exists
//! because T10 P2 creates a SECOND PTY writer: the per-pane parse worker
//! (writing VT query replies — D5) alongside the main thread (user input,
//! paste, Ctrl+C interrupts). [`PtyWriter`] is that write half:
//!
//! - it owns its own dup of the PTY master fd, so the main-thread `Pty` and
//!   the worker each hold an independent handle (either can be dropped
//!   first);
//! - it carries THE write mutex (D4): one lock, internal to the writer,
//!   shared by every clone — `write_sync_reported` and `send_interrupt`
//!   both take it, so the two writers can never interleave a large write.
//!   There is deliberately no second lock anywhere in the write path
//!   (D9 rule: "禁止发明第二套锁序");
//! - `std::sync::Mutex` is used instead of parking_lot (weft_core does not
//!   depend on it). A poisoned lock can only mean a writer panicked
//!   mid-`write`; falling through to the guarded data (`into_inner`) is the
//!   correct recovery for a tty byte pipe, so poisoning is deliberately
//!   ignored.
//!
//! The bounded-EAGAIN retry machinery (`write_all_nonblocking` /
//! [`WriteOutcome`] / `map_write_outcome`) moved here verbatim from
//! `pty.rs` — behavior unchanged (FIX_TERMINAL_CAPABILITY_HARDENING
//! contracts and their tests are untouched, R5).

use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

use super::PtyError;
use super::Result;

/// Total time a non-blocking synchronous write may spend poll-waiting and
/// retrying after EAGAIN before giving up (FIX_TERMINAL_CAPABILITY_HARDENING).
const WRITE_RETRY_BUDGET: Duration = Duration::from_millis(50);

/// The PTY write half — one fd + one write mutex, shared by the main thread
/// and the parse worker via `Arc` clones (see the module docs).
pub struct PtyWriter {
    /// Dup of the PTY master fd, held independently of `Pty::master` so the
    /// two halves can be dropped in either order.
    master: OwnedFd,
    /// D4: the ONLY write lock. `std::sync::Mutex` poisoning is deliberately
    /// fallen through (see the module docs).
    write_lock: Mutex<()>,
}

impl PtyWriter {
    /// Wrap a (dup'd) PTY master fd as the shared write half. The caller
    /// transfers ownership of a fd DUPLICATED from the Pty's master (never
    /// the master itself — `Pty` keeps its own for ioctl/tcflush/drop).
    /// `pub` for the parse-worker test harness; production goes through
    /// `Pty::spawn_with_args`.
    pub fn new(master: OwnedFd) -> Self {
        Self {
            master,
            write_lock: Mutex::new(()),
        }
    }

    /// Write bytes synchronously (thin `Ok(())` wrapper over
    /// [`Self::write_sync_reported`] — every legacy caller compiles
    /// unchanged; see `Pty::write_sync` for the history).
    pub fn write_sync(&self, data: &[u8]) -> Result<()> {
        self.write_sync_reported(data).map(|_| ())
    }

    /// Honest synchronous write — returns the number of bytes that actually
    /// left when the retry budget expires (`Ok(written)`) instead of masking
    /// a partial write as `Ok(())`. Moved verbatim from `Pty` (v1.11.15 FIX
    /// E); the only change is taking the D4 write lock around the fd write.
    pub fn write_sync_reported(&self, data: &[u8]) -> Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        // D4: serialize against the other writer (worker replies ∥ main
        // thread input). The critical section is one bounded fd write; the
        // lock is never held across anything that can block indefinitely.
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fd = self.master.as_raw_fd();
        let master = &self.master;
        let mut write_one = |buf: &[u8]| nix::unistd::write(master, buf).map_err(io::Error::from);
        let outcome = write_all_nonblocking(&mut write_one, fd, data, WRITE_RETRY_BUDGET);
        map_write_outcome(outcome, data.len())
    }

    /// Deliver one Ctrl+C interrupt (ETX byte) to the foreground process.
    /// Moved verbatim from `Pty::send_interrupt`; the only change is taking
    /// the D4 write lock so the ETX cannot split a concurrent large write.
    pub fn send_interrupt(&self) -> bool {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match nix::unistd::write(&self.master, &[0x03]) {
            Ok(1) => {
                tracing::debug!(delivery = "pty-etx", "Ctrl+C delivered once");
                true
            }
            Ok(_) => {
                tracing::warn!("short Ctrl+C PTY write; interrupt not delivered");
                false
            }
            Err(nix::errno::Errno::EAGAIN) => {
                tracing::warn!("Ctrl+C PTY write would block; interrupt not delivered");
                false
            }
            Err(error) => {
                tracing::warn!(%error, "Ctrl+C PTY write failed; interrupt not delivered");
                false
            }
        }
    }

    /// Async write — the test-injection seam (the production input path goes
    /// through [`Self::write_sync_reported`]; see `Pty::write`'s original doc
    /// for the partial-write loop contract, FIX E v1.11.15, preserved
    /// verbatim). Moved from `Pty` together with the rest of the write
    /// surface; it takes the D4 write lock for the whole loop so a
    /// concurrent writer can never interleave.
    //
    // The D4 lock intentionally spans the await points: the whole
    // partial-write loop must be atomic against the other writer. The
    // production write paths are fully synchronous, so no production
    // consumer ever blocks on this guard across an await.
    #[allow(clippy::await_holding_lock)]
    pub async fn write(&self, data: &[u8]) -> Result<()> {
        use tokio::io::unix::AsyncFd;

        if data.is_empty() {
            return Ok(());
        }
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // We need a mutable reference for AsyncWrite, so create a temporary
        // handle. SAFETY: dup the fd so we can get an async writer.
        let duped = unsafe {
            let new_fd = nix::libc::dup(self.master.as_raw_fd());
            if new_fd < 0 {
                return Err(PtyError::Write(io::Error::last_os_error()));
            }
            OwnedFd::from_raw_fd(new_fd)
        };
        let writer = AsyncFd::new(duped).expect("failed to create async fd for write");
        // Use the AsyncFd to perform a non-blocking write.
        let mut written = 0usize;
        loop {
            let mut guard = writer.writable().await.map_err(PtyError::Write)?;
            match guard
                .try_io(|fd| nix::unistd::write(fd, &data[written..]).map_err(io::Error::from))
            {
                Ok(Ok(n)) => {
                    written += n;
                    if written >= data.len() {
                        return Ok(());
                    }
                    if n == 0 {
                        // Zero bytes with data remaining can never make
                        // progress — mirror write_all_nonblocking's contract.
                        return Err(PtyError::Write(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "pty write returned 0 with data remaining",
                        )));
                    }
                }
                Ok(Err(e)) => return Err(PtyError::Write(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

/// Outcome of a bounded non-blocking write loop, observable by callers and
/// unit tests (FIX_TERMINAL_CAPABILITY_HARDENING).
#[derive(Debug)]
#[must_use]
pub(crate) enum WriteOutcome {
    /// All bytes reached the fd.
    WrittenAll,
    /// Budget exhausted before everything was written; `written` bytes made
    /// it out, the rest were dropped (caller should warn with the count).
    TimedOut { written: usize },
    /// A non-EAGAIN write/poll error.
    Error(io::Error),
}

/// Bounded EAGAIN/EWOULDBLOCK retry for a non-blocking writer.
///
/// Moved verbatim from `pty.rs` — see that fn's original doc for the full
/// contract (poll-wait on POLLOUT within `budget`, EINTR retried like
/// EWOULDBLOCK, partial writes advance, `Ok(0)` fails fast, fd and writer
/// injected separately for unit tests).
pub(crate) fn write_all_nonblocking<W>(
    mut write_fn: W,
    fd: RawFd,
    data: &[u8],
    budget: Duration,
) -> WriteOutcome
where
    W: FnMut(&[u8]) -> io::Result<usize>,
{
    let deadline = Instant::now() + budget;
    let mut written = 0;
    let mut rest = data;
    loop {
        match write_fn(rest) {
            Ok(n) => {
                written += n;
                rest = &rest[n..];
                if rest.is_empty() {
                    return WriteOutcome::WrittenAll;
                }
                if n == 0 {
                    // std `write_all` contract: zero bytes with data
                    // remaining can never make progress — fail fast
                    // instead of spinning the budget away.
                    return WriteOutcome::Error(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write returned 0 with data remaining",
                    ));
                }
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::Interrupted =>
            {
                // EWOULDBLOCK (buffer full → poll for writability below) and
                // EINTR (signal interrupted the syscall → retry, mirroring
                // the poll loop's own EINTR continue) are both retried.
            }
            Err(e) => return WriteOutcome::Error(e),
        }
        // Wait for writability with whatever budget remains; never wait
        // past `deadline`.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return WriteOutcome::TimedOut { written };
        }
        // SAFETY: `fd` outlives this call (callers pass their own live fd,
        // held for the duration), so borrowing it for the poll is sound.
        let mut fds = [PollFd::new(
            unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) },
            PollFlags::POLLOUT,
        )];
        let timeout = PollTimeout::try_from(remaining).unwrap_or(PollTimeout::ZERO);
        match poll(&mut fds, timeout) {
            Ok(0) => return WriteOutcome::TimedOut { written },
            // Ready (or POLLERR/POLLNVAL) — retry; the write itself
            // surfaces the real error.
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return WriteOutcome::Error(io::Error::from(e)),
        }
    }
}

/// v1.11.15 (FIX E): the pure `WriteOutcome → honest report` mapping shared
/// by `write_sync_reported`. Moved verbatim from `pty.rs`.
pub(crate) fn map_write_outcome(outcome: WriteOutcome, total: usize) -> Result<usize> {
    match outcome {
        WriteOutcome::WrittenAll => Ok(total),
        WriteOutcome::TimedOut { written } => {
            tracing::warn!(
                dropped = total - written,
                "pty write budget exhausted — dropping remaining bytes"
            );
            Ok(written)
        }
        WriteOutcome::Error(e) => Err(PtyError::Write(e)),
    }
}
