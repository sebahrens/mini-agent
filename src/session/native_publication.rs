//! Strict publication of native replay checkpoints. Runtime permits are
//! exact writer ownership; serialized denials are compatibility data only.

use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use super::Session;
use super::native_replay::{RestartReason, RetiredNativeOutcome};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Unclaimed,
    Staging,
    Active,
    Retired,
    Failed,
}

#[derive(Debug, Default)]
struct PublicationState {
    revision: u64,
    digest: [u8; 32],
    phase: Phase,
}

/// This cell belongs to the existing frontier, not a path or id registry.
#[derive(Debug, Default)]
pub(super) struct PublicationCell {
    state: Mutex<PublicationState>,
    /// Used only inside owned blocking workers. Never held by an async poll.
    io: Mutex<()>,
}

impl PublicationCell {
    fn is_claimed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .phase
            != Phase::Unclaimed
    }
}

#[derive(Debug)]
struct WriterOwner {
    lease: super::lock::NativeWriterLease,
    cell: Arc<PublicationCell>,
}

/// Copying a Session copies these values, not the cell's latest revision.
#[derive(Debug, Clone)]
pub(crate) struct NativePublication {
    owner: Arc<WriterOwner>,
    revision: u64,
    digest: [u8; 32],
}

/// Not serializable or cloneable. An exact frontend actor retains this until
/// its tools, hooks and writer children have retired.
#[derive(Debug)]
pub(crate) struct NativePublicationTicket {
    publication: NativePublication,
    epoch: u64,
    // Retain exclusive ownership through actor retirement, not merely the
    // preflight write. Session switch/force stop can revoke it but cannot let
    // another writer enter while this actor's publication can still run.
    writer: super::lock::NativeWriterGuard,
    paired: bool,
    #[cfg(test)]
    fail_final_before_replace: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActorPhase {
    Prepared,
    Running,
    ModelEnded,
    Retired,
}

#[derive(Debug)]
struct ActorState {
    phase: ActorPhase,
    writer: Option<super::lock::NativeWriterGuard>,
    refusal: Option<(u64, RestartReason)>,
    consumed: bool,
}

/// Exact invocation pair. This cannot be restored from saved compatibility
/// data or manufactured by waiting on an unrelated empty I/O scope.
#[derive(Debug, Clone)]
pub(crate) struct NativeActorPair(Arc<ActorPairInner>);

#[derive(Debug)]
struct ActorPairInner {
    owner: Arc<WriterOwner>,
    epoch: u64,
    state: Mutex<ActorState>,
}

impl NativeActorPair {
    pub(crate) fn pair(
        session: &Session,
        ticket: &mut NativePublicationTicket,
    ) -> anyhow::Result<Self> {
        ensure_actor_value(session, ticket)?;
        anyhow::ensure!(!ticket.paired, "native actor ticket already paired");
        let writer = ticket.publication.owner.lease.begin_write()?;
        ticket.paired = true;
        Ok(Self(Arc::new(ActorPairInner {
            owner: Arc::clone(&ticket.publication.owner),
            epoch: ticket.epoch,
            state: Mutex::new(ActorState {
                phase: ActorPhase::Prepared,
                writer: Some(writer),
                refusal: None,
                consumed: false,
            }),
        })))
    }

    pub(crate) fn claim_retired(
        &self,
        session: &Session,
        ticket: &NativePublicationTicket,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(&self.0.owner, &ticket.publication.owner)
                && self.0.epoch == ticket.epoch
                && self.0.owner.lease.matches(session)
                && session.native_frontier.current() == ticket.epoch,
            "native completion belongs to another actor"
        );
        ticket.writer.check()?;
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            state.phase == ActorPhase::Retired && !state.consumed && state.refusal.is_none(),
            "native model and effects have not retired"
        );
        state.consumed = true;
        Ok(())
    }

    /// Called by the actual model producer before its owned future is polled.
    pub(crate) fn start(&self) -> anyhow::Result<NativeActorRetirementMarker> {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            state.phase == ActorPhase::Prepared,
            "native model invocation already started"
        );
        state
            .writer
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("native model writer retired"))?
            .check()?;
        state.phase = ActorPhase::Running;
        Ok(NativeActorRetirementMarker(self.clone()))
    }

    /// The producer invokes this only after explicitly destroying its exact
    /// model future, including an aborted/never-polled child.
    pub(crate) fn model_ended(&self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.phase == ActorPhase::Running {
            state.phase = ActorPhase::ModelEnded;
        }
    }

    pub(crate) fn accepted_refusal(
        &self,
        session: &Session,
        original_epoch: u64,
        reason: RestartReason,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            original_epoch == self.0.epoch && self.0.owner.lease.matches(session),
            "native refusal belongs to another invocation"
        );
        let cutoff = original_epoch.checked_add(1).ok_or(RestartReason::Limit)?;
        anyhow::ensure!(
            session.native_frontier.current() == cutoff
                && session.native_frontier.refusal() == Some(reason),
            "native refusal was superseded"
        );
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            state.phase != ActorPhase::Prepared && state.refusal.is_none() && !state.consumed,
            "native accepted refusal already retired"
        );
        state.refusal = Some((cutoff, reason));
        Ok(())
    }
}

/// Kept by the existing cleanup owner. Its Drop occurs only after the exact
/// registered children have joined; model termination is a separate condition.
pub(crate) struct NativeActorRetirementMarker(NativeActorPair);

impl Drop for NativeActorRetirementMarker {
    fn drop(&mut self) {
        let mut state = self
            .0
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.phase == ActorPhase::ModelEnded {
            state.phase = ActorPhase::Retired;
            state.writer.take();
        }
    }
}

/// Explicit model-before-cleanup destruction also covers abort-before-poll.
pub(crate) struct NativeModelOwner<F> {
    model: Option<F>,
    pair: NativeActorPair,
    cleanup: Option<crate::agent::runner::AgentRunCleanupGuard>,
}

impl<F> NativeModelOwner<F> {
    pub(crate) fn new(
        model: F,
        pair: NativeActorPair,
        mut cleanup: crate::agent::runner::AgentRunCleanupGuard,
    ) -> anyhow::Result<Self> {
        let marker = pair.start()?;
        cleanup.retain_until_settled(marker);
        Ok(Self {
            model: Some(model),
            pair,
            cleanup: Some(cleanup),
        })
    }

    pub(crate) fn model(&mut self) -> &mut F {
        self.model.as_mut().expect("model already retired")
    }

    pub(crate) async fn settle(mut self) {
        drop(self.model.take());
        self.pair.model_ended();
        self.cleanup.take().unwrap().settle().await;
    }
}

impl<F> Drop for NativeModelOwner<F> {
    fn drop(&mut self) {
        drop(self.model.take());
        self.pair.model_ended();
        // The existing guard transfers its marker to its owned settlement
        // waiter. No model task is spawned here.
        drop(self.cleanup.take());
    }
}

/// Startup retains the preflight ticket and every configured startup hook
/// until their exact scope retires. This owner cannot mint a model outcome.
pub(crate) struct NativeStartupPublication {
    scope: Arc<crate::agent::runner::AgentWorkScope>,
    cleanup: Option<crate::agent::runner::AgentRunCleanupGuard>,
    ticket: Option<NativePublicationTicket>,
}

impl NativeStartupPublication {
    #[cfg(test)]
    pub(crate) fn fail_final_before_replace(&mut self) {
        self.ticket.as_mut().unwrap().fail_final_before_replace = true;
    }

    pub(crate) async fn begin(session: &mut Session) -> anyhow::Result<Self> {
        let scope = crate::agent::runner::AgentWorkScope::new();
        let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(Arc::clone(&scope));
        let ticket = begin_owned(session, &scope).await?;
        Ok(Self {
            scope,
            cleanup: Some(cleanup),
            ticket: Some(ticket),
        })
    }

    /// Construct the hook task while this exact scope is installed. Its whole
    /// async lifetime and any blocking children participate in startup joining.
    #[cfg(any(feature = "hooks", test))]
    #[allow(
        clippy::async_yields_async,
        reason = "return the exact hook handle for the startup owner to join before model transfer"
    )]
    pub(crate) fn spawn_hook<F>(
        &self,
        hook: F,
    ) -> impl std::future::Future<Output = tokio::task::JoinHandle<F::Output>> + Send + '_
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        // Boxing is synchronous and cannot run the hook. Keep its large
        // concrete future out of resolve_prompts and main's future states.
        let hook = crate::startup::heap_future(|| hook);
        let scope = &self.scope;
        async move {
            scope
                .run(async move { crate::agent::runner::spawn_async_scoped(hook) })
                .await
        }
    }

    /// A failed task join is not successful startup retirement. Generic
    /// startup keeps its prior best-effort behavior; native transfer refuses.
    #[cfg(any(feature = "hooks", test))]
    pub(crate) fn hook_retired(
        &self,
        result: Result<(), tokio::task::JoinError>,
    ) -> anyhow::Result<()> {
        if result.is_err() {
            self.scope.cancellation_handle().cancel();
            anyhow::bail!("startup work did not finish; saved context remains unfinished");
        }
        Ok(())
    }

    pub(crate) async fn save_progress(&mut self, session: &mut Session) -> anyhow::Result<()> {
        progress_owned(
            session,
            self.ticket
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("unfinished native context already left startup"))?,
            &self.scope,
        )
        .await
    }

    /// Called only after the stored SessionStart JoinHandle was awaited.
    /// Joining this scope additionally retires children registered by hooks.
    /// The exact ticket transfers; startup never heals its saved denial.
    pub(crate) async fn into_model_ticket(
        mut self,
        session: &Session,
    ) -> anyhow::Result<NativePublicationTicket> {
        self.scope.wait_idle().await;
        anyhow::ensure!(
            !self.scope.is_cancelled(),
            "unfinished native context was cancelled during startup"
        );
        let ticket = self
            .ticket
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("unfinished native context already left startup"))?;
        ensure_actor_value(session, ticket)?;
        // All registered hook/writer work is now retired. This cancels the
        // old empty scope before transferring to the next actor's own scope.
        self.cleanup.take().unwrap().settle().await;
        Ok(self.ticket.take().unwrap())
    }
}

impl Drop for NativeStartupPublication {
    fn drop(&mut self) {
        if let (Some(cleanup), Some(ticket)) = (&mut self.cleanup, self.ticket.take()) {
            cleanup.retain_until_settled(ticket);
        }
    }
}

/// Native-bearing persisted data cannot become ephemeral by a CLI flag. Fresh
/// explicit --no-session runs have no checkpoint to recover or overwrite.
pub(crate) fn startup_requires_publication(
    session: &Session,
    actual_native_protocol: bool,
    no_session: bool,
) -> anyhow::Result<bool> {
    let bearing = is_native_bearing(session);
    anyhow::ensure!(
        !no_session || !bearing,
        "saved native context requires persistence before startup; start a fresh context to use --no-session"
    );
    Ok(!no_session && (actual_native_protocol || bearing))
}

pub(crate) fn is_native_bearing(session: &Session) -> bool {
    session.native_publication.is_some()
        || session.native_history.is_some()
        || !session.retired_native_history.is_empty()
        || !session.native_frontier.is_initial()
        || session.native_frontier.publication_cell().is_claimed()
        || super::lock::requires_native_publication(&session.id)
}

fn digest(session: &Session) -> anyhow::Result<[u8; 32]> {
    // Bind actual canonical rows AND accounting/pricing/shared-owner snapshots.
    // One bounded encoding is the fingerprint, not a separate count pass.
    let encoded = super::storage::serialize_bounded_session(session)?;
    Ok(Sha256::digest(encoded.as_bytes()).into())
}

fn checked_publication(session: &Session) -> anyhow::Result<&NativePublication> {
    let publication = session.native_publication.as_ref().ok_or_else(|| {
        anyhow::anyhow!("unfinished native context requires its saved-session owner")
    })?;
    anyhow::ensure!(
        publication.owner.lease.matches(session),
        "unfinished native context belongs to another session owner"
    );
    anyhow::ensure!(
        Arc::ptr_eq(
            &publication.owner.cell,
            &session.native_frontier.publication_cell()
        ),
        "unfinished native context belongs to another session owner"
    );
    anyhow::ensure!(
        digest(session)? == publication.digest,
        "native context snapshot changed before publication"
    );
    Ok(publication)
}

fn validate_revision(
    publication: &NativePublication,
    state: &PublicationState,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        publication.revision == state.revision && publication.digest == state.digest,
        "native context snapshot was superseded"
    );
    anyhow::ensure!(
        matches!(state.phase, Phase::Active | Phase::Retired),
        "native context publication is unfinished"
    );
    Ok(())
}

/// Ordinary native-bearing snapshots can never heal an admitted in-flight
/// checkpoint. Old clones and independently deserialized same-id data refuse.
pub(super) fn save_snapshot(session: &Session) -> anyhow::Result<()> {
    let publication = checked_publication(session)?;
    let writer = publication.owner.lease.begin_write()?;
    let _io = publication
        .owner
        .cell
        .io
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    writer.check()?;
    let phase = {
        let state = publication
            .owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        validate_revision(publication, &state)?;
        state.phase
    };
    write_projection(session, phase == Phase::Active)
}

fn write_projection(session: &Session, in_flight: bool) -> anyhow::Result<()> {
    let mut snapshot = session.clone();
    if in_flight {
        snapshot.native_frontier = session.native_frontier.in_flight_projection();
    }
    snapshot.validate_native_persistence()?;
    let json = super::storage::serialize_bounded_session(&snapshot)?;
    let publication = snapshot
        .native_publication
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("native checkpoint requires its retained writer"))?;
    publication
        .owner
        .lease
        .publish_checkpoint(session, json.as_bytes())
}

/// Synchronous core called only inside the exact owned blocking worker.
fn begin_blocking(
    mut snapshot: Session,
) -> anyhow::Result<(NativePublication, NativePublicationTicket)> {
    anyhow::ensure!(
        snapshot.native_frontier.ready(),
        "native context is unfinished"
    );
    let cell = snapshot.native_frontier.publication_cell();
    let owner = match &snapshot.native_publication {
        Some(publication) => {
            checked_publication(&snapshot)?;
            Arc::clone(&publication.owner)
        }
        None => Arc::new(WriterOwner {
            lease: super::lock::claim_native_writer(&snapshot)?,
            cell,
        }),
    };
    snapshot.native_frontier.activate_publication();
    let current_digest = digest(&snapshot)?;
    let publication = {
        let mut state = owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match &snapshot.native_publication {
            Some(previous) => {
                validate_revision(previous, &state)?;
                anyhow::ensure!(
                    state.phase == Phase::Retired,
                    "native context still has an active turn"
                );
            }
            None => anyhow::ensure!(
                state.phase == Phase::Unclaimed,
                "unfinished native context requires its original owner"
            ),
        }
        state.revision = state.revision.checked_add(1).ok_or(RestartReason::Limit)?;
        state.digest = current_digest;
        state.phase = Phase::Staging;
        NativePublication {
            owner: Arc::clone(&owner),
            revision: state.revision,
            digest: current_digest,
        }
    };
    snapshot.native_publication = Some(publication.clone());
    let writer = owner.lease.begin_write()?;
    let _io = owner
        .cell
        .io
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    writer.check()?;
    let result = write_projection(&snapshot, true).and_then(|()| writer.check());
    let mut state = owner
        .cell
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    state.phase = if result.is_ok() {
        Phase::Active
    } else {
        Phase::Failed
    };
    result?;
    Ok((
        publication.clone(),
        NativePublicationTicket {
            publication,
            epoch: snapshot.native_frontier.current(),
            writer,
            paired: false,
            #[cfg(test)]
            fail_final_before_replace: false,
        },
    ))
}

/// Construct the blocking child *inside* the installed scope. Dropping this
/// awaiter cannot detach its writer from exact turn settlement.
pub(crate) async fn begin_owned(
    session: &mut Session,
    scope: &Arc<crate::agent::runner::AgentWorkScope>,
) -> anyhow::Result<NativePublicationTicket> {
    let snapshot = session.clone();
    let (publication, ticket) = scope
        .run(async move {
            crate::agent::runner::spawn_blocking_scoped(move || begin_blocking(snapshot)).await
        })
        .await??;
    session.native_publication = Some(publication);
    Ok(ticket)
}

/// Only the current actor's exact mutable ticket can seal a changed snapshot.
/// Staging is issued before queueing; stale clones cannot borrow its revision.
fn ensure_actor_value(session: &Session, ticket: &NativePublicationTicket) -> anyhow::Result<()> {
    let previous = session
        .native_publication
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("native progress requires its current actor"))?;
    anyhow::ensure!(
        previous.revision == ticket.publication.revision
            && previous.digest == ticket.publication.digest
            && Arc::ptr_eq(&previous.owner, &ticket.publication.owner)
            && previous.owner.lease.matches(session)
            && session.native_frontier.current() == ticket.epoch
            && session.native_frontier.ready(),
        "native progress belongs to another actor snapshot"
    );
    Ok(())
}

fn seal_progress(
    session: &mut Session,
    ticket: &mut NativePublicationTicket,
) -> anyhow::Result<(Session, String, NativePublication)> {
    ensure_actor_value(session, ticket)?;
    let canonical = super::storage::serialize_bounded_session(session)?;
    let digest: [u8; 32] = Sha256::digest(canonical.as_bytes()).into();
    // This detached data projection is never a runtime Session. Decode the
    // exact captured bytes so mutable account/todo/goal owners cannot change
    // queued contents. Apply denial before validating an unfinished feed.
    let mut projection: Session = serde_json::from_str(&canonical)?;
    projection.native_frontier = projection.native_frontier.in_flight_projection();
    projection.validate_native_persistence()?;
    let denied = super::storage::serialize_bounded_session(&projection)?;
    ensure_actor_value(session, ticket)?;
    let owner = Arc::clone(&ticket.publication.owner);
    let publication = {
        let mut state = owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        validate_revision(&ticket.publication, &state)?;
        anyhow::ensure!(state.phase == Phase::Active, "native actor already retired");
        state.revision = state.revision.checked_add(1).ok_or(RestartReason::Limit)?;
        state.digest = digest;
        state.phase = Phase::Staging;
        NativePublication {
            owner: Arc::clone(&owner),
            revision: state.revision,
            digest,
        }
    };
    session.native_publication = Some(publication.clone());
    ticket.publication = publication.clone();
    Ok((session.clone(), denied, publication))
}

struct ProgressFence {
    publication: NativePublication,
    frontier: super::native_replay::NativeFrontier,
    epoch: u64,
    acknowledged: bool,
}

impl ProgressFence {
    fn acknowledge(&mut self, success: bool) {
        self.acknowledged = success;
    }

    fn new(
        publication: NativePublication,
        frontier: super::native_replay::NativeFrontier,
        epoch: u64,
    ) -> Self {
        Self {
            publication,
            frontier,
            epoch,
            acknowledged: false,
        }
    }
}

impl Drop for ProgressFence {
    fn drop(&mut self) {
        if self.acknowledged {
            return;
        }
        let mut state = self
            .publication
            .owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.revision != self.publication.revision || state.digest != self.publication.digest {
            return;
        }
        state.phase = Phase::Failed;
        drop(state);
        // Never fence a newer cutoff through this old queued snapshot.
        let _ = self
            .frontier
            .refuse_expected(self.epoch, RestartReason::IncompleteTurn);
    }
}

fn publish_progress(
    identity: Session,
    denied: String,
    publication: NativePublication,
    expected_epoch: u64,
) -> anyhow::Result<()> {
    let owner = &publication.owner;
    let operation = || -> anyhow::Result<()> {
        anyhow::ensure!(
            identity.native_frontier.current() == expected_epoch
                && identity.native_frontier.ready(),
            "native progress cutoff changed"
        );
        let writer = owner.lease.begin_write()?;
        let _io = owner
            .cell
            .io
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        writer.check()?;
        {
            let state = owner
                .cell
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            anyhow::ensure!(
                state.revision == publication.revision
                    && state.digest == publication.digest
                    && state.phase == Phase::Staging,
                "native progress snapshot was superseded"
            );
        }
        let result = owner
            .lease
            .publish_checkpoint(&identity, denied.as_bytes())
            .and_then(|()| writer.check());
        let mut state = owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            state.revision == publication.revision
                && state.digest == publication.digest
                && state.phase == Phase::Staging,
            "native progress acknowledgment was interrupted"
        );
        state.phase = if result.is_ok() {
            Phase::Active
        } else {
            Phase::Failed
        };
        result
    };
    let result = operation();
    if result.is_err() {
        let mut state = owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.revision == publication.revision && state.digest == publication.digest {
            state.phase = Phase::Failed;
        }
        drop(state);
        let _ = identity
            .native_frontier
            .refuse_expected(expected_epoch, RestartReason::IncompleteTurn);
    }
    result
}

pub(crate) async fn progress_owned(
    session: &mut Session,
    ticket: &mut NativePublicationTicket,
    scope: &Arc<crate::agent::runner::AgentWorkScope>,
) -> anyhow::Result<()> {
    let expected_epoch = ticket.epoch;
    ensure_actor_value(session, ticket)?;
    let mut awaiting = ProgressFence::new(
        ticket.publication.clone(),
        session.native_frontier.clone(),
        expected_epoch,
    );
    let (identity, denied, publication) = seal_progress(session, ticket)?;
    awaiting.publication = publication.clone();
    // The awaiter guard covers cancellation before the child is constructed,
    // and lost acknowledgment after it finishes. The child guard covers its
    // own panic/error even if the awaiter is already gone.
    let mut writing = ProgressFence::new(
        publication.clone(),
        session.native_frontier.clone(),
        expected_epoch,
    );
    anyhow::ensure!(!scope.is_cancelled(), "native progress was interrupted");
    scope
        .run(async move {
            crate::agent::runner::spawn_blocking_scoped(move || {
                let result = publish_progress(identity, denied, publication, expected_epoch);
                writing.acknowledge(result.is_ok());
                result
            })
            .await
        })
        .await??;
    anyhow::ensure!(
        !scope.is_cancelled(),
        "native progress acknowledgment was interrupted"
    );
    awaiting.acknowledge(true);
    Ok(())
}

/// Only a successful accepted refusal from the paired, exactly retired model
/// may save at its advanced cutoff. This never selects or heals a boundary.
pub(crate) async fn publish_refused_owned(
    session: &mut Session,
    ticket: NativePublicationTicket,
    pair: NativeActorPair,
    scope: &Arc<crate::agent::runner::AgentWorkScope>,
) -> anyhow::Result<()> {
    let previous = session
        .native_publication
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("native refusal requires its current owner"))?;
    anyhow::ensure!(
        previous.revision == ticket.publication.revision
            && previous.digest == ticket.publication.digest
            && Arc::ptr_eq(&previous.owner, &ticket.publication.owner)
            && Arc::ptr_eq(&pair.0.owner, &ticket.publication.owner)
            && pair.0.epoch == ticket.epoch
            && previous.owner.lease.matches(session),
        "native refusal belongs to another actor snapshot"
    );
    ticket.writer.check()?;
    let (cutoff, reason) = {
        let mut state = pair
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            state.phase == ActorPhase::Retired && !state.consumed,
            "native model and its effects have not retired"
        );
        let refusal = state
            .refusal
            .ok_or_else(|| anyhow::anyhow!("native invocation has no accepted refusal"))?;
        anyhow::ensure!(
            session.native_frontier.current() == refusal.0
                && session.native_frontier.refusal() == Some(refusal.1),
            "native refusal was superseded"
        );
        state.consumed = true;
        refusal
    };
    let json = super::storage::serialize_bounded_session(session)?;
    let projection: Session = serde_json::from_str(&json)?;
    projection.validate_native_persistence()?;
    let digest: [u8; 32] = Sha256::digest(json.as_bytes()).into();
    let owner = Arc::clone(&ticket.publication.owner);
    let publication = {
        let mut state = owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        validate_revision(&ticket.publication, &state)?;
        anyhow::ensure!(
            state.phase == Phase::Active,
            "native refusal owner already retired"
        );
        state.revision = state.revision.checked_add(1).ok_or(RestartReason::Limit)?;
        state.digest = digest;
        state.phase = Phase::Staging;
        NativePublication {
            owner: Arc::clone(&owner),
            revision: state.revision,
            digest,
        }
    };
    session.native_publication = Some(publication.clone());
    let identity = session.clone();
    let mut awaiting = RefusedFence {
        publication: publication.clone(),
        acknowledged: false,
    };
    let mut writing = RefusedFence {
        publication: publication.clone(),
        acknowledged: false,
    };
    anyhow::ensure!(
        !scope.is_cancelled(),
        "native refusal publication was interrupted"
    );
    scope
        .run(async move {
            crate::agent::runner::spawn_blocking_scoped(move || {
                let _io = owner
                    .cell
                    .io
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                ticket.writer.check()?;
                anyhow::ensure!(
                    identity.native_frontier.current() == cutoff
                        && identity.native_frontier.refusal() == Some(reason),
                    "native refusal cutoff changed"
                );
                {
                    let state = owner
                        .cell
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    anyhow::ensure!(
                        state.revision == publication.revision
                            && state.digest == publication.digest
                            && state.phase == Phase::Staging,
                        "native refusal snapshot was superseded"
                    );
                }
                owner.lease.publish_checkpoint(&identity, json.as_bytes())?;
                ticket.writer.check()?;
                let mut state = owner
                    .cell
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                anyhow::ensure!(
                    state.revision == publication.revision
                        && state.digest == publication.digest
                        && state.phase == Phase::Staging,
                    "native refusal acknowledgment was interrupted"
                );
                state.phase = Phase::Retired;
                writing.acknowledge();
                Ok::<(), anyhow::Error>(())
            })
            .await
        })
        .await??;
    anyhow::ensure!(
        !scope.is_cancelled(),
        "native refusal acknowledgment was interrupted"
    );
    awaiting.acknowledge();
    Ok(())
}

struct RefusedFence {
    publication: NativePublication,
    acknowledged: bool,
}

impl RefusedFence {
    fn acknowledge(&mut self) {
        self.acknowledged = true;
    }
}

impl Drop for RefusedFence {
    fn drop(&mut self) {
        if self.acknowledged {
            return;
        }
        let mut state = self
            .publication
            .owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.revision == self.publication.revision && state.digest == self.publication.digest {
            state.phase = Phase::Failed;
        }
        // Keep the already proven original refusal and cutoff unchanged.
    }
}

fn retire_blocking(
    snapshot: Session,
    ticket: NativePublicationTicket,
    outcome: RetiredNativeOutcome,
) -> anyhow::Result<NativePublication> {
    #[cfg(test)]
    let fail = ticket.fail_final_before_replace;
    retire_blocking_using(snapshot, ticket, outcome, |snapshot, in_flight| {
        #[cfg(test)]
        if fail {
            anyhow::bail!("controlled native final publication failure before replacement");
        }
        write_projection(snapshot, in_flight)
    })
}

fn retire_blocking_using(
    mut snapshot: Session,
    ticket: NativePublicationTicket,
    outcome: RetiredNativeOutcome,
    publish: impl FnOnce(&Session, bool) -> anyhow::Result<()>,
) -> anyhow::Result<NativePublication> {
    let owner = &ticket.publication.owner;
    ticket.writer.check()?;
    anyhow::ensure!(
        owner.lease.matches(&snapshot) && snapshot.native_frontier.current() == ticket.epoch,
        "native context retirement belongs to another turn"
    );
    {
        let state = owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        validate_revision(&ticket.publication, &state)?;
        anyhow::ensure!(
            state.phase == Phase::Active,
            "native context actor already retired"
        );
    }
    if outcome == RetiredNativeOutcome::Incomplete {
        snapshot
            .native_frontier
            .refuse_expected(ticket.epoch, RestartReason::IncompleteTurn)?;
    } else if let Some(history) = &snapshot.native_history {
        anyhow::ensure!(
            history.saved_boundary()?
                == match outcome {
                    RetiredNativeOutcome::Completed =>
                        super::native_replay::ReplayBoundary::CompletedTurn,
                    RetiredNativeOutcome::LiveToolReply =>
                        super::native_replay::ReplayBoundary::LiveToolReply,
                    RetiredNativeOutcome::Incomplete => unreachable!(),
                },
            "native context does not match its retired actor"
        );
    }
    let next_digest = digest(&snapshot)?;
    let publication = {
        let mut state = owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        validate_revision(&ticket.publication, &state)?;
        anyhow::ensure!(
            state.phase == Phase::Active,
            "native context actor already retired"
        );
        state.revision = state.revision.checked_add(1).ok_or(RestartReason::Limit)?;
        state.digest = next_digest;
        state.phase = Phase::Staging;
        NativePublication {
            owner: Arc::clone(owner),
            revision: state.revision,
            digest: next_digest,
        }
    };
    snapshot.native_publication = Some(publication.clone());
    let writer = owner.lease.begin_write()?;
    let _io = owner
        .cell
        .io
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    writer.check()?;
    let result = publish(&snapshot, false).and_then(|()| writer.check());
    let mut state = owner
        .cell
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    state.phase = if result.is_ok() {
        Phase::Retired
    } else {
        Phase::Failed
    };
    // A complete retired payload may already be visible after rename even if
    // directory sync failed. The live owner remains conservative; never roll
    // back that newer payload with an older denied copy.
    if result.is_err() && snapshot.native_frontier.ready() {
        let _ = snapshot
            .native_frontier
            .refuse_expected(ticket.epoch, RestartReason::IncompleteTurn);
    }
    result?;
    Ok(publication)
}

pub(crate) async fn retire_owned(
    session: &mut Session,
    ticket: NativePublicationTicket,
    outcome: RetiredNativeOutcome,
    scope: &Arc<crate::agent::runner::AgentWorkScope>,
) -> anyhow::Result<()> {
    let snapshot = session.clone();
    let publication = scope
        .run(async move {
            crate::agent::runner::spawn_blocking_scoped(move || {
                retire_blocking(snapshot, ticket, outcome)
            })
            .await
        })
        .await??;
    session.native_publication = Some(publication);
    Ok(())
}

/// Runs the existing owned helper in a new process; never writes the checkpoint.
#[cfg(all(test, unix))]
pub(crate) fn assert_refused_reload_in_separate_process(
    id: &str,
    workspace: &crate::paths::WorkspaceBinding,
    inputs: u64,
    outputs: u64,
) {
    tests::assert_refused_reload_in_separate_process(id, workspace, inputs, outputs);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn environment(roots: &crate::tests::OwnedTestDirectory) -> crate::tests::ScopedProcessEnv {
        crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_CONFIG_DIR",
                Some(roots.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(roots.path().join("data").into_os_string()),
            ),
            (
                "ZS_STATE_DIR",
                Some(roots.path().join("state").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(roots.path().join("cache").into_os_string()),
            ),
        ])
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    struct Release(String);
    impl Drop for Release {
        fn drop(&mut self) {
            super::super::lock::release_session(&self.0);
        }
    }

    fn imported(session: &Session) -> Session {
        super::super::storage::load_session_exact(&session.id)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn native_publication_revision_and_same_pid_import_are_not_writer_authority() {
        let roots = crate::tests::OwnedTestDirectory::new("native-publication-revision");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        session.add_message(super::super::MessageRole::User, "literal prompt");
        let old = session.clone();
        runtime().block_on(async {
            let scope = crate::agent::runner::AgentWorkScope::new();
            let ticket = begin_owned(&mut session, &scope).await.unwrap();
            scope.wait_idle().await;
            assert_eq!(scope.active_children(), 0);
            assert!(session.native_frontier.ready());
            let disk = imported(&session);
            assert_eq!(
                disk.native_frontier.current(),
                session.native_frontier.current()
            );
            assert_eq!(
                disk.native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );
            assert!(disk.native_publication.is_none());
            assert!(super::super::lock::claim_native_writer(&disk).is_err());
            assert!(super::super::storage::save_session(&disk).is_err());
            assert!(super::super::storage::save_session(&old).is_err());
            let active_clone = session.clone();
            super::super::storage::save_session(&active_clone).unwrap();
            assert_eq!(
                imported(&session).native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );

            session.add_message(super::super::MessageRole::Assistant, "completed answer");
            retire_owned(
                &mut session,
                ticket,
                RetiredNativeOutcome::Completed,
                &scope,
            )
            .await
            .unwrap();
            scope.wait_idle().await;
            let retired_bytes =
                std::fs::read(super::super::storage::session_path(&session.id).unwrap()).unwrap();
            assert!(imported(&session).native_frontier.ready());
            assert!(super::super::storage::save_session(&imported(&session)).is_err());
            assert!(super::super::storage::save_session(&active_clone).is_err());
            assert_eq!(
                std::fs::read(super::super::storage::session_path(&session.id).unwrap()).unwrap(),
                retired_bytes
            );
            super::super::storage::save_session(&session).unwrap();
            assert_eq!(imported(&session).messages.len(), 2);
            assert!(session.permission_allowlist.is_empty());
        });
    }

    #[test]
    fn native_publication_dropped_preflight_keeps_exact_blocking_owner_until_settlement() {
        let roots = crate::tests::OwnedTestDirectory::new("native-publication-held");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        runtime().block_on(async {
            let (scope, started, release) = crate::agent::runner::AgentWorkScope::new_with_blocking_test_gate();
            let mut preflight = Box::pin(begin_owned(&mut session, &scope));
            tokio::select! {
                result = &mut preflight => panic!("held preflight unexpectedly completed: {result:?}"),
                _ = async {
                    tokio::time::timeout(Duration::from_secs(2), async {
                        while started.try_recv().is_err() { tokio::task::yield_now().await; }
                    }).await.unwrap();
                } => {}
            }
            assert_eq!(scope.active_children(), 1);
            drop(preflight);
            scope.cancellation_handle().cancel();
            assert!(tokio::time::timeout(Duration::from_millis(20), scope.wait_idle()).await.is_err());
            release.release();
            tokio::time::timeout(Duration::from_secs(2), scope.wait_idle()).await.unwrap();
            assert_eq!(scope.active_children(), 0);
            assert!(session.native_publication.is_none());
            assert_eq!(imported(&session).native_frontier.refusal(), Some(RestartReason::InFlight));
            assert!(super::super::storage::save_session(&session).is_err());
        });
    }

    #[test]
    fn native_publication_revoked_writer_cannot_publish_or_release_an_active_lock() {
        let roots = crate::tests::OwnedTestDirectory::new("native-publication-revoked");
        let _environment = environment(&roots);
        let session = Session::new("anthropic", "fixture", 32_000, "");
        let lease = super::super::lock::claim_native_writer(&session).unwrap();
        let writer = lease.begin_write().unwrap();
        super::super::lock::release_session(&session.id);
        assert!(writer.check().is_err());
        assert!(lease.begin_write().is_err());
        assert_eq!(
            super::super::lock::claim_session(&session.id).unwrap(),
            super::super::lock::SessionClaim::HeldElsewhere
        );
        drop(writer);
        assert_eq!(
            super::super::lock::claim_session(&session.id).unwrap(),
            super::super::lock::SessionClaim::Owned
        );
        super::super::lock::release_session(&session.id);
    }

    #[cfg(unix)]
    #[test]
    fn native_publication_final_failure_distinguishes_old_denial_from_committed_retired_payload() {
        let roots = crate::tests::OwnedTestDirectory::new("native-publication-final-failure");
        let _environment = environment(&roots);
        runtime().block_on(async {
            for after_replace in [false, true] {
                let mut session = Session::new("anthropic", "fixture", 32_000, "");
                let _release = Release(session.id.to_string());
                let scope = crate::agent::runner::AgentWorkScope::new();
                let ticket = begin_owned(&mut session, &scope).await.unwrap();
                session.add_message(super::super::MessageRole::Assistant, "retired exact answer");
                let snapshot = session.clone();
                let result = scope
                    .run(async move {
                        crate::agent::runner::spawn_blocking_scoped(move || {
                            retire_blocking_using(
                                snapshot,
                                ticket,
                                RetiredNativeOutcome::Completed,
                                |snapshot, in_flight| {
                                    assert!(!in_flight);
                                    if after_replace {
                                        // Inject loss of ACK after actual complete atomic replacement;
                                        // this does not claim to reproduce a kernel fsync fault.
                                        write_projection(snapshot, false)?;
                                        anyhow::bail!("injected error after complete replacement");
                                    }
                                    let path = super::super::storage::session_path(&snapshot.id)?;
                                    let json =
                                        super::super::storage::serialize_bounded_session(snapshot)?;
                                    super::super::storage::atomic_write_with_failure(
                                        &path, &json, true,
                                    )
                                },
                            )
                        })
                        .await
                        .unwrap()
                    })
                    .await;
                assert!(result.is_err());
                scope.wait_idle().await;
                assert_eq!(scope.active_children(), 0);
                assert!(!session.native_frontier.ready());
                assert!(super::super::storage::save_session(&session).is_err());
                let disk = imported(&session);
                if after_replace {
                    assert!(disk.native_frontier.ready());
                    assert_eq!(disk.messages[0].content, "retired exact answer");
                } else {
                    assert_eq!(
                        disk.native_frontier.refusal(),
                        Some(RestartReason::InFlight)
                    );
                    assert!(disk.messages.is_empty());
                }
                assert!(disk.native_publication.is_none());
                assert!(disk.permission_allowlist.is_empty());
                assert!(super::super::lock::claim_native_writer(&disk).is_err());
            }
        });
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "owned helper for the forced-exit publication probe"]
    #[allow(unsafe_code)]
    fn native_publication_force_stop_fixture() {
        use std::io::Write;
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        session.id = "native-force-fixture".into();
        runtime().block_on(async {
            let initial = crate::agent::runner::AgentWorkScope::new();
            let ticket = begin_owned(&mut session, &initial).await.unwrap();
            initial.wait_idle().await;
            assert_eq!(
                super::super::lock::claim_session("legacy-force-fixture").unwrap(),
                super::super::lock::SessionClaim::Owned
            );
            let (scope, started, _release) =
                crate::agent::runner::AgentWorkScope::new_with_blocking_test_gate();
            let mut final_write = Box::pin(retire_owned(
                &mut session,
                ticket,
                RetiredNativeOutcome::Completed,
                &scope,
            ));
            tokio::select! {
                result = &mut final_write => panic!("held final writer completed: {result:?}"),
                _ = async {
                    tokio::time::timeout(Duration::from_secs(2), async {
                        while started.try_recv().is_err() { tokio::task::yield_now().await; }
                    }).await.unwrap();
                } => {}
            }
            assert_eq!(scope.active_children(), 1);
            // The forked child performs only async-signal-safe libc calls.
            // Holding stdin retains the inherited CLOEXEC lock description
            // without executing another program. Closing stdin releases it.
            let child = unsafe { libc::fork() };
            assert!(child >= 0);
            if child == 0 {
                unsafe {
                    libc::close(1);
                    libc::close(2);
                    libc::alarm(10);
                    let mut byte = 0_u8;
                    libc::read(0, std::ptr::from_mut(&mut byte).cast(), 1);
                    libc::_exit(0);
                }
            }
            writeln!(std::io::stdout(), "NATIVE_CHILD_PID={child}").unwrap();
            std::io::stdout().flush().unwrap();
            // Actual production forced-stop path: no writer mutex wait.
            crate::print::exit_force_stopped();
        });
    }

    #[cfg(unix)]
    struct OwnedForceFixture {
        child: std::process::Child,
        release: Option<std::process::ChildStdin>,
        native_lock: std::path::PathBuf,
        root_guard: Option<crate::tests::OwnedTestDirectory>,
    }

    #[cfg(unix)]
    impl Drop for OwnedForceFixture {
        fn drop(&mut self) {
            // EOF is the exact inherited child's normal retirement signal.
            // Close it before killing/reaping the helper on every unwind.
            self.release.take();
            if !matches!(self.child.try_wait(), Ok(Some(_))) {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
            // The child performs only read(stdin) then _exit; the inherited
            // lock is released only by that retirement. Observe the exact
            // inode before the private fixture root can be removed.
            if let Ok(file) = crate::fs::open_private_file_checked(
                &self.native_lock,
                crate::fs::PrivateFileAccess::Read,
            ) {
                let deadline = std::time::Instant::now() + Duration::from_secs(11);
                loop {
                    match file.try_lock() {
                        Ok(()) => {
                            let _ = file.unlock();
                            break;
                        }
                        Err(std::fs::TryLockError::WouldBlock)
                            if std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        _ => {
                            eprintln!(
                                "native publication fixture retirement is unproven; retaining its root"
                            );
                            if let Some(root) = self.root_guard.take() {
                                std::mem::forget(root);
                            }
                            break;
                        }
                    }
                }
            } else if let Some(root) = self.root_guard.take() {
                // Never remove a namespace whose inherited descriptor owner
                // could not be inspected. Preserve it as failure evidence.
                std::mem::forget(root);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn native_publication_forced_exit_retains_native_denial_and_releases_legacy_inherited_lock() {
        use std::io::{Read, Write};
        use std::process::{Command, Stdio};
        let roots = crate::tests::OwnedTestDirectory::new("native-publication-force");
        let _environment = environment(&roots);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session::native_publication::tests::native_publication_force_stop_fixture",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let inherited_release = child.stdin.take().unwrap();
        let mut owned = OwnedForceFixture {
            child,
            release: Some(inherited_release),
            native_lock: super::super::lock::lock_path("native-force-fixture").unwrap(),
            root_guard: Some(roots),
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = owned.child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                panic!("forced exit waited on the held writer");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let mut output = String::new();
        owned
            .child
            .stdout
            .take()
            .unwrap()
            .take(64 * 1024)
            .read_to_string(&mut output)
            .unwrap();
        let mut error = String::new();
        owned
            .child
            .stderr
            .take()
            .unwrap()
            .take(64 * 1024)
            .read_to_string(&mut error)
            .unwrap();
        assert_eq!(status.code(), Some(130), "{output}\n{error}");
        assert!(output.contains("NATIVE_CHILD_PID="), "{output}");
        assert_eq!(
            super::super::lock::claim_session("legacy-force-fixture").unwrap(),
            super::super::lock::SessionClaim::Owned
        );
        super::super::lock::release_session("legacy-force-fixture");
        assert_eq!(
            super::super::lock::claim_session("native-force-fixture").unwrap(),
            super::super::lock::SessionClaim::HeldElsewhere
        );
        let disk = super::super::storage::load_session_exact("native-force-fixture")
            .unwrap()
            .unwrap();
        assert_eq!(
            disk.native_frontier.refusal(),
            Some(RestartReason::InFlight)
        );
        assert!(!disk.native_frontier.ready());
        let mut fork = super::super::lock::fork_session(&disk).unwrap();
        runtime().block_on(async {
            let scope = crate::agent::runner::AgentWorkScope::new();
            assert!(begin_owned(&mut fork, &scope).await.is_err());
            scope.wait_idle().await;
            assert_eq!(scope.active_children(), 0);
        });
        assert!(!fork.native_frontier.ready());
        assert!(disk.permission_allowlist.is_empty());
        // Release the exact inherited child, then observe the kernel lock
        // becoming available. This does not heal the saved denial.
        owned.release.as_mut().unwrap().write_all(b"x").unwrap();
        owned.release.take();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if super::super::lock::claim_session("native-force-fixture").unwrap()
                == super::super::lock::SessionClaim::Owned
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "inherited child did not retire"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        super::super::lock::release_session("native-force-fixture");
        assert_eq!(
            super::super::storage::load_session_exact("native-force-fixture")
                .unwrap()
                .unwrap()
                .native_frontier
                .refusal(),
            Some(RestartReason::InFlight)
        );
    }

    #[test]
    fn native_publication_late_pre_activation_serde_cannot_erase_marker_after_owner_release() {
        let roots = crate::tests::OwnedTestDirectory::new("native-publication-late-serde");
        let _environment = environment(&roots);
        runtime().block_on(async {
            for outcome in [
                RetiredNativeOutcome::Completed,
                RetiredNativeOutcome::Incomplete,
            ] {
                let mut session = Session::new("anthropic", "fixture", 32_000, "");
                let _release = Release(session.id.to_string());
                let ordinary = serde_json::to_string(&session).unwrap();
                let scope = crate::agent::runner::AgentWorkScope::new();
                let ticket = begin_owned(&mut session, &scope).await.unwrap();
                retire_owned(&mut session, ticket, outcome, &scope)
                    .await
                    .unwrap();
                scope.wait_idle().await;
                assert_eq!(scope.active_children(), 0);
                super::super::lock::release_session(&session.id);
                let path = super::super::storage::session_path(&session.id).unwrap();
                let before = std::fs::read(&path).unwrap();
                let stale: Session = serde_json::from_str(&ordinary).unwrap();
                assert!(stale.native_frontier.is_initial());
                assert!(stale.native_publication.is_none());
                assert!(super::super::storage::save_session(&stale).is_err());
                assert_eq!(std::fs::read(&path).unwrap(), before);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
                        .unwrap();
                    assert!(super::super::storage::save_session(&stale).is_err());
                    assert_eq!(
                        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                        0o666
                    );
                    assert_eq!(std::fs::read(&path).unwrap(), before);
                    // Only the fixture owner restores its private mode; the
                    // rejected legacy save must never repair a native marker.
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                        .unwrap();
                }
                let disk = imported(&session);
                assert!(!disk.native_frontier.is_initial());
                assert!(disk.native_publication.is_none());
                assert!(disk.permission_allowlist.is_empty());
            }
        });
    }

    #[test]
    fn native_publication_activation_marker_is_restrictive_and_legacy_cutoff_compatible() {
        use super::super::native_replay::NativeFrontier;
        let legacy: NativeFrontier = serde_json::from_str("7").unwrap();
        assert_eq!(serde_json::to_string(&legacy).unwrap(), "7");
        for invalid in [
            r#"{"epoch":0,"activated":false}"#,
            r#"{"epoch":0,"activated":"yes"}"#,
            r#"{"epoch":0,"activated":true,"permit":true}"#,
        ] {
            assert!(serde_json::from_str::<NativeFrontier>(invalid).is_err());
        }
        let roots = crate::tests::OwnedTestDirectory::new("native-publication-marker");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        session.native_frontier = serde_json::from_str(r#"{"epoch":0,"activated":true}"#).unwrap();
        assert!(session.native_publication.is_none());
        assert!(session.native_history.is_none());
        assert!(super::super::storage::save_session(&session).is_err());
        assert!(super::super::lock::claim_native_writer(&session).is_ok());
        // A data marker restricts the legacy path; it does not reconstruct the
        // runtime publisher or a replay envelope, even under a new lock.
        assert!(session.native_publication.is_none());
        assert!(session.native_history.is_none());
        assert!(session.permission_allowlist.is_empty());
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "owned helper for the replaced-root publication probe"]
    fn native_publication_new_root_writer_fixture() {
        use std::io::{Read, Write};
        if let Some(id) = std::env::var_os("NATIVE_PUBLICATION_REFUSED_RELOAD") {
            let id = id.to_str().unwrap();
            // These paths come from the owning host fixture, independently
            // of all saved Session fields. Capture and retain both bindings.
            let sessions_root = crate::paths::WorkspaceBinding::capture(std::path::Path::new(
                &std::env::var_os("NATIVE_PUBLICATION_RELOAD_SESSIONS").unwrap(),
            ))
            .unwrap();
            let workspace = crate::paths::WorkspaceBinding::capture(std::path::Path::new(
                &std::env::var_os("NATIVE_PUBLICATION_RELOAD_WORKSPACE").unwrap(),
            ))
            .unwrap();
            let session =
                super::super::storage::load_session_exact_read_only(&sessions_root, id, &workspace)
                    .unwrap()
                    .unwrap();
            assert_eq!(session.id.as_str(), id);
            assert!(
                session.native_publication.is_none(),
                "load cannot mint writer authority"
            );
            assert_eq!(
                session.native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );
            assert_eq!(
                session
                    .native_restore_history(
                        crate::session::native_replay::HistoryProtocol::AnthropicMessages,
                        crate::session::native_replay::ReplayBoundary::CompletedTurn,
                    )
                    .unwrap_err(),
                RestartReason::InFlight
            );
            let expected: Vec<u64> = std::env::var("NATIVE_PUBLICATION_RELOAD_USAGE")
                .unwrap()
                .split(',')
                .map(|part| part.parse().unwrap())
                .collect();
            assert_eq!(
                (session.total_input_tokens, session.total_output_tokens),
                (expected[0], expected[1])
            );
            writeln!(std::io::stdout(), "REFUSED_RELOAD_WITH_CANONICAL_USAGE").unwrap();
            return;
        }
        let mut session = Session::new("openai", "fixture", 32_000, "");
        session.id = "native-root-replacement-fixture".into();
        session.name = "new namespace owner".into();
        let _release = Release(session.id.to_string());
        super::super::storage::save_session(&session).unwrap();
        writeln!(std::io::stdout(), "NEW_ROOT_OWNER_READY").unwrap();
        std::io::stdout().flush().unwrap();
        std::io::stdin().read_exact(&mut [0_u8; 1]).unwrap();
    }

    #[cfg(unix)]
    fn spawn_root_writer_fixture(
        reload: Option<(&str, &crate::paths::WorkspaceBinding, u64, u64)>,
    ) -> std::process::Child {
        use std::process::{Command, Stdio};
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "session::native_publication::tests::native_publication_new_root_writer_fixture",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some((id, workspace, inputs, outputs)) = reload {
            let session_path = super::super::storage::session_path(id).unwrap();
            command
                .env("NATIVE_PUBLICATION_RELOAD_WORKSPACE", workspace.root())
                .env(
                    "NATIVE_PUBLICATION_RELOAD_SESSIONS",
                    session_path.parent().unwrap(),
                )
                .env("NATIVE_PUBLICATION_REFUSED_RELOAD", id)
                .env(
                    "NATIVE_PUBLICATION_RELOAD_USAGE",
                    format!("{inputs},{outputs}"),
                );
        } else {
            command
                .env_remove("NATIVE_PUBLICATION_REFUSED_RELOAD")
                .env_remove("NATIVE_PUBLICATION_RELOAD_USAGE")
                .env_remove("NATIVE_PUBLICATION_RELOAD_WORKSPACE")
                .env_remove("NATIVE_PUBLICATION_RELOAD_SESSIONS");
        }
        command.spawn().unwrap()
    }

    #[cfg(unix)]
    pub(super) fn assert_refused_reload_in_separate_process(
        id: &str,
        workspace: &crate::paths::WorkspaceBinding,
        inputs: u64,
        outputs: u64,
    ) {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;
        let path = super::super::storage::session_path(id).unwrap();
        let sessions_root =
            crate::paths::WorkspaceBinding::capture(path.parent().unwrap()).unwrap();
        sessions_root.validate_private_root().unwrap();
        workspace.validate().unwrap();
        let before = std::fs::read(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        let mut child = spawn_root_writer_fixture(Some((id, workspace, inputs, outputs)));
        let release = child.stdin.take();
        let mut owned = OwnedForceFixture {
            child,
            release,
            native_lock: super::super::lock::lock_path(id).unwrap(),
            root_guard: None,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = owned.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "separate-process readonly reload did not retire"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        let mut stdout = String::new();
        owned
            .child
            .stdout
            .take()
            .unwrap()
            .take(16 * 1024)
            .read_to_string(&mut stdout)
            .unwrap();
        let mut stderr = String::new();
        owned
            .child
            .stderr
            .take()
            .unwrap()
            .take(16 * 1024)
            .read_to_string(&mut stderr)
            .unwrap();
        assert!(
            status.success(),
            "readonly reload failed: {stdout} {stderr}"
        );
        assert!(stdout.contains("REFUSED_RELOAD_WITH_CANONICAL_USAGE"));
        sessions_root.validate_private_root().unwrap();
        workspace.validate().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode(), mode);
    }

    #[cfg(unix)]
    #[test]
    fn native_publication_replaced_root_and_lock_namespace_refuse_old_writer() {
        use std::io::{BufRead, Write};
        let roots = crate::tests::OwnedTestDirectory::new("native-publication-root-replace");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        session.id = "native-root-replacement-fixture".into();
        let _release = Release(session.id.to_string());
        runtime().block_on(async {
            let scope = crate::agent::runner::AgentWorkScope::new();
            let ticket = begin_owned(&mut session, &scope).await.unwrap();
            scope.wait_idle().await;
            let path = super::super::storage::session_path(&session.id).unwrap();
            let root = path.parent().unwrap();
            let moved = root.with_extension("retained-old");
            std::fs::rename(root, &moved).unwrap();
            crate::paths::ensure_private_directory(root).unwrap();
            let mut child = spawn_root_writer_fixture(None);
            let release = child.stdin.take();
            let output = child.stdout.take().unwrap();
            let mut owned = OwnedForceFixture {
                child,
                release,
                native_lock: super::super::lock::lock_path(&session.id).unwrap(),
                root_guard: Some(roots),
            };
            let (send, recv) = std::sync::mpsc::sync_channel(1);
            let reader = std::thread::spawn(move || {
                let mut output = std::io::BufReader::new(output);
                let mut found = false;
                for _ in 0..16 {
                    let mut line = String::new();
                    match output.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if line.contains("NEW_ROOT_OWNER_READY") => {
                            found = true;
                            break;
                        }
                        Ok(_) => {}
                    }
                }
                let _ = send.send(found);
                output
            });
            let ready = recv.recv_timeout(Duration::from_secs(5));
            if !matches!(ready, Ok(true)) {
                let _ = owned.child.kill();
                let _ = owned.child.wait();
            }
            let _output = reader.join().unwrap();
            assert!(ready.unwrap());
            let new_bytes = std::fs::read(&path).unwrap();
            let old_path = moved.join(path.file_name().unwrap());
            let old_bytes = std::fs::read(&old_path).unwrap();
            assert!(super::super::lock::claim_session(&session.id).is_err());
            assert!(super::super::storage::save_session(&session).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), new_bytes);
            assert_eq!(std::fs::read(&old_path).unwrap(), old_bytes);
            assert_eq!(imported(&session).name.as_str(), "new namespace owner");
            drop(ticket);
            owned.release.as_mut().unwrap().write_all(b"x").unwrap();
            owned.release.take();
            assert!(owned.child.wait().unwrap().success());
            drop(owned);
            scope.wait_idle().await;
            assert_eq!(scope.active_children(), 0);
        });
    }

    #[test]
    fn native_publication_unowned_save_is_create_only_and_native_creator_wins() {
        let roots = crate::tests::OwnedTestDirectory::new("native-publication-create-only");
        let _environment = environment(&roots);
        let legacy = Session::new("openai", "fixture", 32_000, "");
        let lock_path = super::super::lock::lock_path(&legacy.id).unwrap();
        crate::paths::ensure_private_directory(lock_path.parent().unwrap()).unwrap();
        crate::paths::ensure_private_directory(&lock_path).unwrap();
        assert!(super::super::lock::claim_session(&legacy.id).is_err());
        super::super::storage::save_session(&legacy).unwrap();
        let path = super::super::storage::session_path(&legacy.id).unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut changed = legacy.clone();
        changed.name = "must not replace".into();
        assert!(super::super::storage::save_session(&changed).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let stale = Session::new("openai", "fixture", 32_000, "");
        let _release = Release(stale.id.to_string());
        let target = super::super::storage::session_path(&stale.id).unwrap();
        let root = crate::paths::WorkspaceBinding::capture(target.parent().unwrap()).unwrap();
        root.validate_private_root().unwrap();
        let parent =
            crate::fs::checked_file_metadata(&root.try_clone_directory_file().unwrap()).unwrap();
        assert!(!target.exists());
        let stale_json = super::super::storage::serialize_bounded_session(&stale).unwrap();
        // Hold the fallback's exact parent authority while another actual
        // native controller wins the formerly absent checkpoint namespace.
        let mut native = Session::new("anthropic", "fixture", 32_000, "");
        native.id = stale.id.clone();
        runtime().block_on(async {
            let scope = crate::agent::runner::AgentWorkScope::new();
            let ticket = begin_owned(&mut native, &scope).await.unwrap();
            scope.wait_idle().await;
            let checkpoint = std::fs::read(&target).unwrap();
            let error = crate::fs::private_atomic_create_checked_sync(
                &target,
                stale_json.as_bytes(),
                &parent,
            )
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
            assert!(super::super::storage::save_session(&stale).is_err());
            assert_eq!(std::fs::read(&target).unwrap(), checkpoint);
            assert_eq!(
                imported(&native).native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );
            drop(ticket);
            scope.wait_idle().await;
            assert_eq!(scope.active_children(), 0);
        });
    }

    #[cfg(unix)]
    #[test]
    fn native_publication_force_fixture_unwind_retires_inherited_owner_before_root_cleanup() {
        use std::process::{Command, Stdio};
        let mut observed_root = None;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let roots = crate::tests::OwnedTestDirectory::new("native-publication-force-unwind");
            observed_root = Some(roots.path().to_path_buf());
            let _environment = environment(&roots);
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "session::native_publication::tests::native_publication_force_stop_fixture",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let release = child.stdin.take();
            let mut owned = OwnedForceFixture {
                child,
                release,
                native_lock: super::super::lock::lock_path("native-force-fixture").unwrap(),
                root_guard: Some(roots),
            };
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = owned.child.try_wait().unwrap() {
                    assert_eq!(status.code(), Some(130));
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "held writer blocked force exit"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                super::super::lock::claim_session("native-force-fixture").unwrap(),
                super::super::lock::SessionClaim::HeldElsewhere
            );
            panic!("controlled fixture unwind after inherited child admission");
        }));
        assert!(result.is_err());
        assert!(
            !observed_root.unwrap().exists(),
            "root cleanup requires observed inherited-owner retirement"
        );
    }

    #[test]
    fn native_publication_current_progress_seals_counters_and_rejects_old_clones() {
        let roots = crate::tests::OwnedTestDirectory::new("native-publication-progress");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        runtime().block_on(async {
            let scope = crate::agent::runner::AgentWorkScope::new();
            let mut ticket = begin_owned(&mut session, &scope).await.unwrap();
            let stale = session.clone();
            session.add_message(
                super::super::MessageRole::Assistant,
                "truthful accepted progress",
            );
            session.input_token_cost = 2.0;
            session.output_token_cost = 4.0;
            session.charge_usage_delta_at(
                crate::event::UsageDelta {
                    input_tokens: 10,
                    output_tokens: 2,
                    ..Default::default()
                },
                true,
                2.0,
                4.0,
                crate::pricing::CachePrices::default(),
            );
            assert!(super::super::storage::save_session(&session).is_err());
            progress_owned(&mut session, &mut ticket, &scope)
                .await
                .unwrap();
            scope.wait_idle().await;
            let disk = imported(&session);
            assert_eq!(disk.total_input_tokens, 10);
            assert_eq!(disk.total_output_tokens, 2);
            assert!((disk.total_cost - 0.000028).abs() < 1e-12);
            assert_eq!(disk.input_token_cost, 2.0);
            assert_eq!(disk.output_token_cost, 4.0);
            assert_eq!(disk.messages[0].content, "truthful accepted progress");
            assert_eq!(
                disk.native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );
            let bytes =
                std::fs::read(super::super::storage::session_path(&session.id).unwrap()).unwrap();
            assert!(super::super::storage::save_session(&stale).is_err());
            assert_eq!(
                std::fs::read(super::super::storage::session_path(&session.id).unwrap()).unwrap(),
                bytes
            );
            retire_owned(
                &mut session,
                ticket,
                RetiredNativeOutcome::Completed,
                &scope,
            )
            .await
            .unwrap();
            scope.wait_idle().await;
            assert_eq!(scope.active_children(), 0);
            assert!(imported(&session).native_frontier.ready());
            assert_eq!(imported(&session).total_input_tokens, 10);
        });
    }

    #[test]
    fn native_publication_progress_held_capture_is_immutable_and_drop_is_fenced() {
        let roots = crate::tests::OwnedTestDirectory::new("native-publication-progress-held");
        let _environment = environment(&roots);
        runtime().block_on(async {
            for abandon in [false, true] {
                let mut session = Session::new("anthropic", "fixture", 32_000, "");
                let _release = Release(session.id.to_string());
                let initial = crate::agent::runner::AgentWorkScope::new();
                let mut ticket = begin_owned(&mut session, &initial).await.unwrap();
                initial.wait_idle().await;
                let stale = session.clone();
                let todos = session.todos.clone();
                todos.replace(serde_json::from_str(r#"[{"content":"captured current","status":"pending","priority":"high"}]"#).unwrap());
                let frontier = session.native_frontier.clone();
                let (scope, started, release) = crate::agent::runner::AgentWorkScope::new_with_blocking_test_gate();
                let mut saving = Box::pin(progress_owned(&mut session, &mut ticket, &scope));
                tokio::select! {
                    result = &mut saving => panic!("held progress completed: {result:?}"),
                    _ = async { tokio::time::timeout(Duration::from_secs(2), async { while started.try_recv().is_err() { tokio::task::yield_now().await; } }).await.unwrap(); } => {}
                }
                assert_eq!(scope.active_children(), 1);
                assert!(super::super::storage::save_session(&stale).is_err());
                todos.replace(serde_json::from_str(r#"[{"content":"later shared mutation","status":"completed","priority":"low"}]"#).unwrap());
                if abandon {
                    drop(saving);
                    assert!(!frontier.ready());
                    assert!(tokio::time::timeout(Duration::from_millis(20), scope.wait_idle()).await.is_err());
                    release.release();
                    scope.wait_idle().await;
                    assert!(super::super::storage::save_session(&session).is_err());
                    assert_eq!(imported(&session).native_frontier.refusal(), Some(RestartReason::InFlight));
                } else {
                    release.release();
                    saving.await.unwrap();
                    scope.wait_idle().await;
                    assert_eq!(imported(&session).todos.snapshot()[0].content, "captured current");
                    assert_eq!(todos.snapshot()[0].content, "later shared mutation");
                    assert!(super::super::storage::save_session(&stale).is_err());
                    assert!(super::super::storage::save_session(&session).is_err());
                }
                assert_eq!(scope.active_children(), 0);
                drop(ticket);
            }
        });
    }

    #[test]
    fn native_publication_progress_cancelled_before_child_fences_staged_snapshot() {
        let roots = crate::tests::OwnedTestDirectory::new("native-publication-progress-no-child");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        runtime().block_on(async {
            let initial = crate::agent::runner::AgentWorkScope::new();
            let mut ticket = begin_owned(&mut session, &initial).await.unwrap();
            initial.wait_idle().await;
            session.add_message(super::super::MessageRole::Assistant, "accepted partial");
            let scope = crate::agent::runner::AgentWorkScope::new();
            scope.cancellation_handle().cancel();
            assert!(
                progress_owned(&mut session, &mut ticket, &scope)
                    .await
                    .is_err()
            );
            scope.wait_idle().await;
            assert_eq!(scope.active_children(), 0);
            assert!(!session.native_frontier.ready());
            assert!(super::super::storage::save_session(&session).is_err());
            assert_eq!(
                imported(&session).native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );
            drop(ticket);
        });
    }

    #[test]
    fn native_publication_progress_encoding_failure_fences_before_staging_child() {
        let roots = crate::tests::OwnedTestDirectory::new("native-publication-progress-limit");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        runtime().block_on(async {
            let scope = crate::agent::runner::AgentWorkScope::new();
            let mut ticket = begin_owned(&mut session, &scope).await.unwrap();
            scope.wait_idle().await;
            let path = super::super::storage::session_path(&session.id).unwrap();
            let before = std::fs::read(&path).unwrap();
            session.add_message(
                super::super::MessageRole::Assistant,
                &"x".repeat(super::super::storage::MAX_SESSION_BYTES),
            );
            assert!(
                progress_owned(&mut session, &mut ticket, &scope)
                    .await
                    .is_err()
            );
            scope.wait_idle().await;
            assert_eq!(scope.active_children(), 0);
            assert!(!session.native_frontier.ready());
            assert!(super::super::storage::save_session(&session).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert_eq!(
                imported(&session).native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );
            drop(ticket);
        });
    }

    #[test]
    fn native_publication_startup_idle_cap_and_no_session_policy() {
        let roots = crate::tests::OwnedTestDirectory::new("native-startup-policy");
        let _environment = environment(&roots);
        let mut session = Session::new("native-alias", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        session.initialize_spend_account(Some(0.0)).unwrap();
        assert!(startup_requires_publication(&session, true, false).unwrap());
        assert!(!startup_requires_publication(&session, true, true).unwrap());
        assert!(!startup_requires_publication(&session, false, false).unwrap());
        runtime().block_on(async {
            // Idle startup owns data without attempting paid admission.
            let mut owner = NativeStartupPublication::begin(&mut session).await.unwrap();
            assert_eq!(session.spend_account.snapshot().cap, Some(0.0));
            session.name = "renamed at startup".into();
            owner.save_progress(&mut session).await.unwrap();
            let loaded = imported(&session);
            assert_eq!(loaded.name, "renamed at startup");
            assert_eq!(
                loaded.native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );
            assert!(startup_requires_publication(&loaded, false, true).is_err());
            // Quit without any model work cannot manufacture Completed.
            drop(owner);
            assert_eq!(
                imported(&session).native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );
            assert!(super::super::storage::save_session(&loaded).is_err());
        });
    }

    #[test]
    fn native_publication_startup_handoff_waits_for_exact_hook_and_writer() {
        let roots = crate::tests::OwnedTestDirectory::new("native-startup-transfer");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        runtime().block_on(async {
            let owner = NativeStartupPublication::begin(&mut session).await.unwrap();
            let scope = Arc::clone(&owner.scope);
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
            let task = owner
                .spawn_hook(async move {
                    let _ = started_tx.send(());
                    let _ = release_rx.await;
                })
                .await;
            started_rx.await.unwrap();
            assert_eq!(scope.active_children(), 1);
            let mut transfer = Box::pin(owner.into_model_ticket(&session));
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut transfer)
                    .await
                    .is_err()
            );
            release_tx.send(()).unwrap();
            task.await.unwrap();
            let ticket = transfer.await.unwrap();
            assert_eq!(scope.active_children(), 0);
            assert!(scope.is_cancelled());
            assert!(session.native_frontier.ready());
            assert_eq!(
                imported(&session).native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );
            assert!(ensure_actor_value(&session, &ticket).is_ok());
            drop(ticket);
        });
    }

    #[test]
    fn native_publication_startup_drop_cancels_hook_and_retains_denial() {
        let roots = crate::tests::OwnedTestDirectory::new("native-startup-drop");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        runtime().block_on(async {
            let owner = NativeStartupPublication::begin(&mut session).await.unwrap();
            let scope = Arc::clone(&owner.scope);
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            let hook_scope = Arc::clone(&scope);
            let task = owner
                .spawn_hook(async move {
                    hook_scope.cancelled().await;
                    let _ = release_rx.await;
                })
                .await;
            drop(owner);
            assert!(scope.is_cancelled());
            assert_eq!(scope.active_children(), 1);
            super::super::lock::release_session(&session.id);
            assert!(matches!(
                super::super::lock::claim_session(&session.id).unwrap(),
                super::super::lock::SessionClaim::HeldElsewhere
            ));
            release_tx.send(()).unwrap();
            task.await.unwrap();
            scope.wait_idle().await;
            assert_eq!(scope.active_children(), 0);
            assert!(scope.is_cancelled());
            assert_eq!(
                imported(&session).native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );
        });
    }

    #[test]
    fn native_publication_paired_refusal_rejects_foreign_and_unaccepted_invocations() {
        let roots = crate::tests::OwnedTestDirectory::new("native-paired-refusal-negative");
        let _environment = environment(&roots);
        runtime().block_on(async {
            for foreign in [false, true] {
                let mut session = Session::new("anthropic", "fixture", 32_000, "");
                let _release = Release(session.id.to_string());
                let mut other = Session::new("anthropic", "fixture", 32_000, "");
                let _other_release = Release(other.id.to_string());
                let scope = crate::agent::runner::AgentWorkScope::new();
                let mut ticket = begin_owned(&mut session, &scope).await.unwrap();
                let mut other_ticket = begin_owned(&mut other, &scope).await.unwrap();
                let independently_imported: Session = serde_json::from_str(
                    &super::super::storage::serialize_bounded_session(&session).unwrap(),
                )
                .unwrap();
                assert_eq!(independently_imported.id, session.id);
                assert!(
                    NativeActorPair::pair(&independently_imported, &mut ticket).is_err(),
                    "same-PID serde data cannot borrow the runtime actor ticket"
                );
                let pair = NativeActorPair::pair(
                    if foreign { &other } else { &session },
                    if foreign {
                        &mut other_ticket
                    } else {
                        &mut ticket
                    },
                )
                .unwrap();
                let result = crate::print::run_native_headless_turn(
                    async {
                        crate::agent::runner::HeadlessTurn::failed_before_start(anyhow::anyhow!(
                            "controlled zeroaccepted"
                        ))
                    },
                    pair.clone(),
                )
                .await;
                assert!(result.failure.is_some());
                assert_eq!(pair.0.state.lock().unwrap().phase, ActorPhase::Retired);
                assert!(pair.0.state.lock().unwrap().refusal.is_none());
                let before = serde_json::to_value(imported(&session)).unwrap();
                let live = serde_json::to_value(&session).unwrap();
                assert!(
                    publish_refused_owned(&mut session, ticket, pair, &scope)
                        .await
                        .is_err()
                );
                assert_eq!(serde_json::to_value(imported(&session)).unwrap(), before);
                assert_eq!(serde_json::to_value(&session).unwrap(), live);
                drop(other_ticket);
            }
        });
    }

    #[test]
    fn native_publication_paired_refused_writer_waiter_drop_and_stale_cutoff_stay_denied() {
        let roots = crate::tests::OwnedTestDirectory::new("native-paired-refusal-writer");
        let _environment = environment(&roots);
        runtime().block_on(async {
            for mode in [0, 1, 2] {
                let mut session = Session::new("anthropic", "fixture", 32_000, "");
                let _release = Release(session.id.to_string());
                let initial = crate::agent::runner::AgentWorkScope::new();
                let mut ticket = begin_owned(&mut session, &initial).await.unwrap();
                let epoch = ticket.epoch;
                let pair = NativeActorPair::pair(&session, &mut ticket).unwrap();
                crate::print::run_native_headless_turn(async {
                    crate::agent::runner::HeadlessTurn::failed_before_start(anyhow::anyhow!("fixture actor retirement"))
                }, pair.clone()).await;
                // Lower publisher contract fixture: only tests may construct
                // this private witness. Actual native binding proof is covered
                // by the separate one-POST canonical/interactive controls.
                session.native_frontier.refuse_expected(epoch, RestartReason::ChangedPrefix).unwrap();
                pair.accepted_refusal(&session, epoch, RestartReason::ChangedPrefix).unwrap();
                session.charge_usage_delta(crate::event::UsageDelta { input_tokens: 10, output_tokens: 2, ..Default::default() }, true);
                let before = serde_json::to_value(imported(&session)).unwrap();
                if mode == 2 {
                    session.native_frontier.refuse_expected(epoch + 1, RestartReason::ChangedDomain).unwrap();
                    assert!(publish_refused_owned(&mut session, ticket, pair, &initial).await.is_err());
                    assert_eq!(serde_json::to_value(imported(&session)).unwrap(), before);
                    assert_eq!(session.native_frontier.refusal(), Some(RestartReason::ChangedDomain));
                    continue;
                }
                let frontier = session.native_frontier.clone();
                let (io, started, release) = crate::agent::runner::AgentWorkScope::new_with_blocking_test_gate();
                let mut saving = Box::pin(publish_refused_owned(&mut session, ticket, pair.clone(), &io));
                tokio::select! {
                    result = &mut saving => panic!("held refused write ended: {result:?}"),
                    _ = async { tokio::time::timeout(Duration::from_secs(2), async { while started.try_recv().is_err() { tokio::task::yield_now().await; } }).await.unwrap(); } => {}
                }
                assert_eq!(io.active_children(), 1);
                if mode == 1 {
                    drop(saving);
                    super::super::lock::release_session(&session.id);
                    assert!(matches!(super::super::lock::claim_session(&session.id).unwrap(), super::super::lock::SessionClaim::HeldElsewhere));
                    assert_eq!(frontier.refusal(), Some(RestartReason::ChangedPrefix));
                    assert!(tokio::time::timeout(Duration::from_millis(20), io.wait_idle()).await.is_err());
                    release.release();
                    io.wait_idle().await;
                    assert_eq!(serde_json::to_value(imported(&session)).unwrap(), before);
                } else {
                    release.release();
                    saving.await.unwrap();
                    io.wait_idle().await;
                    let restored = imported(&session);
                    assert_eq!(restored.native_frontier.refusal(), Some(RestartReason::ChangedPrefix));
                    assert_eq!((restored.total_input_tokens, restored.total_output_tokens), (10, 2));
                    assert!(pair.0.state.lock().unwrap().consumed);
                    assert!(super::super::storage::save_session(&session).is_ok());
                }
                assert_eq!(io.active_children(), 0);
                assert_eq!(frontier.refusal(), Some(RestartReason::ChangedPrefix));
                assert_eq!((session.total_input_tokens, session.total_output_tokens), (10, 2));
            }
        });
    }

    #[test]
    fn native_publication_restored_unfinished_account_is_restrictive_without_invented_cost() {
        let roots = crate::tests::OwnedTestDirectory::new("native-restored-cost-unknown");
        let _environment = environment(&roots);
        runtime().block_on(async {
            let mut session = Session::new("anthropic", "fixture", 32_000, "");
            let _release = Release(session.id.to_string());
            session.set_input_output_prices(Some((2.0, 4.0)));
            session.initialize_spend_account(Some(1.0)).unwrap();
            let scope = crate::agent::runner::AgentWorkScope::new();
            let ticket = begin_owned(&mut session, &scope).await.unwrap();
            assert!(!session.spend_account.snapshot().tracking_error);
            drop(ticket);
            super::super::lock::release_session(&session.id);
            let mut restored = imported(&session);
            assert!(
                !restored.spend_account.snapshot().tracking_error,
                "old preflight baseline had no observation"
            );
            restored.initialize_spend_account(Some(1.0)).unwrap();
            assert!(restored.spend_account.snapshot().tracking_error);
            assert_eq!(restored.recorded_total_cost(), 0.0);
            assert_eq!(
                (restored.total_input_tokens, restored.total_output_tokens),
                (0, 0)
            );
            let cfg = crate::config::Config {
                max_budget_usd: Some(1.0),
                ..Default::default()
            };
            let run = crate::spend::run::Run::for_session(
                &mut restored,
                &cfg,
                crate::spend::RequestKind::Turn,
            );
            let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(Arc::clone(&scope));
            assert!(scope.run(run.prepare()).await.is_err());
            assert!(scope.run(run.prepare()).await.is_err());
            cleanup.settle().await;
            assert!(restored.spend_account.snapshot().tracking_error);
            assert_eq!(restored.recorded_total_cost(), 0.0);
            let ledger =
                crate::spend::Ledger::new(&crate::paths::process_paths().unwrap().state_dir);
            assert_eq!(
                ledger
                    .summary_for_session(&restored.id)
                    .unwrap()
                    .session
                    .records,
                0
            );
            // Uncapped policy remains unchanged. This does not heal the same
            // logical session or assert an unknown request was free.
            restored.initialize_spend_account(None).unwrap();
            assert!(restored.spend_account.admit_effect().is_ok());
            assert!(restored.spend_account.snapshot().tracking_error);
            let mut fresh = Session::new("anthropic", "fixture", 32_000, "");
            fresh.initialize_spend_account(Some(1.0)).unwrap();
            assert!(fresh.spend_account.admit_effect().is_ok());
            assert!(!fresh.spend_account.snapshot().tracking_error);
            assert_ne!(fresh.id, restored.id);
        });
    }

    #[test]
    fn native_publication_paired_headless_held_model_has_no_empty_scope_retirement() {
        let roots = crate::tests::OwnedTestDirectory::new("native-paired-held-model");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        runtime().block_on(async {
            let io = crate::agent::runner::AgentWorkScope::new();
            let mut ticket = begin_owned(&mut session, &io).await.unwrap();
            let pair = NativeActorPair::pair(&session, &mut ticket).unwrap();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let model = async move {
                started_tx.send(()).unwrap();
                std::future::pending::<crate::agent::runner::HeadlessTurn>().await
            };
            let mut work = Box::pin(crate::print::run_native_headless_turn(model, pair.clone()));
            tokio::select! {
                _ = started_rx => {},
                _ = &mut work => panic!("held model unexpectedly ended"),
            }
            assert_eq!(pair.0.state.lock().unwrap().phase, ActorPhase::Running);
            assert_eq!(io.active_children(), 0);
            io.wait_idle().await;
            assert_eq!(pair.0.state.lock().unwrap().phase, ActorPhase::Running);
            assert!(pair.0.state.lock().unwrap().refusal.is_none());
            drop(ticket);
            super::super::lock::release_session(&session.id);
            assert!(matches!(
                super::super::lock::claim_session(&session.id).unwrap(),
                super::super::lock::SessionClaim::HeldElsewhere
            ));
            drop(work);
            tokio::time::timeout(Duration::from_secs(2), async {
                while pair.0.state.lock().unwrap().phase != ActorPhase::Retired {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(pair.start().is_err());
            assert!(pair.0.state.lock().unwrap().refusal.is_none());
            assert_eq!(
                imported(&session).native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );
        });
    }

    #[test]
    fn native_publication_paired_model_drop_keeps_lease_until_exact_children_join() {
        let roots = crate::tests::OwnedTestDirectory::new("native-paired-held-child");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        runtime().block_on(async {
            let io = crate::agent::runner::AgentWorkScope::new();
            let mut ticket = begin_owned(&mut session, &io).await.unwrap();
            let pair = NativeActorPair::pair(&session, &mut ticket).unwrap();
            let scope = crate::agent::runner::AgentWorkScope::new();
            let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(Arc::clone(&scope));
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            let (child,) = scope
                .run(async move {
                    // Return the admitted handle without joining the held
                    // child until the model-drop assertion below.
                    let child = crate::agent::runner::spawn_async_scoped(async move {
                        let _ = release_rx.await;
                    });
                    (child,)
                })
                .await;
            let model = std::future::pending::<()>();
            let owner = NativeModelOwner::new(model, pair.clone(), cleanup).unwrap();
            assert_eq!(scope.active_children(), 1);
            drop(ticket);
            drop(owner);
            super::super::lock::release_session(&session.id);
            assert!(matches!(
                super::super::lock::claim_session(&session.id).unwrap(),
                super::super::lock::SessionClaim::HeldElsewhere
            ));
            assert_eq!(pair.0.state.lock().unwrap().phase, ActorPhase::ModelEnded);
            assert_eq!(scope.active_children(), 1);
            release_tx.send(()).unwrap();
            child.await.unwrap();
            scope.wait_idle().await;
            tokio::time::timeout(Duration::from_secs(2), async {
                while pair.0.state.lock().unwrap().phase != ActorPhase::Retired {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(scope.active_children(), 0);
            assert!(scope.is_cancelled());
            assert!(matches!(
                super::super::lock::claim_session(&session.id).unwrap(),
                super::super::lock::SessionClaim::Owned
            ));
        });
    }

    #[test]
    fn native_publication_startup_failed_hook_cannot_transfer() {
        let roots = crate::tests::OwnedTestDirectory::new("native-startup-failed-hook");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        runtime().block_on(async {
            let owner = NativeStartupPublication::begin(&mut session).await.unwrap();
            let task = owner
                .spawn_hook(async { panic!("controlled startup task failure") })
                .await;
            assert!(owner.hook_retired(task.await).is_err());
            assert!(owner.into_model_ticket(&session).await.is_err());
            assert_eq!(
                imported(&session).native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );
        });
    }
}
