//! Per-server idle admission and whole-task ownership. Session queues are not
//! task completion: goal verification and terminal cleanup can outlive them.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use agent_client_protocol::Error;
use tokio::sync::Notify;
use tokio::time::Instant;

pub(super) struct Idle {
    timeout: Option<Duration>,
    state: Mutex<State>,
    changed: Notify,
}

struct State {
    active: usize,
    sessions: usize,
    since: Instant,
    retiring: bool,
}

/// Clones share one admitted activity. The last owner, including a spawned
/// task's captured future, ends it; dropping a connection waiter is insufficient.
#[derive(Default)]
pub(super) struct Activity(Option<Arc<Lease>>);

impl Clone for Activity {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

struct Lease(Arc<Idle>);

/// Field order keeps all future-owned resources alive inside activity, even
/// when the runtime cancels the task before its first poll.
pub(super) struct OwnedFuture<F> {
    future: Pin<Box<F>>,
    _activity: Activity,
}

impl Activity {
    pub(super) fn track<F: Future>(self, future: F) -> OwnedFuture<F> {
        OwnedFuture {
            future: Box::pin(future),
            _activity: self,
        }
    }
}

impl<F: Future> Future for OwnedFuture<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().future.as_mut().poll(cx)
    }
}

impl Idle {
    pub(super) fn new(timeout: Option<Duration>) -> Result<Arc<Self>, Error> {
        let now = Instant::now();
        if timeout.is_some_and(|timeout| timeout.is_zero() || now.checked_add(timeout).is_none()) {
            return Err(Error::new(
                -32602,
                "idle-exit timeout is outside the clock range",
            ));
        }
        Ok(Arc::new(Self {
            timeout,
            state: Mutex::new(State {
                active: 0,
                sessions: 0,
                since: now,
                retiring: false,
            }),
            changed: Notify::new(),
        }))
    }

    pub(super) fn enabled(&self) -> bool {
        self.timeout.is_some()
    }

    pub(super) fn admit(self: &Arc<Self>) -> Result<Activity, Error> {
        if !self.enabled() {
            return Ok(Activity::default());
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.retiring {
            return Err(Error::new(-32000, "ACP server is shutting down after idle"));
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| Error::new(-32000, "ACP activity capacity is full"))?;
        let activity = Activity(Some(Arc::new(Lease(self.clone()))));
        drop(state);
        self.changed.notify_waiters();
        Ok(activity)
    }

    /// Called under the actual session catalogue lock while its request lease
    /// is still held. The timer never acquires the asynchronous catalogue lock.
    pub(super) fn sessions_changed(&self, sessions: usize) {
        if !self.enabled() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.sessions = sessions;
        drop(state);
        self.changed.notify_waiters();
    }

    pub(super) async fn expired(&self) {
        let Some(timeout) = self.timeout else {
            std::future::pending::<()>().await;
            return;
        };
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let deadline = {
                let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                if state.retiring {
                    return;
                }
                if state.active == 0 && state.sessions == 0 {
                    state.since.checked_add(timeout)
                } else {
                    None
                }
            };
            if let Some(deadline) = deadline {
                tokio::select! {
                    () = &mut changed => {},
                    () = tokio::time::sleep_until(deadline) => {
                        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                        if state.active == 0 && state.sessions == 0
                            && Instant::now().duration_since(state.since) >= timeout
                        {
                            // Admission and shutdown commit under the same lock.
                            state.retiring = true;
                            return;
                        }
                    }
                }
            } else {
                changed.await;
            }
        }
    }

    /// After transport teardown, actual prompt/auth futures still own leases.
    /// Wait for their cleanup instead of treating dropped JoinHandles as joins.
    pub(super) async fn wait_settled(&self) {
        if !self.enabled() {
            return;
        }
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .active
                == 0
            {
                return;
            }
            changed.await;
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.active -= 1;
        if state.active == 0 {
            state.since = Instant::now();
        }
        drop(state);
        self.0.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    #[tokio::test]
    async fn idle_exit_unpolled_auth_resources_drop_before_activity() {
        struct Resource(Arc<Idle>);
        impl Drop for Resource {
            fn drop(&mut self) {
                assert!(
                    self.0.state.lock().unwrap().active > 0,
                    "resource cleanup outlived activity"
                );
            }
        }
        let idle = Idle::new(Some(Duration::from_secs(3))).unwrap();
        let resource = Resource(idle.clone());
        let future = idle.admit().unwrap().track(async move {
            let _resource = resource;
            std::future::pending::<()>().await;
        });
        drop(future);
        idle.wait_settled().await;
    }

    #[tokio::test(start_paused = true)]
    async fn idle_exit_admission_and_shutdown_have_one_winner() {
        let idle = Idle::new(Some(Duration::from_secs(3))).unwrap();
        let activity = idle.admit().unwrap();
        let task_owner = activity.clone();
        drop(activity);
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(idle.expired().now_or_never().is_none());
        assert!(idle.wait_settled().now_or_never().is_none());
        drop(task_owner);
        assert!(idle.expired().now_or_never().is_none());
        tokio::time::advance(Duration::from_secs(3)).await;
        idle.expired().await;
        assert!(idle.admit().is_err(), "no new effects after timeout won");
        idle.wait_settled().await;
    }

    #[tokio::test(start_paused = true)]
    async fn idle_exit_catalogue_and_last_task_both_prevent_shutdown() {
        let idle = Idle::new(Some(Duration::from_secs(3))).unwrap();
        let request = idle.admit().unwrap();
        idle.sessions_changed(1);
        drop(request);
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(idle.expired().now_or_never().is_none());
        let cleanup = idle.admit().unwrap();
        idle.sessions_changed(0);
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(idle.expired().now_or_never().is_none());
        drop(cleanup);
        tokio::time::advance(Duration::from_secs(2)).await;
        let newly_admitted = idle.admit().unwrap();
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(idle.expired().now_or_never().is_none());
        drop(newly_admitted);
        tokio::time::advance(Duration::from_secs(3)).await;
        idle.expired().await;
    }
}
