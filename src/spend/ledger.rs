//! Bounded append-only accounting, locked and validated through retained handles.

use super::{Account, MAX_RECEIPT_USD, Row};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const MAX_LEDGER_BYTES: usize = 64 * 1024 * 1024;
const MAX_LEDGER_RECORDS: usize = 65_536;
const MAX_ROW_BYTES: usize = 4096;
const MAX_MODEL_DETAILS: usize = 128;
// Below the existing five-second actor retirement grace. This bounds lock
// contention only; filesystem calls keep their existing scoped I/O ownership.
const WRITE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(1);
const WRITE_LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(10);

#[derive(Debug, Clone, Default)]
pub(crate) struct Totals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_creation: Option<rig::completion::CacheCreationUsage>,
    pub known_cost: f64,
    pub unpriced_records: usize,
    pub unreported_records: usize,
    pub unpriced_tokens: u64,
    pub missing_usage: bool,
    pub records: usize,
}

impl Totals {
    fn add(&mut self, row: &Row) -> anyhow::Result<()> {
        macro_rules! add {
            ($field:ident) => {
                self.$field = self
                    .$field
                    .checked_add(row.usage.$field)
                    .ok_or_else(|| anyhow::anyhow!("usage ledger token total exceeds limits"))?;
            };
        }
        add!(input_tokens);
        add!(output_tokens);
        add!(cached_input_tokens);
        self.cache_creation = rig::completion::CacheCreationUsage::add_optional(
            self.cache_creation_input_tokens,
            self.cache_creation,
            row.usage.cache_creation_input_tokens,
            row.usage.cache_creation,
        );
        add!(cache_creation_input_tokens);
        self.records += 1;
        self.unreported_records += usize::from(!row.usage_reported);
        if let Some(cost) = row.cost {
            self.known_cost += cost;
        } else {
            self.unpriced_records += 1;
            let usage = row.usage;
            let input = crate::session::Session::real_input_tokens(
                row.prices.anthropic_native,
                usage.input_tokens,
                usage.total_tokens,
                usage.output_tokens,
                usage.cached_input_tokens,
                usage.cache_creation_input_tokens,
            );
            self.unpriced_tokens = self
                .unpriced_tokens
                .saturating_add(input)
                .saturating_add(usage.output_tokens);
            self.missing_usage |= !row.usage_reported;
        }
        anyhow::ensure!(
            self.known_cost.is_finite() && self.known_cost <= MAX_RECEIPT_USD,
            "usage ledger cost total exceeds limits"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct LedgerSummary {
    pub all_time: Totals,
    pub today: Totals,
    pub by_model: BTreeMap<(String, String), Totals>,
    pub model_details_capped: bool,
    pub session: Totals,
    pub session_by_model: BTreeMap<(String, String), Totals>,
    pub session_model_details_capped: bool,
    pub last_session_request: Option<CacheRequestSummary>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CacheRequestSummary {
    pub provider: String,
    pub model: String,
    pub kind: super::RequestKind,
    pub usage: crate::event::UsageDelta,
    pub anthropic_native: bool,
    timestamp: chrono::DateTime<chrono::Utc>,
    id: Uuid,
}

impl CacheRequestSummary {
    fn from_row(row: &Row) -> Self {
        Self {
            provider: row.provider.clone(),
            model: row.model.clone(),
            kind: row.kind,
            usage: row.usage,
            anthropic_native: row.prices.anthropic_native,
            timestamp: row.ts,
            id: row.id,
        }
    }
}

struct Scan {
    bytes: usize,
    records: usize,
    identities: BTreeMap<Uuid, [u8; 32]>,
    /// Receipt IDs of rows covered by a verified checkpoint, whose fingerprints
    /// were not recomputed. Empty after a full scan. A pending receipt found
    /// here forces a full rescan so conflicting duplicates are still detected.
    prefix_ids: std::collections::HashSet<Uuid>,
    summary: LedgerSummary,
}

/// A flush scan plus the file state it describes. A later flush reuses it only
/// when the file still has this identity, length and modification time under
/// the writer lock; rows appended by other processes since are scanned from the
/// cached length, and anything else is rescanned in full.
struct FlushCache {
    identity: LedgerIdentity,
    length: u64,
    modified: Option<std::time::SystemTime>,
    /// Inode change time: unlike mtime it cannot be reset from userspace, so
    /// a same-length rewrite followed by `utimes` still forces a rescan.
    changed: Option<(i64, i64)>,
    scan: Scan,
}

fn change_stamp(metadata: &std::fs::Metadata) -> Option<(i64, i64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((metadata.ctime(), metadata.ctime_nsec()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}
impl std::fmt::Debug for FlushCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlushCache")
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

/// Owns the acquired OS lock independently of descriptor close. A concurrent
/// fork can retain the open-file description until exec, despite CLOEXEC.
struct LockedLedgerFile {
    file: File,
    armed: bool,
}
impl LockedLedgerFile {
    fn new(file: File) -> Self {
        Self { file, armed: true }
    }
    fn release(&mut self) -> anyhow::Result<()> {
        self.file
            .unlock()
            .map_err(|_| anyhow::anyhow!("usage ledger unlock failed"))?;
        self.armed = false;
        Ok(())
    }
}
impl std::ops::Deref for LockedLedgerFile {
    type Target = File;
    fn deref(&self) -> &File {
        &self.file
    }
}
impl std::ops::DerefMut for LockedLedgerFile {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.file
    }
}
impl Drop for LockedLedgerFile {
    fn drop(&mut self) {
        if self.armed {
            // Errors/unwind still attempt explicit release; their original
            // failure remains closed and never ACKs an unvalidated receipt.
            let _ = self.file.unlock();
        }
    }
}

/// One ledger's directory authority, not a global registry. Read-only summaries
/// never create a file/directory or repair permissions. Missing storage is empty.
/// The startup session summary's only write is the private `usage.checkpoint`
/// sidecar, replaced atomically beside an existing ledger.
#[derive(Debug)]
pub(crate) struct Ledger {
    path: PathBuf,
    directory: Mutex<Option<Arc<crate::paths::WorkspaceBinding>>>,
    io: Mutex<()>,
    /// The flush scan this process last left behind, valid only for the exact
    /// file identity, length and modification time it was taken at. Read and
    /// replaced under the writer lock only.
    flush_cache: Mutex<Option<FlushCache>>,
    #[cfg(test)]
    pre_sync_gate: Option<Arc<PreSyncGate>>,
    #[cfg(test)]
    lock_blocked: Option<std::sync::mpsc::SyncSender<()>>,
    #[cfg(test)]
    summary_scanned: Option<SummaryScanGate>,
    #[cfg(all(test, unix))]
    lock_gate: Option<LockTestGate>,
}

#[cfg(all(test, unix))]
#[derive(Debug)]
struct LockTestGate {
    locked: std::sync::mpsc::SyncSender<std::os::fd::RawFd>,
    released: Mutex<std::sync::mpsc::Receiver<()>>,
    started: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
#[derive(Debug)]
struct SummaryScanGate {
    scanned: std::sync::mpsc::SyncSender<()>,
    released: Mutex<std::sync::mpsc::Receiver<()>>,
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct PreSyncGate {
    root: PathBuf,
    started: std::sync::atomic::AtomicBool,
}
#[cfg(test)]
tokio::task_local! { static PRE_SYNC_GATE: Arc<PreSyncGate>; }
#[cfg(test)]
impl PreSyncGate {
    pub(crate) fn new(root: &Path) -> Arc<Self> {
        Arc::new(Self {
            root: root.into(),
            started: Default::default(),
        })
    }
    pub(crate) async fn scope<F: std::future::Future>(self: Arc<Self>, future: F) -> F::Output {
        PRE_SYNC_GATE.scope(self, future).await
    }
    fn wait(&self, account: &Account) {
        if self.started.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        // Readers poll `started.exists()` and then parse its content without a
        // retry. A plain `std::fs::write` opens (making `exists()` true) and
        // fills the file in separate steps, so a reader can observe a created
        // but still-empty file and fail to parse it. Writing through a
        // same-directory temp file and renaming into place means the file is
        // never visible at this path until it is complete.
        crate::fs::atomic_create_sync(
            &self.root.join("started"),
            &serde_json::to_vec(&account.snapshot()).unwrap(),
        )
        .unwrap();
        while !self.root.join("release").exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    fn completed(&self) {
        std::fs::write(
            self.root.join("completed"),
            "writer joined through its owning scope",
        )
        .unwrap();
    }
}

impl Ledger {
    pub(crate) fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("usage.jsonl"),
            directory: Mutex::new(None),
            io: Mutex::new(()),
            flush_cache: Mutex::new(None),
            #[cfg(test)]
            pre_sync_gate: PRE_SYNC_GATE.try_with(Clone::clone).ok(),
            #[cfg(test)]
            lock_blocked: None,
            #[cfg(test)]
            summary_scanned: None,
            #[cfg(all(test, unix))]
            lock_gate: None,
        }
    }
    #[cfg(test)]
    pub(crate) fn with_lock_signal(mut self, signal: std::sync::mpsc::SyncSender<()>) -> Self {
        self.lock_blocked = Some(signal);
        self
    }
    #[cfg(test)]
    pub(crate) fn with_summary_gate(
        mut self,
        scanned: std::sync::mpsc::SyncSender<()>,
        released: std::sync::mpsc::Receiver<()>,
    ) -> Self {
        self.summary_scanned = Some(SummaryScanGate {
            scanned,
            released: Mutex::new(released),
        });
        self
    }
    fn binding(&self, write: bool) -> anyhow::Result<Option<Arc<crate::paths::WorkspaceBinding>>> {
        let mut stored = self
            .directory
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(binding) = stored.as_ref() {
            binding
                .validate()
                .map_err(|_| anyhow::anyhow!("usage ledger directory changed"))?;
            return Ok(Some(binding.clone()));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("usage ledger parent unavailable"))?;
        match std::fs::symlink_metadata(parent) {
            Ok(metadata) => anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "usage ledger parent is not a real directory"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !write => {
                return Ok(None);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                crate::fs::ensure_private_directory(parent)?
            }
            Err(_) => anyhow::bail!("usage ledger parent unavailable"),
        }
        let binding = Arc::new(crate::paths::WorkspaceBinding::capture(parent)?);
        binding
            .validate()
            .map_err(|_| anyhow::anyhow!("usage ledger directory changed"))?;
        *stored = Some(binding.clone());
        Ok(Some(binding))
    }
    fn open(
        &self,
        write: bool,
    ) -> anyhow::Result<Option<(File, Arc<crate::paths::WorkspaceBinding>)>> {
        let Some(binding) = self.binding(write)? else {
            return Ok(None);
        };
        let path = binding.root().join("usage.jsonl");
        let access = if write {
            crate::fs::PrivateFileAccess::Append
        } else {
            crate::fs::PrivateFileAccess::Read
        };
        let file = match crate::fs::open_private_file_checked(&path, access) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !write => {
                return Ok(None);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match crate::fs::private_atomic_create_sync(&path, b"") {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => anyhow::bail!("usage ledger creation failed"),
                }
                crate::fs::open_private_file_checked(&path, access)
                    .map_err(|_| anyhow::anyhow!("usage ledger private validation failed"))?
            }
            Err(_) => anyhow::bail!("usage ledger private validation failed"),
        };
        Self::revalidate(&binding, &file)?;
        Ok(Some((file, binding)))
    }
    fn revalidate(binding: &crate::paths::WorkspaceBinding, file: &File) -> anyhow::Result<()> {
        binding
            .validate()
            .map_err(|_| anyhow::anyhow!("usage ledger directory changed"))?;
        crate::fs::validate_private_file(file)
            .map_err(|_| anyhow::anyhow!("usage ledger private validation failed"))?;
        let current = binding
            .open_regular_relative(Path::new("usage.jsonl"))
            .map_err(|_| anyhow::anyhow!("usage ledger entry changed"))?;
        crate::fs::validate_private_file(&current)
            .map_err(|_| anyhow::anyhow!("usage ledger private validation failed"))?;
        let expected = crate::fs::checked_file_metadata(file)?;
        let actual = crate::fs::checked_file_metadata(&current)?;
        crate::fs::ensure_same_file(Path::new("usage ledger"), &expected, &actual)
            .map_err(|_| anyhow::anyhow!("usage ledger entry changed"))?;
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn summary(&self) -> anyhow::Result<LedgerSummary> {
        self.summary_with_session(None)
    }
    #[cfg(all(test, unix))]
    pub(crate) fn with_lock_test_gate(
        state_dir: &Path,
        locked: std::sync::mpsc::SyncSender<std::os::fd::RawFd>,
        released: std::sync::mpsc::Receiver<()>,
    ) -> Self {
        let mut ledger = Self::new(state_dir);
        ledger.lock_gate = Some(LockTestGate {
            locked,
            released: Mutex::new(released),
            started: Default::default(),
        });
        ledger
    }
    #[cfg(all(test, unix))]
    fn pause_lock_for_test(&self, file: &File) {
        use std::os::fd::AsRawFd;
        if let Some(gate) = &self.lock_gate
            && !gate.started.swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            gate.locked
                .try_send(file.as_raw_fd())
                .expect("lock phase published");
            gate.released
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("bounded lock test gate release");
        }
    }
    pub(crate) fn summary_for_session(&self, session_id: &str) -> anyhow::Result<LedgerSummary> {
        self.summary_with_session(Some(session_id))
    }
    /// Startup reconciliation needs only `session`, `session_by_model` and
    /// `last_session_request` (plus `all_time`, which carries the ledger-wide
    /// overflow bounds). Rows covered by a verified checkpoint
    /// (`usage.checkpoint`) are not reread: only the tail past its offset is
    /// bounded, parsed, validated and checked for duplicate receipts against
    /// the checkpoint's receipt IDs. Without a checkpoint that still matches
    /// the ledger's identity, length and boundary rows, the whole file is
    /// scanned exactly as before. The result is identical to a full
    /// session-only scan's; a refreshed checkpoint is then written under the
    /// shared ledger lock.
    pub(crate) fn session_summary(&self, session_id: &str) -> anyhow::Result<LedgerSummary> {
        let _io = self
            .io
            .lock()
            .map_err(|_| anyhow::anyhow!("usage ledger serialization failed"))?;
        let Some((file, binding)) = self.open(false)? else {
            return Ok(LedgerSummary::default());
        };
        file.try_lock_shared()
            .map_err(|_| anyhow::anyhow!("usage ledger is busy"))?;
        let mut file = LockedLedgerFile::new(file);
        #[cfg(all(test, unix))]
        self.pause_lock_for_test(&file);
        Self::revalidate(&binding, &file)?;
        let length = file.metadata()?.len();
        anyhow::ensure!(
            length <= MAX_LEDGER_BYTES as u64,
            "usage ledger exceeds byte limit"
        );
        let checkpoint = load_checkpoint(&binding, &mut file, length);
        let resumed = checkpoint.is_some();
        let mut state = checkpoint.unwrap_or_default();
        file.seek(SeekFrom::Start(state.offset))?;
        let mut tail = Vec::new();
        (&mut *file)
            .take(MAX_LEDGER_BYTES as u64 - state.offset + 1)
            .read_to_end(&mut tail)?;
        anyhow::ensure!(
            state.offset as usize + tail.len() <= MAX_LEDGER_BYTES,
            "usage ledger exceeds byte limit"
        );
        anyhow::ensure!(
            tail.is_empty() || tail.last() == Some(&b'\n'),
            "usage ledger has an incomplete tail"
        );
        state.scan(&tail, session_id)?;
        Self::revalidate(&binding, &file)?;
        if state.checkpointable && state.records > 0 && (!resumed || !tail.is_empty()) {
            // Best effort: a failed write only means the next startup scans
            // more. Written while the shared lock excludes appenders.
            if let Err(error) = store_checkpoint(&binding, &mut file, &state) {
                tracing::debug!(%error, "usage ledger checkpoint not written");
            }
            Self::revalidate(&binding, &file)?;
        }
        file.release()?;
        #[cfg(test)]
        if let Some(gate) = &self.summary_scanned {
            drop(file);
            gate.scanned.try_send(()).unwrap();
            gate.released
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(3))
                .expect("bounded summary test gate release");
        }
        Ok(state.into_summary(session_id))
    }
    /// The full session-only scan the checkpointed path must equal.
    #[cfg(test)]
    fn full_session_summary(&self, session_id: &str) -> anyhow::Result<LedgerSummary> {
        self.summary_with_mode(Some(session_id), ScanMode::SessionOnly)
    }
    fn summary_with_session(&self, session_id: Option<&str>) -> anyhow::Result<LedgerSummary> {
        self.summary_with_mode(session_id, ScanMode::Summary)
    }
    fn summary_with_mode(
        &self,
        session_id: Option<&str>,
        mode: ScanMode,
    ) -> anyhow::Result<LedgerSummary> {
        let _io = self
            .io
            .lock()
            .map_err(|_| anyhow::anyhow!("usage ledger serialization failed"))?;
        let Some((file, binding)) = self.open(false)? else {
            return Ok(LedgerSummary::default());
        };
        file.try_lock_shared()
            .map_err(|_| anyhow::anyhow!("usage ledger is busy"))?;
        let mut file = LockedLedgerFile::new(file);
        #[cfg(all(test, unix))]
        self.pause_lock_for_test(&file);
        Self::revalidate(&binding, &file)?;
        let scan = scan_with(&mut file, session_id, mode)?;
        Self::revalidate(&binding, &file)?;
        file.release()?;
        #[cfg(test)]
        if let Some(gate) = &self.summary_scanned {
            drop(file);
            gate.scanned.try_send(()).unwrap();
            gate.released
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(3))
                .expect("bounded summary test gate release");
        }
        Ok(scan.summary)
    }
    /// Persist only parent-minted new receipts. A repeated flush observes each
    /// matching ID once; a conflicting ID or crash tail fails without repair.
    pub(crate) fn flush(&self, account: &Account) -> anyhow::Result<()> {
        let _io = self
            .io
            .lock()
            .map_err(|_| anyhow::anyhow!("usage ledger serialization failed"))?;
        let pending = account.pending();
        if pending.is_empty() {
            return Ok(());
        }
        let result = self.flush_rows(account, &pending);
        if result.is_err() {
            account.tracking_failed();
        }
        result
    }
    fn lock_writer(
        &self,
        binding: &crate::paths::WorkspaceBinding,
        file: &File,
    ) -> anyhow::Result<()> {
        let started = std::time::Instant::now();
        loop {
            Self::revalidate(binding, file)?;
            match file.try_lock() {
                Ok(()) => return Ok(()),
                Err(std::fs::TryLockError::WouldBlock) => {
                    #[cfg(test)]
                    if let Some(signal) = &self.lock_blocked {
                        let _ = signal.try_send(());
                    }
                    let remaining = WRITE_LOCK_WAIT.saturating_sub(started.elapsed());
                    anyhow::ensure!(!remaining.is_zero(), "usage ledger lock wait expired");
                    std::thread::sleep(WRITE_LOCK_RETRY.min(remaining));
                }
                Err(std::fs::TryLockError::Error(_)) => {
                    anyhow::bail!("usage ledger locking failed")
                }
            }
        }
    }
    /// The ledger's current flush scan, under the writer lock. The cache is
    /// taken, so any failure before `flush_rows` stores it again leaves none.
    fn flush_scan(
        &self,
        binding: &crate::paths::WorkspaceBinding,
        file: &mut File,
    ) -> anyhow::Result<Scan> {
        let cached = self
            .flush_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(mut cache) = cached {
            let metadata = file.metadata()?;
            let length = metadata.len();
            let same_file = LedgerIdentity::of(file).is_ok_and(|id| id == cache.identity);
            if same_file
                && length == cache.length
                && metadata.modified().ok() == cache.modified
                && change_stamp(&metadata) == cache.changed
            {
                return Ok(cache.scan);
            }
            if same_file && length > cache.length {
                anyhow::ensure!(
                    length <= MAX_LEDGER_BYTES as u64,
                    "usage ledger exceeds byte limit"
                );
                scan_appended(file, &mut cache.scan, cache.length, length)?;
                return Ok(cache.scan);
            }
        }
        // First flush of the process: seed from the verified checkpoint (one
        // streamed hash of the covered prefix, no parsing) and parse only the
        // rows past its offset. Any checkpoint problem scans the whole file.
        let length = file.metadata()?.len();
        anyhow::ensure!(
            length <= MAX_LEDGER_BYTES as u64,
            "usage ledger exceeds byte limit"
        );
        if let Some(state) = load_checkpoint(binding, file, length) {
            let offset = state.offset;
            let mut scanned = Scan {
                bytes: offset as usize,
                records: state.records,
                identities: BTreeMap::new(),
                prefix_ids: state.ids,
                summary: LedgerSummary {
                    all_time: state.all_time,
                    ..Default::default()
                },
            };
            if length > offset {
                scan_appended(file, &mut scanned, offset, length)?;
            }
            return Ok(scanned);
        }
        scan(file, None)
    }
    fn flush_rows(&self, account: &Account, pending: &[Arc<Row>]) -> anyhow::Result<()> {
        let (file, binding) = self
            .open(true)?
            .ok_or_else(|| anyhow::anyhow!("usage ledger unavailable"))?;
        self.lock_writer(&binding, &file)?;
        let mut file = LockedLedgerFile::new(file);
        #[cfg(all(test, unix))]
        self.pause_lock_for_test(&file);
        Self::revalidate(&binding, &file)?;
        let mut scanned = self.flush_scan(&binding, &mut file)?;
        // A receipt already in the checkpointed prefix has no fingerprint yet:
        // rescan fully so a conflicting duplicate fails exactly as before.
        if pending
            .iter()
            .any(|row| scanned.prefix_ids.contains(&row.id))
        {
            scanned = scan(&mut file, None)?;
        }
        // Validate the entire pending batch before appending any of it.
        let mut additions = Vec::new();
        for row in pending {
            row.validate()?;
            let bytes = serde_json::to_vec(row.as_ref())?;
            anyhow::ensure!(
                bytes.len() < MAX_ROW_BYTES,
                "usage ledger row exceeds limits"
            );
            let fingerprint = Sha256::digest(&bytes).into();
            if let Some(previous) = scanned.identities.get(&row.id) {
                anyhow::ensure!(*previous == fingerprint, "conflicting usage ledger receipt");
                continue;
            }
            scanned.records += 1;
            scanned.bytes += bytes.len() + 1;
            anyhow::ensure!(
                scanned.records <= MAX_LEDGER_RECORDS && scanned.bytes <= MAX_LEDGER_BYTES,
                "usage ledger capacity reached"
            );
            scanned.summary.all_time.add(row)?;
            scanned.identities.insert(row.id, fingerprint);
            additions.push((row.id, bytes));
        }
        Self::revalidate(&binding, &file)?;
        for (_, bytes) in &additions {
            file.write_all(bytes)
                .map_err(|_| anyhow::anyhow!("usage ledger append failed"))?;
            file.write_all(b"\n")
                .map_err(|_| anyhow::anyhow!("usage ledger append failed"))?;
        }
        #[cfg(test)]
        if let Some(gate) = &self.pre_sync_gate {
            gate.wait(account);
        }
        file.sync_all()
            .map_err(|_| anyhow::anyhow!("usage ledger sync failed"))?;
        Self::revalidate(&binding, &file)?;
        if let Ok(metadata) = file.metadata()
            && metadata.len() == scanned.bytes as u64
            && let Ok(identity) = LedgerIdentity::of(&file)
        {
            *self
                .flush_cache
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(FlushCache {
                identity,
                length: metadata.len(),
                modified: metadata.modified().ok(),
                changed: change_stamp(&metadata),
                scan: scanned,
            });
        }
        file.release()?;
        for row in pending {
            account.acknowledge(row.id);
        }
        #[cfg(test)]
        if let Some(gate) = &self.pre_sync_gate {
            gate.completed();
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanMode {
    /// Appending: receipt fingerprints detect conflicting duplicate IDs.
    Flush,
    /// Every accumulator, for `/cost` style summaries.
    Summary,
    /// Only the requested session's accumulators; duplicate IDs still fail.
    /// Production startup uses the checkpointed [`TailScan`] instead; this
    /// full scan is its test reference.
    #[cfg_attr(not(test), allow(dead_code))]
    SessionOnly,
}

fn scan(file: &mut File, session_id: Option<&str>) -> anyhow::Result<Scan> {
    scan_with(file, session_id, ScanMode::Flush)
}

/// Extends a flush scan with the rows other processes appended in
/// `[from, length)`, applying exactly the per-row checks of a full flush scan.
fn scan_appended(
    file: &mut File,
    scanned: &mut Scan,
    from: u64,
    length: u64,
) -> anyhow::Result<()> {
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = Vec::new();
    (&mut *file).take(length - from).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.last() == Some(&b'\n'),
        "usage ledger has an incomplete tail"
    );
    anyhow::ensure!(
        scanned.bytes as u64 == from,
        "usage ledger scan is out of step"
    );
    scanned.bytes += bytes.len();
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        scanned.records += 1;
        anyhow::ensure!(
            scanned.records <= MAX_LEDGER_RECORDS && line.len() <= MAX_ROW_BYTES && line.len() > 1,
            "usage ledger record exceeds limits"
        );
        let row: Row = serde_json::from_slice(&line[..line.len() - 1])
            .map_err(|_| anyhow::anyhow!("usage ledger record is invalid"))?;
        row.validate()
            .map_err(|_| anyhow::anyhow!("usage ledger record is invalid"))?;
        #[cfg(test)]
        FLUSH_ROWS_PARSED.with(|parsed| parsed.set(parsed.get() + 1));
        let fingerprint = Sha256::digest(serde_json::to_vec(&row)?).into();
        anyhow::ensure!(
            !scanned.prefix_ids.contains(&row.id)
                && scanned.identities.insert(row.id, fingerprint).is_none(),
            "usage ledger contains duplicate receipts"
        );
        scanned.summary.all_time.add(&row)?;
    }
    Ok(())
}

fn scan_with(file: &mut File, session_id: Option<&str>, mode: ScanMode) -> anyhow::Result<Scan> {
    let length = file.metadata()?.len();
    anyhow::ensure!(
        length <= MAX_LEDGER_BYTES as u64,
        "usage ledger exceeds byte limit"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(MAX_LEDGER_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX_LEDGER_BYTES,
        "usage ledger exceeds byte limit"
    );
    anyhow::ensure!(
        bytes.is_empty() || bytes.last() == Some(&b'\n'),
        "usage ledger has an incomplete tail"
    );
    let mut scanned = Scan {
        bytes: bytes.len(),
        records: 0,
        identities: BTreeMap::new(),
        prefix_ids: Default::default(),
        summary: LedgerSummary::default(),
    };
    let mut seen = std::collections::BTreeSet::new();
    let today = chrono::Utc::now().date_naive();
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        scanned.records += 1;
        anyhow::ensure!(
            scanned.records <= MAX_LEDGER_RECORDS && line.len() <= MAX_ROW_BYTES && line.len() > 1,
            "usage ledger record exceeds limits"
        );
        let row: Row = serde_json::from_slice(&line[..line.len() - 1])
            .map_err(|_| anyhow::anyhow!("usage ledger record is invalid"))?;
        row.validate()
            .map_err(|_| anyhow::anyhow!("usage ledger record is invalid"))?;
        #[cfg(test)]
        FLUSH_ROWS_PARSED.with(|parsed| parsed.set(parsed.get() + 1));
        if mode == ScanMode::Flush {
            let fingerprint = Sha256::digest(serde_json::to_vec(&row)?).into();
            anyhow::ensure!(
                scanned.identities.insert(row.id, fingerprint).is_none(),
                "usage ledger contains duplicate receipts"
            );
        } else {
            anyhow::ensure!(
                seen.insert(row.id),
                "usage ledger contains duplicate receipts"
            );
        }
        // All-time totals always run: they enforce the ledger-wide overflow
        // bounds. Today/by-model totals are subsets and cannot fail when it
        // succeeds, so skipping them never changes the scan's outcome.
        scanned.summary.all_time.add(&row)?;
        if mode != ScanMode::SessionOnly && row.ts.date_naive() == today {
            scanned.summary.today.add(&row)?;
        }
        if session_id == Some(row.session_id.as_str()) {
            scanned.summary.session.add(&row)?;
            if scanned
                .summary
                .last_session_request
                .as_ref()
                .is_none_or(|previous| (row.ts, row.id) > (previous.timestamp, previous.id))
            {
                scanned.summary.last_session_request = Some(CacheRequestSummary::from_row(&row));
            }
            let key = (row.provider.clone(), row.model.clone());
            if scanned.summary.session_by_model.contains_key(&key)
                || scanned.summary.session_by_model.len() < MAX_MODEL_DETAILS
            {
                scanned
                    .summary
                    .session_by_model
                    .entry(key)
                    .or_default()
                    .add(&row)?;
            } else {
                scanned.summary.session_model_details_capped = true;
            }
        }
        if mode == ScanMode::SessionOnly {
            continue;
        }
        let key = (row.provider.clone(), row.model.clone());
        if scanned.summary.by_model.contains_key(&key)
            || scanned.summary.by_model.len() < MAX_MODEL_DETAILS
        {
            scanned.summary.by_model.entry(key).or_default().add(&row)?;
        } else {
            scanned.summary.model_details_capped = true;
        }
    }
    Ok(scanned)
}

const CHECKPOINT_FILE: &str = "usage.checkpoint";
const CHECKPOINT_MAGIC: &[u8; 8] = b"MAUSGCK1";
const CHECKPOINT_VERSION: u32 = 1;
const MAX_CHECKPOINT_BYTES: u64 = 16 * 1024 * 1024;
/// Sessions tracked for a checkpoint. A ledger with more distinct sessions is
/// still scanned exactly; it is simply not checkpointed.
const MAX_CHECKPOINT_SESSIONS: usize = 4096;

#[cfg(test)]
thread_local! {
    /// Rows parsed by flush scans (full or appended-tail).
    static FLUSH_ROWS_PARSED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    static TAIL_ROWS_PARSED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `Totals` with the cost stored as exact IEEE-754 bits, so a checkpoint
/// round trip never perturbs the running sum.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TotalsRecord {
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_creation: Option<rig::completion::CacheCreationUsage>,
    known_cost_bits: u64,
    unpriced_records: usize,
    unreported_records: usize,
    unpriced_tokens: u64,
    missing_usage: bool,
    records: usize,
}

impl From<&Totals> for TotalsRecord {
    fn from(totals: &Totals) -> Self {
        Self {
            input_tokens: totals.input_tokens,
            output_tokens: totals.output_tokens,
            cached_input_tokens: totals.cached_input_tokens,
            cache_creation_input_tokens: totals.cache_creation_input_tokens,
            cache_creation: totals.cache_creation,
            known_cost_bits: totals.known_cost.to_bits(),
            unpriced_records: totals.unpriced_records,
            unreported_records: totals.unreported_records,
            unpriced_tokens: totals.unpriced_tokens,
            missing_usage: totals.missing_usage,
            records: totals.records,
        }
    }
}

impl TotalsRecord {
    fn into_totals(self) -> Option<Totals> {
        let known_cost = f64::from_bits(self.known_cost_bits);
        (known_cost.is_finite() && (0.0..=MAX_RECEIPT_USD).contains(&known_cost)).then_some(
            Totals {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                cached_input_tokens: self.cached_input_tokens,
                cache_creation_input_tokens: self.cache_creation_input_tokens,
                cache_creation: self.cache_creation,
                known_cost,
                unpriced_records: self.unpriced_records,
                unreported_records: self.unreported_records,
                unpriced_tokens: self.unpriced_tokens,
                missing_usage: self.missing_usage,
                records: self.records,
            },
        )
    }
}

/// One session's accumulators, updated exactly as `scan_with` updates the
/// requested session's.
#[derive(Debug, Clone, Default)]
struct SessionTotals {
    totals: Totals,
    by_model: BTreeMap<(String, String), Totals>,
    capped: bool,
    last: Option<CacheRequestSummary>,
}

impl SessionTotals {
    fn add(&mut self, row: &Row) -> anyhow::Result<()> {
        self.totals.add(row)?;
        if self
            .last
            .as_ref()
            .is_none_or(|previous| (row.ts, row.id) > (previous.timestamp, previous.id))
        {
            self.last = Some(CacheRequestSummary::from_row(row));
        }
        let key = (row.provider.clone(), row.model.clone());
        if self.by_model.contains_key(&key) || self.by_model.len() < MAX_MODEL_DETAILS {
            self.by_model.entry(key).or_default().add(row)?;
        } else {
            self.capped = true;
        }
        Ok(())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionRecord {
    session_id: String,
    totals: TotalsRecord,
    by_model: Vec<(String, String, TotalsRecord)>,
    capped: bool,
    last: Option<CacheRequestSummary>,
}

/// Identity of the ledger file a checkpoint covers. Replacement (rotation)
/// changes it; truncation is caught by the length and boundary-row checks.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerIdentity {
    device: u64,
    inode: u64,
    created_secs: i64,
    created_nanos: u32,
}

impl LedgerIdentity {
    fn of(file: &File) -> std::io::Result<Self> {
        let metadata = file.metadata()?;
        #[cfg(unix)]
        let (device, inode) = {
            use std::os::unix::fs::MetadataExt;
            (metadata.dev(), metadata.ino())
        };
        #[cfg(not(unix))]
        let (device, inode) = (0, 0);
        let (created_secs, created_nanos) = metadata
            .created()
            .ok()
            .and_then(|created| created.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|elapsed| (elapsed.as_secs() as i64, elapsed.subsec_nanos()))
            .unwrap_or((-1, 0));
        Ok(Self {
            device,
            inode,
            created_secs,
            created_nanos,
        })
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointHeader {
    version: u32,
    identity: LedgerIdentity,
    offset: u64,
    records: usize,
    /// SHA-256 of the ledger bytes `[0, offset)`.
    prefix_sha256: [u8; 32],
    all_time: TotalsRecord,
    sessions: Vec<SessionRecord>,
}

/// Running state of a session-only scan: what a checkpoint persists, plus
/// the per-row checks of `scan_with` applied to each newly read row.
struct TailScan {
    offset: u64,
    records: usize,
    ids: std::collections::HashSet<Uuid>,
    all_time: Totals,
    sessions: BTreeMap<String, SessionTotals>,
    /// Running SHA-256 of the bytes `[0, offset)`.
    prefix: Sha256,
    /// False once other sessions' totals are no longer tracked exactly.
    checkpointable: bool,
}

impl Default for TailScan {
    fn default() -> Self {
        Self {
            offset: 0,
            records: 0,
            ids: Default::default(),
            all_time: Totals::default(),
            sessions: BTreeMap::new(),
            prefix: Sha256::new(),
            checkpointable: true,
        }
    }
}

impl TailScan {
    /// Apply `bytes` (complete rows starting at `self.offset`) in order with
    /// exactly `scan_with`'s session-only checks: record/row bounds, parse,
    /// validate, duplicate receipt, all-time overflow, then the requested
    /// session's accumulators. Other sessions are tracked only for the
    /// checkpoint and can never fail the scan.
    fn scan(&mut self, bytes: &[u8], requested: &str) -> anyhow::Result<()> {
        self.prefix.update(bytes);
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            #[cfg(test)]
            TAIL_ROWS_PARSED.with(|parsed| parsed.set(parsed.get() + 1));
            self.records += 1;
            anyhow::ensure!(
                self.records <= MAX_LEDGER_RECORDS && line.len() <= MAX_ROW_BYTES && line.len() > 1,
                "usage ledger record exceeds limits"
            );
            let row: Row = serde_json::from_slice(&line[..line.len() - 1])
                .map_err(|_| anyhow::anyhow!("usage ledger record is invalid"))?;
            row.validate()
                .map_err(|_| anyhow::anyhow!("usage ledger record is invalid"))?;
            anyhow::ensure!(
                self.ids.insert(row.id),
                "usage ledger contains duplicate receipts"
            );
            self.all_time.add(&row)?;
            if row.session_id == requested {
                self.sessions
                    .entry(row.session_id.clone())
                    .or_default()
                    .add(&row)?;
            } else if self.checkpointable {
                let tracked = self.sessions.contains_key(&row.session_id)
                    || self.sessions.len() < MAX_CHECKPOINT_SESSIONS;
                if !tracked
                    || self
                        .sessions
                        .entry(row.session_id.clone())
                        .or_default()
                        .add(&row)
                        .is_err()
                {
                    self.checkpointable = false;
                    self.sessions.retain(|session, _| session == requested);
                }
            }
        }
        self.offset += bytes.len() as u64;
        Ok(())
    }

    fn into_summary(mut self, requested: &str) -> LedgerSummary {
        let session = self.sessions.remove(requested).unwrap_or_default();
        LedgerSummary {
            all_time: self.all_time,
            session: session.totals,
            session_by_model: session.by_model,
            session_model_details_capped: session.capped,
            last_session_request: session.last,
            ..Default::default()
        }
    }
}

/// Stream `[0, length)` of the ledger into a fresh SHA-256 state.
fn hash_prefix(file: &mut File, length: u64) -> std::io::Result<Sha256> {
    let mut hasher = Sha256::new();
    file.seek(SeekFrom::Start(0))?;
    let mut remaining = length;
    let mut buffer = vec![0; 1024 * 1024];
    while remaining > 0 {
        let chunk = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..chunk])?;
        hasher.update(&buffer[..chunk]);
        remaining -= chunk as u64;
    }
    Ok(hasher)
}

/// Read and verify the checkpoint against the locked ledger. Any problem
/// (missing, unreadable, oversized, corrupt, another version, or a ledger
/// that was replaced, truncated or rewritten anywhere in the covered prefix)
/// yields `None`, which scans the whole ledger.
fn load_checkpoint(
    binding: &crate::paths::WorkspaceBinding,
    ledger: &mut File,
    ledger_length: u64,
) -> Option<TailScan> {
    let result = (|| -> anyhow::Result<TailScan> {
        let path = binding.root().join(CHECKPOINT_FILE);
        let file = crate::fs::open_private_file_checked(&path, crate::fs::PrivateFileAccess::Read)?;
        crate::fs::validate_private_file(&file)?;
        let length = file.metadata()?.len();
        anyhow::ensure!(length <= MAX_CHECKPOINT_BYTES, "checkpoint is oversized");
        let mut bytes = Vec::new();
        file.take(MAX_CHECKPOINT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_CHECKPOINT_BYTES && bytes.len() >= 8 + 8 + 8 + 32,
            "checkpoint size is invalid"
        );
        let (body, digest) = bytes.split_at(bytes.len() - 32);
        anyhow::ensure!(
            Sha256::digest(body).as_slice() == digest,
            "checkpoint digest mismatch"
        );
        anyhow::ensure!(&body[..8] == CHECKPOINT_MAGIC, "checkpoint magic mismatch");
        let header_length = u64::from_le_bytes(body[8..16].try_into()?) as usize;
        let header_end = 16usize
            .checked_add(header_length)
            .filter(|end| *end + 8 <= body.len())
            .ok_or_else(|| anyhow::anyhow!("checkpoint header is truncated"))?;
        let header: CheckpointHeader = serde_json::from_slice(&body[16..header_end])?;
        let id_count = u64::from_le_bytes(body[header_end..header_end + 8].try_into()?) as usize;
        let ids = &body[header_end + 8..];
        anyhow::ensure!(
            header.version == CHECKPOINT_VERSION
                && id_count == header.records
                && ids.len() == id_count.saturating_mul(16)
                && header.records > 0
                && header.records <= MAX_LEDGER_RECORDS
                && header.sessions.len() <= MAX_CHECKPOINT_SESSIONS,
            "checkpoint header is inconsistent"
        );
        anyhow::ensure!(
            header.identity == LedgerIdentity::of(ledger)?,
            "ledger identity changed"
        );
        anyhow::ensure!(
            header.offset <= ledger_length && header.offset <= MAX_LEDGER_BYTES as u64,
            "ledger was truncated"
        );
        // One streamed SHA-256 pass replaces parsing, validating and
        // re-accumulating the covered rows; any byte change fails closed.
        let prefix = hash_prefix(ledger, header.offset)?;
        anyhow::ensure!(
            prefix.clone().finalize().as_slice() == header.prefix_sha256,
            "ledger prefix changed"
        );
        let mut unique = std::collections::HashSet::with_capacity(id_count);
        for id in ids.chunks_exact(16) {
            anyhow::ensure!(
                unique.insert(Uuid::from_slice(id)?),
                "checkpoint receipt IDs repeat"
            );
        }
        let mut sessions = BTreeMap::new();
        for record in header.sessions {
            let mut by_model = BTreeMap::new();
            for (provider, model, totals) in record.by_model {
                let totals = totals
                    .into_totals()
                    .ok_or_else(|| anyhow::anyhow!("checkpoint totals are invalid"))?;
                by_model.insert((provider, model), totals);
            }
            anyhow::ensure!(
                by_model.len() <= MAX_MODEL_DETAILS,
                "checkpoint model details exceed limits"
            );
            let totals = record
                .totals
                .into_totals()
                .ok_or_else(|| anyhow::anyhow!("checkpoint totals are invalid"))?;
            anyhow::ensure!(
                sessions
                    .insert(
                        record.session_id,
                        SessionTotals {
                            totals,
                            by_model,
                            capped: record.capped,
                            last: record.last,
                        },
                    )
                    .is_none(),
                "checkpoint sessions repeat"
            );
        }
        let all_time = header
            .all_time
            .into_totals()
            .ok_or_else(|| anyhow::anyhow!("checkpoint totals are invalid"))?;
        anyhow::ensure!(
            all_time.records == header.records,
            "checkpoint totals are inconsistent"
        );
        Ok(TailScan {
            offset: header.offset,
            records: header.records,
            ids: unique,
            all_time,
            sessions,
            prefix,
            checkpointable: true,
        })
    })();
    match result {
        Ok(state) => Some(state),
        Err(error) => {
            tracing::debug!(%error, "usage ledger checkpoint unavailable; scanning the full ledger");
            None
        }
    }
}

fn encode_checkpoint(ledger: &mut File, state: &TailScan) -> anyhow::Result<Vec<u8>> {
    let header = CheckpointHeader {
        version: CHECKPOINT_VERSION,
        identity: LedgerIdentity::of(ledger)?,
        offset: state.offset,
        records: state.records,
        prefix_sha256: state.prefix.clone().finalize().into(),
        all_time: TotalsRecord::from(&state.all_time),
        sessions: state
            .sessions
            .iter()
            .map(|(session_id, totals)| SessionRecord {
                session_id: session_id.clone(),
                totals: TotalsRecord::from(&totals.totals),
                by_model: totals
                    .by_model
                    .iter()
                    .map(|((provider, model), totals)| {
                        (provider.clone(), model.clone(), TotalsRecord::from(totals))
                    })
                    .collect(),
                capped: totals.capped,
                last: totals.last.clone(),
            })
            .collect(),
    };
    let header = serde_json::to_vec(&header)?;
    let mut ids = state
        .ids
        .iter()
        .map(|id| *id.as_bytes())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    let mut bytes = Vec::with_capacity(16 + header.len() + 8 + ids.len() * 16 + 32);
    bytes.extend_from_slice(CHECKPOINT_MAGIC);
    bytes.extend_from_slice(&(header.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(&(ids.len() as u64).to_le_bytes());
    for id in ids {
        bytes.extend_from_slice(&id);
    }
    let digest = Sha256::digest(&bytes);
    bytes.extend_from_slice(&digest);
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_CHECKPOINT_BYTES,
        "checkpoint exceeds its size limit"
    );
    Ok(bytes)
}

/// Atomically replace the private checkpoint sidecar. The caller holds the
/// ledger's shared lock, so no append can interleave with the covered prefix.
fn store_checkpoint(
    binding: &crate::paths::WorkspaceBinding,
    ledger: &mut File,
    state: &TailScan,
) -> anyhow::Result<()> {
    let bytes = encode_checkpoint(ledger, state)?;
    binding
        .validate()
        .map_err(|_| anyhow::anyhow!("usage ledger directory changed"))?;
    crate::fs::private_atomic_write_sync(&binding.root().join(CHECKPOINT_FILE), &bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::UsageDelta;
    use crate::spend::{PriceSnapshot, Receipt, RequestKind};
    fn receipt(kind: RequestKind) -> Receipt {
        Receipt::new(
            "s",
            "provider",
            "model",
            kind,
            UsageDelta {
                input_tokens: 1_000_000,
                ..Default::default()
            },
            PriceSnapshot::capture(
                Some((2.0, 4.0)),
                crate::pricing::CachePrices::default(),
                false,
            ),
        )
    }
    #[test]
    fn startup_session_summary_equals_full_scan_session_totals() {
        let root = crate::tests::OwnedTestDirectory::new("spend-ledger-session-scan");
        let state = root.path().join("state");
        crate::fs::ensure_private_directory(&state).unwrap();
        let ledger = Ledger::new(&state);
        assert_eq!(
            ledger.session_summary("a").unwrap().session.records,
            0,
            "missing storage is empty"
        );
        for (session, models) in [("a", ["m1", "m2", "m3"]), ("b", ["m1", "m4", "m5"])] {
            let account = Account::new(session, None, 0.0).unwrap();
            for index in 0..120u64 {
                let model = models[(index % 3) as usize];
                let prices = if index % 5 == 0 {
                    PriceSnapshot::capture(None, Default::default(), false)
                } else {
                    PriceSnapshot::capture(Some((1.0, 3.0)), Default::default(), false)
                };
                let usage = if index % 7 == 0 {
                    UsageDelta::default()
                } else {
                    UsageDelta {
                        input_tokens: 1_000 + index,
                        output_tokens: 10 * index,
                        cached_input_tokens: index,
                        ..Default::default()
                    }
                };
                let kind = [RequestKind::Turn, RequestKind::Title, RequestKind::Subagent]
                    [(index % 3) as usize];
                account
                    .observe(&Receipt::new(
                        session, "provider", model, kind, usage, prices,
                    ))
                    .unwrap();
            }
            ledger.flush(&account).unwrap();
        }
        for session in ["a", "b", "absent"] {
            let full = ledger.summary_for_session(session).unwrap();
            let quick = ledger.session_summary(session).unwrap();
            assert_eq!(
                format!("{:?}", quick.session),
                format!("{:?}", full.session),
                "{session}"
            );
            assert_eq!(
                format!("{:?}", quick.session_by_model),
                format!("{:?}", full.session_by_model)
            );
            assert_eq!(
                quick.session_model_details_capped,
                full.session_model_details_capped
            );
            assert_eq!(
                format!("{:?}", quick.last_session_request),
                format!("{:?}", full.last_session_request)
            );
            assert_eq!(
                format!("{:?}", quick.all_time),
                format!("{:?}", full.all_time)
            );
        }
        assert_eq!(ledger.session_summary("a").unwrap().session.records, 120);
        // Duplicate receipts still fail the session-only scan.
        let mut bytes = std::fs::read(&ledger.path).unwrap();
        let first = bytes
            .split_inclusive(|byte| *byte == b'\n')
            .next()
            .unwrap()
            .to_vec();
        bytes.extend_from_slice(&first);
        std::fs::write(&ledger.path, &bytes).unwrap();
        assert!(ledger.summary_for_session("a").is_err());
        assert!(ledger.session_summary("a").is_err());
    }
    fn tail_rows_parsed() -> usize {
        TAIL_ROWS_PARSED.with(std::cell::Cell::get)
    }

    /// The checkpointed startup summary must equal the full session-only scan
    /// in every field, or fail with the same error.
    fn assert_equals_full_scan(ledger: &Ledger, session: &str) -> Option<LedgerSummary> {
        let full = ledger.full_session_summary(session);
        let quick = ledger.session_summary(session);
        match (full, quick) {
            (Ok(full), Ok(quick)) => {
                for (left, right) in [
                    (
                        format!("{:?}", quick.session),
                        format!("{:?}", full.session),
                    ),
                    (
                        format!("{:?}", quick.session_by_model),
                        format!("{:?}", full.session_by_model),
                    ),
                    (
                        format!("{:?}", quick.last_session_request),
                        format!("{:?}", full.last_session_request),
                    ),
                    (
                        format!("{:?}", quick.all_time),
                        format!("{:?}", full.all_time),
                    ),
                    (
                        quick.session_model_details_capped.to_string(),
                        full.session_model_details_capped.to_string(),
                    ),
                ] {
                    assert_eq!(left, right, "session {session}");
                }
                Some(quick)
            }
            (Err(full), Err(quick)) => {
                assert_eq!(quick.to_string(), full.to_string(), "session {session}");
                None
            }
            (full, quick) => panic!(
                "checkpointed scan diverged for {session}: full {:?}, quick {:?}",
                full.map(|summary| summary.session.records),
                quick.map(|summary| summary.session.records)
            ),
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    fn append_random_rows(ledger: &Ledger, rng: &mut Rng, rows: u64) {
        for _ in 0..rows {
            let session = ["a", "b", "c", "d"][rng.below(4) as usize];
            let account = Account::new(session, None, 0.0).unwrap();
            let model = format!("m{}", rng.below(5));
            let prices = if rng.below(4) == 0 {
                PriceSnapshot::capture(None, Default::default(), false)
            } else {
                PriceSnapshot::capture(Some((0.3, 1.7)), Default::default(), false)
            };
            let usage = if rng.below(6) == 0 {
                UsageDelta::default()
            } else {
                UsageDelta {
                    input_tokens: 1 + rng.below(50_000),
                    output_tokens: rng.below(9_000),
                    cached_input_tokens: rng.below(1_000),
                    ..Default::default()
                }
            };
            let kind = [RequestKind::Turn, RequestKind::Title, RequestKind::Subagent]
                [rng.below(3) as usize];
            account
                .observe(&Receipt::new(
                    session, "provider", &model, kind, usage, prices,
                ))
                .unwrap();
            ledger.flush(&account).unwrap();
        }
    }

    fn line_count(ledger: &Ledger) -> usize {
        std::fs::read(&ledger.path)
            .unwrap()
            .split_inclusive(|byte| *byte == b'\n')
            .count()
    }

    fn flush_rows_parsed() -> usize {
        FLUSH_ROWS_PARSED.with(std::cell::Cell::get)
    }

    fn one_receipt(session: &str, input_tokens: u64) -> Receipt {
        Receipt::new(
            session,
            "provider",
            "model",
            RequestKind::Turn,
            UsageDelta {
                input_tokens,
                output_tokens: 1,
                ..Default::default()
            },
            PriceSnapshot::capture(Some((0.3, 1.7)), Default::default(), false),
        )
    }

    fn flush_receipt(ledger: &Ledger, receipt: &Receipt) -> anyhow::Result<()> {
        let account = Account::new(&receipt.row.session_id, None, 0.0).unwrap();
        account.observe(receipt).unwrap();
        ledger.flush(&account)
    }

    #[test]
    fn repeated_flushes_do_not_reread_the_ledger_prefix() {
        let root = crate::tests::OwnedTestDirectory::new("spend-ledger-flush-cache");
        let state = root.path().join("state");
        crate::fs::ensure_private_directory(&state).unwrap();
        let ledger = Ledger::new(&state);
        let mut rng = Rng(0x1234_5678_9abc_def1);
        append_random_rows(&ledger, &mut rng, 60);
        let before = flush_rows_parsed();
        let first = one_receipt("a", 5);
        flush_receipt(&ledger, &first).unwrap();
        append_random_rows(&ledger, &mut rng, 20);
        assert_eq!(flush_rows_parsed() - before, 0, "cached scan is reused");
        assert_eq!(line_count(&ledger), 81);
        // A flush of an already-recorded receipt is still a no-op.
        flush_receipt(&ledger, &first).unwrap();
        assert_eq!(line_count(&ledger), 81);

        // Rows appended by another writer are scanned, and only those.
        let other = Ledger::new(&state);
        flush_receipt(&other, &one_receipt("b", 7)).unwrap();
        flush_receipt(&other, &one_receipt("b", 8)).unwrap();
        let before = flush_rows_parsed();
        let second = one_receipt("a", 9);
        flush_receipt(&ledger, &second).unwrap();
        assert_eq!(flush_rows_parsed() - before, 2, "only external rows parsed");
        assert_eq!(line_count(&ledger), 84);
        assert_eq!(ledger.summary().unwrap().all_time.records, 84);

        // A receipt re-using a recorded id with different content is rejected,
        // both for a cached and for an externally appended row.
        let mut conflicting = one_receipt("a", 11);
        Arc::get_mut(&mut conflicting.row).unwrap().id = second.row.id;
        assert!(flush_receipt(&ledger, &conflicting).is_err());
        let external = one_receipt("b", 12);
        flush_receipt(&other, &external).unwrap();
        let mut conflicting = one_receipt("a", 13);
        Arc::get_mut(&mut conflicting.row).unwrap().id = external.row.id;
        assert!(flush_receipt(&ledger, &conflicting).is_err());
        assert_eq!(line_count(&ledger), 85);

        // Replacing the ledger file forces a full rescan.
        let bytes = std::fs::read(&ledger.path).unwrap();
        let shorter: Vec<u8> = bytes
            .split_inclusive(|byte| *byte == b'\n')
            .take(10)
            .flatten()
            .copied()
            .collect();
        std::fs::write(&ledger.path, &shorter).unwrap();
        let before = flush_rows_parsed();
        flush_receipt(&ledger, &one_receipt("a", 14)).unwrap();
        assert_eq!(flush_rows_parsed() - before, 10, "truncation rescans");
        assert_eq!(line_count(&ledger), 11);
    }

    // mini-agent-vb5vw.60: the first flush of a process seeds from the verified
    // checkpoint and parses only the rows past its offset, while a receipt that
    // re-uses a prefix id still gets the full conflict check.
    #[test]
    fn first_flush_seeds_from_the_checkpoint_and_still_detects_prefix_conflicts() {
        let root = crate::tests::OwnedTestDirectory::new("spend-ledger-flush-seed");
        let state = root.path().join("state");
        crate::fs::ensure_private_directory(&state).unwrap();
        let writer = Ledger::new(&state);
        let mut rng = Rng(0x0bad_cafe_f00d_1234);
        append_random_rows(&writer, &mut rng, 30);
        let first = one_receipt("a", 5);
        flush_receipt(&writer, &first).unwrap();
        // Startup reconciliation writes the checkpoint at the current end.
        Ledger::new(&state).session_summary("a").unwrap();
        assert!(state.join(CHECKPOINT_FILE).exists());
        // Rows past the checkpoint, from another process.
        append_random_rows(&writer, &mut rng, 3);

        let fresh = Ledger::new(&state);
        let before = flush_rows_parsed();
        let new = one_receipt("a", 21);
        flush_receipt(&fresh, &new).unwrap();
        assert_eq!(
            flush_rows_parsed() - before,
            3,
            "only the rows past the checkpoint are parsed"
        );
        assert_eq!(line_count(&fresh), 35);
        assert_eq!(fresh.summary().unwrap().all_time.records, 35);

        // A prefix receipt re-used with different content is still rejected;
        // the identical row is still an idempotent no-op.
        let mut conflicting = one_receipt("a", 22);
        Arc::get_mut(&mut conflicting.row).unwrap().id = first.row.id;
        assert!(flush_receipt(&Ledger::new(&state), &conflicting).is_err());
        assert_eq!(line_count(&fresh), 35);
        let mut same = one_receipt("a", 5);
        same.row = first.row.clone();
        flush_receipt(&Ledger::new(&state), &same).unwrap();
        assert_eq!(line_count(&fresh), 35);
    }

    #[test]
    fn startup_checkpoint_scans_only_the_tail_and_equals_a_full_scan() {
        let root = crate::tests::OwnedTestDirectory::new("spend-ledger-checkpoint");
        let state = root.path().join("state");
        crate::fs::ensure_private_directory(&state).unwrap();
        let ledger = Ledger::new(&state);
        let checkpoint = state.join(CHECKPOINT_FILE);
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);

        // Randomized appends: after the first scan, each startup parses only
        // the rows appended since the previous one.
        append_random_rows(&ledger, &mut rng, 40);
        let before = tail_rows_parsed();
        assert_equals_full_scan(&ledger, "a");
        assert_eq!(
            tail_rows_parsed() - before,
            40,
            "first scan reads every row"
        );
        assert!(checkpoint.exists(), "a checkpoint is written");
        for round in 0..12 {
            let appended = rng.below(9);
            append_random_rows(&ledger, &mut rng, appended);
            for (index, session) in ["a", "b", "c", "d", "absent"].into_iter().enumerate() {
                let before = tail_rows_parsed();
                assert_equals_full_scan(&ledger, session);
                let expected = if index == 0 { appended as usize } else { 0 };
                assert_eq!(
                    tail_rows_parsed() - before,
                    expected,
                    "round {round}, session {session}: only the tail is parsed"
                );
            }
        }
        let total = line_count(&ledger);

        // A corrupt checkpoint fails closed to a full scan, then is replaced.
        let mut corrupt = std::fs::read(&checkpoint).unwrap();
        let middle = corrupt.len() / 2;
        corrupt[middle] ^= 0x55;
        crate::fs::private_atomic_write_sync(&checkpoint, &corrupt).unwrap();
        let before = tail_rows_parsed();
        assert_equals_full_scan(&ledger, "b");
        assert_eq!(tail_rows_parsed() - before, total);
        crate::fs::private_atomic_write_sync(&checkpoint, b"not a checkpoint").unwrap();
        let before = tail_rows_parsed();
        assert_equals_full_scan(&ledger, "b");
        assert_eq!(tail_rows_parsed() - before, total);
        let before = tail_rows_parsed();
        assert_equals_full_scan(&ledger, "c");
        assert_eq!(
            tail_rows_parsed() - before,
            0,
            "the rewritten checkpoint is valid"
        );

        // Truncation below the checkpoint offset fails closed.
        let bytes = std::fs::read(&ledger.path).unwrap();
        let rows = bytes
            .split_inclusive(|byte| *byte == b'\n')
            .collect::<Vec<_>>();
        let keep: usize = rows[..rows.len() / 2].iter().map(|row| row.len()).sum();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&ledger.path)
            .unwrap()
            .set_len(keep as u64)
            .unwrap();
        let before = tail_rows_parsed();
        assert_equals_full_scan(&ledger, "a");
        assert_eq!(tail_rows_parsed() - before, rows.len() / 2);

        // Truncated and regrown past the old offset in place (same inode):
        // the prefix digest no longer matches, so the whole file is rescanned.
        let covered = std::fs::metadata(&ledger.path).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&ledger.path)
            .unwrap()
            .set_len(rows[0].len() as u64)
            .unwrap();
        while std::fs::metadata(&ledger.path).unwrap().len() <= covered {
            append_random_rows(&ledger, &mut rng, 1);
        }
        let before = tail_rows_parsed();
        assert_equals_full_scan(&ledger, "d");
        assert_eq!(tail_rows_parsed() - before, line_count(&ledger));

        // A replaced ledger (a middle row removed) has a new identity and is
        // rescanned in full.
        append_random_rows(&ledger, &mut rng, 4);
        assert_equals_full_scan(&ledger, "a");
        let bytes = std::fs::read(&ledger.path).unwrap();
        let rows = bytes
            .split_inclusive(|byte| *byte == b'\n')
            .collect::<Vec<_>>();
        let replaced = rows
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != rows.len() / 2)
            .flat_map(|(_, row)| row.iter().copied())
            .collect::<Vec<_>>();
        crate::fs::private_atomic_write_sync(&ledger.path, &replaced).unwrap();
        let before = tail_rows_parsed();
        let summary = assert_equals_full_scan(&ledger, "a").unwrap();
        assert_eq!(tail_rows_parsed() - before, rows.len() - 1);
        assert_eq!(summary.all_time.records, rows.len() - 1);

        // An in-place rewrite of one byte inside the covered prefix (same
        // inode and length) changes the prefix digest: full rescan, which
        // fails exactly as the full scan does.
        assert_equals_full_scan(&ledger, "a");
        let mut bytes = std::fs::read(&ledger.path).unwrap();
        let middle = bytes.len() / 2;
        let original = bytes[middle];
        bytes[middle] = if original == b'1' { b'2' } else { b'1' };
        std::fs::write(&ledger.path, &bytes).unwrap();
        let before = tail_rows_parsed();
        assert_equals_full_scan(&ledger, "a");
        assert!(tail_rows_parsed() - before > 0);
        bytes[middle] = original;
        std::fs::write(&ledger.path, &bytes).unwrap();
        assert_equals_full_scan(&ledger, "a");

        // A duplicate receipt in the tail fails exactly as the full scan does.
        assert_equals_full_scan(&ledger, "a");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&ledger.path)
            .unwrap();
        file.write_all(rows[0]).unwrap();
        drop(file);
        assert!(assert_equals_full_scan(&ledger, "a").is_none());
        // An incomplete tail fails the same way too.
        let mut bytes = std::fs::read(&ledger.path).unwrap();
        bytes.truncate(bytes.len() - rows[0].len());
        bytes.extend_from_slice(b"{\"crash\":");
        std::fs::write(&ledger.path, &bytes).unwrap();
        assert!(assert_equals_full_scan(&ledger, "a").is_none());
    }

    #[test]
    fn spend_ledger_records_only_new_usage_and_repeated_saves_are_idempotent() {
        let root = crate::tests::OwnedTestDirectory::new("spend-ledger");
        let state = root.path().join("state");
        crate::fs::ensure_private_directory(&state).unwrap();
        let ledger = Ledger::new(&state);
        assert_eq!(ledger.summary().unwrap().all_time.records, 0);
        assert!(
            !ledger.path.exists(),
            "read-only summary must not create storage"
        );
        let account = Account::new("s", None, 99.0).unwrap();
        let first = receipt(RequestKind::Title);
        account.observe(&first).unwrap();
        ledger.flush(&account).unwrap();
        let saved = std::fs::read(&ledger.path).unwrap();
        ledger.flush(&account).unwrap();
        assert!(!account.observe(&first.clone()).unwrap());
        ledger.flush(&account).unwrap();
        assert_eq!(std::fs::read(&ledger.path).unwrap(), saved);
        let summary = ledger.summary().unwrap();
        assert_eq!(summary.all_time.known_cost, 2.0);
        assert_eq!(summary.today.known_cost, 2.0);
        assert_eq!(
            summary.all_time.records, 1,
            "historical99USD was never imported"
        );
        assert_eq!(account.snapshot().recorded_spend(), 101.0);
    }
    #[test]
    fn usd_budget_concurrent_profile_receipts_share_io_and_never_double_or_drop_charges() {
        let root = crate::tests::OwnedTestDirectory::new("cost-concurrent-ledger");
        let ledger = Arc::new(Ledger::new(root.path()));
        let account = Account::new("s", None, 3.0).unwrap();
        let prices = PriceSnapshot::capture(Some((1.0, 2.0)), Default::default(), false);
        let run = crate::spend::run::Run::fixture(account.clone(), ledger.clone(), "s", prices);
        std::thread::scope(|scope| {
            let barrier = Arc::new(std::sync::Barrier::new(4));
            for (index, kind) in [
                RequestKind::Turn,
                RequestKind::Title,
                RequestKind::Btw,
                RequestKind::Subagent,
            ]
            .into_iter()
            .enumerate()
            {
                let barrier = barrier.clone();
                let run = run.with_captured_profile(
                    "fixture",
                    &format!("model-{index}"),
                    prices,
                    &crate::config::Config::default(),
                    kind,
                );
                let writer = ledger.clone();
                scope.spawn(move || {
                    barrier.wait();
                    run.observe(UsageDelta {
                        input_tokens: 50,
                        output_tokens: 10,
                        total_tokens: 60,
                        ..Default::default()
                    });
                    writer.flush(&run.account).unwrap();
                });
            }
        });
        assert!(!account.snapshot().tracking_error);
        assert!((account.snapshot().recorded_spend() - 3.00028).abs() < 1e-12);
        let bytes = std::fs::read(&ledger.path).unwrap();
        ledger.flush(&account).unwrap();
        assert_eq!(std::fs::read(&ledger.path).unwrap(), bytes);
        let totals = ledger.summary_for_session("s").unwrap();
        assert_eq!(totals.session.records, 4);
        assert_eq!(totals.session_by_model.len(), 4);
        assert_eq!(totals.session.input_tokens, 200);
        assert_eq!(totals.session.output_tokens, 40);
        assert!((totals.session.known_cost - 0.00028).abs() < 1e-12);
        assert_eq!(totals.today.records, 4);
        assert_eq!(totals.all_time.records, 4);
    }

    #[test]
    fn spend_ledger_corrupt_tail_preserves_bytes_pending_and_incurred_charge() {
        let root = crate::tests::OwnedTestDirectory::new("spend-ledger-tail");
        let state = root.path().join("state");
        crate::fs::ensure_private_directory(&state).unwrap();
        let ledger = Ledger::new(&state);
        crate::fs::private_atomic_create_sync(&ledger.path, b"{\"crash\":").unwrap();
        let account = Account::new("s", Some(10.0), 0.0).unwrap();
        account.observe(&receipt(RequestKind::Turn)).unwrap();
        assert!(ledger.flush(&account).is_err());
        assert_eq!(std::fs::read(&ledger.path).unwrap(), b"{\"crash\":");
        assert_eq!(account.snapshot().observed_cost, 2.0);
        assert_eq!(account.pending().len(), 1);
        assert!(account.admit_effect().is_err());
        assert!(ledger.summary().is_err());
    }
    #[test]
    fn spend_ledger_malformed_or_duplicate_rows_are_not_silently_repaired() {
        let root = crate::tests::OwnedTestDirectory::new("spend-ledger-corrupt");
        let ledger = Ledger::new(root.path());
        let row = receipt(RequestKind::Repair).row;
        let valid = serde_json::to_string(row.as_ref()).unwrap();
        for content in [
            "not-json\n".to_owned(),
            format!("{valid}\n{valid}\n"),
            "x".repeat(MAX_ROW_BYTES + 1) + "\n",
        ] {
            crate::fs::private_atomic_write_sync(&ledger.path, content.as_bytes()).unwrap();
            assert!(ledger.summary().is_err());
            assert_eq!(std::fs::read(&ledger.path).unwrap(), content.as_bytes());
        }
    }
    #[test]
    fn spend_ledger_recovers_post_checkpoint_charges_without_importing_or_doubling_rows() {
        let root = crate::tests::OwnedTestDirectory::new("spend-ledger-recovery");
        let ledger = Ledger::new(root.path());
        let account = Account::new("s", Some(6.0), 3.0).unwrap();
        account.observe(&receipt(RequestKind::Turn)).unwrap();
        ledger.flush(&account).unwrap();
        let saved = serde_json::to_value(&account).unwrap();
        account.observe(&receipt(RequestKind::Title)).unwrap();
        ledger.flush(&account).unwrap();
        let loaded: Account = serde_json::from_value(saved).unwrap();
        let summary = ledger.summary_for_session("s").unwrap();
        loaded.reconcile(Some(6.0), &summary.session).unwrap();
        assert_eq!(loaded.snapshot().recorded_spend(), 7.0);
        assert_eq!(loaded.snapshot().historical_observed_cost, 4.0);
        assert!(loaded.admit_effect().is_err());
        assert!(
            loaded.pending().is_empty(),
            "saved checkpoints cannot mint usage rows"
        );
        assert_eq!(summary.session.records, 2);
        assert_eq!(summary.session_by_model.len(), 1);
        assert_eq!(
            ledger
                .summary_for_session("another-session")
                .unwrap()
                .session
                .records,
            0
        );
        loaded.reconcile(Some(6.0), &summary.session).unwrap();
        assert_eq!(loaded.snapshot().recorded_spend(), 7.0);
    }

    #[test]
    fn spend_ledger_concurrent_lock_is_bounded_and_directory_replacement_is_refused() {
        let root = crate::tests::OwnedTestDirectory::new("spend-ledger-boundary");
        let parent = root.path().join("state");
        crate::fs::ensure_private_directory(&parent).unwrap();
        let ledger = Ledger::new(&parent);
        let account = Account::new("s", None, 0.0).unwrap();
        account.observe(&receipt(RequestKind::Turn)).unwrap();
        ledger.flush(&account).unwrap();
        let holder = crate::fs::open_private_file_checked(
            &ledger.path,
            crate::fs::PrivateFileAccess::Append,
        )
        .unwrap();
        holder.try_lock().unwrap();
        assert!(
            ledger.summary().is_err(),
            "a busy file never waits indefinitely"
        );
        drop(holder);
        assert_eq!(ledger.summary().unwrap().all_time.records, 1);
        #[cfg(not(windows))]
        {
            std::fs::rename(&parent, root.path().join("retained")).unwrap();
            crate::fs::ensure_private_directory(&parent).unwrap();
            crate::fs::private_atomic_create_sync(&ledger.path, b"replacement").unwrap();
            account.observe(&receipt(RequestKind::Repair)).unwrap();
            assert!(ledger.flush(&account).is_err());
            assert!(ledger.summary().is_err());
            assert_eq!(std::fs::read(&ledger.path).unwrap(), b"replacement");
        }
        // Windows pins the directory name by omitting FILE_SHARE_DELETE.
    }

    #[cfg(unix)]
    #[test]
    fn spend_ledger_refuses_public_hardlinked_symlink_and_nonregular_files_without_mutation() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = crate::tests::OwnedTestDirectory::new("spend-ledger-private");
        let ledger = Ledger::new(root.path());
        let victim = root.path().join("victim");
        crate::fs::private_atomic_create_sync(&victim, b"private").unwrap();
        symlink(&victim, &ledger.path).unwrap();
        assert!(ledger.summary().is_err());
        std::fs::remove_file(&ledger.path).unwrap();
        std::fs::hard_link(&victim, &ledger.path).unwrap();
        assert!(ledger.summary().is_err());
        std::fs::remove_file(&ledger.path).unwrap();
        std::fs::write(&ledger.path, b"public\n").unwrap();
        std::fs::set_permissions(&ledger.path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ledger.summary().is_err());
        assert_eq!(
            std::fs::metadata(&ledger.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        std::fs::remove_file(&ledger.path).unwrap();
        std::fs::create_dir(&ledger.path).unwrap();
        assert!(ledger.summary().is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"private");
    }
}
