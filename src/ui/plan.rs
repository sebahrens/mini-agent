//! Ephemeral approval of an exact, successfully completed planning turn.
//! This state is never session data, a permission grant, or filesystem authority.
use std::io::Write;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::context::{ActiveContextSelection, ContextFiles, prompts};
use crate::paths::WorkspaceBinding;
use crate::permission::{SecurityMode, checker::PermCheck};
use crate::session::{MessageRole, Session};

pub(super) const MAX_PLAN_BYTES: usize = 64 * 1024;
const MAX_REVISION_BYTES: usize = 1024 * 1024;
pub(super) const QUESTION: &str = "Execute this plan? [Y] execute [N] keep planning [E] edit plan";

#[derive(Default)]
pub(crate) struct PlanState {
    pub run: Option<PlanRun>,
    pub pending: Option<PlanApproval>,
    pub editing: bool,
}

pub(crate) struct PlanRun {
    session_id: compact_str::CompactString,
    workspace: Arc<WorkspaceBinding>,
    selection: ActiveContextSelection,
    prompt: [u8; 32],
    active_prompt: [u8; 32],
    code_prompt: [u8; 32],
    active_mode: SecurityMode,
    user_mode: SecurityMode,
}

pub(crate) struct PlanApproval {
    captured: PlanRun,
    revision: [u8; 32],
    pub plan: String,
}

fn prompt_digest(content: &str) -> Option<[u8; 32]> {
    (content.len() <= MAX_PLAN_BYTES).then(|| Sha256::digest(content.as_bytes()).into())
}

impl PlanRun {
    pub fn capture(
        session: &Session,
        context: &ContextFiles,
        workspace: &Arc<WorkspaceBinding>,
        permission: &Option<PermCheck>,
        cfg: &crate::config::Config,
    ) -> Option<Self> {
        if cfg.chain.as_ref().is_some_and(|chain| !chain.plan_to_code) {
            return None;
        }
        let name = context.current_prompt_name.as_deref()?;
        let content = context.prompts.get(name)?;
        let directives = prompts::parse_directives(content);
        if directives.mode != Some("planwrite")
            || directives.next != Some(prompts::PromptNext::Code)
        {
            return None;
        }
        let checker = permission
            .as_ref()?
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Readonly can prevent the planwrite directive from applying. Retain
        // that stricter operator ceiling through approval as well.
        if checker.mode().privilege_rank() > SecurityMode::PlanWrite.privilege_rank() {
            return None;
        }
        workspace.validate().ok()?;
        Some(Self {
            session_id: session.id.clone(),
            workspace: workspace.clone(),
            selection: context.active_selection(),
            prompt: prompt_digest(content)?,
            active_prompt: prompt_digest(context.current_prompt.as_deref()?)?,
            code_prompt: prompt_digest(context.prompts.get("code")?)?,
            active_mode: checker.mode(),
            user_mode: checker.user_mode(),
        })
    }

    fn valid(
        &self,
        session: &Session,
        context: &ContextFiles,
        workspace: &Arc<WorkspaceBinding>,
        permission: &Option<PermCheck>,
    ) -> bool {
        let Some(checker) = permission else {
            return false;
        };
        let checker = checker.lock().unwrap_or_else(|e| e.into_inner());
        session.id == self.session_id
            && Arc::ptr_eq(workspace, &self.workspace)
            && workspace.validate().is_ok()
            && context.active_selection() == self.selection
            && context.current_prompt.as_deref().and_then(prompt_digest) == Some(self.active_prompt)
            && context
                .current_prompt_name
                .as_ref()
                .and_then(|name| context.prompts.get(name))
                .and_then(|content| prompt_digest(content))
                == Some(self.prompt)
            && context
                .prompts
                .get("code")
                .and_then(|content| prompt_digest(content))
                == Some(self.code_prompt)
            && checker.mode() == self.active_mode
            && checker.user_mode() == self.user_mode
    }

    pub fn complete(
        self,
        plan: &str,
        session: &Session,
        context: &ContextFiles,
        workspace: &Arc<WorkspaceBinding>,
        permission: &Option<PermCheck>,
    ) -> Result<PlanApproval, &'static str> {
        validate_plan(plan)?;
        if !self.valid(session, context, workspace, permission) {
            return Err("Planning turn changed; no execution approval offered");
        }
        Ok(PlanApproval {
            captured: self,
            revision: revision(session)?,
            plan: plan.to_owned(),
        })
    }
}

impl PlanApproval {
    pub fn valid(
        &self,
        session: &Session,
        context: &ContextFiles,
        workspace: &Arc<WorkspaceBinding>,
        permission: &Option<PermCheck>,
    ) -> bool {
        self.captured.valid(session, context, workspace, permission)
            && revision(session).ok() == Some(self.revision)
    }

    pub fn edit(&mut self, plan: &str) -> Result<(), &'static str> {
        validate_plan(plan)?;
        self.plan = plan.to_owned();
        Ok(())
    }

    pub fn continuation(&self) -> String {
        format!(
            "Execute this approved plan under the current permissions.\n\nApproved plan (literal text):\n{}",
            self.plan
        )
    }
}

fn validate_plan(plan: &str) -> Result<(), &'static str> {
    if plan.trim().is_empty() {
        Err("Empty planning response; no execution approval offered")
    } else if plan.len() > MAX_PLAN_BYTES {
        Err("Plan exceeds 64 KiB; shorten it before approving execution")
    } else {
        Ok(())
    }
}

/// Stream the relevant visible turn into a capped digest without copying tool
/// results. Titles and auxiliary accounting cannot invalidate or create approval.
fn revision(session: &Session) -> Result<[u8; 32], &'static str> {
    struct BoundedDigest {
        remaining: usize,
        digest: Sha256,
    }
    impl Write for BoundedDigest {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.remaining {
                return Err(std::io::Error::other("plan revision too large"));
            }
            self.remaining -= bytes.len();
            self.digest.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let start = session
        .messages
        .iter()
        .rposition(|message| message.role == MessageRole::User)
        .unwrap_or(0);
    let mut writer = BoundedDigest {
        remaining: MAX_REVISION_BYTES,
        digest: Sha256::new(),
    };
    serde_json::to_writer(
        &mut writer,
        &(
            session.messages.len(),
            session.compactions.len(),
            session.compacted_context(),
            &session.messages[start..],
        ),
    )
    .map_err(|_| "Planning history exceeds the approval bound; no execution approval offered")?;
    Ok(writer.digest.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{Action, PermissionConfig, PermissionConfigs};
    use std::sync::Mutex;

    #[test]
    fn approval_binds_exact_turn_workspace_prompt_and_mode_without_auxiliary_metadata() {
        let root = crate::tests::OwnedTestDirectory::new("plan-approval-binding");
        let bindings = [
            "ZS_CONFIG_DIR",
            "ZS_DATA_DIR",
            "ZS_LOCAL_DATA_DIR",
            "ZS_STATE_DIR",
            "ZS_CACHE_DIR",
            "ZS_CREDENTIALS_DIR",
        ]
        .map(|name| (name, Some(root.path().join(name).into_os_string())));
        let _environment = crate::tests::ScopedProcessEnv::set(&bindings);
        let workspace = Arc::new(WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let cfg = crate::config::Config::default();
        for user_mode in [
            SecurityMode::Standard,
            SecurityMode::ReadOnly,
            SecurityMode::Restrictive,
            SecurityMode::Guarded,
        ] {
            let permissions = PermissionConfigs::from(PermissionConfig {
                default: Some(Action::Deny),
                ..Default::default()
            });
            let permission = Some(Arc::new(Mutex::new(
                crate::permission::checker::PermissionChecker::new(
                    &permissions,
                    user_mode,
                    Some(workspace.root().to_owned()),
                    None,
                )
                .unwrap(),
            )));
            crate::ui::apply_prompt_mode("plan", &mut context, &permission);
            let mut session =
                Session::new("unused", "unused", 128_000, root.path().to_str().unwrap());
            session.add_message(MessageRole::User, "plan this");
            session.add_message(MessageRole::Assistant, "literal plan雪");
            let captured =
                PlanRun::capture(&session, &context, &workspace, &permission, &cfg).unwrap();
            let mut proposal = captured
                .complete(
                    "literal plan雪",
                    &session,
                    &context,
                    &workspace,
                    &permission,
                )
                .unwrap();
            assert!(proposal.valid(&session, &context, &workspace, &permission));
            session.generated_title = Some("unrelated title".into());
            session.total_cost += 0.5;
            assert!(proposal.valid(&session, &context, &workspace, &permission));
            proposal
                .edit("edited literal $(no execution)\n.plan !cmd")
                .unwrap();
            assert!(
                proposal
                    .continuation()
                    .ends_with("edited literal $(no execution)\n.plan !cmd")
            );
            assert!(!proposal.valid(
                &session,
                &context,
                &Arc::new(WorkspaceBinding::capture(root.path()).unwrap()),
                &permission
            ));
            let old = session.messages.last().unwrap().content.clone();
            session.messages.last_mut().unwrap().content = "different plan same length".into();
            assert!(!proposal.valid(&session, &context, &workspace, &permission));
            session.messages.last_mut().unwrap().content = old;
            let code = context.prompts.get("code").unwrap().clone();
            context
                .prompts
                .insert("code".into(), "changed code prompt".into());
            assert!(!proposal.valid(&session, &context, &workspace, &permission));
            context.prompts.insert("code".into(), code);
            let mut compacted = session.clone();
            compacted.add_message(MessageRole::System, "old recap");
            compacted.messages.rotate_right(1);
            compacted.compactions.push(
                serde_json::from_value(serde_json::json!({
                    "summary":"old recap", "first_kept_index":1,
                    "summarized_count":2,"token_savings":10,"created_at":"owned-recap"
                }))
                .unwrap(),
            );
            let compacted_plan =
                PlanRun::capture(&compacted, &context, &workspace, &permission, &cfg)
                    .unwrap()
                    .complete(
                        "literal plan雪",
                        &compacted,
                        &context,
                        &workspace,
                        &permission,
                    )
                    .unwrap();
            compacted.messages[0].content = "changed recap same turn".into();
            compacted.compactions[0].summary = "changed recap same turn".into();
            assert!(!compacted_plan.valid(&compacted, &context, &workspace, &permission));
            compacted.messages[0].content = "old recap".into();
            compacted.compactions[0].summary = "old recap".into();
            assert!(compacted_plan.valid(&compacted, &context, &workspace, &permission));
            let disabled = crate::config::Config {
                chain: Some(crate::config::types::ChainConfig {
                    plan_to_code: false,
                    ..Default::default()
                }),
                ..Default::default()
            };
            assert!(
                PlanRun::capture(&session, &context, &workspace, &permission, &disabled).is_none()
            );
            permission
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .restore_user_mode();
            assert_eq!(
                permission.as_ref().unwrap().lock().unwrap().mode(),
                user_mode
            );
        }
    }

    #[test]
    fn plan_and_revision_bounds_reject_instead_of_truncating() {
        assert!(validate_plan("").is_err());
        assert!(validate_plan(" \n").is_err());
        assert!(validate_plan(&"x".repeat(MAX_PLAN_BYTES)).is_ok());
        assert!(validate_plan(&"雪".repeat(MAX_PLAN_BYTES / 3 + 1)).is_err());
        let mut session = Session::new("unused", "unused", 128_000, "");
        session.add_message(MessageRole::User, "plan");
        session.add_message(MessageRole::ToolResult, &"x".repeat(MAX_REVISION_BYTES));
        assert!(revision(&session).is_err());
    }
}
