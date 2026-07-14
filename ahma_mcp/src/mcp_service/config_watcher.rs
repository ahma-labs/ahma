use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use notify::{Event, RecursiveMode, Watcher};
use rmcp::service::{Peer, RoleServer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing;

use crate::utils::stdio::emit_stdout_notification;

use super::AhmaMcpService;
use crate::config::{ToolConfig, load_tool_configs};

/// Emit a sandbox JSON-RPC notification directly on stdout (raw primitive).
///
/// Prefer [`emit_sandbox_notification_via_peer`] for anything sent while the
/// rmcp peer is live — the raw path races the transport on Windows (see that
/// function's docs).  This direct write is retained only for the Linux Landlock
/// fatal-exit path, where the process is aborting on a security failure and a
/// synchronous write with no async-transport dependency is the right choice.
///
/// `error` is `None` for `notifications/sandbox/configured`, `Some(msg)` for failed.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn emit_sandbox_notification(method: &str, error: Option<&str>) {
    let payload = match error {
        Some(err) => serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": { "error": err }
        }),
        None => serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": {}
        }),
    };
    match serde_json::to_string(&payload) {
        Ok(notification) => {
            let _ = emit_stdout_notification(&notification);
        }
        Err(_) => {
            tracing::warn!("Failed to serialize sandbox notification: {}", method);
        }
    }
}

/// Emit a sandbox JSON-RPC notification through the rmcp **peer transport**.
///
/// CRITICAL (Windows correctness): sandbox lifecycle notifications MUST travel
/// the same serialized rmcp writer that carries `roots/list`, `ping`, and tool
/// responses — NOT the raw stdout handle used by [`emit_sandbox_notification`].
///
/// The raw path opens a *second*, unsynchronized OS handle to the same
/// subprocess→bridge pipe and writes via `writeln!` (multiple `WriteFile` calls).
/// Windows pipes give no cross-handle multi-write atomicity, so those bytes
/// interleave with concurrent rmcp writes — notably the keepalive `ping` that
/// fires the instant the sandbox locks — corrupting the line. The bridge's
/// line-oriented reader then fails to parse it and silently drops the
/// notification, so the client waits forever for `sandbox/configured`. On Unix
/// per-`write()` atomicity hid the bug. Routing through the peer serializes the
/// write behind rmcp's transport mutex, guaranteeing a clean, whole line.
async fn emit_sandbox_notification_via_peer(
    peer: &Peer<RoleServer>,
    method: &'static str,
    error: Option<&str>,
) {
    emit_sandbox_notification_via_peer_with_scope(peer, method, error, None).await;
}

/// As [`emit_sandbox_notification_via_peer`], but attaches the canonical scope
/// summary (SPEC R5.4) under a `scope` key so the client can display the
/// complete sandbox scope and its provenance without a separate query.
async fn emit_sandbox_notification_via_peer_with_scope(
    peer: &Peer<RoleServer>,
    method: &'static str,
    error: Option<&str>,
    scope: Option<serde_json::Value>,
) {
    let mut params = match error {
        Some(err) => serde_json::json!({ "error": err }),
        None => serde_json::json!({}),
    };
    if let (Some(scope), Some(obj)) = (scope, params.as_object_mut()) {
        obj.insert("scope".to_string(), scope);
    }
    if let Err(e) = peer
        .send_notification(rmcp::model::ServerNotification::CustomNotification(
            rmcp::model::CustomNotification::new(method, Some(params)),
        ))
        .await
    {
        // Best-effort: a send error here means the bridge already tore the
        // transport down; log and continue rather than panic.
        tracing::warn!("Failed to send {} via peer transport: {}", method, e);
    }
}

/// Parse a single `file://` URI from a roots/list response into a `PathBuf`.
/// Returns `None` and logs a warning for non-file or unparseable URIs.
fn parse_root_uri_to_scope(uri: &str) -> Option<PathBuf> {
    let url = url::Url::parse(uri).ok()?;
    if url.scheme() != "file" {
        tracing::warn!("Ignoring non-file URI: {}", uri);
        return None;
    }
    match url.to_file_path() {
        Ok(path) => {
            tracing::info!("Parsed valid file URI: {} -> {:?}", uri, path);
            Some(path)
        }
        Err(()) => {
            tracing::warn!("Failed to convert file URI to path: {}", uri);
            None
        }
    }
}

/// A directory snapshot entry: `(file_name, size, mtime)`.
///
/// `mtime` is `None` only when the platform/filesystem cannot report a
/// modification time; in that case detection degrades to size-only for that
/// file (the pre-existing behavior).
type JsonFileSnapshot = (String, u64, Option<std::time::SystemTime>);

/// Snapshot of JSON files in a directory for polling-based change detection.
///
/// Tracks file name, size, AND last-modified time so the polling fallback can
/// detect additions, removals, and modifications. Modification time is
/// essential: an in-place edit that preserves the byte length (e.g. changing a
/// description from `"original"` to `"modified"` — both 8 bytes) is invisible
/// to a size-only snapshot, so a size-only fallback could never detect it when
/// the OS fs-event is dropped or delayed (routinely observed with macOS
/// FSEvents under load). mtime changes on every write, closing that blind spot.
async fn snapshot_json_files(dir: &Path) -> Vec<JsonFileSnapshot> {
    let mut files = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                let (size, mtime) = match entry.metadata().await {
                    Ok(m) => (m.len(), m.modified().ok()),
                    Err(_) => (0, None),
                };
                files.push((
                    entry.file_name().to_string_lossy().into_owned(),
                    size,
                    mtime,
                ));
            }
        }
    }
    files.sort();
    files
}

impl AhmaMcpService {
    /// Updates the tool configurations and notifies clients.
    pub async fn update_tools(&self, new_configs: HashMap<String, ToolConfig>) {
        {
            let mut configs_lock = self.configs.write().unwrap();
            *configs_lock = new_configs;
        }

        // Notify clients that the tool list has changed.
        // Clone peer outside the lock before async call to avoid holding guard across .await
        let peer_opt = {
            let peer_lock = self.peer.read().unwrap();
            peer_lock.clone()
        };

        if let Some(peer) = peer_opt {
            if let Err(e) = peer.notify_tool_list_changed().await {
                tracing::error!("Failed to send tools/list_changed notification: {}", e);
            } else {
                tracing::info!("Sent tools/list_changed notification to client");
            }
        } else {
            tracing::debug!("No peer connected, skipping tools/list_changed notification");
        }
    }

    /// Starts a background task to watch for changes in the tools directory.
    pub fn start_config_watcher(&self, tools_dir: PathBuf, config: crate::shell::cli::AppConfig) {
        let service = self.clone();
        // Use a weak pointer to the operation monitor to detect when the service is dropped
        let weak_monitor = Arc::downgrade(&self.operation_monitor);

        tokio::spawn(async move {
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);

            // `notify::recommended_watcher` and `Watcher::watch` are
            // synchronous, OS-level calls — FSEvents registration on macOS
            // can block for a noticeable while under system load (observed
            // in CI: enough to starve a current-thread tokio runtime's own
            // task scheduling for minutes). Run them on the blocking-thread
            // pool so a slow registration never stalls this tokio worker,
            // which the debounce loop below (and callers polling this
            // service's state) depend on making progress.
            //
            // Bound the wait with a timeout: a stalled FSEvents registration
            // must not delay the startup config sync below (or block forever
            // in the case where it never returns). If it doesn't complete in
            // time, abandon it and fall through to the polling-fallback path
            // in the debounce loop, which detects changes without any OS
            // watch at all — just with coarser (2s) latency.
            const WATCHER_SETUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
            let watch_dir = tools_dir.clone();
            let watcher_setup = tokio::task::spawn_blocking(move || {
                let mut watcher =
                    notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
                        if let Ok(event) = res {
                            // Only react to relevant events on JSON files or directory changes
                            let relevant = event.paths.iter().any(|p| {
                                p.extension().is_some_and(|ext| ext == "json") || p.is_dir()
                            });

                            if relevant
                                && (event.kind.is_modify()
                                    || event.kind.is_create()
                                    || event.kind.is_remove())
                            {
                                // MUST be a non-blocking `try_send`, never `blocking_send`.
                                //
                                // This closure runs on notify's OS event thread
                                // (the macOS FSEvents CFRunLoop thread). The channel
                                // is a capacity-1 "something changed, wake up" signal:
                                // the debounce loop drains it fully and re-snapshots,
                                // so a coalesced/dropped duplicate is harmless, and the
                                // polling fallback catches anything an event misses.
                                //
                                // `blocking_send` would park this OS thread whenever the
                                // channel is full (a burst of events while the loop is
                                // busy reloading). If the watcher task's future is then
                                // dropped — e.g. runtime shutdown at the end of a test —
                                // its locals drop in reverse declaration order, so the
                                // `notify` watcher (which joins THIS thread in its Drop)
                                // is dropped before `rx`. The join then waits on a thread
                                // parked in `blocking_send`, whose capacity never frees
                                // because `rx` is dropped only afterwards: a permanent
                                // drop-order deadlock that hangs the whole runtime (seen
                                // as a 120s test timeout under load). `try_send` cannot
                                // park the thread, so the deadlock cannot form.
                                let _ = tx.try_send(());
                            }
                        }
                    })?;
                watcher.watch(&watch_dir, RecursiveMode::Recursive)?;
                Ok::<_, notify::Error>(watcher)
            });

            // Never read again — held only so the OS-level watch stays
            // registered for the lifetime of this task (dropping it stops
            // watching). `None` means only the polling fallback is active.
            let _watcher = match tokio::time::timeout(WATCHER_SETUP_TIMEOUT, watcher_setup).await {
                Ok(Ok(Ok(w))) => Some(w),
                Ok(Ok(Err(e))) => {
                    tracing::error!("Failed to create/watch config watcher: {}", e);
                    None
                }
                Ok(Err(e)) => {
                    tracing::error!("Config watcher setup task panicked: {}", e);
                    None
                }
                Err(_) => {
                    tracing::warn!(
                        "Config watcher setup for {:?} did not complete within {:?} \
                         (OS fs-event registration stalled); continuing with \
                         polling-only change detection.",
                        tools_dir,
                        WATCHER_SETUP_TIMEOUT
                    );
                    None
                }
            };

            tracing::info!("Started watching tools directory: {:?}", tools_dir);

            // Take initial snapshot for polling fallback (covers platforms where
            // fs-event delivery is unreliable, e.g. macOS CI VMs with FSEvents).
            let mut last_snapshot = snapshot_json_files(&tools_dir).await;

            // Startup sync: close the race window between initial service config
            // load and watcher task initialization. If tool files were changed
            // just before/while the watcher started, this ensures in-memory
            // configs converge with disk state even if the fs event was missed.
            match load_tool_configs(&config, Some(&tools_dir)).await {
                Ok(new_configs) => {
                    service.update_tools(new_configs).await;
                    tracing::debug!(
                        "Config watcher startup sync completed for tools directory: {:?}",
                        tools_dir
                    );
                }
                Err(e) => {
                    tracing::error!("Config watcher startup sync failed: {}", e);
                }
            }

            // Debounce + polling-fallback loop
            loop {
                tokio::select! {
                    recv = rx.recv() => {
                        if recv.is_none() {
                            break;
                        }

                        // Drain any other events that happened in quick succession
                        while rx.try_recv().is_ok() {}

                        // Wait a bit for file writes to complete
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                        tracing::info!("Detected change in tools directory, reloading configs...");
                        service
                            .reload_tool_configs_from_dir(
                                &config,
                                &tools_dir,
                                "Successfully reloaded tool configurations",
                            )
                            .await;
                        last_snapshot = snapshot_json_files(&tools_dir).await;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                        // Check if the service (via its monitor) is still alive
                        if weak_monitor.upgrade().is_none() {
                            tracing::debug!("AhmaMcpService dropped, stopping config watcher task");
                            break;
                        }

                        // Polling fallback: detect changes even when OS file-system
                        // events are not delivered (common on macOS CI runners).
                        let current = snapshot_json_files(&tools_dir).await;
                        if current != last_snapshot {
                            tracing::info!("Polling fallback detected tools directory change, reloading...");
                            service
                                .reload_tool_configs_from_dir(
                                    &config,
                                    &tools_dir,
                                    "Successfully reloaded tool configurations (polling fallback)",
                                )
                                .await;
                            last_snapshot = current;
                        }
                    }
                }
            }
        });
    }

    /// Query the client for workspace roots and initialize the sandbox scope.
    ///
    /// This implements the MCP roots protocol where the server requests the
    /// client's workspace roots to establish sandbox boundaries.
    /// Update sandbox scopes and (on Linux) enforce Landlock restrictions.
    /// Emits `notifications/sandbox/failed` and returns `false` on error.
    async fn apply_and_enforce_scopes(
        &self,
        new_scopes: Vec<PathBuf>,
        peer: &Peer<RoleServer>,
    ) -> bool {
        match self.adapter.sandbox().update_scopes(new_scopes.clone()) {
            Ok(()) => tracing::info!("Sandbox scopes updated successfully"),
            Err(e) => {
                tracing::error!("Failed to update sandbox from roots: {}", e);
                emit_sandbox_notification_via_peer(
                    peer,
                    "notifications/sandbox/failed",
                    Some(&e.to_string()),
                )
                .await;
                return false;
            }
        }

        // On Linux, apply process-level Landlock now that we have scopes. This
        // restricts only the calling thread (defense-in-depth) and, more
        // importantly, fails fast if the scopes are not enforceable. The actual
        // containment of executed commands happens at spawn time: every child
        // gets the current ruleset applied in pre_exec (see Sandbox::create_command).
        // SECURITY: exit if Landlock enforcement fails — cannot guarantee security without it.
        #[cfg(target_os = "linux")]
        if !self.adapter.sandbox().is_test_mode() {
            if let Err(e) = crate::sandbox::enforce_landlock_sandbox(
                &new_scopes,
                &self.adapter.sandbox().read_scopes(),
                self.adapter.sandbox().is_no_temp_files(),
                self.adapter.sandbox().package_cache_write(),
            ) {
                tracing::error!(
                    "FATAL: Failed to enforce Landlock sandbox: {}. \
                     Exiting to prevent running without kernel-level security.",
                    e
                );
                emit_sandbox_notification("notifications/sandbox/failed", Some(&e.to_string()));
                std::process::exit(1);
            }
            tracing::info!("Landlock sandbox enforced successfully");
        }

        true
    }

    /// Reload tool configs from disk, update in-memory state, and notify clients.
    async fn reload_tool_configs_from_dir(
        &self,
        config: &crate::shell::cli::AppConfig,
        tools_dir: &Path,
        success_msg: &str,
    ) {
        match load_tool_configs(config, Some(tools_dir)).await {
            Ok(new_configs) => {
                self.update_tools(new_configs).await;
                tracing::info!("{}", success_msg);
            }
            Err(e) => {
                tracing::error!("Failed to reload tool configurations: {}", e);
            }
        }
    }

    /// Load tool configs from a per-client `.ahma/` directory if present and not already loaded.
    async fn maybe_load_per_client_tools(&self, discovery_root: Option<PathBuf>) {
        let root = match discovery_root {
            Some(r) => r,
            None => return,
        };

        let candidate = root.join(".ahma");
        let is_dir = tokio::fs::metadata(&candidate)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if !is_dir {
            return;
        }

        let already_loaded = self
            .current_tools_dir
            .read()
            .unwrap()
            .as_ref()
            .map(|p| p == &candidate)
            .unwrap_or(false);
        if already_loaded {
            tracing::debug!(
                "Per-client tools dir already loaded: {}",
                candidate.display()
            );
            return;
        }

        let app_config = match self.app_config.read().unwrap().clone() {
            Some(c) => c,
            None => {
                tracing::debug!(
                    "Per-client tools dir {} found but AppConfig unavailable; skipping reload",
                    candidate.display()
                );
                return;
            }
        };

        tracing::info!(
            "Discovered per-client tools directory: {}",
            candidate.display()
        );
        match load_tool_configs(&app_config, Some(&candidate)).await {
            Ok(new_configs) => {
                let count = new_configs.len();
                self.update_tools(new_configs).await;
                *self.current_tools_dir.write().unwrap() = Some(candidate.clone());
                tracing::info!(
                    "Loaded {} tool configs from per-client {}",
                    count,
                    candidate.display()
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to load per-client tool configs from {}: {}",
                    candidate.display(),
                    e
                );
            }
        }
    }

    pub async fn configure_sandbox_from_roots(&self, peer: &Peer<RoleServer>) {
        // SPEC R5.1 / R5.1.1 / R5.2.2: the sandbox scope is committed exactly once
        // and is immutable thereafter. A second invocation — e.g. a pure-stdio
        // client re-announcing `roots/list_changed`, or a non-deferred
        // `on_initialized` configuration followed by a later `roots/list_changed`
        // — must NOT re-query or re-derive scopes, as that could widen the locked
        // sandbox. This mirrors the HTTP bridge's post-lock tolerated-no-op
        // handling (a roots change after lock is acknowledged but never applied).
        // Fast path: skip the roots/list round-trip entirely once committed.
        if self.adapter.sandbox().is_committed() {
            tracing::warn!(
                "roots/list(_changed) received after sandbox already committed - \
                 ignoring (scope is immutable and is never widened; SPEC R5.2.2)"
            );
            return;
        }

        let timeout_duration = TestTimeouts::get(TimeoutCategory::SseStream);
        tracing::info!(timeout = ?timeout_duration, "Requesting roots/list from client...");

        // Attempt roots/list; fall back to pre-configured scopes on timeout or error
        // so that clients that don't support roots/list (e.g. Antigravity) still work
        // when --sandbox-scope was provided at startup.
        let roots = match tokio::time::timeout(timeout_duration, peer.list_roots()).await {
            Ok(Ok(result)) => {
                self.adapter.sandbox().set_roots_received(true);
                result.roots
            }
            Ok(Err(e)) => {
                if !self.adapter.sandbox().scopes().is_empty() {
                    tracing::info!(
                        "roots/list returned error ({}); using pre-configured scopes",
                        e
                    );
                    vec![]
                } else {
                    tracing::error!("Failed to request roots/list: {}", e);
                    emit_sandbox_notification_via_peer(
                        peer,
                        "notifications/sandbox/failed",
                        Some(&e.to_string()),
                    )
                    .await;
                    return;
                }
            }
            Err(_) => {
                if !self.adapter.sandbox().scopes().is_empty() {
                    tracing::info!(
                        "Timeout waiting for roots/list response after {:?}; \
                         using pre-configured scopes",
                        timeout_duration
                    );
                    vec![]
                } else {
                    tracing::error!(
                        "Timeout waiting for roots/list response after {:?}. \
                         This may indicate a stdio communication issue.",
                        timeout_duration
                    );
                    emit_sandbox_notification_via_peer(
                        peer,
                        "notifications/sandbox/failed",
                        Some(&format!(
                            "Timeout waiting for roots/list response after {:?}",
                            timeout_duration
                        )),
                    )
                    .await;
                    return;
                }
            }
        };
        tracing::debug!("roots/list returned {} roots", roots.len());

        let new_scopes: Vec<PathBuf> = roots
            .iter()
            .filter_map(|r| parse_root_uri_to_scope(&r.uri))
            .collect();
        tracing::info!(
            "Parsed {} valid scopes out of {} client roots",
            new_scopes.len(),
            roots.len()
        );

        // Remember the first client root so we can look for `<root>/.ahma`
        // AFTER the scopes vec is moved into `apply_and_enforce_scopes`.
        let client_root: Option<PathBuf> = new_scopes.first().cloned();

        if !new_scopes.is_empty() {
            // Claim the one-shot commit BEFORE mutating scopes so a concurrent or
            // repeat roots/list can never widen the locked sandbox (SPEC R5.1.1).
            // The loser of the race returns without touching the committed scope.
            if !self.adapter.sandbox().try_commit() {
                tracing::warn!(
                    "roots/list provided scopes but the sandbox is already committed - \
                     ignoring to preserve the immutable scope (SPEC R5.2.2)"
                );
                return;
            }
            tracing::debug!(
                "Attempting to update sandbox scopes with {} paths",
                new_scopes.len()
            );
            if !self.apply_and_enforce_scopes(new_scopes, peer).await {
                return;
            }
        } else if !self.adapter.sandbox().scopes().is_empty() {
            // Client provided no file:// roots but we have pre-configured scopes
            // from --working-directories. These are valid, so proceed.
            if !self.adapter.sandbox().try_commit() {
                tracing::warn!(
                    "Pre-configured scopes present but the sandbox is already committed - \
                     ignoring repeat configuration (SPEC R5.2.2)"
                );
                return;
            }
            tracing::info!(
                "No new scopes from roots/list; using pre-configured scopes: {:?}",
                self.adapter.sandbox().scopes()
            );
        } else {
            // Client returned an empty roots list and there are no pre-configured scopes.
            // Do NOT emit notifications/sandbox/configured here: emitting it would mark the
            // sandbox as "ready" with zero scope, which causes every tool call to fail with a
            // misleading "path outside sandbox" error instead of the observable HTTP 409
            // (-32001) that tells the user to open a workspace folder or pass --sandbox-scope.
            tracing::warn!(
                "roots/list response has no valid file:// roots and no pre-configured scopes \
                 are available. Sandbox configuration deferred. \
                 Fix: open a workspace folder so the client can provide workspace roots, \
                 or pass --sandbox-scope <path> / --sandbox to ahma."
            );
            return;
        }

        // Anchor the default log directory to the primary workspace scope so that
        // logs_list and operation spill files land inside the project.  Best-effort:
        // silently ignored if --log-dir was already set or scope was already recorded.
        // The tracing file appender opened at startup keeps its existing file handle;
        // only subsequent project_log_dir() callers (spill files, logs_list) are affected.
        let primary_scope = self
            .adapter
            .sandbox()
            .scopes()
            .first()
            .map(|p| p.to_path_buf());
        if let Some(ref scope) = primary_scope {
            crate::utils::logging::set_log_dir_from_scope(scope.join("logs"));
        }

        // Per-client tool discovery: load tools from `<root>/.ahma/` if present.
        let discovery_root = client_root.or(primary_scope);
        self.maybe_load_per_client_tools(discovery_root.clone())
            .await;

        // Load external MCP servers and discovery for client workspace on the daemon/serve side
        if let Some(ref root) = discovery_root {
            match crate::mcp_client::McpConnectionManager::load(root) {
                Ok(mut manager) => {
                    tracing::info!("Loaded McpConnectionManager for {}", root.display());
                    let mcp_connections_clone = self.mcp_connections.clone();
                    tokio::spawn(async move {
                        manager.refresh_tools().await;
                        *mcp_connections_clone.write().await = manager;
                        tracing::info!("Refreshed external MCP tools on server side");
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to load McpConnectionManager for {}: {}",
                        root.display(),
                        e
                    );
                }
            }
        }

        // Notify bridge that sandbox has been configured so it can safely
        // forward tools/call requests.  This MUST go through the peer transport
        // (not raw stdout) — see emit_sandbox_notification_via_peer for why the
        // raw path silently drops this notification on Windows.
        // SPEC R5.4: the configured notification carries the complete scope and
        // its provenance so the client can show it without a separate query.
        let sandbox = self.adapter.sandbox();
        let source = if sandbox.has_explicit_scopes() {
            crate::sandbox::ScopeSource::Explicit
        } else if sandbox.roots_received() {
            crate::sandbox::ScopeSource::RootsList
        } else {
            crate::sandbox::ScopeSource::Default
        };
        let scope_json = sandbox.scope_json(source);
        tracing::info!("Sandbox configured:\n{}", sandbox.scope_text(source));
        emit_sandbox_notification_via_peer_with_scope(
            peer,
            "notifications/sandbox/configured",
            None,
            Some(scope_json),
        )
        .await;
        tracing::debug!("Sent notifications/sandbox/configured");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::assertions::assert_eventually;
    use std::time::Duration;
    use tempfile::TempDir;

    /// Timeout for [`assert_eventually`] in this module's config-watcher tests.
    ///
    /// A reload runs the tool-availability probes, which spawn subprocesses, so
    /// this tracks the shared `ProcessSpawn` budget and inherits its platform
    /// multipliers (Windows CI, coverage builds). A flat 10s used to be hard-coded
    /// here and timed out under the process-spawn contention of a full-suite run —
    /// nothing is lost by waiting longer, because `assert_eventually` polls and
    /// returns the moment the condition holds.
    fn watcher_timeout() -> Duration {
        TestTimeouts::get(TimeoutCategory::ProcessSpawn)
    }
    const WATCHER_POLL: Duration = Duration::from_millis(25);

    // ── parse_root_uri_to_scope ──────────────────────────────────────────────

    #[test]
    #[cfg(unix)]
    fn parse_valid_file_uri_returns_path() {
        let result = parse_root_uri_to_scope("file:///tmp/workspace");
        assert!(
            result.is_some(),
            "valid file URI should parse to Some(path)"
        );
        let path = result.unwrap();
        assert_eq!(path, std::path::PathBuf::from("/tmp/workspace"));
    }

    #[test]
    #[cfg(windows)]
    fn parse_valid_file_uri_returns_path() {
        // On Windows, file:// URIs must reference a drive-rooted path.
        let result = parse_root_uri_to_scope("file:///C:/Users/workspace");
        assert!(
            result.is_some(),
            "valid file URI should parse to Some(path)"
        );
        let path = result.unwrap();
        assert_eq!(path, std::path::PathBuf::from("C:\\Users\\workspace"));
    }

    #[test]
    fn parse_non_file_uri_returns_none() {
        let result = parse_root_uri_to_scope("https://github.com/user/repo");
        assert!(result.is_none(), "https URI should be rejected");
    }

    #[test]
    fn parse_invalid_uri_returns_none() {
        let result = parse_root_uri_to_scope("not a uri at all !!!");
        assert!(result.is_none(), "garbage input should return None");
    }

    #[test]
    fn parse_empty_string_returns_none() {
        assert!(parse_root_uri_to_scope("").is_none());
    }

    #[test]
    #[cfg(unix)]
    fn parse_file_uri_with_spaces_encoded() {
        // %20 = space; test on Unix where /tmp/... is a valid absolute path
        let result = parse_root_uri_to_scope("file:///tmp/my%20workspace");
        assert!(result.is_some());
        let path = result.unwrap();
        assert_eq!(path, std::path::PathBuf::from("/tmp/my workspace"));
    }

    #[test]
    #[cfg(windows)]
    fn parse_file_uri_with_spaces_encoded() {
        // %20 = space; on Windows, use a drive-rooted path
        let result = parse_root_uri_to_scope("file:///C:/Users/my%20workspace");
        assert!(result.is_some());
        let path = result.unwrap();
        assert_eq!(path, std::path::PathBuf::from("C:\\Users\\my workspace"));
    }

    // ── snapshot_json_files ─────────────────────────────────────────────────

    #[tokio::test]
    async fn snapshot_empty_directory_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let snap = snapshot_json_files(tmp.path()).await;
        assert!(snap.is_empty());
    }

    #[tokio::test]
    async fn snapshot_counts_only_json_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("tool.json"), b"{}").unwrap();
        std::fs::write(tmp.path().join("readme.md"), b"# hi").unwrap();
        std::fs::write(tmp.path().join("config.toml"), b"[x]").unwrap();

        let snap = snapshot_json_files(tmp.path()).await;
        assert_eq!(
            snap.len(),
            1,
            "only the .json file should appear in snapshot"
        );
        assert_eq!(snap[0].0, "tool.json");
    }

    #[tokio::test]
    async fn snapshot_multiple_json_files_sorted() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("zebra.json"), b"{}").unwrap();
        std::fs::write(tmp.path().join("alpha.json"), b"{}").unwrap();
        std::fs::write(tmp.path().join("middle.json"), b"{}").unwrap();

        let snap = snapshot_json_files(tmp.path()).await;
        let names: Vec<&str> = snap.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, vec!["alpha.json", "middle.json", "zebra.json"]);
    }

    #[tokio::test]
    async fn snapshot_reflects_file_sizes() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("small.json"), b"{}").unwrap();
        std::fs::write(tmp.path().join("large.json"), b"{ \"key\": \"value\" }").unwrap();

        let snap = snapshot_json_files(tmp.path()).await;
        // alpha sort: large first
        assert_eq!(snap[0].0, "large.json");
        assert_eq!(snap[1].0, "small.json");
        assert!(
            snap[0].1 > snap[1].1,
            "large.json should report a bigger size"
        );
    }

    #[tokio::test]
    async fn snapshot_detects_content_change() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("tool.json");
        std::fs::write(&path, b"{}").unwrap();

        let snap1 = snapshot_json_files(tmp.path()).await;
        // Overwrite with longer content
        std::fs::write(&path, b"{ \"key\": \"value\" }").unwrap();
        let snap2 = snapshot_json_files(tmp.path()).await;

        assert_ne!(snap1, snap2, "snapshot must differ after content change");
    }

    /// Regression: a content edit that PRESERVES byte length must still change
    /// the snapshot. `snapshot_json_files` used to track only `(name, size)`, so
    /// a same-size edit (the real watcher's `"original"` → `"modified"` case —
    /// both 8 bytes) was invisible to the polling fallback. When the OS fs-event
    /// was dropped or delayed under load (routine on macOS FSEvents), nothing
    /// detected the change and the watcher's own test hung to a hard timeout.
    /// Including mtime closes the blind spot: the snapshot differs even when the
    /// size is byte-for-byte identical.
    #[tokio::test]
    async fn snapshot_detects_same_size_content_change() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("editable.json");

        // Two payloads of IDENTICAL byte length but different content.
        let before =
            br#"{"name":"editable","description":"original","command":"echo","enabled":true}"#;
        let after =
            br#"{"name":"editable","description":"modified","command":"echo","enabled":true}"#;
        assert_eq!(before.len(), after.len(), "test payloads must be same size");

        std::fs::write(&path, before).unwrap();
        let snap1 = snapshot_json_files(tmp.path()).await;
        assert_eq!(snap1.len(), 1);

        // Rewrite same-size content until the filesystem's mtime advances (it is
        // sub-millisecond on every platform ahma's CI runs on — APFS, ext4,
        // NTFS — so this converges immediately; the bounded loop only guards a
        // hypothetical coarse-granularity filesystem). The size never changes,
        // so a size-only snapshot could never satisfy this assertion.
        let mut snap2 = snap1.clone();
        for _ in 0..200 {
            std::fs::write(&path, after).unwrap();
            snap2 = snapshot_json_files(tmp.path()).await;
            if snap2 != snap1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(
            snap1[0].1, snap2[0].1,
            "size must be unchanged (same length)"
        );
        assert_ne!(
            snap1, snap2,
            "snapshot must differ after a same-size content edit (mtime changed)"
        );
    }

    #[tokio::test]
    async fn snapshot_nonexistent_dir_returns_empty() {
        let path = std::path::Path::new("/this/path/does/not/exist/ever/12345");
        let snap = snapshot_json_files(path).await;
        assert!(
            snap.is_empty(),
            "nonexistent directory should yield empty snapshot"
        );
    }

    // ── emit_sandbox_notification ────────────────────────────────────────────

    #[test]
    fn emit_sandbox_notification_no_error_does_not_panic() {
        // We can't assert stdout content in a test (it's a process-level pipe),
        // but we can verify the function completes without panicking.
        emit_sandbox_notification("notifications/sandbox/configured", None);
    }

    #[test]
    fn emit_sandbox_notification_with_error_does_not_panic() {
        emit_sandbox_notification("notifications/sandbox/failed", Some("something went wrong"));
    }

    // ── service construction helper ──────────────────────────────────────────

    /// Build a minimal `AhmaMcpService` scoped to `scope_dir` for unit tests.
    /// Uses `SandboxMode::Test` so no kernel-level enforcement is applied.
    async fn build_service_for_tests(scope_dir: &std::path::Path) -> AhmaMcpService {
        use crate::adapter::Adapter;
        use crate::operation_monitor::{MonitorConfig, OperationMonitor};
        use crate::sandbox::{Sandbox, SandboxMode};
        use crate::shell_pool::{ShellPoolConfig, ShellPoolManager};
        use std::collections::HashMap;
        use std::sync::Arc;

        let monitor_config = MonitorConfig::with_timeout(std::time::Duration::from_secs(300));
        let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));
        let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
        let sandbox = Arc::new(
            Sandbox::new(
                vec![scope_dir.to_path_buf()],
                SandboxMode::Test,
                false,
                false,
                false,
            )
            .unwrap(),
        );
        let adapter =
            Arc::new(Adapter::new(Arc::clone(&operation_monitor), shell_pool, sandbox).unwrap());
        AhmaMcpService::new(
            adapter,
            operation_monitor,
            Arc::new(HashMap::new()),
            Arc::new(None),
            false,
            false,
        )
        .await
        .unwrap()
    }

    // ── reload_tool_configs_from_dir ──────────────────────────────────────────

    /// reload_tool_configs_from_dir with a valid tool file updates configs.
    #[tokio::test]
    async fn reload_tool_configs_from_dir_success_updates_configs() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;
        let app_config = crate::shell::cli::AppConfig::default();

        tokio::fs::write(
            tmp.path().join("reload_tool.json"),
            r#"{"name":"reload_tool","description":"Reload test","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();

        service
            .reload_tool_configs_from_dir(&app_config, tmp.path(), "reload succeeded")
            .await;

        assert!(
            service.configs.read().unwrap().contains_key("reload_tool"),
            "reload_tool should be present after reload"
        );
    }

    /// reload_tool_configs_from_dir with a reserved tool name propagates the
    /// error gracefully — logs it and does NOT panic.
    #[tokio::test]
    async fn reload_tool_configs_from_dir_reserved_name_logs_error_no_panic() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;
        let app_config = crate::shell::cli::AppConfig::default();

        // "await" is a reserved name; load_tool_configs returns Err.
        tokio::fs::write(
            tmp.path().join("await.json"),
            r#"{"name":"await","description":"reserved","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();

        // Must not panic even when the load returns an error.
        service
            .reload_tool_configs_from_dir(&app_config, tmp.path(), "should not print")
            .await;
    }

    /// reload_tool_configs_from_dir with an empty directory removes tools that
    /// were previously present.
    #[tokio::test]
    async fn reload_tool_configs_from_dir_empty_dir_clears_tools() {
        let tmp = TempDir::new().unwrap();

        // Pre-populate with one tool via the configs lock.
        let service = build_service_for_tests(tmp.path()).await;
        {
            let mut lock = service.configs.write().unwrap();
            lock.insert(
                "old".to_string(),
                crate::config::ToolConfig {
                    name: "old".to_string(),
                    description: "old".to_string(),
                    command: "echo".to_string(),
                    enabled: true,
                    ..Default::default()
                },
            );
        }

        let app_config = crate::shell::cli::AppConfig::default();
        service
            .reload_tool_configs_from_dir(&app_config, tmp.path(), "cleared")
            .await;

        assert!(
            !service.configs.read().unwrap().contains_key("old"),
            "old tool should be gone after reload from empty dir"
        );
    }

    // ── maybe_load_per_client_tools ───────────────────────────────────────────

    /// None discovery_root → function returns immediately with no side-effects.
    #[tokio::test]
    async fn maybe_load_per_client_none_root_is_noop() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;

        service.maybe_load_per_client_tools(None).await;

        assert!(
            service.configs.read().unwrap().is_empty(),
            "configs should be untouched when discovery_root is None"
        );
    }

    /// Discovery root that has no `.ahma` subdirectory → function returns early.
    #[tokio::test]
    async fn maybe_load_per_client_no_ahma_subdir_is_noop() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;

        service
            .maybe_load_per_client_tools(Some(tmp.path().to_path_buf()))
            .await;

        assert!(
            service.configs.read().unwrap().is_empty(),
            "configs should be untouched when .ahma directory is absent"
        );
    }

    /// `.ahma` path exists as a file (not a directory) → treated as absent.
    #[tokio::test]
    async fn maybe_load_per_client_ahma_is_file_not_dir_is_noop() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;

        // Create `.ahma` as a plain file, not a directory.
        tokio::fs::write(tmp.path().join(".ahma"), b"not a directory")
            .await
            .unwrap();

        service
            .maybe_load_per_client_tools(Some(tmp.path().to_path_buf()))
            .await;

        assert!(
            service.configs.read().unwrap().is_empty(),
            "configs should be untouched when .ahma is a file, not a directory"
        );
    }

    /// Candidate `.ahma` directory is already the current_tools_dir → skip.
    #[tokio::test]
    async fn maybe_load_per_client_already_loaded_dir_skips_reload() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;

        let ahma_dir = tmp.path().join(".ahma");
        tokio::fs::create_dir_all(&ahma_dir).await.unwrap();

        // Write a valid tool so we can verify it does NOT get loaded.
        tokio::fs::write(
            ahma_dir.join("skip_tool.json"),
            r#"{"name":"skip_tool","description":"Should not be loaded","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();

        // Mark the .ahma directory as already loaded.
        *service.current_tools_dir.write().unwrap() = Some(ahma_dir.clone());

        service
            .maybe_load_per_client_tools(Some(tmp.path().to_path_buf()))
            .await;

        assert!(
            !service.configs.read().unwrap().contains_key("skip_tool"),
            "skip_tool must NOT be loaded because the directory is already loaded"
        );
    }

    /// `app_config` is None → function skips the load and returns early.
    #[tokio::test]
    async fn maybe_load_per_client_no_app_config_skips_reload() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;

        let ahma_dir = tmp.path().join(".ahma");
        tokio::fs::create_dir_all(&ahma_dir).await.unwrap();
        tokio::fs::write(
            ahma_dir.join("no_cfg_tool.json"),
            r#"{"name":"no_cfg_tool","description":"Skipped","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();

        // Do NOT call service.set_app_config() → app_config stays None.
        service
            .maybe_load_per_client_tools(Some(tmp.path().to_path_buf()))
            .await;

        assert!(
            !service.configs.read().unwrap().contains_key("no_cfg_tool"),
            "tool should NOT be loaded when app_config is absent"
        );
    }

    /// Happy path: `.ahma` directory with a valid tool → tool is loaded and
    /// `current_tools_dir` is updated to the `.ahma` path.
    #[tokio::test]
    async fn maybe_load_per_client_success_loads_tool_and_updates_dir() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;

        let ahma_dir = tmp.path().join(".ahma");
        tokio::fs::create_dir_all(&ahma_dir).await.unwrap();
        tokio::fs::write(
            ahma_dir.join("pclient.json"),
            r#"{"name":"pclient","description":"Per-client tool","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();

        // Provide app_config so the load proceeds.
        service.set_app_config(std::sync::Arc::new(crate::shell::cli::AppConfig::default()));

        service
            .maybe_load_per_client_tools(Some(tmp.path().to_path_buf()))
            .await;

        assert!(
            service.configs.read().unwrap().contains_key("pclient"),
            "per-client tool 'pclient' should be present after successful load"
        );
        assert_eq!(
            service.current_tools_dir.read().unwrap().as_deref(),
            Some(ahma_dir.as_path()),
            "current_tools_dir should point to the .ahma directory that was just loaded"
        );
    }

    /// Error path: a reserved tool name inside `.ahma/` causes load_tool_configs
    /// to return Err → warning is logged but function does not panic.
    #[tokio::test]
    async fn maybe_load_per_client_reserved_name_logs_warning_no_panic() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;

        let ahma_dir = tmp.path().join(".ahma");
        tokio::fs::create_dir_all(&ahma_dir).await.unwrap();

        // "status" is a reserved tool name; load_tool_configs returns Err.
        tokio::fs::write(
            ahma_dir.join("status.json"),
            r#"{"name":"status","description":"reserved","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();

        service.set_app_config(std::sync::Arc::new(crate::shell::cli::AppConfig::default()));

        // Should log a warning but not panic.
        service
            .maybe_load_per_client_tools(Some(tmp.path().to_path_buf()))
            .await;
    }

    // ── start_config_watcher ─────────────────────────────────────────────────

    /// Calling start_config_watcher on an empty directory must not panic.
    /// The background task starts, takes a snapshot of zero files, runs an
    /// initial sync (empty result), and enters the event loop.
    #[tokio::test]
    async fn start_config_watcher_empty_dir_does_not_panic() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;
        let app_config = crate::shell::cli::AppConfig::default();

        service.start_config_watcher(tmp.path().to_path_buf(), app_config);

        // Give the background task time to complete the startup sync.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    /// Files written BEFORE start_config_watcher is called are picked up by the
    /// initial startup sync inside the spawned task.
    #[tokio::test]
    async fn start_config_watcher_startup_sync_loads_existing_tools() {
        let tmp = TempDir::new().unwrap();

        // Write the tool file BEFORE starting the watcher.
        tokio::fs::write(
            tmp.path().join("pre_existing.json"),
            r#"{"name":"pre_existing","description":"was here at start","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();

        let service = build_service_for_tests(tmp.path()).await;
        let app_config = crate::shell::cli::AppConfig::default();
        service.start_config_watcher(tmp.path().to_path_buf(), app_config);

        // Wait for the startup sync to complete (polled, not a fixed sleep —
        // a fixed delay flakes under scheduler contention).
        assert_eventually(
            watcher_timeout(),
            WATCHER_POLL,
            "pre_existing tool loaded by the startup sync",
            || async { service.configs.read().unwrap().contains_key("pre_existing") },
        )
        .await;
    }

    /// A new JSON file added after the watcher starts is detected and reloaded.
    #[tokio::test]
    async fn start_config_watcher_detects_new_json_file() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;
        let app_config = crate::shell::cli::AppConfig::default();
        service.start_config_watcher(tmp.path().to_path_buf(), app_config);

        // No need to wait for the startup sync first: `watcher.watch(..)` is
        // established before the startup sync runs, so a change made here
        // queues on the (capacity-1) event channel and is processed once the
        // watcher's debounce loop starts draining it — nothing is lost.
        tokio::fs::write(
            tmp.path().join("dynamic_tool.json"),
            r#"{"name":"dynamic_tool","description":"added dynamically","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();

        // Wait for the watcher's debounce + reload to pick it up (polled, not
        // a fixed sleep — a fixed delay flakes under scheduler contention).
        assert_eventually(
            watcher_timeout(),
            WATCHER_POLL,
            "dynamic_tool detected and loaded by the fs watcher",
            || async { service.configs.read().unwrap().contains_key("dynamic_tool") },
        )
        .await;
    }

    /// Deleting a JSON file after the watcher starts removes that tool from configs.
    #[tokio::test]
    async fn start_config_watcher_detects_removed_json_file() {
        let tmp = TempDir::new().unwrap();
        let tool_path = tmp.path().join("remove_me.json");
        tokio::fs::write(
            &tool_path,
            r#"{"name":"remove_me","description":"will be deleted","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();

        let service = build_service_for_tests(tmp.path()).await;
        let app_config = crate::shell::cli::AppConfig::default();
        service.start_config_watcher(tmp.path().to_path_buf(), app_config);

        // Wait for startup sync to pick up the existing tool (polled, not a
        // fixed sleep — a fixed delay flakes under scheduler contention: the
        // spawned watcher task may not have reached the startup-sync step
        // yet on a busy machine).
        assert_eventually(
            watcher_timeout(),
            WATCHER_POLL,
            "remove_me loaded by startup sync",
            || async { service.configs.read().unwrap().contains_key("remove_me") },
        )
        .await;

        // Delete the file.
        tokio::fs::remove_file(&tool_path).await.unwrap();

        // Wait for the watcher to fire and reload.
        assert_eventually(
            watcher_timeout(),
            WATCHER_POLL,
            "remove_me removed after the file is deleted",
            || async { !service.configs.read().unwrap().contains_key("remove_me") },
        )
        .await;
    }

    /// Modifying a JSON file is detected and the description is updated.
    #[tokio::test]
    async fn start_config_watcher_detects_modified_json_file() {
        let tmp = TempDir::new().unwrap();
        let tool_path = tmp.path().join("editable.json");
        tokio::fs::write(
            &tool_path,
            r#"{"name":"editable","description":"original","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();

        let service = build_service_for_tests(tmp.path()).await;
        let app_config = crate::shell::cli::AppConfig::default();
        service.start_config_watcher(tmp.path().to_path_buf(), app_config);

        // Wait for startup sync to load the original description first, so
        // the overwrite below genuinely exercises the fs-watcher reload path
        // rather than racing to be included in the startup sync itself.
        assert_eventually(
            watcher_timeout(),
            WATCHER_POLL,
            "editable loaded with its original description by startup sync",
            || async {
                service
                    .configs
                    .read()
                    .unwrap()
                    .get("editable")
                    .is_some_and(|c| c.description == "original")
            },
        )
        .await;

        // Overwrite with updated description.
        tokio::fs::write(
            &tool_path,
            r#"{"name":"editable","description":"modified","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();

        // Wait for the watcher to detect and reload.
        assert_eventually(
            watcher_timeout(),
            WATCHER_POLL,
            "editable description updated to 'modified' after reload",
            || async {
                service
                    .configs
                    .read()
                    .unwrap()
                    .get("editable")
                    .is_some_and(|c| c.description == "modified")
            },
        )
        .await;
    }

    /// Non-JSON files (readme, txt, hidden) in the watched directory do not
    /// introduce any tools into the service configs.
    #[tokio::test]
    async fn start_config_watcher_ignores_non_json_files() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;
        let app_config = crate::shell::cli::AppConfig::default();
        service.start_config_watcher(tmp.path().to_path_buf(), app_config);

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Write non-JSON files.
        tokio::fs::write(tmp.path().join("readme.md"), b"# readme")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("notes.txt"), b"some notes")
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // Only the synthetic run_terminal_command config should be present
        // (inserted by load_tool_configs itself), not any user tools.
        let configs = service.configs.read().unwrap();
        assert!(
            !configs
                .values()
                .any(|c| c.name == "readme" || c.name == "notes"),
            "non-JSON files must not produce tool configs"
        );
    }

    /// start_config_watcher with a non-existent directory logs an error inside
    /// the spawned task and exits gracefully without panicking.
    #[tokio::test]
    async fn start_config_watcher_nonexistent_dir_exits_gracefully() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;
        let app_config = crate::shell::cli::AppConfig::default();

        // The directory we pass in does not exist.
        let nonexistent = tmp.path().join("does_not_exist_ever");
        service.start_config_watcher(nonexistent, app_config);

        // Give the task a moment to attempt and fail gracefully.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        // If we reach here without a panic the test passes.
    }
}
