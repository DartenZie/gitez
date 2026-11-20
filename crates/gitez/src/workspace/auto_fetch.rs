//! Fetching from the remotes in the background, so new commits on them show
//! without asking.
//!
//! The repository fetches every [`Snapshot::auto_fetch`] (five minutes
//! unless `gitez.autoFetch` or `gitez.autoFetchMinutes` say otherwise),
//! counted from the last fetch, whoever ran it. Nothing is shown while it
//! runs but the status bar; it gives way to a fetch, pull or push the user
//! starts, and it never asks for a password: a remote that needs one fails,
//! and that is told once until a fetch works again.

use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::Duration;

use gitez_command::{Error, Git, Operation};
use gpui_kit::component::*;
use gpui_kit::*;

use super::Workspace;
use crate::repository::Snapshot;
use crate::time;

/// The shortest wait between two checks, so a fetch is never started in a
/// tight loop, and the first fetch after opening waits for the window.
const MIN_WAIT: Duration = Duration::from_secs(5);

/// How long to wait before looking again while automatic fetching is off.
const OFF_WAIT: Duration = Duration::from_secs(60);

/// How long a background fetch may take before it is cancelled: one stuck
/// on a prompt nobody sees would otherwise hold the remotes forever.
const TIMEOUT: Duration = Duration::from_secs(120);

/// Held while git talks to a remote, so a background fetch and an operation
/// the user started never run at once.
#[derive(Clone, Default)]
pub(super) struct RemoteLock(Arc<Mutex<()>>);

impl RemoteLock {
    /// Waits for the remotes to be free. Called off the UI thread.
    pub fn lock(&self) -> MutexGuard<'_, ()> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The remotes, if they are free now.
    fn try_lock(&self) -> Option<MutexGuard<'_, ()>> {
        match self.0.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }
}

/// The background fetch running.
pub(super) struct AutoFetching {
    /// Ends the git it runs.
    pub operation: Operation,
    _run: Task<()>,
    _timeout: Task<()>,
}

/// The state of background fetching in a workspace.
#[derive(Default)]
pub(super) struct AutoFetch {
    /// Wakes up to fetch when one is due.
    _schedule: Option<Task<()>>,
    /// The fetch running, if one is.
    pub running: Option<AutoFetching>,
    /// When the last background fetch started, in seconds since the epoch,
    /// so one that failed is not tried again at once.
    attempted: Option<i64>,
    /// The last error shown, so a remote that keeps failing is told once.
    error_shown: bool,
}

/// How long to wait from `now` before fetching again every `interval`,
/// after the last fetch at `fetched` and the last attempt at `attempted`
/// (seconds since the epoch).
fn wait(interval: Duration, fetched: Option<i64>, attempted: Option<i64>, now: i64) -> Duration {
    let Some(last) = fetched.max(attempted) else {
        return Duration::ZERO;
    };
    let interval = interval.as_secs();
    let elapsed = u64::try_from(now.saturating_sub(last)).unwrap_or(0);
    Duration::from_secs(interval.saturating_sub(elapsed).min(interval))
}

impl Workspace {
    /// Starts fetching `snapshot`'s repository in the background, replacing
    /// any earlier schedule.
    pub(super) fn schedule_auto_fetch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let schedule = cx.spawn_in(window, async move |this, cx| {
            loop {
                let Ok(wait) = this.update(cx, |this, _| this.auto_fetch_wait()) else {
                    return;
                };
                cx.background_executor().timer(wait.max(MIN_WAIT)).await;
                if this
                    .update_in(cx, |this, window, cx| this.auto_fetch_if_due(window, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        self.cancel_auto_fetch();
        self.auto_fetch = AutoFetch {
            _schedule: Some(schedule),
            ..AutoFetch::default()
        };
    }

    /// How long until the next background fetch is due.
    fn auto_fetch_wait(&self) -> Duration {
        let Some(snapshot) = &self.snapshot else {
            return OFF_WAIT;
        };
        let Some(interval) = snapshot.auto_fetch else {
            return OFF_WAIT;
        };
        wait(
            interval,
            snapshot.fetched,
            self.auto_fetch.attempted,
            time::now(),
        )
    }

    /// Whether the repository should fetch in the background now.
    fn auto_fetch_due(&self, snapshot: &Snapshot) -> bool {
        snapshot.auto_fetch.is_some()
            && !snapshot.remotes.is_empty()
            && self.remote_op.is_none()
            && self.auto_fetch.running.is_none()
            && self.loading.is_none()
            && self.auto_fetch_wait().is_zero()
    }

    /// Fetches from every remote in the background, if that is due.
    fn auto_fetch_if_due(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        if !self.auto_fetch_due(&snapshot) {
            return;
        }
        self.auto_fetch.attempted = Some(time::now());
        let dir = snapshot.path.clone();
        let operation = Operation::new();
        let lock = self.remote_lock.clone();
        let run = cx.background_executor().spawn({
            let operation = operation.clone();
            async move {
                // Something else talks to the remotes: it is not due.
                let _remotes = lock.try_lock()?;
                Some(Git::new(dir).with_operation(&operation).fetch_unattended())
            }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = run.await;
            this.update_in(cx, |this, window, cx| {
                this.auto_fetch.running = None;
                if this
                    .snapshot
                    .as_ref()
                    .is_some_and(|open| open.path == snapshot.path)
                {
                    this.auto_fetched(result, window, cx);
                }
                cx.notify();
            })
            .ok();
        });
        let timeout = cx.background_executor().spawn({
            let operation = operation.clone();
            let timer = cx.background_executor().timer(TIMEOUT);
            async move {
                timer.await;
                operation.cancel();
            }
        });
        self.auto_fetch.running = Some(AutoFetching {
            operation,
            _run: task,
            _timeout: timeout,
        });
        cx.notify();
    }

    /// Tells about a background fetch that ended with `result`, `None` if
    /// it did not run, and shows what it brought.
    fn auto_fetched(
        &mut self,
        result: Option<gitez_command::Result<()>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            None => {}
            Some(Ok(())) => {
                self.auto_fetch.error_shown = false;
                self.reload(window, cx);
            }
            // Given way to the user's own operation, or timed out; the
            // next one tries again.
            Some(Err(Error::Cancelled { .. })) => {}
            Some(Err(error)) if !self.auto_fetch.error_shown => {
                self.auto_fetch.error_shown = true;
                window.push_notification(
                    crate::toast::warning(format!("Could not fetch in the background: {error}")),
                    cx,
                );
            }
            // Told already.
            Some(Err(_)) => {}
        }
    }

    /// Ends the background fetch, if one runs, for an operation the user
    /// started.
    pub(super) fn cancel_auto_fetch(&mut self) {
        if let Some(running) = &self.auto_fetch.running {
            running.operation.cancel();
        }
    }

    /// Whether a background fetch is running.
    pub(super) fn auto_fetching(&self) -> bool {
        self.auto_fetch.running.is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::wait;

    const FIVE_MINUTES: Duration = Duration::from_secs(300);

    #[test]
    fn fetches_at_once_when_never_fetched() {
        assert_eq!(wait(FIVE_MINUTES, None, None, 1_000), Duration::ZERO);
    }

    #[test]
    fn waits_out_the_interval_from_the_last_fetch_or_attempt() {
        let secs = Duration::from_secs;
        assert_eq!(wait(FIVE_MINUTES, Some(1_000), None, 1_100), secs(200));
        assert_eq!(
            wait(FIVE_MINUTES, Some(1_000), Some(1_200), 1_300),
            secs(200)
        );
        assert_eq!(wait(FIVE_MINUTES, None, Some(1_000), 1_300), Duration::ZERO);
        assert_eq!(wait(FIVE_MINUTES, Some(1_000), None, 9_000), Duration::ZERO);
    }

    #[test]
    fn a_fetch_in_the_future_waits_one_interval_at_most() {
        assert_eq!(wait(FIVE_MINUTES, Some(9_000), None, 1_000), FIVE_MINUTES);
    }
}
