pub(crate) mod headless;
pub mod plan;

pub const DEFAULT_PLAN_FILENAME: &str = "LOOP_PLAN.md";
/// Interactive loops must terminate even when the user does not specify a bound.
pub const DEFAULT_TUI_MAX_ITERATIONS: u32 = 100;
/// How much of an iteration's response a restart round carries forward.
pub const SUMMARY_TRUNCATION_CHARS: usize = 1024;
