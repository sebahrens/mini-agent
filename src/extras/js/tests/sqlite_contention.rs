//! Owned locks and operation observations for SQLite responsiveness regressions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, mpsc};
use std::thread::{JoinHandle, ThreadId};
use std::time::Duration;

use crate::extras::js::skills::coordinator::IndexCoordinator;

// Only a rescue for broken synchronous implementations, never a latency target.
const HANG_GUARD: Duration = Duration::from_secs(15);

#[derive(Clone, Hash, PartialEq, Eq)]
enum Point {
    Open(PathBuf),
    Refresh(usize),
}

static OBSERVERS: LazyLock<Mutex<HashMap<Point, tokio::sync::oneshot::Sender<ThreadId>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) struct BlockingProbe {
    point: Point,
    observed: tokio::sync::oneshot::Receiver<ThreadId>,
}

impl BlockingProbe {
    fn new(point: Point) -> Self {
        let (sender, observed) = tokio::sync::oneshot::channel();
        assert!(
            OBSERVERS
                .lock()
                .unwrap()
                .insert(point.clone(), sender)
                .is_none()
        );
        Self { point, observed }
    }
}

impl Drop for BlockingProbe {
    fn drop(&mut self) {
        OBSERVERS.lock().unwrap().remove(&self.point);
    }
}

fn observe(point: Point) {
    if let Some(sender) = OBSERVERS.lock().unwrap().remove(&point) {
        let _ = sender.send(std::thread::current().id());
    }
}

pub(crate) fn watch_open(path: PathBuf) -> BlockingProbe {
    BlockingProbe::new(Point::Open(path))
}

pub(crate) fn observe_open(path: &Path) {
    observe(Point::Open(path.to_owned()));
}

pub(crate) fn watch_refresh(coordinator: &IndexCoordinator) -> BlockingProbe {
    BlockingProbe::new(Point::Refresh(
        coordinator as *const IndexCoordinator as usize,
    ))
}

pub(crate) fn observe_refresh(coordinator: &IndexCoordinator) {
    observe(Point::Refresh(
        coordinator as *const IndexCoordinator as usize,
    ));
}

pub(crate) struct LockWait {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

impl LockWait {
    /// Call only after acquiring the real database transaction or store mutex.
    pub(crate) fn wait(self) -> bool {
        self.entered.send(()).expect("announce owned lock");
        self.release.recv_timeout(HANG_GUARD).is_ok()
    }
}

pub(crate) struct HeldTestLock {
    entered: mpsc::Receiver<()>,
    release: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<bool>>,
}

impl HeldTestLock {
    pub(crate) fn spawn(holder: impl FnOnce(LockWait) -> bool + Send + 'static) -> Self {
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            holder(LockWait {
                entered: entered_tx,
                release: release_rx,
            })
        });
        Self {
            entered,
            release: Some(release),
            thread: Some(thread),
        }
    }

    pub(crate) fn wait_until_held(&self) {
        self.entered
            .recv_timeout(HANG_GUARD)
            .expect("fixture must acquire its lock");
    }

    fn release_and_join(mut self) -> bool {
        let _ = self.release.take().unwrap().send(());
        self.thread
            .take()
            .unwrap()
            .join()
            .expect("lock holder panicked")
    }
}

impl Drop for HeldTestLock {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.expect("lock holder panicked");
            }
        }
    }
}

/// Drive the real operation until its blocking boundary is observed. The lock
/// stays held until this current-thread async task chooses to release it.
pub(crate) async fn run_while_held<F: std::future::Future>(
    holder: HeldTestLock,
    mut probe: BlockingProbe,
    work: F,
) -> F::Output {
    let executor_thread = std::thread::current().id();
    tokio::pin!(work);
    let mut early_result = None;
    let observation = tokio::time::timeout(HANG_GUARD, async {
        tokio::select! {
            biased;
            result = &mut work => { early_result = Some(result); None }
            thread = &mut probe.observed => Some(thread),
        }
    })
    .await;
    let released_by_test = holder.release_and_join();
    let finished_early = early_result.is_some();
    let result = match early_result {
        Some(result) => result,
        None => tokio::time::timeout(HANG_GUARD, &mut work)
            .await
            .expect("operation must settle after releasing the real lock"),
    };
    assert!(
        released_by_test,
        "synchronous work required the lock-holder rescue"
    );
    assert!(
        !finished_early,
        "operation completed before the held lock was released"
    );
    let thread = observation
        .expect("blocking operation was never observed")
        .expect("operation completed without reaching the blocking boundary")
        .expect("blocking observer was abandoned");
    assert_ne!(
        thread, executor_thread,
        "blocking operation ran on the async executor"
    );
    result
}

#[test]
fn held_lock_drop_joins_on_normal_exit_and_unwinding() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    for unwind in [false, true] {
        let finished = Arc::new(AtomicBool::new(false));
        let observed = finished.clone();
        let holder = HeldTestLock::spawn(move |wait| {
            let released = wait.wait();
            observed.store(true, Ordering::Release);
            released
        });
        holder.wait_until_held();
        assert!(
            !finished.load(Ordering::Acquire),
            "fixture lock was not held"
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _holder = holder;
            if unwind {
                panic!("injected contention fixture failure");
            }
        }));
        assert_eq!(result.is_err(), unwind);
        assert!(finished.load(Ordering::Acquire), "holder was not joined");
    }
}
