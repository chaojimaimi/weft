#[derive(Debug, PartialEq, Eq)]
pub(super) enum ProfileRestoreTarget {
    Keep,
    Base,
    Named(String),
    MissingUseBase(String),
    MissingAlreadyBase(String),
}

pub(super) fn profile_restore_target(
    requested: Option<&str>,
    active: Option<&str>,
    requested_exists: bool,
) -> ProfileRestoreTarget {
    match requested {
        None if active.is_none() => ProfileRestoreTarget::Keep,
        None => ProfileRestoreTarget::Base,
        Some(requested) if !requested_exists && active.is_some() => {
            ProfileRestoreTarget::MissingUseBase(requested.to_string())
        }
        Some(requested) if !requested_exists => {
            ProfileRestoreTarget::MissingAlreadyBase(requested.to_string())
        }
        Some(requested) if active == Some(requested) => ProfileRestoreTarget::Keep,
        Some(requested) => ProfileRestoreTarget::Named(requested.to_string()),
    }
}

#[derive(Debug)]
pub(crate) struct WorkspaceRestoreOutcome {
    warning: Option<WorkspaceRestoreWarning>,
}

impl WorkspaceRestoreOutcome {
    pub(super) fn new(warning: Option<WorkspaceRestoreWarning>) -> Self {
        Self { warning }
    }

    pub(crate) fn warning(&self) -> Option<&WorkspaceRestoreWarning> {
        self.warning.as_ref()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceRestoreWarning {
    ProfileNotFound { profile: String },
}

impl std::fmt::Display for WorkspaceRestoreWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProfileNotFound { profile } => {
                write!(f, "workspace profile '{profile}' not found, using base")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_restore_target_covers_base_named_missing_and_noop() {
        assert_eq!(
            profile_restore_target(None, Some("work"), false),
            ProfileRestoreTarget::Base
        );
        assert_eq!(
            profile_restore_target(None, None, false),
            ProfileRestoreTarget::Keep
        );
        assert_eq!(
            profile_restore_target(Some("work"), Some("work"), true),
            ProfileRestoreTarget::Keep
        );
        assert_eq!(
            profile_restore_target(Some("demo"), Some("work"), true),
            ProfileRestoreTarget::Named("demo".into())
        );
        assert_eq!(
            profile_restore_target(Some("missing"), Some("work"), false),
            ProfileRestoreTarget::MissingUseBase("missing".into())
        );
        assert_eq!(
            profile_restore_target(Some("missing"), None, false),
            ProfileRestoreTarget::MissingAlreadyBase("missing".into())
        );
    }
}
