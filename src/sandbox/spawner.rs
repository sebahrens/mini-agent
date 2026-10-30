//! A single process-lifetime thread that performs `Command::spawn()` for
//! `--die-with-parent` JavaScript worker children (mini-agent-hdl3i).
//!
//! `PR_SET_PDEATHSIG` — which bubblewrap's `--die-with-parent` installs during its own
//! startup — is delivered when the OS *thread* that was the child's parent at
//! clone()/fork() time exits, not when the owning process exits (`man 2 prctl`: "the
//! parent-death signal is sent upon subsequent termination of the parent thread"). The
//! production JS worker launch used to perform the bwrap `Command::spawn()` on a
//! transient `std::thread` that delivered the resulting [`WorkerProcess`] over a channel
//! and then exited immediately. If bwrap installed its parent-death signal before that
//! transient thread's ordinary exit, the kernel SIGKILLed the freshly launched worker
//! (and, inside its own PID namespace, bwrap's init and the contained worker with it).
//! The race is timing-dependent, so it is intermittent and worse under CPU contention.
//!
//! The fix is to perform the actual spawn syscall on a single, lazily started thread
//! that never exits for the life of the process. Every `--die-with-parent` launch then
//! has the same kernel-tracked parent thread regardless of which (possibly transient)
//! caller thread requested the launch, so an ordinary launch-thread exit can never race
//! bwrap's own startup. Only the `Command::spawn()` call is routed through this thread;
//! the rest of `launch()` (containment preflight, trusted-path resolution, pipe
//! handling, protocol handshake) stays on its existing caller, which keeps this module
//! small, keeps the existing launch-lease/cancellation machinery in
//! `src/extras/js/supervisor.rs` unchanged, and keeps the spawner reusable for any
//! future `--die-with-parent` child that needs the same guarantee.
//!
//! If the spawner thread fails to start, or ever dies or panics while handling a job,
//! every subsequent (and the in-flight) spawn request fails closed with an I/O error.
//! There is no fallback to spawning on the caller's thread: a fallback would silently
//! reintroduce the exact race this module exists to remove.

use std::io;
use std::process::{Child, Command};
use std::sync::OnceLock;
use std::sync::mpsc;

/// Name of the dedicated, never-exiting spawner thread. Stable across the process
/// lifetime so tests can assert that every launch funnels through the same thread.
const SPAWNER_THREAD_NAME: &str = "mini-agent-js-worker-spawner";

struct SpawnJob {
    command: Command,
    reply: mpsc::Sender<io::Result<Child>>,
}

static SPAWNER_CHANNEL: OnceLock<Option<mpsc::Sender<SpawnJob>>> = OnceLock::new();

#[cfg(test)]
type ObservedSpawningThreads = std::sync::Mutex<Vec<(std::thread::ThreadId, Option<String>)>>;

#[cfg(test)]
static OBSERVED_SPAWNING_THREADS: OnceLock<ObservedSpawningThreads> = OnceLock::new();

#[cfg(test)]
fn record_spawning_thread_for_test() {
    let cell = OBSERVED_SPAWNING_THREADS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut observed = cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    observed.push((
        std::thread::current().id(),
        std::thread::current().name().map(str::to_owned),
    ));
}

/// Test-only observation of every thread that has actually executed a spawn job.
/// Asserting these are all identical (and named [`SPAWNER_THREAD_NAME`]) is how the
/// dedicated-thread regression proves every launch funneled through one stable,
/// long-lived thread rather than whichever transient caller requested it.
#[cfg(test)]
pub(crate) fn observed_spawning_threads_for_test() -> Vec<(std::thread::ThreadId, Option<String>)> {
    OBSERVED_SPAWNING_THREADS
        .get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Returns the channel to the dedicated spawner thread, lazily starting it on first
/// use. `None` means the thread could not be started; callers must fail closed rather
/// than spawning on their own thread.
fn spawner_channel() -> Option<&'static mpsc::Sender<SpawnJob>> {
    SPAWNER_CHANNEL
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel::<SpawnJob>();
            std::thread::Builder::new()
                .name(SPAWNER_THREAD_NAME.into())
                .spawn(move || spawner_loop(receiver))
                .ok()
                .map(|_handle| sender)
        })
        .as_ref()
}

/// Body of the dedicated spawner thread. Blocks on the channel for the life of the
/// process (the channel never closes: the sender lives in the static above), so this
/// thread is never the one that exits out from under a freshly spawned
/// `--die-with-parent` child.
fn spawner_loop(receiver: mpsc::Receiver<SpawnJob>) {
    for mut job in receiver {
        #[cfg(test)]
        record_spawning_thread_for_test();
        let result = job.command.spawn();
        // The caller may have stopped waiting; a disconnected reply channel only drops
        // the freshly spawned child's handle here, it does not leave the process behind
        // unsupervised. Production launch callers always wait for this reply.
        let _ = job.reply.send(result);
    }
}

/// Spawns `command` on the dedicated, process-lifetime spawner thread so a
/// `--die-with-parent` child's kernel-tracked parent thread is pinned to a thread that
/// never exits for the life of the process, never to whichever (possibly transient)
/// thread called this function.
///
/// Fails closed with an I/O error if the spawner thread could not be started, has
/// disconnected, or does not reply; it never falls back to spawning on the calling
/// thread.
pub(crate) fn spawn_die_with_parent_child(command: Command) -> io::Result<Child> {
    let Some(channel) = spawner_channel() else {
        return Err(io::Error::other(
            "the process-lifetime spawner thread for die-with-parent children could not be started",
        ));
    };
    let (reply_sender, reply_receiver) = mpsc::channel();
    let job = SpawnJob {
        command,
        reply: reply_sender,
    };
    channel.send(job).map_err(|_| {
        io::Error::other(
            "the process-lifetime spawner thread for die-with-parent children has exited",
        )
    })?;
    reply_receiver.recv().map_err(|_| {
        io::Error::other(
            "the process-lifetime spawner thread for die-with-parent children stopped before replying",
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (a) Every spawn — including ones requested from distinct, transient caller
    /// threads that exit immediately after requesting the launch — executes on one
    /// stable, long-lived thread with a fixed name. This is the property the
    /// `PR_SET_PDEATHSIG` fix depends on: the thread that performs `Command::spawn()`
    /// for a `--die-with-parent` child must never be a thread that goes on to exit
    /// out from under that child.
    #[test]
    fn every_spawn_runs_on_the_same_dedicated_thread() {
        const ATTEMPTS: usize = 8;
        for attempt in 0..ATTEMPTS {
            let handle = std::thread::Builder::new()
                .name(format!("transient-test-caller-{attempt}"))
                .spawn(|| {
                    let mut child = spawn_die_with_parent_child(Command::new("true"))
                        .expect("spawning a trivial command must succeed");
                    child.wait().expect("trivial command must exit");
                })
                .expect("failed to start a transient test caller thread");
            // The transient caller thread exits here; the dedicated spawner thread must
            // not have exited with it.
            handle
                .join()
                .expect("transient test caller thread panicked");
        }

        let observed = observed_spawning_threads_for_test();
        // `OBSERVED_SPAWNING_THREADS` is shared by every test in this process, including
        // ones libtest runs concurrently in other threads, so this can only assert a
        // lower bound on the count. Every one of those other spawns went through the
        // same dedicated thread too, so they cannot break the identity check below.
        assert!(
            observed.len() >= ATTEMPTS,
            "expected at least one recorded spawn per attempt, got {}",
            observed.len()
        );
        let (first_id, first_name) = &observed[0];
        for (id, name) in &observed {
            assert_eq!(id, first_id, "every spawn must run on the same thread id");
            assert_eq!(
                name, first_name,
                "every spawn must run on the same named thread"
            );
        }
        assert_eq!(
            first_name.as_deref(),
            Some(SPAWNER_THREAD_NAME),
            "the dedicated spawner thread must keep its documented name"
        );
    }

    /// A trivial command spawns and exits successfully through the dedicated thread,
    /// proving the happy path (not just the thread-identity bookkeeping) works.
    #[test]
    fn spawn_die_with_parent_child_runs_a_trivial_command() {
        let mut child = spawn_die_with_parent_child(Command::new("true"))
            .expect("spawning a trivial command must succeed");
        let status = child.wait().expect("trivial command must exit");
        assert!(status.success());
    }
}
