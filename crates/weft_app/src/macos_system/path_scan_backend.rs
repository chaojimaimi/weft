//! Login-shell / PATH-resolution backend for the T16c path-scan family,
//! moved verbatim out of `macos_system.rs` (v1.13.8 S3 zero-behavior
//! file-budget split). The three crate-visible entry points
//! (`resolve_user_shell` / `scan_path_bins` / `bins_from_path_strings`)
//! are re-exported from `macos_system` so the path_scan.rs call sites
//! stay unchanged.

/// Extract the login `$PATH` from a (possibly polluted) shell stdout.
/// The shell prints `\x01$PATH\x02`; anything the user's rc files printed
/// before that (fortune/neofetch/echo) is noise and must not corrupt the
/// leading PATH entry — without sentinels, `<pollution>\n/opt/homebrew/bin`
/// would glue pollution to the first real entry and drop homebrew from the
/// scan. Returns `None` when either sentinel is missing, when they appear in
/// the wrong order, or when the extracted value fails the sanity check
/// (non-empty and containing `/`).
fn extract_path_from_output(s: &str) -> Option<&str> {
    let (start, end) = (s.find('\x01')?, s.find('\x02')?);
    if end <= start {
        return None;
    }
    let path = &s[start + 1..end];
    if path.is_empty() || !path.contains('/') {
        return None;
    }
    Some(path)
}

/// Resolve the user's login shell: `$SHELL` env → `dscl` user record →
/// `/bin/zsh` fallback. GUI/Dock-launched processes usually have no
/// `SHELL` variable, so the env-only lookup silently picked the fallback
/// even when the real shell is bash/fish. `dscl . -read ... UserShell`
/// reads the actual login shell from the directory service without touching
/// any rc file. Best-effort: any failure returns `None`, letting the
/// caller fall back. v1.13.5 (T16c): `pub(super)` — path_scan.rs keys its
/// cache fingerprint off the same shell this resolves.
pub(crate) fn resolve_user_shell() -> Option<std::ffi::OsString> {
    use std::process::Command;
    use std::time::{Duration, Instant};

    if let Some(shell) = std::env::var_os("SHELL") {
        if !shell.is_empty() {
            return Some(shell);
        }
    }
    // dscl . -read /Users/<user> UserShell  → "UserShell: /bin/zsh"
    let user = std::env::var("USER").unwrap_or_default();
    if user.is_empty() {
        return None;
    }
    let mut child = Command::new("/usr/bin/dscl")
        .args([".", "-read", &format!("/Users/{user}"), "UserShell"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // reap — never leave a zombie
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.wait();
                return None;
            }
        }
    }
    use std::io::Read;
    let mut out = String::new();
    if child.stdout.take()?.read_to_string(&mut out).is_err() {
        let _ = child.wait();
        return None;
    }
    let shell = out
        .lines()
        .find_map(|line| line.strip_prefix("UserShell:"))
        .map(str::trim)
        .filter(|s| s.starts_with('/'));
    let shell = shell.map(std::ffi::OsString::from);
    // Child already reaped by try_wait above; this wait is a no-op guard.
    let _ = child.wait();
    shell
}

/// Parse the `PATH="..."` assignment emitted by `/usr/libexec/path_helper -s`.
/// macOS's path_helper reads `/etc/paths` + `/etc/paths.d/*` (Homebrew and
/// other installers drop files there) and prints
/// `PATH="/a:/b"; export PATH;`. Returns `None` when the quoted value is
/// missing or fails the same sanity check as the login-PATH sentinels.
fn extract_path_helper_path(s: &str) -> Option<&str> {
    let start = s.find("PATH=\"")? + "PATH=\"".len();
    let rest = &s[start..];
    let end = rest.find('"')?;
    let path = &rest[..end];
    if path.is_empty() || !path.contains('/') {
        return None;
    }
    Some(path)
}

/// 启动时解析用户登录 shell 的 `$PATH`(GUI/Dock 启动的 weft 进程 PATH 是
/// macOS 默认四目录,不含 homebrew/`~/.local/bin`/nvm/cargo 等;这些由登录
/// shell 的 .zprofile/.zshrc 添加)。失败/为空时回退到 path_helper 输出,
/// 再回退到进程 env PATH(见 `scan_path_bins`)。
///
/// 不继承 stdin/stderr;1500ms deadline 后强杀子进程,防止病态 .zshrc
/// (阻塞 daemon、卡住的 eval)拖死主线程上的启动扫描。shell 经
/// `resolve_user_shell` 探测(dscl),不再假设 `$SHELL` 存在。
fn resolve_login_path() -> Option<std::ffi::OsString> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let shell = resolve_user_shell().unwrap_or_else(|| "/bin/zsh".into());
    // -l 登录(source .zprofile/.bash_profile)+ -i 交互(source .zshrc/.bashrc)
    // + -c 跑命令。SOH/STX 哨兵包裹 PATH,屏蔽 rc 文件的 stdout 污染;
    // printf 不带尾换行,避免解析噪音。Rust 侧 \\x01 是字面反斜杠+x01,
    // shell 的 printf 再解释为 SOH 控制字节。
    let mut child = Command::new(&shell)
        .args(["-lic", "printf '\\x01%s\\x02' \"$PATH\""])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let _ = child.kill();
                    return None;
                }
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
    // 子进程已退出(被 try_wait reap),读管道即时返回,不会阻塞。
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let path = extract_path_from_output(&out)?;
    Some(std::ffi::OsString::from(path.to_string()))
}

/// Pure-ish:给定路径,判断是否为"可执行文件"(跟随符号链接)。
/// pnpm/node/brew 等是 symlink → Cellar/shim;`metadata`(非 symlink_metadata)
/// 解析链接目标,is_file() 对指向普通文件/脚本的 symlink 返回 true。
/// 额外要求可执行位(mode & 0o111),排除 PATH 目录里的 .DS_Store/README 等。
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
        .unwrap_or(false)
}

/// Collect executable names from the *union* of all PATH sources (login
/// shell → path_helper → env PATH; see `path_sources`). v1.13.5 (T16c):
/// runs only on the path_scan background thread / cache-miss path.
pub(crate) fn scan_path_bins() -> Vec<String> {
    bins_from_path_strings(path_sources())
}

/// The shared PATH-string traversal behind `scan_path_bins` and the
/// env-PATH fast path (path_scan.rs) — BTreeSet dedupes + sorts.
pub(crate) fn bins_from_path_strings(sources: Vec<std::ffi::OsString>) -> Vec<String> {
    let mut bins = std::collections::BTreeSet::new();
    for path in sources {
        for dir in std::env::split_paths(&path) {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if is_executable_file(&entry.path()) {
                        if let Some(name) = entry.file_name().to_str() {
                            bins.insert(name.to_string());
                        }
                    }
                }
            }
        }
    }
    bins.into_iter().collect()
}

/// The ordered list of PATH strings to scan, deduped (a dir appearing in
/// several sources is only scanned once — split_paths cost is negligible,
/// the BTreeSet dedupe in `bins_from_path_strings` covers names).
pub(super) fn path_sources() -> Vec<std::ffi::OsString> {
    let mut sources: Vec<std::ffi::OsString> = Vec::new();
    if let Some(path) = resolve_login_path() {
        sources.push(path);
    }
    if let Some(path) = path_helper_path() {
        sources.push(path);
    }
    if let Some(path) = std::env::var_os("PATH") {
        sources.push(path);
    }
    sources
}

/// Run `/usr/libexec/path_helper -s` and parse the emitted `PATH="..."`.
/// path_helper is macOS-native, reads /etc/paths.d (Homebrew writes
/// `/etc/paths.d/homebrew`), has no side effects and is fast (<10ms).
/// Returns `None` on spawn/parse failure so the caller falls through to
/// the next source.
fn path_helper_path() -> Option<std::ffi::OsString> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new("/usr/libexec/path_helper")
        .args(["-s"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    // path_helper is normally <10ms, but a wedged /etc/paths.d entry must
    // not block the startup scan forever — same deadline pattern as
    // resolve_login_path (shorter: no rc files involved).
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // reap — never leave a zombie
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    if child.stdout.take()?.read_to_string(&mut out).is_err() {
        let _ = child.wait();
        return None;
    }
    let path = extract_path_helper_path(&out).map(std::ffi::OsString::from);
    // Child already reaped by try_wait above; this wait is a no-op guard.
    let _ = child.wait();
    path
}

#[cfg(test)]
mod tests {
    use super::{
        extract_path_from_output, extract_path_helper_path, is_executable_file, path_sources,
        resolve_user_shell, scan_path_bins,
    };

    #[test]
    fn is_executable_file_follows_symlinks_and_requires_exec_bit() {
        use std::os::unix::fs::symlink;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("weft-exec-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let plain_file = dir.join("tool");
        let no_exec = dir.join("readme.txt");
        let link = dir.join("tool-link");
        let dir_link = dir.join("dir-link");
        let target_dir = dir.join("target-dir");
        std::fs::write(&plain_file, b"#!/bin/sh\n").expect("write tool");
        std::fs::write(&no_exec, b"notes\n").expect("write readme");
        std::fs::create_dir_all(&target_dir).expect("create target dir");

        std::fs::set_permissions(&plain_file, std::fs::Permissions::from_mode(0o755))
            .expect("chmod 755");
        std::fs::set_permissions(&no_exec, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 644");
        symlink(&plain_file, &link).expect("symlink to file");
        symlink(&target_dir, &dir_link).expect("symlink to dir");

        assert!(is_executable_file(&plain_file), "0o755 file");
        assert!(is_executable_file(&link), "symlink to executable file");
        assert!(!is_executable_file(&no_exec), "0o644 file");
        assert!(!is_executable_file(&dir_link), "symlink to dir");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_login_path_extracts_between_sentinels() {
        // Clean output: extract exactly the sentinel-wrapped PATH.
        assert_eq!(
            extract_path_from_output("\x01/opt/homebrew/bin:/usr/bin\x02"),
            Some("/opt/homebrew/bin:/usr/bin")
        );
        // Polluted output (fortune/neofetch/echo before the printf): the
        // leading PATH entry must NOT absorb the pollution.
        assert_eq!(
            extract_path_from_output("fortune line\n\x01/opt/homebrew/bin:/usr/bin\x02"),
            Some("/opt/homebrew/bin:/usr/bin")
        );
        // Missing sentinels → fall back to env PATH.
        assert_eq!(extract_path_from_output("garbage"), None);
        // Reversed sentinel order → fall back.
        assert_eq!(extract_path_from_output("\x02/usr/bin\x01"), None);
        // Sanity check: non-empty and containing '/' is required.
        assert_eq!(
            extract_path_from_output("\x01/usr/bin\x02"),
            Some("/usr/bin")
        );
        assert_eq!(extract_path_from_output("\x01\x02"), None);
    }

    #[test]
    fn scan_path_bins_finds_system_commands() {
        // Smoke: on macOS the PATH (login or fallback) always resolves core
        // system commands, so the scan must be non-empty and include ls/cat.
        let bins = scan_path_bins();
        assert!(!bins.is_empty(), "PATH scan must not be empty");
        assert!(
            bins.iter().any(|b| b == "ls" || b == "cat"),
            "expected ls or cat in PATH bins, got {bins:?}"
        );
    }

    #[test]
    fn extract_path_helper_path_parses_quoted_assignment() {
        // Canonical path_helper -s output.
        assert_eq!(
            extract_path_helper_path("PATH=\"/opt/homebrew/bin:/usr/bin\"; export PATH;"),
            Some("/opt/homebrew/bin:/usr/bin")
        );
        // Multiple lines / leading noise are tolerated.
        assert_eq!(
            extract_path_helper_path(
                "foo
PATH=\"/usr/local/bin:/usr/bin\"; export PATH;"
            ),
            Some("/usr/local/bin:/usr/bin")
        );
        // Missing / empty / path-less values → None.
        assert_eq!(extract_path_helper_path("export PATH;"), None);
        assert_eq!(extract_path_helper_path("PATH=\"\";"), None);
        assert_eq!(extract_path_helper_path("PATH=\"relative\";"), None);
    }

    #[test]
    fn path_sources_is_non_empty_and_deduped_by_scan() {
        // The union must at least cover the process env PATH, and the bins
        // scan must remain deterministic (BTreeSet).
        let sources = path_sources();
        assert!(!sources.is_empty(), "must have at least one PATH source");
        let bins = scan_path_bins();
        let mut sorted = bins.clone();
        sorted.sort();
        assert_eq!(bins, sorted, "bins must be sorted");
    }

    #[test]
    fn resolve_user_shell_prefers_env_or_falls_back_safely() {
        // Save the real SHELL and always restore it, so a panicking assert
        // cannot leak a polluted environment into parallel tests.
        let original_shell = std::env::var_os("SHELL");
        // With SHELL set, it is used directly (no subprocess).
        std::env::set_var("SHELL", "/bin/bash");
        let result = resolve_user_shell();
        match original_shell.as_ref() {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
        assert_eq!(result, Some(std::ffi::OsString::from("/bin/bash")));
        // Empty SHELL + no USER → pure fallback path, no dscl spawn: the
        // guard must return None without panicking or leaking a child.
        std::env::set_var("SHELL", "");
        let original_user = std::env::var_os("USER");
        std::env::remove_var("USER");
        let result = resolve_user_shell();
        match original_user.as_ref() {
            Some(v) => std::env::set_var("USER", v),
            None => std::env::remove_var("USER"),
        }
        match original_shell.as_ref() {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
        assert_eq!(result, None, "no SHELL and no USER must fall back safely");
    }
}
