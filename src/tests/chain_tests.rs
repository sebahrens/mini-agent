use crate::config::types::ChainConfig;
use crate::extras::chain::ChainPhase;

#[test]
fn test_phase_from_prompt_name() {
    assert_eq!(
        ChainPhase::from_prompt_name("brainstorm"),
        Some(ChainPhase::Brainstorm)
    );
    assert_eq!(ChainPhase::from_prompt_name("plan"), Some(ChainPhase::Plan));
    assert_eq!(ChainPhase::from_prompt_name("code"), Some(ChainPhase::Code));
    assert_eq!(ChainPhase::from_prompt_name("review"), None);
    assert_eq!(ChainPhase::from_prompt_name("ask"), None);
    assert_eq!(ChainPhase::from_prompt_name(""), None);
}

#[test]
fn test_next_prompt_name() {
    assert_eq!(ChainPhase::Brainstorm.next_prompt_name(), "plan");
    assert_eq!(ChainPhase::Plan.next_prompt_name(), "code");
    assert_eq!(ChainPhase::Code.next_prompt_name(), "review");
}

#[test]
fn test_transition_messages_are_not_empty() {
    assert!(!ChainPhase::Brainstorm.transition_message().is_empty());
    assert!(!ChainPhase::Plan.transition_message().is_empty());
    assert!(!ChainPhase::Code.transition_message().is_empty());
}

#[test]
fn test_chain_config_defaults() {
    let cfg = ChainConfig::default();
    assert!(cfg.brainstorm_to_plan);
    assert!(cfg.plan_to_code);
    assert!(!cfg.code_to_review);
}

#[test]
fn test_is_enabled_default_config() {
    let cfg = ChainConfig::default();
    assert!(ChainPhase::Brainstorm.is_enabled(&cfg));
    assert!(ChainPhase::Plan.is_enabled(&cfg));
    assert!(!ChainPhase::Code.is_enabled(&cfg));
}

#[test]
fn test_is_enabled_all_off() {
    let cfg = ChainConfig {
        brainstorm_to_plan: false,
        plan_to_code: false,
        code_to_review: false,
    };
    assert!(!ChainPhase::Brainstorm.is_enabled(&cfg));
    assert!(!ChainPhase::Plan.is_enabled(&cfg));
    assert!(!ChainPhase::Code.is_enabled(&cfg));
}

#[test]
fn test_is_enabled_all_on() {
    let cfg = ChainConfig {
        brainstorm_to_plan: true,
        plan_to_code: true,
        code_to_review: true,
    };
    assert!(ChainPhase::Brainstorm.is_enabled(&cfg));
    assert!(ChainPhase::Plan.is_enabled(&cfg));
    assert!(ChainPhase::Code.is_enabled(&cfg));
}

#[test]
fn test_is_enabled_only_review() {
    let cfg = ChainConfig {
        brainstorm_to_plan: false,
        plan_to_code: false,
        code_to_review: true,
    };
    assert!(!ChainPhase::Brainstorm.is_enabled(&cfg));
    assert!(!ChainPhase::Plan.is_enabled(&cfg));
    assert!(ChainPhase::Code.is_enabled(&cfg));
}

#[test]
fn test_full_progression_default_config() {
    // brainstorm → plan → code, code→review off by default
    let cfg = ChainConfig::default();

    let phase = ChainPhase::from_prompt_name("brainstorm").unwrap();
    assert_eq!(phase, ChainPhase::Brainstorm);
    assert!(phase.is_enabled(&cfg));
    assert_eq!(phase.next_prompt_name(), "plan");

    let phase = ChainPhase::from_prompt_name("plan").unwrap();
    assert_eq!(phase, ChainPhase::Plan);
    assert!(phase.is_enabled(&cfg));
    assert_eq!(phase.next_prompt_name(), "code");

    let phase = ChainPhase::from_prompt_name("code").unwrap();
    assert_eq!(phase, ChainPhase::Code);
    assert!(!phase.is_enabled(&cfg));
    assert_eq!(phase.next_prompt_name(), "review");
}

#[test]
fn test_full_progression_all_enabled() {
    let cfg = ChainConfig {
        brainstorm_to_plan: true,
        plan_to_code: true,
        code_to_review: true,
    };

    for name in &["brainstorm", "plan", "code"] {
        let phase = ChainPhase::from_prompt_name(name).unwrap();
        assert!(phase.is_enabled(&cfg));
    }
}
