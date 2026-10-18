//! `/cron` for ACP sessions: interval schedules that start a turn in their
//! session whenever that session is idle.
//!
//! The schedule store, interval grammar and fire accounting are the TUI's
//! [`CronStore`]; this module adds the ACP owner around it. Entries are per
//! ACP session and never persisted: closing (or replacing) the session,
//! connection teardown and `/cron stop all` drop them. While any entry is
//! armed the session holds one idle [`Activity`] lease, so `--idle-exit`
//! cannot stop the server between fires; the lease is released when the last
//! entry goes (stop, `cron_max_fires` retirement or session teardown).
//!
//! v1 is deliberately narrower than the TUI:
//! - only `/cron <interval> <prompt>`, `/cron` / `/cron list` and
//!   `/cron stop <id>|all`;
//! - self-paced `/cron <prompt>` is refused: it relies on the turn-scoped
//!   `schedule_wakeup` tool, which ACP turns do not register;
//! - `/cron run` is refused: ACP has no host idle-point queue a parked manual
//!   fire could wait in (fires happen only between client prompts);
//! - `/cron resume` is refused: ACP schedules are never saved;
//! - slash and `!` payloads are refused: ACP has no host slash dispatcher or
//!   shell-line path a fire could run them through.
//!
//! A fire starts only when the session has no turn in flight, no unanswered
//! question and no running goal. It is admitted exactly like a client prompt
//! (so a client prompt that arrives meanwhile gets the usual "already has an
//! active prompt" refusal and two turns never interleave), but runs with
//! [`RequestKind::Scheduled`] in scope: receipts record `scheduled` and
//! `UserPromptSubmit` hooks see `"origin": "scheduled"`. The client learns of
//! it through a host-labelled `agent_message_chunk` carrying `_meta.cron`,
//! then the turn streams through the ordinary `session/update`s.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use agent_client_protocol::schema::v1::*;
use agent_client_protocol::{Client, ConnectionTo, Responder};
use tokio::sync::Notify;
use tokio::time::Instant;

use super::idle::{Activity, Idle};
use super::{AcpState, PromptReply, lock_unpoisoned};
use crate::spend::RequestKind;
use crate::ui::cron::{self as schedule, ArmError, CronFire, CronStore};

const USAGE: &str =
    "usage over ACP: /cron <interval> <prompt>  |  /cron list  |  /cron stop <id>|all";
const SELF_PACED_REFUSED: &str = "cron: self-paced /cron <prompt> is not available over ACP (ACP turns do not offer the schedule_wakeup tool); use /cron <interval> <prompt>";
const RUN_REFUSED: &str = "cron: /cron run is not available over ACP (there is no host idle point to park a manual fire); entries fire on their interval while the session is idle";
const RESUME_REFUSED: &str =
    "cron: /cron resume is not available over ACP (ACP schedules are never saved)";
const PAYLOAD_REFUSED: &str = "cron: ACP cron payloads are plain prompts; slash commands and shell (!) lines cannot be scheduled over ACP";
const BUDGET_UNBOUNDED: &str =
    "cron: no max_budget_usd is set, so scheduled turns are unbounded in spend";

/// How long a due fire waits before re-checking a busy session.
const BUSY_POLL: Duration = Duration::from_secs(1);

/// The question origin of the turn whose spend profile is in scope: a `/cron`
/// fire runs with [`RequestKind::Scheduled`].
pub(super) fn question_origin() -> crate::question::QuestionOrigin {
    match crate::spend::run::Run::current().map(|run| run.kind()) {
        Some(RequestKind::Scheduled) => crate::question::QuestionOrigin::Scheduled,
        _ => crate::question::QuestionOrigin::NewUserTurn,
    }
}

/// A parsed ACP `/cron` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Request {
    List,
    Stop(String),
    StopAll,
    Arm { interval: String, prompt: String },
}

/// Parse the arguments after `/cron`; `Err` is the one-line refusal or usage.
pub(super) fn parse(arguments: &str) -> Result<Request, String> {
    let arguments = arguments.trim();
    let (first, rest) = arguments
        .split_once(char::is_whitespace)
        .map_or((arguments, ""), |(first, rest)| (first, rest.trim()));
    match (first, rest) {
        ("", _) | ("list", "") => Ok(Request::List),
        ("stop", "all") => Ok(Request::StopAll),
        ("stop", id) if !id.is_empty() && !id.contains(char::is_whitespace) => {
            Ok(Request::Stop(id.to_owned()))
        }
        ("list" | "stop", _) => Err(USAGE.to_owned()),
        ("run", _) => Err(RUN_REFUSED.to_owned()),
        ("resume", _) => Err(RESUME_REFUSED.to_owned()),
        (interval, "") if schedule::is_interval_token(interval) => {
            Err("usage: /cron <interval> <prompt>".to_owned())
        }
        (interval, prompt) if schedule::is_interval_token(interval) => {
            if matches!(prompt.chars().next(), Some('/' | '!')) {
                return Err(PAYLOAD_REFUSED.to_owned());
            }
            Ok(Request::Arm {
                interval: interval.to_owned(),
                prompt: prompt.to_owned(),
            })
        }
        _ => Err(SELF_PACED_REFUSED.to_owned()),
    }
}

/// One session's schedules, shared by its catalogue entry and its driver.
#[derive(Default)]
pub(super) struct SessionCron {
    inner: StdMutex<Inner>,
    changed: Notify,
}

#[derive(Default)]
struct Inner {
    store: CronStore,
    /// Held exactly while `store` has an entry.
    lease: Option<Activity>,
    /// A driver task is running for this session.
    driver: bool,
    /// The session is gone; nothing may be armed again.
    closed: bool,
}

impl Inner {
    fn release_if_empty(&mut self) {
        if self.store.is_empty() {
            self.lease = None;
        }
    }
}

/// The result of a successful `/cron` edit.
pub(super) struct Edit {
    pub(super) lines: Vec<String>,
    /// The caller must start [`drive`] for this session.
    pub(super) spawn_driver: bool,
}

/// What the driver does next.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// No entry is left (or the session is gone); the driver ends.
    Exit,
    /// An entry is due; try to fire it.
    Due,
    /// Sleep until the deadline, or until an edit when there is none.
    Wait(Option<Instant>),
}

impl SessionCron {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        lock_unpoisoned(&self.inner)
    }

    /// Apply one parsed request. `Err` is a one-line refusal; the store is
    /// unchanged then.
    pub(super) fn apply(
        &self,
        request: Request,
        idle: &Arc<Idle>,
        max_fires: u32,
        budget_capped: bool,
        now: Instant,
    ) -> Result<Edit, String> {
        let mut inner = self.lock();
        let mut edit = Edit {
            lines: Vec::new(),
            spawn_driver: false,
        };
        match request {
            Request::List => edit.lines = list(&inner.store, max_fires, now),
            Request::StopAll => {
                let count = inner.store.stop_all();
                edit.lines
                    .push(format!("cron: stopped {count} schedule(s)"));
            }
            Request::Stop(id) => {
                if !inner.store.stop(&id) {
                    return Err(format!("cron: no schedule {id} (see /cron list)"));
                }
                edit.lines.push(format!("cron {id}: stopped"));
            }
            Request::Arm { interval, prompt } => {
                let interval = schedule::parse_interval(&interval)
                    .map_err(|error| format!("cron: {error}"))?;
                if inner.closed {
                    return Err("cron: this ACP session is closing".to_owned());
                }
                if inner.lease.is_none() {
                    inner.lease = Some(
                        idle.admit()
                            .map_err(|error| format!("cron: {}", error.message))?,
                    );
                }
                let id = match inner.store.arm(interval.duration, prompt, now) {
                    Ok(entry) => entry.id.clone(),
                    Err(ArmError::Full) => {
                        inner.release_if_empty();
                        return Err(format!(
                            "cron: at most {} schedules per session; /cron stop one first",
                            schedule::CRON_MAX_ENTRIES
                        ));
                    }
                };
                if interval.rounded_up {
                    edit.lines.push(format!(
                        "cron: interval raised to the {} minimum",
                        schedule::format_duration(schedule::CRON_MIN_INTERVAL)
                    ));
                }
                edit.lines.push(format!(
                    "cron {id}: every {}, first fire {}; up to {max_fires} fires, only while this session is idle",
                    schedule::format_duration(interval.duration),
                    schedule::format_until(now + interval.duration, now),
                ));
                if !budget_capped && inner.store.take_budget_warning() {
                    edit.lines.push(BUDGET_UNBOUNDED.to_owned());
                }
                edit.spawn_driver = !std::mem::replace(&mut inner.driver, true);
            }
        }
        inner.release_if_empty();
        drop(inner);
        self.changed.notify_waiters();
        Ok(edit)
    }

    fn step(&self, now: Instant) -> Step {
        let mut inner = self.lock();
        if inner.closed || inner.store.is_empty() {
            inner.driver = false;
            inner.lease = None;
            return Step::Exit;
        }
        inner.store.mark_expired_due(now);
        if inner.store.has_due() {
            Step::Due
        } else {
            Step::Wait(inner.store.next_deadline())
        }
    }

    /// Start the earliest due fire; the store releases the lease when that
    /// fire retired the last entry.
    fn begin_fire(&self, now: Instant, max_fires: u32) -> Option<CronFire> {
        let mut inner = self.lock();
        let id = inner.store.next_due_id()?.to_owned();
        let fire = inner.store.begin_fire(&id, now, max_fires);
        inner.release_if_empty();
        fire
    }

    /// Suspend firing (budget exhaustion), keeping every entry. `Some(count)`
    /// only the first time, so the client sees one notice.
    fn suspend(&self) -> Option<usize> {
        let mut inner = self.lock();
        if inner.store.is_suspended() {
            return None;
        }
        inner.store.suspend();
        Some(inner.store.len())
    }

    /// Drop every entry and the lease for good, and wake the driver so it
    /// ends.
    pub(super) fn clear(&self) {
        let mut inner = self.lock();
        inner.closed = true;
        inner.store.stop_all();
        inner.lease = None;
        drop(inner);
        self.changed.notify_waiters();
    }

    #[cfg(test)]
    pub(super) fn holds_lease(&self) -> bool {
        self.lock().lease.is_some()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.lock().store.len()
    }
}

fn list(store: &CronStore, max_fires: u32, now: Instant) -> Vec<String> {
    if store.is_empty() {
        return vec![
            "no cron schedules in this session".to_owned(),
            USAGE.to_owned(),
        ];
    }
    store
        .entries()
        .iter()
        .map(|entry| {
            let next = if entry.due {
                "due when idle".to_owned()
            } else if store.is_suspended() {
                "suspended".to_owned()
            } else {
                schedule::format_until(entry.next_at, now)
            };
            format!(
                "{}  every {}  {next}  {}/{max_fires}  {}",
                entry.id,
                schedule::format_duration(entry.interval),
                entry.fires,
                one_line(&entry.payload, 80),
            )
        })
        .collect()
}

fn one_line(payload: &str, max: usize) -> String {
    let flat = crate::ui::events::sanitize_output(payload)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The catalogue's owner of a session's schedules: dropping the session
/// (close or replacement by a load) clears them.
#[derive(Default)]
pub(super) struct CronOwner(Arc<SessionCron>);

impl CronOwner {
    pub(super) fn shared(&self) -> Arc<SessionCron> {
        self.0.clone()
    }

    pub(super) fn clear(&self) {
        self.0.clear();
    }

    fn owns(&self, cron: &Arc<SessionCron>) -> bool {
        Arc::ptr_eq(&self.0, cron)
    }
}

impl Drop for CronOwner {
    fn drop(&mut self) {
        self.0.clear();
    }
}

fn notice(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    text: String,
    meta: Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<(), agent_client_protocol::Error> {
    let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
    let mut notification =
        SessionNotification::new(session_id.clone(), SessionUpdate::AgentMessageChunk(chunk));
    notification.meta = meta;
    cx.send_notification(notification)
}

/// `_meta.cron` of a host-labelled scheduler notice.
fn meta(fields: serde_json::Value) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut cron =
        serde_json::Map::from_iter([("origin".to_owned(), serde_json::Value::from("scheduled"))]);
    if let serde_json::Value::Object(fields) = fields {
        cron.extend(fields);
    }
    Some(serde_json::Map::from_iter([(
        "cron".to_owned(),
        serde_json::Value::Object(cron),
    )]))
}

/// Answer one `/cron` prompt. It edits only the store: no turn is admitted
/// and a running turn (a fire or a client prompt) is untouched.
pub(super) async fn handle_command(
    arguments: &str,
    session_id: &SessionId,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
    state: &Arc<AcpState>,
) -> Result<(), agent_client_protocol::Error> {
    let request =
        parse(arguments).map_err(|reason| agent_client_protocol::Error::new(-32602, reason))?;
    let (cron, budget_capped) = {
        let sessions = state.sessions.lock().await;
        let sess = sessions
            .get(session_id)
            .ok_or_else(|| agent_client_protocol::Error::new(-32602, "unknown ACP session"))?;
        (
            sess.cron.shared(),
            sess.spend.account.snapshot().cap.is_some(),
        )
    };
    let edit = cron
        .apply(
            request,
            &state.idle,
            state.cfg.resolve_cron_max_fires(),
            budget_capped,
            Instant::now(),
        )
        .map_err(|reason| agent_client_protocol::Error::new(-32602, reason))?;
    if edit.spawn_driver
        && let Err(error) = cx.spawn(drive(
            state.clone(),
            cx.clone(),
            session_id.clone(),
            cron.clone(),
        ))
    {
        cron.clear();
        return Err(error);
    }
    for line in edit.lines {
        notice(&cx, session_id, line, None)?;
    }
    responder.respond(PromptResponse::new(StopReason::EndTurn))
}

/// Clears the schedules when the connection drops a running driver.
struct ClearOnDrop {
    cron: Arc<SessionCron>,
    defused: bool,
}

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        if !self.defused {
            self.cron.clear();
        }
    }
}

enum Fire {
    Started,
    Busy,
    Suspended,
    Gone,
}

/// One session's scheduler. Connection-owned (`cx.spawn`), so teardown drops
/// it; it always returns `Ok` because a spawned task's error would end the
/// whole connection.
async fn drive(
    state: Arc<AcpState>,
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    cron: Arc<SessionCron>,
) -> Result<(), agent_client_protocol::Error> {
    let mut guard = ClearOnDrop {
        cron: cron.clone(),
        defused: false,
    };
    loop {
        let changed = cron.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let wait = match cron.step(Instant::now()) {
            Step::Exit => {
                guard.defused = true;
                return Ok(());
            }
            Step::Due => match fire(&state, &cx, &session_id, &cron).await {
                Fire::Started | Fire::Suspended => continue,
                Fire::Busy => Some(Instant::now() + BUSY_POLL),
                Fire::Gone => {
                    cron.clear();
                    guard.defused = true;
                    return Ok(());
                }
            },
            Step::Wait(deadline) => deadline,
        };
        match wait {
            Some(deadline) => {
                tokio::select! {
                    () = &mut changed => {}
                    () = tokio::time::sleep_until(deadline) => {}
                }
            }
            None => changed.await,
        }
    }
}

/// Start the earliest due fire when the session is idle.
async fn fire(
    state: &Arc<AcpState>,
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    cron: &Arc<SessionCron>,
) -> Fire {
    let Ok(activity) = state.idle.admit() else {
        return Fire::Gone;
    };
    let sessions = state.sessions.lock().await;
    let Some(sess) = sessions.get(session_id).filter(|sess| sess.cron.owns(cron)) else {
        return Fire::Gone;
    };
    // The idle predicate: no turn in flight or still settling, no question
    // waiting for the client's next prompt (a fire would be taken as its
    // answer), and no running goal (a fire would be judged as its round).
    if !lock_unpoisoned(&sess.turns).queue.is_empty() {
        return Fire::Busy;
    }
    match sess.history.try_lock() {
        Ok(history) if history.pending_question.is_none() => {}
        _ => return Fire::Busy,
    }
    #[cfg(feature = "goal")]
    if sess
        .goal_store
        .snapshot()
        .is_some_and(|goal| goal.status.is_running())
    {
        return Fire::Busy;
    }
    let account = &sess.spend.account;
    if account.snapshot().cap.is_some() && account.admit(sess.spend.prices).is_err() {
        if let Some(count) = cron.suspend() {
            let _ = notice(
                cx,
                session_id,
                format!(
                    "cron: USD budget reached; {count} schedule(s) suspended and kept (/cron stop to remove)"
                ),
                meta(serde_json::json!({ "event": "suspended" })),
            );
        }
        return Fire::Suspended;
    }
    let Ok(mut snapshot) = super::admit_turn(state, sess) else {
        return Fire::Busy;
    };
    drop(sessions);
    let max_fires = state.cfg.resolve_cron_max_fires();
    // The deadline moves before the turn starts, so a long turn cannot
    // double-fire. A stop that raced this admission drops `snapshot`, which
    // settles the just-registered generation.
    let Some(fire) = cron.begin_fire(Instant::now(), max_fires) else {
        return Fire::Started;
    };
    snapshot.spend = snapshot.spend.with_kind(RequestKind::Scheduled);
    let _ = notice(
        cx,
        session_id,
        format!(
            "cron {}: scheduled turn (fire {}/{max_fires}, every {}): {}",
            fire.id,
            fire.fire,
            schedule::format_duration(fire.interval),
            one_line(&fire.payload, 120),
        ),
        meta(serde_json::json!({
            "event": "started",
            "id": fire.id,
            "fire": fire.fire,
            "max_fires": max_fires,
            "retired": fire.retired,
        })),
    );
    if fire.retired {
        let _ = notice(
            cx,
            session_id,
            format!(
                "cron {}: reached cron_max_fires ({max_fires}); schedule removed after this fire",
                fire.id
            ),
            None,
        );
    }
    let reply = PromptReply::Scheduled(ScheduledReply {
        cx: cx.clone(),
        session_id: session_id.clone(),
        id: fire.id,
    });
    match super::spawn_turn(
        state.clone(),
        cx.clone(),
        snapshot,
        crate::agent::input::PromptInput::text(fire.payload),
        None,
        session_id.clone(),
        reply,
        None,
        activity,
    ) {
        Ok(()) => Fire::Started,
        Err(_) => Fire::Gone,
    }
}

/// The terminal outcome of a fire's turn, which no client request awaits.
pub(super) struct ScheduledReply {
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    id: String,
}

impl ScheduledReply {
    /// Report anything other than a plain completed turn; a completed turn's
    /// streamed output already ended the fire.
    pub(super) fn finished(
        self,
        response: PromptResponse,
    ) -> Result<(), agent_client_protocol::Error> {
        let stop_reason = serde_json::to_value(response.stop_reason).unwrap_or_default();
        let pending_question = response
            .meta
            .as_ref()
            .and_then(|meta| meta.get("pending_question"))
            .cloned();
        let text = if pending_question.is_some() {
            format!(
                "cron {}: scheduled turn is waiting for an answer; the next prompt answers it",
                self.id
            )
        } else if response.stop_reason == StopReason::EndTurn {
            return Ok(());
        } else {
            format!(
                "cron {}: scheduled turn ended ({})",
                self.id,
                stop_reason.as_str().unwrap_or("stopped")
            )
        };
        let mut fields = serde_json::json!({
            "event": "finished",
            "id": self.id,
            "stop_reason": stop_reason,
        });
        if let Some(question) = pending_question {
            fields["pending_question"] = question;
        }
        notice(&self.cx, &self.session_id, text, meta(fields))
    }

    pub(super) fn failed(
        self,
        error: agent_client_protocol::Error,
    ) -> Result<(), agent_client_protocol::Error> {
        notice(
            &self.cx,
            &self.session_id,
            format!(
                "cron {}: scheduled turn failed: {}",
                self.id,
                crate::ui::events::sanitize_output(&error.message)
            ),
            meta(serde_json::json!({ "event": "failed", "id": self.id })),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    #[test]
    fn acp_cron_parse_accepts_interval_entries_and_refuses_other_verbs() {
        assert_eq!(parse(""), Ok(Request::List));
        assert_eq!(parse("list"), Ok(Request::List));
        assert_eq!(parse("stop all"), Ok(Request::StopAll));
        assert_eq!(parse("stop c3"), Ok(Request::Stop("c3".into())));
        assert_eq!(
            parse("5m  check the  build "),
            Ok(Request::Arm {
                interval: "5m".into(),
                prompt: "check the  build".into()
            })
        );
        assert_eq!(parse("5m").unwrap_err(), "usage: /cron <interval> <prompt>");
        assert_eq!(parse("list extra").unwrap_err(), USAGE);
        assert_eq!(parse("stop").unwrap_err(), USAGE);
        assert_eq!(parse("stop c1 c2").unwrap_err(), USAGE);
        assert_eq!(parse("run c1").unwrap_err(), RUN_REFUSED);
        assert_eq!(parse("resume").unwrap_err(), RESUME_REFUSED);
        assert_eq!(parse("check the build").unwrap_err(), SELF_PACED_REFUSED);
        assert_eq!(parse("5m /cost").unwrap_err(), PAYLOAD_REFUSED);
        assert_eq!(parse("5m /goal status").unwrap_err(), PAYLOAD_REFUSED);
        assert_eq!(parse("5m !ls").unwrap_err(), PAYLOAD_REFUSED);
    }

    fn arm(cron: &SessionCron, idle: &Arc<Idle>, text: &str) -> Result<Edit, String> {
        cron.apply(parse(text)?, idle, 24, true, Instant::now())
    }

    #[tokio::test(start_paused = true)]
    async fn acp_cron_armed_entry_holds_one_idle_lease_until_the_last_stop() {
        let idle = Idle::new(Some(Duration::from_secs(3))).unwrap();
        let cron = SessionCron::default();
        // The catalogue is empty, so only the schedule can hold the server.
        idle.sessions_changed(0);
        let first = arm(&cron, &idle, "5m one").unwrap();
        assert!(first.spawn_driver, "the first entry starts the driver");
        assert!(!arm(&cron, &idle, "10m two").unwrap().spawn_driver);
        assert!(cron.holds_lease());
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(
            idle.expired().now_or_never().is_none(),
            "an armed entry defers idle-exit"
        );
        cron.apply(Request::Stop("c1".into()), &idle, 24, true, Instant::now())
            .unwrap();
        assert!(cron.holds_lease(), "one entry is still armed");
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(idle.expired().now_or_never().is_none());
        cron.apply(Request::StopAll, &idle, 24, true, Instant::now())
            .unwrap();
        assert!(!cron.holds_lease(), "the last stop releases the lease");
        tokio::time::advance(Duration::from_secs(3)).await;
        idle.expired().await;
    }

    #[tokio::test(start_paused = true)]
    async fn acp_cron_retirement_and_clear_release_the_lease() {
        let idle = Idle::new(Some(Duration::from_secs(3))).unwrap();
        let cron = SessionCron::default();
        arm(&cron, &idle, "1m once").unwrap();
        tokio::time::advance(Duration::from_secs(61)).await;
        assert_eq!(cron.step(Instant::now()), Step::Due);
        let fire = cron.begin_fire(Instant::now(), 1).unwrap();
        assert!(fire.retired);
        assert!(!cron.holds_lease(), "retiring the last entry releases it");
        assert_eq!(cron.step(Instant::now()), Step::Exit);

        arm(&cron, &idle, "1m again").unwrap();
        assert!(cron.holds_lease());
        cron.clear();
        assert!(!cron.holds_lease());
        assert_eq!(cron.len(), 0);
        assert!(
            arm(&cron, &idle, "1m after close").is_err(),
            "a cleared session cannot arm again"
        );
        assert_eq!(cron.step(Instant::now()), Step::Exit);
        tokio::time::advance(Duration::from_secs(3)).await;
        idle.expired().await;
    }

    #[tokio::test(start_paused = true)]
    async fn acp_cron_refusals_leave_no_lease_and_full_store_is_refused() {
        let idle = Idle::new(Some(Duration::from_secs(3))).unwrap();
        let cron = SessionCron::default();
        assert!(arm(&cron, &idle, "0m x").is_err());
        assert!(
            cron.apply(Request::Stop("c9".into()), &idle, 24, true, Instant::now())
                .is_err()
        );
        assert!(!cron.holds_lease());
        for index in 0..schedule::CRON_MAX_ENTRIES {
            arm(&cron, &idle, &format!("5m entry {index}")).unwrap();
        }
        assert!(arm(&cron, &idle, "5m one too many").is_err());
        assert_eq!(cron.len(), schedule::CRON_MAX_ENTRIES);
    }

    #[test]
    fn acp_cron_warns_once_without_a_budget_and_lists_entries() {
        let idle = Idle::new(None).unwrap();
        let cron = SessionCron::default();
        let now = Instant::now();
        let first = cron
            .apply(parse("30s ping").unwrap(), &idle, 5, false, now)
            .unwrap();
        assert_eq!(
            first.lines,
            [
                "cron: interval raised to the 1m minimum",
                "cron c1: every 1m, first fire in 1m; up to 5 fires, only while this session is idle",
                BUDGET_UNBOUNDED,
            ]
        );
        let second = cron
            .apply(parse("2h pong").unwrap(), &idle, 5, false, now)
            .unwrap();
        assert!(!second.lines.iter().any(|line| line == BUDGET_UNBOUNDED));
        let listed = cron.apply(Request::List, &idle, 5, false, now).unwrap();
        assert_eq!(
            listed.lines,
            [
                "c1  every 1m  in 1m  0/5  ping",
                "c2  every 2h  in 2h  0/5  pong"
            ]
        );
    }
}
