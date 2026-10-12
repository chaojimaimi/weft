//! Startup runtime diagnostics + log-hygiene family, moved verbatim out
//! of `app_runtime.rs` (v1.13.9 backlog clear zero-behavior file-budget
//! split). Free-function family; the log fns that `log_rotate_tests.rs`
//! exercises are `pub(super)` so the sibling test module keeps access.

use std::time::Duration;

pub(crate) fn install_runtime_diagnostics() {
    // v1.10.4: write to ~/Library/Logs/Weft/weft.log (macOS standard location)
    // and APPEND across launches so a crash/relaunch doesn't wipe the prior
    // session's diagnostics. Previously this truncated /tmp/weft.log on every
    // start, which erased the exact现场 dogfood bugs need.
    // v1.4.0: honor RUST_LOG, default to `info`.
    let log_path = runtime_log_path(std::env::var_os("HOME").as_deref());
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // v1.12.24: precedes the open — renaming an open handle would write into the rotated file.
    rotate_log_if_huge(&log_path);
    // T15e (PLAN_v11217 §3.10): drop rotated generations past 7 days —
    // v1.12.24's rotation never cleaned them (field residue: 167MB .log.1).
    remove_stale_rotated_log(&log_path);
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(_) => {
            // v1.10.6: local-time timestamps even in the stderr fallback.
            tracing_subscriber::fmt()
                .with_timer(tracing_subscriber::fmt::time::LocalTime::rfc_3339())
                .init();
            let default_hook = std::panic::take_hook();
            setup_panic_hook(default_hook);
            return;
        }
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // v1.10.6: timestamp in LOCAL time (CST) instead of the default UTC.
    // The default SystemTime formatter emits RFC3339 UTC, which read as
    // "8 hours in the past" for Asia/Shanghai users and made logs confusing.
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::sync::Arc::new(file))
        .with_ansi(false)
        .with_timer(tracing_subscriber::fmt::time::LocalTime::rfc_3339())
        .with_env_filter(filter)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
    // v1.10.4: session separator so multiple appended launches stay readable.
    tracing::info!(
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        hash = option_env!("WEFT_BUILD_HASH").unwrap_or("dev"),
        "=== Weft session start ==="
    );
    setup_panic_hook(std::panic::take_hook());
}

// `PanicInfo` was renamed to `PanicHookInfo` in Rust 1.81, but our MSRV is
// 1.75 (`rust-version` in Cargo.toml). Stay on the old name until the MSRV
// bump and silence the deprecation at the use sites below.
#[allow(deprecated)]
fn setup_panic_hook(default_hook: Box<dyn Fn(&std::panic::PanicInfo<'_>) + Send + Sync + 'static>) {
    std::panic::set_hook(Box::new(move |info: &std::panic::PanicInfo<'_>| {
        let path = panic_log_path(std::env::var_os("HOME").as_deref());
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // v1.10.4: append (not truncate) so multiple panics across launches
        // don't overwrite each other's backtrace — essential for dogfood.
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write;
            let _ = writeln!(
                file,
                "\n=== Weft panic pid={} at {:?} ===\n{}\n{}",
                std::process::id(),
                std::time::SystemTime::now(),
                info,
                std::backtrace::Backtrace::force_capture()
            );
        }
        default_hook(info);
    }));
}

pub(crate) fn terminal_capability_env() -> Vec<(String, String)> {
    vec![
        ("TERM".into(), "xterm-256color".into()),
        ("COLORTERM".into(), "truecolor".into()),
        // v1.10: BSD `ls` only colorizes with `-G` or CLICOLOR=1. Setting it
        // here makes plain `ls` color directories (SGR 34 → theme palette[4])
        // while regular files keep the default foreground, so directory tint
        // follows the active theme automatically. LSCOLORS is deliberately
        // NOT set: the macOS default already matches, keeping us minimally
        // invasive — user dotfiles stay free to override either variable.
        ("CLICOLOR".into(), "1".into()),
        ("TERM_PROGRAM".into(), "Weft".into()),
        (
            "TERM_PROGRAM_VERSION".into(),
            env!("CARGO_PKG_VERSION").into(),
        ),
    ]
}

pub(super) fn panic_log_path(home: Option<&std::ffi::OsStr>) -> std::path::PathBuf {
    home.map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Library/Logs/Weft/panic.log")
}

/// v1.10.4: runtime tracing log path under the macOS-standard location.
/// Falls back to `/tmp/weft.log` only when `$HOME` is unavailable.
fn runtime_log_path(home: Option<&std::ffi::OsStr>) -> std::path::PathBuf {
    match home {
        Some(h) => std::path::PathBuf::from(h).join("Library/Logs/Weft/weft.log"),
        None => std::path::PathBuf::from("/tmp/weft.log"),
    }
}

/// v1.12.24: rotate the append-only runtime log at startup — it grew to
/// 175 MB in the field with no cap. Keeps one generation (.log.1).
pub(super) fn rotate_log_if_huge(log_path: &std::path::Path) {
    const WEFT_LOG_ROTATE_BYTES: u64 = 32 * 1024 * 1024;
    if let Ok(meta) = std::fs::metadata(log_path) {
        if meta.len() > WEFT_LOG_ROTATE_BYTES {
            let mut rotated = log_path.to_path_buf();
            rotated.set_extension("log.1");
            let _ = std::fs::remove_file(&rotated);
            let _ = std::fs::rename(log_path, &rotated)
                .map_err(|e| tracing::debug!(error = %e, "log rotate: rename failed"));
        }
    }
}

/// T15e (PLAN_v11217 §3.10): v1.12.24's rotation keeps ONE generation
/// (`weft.log.1`) but nothing ever removed it — the field accumulated a
/// 167MB residue. At startup, delete `weft.log.1` when its mtime is older
/// than 7 days: rename preserves mtime, so the stamp is the old log's
/// LAST-WRITE time, not the rotation moment. The window keeps the newest
/// generation; the worst in-window coexistence (~200MB) is an accepted
/// hygiene tradeoff.
pub(super) fn remove_stale_rotated_log(log_path: &std::path::Path) {
    let mut rotated = log_path.to_path_buf();
    rotated.set_extension("log.1");
    let Ok(meta) = std::fs::metadata(&rotated) else {
        return; // no generation on disk — nothing to clean
    };
    let Ok(mtime) = meta.modified() else {
        return; // unreadable stamp — keep the file
    };
    if rotated_log_is_stale(mtime, std::time::SystemTime::now()) {
        let _ = std::fs::remove_file(&rotated)
            .map_err(|e| tracing::debug!(error = %e, "stale log.1 removal failed"));
    }
}

/// T15e age predicate, factored for the truth-table test (the temp-dir test
/// covers the real path by backdating mtime via `File::set_times`).
pub(super) fn rotated_log_is_stale(
    mtime: std::time::SystemTime,
    now: std::time::SystemTime,
) -> bool {
    const STALE_ROTATED_LOG_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    now.duration_since(mtime)
        .is_ok_and(|age| age > STALE_ROTATED_LOG_AGE)
}
