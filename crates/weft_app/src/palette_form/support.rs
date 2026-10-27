use weft_core::input::{KeyCode, Modifiers};
use weft_core::workflow::WorkflowStoreError;

pub(crate) fn palette_form_field_direction(key: KeyCode, mods: Modifiers) -> Option<bool> {
    use crate::paint::command_surface::CommandSurfaceKeyAction;
    match crate::paint::command_surface::resolve_command_surface_key(key, mods) {
        CommandSurfaceKeyAction::MoveUp | CommandSurfaceKeyAction::PageUp => Some(false),
        CommandSurfaceKeyAction::MoveDown | CommandSurfaceKeyAction::PageDown => Some(true),
        CommandSurfaceKeyAction::CycleFocus => Some(!mods.contains(Modifiers::SHIFT)),
        _ => None,
    }
}

/// v1.13.3: classify a workflow-store save failure for the toast copy.
/// `name` is the table's only UNIQUE column (weft_core workflow.rs SCHEMA),
/// so a constraint violation means "duplicate name" — the one deterministic
/// failure an Enter-retry can never fix. Everything else is transient from
/// the user's point of view and gets the generic retry copy.
pub(crate) fn workflow_save_failure_message(err: &WorkflowStoreError) -> &'static str {
    use rusqlite::ffi::ErrorCode;
    // 判定精度论证（v1.13.3 r2）：workflows 表中 `name` 是唯一 UNIQUE 列；其余
    // 约束仅 NOT NULL —— insert 恒供全列（Create 表单拒空名 + serde 恒非 NULL），
    // NOT NULL 不可达；update SET name 在 Edit 不改名前提下不触发 UNIQUE。故
    // 主码 ConstraintViolation 在实践上等价于"重名"。
    // 耦合注明：若未来升级为 extended_code == SQLITE_CONSTRAINT_UNIQUE 精确判定，
    // 下方单测的构造器须同步换结构体字面量 `Error { code, extended_code }`——
    // libsqlite3-sys 0.28 只有 `new(c_int)` 一个构造器且它把 extended_code 设为
    // result_code 本身（无 new_extended；字段 pub 可字面量构造）——见测试内钉死注释。
    if matches!(
        err,
        WorkflowStoreError::Sqlite(rusqlite::Error::SqliteFailure(e, _))
            if e.code == ErrorCode::ConstraintViolation
    ) {
        "名称已存在，请换一个"
    } else {
        "保存失败，请重试"
    }
}
