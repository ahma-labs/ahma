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

/// Snapshot of JSON files in a directory for polling-based change detection.
/// Tracks file names and sizes to detect additions, removals, and modifications.
async fn snapshot_json_files(dir: &Path) -> Vec<(String, u64)> {
    let mut files = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
                files.push((entry.file_name().to_string_lossy().into_owned(), size));
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

            let mut watcher =
                match notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
                    if let Ok(event) = res {
                        // Only react to relevant events on JSON files or directory changes
                        let relevant = event
                            .paths
                            .iter()
                            .any(|p| p.extension().is_some_and(|ext| ext == "json") || p.is_dir());

                        if relevant
                            && (event.kind.is_modify()
                                || event.kind.is_create()
                                || event.kind.is_remove())
                        {
                            let _ = tx.blocking_send(());
                        }
                    }
                }) {
                    Ok(w) => w,
                    Err(e) => {
                        tracing::error!("Failed to create config watcher: {}", e);
                        return;
                    }
                };

            if let Err(e) = watcher.watch(&tools_dir, RecursiveMode::Recursive) {
                tracing::error!("Failed to watch tools directory: {}", e);
                return;
            }

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
    use tempfile::TempDir;

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
        let names: Vec<&str> = snap.iter().map(|(n, _)| n.as_str()).collect();
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
}
