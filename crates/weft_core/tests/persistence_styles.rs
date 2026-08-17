use std::sync::Arc;
use std::time::SystemTime;

use weft_core::blocks::{Block, BlockId, ForegroundSpan, StyledLine, StyledOutput};
use weft_core::grid::CellColor;
use weft_core::persistence::BlockStore;

#[test]
fn oversized_styled_output_is_not_persisted() {
    let path = std::env::temp_dir().join(format!(
        "weft-oversized-styles-{}-{}.db",
        std::process::id(),
        SystemTime::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    let store = BlockStore::open(&path).unwrap();
    let block = Block {
        id: BlockId(1),
        command: "large-style-map".into(),
        cwd: Some("/tmp/project".into()),
        output: "plain text survives".into(),
        styled_output: Some(Arc::new(StyledOutput {
            lines: (0..20_000)
                .map(|line| StyledLine {
                    line,
                    foregrounds: vec![ForegroundSpan {
                        start: 0,
                        end: 1,
                        color: CellColor::Palette(1),
                    }],
                    backgrounds: Vec::new(),
                    links: Vec::new(),
                    attributes: Vec::new(),
                })
                .collect(),
        })),
        exit_code: Some(0),
        started_at: SystemTime::UNIX_EPOCH,
        finished_at: Some(SystemTime::UNIX_EPOCH),
        collapsed: false,
        screen_origin: false,
    };

    store.insert(&block).unwrap();
    let loaded = store.recent(1).unwrap().pop().unwrap();

    assert_eq!(loaded.output.as_ref(), "plain text survives");
    assert_eq!(loaded.cwd.as_deref(), Some("/tmp/project"));
    assert!(loaded.styled_output.is_none());
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn oversized_database_style_text_is_filtered_inside_sqlite() {
    let path = std::env::temp_dir().join(format!(
        "weft-oversized-db-styles-{}-{}.db",
        std::process::id(),
        SystemTime::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    drop(BlockStore::open(&path).unwrap());
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "INSERT INTO blocks \
             (id, command, cwd, output, styled_output, exit_code, started_ms, finished_ms, collapsed) \
             VALUES (1, 'tampered', '/tmp', 'safe text', ?1, 0, 0, 0, 0)",
            ["界".repeat(100_000)],
        )
        .unwrap();
    drop(connection);

    let store = BlockStore::open(&path).unwrap();
    let loaded = store.recent(1).unwrap().pop().unwrap();

    assert_eq!(loaded.output.as_ref(), "safe text");
    assert!(loaded.styled_output.is_none());
    drop(store);
    let _ = std::fs::remove_file(path);
}
