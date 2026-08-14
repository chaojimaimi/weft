//! macOS system integration kept outside the application orchestrator.

/// v0.9 U-D1: Query macOS system appearance via `NSUserDefaults`.
/// Returns `true` when the user has Dark mode selected in System
/// Settings, `false` for Light (the macOS default — `AppleInterfaceStyle`
/// is absent/empty when Light is active). Used by `poll_system_appearance`
/// to follow the system appearance live (throttled to 1Hz by the caller).
///
/// Reads `AppleInterfaceStyle` from `NSUserDefaults.standardUserDefaults`,
/// which is kept in sync by the OS across `AppleInterfaceThemeChangedNotification`.
/// We poll rather than register a distributed-notification observer because
/// winit owns the `NSApplication` and its delegate, making selector-based
/// callbacks awkward; a 1Hz poll is cheap and matches the existing
/// config-mtime poller pattern.
pub(super) unsafe fn system_appearance_is_dark() -> bool {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let defaults_cls = objc2::ffi::objc_getClass(c"NSUserDefaults".as_ptr());
    let str_cls = objc2::ffi::objc_getClass(c"NSString".as_ptr());
    if defaults_cls.is_null() || str_cls.is_null() {
        return false;
    }
    let defaults: *mut AnyObject =
        msg_send![defaults_cls as *const AnyObject, standardUserDefaults];
    if defaults.is_null() {
        return false;
    }
    let c_key = std::ffi::CString::new("AppleInterfaceStyle").unwrap_or_default();
    let key_ns: *mut AnyObject =
        msg_send![str_cls as *const AnyObject, stringWithUTF8String: c_key.as_ptr()];
    if key_ns.is_null() {
        return false;
    }
    // stringForKey: returns nil for absent keys (Light mode default).
    let value_ns: *mut AnyObject = msg_send![defaults, stringForKey: key_ns];
    if value_ns.is_null() {
        return false;
    }
    let c_str: *const i8 = msg_send![value_ns, UTF8String];
    if c_str.is_null() {
        return false;
    }
    let raw = std::ffi::CStr::from_ptr(c_str);
    let s = raw.to_str().unwrap_or("").trim().to_ascii_lowercase();
    s == "dark"
}

/// F3-2: Query the macOS "Reduce Motion" accessibility setting.
/// Returns `true` when the user has enabled System Settings → Accessibility →
/// Display → Reduce Motion. When true, the running-command spinner uses a
/// static `●` instead of the animated braille glyphs. Polled at 1Hz alongside
/// `system_appearance_is_dark` (see `poll_system_appearance`).
pub(super) unsafe fn system_reduce_motion() -> bool {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let workspace_cls = objc2::ffi::objc_getClass(c"NSWorkspace".as_ptr());
    if workspace_cls.is_null() {
        return false;
    }
    let shared: *mut AnyObject = msg_send![workspace_cls as *const AnyObject, sharedWorkspace];
    if shared.is_null() {
        return false;
    }
    let reduce: bool = msg_send![shared, accessibilityDisplayShouldReduceMotion];
    reduce
}

/// F6: Query the macOS "Increase Contrast" accessibility setting.
/// Returns `true` when the user has enabled System Settings → Accessibility →
/// Display → Increase Contrast. When true, the renderer strengthens borders,
/// selection highlights and focus rings so state is perceivable without
/// relying on subtle color differences. Polled at 1Hz alongside
/// `system_appearance_is_dark` (see `poll_system_appearance`).
pub(super) unsafe fn system_increase_contrast() -> bool {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let workspace_cls = objc2::ffi::objc_getClass(c"NSWorkspace".as_ptr());
    if workspace_cls.is_null() {
        return false;
    }
    let shared: *mut AnyObject = msg_send![workspace_cls as *const AnyObject, sharedWorkspace];
    if shared.is_null() {
        return false;
    }
    let contrast: bool = msg_send![shared, accessibilityDisplayShouldIncreaseContrast];
    contrast
}

/// Copy text to macOS system clipboard using NSPasteboard.
pub(super) fn clipboard_copy(text: &str) {
    unsafe {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;

        let pb_cls = objc2::ffi::objc_getClass(c"NSPasteboard".as_ptr());
        let str_cls = objc2::ffi::objc_getClass(c"NSString".as_ptr());
        if pb_cls.is_null() || str_cls.is_null() {
            return;
        }
        let pasteboard: *mut AnyObject = msg_send![pb_cls as *const AnyObject, generalPasteboard];
        if pasteboard.is_null() {
            return;
        }

        // NSPasteboardTypeString == "public.utf8-plain-text". Build NSStrings
        // for the value and the type, then use the real setters (the old code
        // called non-existent `setString:` and `string` selectors, so the
        // clipboard never actually worked).
        let c_text = std::ffi::CString::new(text).unwrap_or_default();
        let value_ns: *mut AnyObject =
            msg_send![str_cls as *const AnyObject, stringWithUTF8String: c_text.as_ptr()];
        let c_type = std::ffi::CString::new("public.utf8-plain-text").unwrap();
        let type_ns: *mut AnyObject =
            msg_send![str_cls as *const AnyObject, stringWithUTF8String: c_type.as_ptr()];
        if value_ns.is_null() || type_ns.is_null() {
            return;
        }

        // clearContents returns NSInteger (objc2 verifies the return type code
        // against the method signature at runtime in debug, so this must be
        // `isize` = 'q', not `()`).
        let _: isize = msg_send![pasteboard, clearContents];
        // `setString:forType:` returns BOOL (arm64 macOS: `_Bool` = type code
        // 'B', matching Rust `bool`); we ignore it.
        let _: bool = msg_send![pasteboard, setString: value_ns forType: type_ns];
    }
}

/// Paste text from macOS system clipboard using NSPasteboard.
pub(super) fn clipboard_paste() -> Option<String> {
    unsafe {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;

        let pb_cls = objc2::ffi::objc_getClass(c"NSPasteboard".as_ptr());
        let str_cls = objc2::ffi::objc_getClass(c"NSString".as_ptr());
        if pb_cls.is_null() || str_cls.is_null() {
            return None;
        }
        let pasteboard: *mut AnyObject = msg_send![pb_cls as *const AnyObject, generalPasteboard];
        if pasteboard.is_null() {
            return None;
        }

        let c_type = std::ffi::CString::new("public.utf8-plain-text").unwrap();
        let type_ns: *mut AnyObject =
            msg_send![str_cls as *const AnyObject, stringWithUTF8String: c_type.as_ptr()];

        // stringForType: returns a nullable NSString (nil if no string of that
        // type is on the pasteboard).
        let ns_string: *mut AnyObject = msg_send![pasteboard, stringForType: type_ns];
        if ns_string.is_null() {
            return None;
        }

        let c_str: *const i8 = msg_send![ns_string, UTF8String];
        if c_str.is_null() {
            return None;
        }

        std::ffi::CStr::from_ptr(c_str)
            .to_str()
            .ok()
            .map(|s| s.to_owned())
    }
}

/// Error returned by [`open_url`]. Carries enough context for the caller
/// to surface a user-visible message via the status hint mechanism.
#[derive(Debug)]
pub struct OpenUrlError {
    /// The URL that failed to open.
    pub url: String,
    /// Human-readable reason.
    pub reason: String,
}

impl std::fmt::Display for OpenUrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Failed to open {}: {}", self.url, self.reason)
    }
}

impl std::error::Error for OpenUrlError {}

/// Open `url` using the system default handler (macOS `open`).
/// Used by OSC 8 Cmd+Click. Returns `Err` on failure so the caller can
/// surface a user-visible error via the status hint mechanism.
pub(super) fn open_url(url: &str) -> Result<(), OpenUrlError> {
    // Sanity-check the scheme before handing it to `open` — we don't want
    // `open file:///etc/passwd` surprises or arbitrary `open <path>` shells.
    let is_safe = url
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
        || url
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"));
    if !is_safe {
        tracing::warn!(url, "OSC 8 Cmd+Click refused non-http(s) URL");
        return Err(OpenUrlError {
            url: url.to_string(),
            reason: "Only http(s) URLs can be opened".into(),
        });
    }
    match std::process::Command::new("open").arg(url).status() {
        Ok(status) if !status.success() => {
            tracing::warn!(?status, url, "open exited non-zero");
            Err(OpenUrlError {
                url: url.to_string(),
                reason: format!("open command exited with status {}", status),
            })
        }
        Err(e) => {
            tracing::warn!(error = %e, url, "open spawn failed");
            Err(OpenUrlError {
                url: url.to_string(),
                reason: format!("Failed to spawn open: {e}"),
            })
        }
        Ok(_) => Ok(()),
    }
}

/// Reveal an existing local path in Finder without invoking a shell.
pub(super) fn reveal_path_in_finder(path: &std::path::Path) -> Result<(), OpenUrlError> {
    if !path.is_absolute() || !path.exists() {
        return Err(OpenUrlError {
            url: path.display().to_string(),
            reason: "Path must be absolute and exist".into(),
        });
    }
    match std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(OpenUrlError {
            url: path.display().to_string(),
            reason: format!("Finder reveal exited with status {status}"),
        }),
        Err(error) => Err(OpenUrlError {
            url: path.display().to_string(),
            reason: format!("Failed to spawn Finder reveal: {error}"),
        }),
    }
}

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

/// 启动时解析用户登录 shell 的 `$PATH`(GUI/Dock 启动的 weft 进程 PATH 是
/// macOS 默认四目录,不含 homebrew/`~/.local/bin`/nvm/cargo 等;这些由登录
/// shell 的 .zprofile/.zshrc 添加)。失败/为空时回退到进程 env PATH。
///
/// 不继承 stdin/stderr;500ms deadline 后强杀子进程,防止病态 .zshrc
/// (阻塞 daemon、卡住的 eval)拖死主线程上的启动扫描。
fn resolve_login_path() -> Option<std::ffi::OsString> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
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
    let deadline = Instant::now() + Duration::from_millis(500);
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

/// Scan `$PATH` for executable names (files, not dirs). Best-effort: unreadable
/// / missing dirs are skipped. Deduped + sorted. Cached once at startup.
pub(super) fn scan_path_bins() -> Vec<String> {
    let mut bins = std::collections::BTreeSet::new();
    let path = resolve_login_path().or_else(|| std::env::var_os("PATH"));
    let Some(path) = path else { return Vec::new() };
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
    bins.into_iter().collect()
}

/// Load the weft window icon from the embedded 256×256 PNG. Returns `None`
/// (winit default icon) if decode fails — best-effort, not a hard error.
/// The PNG is embedded at compile time via `include_bytes!`, so there's no
/// runtime file dependency.
pub(super) fn load_window_icon() -> Option<winit::window::Icon> {
    // Use the Cool variant (default) for the window title-bar icon. Set once
    // at window creation; runtime Dock icon switching via set_dock_icon() does
    // not update this (would need window recreation — acceptable trade-off).
    let png_bytes = include_bytes!("../../../assets/logo/variants/png/cool-256.png");
    let img = image::load_from_memory(png_bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    winit::window::Icon::from_rgba(rgba.into_raw(), w, h).ok()
}

/// High-resolution PNG bytes used for runtime Dock variants.
///
/// Unlike the bundled icon, an image installed with
/// `setApplicationIconImage` does not receive the bundle icon's automatic
/// visual inset. These rasters therefore include a transparent 12.5% margin
/// on every side so Dock magnification cannot overlap the running indicator.
fn logo_image_bytes(variant: weft_core::config::LogoVariant) -> &'static [u8] {
    use weft_core::config::LogoVariant;
    match variant {
        LogoVariant::Cool => include_bytes!("../../../assets/logo/variants/png/cool-1024.png"),
        LogoVariant::Warm => include_bytes!("../../../assets/logo/variants/png/warm-1024.png"),
        LogoVariant::Light => include_bytes!("../../../assets/logo/variants/png/light-1024.png"),
        LogoVariant::Transparent => {
            include_bytes!("../../../assets/logo/variants/png/transparent-1024.png")
        }
    }
}

fn should_use_bundle_dock_icon(
    variant: weft_core::config::LogoVariant,
    executable: &std::path::Path,
) -> bool {
    variant == weft_core::config::LogoVariant::Cool
        && executable
            .to_string_lossy()
            .contains(".app/Contents/MacOS/")
}

/// v1.0 Logo: set the macOS Dock app icon at runtime via
/// `NSApp.setApplicationIconImage:`. Constructs an NSImage from PNG bytes
/// using typed `objc2-app-kit` safe methods.
///
/// v1.2 fix: re-enabled using the same typed objc2-app-kit pattern proven
/// in `configure_titlebar` (renderer.rs). The original raw `msg_send!`
/// implementation panicked under an `extern "C"` boundary (nounwind →
/// abort). The typed `NSImage::initWithData:` + `NSApplication` setters
/// are safe and wrapped in `catch_unwind` as a belt-and-suspenders guard.
///
/// On failure (not on main thread, image decode, ObjC call), this is a
/// no-op: the Dock keeps whatever icon it currently has. Safe to call
/// repeatedly.
pub(super) unsafe fn set_dock_icon(variant: weft_core::config::LogoVariant) {
    use objc2::ClassType;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{MainThreadMarker, NSData};

    // NSApplication::sharedApplication requires a MainThreadMarker. If we're
    // not on the main thread (shouldn't happen — both call sites are in the
    // event loop), silently skip.
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!(?variant, "set_dock_icon skipped — not on main thread");
        return;
    };

    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let app = NSApplication::sharedApplication(mtm);
        if std::env::current_exe()
            .ok()
            .is_some_and(|path| should_use_bundle_dock_icon(variant, &path))
        {
            // `None` restores CFBundleIconFile. The bundled ICNS contains
            // macOS-native multi-resolution representations and lets Dock own
            // magnification/inset geometry instead of treating a runtime
            // 256px PNG as an unscaled replacement image.
            app.setApplicationIconImage(None);
            tracing::info!("using bundled Dock icon");
            return;
        }
        let icon_bytes = logo_image_bytes(variant);
        let ns_data = NSData::dataWithBytes_length(
            icon_bytes.as_ptr() as *mut std::ffi::c_void,
            icon_bytes.len(),
        );
        let ns_image = NSImage::initWithData(NSImage::alloc(), &ns_data);
        if let Some(image) = ns_image {
            app.setApplicationIconImage(Some(&image));
            tracing::info!(?variant, "dock icon updated");
        } else {
            tracing::warn!(?variant, "NSImage::initWithData returned nil");
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::{
        extract_path_from_output, is_executable_file, logo_image_bytes, scan_path_bins,
        should_use_bundle_dock_icon,
    };
    use std::path::Path;
    use weft_core::config::LogoVariant;

    #[test]
    fn cool_bundle_uses_icns_while_dev_and_custom_variants_override() {
        let bundled = Path::new("/Applications/Weft.app/Contents/MacOS/weft");
        assert!(should_use_bundle_dock_icon(LogoVariant::Cool, bundled));
        assert!(!should_use_bundle_dock_icon(LogoVariant::Warm, bundled));
        assert!(!should_use_bundle_dock_icon(
            LogoVariant::Cool,
            Path::new("/tmp/target/release/weft")
        ));
    }

    #[test]
    fn every_runtime_dock_variant_has_a_high_resolution_safe_area() {
        for variant in LogoVariant::ALL {
            let rgba = image::load_from_memory(logo_image_bytes(variant))
                .expect("embedded Dock icon must decode")
                .to_rgba8();
            assert_eq!(rgba.dimensions(), (1024, 1024));

            let mut bounds = (1024, 1024, 0, 0);
            for (x, y, pixel) in rgba.enumerate_pixels() {
                if pixel.0[3] > 0 {
                    bounds.0 = bounds.0.min(x);
                    bounds.1 = bounds.1.min(y);
                    bounds.2 = bounds.2.max(x);
                    bounds.3 = bounds.3.max(y);
                }
            }
            assert!(
                bounds.0 <= bounds.2 && bounds.1 <= bounds.3,
                "{variant:?} Dock icon must contain visible pixels"
            );
            assert!(
                bounds.0 >= 120 && bounds.1 >= 120 && bounds.2 <= 903 && bounds.3 <= 903,
                "{variant:?} alpha bounds {bounds:?} must preserve the Dock safe area"
            );
        }
    }

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
}
