use super::support::{palette_form_field_direction, workflow_save_failure_message};
use weft_core::input::{KeyCode, Modifiers};
use weft_core::workflow::WorkflowStoreError;

#[test]
fn palette_form_owns_tab_arrows_and_page_navigation() {
    assert_eq!(
        palette_form_field_direction(KeyCode::Tab, Modifiers::empty()),
        Some(true)
    );
    assert_eq!(
        palette_form_field_direction(KeyCode::Tab, Modifiers::SHIFT),
        Some(false)
    );
    assert_eq!(
        palette_form_field_direction(KeyCode::Up, Modifiers::empty()),
        Some(false)
    );
    assert_eq!(
        palette_form_field_direction(KeyCode::Down, Modifiers::empty()),
        Some(true)
    );
    assert_eq!(
        palette_form_field_direction(KeyCode::PageUp, Modifiers::empty()),
        Some(false)
    );
    assert_eq!(
        palette_form_field_direction(KeyCode::PageDown, Modifiers::empty()),
        Some(true)
    );
    assert_eq!(
        palette_form_field_direction(KeyCode::Char('x'), Modifiers::empty()),
        None
    );
}

#[test]
fn save_failure_message_maps_constraint_violation_to_duplicate_name() {
    // 构造器耦合钉死（v1.13.3）：`ffi::Error::new(c_int)` 是 libsqlite3-sys 0.28
    // 唯一构造器，它把 extended_code 设为 result_code 本身（无 new_extended；
    // 字段 pub）。若 `workflow_save_failure_message` 升级为 extended_code ==
    // SQLITE_CONSTRAINT_UNIQUE 精确判定，本测试必须同步换结构体字面量
    // `Error { code, extended_code }` 构造——此处失败即提醒两处一起改。
    let err = WorkflowStoreError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
        None,
    ));
    assert_eq!(workflow_save_failure_message(&err), "名称已存在，请换一个");
}

#[test]
fn save_failure_message_maps_io_to_generic_copy() {
    let err = WorkflowStoreError::Io(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "db not writable",
    ));
    assert_eq!(workflow_save_failure_message(&err), "保存失败，请重试");
}

#[test]
fn save_failure_message_maps_non_constraint_sqlite_to_generic_copy() {
    let err = WorkflowStoreError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
        None,
    ));
    assert_eq!(workflow_save_failure_message(&err), "保存失败，请重试");
}
