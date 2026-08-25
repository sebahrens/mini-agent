//! Workspace-bound, session-scoped ownership for learned-JS services.
//!
//! Agent rebuilds borrow handles from this owner. Storage/index initialization and the
//! proposal, admission, and telemetry workers therefore run once per logical session and
//! workspace instead of once per rebuilt `JsTool`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::OnceCell;

use super::admission::{AdmissionEvaluator, AdmissionWorker};
use super::embed::Embedder;
use super::proposal::{
    AttemptBudget, DEFAULT_SESSION_ATTEMPTS, ProposalEffectService, ProposalHost, ProposalQueue,
    ProposalWorker,
};
use super::store::SkillStore;
use super::telemetry::TelemetryDispatcher;
use super::turn::{SkillRuntime, SkillTurnContext, shared_coordinator};
use crate::config::EmbeddingConfig;
use crate::paths::WorkspaceBinding;

struct WorkspaceSlot<T> {
    root: PathBuf,
    services: Arc<OnceCell<Option<Arc<T>>>>,
}

struct WorkspaceServiceCache<T> {
    slot: Mutex<Option<WorkspaceSlot<T>>>,
}

impl<T> WorkspaceServiceCache<T> {
    fn new() -> Self {
        Self {
            slot: Mutex::new(None),
        }
    }

    async fn resolve<F, Fut>(&self, root: PathBuf, initialize: F) -> Option<Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Option<Arc<T>>>,
    {
        let cell = {
            let mut slot = self.slot.lock().unwrap_or_else(|error| error.into_inner());
            match slot.as_ref() {
                Some(slot) if slot.root == root => Arc::clone(&slot.services),
                _ => {
                    let services = Arc::new(OnceCell::new());
                    *slot = Some(WorkspaceSlot {
                        root,
                        services: Arc::clone(&services),
                    });
                    services
                }
            }
        };
        cell.get_or_init(initialize).await.clone()
    }
}

/// A cheap session handle whose current workspace is initialized at most once.
///
/// Rebinding to another canonical workspace replaces the slot. Existing agents retain the old
/// service `Arc` until they finish, while subsequent rebuilds initialize services for the new
/// authority. Failed initialization is cached as `None` so rebuilds do not churn on startup.
pub(crate) struct SkillServiceOwner {
    cache: WorkspaceServiceCache<SkillSessionServices>,
    #[cfg(test)]
    initialization_attempts: std::sync::atomic::AtomicUsize,
}

impl SkillServiceOwner {
    pub(crate) fn new() -> Self {
        Self {
            cache: WorkspaceServiceCache::new(),
            #[cfg(test)]
            initialization_attempts: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(crate) async fn resolve(
        &self,
        workspace: &Arc<WorkspaceBinding>,
        embedding: Option<EmbeddingConfig>,
    ) -> Option<Arc<SkillSessionServices>> {
        let root = workspace.root().to_path_buf();
        self.cache
            .resolve(root.clone(), || async {
                #[cfg(test)]
                self.initialization_attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                SkillSessionServices::open(root, embedding).await
            })
            .await
    }

    #[cfg(test)]
    pub(crate) fn initialization_attempts(&self) -> usize {
        self.initialization_attempts
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

// These cache tests sit next to the private cache they exercise; production services follow below.
#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use super::WorkspaceServiceCache;
    use crate::extras::js::skills::turn::{SkillTurnContext, TurnSkillBundle};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn repeated_rebuilds_initialize_runtime_and_each_worker_once() {
        let cache = WorkspaceServiceCache::new();
        let starts = Arc::new([const { AtomicUsize::new(0) }; 4]);
        let first_starts = Arc::clone(&starts);
        let first = cache
            .resolve("workspace-a".into(), || async move {
                for count in first_starts.iter() {
                    count.fetch_add(1, Ordering::SeqCst);
                }
                Some(Arc::new("services"))
            })
            .await
            .expect("first service initialization");
        let second_starts = Arc::clone(&starts);
        let second = cache
            .resolve("workspace-a".into(), || async move {
                for count in second_starts.iter() {
                    count.fetch_add(1, Ordering::SeqCst);
                }
                Some(Arc::new("unexpected replacement"))
            })
            .await
            .expect("cached services");

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(
            starts
                .iter()
                .map(|count| count.load(Ordering::SeqCst))
                .collect::<Vec<_>>(),
            vec![1, 1, 1, 1],
            "runtime, proposal, admission, and telemetry must each start once"
        );
    }

    #[tokio::test]
    async fn failed_initialization_is_cached_without_rebuild_churn() {
        let cache = WorkspaceServiceCache::<()>::new();
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let calls = Arc::clone(&calls);
            assert!(
                cache
                    .resolve("workspace-a".into(), || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        None
                    })
                    .await
                    .is_none()
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn workspace_rebind_gets_a_fresh_service_bundle() {
        let cache = WorkspaceServiceCache::new();
        let first = cache
            .resolve("workspace-a".into(), || async { Some(Arc::new(1_u8)) })
            .await
            .unwrap();
        let second = cache
            .resolve("workspace-b".into(), || async { Some(Arc::new(2_u8)) })
            .await
            .unwrap();

        assert_eq!(*first, 1);
        assert_eq!(*second, 2);
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn independent_sessions_do_not_share_turn_service_state() {
        let first = WorkspaceServiceCache::new();
        let second = WorkspaceServiceCache::new();
        let first_service = first
            .resolve("workspace-a".into(), || async {
                Some(Arc::new(SkillTurnContext::new(TurnSkillBundle::empty(
                    "first",
                ))))
            })
            .await
            .unwrap();
        let second_service = second
            .resolve("workspace-a".into(), || async {
                Some(Arc::new(SkillTurnContext::new(TurnSkillBundle::empty(
                    "second",
                ))))
            })
            .await
            .unwrap();

        assert!(!Arc::ptr_eq(&first_service, &second_service));
    }

    #[tokio::test]
    async fn session_owner_releases_all_owned_workers_on_teardown() {
        struct DropProbe(Arc<AtomicUsize>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let cache = WorkspaceServiceCache::new();
        let probe = Arc::clone(&drops);
        let service = cache
            .resolve("workspace-a".into(), || async move {
                Some(Arc::new([
                    DropProbe(Arc::clone(&probe)),
                    DropProbe(Arc::clone(&probe)),
                    DropProbe(probe),
                ]))
            })
            .await
            .unwrap();
        drop(service);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(cache);
        assert_eq!(drops.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_new_owner_does_not_initialize_unused_services() {
        let owner = super::SkillServiceOwner::new();
        assert_eq!(owner.initialization_attempts(), 0);
    }
}

struct MutationServices {
    proposal: ProposalEffectService,
    telemetry: Arc<TelemetryDispatcher>,
    _proposal_worker: ProposalWorker,
    _admission_worker: AdmissionWorker,
}

/// The initialized learned-JS runtime and parent-side service workers for one workspace session.
pub(crate) struct SkillSessionServices {
    runtime: Arc<SkillRuntime>,
    mutation: Option<MutationServices>,
    turn_gate: Arc<tokio::sync::Mutex<()>>,
}

impl SkillSessionServices {
    async fn open(
        workspace_root: PathBuf,
        embedding: Option<EmbeddingConfig>,
    ) -> Option<Arc<Self>> {
        let paths = match crate::paths::process_paths()
            .and_then(|paths| paths.with_workspace_root(&workspace_root))
        {
            Ok(paths) => paths,
            Err(error) => {
                tracing::warn!("skill discovery paths unavailable: {error}");
                return None;
            }
        };

        let runtime_paths = paths.clone();
        let runtime_embedding = embedding.clone();
        let runtime = match crate::agent::runner::spawn_blocking_scoped(move || {
            SkillRuntime::open_with_learned_js(&runtime_paths, runtime_embedding.as_ref(), true)
        })
        .await
        {
            Ok(Ok(runtime)) => Arc::new(runtime),
            Ok(Err(error)) => {
                tracing::warn!("skill discovery disabled: {error}");
                return None;
            }
            Err(error) => {
                tracing::warn!("skill discovery startup worker failed: {error}");
                return None;
            }
        };
        runtime.schedule_learned_rebuild();

        let mutation = match Self::start_mutation_services(&paths, embedding.as_ref()) {
            Ok(services) => Some(services),
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "skill proposal storage unavailable; propose_skill is disabled"
                );
                None
            }
        };

        Some(Arc::new(Self {
            runtime,
            mutation,
            turn_gate: Arc::new(tokio::sync::Mutex::new(())),
        }))
    }

    fn start_mutation_services(
        paths: &crate::paths::AppPaths,
        embedding: Option<&EmbeddingConfig>,
    ) -> Result<MutationServices, String> {
        let proposal_store = SkillStore::open_at(paths).map_err(|error| error.to_string())?;
        let evaluator_store = SkillStore::open_at(paths).map_err(|error| error.to_string())?;
        let embedder = Embedder::from_config(embedding).map_err(|error| error.to_string())?;
        let telemetry_embedder =
            Arc::new(Embedder::from_config(embedding).map_err(|error| error.to_string())?);
        let (coordinator, _) =
            shared_coordinator(paths, telemetry_embedder).map_err(|error| error.to_string())?;
        let telemetry = Arc::new(
            TelemetryDispatcher::spawn_session_scoped_with_coordinator(paths, coordinator)
                .map_err(|error| error.to_string())?,
        );
        let evaluator = AdmissionEvaluator::new(
            evaluator_store,
            embedder,
            format!("mini-agent-{}", std::process::id()),
        )
        .map_err(|error| error.to_string())?;
        let admission_worker =
            AdmissionWorker::start_session_scoped(evaluator).map_err(|error| error.to_string())?;
        let proposal_worker =
            ProposalQueue::start_store_worker(proposal_store, 16, Duration::from_secs(2))
                .map_err(|error| error.to_string())?;
        let proposal = ProposalEffectService::new(ProposalHost::new(
            proposal_worker.sender(),
            AttemptBudget::new(DEFAULT_SESSION_ATTEMPTS),
        ));
        Ok(MutationServices {
            proposal,
            telemetry,
            _proposal_worker: proposal_worker,
            _admission_worker: admission_worker,
        })
    }

    pub(crate) fn turn_context(&self) -> Arc<SkillTurnContext> {
        self.runtime.turn_context()
    }

    pub(crate) async fn prepare_prompt(&self, prompt: &str) -> String {
        self.runtime.prepare_prompt(prompt).await
    }

    pub(crate) fn turn_gate(&self) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(&self.turn_gate)
    }

    pub(crate) fn proposal(&self) -> Option<ProposalEffectService> {
        self.mutation
            .as_ref()
            .map(|services| services.proposal.clone())
    }

    pub(crate) fn telemetry(&self) -> Option<Arc<TelemetryDispatcher>> {
        self.mutation
            .as_ref()
            .map(|services| Arc::clone(&services.telemetry))
    }
}
