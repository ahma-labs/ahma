use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Payload sent with enhanced Ahma keep-alive / heartbeat notifications.
///
/// The session-health fields (`pending_grants`, `reconnects`) are `#[serde(default)]`
/// so old and new peers stay wire-compatible in both directions: an old payload
/// deserializes here with zeros, and an old peer ignores the extra fields
/// (issue #485, `docs/session-health-notifications.md` §3.1).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct HeartbeatPayload {
    /// Application version (e.g., "0.11.9")
    pub version: String,
    /// Secure hash or build ID of the executable
    pub hash: String,
    /// Timestamp for latency calculation and deduplication
    pub timestamp: u64,
    /// Sandbox scope grants currently awaiting a human decision. Filled by the
    /// server from its `GrantCoordinator`; `0` when none (or an old server).
    #[serde(default)]
    pub pending_grants: u32,
    /// Transparent bridge reconnects performed this session. Overlaid by the
    /// stdio proxy — the server behind it cannot know; `0` from the server.
    #[serde(default)]
    pub reconnects: u32,
}

/// Abstract trait for sending a keep-alive signal over a specific transport.
pub trait KeepAlive {
    /// Send a standard MCP `ping` method request (for 3rd-party compatibility)
    fn send_standard_ping(&self) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Send an enhanced Ahma heartbeat notification (`notifications/ahma/heartbeat`)
    fn send_enhanced_heartbeat(
        &self,
        payload: HeartbeatPayload,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Check if the connection has received any data recently.
    /// Should return the duration since the last received byte or JSON-RPC message.
    fn time_since_last_received(&self) -> Duration;

    /// The maximum allowed time without receiving any data before the connection is considered dead.
    fn heartbeat_timeout(&self) -> Duration;

    /// True if the peer has been identified as an Ahma node (so we can send enhanced heartbeats).
    fn is_ahma_peer(&self) -> bool;

    /// True if the transport supports Server-to-Client `ping` requests properly.
    /// Some HTTP transports might only support passive timeouts for 3rd-party clients.
    fn supports_active_pings(&self) -> bool {
        true
    }

    /// Action to take when the connection is deemed dead
    fn on_timeout(&self) -> impl std::future::Future<Output = ()> + Send;
}

/// Helper function to get the current timestamp in milliseconds
pub fn current_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_millis() as u64
}

/// Spawns a background task that periodically checks connection health and sends keep-alives.
pub fn spawn_keepalive_task<T: KeepAlive + Send + Sync + 'static>(
    connection: Arc<T>,
    last_sent_signal: Arc<AtomicU64>,
    executable_version: String,
    executable_hash: String,
) {
    let timeout = connection.heartbeat_timeout();
    let tickle_interval = timeout / 3;

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tickle_interval).await;

            let idle_duration = connection.time_since_last_received();

            if idle_duration > timeout {
                tracing::warn!(
                    "KeepAlive timeout exceeded (idle for {}s). Terminating connection.",
                    idle_duration.as_secs()
                );
                connection.on_timeout().await;
                break;
            }

            // Optimization: Only send tickle if we haven't sent any data recently
            let now = current_timestamp_ms();
            let last_sent = last_sent_signal.load(Ordering::Relaxed);
            let time_since_last_sent = Duration::from_millis(now.saturating_sub(last_sent));

            if time_since_last_sent >= tickle_interval {
                let payload = HeartbeatPayload {
                    version: executable_version.clone(),
                    hash: executable_hash.clone(),
                    timestamp: now,
                    ..Default::default()
                };

                let res = if connection.is_ahma_peer() {
                    connection.send_enhanced_heartbeat(payload).await
                } else if connection.supports_active_pings() {
                    connection.send_standard_ping().await
                } else {
                    Ok(()) // Passive monitoring only
                };

                if res.is_err() {
                    tracing::debug!("Failed to send keep-alive signal. Connection likely closed.");
                    break;
                }

                last_sent_signal.store(current_timestamp_ms(), Ordering::Relaxed);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    struct MockKeepAlive {
        pings_sent: Arc<AtomicU64>,
        heartbeats_sent: Arc<Mutex<Vec<HeartbeatPayload>>>,
        time_since_last_received: Arc<Mutex<Duration>>,
        heartbeat_timeout: Duration,
        is_ahma_peer: bool,
        supports_active_pings: bool,
        timeout_tx: mpsc::Sender<()>,
        fail_sends: bool,
    }

    impl KeepAlive for MockKeepAlive {
        async fn send_standard_ping(&self) -> anyhow::Result<()> {
            if self.fail_sends {
                return Err(anyhow::anyhow!("send failed"));
            }
            self.pings_sent.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn send_enhanced_heartbeat(&self, payload: HeartbeatPayload) -> anyhow::Result<()> {
            if self.fail_sends {
                return Err(anyhow::anyhow!("send failed"));
            }
            self.heartbeats_sent.lock().unwrap().push(payload);
            Ok(())
        }

        fn time_since_last_received(&self) -> Duration {
            *self.time_since_last_received.lock().unwrap()
        }

        fn heartbeat_timeout(&self) -> Duration {
            self.heartbeat_timeout
        }

        fn is_ahma_peer(&self) -> bool {
            self.is_ahma_peer
        }

        fn supports_active_pings(&self) -> bool {
            self.supports_active_pings
        }

        async fn on_timeout(&self) {
            let _ = self.timeout_tx.send(()).await;
        }
    }

    #[test]
    fn test_current_timestamp_ms() {
        let ts1 = current_timestamp_ms();
        std::thread::sleep(Duration::from_millis(2));
        let ts2 = current_timestamp_ms();
        assert!(ts2 >= ts1);
        assert!(ts1 > 0);
    }

    // Use start_paused=true + advance() instead of real sleep for deterministic
    // timing on all platforms (Windows timer resolution is ~15ms, which makes
    // small real sleeps unreliable in CI).
    //
    // Pattern: yield_now() once after spawn so the task creates its first
    // sleep(), THEN advance() past it — otherwise the sleep is registered
    // after the clock already moved and won't fire until the next interval.
    //
    // Note: current_timestamp_ms() uses SystemTime::now() (real wall-clock),
    // unaffected by tokio time pause — so rate-limit arithmetic still works.

    #[tokio::test(start_paused = true)]
    async fn test_enhanced_heartbeat_sent() {
        let pings_sent = Arc::new(AtomicU64::new(0));
        let heartbeats_sent = Arc::new(Mutex::new(Vec::new()));
        let time_since_last_received = Arc::new(Mutex::new(Duration::ZERO));
        let (timeout_tx, _timeout_rx) = mpsc::channel(1);

        let conn = Arc::new(MockKeepAlive {
            pings_sent: pings_sent.clone(),
            heartbeats_sent: heartbeats_sent.clone(),
            time_since_last_received: time_since_last_received.clone(),
            heartbeat_timeout: Duration::from_millis(30),
            is_ahma_peer: true,
            supports_active_pings: true,
            timeout_tx,
            fail_sends: false,
        });

        let last_sent_signal = Arc::new(AtomicU64::new(0));
        spawn_keepalive_task(
            conn.clone(),
            last_sent_signal.clone(),
            "1.2.3".to_string(),
            "abc".to_string(),
        );

        // Let the spawned task run its first poll so it registers its sleep(),
        // then advance past the first tick interval.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(11)).await;
        tokio::task::yield_now().await;

        let heartbeats = heartbeats_sent.lock().unwrap();
        assert!(
            !heartbeats.is_empty(),
            "enhanced heartbeat should have been sent"
        );
        assert_eq!(heartbeats[0].version, "1.2.3");
        assert_eq!(heartbeats[0].hash, "abc");
        assert!(heartbeats[0].timestamp > 0);
        assert_eq!(pings_sent.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn test_standard_ping_sent() {
        let pings_sent = Arc::new(AtomicU64::new(0));
        let heartbeats_sent = Arc::new(Mutex::new(Vec::new()));
        let time_since_last_received = Arc::new(Mutex::new(Duration::ZERO));
        let (timeout_tx, _timeout_rx) = mpsc::channel(1);

        let conn = Arc::new(MockKeepAlive {
            pings_sent: pings_sent.clone(),
            heartbeats_sent: heartbeats_sent.clone(),
            time_since_last_received: time_since_last_received.clone(),
            heartbeat_timeout: Duration::from_millis(30),
            is_ahma_peer: false,
            supports_active_pings: true,
            timeout_tx,
            fail_sends: false,
        });

        let last_sent_signal = Arc::new(AtomicU64::new(0));
        spawn_keepalive_task(
            conn.clone(),
            last_sent_signal.clone(),
            "1.2.3".to_string(),
            "abc".to_string(),
        );

        // Let the spawned task register its sleep(), then advance past first tick.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(11)).await;
        tokio::task::yield_now().await;

        assert!(
            pings_sent.load(Ordering::Relaxed) > 0,
            "standard ping should have been sent"
        );
        assert!(
            heartbeats_sent.lock().unwrap().is_empty(),
            "no enhanced heartbeat should be sent"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_passive_monitoring() {
        let pings_sent = Arc::new(AtomicU64::new(0));
        let heartbeats_sent = Arc::new(Mutex::new(Vec::new()));
        let time_since_last_received = Arc::new(Mutex::new(Duration::ZERO));
        let (timeout_tx, _timeout_rx) = mpsc::channel(1);

        let conn = Arc::new(MockKeepAlive {
            pings_sent: pings_sent.clone(),
            heartbeats_sent: heartbeats_sent.clone(),
            time_since_last_received: time_since_last_received.clone(),
            heartbeat_timeout: Duration::from_millis(30),
            is_ahma_peer: false,
            supports_active_pings: false,
            timeout_tx,
            fail_sends: false,
        });

        let last_sent_signal = Arc::new(AtomicU64::new(0));
        spawn_keepalive_task(
            conn.clone(),
            last_sent_signal.clone(),
            "1.2.3".to_string(),
            "abc".to_string(),
        );

        // Let the spawned task register its sleep(), then advance past first tick.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(11)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            pings_sent.load(Ordering::Relaxed),
            0,
            "no standard ping should be sent"
        );
        assert!(
            heartbeats_sent.lock().unwrap().is_empty(),
            "no enhanced heartbeat should be sent"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_timeout_termination() {
        let pings_sent = Arc::new(AtomicU64::new(0));
        let heartbeats_sent = Arc::new(Mutex::new(Vec::new()));
        let time_since_last_received = Arc::new(Mutex::new(Duration::ZERO));
        let (timeout_tx, mut timeout_rx) = mpsc::channel(1);

        let conn = Arc::new(MockKeepAlive {
            pings_sent: pings_sent.clone(),
            heartbeats_sent: heartbeats_sent.clone(),
            time_since_last_received: time_since_last_received.clone(),
            heartbeat_timeout: Duration::from_millis(15),
            is_ahma_peer: true,
            supports_active_pings: true,
            timeout_tx,
            fail_sends: false,
        });

        *time_since_last_received.lock().unwrap() = Duration::from_millis(20);

        let last_sent_signal = Arc::new(AtomicU64::new(0));
        spawn_keepalive_task(
            conn.clone(),
            last_sent_signal.clone(),
            "1.2.3".to_string(),
            "abc".to_string(),
        );

        // Let the spawned task register its sleep(), then advance past first tick.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(6)).await;
        tokio::task::yield_now().await;

        let r = timeout_rx.try_recv();
        assert!(
            r.is_ok(),
            "on_timeout should be called and terminate the task"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_send_error_termination() {
        let pings_sent = Arc::new(AtomicU64::new(0));
        let heartbeats_sent = Arc::new(Mutex::new(Vec::new()));
        let time_since_last_received = Arc::new(Mutex::new(Duration::ZERO));
        let (timeout_tx, mut timeout_rx) = mpsc::channel(1);

        let conn = Arc::new(MockKeepAlive {
            pings_sent: pings_sent.clone(),
            heartbeats_sent: heartbeats_sent.clone(),
            time_since_last_received: time_since_last_received.clone(),
            heartbeat_timeout: Duration::from_millis(15),
            is_ahma_peer: true,
            supports_active_pings: true,
            timeout_tx,
            fail_sends: true,
        });

        let last_sent_signal = Arc::new(AtomicU64::new(0));
        spawn_keepalive_task(
            conn.clone(),
            last_sent_signal.clone(),
            "1.2.3".to_string(),
            "abc".to_string(),
        );

        // Let the spawned task register its sleep(), then advance past first tick.
        tokio::task::yield_now().await;
        // tickle_interval = 15ms / 3 = 5ms; advance past first tick — send fails, task breaks
        tokio::time::advance(Duration::from_millis(6)).await;
        tokio::task::yield_now().await;

        let r = timeout_rx.try_recv();
        assert!(
            r.is_err(),
            "on_timeout should not be called because task terminated early on send error"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_rate_limiting() {
        let pings_sent = Arc::new(AtomicU64::new(0));
        let heartbeats_sent = Arc::new(Mutex::new(Vec::new()));
        let time_since_last_received = Arc::new(Mutex::new(Duration::ZERO));
        let (timeout_tx, _timeout_rx) = mpsc::channel(1);

        let conn = Arc::new(MockKeepAlive {
            pings_sent: pings_sent.clone(),
            heartbeats_sent: heartbeats_sent.clone(),
            time_since_last_received: time_since_last_received.clone(),
            heartbeat_timeout: Duration::from_millis(60),
            is_ahma_peer: true,
            supports_active_pings: true,
            timeout_tx,
            fail_sends: false,
        });

        let last_sent_signal = Arc::new(AtomicU64::new(current_timestamp_ms()));

        spawn_keepalive_task(
            conn.clone(),
            last_sent_signal.clone(),
            "1.2.3".to_string(),
            "abc".to_string(),
        );

        // Set last_sent 10 seconds into the future (real time) so rate limit suppresses send
        last_sent_signal.store(current_timestamp_ms() + 10000, Ordering::Relaxed);

        // Let the spawned task register its sleep(), then advance past first tick.
        tokio::task::yield_now().await;
        // tickle_interval = 60ms / 3 = 20ms; advance past first tick
        tokio::time::advance(Duration::from_millis(21)).await;
        tokio::task::yield_now().await;

        let heartbeats = heartbeats_sent.lock().unwrap();
        assert!(
            heartbeats.is_empty(),
            "heartbeat should be skipped because of rate limiting"
        );
    }

    /// Wire-compat both ways for the #485 session-health fields: an old
    /// three-field payload deserializes under the new struct (defaults), and a
    /// new payload deserializes under an old-shaped consumer (extra fields
    /// ignored by serde's default unknown-field handling).
    #[test]
    fn heartbeat_payload_wire_compat_across_versions() {
        let old_wire = serde_json::json!({
            "version": "0.16.0", "hash": "abc", "timestamp": 42u64
        });
        let new: HeartbeatPayload = serde_json::from_value(old_wire).unwrap();
        assert_eq!(new.pending_grants, 0);
        assert_eq!(new.reconnects, 0);

        #[derive(serde::Deserialize)]
        struct OldPayload {
            version: String,
            #[allow(dead_code)]
            hash: String,
            #[allow(dead_code)]
            timestamp: u64,
        }
        let new_wire = serde_json::to_value(HeartbeatPayload {
            version: "0.17.0".into(),
            hash: "def".into(),
            timestamp: 43,
            pending_grants: 2,
            reconnects: 1,
        })
        .unwrap();
        let old: OldPayload = serde_json::from_value(new_wire).unwrap();
        assert_eq!(old.version, "0.17.0");
    }
}
