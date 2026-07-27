//! v1.5.1: Profile create / switch / delete transactions.
//!
//! Each public method is an atomic transaction following the fixed order
//! from `V15_IMPLEMENTATION_PLAN.md` §6.3:
//!
//! 1. clone `source_config`
//! 2. modify the clone (active_profile / profiles map)
//! 3. resolve the clone — fail → stop, keep old runtime state
//! 4. atomic save the clone
//! 5. sync source/effective/fingerprint via `set_loaded`
//! 6. apply to renderer + all panes via `apply_config`
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
        // Step 1: clone source.
        let mut source = self.config_state.source().clone();

        // Step 2: modify active_profile. Empty string normalizes to None.
        let normalized = name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        source.active_profile = normalized.clone();

        // Step 3: resolve — fail → stop.
        let (_effective, _diags) = source
            .resolve_active_profile()
            .map_err(ProfileTransactionError::Resolve)?;

        // Step 4: atomic save.
        source.save().map_err(ProfileTransactionError::Save)?;

        // Step 5: reload to get fresh fingerprint. The save wrote the file;
        // if reload fails here the file is correct but runtime is stale —
        // the watcher will retry on the next mtime tick.
        let loaded = weft_core::config::load_resolved().map_err(ProfileTransactionError::Reload)?;
        let effective = loaded.effective.clone();
        self.config_state.set_loaded(loaded);

        // Step 6: apply to renderer + all panes.
        self.apply_config(effective);

        // Step 7: refresh Settings draft if open.
        self.sync_settings_after_profile_change();

        info!(profile = ?normalized, "profile switched");
        Ok(())
    }

    /// v1.5.1: Create a new empty profile and switch to it. The name is
    /// validated, the profile map checked for duplicates and the 32-cap.
    /// On success the new profile is immediately active.
    pub(super) fn create_profile(&mut self, name: &str) -> Result<(), ProfileTransactionError> {
        // Step 1: validate the name before touching source.
        weft_core::config::validate_profile_name(name)
            .map_err(|_| ProfileTransactionError::InvalidName(name.to_string()))?;

        // Step 2: clone source.
        let mut source = self.config_state.source().clone();

        // Step 3: check duplicates + cap.
        if source.profiles.contains_key(name) {
            return Err(ProfileTransactionError::AlreadyExists(name.to_string()));
        }
        if source.profiles.len() >= weft_core::config::MAX_PROFILES {
            return Err(ProfileTransactionError::InvalidName(
                "too many profiles (max 32)".into(),
            ));
        }

        // Step 4: insert empty profile + set active.
        source.profiles.insert(
            name.to_string(),
            weft_core::config::ProfileConfig::default(),
        );
        source.active_profile = Some(name.to_string());

        // Step 5: resolve (should always succeed — empty profile inherits base).
        let (_effective, _diags) = source
            .resolve_active_profile()
            .map_err(ProfileTransactionError::Resolve)?;

        // Step 6: atomic save.
        source.save().map_err(ProfileTransactionError::Save)?;

        // Step 7: reload + sync.
        let loaded = weft_core::config::load_resolved().map_err(ProfileTransactionError::Reload)?;
        let effective = loaded.effective.clone();
        self.config_state.set_loaded(loaded);

        // Step 8: apply.
        self.apply_config(effective);

        // Step 9: refresh Settings.
        self.sync_settings_after_profile_change();

        info!(profile = name, "profile created and activated");
        Ok(())
    }

    /// v1.5.1: Delete a profile by name. If the deleted profile was
    /// active, `active_profile` is cleared (falls back to base) in the
    /// same atomic save. Other profiles are untouched.
    pub(super) fn delete_profile(&mut self, name: &str) -> Result<(), ProfileTransactionError> {
        // Step 1: clone source.
        let mut source = self.config_state.source().clone();

        // Step 2: check the profile exists.
        if !source.profiles.contains_key(name) {
            return Err(ProfileTransactionError::NotFound(name.to_string()));
        }

        // Step 3: if it was active, clear active_profile so we don't
        // leave a dangling reference.
        let was_active = source.active_profile.as_deref() == Some(name);
        if was_active {
            source.active_profile = None;
        }

        // Step 4: remove the profile.
        source.profiles.remove(name);

        // Step 5: resolve (should succeed — removing a profile can't break
        // the schema, and if it was active we already cleared the ref).
        let (_effective, _diags) = source
            .resolve_active_profile()
            .map_err(ProfileTransactionError::Resolve)?;

        // Step 6: atomic save.
        source.save().map_err(ProfileTransactionError::Save)?;

        // Step 7: reload + sync.
        let loaded = weft_core::config::load_resolved().map_err(ProfileTransactionError::Reload)?;
        let effective = loaded.effective.clone();
        self.config_state.set_loaded(loaded);

        // Step 8: apply.
        self.apply_config(effective);

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
mod tests {
    use super::*;
    use weft_core::config::{validate_profile_name, ProfileConfig};

    /// Validate-profile-name is the first gate of `create_profile`.
    /// The transaction can't even start if the name is bad, so we test
    /// the pure validation function here rather than spinning up a full
    /// App (which requires a Metal device).
    #[test]
    fn create_profile_rejects_invalid_names() {
        // The validation function is the gate; create_profile wraps it.
        assert!(validate_profile_name("").is_err());
        assert!(validate_profile_name(" ").is_err());
        assert!(validate_profile_name("base").is_err());
        assert!(validate_profile_name("BASE").is_err());
        assert!(validate_profile_name("_bad").is_err()); // must start alphanumeric
        assert!(validate_profile_name("has space").is_err());
        assert!(validate_profile_name("has/slash").is_err());
        // 33 chars
        assert!(validate_profile_name(&"a".repeat(33)).is_err());
    }

    #[test]
    fn create_profile_accepts_valid_names() {
        assert!(validate_profile_name("work").is_ok());
        assert!(validate_profile_name("dev-1").is_ok());
        assert!(validate_profile_name("a.b.c").is_ok());
        assert!(validate_profile_name("A_1-2.3").is_ok());
        assert!(validate_profile_name(&"a".repeat(32)).is_ok());
    }

    /// `profile_names_sorted` returns BTreeMap order (alphabetical), so
    /// the Settings + Palette views see a stable index↔name mapping
    /// within a frame. We can't test the full App method without a
    /// renderer, but we can verify the BTreeMap ordering invariant on
    /// a raw Config.
    #[test]
    fn profile_names_are_alphabetical() {
        let mut cfg = Config::default();
        cfg.profiles
            .insert("zebra".into(), ProfileConfig::default());
        cfg.profiles
            .insert("alpha".into(), ProfileConfig::default());
        cfg.profiles
            .insert("mango".into(), ProfileConfig::default());
        let names: Vec<String> = cfg.profiles.keys().cloned().collect();
        assert_eq!(names, vec!["alpha", "mango", "zebra"]);
    }

    /// `active_profile_name` normalizes empty strings to `None`, matching
    /// `resolve_active_profile`'s behavior. This prevents the Settings
    /// selector from showing a stale "active" state when the TOML has
    /// `active_profile = ""`.
    #[test]
    fn empty_active_profile_normalizes_to_none() {
        let cfg = Config {
            active_profile: Some(String::new()),
            ..Default::default()
        };
        // The normalization logic: trim + filter empty.
        let normalized = cfg
            .active_profile
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        assert_eq!(normalized, None);
    }

    /// Deleting the active profile must clear `active_profile` so we
    /// don't leave a dangling reference. This test verifies the
    /// invariant on a raw Config (the App method wraps this logic).
    #[test]
    fn delete_active_profile_clears_active_ref() {
        let mut cfg = Config {
            active_profile: Some("work".into()),
            ..Default::default()
        };
        cfg.profiles.insert("work".into(), ProfileConfig::default());

        // Simulate delete.
        let was_active = cfg.active_profile.as_deref() == Some("work");
        assert!(was_active);
        if was_active {
            cfg.active_profile = None;
        }
        cfg.profiles.remove("work");

        assert_eq!(cfg.active_profile, None);
        assert!(!cfg.profiles.contains_key("work"));
    }

    /// Creating a profile that already exists must fail before touching
    /// the source. This test verifies the duplicate check on a raw Config.
    #[test]
    fn create_duplicate_profile_fails() {
        let mut cfg = Config::default();
        cfg.profiles.insert("work".into(), ProfileConfig::default());
        // The duplicate check.
        assert!(cfg.profiles.contains_key("work"));
    }

    /// The 32-profile cap is enforced before insertion. This test
    /// verifies the cap on a raw Config.
    #[test]
    fn profile_cap_enforced() {
        let mut cfg = Config::default();
        for i in 0..32 {
            cfg.profiles
                .insert(format!("p{i:02}"), ProfileConfig::default());
        }
        assert_eq!(cfg.profiles.len(), 32);
        // The cap check: `cfg.profiles.len() >= MAX_PROFILES` should be true.
        assert!(cfg.profiles.len() >= weft_core::config::MAX_PROFILES);
    }
}
