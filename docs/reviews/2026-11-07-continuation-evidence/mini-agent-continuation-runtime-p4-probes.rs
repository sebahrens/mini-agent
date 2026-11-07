// Review-only unit probe; append to src/session/file_checkpoints.rs.
#[cfg(test)]
mod continuation_runtime_p4_probes {
    use super::*;

    #[test]
    fn continuation_p4_rewind_drops_the_retired_skip_reason() {
        let checkpoints = FileCheckpoints::default();
        {
            let mut state = checkpoints.edit();
            skip(&mut state, 1, SkipReason::ReadPolicy);
            skip(&mut state, 2, SkipReason::FileSize(12345));
        }
        checkpoints.retire_from(2);
        let summary = checkpoints.summary_from("session", 1);
        assert!(summary.contains("1 edit not restorable"), "{summary}");
        assert!(!summary.contains("12345-byte"),
            "the only surviving unavailable edit was READ-denied, but the summary names a retired size failure: {summary}");
    }
}
