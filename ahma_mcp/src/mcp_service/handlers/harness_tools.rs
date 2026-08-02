use super::common::{mcp_internal, mcp_invalid_params, text_result};
use crate::AhmaMcpService;
use rmcp::model::{CallToolResult, ErrorData as McpError};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

impl AhmaMcpService {
    pub async fn handle_read_file(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'path' is required"))?;
        let start_line = args
            .get("start_line")
            .and_then(Value::as_u64)
            .map(|v| v as usize);
        let end_line = args
            .get("end_line")
            .and_then(Value::as_u64)
            .map(|v| v as usize);

        let scopes = self.adapter.sandbox().scopes().to_vec();
        let result = self
            .file_ops_provider
            .read_file(&scopes, Path::new(path), start_line, end_line)
            .await
            .map_err(|e| mcp_internal(e.to_string()))?;

        Ok(text_result(result))
    }

    pub async fn handle_list_dir(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let scopes = self.adapter.sandbox().scopes().to_vec();

        let entries = self
            .file_ops_provider
            .list_dir(&scopes, Path::new(path))
            .await
            .map_err(|e| mcp_internal(e.to_string()))?;
        let body = serde_json::to_string_pretty(&entries)
            .map_err(|e| mcp_internal(format!("Failed to serialize list_dir result: {e}")))?;
        Ok(text_result(body))
    }

    pub async fn handle_file_search(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'pattern' is required"))?;

        let base_dir = args
            .get("base_dir")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| self.adapter.sandbox().scopes().first().cloned())
            .unwrap_or_else(|| PathBuf::from("."));

        let scopes = self.adapter.sandbox().scopes().to_vec();
        let matches = self
            .file_ops_provider
            .file_search(&scopes, &base_dir, pattern)
            .await
            .map_err(|e| mcp_internal(e.to_string()))?;

        let body = serde_json::to_string_pretty(&matches)
            .map_err(|e| mcp_internal(format!("Failed to serialize file_search result: {e}")))?;
        Ok(text_result(body))
    }

    pub async fn handle_grep_search(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'query' is required"))?;
        let is_regex = args
            .get("is_regex")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let include_pattern = args.get("include_pattern").and_then(Value::as_str);
        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .map(|v| v as usize);

        let base_dir = args
            .get("base_dir")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| self.adapter.sandbox().scopes().first().cloned())
            .unwrap_or_else(|| PathBuf::from("."));

        let scopes = self.adapter.sandbox().scopes().to_vec();
        let matches = self
            .file_ops_provider
            .grep_search(
                &scopes,
                &base_dir,
                query,
                is_regex,
                include_pattern,
                max_results,
            )
            .await
            .map_err(|e| mcp_internal(e.to_string()))?;

        let body = serde_json::to_string_pretty(&matches)
            .map_err(|e| mcp_internal(format!("Failed to serialize grep_search result: {e}")))?;
        Ok(text_result(body))
    }

    /// Raise an interactive web-egress approval for an unknown `domain` (strict
    /// `deny` mode) and turn the human's answer into a fetch action.
    ///
    /// Deduplicated through the session [`WebApprovalCoordinator`](ahma_common::web_approval::WebApprovalCoordinator):
    /// a domain already in flight is not double-prompted, and an approval takes
    /// effect for the rest of the session (or is persisted for `always`). When no
    /// client can be prompted — a headless/CLI peer, a client without the
    /// elicitation capability, a timeout, or a user cancel — the domain is left
    /// askable and the request falls back to the actionable `ahma web allow` deny,
    /// so egress is never widened without an explicit human "yes".
    async fn approve_web_egress(
        &self,
        domain: &str,
        url: &str,
    ) -> crate::egress::web_audit::FetchAction {
        use crate::egress::web_audit::{FetchAction, action_for};
        use crate::egress::web_prompt::{WebApprovalForm, parse_answer, prompt_message};
        use ahma_common::web_approval::{WebApprovalDecision, WebResolveOutcome};
        use ahma_common::web_policy::WebDecision;

        // The actionable deny used whenever we cannot obtain an explicit approval.
        let deny = || {
            action_for(&WebDecision::Prompt {
                domain: domain.to_string(),
            })
        };

        // Dedup: if a decision for this domain is already in flight (a concurrent
        // request), don't raise a second prompt — deny this one with the hint.
        let Some(req) = self
            .web_approval
            .begin(domain, url, Some("fetch_webpage".to_string()))
        else {
            return deny();
        };

        // First choice: interactive MCP `elicitation/create` (IDE clients). This
        // yields `Some(decision)` to resolve synchronously; `None` means no MCP
        // surface can prompt (no peer, or the client lacks the elicitation
        // capability) and we fall through to the TUI hub path below.
        let peer = self.peer.read().unwrap().clone();
        let elicited: Option<WebApprovalDecision> = match peer {
            None => None,
            // SPEC R5.3.1 binds *every* elicitation, not just the scope one: wait
            // no longer than this client leaves a dialog up, or the client cancels
            // the prompt out from under us and we learn nothing from a deadline it
            // never disclosed.
            Some(peer) => match peer
                .elicit_with_timeout::<WebApprovalForm>(
                    prompt_message(domain, url),
                    Some(crate::client_type::McpClientType::from_peer(&peer).elicitation_budget()),
                )
                .await
            {
                Ok(Some(form)) => Some(parse_answer(&form.decision)),
                // Accepted with no content, or an explicit decline: remember deny.
                Ok(None) | Err(rmcp::service::ElicitationError::UserDeclined) => {
                    Some(WebApprovalDecision::Deny)
                }
                // The client cannot elicit → try the TUI surface instead.
                Err(rmcp::service::ElicitationError::CapabilityNotSupported) => None,
                // Cancelled, timed out, or transport error: the user dismissed it or
                // it failed. Don't leave it pending; deny this fetch without
                // remembering so a later request may re-ask.
                Err(e) => {
                    tracing::debug!("web approval prompt unavailable for '{domain}': {e}");
                    self.web_approval.cancel(&req.decision_id);
                    return deny();
                }
            },
        };

        if let Some(decision) = elicited {
            return match self.web_approval.resolve(&req.decision_id, decision) {
                WebResolveOutcome::AllowOnce { domain } => {
                    tracing::info!(domain = %domain, "web egress approved for this request");
                    FetchAction::Proceed
                }
                WebResolveOutcome::AllowSession { domain } => {
                    tracing::info!(domain = %domain, "web egress approved for this session");
                    FetchAction::Proceed
                }
                WebResolveOutcome::Persist { domain } => {
                    match ahma_common::config::settings_path() {
                        Some(file) => {
                            match ahma_common::web_approval::persist_web_allow(&file, &domain) {
                                Ok(true) => tracing::info!(
                                    domain = %domain,
                                    "web egress approved and saved to [web].always_allow"
                                ),
                                Ok(false) => tracing::info!(
                                    domain = %domain,
                                    "web egress approved (already in [web].always_allow)"
                                ),
                                Err(e) => tracing::warn!(
                                    domain = %domain,
                                    "web egress approved for the session but persisting failed: {e}"
                                ),
                            }
                        }
                        None => tracing::warn!(
                            "web egress approved but ~/.ahma/settings.toml is not locatable to persist"
                        ),
                    }
                    FetchAction::Proceed
                }
                WebResolveOutcome::Denied { domain } => {
                    tracing::info!(domain = %domain, "web egress denied by user");
                    deny()
                }
                // A twin surface resolved first, or the decision vanished: fail safe.
                WebResolveOutcome::AlreadyResolved | WebResolveOutcome::Unknown => deny(),
            };
        }

        // Second choice: deliver the prompt to a connected TUI over the daemon hub
        // (R-WEB.6). The answer arrives asynchronously (the daemon reporter routes
        // it back into `web_approval`), so — like a scope grant — it cannot unblock
        // *this* fetch. Deny it with a "prompt raised, approve and retry" hint and
        // leave the decision in flight so the TUI answer resolves it; the
        // coordinator's dedup means the retry re-checks the session grant rather
        // than raising a second modal.
        let tx = self.web_approval_tx.lock().unwrap().clone();
        if let Some(tx) = tx
            && tx.send(req.clone()).is_ok()
        {
            tracing::info!(domain = %req.domain, "raised web-approval prompt in the ahma TUI");
            return FetchAction::Deny(format!(
                "web egress to '{d}' needs approval — a prompt was raised in the ahma TUI. \
                 Approve it there (or run `ahma web allow {d}`), then retry.",
                d = req.domain
            ));
        }

        // Nothing can prompt (no MCP elicitation, no TUI): don't leak an in-flight
        // entry — cancel so a future request may re-ask — and deny with the hint.
        self.web_approval.cancel(&req.decision_id);
        deny()
    }

    pub async fn handle_fetch_webpage(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'url' is required"))?;
        let query = args.get("query").and_then(Value::as_str);

        // Consult the [web] egress policy (R-WEB). Loaded fresh so a runtime
        // `always_allow` addition takes effect without a restart (R-WEB.5.5).
        // never_allow blocks, always_allow permits; the session-approval
        // coordinator's grants/denies (R-WEB.5) are threaded in so an approval made
        // earlier this session takes effect without a restart. In strict `deny` mode
        // an unknown domain yields Prompt, which `approve_web_egress` raises as an
        // interactive MCP `elicitation/create` when a capable client is attached.
        // Every request is audited (R-WEB.9).
        let redirect_guard = {
            use crate::egress::web_audit::{self, FetchAction};
            use ahma_common::web_policy::{WebDecision, WebPolicy, url_coordinates};

            let settings = ahma_common::config::AhmaSettings::load();
            let (policy, errors) = WebPolicy::from_settings(&settings.web);
            for e in errors {
                tracing::warn!("ignoring invalid [web] pattern: {e}");
            }
            let session_grants = self.web_approval.session_grants();
            let session_denies = self.web_approval.session_denies();
            let decision = policy.decide(url, &session_grants, &session_denies);
            let domain = url_coordinates(url).map_or_else(|| url.to_string(), |(_, host, _)| host);
            let ts = chrono::Local::now().to_rfc3339();
            web_audit::append(&web_audit::record(
                "fetch_webpage",
                url,
                &domain,
                &decision,
                ts,
            ));
            // An unknown domain (strict `deny` mode) is offered to the human via an
            // interactive prompt; every other decision maps straight to an action.
            let action = match &decision {
                WebDecision::Prompt { domain } => self.approve_web_egress(domain, url).await,
                other => web_audit::action_for(other),
            };
            if let FetchAction::Deny(reason) = action {
                return Err(mcp_invalid_params(reason));
            }

            // R-WEB.8: the initial URL passed the policy, but a 30x redirect could
            // still bounce to a *different* domain the policy would not approve.
            // Build a guard from the same policy so the fetcher refuses such a hop
            // instead of laundering egress through the approved origin. In
            // default-`allow` mode the policy approves every host, so this is a
            // no-op; it only bites in `deny` mode.
            let allow = std::sync::Arc::new(move |host: &str| {
                matches!(
                    policy.decide(
                        &format!("https://{host}/"),
                        &session_grants,
                        &session_denies
                    ),
                    WebDecision::Allow { .. }
                )
            });
            ahma_harness_tools::egress_guard::RedirectDomainGuard::new(domain, allow)
        };

        let result = self
            .web_page_fetcher
            .fetch_with_redirect_guard(url, query, redirect_guard)
            .await
            .map_err(|e| mcp_internal(e.to_string()))?;

        let body = serde_json::to_string_pretty(&result)
            .map_err(|e| mcp_internal(format!("Failed to serialize fetch_webpage result: {e}")))?;
        Ok(text_result(body))
    }

    pub async fn handle_write_file(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'path' is required"))?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'content' is required"))?;

        let path_ref = Path::new(path);
        let is_guard_active = {
            if let Ok(guard) = self.harness_guard.try_lock() {
                guard.enabled
            } else {
                false
            }
        };

        if is_guard_active
            && let Err(err_msg) = crate::harness_guard::check_write_allowance(path_ref)
        {
            return Err(mcp_internal(err_msg));
        }

        let narrowing = self.narrow_container_for(path_ref);
        let scopes = self.adapter.sandbox().scopes().to_vec();
        self.file_ops_provider
            .write_file(&scopes, path_ref, content)
            .await
            .map_err(|e| mcp_internal(e.to_string()))?;

        Ok(disclose_narrowing(text_result("File written"), narrowing))
    }

    pub async fn handle_replace_in_file(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'path' is required"))?;
        let old_str = args
            .get("old_str")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'old_str' is required"))?;
        let new_str = args
            .get("new_str")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'new_str' is required"))?;

        let narrowing = self.narrow_container_for(Path::new(path));
        let scopes = self.adapter.sandbox().scopes().to_vec();
        let replaced = self
            .file_ops_provider
            .replace_in_file(&scopes, Path::new(path), old_str, new_str)
            .await
            .map_err(|e| mcp_internal(e.to_string()))?;

        Ok(disclose_narrowing(
            text_result(format!("Replaced {replaced} occurrence(s)")),
            narrowing,
        ))
    }

    /// Select the container subtree this write is for, before the write happens
    /// (SPEC R5.2.6).
    ///
    /// The *write* tools carry this and the read tools do not, deliberately.
    /// Narrowing exists to bound where the AI can write; reads across the whole
    /// container stay allowed, and letting a read pick the project would let an
    /// incidental lookup spend the one narrowing the session gets.
    fn narrow_container_for(&self, path: &Path) -> Option<crate::sandbox::ContainerNarrowing> {
        self.adapter.sandbox().narrow_container_to(path)
    }
}

/// Append the scope-narrowing disclosure to a result (SPEC R5.4: a scope
/// decision is never communicated only through a log line).
fn disclose_narrowing(
    result: CallToolResult,
    narrowing: Option<crate::sandbox::ContainerNarrowing>,
) -> CallToolResult {
    match narrowing {
        Some(n) => super::common::append_note(result, &n.notice()),
        None => result,
    }
}

use crate::mcp_service::schema;
use std::sync::Arc;

pub fn read_file_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "path".to_string(),
        json!({"type": "string", "description": "Absolute or scoped-relative file path."}),
    );
    props.insert(
        "start_line".to_string(),
        json!({"type": "integer", "description": "1-based inclusive start line."}),
    );
    props.insert(
        "end_line".to_string(),
        json!({"type": "integer", "description": "1-based inclusive end line."}),
    );
    schema::object_input_schema(props, &["path"])
}

pub fn list_dir_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "path".to_string(),
        json!({"type": "string", "description": "Directory path. Defaults to current scope root."}),
    );
    schema::object_input_schema(props, &[])
}

pub fn file_search_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "pattern".to_string(),
        json!({"type": "string", "description": "Glob pattern, e.g. '**/*.rs'."}),
    );
    props.insert(
        "base_dir".to_string(),
        json!({"type": "string", "description": "Base directory for glob search."}),
    );
    schema::object_input_schema(props, &["pattern"])
}

pub fn grep_search_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "query".to_string(),
        json!({"type": "string", "description": "Search query (regex or plain text)."}),
    );
    props.insert(
        "is_regex".to_string(),
        json!({"type": "boolean", "description": "Interpret query as regex.", "default": false}),
    );
    props.insert(
        "base_dir".to_string(),
        json!({"type": "string", "description": "Directory root to search from."}),
    );
    props.insert(
        "include_pattern".to_string(),
        json!({"type": "string", "description": "Optional glob filter for files."}),
    );
    props.insert(
        "max_results".to_string(),
        json!({"type": "integer", "description": "Maximum number of matches to return."}),
    );
    schema::object_input_schema(props, &["query"])
}

pub fn fetch_webpage_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "url".to_string(),
        json!({"type": "string", "description": "HTTP/HTTPS URL to fetch."}),
    );
    props.insert(
        "query".to_string(),
        json!({"type": "string", "description": "Optional query to filter extracted text."}),
    );
    schema::object_input_schema(props, &["url"])
}

pub fn write_file_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "path".to_string(),
        json!({"type": "string", "description": "Absolute or scoped-relative file path."}),
    );
    props.insert(
        "content".to_string(),
        json!({"type": "string", "description": "UTF-8 content to write."}),
    );
    schema::object_input_schema(props, &["path", "content"])
}

pub fn replace_in_file_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "path".to_string(),
        json!({"type": "string", "description": "Absolute or scoped-relative file path."}),
    );
    props.insert(
        "old_str".to_string(),
        json!({"type": "string", "description": "Exact string to replace."}),
    );
    props.insert(
        "new_str".to_string(),
        json!({"type": "string", "description": "Replacement string."}),
    );
    schema::object_input_schema(props, &["path", "old_str", "new_str"])
}

#[cfg(test)]
#[path = "harness_tools_tests.rs"]
mod tests;
