//! End-to-end shell-integration test.
//!
//! Spawns a real `/bin/zsh` with the ZDOTDIR rc-redirect and confirms the OSC
//! 133 command-boundary markers arrive — and that the hook *source* is NOT
//! echoed into the terminal (the exact bug that forced PTY-stdin injection to
//! be disabled).
//!
//! The shell is sandboxed: ZDOTDIR is redirected at our generated `.zshenv`,
//! and restored to an empty "fake home" so the user's real `~/.zshrc` never
//! loads (keeps the test deterministic and side-effect-free).

use std::time::Duration;

use weft_core::pty::{Pty, PtyEvent};
use weft_core::shell::Integration;

/// Drain PTY output until the child exits or the deadline passes.
async fn collect(pty: &mut Pty, deadline: tokio::time::Instant) -> Vec<u8> {
    let mut buf = Vec::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), pty.recv()).await {
            Ok(Some(PtyEvent::Output(d))) => buf.extend_from_slice(&d),
            Ok(Some(PtyEvent::Exit(_))) => break,
            Ok(None) => break,
            Err(_) => {} // keep draining until deadline/exit
        }
    }
    buf
}

#[tokio::test]
async fn zsh_emits_osc133_markers_without_echoing_hook() {
    if !std::path::Path::new("/bin/zsh").exists() {
        eprintln!("skipping: /bin/zsh not present on this system");
        return;
    }

    // Sandbox layout.
    let sandbox = std::env::temp_dir().join(format!("weft-shell-it-{}", std::process::id()));
    let redirect_dir = sandbox.join("zsh"); // ZDOTDIR → here (holds our .zshenv)
    let fake_home = sandbox.join("home"); // restored ZDOTDIR/HOME → empty, no user rc
    std::fs::create_dir_all(&redirect_dir).unwrap();
    std::fs::create_dir_all(&fake_home).unwrap();
    let fake_home_str = fake_home.to_string_lossy().into_owned();

    // Write the generated .zshenv that registers the OSC 133 hooks.
    let (_, file) = Integration::Zsh.rc_redirect().expect("zsh has rc redirect");
    std::fs::write(redirect_dir.join(file.filename), file.body).unwrap();

    // Build child env: integration flag + restore target + ZDOTDIR + isolated HOME.
    let mut env: Vec<(&str, String)> = Integration::Zsh
        .child_env(Some(&fake_home_str))
        .into_iter()
        .collect();
    env.push(("ZDOTDIR", redirect_dir.to_string_lossy().into_owned()));
    env.push(("HOME", fake_home_str));
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let mut pty = Pty::spawn_with_args("/bin/zsh", &[], (24, 80), &env_refs, || {})
        .expect("failed to spawn zsh");

    // Let the first prompt render, then run a command and exit.
    tokio::time::sleep(Duration::from_millis(300)).await;
    pty.write(b"echo weft_marker_probe\n").await.unwrap();
    pty.write(b"exit\n").await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let out = collect(&mut pty, deadline).await;
    let _ = std::fs::remove_dir_all(&sandbox);

    let s = String::from_utf8_lossy(&out);

    // Sanity: the command actually executed.
    assert!(
        s.contains("weft_marker_probe"),
        "command output missing;\n{s}"
    );

    // OSC 133 markers: prompt start (A), command start (B), command end w/ exit (D;0).
    assert!(
        s.contains("\u{1b}]133;A"),
        "missing prompt marker 133;A;\n{s}"
    );
    assert!(
        s.contains("\u{1b}]133;B"),
        "missing command-start marker 133;B;\n{s}"
    );
    assert!(
        s.contains("\u{1b}]133;D;0"),
        "missing command-end marker 133;D;0;\n{s}"
    );

    // The hook source must NOT appear — this is the regression guard for the
    // original PTY-stdin echo bug.
    assert!(
        !s.contains("precmd_functions=("),
        "hook source leaked to terminal (echo bug returned!);\n{s}"
    );
    assert!(
        !s.contains("WEFT_ORIG_ZDOTDIR"),
        "bootstrap env var leaked to terminal;\n{s}"
    );
}
