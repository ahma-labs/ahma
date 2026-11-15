//! A server nobody is using stops by itself (SPEC R-GUI.12).
//!
//! `ahma web` returns to the prompt and leaves its server running, so the
//! question of when it ends cannot be left to a Ctrl-C nobody will press. A
//! listener that is up with no one paired is a door left open: anything on
//! this machine can try codes at it. It therefore stops once no tab has had
//! its event stream open for the idle timeout, counted from the start until a
//! tab first pairs.

use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// How many event streams are open, and since when there have been none.
#[derive(Debug)]
pub struct Idle {
    inner: Mutex<State>,
}

#[derive(Debug)]
struct State {
    open: usize,
    /// When the count last fell to zero (or the start), while it is zero.
    idle_since: Instant,
}

impl Idle {
    /// No stream open, idle from `now`.
    pub fn new(now: Instant) -> Self {
        Self {
            inner: Mutex::new(State {
                open: 0,
                idle_since: now,
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A tab's event stream opened.
    pub fn opened(&self) {
        self.state().open += 1;
    }

    /// A tab's event stream closed, at `now`.
    pub fn closed(&self, now: Instant) {
        let mut st = self.state();
        st.open = st.open.saturating_sub(1);
        if st.open == 0 {
            st.idle_since = now;
        }
    }

    /// Whether, at `now`, no stream has been open for `timeout`.
    pub fn due(&self, now: Instant, timeout: Duration) -> bool {
        let st = self.state();
        st.open == 0 && now.saturating_duration_since(st.idle_since) >= timeout
    }
}

/// Stop the server (`stop` becomes `true`) once `idle` has been due for
/// `timeout`. Returns when it has, or when the server stops for another reason.
pub async fn stop_when_idle(
    idle: std::sync::Arc<Idle>,
    timeout: Duration,
    stop: std::sync::Arc<watch::Sender<bool>>,
) {
    let mut stopped = stop.subscribe();
    // Often enough that the server goes within a second or so of the timeout,
    // and never busier than four times a second.
    let every = (timeout / 4).clamp(Duration::from_millis(250), Duration::from_secs(1));
    let mut tick = tokio::time::interval(every);
    loop {
        if *stopped.borrow_and_update() {
            return;
        }
        tokio::select! {
            _ = tick.tick() => {
                if idle.due(Instant::now(), timeout) {
                    tracing::info!("web: no tab for {timeout:?}; stopping");
                    stop.send_replace(true);
                    return;
                }
            }
            _ = stopped.changed() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const MINUTE: Duration = Duration::from_secs(60);

    /// From the start, with no tab ever paired, the clock runs.
    #[test]
    fn an_unused_server_is_due_after_the_timeout() {
        let t0 = Instant::now();
        let idle = Idle::new(t0);
        assert!(!idle.due(t0 + Duration::from_secs(59), MINUTE));
        assert!(idle.due(t0 + MINUTE, MINUTE));
    }

    /// An open stream is someone watching, however long it lasts; the clock
    /// starts again when the last one closes.
    #[test]
    fn an_open_stream_holds_it_and_the_last_close_restarts_the_clock() {
        let t0 = Instant::now();
        let idle = Idle::new(t0);
        idle.opened();
        idle.opened();
        assert!(!idle.due(t0 + 10 * MINUTE, MINUTE));
        idle.closed(t0 + 10 * MINUTE);
        assert!(!idle.due(t0 + 20 * MINUTE, MINUTE), "one still open");
        idle.closed(t0 + 20 * MINUTE);
        assert!(!idle.due(t0 + 20 * MINUTE + Duration::from_secs(30), MINUTE));
        assert!(idle.due(t0 + 21 * MINUTE, MINUTE));
    }

    /// The watcher stops the server once due, and returns when the server
    /// stops for another reason.
    #[tokio::test]
    async fn the_watcher_stops_the_server_once_due() {
        let idle = Arc::new(Idle::new(Instant::now()));
        let (tx, mut rx) = watch::channel(false);
        let tx = Arc::new(tx);
        let wait =
            ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick);
        tokio::time::timeout(
            wait,
            stop_when_idle(idle, Duration::from_millis(50), tx.clone()),
        )
        .await
        .expect("the watcher returns once due");
        assert!(*rx.borrow_and_update(), "and the server was told to stop");

        let held = Arc::new(Idle::new(Instant::now()));
        held.opened();
        let (tx2, _rx2) = watch::channel(false);
        let tx2 = Arc::new(tx2);
        let watcher = tokio::spawn(stop_when_idle(held, Duration::from_millis(50), tx2.clone()));
        tx2.send_replace(true);
        tokio::time::timeout(wait, watcher)
            .await
            .expect("a stop for another reason ends the watcher")
            .unwrap();
    }
}
