//! Exact native mutation preimages. Display diffs and imported text are never restore data.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub(crate) const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_RECORDS: usize = 512;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Record {
    session: String,
    turn: usize,
    workspace: [u8; 32],
    path: String,
    before: Option<String>,
    after: String,
    #[serde(default)]
    retired: bool,
}
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Data {
    records: Vec<Record>,
    blobs: BTreeMap<String, usize>,
    #[serde(default)]
    skipped: BTreeMap<usize, usize>,
    #[serde(default)]
    mutations: u64,
}
#[derive(Debug)]
struct State {
    data: Data,
    trusted: bool,
    memory: BTreeMap<String, Vec<u8>>,
    /// Blob directory whose contents are already merged into `data.blobs`.
    /// Runtime-only; cleared by any create failure so the next prepare rescans.
    scanned: Option<PathBuf>,
    #[cfg(test)]
    scans: usize,
    #[cfg(feature = "subagents")]
    observation: crate::session::background_observation::ObservationStamp,
}
/// Live clones deliberately share actual filesystem observations, like TodoStore.
#[derive(Clone, Debug)]
pub(crate) struct FileCheckpoints(Arc<Mutex<State>>);
impl Default for FileCheckpoints {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(State {
            data: Data::default(),
            trusted: true,
            memory: BTreeMap::new(),
            scanned: None,
            #[cfg(test)]
            scans: 0,
            #[cfg(feature = "subagents")]
            observation: Default::default(),
        })))
    }
}
impl Serialize for FileCheckpoints {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .data
            .serialize(s)
    }
}
impl<'de> Deserialize<'de> for FileCheckpoints {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let data = Data::deserialize(d)?;
        if data.skipped.len() > MAX_RECORDS
            || data.records.len() > MAX_RECORDS
            || data.blobs.len() > MAX_RECORDS
            || data.blobs.values().any(|n| *n > MAX_FILE_BYTES)
            || data
                .blobs
                .values()
                .try_fold(0usize, |a, n| a.checked_add(*n))
                .is_none_or(|n| n > MAX_TOTAL_BYTES)
            || data.blobs.keys().any(|s| !valid_digest(s))
            || data.records.iter().any(|r| {
                r.session.len() > 256
                    || r.path.len() > 4096
                    || !relative_path(Path::new(&r.path))
                    || !valid_digest(&r.after)
                    || r.before
                        .as_deref()
                        .is_some_and(|s| !valid_digest(s) || !data.blobs.contains_key(s))
            })
        {
            return Err(serde::de::Error::custom(
                "invalid file checkpoint inventory",
            ));
        }
        Ok(Self(Arc::new(Mutex::new(State {
            data,
            trusted: false,
            memory: BTreeMap::new(),
            scanned: None,
            #[cfg(test)]
            scans: 0,
            #[cfg(feature = "subagents")]
            observation: Default::default(),
        }))))
    }
}
// The guard stamps every serialized change, including early failures during
// preimage preparation. Runtime-only blob bytes/trust flags are not serialized.
struct FileMutation<'a> {
    state: std::sync::MutexGuard<'a, State>,
    /// Rare whole-inventory edits compare against a snapshot; per-tool-call
    /// paths set `changed` explicitly through `mark` instead of cloning.
    #[cfg(feature = "subagents")]
    before: Option<Data>,
    #[cfg(feature = "subagents")]
    changed: bool,
    #[cfg(feature = "subagents")]
    observation: Option<crate::session::background_observation::JobObservation>,
}
impl std::ops::Deref for FileMutation<'_> {
    type Target = State;
    fn deref(&self) -> &State {
        &self.state
    }
}
impl std::ops::DerefMut for FileMutation<'_> {
    fn deref_mut(&mut self) -> &mut State {
        &mut self.state
    }
}
impl FileMutation<'_> {
    /// Records that serialized data changed (observation stamp must advance).
    fn mark(&mut self) {
        #[cfg(feature = "subagents")]
        {
            self.changed = true;
        }
    }
}
impl Drop for FileMutation<'_> {
    fn drop(&mut self) {
        #[cfg(feature = "subagents")]
        if self.changed
            || self
                .before
                .as_ref()
                .is_some_and(|before| *before != self.state.data)
        {
            self.state.observation.changed(self.observation.as_ref());
        }
    }
}
#[cfg(feature = "subagents")]
#[derive(Debug, Clone)]
pub(crate) struct FileObservation {
    original: FileCheckpoints,
    data: Data,
    stamp: crate::session::background_observation::ObservationStamp,
}
#[cfg(feature = "subagents")]
impl FileObservation {
    pub(crate) fn frozen(&self) -> FileCheckpoints {
        FileCheckpoints(Arc::new(Mutex::new(State {
            data: self.data.clone(),
            trusted: false,
            memory: Default::default(),
            scanned: None,
            #[cfg(test)]
            scans: 0,
            observation: Default::default(),
        })))
    }
    pub(crate) fn validates_since(
        &self,
        previous: &Self,
        owner: &Arc<crate::session::background_observation::ObservationOwner>,
    ) -> bool {
        Arc::ptr_eq(&self.original.0, &previous.original.0)
            && self.stamp.validates_since(&previous.stamp, owner)
    }
}
fn valid_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn digest(bytes: &[u8]) -> String {
    crate::hex::encode_lower(Sha256::digest(bytes))
}
fn relative_path(p: &Path) -> bool {
    !p.as_os_str().is_empty()
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}
impl FileCheckpoints {
    #[cfg(feature = "subagents")]
    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    /// Whole-inventory edits (rewind, compaction, restore) diff a snapshot.
    fn edit(&self) -> FileMutation<'_> {
        #[cfg(feature = "subagents")]
        {
            let mut mutation = self.edit_marked();
            mutation.before = Some(mutation.state.data.clone());
            mutation
        }
        #[cfg(not(feature = "subagents"))]
        self.edit_marked()
    }
    /// Per-tool-call edits call `FileMutation::mark` where data actually changes.
    fn edit_marked(&self) -> FileMutation<'_> {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        FileMutation {
            #[cfg(feature = "subagents")]
            before: None,
            #[cfg(feature = "subagents")]
            changed: false,
            state,
            #[cfg(feature = "subagents")]
            observation: None,
        }
    }
    #[cfg(feature = "subagents")]
    fn edit_observation(
        &self,
        observation: Option<&crate::session::background_observation::JobObservation>,
    ) -> FileMutation<'_> {
        let mut mutation = self.edit_marked();
        mutation.observation = observation.cloned();
        mutation
    }

    #[cfg(feature = "subagents")]
    pub(crate) fn with_observation_snapshot<T>(
        &self,
        capture: impl FnOnce(FileObservation) -> T,
    ) -> T {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        capture(FileObservation {
            original: self.clone(),
            data: state.data.clone(),
            stamp: state.observation.clone(),
        })
    }
    #[cfg(feature = "subagents")]
    pub(crate) fn bind_observation_owner(
        &self,
        owner: &Arc<crate::session::background_observation::ObservationOwner>,
    ) -> anyhow::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .observation
            .bind(owner)
    }
    pub(crate) fn revision(&self) -> u64 {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .data
            .mutations
    }
    pub(crate) fn is_empty(&self) -> bool {
        let s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        s.data.records.is_empty() && s.data.skipped.is_empty()
    }
    pub(crate) fn trust_owned_load(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).trusted = true;
    }
    pub(crate) fn recorder(&self, session: &str, turn: usize, ephemeral: bool) -> Recorder {
        Recorder {
            store: self.clone(),
            session: session.into(),
            turn: Arc::new(std::sync::atomic::AtomicUsize::new(turn)),
            ephemeral,
            #[cfg(feature = "subagents")]
            observation: None,
            #[cfg(test)]
            blob_root: None,
        }
    }
    pub(crate) fn summary_from(&self, session: &str, index: usize) -> String {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut paths = std::collections::BTreeSet::new();
        for r in &state.data.records {
            if !r.retired && r.turn >= index && r.session == session {
                paths.insert(r.path.as_str());
            }
        }
        let names = paths
            .iter()
            .take(5)
            .map(|path| format!("{path:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let missing = state.data.skipped.range(index..).next().is_some();
        format!(
            "{} files{}: {names}{}",
            paths.len(),
            if missing {
                "; unavailable checkpoints"
            } else {
                ""
            },
            if paths.len() > 5 { ", …" } else { "" }
        )
    }
    pub(crate) fn retire_from(&self, index: usize) {
        let mut state = self.edit();
        for r in &mut state.data.records {
            if r.turn >= index {
                r.retired = true;
            }
        }
        state
            .data
            .skipped
            .retain(|turn, _| *turn < index || *turn == usize::MAX);
    }
    /// The summary replaces a prefix, so surviving user rows move together
    /// with their exact checkpoints. Pruned turns cannot become later turns.
    pub(crate) fn compact_prefix(&self, removed: usize) {
        let mut state = self.edit();
        for record in &mut state.data.records {
            if record.turn < removed {
                record.retired = true;
            } else {
                record.turn = record.turn.saturating_sub(removed).saturating_add(1);
            }
        }
        state.data.skipped = std::mem::take(&mut state.data.skipped)
            .into_iter()
            .filter_map(|(turn, count)| {
                if turn == usize::MAX {
                    Some((turn, count))
                } else if turn >= removed {
                    Some((turn - removed + 1, count))
                } else {
                    None
                }
            })
            .collect();
    }
    /// Explicit operator action; all inputs remain bound to the current retained workspace.
    /// Preflight every file before the first write, then each publication checks identity+content again.
    pub(crate) fn restore(
        &self,
        session: &str,
        index: usize,
        workspace: &crate::paths::WorkspaceBinding,
    ) -> anyhow::Result<usize> {
        self.restore_with_root(session, index, workspace, None)
    }
    fn restore_with_root(
        &self,
        session: &str,
        index: usize,
        workspace: &crate::paths::WorkspaceBinding,
        root: Option<&Path>,
    ) -> anyhow::Result<usize> {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(state.trusted, "imported checkpoints cannot restore files");
        anyhow::ensure!(
            state.data.skipped.range(index..).next().is_none(),
            "some changes have no restorable checkpoint; choose conversation only"
        );
        let fingerprint = workspace
            .compatibility_fingerprint()
            .map_err(anyhow::Error::msg)?;
        let mut selected = BTreeMap::<String, Record>::new();
        for record in state
            .data
            .records
            .iter()
            .filter(|r| !r.retired && r.turn >= index && r.session == session)
        {
            anyhow::ensure!(
                record.workspace == fingerprint,
                "checkpoint belongs to another workspace identity"
            );
            if let Some(prior) = selected.get_mut(&record.path) {
                // Intervening edits outside this checkpoint chain must never be silently undone.
                anyhow::ensure!(
                    record.before.as_deref() == Some(prior.after.as_str()),
                    "checkpoint chain has an intervening change"
                );
                prior.after = record.after.clone();
            } else {
                selected.insert(record.path.clone(), record.clone());
            }
        }
        let mut prepared = Vec::new();
        for record in selected.values() {
            let path = Path::new(&record.path);
            anyhow::ensure!(
                workspace.symlink_component(path).is_none(),
                "checkpoint path now contains a symbolic link"
            );
            let mut current = workspace.open_regular_relative(path)?;
            let expected = crate::fs::checked_file_metadata(&current)?;
            let mut bytes = Vec::new();
            current
                .by_ref()
                .take(MAX_FILE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            anyhow::ensure!(
                bytes.len() <= MAX_FILE_BYTES && digest(&bytes) == record.after,
                "file changed since checkpoint: {}",
                record.path
            );
            let before = match &record.before {
                Some(hash) => Some(match state.memory.get(hash) {
                    Some(bytes) => bytes.clone(),
                    None => read_blob(
                        root.map(Path::to_path_buf)
                            .unwrap_or_else(|| blob_root(session)),
                        hash,
                    )?,
                }),
                None => None,
            };
            prepared.push((
                record.path.clone(),
                expected,
                crate::fs::ContentDigest::of(&bytes),
                before,
            ));
        }
        drop(state);
        let mut restored = 0;
        for (path, expected, base, before) in prepared {
            let result = match before {
                Some(bytes) => workspace.replace_relative_atomic_expecting(
                    Path::new(&path),
                    &bytes,
                    &expected,
                    &base,
                ),
                None => workspace.retire_relative_expecting(Path::new(&path), &expected, &base),
            };
            if let Err(error) = result {
                anyhow::bail!("restored {restored} files; stopped at {path}: {error}");
            }
            restored += 1;
            for record in &mut self.edit().data.records {
                if record.session == session && record.turn >= index && record.path == path {
                    record.retired = true;
                }
            }
        }
        Ok(restored)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Recorder {
    store: FileCheckpoints,
    session: String,
    turn: Arc<std::sync::atomic::AtomicUsize>,
    ephemeral: bool,
    #[cfg(feature = "subagents")]
    observation: Option<crate::session::background_observation::JobObservation>,
    #[cfg(test)]
    blob_root: Option<PathBuf>,
}
impl Recorder {
    #[cfg(feature = "subagents")]
    pub(crate) fn belongs_to_store(&self, session: &str, store: &FileCheckpoints) -> bool {
        self.session == session && self.store.same_owner(store)
    }
    #[cfg(feature = "subagents")]
    pub(crate) fn for_background(
        &self,
        observation: crate::session::background_observation::JobObservation,
    ) -> Self {
        Self {
            // This launch row stays fixed even when foreground steering rebases
            // the original recorder before subsequent foreground effects.
            turn: Arc::new(std::sync::atomic::AtomicUsize::new(self.turn())),
            observation: Some(observation),
            ..self.clone()
        }
    }
    fn edit_store(&self) -> FileMutation<'_> {
        #[cfg(feature = "subagents")]
        {
            self.store.edit_observation(self.observation.as_ref())
        }
        #[cfg(not(feature = "subagents"))]
        {
            self.store.edit_marked()
        }
    }
    fn turn(&self) -> usize {
        self.turn.load(std::sync::atomic::Ordering::Relaxed)
    }
    // Clones captured by the actor share the exact UI-acknowledged turn anchor.
    pub(crate) fn rebase_turn(&self, turn: usize) {
        self.turn.store(turn, std::sync::atomic::Ordering::Relaxed);
    }
}
/// Runtime-only acknowledgement: the SDK cannot poll the next completion until
/// the same live Session has appended the injected user row. Clones share one
/// consuming claim; imported/model-visible data cannot construct this token.
#[derive(Clone, Debug)]
pub struct SteeringHandoff(Arc<Mutex<Option<PendingSteeringHandoff>>>);
#[derive(Debug)]
struct PendingSteeringHandoff {
    recorder: Recorder,
    acknowledgement: tokio::sync::oneshot::Sender<()>,
}
impl SteeringHandoff {
    pub(crate) fn acknowledge(self, session: &super::Session, turn: usize) -> anyhow::Result<()> {
        let PendingSteeringHandoff {
            recorder,
            acknowledgement,
        } = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| anyhow::anyhow!("steering checkpoint already settled"))?;
        anyhow::ensure!(
            recorder.session == session.id.as_str()
                && Arc::ptr_eq(&recorder.store.0, &session.file_checkpoints.0)
                && session
                    .messages
                    .get(turn)
                    .is_some_and(|message| message.role == super::MessageRole::User),
            "steering checkpoint session changed"
        );
        recorder.rebase_turn(turn);
        acknowledgement
            .send(())
            .map_err(|_| anyhow::anyhow!("steering run already retired"))
    }
}
pub(crate) fn steering_handoff() -> (
    Option<SteeringHandoff>,
    Option<tokio::sync::oneshot::Receiver<()>>,
) {
    let Some(recorder) = current() else {
        return (None, None);
    };
    let (sender, receiver) = tokio::sync::oneshot::channel();
    (
        Some(SteeringHandoff(Arc::new(Mutex::new(Some(
            PendingSteeringHandoff {
                recorder,
                acknowledgement: sender,
            },
        ))))),
        Some(receiver),
    )
}

#[cfg(any(feature = "hooks", feature = "memory", feature = "subagents"))]
pub(crate) fn inserted_context(rows: usize) {
    if let Some(recorder) = current() {
        let _ = recorder.turn.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |turn| Some(turn.saturating_add(rows)),
        );
    }
}

tokio::task_local! { static CURRENT: Option<Recorder>; }
pub(crate) fn current() -> Option<Recorder> {
    CURRENT.try_with(Clone::clone).ok().flatten()
}
pub(crate) fn scope<F: std::future::Future>(
    recorder: Option<Recorder>,
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    CURRENT.scope(recorder, future)
}
pub(crate) fn skipped() {
    if let Some(recorder) = current() {
        let mut state = recorder.edit_store();
        skip(&mut state, recorder.turn());
        state.mark();
    }
}
pub(crate) fn published() {
    if let Some(recorder) = current() {
        let mut state = recorder.edit_store();
        state.data.mutations = state.data.mutations.wrapping_add(1);
        state.mark();
    }
}

fn blob_root(session: &str) -> PathBuf {
    super::storage::tool_output_dir(session).join("checkpoints")
}
// Include orphaned preimages from interrupted preparations in the physical
// budget. Reloading a session must not reset its on-disk storage ceiling.
fn existing_blobs(root: &Path) -> anyhow::Result<BTreeMap<String, usize>> {
    let binding = crate::paths::WorkspaceBinding::capture(root)?;
    binding.validate_private_root()?;
    let directory = binding.open_dir_relative(Path::new(""))?;
    let mut blobs = BTreeMap::new();
    let mut total = 0usize;
    for (index, entry) in directory.read_dir(".")?.enumerate() {
        anyhow::ensure!(index < MAX_RECORDS, "checkpoint directory entry limit");
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid checkpoint name"))?;
        let hash = name
            .strip_suffix(".bin")
            .filter(|hash| valid_digest(hash))
            .ok_or_else(|| anyhow::anyhow!("invalid checkpoint name"))?;
        let metadata = directory.symlink_metadata(name)?;
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "checkpoint entry is not regular"
        );
        let file = binding.open_regular_relative(Path::new(name))?;
        crate::fs::validate_private_file(&file)?;
        let size = usize::try_from(file.metadata()?.len())?;
        total = total
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("checkpoint byte limit"))?;
        anyhow::ensure!(
            size <= MAX_FILE_BYTES && total <= MAX_TOTAL_BYTES,
            "checkpoint byte limit"
        );
        blobs.insert(hash.into(), size);
    }
    binding.validate_private_root()?;
    Ok(blobs)
}

fn read_blob(root: PathBuf, hash: &str) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(valid_digest(hash), "invalid checkpoint digest");
    let binding = crate::paths::WorkspaceBinding::capture(&root)?;
    binding.validate_private_root()?;
    let mut file = binding.open_regular_relative(Path::new(&format!("{hash}.bin")))?;
    crate::fs::validate_private_file(&file)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX_FILE_BYTES && digest(&bytes) == hash,
        "checkpoint blob changed"
    );
    crate::fs::validate_private_file(&file)?;
    binding.validate_private_root()?;
    Ok(bytes)
}
/// Held before CAS, then committed immediately after publication, before any optional await.
pub(crate) struct Pending {
    recorder: Recorder,
    record: Record,
    recorded_index: Option<usize>,
}
/// Preimage bytes with their precomputed lowercase SHA-256 hex.
pub(crate) struct Preimage {
    bytes: Arc<[u8]>,
    hash: String,
}
impl Preimage {
    /// Takes shared bytes so a caller that also keeps the preimage for a diff
    /// preview does not hold a second copy.
    pub(crate) fn new(bytes: impl Into<Arc<[u8]>>, digest: &crate::fs::ContentDigest) -> Self {
        Self {
            bytes: bytes.into(),
            hash: digest.hex(),
        }
    }
}
/// Runs the filesystem work (blob create with durable fsyncs) on the blocking
/// pool. The task-local recorder is captured here and moved in by value.
pub(crate) async fn prepare(
    workspace: &Arc<crate::paths::WorkspaceBinding>,
    path: &Path,
    before: Option<Preimage>,
    read_allowed: bool,
) -> Option<Pending> {
    let recorder = current()?;
    let workspace = Arc::clone(workspace);
    let path = path.to_path_buf();
    let task = crate::agent::runner::spawn_blocking_scoped({
        let recorder = recorder.clone();
        move || {
            prepare_with(
                recorder,
                &workspace,
                &path,
                before.as_ref().map(|p| (&*p.bytes, p.hash.as_str())),
                read_allowed,
            )
        }
    });
    match task.await {
        Ok(pending) => pending,
        Err(_) => {
            let mut state = recorder.edit_store();
            skip(&mut state, recorder.turn());
            state.mark();
            None
        }
    }
}
#[cfg(test)]
fn prepare_sync(
    workspace: &crate::paths::WorkspaceBinding,
    path: &Path,
    before: Option<&[u8]>,
    read_allowed: bool,
) -> Option<Pending> {
    let recorder = current()?;
    let hash = before.map(digest);
    prepare_with(
        recorder,
        workspace,
        path,
        before.zip(hash.as_deref()),
        read_allowed,
    )
}
fn prepare_with(
    recorder: Recorder,
    workspace: &crate::paths::WorkspaceBinding,
    path: &Path,
    before: Option<(&[u8], &str)>,
    read_allowed: bool,
) -> Option<Pending> {
    let mut state = recorder.edit_store();
    let make = (|| -> anyhow::Result<Record> {
        anyhow::ensure!(
            read_allowed && state.data.records.len() < MAX_RECORDS,
            "checkpoint limit or read policy"
        );
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::Normal(name) => normalized.push(name),
                std::path::Component::CurDir => {}
                _ => anyhow::bail!("checkpoint path is not contained"),
            }
        }
        let path = normalized
            .to_str()
            .filter(|s| s.len() <= 4096 && relative_path(Path::new(s)))
            .ok_or_else(|| anyhow::anyhow!("checkpoint path is not contained UTF-8"))?;
        let hash = before.map(|(_, hash)| hash.to_owned());
        if let Some((bytes, hash)) = before {
            anyhow::ensure!(bytes.len() <= MAX_FILE_BYTES, "checkpoint file limit");
            if !state.data.blobs.contains_key(hash) {
                anyhow::ensure!(
                    state.data.blobs.len() < MAX_RECORDS
                        && state
                            .data
                            .blobs
                            .values()
                            .sum::<usize>()
                            .saturating_add(bytes.len())
                            <= MAX_TOTAL_BYTES,
                    "checkpoint session byte limit"
                );
                if recorder.ephemeral {
                    state.memory.insert(hash.to_owned(), bytes.to_vec());
                } else {
                    #[cfg(test)]
                    let root = recorder
                        .blob_root
                        .clone()
                        .unwrap_or_else(|| blob_root(&recorder.session));
                    #[cfg(not(test))]
                    let root = blob_root(&recorder.session);
                    // The directory is scanned once per store and root; afterwards
                    // `data.blobs` is the authoritative inventory (every create
                    // inserts into it). A create failure clears the marker so the
                    // next call rescans the physical directory.
                    if state.scanned.as_deref() != Some(root.as_path()) {
                        crate::paths::ensure_private_directory(&root)?;
                        #[cfg(test)]
                        {
                            state.scans += 1;
                        }
                        let existing = existing_blobs(&root)?;
                        anyhow::ensure!(
                            existing.contains_key(hash)
                                || (existing.len() < MAX_RECORDS
                                    && existing
                                        .values()
                                        .sum::<usize>()
                                        .saturating_add(bytes.len())
                                        <= MAX_TOTAL_BYTES),
                            "physical checkpoint byte limit"
                        );
                        let before_len = state.data.blobs.len();
                        state.data.blobs.extend(existing);
                        if state.data.blobs.len() != before_len {
                            state.mark();
                        }
                        state.scanned = Some(root.clone());
                    }
                    if !state.data.blobs.contains_key(hash) {
                        match crate::fs::private_atomic_create_sync(
                            &root.join(format!("{hash}.bin")),
                            bytes,
                        ) {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                                state.scanned = None;
                                anyhow::ensure!(
                                    read_blob(root, hash)? == bytes,
                                    "checkpoint blob mismatch"
                                );
                            }
                            Err(e) => {
                                state.scanned = None;
                                return Err(e.into());
                            }
                        }
                    }
                }
                state.data.blobs.insert(hash.to_owned(), bytes.len());
                state.mark();
            }
        }
        Ok(Record {
            session: recorder.session.clone(),
            turn: recorder.turn(),
            workspace: workspace
                .compatibility_fingerprint()
                .map_err(anyhow::Error::msg)?,
            path: path.into(),
            before: hash,
            after: String::new(),
            retired: false,
        })
    })();
    match make {
        Ok(record) => {
            drop(state);
            Some(Pending {
                recorder,
                record,
                recorded_index: None,
            })
        }
        Err(_) => {
            skip(&mut state, recorder.turn());
            state.mark();
            None
        }
    }
}
impl Pending {
    pub(crate) fn committed(&mut self, after: &crate::fs::ContentDigest, after_len: usize) {
        self.record.after = after.hex();
        let mut state = self.recorder.edit_store();
        if state.data.records.len() >= MAX_RECORDS || after_len > MAX_FILE_BYTES {
            skip(&mut state, self.record.turn);
            state.mark();
            return;
        }
        self.recorded_index = Some(state.data.records.len());
        state.data.records.push(self.record.clone());
        state.mark();
    }
    pub(crate) fn formatted(&self, after: Option<&str>) {
        let mut state = self.recorder.edit_store();
        let mut changed = false;
        if let Some(record) = self
            .recorded_index
            .and_then(|index| state.data.records.get_mut(index))
        {
            // Missing authorized final bytes leaves the original posthash; restore safely conflicts.
            if let Some(after) = after {
                let after = digest(after.as_bytes());
                if record.after != after {
                    record.after = after;
                    changed = true;
                }
            }
        }
        if changed {
            state.mark();
        }
    }
}

fn skip(state: &mut State, turn: usize) {
    let key = if state.data.skipped.len() >= MAX_RECORDS && !state.data.skipped.contains_key(&turn)
    {
        state.data.skipped.pop_last();
        usize::MAX
    } else {
        turn
    };
    let count = state.data.skipped.entry(key).or_default();
    *count = count.saturating_add(1);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::agent::tools::{EditArgs, ReadArgs, ReadTracker, WriteArgs};
    use rig::tool::Tool;

    async fn overwrite(workspace: &Arc<crate::paths::WorkspaceBinding>, path: &str, text: &str) {
        let tracker = ReadTracker::new(true);
        crate::agent::tools::read::ReadTool::new_with_tracker(
            None,
            None,
            None,
            100,
            tracker.clone(),
        )
        .with_workspace(workspace.root())
        .call(ReadArgs {
            raw_notebook: false,
            path: path.into(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();
        crate::agent::tools::write::WriteTool::new_with_tracker(None, None, None, tracker)
            .with_workspace_binding(workspace.clone())
            .with_diff_options(crate::agent::tools::mutation_diff::DiffOptions {
                context: 0,
                max_lines: 0,
            })
            .call(WriteArgs {
                path: path.into(),
                content: text.into(),
                overwrite: true,
            })
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn rewind35_actual_write_edit_chain_and_creation_restore_exact_files_after_failure() {
        let root = crate::tests::OwnedTestDirectory::new("rewind35-native");
        std::fs::write(root.path().join("old.txt"), "original α\r\n").unwrap();
        std::fs::write(root.path().join("unrelated"), "operator").unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let store = FileCheckpoints::default();
        let failed:Result<(),&str>=scope(Some(store.recorder("session",0,true)),async {
            overwrite(&workspace,"old.txt","middle🙂\n").await;
            let edit=crate::agent::tools::edit::EditTool::new(None,None).with_workspace(root.path());
            edit.call(serde_json::from_value::<EditArgs>(serde_json::json!({"path":"old.txt","block":"<<<<<<< SEARCH\nmiddle🙂\n=======\nfinal\n>>>>>>> REPLACE"})).unwrap()).await.unwrap();
            crate::agent::tools::write::WriteTool::new(None,None,None).with_workspace(root.path())
                .call(WriteArgs{path:"new.txt".into(),content:"created".into(),overwrite:false}).await.unwrap();
            Err("caller stopped after committed effects")
        }).await;
        assert!(failed.is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("old.txt")).unwrap(),
            "final\n"
        );
        assert!(store.summary_from("session", 0).contains("2 files"));
        assert_eq!(store.restore("session", 0, &workspace).unwrap(), 2);
        assert_eq!(
            std::fs::read(root.path().join("old.txt")).unwrap(),
            "original α\r\n".as_bytes()
        );
        assert!(!root.path().join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.path().join("unrelated")).unwrap(),
            "operator"
        );
        assert_eq!(
            store.restore("session", 0, &workspace).unwrap(),
            0,
            "retired files are not applied twice"
        );
    }
    #[tokio::test]
    async fn rewind35_checks_all_postimages_before_any_restore_and_preserves_editor_changes() {
        let root = crate::tests::OwnedTestDirectory::new("rewind35-conflict");
        for path in ["a", "z"] {
            std::fs::write(root.path().join(path), "old").unwrap();
        }
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let store = FileCheckpoints::default();
        scope(Some(store.recorder("session", 4, true)), async {
            overwrite(&workspace, "a", "new").await;
            overwrite(&workspace, "z", "new").await;
        })
        .await;
        std::fs::write(root.path().join("z"), "editor").unwrap();
        let error = store.restore("session", 4, &workspace).unwrap_err();
        assert!(error.to_string().contains("changed since checkpoint"));
        assert_eq!(
            std::fs::read_to_string(root.path().join("a")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("z")).unwrap(),
            "editor"
        );
        // A symlink replacement is never followed to undo a different file.
        std::fs::remove_file(root.path().join("z")).unwrap();
        std::os::unix::fs::symlink("a", root.path().join("z")).unwrap();
        assert!(store.restore("session", 4, &workspace).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("a")).unwrap(),
            "new"
        );
    }
    #[tokio::test]
    async fn rewind35_private_saved_reload_restores_but_import_fork_and_corrupt_blob_do_not() {
        let root = crate::tests::OwnedTestDirectory::new("rewind35-storage");
        let _env = crate::tests::ScopedProcessEnv::set(&[
            (
                "MINI_AGENT_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "MINI_AGENT_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "MINI_AGENT_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ]);
        let work = root.path().join("workspace");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join("file"), "private before").unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&work).unwrap());
        let mut session = crate::session::Session::new("openrouter", "test", 1000, "");
        session.working_dir = work.to_string_lossy().as_ref().into();
        session.add_message(crate::session::MessageRole::User, "change file");
        scope(
            Some(session.file_checkpoints.recorder(&session.id, 0, false)),
            overwrite(&workspace, "file", "after"),
        )
        .await;
        super::super::storage::save_session(&session).unwrap();
        let raw = serde_json::to_string(&session.file_checkpoints).unwrap();
        let imported: FileCheckpoints = serde_json::from_str(&raw).unwrap();
        assert!(
            imported
                .restore(&session.id, 0, &workspace)
                .unwrap_err()
                .to_string()
                .contains("imported")
        );
        let fork = super::super::lock::fork_session(&session).unwrap();
        assert!(fork.file_checkpoints.is_empty());
        let loaded = super::super::storage::load_session_exact(&session.id)
            .unwrap()
            .unwrap();
        let hash = digest(b"private before");
        let path = blob_root(&session.id).join(format!("{hash}.bin"));
        assert_eq!(std::fs::read(&path).unwrap(), b"private before");
        std::fs::write(&path, b"tampered").unwrap();
        assert!(
            loaded
                .file_checkpoints
                .restore(&loaded.id, 0, &workspace)
                .unwrap_err()
                .to_string()
                .contains("blob changed")
        );
        assert_eq!(std::fs::read_to_string(work.join("file")).unwrap(), "after");
        std::fs::write(&path, b"private before").unwrap();
        assert_eq!(
            loaded
                .file_checkpoints
                .restore(&loaded.id, 0, &workspace)
                .unwrap(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(work.join("file")).unwrap(),
            "private before"
        );
        crate::session::lock::release_session(&session.id);
    }
    #[tokio::test]
    async fn rewind35_read_policy_limits_and_replaced_workspace_fail_closed() {
        let root = crate::tests::OwnedTestDirectory::new("rewind35-bounds");
        let work = root.path().join("workspace");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join("file"), "old").unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&work).unwrap());
        let denied = FileCheckpoints::default();
        scope(Some(denied.recorder("session", 0, true)), async {
            assert!(prepare_sync(&workspace, Path::new("file"), Some(b"old"), false).is_none());
        })
        .await;
        assert!(
            denied
                .restore("session", 0, &workspace)
                .unwrap_err()
                .to_string()
                .contains("no restorable checkpoint")
        );
        let large = vec![b'x'; MAX_FILE_BYTES + 1];
        let bounded = FileCheckpoints::default();
        scope(Some(bounded.recorder("session", 0, true)), async {
            assert!(prepare_sync(&workspace, Path::new("file"), Some(&large), true).is_none());
        })
        .await;
        assert!(bounded.restore("session", 0, &workspace).is_err());
        let store = FileCheckpoints::default();
        scope(
            Some(store.recorder("session", 0, true)),
            overwrite(&workspace, "file", "new"),
        )
        .await;
        std::fs::rename(&work, root.path().join("old-workspace")).unwrap();
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join("file"), "new").unwrap();
        assert!(store.restore("session", 0, &workspace).is_err());
        assert_eq!(std::fs::read_to_string(work.join("file")).unwrap(), "new");
        assert_eq!(
            std::fs::read_to_string(root.path().join("old-workspace/file")).unwrap(),
            "new"
        );
    }
    #[tokio::test]
    async fn rewind35_actual_sdk_dispatch_keeps_private_checkpoint_without_display_diff() {
        use futures::StreamExt;
        use rig::{
            agent::AgentBuilder,
            streaming::StreamingChat,
            test_utils::{MockCompletionModel, MockStreamEvent},
        };
        let root = crate::tests::OwnedTestDirectory::new("rewind35-sdk");
        std::fs::write(root.path().join("file"), "PRIVATE_PREIMAGE").unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let tracker = ReadTracker::new(true);
        crate::agent::tools::read::ReadTool::new_with_tracker(
            None,
            None,
            None,
            100,
            tracker.clone(),
        )
        .with_workspace(root.path())
        .call(ReadArgs {
            raw_notebook: false,
            path: "file".into(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();
        let write =
            crate::agent::tools::write::WriteTool::new_with_tracker(None, None, None, tracker)
                .with_workspace(root.path())
                .with_diff_options(crate::agent::tools::mutation_diff::DiffOptions {
                    context: 0,
                    max_lines: 0,
                });
        let tools = crate::agent::tools::concurrency::bind(
            crate::agent::tools::memoize::definitions(vec![Box::new(write)]),
        );
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call(
                    "write-current",
                    "write",
                    serde_json::json!({"path":"file","content":"actual published","overwrite":true}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model).tools(tools).build();
        let session = crate::session::Session::new("openrouter", "test", 1000, "");
        let pending = crate::ui::state::PendingMainTurn::capture(&session, "edit");
        let store = session.file_checkpoints.clone();
        let recorder = store.recorder("session", 0, true);
        let captured_before_rebase = recorder.clone();
        recorder.rebase_turn(1); // Owned SessionStart context precedes this user.
        scope(Some(captured_before_rebase), async {
            #[cfg(feature = "hooks")]
            inserted_context(1); // UserPromptSubmit adds a second System row.
            let mut stream = agent
                .stream_chat("edit", Vec::<rig::completion::Message>::new())
                .max_turns(2)
                .await;
            while let Some(event) = stream.next().await {
                event.unwrap();
            }
        })
        .await;
        assert_eq!(
            std::fs::read_to_string(root.path().join("file")).unwrap(),
            "actual published"
        );
        assert!(
            pending.has_progress(&session, "", &[]),
            "a committed file is progress even before any transcript event is consumed"
        );
        let user_index = if cfg!(feature = "hooks") { 2 } else { 1 };
        assert_eq!(store.restore("session", user_index, &workspace).unwrap(), 1);
        assert_eq!(
            std::fs::read_to_string(root.path().join("file")).unwrap(),
            "PRIVATE_PREIMAGE"
        );
    }
    #[tokio::test]
    async fn rewind35_actual_steering_ack_anchors_later_write_and_refuses_stale_or_cancelled_turn()
    {
        use crate::agent::runner::{AgentWorkScope, spawn_agent_paused_in_scope};
        use crate::{
            event::AgentEvent,
            session::{MessageRole, Session},
        };
        use futures::FutureExt;
        use rig::{
            agent::AgentBuilder,
            test_utils::{MockCompletionModel, MockStreamEvent},
        };
        for case in 0..4 {
            let root = crate::tests::OwnedTestDirectory::new("rewind35-steering");
            let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
            let mut session = Session::new("openrouter", "test", 1000, "");
            session.add_message(MessageRole::User, "original");
            let recorder = session.file_checkpoints.recorder(&session.id, 0, true);
            let model = MockCompletionModel::from_stream_turns(vec![
                vec![
                    MockStreamEvent::tool_call(
                        "before",
                        "write",
                        serde_json::json!({"path":"before","content":"original turn"}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
                vec![
                    MockStreamEvent::tool_call(
                        "after",
                        "write",
                        serde_json::json!({"path":"after","content":"steered turn"}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
                vec![
                    MockStreamEvent::text("done"),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
            ]);
            let write = crate::agent::tools::write::WriteTool::new_with_tracker(
                None,
                None,
                None,
                ReadTracker::new(true),
            )
            .with_workspace_binding(workspace.clone());
            let agent = AgentBuilder::new(model.clone())
                .tool(write)
                .default_max_turns(3)
                .build();
            let work = AgentWorkScope::new();
            let paused = scope(Some(recorder.clone()), async {
                spawn_agent_paused_in_scope(
                    agent,
                    "original".into(),
                    Vec::new().into(),
                    crate::retry::RetryConfig::default(),
                    None,
                    #[cfg(feature = "skills")]
                    None,
                    #[cfg(feature = "hooks")]
                    None,
                    None,
                    work.clone(),
                )
            })
            .await;
            let cancellation = paused.cancellation_handle();
            let mut runner = paused.start();
            runner.steering.enable();
            runner.steering.submit("steering user").unwrap();
            let result = std::panic::AssertUnwindSafe(async {
                let (messages, checkpoint) =
                    tokio::time::timeout(std::time::Duration::from_secs(3), async {
                        loop {
                            match runner.event_rx.recv().await.expect("owned consumed event") {
                                AgentEvent::SteerConsumed {
                                    messages,
                                    checkpoint,
                                } => break (messages, checkpoint.expect("captured recorder")),
                                AgentEvent::Error { message, .. } => panic!("{message}"),
                                _ => {}
                            }
                        }
                    })
                    .await
                    .expect("SDK reaches steering boundary");
                assert_eq!(
                    messages,
                    vec![rig::completion::Message::user("steering user")]
                );
                assert_eq!(
                    std::fs::read_to_string(root.path().join("before")).unwrap(),
                    "original turn"
                );
                // A live, deliberately held acknowledgement cannot be passed by
                // queueing the next HTTP or by executing its actual native tool.
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(30), async {
                        while let Some(event) = runner.event_rx.recv().await {
                            assert!(!matches!(
                                event,
                                AgentEvent::Done { .. } | AgentEvent::Error { .. }
                            ));
                        }
                    })
                    .await
                    .is_err()
                );
                assert_eq!(model.requests().len(), 1);
                assert!(!root.path().join("after").exists());
                session.add_message(MessageRole::Assistant, "settled original segment");
                let steer_index = session.messages.len();
                session.add_message(MessageRole::User, "steering user");
                let duplicate = checkpoint.clone();
                match case {
                    0 => {
                        checkpoint.acknowledge(&session, steer_index).unwrap();
                        assert!(duplicate.acknowledge(&session, 0).is_err());
                    }
                    1 => {
                        let foreign = Session::new("openrouter", "test", 1000, "");
                        assert!(checkpoint.acknowledge(&foreign, 0).is_err());
                        drop(duplicate);
                    }
                    2 => {
                        drop(checkpoint);
                        drop(duplicate);
                    }
                    _ => {
                        cancellation.cancel();
                        drop(checkpoint);
                        drop(duplicate);
                    }
                }
                let mut done = false;
                while let Some(event) = runner.event_rx.recv().await {
                    done |= matches!(event, AgentEvent::Done { .. });
                }
                if case == 0 {
                    assert!(done);
                    assert_eq!(recorder.turn(), steer_index);
                    assert_eq!(model.requests().len(), 3);
                    assert_eq!(
                        std::fs::read_to_string(root.path().join("after")).unwrap(),
                        "steered turn"
                    );
                    assert_eq!(
                        session
                            .file_checkpoints
                            .restore(&session.id, steer_index, &workspace)
                            .unwrap(),
                        1
                    );
                    assert!(!root.path().join("after").exists());
                    assert_eq!(
                        std::fs::read_to_string(root.path().join("before")).unwrap(),
                        "original turn"
                    );
                } else {
                    assert!(!done);
                    assert_eq!(model.requests().len(), 1);
                    assert!(!root.path().join("after").exists());
                    assert_eq!(recorder.turn(), 0);
                }
            })
            .catch_unwind();
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), result).await;
            cancellation.cancel();
            runner.abort_handle.abort();
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while runner.event_rx.recv().await.is_some() {}
                work.wait_idle().await;
            })
            .await
            .expect("all owned work retires before fixture deletion");
            match result {
                Ok(Ok(())) => {}
                Ok(Err(panic)) => std::panic::resume_unwind(panic),
                Err(error) => panic!("{error}"),
            }
        }
    }

    #[tokio::test]
    async fn rewind35_actual_compaction_remaps_kept_turn_without_restoring_pruned_edits() {
        let root = crate::tests::OwnedTestDirectory::new("rewind35-compaction");
        for path in ["old-turn", "kept-turn"] {
            std::fs::write(root.path().join(path), "original").unwrap();
        }
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut session = crate::session::Session::new("openrouter", "test", 1000, "");
        for content in ["oldest", "pruned", "kept"] {
            session.add_message(crate::session::MessageRole::User, content);
            let index = session.messages.len() - 1;
            if content != "oldest" {
                scope(
                    Some(session.file_checkpoints.recorder(&session.id, index, true)),
                    async {
                        overwrite(
                            &workspace,
                            if content == "pruned" {
                                "old-turn"
                            } else {
                                "kept-turn"
                            },
                            "modified",
                        )
                        .await;
                    },
                )
                .await;
            }
            session.add_message(crate::session::MessageRole::Assistant, "done");
        }
        session.compress("summary".into(), 4, 0);
        assert_eq!(session.messages[1].content, "kept");
        assert_eq!(
            session
                .file_checkpoints
                .restore(&session.id, 1, &workspace)
                .unwrap(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("old-turn")).unwrap(),
            "modified"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("kept-turn")).unwrap(),
            "original"
        );
    }
    #[tokio::test]
    async fn rewind35_orphaned_preimages_keep_physical_budget_and_private_links_fail_closed() {
        let root = crate::tests::OwnedTestDirectory::new("rewind35-orphans");
        let blobs = root.path().join("blobs");
        crate::paths::ensure_private_directory(&blobs).unwrap();
        for value in 0..16u8 {
            let bytes = vec![value; MAX_FILE_BYTES];
            let name = digest(&bytes);
            crate::fs::private_atomic_create_sync(&blobs.join(format!("{name}.bin")), &bytes)
                .unwrap();
        }
        let workspace = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let store = FileCheckpoints::default();
        let mut recorder = store.recorder("session", 1, false);
        recorder.blob_root = Some(blobs.clone());
        scope(Some(recorder), async {
            assert!(
                prepare_sync(&workspace, Path::new("file"), Some(b"one extra byte"), true)
                    .is_none()
            );
        })
        .await;
        assert_eq!(std::fs::read_dir(&blobs).unwrap().count(), 16);
        assert!(store.restore("session", 1, &workspace).is_err());
        let links = root.path().join("links");
        crate::paths::ensure_private_directory(&links).unwrap();
        let name = format!("{}.bin", digest(b"secret"));
        crate::fs::private_atomic_create_sync(&links.join(&name), b"secret").unwrap();
        std::fs::hard_link(links.join(&name), root.path().join("alias")).unwrap();
        assert!(
            existing_blobs(&links).is_err(),
            "hardlink metadata cannot become private checkpoint storage"
        );
    }

    #[tokio::test]
    async fn perf_prepare_scans_blob_directory_once_and_syncs_off_the_caller() {
        let root = crate::tests::OwnedTestDirectory::new("perf-prepare-scan");
        let blobs = root.path().join("blobs");
        crate::paths::ensure_private_directory(&blobs).unwrap();
        let orphan = vec![9u8; 16];
        crate::fs::private_atomic_create_sync(
            &blobs.join(format!("{}.bin", digest(&orphan))),
            &orphan,
        )
        .unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let store = FileCheckpoints::default();
        let mut recorder = store.recorder("session", 1, false);
        recorder.blob_root = Some(blobs.clone());
        let syncs_before = crate::fs::DURABLE_SYNCS.with(|count| count.get());
        scope(Some(recorder), async {
            for value in 0..5u8 {
                let bytes = vec![value; 32];
                let digest = crate::fs::ContentDigest::of(&bytes);
                let pending = prepare(
                    &workspace,
                    Path::new("file"),
                    Some(Preimage::new(bytes, &digest)),
                    true,
                )
                .await;
                assert!(pending.is_some());
            }
            // A repeated preimage is already inventoried: no scan, no write.
            let bytes = vec![0u8; 32];
            let digest = crate::fs::ContentDigest::of(&bytes);
            assert!(
                prepare(
                    &workspace,
                    Path::new("file"),
                    Some(Preimage::new(bytes, &digest)),
                    true
                )
                .await
                .is_some()
            );
        })
        .await;
        assert_eq!(
            crate::fs::DURABLE_SYNCS.with(|count| count.get()),
            syncs_before,
            "durable fsyncs must run on the blocking pool, not the caller"
        );
        let state = store.0.lock().unwrap();
        assert_eq!(state.scans, 1, "blob directory scanned once per store");
        assert_eq!(state.data.blobs.len(), 6, "orphan merged plus five new");
        drop(state);
        assert_eq!(std::fs::read_dir(&blobs).unwrap().count(), 6);
    }
}
