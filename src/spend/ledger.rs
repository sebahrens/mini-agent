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
}

struct Scan {
    bytes: usize,
    records: usize,
    identities: BTreeMap<Uuid, [u8; 32]>,
    summary: LedgerSummary,
}

/// One ledger's directory authority, not a global registry. Read-only summaries
/// never create a file/directory or repair permissions. Missing storage is empty.
#[derive(Debug)]
pub(crate) struct Ledger {
    path: PathBuf,
    directory: Mutex<Option<Arc<crate::paths::WorkspaceBinding>>>,
    io: Mutex<()>,
    #[cfg(test)]
    pre_sync_gate: Option<Arc<PreSyncGate>>,
    #[cfg(test)]
    lock_blocked: Option<std::sync::mpsc::SyncSender<()>>,
    #[cfg(test)]
    summary_scanned: Option<SummaryScanGate>,
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
        std::fs::write(
            self.root.join("started"),
            serde_json::to_vec(&account.snapshot()).unwrap(),
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
            #[cfg(test)]
            pre_sync_gate: PRE_SYNC_GATE.try_with(Clone::clone).ok(),
            #[cfg(test)]
            lock_blocked: None,
            #[cfg(test)]
            summary_scanned: None,
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
    pub(crate) fn summary_for_session(&self, session_id: &str) -> anyhow::Result<LedgerSummary> {
        self.summary_with_session(Some(session_id))
    }
    fn summary_with_session(&self, session_id: Option<&str>) -> anyhow::Result<LedgerSummary> {
        let _io = self
            .io
            .lock()
            .map_err(|_| anyhow::anyhow!("usage ledger serialization failed"))?;
        let Some((mut file, binding)) = self.open(false)? else {
            return Ok(LedgerSummary::default());
        };
        file.try_lock_shared()
            .map_err(|_| anyhow::anyhow!("usage ledger is busy"))?;
        Self::revalidate(&binding, &file)?;
        let scan = scan(&mut file, session_id)?;
        Self::revalidate(&binding, &file)?;
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
    fn flush_rows(&self, account: &Account, pending: &[Arc<Row>]) -> anyhow::Result<()> {
        let (mut file, binding) = self
            .open(true)?
            .ok_or_else(|| anyhow::anyhow!("usage ledger unavailable"))?;
        self.lock_writer(&binding, &file)?;
        Self::revalidate(&binding, &file)?;
        let mut scanned = scan(&mut file, None)?;
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

fn scan(file: &mut File, session_id: Option<&str>) -> anyhow::Result<Scan> {
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
        summary: LedgerSummary::default(),
    };
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
        let fingerprint = Sha256::digest(serde_json::to_vec(&row)?).into();
        anyhow::ensure!(
            scanned.identities.insert(row.id, fingerprint).is_none(),
            "usage ledger contains duplicate receipts"
        );
        scanned.summary.all_time.add(&row)?;
        if row.ts.date_naive() == today {
            scanned.summary.today.add(&row)?;
        }
        if session_id == Some(row.session_id.as_str()) {
            scanned.summary.session.add(&row)?;
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
    fn spend_ledger_records_only_new_usage_and_repeated_saves_are_idempotent() {
        let root = crate::tests::OwnedTestDirectory::new("spend-ledger");
        let ledger = Ledger::new(root.path());
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
        let ledger = Ledger::new(root.path());
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
