//! `[sections]` unit tests (split from sections.rs to keep the production
//! file within the 800-line gate; the child module resolves items via the
//! `crate::config::` re-exports, which are the same items `super::` saw
//! when these modules were inline).

#[cfg(test)]
mod paste_config_tests {
    use crate::config::{PasteConfig, PASTE_SIZE_TIERS_KIB};
    #[test]
    fn defaults_confirm_both_risks_at_16kib() {
        let cfg = PasteConfig::default();
        assert!(cfg.confirm_large);
        assert!(cfg.confirm_control_chars);
        assert_eq!(cfg.size_threshold_kib, 16);
    }

    #[test]
    fn missing_section_deserializes_to_defaults() {
        let cfg: PasteConfig = toml::from_str("[paste]").unwrap();
        assert_eq!(cfg, PasteConfig::default());
    }

    #[test]
    fn threshold_tier_list_is_the_six_documented_steps() {
        assert_eq!(PASTE_SIZE_TIERS_KIB, [8, 16, 32, 64, 128, 256]);
        // The default must be one of the cycle steps so the Settings row can
        // round-trip its position.
        assert!(PASTE_SIZE_TIERS_KIB.contains(&PasteConfig::default().size_threshold_kib));
    }
}

#[cfg(test)]
mod blocks_config_tests {
    use crate::config::BlocksConfig;

    /// v1.11.2 X4: default matches the tracker default; section optional.
    #[test]
    fn defaults_to_tracker_retention_cap() {
        assert_eq!(
            BlocksConfig::default().retained_limit,
            crate::blocks::retention::DEFAULT_BLOCKS_RETAINED_LIMIT
        );
        let cfg: BlocksConfig = toml::from_str("[blocks]").unwrap();
        assert_eq!(cfg, BlocksConfig::default());
    }

    #[test]
    fn parses_hand_written_value_including_zero_disable() {
        // Deserialize from the section's own table body.
        let cfg: BlocksConfig = toml::from_str("retained_limit = 500").unwrap();
        assert_eq!(cfg.retained_limit, 500);
        let cfg: BlocksConfig = toml::from_str("retained_limit = 0").unwrap();
        assert_eq!(cfg.retained_limit, 0, "0 = retention disabled");
    }

    /// PLAN_v11217 §3.5 (T4): defaults to 1, parses from TOML; parse does
    /// NOT clamp (that lives in `normalize_blocks` / `set_output_cap`).
    #[test]
    fn output_cap_mib_defaults_and_parses() {
        let default = BlocksConfig::default();
        assert_eq!(
            default.output_cap_mib,
            crate::blocks::OUTPUT_CAP_DEFAULT_MIB
        );
        let cfg: BlocksConfig = toml::from_str("output_cap_mib = 8").unwrap();
        assert_eq!(cfg.output_cap_mib, 8);
        let cfg: BlocksConfig = toml::from_str("[blocks]").unwrap();
        assert_eq!(cfg, default, "section is optional");
    }
}
#[cfg(test)]
mod recovery_mode_tests {
    use crate::config::{Config, RecoveryMode, SessionConfig};

    /// v1.12.19 (T13a): the three canonical spellings round-trip and the
    /// section is optional.
    #[test]
    fn recovery_mode_round_trips_and_section_is_optional() {
        assert_eq!(SessionConfig::default().recovery, RecoveryMode::Ask);
        let cfg: SessionConfig = toml::from_str("[session]").unwrap();
        assert_eq!(cfg.recovery, RecoveryMode::Ask, "missing key → default ask");
        for (raw, expected) in [
            ("ask", RecoveryMode::Ask),
            ("auto", RecoveryMode::Auto),
            ("never", RecoveryMode::Never),
        ] {
            let cfg: SessionConfig = toml::from_str(&format!("recovery = \"{raw}\"")).unwrap();
            assert_eq!(cfg.recovery, expected, "raw = {raw:?}");
            assert_eq!(RecoveryMode::parse(expected.as_str()), expected);
            assert_eq!(expected.as_str(), raw);
        }
    }

    /// Plan review P1 (serde-lowercase derives would abort the WHOLE config
    /// parse on an unknown string): a typo'd recovery value must degrade to
    /// the `Ask` default while every other section still parses.
    #[test]
    fn unknown_recovery_value_degrades_without_killing_the_config() {
        let cfg: Result<Config, _> =
            toml::from_str("[session]\nrecovery = \"someday\"\n\n[blocks]\nretained_limit = 500\n");
        let cfg = cfg.expect("an unknown recovery value must not fail the parse");
        assert_eq!(cfg.session.recovery, RecoveryMode::Ask);
        assert_eq!(
            cfg.blocks.retained_limit, 500,
            "the rest of the document survives"
        );
    }
}

#[cfg(test)]
mod ai_config_tests {
    use crate::config::*;

    #[test]
    fn unconfigured_when_no_provider() {
        let cfg = AiConfig::default();
        assert!(!cfg.is_configured());
        assert_eq!(cfg.provider_kind(), None);
        assert_eq!(cfg.effective_timeout_secs(), 30);
        assert_eq!(cfg.effective_max_tokens(), 4096);
    }

    #[test]
    fn ollama_configured_without_api_key() {
        let cfg = AiConfig {
            provider: Some("ollama".into()),
            ..Default::default()
        };
        assert!(cfg.is_configured());
        assert_eq!(cfg.provider_kind(), Some("ollama"));
    }

    #[test]
    fn openai_no_longer_configured_in_v18() {
        // v1.8: only "ollama" is accepted. Old configs with "openai" /
        // "anthropic" / "custom" should be treated as unconfigured so the
        // user sees a hint to switch rather than a silent breakage.
        let cfg = AiConfig {
            provider: Some("openai".into()),
            api_key: Some("sk-test".into()),
            ..Default::default()
        };
        assert!(!cfg.is_configured());
    }

    #[test]
    fn anthropic_no_longer_configured_in_v18() {
        let cfg = AiConfig {
            provider: Some("anthropic".into()),
            api_key: Some("sk-ant-test".into()),
            ..Default::default()
        };
        assert!(!cfg.is_configured());
    }

    #[test]
    fn custom_no_longer_configured_in_v18() {
        let cfg = AiConfig {
            provider: Some("custom".into()),
            base_url: Some("https://internal.example.com/v1".into()),
            ..Default::default()
        };
        assert!(!cfg.is_configured());
    }

    #[test]
    fn empty_api_key_treated_as_unset() {
        // v1.8: api_key is ignored entirely, but the field is kept for
        // backwards-compat deserialization. Any value is "unset".
        let cfg = AiConfig {
            provider: Some("anthropic".into()),
            api_key: Some("   ".into()),
            ..Default::default()
        };
        assert!(!cfg.is_configured());
    }

    #[test]
    fn defaults_command_generation_on_diagnosis_off() {
        let cfg = AiConfig::default();
        assert!(cfg.enable_command_generation);
        assert!(!cfg.enable_error_diagnosis);
    }
}
