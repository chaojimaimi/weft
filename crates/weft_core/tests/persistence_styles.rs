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
                    underline_colors: Vec::new(),
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

/// v1.11.3 (PLAN_v1113 §4.6): a persisted StyledLine carrying a Wavy
/// underline style and an explicit SGR 58 underline color survives a
/// store insert → recent reload round-trip.
#[test]
fn styled_underline_style_and_color_roundtrip_through_sqlite() {
    let path = std::env::temp_dir().join(format!(
        "weft-underline-roundtrip-{}-{}.db",
        std::process::id(),
        SystemTime::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    let store = BlockStore::open(&path).unwrap();
    let block = Block {
        id: BlockId(1),
        command: "wavy".into(),
        cwd: None,
        output: "wave".into(),
        styled_output: Some(Arc::new(StyledOutput {
            lines: vec![StyledLine {
                line: 0,
                foregrounds: Vec::new(),
                backgrounds: Vec::new(),
                links: Vec::new(),
                attributes: vec![weft_core::blocks::AttributeSpan {
                    start: 0,
                    end: 4,
                    flags: weft_core::grid::CellFlags::UNDERLINE,
                    underline_style: 3, // Wavy
                }],
                underline_colors: vec![weft_core::blocks::ColorSpan {
                    start: 0,
                    end: 4,
                    color: CellColor::Rgb(weft_core::grid::Color::rgb(9, 8, 7)),
                }],
            }],
        })),
        exit_code: Some(0),
        started_at: SystemTime::UNIX_EPOCH,
        finished_at: Some(SystemTime::UNIX_EPOCH),
        collapsed: false,
        screen_origin: false,
    };
    store.insert(&block).unwrap();
    let loaded = store.recent(1).unwrap().pop().unwrap();
    let styled = loaded.styled_output.expect("styled output persisted");
    let line = styled.line(0).expect("line 0");
    assert_eq!(
        line.underline_style_at(0),
        weft_core::grid::UnderlineStyle::Wavy,
        "u8 style carrier survives SQLite JSON"
    );
    assert_eq!(
        line.underline_color_at(0),
        Some(CellColor::Rgb(weft_core::grid::Color::rgb(9, 8, 7)))
    );
    drop(store);
    let _ = std::fs::remove_file(path);
}

/// v1.11.3 (PLAN_v1113 §4.6): a SQLite row written by a pre-v1.11.3 build
/// (styled_output JSON without underline_style/underline_colors) loads as
/// Single/None — backward-compatible read.
#[test]
fn legacy_styled_output_json_loads_with_single_underline() {
    let path = std::env::temp_dir().join(format!(
        "weft-legacy-styles-{}-{}.db",
        std::process::id(),
        SystemTime::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    drop(BlockStore::open(&path).unwrap());
    let connection = rusqlite::Connection::open(&path).unwrap();
    let legacy_json = r#"{"lines":[{"line":0,"foregrounds":[],"backgrounds":[],"links":[],"attributes":[{"start":0,"end":4,"flags":"UNDERLINE"}]}]}"#;
    connection
        .execute(
            "INSERT INTO blocks \
             (id, command, cwd, output, styled_output, exit_code, started_ms, finished_ms, collapsed) \
             VALUES (1, 'legacy', '/tmp', 'old', ?1, 0, 0, 0, 0)",
            [legacy_json],
        )
        .unwrap();
    drop(connection);

    let store = BlockStore::open(&path).unwrap();
    let loaded = store.recent(1).unwrap().pop().unwrap();
    let styled = loaded.styled_output.expect("legacy styled output loads");
    let line = styled.line(0).expect("line 0");
    assert_eq!(
        line.underline_style_at(0),
        weft_core::grid::UnderlineStyle::Single,
        "absent underline_style reads as Single"
    );
    assert_eq!(
        line.underline_color_at(0),
        None,
        "absent colors read as None"
    );
    assert_eq!(line.attributes_at(0), weft_core::grid::CellFlags::UNDERLINE);
    drop(store);
    let _ = std::fs::remove_file(path);
}
