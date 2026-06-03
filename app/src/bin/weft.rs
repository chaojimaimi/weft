// Weft — A modern, local-first terminal with AI-powered command intelligence.
// Based on warpdotdev/warp, with all cloud/team features removed.
//
// On Windows, we don't want to display a console window when the application is running in release
// builds. See https://doc.rust-lang.org/reference/runtime.html#the-windows_subsystem-attribute.
#![cfg_attr(feature = "release_bundle", windows_subsystem = "windows")]

use anyhow::Result;
use warp_core::channel::{Channel, ChannelConfig, ChannelState, OzConfig, WarpServerConfig};
use warp_core::AppId;

/// Weft 纯本地终端入口
///
/// 相比 Warp OSS 的关键区别：
/// 1. 使用 Channel::Local 允许自定义服务器配置
/// 2. 服务器配置为空（不依赖任何 Warp 服务器）
/// 3. 启用 WEFT_FLAGS 特性集（纯本地核心功能）
/// 4. 无自动更新、无遥测、无会话共享
fn main() -> Result<()> {
    let mut state = ChannelState::new(
        Channel::Local,
        ChannelConfig {
            app_id: AppId::new("dev", "weft", "Weft"),
            logfile_name: "weft.log".into(),
            server_config: WarpServerConfig::none(),
            oz_config: OzConfig::none(),
            telemetry_config: None,
            crash_reporting_config: None,
            autoupdate_config: None,
            mcp_static_config: None,
        },
    );

    // Weft 核心本地功能 Flags
    state = state.with_additional_features(warp_core::features::WEFT_FLAGS);

    // Debug 构建下额外启用调试 Flags
    if cfg!(debug_assertions) {
        state = state.with_additional_features(warp_core::features::DEBUG_FLAGS);
    }

    ChannelState::set(state);
    warp::run()
}

// macOS 嵌入 Info.plist
#[cfg(all(not(feature = "extern_plist"), target_os = "macos"))]
embed_plist::embed_info_plist_bytes!(r#"
    <?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE plist PUBLIC "-//Apple Computer//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
    <plist version="1.0">
    <dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>English</string>
    <key>CFBundleDisplayName</key>
    <string>Weft</string>
    <key>CFBundleExecutable</key>
    <string>weft</string>
    <key>CFBundleIdentifier</key>
    <string>dev.weft.Weft</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Weft</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>UIDesignRequiresCompatibility</key>
    <true/>
    <key>CFBundleURLTypes</key>
    <array><dict><key>CFBundleURLName</key><string>Custom App</string><key>CFBundleURLSchemes</key><array><string>weft</string></array></dict></array>
    <key>NSHumanReadableCopyright</key>
    <string>Weft — Pure Local Terminal</string>
    </dict>
    </plist>
"#.as_bytes());
