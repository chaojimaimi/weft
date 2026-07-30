use super::{settings_enter_action, SettingsEnterAction};

#[test]
fn enter_drills_only_from_narrow_sidebar() {
    assert_eq!(
        settings_enter_action(true, false),
        SettingsEnterAction::DrillDown
    );
    assert_eq!(
        settings_enter_action(true, true),
        SettingsEnterAction::Apply
    );
    assert_eq!(
        settings_enter_action(false, false),
        SettingsEnterAction::Apply
    );
}
