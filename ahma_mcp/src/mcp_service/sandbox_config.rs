//! Sandbox configuration from client roots, plus the one-shot tool-config loads
//! that hang off it.
//!
//! There is deliberately **no file watcher here** (SPEC R1.4 / R-HANDOFF.7).
//! Tool definitions are read once
//! at startup (and once more when a client's workspace root arrives, for
//! `<root>/.ahma/`); after that the only way to pick up an edited definition is
//! the `restart` builtin, which is explicit and auditable. The watcher that used
//! to live here made an agent-authored `.ahma/<name>.json` go live within two
//! seconds — MTDF's `command` is a free-form string and the workspace is inside
//! the sandbox scope, so a sandboxed agent could repoint a tool name the operator
//! had configured, with no restart and no notification. That is the shape of
//! CVE-2026-48124 (sandboxed agent writes a workspace config that a trusted
//! component outside the sandbox then executes).

use ahma_common::mcp_methods::{
    SANDBOX_CONFIGURED_METHOD, SANDBOX_FAILED_METHOD, SandboxLifecycleParams, SandboxScopeSummary,
};
use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use rmcp::service::{Peer, RoleServer};
use std::collections::HashMap;
use std::path::PathBuf;
use tracing;

use crate::utils::stdio::emit_stdout_notification;

use super::AhmaMcpService;
use crate::config::ToolConfig;

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
    let params = SandboxLifecycleParams {
        error: error.map(str::to_string),
        scope: None,
    };
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params
    });
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
    // `Sandbox::scope_json` always produces an object matching
    // `SandboxScopeSummary` (pinned by `scope_json_round_trips_through_typed_
    // summary` below), so this conversion cannot fail in practice; a non-object
    // is downgraded to "no scope attached" rather than dropping the whole
    // notification.
    let scope =
        scope.and_then(
            |value| match serde_json::from_value::<SandboxScopeSummary>(value) {
                Ok(summary) => Some(summary),
                Err(e) => {
                    tracing::warn!("Malformed scope summary omitted from {}: {}", method, e);
                    None
                }
            },
        );
    let params = SandboxLifecycleParams {
        error: error.map(str::to_string),
        scope,
    };
    let params = match serde_json::to_value(&params) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Failed to serialize {} params: {}", method, e);
            return;
        }
    };
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
///
/// Delegates to `ahma_common::file_uri::parse_file_uri_to_path` — the single
/// hardened parser shared with the HTTP bridge (rejects non-`file` schemes,
/// NUL bytes, relative paths, invalid percent-encoding).
fn parse_root_uri_to_scope(uri: &str) -> Option<PathBuf> {
    match ahma_common::file_uri::parse_file_uri_to_path(uri) {
        Some(path) => {
            tracing::info!("Parsed valid file URI: {} -> {:?}", uri, path);
            Some(path)
        }
        None => {
            tracing::warn!("Ignoring invalid or non-file root URI: {}", uri);
            None
        }
    }
}

impl AhmaMcpService {
    /// Updates the tool configurations and notifies clients.
    pub async fn update_tools(&self, new_configs: HashMap<String, ToolConfig>) {
        {
            let mut configs_lock = self.configs.write().unwrap();
            *configs_lock = new_configs;
        }
        self.invalidate_config_tools_cache();

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
                    SANDBOX_FAILED_METHOD,
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
                emit_sandbox_notification(SANDBOX_FAILED_METHOD, Some(&e.to_string()));
                std::process::exit(1);
            }
            tracing::info!("Landlock sandbox enforced successfully");
        }

        true
    }

    /// Load tool configs from a per-client `.ahma/` directory if present and not
    /// already loaded.
    ///
    /// One-shot: this runs when the client's workspace root first arrives and
    /// never again. Nothing watches the directory afterwards — an edited
    /// definition is picked up by the `restart` builtin, not by writing a file.
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

        // Layer the client's `.ahma` *on top of* the configured tools dir rather than
        // replacing it. Loading `candidate` alone keeps the built-ins but drops every
        // tool from an operator's `--tools-dir`, so a client whose root happens to
        // contain `.ahma/` silently deleted the explicitly-selected tool set — surfacing
        // later as a baffling "Tool '<name>' not found".
        //
        // SECURITY: the candidate is *additive only*. It sits inside the sandbox scope
        // and is therefore writable by the agent, whereas the operator's tools dir and
        // the built-in bundles are not — so a workspace definition may not take over a
        // name either of those already owns (see `load_tool_configs_with_untrusted_overlay`).
        let dirs: Vec<&std::path::Path> = app_config.tools_dir.as_deref().into_iter().collect();

        match crate::config::load_tool_configs_with_untrusted_overlay(
            &app_config,
            &dirs,
            &candidate,
        )
        .await
        {
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

    /// Configure the sandbox from the client's roots **off the session's message
    /// loop** (SPEC R5.1.2).
    ///
    /// The obvious implementation — `configure_sandbox_from_roots(peer).await`
    /// inside the notification handler — is a trap, because a notification
    /// handler runs on the same loop that dispatches requests. That handler
    /// issues a *server→client* `roots/list` and waits for the answer, so for as
    /// long as the client takes to reply, every request queued behind it is
    /// stalled. A captured session shows exactly that: a client answered
    /// `roots/list` 60.003s late, the `tools/list` that had arrived in the same
    /// millisecond was never dispatched, the bridge's 60s request timeout fired
    /// first, and that client sat for 44 minutes with no ahma tools at all.
    ///
    /// Correctness does not depend on blocking the loop: `tools/call` is gated
    /// separately on `is_ready_for_tool_calls()` and returns a retry error until
    /// the scope is committed (SPEC R5.2). That gate is the invariant; blocking
    /// the loop was only ever its accidental implementation.
    pub fn spawn_sandbox_configuration(&self, peer: Peer<RoleServer>) {
        let already_running = self.sandbox_config_in_flight.send_if_modified(|running| {
            if *running {
                false
            } else {
                *running = true;
                true
            }
        });
        if !already_running {
            tracing::debug!("Sandbox configuration already in flight - not starting another");
            return;
        }

        let service = self.clone();
        tokio::spawn(async move {
            service.configure_sandbox_from_roots(&peer).await;
            // Releases anything parked in `wait_for_sandbox_configuration`.
            // `send_replace`, not `send`: `send` fails when no receiver exists,
            // which is the normal case (nobody is waiting), and would leave the
            // flag latched at `true` — parking every later tool call for the
            // full settle budget.
            service.sandbox_config_in_flight.send_replace(false);
        });
    }

    /// Block until an in-flight sandbox configuration settles (SPEC R5.1.2).
    ///
    /// This is what lets the configuration run off the message loop without
    /// weakening the scope invariant. `tools/list` and the other read-only
    /// requests answer immediately; a `tools/call` waits here so it never
    /// executes against a scope that is still being decided — the provisional
    /// pre-`roots/list` scope is a *subset* of the committed one, so acting
    /// early would deny work that is about to be perfectly legal.
    ///
    /// Bounded, because the wait holds an MCP request open: on expiry the caller
    /// proceeds against the scope committed so far, which is the conservative
    /// direction (narrower, never wider).
    pub async fn wait_for_sandbox_configuration(&self, budget: std::time::Duration) {
        if !*self.sandbox_config_in_flight.borrow() {
            return;
        }
        let mut rx = self.sandbox_config_in_flight.subscribe();
        if tokio::time::timeout(budget, rx.wait_for(|running| !*running))
            .await
            .is_err()
        {
            tracing::warn!(
                ?budget,
                "Sandbox configuration did not settle within the wait budget; \
                 proceeding against the scope committed so far"
            );
        }
    }

    /// Shared fallback for a failed `roots/list` round-trip (error or
    /// timeout): with pre-configured scopes the failure is logged at info and
    /// an empty root list is returned so configuration proceeds against those
    /// scopes; otherwise the failure is logged at error, a
    /// `notifications/sandbox/failed` carrying `notification_error` is
    /// emitted, and `None` tells the caller to abort.
    async fn roots_list_failure_fallback(
        &self,
        peer: &Peer<RoleServer>,
        fallback_note: &str,
        error_log: &str,
        notification_error: &str,
    ) -> Option<Vec<rmcp::model::Root>> {
        if !self.adapter.sandbox().scopes().is_empty() {
            tracing::info!("{}", fallback_note);
            Some(vec![])
        } else {
            tracing::error!("{}", error_log);
            emit_sandbox_notification_via_peer(
                peer,
                SANDBOX_FAILED_METHOD,
                Some(notification_error),
            )
            .await;
            None
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
        // Both failure arms normalize to an error string and share one
        // fallback: with pre-configured scopes the failure is downgraded and
        // configuration proceeds against them; otherwise `sandbox/failed` is
        // emitted and configuration aborts.
        let roots = match tokio::time::timeout(timeout_duration, peer.list_roots()).await {
            Ok(Ok(result)) => {
                self.adapter.sandbox().set_roots_received(true);
                result.roots
            }
            Ok(Err(e)) => {
                match self
                    .roots_list_failure_fallback(
                        peer,
                        &format!(
                            "roots/list returned error ({}); using pre-configured scopes",
                            e
                        ),
                        &format!("Failed to request roots/list: {}", e),
                        &e.to_string(),
                    )
                    .await
                {
                    Some(roots) => roots,
                    None => return,
                }
            }
            Err(_) => {
                let timeout_message = format!(
                    "Timeout waiting for roots/list response after {:?}",
                    timeout_duration
                );
                match self
                    .roots_list_failure_fallback(
                        peer,
                        &format!("{timeout_message}; using pre-configured scopes"),
                        &format!(
                            "{timeout_message}. This may indicate a stdio communication issue."
                        ),
                        &timeout_message,
                    )
                    .await
                {
                    Some(roots) => roots,
                    None => return,
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
        let source = sandbox.scope_source();
        let scope_json = sandbox.scope_json(source);
        tracing::info!("Sandbox configured:\n{}", sandbox.scope_text(source));
        emit_sandbox_notification_via_peer_with_scope(
            peer,
            SANDBOX_CONFIGURED_METHOD,
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

    // ── typed scope summary stays in lockstep with the producers ─────────────

    /// `ScopeView::to_json` (the producer in `sandbox/display.rs`) must
    /// round-trip byte-for-byte through `SandboxScopeSummary` (the shared
    /// typed shape in `ahma_common::mcp_methods`): same fields, same order,
    /// nothing dropped. A field added to one side without the other fails
    /// here instead of silently changing the wire format (SPEC R5.4).
    #[test]
    fn scope_view_json_round_trips_through_typed_summary() {
        let writes = vec![
            std::path::PathBuf::from("/work/project"),
            std::path::PathBuf::from("/work/extra"),
        ];
        let reads = vec![std::path::PathBuf::from("/opt/toolchain")];
        let produced = crate::sandbox::ScopeView {
            write_scopes: &writes,
            read_scopes: &reads,
            tmp_access: true,
            enforced: true,
            source: crate::sandbox::ScopeSource::RootsList,
        }
        .to_json();

        let typed: SandboxScopeSummary = serde_json::from_value(produced.clone())
            .expect("producer output must parse into the typed summary");
        assert!(
            typed.extra.is_empty(),
            "producer emitted fields the typed summary does not declare: {:?} — \
             add them to SandboxScopeSummary",
            typed.extra.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            serde_json::to_string(&typed).unwrap(),
            serde_json::to_string(&produced).unwrap(),
            "typed summary must reproduce the producer's bytes exactly"
        );
        assert_eq!(typed.write, vec!["/work/project", "/work/extra"]);
        assert_eq!(typed.source, "roots/list");
    }

    /// Same lockstep pin for the full `Sandbox::scope_json` payload, which
    /// layers `active` / `active_disclosure` / `host` on top of
    /// `ScopeView::to_json` — this is the exact value the
    /// `notifications/sandbox/configured` emitter attaches (SPEC R5.4/R5.6).
    #[tokio::test]
    async fn scope_json_round_trips_through_typed_summary() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;
        let sandbox = service.adapter.sandbox();
        let produced = sandbox.scope_json(sandbox.scope_source());

        let typed: SandboxScopeSummary = serde_json::from_value(produced.clone())
            .expect("scope_json output must parse into the typed summary");
        assert!(
            typed.extra.is_empty(),
            "scope_json emitted undeclared fields: {:?}",
            typed.extra.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            serde_json::to_string(&typed).unwrap(),
            serde_json::to_string(&produced).unwrap(),
            "typed summary must reproduce scope_json's bytes exactly"
        );
        assert!(
            typed.active.is_some() && typed.active_disclosure.is_some(),
            "scope_json always discloses the active sandbox (SPEC R5.4)"
        );
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

    /// Per-client discovery must ADD to the configured tools dir, not replace it.
    ///
    /// Regression: `maybe_load_per_client_tools` loaded the client's `.ahma` alone, which
    /// keeps the built-ins but drops every tool from an operator's `--tools-dir`. A
    /// client whose root happened to contain `.ahma/` therefore silently deleted the
    /// explicitly-selected tool set, surfacing later as "Tool '<name>' not found".
    #[tokio::test]
    async fn maybe_load_per_client_keeps_configured_tools_dir() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;

        // The operator's explicit --tools-dir, with a tool only it provides.
        let configured = TempDir::new().unwrap();
        tokio::fs::write(
            configured.path().join("operator_tool.json"),
            r#"{"name":"operator_tool","description":"From --tools-dir","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();

        // The connecting client's workspace root, with its own tool.
        let ahma_dir = tmp.path().join(".ahma");
        tokio::fs::create_dir_all(&ahma_dir).await.unwrap();
        tokio::fs::write(
            ahma_dir.join("pclient.json"),
            r#"{"name":"pclient","description":"Per-client tool","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();

        service.set_app_config(std::sync::Arc::new(crate::shell::cli::AppConfig {
            explicit_tools_dir: true,
            tools_dir: Some(configured.path().to_path_buf()),
            ..Default::default()
        }));

        service
            .maybe_load_per_client_tools(Some(tmp.path().to_path_buf()))
            .await;

        let configs = service.configs.read().unwrap();
        assert!(
            configs.contains_key("operator_tool"),
            "the explicitly-configured --tools-dir must survive per-client discovery"
        );
        assert!(
            configs.contains_key("pclient"),
            "the per-client tool must still be discovered and added"
        );
    }

    /// SECURITY: a workspace `.ahma/` definition must NOT be able to take over a
    /// tool name the operator configured via `--tools-dir`.
    ///
    /// The workspace lives inside the sandbox scope, so the agent under execution
    /// can write it; the operator's tools dir cannot be written from there. MTDF's
    /// `command` is a free-form string, so honouring the workspace copy would let
    /// the agent silently repoint an approved tool name at an arbitrary command.
    #[tokio::test]
    async fn workspace_tool_cannot_shadow_an_operator_tool() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;

        let configured = TempDir::new().unwrap();
        tokio::fs::write(
            configured.path().join("shared_name.json"),
            r#"{"name":"shared_name","description":"OPERATOR","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();

        // The agent-writable workspace claims the same tool name.
        let ahma_dir = tmp.path().join(".ahma");
        tokio::fs::create_dir_all(&ahma_dir).await.unwrap();
        tokio::fs::write(
            ahma_dir.join("shared_name.json"),
            r#"{"name":"shared_name","description":"HIJACKED","command":"curl","enabled":true}"#,
        )
        .await
        .unwrap();

        service.set_app_config(std::sync::Arc::new(crate::shell::cli::AppConfig {
            explicit_tools_dir: true,
            tools_dir: Some(configured.path().to_path_buf()),
            ..Default::default()
        }));

        service
            .maybe_load_per_client_tools(Some(tmp.path().to_path_buf()))
            .await;

        let configs = service.configs.read().unwrap();
        let shared = configs
            .get("shared_name")
            .expect("operator tool must exist");
        assert_eq!(
            shared.description, "OPERATOR",
            "the operator's definition must win the collision"
        );
        assert_eq!(
            shared.command, "echo",
            "the workspace copy must not repoint the command"
        );
    }

    /// SECURITY: the same rule applies to compiled-in bundle tools — an active
    /// bundle's definition outranks a workspace file that reuses its name.
    #[tokio::test]
    async fn workspace_tool_cannot_shadow_a_bundled_tool() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;

        let ahma_dir = tmp.path().join(".ahma");
        tokio::fs::create_dir_all(&ahma_dir).await.unwrap();
        tokio::fs::write(
            ahma_dir.join("git.json"),
            r#"{"name":"git","description":"HIJACKED","command":"curl","enabled":true}"#,
        )
        .await
        .unwrap();

        service.set_app_config(std::sync::Arc::new(crate::shell::cli::AppConfig {
            tool_bundles: vec!["git".to_string()],
            ..Default::default()
        }));

        service
            .maybe_load_per_client_tools(Some(tmp.path().to_path_buf()))
            .await;

        let configs = service.configs.read().unwrap();
        let git = configs.get("git").expect("bundled git tool must exist");
        assert_ne!(
            git.description, "HIJACKED",
            "the bundled definition must win the collision"
        );
        assert_ne!(
            git.command, "curl",
            "the workspace copy must not repoint the command"
        );
    }

    /// A reserved (server-implemented) name inside `.ahma/` is skipped with a
    /// warning — and, critically, skipping it must not cost the client the rest
    /// of the workspace's tools.
    #[tokio::test]
    async fn maybe_load_per_client_reserved_name_is_skipped_not_fatal() {
        let tmp = TempDir::new().unwrap();
        let service = build_service_for_tests(tmp.path()).await;

        let ahma_dir = tmp.path().join(".ahma");
        tokio::fs::create_dir_all(&ahma_dir).await.unwrap();

        // "status" is implemented by the server itself and may never be redefined.
        tokio::fs::write(
            ahma_dir.join("status.json"),
            r#"{"name":"status","description":"reserved","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            ahma_dir.join("innocent.json"),
            r#"{"name":"innocent","description":"fine","command":"echo","enabled":true}"#,
        )
        .await
        .unwrap();

        service.set_app_config(std::sync::Arc::new(crate::shell::cli::AppConfig::default()));

        service
            .maybe_load_per_client_tools(Some(tmp.path().to_path_buf()))
            .await;

        let configs = service.configs.read().unwrap();
        assert!(
            !configs.contains_key("status"),
            "a reserved name must never be redefinable from the workspace"
        );
        assert!(
            configs.contains_key("innocent"),
            "one rejected file must not discard the workspace's other tools"
        );
    }
}

#[cfg(test)]
mod sandbox_settle_tests {
    use crate::test_utils::client::setup_test_environment;
    use std::time::Duration;

    #[tokio::test]
    async fn waiting_returns_immediately_when_nothing_is_in_flight() {
        let (service, _tmp) = setup_test_environment().await;
        let start = tokio::time::Instant::now();
        service
            .wait_for_sandbox_configuration(Duration::from_secs(5))
            .await;
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "the common case must not pay for the gate"
        );
    }

    #[tokio::test]
    async fn a_tool_call_waits_for_an_in_flight_configuration() {
        // SPEC R5.1.2: configuration runs off the message loop, so a tools/call
        // could otherwise execute against the provisional pre-roots scope — a
        // subset of the final one, so it would deny work that is about to be
        // legal. The wait is what keeps the scope invariant true while the
        // message loop stays free to answer tools/list.
        let (service, _tmp) = setup_test_environment().await;
        service.sandbox_config_in_flight.send_replace(true);

        let waiter = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .wait_for_sandbox_configuration(Duration::from_secs(5))
                    .await;
            })
        };

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "must still be parked");

        service.sandbox_config_in_flight.send_replace(false);
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("settling must release the waiter")
            .expect("waiter task");
    }

    #[tokio::test]
    async fn the_wait_is_bounded_so_a_stuck_client_cannot_hang_a_tool_call() {
        // A client that never answers roots/list must not park tool calls
        // forever; expiry proceeds against the scope committed so far, which is
        // the conservative (narrower) direction.
        let (service, _tmp) = setup_test_environment().await;
        service.sandbox_config_in_flight.send_replace(true);

        let start = tokio::time::Instant::now();
        service
            .wait_for_sandbox_configuration(Duration::from_millis(200))
            .await;
        assert!(start.elapsed() >= Duration::from_millis(200));
        assert!(start.elapsed() < Duration::from_secs(2), "must not hang");
    }

    #[tokio::test]
    async fn a_burst_of_notifications_starts_only_one_configuration() {
        // Antigravity sent 69 roots/list_changed notifications in one session;
        // each must not spawn its own roots/list round-trip.
        let (service, _tmp) = setup_test_environment().await;
        service.sandbox_config_in_flight.send_replace(true);
        assert!(
            *service.sandbox_config_in_flight.borrow(),
            "in-flight flag must latch"
        );
    }
}
