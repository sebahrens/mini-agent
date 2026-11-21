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
    /// Generation of the actor authority (a ticket or a retired-session permit).
    /// It advances when an actor begins or retires, never for an idle republish
    /// of metadata under the same owner, so a permit survives a title, a
    /// `/rename` or a permission save while a stale one still cannot act.
    authority: u64,
}

/// This cell belongs to the existing frontier, not a path or id registry.
#[derive(Debug, Default)]
pub(super) struct PublicationCell {
    state: Mutex<PublicationState>,
    /// Used only inside owned blocking workers. Never held by an async poll.
    io: Mutex<()>,
}

impl PublicationCell {
    pub(super) fn is_claimed(&self) -> bool {
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
    encoding: super::storage::SaveEncoding,
    #[cfg(feature = "subagents")]
    observations: Option<Arc<super::background_observation::SharedObservations>>,
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
    actor_pair: Option<std::sync::Weak<ActorPairInner>>,
    /// An idle control republish: it leaves the permit generation unchanged.
    idle: bool,
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
    session_end_issued: bool,
    acknowledged_retirement: Option<AcknowledgedRetirement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AcknowledgedRetirement {
    revision: u64,
    digest: [u8; 32],
    epoch: u64,
    outcome: RetiredNativeOutcome,
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
        let pair = Self(Arc::new(ActorPairInner {
            owner: Arc::clone(&ticket.publication.owner),
            epoch: ticket.epoch,
            state: Mutex::new(ActorState {
                phase: ActorPhase::Prepared,
                writer: Some(writer),
                refusal: None,
                consumed: false,
                session_end_issued: false,
                acknowledged_retirement: None,
            }),
        }));
        ticket.actor_pair = Some(Arc::downgrade(&pair.0));
        Ok(pair)
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

    /// Open startup ownership. A chain that cannot be replayed (a recoverable
    /// refusal, a different model, a compacted transcript, or the finite
    /// bounds) is dropped first and a new epoch starts from the transcript, so
    /// such a session resumes instead of being refused; the one-line notice
    /// goes to stderr.
    pub(crate) async fn begin(session: &mut Session) -> anyhow::Result<Self> {
        let (startup, cause) = Self::begin_with_cause(session).await?;
        if let Some(cause) = cause {
            eprintln!("{}", cause.notice());
        }
        Ok(startup)
    }

    /// [`begin`](Self::begin) for a caller that must not write to stderr (a
    /// running TUI): the restart notice is returned instead.
    pub(crate) async fn begin_with_cause(
        session: &mut Session,
    ) -> anyhow::Result<(Self, Option<super::native_replay::NativeRestartCause>)> {
        let scope = crate::agent::runner::AgentWorkScope::new();
        let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(Arc::clone(&scope));
        let unfinished = session.native_unfinished_cause();
        let restart = session.native_restart_cause().or(unfinished);
        if unfinished.is_some() {
            // A fresh process that claims the session's lock below is proof the
            // writer that left the in-flight denial is gone. If the claim fails
            // (another live process), startup aborts before anything persists.
            session.restart_unfinished_native_chain()?;
        } else if restart.is_some() {
            session.restart_native_chain()?;
        }
        let ticket = begin_owned_with(session, &scope, restart.is_some()).await?;
        Ok((
            Self {
                scope,
                cleanup: Some(cleanup),
                ticket: Some(ticket),
            },
            restart,
        ))
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
        "saved native context requires persistence before startup; drop --no-session to resume it, or add --fork-session to continue in a copy"
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

/// [`checked_publication`] that also returns the canonical encoding it hashed,
/// so an unprojected write of the same Session reuses it.
fn checked_publication_encoded(session: &Session) -> anyhow::Result<(&NativePublication, String)> {
    let publication = checked_publication_owner(session)?;
    let encoded = super::storage::serialize_bounded_session(session)?;
    let digest: [u8; 32] = Sha256::digest(encoded.as_bytes()).into();
    anyhow::ensure!(
        digest == publication.digest,
        "native context snapshot changed before publication; run `/fork` (or restart with `--continue --fork-session`) to continue from the visible transcript"
    );
    Ok((publication, encoded))
}

fn checked_publication_owner(session: &Session) -> anyhow::Result<&NativePublication> {
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
    Ok(publication)
}

fn checked_publication(session: &Session) -> anyhow::Result<&NativePublication> {
    let publication = checked_publication_owner(session)?;
    anyhow::ensure!(
        digest(session)? == publication.digest,
        "native context snapshot changed before publication; run `/fork` (or restart with `--continue --fork-session`) to continue from the visible transcript"
    );
    Ok(publication)
}

// Derived cleanup/control authority still binds the entire published Session.
// Only genuine same-owner observations may advance behind its retired actor;
// their new bytes are written only by the existing begin/retire transition.
fn checked_retired_publication(session: &Session) -> anyhow::Result<&NativePublication> {
    #[cfg(feature = "subagents")]
    let publication = checked_publication_owner(session)?;
    #[cfg(feature = "subagents")]
    if let Some(previous) = &publication.observations {
        previous.current_since(session)?;
        anyhow::ensure!(
            digest(&previous.frozen_session(session))? == publication.digest,
            "native context changed beyond background observations"
        );
        return Ok(publication);
    }
    checked_publication(session)
}

fn unchanged_retired_body(
    session: &Session,
    publication: &NativePublication,
) -> anyhow::Result<bool> {
    #[cfg(feature = "subagents")]
    if let Some(previous) = &publication.observations {
        previous.current_since(session)?;
        return Ok(digest(&previous.frozen_session(session))? == publication.digest);
    }
    Ok(digest(session)? == publication.digest)
}

#[cfg(feature = "subagents")]
fn freeze_shared_observations(
    session: &mut Session,
) -> Option<Arc<super::background_observation::SharedObservations>> {
    let observations =
        super::background_observation::SharedObservations::capture(session).map(Arc::new);
    if let Some(observations) = &observations {
        *session = observations.frozen_session(session);
    }
    observations
}

// This does not publish or issue a completed-actor permit. Only the existing
// retired->staging begin may use it, under its exact writer/revision checks.
#[cfg(feature = "subagents")]
fn observed_begin_snapshot(
    session: &mut Session,
) -> anyhow::Result<Option<Arc<super::background_observation::SharedObservations>>> {
    let publication = checked_publication_owner(session)?;
    match &publication.observations {
        Some(previous) => {
            // Reconstruct the ENTIRE prior Session, not an excluded-field hash.
            // Even a byte-identical replacement runtime store is forbidden.
            let next = previous.current_since(session)?;
            anyhow::ensure!(
                digest(&previous.frozen_session(session))? == publication.digest,
                "native context changed beyond background observations"
            );
            *session = next.frozen_session(session);
            Ok(Some(Arc::new(next)))
        }
        None => {
            checked_publication(session)?;
            Ok(freeze_shared_observations(session))
        }
    }
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
    save_snapshot_inner(session, false)
}

/// The cheap, synchronous part of a snapshot's admission: the exact retained
/// owner of this Session. The digest comparison runs with the write.
pub(super) fn ensure_snapshot_owner(session: &Session) -> anyhow::Result<()> {
    checked_publication_owner(session).map(|_| ())
}

/// The deferred writer's form of [`save_snapshot`]. The snapshot was cloned
/// from the live Session some time ago, so its owner may have since published
/// a newer revision (next turn, progress, retirement). That checkpoint already
/// carries everything this snapshot did, so the older one is dropped instead
/// of reported; every other refusal is unchanged.
pub(super) fn save_snapshot_deferred(session: &Session) -> anyhow::Result<()> {
    save_snapshot_inner(session, true)
}

fn save_snapshot_inner(session: &Session, drop_superseded: bool) -> anyhow::Result<()> {
    let (publication, encoded) = checked_publication_encoded(session)?;
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
        if drop_superseded && publication.revision != state.revision {
            tracing::debug!("deferred native snapshot superseded by a newer checkpoint");
            return Ok(());
        }
        validate_revision(publication, &state)?;
        state.phase
    };
    write_projection_with(session, phase == Phase::Active, Some(encoded))
}

fn write_projection(session: &Session, in_flight: bool) -> anyhow::Result<()> {
    write_projection_with(session, in_flight, None)
}

/// `encoded` is the canonical encoding of exactly `session`; it is the bytes
/// to write when no in-flight projection changes the Session.
fn write_projection_with(
    session: &Session,
    in_flight: bool,
    encoded: Option<String>,
) -> anyhow::Result<()> {
    // The in-flight projection differs from `session` only in its encoded
    // frontier, which the thread-local encoding mode supplies; no Session
    // clone is built. Validation reads only the (unchanged) epoch.
    session.validate_native_persistence()?;
    let json = match encoded.filter(|_| !in_flight) {
        Some(encoded) => encoded,
        None => {
            let _projection = in_flight.then(super::native_replay::InFlightEncoding::install);
            super::storage::serialize_bounded_session(session)?
        }
    };
    write_encoded_projection(session, &json)
}

fn write_encoded_projection(session: &Session, json: &str) -> anyhow::Result<()> {
    session.validate_native_persistence()?;
    let publication = session
        .native_publication
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("native checkpoint requires its retained writer"))?;
    publication
        .owner
        .lease
        .publish_checkpoint(session, json.as_bytes())
}

/// Synchronous core called only inside the exact owned blocking worker.
///
/// `restarted` is true only after the host dropped the native chain
/// ([`Session::restart_native_chain`]). The previous publication's *body*
/// digest is then not compared: the transcript may legitimately have changed
/// since (compaction, a model switch), and there are no opaque frames left for
/// that digest to protect. The retained owner, lease, publication revision and
/// retired phase are still all required, so a stale clone cannot publish.
fn begin_blocking(
    mut snapshot: Session,
    restarted: bool,
    idle: bool,
) -> anyhow::Result<(NativePublication, NativePublicationTicket)> {
    anyhow::ensure!(
        snapshot.native_frontier.ready(),
        "native context is unfinished; run `/fork` (or restart with `--continue --fork-session`) to continue from the visible transcript"
    );
    let cell = snapshot.native_frontier.publication_cell();
    let owner = match &snapshot.native_publication {
        Some(_) => Arc::clone(&checked_publication_owner(&snapshot)?.owner),
        None => Arc::new(WriterOwner {
            lease: super::lock::claim_native_writer(&snapshot)?,
            cell,
        }),
    };
    #[cfg(feature = "subagents")]
    let observations = if snapshot.native_publication.is_some() && !restarted {
        observed_begin_snapshot(&mut snapshot)?
    } else {
        freeze_shared_observations(&mut snapshot)
    };
    #[cfg(not(feature = "subagents"))]
    if snapshot.native_publication.is_some() && !restarted {
        checked_publication(&snapshot)?;
    }
    snapshot.native_frontier.activate_publication();
    let (canonical, denied, encoding) = super::storage::encode_owned_session(&mut snapshot, true)?;
    let current_digest: [u8; 32] = Sha256::digest(canonical.as_bytes()).into();
    drop(canonical);
    let publication = {
        let mut state = owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match &snapshot.native_publication {
            // A failed publication leaves no live actor once every writer guard
            // is gone. The exact lease holder may then start a new epoch from
            // the transcript; the failed value is never replayed.
            Some(previous)
                if restarted
                    && state.phase == Phase::Failed
                    && previous.owner.lease.live_writers() == 0 =>
            {
                anyhow::ensure!(
                    Arc::ptr_eq(&previous.owner, &owner),
                    "unfinished native context belongs to another session owner"
                );
            }
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
        if !idle {
            state.authority = state.authority.wrapping_add(1);
        }
        NativePublication {
            owner: Arc::clone(&owner),
            revision: state.revision,
            digest: current_digest,
            encoding,
            #[cfg(feature = "subagents")]
            observations,
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
    let result =
        write_encoded_projection(&snapshot, denied.as_deref().expect("in-flight encoding"))
            .and_then(|()| writer.check());
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
            actor_pair: None,
            idle,
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
    begin_owned_with(session, scope, false).await
}

/// [`begin_owned`] for a frontend that has no idle owner of its own (ACP): a
/// chain that cannot be replayed, or a failed publication, is dropped and a
/// new epoch starts from the transcript, as the TUI and headless frontends
/// do. Returns the restart cause for the user-facing notice.
#[cfg(feature = "acp")]
pub(crate) async fn begin_owned_reconciled(
    session: &mut Session,
    scope: &Arc<crate::agent::runner::AgentWorkScope>,
) -> anyhow::Result<(
    NativePublicationTicket,
    Option<super::native_replay::NativeRestartCause>,
)> {
    // Metadata the editor changed while idle (a rename, a title, usage) is
    // published as its own revision first, as the TUI does.
    let republished = recover_idle(session)?.1;
    let unfinished = session.native_unfinished_cause();
    let failed = session.native_publication.as_ref().is_some_and(|p| {
        p.owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .phase
            == Phase::Failed
            && p.owner.lease.live_writers() == 0
    });
    let cause = session
        .native_restart_cause()
        .or(unfinished)
        .or_else(|| failed.then_some(super::native_replay::NativeRestartCause::WriteFailed))
        .or(republished);
    if unfinished.is_some() {
        session.restart_unfinished_native_chain()?;
    } else if cause.is_some() {
        session.restart_native_chain()?;
    }
    let ticket = begin_owned_with(session, scope, cause.is_some()).await?;
    Ok((ticket, cause))
}

async fn begin_owned_with(
    session: &mut Session,
    scope: &Arc<crate::agent::runner::AgentWorkScope>,
    restarted: bool,
) -> anyhow::Result<NativePublicationTicket> {
    let snapshot = session.clone();
    let (publication, ticket) = scope
        .run(async move {
            crate::agent::runner::spawn_blocking_scoped(move || {
                begin_blocking(snapshot, restarted, false)
            })
            .await
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result)
        .inspect_err(|error| super::storage::queue_native_save_failure(session, error))?;
    super::storage::adopt_native_encoding(session, publication.encoding);
    session.native_publication = Some(publication);
    Ok(ticket)
}

/// The retirement outcome that matches a session's saved replay boundary: a
/// chain-less transcript and a completed turn are `Completed`.
fn boundary_outcome(session: &Session) -> anyhow::Result<RetiredNativeOutcome> {
    Ok(match session.native_history_snapshot() {
        Some(history) => match history.saved_boundary()? {
            super::native_replay::ReplayBoundary::CompletedTurn => RetiredNativeOutcome::Completed,
            super::native_replay::ReplayBoundary::LiveToolReply => {
                RetiredNativeOutcome::LiveToolReply
            }
        },
        None => RetiredNativeOutcome::Completed,
    })
}

/// Retire the idle startup ticket of an interactive session back to a healthy
/// checkpoint. No model request was admitted, so nothing is unfinished: the
/// resumed (or restarted-from-transcript) value is published as it stands and
/// the session is then an ordinary idle native session. This is what keeps a
/// quit before the first prompt from leaving an in-flight file behind.
pub(crate) async fn retire_idle_startup(
    session: &mut Session,
    ticket: NativePublicationTicket,
) -> anyhow::Result<RetiredSessionEndPermit> {
    anyhow::ensure!(
        !ticket.paired,
        "native context retirement requires the idle startup owner"
    );
    ensure_actor_value(session, &ticket)?;
    let outcome = boundary_outcome(session)?;
    let scope = crate::agent::runner::AgentWorkScope::new();
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
    let result = retire_owned(session, ticket, outcome, &scope).await;
    cleanup.settle().await;
    result?;
    RetiredSessionEndPermit::from_idle_retirement(session)
}

/// Open native ownership of a new or freshly loaded idle session and publish it
/// as a healthy checkpoint at once (no model work is involved): a forked
/// session, or one entered through `/sessions`. A chain the transcript no
/// longer matches is dropped as the startup path does. Returns the restart
/// cause when it was.
pub(crate) async fn publish_new_session(
    session: &mut Session,
) -> anyhow::Result<Option<super::native_replay::NativeRestartCause>> {
    let (startup, cause) = NativeStartupPublication::begin_with_cause(session).await?;
    let ticket = startup.into_model_ticket(session).await?;
    retire_idle_startup(session, ticket).await?;
    Ok(cause)
}

/// Whether this session is currently idle under a retired native publication
/// whose owner is this process, so a [`RetiredSessionEndPermit`] can be minted.
pub(crate) fn idle_permit(session: &Session) -> anyhow::Result<RetiredSessionEndPermit> {
    RetiredSessionEndPermit::from_idle_retirement(session)
}

/// Republish an idle session that changed after its last publication, or whose
/// failed publication left no live actor. `(false, None)` when nothing was
/// needed; otherwise whether a republish ran and the restart cause when the
/// chain was dropped.
pub(crate) fn recover_idle(
    session: &mut Session,
) -> anyhow::Result<(bool, Option<super::native_replay::NativeRestartCause>)> {
    if !idle_republish_needed(session) {
        return Ok((false, None));
    }
    Ok((true, republish_idle_sync(session)?))
}

impl NativePublicationTicket {
    /// Whether this ticket belongs to the session's own writer lease, not to a
    /// session that was left behind by a switch.
    pub(crate) fn belongs_to(&self, session: &Session) -> bool {
        self.publication.owner.lease.matches(session)
    }
}

impl RetiredSessionEndPermit {
    pub(crate) fn belongs_to(&self, session: &Session) -> bool {
        self.publication.owner.lease.matches(session)
    }
}

/// Whether an idle, native-bearing session needs a new revision before it can
/// be saved or start a turn: its metadata changed after the last publication
/// (title, name, permissions, schedules, usage charges), or a failed
/// publication left no live actor. Anything else (an active turn, a stale clone
/// or a session this process does not own) is left to the strict paths.
pub(super) fn idle_republish_needed(session: &Session) -> bool {
    idle_republish_needed_with(session, true)
}

/// `compare_digest` false skips the whole-session encode a changed-metadata
/// check needs (the turn-end deferred save keeps its no-encode contract); a
/// failed publication and a replaced observation owner are still detected.
pub(super) fn idle_republish_needed_with(session: &Session, compare_digest: bool) -> bool {
    let Some(publication) = session.native_publication.as_ref() else {
        return false;
    };
    if !publication.owner.lease.matches(session)
        || !Arc::ptr_eq(
            &publication.owner.cell,
            &session.native_frontier.publication_cell(),
        )
    {
        return false;
    }
    let (phase, current) = {
        let state = publication
            .owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        (
            state.phase,
            publication.revision == state.revision && publication.digest == state.digest,
        )
    };
    // `/undo` and `/clear` replace the background task pool (runtime-only): the
    // retired publication still names the old observation owner, so a turn
    // could not begin. The same exact owner re-baselines it from the session.
    #[cfg(feature = "subagents")]
    let replaced = publication.observations.is_some() && session.background_observations.is_none();
    #[cfg(not(feature = "subagents"))]
    let replaced = false;
    match phase {
        Phase::Retired => {
            current
                && (replaced
                    || (compare_digest
                        && digest(session).is_ok_and(|now| now != publication.digest)))
        }
        Phase::Failed => publication.owner.lease.live_writers() == 0,
        _ => false,
    }
}

/// Publish the changed idle `session` as a new revision under its own owner.
/// A chain the new transcript no longer matches (or one a failed publication
/// left unreliable) is dropped first and replaced by a new epoch; the previous
/// value is denied on disk before the healthy payload replaces it, so an
/// interrupted republish never leaves a stale chain replayable. The actor
/// authority generation is unchanged. Returns the restart cause when the chain
/// was dropped.
pub(super) fn republish_idle_sync(
    session: &mut Session,
) -> anyhow::Result<Option<super::native_replay::NativeRestartCause>> {
    let failed = session.native_publication.as_ref().is_some_and(|p| {
        p.owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .phase
            == Phase::Failed
    });
    let cause = session
        .native_restart_cause()
        .or_else(|| failed.then_some(super::native_replay::NativeRestartCause::WriteFailed));
    if cause.is_some() {
        session.restart_native_chain()?;
    }
    let result = (|| -> anyhow::Result<()> {
        let (publication, ticket) = begin_blocking(session.clone(), true, true)?;
        super::storage::adopt_native_encoding(session, publication.encoding);
        session.native_publication = Some(publication);
        let outcome = boundary_outcome(session)?;
        let publication = retire_blocking(session.clone(), ticket, outcome)?;
        super::storage::adopt_native_encoding(session, publication.encoding);
        session.native_publication = Some(publication);
        Ok(())
    })();
    if let Err(error) = result {
        if cause.is_some() {
            // The in-memory chain is gone but its replacement was not
            // acknowledged: stay conservative instead of replaying a stale one.
            let _ = session.native_frontier.refuse_expected(
                session.native_frontier.current(),
                RestartReason::IncompleteTurn,
            );
        }
        super::storage::queue_native_save_failure(session, &error);
        return Err(error);
    }
    Ok(cause)
}

/// Drop the native chain under a retained idle ticket (a startup ticket that
/// has not met a model yet) and republish the new epoch's transcript. The
/// session stays denied on disk until the next model turn retires, exactly as
/// before the restart; no completed boundary is minted.
pub(crate) async fn restart_with_ticket(
    session: &mut Session,
    ticket: &mut NativePublicationTicket,
    scope: &Arc<crate::agent::runner::AgentWorkScope>,
) -> anyhow::Result<()> {
    ensure_actor_value(session, ticket)?;
    anyhow::ensure!(
        !ticket.paired,
        "native context restart requires the idle startup owner"
    );
    session.restart_native_chain()?;
    ticket.epoch = session.native_frontier.current();
    progress_owned(session, ticket, scope).await
}

/// Durably republishes a native-bearing session after an idle control command
/// changed it without a model turn (`/compact`, `/undo`, `/rename` over ACP).
/// A chain the new transcript no longer matches is dropped first and replaced
/// by a new epoch; the old value is denied on disk before the healthy payload
/// replaces it, exactly as [`NativeSessionEndOwner::restart_chain`] does for an
/// idle host change, so an interrupted republish never leaves a stale chain
/// behind. Nothing is published for an unfinished context.
#[cfg(feature = "acp")]
pub(crate) async fn republish_idle(session: &mut Session) -> anyhow::Result<()> {
    let scope = crate::agent::runner::AgentWorkScope::new();
    let mut cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
    if let Some(publication) = &session.native_publication {
        cleanup.retain_until_settled(publication.owner.lease.begin_write()?);
    }
    let mut dropped_chain = false;
    let result = async {
        if session.native_restart_cause().is_some() {
            session.restart_native_chain()?;
            dropped_chain = true;
        }
        // The Session changed since its last digest: this is a new revision
        // (like a restart), not a continuation of the retired one.
        let ticket = begin_owned_with(session, &scope, true).await?;
        retire_owned(session, ticket, RetiredNativeOutcome::Completed, &scope).await
    }
    .await;
    cleanup.settle().await;
    if let Err(error) = result {
        if dropped_chain {
            // The in-memory chain is gone but its replacement was not
            // acknowledged: stay conservative instead of replaying a stale
            // chain.
            let _ = session.native_frontier.refuse_expected(
                session.native_frontier.current(),
                RestartReason::IncompleteTurn,
            );
        }
        return Err(error);
    }
    Ok(())
}

/// Before the first model, only the retained, unpaired startup ticket may
/// adopt an explicit control. It stays InFlight; no completed boundary is made.
pub(crate) async fn startup_update_reasoning(
    session: &mut Session,
    ticket: &mut NativePublicationTicket,
    next: crate::provider::effort::SessionReasoning,
    scope: &Arc<crate::agent::runner::AgentWorkScope>,
) -> anyhow::Result<()> {
    checked_publication(session)?;
    ensure_actor_value(session, ticket)?;
    anyhow::ensure!(
        !ticket.paired,
        "native control update requires the idle startup owner"
    );
    if session.reasoning_controls != next {
        session.reasoning_controls = next;
        progress_owned(session, ticket, scope).await?;
    }
    Ok(())
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
    let mut snapshot = session.clone();
    #[cfg(feature = "subagents")]
    let observations = freeze_shared_observations(&mut snapshot);
    let (canonical, denied, encoding) = super::storage::encode_owned_session(&mut snapshot, true)?;
    let digest: [u8; 32] = Sha256::digest(canonical.as_bytes()).into();
    // This detached data projection is never a runtime Session. Decode the
    // exact captured bytes so mutable account/todo/goal owners cannot change
    // queued contents. Apply denial before validating an unfinished feed.
    let mut projection: Session = serde_json::from_str(&canonical)?;
    projection.native_frontier = projection.native_frontier.in_flight_projection();
    projection.validate_native_persistence()?;
    let denied = denied.expect("in-flight encoding");
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
            encoding,
            #[cfg(feature = "subagents")]
            observations,
        }
    };
    super::storage::record_native_undo_drop(session, encoding);
    session.native_publication = Some(publication.clone());
    ticket.publication = publication.clone();
    snapshot.native_publication = Some(publication.clone());
    Ok((snapshot, denied, publication))
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
    let (identity, denied, publication) = seal_progress(session, ticket).inspect_err(|error| {
        super::storage::queue_native_save_failure(session, error);
    })?;
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
    super::storage::adopt_native_encoding(session, awaiting.publication.encoding);
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
    let mut snapshot = session.clone();
    #[cfg(feature = "subagents")]
    let observations = freeze_shared_observations(&mut snapshot);
    let (json, _, encoding) = super::storage::encode_owned_session(&mut snapshot, false)
        .inspect_err(|error| {
            super::storage::queue_native_save_failure(session, error);
        })?;
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
        state.authority = state.authority.wrapping_add(1);
        NativePublication {
            owner: Arc::clone(&owner),
            revision: state.revision,
            digest,
            encoding,
            #[cfg(feature = "subagents")]
            observations,
        }
    };
    super::storage::record_native_undo_drop(session, encoding);
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
                drop(state);
                acknowledge_retired_actor(
                    &ticket,
                    &publication,
                    cutoff,
                    RetiredNativeOutcome::Incomplete,
                )?;
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
    super::storage::adopt_native_encoding(session, awaiting.publication.encoding);
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
    #[cfg(feature = "subagents")]
    let observations = freeze_shared_observations(&mut snapshot);
    let (canonical, _, encoding) = super::storage::encode_owned_session(&mut snapshot, false)?;
    let next_digest: [u8; 32] = Sha256::digest(canonical.as_bytes()).into();
    drop(canonical);
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
        if !ticket.idle {
            state.authority = state.authority.wrapping_add(1);
        }
        NativePublication {
            owner: Arc::clone(owner),
            revision: state.revision,
            digest: next_digest,
            encoding,
            #[cfg(feature = "subagents")]
            observations,
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
    drop(state);
    acknowledge_retired_actor(
        &ticket,
        &publication,
        snapshot.native_frontier.current(),
        outcome,
    )?;
    Ok(publication)
}

fn acknowledge_retired_actor(
    ticket: &NativePublicationTicket,
    publication: &NativePublication,
    epoch: u64,
    outcome: RetiredNativeOutcome,
) -> anyhow::Result<()> {
    let Some(pair) = ticket
        .actor_pair
        .as_ref()
        .and_then(std::sync::Weak::upgrade)
    else {
        return Ok(());
    };
    anyhow::ensure!(
        Arc::ptr_eq(&pair.owner, &publication.owner),
        "native retirement belongs to another writer"
    );
    let mut actor = pair.state.lock().unwrap_or_else(|e| e.into_inner());
    anyhow::ensure!(
        actor.phase == ActorPhase::Retired
            && actor.consumed
            && actor.acknowledged_retirement.is_none(),
        "native actor retirement was already acknowledged"
    );
    actor.acknowledged_retirement = Some(AcknowledgedRetirement {
        revision: publication.revision,
        digest: publication.digest,
        epoch,
        outcome,
    });
    Ok(())
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
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result)
        .inspect_err(|error| super::storage::queue_native_save_failure(session, error))?;
    super::storage::adopt_native_encoding(session, publication.encoding);
    session.native_publication = Some(publication);
    Ok(())
}

/// A runtime-only value permit from an already retired, published actor. This
/// role can run SessionEnd without inventing another model completion.
pub(crate) struct RetiredSessionEndPermit {
    publication: NativePublication,
    authority: u64,
    outcome: RetiredNativeOutcome,
    #[cfg(test)]
    fail_next_control_publication: bool,
}

impl RetiredSessionEndPermit {
    pub(crate) fn from_retired_actor(
        session: &Session,
        pair: &NativeActorPair,
    ) -> anyhow::Result<Self> {
        let publication = checked_retired_publication(session)?;
        let mut actor = pair.0.state.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            Arc::ptr_eq(&pair.0.owner, &publication.owner)
                && actor.phase == ActorPhase::Retired
                && actor.consumed
                && !actor.session_end_issued,
            "session cleanup requires its retired model owner"
        );
        let acknowledged = actor
            .acknowledged_retirement
            .ok_or_else(|| anyhow::anyhow!("native retirement was not durably acknowledged"))?;
        anyhow::ensure!(
            acknowledged.revision == publication.revision
                && acknowledged.digest == publication.digest
                && acknowledged.epoch == session.native_frontier.current(),
            "session cleanup value belongs to another retired actor"
        );
        let state = publication
            .owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        validate_revision(publication, &state)?;
        anyhow::ensure!(
            state.phase == Phase::Retired,
            "session cleanup precedes publication"
        );
        actor.session_end_issued = true;
        Ok(Self {
            publication: publication.clone(),
            authority: state.authority,
            outcome: acknowledged.outcome,
            #[cfg(test)]
            fail_next_control_publication: false,
        })
    }

    /// The permit's actor authority is still the current one for `publication`
    /// (the session's own, already digest-checked, publication). An idle
    /// republish of metadata keeps the generation; any actor begin or
    /// retirement advances it, so an older permit cannot act.
    fn verify(&self, publication: &NativePublication) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(&publication.owner, &self.publication.owner),
            "finish or recover the current native context before continuing"
        );
        let state = publication
            .owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            state.authority == self.authority,
            "finish or recover the current native context before continuing"
        );
        validate_revision(publication, &state)
    }

    /// Adopt the publication this permit's own control just wrote.
    fn rebind(&mut self, publication: NativePublication) {
        self.authority = publication
            .owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .authority;
        self.publication = publication;
    }

    /// A permit for a session whose idle startup ticket was retired back to a
    /// healthy checkpoint without any model work (a resumed session, or a
    /// quit before the first prompt). It carries the same SessionEnd, effort
    /// and restart authority a retired model actor would.
    pub(crate) fn from_idle_retirement(session: &Session) -> anyhow::Result<Self> {
        let publication = checked_retired_publication(session)?;
        let state = publication
            .owner
            .cell
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        validate_revision(publication, &state)?;
        anyhow::ensure!(
            state.phase == Phase::Retired,
            "idle native startup was not retired"
        );
        Ok(Self {
            publication: publication.clone(),
            authority: state.authority,
            outcome: boundary_outcome(session)?,
            #[cfg(test)]
            fail_next_control_publication: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn fail_next_control_publication(&mut self) {
        self.fail_next_control_publication = true;
    }

    /// An explicit idle control may change only this closed setting. The
    /// existing completed actor supplies the outcome; the command supplies no
    /// model-completion witness and cannot heal a refused/live-tool boundary.
    pub(crate) async fn update_reasoning(
        &mut self,
        session: &mut Session,
        host: Option<&crate::provider::NativeHostGuard>,
        next: crate::provider::effort::SessionReasoning,
    ) -> anyhow::Result<()> {
        let publication = checked_retired_publication(session)?;
        self.verify(publication)?;
        anyhow::ensure!(
            session.native_frontier.ready() && self.outcome == RetiredNativeOutcome::Completed,
            "finish or recover the current native context before changing effort"
        );
        let published = publication.clone();
        let history = session
            .native_history_snapshot()
            .ok_or(RestartReason::UnavailableSource)?;
        anyhow::ensure!(
            history.saved_boundary()? == super::native_replay::ReplayBoundary::CompletedTurn,
            "finish the native tool reply before changing effort"
        );
        if let Some(host) = host {
            host.validate()?;
        }
        if session.reasoning_controls == next {
            return Ok(());
        }
        #[cfg(test)]
        let fail = std::mem::take(&mut self.fail_next_control_publication);
        let previous = session.reasoning_controls;
        let scope = crate::agent::runner::AgentWorkScope::new();
        let mut cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
        cleanup.retain_until_settled(self.publication.owner.lease.begin_write()?);
        // The old value is durably denied before mutation. An interrupted
        // setter never restores healthy data or reruns lifecycle hooks.
        let result = async {
            let mut ticket = begin_owned(session, &scope).await?;
            session.reasoning_controls = next;
            let mut check = session.clone();
            check.reasoning_controls = previous;
            anyhow::ensure!(
                unchanged_retired_body(&check, &published)?,
                "session context changed while updating effort"
            );
            progress_owned(session, &mut ticket, &scope).await?;
            let snapshot = session.clone();
            let host = host.cloned();
            let outcome = self.outcome;
            let publication = scope
                .run(async move {
                    crate::agent::runner::spawn_blocking_scoped(move || {
                        retire_blocking_using(snapshot, ticket, outcome, |snapshot, in_flight| {
                            if let Some(host) = &host {
                                host.validate()?;
                            }
                            #[cfg(test)]
                            if fail {
                                anyhow::bail!(
                                    "controlled effort publication failure before replacement"
                                );
                            }
                            write_projection(snapshot, in_flight)
                        })
                    })
                    .await
                })
                .await??;
            session.native_publication = Some(publication.clone());
            self.rebind(publication);
            Ok(())
        }
        .await;
        cleanup.settle().await;
        result
    }

    /// Drop the native chain after an idle host change (compaction, a model
    /// switch, bounds or a recoverable refusal) and durably republish the
    /// transcript as a new epoch. The old value is denied on disk first, then
    /// the healthy payload replaces it, as for an effort change; an interrupted
    /// restart never leaves a healthy chain behind.
    pub(crate) async fn restart_chain(&mut self, session: &mut Session) -> anyhow::Result<()> {
        let publication = checked_publication_owner(session)?;
        self.verify(publication)?;
        session.restart_native_chain()?;
        let scope = crate::agent::runner::AgentWorkScope::new();
        let mut cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
        cleanup.retain_until_settled(self.publication.owner.lease.begin_write()?);
        let result = async {
            let ticket = begin_owned_with(session, &scope, true).await?;
            retire_owned(session, ticket, RetiredNativeOutcome::Completed, &scope).await
        }
        .await;
        cleanup.settle().await;
        if let Err(error) = result {
            // The in-memory chain is already gone but its replacement was not
            // acknowledged: stay conservative instead of replaying a stale chain.
            let _ = session.native_frontier.refuse_expected(
                session.native_frontier.current(),
                RestartReason::IncompleteTurn,
            );
            return Err(error);
        }
        let republished = session
            .native_publication
            .clone()
            .ok_or_else(|| anyhow::anyhow!("native restart was not published"))?;
        self.rebind(republished);
        self.outcome = RetiredNativeOutcome::Completed;
        Ok(())
    }

    #[cfg(any(feature = "hooks", feature = "subagents"))]
    pub(crate) async fn run(
        self,
        session: &mut Session,
        host: Option<&crate::provider::NativeHostGuard>,
    ) -> anyhow::Result<()> {
        let publication = checked_retired_publication(session)?;
        self.verify(publication).map_err(|_| {
            anyhow::anyhow!("session cleanup context changed after its model retired")
        })?;
        let baseline = publication.clone();
        {
            let state = publication
                .owner
                .cell
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            validate_revision(publication, &state)?;
            anyhow::ensure!(
                state.phase == Phase::Retired,
                "session cleanup context is no longer retired"
            );
        }
        if let Some(host) = host {
            host.validate()?;
        }
        let scope = crate::agent::runner::AgentWorkScope::new();
        let mut cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
        // Even a denied final checkpoint retains exact root/lock authority
        // through hook cleanup. It never receives a healthy replacement.
        let writer = self.publication.owner.lease.begin_write()?;
        let ticket = Arc::new(Mutex::new(None));
        cleanup.retain_until_settled((writer, ticket.clone()));
        if session.native_frontier.ready() {
            let active = begin_owned(session, &scope).await?;
            *ticket.lock().unwrap_or_else(|e| e.into_inner()) = Some(active);
        }
        #[cfg(feature = "hooks")]
        scope
            .run(crate::extras::hooks::dispatch_session_end("exit"))
            .await;
        cleanup.settle().await;
        // The hook scope is closed. Its exact model boundary is unchanged;
        // all final IO is owned separately, just as in model publication.
        let mut ticket = ticket.lock().unwrap_or_else(|e| e.into_inner()).take();
        let unchanged = unchanged_retired_body(session, &baseline)?;
        let valid_host = host.map_or(Ok(()), |host| host.validate());
        if !unchanged || valid_host.is_err() {
            if session.native_frontier.ready() {
                let _ = session.native_frontier.refuse_expected(
                    session.native_frontier.current(),
                    RestartReason::ChangedPrefix,
                );
            }
            anyhow::bail!(
                "session context changed during cleanup; run `/fork` (or restart with `--continue --fork-session`) to continue from the visible transcript"
            );
        }
        if let Some(ticket) = ticket.take() {
            let io = crate::agent::runner::AgentWorkScope::new();
            let mut cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(io.clone());
            let retained = Arc::new(Mutex::new(Some(ticket)));
            cleanup.retain_until_settled(retained.clone());
            let ticket = retained
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .unwrap();
            let result = retire_owned(session, ticket, self.outcome, &io).await;
            cleanup.settle().await;
            result?;
        }
        Ok(())
    }
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
                "MINI_AGENT_CONFIG_DIR",
                Some(roots.path().join("config").into_os_string()),
            ),
            (
                "MINI_AGENT_DATA_DIR",
                Some(roots.path().join("data").into_os_string()),
            ),
            (
                "MINI_AGENT_STATE_DIR",
                Some(roots.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
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

    // Controlled lower publisher regression. The later real-wire task fixture
    // must additionally establish signed-prefix compatibility; this test proves
    // the exact current rejection caused by a genuine shared paid receipt.
    #[cfg(feature = "subagents")]
    #[test]
    fn background56_held_shared_receipt_allows_next_native_prompt() {
        let roots = crate::tests::OwnedTestDirectory::new("background56-observed-native");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        session.add_message(super::super::MessageRole::User, "launch background child");
        session.initialize_spend_account(None).unwrap();
        let observations =
            super::super::background_observation::ObservationOwner::bind(&mut session).unwrap();
        let (observation, observation_lifetime) = observations.admit().unwrap();
        let ledger = Arc::new(crate::spend::Ledger::new(&roots.path().join("state")));
        let run = crate::spend::run::Run::fixture(
            session.spend_account.clone(),
            ledger,
            &session.id,
            crate::spend::PriceSnapshot::capture(Some((2.0, 4.0)), Default::default(), false),
        )
        .with_background_observation(observation.clone());
        let recorder = session
            .file_checkpoints
            .recorder(&session.id, 0, true)
            .for_background(observation);
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(roots.path()).unwrap());
        runtime().block_on(async {
            let foreground = crate::agent::runner::AgentWorkScope::new();
            let background = crate::agent::runner::AgentWorkScope::new();
            let mut cleanup =
                crate::agent::runner::AgentRunCleanupGuard::auxiliary(background.clone());
            cleanup.retain_until_settled(observation_lifetime);
            let (observe_tx, observe_rx) = tokio::sync::oneshot::channel();
            let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
            let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
            let held = std::sync::atomic::AtomicBool::new(true);
            let child = background.run(async {
                observe_rx.await.unwrap();
                run.observe(crate::event::UsageDelta {
                    input_tokens: 10,
                    output_tokens: 2,
                    total_tokens: 12,
                    ..Default::default()
                });
                run.flush().await;
                assert!(!run.account.has_pending());
                use rig::tool::Tool;
                super::super::file_checkpoints::scope(Some(recorder), async {
                    crate::agent::tools::write::WriteTool::new(None, None, None)
                        .with_workspace_binding(workspace)
                        .call(crate::agent::tools::WriteArgs {
                            path: "owned-child.txt".into(),
                            content: "completed before child returns".into(),
                            overwrite: false,
                        })
                        .await
                        .unwrap();
                })
                .await;
                observed_tx.send(()).unwrap();
                finish_rx.await.unwrap();
                held.store(false, std::sync::atomic::Ordering::Release);
            });
            let parent = async {
                let mut ticket = begin_owned(&mut session, &foreground).await.unwrap();
                let pair = NativeActorPair::pair(&session, &mut ticket).unwrap();
                let actor_scope = crate::agent::runner::AgentWorkScope::new();
                NativeModelOwner::new(
                    async {},
                    pair.clone(),
                    crate::agent::runner::AgentRunCleanupGuard::auxiliary(actor_scope),
                )
                .unwrap()
                .settle()
                .await;
                pair.claim_retired(&session, &ticket).unwrap();
                retire_owned(
                    &mut session,
                    ticket,
                    RetiredNativeOutcome::Completed,
                    &foreground,
                )
                .await
                .unwrap();
                assert_eq!(foreground.active_children(), 0);
                observe_tx.send(()).unwrap();
                observed_rx.await.unwrap();
                assert!(held.load(std::sync::atomic::Ordering::Acquire));
                assert!((session.recorded_total_cost() - 0.000028).abs() < 1e-12);
                // Every negative preserves the same valid background receipt
                // and checkpoint; those cannot authorize unrelated changes.
                for change in 0..8 {
                    let mut tampered = session.clone();
                    match change {
                        0 => tampered.messages[0].content = "forged transcript".into(),
                        1 => tampered.name = "forged metadata".into(),
                        2 => tampered.input_token_cost = 999.0,
                        3 => {
                            tampered.spend_account = serde_json::from_value(
                                serde_json::to_value(&session.spend_account).unwrap(),
                            )
                            .unwrap();
                            tampered
                                .spend_account
                                .bind_observation_owner(&observations)
                                .unwrap();
                        }
                        4 => {
                            tampered.file_checkpoints = serde_json::from_value(
                                serde_json::to_value(&session.file_checkpoints).unwrap(),
                            )
                            .unwrap();
                            tampered
                                .file_checkpoints
                                .bind_observation_owner(&observations)
                                .unwrap();
                        }
                        5 => {
                            tampered.background_observations =
                                Some(super::super::background_observation::ObservationOwner::new())
                        }
                        6 => {
                            tampered.background_history = serde_json::from_value(
                                serde_json::to_value(&session.background_history).unwrap(),
                            )
                            .unwrap();
                            tampered
                                .background_history
                                .bind_observation_owner(&observations)
                                .unwrap();
                        }
                        7 => {
                            let mut foreign = Session::new("anthropic", "fixture", 32000, "");
                            tampered.background_pool = Some(
                                crate::extras::subagents::background::Pool::for_session(
                                    &mut foreign,
                                    1,
                                )
                                .unwrap(),
                            );
                        }
                        _ => unreachable!(),
                    }
                    assert!(
                        begin_owned(&mut tampered, &foreground).await.is_err(),
                        "substitution case {change}"
                    );
                }
                // The prior whole-digest implementation rejected here. The
                // typed observation proof permits the next prompt while the
                // exact paid/writing child is still held, without new grants.
                let next = begin_owned(&mut session, &foreground).await.unwrap();
                assert!(held.load(std::sync::atomic::Ordering::Acquire));
                finish_tx.send(()).unwrap();
                drop(next);
            };
            tokio::join!(child, parent);
            cleanup.settle().await;
        });
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
    fn session_save_recovery_native_owner_seals_after_dropping_undo_and_raw_snapshots_stay_strict()
    {
        let roots = crate::tests::OwnedTestDirectory::new("native-save-recovery-undo");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        session.add_message(super::super::MessageRole::User, "retained history");
        let oversized_undo = || {
            let mut seed = Session::new("anthropic", "fixture", 32_000, "");
            seed.add_message(super::super::MessageRole::User, "kept");
            seed.add_message(
                super::super::MessageRole::Assistant,
                &"x".repeat(super::super::storage::MAX_SESSION_BYTES),
            );
            seed.rewind_to(1);
            seed.rewind_undo.take().unwrap()
        };
        runtime().block_on(async {
            let scope = crate::agent::runner::AgentWorkScope::new();
            session.rewind_undo = Some(oversized_undo());
            let mut ticket = begin_owned(&mut session, &scope).await.unwrap();
            assert!(
                session.rewind_undo.is_none(),
                "startup owner may shrink before its first digest"
            );
            let mut notices = Vec::new();
            super::super::storage::report_pending_save_notices(&mut session, |notice| {
                notices.push(notice.to_owned())
            });
            assert_eq!(
                notices,
                ["undo snapshot dropped to stay within the session size limit"]
            );
            let stale = session.clone();
            let path = super::super::storage::session_path(&session.id).unwrap();
            let before = std::fs::read(&path).unwrap();
            session.rewind_undo = Some(oversized_undo());
            assert!(super::super::storage::serialize_bounded_session(&session).is_err());
            assert!(super::super::storage::save_session_recovering(&mut session, |_| {}).is_err());
            assert!(
                session.rewind_undo.is_some(),
                "raw storage cannot reseal a native digest"
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
            let history = serde_json::to_value(&session.messages).unwrap();
            progress_owned(&mut session, &mut ticket, &scope)
                .await
                .unwrap();
            assert!(session.rewind_undo.is_none());
            assert_eq!(serde_json::to_value(&session.messages).unwrap(), history);
            checked_publication(&session).expect("the authorized digest binds the shrunk snapshot");
            notices.clear();
            super::super::storage::report_pending_save_notices(&mut session, |notice| {
                notices.push(notice.to_owned())
            });
            assert_eq!(
                notices,
                ["undo snapshot dropped to stay within the session size limit"]
            );
            let current = std::fs::read(&path).unwrap();
            assert!(super::super::storage::save_session(&stale).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), current);
            let mut unowned = imported(&session);
            unowned.rewind_undo = Some(oversized_undo());
            assert!(super::super::storage::save_session_recovering(&mut unowned, |_| {}).is_err());
            assert!(unowned.rewind_undo.is_some());
            assert_eq!(std::fs::read(&path).unwrap(), current);
            session.rewind_undo = Some(oversized_undo());
            retire_owned(
                &mut session,
                ticket,
                RetiredNativeOutcome::Completed,
                &scope,
            )
            .await
            .unwrap();
            assert!(
                session.rewind_undo.is_none(),
                "retirement seals its own shrunk digest"
            );
            checked_publication(&session).unwrap();
            super::super::storage::save_session(&session).unwrap();
            notices.clear();
            super::super::storage::report_pending_save_notices(&mut session, |notice| {
                notices.push(notice.to_owned())
            });
            assert_eq!(
                notices,
                ["undo snapshot dropped to stay within the session size limit"]
            );
            super::super::storage::report_pending_save_notices(&mut session, |notice| {
                notices.push(notice.to_owned())
            });
            assert_eq!(notices.len(), 1);
            scope.wait_idle().await;
        });
    }

    #[test]
    fn session_save_recovery_native_failed_ack_still_reports_the_live_undo_drop_once() {
        let roots = crate::tests::OwnedTestDirectory::new("native-save-recovery-failed-ack");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        session.add_message(super::super::MessageRole::User, "kept");
        runtime().block_on(async {
            let scope = crate::agent::runner::AgentWorkScope::new();
            let mut ticket = begin_owned(&mut session, &scope).await.unwrap();
            let path = super::super::storage::session_path(&session.id).unwrap();
            let previous = std::fs::read(&path).unwrap();
            let mut seed = Session::new("anthropic", "fixture", 32_000, "");
            seed.add_message(
                super::super::MessageRole::Assistant,
                &"x".repeat(super::super::storage::MAX_SESSION_BYTES),
            );
            seed.rewind_to(0);
            session.rewind_undo = seed.rewind_undo.take();
            scope.cancellation_handle().cancel();
            assert!(
                progress_owned(&mut session, &mut ticket, &scope)
                    .await
                    .is_err()
            );
            assert!(session.rewind_undo.is_none());
            assert_eq!(std::fs::read(&path).unwrap(), previous);
            let mut notices = Vec::new();
            super::super::storage::report_pending_save_notices(&mut session, |notice| {
                notices.push(notice.to_owned())
            });
            super::super::storage::report_pending_save_notices(&mut session, |notice| {
                notices.push(notice.to_owned())
            });
            assert_eq!(
                notices,
                ["undo snapshot dropped to stay within the session size limit"]
            );
            assert!(super::super::storage::save_session(&session).is_err());
            scope.wait_idle().await;
        });
    }

    #[test]
    fn session_save_recovery_native_checks_larger_denied_projection_before_sealing() {
        let roots =
            crate::tests::OwnedTestDirectory::new("native-save-recovery-projected-boundary");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        session.add_message(super::super::MessageRole::User, "x");
        let mut seed = Session::new("anthropic", "fixture", 32_000, "");
        seed.add_message(super::super::MessageRole::Assistant, "optional undo");
        seed.rewind_to(0);
        session.rewind_undo = seed.rewind_undo.take();
        session.native_frontier.activate_publication();
        let overhead = super::super::storage::serialize_bounded_session(&session)
            .unwrap()
            .len()
            - 1;
        session.messages[0].content = "x"
            .repeat(super::super::storage::MAX_SESSION_BYTES - overhead)
            .into();
        assert_eq!(
            super::super::storage::serialize_bounded_session(&session)
                .unwrap()
                .len(),
            super::super::storage::MAX_SESSION_BYTES
        );
        {
            let _projection = super::super::native_replay::InFlightEncoding::install();
            assert!(
                super::super::storage::serialize_bounded_session(&session).is_err(),
                "the denied projection is larger than the valid canonical encoding"
            );
        }
        runtime().block_on(async {
            let scope = crate::agent::runner::AgentWorkScope::new();
            let ticket = begin_owned(&mut session, &scope).await.unwrap();
            assert!(session.rewind_undo.is_none());
            let saved =
                std::fs::read(super::super::storage::session_path(&session.id).unwrap()).unwrap();
            assert!(saved.len() <= super::super::storage::MAX_SESSION_BYTES);
            checked_publication(&session).unwrap();
            let mut notices = Vec::new();
            super::super::storage::report_pending_save_notices(&mut session, |notice| {
                notices.push(notice.to_owned())
            });
            assert_eq!(notices.len(), 2);
            assert_eq!(
                notices[0],
                "undo snapshot dropped to stay within the session size limit"
            );
            assert!(notices[1].contains("75%"));
            retire_owned(
                &mut session,
                ticket,
                RetiredNativeOutcome::Completed,
                &scope,
            )
            .await
            .unwrap();
            super::super::storage::report_pending_save_notices(&mut session, |notice| {
                notices.push(notice.to_owned())
            });
            assert_eq!(
                notices.len(),
                2,
                "retirement above the threshold must not warn again"
            );
            scope.wait_idle().await;
        });
    }

    // mini-agent-vb5vw.57: the in-flight projection is encoded from the borrowed
    // Session by a thread-local mode instead of a deep clone, and the bytes are
    // exactly those of a clone carrying `in_flight_projection()`.
    #[test]
    fn in_flight_projection_encoding_equals_the_cloned_projection_without_a_clone() {
        for activated in [false, true] {
            let mut session = Session::new("anthropic", "fixture", 32_000, "");
            session.add_message(super::super::MessageRole::User, "literal prompt");
            if activated {
                session.native_frontier.activate_publication();
            }
            let mut clone = session.clone();
            clone.native_frontier = session.native_frontier.in_flight_projection();
            let expected = super::super::storage::serialize_bounded_session(&clone).unwrap();
            let live = super::super::storage::serialize_bounded_session(&session).unwrap();
            let projected = {
                let _mode = super::super::native_replay::InFlightEncoding::install();
                super::super::storage::serialize_bounded_session(&session).unwrap()
            };
            assert_eq!(projected, expected, "activated={activated}");
            assert_ne!(projected, live);
            assert_eq!(
                super::super::storage::serialize_bounded_session(&session).unwrap(),
                live,
                "the mode ends with its guard"
            );
        }
    }

    #[test]
    fn native_publication_turn_end_save_is_deferred_off_the_calling_thread() {
        let roots = crate::tests::OwnedTestDirectory::new("native-publication-deferred");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        session.add_message(super::super::MessageRole::User, "literal prompt");
        runtime().block_on(async {
            let scope = crate::agent::runner::AgentWorkScope::new();
            let ticket = begin_owned(&mut session, &scope).await.unwrap();
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
            assert!(is_native_bearing(&session));

            let path = super::super::storage::session_path(&session.id).unwrap();
            super::super::storage::save_session(&session).unwrap();
            let expected = std::fs::read(&path).unwrap();
            std::fs::remove_file(&path).unwrap();

            let _defer = super::super::storage::defer_saves_for_test();
            let before = crate::fs::DURABLE_SYNCS.with(std::cell::Cell::get);
            let encodes = super::super::storage::SERIALIZE_BOUNDED_CALLS.with(std::cell::Cell::get);
            super::super::storage::save_session_deferred_recovering(&mut session, |_| {}).unwrap();
            assert_eq!(
                super::super::storage::SERIALIZE_BOUNDED_CALLS.with(std::cell::Cell::get),
                encodes,
                "the calling thread performs no whole-session encode"
            );
            assert_eq!(
                crate::fs::DURABLE_SYNCS.with(std::cell::Cell::get),
                before,
                "the calling thread performs no durable write"
            );
            super::super::storage::flush_deferred_sessions().unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), expected);
            assert_eq!(
                crate::fs::DURABLE_SYNCS.with(std::cell::Cell::get),
                before,
                "the writer thread, not the caller, synced the snapshot"
            );
        });
    }

    #[test]
    fn native_publication_deferred_snapshot_never_overwrites_a_newer_checkpoint() {
        let roots = crate::tests::OwnedTestDirectory::new("native-publication-deferred-stale");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        session.add_message(super::super::MessageRole::User, "literal prompt");
        runtime().block_on(async {
            let scope = crate::agent::runner::AgentWorkScope::new();
            let ticket = begin_owned(&mut session, &scope).await.unwrap();
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
            let queued = session.clone();
            // The next turn publishes a newer checkpoint before the writer runs.
            let next = begin_owned(&mut session, &scope).await.unwrap();
            scope.wait_idle().await;
            let path = super::super::storage::session_path(&session.id).unwrap();
            let newer = std::fs::read(&path).unwrap();
            assert!(save_snapshot(&queued).is_err(), "a sync stale save refuses");
            save_snapshot_deferred(&queued).expect("the deferred writer drops it");
            assert_eq!(std::fs::read(&path).unwrap(), newer);
            drop(next);
            scope.wait_idle().await;
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

    #[cfg(feature = "hooks")]
    #[test]
    fn native_session_end_permit_is_bound_to_its_acknowledged_retired_value() {
        let roots = crate::tests::OwnedTestDirectory::new("native-retired-cleanup-value");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        runtime().block_on(async {
            let mut actors = Vec::new();
            for _ in 0..2 {
                let io = crate::agent::runner::AgentWorkScope::new();
                let mut ticket = begin_owned(&mut session, &io).await.unwrap();
                let pair = NativeActorPair::pair(&session, &mut ticket).unwrap();
                let scope = crate::agent::runner::AgentWorkScope::new();
                let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope);
                // Lower ownership fixture only: real scope/model retirement,
                // without claiming a provider protocol completion witness.
                let mut model =
                    NativeModelOwner::new(std::future::ready(()), pair.clone(), cleanup).unwrap();
                model.model().await;
                model.settle().await;
                pair.claim_retired(&session, &ticket).unwrap();
                retire_owned(&mut session, ticket, RetiredNativeOutcome::Completed, &io)
                    .await
                    .unwrap();
                actors.push(pair);
            }
            let before = super::super::storage::serialize_bounded_session(&session).unwrap();
            assert!(
                RetiredSessionEndPermit::from_retired_actor(&session, &actors[0]).is_err(),
                "an older retired actor cannot borrow the latest value from the same publisher"
            );
            let deserialized: Session = serde_json::from_str(&before).unwrap();
            assert!(
                RetiredSessionEndPermit::from_retired_actor(&deserialized, &actors[1]).is_err()
            );
            let _permit =
                RetiredSessionEndPermit::from_retired_actor(&session, &actors[1]).unwrap();
            assert!(
                RetiredSessionEndPermit::from_retired_actor(&session, &actors[1]).is_err(),
                "cleanup role is issued once"
            );
            assert_eq!(
                super::super::storage::serialize_bounded_session(&session).unwrap(),
                before
            );
            assert_eq!(
                serde_json::to_value(imported(&session)).unwrap(),
                serde_json::to_value(&session).unwrap()
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
                crate::spend::Ledger::new(&crate::paths::process_paths_ref().unwrap().state_dir);
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

    /// Opens startup ownership and retires the idle ticket, as the TUI does.
    async fn idle_owned(session: &mut Session) -> RetiredSessionEndPermit {
        let owner = NativeStartupPublication::begin(session).await.unwrap();
        let ticket = owner.into_model_ticket(session).await.unwrap();
        retire_idle_startup(session, ticket).await.unwrap()
    }

    #[test]
    fn idle_startup_retires_to_a_healthy_checkpoint() {
        let roots = crate::tests::OwnedTestDirectory::new("native-idle-retire");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        runtime().block_on(async {
            let _permit = idle_owned(&mut session).await;
            // Quitting here leaves a healthy file, not an in-flight one.
            assert_eq!(imported(&session).native_frontier.refusal(), None);
        });
    }

    #[test]
    fn idle_metadata_change_republishes_and_the_next_turn_begins() {
        // mini-agent-nvnkx.262: an idle title or /rename change must not break
        // the next save or prompt.
        let roots = crate::tests::OwnedTestDirectory::new("native-idle-metadata");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        runtime().block_on(async {
            let mut permit = idle_owned(&mut session).await;
            session.generated_title = Some("An idle title".into());
            session.total_input_tokens += 7;
            super::super::storage::save_session_recovering(&mut session, |_| {}).unwrap();
            let saved = imported(&session);
            assert_eq!(saved.generated_title.as_deref(), Some("An idle title"));
            assert_eq!(saved.native_frontier.refusal(), None);
            session.name = "renamed".into();
            super::super::storage::save_session_recovering(&mut session, |_| {}).unwrap();
            assert_eq!(imported(&session).name, "renamed");
            // The retired owner survives idle republication.
            permit.restart_chain(&mut session).await.unwrap();
            assert_eq!(imported(&session).native_frontier.refusal(), None);
            // A change that was never saved is refused by the strict turn
            // begin; the TUI publishes it first (`rebind_native_session`).
            session.name = "renamed again".into();
            let scope = crate::agent::runner::AgentWorkScope::new();
            assert!(begin_owned(&mut session, &scope).await.is_err());
            assert!(recover_idle(&mut session).unwrap().0);
            let ticket = begin_owned(&mut session, &scope).await.unwrap();
            assert_eq!(
                imported(&session).native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );
            assert_eq!(imported(&session).name, "renamed again");
            // The turn's actor supersedes the old owner.
            assert!(permit.restart_chain(&mut session).await.is_err());
            drop(ticket);
        });
    }

    #[test]
    fn stale_clone_cannot_use_idle_republication() {
        let roots = crate::tests::OwnedTestDirectory::new("native-idle-stale");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        runtime().block_on(async {
            let _permit = idle_owned(&mut session).await;
            let mut stale = session.clone();
            session.name = "newer".into();
            super::super::storage::save_session_recovering(&mut session, |_| {}).unwrap();
            stale.name = "stale".into();
            assert!(super::super::storage::save_session_recovering(&mut stale, |_| {}).is_err());
            assert_eq!(imported(&session).name, "newer");
        });
    }

    #[test]
    fn in_flight_file_of_a_dead_writer_restarts_on_resume() {
        // mini-agent-nvnkx.209: a file left in flight by a process that was
        // killed (or quit before its first prompt) must not strand the session.
        let roots = crate::tests::OwnedTestDirectory::new("native-in-flight-resume");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let id = session.id.to_string();
        let _release = Release(id.clone());
        session.add_message(super::super::MessageRole::User, "kept transcript row");
        runtime().block_on(async {
            let owner = NativeStartupPublication::begin(&mut session).await.unwrap();
            drop(owner);
        });
        assert_eq!(
            imported(&session).native_frontier.refusal(),
            Some(RestartReason::InFlight)
        );
        // The first process is gone: its lock is released.
        super::super::lock::release_session(&id);
        let mut resumed = imported(&session);
        assert_eq!(
            resumed.native_frontier.refusal(),
            Some(RestartReason::InFlight)
        );
        let before_epoch = resumed.native_frontier.current();
        runtime().block_on(async {
            let (owner, cause) = NativeStartupPublication::begin_with_cause(&mut resumed)
                .await
                .unwrap();
            assert_eq!(
                cause,
                Some(super::super::native_replay::NativeRestartCause::Unfinished)
            );
            let ticket = owner.into_model_ticket(&resumed).await.unwrap();
            retire_idle_startup(&mut resumed, ticket).await.unwrap();
        });
        assert!(resumed.native_frontier.current() > before_epoch);
        let healthy = imported(&resumed);
        assert_eq!(healthy.native_frontier.refusal(), None);
        assert_eq!(healthy.messages.len(), 1);
        assert_eq!(healthy.messages[0].content, "kept transcript row");
    }

    #[test]
    fn in_flight_file_owned_by_another_process_still_refuses() {
        let roots = crate::tests::OwnedTestDirectory::new("native-in-flight-held");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let id = session.id.to_string();
        let _release = Release(id.clone());
        runtime().block_on(async {
            drop(NativeStartupPublication::begin(&mut session).await.unwrap());
        });
        // This process still holds the lock (the "live writer"): a loaded copy
        // is not a fresh claim, so the live cell keeps refusing replacement.
        let mut other = imported(&session);
        runtime().block_on(async {
            let result = NativeStartupPublication::begin(&mut other).await;
            // The same process owning the lock cannot be a second owner.
            assert!(result.is_err());
        });
    }

    #[test]
    fn failed_final_publication_recovers_on_the_next_save() {
        // mini-agent-nvnkx.266
        let roots = crate::tests::OwnedTestDirectory::new("native-failed-recover");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        session.add_message(super::super::MessageRole::User, "hello");
        runtime().block_on(async {
            let mut owner = NativeStartupPublication::begin(&mut session).await.unwrap();
            owner.fail_final_before_replace();
            let ticket = owner.into_model_ticket(&session).await.unwrap();
            assert!(retire_idle_startup(&mut session, ticket).await.is_err());
            assert_eq!(
                imported(&session).native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );
            // Nothing can save while the cell is failed, until recovery.
            assert!(idle_republish_needed(&session));
            super::super::storage::save_session_recovering(&mut session, |_| {}).unwrap();
            let saved = imported(&session);
            assert_eq!(saved.native_frontier.refusal(), None);
            assert_eq!(saved.messages.len(), 1);
            // The recovered owner can start a turn.
            let scope = crate::agent::runner::AgentWorkScope::new();
            drop(begin_owned(&mut session, &scope).await.unwrap());
        });
    }

    #[test]
    fn fork_of_a_native_session_is_published_through_its_own_owner() {
        // mini-agent-nvnkx.267
        let roots = crate::tests::OwnedTestDirectory::new("native-fork-publish");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        session.add_message(super::super::MessageRole::User, "first");
        session.add_message(super::super::MessageRole::Assistant, "answer");
        runtime().block_on(async {
            let _permit = idle_owned(&mut session).await;
            let mut fork = super::super::lock::fork_at_turn(&session, Some(1)).unwrap();
            let _fork_release = Release(fork.id.to_string());
            assert!(is_native_bearing(&fork));
            // The ordinary save has no owner for the copy.
            assert!(super::super::storage::save_session_recovering(&mut fork, |_| {}).is_err());
            publish_new_session(&mut fork).await.unwrap();
            let saved = imported(&fork);
            assert_eq!(saved.messages.len(), 1);
            assert_eq!(saved.native_frontier.refusal(), None);
            // And the fork then behaves like any idle owned session.
            fork.name = "fork renamed".into();
            super::super::storage::save_session_recovering(&mut fork, |_| {}).unwrap();
            assert_eq!(imported(&fork).name, "fork renamed");
        });
    }

    #[cfg(feature = "acp")]
    #[test]
    fn cancelled_turn_restarts_for_the_next_prompt_in_every_frontend() {
        // mini-agent-nvnkx.265: ACP calls begin_owned directly; a cancelled or
        // failed turn leaves an IncompleteTurn refusal that only a restart
        // clears.
        let roots = crate::tests::OwnedTestDirectory::new("native-cancelled-restart");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        session.add_message(super::super::MessageRole::User, "cancelled prompt");
        runtime().block_on(async {
            let scope = crate::agent::runner::AgentWorkScope::new();
            let ticket = begin_owned(&mut session, &scope).await.unwrap();
            retire_owned(
                &mut session,
                ticket,
                RetiredNativeOutcome::Incomplete,
                &scope,
            )
            .await
            .unwrap();
            assert_eq!(
                session.native_frontier.refusal(),
                Some(RestartReason::IncompleteTurn)
            );
            let scope = crate::agent::runner::AgentWorkScope::new();
            // The plain begin refuses every later prompt.
            assert!(begin_owned(&mut session, &scope).await.is_err());
            let (ticket, cause) = begin_owned_reconciled(&mut session, &scope).await.unwrap();
            assert_eq!(
                cause,
                Some(super::super::native_replay::NativeRestartCause::Refused(
                    RestartReason::IncompleteTurn
                ))
            );
            assert!(session.native_frontier.ready());
            retire_owned(
                &mut session,
                ticket,
                RetiredNativeOutcome::Completed,
                &scope,
            )
            .await
            .unwrap();
            assert_eq!(imported(&session).native_frontier.refusal(), None);
        });
    }

    #[test]
    fn non_native_turn_on_a_native_bearing_session_saves_under_its_owner() {
        // mini-agent-nvnkx.264: after a provider switch the turn runs without a
        // ticket, but its save must still land.
        let roots = crate::tests::OwnedTestDirectory::new("native-legacy-turn");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        runtime().block_on(async {
            let _permit = idle_owned(&mut session).await;
            session.provider = "openai".into();
            session.add_message(super::super::MessageRole::User, "turn after the switch");
            session.add_message(super::super::MessageRole::Assistant, "answer");
            super::super::storage::save_session_recovering(&mut session, |_| {}).unwrap();
            let saved = imported(&session);
            assert_eq!(saved.messages.len(), 2);
            assert_eq!(saved.provider, "openai");
            session.add_message(super::super::MessageRole::User, "another turn");
            super::super::storage::save_session_recovering(&mut session, |_| {}).unwrap();
            assert_eq!(imported(&session).messages.len(), 3);
        });
    }

    #[test]
    fn every_refusal_text_names_a_way_out_and_a_fork_continues_from_the_transcript() {
        // mini-agent-nvnkx.271
        for reason in [
            RestartReason::ChangedDomain,
            RestartReason::ChangedPrefix,
            RestartReason::IncompleteTurn,
            RestartReason::UnavailableSource,
            RestartReason::Limit,
            RestartReason::InFlight,
        ] {
            let roots = crate::tests::OwnedTestDirectory::new("native-refusal-fork");
            let _environment = environment(&roots);
            let mut session = Session::new("anthropic", "fixture", 32_000, "");
            session.add_message(super::super::MessageRole::User, "visible row");
            session.add_message(super::super::MessageRole::Assistant, "visible answer");
            session.native_frontier.refuse_expected(0, reason).unwrap();
            if reason == RestartReason::InFlight {
                assert!(reason.to_string().contains("/fork"), "{reason}");
                assert!(reason.to_string().contains("--fork-session"), "{reason}");
            }
            let mut fork = super::super::lock::fork_at_turn(&session, None).unwrap();
            let _release = Release(fork.id.to_string());
            runtime().block_on(async {
                publish_new_session(&mut fork)
                    .await
                    .unwrap_or_else(|error| panic!("{reason:?}: {error:#}"));
            });
            let saved = imported(&fork);
            assert_eq!(saved.native_frontier.refusal(), None, "{reason:?}");
            assert_eq!(saved.messages.len(), 2, "{reason:?}: rows intact");
        }
        // Raw overrides refuse a fresh chain too: the text says to remove them.
        let text = RestartReason::UnsupportedOverride.to_string();
        assert!(text.contains("remove the raw request overrides"), "{text}");
    }

    #[test]
    fn legacy_file_of_an_unpublished_interactive_session_takes_its_first_native_turn() {
        // mini-agent-nvnkx.210: a fresh interactive session publishes nothing
        // before its first prompt; a /rename before it writes an ordinary file
        // that the first native turn must be able to take over.
        let roots = crate::tests::OwnedTestDirectory::new("native-legacy-first-turn");
        let _environment = environment(&roots);
        let mut session = Session::new("anthropic", "fixture", 32_000, "");
        let _release = Release(session.id.to_string());
        assert!(!is_native_bearing(&session));
        session.name = "named before the first prompt".into();
        super::super::storage::save_session_recovering(&mut session, |_| {}).unwrap();
        assert_eq!(imported(&session).name, "named before the first prompt");
        runtime().block_on(async {
            let scope = crate::agent::runner::AgentWorkScope::new();
            let ticket = begin_owned(&mut session, &scope).await.unwrap();
            assert_eq!(
                imported(&session).native_frontier.refusal(),
                Some(RestartReason::InFlight)
            );
            retire_owned(
                &mut session,
                ticket,
                RetiredNativeOutcome::Completed,
                &scope,
            )
            .await
            .unwrap();
            let saved = imported(&session);
            assert_eq!(saved.native_frontier.refusal(), None);
            assert_eq!(saved.name, "named before the first prompt");
        });
    }
}
