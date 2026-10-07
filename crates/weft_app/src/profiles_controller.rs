//! v1.5.1: Profile create / switch / delete transactions.
//!
//! Each public method is an atomic transaction following the fixed order
//! from `V15_IMPLEMENTATION_PLAN.md` §6.3:
//!
//! 1. clone `source_config`
//! 2. modify the clone (active_profile / profiles map)
//! 3. resolve the clone — fail → stop, keep old runtime state
//! 4. atomic save the clone
//! 5. reload a validated `LoadedConfig`
//! 6. apply against the old runtime, then sync source/effective/fingerprint
//! 7. refresh Settings draft/validation if the panel is open
//! 8. the watcher will later read the same fingerprint and skip
//!
//! Any failure at any step leaves the previous source/effective/runtime
//! untouched. The caller surfaces the error to the Settings panel or the
//! palette.

use super::*;

/// Errors raised by profile transactions (switch / create / delete).
///
/// Each variant carries enough context for the Settings UI to show a
/// useful field error. `Display` is implemented so callers can log the
/// error verbatim.
#[derive(Debug)]
pub(super) enum ProfileTransactionError {
    /// Profile name failed validation (regex / reserved / length).
    InvalidName(String),
    /// A profile with this name already exists (create).
    AlreadyExists(String),
    /// Profile not found (delete / switch).
    NotFound(String),
    /// `active_profile = "x"` but no `[profiles.x]` exists. Non-fatal —
    /// the transaction proceeds with base — but we surface it so the UI
    /// can show a hint. Reserved for future status-hint integration.
    #[allow(dead_code)]
    MissingActiveProfile(String),
    /// `resolve_active_profile` rejected the config (bad name, too many).
    Resolve(weft_core::config::ProfileError),
    /// The config file couldn't be saved (disk full, permission, etc.).
    Save(weft_core::config::ConfigSaveError),
    /// The post-save reload failed (file disappeared, re-parse error).
    /// The save itself succeeded, so the file on disk is correct; the
    /// runtime just couldn't pick up the new fingerprint in this call.
    Reload(weft_core::config::ConfigLoadError),
}

impl std::fmt::Display for ProfileTransactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(name) => write!(
                f,
                "invalid profile name {:?}: must match [A-Za-z0-9][A-Za-z0-9._-]{{0,31}}",
                name
            ),
            Self::AlreadyExists(name) => {
                write!(f, "profile {:?} already exists", name)
            }
            Self::NotFound(name) => write!(f, "profile {:?} not found", name),
            Self::MissingActiveProfile(name) => {
                write!(f, "active profile {:?} not found; using base", name)
            }
            Self::Resolve(e) => write!(f, "profile resolve failed: {e}"),
            Self::Save(e) => write!(f, "config save failed: {e}"),
            Self::Reload(e) => write!(f, "config reload after save failed: {e}"),
        }
    }
}

impl std::error::Error for ProfileTransactionError {}

#[derive(Clone, Copy, Debug)]
enum ProfileChange<'a> {
    Switch(Option<&'a str>),
    Create(&'a str),
    Delete(&'a str),
}

fn prepare_profile_change(
    source: &Config,
    change: ProfileChange<'_>,
) -> Result<Config, ProfileTransactionError> {
    let mut candidate = source.clone();
    match change {
        ProfileChange::Switch(name) => {
            let normalized = name.map(str::trim).filter(|name| !name.is_empty());
            if let Some(name) = normalized {
                if !candidate.profiles.contains_key(name) {
                    return Err(ProfileTransactionError::NotFound(name.to_string()));
                }
                candidate.active_profile = Some(name.to_string());
            } else {
                candidate.active_profile = None;
            }
        }
        ProfileChange::Create(name) => {
            weft_core::config::validate_profile_name(name)
                .map_err(|_| ProfileTransactionError::InvalidName(name.to_string()))?;
            if candidate.profiles.contains_key(name) {
                return Err(ProfileTransactionError::AlreadyExists(name.to_string()));
            }
            if candidate.profiles.len() >= weft_core::config::MAX_PROFILES {
                return Err(ProfileTransactionError::Resolve(
                    weft_core::config::ProfileError::TooManyProfiles(candidate.profiles.len() + 1),
                ));
            }
            candidate.profiles.insert(
                name.to_string(),
                weft_core::config::ProfileConfig::default(),
            );
            candidate.active_profile = Some(name.to_string());
        }
        ProfileChange::Delete(name) => {
            if !candidate.profiles.contains_key(name) {
                return Err(ProfileTransactionError::NotFound(name.to_string()));
            }
            if candidate.active_profile.as_deref() == Some(name) {
                candidate.active_profile = None;
            }
            candidate.profiles.remove(name);
        }
    }
    candidate
        .resolve_active_profile()
        .map_err(ProfileTransactionError::Resolve)?;
    Ok(candidate)
}

fn run_profile_transaction<F>(
    source: &Config,
    change: ProfileChange<'_>,
    persist_and_reload: F,
) -> Result<weft_core::config::LoadedConfig, ProfileTransactionError>
where
    F: FnOnce(&Config) -> Result<weft_core::config::LoadedConfig, ProfileTransactionError>,
{
    let candidate = prepare_profile_change(source, change)?;
    persist_and_reload(&candidate)
}

fn save_and_reload_profile(
    source: &Config,
) -> Result<weft_core::config::LoadedConfig, ProfileTransactionError> {
    source.save().map_err(ProfileTransactionError::Save)?;
    weft_core::config::load_resolved().map_err(ProfileTransactionError::Reload)
}

pub(super) fn merge_settings_draft(
    source: &Config,
    draft: &Config,
    dirty: weft_core::config::ConfigSectionMask,
) -> Result<Config, ProfileTransactionError> {
    let mut candidate = source.clone();
    let active = source
        .active_profile
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());

    if let Some(name) = active {
        let profile = candidate
            .profiles
            .get_mut(name)
            .ok_or_else(|| ProfileTransactionError::NotFound(name.to_string()))?;
        if dirty.contains(weft_core::config::ConfigSectionMask::FONT) {
            profile.font = Some(draft.font.clone());
        }
        if dirty.contains(weft_core::config::ConfigSectionMask::THEME) {
            profile.theme = Some(draft.theme.clone());
        }
        if dirty.contains(weft_core::config::ConfigSectionMask::WINDOW) {
            profile.window = Some(draft.window.clone());
        }
        if dirty.contains(weft_core::config::ConfigSectionMask::SCROLLBACK) {
            profile.scrollback = Some(draft.scrollback.clone());
        }
        if dirty.contains(weft_core::config::ConfigSectionMask::EDITOR) {
            profile.editor = Some(draft.editor.clone());
        }
        // v1.11.1 (PLAN_v1111 §4.6): paste protection switches follow the
        // editor pattern — full-section override when the Input page was
        // touched (ConfigSectionMask::PASTE).
        if dirty.contains(weft_core::config::ConfigSectionMask::PASTE) {
            profile.paste = Some(draft.paste.clone());
        }
        if dirty.contains(weft_core::config::ConfigSectionMask::LOGO) {
            profile.logo = Some(draft.logo.clone());
        }
        if dirty.contains(weft_core::config::ConfigSectionMask::KEYBINDINGS) {
            profile.keybindings = Some(draft.keybindings.clone());
        }
        // v1.11.5 (PLAN_v1115 §M8): clipboard / notifications are
        // profile-overridable like the other sections.
        if dirty.contains(weft_core::config::ConfigSectionMask::CLIPBOARD) {
            profile.clipboard = Some(draft.clipboard); // Copy
        }
        if dirty.contains(weft_core::config::ConfigSectionMask::NOTIFICATIONS) {
            profile.notifications = Some(draft.notifications); // Copy
        }
        // v1.12.19 (PLAN_v11217 §3.8 T13a.1/T13b): [blocks] Settings rows
        // (Retained limit / Output cap) are profile-overridable — the
        // CLIPBOARD/NOTIFICATIONS precedent. Missing this arm in BOTH the
        // profile and base branches silently drops the user's edit (review
        // P0); guarded by the draft→persist BLOCKS round-trip test below.
        if dirty.contains(weft_core::config::ConfigSectionMask::BLOCKS) {
            profile.blocks = Some(draft.blocks.clone());
        }
    } else {
        if dirty.contains(weft_core::config::ConfigSectionMask::FONT) {
            candidate.font = draft.font.clone();
        }
        if dirty.contains(weft_core::config::ConfigSectionMask::THEME) {
            candidate.theme = draft.theme.clone();
        }
        if dirty.contains(weft_core::config::ConfigSectionMask::WINDOW) {
            candidate.window = draft.window.clone();
        }
        if dirty.contains(weft_core::config::ConfigSectionMask::SCROLLBACK) {
            candidate.scrollback = draft.scrollback.clone();
        }
        if dirty.contains(weft_core::config::ConfigSectionMask::EDITOR) {
            candidate.editor = draft.editor.clone();
        }
        // v1.11.1: base-config paste switches (PLAN_v1111 §4.6).
        if dirty.contains(weft_core::config::ConfigSectionMask::PASTE) {
            candidate.paste = draft.paste.clone();
        }
        if dirty.contains(weft_core::config::ConfigSectionMask::LOGO) {
            candidate.logo = draft.logo.clone();
        }
        if dirty.contains(weft_core::config::ConfigSectionMask::KEYBINDINGS) {
            candidate.keybindings = draft.keybindings.clone();
        }
        // v1.11.5 (PLAN_v1115 §M8): clipboard / notifications base edits.
        if dirty.contains(weft_core::config::ConfigSectionMask::CLIPBOARD) {
            candidate.clipboard = draft.clipboard; // Copy
        }
        if dirty.contains(weft_core::config::ConfigSectionMask::NOTIFICATIONS) {
            candidate.notifications = draft.notifications; // Copy
        }
        // v1.12.19 (PLAN_v11217 §3.8 T13b): [blocks] base edits (the
        // profile branch carries the twin arm above).
        if dirty.contains(weft_core::config::ConfigSectionMask::BLOCKS) {
            candidate.blocks = draft.blocks.clone();
        }
    }

    // v1.8.3: AI config is global only — never written into a profile.
    // `ProfileConfig` has no `ai` field, so we always write to the base
    // config on `candidate` regardless of whether a profile is active.
    if dirty.contains(weft_core::config::ConfigSectionMask::AI) {
        candidate.ai = draft.ai.clone();
    }

    // v1.12.19 (PLAN_v11217 §3.8 T13a.1): [session] recovery is global
    // only too — `ProfileConfig` has no `session` field (a `session`
    // section inside a profile is a schema error), so the Terminal tab's
    // Session recovery row always writes the base config (AI-mask
    // precedent).
    if dirty.contains(weft_core::config::ConfigSectionMask::SESSION) {
        candidate.session = draft.session;
    }

    // v1.13.0 (PLAN_v1.13.0_SPARKLE §WP2): [update] check tier is global
    // only too — `ProfileConfig` has no `update` field (SESSION/AI
    // precedent), so the Update tab always writes the base config.
    if dirty.contains(weft_core::config::ConfigSectionMask::UPDATE) {
        candidate.update = draft.update;
    }

    candidate
        .resolve_active_profile()
        .map_err(ProfileTransactionError::Resolve)?;
    Ok(candidate)
}

pub(super) fn persist_settings_draft(
    source: &Config,
    draft: &Config,
    dirty: weft_core::config::ConfigSectionMask,
) -> Result<weft_core::config::LoadedConfig, ProfileTransactionError> {
    run_settings_draft_transaction(source, draft, dirty, save_and_reload_profile)
}

fn run_settings_draft_transaction<F>(
    source: &Config,
    draft: &Config,
    dirty: weft_core::config::ConfigSectionMask,
    persist_and_reload: F,
) -> Result<weft_core::config::LoadedConfig, ProfileTransactionError>
where
    F: FnOnce(&Config) -> Result<weft_core::config::LoadedConfig, ProfileTransactionError>,
{
    let candidate = merge_settings_draft(source, draft, dirty)?;
    persist_and_reload(&candidate)
}

impl App {
    /// v1.5.1: Switch to a profile by name. `None` switches to base
    /// (clears `active_profile`). The transaction is atomic — on any
    /// failure the previous runtime state is untouched.
    ///
    /// Called from the Settings profile selector and the Command Palette
    /// `Profile` entry.
    pub(super) fn switch_profile(
        &mut self,
        name: Option<&str>,
    ) -> Result<(), ProfileTransactionError> {
        let normalized = name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let loaded = run_profile_transaction(
            self.config_state.source(),
            ProfileChange::Switch(name),
            save_and_reload_profile,
        )?;
        self.commit_loaded_config(loaded);

        // Step 7: refresh Settings draft if open.
        self.sync_settings_after_profile_change();

        info!(profile = ?normalized, "profile switched");
        Ok(())
    }

    /// v1.5.1: Create a new empty profile and switch to it. The name is
    /// validated, the profile map checked for duplicates and the 32-cap.
    /// On success the new profile is immediately active.
    pub(super) fn create_profile(&mut self, name: &str) -> Result<(), ProfileTransactionError> {
        let loaded = run_profile_transaction(
            self.config_state.source(),
            ProfileChange::Create(name),
            save_and_reload_profile,
        )?;
        self.commit_loaded_config(loaded);

        // Step 9: refresh Settings.
        self.sync_settings_after_profile_change();

        info!(profile = name, "profile created and activated");
        Ok(())
    }

    /// v1.5.1: Delete a profile by name. If the deleted profile was
    /// active, `active_profile` is cleared (falls back to base) in the
    /// same atomic save. Other profiles are untouched.
    pub(super) fn delete_profile(&mut self, name: &str) -> Result<(), ProfileTransactionError> {
        let was_active = self.config_state.source().active_profile.as_deref() == Some(name);
        let loaded = run_profile_transaction(
            self.config_state.source(),
            ProfileChange::Delete(name),
            save_and_reload_profile,
        )?;
        self.commit_loaded_config(loaded);

        // Step 9: refresh Settings.
        self.sync_settings_after_profile_change();

        info!(profile = name, was_active, "profile deleted");
        Ok(())
    }

    /// v1.5.1: After a profile transaction, refresh the Settings draft
    /// so the panel reflects the new effective config. If Settings is
    /// closed, this is a no-op.
    ///
    /// The draft is re-seeded from `config_state.config` (effective) so
    /// any in-progress edits are discarded — switching profiles mid-edit
    /// would otherwise mix edits from two different profile contexts.
    ///
    /// v1.5.2: `pub(super)` so `transfer_controller` can reuse the same
    /// Settings refresh after an import transaction.
    pub(super) fn sync_settings_after_profile_change(&mut self) {
        if !self.settings.open {
            return;
        }
        // Re-seed draft from the new effective config. Uncommitted edits
        // are lost — this is intentional: a profile switch is a context
        // change, not a merge.
        self.settings.open_from(&self.config_state.config);
        self.refresh_settings_validation();
    }

    /// v1.5.1: Sorted list of profile names for Settings + Palette
    /// display. Uses `BTreeMap` iteration order (alphabetical), so the
    /// index↔name mapping is stable within a frame.
    ///
    /// Returns `Vec<String>` so callers that need owned names (e.g. the
    /// palette results list) can use them directly.
    pub(super) fn profile_names_sorted(&self) -> Vec<String> {
        self.config_state
            .source()
            .profiles
            .keys()
            .cloned()
            .collect()
    }

    /// v1.5.1: The active profile name, or `None` for base. Empty
    /// strings normalize to `None` (matching `resolve_active_profile`).
    pub(super) fn active_profile_name(&self) -> Option<&str> {
        self.config_state
            .source()
            .active_profile
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// v1.5.1: Handle a click on profile entry `i` in the Settings toolbar.
    /// Index 0 = "Base" (switch to no profile); 1.. = the sorted profile
    /// names. Out-of-bounds is a no-op (the view may have shrunk between
    /// the hit test and this dispatch — §6.4 "profile index hit test
    /// 越界安全").
    pub(super) fn handle_profile_entry_click(&mut self, i: usize) {
        if i == 0 {
            // "Base" — switch to no active profile.
            if self.active_profile_name().is_some() {
                if let Err(e) = self.switch_profile(None) {
                    warn!(error = %e, "Settings: switch to Base failed");
                    self.settings.error = Some(format!("Switch to Base failed: {e}"));
                }
            }
            return;
        }
        let names = self.profile_names_sorted();
        let Some(name) = names.get(i - 1) else {
            // Out of bounds — no-op.
            return;
        };
        // No-op if already active.
        if self.active_profile_name() == Some(name.as_str()) {
            return;
        }
        if let Err(e) = self.switch_profile(Some(name.as_str())) {
            warn!(error = %e, "Settings: switch to profile failed");
            self.settings.error = Some(format!("Switch to {name} failed: {e}"));
        }
    }

    /// v1.5.1: Handle a click on the "+" (New) button. Creates a new
    /// profile with a default name `profile-N` (where N is the smallest
    /// unused integer ≥ 1) and switches to it. The name is validated by
    /// `create_profile`; on failure the error is shown in the Settings
    /// error banner.
    ///
    /// TODO: a text-input banner for custom names (deferred to keep the
    /// Settings controller within its architecture-gate budget). The
    /// palette doesn't help here — it closes on activation.
    pub(super) fn handle_profile_create_click(&mut self) {
        // Find the smallest unused `profile-N` name.
        let existing = self.profile_names_sorted();
        let mut n = 1;
        let name = loop {
            let candidate = format!("profile-{n}");
            if !existing.contains(&candidate) {
                break candidate;
            }
            n += 1;
        };
        if let Err(e) = self.create_profile(&name) {
            warn!(error = %e, "Settings: create profile failed");
            self.settings.error = Some(format!("Create profile failed: {e}"));
        }
    }

    /// v1.5.1: Handle a click on the "−" (Delete) button. Implements a
    /// two-click confirmation: the first click sets a pending-delete
    /// state (shown as an error banner); the second click within the
    /// same Settings session confirms. Any other click cancels.
    ///
    /// The pending-delete state is stored in `settings.error` so it
    /// surfaces through the existing error banner UI without new state.
    pub(super) fn handle_profile_delete_click(&mut self) {
        let Some(active) = self.active_profile_name().map(str::to_owned) else {
            // No active profile — nothing to delete.
            self.settings.error = Some("No active profile to delete".into());
            return;
        };
        // Check if there's a pending delete for this profile.
        let pending_msg = format!("Click − again to confirm delete of '{active}'");
        if self.settings.error.as_deref() == Some(pending_msg.as_str()) {
            // Confirm: delete the active profile.
            if let Err(e) = self.delete_profile(&active) {
                warn!(error = %e, "Settings: delete profile failed");
                self.settings.error = Some(format!("Delete {active} failed: {e}"));
            } else {
                self.settings.error = None;
            }
        } else {
            // First click — set pending-delete state.
            self.settings.error = Some(pending_msg);
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "profiles_controller/tests.rs"]
mod tests;
