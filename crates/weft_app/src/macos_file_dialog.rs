//! v1.5.2: Type-safe NSOpenPanel / NSSavePanel wrappers for config
//! import/export.
//!
//! Per V15_IMPLEMENTATION_PLAN.md §7.3:
//!
//! - Only call on the main thread (enforced via `MainThreadMarker`).
//! - Use objc2-app-kit typed APIs, not raw `msg_send!` (except for the
//!   few setters that aren't exposed via the safe wrapper).
//! - OpenPanel: single-file selection, `.toml` only.
//! - SavePanel: default file name `weft-config.toml`.
//! - Cancel returns `Ok(None)`, not an error.
//! - NSURL → PathBuf conversion failure is surfaced as an error, never a panic.
//!
//! The wrappers are intentionally minimal — they only produce a `PathBuf`.
//! The caller (App layer) feeds that path into
//! `import_config_document` / `export_config_document`.

use std::path::PathBuf;

use objc2_app_kit::{
    NSModalResponse, NSModalResponseCancel, NSModalResponseOK, NSOpenPanel, NSSavePanel,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSString, NSURL};

/// Result of a file panel interaction. `Ok(None)` means the user cancelled.
pub type FilePanelResult = Result<Option<PathBuf>, FilePanelError>;

/// Errors raised by the file panel wrappers.
#[derive(Debug)]
pub enum FilePanelError {
    /// Not on the main thread. NSOpenPanel/NSSavePanel are UI APIs and
    /// must be invoked from the main thread.
    NotMainThread,
    /// The panel ran but returned an unexpected modal response (not OK
    /// and not Cancel).
    UnexpectedResponse(NSModalResponse),
    /// The selected URL's `path` returned nil. This shouldn't happen for
    /// file:// URLs but is surfaced as an error rather than a panic.
    NoPath,
}

impl std::fmt::Display for FilePanelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotMainThread => write!(f, "file panel must be invoked on the main thread"),
            Self::UnexpectedResponse(r) => {
                write!(f, "file panel returned unexpected modal response: {r:?}")
            }
            Self::NoPath => write!(f, "selected URL has no file path"),
        }
    }
}

impl std::error::Error for FilePanelError {}

/// Show an NSOpenPanel for selecting a single `.toml` config file.
///
/// Returns:
/// - `Ok(Some(path))` — user selected a file.
/// - `Ok(None)` — user cancelled (Cancel button or Esc).
/// - `Err(NotMainThread)` — called off the main thread.
/// - `Err(UnexpectedResponse)` — panel returned an unknown modal code.
/// - `Err(NoPath)` — selected URL's path was nil.
///
/// The panel is modal (`runModal`) so it blocks the main thread until the
/// user confirms or cancels. This matches the V15 plan §7.3 requirement
/// that panels run on the main thread.
pub fn pick_config_import_path(mtm: MainThreadMarker) -> FilePanelResult {
    // SAFETY: NSOpenPanel::openPanel requires MainThreadMarker; we have it.
    // The returned panel is retained by us and autoreleased per the
    // standard objc2 retain semantics.
    let panel = unsafe { NSOpenPanel::openPanel(mtm) };
    unsafe {
        panel.setCanChooseFiles(true);
        panel.setCanChooseDirectories(false);
        panel.setAllowsMultipleSelection(false);
        // setAllowedFileTypes takes NSArray<NSString>. NSString has a
        // mutable subclass so `from_slice` (which requires `IsRetainable`)
        // doesn't apply; `from_vec` consumes `Retained<NSString>` and
        // works without that bound. The method is deprecated in favor of
        // setAllowedContentTypes:, but the V15 plan §7.3 explicitly chose
        // `allowedFileTypes` for its simplicity (single string `["toml"]`
        // vs. constructing UTType). The API still works on macOS 14+.
        let toml_type = NSString::from_str("toml");
        let types = NSArray::from_vec(vec![toml_type]);
        #[allow(deprecated)]
        panel.setAllowedFileTypes(Some(&types));
        let title = NSString::from_str("Import Config");
        panel.setTitle(Some(&title));
        let prompt = NSString::from_str("Import");
        panel.setPrompt(Some(&prompt));
    }
    run_modal_open_panel(&panel)
}

/// Show an NSSavePanel for exporting the config to a `.toml` file.
///
/// The default file name is `weft-config.toml`. The default directory is
/// the current config file's parent, if known.
///
/// Returns the same variants as [`pick_config_import_path`].
pub fn pick_config_export_path(mtm: MainThreadMarker) -> FilePanelResult {
    // SAFETY: NSSavePanel::savePanel requires MainThreadMarker; we have it.
    let panel = unsafe { NSSavePanel::savePanel(mtm) };
    unsafe {
        let title = NSString::from_str("Export Config");
        panel.setTitle(Some(&title));
        let prompt = NSString::from_str("Export");
        panel.setPrompt(Some(&prompt));
        let default_name = NSString::from_str("weft-config.toml");
        panel.setNameFieldStringValue(&default_name);
        let toml_type = NSString::from_str("toml");
        let types = NSArray::from_vec(vec![toml_type]);
        #[allow(deprecated)]
        panel.setAllowedFileTypes(Some(&types));

        // Default directory: the current config file's parent, if known.
        // Using `Config::config_path()` keeps the default consistent with
        // the load/save paths (including the future WEFT_CONFIG override
        // in v1.5.3).
        if let Some(parent) = weft_core::config::Config::config_path()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        {
            let ns_parent = NSString::from_str(&parent.to_string_lossy());
            let ns_url = NSURL::fileURLWithPath(&ns_parent);
            panel.setDirectoryURL(Some(&ns_url));
        }
    }
    run_modal_save_panel(&panel)
}

/// Run an NSOpenPanel modally and extract the selected path on OK.
fn run_modal_open_panel(panel: &NSOpenPanel) -> FilePanelResult {
    // SAFETY: runModal is safe to call on the main thread; we have the
    // MainThreadMarker from the caller.
    let response = unsafe { panel.runModal() };
    if response == NSModalResponseCancel {
        return Ok(None);
    }
    if response != NSModalResponseOK {
        return Err(FilePanelError::UnexpectedResponse(response));
    }
    // SAFETY: URL is inherited from NSSavePanel; safe to call on NSOpenPanel.
    let url = unsafe { panel.URL() }.ok_or(FilePanelError::NoPath)?;
    let path = unsafe { url.path() }.ok_or(FilePanelError::NoPath)?;
    Ok(Some(PathBuf::from(path.to_string())))
}

/// Run an NSSavePanel modally and extract the target path on OK.
fn run_modal_save_panel(panel: &NSSavePanel) -> FilePanelResult {
    let response = unsafe { panel.runModal() };
    if response == NSModalResponseCancel {
        return Ok(None);
    }
    if response != NSModalResponseOK {
        return Err(FilePanelError::UnexpectedResponse(response));
    }
    let url = unsafe { panel.URL() }.ok_or(FilePanelError::NoPath)?;
    let path = unsafe { url.path() }.ok_or(FilePanelError::NoPath)?;
    Ok(Some(PathBuf::from(path.to_string())))
}
