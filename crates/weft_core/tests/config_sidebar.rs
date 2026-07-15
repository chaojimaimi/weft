use std::sync::atomic::{AtomicUsize, Ordering};

use weft_core::config::Config;

fn temp_config_path() -> std::path::PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "weft-sidebar-config-{}-{id}.toml",
        std::process::id()
    ))
}

#[test]
fn sidebar_width_survives_save_and_reload() {
    let path = temp_config_path();
    let mut config = Config::default();
    config.window.sidebar_width = Some(260.0);
    config.save_to_path(&path).expect("sidebar width saves");

    let text = std::fs::read_to_string(&path).expect("saved config is readable");
    let reloaded: Config = toml::from_str(&text).expect("saved config is valid TOML");
    assert_eq!(reloaded.window.sidebar_width, Some(260.0));

    let _ = std::fs::remove_file(path);
}
