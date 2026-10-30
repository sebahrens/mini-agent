//! Turn-local detection of successful reads that return no new evidence.
//!
//! Observe completed output, never suppress a read before it can reveal a change.
//! Keep only bounded hashes; polling and arbitrary executable tools are excluded.
use std::collections::{HashMap, VecDeque};

use sha2::{Digest, Sha256};

const MAX_OBSERVATIONS: usize = 128;
const STOP_AFTER: u32 = 5;
pub(super) const STOP_REASON: &str = "tool-loop detected: repeated successful reads returned unchanged evidence; use the retained results or change the approach";

#[derive(Clone)]
struct Observation {
    output: [u8; 32],
    repetitions: u32,
}

#[derive(Clone, Default)]
pub(super) struct ReadLoopState {
    observations: HashMap<[u8; 32], Observation>,
    order: VecDeque<[u8; 32]>,
    stop: bool,
}

pub(super) fn is_evidence_read(tool: &str) -> bool {
    matches!(
        tool,
        "read"
            | "read_image"
            | "grep"
            | "find_files"
            | "list_dir"
            | "docs"
            | "skill_search"
            | "skill_list"
            | "todo_read"
    )
}

/// Unknown/executable tools can mutate even when they subsequently report an error.
/// Polling does not erase read history; the next completed read itself detects changed bytes.
pub(super) fn invalidates_evidence(tool: &str) -> bool {
    !is_evidence_read(tool)
        && !matches!(
            tool,
            "job_status"
                | "wait"
                | "web_search"
                | "web_fetch"
                | "ask_user_question"
                | "schedule_wakeup"
        )
}

impl ReadLoopState {
    pub(super) fn stop_requested(&self) -> bool {
        self.stop
    }

    pub(super) fn clear_after_mutation(&mut self) {
        *self = Self::default();
    }

    pub(super) fn observe(&mut self, fingerprint: [u8; 32], output: &str) {
        let output: [u8; 32] = Sha256::digest(output.as_bytes()).into();
        let repetitions = if let Some(previous) = self.observations.get_mut(&fingerprint) {
            if previous.output == output {
                previous.repetitions = previous.repetitions.saturating_add(1);
            } else {
                previous.output = output;
                previous.repetitions = 1;
                self.stop = false;
            }
            previous.repetitions
        } else {
            while self.order.len() >= MAX_OBSERVATIONS {
                if let Some(oldest) = self.order.pop_front() {
                    self.observations.remove(&oldest);
                }
            }
            self.order.push_back(fingerprint);
            self.observations.insert(
                fingerprint,
                Observation {
                    output,
                    repetitions: 1,
                },
            );
            self.stop = false;
            1
        };
        if repetitions >= STOP_AFTER {
            self.stop = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_reads_stop_but_changed_evidence_recovers() {
        let mut state = ReadLoopState::default();
        state.observe([1; 32], "first");
        state.observe([1; 32], "first");
        state.observe([1; 32], "first");
        assert!(!state.stop_requested());
        state.observe([1; 32], "first");
        state.observe([1; 32], "first");
        assert!(state.stop_requested());
        state.observe([1; 32], "changed");
        assert!(!state.stop_requested());
        state.clear_after_mutation();
        state.observe([1; 32], "changed");
    }

    #[test]
    fn interleaved_reads_are_detected_without_retaining_raw_evidence() {
        let mut state = ReadLoopState::default();
        for _ in 0..4 {
            state.observe([1; 32], "a");
            state.observe([2; 32], "b");
        }
        state.observe([1; 32], "a");
        assert!(state.stop_requested());
        state.observe([2; 32], "new b");
        assert!(!state.stop_requested());
    }

    #[test]
    fn state_is_bounded_and_polling_is_excluded() {
        let mut state = ReadLoopState::default();
        for n in 0u32..256 {
            let key = Sha256::digest(n.to_be_bytes()).into();
            state.observe(key, "evidence");
        }
        assert_eq!(state.observations.len(), MAX_OBSERVATIONS);
        assert_eq!(state.order.len(), MAX_OBSERVATIONS);
        for tool in [
            "shell",
            "bash",
            "job_status",
            "wait",
            "web_fetch",
            "web_search",
            "js",
            "mcp_tool",
        ] {
            assert!(!is_evidence_read(tool), "{tool}");
        }
        for tool in [
            "job_status",
            "wait",
            "web_search",
            "web_fetch",
            "schedule_wakeup",
        ] {
            assert!(!invalidates_evidence(tool), "{tool}");
        }
        assert!(invalidates_evidence("shell"));
        assert!(is_evidence_read("grep"));
    }
}
