//! Runtime-only choices belonging to one ACP session. Transcript imports never
//! construct this state; each admitted turn builds current configured policy.

use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{SessionMode, SessionModeState};

use crate::permission::{SecurityMode, checker::PermCheck};

const MAX_RULES: usize = 128;
const MAX_BYTES: usize = 64 * 1024;
const MAX_RULE_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Choice {
    Allow(Vec<(String, String)>),
    Deny(String, String),
}

impl Choice {
    fn entries(&self) -> usize {
        match self {
            Self::Allow(entries) => entries.len(),
            Self::Deny(..) => 1,
        }
    }

    fn bytes(&self) -> Option<usize> {
        let bounded = |tool: &String, input: &String| {
            let size = tool.len().checked_add(input.len())?;
            (size <= MAX_RULE_BYTES).then_some(size)
        };
        match self {
            Self::Allow(entries) if !entries.is_empty() => {
                entries.iter().try_fold(0usize, |sum, (tool, input)| {
                    sum.checked_add(bounded(tool, input)?)
                })
            }
            Self::Allow(_) => None,
            Self::Deny(tool, input) => bounded(tool, input),
        }
    }
}

#[derive(Default)]
struct State {
    mode: Option<SecurityMode>,
    enabled: bool,
    choices: Vec<Choice>,
    rules: usize,
    bytes: usize,
}

#[derive(Clone, Default)]
pub(super) struct SessionPolicy(Arc<Mutex<State>>);

impl SessionPolicy {
    pub(super) fn new(mode: SecurityMode, enabled: bool) -> Self {
        Self(Arc::new(Mutex::new(State {
            mode: Some(mode),
            enabled,
            ..Default::default()
        })))
    }

    pub(super) fn mode(&self) -> Option<SecurityMode> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .mode
    }

    pub(super) fn modes(&self) -> Option<SessionModeState> {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.enabled.then(|| {
            SessionModeState::new(
                state.mode.expect("enabled policy has a mode").to_string(),
                SecurityMode::all()
                    .map(|mode| {
                        SessionMode::new(mode.to_string(), mode.to_string())
                            .description(mode.description())
                    })
                    .collect(),
            )
        })
    }

    pub(super) fn set_mode(&self, name: &str) -> Result<(), &'static str> {
        let mode = SecurityMode::from_str(name).ok_or("unknown ACP permission mode")?;
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if !state.enabled {
            return Err("permission mode switching is disabled for this session");
        }
        state.mode = Some(mode);
        Ok(())
    }

    fn fits(state: &State, choice: &Choice) -> bool {
        if state.choices.contains(choice) {
            return true;
        }
        choice.bytes().is_some_and(|bytes| {
            state.rules.saturating_add(choice.entries()) <= MAX_RULES
                && state.bytes.saturating_add(bytes) <= MAX_BYTES
        })
    }

    pub(super) fn can_record(&self, choice: &Choice) -> bool {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.enabled && Self::fits(&state, choice)
    }

    /// Called only after delivery of an exact currently advertised selection.
    /// The per-turn bridge is the sole choice writer; mode changes require idle.
    pub(super) fn record(&self, choice: Choice) -> Result<(), &'static str> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if !state.enabled || !Self::fits(&state, &choice) {
            return Err("ACP session permission choices exceed their bounded capacity");
        }
        if !state.choices.contains(&choice) {
            state.bytes += choice.bytes().expect("bounded choice");
            state.rules += choice.entries();
            state.choices.push(choice);
        }
        Ok(())
    }

    pub(super) fn apply(&self, permission: &Option<PermCheck>) {
        let Some(permission) = permission else { return };
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let mut checker = permission.lock().unwrap_or_else(|error| error.into_inner());
        for choice in &state.choices {
            match choice {
                Choice::Allow(entries) => {
                    for (tool, pattern) in entries {
                        checker.add_session_allowlist(tool.clone(), pattern);
                    }
                }
                Choice::Deny(tool, input) => checker.add_session_exact_deny(tool, input),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{
        PermissionConfigs,
        checker::{CheckResult, PermissionChecker},
    };

    fn checker(policy: &SessionPolicy, root: &std::path::Path) -> Option<PermCheck> {
        let checker = PermissionChecker::new(
            &PermissionConfigs::default(),
            policy.mode().unwrap(),
            Some(root.to_path_buf()),
            None,
        )
        .unwrap();
        let result = Some(Arc::new(Mutex::new(checker)));
        policy.apply(&result);
        result
    }

    #[test]
    fn acp53_session_choices_survive_checker_rebuild_and_exact_denies_win() {
        let root = crate::tests::OwnedTestDirectory::new("acp53-runtime-policy");
        let target = root
            .path()
            .join("literal*.txt")
            .to_string_lossy()
            .into_owned();
        let policy = SessionPolicy::new(SecurityMode::Guarded, true);
        let scope =
            crate::permission::checker::exact_allow_scope_for_input("write", &target).unwrap();
        policy
            .record(Choice::Allow(vec![("write".into(), scope)]))
            .unwrap();
        let first = checker(&policy, root.path()).unwrap();
        assert_eq!(
            first.lock().unwrap().check_bound_path("write", &target),
            CheckResult::Allowed
        );
        policy
            .record(Choice::Deny("write".into(), target.clone()))
            .unwrap();
        policy.set_mode("yolo").unwrap();
        let next = checker(&policy, root.path()).unwrap();
        assert!(matches!(
            next.lock().unwrap().check_bound_path("write", &target),
            CheckResult::Denied(_)
        ));
        assert!(matches!(
            next.lock()
                .unwrap()
                .check_bound_path("js/write_file", &target),
            CheckResult::Denied(_)
        ));
        let neighbor = root
            .path()
            .join("literal-other.txt")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            next.lock().unwrap().check_bound_path("write", &neighbor),
            CheckResult::Allowed
        );
        assert!(
            next.lock()
                .unwrap()
                .path_deny_probe("write")
                .is_denied(std::path::Path::new(&target))
        );
    }

    #[test]
    fn acp53_session_shell_denials_are_literal_and_do_not_widen_to_siblings() {
        let root = crate::tests::OwnedTestDirectory::new("acp53-literal-deny");
        let policy = SessionPolicy::new(SecurityMode::Yolo, true);
        policy
            .record(Choice::Deny("bash".into(), "echo *\nnext?".into()))
            .unwrap();
        let permission = checker(&policy, root.path()).unwrap();
        let mut checker = permission.lock().unwrap();
        assert!(matches!(
            checker.check("shell", "echo *\nnext?"),
            CheckResult::Denied(_)
        ));
        assert!(matches!(
            checker.check("bash", "echo *\nnext?"),
            CheckResult::Denied(_)
        ));
        assert!(!matches!(
            checker.check("shell", "echo hello\nnext1"),
            CheckResult::Denied(_)
        ));
        assert!(!matches!(
            checker.check("shell", "echo *\nnext?\n"),
            CheckResult::Denied(_)
        ));
    }

    #[test]
    fn acp53_session_policy_bounds_choices_and_rejects_unknown_or_disabled_modes() {
        let policy = SessionPolicy::new(SecurityMode::ReadOnly, true);
        assert!(policy.set_mode("YOLO").is_err());
        assert_eq!(policy.mode(), Some(SecurityMode::ReadOnly));
        assert_eq!(policy.modes().unwrap().available_modes.len(), 6);
        let oversized = Choice::Deny("shell".into(), "x".repeat(MAX_RULE_BYTES));
        assert!(!policy.can_record(&oversized));
        assert!(policy.record(oversized).is_err());
        for index in 0..MAX_RULES {
            policy
                .record(Choice::Deny("shell".into(), index.to_string()))
                .unwrap();
        }
        assert!(
            policy
                .record(Choice::Deny("shell".into(), "next".into()))
                .is_err()
        );
        assert!(
            policy
                .record(Choice::Deny("shell".into(), "0".into()))
                .is_ok()
        );
        let disabled = SessionPolicy::new(SecurityMode::Standard, false);
        assert!(disabled.modes().is_none());
        assert!(disabled.set_mode("readonly").is_err());
        assert!(!disabled.can_record(&Choice::Deny("shell".into(), "x".into())));
    }
}
