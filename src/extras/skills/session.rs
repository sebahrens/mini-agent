//! Explicit session-owned instruction catalogues and per-run selection. No
//! learned store, worker, registry, or ambient authority is initialized here.
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::paths::{AppPaths, WorkspaceBinding};
use crate::permission::checker::PermCheck;

use super::discovery::{Discovery, Package, SourceRoot};
use super::index::AgentSkillIndex;
use super::selection::InstructionSelection;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pin {
    pub(crate) name: String,
    pub(crate) digest: String,
    pub(crate) origin: &'static str,
    scope: uuid::Uuid,
}

#[derive(Default)]
pub(crate) struct InstructionOwner {
    cached: Mutex<Option<Arc<InstructionServices>>>,
    #[cfg(test)]
    home_override: Option<Option<PathBuf>>,
}

pub(crate) struct InstructionServices {
    scope: uuid::Uuid,
    paths: AppPaths,
    workspace: Arc<WorkspaceBinding>,
    permission: Option<PermCheck>,
    home: Option<PathBuf>,
    roots: OnceLock<Vec<SourceRoot>>,
    no_context_files: bool,
    current: RwLock<Arc<Discovery>>,
    generation: AtomicU64,
    refresh_gate: Arc<tokio::sync::Mutex<()>>,
}

impl InstructionOwner {
    #[cfg(test)]
    pub(crate) fn for_test(home: Option<PathBuf>) -> Self {
        Self {
            cached: Mutex::new(None),
            home_override: Some(home),
        }
    }

    pub(crate) fn resolve(
        &self,
        paths: AppPaths,
        workspace: Arc<WorkspaceBinding>,
        permission: Option<PermCheck>,
        no_context_files: bool,
    ) -> Arc<InstructionServices> {
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(service) = cached.as_ref()
            && Arc::ptr_eq(&service.workspace, &workspace)
            && same_permission(&service.permission, &permission)
            && service.no_context_files == no_context_files
            && service.paths == paths
        {
            return Arc::clone(service);
        }
        let service = Arc::new(InstructionServices {
            scope: uuid::Uuid::new_v4(),
            paths,
            workspace,
            permission,
            #[cfg(test)]
            home: self
                .home_override
                .clone()
                .unwrap_or_else(crate::paths::process_home_dir),
            #[cfg(not(test))]
            home: crate::paths::process_home_dir(),
            roots: OnceLock::new(),
            no_context_files,
            current: RwLock::new(Arc::new(Discovery::default())),
            generation: AtomicU64::new(0),
            refresh_gate: Arc::new(tokio::sync::Mutex::new(())),
        });
        *cached = Some(Arc::clone(&service));
        service
    }
}

fn same_permission(left: &Option<PermCheck>, right: &Option<PermCheck>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

impl InstructionServices {
    fn source_roots(&self, explicit: bool) -> Vec<SourceRoot> {
        if self.no_context_files && !explicit {
            return Vec::new();
        }
        self.roots
            .get_or_init(|| {
                let mut roots = vec![
                    SourceRoot {
                        binding: Arc::clone(&self.workspace),
                        directory: ".zerostack/skills".into(),
                        origin: "project .zerostack",
                        project: true,
                    },
                    SourceRoot {
                        binding: Arc::clone(&self.workspace),
                        directory: ".mini-agent/skills".into(),
                        origin: "project .mini-agent",
                        project: true,
                    },
                    SourceRoot {
                        binding: Arc::clone(&self.workspace),
                        directory: ".claude/skills".into(),
                        origin: "project .claude",
                        project: true,
                    },
                ];
                if let Some(home) = &self.home
                    && let Ok(binding) = WorkspaceBinding::capture(home)
                {
                    roots.push(SourceRoot {
                        binding: Arc::new(binding),
                        directory: ".claude/skills".into(),
                        origin: "user .claude",
                        project: false,
                    });
                }
                roots
            })
            .clone()
    }

    pub(crate) async fn refresh(
        self: &Arc<Self>,
        explicit: bool,
    ) -> Result<Arc<Discovery>, String> {
        let _gate = Arc::clone(&self.refresh_gate).lock_owned().await;
        let service = Arc::clone(self);
        let discovered = blocking_owned(move || {
            service.workspace.validate()?;
            let roots = service.source_roots(explicit);
            Ok::<_, String>(Arc::new(super::discovery::discover(
                &service.paths,
                &roots,
                &service.permission,
            )))
        })
        .await
        .map_err(|_| "instruction discovery worker unavailable")??;
        if crate::agent::runner::current_work_cancellation()
            .is_some_and(|cancel| cancel.is_cancelled())
        {
            return Err("instruction discovery cancelled".into());
        }
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = Arc::clone(&discovered);
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(discovered)
    }

    pub(crate) fn same_authority(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.workspace, &other.workspace)
            && same_permission(&self.permission, &other.permission)
            && self.no_context_files == other.no_context_files
            && self.paths == other.paths
    }

    pub(crate) fn cached(&self) -> Arc<Discovery> {
        self.current
            .read()
            .map(|snapshot| Arc::clone(&snapshot))
            .unwrap_or_else(|e| Arc::clone(&e.into_inner()))
    }

    pub(crate) async fn pin(self: &Arc<Self>, name: &str) -> Result<Pin, String> {
        let snapshot = self.refresh(true).await?;
        let package = snapshot
            .packages
            .get(name)
            .ok_or("skill is unavailable or requires source approval/read permission")?;
        if crate::ui::pickers::list::reserved_command(name) {
            return Err("skill name collides with a built-in command".into());
        }
        Ok(Pin {
            name: name.to_string(),
            digest: package.record.digest.clone(),
            origin: package.origin,
            scope: self.scope,
        })
    }

    pub(crate) async fn select(
        self: &Arc<Self>,
        query: &str,
        pin: Option<&Pin>,
    ) -> Result<InstructionSelection, String> {
        self.select_with_embedder(
            query,
            pin,
            #[cfg(feature = "skills")]
            None,
        )
        .await
    }

    pub(crate) async fn select_with_embedder(
        self: &Arc<Self>,
        query: &str,
        pin: Option<&Pin>,
        #[cfg(feature = "skills")] embedder: Option<
            Arc<crate::extras::js::skills::embed::Embedder>,
        >,
    ) -> Result<InstructionSelection, String> {
        let snapshot = self.refresh(pin.is_some()).await?;
        if let Some(pin) = pin
            && !snapshot.packages.get(&pin.name).is_some_and(|package| {
                package.record.digest == pin.digest
                    && package.origin == pin.origin
                    && pin.scope == self.scope
            })
        {
            return Err("pinned skill source changed or lost current read/trust authority".into());
        }
        let query = super::retrieval::normalize_query(query);
        #[cfg(feature = "skills")]
        let vector = if let Some(embedder) = &embedder {
            if embedder.supports_semantic_retrieval() {
                Some(
                    embedder
                        .embed_query_cached(&query)
                        .await
                        .map_err(|_| "skill query embedding unavailable")?,
                )
            } else {
                None
            }
        } else {
            None
        };
        let pin = pin.cloned();
        let generation = self.generation.load(Ordering::Relaxed);
        let services = Arc::clone(self);
        blocking_owned(move || {
            // Recheck original source authority immediately before loading the
            // immutable snapshot. Cached publication is not continued approval.
            #[allow(unused_mut)]
            let mut records: Vec<_> = snapshot
                .packages
                .values()
                .filter(|package| package.authorized(&services.paths, &services.permission))
                .map(|package| {
                    let mut record = (*package.record).clone();
                    record.embedding.clear();
                    record
                })
                .collect();
            #[cfg(feature = "skills")]
            let semantic_model = if vector.is_some() {
                let embedder = embedder.as_ref().expect("query vector has an embedder");
                let documents = records
                    .iter()
                    .map(|record| record.embedding_document())
                    .collect::<Vec<_>>();
                let vectors = embedder
                    .embed_documents(&documents)
                    .map_err(|_| "skill metadata embedding unavailable")?;
                for (record, embedding) in records.iter_mut().zip(vectors) {
                    record.embedding = embedding;
                }
                Some(embedder.model_metadata().clone())
            } else {
                None
            };
            let model = super::retrieval::ModelMetadata {
                model_id: "lexical-only".into(),
                model_revision: "agent-skills-lexical-v1".into(),
                dimensions: 0,
                normalized: false,
            };
            #[cfg(feature = "skills")]
            let model = semantic_model.unwrap_or(model);
            let index = AgentSkillIndex::build(generation, model, records)
                .map_err(|error| error.to_string())?;
            super::selection::select_with_vector(
                &index,
                &query,
                pin.as_ref().map(|pin| pin.digest.as_str()),
                #[cfg(feature = "skills")]
                vector.as_deref(),
                #[cfg(not(feature = "skills"))]
                None,
            )
        })
        .await
        .map_err(|_| "instruction selection worker unavailable")?
    }
}

/// One captured ordinary invocation. Cloning it for retry/compaction retains
/// its selected pin; a new explicit user turn creates another cell.
pub(crate) struct InstructionTurn {
    pub(crate) services: Arc<InstructionServices>,
    pub(crate) pin: Option<Pin>,
    selected: tokio::sync::Mutex<Option<PreparedSelection>>,
    native: std::sync::atomic::AtomicBool,
    native_composition: tokio::sync::Mutex<Option<NativeComposition>>,
    #[cfg(feature = "skills")]
    learned_started: std::sync::atomic::AtomicBool,
}
#[derive(Clone)]
pub(crate) struct PreparedSelection {
    pub(crate) selection: Arc<InstructionSelection>,
    packages: Vec<Package>,
    _lease: Arc<Discovery>,
}

struct NativeComposition {
    base_digest: Vec<u8>,
    block: String,
}

impl InstructionTurn {
    pub(crate) fn new(services: Arc<InstructionServices>, pin: Option<Pin>) -> Arc<Self> {
        Arc::new(Self {
            services,
            pin,
            selected: tokio::sync::Mutex::new(None),
            native: std::sync::atomic::AtomicBool::new(false),
            native_composition: tokio::sync::Mutex::new(None),
            #[cfg(feature = "skills")]
            learned_started: std::sync::atomic::AtomicBool::new(false),
        })
    }
    pub(crate) async fn prepare(&self, prompt: &str) -> Result<Arc<InstructionSelection>, String> {
        self.prepare_with_embedder(
            prompt,
            #[cfg(feature = "skills")]
            None,
        )
        .await
    }
    pub(crate) async fn prepare_with_embedder(
        &self,
        prompt: &str,
        #[cfg(feature = "skills")] embedder: Option<
            Arc<crate::extras::js::skills::embed::Embedder>,
        >,
    ) -> Result<Arc<InstructionSelection>, String> {
        let mut selected = self.selected.lock().await;
        if let Some(prepared) = &*selected {
            self.revalidate_packages(prepared.packages.clone()).await?;
            return Ok(Arc::clone(&prepared.selection));
        }
        let selection = self
            .services
            .select_with_embedder(
                prompt,
                self.pin.as_ref(),
                #[cfg(feature = "skills")]
                embedder,
            )
            .await?;
        // Prove pin fit before the caller installs it or dispatches a request.
        super::selection::render_plain(&selection)?;
        let snapshot = self.services.cached();
        let packages = selection
            .selected
            .iter()
            .map(|selected| {
                snapshot
                    .packages
                    .get(&selected.name)
                    .filter(|package| package.record.digest == selected.digest)
                    .cloned()
                    .ok_or_else(|| "skill catalogue changed during selection".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.revalidate_packages(packages.clone()).await?;
        let selection = Arc::new(selection);
        *selected = Some(PreparedSelection {
            selection: Arc::clone(&selection),
            packages,
            _lease: snapshot,
        });
        Ok(selection)
    }
    #[cfg(feature = "skills")]
    pub(crate) fn preserve_learned_turn(&self) -> bool {
        self.learned_started.swap(true, Ordering::Relaxed)
    }

    pub(crate) async fn candidate(
        &self,
        query: &str,
        #[cfg(feature = "skills")] embedder: Option<
            Arc<crate::extras::js::skills::embed::Embedder>,
        >,
    ) -> Result<PreparedSelection, String> {
        let selection = self
            .services
            .select_with_embedder(
                query,
                self.pin.as_ref(),
                #[cfg(feature = "skills")]
                embedder,
            )
            .await?;
        super::selection::render_plain(&selection)?;
        let snapshot = self.services.cached();
        let packages = selection
            .selected
            .iter()
            .map(|selected| {
                snapshot
                    .packages
                    .get(&selected.name)
                    .filter(|package| package.record.digest == selected.digest)
                    .cloned()
                    .ok_or_else(|| "skill catalogue changed during selection".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.revalidate_packages(packages.clone()).await?;
        Ok(PreparedSelection {
            selection: Arc::new(selection),
            packages,
            _lease: snapshot,
        })
    }
    pub(crate) fn native_result(&self, metadata: String, block: String) -> Result<String, String> {
        if !self.native.load(Ordering::Acquire) {
            return Ok(metadata);
        }
        if block.len() > 64 * 1024 {
            return Err("native skill context budget exceeded".into());
        }
        let output = if block.is_empty() {
            metadata
        } else {
            format!("{metadata}\n\n{block}")
        };
        if output.len() > 80 * 1024 {
            return Err("native skill result budget exceeded".into());
        }
        Ok(output)
    }

    pub(crate) async fn publish_candidate(&self, candidate: PreparedSelection) {
        *self.selected.lock().await = Some(candidate);
    }

    async fn revalidate_packages(&self, packages: Vec<Package>) -> Result<(), String> {
        let services = Arc::clone(&self.services);
        let allowed = blocking_owned(move || {
            services.workspace.validate().is_ok()
                && packages
                    .iter()
                    .all(|package| package.authorized(&services.paths, &services.permission))
        })
        .await
        .map_err(|_| "skill source revalidation worker unavailable")?;
        if !allowed
            || crate::agent::runner::current_work_cancellation()
                .is_some_and(|cancel| cancel.is_cancelled())
        {
            return Err("skill source lost current read/trust authority or was cancelled".into());
        }
        Ok(())
    }
}

impl std::fmt::Debug for InstructionOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("InstructionOwner { captured catalogue }")
    }
}

async fn blocking_owned<F, R>(operation: F) -> Result<R, tokio::task::JoinError>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    if crate::agent::runner::current_work_cancellation().is_some() {
        return crate::agent::runner::spawn_blocking_scoped(operation).await;
    }
    let scope = crate::agent::runner::AgentWorkScope::new();
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(Arc::clone(&scope));
    let result = scope
        .run(async move { crate::agent::runner::spawn_blocking_scoped(operation).await })
        .await;
    cleanup.settle().await;
    result
}

#[derive(Clone)]
pub(crate) struct InstructionCarrier(pub(crate) Arc<InstructionTurn>);

#[derive(Clone)]
pub(crate) struct SearchCandidate {
    pub(crate) turn: Arc<InstructionTurn>,
    pub(crate) prepared: PreparedSelection,
    pub(crate) output: String,
    #[cfg(feature = "skills")]
    pub(crate) learned: Option<Arc<PendingLearned>>,
}

#[cfg(feature = "skills")]
pub(crate) struct PendingLearned {
    pub(crate) services: Arc<crate::extras::js::skills::session::SkillSessionServices>,
    pub(crate) discovery: crate::extras::js::skills::turn::TurnDiscoveryBundle,
    pub(crate) committed: Mutex<bool>,
}
#[cfg(feature = "skills")]
impl PendingLearned {
    fn publish(&self, turn: &Arc<InstructionTurn>) -> bool {
        let mut committed = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        if *committed {
            return true;
        }
        if !self
            .services
            .publish_search_candidate(turn, &self.discovery)
        {
            return false;
        }
        *committed = true;
        true
    }
}

#[derive(Clone, Default)]
struct NativeDelivery(Arc<Mutex<NativeDeliveryState>>);
#[derive(Default)]
struct NativeDeliveryState {
    failed: bool,
    receipts: Vec<NativeSkillReceipt>,
}
struct NativeSkillReceipt {
    id: String,
    internal_id: String,
    call_id: Option<String>,
    output: String,
}

/// A single request-preamble compositor, captured by this exact runner. It
/// never appends instructions to the user's prompt or changes permission state.
#[derive(Clone)]
pub(crate) struct InstructionContextHook {
    base: String,
    turn: Arc<InstructionTurn>,
    query: String,
    protocol: crate::context::instructions::InstructionProtocol,
    #[cfg(feature = "skills")]
    learned: Option<Arc<crate::extras::js::skills::session::SkillSessionServices>>,
}
impl InstructionContextHook {
    pub(crate) fn new(
        base: String,
        turn: Arc<InstructionTurn>,
        query: String,
        protocol: crate::context::instructions::InstructionProtocol,
        #[cfg(feature = "skills")] learned: Option<
            Arc<crate::extras::js::skills::session::SkillSessionServices>,
        >,
    ) -> Self {
        Self {
            base,
            turn,
            query,
            protocol,
            #[cfg(feature = "skills")]
            learned,
        }
    }
}
#[derive(Clone)]
struct PreparedInstruction {
    digest: Vec<u8>,
    bytes: usize,
    selected: bool,
}
impl<M: rig::completion::CompletionModel> rig::agent::AgentHook<M> for InstructionContextHook {
    async fn on_event(
        &self,
        ctx: &rig::agent::HookContext,
        event: rig::agent::StepEvent<'_, M>,
    ) -> rig::agent::Flow {
        use sha2::{Digest, Sha256};
        if let rig::agent::StepEvent::ToolResult {
            tool_id,
            tool_name,
            internal_call_id,
            tool_call_id,
            result,
            outcome,
            extensions,
            ..
        } = &event
        {
            let native = self.turn.native.load(Ordering::Acquire);
            if native {
                ctx.scratchpad().update(|delivery: &mut NativeDelivery| {
                    delivery
                        .0
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .receipts
                        .retain(|old| old.id != *tool_id);
                });
            }
            if *tool_name == "skills_search"
                && let Some(candidate) = extensions.get::<SearchCandidate>()
                && Arc::ptr_eq(&candidate.turn, &self.turn)
            {
                let intact = matches!(outcome, rig::tool::ToolOutcome::Success)
                    && *result == candidate.output;
                if native {
                    let accepted = ctx.scratchpad().update(|delivery: &mut NativeDelivery| {
                        let mut state = delivery.0.lock().unwrap_or_else(|e| e.into_inner());
                        if !intact {
                            state.failed = true;
                            return true;
                        }
                        if state.receipts.len() >= 16
                            || [*tool_id, *internal_call_id]
                                .iter()
                                .any(|id| id.is_empty() || id.len() > 1024)
                            || tool_call_id.is_some_and(|id| id.len() > 1024)
                        {
                            return false;
                        }
                        state.receipts.push(NativeSkillReceipt {
                            id: (*tool_id).into(),
                            internal_id: (*internal_call_id).into(),
                            call_id: tool_call_id.map(str::to_owned),
                            output: candidate.output.clone(),
                        });
                        true
                    });
                    if !accepted {
                        return rig::agent::Flow::terminate("native skill receipt limit reached");
                    }
                }
                if intact {
                    #[cfg(feature = "skills")]
                    if let Some(learned) = &candidate.learned
                        && !learned.publish(&self.turn)
                    {
                        return rig::agent::Flow::terminate(
                            "instruction search belongs to a retired turn",
                        );
                    }
                    self.turn
                        .publish_candidate(candidate.prepared.clone())
                        .await;
                }
            }
            return rig::agent::Flow::cont();
        }
        if let rig::agent::StepEvent::CompletionRequestPrepared { request, .. } = &event {
            if let Some(expected) = ctx.scratchpad().get::<PreparedInstruction>()
                && expected.selected
                && (self
                    .protocol
                    .extra_body_overrides(request.additional_params.as_ref())
                    || !request.chat_history.iter().next().is_some_and(|message| {
                        if let rig::message::Message::System { content } = message {
                            content.get(..expected.bytes).is_some_and(|prefix| {
                                Sha256::digest(prefix.as_bytes()).as_slice() == expected.digest
                            })
                        } else {
                            false
                        }
                    }))
            {
                return rig::agent::Flow::terminate(
                    "Skill context is incompatible with the final prepared transcript/system override",
                );
            }
            if let Some(delivery) = ctx.scratchpad().get::<NativeDelivery>() {
                let state = delivery.0.lock().unwrap_or_else(|e| e.into_inner());
                if state.failed
                    || state.receipts.iter().any(|receipt| {
                        !crate::context::instructions::effective_context_result_visible(
                            request,
                            "skills_search",
                            &receipt.id,
                            &receipt.internal_id,
                            receipt.call_id.as_deref(),
                            &receipt.output,
                        )
                    })
                {
                    return rig::agent::Flow::terminate(
                        "Native skill guidance was rewritten or removed; start a fresh context before retrying",
                    );
                }
            }
            return rig::agent::Flow::cont();
        }
        if !matches!(event, rig::agent::StepEvent::CompletionCall { .. }) {
            return rig::agent::Flow::cont();
        }
        let native = matches!(
            self.protocol,
            crate::context::instructions::InstructionProtocol::Anthropic
        );
        self.turn.native.store(native, Ordering::Release);
        let selection = match self.turn.prepare(&self.query).await {
            Ok(selection) => selection,
            Err(error) => return rig::agent::Flow::terminate(error),
        };
        ctx.scratchpad()
            .insert(InstructionCarrier(Arc::clone(&self.turn)));
        #[cfg(feature = "skills")]
        let block = if let Some(learned) = &self.learned {
            match learned.compose_instructions(&selection) {
                Ok(block) => block,
                Err(error) => return rig::agent::Flow::terminate(error),
            }
        } else {
            match super::selection::render_plain(&selection) {
                Ok(block) => block,
                Err(error) => return rig::agent::Flow::terminate(error),
            }
        };
        #[cfg(not(feature = "skills"))]
        let block = match super::selection::render_plain(&selection) {
            Ok(block) => block,
            Err(error) => return rig::agent::Flow::terminate(error),
        };
        let block = if native {
            if !self.base.contains(super::selection::POLICY) {
                return rig::agent::Flow::terminate(
                    "Native skill context requires its captured static instruction policy",
                );
            }
            let digest = Sha256::digest(self.base.as_bytes()).to_vec();
            let mut stable = self.turn.native_composition.lock().await;
            if let Some(original) = stable.as_ref() {
                if original.base_digest != digest {
                    return rig::agent::Flow::terminate(
                        "Native skill system prefix changed; start a fresh context before retrying",
                    );
                }
                original.block.clone()
            } else {
                *stable = Some(NativeComposition {
                    base_digest: digest,
                    block: block.clone(),
                });
                block
            }
        } else {
            block
        };
        let preamble = if block.is_empty() {
            self.base.clone()
        } else {
            format!("{}\n\n{}", self.base, block)
        };
        ctx.scratchpad().insert(PreparedInstruction {
            digest: Sha256::digest(preamble.as_bytes()).to_vec(),
            bytes: preamble.len(),
            selected: !selection.selected.is_empty(),
        });
        if block.is_empty() {
            rig::agent::Flow::cont()
        } else {
            rig::agent::Flow::patch_request(rig::agent::RequestPatch::new().preamble(preamble))
        }
    }
    fn observes(&self, event: rig::agent::StepEventKind) -> bool {
        matches!(
            event,
            rig::agent::StepEventKind::CompletionCall
                | rig::agent::StepEventKind::CompletionRequestPrepared
                | rig::agent::StepEventKind::ToolResult
        )
    }
}

#[cfg(test)]
pub(crate) struct ImmutableFixtureCleanup(pub(crate) std::path::PathBuf);
#[cfg(test)]
impl Drop for ImmutableFixtureCleanup {
    fn drop(&mut self) {
        fn remove(path: &std::path::Path) -> std::io::Result<()> {
            let metadata = match std::fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
            #[cfg(windows)]
            let linked = {
                use std::os::windows::fs::MetadataExt;
                metadata.file_attributes() & 0x400 != 0
            };
            #[cfg(not(windows))]
            let linked = metadata.file_type().is_symlink();
            if linked {
                return if metadata.is_dir() {
                    std::fs::remove_dir(path)
                } else {
                    std::fs::remove_file(path)
                };
            }
            if metadata.is_dir() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
                }
                for entry in std::fs::read_dir(path)? {
                    remove(&entry?.path())?;
                }
                std::fs::remove_dir(path)
            } else {
                #[cfg(windows)]
                if metadata.permissions().readonly() {
                    let mut permissions = metadata.permissions();
                    permissions.set_readonly(false);
                    std::fs::set_permissions(path, permissions)?;
                }
                std::fs::remove_file(path)
            }
        }
        if let Err(error) = remove(&self.0) {
            eprintln!("owned instruction fixture cleanup failed: {error}");
        }
    }
}

#[cfg(test)]
mod discovery_tests {
    use super::*;
    struct Fixture {
        services: Arc<InstructionServices>,
        _cleanup: ImmutableFixtureCleanup,
        root: crate::tests::OwnedTestDirectory,
    }
    impl Fixture {
        fn new(no_context: bool) -> Self {
            let root = crate::tests::OwnedTestDirectory::new("instruction61");
            let paths = AppPaths {
                config_dir: root.path().join("config"),
                data_dir: root.path().join("data"),
                local_data_dir: root.path().join("local"),
                state_dir: root.path().join("state"),
                cache_dir: root.path().join("cache"),
                credentials_dir: root.path().join("credentials"),
                project_dir: Some(root.path().join(".zerostack")),
            };
            crate::paths::prepare_storage_roots(&paths).unwrap();
            std::fs::create_dir_all(root.path().join(".zerostack")).unwrap();
            let config = paths.project_config_file().unwrap();
            std::fs::write(&config, "model='fixture'\n").unwrap();
            crate::config::load::trust_project_config(&config, &paths.project_config_trust_file())
                .unwrap();
            let workspace = Arc::new(WorkspaceBinding::capture(root.path()).unwrap());
            let services = Arc::new(InstructionServices {
                scope: uuid::Uuid::new_v4(),
                paths,
                workspace,
                permission: None,
                home: None,
                roots: OnceLock::new(),
                no_context_files: no_context,
                current: RwLock::new(Arc::new(Discovery::default())),
                generation: AtomicU64::new(0),
                refresh_gate: Arc::new(tokio::sync::Mutex::new(())),
            });
            Self {
                services,
                _cleanup: ImmutableFixtureCleanup(root.path().to_path_buf()),
                root,
            }
        }
        fn skill(&self, source: &str, name: &str, description: &str, body: &str) -> PathBuf {
            let package = self.root.path().join(source).join(name);
            std::fs::create_dir_all(&package).unwrap();
            let path = package.join("SKILL.md");
            std::fs::write(
                &path,
                format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
            )
            .unwrap();
            path
        }
    }

    #[cfg(feature = "skills")]
    #[tokio::test]
    async fn learned_receipt_commits_once_and_rejects_retired_turns() {
        let fixture = Fixture::new(true);
        let services = crate::extras::js::skills::session::SkillSessionServices::for_test(
            &fixture.services.paths,
        );
        let turn = InstructionTurn::new(fixture.services.clone(), None);
        services
            .prepare_instruction_prompt("initial", &turn)
            .await
            .unwrap();
        let (discovery, _) = services.search_candidate("candidate").await.unwrap();
        let pending = PendingLearned {
            services: services.clone(),
            discovery,
            committed: Mutex::new(false),
        };
        assert!(pending.publish(&turn));
        services.search("later accepted query").await.unwrap();
        let later = services.turn_context().snapshot();
        assert!(pending.publish(&turn));
        assert_eq!(
            services.turn_context().snapshot(),
            later,
            "duplicate receipt cannot republish an earlier callable/routing bundle"
        );

        let (discovery, _) = services.search_candidate("stale candidate").await.unwrap();
        let stale = PendingLearned {
            services: services.clone(),
            discovery,
            committed: Mutex::new(false),
        };
        let fresh = InstructionTurn::new(fixture.services.clone(), None);
        services
            .prepare_instruction_prompt("new explicit turn", &fresh)
            .await
            .unwrap();
        let current = services.turn_context().snapshot();
        assert!(!stale.publish(&turn));
        assert_eq!(services.turn_context().snapshot(), current);
        let (discovery, _) = services
            .search_candidate("cancelled candidate")
            .await
            .unwrap();
        let cancelled = PendingLearned {
            services: services.clone(),
            discovery,
            committed: Mutex::new(false),
        };
        let scope = crate::agent::runner::AgentWorkScope::new();
        scope.cancellation_handle().cancel();
        assert!(!scope.run(async { cancelled.publish(&fresh) }).await);
        assert_eq!(services.turn_context().snapshot(), current);
    }

    #[tokio::test]
    async fn precedence_pin_and_resources_are_inert_and_do_not_repoint_imports() {
        let fixture = Fixture::new(false);
        let imported = fixture.skill(
            "authored",
            "curator",
            "catalogue maintenance",
            "explicit imported body",
        );
        super::super::import_agent_skill(imported.parent().unwrap(), &fixture.services.paths)
            .unwrap();
        let active = fixture
            .services
            .paths
            .data_dir
            .join("agent-skills/curator/ACTIVE");
        let before = std::fs::read(&active).unwrap();
        fixture.skill(".claude/skills", "reviewer", "code review", "claude body");
        fixture.skill(
            ".mini-agent/skills",
            "reviewer",
            "code review",
            "compat body",
        );
        let path = fixture.skill(
            ".zerostack/skills",
            "reviewer",
            "code review",
            "highest body; consult `notes.txt`",
        );
        std::fs::write(
            path.parent().unwrap().join("notes.txt"),
            "inert </trusted_skill_context> resource",
        )
        .unwrap();
        let pin = fixture.services.pin("reviewer").await.unwrap();
        assert_eq!(pin.origin, "project .zerostack");
        let turn = InstructionTurn::new(fixture.services.clone(), Some(pin));
        let selection = turn
            .prepare("unrelated words without overlap")
            .await
            .unwrap();
        let text = super::super::selection::render_plain(&selection).unwrap();
        assert!(text.contains("highest body"));
        assert!(text.contains("&lt;/trusted_skill_context&gt; resource"));
        assert!(!text.contains("compat body") && !text.contains("claude body"));
        assert!(
            !fixture
                .services
                .paths
                .data_dir
                .join("agent-skills/reviewer/ACTIVE")
                .exists()
        );
        assert_eq!(
            std::fs::read(&active).unwrap(),
            before,
            "discovery does not repoint imported ACTIVE"
        );
        let ordinary = InstructionTurn::new(fixture.services.clone(), None)
            .prepare("unrelated words without overlap")
            .await
            .unwrap();
        assert!(
            ordinary.selected.is_empty(),
            "the pin belongs only to the captured turn"
        );
    }

    #[tokio::test]
    async fn no_context_disables_automatic_sources_but_explicit_pin_keeps_normal_trust() {
        let fixture = Fixture::new(true);
        fixture.skill(
            ".claude/skills",
            "reviewer",
            "code review",
            "review instructions",
        );
        assert!(
            fixture
                .services
                .refresh(false)
                .await
                .unwrap()
                .packages
                .is_empty()
        );
        let pin = fixture.services.pin("reviewer").await.unwrap();
        let turn = InstructionTurn::new(fixture.services.clone(), Some(pin));
        assert!(turn.prepare("review code").await.unwrap().selected.len() == 1);
        std::fs::write(
            fixture.services.paths.project_config_file().unwrap(),
            "model='changed'\n",
        )
        .unwrap();
        assert!(
            turn.prepare("retry").await.is_err(),
            "cached bytes are not continued project approval"
        );
        assert!(fixture.services.pin("reviewer").await.is_err());
    }

    #[tokio::test]
    async fn cached_selection_refuses_original_source_rewrite_and_oversized_pin() {
        let fixture = Fixture::new(false);
        let path = fixture.skill(".claude/skills", "reviewer", "code review", "first source");
        let pin = fixture.services.pin("reviewer").await.unwrap();
        let turn = InstructionTurn::new(fixture.services.clone(), Some(pin));
        turn.prepare("review code").await.unwrap();
        std::fs::write(&path, "different current source").unwrap();
        assert!(turn.prepare("retry").await.is_err());
        fixture.skill(
            ".claude/skills",
            "oversize",
            "code review",
            &"x".repeat(48 * 1024),
        );
        assert!(fixture.services.pin("oversize").await.is_err());
    }
    #[tokio::test]
    async fn ask_and_deny_source_read_never_prompt_or_prepare_a_pin() {
        use crate::permission::{
            Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm,
        };
        for action in [Action::Ask, Action::Deny] {
            let mut fixture = Fixture::new(false);
            fixture.skill(
                ".claude/skills",
                "reviewer",
                "code review",
                "private instruction",
            );
            let policy = PermissionConfigs::from(PermissionConfig {
                default: Some(Action::Allow),
                read: Some(ToolPerm::Simple(action)),
                ..Default::default()
            });
            let checker = crate::permission::checker::PermissionChecker::new(
                &policy,
                SecurityMode::Standard,
                Some(fixture.root.path().to_path_buf()),
                None,
            )
            .unwrap();
            Arc::get_mut(&mut fixture.services).unwrap().permission =
                Some(Arc::new(std::sync::Mutex::new(checker)));
            assert!(
                fixture
                    .services
                    .refresh(false)
                    .await
                    .unwrap()
                    .packages
                    .is_empty()
            );
            assert!(fixture.services.pin("reviewer").await.is_err());
            assert!(
                InstructionTurn::new(fixture.services.clone(), None)
                    .prepare("code review")
                    .await
                    .unwrap()
                    .selected
                    .is_empty()
            );
        }
    }

    #[test]
    fn catalogue_cache_is_scoped_to_captured_workspace_permission_and_storage_paths() {
        let fixture = Fixture::new(true);
        let owner = InstructionOwner::for_test(None);
        let first = owner.resolve(
            fixture.services.paths.clone(),
            fixture.services.workspace.clone(),
            None,
            true,
        );
        let same = owner.resolve(
            fixture.services.paths.clone(),
            fixture.services.workspace.clone(),
            None,
            true,
        );
        assert!(Arc::ptr_eq(&first, &same));
        let mut changed = fixture.services.paths.clone();
        changed.project_dir = Some(fixture.root.path().join("different-project"));
        let next = owner.resolve(changed, fixture.services.workspace.clone(), None, true);
        assert!(!Arc::ptr_eq(&first, &next));
        assert!(!first.same_authority(&next));
        let other = Arc::new(WorkspaceBinding::capture(fixture.root.path()).unwrap());
        let reopened = owner.resolve(fixture.services.paths.clone(), other, None, true);
        assert!(!Arc::ptr_eq(&first, &reopened));
    }

    #[tokio::test]
    async fn pin_budget_counts_referenced_resource_and_refuses_without_installing_selection() {
        let fixture = Fixture::new(false);
        let path = fixture.skill(
            ".claude/skills",
            "large",
            "budget detail",
            &format!("consult `notes.txt`\n{}", "x".repeat(40_000)),
        );
        std::fs::write(path.parent().unwrap().join("notes.txt"), "r".repeat(30_000)).unwrap();
        let pin = fixture.services.pin("large").await.unwrap();
        let turn = InstructionTurn::new(fixture.services.clone(), Some(pin));
        assert!(
            turn.prepare("literal user bytes")
                .await
                .unwrap_err()
                .contains("composed context budget")
        );
        assert!(turn.selected.lock().await.is_none());
    }

    #[cfg(feature = "skills")]
    #[tokio::test]
    async fn learned_compositor_uses_one_pin_and_retains_the_same_turn_during_retry() {
        let fixture = Fixture::new(false);
        fixture.skill(
            ".claude/skills",
            "reviewer",
            "code review",
            "SHARED_PIN_CONTEXT",
        );
        let pin = fixture.services.pin("reviewer").await.unwrap();
        let turn = InstructionTurn::new(fixture.services.clone(), Some(pin));
        let learned = crate::extras::js::skills::session::SkillSessionServices::for_test(
            &fixture.services.paths,
        );
        let literal = " code\n review 雪 ";
        assert_eq!(
            learned
                .prepare_instruction_prompt(literal, &turn)
                .await
                .unwrap(),
            literal
        );
        let original = learned.turn_context().snapshot().turn_id.clone();
        let selected = turn.prepare(literal).await.unwrap();
        assert_eq!(
            learned
                .compose_instructions(&selected)
                .unwrap()
                .matches("SHARED_PIN_CONTEXT")
                .count(),
            1
        );
        learned
            .prepare_instruction_prompt("automatic retry", &turn)
            .await
            .unwrap();
        assert_eq!(learned.turn_context().snapshot().turn_id, original);
        assert_eq!(
            learned
                .compose_instructions(&turn.prepare("retry").await.unwrap())
                .unwrap()
                .matches("SHARED_PIN_CONTEXT")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn all_discovery_roots_share_the_package_count_limit_and_home_is_captured() {
        let mut fixture = Fixture::new(false);
        let home = fixture.root.path().join("owned-home");
        std::fs::create_dir_all(&home).unwrap();
        Arc::get_mut(&mut fixture.services).unwrap().home = Some(home.clone());
        std::fs::create_dir_all(home.join(".claude/skills/home-skill")).unwrap();
        std::fs::write(
            home.join(".claude/skills/home-skill/SKILL.md"),
            "---\nname: home-skill\ndescription: home instructions\n---\nHOME_CAPTURED_ONLY\n",
        )
        .unwrap();
        let snapshot = fixture.services.refresh(false).await.unwrap();
        assert_eq!(snapshot.packages["home-skill"].origin, "user .claude");
        for n in 0..65 {
            fixture.skill(
                ".claude/skills",
                &format!("skill-{n:02}"),
                "bounded catalogue",
                "inert",
            );
        }
        let snapshot = fixture.services.refresh(false).await.unwrap();
        assert_eq!(snapshot.packages.len(), 64);
        assert!(
            snapshot
                .diagnostics
                .iter()
                .any(|line| line == "skill package limit exceeded")
        );
        assert!(
            !snapshot.packages.contains_key("home-skill"),
            "lower priority root cannot escape shared cap"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(unsafe_code)]
    async fn retained_discovery_rejects_fifo_links_and_source_replacement_without_blocking() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new(false);
        let fifo = fixture.root.path().join(".claude/skills/fifo");
        std::fs::create_dir_all(&fifo).unwrap();
        let path =
            std::ffi::CString::new(fifo.join("SKILL.md").as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: owned NUL-terminated fixture path; mkfifo retains no pointer.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let good = fixture.skill(
            ".claude/skills",
            "reviewer",
            "code review",
            "current instructions",
        );
        let linked = fixture.root.path().join(".claude/skills/linked");
        std::fs::create_dir(&linked).unwrap();
        symlink(&good, linked.join("SKILL.md")).unwrap();
        let hard = fixture.root.path().join(".claude/skills/hard");
        std::fs::create_dir(&hard).unwrap();
        std::fs::hard_link(&good, hard.join("SKILL.md")).unwrap();
        assert!(
            fixture
                .services
                .refresh(false)
                .await
                .unwrap()
                .packages
                .is_empty(),
            "even original shared inode must be refused"
        );
        std::fs::remove_file(hard.join("SKILL.md")).unwrap();
        let pin = fixture.services.pin("reviewer").await.unwrap();
        let turn = InstructionTurn::new(fixture.services.clone(), Some(pin));
        turn.prepare("review code").await.unwrap();
        std::fs::rename(
            good.parent().unwrap(),
            good.parent().unwrap().with_file_name("retired"),
        )
        .unwrap();
        fixture.skill(
            ".claude/skills",
            "reviewer",
            "code review",
            "current instructions",
        );
        assert!(
            turn.prepare("retry").await.is_err(),
            "same bytes at a new directory are not captured authority"
        );
    }
}
