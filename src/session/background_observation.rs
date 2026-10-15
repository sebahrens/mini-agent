//! Runtime provenance for shared observations made by admitted background jobs.
//! These values are never serialized and are not effect/permission grants.

use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

/// One logical client/session lifetime. The pool retains this value; stores
/// retain only a Weak pointer, so effects cannot keep their owner alive.
#[derive(Debug)]
pub(crate) struct ObservationOwner {
    generation: AtomicU64,
    accepting: AtomicBool,
    retired: AtomicBool,
}

impl ObservationOwner {
    pub(crate) fn bind(session: &mut super::Session) -> anyhow::Result<Arc<Self>> {
        let owner = session
            .background_observations
            .clone()
            .unwrap_or_else(Self::new);
        anyhow::ensure!(
            !owner.retired.load(Ordering::Acquire),
            "background owner already retired"
        );
        session.spend_account.bind_observation_owner(&owner)?;
        session.file_checkpoints.bind_observation_owner(&owner)?;
        session.background_history.bind_observation_owner(&owner)?;
        session.background_observations = Some(owner.clone());
        Ok(owner)
    }
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            generation: AtomicU64::new(1),
            accepting: AtomicBool::new(true),
            retired: AtomicBool::new(false),
        })
    }

    /// Only the actual pool admission creates this pair, after ordinary Task
    /// permission/profile and captured session/store identity checks.
    pub(crate) fn admit(self: &Arc<Self>) -> anyhow::Result<(JobObservation, ObservationLifetime)> {
        anyhow::ensure!(
            self.accepting.load(Ordering::Acquire),
            "background owner is closing"
        );
        let job = Arc::new(Job {
            owner: Arc::downgrade(self),
            generation: self.generation.load(Ordering::Acquire),
            settled: AtomicBool::new(false),
        });
        Ok((JobObservation(Arc::clone(&job)), ObservationLifetime(job)))
    }

    pub(crate) fn stop_admission(&self) {
        self.accepting.store(false, Ordering::Release);
    }

    /// Called only after every old job and its ledger/effect children joined.
    /// Revocation is not allowed to erase a partial effect or paid receipt.
    pub(crate) fn retire(&self) {
        self.stop_admission();
        self.retired.store(true, Ordering::Release);
        // A closed owner is never reused. A saturated generation remains
        // closed rather than rolling over into an old imported-looking value.
        let _ = self
            .generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            });
    }
}

#[derive(Debug)]
struct Job {
    owner: Weak<ObservationOwner>,
    generation: u64,
    settled: AtomicBool,
}

#[derive(Clone, Debug)]
pub(crate) struct JobObservation(Arc<Job>);

/// Retained by exact job cleanup until its model, effects and receipt flushes
/// actually retire. Cancellation alone does not complete this lifetime.
pub(crate) struct ObservationLifetime(Arc<Job>);

impl Drop for ObservationLifetime {
    fn drop(&mut self) {
        self.0.settled.store(true, Ordering::Release);
    }
}

impl JobObservation {
    pub(crate) fn same_job(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn active_for(&self, owner: &Arc<ObservationOwner>) -> bool {
        self.authorizes(&Arc::downgrade(owner))
    }
    fn authorizes(&self, expected: &Weak<ObservationOwner>) -> bool {
        !self.0.settled.load(Ordering::Acquire)
            && self.0.owner.upgrade().is_some_and(|owner| {
                !owner.retired.load(Ordering::Acquire)
                    && Weak::ptr_eq(&self.0.owner, expected)
                    && owner.generation.load(Ordering::Acquire) == self.0.generation
            })
    }
}

/// Every change to serialized store data advances this value while holding
/// that store's existing mutex. Unattributed changes can never be laundered by
/// a later valid background receipt. There is no retained event/ID registry.
#[derive(Debug, Default, Clone)]
pub(crate) struct ObservationStamp {
    owner: Weak<ObservationOwner>,
    generation: u64,
    sequence: u64,
    unattributed: u64,
    overflowed: bool,
}

impl ObservationStamp {
    pub(crate) fn bind(&mut self, owner: &Arc<ObservationOwner>) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.owner.strong_count() == 0 || self.owner.ptr_eq(&Arc::downgrade(owner)),
            "shared observation store belongs to another owner"
        );
        // Rebinding an expired owner is itself an unattributed transition;
        // previous native baselines cannot authorize the replacement pool.
        if !self.owner.ptr_eq(&Arc::downgrade(owner)) {
            self.changed(None);
            self.owner = Arc::downgrade(owner);
            self.generation = owner.generation.load(Ordering::Acquire);
        }
        Ok(())
    }

    pub(crate) fn changed(&mut self, job: Option<&JobObservation>) {
        match self.sequence.checked_add(1) {
            Some(next) => self.sequence = next,
            None => self.overflowed = true,
        }
        if job.is_none_or(|job| !job.authorizes(&self.owner)) {
            match self.unattributed.checked_add(1) {
                Some(next) => self.unattributed = next,
                None => self.overflowed = true,
            }
        }
    }

    pub(crate) fn validates_since(&self, previous: &Self, owner: &Arc<ObservationOwner>) -> bool {
        !self.overflowed
            && !previous.overflowed
            && !owner.retired.load(Ordering::Acquire)
            && self.generation == owner.generation.load(Ordering::Acquire)
            && self.generation == previous.generation
            && self.owner.ptr_eq(&Arc::downgrade(owner))
            && previous.owner.ptr_eq(&self.owner)
            && self.sequence >= previous.sequence
            && self.unattributed == previous.unattributed
    }
}

/// Exact old data and runtime identities beside a native publication's whole
/// Session fingerprint. Never Deserialize; the detached copies grant nothing.
#[derive(Debug, Clone)]
pub(super) struct SharedObservations {
    pool: Option<Weak<crate::extras::subagents::background::Pool>>,
    owner: Weak<ObservationOwner>,
    account: crate::spend::AccountObservation,
    files: super::file_checkpoints::FileObservation,
    history: crate::extras::subagents::background::HistoryObservation,
}
impl SharedObservations {
    pub(super) fn capture(session: &super::Session) -> Option<Self> {
        let owner = Arc::downgrade(session.background_observations.as_ref()?);
        // Fixed lock order: Account -> Checkpoints -> Task history. Mutators
        // never hold two of these locks. This is one coherent observation cut,
        // not a completed task paired with an earlier missing receipt/preimage.
        Some(session.spend_account.with_observation_snapshot(|account| {
            session.file_checkpoints.with_observation_snapshot(|files| {
                session
                    .background_history
                    .with_observation_snapshot(|history| Self {
                        pool: session.background_pool.as_ref().map(Arc::downgrade),
                        owner,
                        account,
                        files,
                        history,
                    })
            })
        }))
    }
    pub(super) fn frozen_session(&self, session: &super::Session) -> super::Session {
        let mut frozen = session.clone();
        frozen.spend_account = self.account.frozen();
        frozen.file_checkpoints = self.files.frozen();
        frozen.background_history = self.history.frozen();
        frozen
    }
    pub(super) fn current_since(&self, session: &super::Session) -> anyhow::Result<Self> {
        let owner = session
            .background_observations
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("background observation owner is absent"))?;
        anyhow::ensure!(
            self.owner.ptr_eq(&Arc::downgrade(owner)),
            "background observation owner was replaced"
        );
        let same_pool = match (&self.pool, &session.background_pool) {
            (None, None) => true,
            (Some(previous), Some(current)) => previous.ptr_eq(&Arc::downgrade(current)),
            _ => false,
        };
        anyhow::ensure!(same_pool, "background task pool was replaced");
        let next = Self::capture(session).expect("checked owner");
        anyhow::ensure!(
            next.account.validates_since(&self.account, owner)
                && next.files.validates_since(&self.files, owner)
                && next.history.validates_since(&self.history, owner),
            "unattributed or replaced shared observation state"
        );
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background56_exact_account_receipt_ack_is_observed_but_replacement_and_setter_are_not() {
        let root = crate::tests::OwnedTestDirectory::new("background56-receipt-proof");
        let mut session = super::super::Session::new("openai", "fixture", 32000, "");
        session.initialize_spend_account(None).unwrap();
        let owner = ObservationOwner::bind(&mut session).unwrap();
        let before = SharedObservations::capture(&session).unwrap();
        let (job, _lifetime) = owner.admit().unwrap();
        let receipt = crate::spend::Receipt::new(
            &session.id,
            "openai",
            "fixture",
            crate::spend::RequestKind::Subagent,
            crate::event::UsageDelta {
                input_tokens: 10,
                output_tokens: 2,
                total_tokens: 12,
                ..Default::default()
            },
            crate::spend::PriceSnapshot::capture(Some((2.0, 4.0)), Default::default(), false),
        )
        .with_background_observation(Some(job));
        session.spend_account.observe(&receipt).unwrap();
        assert!(session.spend_account.has_pending());
        assert!(before.current_since(&session).is_ok());
        let ledger = crate::spend::Ledger::new(&root.path().join("state"));
        ledger.flush(&session.spend_account).unwrap();
        assert!(!session.spend_account.has_pending());
        let after = before.current_since(&session).unwrap();
        assert!((session.recorded_total_cost() - 0.000028).abs() < 1e-12);
        assert_eq!(
            serde_json::to_value(after.frozen_session(&session).spend_account).unwrap(),
            serde_json::to_value(&session.spend_account).unwrap()
        );
        let mut replacement = session.clone();
        replacement.spend_account =
            serde_json::from_value(serde_json::to_value(&session.spend_account).unwrap()).unwrap();
        replacement
            .spend_account
            .bind_observation_owner(&owner)
            .unwrap();
        assert!(before.current_since(&replacement).is_err());
        session.spend_account.inherit_call_count(3, false);
        assert!(after.current_since(&session).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background56_real_write_keeps_launch_anchor_and_rewind_is_unattributed() {
        use rig::tool::Tool;
        let root = crate::tests::OwnedTestDirectory::new("background56-fixed-checkpoint");
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut session = super::super::Session::new("openai", "fixture", 32000, "");
        session.initialize_spend_account(None).unwrap();
        let owner = ObservationOwner::bind(&mut session).unwrap();
        let baseline = SharedObservations::capture(&session).unwrap();
        let (job, _lifetime) = owner.admit().unwrap();
        let foreground = session.file_checkpoints.recorder(&session.id, 0, true);
        let background = foreground.for_background(job);
        foreground.rebase_turn(2);
        super::super::file_checkpoints::scope(Some(background), async {
            crate::agent::tools::write::WriteTool::new(None, None, None)
                .with_workspace_binding(workspace.clone())
                .call(crate::agent::tools::WriteArgs {
                    path: "owned.txt".into(),
                    content: "actual background write".into(),
                    overwrite: false,
                })
                .await
                .unwrap();
        })
        .await;
        assert_eq!(
            std::fs::read_to_string(root.path().join("owned.txt")).unwrap(),
            "actual background write"
        );
        assert!(
            session
                .file_checkpoints
                .summary_from(&session.id, 0)
                .contains("1 files")
        );
        assert!(
            session
                .file_checkpoints
                .summary_from(&session.id, 1)
                .contains("0 files")
        );
        assert!(baseline.current_since(&session).is_ok());
        let mut replaced = session.clone();
        replaced.file_checkpoints =
            serde_json::from_value(serde_json::to_value(&session.file_checkpoints).unwrap())
                .unwrap();
        replaced
            .file_checkpoints
            .bind_observation_owner(&owner)
            .unwrap();
        assert!(baseline.current_since(&replaced).is_err());
        assert_eq!(
            session
                .file_checkpoints
                .restore(&session.id, 0, &workspace)
                .unwrap(),
            1
        );
        assert!(!root.path().join("owned.txt").exists());
        assert!(
            baseline.current_since(&session).is_err(),
            "restoring checkpoints is an explicit operator mutation, not a background observation"
        );
    }

    #[test]
    fn background56_observation_stamp_rejects_foreign_settled_and_unattributed_changes() {
        let owner = ObservationOwner::new();
        let mut stamp = ObservationStamp::default();
        stamp.bind(&owner).unwrap();
        let baseline = stamp.clone();
        let (job, lifetime) = owner.admit().unwrap();
        stamp.changed(Some(&job));
        assert!(stamp.validates_since(&baseline, &owner));
        // Stopping admission does not erase real cancellation-time receipts.
        owner.stop_admission();
        stamp.changed(Some(&job));
        assert!(stamp.validates_since(&baseline, &owner));
        assert!(owner.admit().is_err());
        let foreign = ObservationOwner::new();
        let (foreign_job, _foreign_lifetime) = foreign.admit().unwrap();
        let mut wrong = stamp.clone();
        wrong.changed(Some(&foreign_job));
        assert!(!wrong.validates_since(&baseline, &owner));
        assert!(!stamp.validates_since(&baseline, &foreign));
        drop(lifetime);
        let mut settled = stamp.clone();
        settled.changed(Some(&job));
        assert!(!settled.validates_since(&baseline, &owner));
        let mut untagged = stamp.clone();
        untagged.changed(None);
        untagged.changed(Some(&foreign_job));
        assert!(!untagged.validates_since(&baseline, &owner));
        owner.retire();
        assert!(!job.authorizes(&stamp.owner));
    }

    #[test]
    fn background56_observation_stamp_overflow_and_rebinding_never_authorize() {
        let owner = ObservationOwner::new();
        let mut stamp = ObservationStamp::default();
        stamp.bind(&owner).unwrap();
        let baseline = stamp.clone();
        let (job, _lifetime) = owner.admit().unwrap();
        stamp.sequence = u64::MAX;
        stamp.changed(Some(&job));
        assert!(!stamp.validates_since(&baseline, &owner));
        assert!(stamp.bind(&ObservationOwner::new()).is_err());
        drop(owner);
        let replacement = ObservationOwner::new();
        stamp.bind(&replacement).unwrap();
        assert!(!stamp.validates_since(&baseline, &replacement));
    }
}
