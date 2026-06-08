use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Payload sent with enhanced Ahma keep-alive / heartbeat notifications.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HeartbeatPayload {
    /// Application version (e.g., "0.11.9")
    pub version: String,
    /// Secure hash or build ID of the executable
    pub hash: String,
    /// Timestamp for latency calculation and deduplication
    pub timestamp: u64,
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
