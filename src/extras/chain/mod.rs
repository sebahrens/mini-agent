use crate::config::types::ChainConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainPhase {
    Brainstorm,
    Plan,
    Code,
}

impl ChainPhase {
    pub fn from_prompt_name(name: &str) -> Option<Self> {
        match name {
            "brainstorm" => Some(ChainPhase::Brainstorm),
            "plan" => Some(ChainPhase::Plan),
            "code" => Some(ChainPhase::Code),
            _ => None,
        }
    }

    pub fn next_prompt_name(self) -> &'static str {
        match self {
            ChainPhase::Brainstorm => "plan",
            ChainPhase::Plan => "code",
            ChainPhase::Code => "review",
        }
    }

    pub fn transition_message(self) -> &'static str {
        match self {
            ChainPhase::Brainstorm => {
                "Based on the brainstorm above, create a detailed implementation plan."
            }
            ChainPhase::Plan => "Implement the plan above. Write code, tests, and verify.",
            ChainPhase::Code => {
                "Review the changes for correctness, design, testing, and security."
            }
        }
    }

    pub fn is_enabled(self, cfg: &ChainConfig) -> bool {
        match self {
            ChainPhase::Brainstorm => cfg.brainstorm_to_plan,
            ChainPhase::Plan => cfg.plan_to_code,
            ChainPhase::Code => cfg.code_to_review,
        }
    }

    pub fn chain_label(self) -> &'static str {
        match self {
            ChainPhase::Brainstorm => "Continue to plan? [Y/N/B]",
            ChainPhase::Plan => "Continue to code? [Y/N/B]",
            ChainPhase::Code => "Run /review? [Y/N/B]",
        }
    }
}
