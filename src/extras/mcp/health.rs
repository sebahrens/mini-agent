//! Bounded observations of owned connection/catalogue attempts, never a probe
//! or a permission identity. Retained handles are reported independently.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const MAX_HEALTH_SERVERS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptOutcome {
    Pending,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    Removed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptKind {
    Connection,
    Catalogue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Attempt {
    pub id: u64,
    pub outcome: AttemptOutcome,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ServerHealth {
    pub connection: Option<Attempt>,
    pub catalogue: Option<Attempt>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HealthSnapshot {
    pub servers: BTreeMap<String, ServerHealth>,
    pub omitted: bool,
}

#[derive(Debug, Default)]
struct State {
    generation: u64,
    snapshot: HealthSnapshot,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Health(Arc<Mutex<State>>);

impl Health {
    pub(crate) fn snapshot(&self) -> HealthSnapshot {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot
            .clone()
    }

    pub(crate) fn begin(&self, name: &str, kind: AttemptKind) -> AttemptGuard {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let id = state.generation.checked_add(1);
        if id.is_none()
            || name.is_empty()
            || name.len() > 256
            || name.chars().any(char::is_control)
            || (!state.snapshot.servers.contains_key(name)
                && state.snapshot.servers.len() == MAX_HEALTH_SERVERS)
        {
            state.snapshot.omitted = true;
            return AttemptGuard {
                health: self.clone(),
                name: String::new(),
                kind,
                id: None,
                connection: None,
            };
        }
        let id = id.unwrap();
        state.generation = id;
        let row = state.snapshot.servers.entry(name.to_owned()).or_default();
        let connection = row.connection.map(|attempt| attempt.id);
        let attempt = Some(Attempt {
            id,
            outcome: AttemptOutcome::Pending,
        });
        match kind {
            AttemptKind::Connection => {
                row.connection = attempt;
                row.catalogue = None;
            }
            AttemptKind::Catalogue => row.catalogue = attempt,
        }
        AttemptGuard {
            health: self.clone(),
            name: name.to_owned(),
            kind,
            id: Some(id),
            connection,
        }
    }
}

/// A dropped owning future records cancellation for its exact attempt. An old
/// catalogue result cannot overwrite a newer connection's observations.
pub(crate) struct AttemptGuard {
    health: Health,
    name: String,
    kind: AttemptKind,
    id: Option<u64>,
    connection: Option<u64>,
}
impl AttemptGuard {
    pub(crate) fn finish(mut self, outcome: AttemptOutcome) {
        self.record(outcome);
        self.id = None;
    }
    fn record(&self, outcome: AttemptOutcome) {
        let Some(id) = self.id else {
            return;
        };
        let mut state = self
            .health
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(row) = state.snapshot.servers.get_mut(&self.name) else {
            return;
        };
        if self.kind == AttemptKind::Catalogue
            && row.connection.map(|attempt| attempt.id) != self.connection
        {
            return;
        }
        let current = match self.kind {
            AttemptKind::Connection => &mut row.connection,
            AttemptKind::Catalogue => &mut row.catalogue,
        };
        if let Some(attempt) = current.as_mut().filter(|attempt| attempt.id == id) {
            attempt.outcome = outcome;
        }
    }
}
impl Drop for AttemptGuard {
    fn drop(&mut self) {
        self.record(AttemptOutcome::Cancelled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_connection_and_catalogue_completion_cannot_relabel_a_new_attempt() {
        let health = Health::default();
        let first = health.begin("server", AttemptKind::Connection);
        let old_listing = health.begin("server", AttemptKind::Catalogue);
        let second = health.begin("server", AttemptKind::Connection);
        let current = health.snapshot().servers["server"].connection.unwrap();
        assert_eq!(current.outcome, AttemptOutcome::Pending);
        first.finish(AttemptOutcome::Failed);
        old_listing.finish(AttemptOutcome::Succeeded);
        assert_eq!(
            health.snapshot().servers["server"].connection,
            Some(current)
        );
        assert!(health.snapshot().servers["server"].catalogue.is_none());
        second.finish(AttemptOutcome::Succeeded);
        let listing = health.begin("server", AttemptKind::Catalogue);
        drop(listing);
        let snapshot = health.snapshot();
        assert_eq!(
            snapshot.servers["server"].connection.unwrap().outcome,
            AttemptOutcome::Succeeded
        );
        assert_eq!(
            snapshot.servers["server"].catalogue.unwrap().outcome,
            AttemptOutcome::Cancelled
        );
    }
    #[test]
    fn bounded_metadata_omission_never_claims_a_connection_or_loses_cancellation() {
        let health = Health::default();
        for index in 0..MAX_HEALTH_SERVERS {
            health
                .begin(&format!("server-{index}"), AttemptKind::Connection)
                .finish(AttemptOutcome::Failed);
        }
        drop(health.begin("omitted", AttemptKind::Connection));
        let snapshot = health.snapshot();
        assert!(snapshot.omitted);
        assert_eq!(snapshot.servers.len(), MAX_HEALTH_SERVERS);
        assert!(!snapshot.servers.contains_key("omitted"));
        assert!(
            snapshot
                .servers
                .values()
                .all(|row| row.connection.unwrap().outcome == AttemptOutcome::Failed)
        );
        drop(health.begin("server-0", AttemptKind::Connection));
        assert_eq!(
            health.snapshot().servers["server-0"]
                .connection
                .unwrap()
                .outcome,
            AttemptOutcome::Cancelled
        );
    }
}
