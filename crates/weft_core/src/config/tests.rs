//! Inline tests for `Config`, extracted from `mod.rs` to keep the
//! production file under the 800-line architecture gate. See `mod.rs`
//! (where `#[cfg(test)] #[path = "config/tests.rs"] mod tests;` declares
//! this module).

use super::*;
/// Generates a unique temp path for a test artifact. Combines a tag,
/// process id, monotonic counter, and nanosecond timestamp so parallel
/// tests (even within the same process) never collide on the same file
/// or directory name.
fn temp_path(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "weft-config-{tag}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .as_nanos(),
        id
    ))
}

/// Serializes tests that mutate `XDG_CONFIG_HOME` / `WEFT_CONFIG` / `HOME` —
/// env vars are process-global, so parallel tests that touch the same var
/// would clobber each other's values. The original values are saved and
/// restored around `f` to keep tests hermetic.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_xdg_config<F: FnOnce(&std::path::Path)>(tmp: &std::path::Path, f: F) {
    let _guard = ENV_LOCK.lock().unwrap();
    let old = std::env::var_os("XDG_CONFIG_HOME");
    std::fs::create_dir_all(tmp).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", tmp);
    f(tmp);
    match old {
        Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
}

/// v1.5.3: Run `f` with `WEFT_CONFIG` (and optionally `XDG_CONFIG_HOME` /
/// `HOME`) temporarily set to the given values, then restore the originals.
///
/// Holds `ENV_LOCK` for the whole call so parallel env-mutating tests in
/// this crate stay hermetic. The lock is also taken by `with_xdg_config`,
/// so the two helpers never overlap.
fn with_weft_config<F: FnOnce()>(
    weft_config: Option<&std::ffi::OsStr>,
    xdg: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    f: F,
) {
    let _guard = ENV_LOCK.lock().unwrap();
    let old_weft = std::env::var_os("WEFT_CONFIG");
    let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
    let old_home = std::env::var_os("HOME");

    match weft_config {
        Some(v) => std::env::set_var("WEFT_CONFIG", v),
        None => std::env::remove_var("WEFT_CONFIG"),
    }
    match xdg {
        Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
    match home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }

    f();

    match old_weft {
        Some(v) => std::env::set_var("WEFT_CONFIG", v),
        None => std::env::remove_var("WEFT_CONFIG"),
    }
    match old_xdg {
        Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
    match old_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
}

#[test]
fn defaults_are_dark_menlo_10000() {
    let c = Config::default();
    assert_eq!(c.theme.name, "weft-warm");
    assert_eq!(c.theme.minimum_contrast, 7.0);
    assert_eq!(c.font.family, "Menlo");
    assert_eq!(c.font.size, 14.0);
    assert_eq!(c.scrollback.lines, 10_000);
    assert_eq!(c.window.width, 800);
    // L4 window fields: opaque by default, no content padding.
    assert_eq!(c.window.opacity, 1.0);
    assert_eq!(c.window.padding_x, 0);
    assert_eq!(c.window.padding_y, 0);
}

#[test]
fn empty_toml_uses_defaults() {
    let c: Config = toml::from_str("").unwrap();
    assert_eq!(c.font.family, "Menlo");
    assert_eq!(c.theme.name, "weft-warm");
}

#[test]
fn partial_config_merges_defaults() {
    let toml_text = r#"
[font]
size = 18.0

[theme]
name = "weft-light"
"#;
    let c: Config = toml::from_str(toml_text).unwrap();
    assert_eq!(c.font.size, 18.0);
    assert_eq!(c.font.family, "Menlo"); // default retained
    assert_eq!(c.theme.name, "weft-light");
    assert_eq!(c.theme.minimum_contrast, 7.0);
    assert_eq!(c.scrollback.lines, 10_000); // default retained
}

#[test]
fn parse_hex_rrggbb() {
    assert_eq!(parse_hex("#ff8800"), Some(Color::rgb(255, 136, 0)));
    assert_eq!(parse_hex("1a2b3c"), Some(Color::rgb(0x1a, 0x2b, 0x3c)));
}

#[test]
fn parse_hex_short_and_alpha() {
    assert_eq!(parse_hex("#f0a"), Some(Color::rgb(255, 0, 170)));
    let c = parse_hex("#80808080").unwrap();
    assert_eq!((c.r, c.g, c.b, c.a), (0x80, 0x80, 0x80, 0x80));
}

#[test]
fn parse_hex_rejects_garbage() {
    assert_eq!(parse_hex("nope"), None);
    assert_eq!(parse_hex("#12345"), None);
}

#[test]
fn theme_resolve_applies_overrides() {
    let cfg = ThemeConfig {
        name: "weft-dark".into(), // legacy alias → resolves to weft-warm
        foreground: Some("#abcdef".into()),
        palette: vec!["#112233".into(), "#445566".into()],
        ..Default::default()
    };
    let theme = Theme::resolve(&cfg);
    assert_eq!(theme.foreground, Color::rgb(0xab, 0xcd, 0xef));
    assert_eq!(theme.palette[0], Color::rgb(0x11, 0x22, 0x33));
    assert_eq!(theme.palette[1], Color::rgb(0x44, 0x55, 0x66));
    // v0.8: weft-dark alias resolves to weft-warm (amber accent #d4a574).
    assert_eq!(theme.accent, Color::rgb(0xd4, 0xa5, 0x74));
}

#[test]
fn weft_warm_default_has_warm_palette() {
    // v0.8 visual identity: warm brown bg + amber accent.
    let t = Theme::weft_warm();
    assert_eq!(t.background, Color::rgb(0x22, 0x1c, 0x18)); // warm brown
    assert_eq!(t.accent, Color::rgb(0xd4, 0xa5, 0x74)); // amber
    assert_eq!(t.accent_dim, Color::rgb(0x7a, 0x6a, 0x58)); // warm gray
                                                            // Syntax: all 9 colors distinct from background.
    let bg = t.background;
    for c in [
        t.syntax.command,
        t.syntax.flag,
        t.syntax.path,
        t.syntax.string,
        t.syntax.number,
        t.syntax.variable,
        t.syntax.operator,
        t.syntax.comment,
        t.syntax.default,
    ] {
        assert_ne!(c, bg, "syntax color must differ from background");
    }
    // flag == accent (design intent: flags carry the signature color).
    assert_eq!(t.syntax.flag, t.accent);
    // comment == accent_dim (Quiet: comments recede like dim chrome).
    assert_eq!(t.syntax.comment, t.accent_dim);
    // default == foreground.
    assert_eq!(t.syntax.default, t.foreground);
}

#[test]
fn theme_unknown_name_falls_back_to_dark() {
    let cfg = ThemeConfig {
        name: "nonsense".into(),
        ..Default::default()
    };
    assert_eq!(Theme::resolve(&cfg), Theme::weft_dark());
}

#[test]
fn warp_theme_resolves_by_name() {
    // v0.9 W2+: "warp" and "warp-dark" both resolve to the Warp dark theme.
    let cfg = ThemeConfig {
        name: "warp".into(),
        ..Default::default()
    };
    let theme = Theme::resolve(&cfg);
    assert_eq!(theme, Theme::warp_dark());
    // Also test the dashed alias.
    let cfg2 = ThemeConfig {
        name: "warp-dark".into(),
        ..Default::default()
    };
    assert_eq!(Theme::resolve(&cfg2), Theme::warp_dark());
}

#[test]
fn warp_theme_has_coral_accent_and_distinct_syntax() {
    // v0.9 W2+: Warp's signature coral accent + 9 distinct syntax colors.
    let t = Theme::warp_dark();
    // Coral accent (#ff5d38) is the Warp signature.
    assert_eq!(t.accent, Color::rgb(0xff, 0x5d, 0x38));
    // flag == accent (consistent with weft_warm's design intent).
    assert_eq!(t.syntax.flag, t.accent);
    // default == foreground.
    assert_eq!(t.syntax.default, t.foreground);
    // All 9 syntax colors distinct from background (must be visible).
    let bg = t.background;
    for c in [
        t.syntax.command,
        t.syntax.flag,
        t.syntax.path,
        t.syntax.string,
        t.syntax.number,
        t.syntax.variable,
        t.syntax.operator,
        t.syntax.comment,
        t.syntax.default,
    ] {
        assert_ne!(c, bg, "syntax color must differ from background");
    }
}

#[test]
fn warp_theme_supports_inline_overrides() {
    // v0.9 W2+: inline overrides apply on top of the warp base, just like
    // weft-warm. Verifies resolve_named() applies cfg overrides to warp.
    let cfg = ThemeConfig {
        name: "warp".into(),
        accent: Some("#00ff00".into()),
        ..Default::default()
    };
    let theme = Theme::resolve(&cfg);
    assert_eq!(theme.accent, Color::rgb(0x00, 0xff, 0x00));
    // Background is still the warp default (override only touched accent).
    assert_eq!(theme.background, Color::rgb(0x1b, 0x1b, 0x28));
}

#[test]
fn classic_themes_resolve_by_name() {
    // v0.9 W2+: Dracula / Solarized Dark / Gruvbox Dark resolve by name
    // and match their constructors.
    for (name, expected) in [
        ("dracula", Theme::dracula()),
        ("solarized-dark", Theme::solarized_dark()),
        ("solarized_dark", Theme::solarized_dark()),
        ("solarized", Theme::solarized_dark()),
        ("gruvbox-dark", Theme::gruvbox_dark()),
        ("gruvbox_dark", Theme::gruvbox_dark()),
        ("gruvbox", Theme::gruvbox_dark()),
    ] {
        let cfg = ThemeConfig {
            name: name.into(),
            ..Default::default()
        };
        assert_eq!(Theme::resolve(&cfg), expected, "name = {name}");
    }
}

#[test]
fn classic_themes_have_distinct_accent_and_visible_syntax() {
    // v0.9 W2+: each classic theme has a signature accent and all syntax
    // colors differ from the background (must be visible).
    for (name, theme) in [
        ("dracula", Theme::dracula()),
        ("solarized-dark", Theme::solarized_dark()),
        ("gruvbox-dark", Theme::gruvbox_dark()),
    ] {
        // accent must differ from background (otherwise it's invisible).
        assert_ne!(
            theme.accent, theme.background,
            "{name}: accent must differ from background"
        );
        // every syntax color must differ from background.
        let bg = theme.background;
        for c in [
            theme.syntax.command,
            theme.syntax.flag,
            theme.syntax.path,
            theme.syntax.string,
            theme.syntax.number,
            theme.syntax.variable,
            theme.syntax.operator,
            theme.syntax.comment,
            theme.syntax.default,
        ] {
            assert_ne!(
                c, bg,
                "{name}: syntax color {c:?} must differ from background"
            );
        }
    }
}

#[test]
fn dracula_signature_colors() {
    // v0.9 W2+: verify Dracula's signature palette values.
    let t = Theme::dracula();
    assert_eq!(t.background, Color::rgb(0x28, 0x2a, 0x36));
    assert_eq!(t.foreground, Color::rgb(0xf8, 0xf8, 0xf2));
    assert_eq!(t.accent, Color::rgb(0xbd, 0x93, 0xf9)); // purple
    assert_eq!(t.syntax.flag, Color::rgb(0xff, 0x79, 0xc6)); // pink
}

#[test]
fn solarized_signature_colors() {
    // v0.9 W2+: verify Solarized Dark's signature base03/base1/blue accent.
    let t = Theme::solarized_dark();
    assert_eq!(t.background, Color::rgb(0x00, 0x2b, 0x36)); // base03
    assert_eq!(t.foreground, Color::rgb(0x93, 0xa1, 0xa1)); // base1
    assert_eq!(t.accent, Color::rgb(0x26, 0x8b, 0xd2)); // blue
}

#[test]
fn gruvbox_signature_colors() {
    // v0.9 W2+: verify Gruvbox Dark's signature bg/fg/orange accent.
    let t = Theme::gruvbox_dark();
    assert_eq!(t.background, Color::rgb(0x28, 0x28, 0x28));
    assert_eq!(t.foreground, Color::rgb(0xeb, 0xdb, 0xb2));
    assert_eq!(t.accent, Color::rgb(0xfe, 0x80, 0x19)); // orange
}

#[test]
fn community_themes_resolve_by_name() {
    // v0.9 W2+: Nord / Tokyo Night / Catppuccin / One Dark / Monokai Pro.
    for (name, expected) in [
        ("nord", Theme::nord()),
        ("tokyo-night", Theme::tokyo_night()),
        ("tokyo_night", Theme::tokyo_night()),
        ("catppuccin", Theme::catppuccin_mocha()),
        ("catppuccin-mocha", Theme::catppuccin_mocha()),
        ("one-dark", Theme::one_dark()),
        ("one_dark", Theme::one_dark()),
        ("onedark", Theme::one_dark()),
        ("monokai-pro", Theme::monokai_pro()),
        ("monokai_pro", Theme::monokai_pro()),
        ("monokai", Theme::monokai_pro()),
    ] {
        let cfg = ThemeConfig {
            name: name.into(),
            ..Default::default()
        };
        assert_eq!(Theme::resolve(&cfg), expected, "name = {name}");
    }
}

#[test]
fn community_themes_have_visible_syntax() {
    // v0.9 W2+: each community theme has accent != bg and all syntax
    // colors differ from background.
    for (name, theme) in [
        ("nord", Theme::nord()),
        ("tokyo-night", Theme::tokyo_night()),
        ("catppuccin", Theme::catppuccin_mocha()),
        ("one-dark", Theme::one_dark()),
        ("monokai-pro", Theme::monokai_pro()),
    ] {
        assert_ne!(
            theme.accent, theme.background,
            "{name}: accent must differ from background"
        );
        let bg = theme.background;
        for c in [
            theme.syntax.command,
            theme.syntax.flag,
            theme.syntax.path,
            theme.syntax.string,
            theme.syntax.number,
            theme.syntax.variable,
            theme.syntax.operator,
            theme.syntax.comment,
            theme.syntax.default,
        ] {
            assert_ne!(c, bg, "{name}: syntax color must differ from background");
        }
    }
}

#[test]
fn nord_signature_colors() {
    let t = Theme::nord();
    assert_eq!(t.background, Color::rgb(0x2e, 0x34, 0x40)); // nord0
    assert_eq!(t.foreground, Color::rgb(0xd8, 0xde, 0xe9)); // nord4
    assert_eq!(t.accent, Color::rgb(0x88, 0xc0, 0xd0)); // nord8 frost
}

#[test]
fn tokyo_night_signature_colors() {
    let t = Theme::tokyo_night();
    assert_eq!(t.background, Color::rgb(0x1a, 0x1b, 0x26));
    assert_eq!(t.foreground, Color::rgb(0xa9, 0xb1, 0xd6));
    assert_eq!(t.accent, Color::rgb(0x7a, 0xa2, 0xf7)); // blue
}

#[test]
fn catppuccin_signature_colors() {
    let t = Theme::catppuccin_mocha();
    assert_eq!(t.background, Color::rgb(0x1e, 0x1e, 0x2e)); // base
    assert_eq!(t.foreground, Color::rgb(0xcd, 0xd6, 0xf4)); // text
    assert_eq!(t.accent, Color::rgb(0xcb, 0xa6, 0xf7)); // mauve
}

#[test]
fn one_dark_signature_colors() {
    let t = Theme::one_dark();
    assert_eq!(t.background, Color::rgb(0x28, 0x2c, 0x34));
    assert_eq!(t.foreground, Color::rgb(0xab, 0xb2, 0xbf));
    assert_eq!(t.accent, Color::rgb(0x61, 0xaf, 0xef)); // blue
}

#[test]
fn monokai_pro_signature_colors() {
    let t = Theme::monokai_pro();
    assert_eq!(t.background, Color::rgb(0x2d, 0x2a, 0x2e));
    assert_eq!(t.foreground, Color::rgb(0xfc, 0xfc, 0xfa));
    assert_eq!(t.accent, Color::rgb(0xff, 0xd8, 0x66)); // yellow
}

#[test]
fn load_theme_from_toml_file() {
    // v0.9 W2+: load a custom theme from a .toml file. Uses a unique
    // temp dir (process id + counter) to avoid parallel-test collisions.
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("weft-theme-test-{id}-toml"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("custom.toml"),
        "foreground = \"#abcdef\"\nbackground = \"#112233\"\naccent = \"#ff0000\"\n",
    )
    .unwrap();

    let theme = Theme::load_from_dir(&dir, "custom").expect("should load custom.toml");
    assert_eq!(theme.foreground, Color::rgb(0xab, 0xcd, 0xef));
    assert_eq!(theme.background, Color::rgb(0x11, 0x22, 0x33));
    assert_eq!(theme.accent, Color::rgb(0xff, 0x00, 0x00));
    // Unspecified fields (cursor, syntax, palette) inherit from weft_warm base.
    let warm = Theme::weft_warm();
    assert_eq!(theme.cursor, warm.cursor);
    assert_eq!(theme.syntax.command, warm.syntax.command);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_theme_from_yaml_file() {
    // v0.9 W2+: load a custom theme from a .yaml file.
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("weft-theme-test-{id}-yaml"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("custom.yaml"),
        "foreground: \"#abcdef\"\nbackground: \"#112233\"\naccent: \"#ff0000\"\n",
    )
    .unwrap();

    let theme = Theme::load_from_dir(&dir, "custom").expect("should load custom.yaml");
    assert_eq!(theme.foreground, Color::rgb(0xab, 0xcd, 0xef));
    assert_eq!(theme.background, Color::rgb(0x11, 0x22, 0x33));
    assert_eq!(theme.accent, Color::rgb(0xff, 0x00, 0x00));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_theme_missing_file_returns_none() {
    // v0.9 W2+: when no file matches, returns None (falls back to default).
    let dir = temp_path("theme-nonexistent");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let result = Theme::load_from_dir(&dir, "does-not-exist");
    assert!(result.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_theme_with_palette_override() {
    // v0.9 W2+: palette array in theme file overrides ANSI slots.
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("weft-theme-test-{id}-palette"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pal.toml"),
        "palette = [\"#000000\", \"#ff0000\", \"#00ff00\"]\n",
    )
    .unwrap();

    let theme = Theme::load_from_dir(&dir, "pal").expect("should load pal.toml");
    assert_eq!(theme.palette[0], Color::rgb(0x00, 0x00, 0x00));
    assert_eq!(theme.palette[1], Color::rgb(0xff, 0x00, 0x00));
    assert_eq!(theme.palette[2], Color::rgb(0x00, 0xff, 0x00));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dark_and_light_differ() {
    assert_ne!(Theme::weft_dark(), Theme::weft_light());
}

#[test]
fn parse_binding_modifiers() {
    let (k, m) = parse_binding("cmd+shift+c").unwrap();
    assert_eq!(k, KeyCode::Char('c'));
    assert!(m.contains(Modifiers::SUPER));
    assert!(m.contains(Modifiers::SHIFT));
    assert!(!m.contains(Modifiers::CONTROL));
}

#[test]
fn parse_binding_named_key() {
    let (k, _) = parse_binding("shift+page_up").unwrap();
    assert_eq!(k, KeyCode::PageUp);
    let (k, _) = parse_binding("cmd+comma").unwrap();
    assert_eq!(k, KeyCode::Char(','));
    let (k, _) = parse_binding("cmd+f5").unwrap();
    assert_eq!(k, KeyCode::F(5));
}

#[test]
fn parse_binding_rejects_unknown() {
    assert!(parse_binding("win+foobar").is_none());
}

#[test]
fn keybindings_default_has_copy_paste() {
    let kb = KeyBindings::default();
    let copy = kb.lookup(KeyCode::Char('c'), Modifiers::SUPER);
    assert_eq!(copy, Some(Action::Copy));
    assert_eq!(
        kb.lookup(KeyCode::Char('v'), Modifiers::SUPER),
        Some(Action::Paste)
    );
    // v0.9 fix: Cmd+Shift+V also pastes.
    assert_eq!(
        kb.lookup(KeyCode::Char('v'), Modifiers::SUPER | Modifiers::SHIFT),
        Some(Action::Paste)
    );
}

#[test]
fn keybindings_overrides_merge() {
    let mut overrides = HashMap::new();
    overrides.insert("cmd+x".into(), Action::Copy);
    let kb = KeyBindings::from_overrides(&overrides);
    // Override present.
    assert_eq!(
        kb.lookup(KeyCode::Char('x'), Modifiers::SUPER),
        Some(Action::Copy)
    );
    // Default retained.
    assert_eq!(
        kb.lookup(KeyCode::Char('v'), Modifiers::SUPER),
        Some(Action::Paste)
    );
}

#[test]
fn config_keybindings_resolve() {
    let toml_text = r#"
[keybindings]
"cmd+x" = "copy"
"#;
    let c: Config = toml::from_str(toml_text).unwrap();
    let kb = c.keybindings();
    assert_eq!(
        kb.lookup(KeyCode::Char('x'), Modifiers::SUPER),
        Some(Action::Copy)
    );
}

#[test]
fn load_missing_file_is_default() {
    // Point XDG_CONFIG_HOME to a non-existent directory so Config::load()
    // cannot find a user config file, guaranteeing we hit the default path.
    // Without this isolation the test picks up the developer's real
    // config.toml and fails on the font-family assertion.
    // `with_xdg_config` serializes env mutation and restores the original
    // value so parallel tests don't clobber each other's XDG_CONFIG_HOME.
    let tmp = temp_path("load-missing");
    with_xdg_config(&tmp, |_| {
        let c = Config::load();
        assert_eq!(c.font.family, "Menlo");
    });
}

#[test]
fn editor_submit_on_ctrl_enter_parses() {
    let c: Config = toml::from_str("[editor]\nsubmit_on_ctrl_enter = true\n").unwrap();
    assert!(c.editor.submit_on_ctrl_enter);
    // default is false
    let d: Config = toml::from_str("").unwrap();
    assert!(!d.editor.submit_on_ctrl_enter);
}

#[test]
fn editor_smart_select_defaults_on_and_parses_off() {
    let defaults: Config = toml::from_str("").unwrap();
    assert!(defaults.editor.smart_select);
    let disabled: Config = toml::from_str("[editor]\nsmart_select = false\n").unwrap();
    assert!(!disabled.editor.smart_select);
}

#[test]
fn zoom_actions_have_default_keybindings() {
    let kb = KeyBindings::default();
    // Cmd+= → ZoomIn, Cmd+- → ZoomOut, Cmd+0 → ZoomReset.
    assert_eq!(
        kb.lookup(KeyCode::Char('='), Modifiers::SUPER),
        Some(Action::ZoomIn)
    );
    assert_eq!(
        kb.lookup(KeyCode::Char('-'), Modifiers::SUPER),
        Some(Action::ZoomOut)
    );
    assert_eq!(
        kb.lookup(KeyCode::Char('0'), Modifiers::SUPER),
        Some(Action::ZoomReset)
    );
}

#[test]
fn zoom_actions_serde_roundtrip() {
    // The serde rename must match what users would write in config.toml.
    // Action only derives Deserialize (config is read-only), so we
    // round-trip via a serde_json string (matching the rename attribute).
    for (action, name) in [
        (Action::ZoomIn, "zoom_in"),
        (Action::ZoomOut, "zoom_out"),
        (Action::ZoomReset, "zoom_reset"),
    ] {
        let s = format!("\"{name}\"");
        let back: Action = serde_json::from_str(&s).unwrap();
        assert_eq!(back, action, "serde roundtrip failed for {name}");
    }
}

// ── v1.0 S5: [theme.syntax] config override tests ─────────────────

#[test]
fn syntax_config_default_is_all_none() {
    let s = SyntaxConfig::default();
    assert!(s.command.is_none());
    assert!(s.flag.is_none());
    assert!(s.path.is_none());
    assert!(s.string.is_none());
    assert!(s.number.is_none());
    assert!(s.variable.is_none());
    assert!(s.operator.is_none());
    assert!(s.comment.is_none());
    assert!(s.default.is_none());
}

#[test]
fn theme_config_syntax_defaults_none() {
    // ThemeConfig::default() should leave syntax as None (no overrides).
    let cfg = ThemeConfig::default();
    assert!(cfg.syntax.is_none());
}

#[test]
fn syntax_override_parses_from_toml() {
    let toml_text = r##"
[theme]
name = "weft-warm"

[theme.syntax]
command = "#ff0000"
flag = "#00ff00"
path = "#0000ff"
"##;
    let c: Config = toml::from_str(toml_text).unwrap();
    let syn = c.theme.syntax.expect("syntax section should parse");
    assert_eq!(syn.command.as_deref(), Some("#ff0000"));
    assert_eq!(syn.flag.as_deref(), Some("#00ff00"));
    assert_eq!(syn.path.as_deref(), Some("#0000ff"));
    // Unspecified fields are None.
    assert!(syn.string.is_none());
    assert!(syn.number.is_none());
    assert!(syn.comment.is_none());
}

#[test]
fn syntax_override_applies_to_resolved_theme() {
    // v1.0 S5: [theme.syntax] fields override the base theme's
    // SyntaxColors. Specified fields change; unspecified fields retain
    // the base theme's value.
    let warm = Theme::weft_warm();
    let cfg = ThemeConfig {
        name: "weft-warm".into(),
        syntax: Some(SyntaxConfig {
            command: Some("#ff0000".into()),
            flag: Some("#00ff00".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let theme = Theme::resolve(&cfg);
    // Overridden fields.
    assert_eq!(theme.syntax.command, Color::rgb(0xff, 0x00, 0x00));
    assert_eq!(theme.syntax.flag, Color::rgb(0x00, 0xff, 0x00));
    // Unspecified fields inherit from the base theme.
    assert_eq!(theme.syntax.path, warm.syntax.path);
    assert_eq!(theme.syntax.string, warm.syntax.string);
    assert_eq!(theme.syntax.comment, warm.syntax.comment);
    assert_eq!(theme.syntax.default, warm.syntax.default);
}

#[test]
fn syntax_override_all_ten_fields() {
    let cfg = ThemeConfig {
        name: "weft-warm".into(),
        syntax: Some(SyntaxConfig {
            command: Some("#111111".into()),
            flag: Some("#222222".into()),
            path: Some("#333333".into()),
            string: Some("#444444".into()),
            number: Some("#555555".into()),
            variable: Some("#666666".into()),
            operator: Some("#777777".into()),
            comment: Some("#888888".into()),
            argument: Some("#aaaaaa".into()),
            default: Some("#999999".into()),
        }),
        ..Default::default()
    };
    let theme = Theme::resolve(&cfg);
    assert_eq!(theme.syntax.command, Color::rgb(0x11, 0x11, 0x11));
    assert_eq!(theme.syntax.flag, Color::rgb(0x22, 0x22, 0x22));
    assert_eq!(theme.syntax.path, Color::rgb(0x33, 0x33, 0x33));
    assert_eq!(theme.syntax.string, Color::rgb(0x44, 0x44, 0x44));
    assert_eq!(theme.syntax.number, Color::rgb(0x55, 0x55, 0x55));
    assert_eq!(theme.syntax.variable, Color::rgb(0x66, 0x66, 0x66));
    assert_eq!(theme.syntax.operator, Color::rgb(0x77, 0x77, 0x77));
    assert_eq!(theme.syntax.comment, Color::rgb(0x88, 0x88, 0x88));
    assert_eq!(theme.syntax.argument, Color::rgb(0xaa, 0xaa, 0xaa));
    assert_eq!(theme.syntax.default, Color::rgb(0x99, 0x99, 0x99));
}

#[test]
fn syntax_override_works_with_inline_color_override() {
    // v1.0 S5: [theme.syntax] composes with inline color overrides —
    // both apply, and they touch independent fields.
    let cfg = ThemeConfig {
        name: "weft-warm".into(),
        accent: Some("#abcdef".into()),
        syntax: Some(SyntaxConfig {
            command: Some("#ff0000".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let theme = Theme::resolve(&cfg);
    // Inline accent override.
    assert_eq!(theme.accent, Color::rgb(0xab, 0xcd, 0xef));
    // Syntax override.
    assert_eq!(theme.syntax.command, Color::rgb(0xff, 0x00, 0x00));
    // Unspecified syntax fields inherit base.
    let warm = Theme::weft_warm();
    assert_eq!(theme.syntax.flag, warm.syntax.flag);
}

#[test]
fn syntax_override_invalid_hex_is_silently_ignored() {
    // v1.0 S5: an invalid hex string is skipped (parse_hex returns None),
    // leaving the base theme's value intact. This matches the behavior of
    // the existing inline color overrides.
    let warm = Theme::weft_warm();
    let cfg = ThemeConfig {
        name: "weft-warm".into(),
        syntax: Some(SyntaxConfig {
            command: Some("not-a-color".into()),
            flag: Some("#00ff00".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let theme = Theme::resolve(&cfg);
    // Invalid command override → retained from base.
    assert_eq!(theme.syntax.command, warm.syntax.command);
    // Valid flag override → applied.
    assert_eq!(theme.syntax.flag, Color::rgb(0x00, 0xff, 0x00));
}

#[test]
fn syntax_override_applies_to_all_themes() {
    // v1.0 S5: syntax overrides apply regardless of the base theme name.
    // Verify with warp_dark.
    let warp = Theme::warp_dark();
    let cfg = ThemeConfig {
        name: "warp".into(),
        syntax: Some(SyntaxConfig {
            command: Some("#abcdef".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let theme = Theme::resolve(&cfg);
    assert_eq!(theme.syntax.command, Color::rgb(0xab, 0xcd, 0xef));
    // Unspecified fields inherit from warp base.
    assert_eq!(theme.syntax.flag, warp.syntax.flag);
    assert_eq!(theme.syntax.path, warp.syntax.path);
}

// ── v1.7.0-B: output semantic color override tests ─────────────────

#[test]
fn output_semantic_override_applies() {
    let warm = Theme::weft_warm();
    let cfg = ThemeConfig {
        name: "weft-warm".into(),
        output: Some(OutputSemanticConfig {
            success: Some("#00ff00".into()),
            failure: Some("#ff0000".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let theme = Theme::resolve(&cfg);
    assert_eq!(theme.output.success, Color::rgb(0x00, 0xff, 0x00));
    assert_eq!(theme.output.failure, Color::rgb(0xff, 0x00, 0x00));
    // Unspecified fields inherit from base.
    // v1.11.0: `cwd` was a dead key (painter derives CWD gray from fg×0.65)
    // and is removed from the resolved theme — see AUDIT_v1.10.39.
    assert_eq!(theme.output.output_default, warm.output.output_default);
    assert_eq!(theme.output.metadata, warm.output.metadata);
}

#[test]
fn output_semantic_override_all_fields() {
    let cfg = ThemeConfig {
        name: "weft-warm".into(),
        output: Some(OutputSemanticConfig {
            enabled: None,
            output_default: Some("#111111".into()),
            metadata: Some("#333333".into()),
            success: Some("#444444".into()),
            failure: Some("#555555".into()),
        }),
        ..Default::default()
    };
    let theme = Theme::resolve(&cfg);
    assert_eq!(theme.output.output_default, Color::rgb(0x11, 0x11, 0x11));
    assert_eq!(theme.output.metadata, Color::rgb(0x33, 0x33, 0x33));
    assert_eq!(theme.output.success, Color::rgb(0x44, 0x44, 0x44));
    assert_eq!(theme.output.failure, Color::rgb(0x55, 0x55, 0x55));
}

// ── v1.7.0-D: semantic_output_enabled toggle tests ────────────────

#[test]
fn semantic_output_enabled_defaults_to_true() {
    let cfg = ThemeConfig::default();
    assert!(cfg.semantic_output_enabled(), "default should be true");
}

#[test]
fn semantic_output_enabled_explicit_false() {
    let cfg = ThemeConfig {
        output: Some(OutputSemanticConfig {
            enabled: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(!cfg.semantic_output_enabled());
}

#[test]
fn semantic_output_enabled_explicit_true() {
    let cfg = ThemeConfig {
        output: Some(OutputSemanticConfig {
            enabled: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(cfg.semantic_output_enabled());
}

#[test]
fn semantic_output_enabled_persists_through_save_load() {
    let dir = unique_tmp_path("semantic-toggle");
    let path = dir.join("config.toml");
    let cfg = Config {
        theme: ThemeConfig {
            name: "weft-warm".into(),
            output: Some(OutputSemanticConfig {
                enabled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    cfg.save_to_path(&path).expect("save");
    let text = std::fs::read_to_string(&path).expect("read");
    assert!(
        text.contains("enabled = false"),
        "expected 'enabled = false' in saved config:\n{}",
        text
    );
    // Reload and verify.
    let loaded: Config = toml::from_str(&text).expect("parse");
    assert!(
        !loaded.theme.semantic_output_enabled(),
        "reload should preserve enabled = false"
    );
}

#[test]
fn semantic_output_enabled_true_not_written_to_disk() {
    // v1.7.0-D review fix: `enabled = true` is the default, so it should NOT
    // be written to disk (minimal-write contract, matching `follow_system`).
    let dir = unique_tmp_path("semantic-toggle-true");
    let path = dir.join("config.toml");
    let cfg = Config {
        theme: ThemeConfig {
            name: "weft-warm".into(),
            output: Some(OutputSemanticConfig {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    cfg.save_to_path(&path).expect("save");
    let text = std::fs::read_to_string(&path).expect("read");
    assert!(
        !text.contains("enabled"),
        "expected 'enabled' to be absent from saved config (default true is implicit):\n{}",
        text
    );
    // Reload still reports true (default).
    let loaded: Config = toml::from_str(&text).expect("parse");
    assert!(loaded.theme.semantic_output_enabled());
}

#[test]
fn semantic_output_toggle_off_then_on_clears_false_from_disk() {
    // v1.7.5 regression: 用户通过 Settings 把 Semantic 从 on 切到 off 再切回 on，
    // 磁盘上仍残留 `enabled = false`，导致 reload 后仍为 false。
    // 根因：save_to_path 只在 `enabled == Some(false)` 时写入该键，切回 on 时
    // 既不写 `enabled = true` 也不删除已有的 `enabled = false`，toml_edit 增量
    // 编辑保留了旧键。
    let dir = unique_tmp_path("semantic-toggle-roundtrip");
    // unique_tmp_path 返回的是 config.toml 路径，其父目录已创建
    let path = &dir;
    // 初始文件：无 enabled 键（默认 true）
    std::fs::write(path, "[theme]\nname = \"weft-warm\"\n").unwrap();

    // off: load → set enabled = Some(false) → save
    let mut cfg: Config = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    cfg.theme.output = Some(OutputSemanticConfig {
        enabled: Some(false),
        ..Default::default()
    });
    cfg.save_to_path(path).unwrap();
    let after_off = std::fs::read_to_string(path).unwrap();
    assert!(after_off.contains("enabled = false"));

    // on: load → set enabled = Some(true) → save
    let mut cfg: Config = toml::from_str(&after_off).unwrap();
    cfg.theme.output = Some(OutputSemanticConfig {
        enabled: Some(true),
        ..Default::default()
    });
    cfg.save_to_path(path).unwrap();
    let after_on = std::fs::read_to_string(path).unwrap();

    // 关键断言：磁盘上不应残留 enabled = false
    assert!(
        !after_on.contains("enabled = false"),
        "BUG: 'enabled = false' still on disk after toggle to ON:\n{}",
        after_on
    );
    // reload 后应为 true
    let reloaded: Config = toml::from_str(&after_on).unwrap();
    assert!(
        reloaded.theme.semantic_output_enabled(),
        "after toggle ON, reload should report true; disk:\n{}",
        after_on
    );
}

#[test]
fn syntax_overrides_cleared_from_disk_when_all_fields_become_none() {
    // 举一反三：[theme.syntax] 也有"创建新表+仅非空写入"的反模式。
    // 当所有 syntax 字段从 Some 切回 None 时，磁盘上的旧值应被清除。
    let dir = unique_tmp_path("syntax-clear-roundtrip");
    let path = &dir;
    // Step 1: 写入有 syntax 覆盖的配置
    std::fs::write(
        path,
        "[theme]\nname = \"weft-warm\"\n[theme.syntax]\ncommand = \"#ff0000\"\nflag = \"#00ff00\"\n",
    )
    .unwrap();

    // Step 2: load → 清空所有 syntax 字段 → save
    let mut cfg: Config = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    cfg.theme.syntax = Some(crate::config::SyntaxConfig {
        command: None,
        flag: None,
        argument: None,
        path: None,
        string: None,
        number: None,
        variable: None,
        operator: None,
        comment: None,
        default: None,
    });
    cfg.save_to_path(path).unwrap();
    let after = std::fs::read_to_string(path).unwrap();

    // 关键断言：磁盘上不应残留 syntax 覆盖
    assert!(
        !after.contains("command ="),
        "BUG: syntax.command still on disk after clearing:\n{}",
        after
    );
    assert!(
        !after.contains("[theme.syntax]"),
        "BUG: [theme.syntax] table still on disk after clearing (should be removed):\n{}",
        after
    );
}

#[test]
fn palette_cleared_from_disk_when_emptied() {
    // 举一反三：theme.palette 清空时也应从磁盘删除旧值。
    let dir = unique_tmp_path("palette-clear-roundtrip");
    let path = &dir;
    std::fs::write(
        path,
        "[theme]\nname = \"weft-warm\"\npalette = [\"#1f1f1f\", \"#ff0000\"]\n",
    )
    .unwrap();

    let mut cfg: Config = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    cfg.theme.palette.clear();
    cfg.save_to_path(path).unwrap();
    let after = std::fs::read_to_string(path).unwrap();

    assert!(
        !after.contains("palette"),
        "BUG: palette still on disk after clearing:\n{}",
        after
    );
}

#[test]
fn syntax_argument_field_round_trips_through_save_load() {
    // v1.7.0-B 回归：SyntaxConfig.argument 字段在 save_to_path 中漏序列化，
    // 用户在 Settings 中配置的 argument 颜色 save 后会丢失。本测试验证修复后
    // argument 字段能正确 round-trip。
    let dir = unique_tmp_path("syntax-argument-roundtrip");
    let path = &dir;
    std::fs::write(path, "[theme]\nname = \"weft-warm\"\n").unwrap();

    let mut cfg: Config = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    cfg.theme.syntax = Some(crate::config::SyntaxConfig {
        argument: Some("#ff8800".to_string()),
        command: None,
        flag: None,
        path: None,
        string: None,
        number: None,
        variable: None,
        operator: None,
        comment: None,
        default: None,
    });
    cfg.save_to_path(path).unwrap();
    let after = std::fs::read_to_string(path).unwrap();

    // 磁盘上应有 argument = "#ff8800"
    assert!(
        after.contains("argument = \"#ff8800\""),
        "BUG: argument field not written to disk:\n{}",
        after
    );

    // reload 后 argument 字段应为 Some("#ff8800")
    let reloaded: Config = toml::from_str(&after).unwrap();
    assert_eq!(
        reloaded.theme.syntax.as_ref().unwrap().argument.as_deref(),
        Some("#ff8800"),
        "argument field must round-trip through save/load"
    );
}

#[test]
fn syntax_section_removed_from_disk_when_set_to_none() {
    // 审查建议：cfg.theme.syntax = None 时应删除磁盘上的整个 [theme.syntax] 表。
    let dir = unique_tmp_path("syntax-none-clear");
    let path = &dir;
    std::fs::write(
        path,
        "[theme]\nname = \"weft-warm\"\n[theme.syntax]\ncommand = \"#ff0000\"\n",
    )
    .unwrap();

    let mut cfg: Config = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    cfg.theme.syntax = None;
    cfg.save_to_path(path).unwrap();
    let after = std::fs::read_to_string(path).unwrap();

    assert!(
        !after.contains("[theme.syntax]"),
        "BUG: [theme.syntax] still on disk after setting to None:\n{}",
        after
    );
    assert!(
        !after.contains("command ="),
        "BUG: syntax.command still on disk after setting to None:\n{}",
        after
    );
}

#[test]
fn output_section_removed_from_disk_when_set_to_none() {
    // 审查建议：cfg.theme.output = None 时应删除磁盘上的整个 [theme.output] 表。
    let dir = unique_tmp_path("output-none-clear");
    let path = &dir;
    std::fs::write(
        path,
        "[theme]\nname = \"weft-warm\"\n[theme.output]\nenabled = false\n",
    )
    .unwrap();

    let mut cfg: Config = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    cfg.theme.output = None;
    cfg.save_to_path(path).unwrap();
    let after = std::fs::read_to_string(path).unwrap();

    assert!(
        !after.contains("[theme.output]"),
        "BUG: [theme.output] still on disk after setting to None:\n{}",
        after
    );
    assert!(
        !after.contains("enabled = false"),
        "BUG: enabled=false still on disk after setting output to None:\n{}",
        after
    );
}

#[test]
fn semantic_output_toggle_preserves_color_overrides() {
    // v1.7.0-D review fix regression: toggling `enabled` must NOT wipe
    // sibling color-override fields (output_default/metadata/success/
    // failure). Simulates the settings_controller case-5 toggle path.
    let mut output = OutputSemanticConfig {
        enabled: Some(true),
        output_default: Some("#aaaaaa".into()),
        metadata: Some("#cccccc".into()),
        success: Some("#00ff00".into()),
        failure: Some("#ff0000".into()),
    };
    // Toggle: flip enabled, preserve all other fields via struct-update.
    let prev = std::mem::take(&mut output);
    output = OutputSemanticConfig {
        enabled: Some(!prev.enabled.unwrap_or(true)),
        ..prev
    };
    // Assert siblings survived.
    assert_eq!(output.enabled, Some(false));
    assert_eq!(output.output_default.as_deref(), Some("#aaaaaa"));
    assert_eq!(output.metadata.as_deref(), Some("#cccccc"));
    assert_eq!(output.success.as_deref(), Some("#00ff00"));
    assert_eq!(output.failure.as_deref(), Some("#ff0000"));
}

// ── v1.7.0-E: Theme switching re-resolution tests ──────────────────

/// v1.7.0-E §2.7: "主题切换只重解析 palette index，不改显式 RGB". Verifies
/// the storage contract that makes theme-switch re-resolution possible:
/// `CellColor::Palette(i)` stores a theme-owned index (not a pre-baked RGB),
/// so switching themes changes the resolved color; `CellColor::Rgb(c)` stores
/// an explicit truecolor that is program-owned and must not change.
///
/// This test verifies the **storage** half of the contract at the `weft_core`
/// layer. The **rendering** half (palette[i] → RGB lookup in the painter) is
/// verified by the visual hierarchy contract test in `paint/primitives.rs`.
#[test]
fn theme_switch_re_resolves_palette_but_preserves_explicit_rgb() {
    use crate::grid::Color;

    let warm = Theme::weft_warm();
    let dracula = Theme::dracula();

    // Palette index 1 (ANSI red) — the stored index is the same, but the
    // resolved RGB differs because each theme owns its palette.
    let red_warm: Color = warm.palette[1];
    let red_dracula: Color = dracula.palette[1];
    assert_ne!(
        red_warm, red_dracula,
        "palette index 1 must re-resolve to different RGB under different themes"
    );

    // Explicit truecolor RGB: CellColor::Rgb stores the Color directly (no
    // theme lookup), so it is byte-identical across theme switches by
    // construction. The type system enforces this — no runtime assertion
    // needed. See paint/primitives.rs color_to_normalized for the renderer
    // path that consumes CellColor::Rgb without theme resolution.
}

/// v1.7.0-E: Verifies that all 11 built-in themes have distinct palette
/// entries for the primary ANSI colors (indices 0-7). This ensures a
/// palette-indexed `CapturedStyle` actually re-resolves to a different
/// visual when the user switches themes — the whole point of storing
/// `CellColor::Palette(i)` instead of pre-baked RGB.
#[test]
fn built_in_themes_have_distinct_ansi_palettes() {
    let themes: Vec<(&str, Theme)> = vec![
        ("weft_warm", Theme::weft_warm()),
        ("weft_light", Theme::weft_light()),
        ("warp_dark", Theme::warp_dark()),
        ("dracula", Theme::dracula()),
        ("solarized_dark", Theme::solarized_dark()),
        ("gruvbox_dark", Theme::gruvbox_dark()),
        ("nord", Theme::nord()),
        ("tokyo_night", Theme::tokyo_night()),
        ("catppuccin_mocha", Theme::catppuccin_mocha()),
        ("one_dark", Theme::one_dark()),
        ("monokai_pro", Theme::monokai_pro()),
    ];
    // For each ANSI primary (0-7), at least two themes should disagree on
    // the resolved RGB. (In practice many will — we just need to confirm
    // palettes aren't accidentally identical.)
    for idx in 0..8u8 {
        let resolved: Vec<(&str, crate::grid::Color)> = themes
            .iter()
            .map(|(name, t)| (*name, t.palette[idx as usize]))
            .collect();
        let unique: Vec<_> = resolved.windows(2).filter(|w| w[0].1 != w[1].1).collect();
        assert!(
            !unique.is_empty(),
            "ANSI palette index {idx} is identical across all 11 themes — palettes not distinct"
        );
    }
}

// ── v1.0 S2: Config::save() tests ──────────────────────────────────

fn unique_tmp_path(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("weft-config-save-{pid}-{id}-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("config.toml")
}

#[test]
fn save_default_config_creates_empty_file() {
    // Saving a default Config should produce a parseable file (with no
    // sections, since nothing differs from defaults).
    let path = unique_tmp_path("default");
    let cfg = Config::default();
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    // Default config writes nothing (all fields match defaults).
    // The file should be valid TOML (possibly empty).
    let reloaded: Config = toml::from_str(&text).unwrap();
    assert_eq!(reloaded.font.family, "Menlo");
    assert_eq!(reloaded.theme.name, "weft-warm");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn save_and_reload_roundtrip() {
    // A Config with non-default fields should survive a save → reload
    // cycle.
    let path = unique_tmp_path("roundtrip");
    let cfg = Config {
        font: FontConfig {
            family: "Monaco".into(),
            size: 16.0,
            ..Default::default()
        },
        theme: ThemeConfig {
            name: "warp".into(),
            accent: Some("#ff0000".into()),
            syntax: Some(SyntaxConfig {
                command: Some("#abcdef".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        window: WindowConfig {
            width: 1024,
            height: 768,
            ..Default::default()
        },
        scrollback: ScrollbackConfig { lines: 50_000 },
        ..Default::default()
    };
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    let reloaded: Config = toml::from_str(&text).unwrap();
    assert_eq!(reloaded.font.family, "Monaco");
    assert_eq!(reloaded.font.size, 16.0);
    assert_eq!(reloaded.theme.name, "warp");
    assert_eq!(reloaded.theme.accent.as_deref(), Some("#ff0000"));
    assert_eq!(
        reloaded.theme.syntax.as_ref().unwrap().command.as_deref(),
        Some("#abcdef")
    );
    assert_eq!(reloaded.window.width, 1024);
    assert_eq!(reloaded.window.height, 768);
    assert_eq!(reloaded.scrollback.lines, 50_000);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn save_preserves_user_comments() {
    // v1.0 S2: toml_edit preserves comments. Write a file with a comment,
    // save over it, and verify the comment survives.
    let path = unique_tmp_path("comments");
    std::fs::write(
        &path,
        "# this is my comment\n[font]\nfamily = \"Menlo\"\nsize = 14.0\n",
    )
    .unwrap();
    let cfg = Config {
        font: FontConfig {
            size: 18.0,
            ..Default::default()
        },
        ..Default::default()
    };
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("# this is my comment"),
        "comment should be preserved: {text}"
    );
    assert!(text.contains("size = 18"));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn save_preserves_unknown_fields() {
    // v1.0 S2: unknown fields the user added should survive the save.
    let path = unique_tmp_path("unknown");
    std::fs::write(
        &path,
        "[font]\nsize = 14.0\n\n[unknown_section]\nfoo = \"bar\"\n",
    )
    .unwrap();
    let cfg = Config::default();
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("foo = \"bar\""),
        "unknown field should survive"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn save_only_writes_non_default_fields() {
    // v1.0 S2: fields that match defaults shouldn't appear in the output
    // (keeps the file clean for a fresh save).
    let path = unique_tmp_path("nondefault");
    let cfg = Config {
        font: FontConfig {
            size: 18.0,
            ..Default::default()
        },
        ..Default::default()
    };
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    // size should be written (non-default).
    assert!(text.contains("size = 18"));
    // family should NOT be written (matches default "Menlo").
    assert!(
        !text.contains("family = \"Menlo\""),
        "default family should not be written"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// v1.11.3 (PLAN_v1113 §3.3): `[compat] bold_is_bright = true` survives a
/// save → reload round-trip; the default (false) writes nothing.
#[test]
fn compat_bold_is_bright_roundtrip_and_default_omission() {
    let path = unique_tmp_path("compat");
    let cfg = Config {
        compat: CompatConfig {
            bold_is_bright: true,
            ..CompatConfig::default()
        },
        ..Default::default()
    };
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("bold_is_bright = true"), "text: {text}");
    let reloaded: Config = toml::from_str(&text).unwrap();
    assert!(reloaded.compat.bold_is_bright, "round-trip keeps the flag");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());

    // Default config: the [compat] section must not appear on disk.
    let path2 = unique_tmp_path("compat-default");
    Config::default()
        .save_to_path(&path2)
        .expect("default save should succeed");
    let text2 = std::fs::read_to_string(&path2).unwrap();
    assert!(!text2.contains("compat"), "default compat omitted: {text2}");
    let _ = std::fs::remove_dir_all(path2.parent().unwrap());
}

/// v1.11.4 (PLAN_v1114 §3): `[compat] kitty_keyboard = false` survives a
/// save → reload round-trip; the default (true) writes nothing.
#[test]
fn compat_kitty_keyboard_roundtrip_and_default_omission() {
    let path = unique_tmp_path("compat-kitty");
    let cfg = Config {
        compat: CompatConfig {
            kitty_keyboard: false,
            ..CompatConfig::default()
        },
        ..Default::default()
    };
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("kitty_keyboard = false"), "text: {text}");
    let reloaded: Config = toml::from_str(&text).unwrap();
    assert!(
        !reloaded.compat.kitty_keyboard,
        "round-trip keeps the flag off"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());

    // Default config: no [compat] section on disk, kitty on.
    let path2 = unique_tmp_path("compat-kitty-default");
    Config::default()
        .save_to_path(&path2)
        .expect("default save should succeed");
    let text2 = std::fs::read_to_string(&path2).unwrap();
    assert!(!text2.contains("compat"), "default compat omitted: {text2}");
    assert!(Config::default().compat.kitty_keyboard, "default is on");
    let _ = std::fs::remove_dir_all(path2.parent().unwrap());
}

/// v1.11.4 (PLAN_v1114 §3): profile-level `[profiles.x.compat]`
/// kitty_keyboard override survives save → reload.
#[test]
fn compat_kitty_keyboard_profile_override_roundtrips() {
    let path = unique_tmp_path("compat-kitty-profile");
    let mut cfg = Config::default();
    cfg.profiles.insert(
        "legacy-keys".into(),
        ProfileConfig {
            compat: Some(CompatConfig {
                kitty_keyboard: false,
                ..CompatConfig::default()
            }),
            ..Default::default()
        },
    );
    cfg.active_profile = Some("legacy-keys".into());
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("kitty_keyboard = false"), "text: {text}");
    let reloaded: Config = toml::from_str(&text).unwrap();
    assert!(
        !reloaded.profiles["legacy-keys"]
            .compat
            .as_ref()
            .unwrap()
            .kitty_keyboard
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// v1.11.3 (PLAN_v1113 §3.3): profile-level `[profiles.x.compat]` override
/// survives save → reload and applies to the effective config.
#[test]
fn compat_profile_override_roundtrips_and_applies() {
    let path = unique_tmp_path("compat-profile");
    let mut cfg = Config::default();
    cfg.profiles.insert(
        "bright".into(),
        ProfileConfig {
            compat: Some(CompatConfig {
                bold_is_bright: true,
                ..CompatConfig::default()
            }),
            ..Default::default()
        },
    );
    cfg.active_profile = Some("bright".into());
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("bold_is_bright = true"), "text: {text}");
    let reloaded: Config = toml::from_str(&text).unwrap();
    assert_eq!(
        reloaded.profiles["bright"]
            .compat
            .as_ref()
            .map(|c| c.bold_is_bright),
        Some(true)
    );
    let base = Config::default();
    let (effective, diagnostics) = reloaded.resolve_active_profile().unwrap();
    assert!(diagnostics.is_empty());
    assert_ne!(effective.compat.bold_is_bright, base.compat.bold_is_bright);
    assert!(
        effective.compat.bold_is_bright,
        "override reaches effective config"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn save_creates_parent_dir() {
    // v1.0 S2: save should create the parent directory if it doesn't
    // exist.
    let dir = std::env::temp_dir().join(format!("weft-config-save-nested-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let nested = dir.join("a/b/c/config.toml");
    let cfg = Config::default();
    cfg.save_to_path(&nested).expect("save should succeed");
    assert!(nested.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_error_display() {
    // v1.0 S2: ConfigSaveError should have a useful Display impl.
    let e = ConfigSaveError::NoConfigPath;
    assert!(format!("{e}").contains("HOME"));
    let e = ConfigSaveError::NoParentDir;
    assert!(format!("{e}").contains("parent"));
    let e = ConfigSaveError::Io(std::io::Error::from_raw_os_error(13));
    assert!(format!("{e}").contains("config save failed"));
}

#[test]
fn toggle_settings_has_default_keybinding() {
    // v1.0 S1: Cmd+, opens Settings.
    let kb = KeyBindings::default();
    assert_eq!(
        kb.lookup(KeyCode::Char(','), Modifiers::SUPER),
        Some(Action::ToggleSettings)
    );
}

#[test]
fn toggle_settings_serde_roundtrip() {
    let s = "\"toggle_settings\"";
    let back: Action = serde_json::from_str(s).unwrap();
    assert_eq!(back, Action::ToggleSettings);
}

// ── v1.3: pane split / focus / close default bindings ─────────────

#[test]
fn split_vertical_has_default_keybinding_cmd_d() {
    let kb = KeyBindings::default();
    assert_eq!(
        kb.lookup(KeyCode::Char('d'), Modifiers::SUPER),
        Some(Action::SplitVertical)
    );
}

#[test]
fn split_horizontal_has_default_keybinding_cmd_shift_d() {
    let kb = KeyBindings::default();
    assert_eq!(
        kb.lookup(KeyCode::Char('d'), Modifiers::SUPER | Modifiers::SHIFT),
        Some(Action::SplitHorizontal)
    );
}

#[test]
fn focus_next_pane_bracket_keybinds_cyclic_focus() {
    let kb = KeyBindings::default();
    // Cmd+Option+] — cyclic focus next (still bound in v1.3.3; only the
    // arrow-key bindings were redirected to spatial FocusPane* variants).
    assert_eq!(
        kb.lookup(KeyCode::Char(']'), Modifiers::SUPER | Modifiers::ALT),
        Some(Action::FocusNextPane)
    );
}

#[test]
fn focus_prev_pane_bracket_keybinds_cyclic_focus() {
    let kb = KeyBindings::default();
    // Cmd+Option+[ — cyclic focus prev.
    assert_eq!(
        kb.lookup(KeyCode::Char('['), Modifiers::SUPER | Modifiers::ALT),
        Some(Action::FocusPrevPane)
    );
}

#[test]
fn focus_pane_directions_have_cmd_alt_arrow_keybinds() {
    // v1.3.3: arrow keys now do spatial direction focus (replacing the
    // v1.3.0 cyclic binding on the same keys). Cyclic focus is still on
    // the bracket keys (tested above).
    let kb = KeyBindings::default();
    assert_eq!(
        kb.lookup(KeyCode::Up, Modifiers::SUPER | Modifiers::ALT),
        Some(Action::FocusPaneUp)
    );
    assert_eq!(
        kb.lookup(KeyCode::Down, Modifiers::SUPER | Modifiers::ALT),
        Some(Action::FocusPaneDown)
    );
    assert_eq!(
        kb.lookup(KeyCode::Left, Modifiers::SUPER | Modifiers::ALT),
        Some(Action::FocusPaneLeft)
    );
    assert_eq!(
        kb.lookup(KeyCode::Right, Modifiers::SUPER | Modifiers::ALT),
        Some(Action::FocusPaneRight)
    );
}

#[test]
fn toggle_pane_zoom_has_cmd_shift_return_keybind() {
    let kb = KeyBindings::default();
    assert_eq!(
        kb.lookup(KeyCode::Enter, Modifiers::SUPER | Modifiers::SHIFT),
        Some(Action::TogglePaneZoom)
    );
}

#[test]
fn close_pane_has_contextual_cmd_w_binding() {
    let kb = KeyBindings::default();
    assert_eq!(
        kb.lookup(KeyCode::Char('w'), Modifiers::SUPER),
        Some(Action::ClosePane)
    );
}

#[test]
fn pane_actions_serde_roundtrip() {
    // Snake-case names survive a serde_json roundtrip so user TOML like
    // `"cmd+d" = "split_horizontal"` parses to the right Action.
    for (name, expected) in [
        ("split_horizontal", Action::SplitHorizontal),
        ("split_vertical", Action::SplitVertical),
        ("focus_next_pane", Action::FocusNextPane),
        ("focus_prev_pane", Action::FocusPrevPane),
        ("close_pane", Action::ClosePane),
        // v1.3.3 additions.
        ("toggle_pane_zoom", Action::TogglePaneZoom),
        ("focus_pane_up", Action::FocusPaneUp),
        ("focus_pane_down", Action::FocusPaneDown),
        ("focus_pane_left", Action::FocusPaneLeft),
        ("focus_pane_right", Action::FocusPaneRight),
    ] {
        let s = format!("\"{name}\"");
        let back: Action = serde_json::from_str(&s).unwrap();
        assert_eq!(back, expected, "serde roundtrip for {name}");
    }
}

#[test]
fn close_tab_has_explicit_cmd_ctrl_w_binding() {
    let kb = KeyBindings::default();
    assert_eq!(
        kb.lookup(KeyCode::Char('w'), Modifiers::SUPER | Modifiers::CONTROL),
        Some(Action::CloseTab)
    );
}

// ── T2: per-field Config::save roundtrip tests ────────────────────

#[test]
fn save_font_size_change() {
    let path = unique_tmp_path("font-size");
    let cfg = Config {
        font: FontConfig {
            size: 13.0,
            ..Default::default()
        },
        ..Default::default()
    };
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    let reloaded: Config = toml::from_str(&text).unwrap();
    assert_eq!(reloaded.font.size, 13.0);
    // Other fields retain defaults.
    assert_eq!(reloaded.font.family, "Menlo");
    assert_eq!(reloaded.theme.name, "weft-warm");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn save_window_opacity_change() {
    let path = unique_tmp_path("win-opacity");
    let cfg = Config {
        window: WindowConfig {
            opacity: 0.85,
            ..Default::default()
        },
        ..Default::default()
    };
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    let reloaded: Config = toml::from_str(&text).unwrap();
    // f32 round-trip through TOML f64 — compare with small epsilon.
    assert!((reloaded.window.opacity - 0.85).abs() < 1e-6);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn save_theme_name_change() {
    let path = unique_tmp_path("theme-name");
    let cfg = Config {
        theme: ThemeConfig {
            name: "dracula".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    let reloaded: Config = toml::from_str(&text).unwrap();
    assert_eq!(reloaded.theme.name, "dracula");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn save_minimum_contrast_change() {
    let path = unique_tmp_path("minimum-contrast");
    let cfg = Config {
        theme: ThemeConfig {
            minimum_contrast: 5.5,
            ..Default::default()
        },
        ..Default::default()
    };
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("minimum_contrast = 5.5"));
    let reloaded: Config = toml::from_str(&text).unwrap();
    assert!((reloaded.theme.minimum_contrast - 5.5).abs() < 1e-6);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn save_scrollback_lines_change() {
    let path = unique_tmp_path("scrollback");
    let cfg = Config {
        scrollback: ScrollbackConfig { lines: 25_000 },
        ..Default::default()
    };
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    let reloaded: Config = toml::from_str(&text).unwrap();
    assert_eq!(reloaded.scrollback.lines, 25_000);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn save_preserves_other_fields_when_one_changes() {
    // Changing only window.width should leave font, theme, and scrollback
    // at their configured (non-default) values after a save + reload cycle.
    let path = unique_tmp_path("preserve");
    let cfg = Config {
        font: FontConfig {
            family: "Monaco".into(),
            size: 16.0,
            ..Default::default()
        },
        theme: ThemeConfig {
            name: "nord".into(),
            ..Default::default()
        },
        window: WindowConfig {
            width: 1200,
            ..Default::default()
        },
        scrollback: ScrollbackConfig { lines: 50_000 },
        ..Default::default()
    };
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    let reloaded: Config = toml::from_str(&text).unwrap();
    // The field we changed.
    assert_eq!(reloaded.window.width, 1200);
    // Other configured fields must survive unchanged.
    assert_eq!(reloaded.font.family, "Monaco");
    assert_eq!(reloaded.font.size, 16.0);
    assert_eq!(reloaded.theme.name, "nord");
    assert_eq!(reloaded.scrollback.lines, 50_000);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn settings_editable_fields_survive_one_save_reload_cycle() {
    let path = unique_tmp_path("settings-editable-audit");
    let mut cfg = Config::default();
    cfg.font.family = "Monaco".into();
    cfg.font.size = 17.5;
    cfg.font.line_height = 1.35;
    cfg.theme.name = "nord".into();
    cfg.theme.follow_system = false;
    cfg.theme.minimum_contrast = 6.5;
    cfg.theme.output = Some(OutputSemanticConfig {
        enabled: Some(false),
        ..Default::default()
    });
    cfg.window.width = 1_180;
    cfg.window.height = 820;
    cfg.window.opacity = 0.65;
    cfg.window.padding_x = 3;
    cfg.window.padding_y = 4;
    cfg.window.sidebar_width = Some(310.0);
    cfg.scrollback.lines = 42_000;
    cfg.editor.submit_on_ctrl_enter = true;
    cfg.editor.smart_select = false;
    cfg.logo.variant = LogoVariant::Light;

    cfg.save_to_path(&path).expect("settings audit save");
    let reloaded: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

    assert_eq!(reloaded.font.family, "Monaco");
    assert!((reloaded.font.size - 17.5).abs() < 1e-6);
    assert!((reloaded.font.line_height - 1.35).abs() < 1e-6);
    assert_eq!(reloaded.theme.name, "nord");
    assert!(!reloaded.theme.follow_system);
    assert!((reloaded.theme.minimum_contrast - 6.5).abs() < 1e-6);
    assert!(!reloaded.theme.semantic_output_enabled());
    assert_eq!(reloaded.window.width, 1_180);
    assert_eq!(reloaded.window.height, 820);
    assert!((reloaded.window.opacity - 0.65).abs() < 1e-6);
    assert_eq!(reloaded.window.padding_x, 3);
    assert_eq!(reloaded.window.padding_y, 4);
    assert_eq!(reloaded.window.sidebar_width, Some(310.0));
    assert_eq!(reloaded.scrollback.lines, 42_000);
    assert!(reloaded.editor.submit_on_ctrl_enter);
    assert!(!reloaded.editor.smart_select);
    assert_eq!(reloaded.logo.variant, LogoVariant::Light);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

// ── Logo config ───────────────────────────────────────────────────

#[test]
fn logo_variant_default_is_cool() {
    let c = Config::default();
    assert_eq!(c.logo.variant, LogoVariant::Cool);
}

#[test]
fn logo_variant_round_trip_all_variants() {
    for v in LogoVariant::ALL {
        let toml_str = format!("[logo]\nvariant = \"{}\"\n", v.as_str());
        let cfg: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(cfg.logo.variant, v, "round-trip failed for {:?}", v);
    }
}

#[test]
fn logo_variant_unknown_falls_back_to_cool() {
    let toml_str = "[logo]\nvariant = \"nonexistent\"\n";
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.logo.variant, LogoVariant::Cool);
}

#[test]
fn logo_variant_save_writes_non_default() {
    let tmp = temp_path("logo-save");
    let _ = std::fs::remove_file(&tmp);
    let mut cfg = Config::default();
    cfg.logo.variant = LogoVariant::Warm;
    cfg.save_to_path(&tmp).unwrap();
    let text = std::fs::read_to_string(&tmp).unwrap();
    assert!(text.contains("[logo]"), "missing [logo] section");
    assert!(
        text.contains("variant = \"warm\""),
        "missing variant = warm"
    );
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn logo_variant_default_not_written() {
    let tmp = temp_path("logo-default");
    let _ = std::fs::remove_file(&tmp);
    let cfg = Config::default();
    cfg.save_to_path(&tmp).unwrap();
    let text = std::fs::read_to_string(&tmp).unwrap();
    assert!(
        !text.contains("[logo]"),
        "default logo should not be written"
    );
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn logo_variant_from_str_round_trip() {
    for v in LogoVariant::ALL {
        assert_eq!(LogoVariant::from_str(v.as_str()), v);
    }
}

#[test]
fn logo_variant_label_not_empty() {
    for v in LogoVariant::ALL {
        assert!(!v.label().is_empty());
    }
}

#[test]
fn logo_variant_preserves_other_sections() {
    let tmp = temp_path("logo-preserve");
    let _ = std::fs::remove_file(&tmp);
    let initial = "[font]\nfamily = \"Monaco\"\nsize = 14.0\n\n[logo]\nvariant = \"light\"\n";
    std::fs::write(&tmp, initial).unwrap();
    let text = std::fs::read_to_string(&tmp).unwrap();
    let mut cfg: Config = toml::from_str(&text).unwrap();
    assert_eq!(cfg.logo.variant, LogoVariant::Light);
    cfg.logo.variant = LogoVariant::Warm;
    cfg.save_to_path(&tmp).unwrap();
    let reloaded_text = std::fs::read_to_string(&tmp).unwrap();
    let reloaded: Config = toml::from_str(&reloaded_text).unwrap();
    assert_eq!(reloaded.font.family, "Monaco");
    assert_eq!(reloaded.font.size, 14.0);
    assert_eq!(reloaded.logo.variant, LogoVariant::Warm);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn logo_variant_save_cool_clears_stale_non_default() {
    // v1.0 fix: switching back to Cool (default) after saving a non-default
    // variant must clear the stale `variant = "warm"` from the file.
    // Otherwise the saved non-default value would override the default
    // on next load.
    let tmp = temp_path("logo-clear-stale");
    let _ = std::fs::remove_file(&tmp);
    // Step 1: save with Warm — writes [logo] variant = "warm".
    let mut cfg = Config::default();
    cfg.logo.variant = LogoVariant::Warm;
    cfg.save_to_path(&tmp).unwrap();
    let text = std::fs::read_to_string(&tmp).unwrap();
    assert!(text.contains("variant = \"warm\""));
    // Step 2: switch back to Cool and save — must remove the stale key.
    cfg.logo.variant = LogoVariant::Cool;
    cfg.save_to_path(&tmp).unwrap();
    let reloaded: Config = toml::from_str(&std::fs::read_to_string(&tmp).unwrap()).unwrap();
    assert_eq!(
        reloaded.logo.variant,
        LogoVariant::Cool,
        "stale non-default variant should be cleared on save"
    );
    let _ = std::fs::remove_file(&tmp);
}

// ── v1.5.0: Profile integration tests ──────────────────────────────────
//
// These tests live in `tests.rs` (not next to `profiles.rs` / `io.rs`) so
// they exercise the full `Config::save_to_path` + `load_resolved_from_path`
// round-trip the way the runtime does — catching toml_edit persistence bugs
// that unit tests on the in-memory structs can't. The pure-logic coverage
// (apply_overrides, name validation, resolve_active_profile) lives next to
// the impl files; this file owns the persistence semantics.

fn semantic_profile(enabled: bool) -> Config {
    let mut source = Config {
        active_profile: Some("work".into()),
        ..Config::default()
    };
    source.profiles.insert(
        "work".into(),
        ProfileConfig {
            theme: Some(ThemeConfig {
                output: Some(OutputSemanticConfig {
                    enabled: Some(enabled),
                    ..OutputSemanticConfig::default()
                }),
                ..ThemeConfig::default()
            }),
            ..ProfileConfig::default()
        },
    );
    source
}

#[test]
fn active_profile_semantic_off_survives_save_and_reload() {
    let path = unique_tmp_path("profile-semantic-off").join("config.toml");
    semantic_profile(false).save_to_path(&path).unwrap();
    let loaded = load_resolved_from_path(&path).unwrap();
    assert!(!loaded.effective.theme.semantic_output_enabled());
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("enabled = false"));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn active_profile_semantic_on_removes_stale_false() {
    let path = unique_tmp_path("profile-semantic-stale").join("config.toml");
    let mut source = semantic_profile(false);
    source.save_to_path(&path).unwrap();
    source
        .profiles
        .get_mut("work")
        .unwrap()
        .theme
        .as_mut()
        .unwrap()
        .output
        .as_mut()
        .unwrap()
        .enabled = Some(true);
    source.save_to_path(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("enabled = false"), "stale toggle: {text}");
    assert!(load_resolved_from_path(&path)
        .unwrap()
        .effective
        .theme
        .semantic_output_enabled());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn active_profile_semantic_toggle_preserves_color_overrides() {
    let path = unique_tmp_path("profile-semantic-colors").join("config.toml");
    let mut source = semantic_profile(false);
    {
        let output = source
            .profiles
            .get_mut("work")
            .unwrap()
            .theme
            .as_mut()
            .unwrap()
            .output
            .as_mut()
            .unwrap();
        output.success = Some("#12ab34".into());
        output.failure = Some("#ef4567".into());
    }
    source.save_to_path(&path).unwrap();
    source
        .profiles
        .get_mut("work")
        .unwrap()
        .theme
        .as_mut()
        .unwrap()
        .output
        .as_mut()
        .unwrap()
        .enabled = Some(true);
    source.save_to_path(&path).unwrap();

    let loaded = load_resolved_from_path(&path).unwrap();
    let output = loaded.source.profiles["work"]
        .theme
        .as_ref()
        .unwrap()
        .output
        .as_ref()
        .unwrap();
    assert_eq!(output.success.as_deref(), Some("#12ab34"));
    assert_eq!(output.failure.as_deref(), Some("#ef4567"));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Profile `[keybindings]` is a full-section override: a profile that
/// specifies keybindings replaces the base keybindings entirely, not a
/// per-key merge. A base binding absent from the profile must NOT survive
/// into the effective config.
#[test]
fn profile_keybindings_replace_instead_of_merge() {
    let mut base = Config::default();
    base.keybindings.insert("cmd+c".into(), Action::Copy);
    base.keybindings.insert("cmd+v".into(), Action::Paste);

    let mut profile = ProfileConfig::default();
    let mut kb = std::collections::HashMap::new();
    // Only `cmd+x` is in the profile. `cmd+c` / `cmd+v` from base must
    // NOT appear in the effective keybindings.
    kb.insert("cmd+x".into(), Action::Copy);
    profile.keybindings = Some(kb);

    apply_overrides(&mut base, &profile);
    assert_eq!(base.keybindings.len(), 1);
    assert!(base.keybindings.contains_key("cmd+x"));
    assert!(!base.keybindings.contains_key("cmd+c"));
    assert!(!base.keybindings.contains_key("cmd+v"));
}

/// Unknown top-level fields the user added (e.g. a future `[ai]` section
/// or a comment marker) must survive a save that also writes profiles.
/// This is the v1.5 analog of `save_preserves_unknown_fields` — the
/// `profiles` table write path must not blow away unrelated top-level
/// keys when it rewrites the `[profiles]` table.
#[test]
fn unknown_top_level_fields_survive_save_with_profiles() {
    let path = unique_tmp_path("unknown-with-profiles");
    std::fs::write(
        &path,
        "[font]\nsize = 14.0\n\n\
         [unknown_section]\nfoo = \"bar\"\n\n\
         [profiles.work.font]\nfamily = \"Profile-Mono\"\n",
    )
    .unwrap();
    // Save a config that actually has a `work` profile so the
    // `[profiles.work]` table survives the rewrite. Saving `default()`
    // (no profiles) would correctly remove the table.
    let mut cfg = Config::default();
    cfg.profiles.insert(
        "work".into(),
        ProfileConfig {
            font: Some(FontConfig {
                family: "Profile-Mono".into(),
                ..FontConfig::default()
            }),
            ..ProfileConfig::default()
        },
    );
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("foo = \"bar\""),
        "unknown top-level field should survive: {text}"
    );
    assert!(
        text.contains("[profiles.work"),
        "profile section should survive: {text}"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Saving a config with an active profile must not flatten the profile's
/// overrides into the base `[font]` section. This is the persistence
/// analog of `resolved_config_cannot_flatten_into_source_on_save` in
/// `io.rs` — it verifies the save path itself preserves the split.
#[test]
fn save_does_not_flatten_active_profile_into_base() {
    let path = unique_tmp_path("no-flatten-save");
    let mut source = Config::default();
    source.font.family = "Base-Mono".into();
    source.active_profile = Some("work".into());
    source.profiles.insert(
        "work".into(),
        ProfileConfig {
            font: Some(FontConfig {
                family: "Profile-Mono".into(),
                ..FontConfig::default()
            }),
            ..ProfileConfig::default()
        },
    );
    source.save_to_path(&path).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    // Base [font].family must remain "Base-Mono" — the profile override
    // must NOT leak into the base section.
    assert!(
        saved.contains("family = \"Base-Mono\""),
        "base font must not be flattened: {saved}"
    );
    // And the profile section must be present with its own family.
    assert!(
        saved.contains("Profile-Mono"),
        "profile font must be preserved: {saved}"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Deleting a profile from `source.profiles` and saving must remove the
/// corresponding `[profiles.<name>]` table from disk. Stale profiles must
/// not survive a save.
#[test]
fn save_removes_deleted_profiles_from_disk() {
    let path = unique_tmp_path("delete-profile");
    let mut source = Config::default();
    source.profiles.insert(
        "work".into(),
        ProfileConfig {
            font: Some(FontConfig {
                family: "Work-Mono".into(),
                ..FontConfig::default()
            }),
            ..ProfileConfig::default()
        },
    );
    source.profiles.insert(
        "play".into(),
        ProfileConfig {
            font: Some(FontConfig {
                family: "Play-Mono".into(),
                ..FontConfig::default()
            }),
            ..ProfileConfig::default()
        },
    );
    source.save_to_path(&path).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("work"), "work profile should be present");
    assert!(saved.contains("play"), "play profile should be present");

    // Now delete `play` and re-save.
    source.profiles.remove("play");
    source.save_to_path(&path).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(
        saved.contains("work"),
        "work profile should still be present"
    );
    assert!(
        !saved.contains("Play-Mono"),
        "deleted profile content should be gone: {saved}"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Clearing `active_profile` to `None` must remove the top-level
/// `active_profile` key from disk, not leave a stale `active_profile = ""`.
#[test]
fn save_clears_active_profile_when_none() {
    let path = unique_tmp_path("clear-active");
    let mut source = Config {
        active_profile: Some("work".into()),
        ..Config::default()
    };
    source.profiles.insert(
        "work".into(),
        ProfileConfig {
            font: Some(FontConfig {
                family: "Work-Mono".into(),
                ..FontConfig::default()
            }),
            ..ProfileConfig::default()
        },
    );
    source.save_to_path(&path).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("active_profile"));

    // Now clear active_profile and re-save.
    source.active_profile = None;
    source.save_to_path(&path).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(
        !saved.contains("active_profile"),
        "active_profile should be removed: {saved}"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

// ── v1.5.3: WEFT_CONFIG override ──────────────────────────────────────
//
// The V15 plan §8.2 requires `Config::config_path()` to honor an absolute
// `WEFT_CONFIG` env var ahead of `XDG_CONFIG_HOME` / `HOME`. Empty values
// are ignored; relative paths log a warning and fall through (Finder launches
// have an indeterminate `cwd`, so a relative `WEFT_CONFIG` would be a
// footgun). These tests cover all four cases required by the v1.5.3 exit
// criteria: absolute, empty, relative, and "parent dir doesn't exist"
// (which is just a path that doesn't resolve yet — `config_path()` itself
// doesn't check existence, only callers do, so we just verify it returns
// the path verbatim and let `load_resolved_from_path` report the missing
// file separately).
//
// All tests run under `ENV_LOCK` (via `with_weft_config`) because env vars
// are process-global — without the lock, parallel tests in this module
// would race on `WEFT_CONFIG` / `XDG_CONFIG_HOME` / `HOME`.

#[test]
fn weft_config_absolute_override_wins() {
    // Absolute WEFT_CONFIG takes priority over both XDG_CONFIG_HOME and HOME.
    // Path doesn't need to exist — config_path() only resolves the path,
    // it doesn't stat it.
    let abs = std::env::temp_dir().join("weft-config-absolute-override.toml");
    let xdg_dir = std::env::temp_dir().join("weft-config-xdg-ignored");
    let home_dir = std::env::temp_dir().join("weft-config-home-ignored");

    let xdg_os: std::ffi::OsString = xdg_dir.as_os_str().into();
    let home_os: std::ffi::OsString = home_dir.as_os_str().into();
    let weft_os: std::ffi::OsString = abs.as_os_str().into();

    let resolved = std::cell::RefCell::new(None);
    with_weft_config(
        Some(weft_os.as_os_str()),
        Some(xdg_os.as_os_str()),
        Some(home_os.as_os_str()),
        || {
            *resolved.borrow_mut() = Config::config_path();
        },
    );
    let got = resolved
        .borrow()
        .clone()
        .expect("config_path returned None");
    assert_eq!(
        got, abs,
        "absolute WEFT_CONFIG must override XDG_CONFIG_HOME and HOME"
    );
}

#[test]
fn weft_config_empty_value_falls_through_to_xdg() {
    // Empty WEFT_CONFIG is ignored — XDG_CONFIG_HOME wins.
    let xdg_dir = std::env::temp_dir().join("weft-config-empty-xdg");
    std::fs::create_dir_all(&xdg_dir).unwrap();
    let home_dir = std::env::temp_dir().join("weft-config-empty-home");

    let xdg_os: std::ffi::OsString = xdg_dir.as_os_str().into();
    let home_os: std::ffi::OsString = home_dir.as_os_str().into();

    let resolved = std::cell::RefCell::new(None);
    with_weft_config(
        Some(std::ffi::OsStr::new("")),
        Some(xdg_os.as_os_str()),
        Some(home_os.as_os_str()),
        || {
            *resolved.borrow_mut() = Config::config_path();
        },
    );
    let got = resolved
        .borrow()
        .clone()
        .expect("config_path returned None");
    assert_eq!(
        got,
        xdg_dir.join("weft").join("config.toml"),
        "empty WEFT_CONFIG must fall through to XDG_CONFIG_HOME"
    );
    let _ = std::fs::remove_dir_all(&xdg_dir);
}

#[test]
fn weft_config_relative_falls_through_to_xdg() {
    // Relative WEFT_CONFIG is rejected (warns and falls through) so a
    // Finder launch with indeterminate cwd can't silently pick up an
    // unintended file. XDG_CONFIG_HOME wins.
    let xdg_dir = std::env::temp_dir().join("weft-config-rel-xdg");
    std::fs::create_dir_all(&xdg_dir).unwrap();
    let home_dir = std::env::temp_dir().join("weft-config-rel-home");

    let xdg_os: std::ffi::OsString = xdg_dir.as_os_str().into();
    let home_os: std::ffi::OsString = home_dir.as_os_str().into();

    let resolved = std::cell::RefCell::new(None);
    with_weft_config(
        Some(std::ffi::OsStr::new("relative/config.toml")),
        Some(xdg_os.as_os_str()),
        Some(home_os.as_os_str()),
        || {
            *resolved.borrow_mut() = Config::config_path();
        },
    );
    let got = resolved
        .borrow()
        .clone()
        .expect("config_path returned None");
    assert_eq!(
        got,
        xdg_dir.join("weft").join("config.toml"),
        "relative WEFT_CONFIG must fall through to XDG_CONFIG_HOME"
    );
    let _ = std::fs::remove_dir_all(&xdg_dir);
}

#[test]
fn weft_config_missing_parent_returns_path_verbatim() {
    // config_path() doesn't check whether the parent dir exists — it just
    // returns the path. Callers (load_resolved_from_path, save, watcher)
    // are responsible for reporting a missing file / unwritable parent.
    // The v1.5.3 exit criteria lists "missing parent" as a case to verify;
    // here we confirm config_path() returns the absolute path as-is so a
    // later load correctly surfaces `NotFound`.
    let abs = std::env::temp_dir()
        .join("weft-config-missing-parent-dir")
        .join("nested")
        .join("config.toml");

    let weft_os: std::ffi::OsString = abs.as_os_str().into();
    let xdg_os: std::ffi::OsString = std::env::temp_dir()
        .join("weft-config-missing-xdg")
        .into_os_string();
    let home_os: std::ffi::OsString = std::env::temp_dir()
        .join("weft-config-missing-home")
        .into_os_string();

    let resolved = std::cell::RefCell::new(None);
    with_weft_config(
        Some(weft_os.as_os_str()),
        Some(xdg_os.as_os_str()),
        Some(home_os.as_os_str()),
        || {
            *resolved.borrow_mut() = Config::config_path();
        },
    );
    let got = resolved
        .borrow()
        .clone()
        .expect("config_path returned None");
    assert_eq!(
        got, abs,
        "absolute WEFT_CONFIG is returned verbatim even when parent doesn't exist"
    );
    // And load_resolved_from_path must surface the missing file as an
    // Io(NotFound) error rather than panicking.
    let err = load_resolved_from_path(&abs).unwrap_err();
    assert!(
        matches!(
            err,
            ConfigLoadError::Io(ref e) if e.kind() == std::io::ErrorKind::NotFound
        ),
        "missing parent must surface as Io(NotFound), got {err:?}"
    );
}

#[test]
fn weft_config_unset_falls_through_to_xdg_then_home() {
    // With WEFT_CONFIG unset, the legacy XDG_CONFIG_HOME → HOME order is
    // preserved. This guards against regressions where the new WEFT_CONFIG
    // branch accidentally short-circuits the fallback chain.
    let xdg_dir = std::env::temp_dir().join("weft-config-unset-xdg");
    std::fs::create_dir_all(&xdg_dir).unwrap();
    let home_dir = std::env::temp_dir().join("weft-config-unset-home");

    let xdg_os: std::ffi::OsString = xdg_dir.as_os_str().into();
    let home_os: std::ffi::OsString = home_dir.as_os_str().into();

    // XDG wins when both XDG and HOME are set.
    let resolved = std::cell::RefCell::new(None);
    with_weft_config(
        None,
        Some(xdg_os.as_os_str()),
        Some(home_os.as_os_str()),
        || {
            *resolved.borrow_mut() = Config::config_path();
        },
    );
    let got = resolved
        .borrow()
        .clone()
        .expect("config_path returned None");
    assert_eq!(
        got,
        xdg_dir.join("weft").join("config.toml"),
        "XDG_CONFIG_HOME must win when WEFT_CONFIG is unset"
    );
    let _ = std::fs::remove_dir_all(&xdg_dir);

    // HOME fallback when XDG is unset (and WEFT_CONFIG still unset).
    // `~/.config/weft/config.toml` — the legacy default.
    let resolved2 = std::cell::RefCell::new(None);
    with_weft_config(None, None, Some(home_os.as_os_str()), || {
        *resolved2.borrow_mut() = Config::config_path();
    });
    let got2 = resolved2
        .borrow()
        .clone()
        .expect("config_path returned None with HOME set");
    assert_eq!(
        got2,
        home_dir.join(".config").join("weft").join("config.toml"),
        "HOME must be used when WEFT_CONFIG and XDG_CONFIG_HOME are both unset"
    );
}

// ── v1.11.5 [clipboard] / [notifications] (PLAN_v1115 §M8) ──────────────

/// `[clipboard] osc52 = "unrestricted"` survives save → reload; the default
/// writes nothing to disk (a stale key must not resurrect later).
#[test]
fn clipboard_osc52_roundtrip_and_default_omission() {
    use crate::config::Osc52Mode;

    let path = unique_tmp_path("clipboard-osc52");
    let cfg = Config {
        clipboard: crate::config::ClipboardConfig {
            osc52: Osc52Mode::Unrestricted,
        },
        ..Default::default()
    };
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("osc52 = \"unrestricted\""), "text: {text}");
    let reloaded: Config = toml::from_str(&text).unwrap();
    assert_eq!(
        reloaded.clipboard.osc52,
        Osc52Mode::Unrestricted,
        "round-trip keeps the mode"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());

    // Default config: no [clipboard] section on disk.
    let path2 = unique_tmp_path("clipboard-default");
    Config::default()
        .save_to_path(&path2)
        .expect("default save should succeed");
    let text2 = std::fs::read_to_string(&path2).unwrap();
    assert!(
        !text2.contains("clipboard"),
        "default clipboard omitted: {text2}"
    );
    let _ = std::fs::remove_dir_all(path2.parent().unwrap());
}

/// Illegal OSC 52 values fall back to `Default` — a typo must never fail
/// the whole config parse (PLAN_v1115 §M8 serde rule).
#[test]
fn clipboard_osc52_illegal_value_falls_back_to_default() {
    use crate::config::Osc52Mode;

    let cfg: Config = toml::from_str("[clipboard]\nosc52 = \"garbage\"\n").unwrap();
    assert_eq!(cfg.clipboard.osc52, Osc52Mode::Default);
    assert!(cfg.notifications.enabled, "rest of config still parses");

    let cfg: Config = toml::from_str("[clipboard]\nosc52 = \"OFF\"\n").unwrap();
    assert_eq!(
        cfg.clipboard.osc52,
        Osc52Mode::Default,
        "values are lowercase-only; uppercase is illegal → default"
    );
}

/// `[notifications]` non-default keys round-trip; all-default writes nothing.
#[test]
fn notifications_roundtrip_and_default_omission() {
    let path = unique_tmp_path("notifications");
    let cfg = Config {
        notifications: crate::config::NotificationsConfig {
            enabled: false,
            threshold_secs: 120,
            sound: true,
        },
        ..Default::default()
    };
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("enabled = false"), "text: {text}");
    assert!(text.contains("threshold_secs = 120"), "text: {text}");
    assert!(text.contains("sound = true"), "text: {text}");
    let reloaded: Config = toml::from_str(&text).unwrap();
    assert!(!reloaded.notifications.enabled);
    assert_eq!(reloaded.notifications.threshold_secs, 120);
    assert!(reloaded.notifications.sound);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());

    // Default config: no [notifications] section on disk.
    let path2 = unique_tmp_path("notifications-default");
    Config::default()
        .save_to_path(&path2)
        .expect("default save should succeed");
    let text2 = std::fs::read_to_string(&path2).unwrap();
    assert!(
        !text2.contains("notifications"),
        "default notifications omitted: {text2}"
    );
    let _ = std::fs::remove_dir_all(path2.parent().unwrap());
}

/// Defaults per PLAN_v1115 §M8 TOML: enabled=true, threshold 30s, sound off.
#[test]
fn notifications_defaults_match_plan() {
    let d = crate::config::NotificationsConfig::default();
    assert!(d.enabled, "总闸 default on per plan");
    assert_eq!(d.threshold_secs, 30, "X7 threshold");
    assert!(!d.sound, "no surprise sound");
}

/// Section-level profile overrides for both new sections survive save →
/// reload and apply to the effective config.
#[test]
fn clipboard_and_notifications_profile_override_applies() {
    use crate::config::{ClipboardConfig, NotificationsConfig, Osc52Mode};

    let mut cfg = Config::default();
    cfg.profiles.insert(
        "silent".into(),
        ProfileConfig {
            clipboard: Some(ClipboardConfig {
                osc52: Osc52Mode::Off,
            }),
            notifications: Some(NotificationsConfig {
                enabled: false,
                ..NotificationsConfig::default()
            }),
            ..Default::default()
        },
    );
    cfg.active_profile = Some("silent".into());
    let (effective, diagnostics) = cfg.resolve_active_profile().unwrap();
    assert!(diagnostics.is_empty());
    assert_eq!(effective.clipboard.osc52, Osc52Mode::Off);
    assert!(!effective.notifications.enabled);
    assert_eq!(
        effective.notifications.threshold_secs, 30,
        "unlisted section keys inherit the base defaults"
    );

    // save → reload keeps the profile overrides
    let path = unique_tmp_path("clipboard-notify-profile");
    cfg.save_to_path(&path).expect("save should succeed");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("osc52 = \"off\""), "text: {text}");
    assert!(text.contains("enabled = false"), "text: {text}");
    let reloaded: Config = toml::from_str(&text).unwrap();
    assert_eq!(
        reloaded.profiles["silent"]
            .clipboard
            .as_ref()
            .unwrap()
            .osc52,
        Osc52Mode::Off
    );
    assert!(
        !reloaded.profiles["silent"]
            .notifications
            .as_ref()
            .unwrap()
            .enabled
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

// ── v1.11.6 (PLAN_v1116 M6): Theme link + [theme.ui] keys ────────────

#[test]
fn theme_link_default_matches_old_hyperlink_const_bitwise() {
    // v1.11.6 (PLAN_v1116 M6/D-f): Theme::link default must be bit-equal
    // to the old HYPERLINK_COLOR const ([0.36, 0.62, 0.94, 1.0]) that
    // grid_instances.rs carried before the theme-key migration, and
    // Theme::ui must default to all-None (UiColors dual-branch fallback).
    let old_const: [f32; 4] = [0.36, 0.62, 0.94, 1.0];
    let themes = [
        Theme::weft_warm(),
        Theme::weft_light(),
        Theme::warp_dark(),
        Theme::dracula(),
        Theme::solarized_dark(),
        Theme::gruvbox_dark(),
        Theme::nord(),
        Theme::tokyo_night(),
        Theme::catppuccin_mocha(),
        Theme::one_dark(),
        Theme::monokai_pro(),
    ];
    for theme in themes {
        for (i, (actual, expected)) in theme.link.iter().zip(old_const).enumerate() {
            assert_eq!(
                actual.to_bits(),
                expected.to_bits(),
                "link channel {i} of {theme:?} drifted from the old const"
            );
        }
        assert_eq!(
            theme.ui,
            ThemeUi::default(),
            "ui seeds must default to None"
        );
    }
}

#[test]
fn theme_link_and_ui_parse_from_toml_and_resolve() {
    let toml_text = r##"
[theme]
name = "weft-warm"
link = "#ffcc00"

[theme.ui]
success = "#11ff22"
warning = "#ffaa00"
error = "#ff2244"
find_match = "#66ffcc"
"##;
    let c: Config = toml::from_str(toml_text).unwrap();
    assert_eq!(c.theme.link.as_deref(), Some("#ffcc00"));
    let ui = c.theme.ui.as_ref().expect("[theme.ui] should parse");
    assert_eq!(ui.success.as_deref(), Some("#11ff22"));
    assert_eq!(ui.warning.as_deref(), Some("#ffaa00"));
    assert_eq!(ui.error.as_deref(), Some("#ff2244"));
    assert_eq!(ui.find_match.as_deref(), Some("#66ffcc"));

    // link: user hex is u8-granular → /255-normalized (P1-4 doc note).
    let theme = Theme::resolve(&c.theme);
    for (actual, expected) in theme.link.iter().zip([0xffu8, 0xcc, 0x00]) {
        assert_eq!(actual.to_bits(), (expected as f32 / 255.0).to_bits());
    }
    assert_eq!(theme.link[3].to_bits(), 1.0f32.to_bits());
    assert_eq!(theme.ui.success, Some(Color::rgb(0x11, 0xff, 0x22)));
    assert_eq!(theme.ui.warning, Some(Color::rgb(0xff, 0xaa, 0x00)));
    assert_eq!(theme.ui.error, Some(Color::rgb(0xff, 0x22, 0x44)));
    assert_eq!(theme.ui.find_match, Some(Color::rgb(0x66, 0xff, 0xcc)));
}

#[test]
fn theme_link_invalid_hex_falls_back_to_default() {
    // Invalid hex must be silently ignored — the base theme's link/ui
    // stay untouched (same policy as syntax/output overrides).
    let cfg = ThemeConfig {
        name: "weft-warm".into(),
        link: Some("not-a-hex".into()),
        ui: Some(UiConfig {
            success: Some("zzzz".into()),
            warning: Some("#12345".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let theme = Theme::resolve(&cfg);
    let default_link: [f32; 4] = [0.36, 0.62, 0.94, 1.0];
    for (actual, expected) in theme.link.iter().zip(default_link) {
        assert_eq!(actual.to_bits(), expected.to_bits());
    }
    assert_eq!(theme.ui, ThemeUi::default());
}

#[test]
fn theme_link_and_ui_round_trip_through_save_load() {
    // save_to_path must persist [theme] link + [theme.ui], and a reload
    // must recover them (mirrors syntax_argument_field_round_trips).
    let dir = unique_tmp_path("theme-link-ui-roundtrip");
    let path = &dir;
    std::fs::write(path, "[theme]\nname = \"weft-warm\"\n").unwrap();

    let mut cfg: Config = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    cfg.theme.link = Some("#ffcc00".to_string());
    cfg.theme.ui = Some(UiConfig {
        success: Some("#11ff22".to_string()),
        warning: None,
        error: Some("#ff2244".to_string()),
        find_match: None,
    });
    cfg.save_to_path(path).unwrap();
    let after = std::fs::read_to_string(path).unwrap();
    assert!(
        after.contains("link = \"#ffcc00\""),
        "link not on disk:\n{after}"
    );
    assert!(
        after.contains("success = \"#11ff22\""),
        "[theme.ui] success not on disk:\n{after}"
    );

    let reloaded: Config = toml::from_str(&after).unwrap();
    assert_eq!(reloaded.theme.link.as_deref(), Some("#ffcc00"));
    let ui = reloaded.theme.ui.expect("[theme.ui] should survive reload");
    assert_eq!(ui.success.as_deref(), Some("#11ff22"));
    assert_eq!(ui.error.as_deref(), Some("#ff2244"));
    assert!(ui.warning.is_none() && ui.find_match.is_none());
}

#[test]
fn profile_theme_link_and_ui_override_apply() {
    // A profile's [theme] link/ui must replace the base config wholesale
    // (apply_overrides semantics: base.theme = profile.theme.clone()).
    let mut base = Config::default();
    assert!(base.theme.link.is_none() && base.theme.ui.is_none());
    let profile = ProfileConfig {
        theme: Some(ThemeConfig {
            name: "weft-warm".into(),
            link: Some("#00aaff".into()),
            ui: Some(UiConfig {
                success: Some("#00ff88".into()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    apply_overrides(&mut base, &profile);
    assert_eq!(base.theme.link.as_deref(), Some("#00aaff"));
    assert_eq!(
        base.theme.ui.as_ref().unwrap().success.as_deref(),
        Some("#00ff88")
    );
}
