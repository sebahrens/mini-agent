use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::context::ContextFiles;
use crate::permission::checker::{PermCheck, PermissionChecker};
use crate::permission::{PermissionConfigs, SecurityMode};
#[cfg(feature = "git-worktree")]
use crate::ui::apply_current_prompt_mode;
use crate::ui::{PromptModeOutcome, apply_main_agent, apply_prompt_mode};

fn make_context(prompts: &[(&str, &str)]) -> ContextFiles {
    ContextFiles {
        workspace_root: std::env::current_dir().unwrap(),
        agents: None,
        prompts: prompts
            .iter()
            .map(|(name, content)| (name.to_string(), content.to_string()))
            .collect::<HashMap<_, _>>(),
        current_prompt: None,
        current_prompt_name: None,
        agent_definitions: HashMap::new(),
        current_agent_name: None,
        current_agent_explicit: false,
        themes: HashMap::new(),
        current_theme_name: None,
        extra_files: Vec::new(),
        extra_file_contents: HashMap::new(),
        one_shot_restore: None,
        chain_declined: Vec::new(),
        #[cfg(feature = "memory")]
        memory: None,
        #[cfg(feature = "archmd")]
        architecture: None,
    }
}

fn add_agent(context: &mut ContextFiles, name: &str, prompt: &str, mode: Option<&str>) {
    context.agent_definitions.insert(
        name.to_string(),
        crate::context::agents::AgentDefinition::for_test(prompt, mode),
    );
}

fn make_perm(mode: SecurityMode) -> PermCheck {
    Arc::new(Mutex::new(
        PermissionChecker::new(&PermissionConfigs::default(), mode, None, None)
            .expect("valid permission test configuration"),
    ))
}

fn current_mode(perm: &PermCheck) -> SecurityMode {
    perm.lock().unwrap_or_else(|e| e.into_inner()).mode()
}

#[cfg(feature = "memory")]
#[test]
fn memory_refresh_only_marks_byte_changes() {
    let mut context = make_context(&[]);

    assert!(context.replace_memory_if_changed(Some("<memory>one</memory>".to_string())));
    assert!(!context.replace_memory_if_changed(Some("<memory>one</memory>".to_string())));
    assert!(context.replace_memory_if_changed(Some("<memory>two</memory>".to_string())));
    assert!(context.replace_memory_if_changed(None));
    assert!(!context.replace_memory_if_changed(None));
}

#[test]
fn prompt_without_directive_keeps_content_and_mode_untouched() {
    let mut context = make_context(&[("code", "You are a coder.")]);
    let perm = make_perm(SecurityMode::Standard);

    let outcome = apply_prompt_mode("code", &mut context, &Some(perm.clone()));

    assert_eq!(outcome, PromptModeOutcome::None);
    assert_eq!(context.current_prompt.as_deref(), Some("You are a coder."));
    assert_eq!(context.current_prompt_name.as_deref(), Some("code"));
    assert_eq!(current_mode(&perm), SecurityMode::Standard);
}

#[test]
fn mode_directive_is_stripped_and_applied() {
    let mut context = make_context(&[("review", "%%mode=readonly\nReview the code.")]);
    let perm = make_perm(SecurityMode::Standard);

    let outcome = apply_prompt_mode("review", &mut context, &Some(perm.clone()));

    assert_eq!(outcome, PromptModeOutcome::Applied(SecurityMode::ReadOnly));
    assert_eq!(context.current_prompt.as_deref(), Some("Review the code."));
    assert_eq!(context.current_prompt_name.as_deref(), Some("review"));
    assert_eq!(current_mode(&perm), SecurityMode::ReadOnly);
}

#[test]
fn prompt_mode_composes_security_and_main_agent_directives_in_either_order() {
    let mut context = make_context(&[(
        "review",
        "%%agent=rust-review\n%%mode=readonly\nReview the code.",
    )]);
    add_agent(&mut context, "rust-review", "You are a reviewer.", None);
    let perm = make_perm(SecurityMode::Standard);

    let outcome = apply_prompt_mode("review", &mut context, &Some(perm.clone()));

    assert_eq!(outcome, PromptModeOutcome::Applied(SecurityMode::ReadOnly));
    assert_eq!(context.current_prompt.as_deref(), Some("Review the code."));
    assert_eq!(context.current_agent_name.as_deref(), Some("rust-review"));
}

#[test]
fn persona_default_mode_and_selection_snapshot_are_composable() {
    let mut context = make_context(&[("review", "%%agent=other\nReview the code.")]);
    add_agent(
        &mut context,
        "rust-review",
        "You are a reviewer.",
        Some("review"),
    );
    add_agent(&mut context, "other", "You are another reviewer.", None);

    let outcome = apply_main_agent("rust-review", &mut context, &None).unwrap();
    assert_eq!(outcome.default_prompt.as_deref(), Some("review"));
    assert!(outcome.prompt_applied);
    assert_eq!(context.current_prompt.as_deref(), Some("Review the code."));
    assert_eq!(context.current_agent_name.as_deref(), Some("rust-review"));
    let before = context.active_selection();
    let _ = context.activate_prompt("review");
    assert_eq!(context.current_agent_name.as_deref(), Some("other"));
    context.restore_selection(before);
    assert_eq!(context.current_agent_name.as_deref(), Some("rust-review"));
}

#[test]
fn selection_snapshot_restores_explicit_persona_without_a_prompt() {
    let mut context = make_context(&[("review", "%%agent=other\nReview the code.")]);
    add_agent(&mut context, "rust-review", "You are a reviewer.", None);
    add_agent(&mut context, "other", "You are another reviewer.", None);
    context.activate_agent("rust-review").unwrap();

    let before = context.active_selection();
    let _ = context.activate_prompt("review");
    assert_eq!(context.current_agent_name.as_deref(), Some("other"));

    context.restore_selection(before);
    assert!(context.current_prompt.is_none());
    assert!(context.current_prompt_name.is_none());
    assert_eq!(context.current_agent_name.as_deref(), Some("rust-review"));
    assert!(context.current_agent_explicit);
}

#[cfg(feature = "git-worktree")]
#[test]
fn explicit_main_agent_survives_current_prompt_reload() {
    let mut context = make_context(&[("review", "%%agent=other\nReview the code.")]);
    add_agent(&mut context, "rust-review", "You are a reviewer.", None);
    add_agent(&mut context, "other", "You are another reviewer.", None);
    let _ = apply_prompt_mode("review", &mut context, &None);
    context.activate_agent("rust-review").unwrap();

    apply_current_prompt_mode(&mut context, &None);

    assert_eq!(context.current_agent_name.as_deref(), Some("rust-review"));
    assert!(context.current_agent_explicit);
    assert_eq!(context.current_prompt.as_deref(), Some("Review the code."));
}

#[test]
fn unavailable_prompt_persona_is_stripped_and_clears_stale_selection() {
    let mut context = make_context(&[("review", "%%agent=missing\nReview the code.")]);
    add_agent(&mut context, "old", "Old persona", None);
    context.current_agent_name = Some("old".into());

    let _ = apply_prompt_mode("review", &mut context, &None);

    assert_eq!(context.current_prompt.as_deref(), Some("Review the code."));
    assert!(context.current_agent_name.is_none());
}

#[test]
fn last_user_mode_restores_the_user_selected_mode() {
    let mut context = make_context(&[
        ("review", "%%mode=readonly\nReview the code."),
        ("code", "%%mode=last_user_mode\nYou are a coder."),
    ]);
    let perm = make_perm(SecurityMode::Guarded);

    // A prompt-imposed mode must not clobber the user-selected mode...
    apply_prompt_mode("review", &mut context, &Some(perm.clone()));
    assert_eq!(current_mode(&perm), SecurityMode::ReadOnly);

    // ...so a `last_user_mode` prompt restores it afterwards.
    let outcome = apply_prompt_mode("code", &mut context, &Some(perm.clone()));

    assert_eq!(outcome, PromptModeOutcome::RestoredUserMode);
    assert_eq!(current_mode(&perm), SecurityMode::Guarded);
    assert_eq!(context.current_prompt.as_deref(), Some("You are a coder."));
    assert_eq!(context.current_prompt_name.as_deref(), Some("code"));
}

#[test]
fn unknown_prompt_is_a_noop() {
    let mut context = make_context(&[("code", "You are a coder.")]);
    let perm = make_perm(SecurityMode::Standard);

    let outcome = apply_prompt_mode("missing", &mut context, &Some(perm.clone()));

    assert_eq!(outcome, PromptModeOutcome::None);
    assert!(context.current_prompt.is_none());
    assert!(context.current_prompt_name.is_none());
    assert_eq!(current_mode(&perm), SecurityMode::Standard);
}

#[test]
fn unrecognized_mode_strips_directive_but_keeps_current_mode() {
    let mut context = make_context(&[("weird", "%%mode=bogus\nBody.")]);
    let perm = make_perm(SecurityMode::Standard);

    let outcome = apply_prompt_mode("weird", &mut context, &Some(perm.clone()));

    assert_eq!(outcome, PromptModeOutcome::None);
    assert_eq!(context.current_prompt.as_deref(), Some("Body."));
    assert_eq!(context.current_prompt_name.as_deref(), Some("weird"));
    assert_eq!(current_mode(&perm), SecurityMode::Standard);
}

#[test]
fn without_permission_checker_prompt_is_still_selected() {
    let mut context = make_context(&[("review", "%%mode=readonly\nReview the code.")]);

    let outcome = apply_prompt_mode("review", &mut context, &None);

    assert_eq!(outcome, PromptModeOutcome::None);
    assert_eq!(context.current_prompt.as_deref(), Some("Review the code."));
    assert_eq!(context.current_prompt_name.as_deref(), Some("review"));
}

#[test]
fn directive_cannot_raise_mode_above_user_mode() {
    let mut context = make_context(&[("escalate", "%%mode=yolo\nDo anything.")]);
    let perm = make_perm(SecurityMode::Guarded);

    let outcome = apply_prompt_mode("escalate", &mut context, &Some(perm.clone()));

    // The directive is stripped and the prompt is selected, but the mode is
    // left exactly where the user put it.
    assert_eq!(outcome, PromptModeOutcome::None);
    assert_eq!(context.current_prompt.as_deref(), Some("Do anything."));
    assert_eq!(context.current_prompt_name.as_deref(), Some("escalate"));
    assert_eq!(current_mode(&perm), SecurityMode::Guarded);
}

#[test]
fn directive_cannot_raise_mode_from_read_only() {
    let mut context = make_context(&[("standardize", "%%mode=standard\nBody.")]);
    let perm = make_perm(SecurityMode::ReadOnly);

    let outcome = apply_prompt_mode("standardize", &mut context, &Some(perm.clone()));

    assert_eq!(outcome, PromptModeOutcome::None);
    assert_eq!(current_mode(&perm), SecurityMode::ReadOnly);
}

#[test]
fn directive_may_lower_mode_and_last_user_mode_restores_it() {
    let mut context = make_context(&[
        ("lock", "%%mode=readonly\nBody."),
        ("back", "%%mode=last_user_mode\nBody."),
    ]);
    let perm = make_perm(SecurityMode::Yolo);

    assert_eq!(
        apply_prompt_mode("lock", &mut context, &Some(perm.clone())),
        PromptModeOutcome::Applied(SecurityMode::ReadOnly)
    );
    assert_eq!(current_mode(&perm), SecurityMode::ReadOnly);
    assert_eq!(
        apply_prompt_mode("back", &mut context, &Some(perm.clone())),
        PromptModeOutcome::RestoredUserMode
    );
    assert_eq!(current_mode(&perm), SecurityMode::Yolo);
}
