//! v1.5.2: Config import/export controller — wires the atomic transfer
//! APIs (`weft_core::config::transfer`) to the macOS file panels
//! (`macos_file_dialog`) and the runtime apply path
//! (`config_state.set_loaded` + `apply_config`).
//!
//! Per V15_IMPLEMENTATION_PLAN.md §7:
//!
//! - Import is a complete replace: pick a `.toml` file → validate →
//!   backup current → atomically replace → reload → apply. Failures at
//!   any step leave the current file and runtime untouched.
//! - Export writes the **source** document (preserving comments and
//!   unknown fields when the source file exists).
//! - Cancel returns `Ok(())` and is indistinguishable from success at
//!   the call site (the only side effect is "no file was written").
//!
//! Both entry points are `pub(super)` so the Settings → Advanced click
//! handler and the Command Palette `ImportConfig`/`ExportConfig` builtins
//! can call them.

use super::*;

impl App {
    /// v1.5.2: Show NSOpenPanel, then `import_config_document` the picked
    /// file into the current config path. On success the runtime is
    /// reloaded and applied atomically. On cancel the function returns
    /// `Ok(())` (no error surfaced). On failure the error is surfaced
    /// via the Settings error banner (if open) and the status hint
    /// (visible when Settings is closed).
    pub(super) fn import_config_interactive(
        &mut self,
    ) -> Result<(), crate::macos_file_dialog::FilePanelError> {
        // Step 1: get MainThreadMarker. The event loop runs on the main
        // thread, so this should always succeed.
        let Some(mtm) = MainThreadMarker::new() else {
            return Err(crate::macos_file_dialog::FilePanelError::NotMainThread);
        };

        // Step 2: show the open panel. This blocks until the user picks
        // a file or cancels.
        let picked = crate::macos_file_dialog::pick_config_import_path(mtm)?;
        let Some(import_path) = picked else {
            // Cancel — no error, no side effect.
            return Ok(());
        };

        // Step 3: resolve the current config path. We don't write to
        // the picked file; we write to the *current* config path.
        let Some(config_path) = weft_core::config::Config::config_path() else {
            tracing::warn!("import: no config path resolved (HOME/XDG unset)");
            self.surface_config_error("Import failed: no config path (HOME/XDG unset)");
            return Ok(());
        };

        // Step 4: call the atomic import. On any failure the current
        // file is untouched (the transfer API guarantees this).
        match weft_core::config::import_config_document(&import_path, &config_path) {
            Ok(loaded) => {
                // Step 5: apply to runtime. set_loaded updates source +
                // effective + fingerprint atomically; apply_config
                // rebuilds renderer state (theme/font/etc.) and reseeds
                // all panes.
                let effective = loaded.effective.clone();
                self.config_state.set_loaded(loaded);
                self.apply_config(effective);
                self.sync_settings_after_profile_change();
                info!(?import_path, "config imported");
                self.clear_config_error();
            }
            Err(e) => {
                tracing::warn!(error = %e, ?import_path, "config import failed");
                self.surface_config_error(&format!("Import failed: {e}"));
            }
        }
        Ok(())
    }

    /// v1.5.2: Show NSSavePanel, then `export_config_document` the
    /// source config to the picked path. Cancel returns `Ok(())`.
    /// Failures are surfaced via the Settings error banner / status hint.
    pub(super) fn export_config_interactive(
        &mut self,
    ) -> Result<(), crate::macos_file_dialog::FilePanelError> {
        let Some(mtm) = MainThreadMarker::new() else {
            return Err(crate::macos_file_dialog::FilePanelError::NotMainThread);
        };

        let picked = crate::macos_file_dialog::pick_config_export_path(mtm)?;
        let Some(dest) = picked else {
            return Ok(());
        };

        // Source path: the current config file, if known. `export_config_document`
        // copies the raw file verbatim (preserving comments + unknown fields)
        // when the source_path exists, and falls back to canonical TOML
        // serialization when it doesn't.
        let source_path = weft_core::config::Config::config_path();
        let source = self.config_state.source().clone();

        match weft_core::config::export_config_document(source_path.as_deref(), &source, &dest) {
            Ok(()) => {
                info!(?dest, "config exported");
                self.clear_config_error();
            }
            Err(e) => {
                tracing::warn!(error = %e, ?dest, "config export failed");
                self.surface_config_error(&format!("Export failed: {e}"));
            }
        }
        Ok(())
    }

    /// v1.5.2: Surface a config-related error in the Settings error banner
    /// (if Settings is open). When Settings is closed, the error is only
    /// logged — the v1.5.3 batch will add the status hint.
    fn surface_config_error(&mut self, message: &str) {
        if self.settings.open {
            self.settings.error = Some(message.to_string());
            self.request_redraw();
        }
        // TODO(v1.5.3): surface in status hint when Settings is closed.
    }

    /// v1.5.2: Clear any prior config error after a successful transfer.
    fn clear_config_error(&mut self) {
        if self.settings.open && self.settings.error.is_some() {
            self.settings.error = None;
            self.request_redraw();
        }
    }
}
