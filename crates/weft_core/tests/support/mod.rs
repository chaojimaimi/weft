#[cfg(target_os = "macos")]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use weft_core::grid::{CellFlags, CellWidth};
use weft_core::pty::{Pty, PtyEvent};
use weft_core::vt::Terminal;

static SANDBOX_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub fn require_command(path: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        assert!(
            Path::new(path).is_file(),
            "required macOS TUI command is missing: {path}"
        );
        true
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("skipping macOS system TUI integration outside macOS: {path}");
        false
    }
}

pub fn require_path_command(name: &str, required: bool) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let path = std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(name))
                .find(|candidate| {
                    candidate.is_file()
                        && candidate
                            .metadata()
                            .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
                })
        });
        if path.is_none() {
            assert!(
                !required,
                "required macOS TUI command is missing from PATH: {name}"
            );
            eprintln!("skipping optional macOS TUI integration: {name} is not in PATH");
        }
        path
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("skipping macOS system TUI integration outside macOS: {name}");
        None
    }
}

pub fn sandbox(tag: &str) -> PathBuf {
    let id = SANDBOX_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("weft-tui-{tag}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create TUI test sandbox");
    path
}

/// A real process attached to Weft's PTY and VT parser.
///
/// Output is processed incrementally and terminal query responses are written
/// back exactly as the App does, so Vim/less can finish terminal negotiation.
pub struct TuiSession {
    pub pty: Pty,
    pub terminal: Terminal,
    pub raw_output: Vec<u8>,
    pub exited: bool,
    pub exit_status: Option<Result<i32, String>>,
}

impl TuiSession {
    pub fn spawn(program: &str, args: &[&str], rows: u16, cols: u16, cwd: &Path) -> Self {
        let cwd_text = cwd.to_string_lossy();
        let env = [
            ("TERM", "xterm-256color"),
            ("LANG", "en_US.UTF-8"),
            ("LC_ALL", "en_US.UTF-8"),
            ("HOME", cwd_text.as_ref()),
            ("LESS", ""),
            ("LESSOPEN", ""),
            ("LESSCLOSE", ""),
            ("PAGER", ""),
            ("GIT_PAGER", ""),
            ("VIMINIT", ""),
            ("EXINIT", ""),
            ("TMUX", ""),
        ];
        let pty = Pty::spawn_with_args(
            program,
            args,
            (rows, cols),
            &env,
            Some(cwd_text.as_ref()),
            || {},
        )
        .unwrap_or_else(|error| panic!("failed to spawn {program}: {error}"));
        Self {
            pty,
            terminal: Terminal::new(rows as usize, cols as usize),
            raw_output: Vec::new(),
            exited: false,
            exit_status: None,
        }
    }

    pub fn send(&self, bytes: &[u8]) {
        self.pty.write_sync(bytes).expect("write TUI input");
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.pty.resize(rows, cols).expect("resize TUI PTY");
        self.terminal.resize(rows as usize, cols as usize);
    }

    pub async fn pump_for(&mut self, duration: Duration) {
        let deadline = tokio::time::Instant::now() + duration;
        while tokio::time::Instant::now() < deadline && !self.exited {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let wait = remaining.min(Duration::from_millis(25));
            match tokio::time::timeout(wait, self.pty.recv()).await {
                Ok(Some(PtyEvent::Output(bytes))) => {
                    self.raw_output.extend_from_slice(&bytes);
                    self.terminal.process(&bytes);
                    let response = self.terminal.take_response();
                    if !response.is_empty() {
                        self.pty
                            .write_sync(&response)
                            .expect("write terminal query response");
                    }
                }
                Ok(Some(PtyEvent::Exit(status))) => {
                    self.exited = true;
                    self.exit_status = Some(status);
                }
                Ok(None) => self.exited = true,
                Err(_) => {}
            }
        }
    }

    pub async fn wait_until<F>(&mut self, timeout: Duration, predicate: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if predicate(self) {
                return true;
            }
            if self.exited {
                break;
            }
            self.pump_for(Duration::from_millis(50)).await;
        }
        predicate(self)
    }

    pub fn visible_lines(&self) -> Vec<String> {
        (0..self.terminal.grid().num_rows)
            .map(|row| self.terminal.grid().row_text(row))
            .collect()
    }

    pub fn visible_text(&self) -> String {
        self.visible_lines().join("\n")
    }

    pub fn bottom_line(&self) -> String {
        let row = self.terminal.grid().num_rows.saturating_sub(1);
        self.terminal.grid().row_text(row)
    }

    pub fn assert_no_orphaned_wide_cells(&self) {
        let grid = self.terminal.grid();
        for row in 0..grid.num_rows {
            for col in 0..grid.num_cols {
                let cell = grid.cell(row, col);
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    assert!(col > 0, "wide spacer at left edge: {row}:{col}");
                    assert_eq!(
                        grid.cell(row, col - 1).width,
                        CellWidth::Full,
                        "orphaned wide spacer at {row}:{col}"
                    );
                }
                if cell.width == CellWidth::Full {
                    assert!(
                        col + 1 < grid.num_cols,
                        "wide lead at right edge: {row}:{col}"
                    );
                    assert!(
                        grid.cell(row, col + 1)
                            .flags
                            .contains(CellFlags::WIDE_SPACER),
                        "orphaned wide lead at {row}:{col}"
                    );
                }
            }
        }
    }
}
