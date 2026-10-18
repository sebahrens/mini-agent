//! Per-session recurring scheduler state behind `/cron` (saved disarmed with the session).
//!
//! The store is plain data owned by the TUI `App`: slash edits run inside the
//! user-event batch and the event loop recomputes [`CronStore::next_deadline`]
//! at the top of each iteration, so no `Notify` or task is needed. Deadlines
//! are tokio [`Instant`]s, which are monotonic and do not advance across a
//! system suspend; every display is therefore relative ("in 4m"), never a wall
//! clock time.
//!
//! An entry fires only when the TUI is idle. A deadline that passes while the
//! TUI is busy marks the entry due ([`CronStore::mark_expired_due`]); it then
//! fires once at the next idle point. There is no catch-up: however many
//! deadlines were missed, an entry holds at most one parked fire.
//!
//! Schedules are saved with the session ([`CronStore::export`] into
//! `Session.cron_schedule`) but only ever fire in the process that armed them.
//! A loaded session's schedules are held disarmed ([`CronStore::from_saved`])
//! until `/cron resume` re-arms them with fresh deadlines.

use crate::session::{SavedCronEntry, SavedCronSchedule};
use std::time::Duration;
use tokio::time::Instant;

/// Shortest accepted interval; shorter requests round up with a notice.
pub(crate) const CRON_MIN_INTERVAL: Duration = Duration::from_secs(60);
/// Longest accepted interval.
pub(crate) const CRON_MAX_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Most entries one session may hold at once.
pub(crate) const CRON_MAX_ENTRIES: usize = 16;
/// Default for the `cron_max_fires` config key.
pub(crate) const DEFAULT_CRON_MAX_FIRES: u32 = 24;

/// A parsed `/cron` interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Interval {
    pub duration: Duration,
    /// The request was shorter than [`CRON_MIN_INTERVAL`] and was raised to it.
    pub rounded_up: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntervalError {
    /// The token does not match `^\d+(s|m|h)$`.
    Syntax,
    /// A zero interval.
    Zero,
    /// Longer than [`CRON_MAX_INTERVAL`] (or too large to represent).
    TooLong,
}

impl std::fmt::Display for IntervalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Syntax => f.write_str("interval must look like 90s, 5m or 2h"),
            Self::Zero => f.write_str("interval must be greater than zero"),
            Self::TooLong => write!(
                f,
                "interval must be at most {}",
                format_duration(CRON_MAX_INTERVAL)
            ),
        }
    }
}

/// Whether `token` matches the interval grammar `^\d+(s|m|h)$` as a whole,
/// regardless of whether its value is in range. `/loop` uses this to refuse an
/// interval-first argument, so `5minute-plan` is not an interval.
pub(crate) fn is_interval_token(token: &str) -> bool {
    split_interval(token).is_some()
}

fn split_interval(token: &str) -> Option<(&str, u64)> {
    let unit = token.chars().last()?;
    let seconds = match unit {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        _ => return None,
    };
    let digits = &token[..token.len() - 1];
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some((digits, seconds))
}

/// Parse one interval token, rounding a positive interval below
/// [`CRON_MIN_INTERVAL`] up to it.
pub(crate) fn parse_interval(token: &str) -> Result<Interval, IntervalError> {
    let (digits, unit_seconds) = split_interval(token).ok_or(IntervalError::Syntax)?;
    let value: u64 = digits.parse().map_err(|_| IntervalError::TooLong)?;
    if value == 0 {
        return Err(IntervalError::Zero);
    }
    let seconds = value
        .checked_mul(unit_seconds)
        .ok_or(IntervalError::TooLong)?;
    let duration = Duration::from_secs(seconds);
    if duration > CRON_MAX_INTERVAL {
        return Err(IntervalError::TooLong);
    }
    Ok(if duration < CRON_MIN_INTERVAL {
        Interval {
            duration: CRON_MIN_INTERVAL,
            rounded_up: true,
        }
    } else {
        Interval {
            duration,
            rounded_up: false,
        }
    })
}

/// Compact human form of a duration: `90s`, `5m`, `1h30m`.
pub(crate) fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m, s) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m{s}s"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h{m}m"),
    }
}

/// Relative time until `deadline`, rounded up to whole seconds/minutes:
/// `in 45s`, `in 4m`, `in 1h5m`, or `now` once it has passed.
pub(crate) fn format_until(deadline: Instant, now: Instant) -> String {
    let left = deadline.saturating_duration_since(now);
    if left.is_zero() {
        return "now".to_owned();
    }
    let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    let rounded = if secs < 60 {
        secs
    } else {
        secs.div_ceil(60) * 60
    };
    format!("in {}", format_duration(Duration::from_secs(rounded)))
}

/// One armed schedule.
#[derive(Debug, Clone)]
pub(crate) struct CronEntry {
    /// `c1`, `c2`, ... monotonic per session; never reused.
    pub id: String,
    pub interval: Duration,
    /// Next monotonic deadline. Reset to `now + interval` at arming and at the
    /// start of each fire.
    pub next_at: Instant,
    /// A deadline passed (or `/cron run` asked) and one fire is parked until
    /// the TUI is idle.
    pub due: bool,
    /// Fires started so far.
    pub fires: u32,
    /// The plain prompt submitted on each fire.
    pub payload: String,
    /// Self-paced: the model picks each next fire with `schedule_wakeup`
    /// during the fire's turn; `interval` is unused (zero).
    pub dynamic: bool,
    /// A dynamic entry's fire is running and its next deadline is not chosen
    /// yet; it neither parks nor drives the wakeup until the turn settles.
    pub in_flight: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArmError {
    /// The store already holds [`CRON_MAX_ENTRIES`].
    Full,
}

/// What a fire must submit, returned by [`CronStore::begin_fire`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CronFire {
    pub id: String,
    pub interval: Duration,
    pub payload: String,
    /// 1-based number of this fire.
    pub fire: u32,
    /// This fire reached `cron_max_fires` and the entry was removed.
    pub retired: bool,
    /// The entry is self-paced; its turn is offered `schedule_wakeup`.
    pub dynamic: bool,
}

/// Aggregate view for the statusline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct CronSummary {
    pub armed: usize,
    /// Self-paced entries whose fire's turn is running and has not chosen
    /// the next wakeup yet.
    pub running: usize,
    /// Time until the earliest pending (not yet due) deadline.
    pub next_in: Option<Duration>,
    /// Entries holding a parked fire.
    pub parked: usize,
    pub suspended: bool,
}

/// Session-local `/cron` schedules.
#[derive(Debug, Default)]
pub(crate) struct CronStore {
    entries: Vec<CronEntry>,
    last_id: u64,
    /// Budget exhaustion suspended firing; entries are kept.
    suspended: bool,
    /// The unbounded-spend warning has been shown once this session.
    budget_warned: bool,
    /// Schedules restored from the saved session, kept disarmed until
    /// `/cron resume` (see [`CronStore::from_saved`]).
    disarmed: Vec<SavedCronEntry>,
}

impl CronStore {
    pub(crate) fn entries(&self) -> &[CronEntry] {
        &self.entries
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn get(&self, id: &str) -> Option<&CronEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// Arm a new entry whose first deadline is `now + interval`.
    pub(crate) fn arm(
        &mut self,
        interval: Duration,
        payload: String,
        now: Instant,
    ) -> Result<&CronEntry, ArmError> {
        if self.entries.len() >= CRON_MAX_ENTRIES {
            return Err(ArmError::Full);
        }
        self.last_id += 1;
        self.entries.push(CronEntry {
            id: format!("c{}", self.last_id),
            interval,
            next_at: now + interval,
            due: false,
            fires: 0,
            payload,
            dynamic: false,
            in_flight: false,
        });
        Ok(self.entries.last().expect("just pushed"))
    }

    /// Arm a self-paced entry. Its first fire is parked for the next idle
    /// point; each later deadline comes from that fire's `schedule_wakeup`.
    pub(crate) fn arm_dynamic(
        &mut self,
        payload: String,
        now: Instant,
    ) -> Result<&CronEntry, ArmError> {
        if self.entries.len() >= CRON_MAX_ENTRIES {
            return Err(ArmError::Full);
        }
        self.last_id += 1;
        self.entries.push(CronEntry {
            id: format!("c{}", self.last_id),
            interval: Duration::ZERO,
            next_at: now,
            due: true,
            fires: 0,
            payload,
            dynamic: true,
            in_flight: false,
        });
        Ok(self.entries.last().expect("just pushed"))
    }

    /// Apply a dynamic fire's `schedule_wakeup`: the next deadline is
    /// `now + delay`. `false` when the entry is gone (stopped or retired).
    pub(crate) fn schedule_wakeup(&mut self, id: &str, delay: Duration, now: Instant) -> bool {
        match self.entries.iter_mut().find(|entry| entry.id == id) {
            Some(entry) => {
                entry.in_flight = false;
                entry.due = false;
                entry.next_at = now + delay;
                true
            }
            None => false,
        }
    }

    /// Remove every dynamic entry that is waiting for its next wakeup (Esc at
    /// idle), returning their ids. A dynamic entry has no interval to fall
    /// back to, so clearing its wakeup ends it.
    pub(crate) fn clear_dynamic_wakeups(&mut self) -> Vec<String> {
        let mut cleared = Vec::new();
        self.entries.retain(|entry| {
            let clear = entry.dynamic && !entry.in_flight;
            if clear {
                cleared.push(entry.id.clone());
            }
            !clear
        });
        cleared
    }

    /// Whether any dynamic entry is waiting for its next wakeup.
    pub(crate) fn has_dynamic_wakeup(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.dynamic && !entry.in_flight)
    }

    /// Remove one entry; `false` when no entry has that id.
    pub(crate) fn stop(&mut self, id: &str) -> bool {
        let before = self.entries.len() + self.disarmed.len();
        self.entries.retain(|entry| entry.id != id);
        self.disarmed.retain(|entry| entry.id != id);
        self.entries.len() + self.disarmed.len() != before
    }

    /// Remove every entry, returning how many there were. Ids keep counting.
    pub(crate) fn stop_all(&mut self) -> usize {
        let count = self.entries.len() + self.disarmed.len();
        self.entries.clear();
        self.disarmed.clear();
        self.suspended = false;
        count
    }

    /// Park a fire for `id` (`/cron run`); it runs at the next idle point.
    pub(crate) fn mark_due(&mut self, id: &str) -> bool {
        match self.entries.iter_mut().find(|entry| entry.id == id) {
            Some(entry) => {
                entry.due = true;
                true
            }
            None => false,
        }
    }

    /// Mark every entry whose deadline has passed as due, returning how many
    /// became due now. An already-due entry stays at one parked fire.
    pub(crate) fn mark_expired_due(&mut self, now: Instant) -> usize {
        let mut newly = 0;
        for entry in &mut self.entries {
            if !entry.due && !entry.in_flight && entry.next_at <= now {
                entry.due = true;
                newly += 1;
            }
        }
        newly
    }

    pub(crate) fn has_due(&self) -> bool {
        !self.suspended && self.entries.iter().any(|entry| entry.due)
    }

    /// The due entry to fire first: the one with the earliest deadline.
    pub(crate) fn next_due_id(&self) -> Option<&str> {
        if self.suspended {
            return None;
        }
        self.entries
            .iter()
            .filter(|entry| entry.due)
            .min_by_key(|entry| entry.next_at)
            .map(|entry| entry.id.as_str())
    }

    /// Drop every parked fire without removing the schedules (Esc at idle).
    pub(crate) fn clear_due(&mut self) -> usize {
        let mut cleared = 0;
        for entry in self.entries.iter_mut().filter(|entry| entry.due) {
            entry.due = false;
            cleared += 1;
        }
        cleared
    }

    /// Earliest deadline among entries that are not already due, or `None`
    /// when nothing is pending or firing is suspended. Due entries are left
    /// out so a busy TUI does not wake repeatedly for a deadline it already
    /// recorded; they fire at the next idle point instead.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        if self.suspended {
            return None;
        }
        self.entries
            .iter()
            .filter(|entry| !entry.due && !entry.in_flight)
            .map(|entry| entry.next_at)
            .min()
    }

    /// Start a fire for `id`: clear its parked flag, move its deadline to
    /// `now + interval` before the turn runs (so a long turn cannot double
    /// fire), count it, and remove the entry once it reaches `max_fires`. A
    /// dynamic entry instead goes in flight until its turn's
    /// `schedule_wakeup` (or its absence) settles it.
    pub(crate) fn begin_fire(
        &mut self,
        id: &str,
        now: Instant,
        max_fires: u32,
    ) -> Option<CronFire> {
        let index = self.entries.iter().position(|entry| entry.id == id)?;
        let entry = &mut self.entries[index];
        entry.due = false;
        entry.next_at = now + entry.interval;
        entry.in_flight = entry.dynamic;
        entry.fires = entry.fires.saturating_add(1);
        let fire = CronFire {
            id: entry.id.clone(),
            interval: entry.interval,
            payload: entry.payload.clone(),
            fire: entry.fires,
            retired: entry.fires >= max_fires.max(1),
            dynamic: entry.dynamic,
        };
        if fire.retired {
            self.entries.remove(index);
        }
        Some(fire)
    }

    /// Stop firing (budget exhaustion) while keeping every entry.
    pub(crate) fn suspend(&mut self) {
        self.suspended = true;
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn resume(&mut self) {
        self.suspended = false;
    }

    pub(crate) fn is_suspended(&self) -> bool {
        self.suspended
    }

    /// `true` exactly once per session: the first time an entry is armed
    /// without a USD budget ceiling.
    pub(crate) fn take_budget_warning(&mut self) -> bool {
        !std::mem::replace(&mut self.budget_warned, true)
    }

    pub(crate) fn summary(&self, now: Instant) -> CronSummary {
        CronSummary {
            armed: self.entries.len(),
            running: self.entries.iter().filter(|entry| entry.in_flight).count(),
            next_in: self
                .next_deadline()
                .map(|deadline| deadline.saturating_duration_since(now)),
            parked: self.entries.iter().filter(|entry| entry.due).count(),
            suspended: self.suspended,
        }
    }
}

/// The numeric part of a `c<N>` id, if it has that shape.
fn id_number(id: &str) -> Option<u64> {
    id.strip_prefix('c')?.parse().ok()
}

/// Payload shown on one line, sanitized and cut to `max` characters.
fn payload_preview(payload: &str, max: usize) -> String {
    let flat = crate::ui::events::sanitize_output(payload)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let mut cut: String = flat.chars().take(max.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

/// The one notice shown when a resumed session carries saved schedules, or
/// `None` when it has none.
pub(crate) fn saved_schedule_notice(saved: &SavedCronSchedule) -> Option<String> {
    if saved.entries.is_empty() {
        return None;
    }
    let count = saved.entries.len();
    let listed = saved
        .entries
        .iter()
        .map(|entry| {
            let pace = if entry.dynamic {
                "self-paced".to_owned()
            } else {
                format!(
                    "every {}",
                    format_duration(Duration::from_secs(entry.interval_secs))
                )
            };
            format!(
                "{} {pace}: {}",
                entry.id,
                payload_preview(&entry.payload, 40)
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let noun = if count == 1 { "schedule" } else { "schedules" };
    Some(format!(
        "this session had {count} /cron {noun} ({listed}); /cron resume to re-arm"
    ))
}

impl CronStore {
    /// A store for a freshly loaded session: every saved schedule is held
    /// disarmed, and ids continue after the highest saved one, so a schedule
    /// armed before `/cron resume` never takes a restored id.
    pub(crate) fn from_saved(saved: &SavedCronSchedule) -> Self {
        let highest = saved
            .entries
            .iter()
            .filter_map(|entry| id_number(&entry.id))
            .max()
            .unwrap_or(0);
        Self {
            last_id: saved.last_id.max(highest),
            disarmed: saved.entries.clone(),
            ..Self::default()
        }
    }

    /// Restored schedules waiting for `/cron resume`.
    pub(crate) fn disarmed(&self) -> &[SavedCronEntry] {
        &self.disarmed
    }

    /// Re-arm every disarmed schedule with a fresh deadline `now + interval`
    /// (never a catch-up), keeping its id and fire count; a self-paced one
    /// parks its first fire for the next idle point. Returns the ids
    /// re-armed; schedules beyond [`CRON_MAX_ENTRIES`] stay disarmed.
    pub(crate) fn rearm_saved(&mut self, now: Instant) -> Vec<String> {
        let mut rearmed = Vec::new();
        let mut kept = Vec::new();
        for saved in std::mem::take(&mut self.disarmed) {
            if self.entries.len() >= CRON_MAX_ENTRIES {
                kept.push(saved);
                continue;
            }
            let interval = Duration::from_secs(saved.interval_secs)
                .clamp(CRON_MIN_INTERVAL, CRON_MAX_INTERVAL);
            if let Some(number) = id_number(&saved.id) {
                self.last_id = self.last_id.max(number);
            }
            rearmed.push(saved.id.clone());
            if saved.dynamic {
                self.entries.push(CronEntry {
                    id: saved.id,
                    interval: Duration::ZERO,
                    next_at: now,
                    due: true,
                    fires: saved.fires,
                    payload: saved.payload,
                    dynamic: true,
                    in_flight: false,
                });
                continue;
            }
            self.entries.push(CronEntry {
                id: saved.id,
                interval,
                next_at: now + interval,
                due: false,
                fires: saved.fires,
                payload: saved.payload,
                dynamic: false,
                in_flight: false,
            });
        }
        self.disarmed = kept;
        rearmed
    }

    /// The schedule to save with the session: armed entries (with their next
    /// fire projected onto the wall clock for display) and disarmed ones as
    /// they were restored.
    pub(crate) fn export(
        &self,
        now: Instant,
        wall_now: chrono::DateTime<chrono::Utc>,
    ) -> SavedCronSchedule {
        let armed = self.entries.iter().map(|entry| {
            // An in-flight self-paced entry saves as not in flight: its next
            // fire is unknown, so it is shown as due now.
            let left = if entry.due || entry.in_flight {
                Duration::ZERO
            } else {
                entry.next_at.saturating_duration_since(now)
            };
            let next = chrono::Duration::from_std(left)
                .ok()
                .and_then(|left| wall_now.checked_add_signed(left))
                .unwrap_or(wall_now);
            SavedCronEntry {
                id: entry.id.clone(),
                interval_secs: entry.interval.as_secs(),
                payload: entry.payload.clone(),
                fires: entry.fires,
                next_fire_at: next
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    .into(),
                dynamic: entry.dynamic,
            }
        });
        SavedCronSchedule {
            last_id: self.last_id,
            entries: armed.chain(self.disarmed.iter().cloned()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn interval_grammar_accepts_whole_tokens_only() {
        assert_eq!(parse_interval("5m").unwrap().duration, secs(300));
        assert_eq!(parse_interval("90s").unwrap().duration, secs(90));
        assert_eq!(parse_interval("2h").unwrap().duration, secs(7200));
        assert_eq!(parse_interval("24h").unwrap().duration, CRON_MAX_INTERVAL);
        for bad in [
            "",
            "m",
            "5",
            "5d",
            "5M",
            "-5m",
            "+5m",
            "5 m",
            "5mm",
            "5minute-plan",
            "1.5h",
            "٣m",
        ] {
            assert_eq!(parse_interval(bad), Err(IntervalError::Syntax), "{bad:?}");
            assert!(!is_interval_token(bad), "{bad:?}");
        }
        assert!(is_interval_token("0s"));
        assert!(is_interval_token("99h"));
    }

    #[test]
    fn interval_rejects_zero_and_rounds_short_ones_up() {
        for zero in ["0s", "0m", "000h"] {
            assert_eq!(parse_interval(zero), Err(IntervalError::Zero));
        }
        assert_eq!(
            parse_interval("59s").unwrap(),
            Interval {
                duration: CRON_MIN_INTERVAL,
                rounded_up: true
            }
        );
        assert!(!parse_interval("60s").unwrap().rounded_up);
        assert!(!parse_interval("1m").unwrap().rounded_up);
    }

    #[test]
    fn interval_refuses_longer_than_a_day() {
        assert_eq!(parse_interval("25h"), Err(IntervalError::TooLong));
        assert_eq!(parse_interval("86401s"), Err(IntervalError::TooLong));
        assert_eq!(
            parse_interval("99999999999999999999999h"),
            Err(IntervalError::TooLong)
        );
        assert_eq!(
            parse_interval(&format!("{}h", u64::MAX / 2)),
            Err(IntervalError::TooLong)
        );
    }

    #[test]
    fn store_refuses_the_seventeenth_entry() {
        let now = Instant::now();
        let mut store = CronStore::default();
        for n in 0..CRON_MAX_ENTRIES {
            store.arm(secs(60), format!("p{n}"), now).unwrap();
        }
        assert_eq!(
            store.arm(secs(60), "x".into(), now).unwrap_err(),
            ArmError::Full
        );
        assert_eq!(store.len(), CRON_MAX_ENTRIES);
    }

    #[test]
    fn ids_are_monotonic_and_never_reused() {
        let now = Instant::now();
        let mut store = CronStore::default();
        assert_eq!(store.arm(secs(60), "a".into(), now).unwrap().id, "c1");
        assert_eq!(store.arm(secs(60), "b".into(), now).unwrap().id, "c2");
        assert!(store.stop("c2"));
        assert_eq!(store.arm(secs(60), "c".into(), now).unwrap().id, "c3");
        store.stop_all();
        assert_eq!(store.arm(secs(60), "d".into(), now).unwrap().id, "c4");
    }

    #[test]
    fn next_deadline_picks_the_earliest_pending_entry() {
        let now = Instant::now();
        let mut store = CronStore::default();
        assert_eq!(store.next_deadline(), None);
        store.arm(secs(600), "slow".into(), now).unwrap();
        store.arm(secs(120), "fast".into(), now).unwrap();
        store.arm(secs(300), "mid".into(), now).unwrap();
        assert_eq!(store.next_deadline(), Some(now + secs(120)));

        // A due entry no longer drives the wakeup; the next pending one does.
        assert!(store.mark_due("c2"));
        assert_eq!(store.next_deadline(), Some(now + secs(300)));
        store.suspend();
        assert_eq!(store.next_deadline(), None);
        assert!(!store.has_due());
        store.resume();
        assert!(store.has_due());
    }

    #[test]
    fn stop_removes_one_and_all_removes_every_entry() {
        let now = Instant::now();
        let mut store = CronStore::default();
        store.arm(secs(60), "a".into(), now).unwrap();
        store.arm(secs(60), "b".into(), now).unwrap();
        store.arm(secs(60), "c".into(), now).unwrap();
        assert!(store.stop("c2"));
        assert!(!store.stop("c2"));
        assert!(!store.stop("c9"));
        assert_eq!(
            store
                .entries()
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["c1", "c3"]
        );
        assert_eq!(store.stop_all(), 2);
        assert!(store.is_empty());
        assert_eq!(store.stop_all(), 0);
    }

    #[test]
    fn missed_deadlines_park_a_single_fire_and_the_fire_reschedules_from_its_start() {
        let now = Instant::now();
        let mut store = CronStore::default();
        store.arm(secs(60), "tick".into(), now).unwrap();
        assert_eq!(store.mark_expired_due(now + secs(59)), 0);
        assert_eq!(store.mark_expired_due(now + secs(60)), 1);
        // Further missed deadlines do not stack.
        assert_eq!(store.mark_expired_due(now + secs(300)), 0);
        assert_eq!(store.next_due_id(), Some("c1"));
        assert_eq!(store.summary(now + secs(300)).parked, 1);

        let start = now + secs(301);
        let fire = store.begin_fire("c1", start, 24).unwrap();
        assert_eq!(fire.fire, 1);
        assert_eq!(fire.payload, "tick");
        assert!(!fire.retired);
        let entry = store.get("c1").unwrap();
        assert!(!entry.due);
        assert_eq!(entry.next_at, start + secs(60));
        assert_eq!(store.next_due_id(), None);
    }

    #[test]
    fn reaching_max_fires_removes_the_entry() {
        let now = Instant::now();
        let mut store = CronStore::default();
        store.arm(secs(60), "x".into(), now).unwrap();
        assert!(!store.begin_fire("c1", now, 2).unwrap().retired);
        let last = store.begin_fire("c1", now, 2).unwrap();
        assert!(last.retired);
        assert_eq!(last.fire, 2);
        assert!(store.is_empty());
        assert!(store.begin_fire("c1", now, 2).is_none());
    }

    #[test]
    fn clear_due_keeps_schedules_and_budget_warning_is_one_time() {
        let now = Instant::now();
        let mut store = CronStore::default();
        store.arm(secs(60), "x".into(), now).unwrap();
        store.mark_due("c1");
        assert_eq!(store.clear_due(), 1);
        assert_eq!(store.len(), 1);
        assert!(!store.has_due());
        assert!(store.take_budget_warning());
        assert!(!store.take_budget_warning());
    }

    #[test]
    fn dynamic_entries_fire_first_at_idle_and_reschedule_from_the_wakeup() {
        let now = Instant::now();
        let mut store = CronStore::default();
        let entry = store.arm_dynamic("watch ci".into(), now).unwrap();
        assert!(entry.dynamic && entry.due && entry.id == "c1");
        assert_eq!(store.next_due_id(), Some("c1"));

        let fire = store.begin_fire("c1", now, 24).unwrap();
        assert!(fire.dynamic && !fire.retired);
        let entry = store.get("c1").unwrap();
        assert!(entry.in_flight && !entry.due);
        // In flight: never parked again and never drives the wakeup.
        assert_eq!(store.mark_expired_due(now + secs(7200)), 0);
        assert_eq!(store.next_deadline(), None);
        assert!(!store.has_dynamic_wakeup());
        assert!(store.clear_dynamic_wakeups().is_empty());

        assert!(store.schedule_wakeup("c1", secs(300), now + secs(10)));
        let entry = store.get("c1").unwrap();
        assert!(!entry.in_flight && !entry.due);
        assert_eq!(store.next_deadline(), Some(now + secs(310)));
        assert!(store.has_dynamic_wakeup());
        assert!(!store.schedule_wakeup("c9", secs(60), now));
    }

    #[test]
    fn an_in_flight_self_paced_entry_exports_as_due_and_not_in_flight() {
        let now = Instant::now();
        let mut store = CronStore::default();
        store.arm_dynamic("watch".into(), now).unwrap();
        store.begin_fire("c1", now, 24).unwrap();
        assert_eq!(store.summary(now).running, 1);
        let wall = chrono::Utc::now();
        let saved = store.export(now + secs(30), wall);
        let entry = &saved.entries[0];
        assert!(entry.dynamic && entry.fires == 1);
        assert_eq!(
            entry.next_fire_at,
            wall.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        );
        let mut restored = CronStore::from_saved(&saved);
        restored.rearm_saved(now);
        let entry = restored.get("c1").unwrap();
        assert!(entry.dynamic && entry.due && !entry.in_flight);
    }

    #[test]
    fn clearing_dynamic_wakeups_keeps_interval_entries() {
        let now = Instant::now();
        let mut store = CronStore::default();
        store.arm(secs(60), "tick".into(), now).unwrap();
        store.arm_dynamic("watch".into(), now).unwrap();
        store.arm_dynamic("busy".into(), now).unwrap();
        store.begin_fire("c3", now, 24).unwrap();
        assert_eq!(store.clear_dynamic_wakeups(), ["c2"]);
        let ids: Vec<_> = store.entries().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["c1", "c3"], "the in-flight fire settles on its own");
    }

    #[test]
    fn relative_times_round_up_and_read_compactly() {
        let now = Instant::now();
        assert_eq!(format_until(now, now), "now");
        assert_eq!(
            format_until(now + Duration::from_millis(1500), now),
            "in 2s"
        );
        assert_eq!(format_until(now + secs(45), now), "in 45s");
        assert_eq!(format_until(now + secs(181), now), "in 4m");
        assert_eq!(format_until(now + secs(3900), now), "in 1h5m");
        assert_eq!(format_duration(secs(3600)), "1h");
        assert_eq!(format_duration(secs(90)), "1m30s");
    }

    fn saved(id: &str, interval_secs: u64, fires: u32, payload: &str) -> SavedCronEntry {
        SavedCronEntry {
            id: id.into(),
            interval_secs,
            payload: payload.into(),
            fires,
            next_fire_at: "2026-10-18T12:00:00Z".into(),
            dynamic: false,
        }
    }

    #[test]
    fn restored_schedules_stay_disarmed_until_rearmed_with_fresh_deadlines() {
        let schedule = SavedCronSchedule {
            last_id: 4,
            entries: vec![saved("c2", 300, 5, "a"), saved("c4", 3600, 0, "b")],
        };
        let mut store = CronStore::from_saved(&schedule);
        assert!(store.is_empty(), "nothing is armed on load");
        assert_eq!(store.next_deadline(), None);
        assert_eq!(store.disarmed().len(), 2);

        // Arming before /cron resume continues after the saved ids.
        let now = Instant::now();
        assert_eq!(store.arm(secs(60), "new".into(), now).unwrap().id, "c5");

        let later = now + secs(10_000);
        assert_eq!(store.rearm_saved(later), ["c2", "c4"]);
        assert!(store.disarmed().is_empty());
        let c2 = store.get("c2").unwrap();
        assert_eq!(
            (c2.fires, c2.next_at, c2.due),
            (5, later + secs(300), false)
        );
        assert_eq!(store.get("c4").unwrap().next_at, later + secs(3600));
        assert!(store.rearm_saved(later).is_empty());
        assert_eq!(store.arm(secs(60), "x".into(), later).unwrap().id, "c6");
    }

    #[test]
    fn next_id_passes_the_highest_restored_id_even_without_last_id() {
        let schedule = SavedCronSchedule {
            last_id: 0,
            entries: vec![saved("c9", 60, 0, "a")],
        };
        let mut store = CronStore::from_saved(&schedule);
        assert_eq!(
            store.arm(secs(60), "b".into(), Instant::now()).unwrap().id,
            "c10"
        );
    }

    #[test]
    fn export_keeps_ids_fires_and_disarmed_entries_and_stop_removes_disarmed() {
        let now = Instant::now();
        let wall = chrono::DateTime::parse_from_rfc3339("2026-10-18T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let mut store = CronStore::from_saved(&SavedCronSchedule {
            last_id: 1,
            entries: vec![saved("c1", 120, 3, "old")],
        });
        store.arm(secs(300), "new".into(), now).unwrap();
        let exported = store.export(now + secs(60), wall);
        assert_eq!(exported.last_id, 2);
        assert_eq!(
            exported.entries,
            [
                SavedCronEntry {
                    id: "c2".into(),
                    interval_secs: 300,
                    payload: "new".into(),
                    fires: 0,
                    next_fire_at: "2026-10-18T12:04:00Z".into(),
                    dynamic: false,
                },
                saved("c1", 120, 3, "old"),
            ]
        );
        assert!(store.stop("c1"));
        assert!(store.disarmed().is_empty());
        assert!(CronStore::default().export(now, wall).is_empty());
    }

    #[test]
    fn saved_schedule_notice_lists_every_entry_once() {
        assert_eq!(saved_schedule_notice(&SavedCronSchedule::default()), None);
        let notice = saved_schedule_notice(&SavedCronSchedule {
            last_id: 2,
            entries: vec![
                saved("c1", 300, 0, "check\nthe deploy"),
                saved("c2", 7200, 1, &"x".repeat(60)),
            ],
        })
        .unwrap();
        assert!(
            notice.starts_with(
                "this session had 2 /cron schedules (c1 every 5m: check the deploy; c2 every 2h: xxx"
            ),
            "{notice}"
        );
        assert!(notice.ends_with("…); /cron resume to re-arm"), "{notice}");
    }
}
