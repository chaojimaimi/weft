//! v1.13.5 (PLAN_v11217 §3.11 T16c): PATH-bin 扫描异步化 + 结果缓存。
//!
//! 之前：main.rs 启动时同步 `scan_path_bins()`（spawn 登录 shell -lic，实测
//! 172.8ms，1500ms deadline 护栏在 `resolve_login_path` 内），阻塞整个冷启动。
//!
//! 现在（语义变化声明）：
//! - **缓存命中** → 同步零成本读 `~/.cache/weft/path-bins.json`（指纹校验
//!   通过；仍会跑一次 `resolve_user_shell` 的 dscl 探测，远轻于登录 shell）。
//! - **缓存未命中** → 主线程先取进程 env PATH 的快路径结果，同时后台线程
//!   复刻 `scan_path_bins` 三源并集（登录 shell + path_helper + env PATH），
//!   完成后写缓存并经 `AppEvent::PathBinsResolved` 回填
//!   `ConfigState::path_bins`。
//! - **语义代价**：冷启动后约 172ms 内 PATH 补全可能不含 homebrew 等
//!   （异步回填前）；首帧不受影响——相比现状（同步阻塞所有启动）严格更优。
//! - **false-hit 代价**：指纹一致但 rc 语义已变（如 mtime 未动的内容改写）
//!   时 PATH 陈旧至下次启动——低危可接受，故后台线程直接沿用启动时算出的
//!   指纹写缓存（指纹每次启动重算重校验，最坏代价是下次启动重扫一次，
//!   省掉第二次 dscl）。
//!
//! 指纹（评审 P2，PATH 三源全覆盖）为以下条目的并集（排序后逐项比较）：
//! 1. `resolve_user_shell` 结果（回退 `/bin/zsh` 为常量，等价覆盖）；
//! 2. 按 shell 类型分派的 rc 文件集的 (path, mtime)：zsh 四件套
//!    `~/.zshenv ~/.zprofile ~/.zshrc ~/.zlogin`、bash 两件
//!    `~/.bash_profile ~/.bashrc`、fish `~/.config/fish/config.fish`；
//!    其他 shell 按 zsh 集（macOS 默认登录 shell，`resolve_login_path`
//!    的回退分支同为 `/bin/zsh`）。rc 文件缺失 = 跳过该条目；
//! 3. `/etc/paths` 与 `/etc/paths.d/*` 的 (path, mtime)（Homebrew/Docker
//!    类安装器写这里）。
//!
//! 缓存写入方是后台扫描线程（主线程零 IO）；两个实例并发写最坏产生一次
//! 撕裂 JSON → 下次启动解析失败视为 miss 重扫，无其他后果。

use crate::macos_system;
use crate::AppEvent;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use winit::event_loop::EventLoopProxy;

/// Cache file name, resolved under `weft_cache_dir()` at call time.
const CACHE_FILE: &str = "path-bins.json";

/// Fingerprint field separator (US \x1f — never appears in real paths).
const SEP: char = '\u{1f}';

/// Persisted cache record (`path-bins.json`).
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct PathBinsCache {
    fingerprint: Vec<String>,
    bins: Vec<String>,
}

/// The effective login shell: `resolve_user_shell` probe with the same
/// `/bin/zsh` fallback `resolve_login_path` uses — the rc dispatch below
/// must key off the shell whose rc files the scan actually reads.
fn effective_shell() -> String {
    macos_system::resolve_user_shell()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/bin/zsh".to_string())
}

/// Rc files whose sourcing can change the login-shell PATH, dispatched by
/// shell basename. Unknown shells key off the zsh set (macOS default login
/// shell; the `resolve_login_path` fallback is `/bin/zsh` too).
fn rc_files_for_shell(shell: &str, home: &Path) -> Vec<PathBuf> {
    let base = Path::new(shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("zsh");
    let rels: &[&str] = match base {
        "bash" => &[".bash_profile", ".bashrc"],
        "fish" => &[".config/fish/config.fish"],
        _ => &[".zshenv", ".zprofile", ".zshrc", ".zlogin"],
    };
    rels.iter().map(|rel| home.join(rel)).collect()
}

/// `(path, mtime-nanos)` for an existing file; `None` for missing/unreadable
/// — a missing rc file or paths.d entry simply drops out of the fingerprint
/// (its appearance later flips the fingerprint by itself).
fn mtime_entry(path: &Path) -> Option<(PathBuf, String)> {
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    let nanos = mtime
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((path.to_path_buf(), nanos.to_string()))
}

/// Fingerprint entries covering all three PATH sources (see module doc).
/// Sorted for order-stable comparison (`read_dir` order is not). `etc_paths`
/// / `etc_paths_d` are parameters for testability; production passes
/// `/etc/paths` and `/etc/paths.d`.
fn compute_fingerprint(
    shell: &str,
    home: &Path,
    etc_paths: &Path,
    etc_paths_d: &Path,
) -> Vec<String> {
    let mut entries = vec![format!("shell{SEP}{shell}")];
    // Rc set (skipped entirely when HOME is unset — no rc can be sourced).
    if !home.as_os_str().is_empty() {
        for rc in rc_files_for_shell(shell, home) {
            if let Some((path, mtime)) = mtime_entry(&rc) {
                entries.push(format!("rc{SEP}{}{SEP}{mtime}", path.display()));
            }
        }
    }
    // System sources: /etc/paths itself + every /etc/paths.d/* entry.
    if let Some((path, mtime)) = mtime_entry(etc_paths) {
        entries.push(format!("sys{SEP}{}{SEP}{mtime}", path.display()));
    }
    if let Ok(read) = std::fs::read_dir(etc_paths_d) {
        for entry in read.flatten() {
            if let Some((path, mtime)) = mtime_entry(&entry.path()) {
                entries.push(format!("sysd{SEP}{}{SEP}{mtime}", path.display()));
            }
        }
    }
    entries.sort();
    entries
}

/// Recompute the fingerprint for the running process (real HOME, /etc).
fn current_fingerprint() -> Vec<String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    compute_fingerprint(
        &effective_shell(),
        &home,
        Path::new("/etc/paths"),
        Path::new("/etc/paths.d"),
    )
}

/// Load cached bins; `Some` only when the stored fingerprint matches the
/// expected one exactly (all entries equal — conjunction semantics).
fn load_cache_at(path: &Path, fingerprint: &[String]) -> Option<Vec<String>> {
    let bytes = std::fs::read(path).ok()?;
    let cache: PathBinsCache = serde_json::from_slice(&bytes).ok()?;
    (cache.fingerprint == *fingerprint).then_some(cache.bins)
}

/// Write the cache file. Called from the background scan thread; a failed
/// write only costs one re-scan on the next launch, so it logs and gives up.
fn write_cache_at(path: &Path, fingerprint: &[String], bins: &[String]) {
    let cache = PathBinsCache {
        fingerprint: fingerprint.to_vec(),
        bins: bins.to_vec(),
    };
    let Ok(bytes) = serde_json::to_vec(&cache) else {
        return;
    };
    let write = || -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)?;
        Ok(())
    };
    if let Err(e) = write() {
        tracing::warn!(
            error = %e,
            path = %path.display(),
            "path-bins cache write failed"
        );
    }
}

/// Full path of the cache file (`$XDG_CACHE_HOME/weft` else `~/.cache/weft`).
fn cache_path() -> Option<PathBuf> {
    crate::weft_cache_dir().map(|dir| dir.join(CACHE_FILE))
}

fn load_cache(fingerprint: &[String]) -> Option<Vec<String>> {
    load_cache_at(&cache_path()?, fingerprint)
}

fn write_cache(fingerprint: &[String], bins: &[String]) {
    let Some(path) = cache_path() else {
        tracing::debug!("no cache dir; skipping path-bins cache write");
        return;
    };
    write_cache_at(&path, fingerprint, bins);
}

/// Startup PATH resolution (called from `App::new` on the main thread).
/// Cache hit → synchronous read, no subprocess. Miss → the env-PATH fast
/// subset now, plus a background thread that replicates the full
/// `scan_path_bins` union; that thread writes the cache and posts
/// `AppEvent::PathBinsResolved` to backfill `ConfigState::path_bins`.
pub(super) fn startup_path_bins(proxy: &EventLoopProxy<AppEvent>) -> Vec<String> {
    let fingerprint = current_fingerprint();
    if let Some(bins) = load_cache(&fingerprint) {
        tracing::info!(bins = bins.len(), "path-bins cache hit");
        return bins;
    }
    // Fast path: the env-PATH arm of the union (macOS default four dirs for
    // GUI launches). Completions upgrade when the background scan lands.
    let fast = macos_system::bins_from_path_strings(std::env::var_os("PATH").into_iter().collect());
    spawn_background_scan(proxy.clone(), fingerprint);
    fast
}

/// Background replica of the full `scan_path_bins` union (login shell +
/// path_helper + env PATH; the 1500ms deadline guard lives inside
/// `resolve_login_path`, the 500ms one inside `path_helper_path`). The cache
/// write completes before the event so a handler observing
/// `PathBinsResolved` always has a persisted cache.
fn spawn_background_scan(proxy: EventLoopProxy<AppEvent>, fingerprint: Vec<String>) {
    let spawned = std::thread::Builder::new()
        .name("path-scan".into())
        .spawn(move || {
            let bins = macos_system::scan_path_bins();
            write_cache(&fingerprint, &bins);
            tracing::info!(bins = bins.len(), "background path scan complete");
            // Event-loop-closed is the only failure mode (benign); log it
            // per the repo's inspect_err precedent instead of full silence.
            if proxy.send_event(AppEvent::PathBinsResolved(bins)).is_err() {
                tracing::debug!("path-bins result dropped: event loop closed");
            }
        });
    if let Err(e) = spawned {
        // The fast path already applies; only the backfill is lost this run.
        tracing::warn!(error = %e, "path-scan thread spawn failed; keeping env-PATH fast bins");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Isolated temp tree (home + etc/paths + etc/paths.d) — no process-env
    /// mutation, so parallel cargo-test threads are unaffected. Removed on
    /// drop.
    struct ScanFixture {
        root: PathBuf,
    }

    impl ScanFixture {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "weft-path-scan-{}-{}-{}",
                tag,
                std::process::id(),
                std::time::SystemTime::UNIX_EPOCH
                    .elapsed()
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(root.join("home/.config/fish")).unwrap();
            std::fs::create_dir_all(root.join("etc/paths.d")).unwrap();
            std::fs::write(root.join("etc/paths"), "/usr/bin:/bin").unwrap();
            Self { root }
        }

        fn home(&self) -> PathBuf {
            self.root.join("home")
        }

        fn etc_paths(&self) -> PathBuf {
            self.root.join("etc/paths")
        }

        fn etc_paths_d(&self) -> PathBuf {
            self.root.join("etc/paths.d")
        }

        fn fingerprint(&self, shell: &str) -> Vec<String> {
            compute_fingerprint(shell, &self.home(), &self.etc_paths(), &self.etc_paths_d())
        }
    }

    impl Drop for ScanFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Deterministic mtime for invalidation tests (`File::set_times`,
    /// stable 1.75 — same pattern as app_runtime/log_rotate_tests.rs).
    fn set_mtime(path: &Path, t: std::time::SystemTime) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(t))
            .unwrap();
    }

    #[test]
    fn rc_set_dispatches_by_shell_basename() {
        let home = Path::new("/Users/t");
        // zsh 四件套 / bash 两件 / fish 一件 / 未知 shell → zsh 集。
        assert_eq!(rc_files_for_shell("/bin/zsh", home).len(), 4);
        assert_eq!(rc_files_for_shell("/bin/bash", home).len(), 2);
        assert_eq!(rc_files_for_shell("/opt/homebrew/bin/fish", home).len(), 1);
        assert_eq!(
            rc_files_for_shell("/usr/local/bin/elvish", home),
            rc_files_for_shell("/bin/zsh", home)
        );
        assert_eq!(
            rc_files_for_shell("/bin/bash", home)[0],
            PathBuf::from("/Users/t/.bash_profile")
        );
        assert_eq!(
            rc_files_for_shell("/opt/homebrew/bin/fish", home)[0],
            PathBuf::from("/Users/t/.config/fish/config.fish")
        );
    }

    #[test]
    fn fingerprint_is_stable_when_all_entries_match() {
        let fx = ScanFixture::new("stable");
        std::fs::write(fx.home().join(".zshrc"), "export PATH=$PATH:/a").unwrap();
        std::fs::write(fx.etc_paths_d().join("homebrew"), "/opt/homebrew/bin").unwrap();
        assert_eq!(fx.fingerprint("/bin/zsh"), fx.fingerprint("/bin/zsh"));
    }

    #[test]
    fn fingerprint_misses_when_rc_mtime_changes() {
        let fx = ScanFixture::new("rc-mtime");
        let rc = fx.home().join(".zshrc");
        std::fs::write(&rc, "export PATH=$PATH:/a").unwrap();
        let before = fx.fingerprint("/bin/zsh");
        set_mtime(
            &rc,
            std::time::SystemTime::now() - std::time::Duration::from_secs(60),
        );
        assert_ne!(before, fx.fingerprint("/bin/zsh"), "rc mtime 变 → miss");
    }

    #[test]
    fn fingerprint_misses_when_paths_d_gains_entry() {
        let fx = ScanFixture::new("paths-d");
        let before = fx.fingerprint("/bin/zsh");
        std::fs::write(fx.etc_paths_d().join("docker"), "/usr/local/bin").unwrap();
        assert_ne!(before, fx.fingerprint("/bin/zsh"), "paths.d 新增 → miss");
    }

    #[test]
    fn fingerprint_covers_shell_result_and_rc_presence() {
        let fx = ScanFixture::new("shell-rc");
        // resolve_user_shell 结果参与指纹（shell 类型换 → rc 集换 → 必 miss）。
        assert_ne!(fx.fingerprint("/bin/zsh"), fx.fingerprint("/bin/bash"));
        // rc 文件缺失 = 跳过该条目；出现即失效。
        let without_rc = fx.fingerprint("/bin/zsh");
        std::fs::write(fx.home().join(".zprofile"), "export PATH=$PATH:/b").unwrap();
        assert_ne!(without_rc, fx.fingerprint("/bin/zsh"));
    }

    #[test]
    fn cache_roundtrip_and_fingerprint_gate() {
        let fx = ScanFixture::new("cache");
        let path = fx.root.join("path-bins.json");
        let fp = vec![
            "shell\u{1f}/bin/zsh".to_string(),
            "rc\u{1f}/h/.zshrc\u{1f}42".to_string(),
        ];
        let bins = vec!["grep".to_string(), "ls".to_string()];
        write_cache_at(&path, &fp, &bins);
        // 往返：同指纹 → 命中且 bins 保真。
        assert_eq!(load_cache_at(&path, &fp), Some(bins));
        // 指纹不一致 → miss（不返回陈旧 bins）。
        let stale = vec!["shell\u{1f}/bin/zsh".to_string()];
        assert_eq!(load_cache_at(&path, &stale), None);
        // 损坏 JSON → miss（下次启动后台重扫）。
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(load_cache_at(&path, &fp), None);
        // 缺失文件 → miss。
        std::fs::remove_file(&path).unwrap();
        assert_eq!(load_cache_at(&path, &fp), None);
    }
}
