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

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ChainDecision {
    Decline,
    Accept(Option<String>),
    NotChain,
}

#[allow(dead_code)]
pub fn parse_chain_decision(input: &str) -> ChainDecision {
    let trimmed = input.trim();
    let lower = trimmed.to_lowercase();

    if lower == "n" || lower == "no" {
        return ChainDecision::Decline;
    }

    if lower == "y" || lower == "yes" {
        return ChainDecision::Accept(None);
    }

    // Match "but <msg>", "b <msg>", "yes but <msg>", etc.
    for prefix in &["but ", "b ", "yes but ", "yes b ", "y but ", "y b "] {
        if lower.starts_with(prefix) {
            let extra = trimmed[prefix.len()..].trim().to_string();
            if extra.is_empty() {
                return ChainDecision::NotChain;
            }
            return ChainDecision::Accept(Some(extra));
        }
    }

    ChainDecision::NotChain
}
