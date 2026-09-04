use ahma_common::daemon_hub::{ClientMsg, DaemonChatMessage};
use ahma_http_mcp_client::streamable::{
    ConflictRetryPolicy, ConnectOptions, Connector, StreamableHttpMcpClient, ToolCallOutcome,
};
use ahma_llm_monitor::ChatMessage;
use ahma_llm_monitor::client::LlmClient;
use ahma_mcp::ActiveAgentSession;
use async_trait::async_trait;
use futures::future::join_all;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Token(String),
    /// Reasoning / "thinking" text streamed from the model, shown in lower
    /// contrast so the user can see it is thinking without it dominating.
    Thinking(String),
    Done,
    Error(String),
    ToolCallStarted {
        id: String,
        name: String,
        args: String,
    },
    ToolCallFinished {
        id: String,
        result: String,
        failed: bool,
    },
    Usage(ahma_llm_monitor::client::TokenUsage),
    /// The model's response was cut off by a length/context limit rather than
    /// finishing on its own — surfaced loudly (never silently) so a stalled
    /// "thinking forever" turn is visibly explained, right before the agent
    /// requests a continuation.
    Truncated {
        reason: String,
    },
}

#[async_trait]
pub trait AgentApprovalGate: Send + Sync {
    async fn request_approval(&self, id: &str, tool: &str, args: &str) -> bool;
}

#[derive(Clone)]
pub struct McpChatConfig {
    pub base_url: String,
    pub workspace_root: PathBuf,
    pub session_id: Option<String>,
    pub external_http_servers: BTreeMap<String, String>,
    pub max_turns: u32,
    pub tool_approval: bool,
    /// All external MCP servers (HTTP and stdio) for agent tool routing.
    pub mcp_connections: ahma_mcp::mcp_client::McpConnectionManager,
    pub minimize_tokens: bool,
    pub small_model_harness: bool,
    /// Model context window in tokens (from `--context-length`, settings, or
    /// profile). Sizes the conversation and tool-result character budgets so
    /// small local models are not flooded past their window.  `None` uses
    /// generous defaults (tighter ones when `small_model_harness` is on).
    pub context_length: Option<u32>,
}

// ─── Context budgets (small-model support) ───────────────────────────────────
//
// Local models have hard context windows; flooding them silently truncates
// the *oldest* content (often the system prompt) and degrades tool use.
// These budgets keep the conversation and individual tool results inside a
// predictable share of the window.  All budgets are in characters with a
// ~4 chars/token approximation; exact tokenisation is model-specific and not
// worth a tokenizer dependency here.

/// Approximate characters per token for budget math.
const CHARS_PER_TOKEN: usize = 4;
/// Single tool-result cap (chars) when the context window is unknown.
const DEFAULT_TOOL_RESULT_CHAR_CAP: usize = 60_000;
/// Tighter tool-result cap under `--small-model-harness`.
const SMALL_MODEL_TOOL_RESULT_CHAR_CAP: usize = 8_000;
/// Conversation budget (chars) when the context window is unknown.
const DEFAULT_CONVERSATION_CHAR_BUDGET: usize = 240_000;
/// Tighter conversation budget under `--small-model-harness`.
const SMALL_MODEL_CONVERSATION_CHAR_BUDGET: usize = 24_000;

/// Fraction of the context window (measured from real reported prompt-token
/// usage) at which proactive compaction triggers for small/local models.
/// Tighter than [`DEFAULT_COMPACTION_THRESHOLD`]: quality degrades earlier
/// for small models than for large hosted ones.
const SMALL_MODEL_COMPACTION_THRESHOLD: f32 = 0.70;
/// Fraction of the context window at which proactive compaction triggers
/// when the model is not flagged as small/local.
const DEFAULT_COMPACTION_THRESHOLD: f32 = 0.85;
/// How many of the most recent raw messages proactive compaction always
/// leaves untouched, verbatim — recency matters most for the model's
/// immediate coherence, and tool-call/tool-result pairs must stay adjacent.
const COMPACTION_KEEP_RECENT_MESSAGES: usize = 4;

/// Instructions for the isolated, tools-off compaction call — sent as that
/// call's only user message (not layered onto the real conversation's system
/// prompt). Structured sections (not "just summarize") survive repeated
/// compaction rounds without drifting, and give the model something concrete
/// to act on afterward instead of a vague paragraph.
const COMPACTION_INSTRUCTIONS: &str = "You are compacting an earlier portion of a long tool-using \
    conversation so work can continue without the full history. Do not call any tools. \
    Read the transcript below and respond with EXACTLY this structure, one section per line \
    (bullet points under each, terse, no preamble):\n\n\
    Goal: <the original task, one line>\n\
    Decisions and facts established: <bullets>\n\
    Files touched: <bullets, with paths and line numbers where known>\n\
    Approaches already tried and why they failed: <bullets, or \"none\">\n\
    Pending todos: <bullets>\n\
    Next step: <one line>\n\n\
    Transcript to compact:\n\n";

/// System-prompt suffix that asks for terse output when minimizing tokens.
const MINIMIZE_CONCISENESS_RULE: &str = "\n\nRespond concisely. No preamble, no conversational filler. Output only the tool call, code, or bare answer.";

/// Policy controlling how much context is sent to the model: the per-result and
/// per-conversation character budgets, plus any system-prompt augmentation for
/// token minimization. This is the single seam for context handling — swap in a
/// smarter (e.g. real-tokenizer) strategy in future without touching the agent
/// loop. [`BudgetStrategy`] is the default character-heuristic implementation.
pub trait ContextStrategy: Send + Sync {
    /// Max characters for a single tool result injected into the conversation.
    fn tool_result_char_cap(&self) -> usize;
    /// Total character budget for the whole conversation sent to the model.
    fn conversation_char_budget(&self) -> usize;
    /// Optional suffix appended to the system prompt (e.g. a conciseness rule
    /// when minimizing tokens). `None` leaves the prompt unchanged.
    fn system_prompt_suffix(&self) -> Option<&'static str> {
        None
    }
    /// Fraction of the context window (0.0-1.0), measured from the *real*
    /// reported prompt-token usage, at which proactive compaction should
    /// trigger — replacing the oldest non-recent history with a structured
    /// summary before the model ever runs out of room. `None` disables
    /// proactive compaction; [`trim_conversation`]'s reactive hard-drop stays
    /// as the last-resort safety net either way.
    ///
    /// Quality degrades well before the hard token wall ("lost in the
    /// middle"), and that cliff comes sooner for small/local models than for
    /// large hosted ones — so the default is deliberately tighter under
    /// `small_model_harness`.
    fn compaction_threshold(&self) -> Option<f32> {
        None
    }
}

/// The default context strategy: character budgets derived from the model's
/// context window (or generous/tight defaults) using a ~4 chars/token heuristic.
pub struct BudgetStrategy {
    /// Model context window in tokens, when known.
    pub context_length: Option<u32>,
    /// Tighten budgets for small local models.
    pub small_model_harness: bool,
    /// Append the conciseness rule to the system prompt.
    pub minimize_tokens: bool,
}

impl ContextStrategy for BudgetStrategy {
    fn tool_result_char_cap(&self) -> usize {
        match self.context_length {
            // A single tool result may use at most a quarter of the window.
            Some(tokens) => ((tokens as usize) * CHARS_PER_TOKEN / 4).max(1_000),
            None if self.small_model_harness => SMALL_MODEL_TOOL_RESULT_CHAR_CAP,
            None => DEFAULT_TOOL_RESULT_CHAR_CAP,
        }
    }

    fn conversation_char_budget(&self) -> usize {
        match self.context_length {
            // Keep a quarter of the window free for the model's response.
            Some(tokens) => ((tokens as usize) * CHARS_PER_TOKEN * 3 / 4).max(4_000),
            None if self.small_model_harness => SMALL_MODEL_CONVERSATION_CHAR_BUDGET,
            None => DEFAULT_CONVERSATION_CHAR_BUDGET,
        }
    }

    fn system_prompt_suffix(&self) -> Option<&'static str> {
        self.minimize_tokens.then_some(MINIMIZE_CONCISENESS_RULE)
    }

    fn compaction_threshold(&self) -> Option<f32> {
        // Only meaningful when the window size is actually known — without it
        // there is no denominator to compute a fill fraction against, so the
        // reactive char-budget trim remains the only safety net.
        self.context_length.map(|_| {
            if self.small_model_harness {
                SMALL_MODEL_COMPACTION_THRESHOLD
            } else {
                DEFAULT_COMPACTION_THRESHOLD
            }
        })
    }
}

impl McpChatConfig {
    /// The context strategy for this run. Currently always a [`BudgetStrategy`];
    /// returning it through the [`ContextStrategy`] trait keeps the agent loop
    /// decoupled from the concrete choice.
    pub fn context_strategy(&self) -> BudgetStrategy {
        BudgetStrategy {
            context_length: self.context_length,
            small_model_harness: self.small_model_harness,
            minimize_tokens: self.minimize_tokens,
        }
    }
}

/// Character cap for a single tool result injected into the conversation.
/// Thin wrapper over the run's [`ContextStrategy`].
pub fn tool_result_char_cap(cfg: &McpChatConfig) -> usize {
    cfg.context_strategy().tool_result_char_cap()
}

/// Total character budget for the conversation sent to the model.
/// Thin wrapper over the run's [`ContextStrategy`].
pub fn conversation_char_budget(cfg: &McpChatConfig) -> usize {
    cfg.context_strategy().conversation_char_budget()
}

/// Truncate the middle of `s` to at most `cap` characters, keeping the head
/// (where commands/errors usually start) and the tail (where summaries and
/// exit codes land), with an explicit marker so the model knows content was
/// elided.
pub fn truncate_middle(s: &str, cap: usize) -> String {
    let total_chars = s.chars().count();
    if total_chars <= cap {
        return s.to_string();
    }
    let head_chars = cap * 3 / 5;
    let tail_chars = cap - head_chars;
    let head: String = s.chars().take(head_chars).collect();
    let tail: String = s
        .chars()
        .skip(total_chars.saturating_sub(tail_chars))
        .collect();
    format!(
        "{head}\n…[{} characters elided to fit the model context — use the `status` tool for the full output]…\n{tail}",
        total_chars - cap
    )
}

/// Trim the oldest non-system messages until the conversation fits `budget`
/// characters.  Three things are always preserved: the system prompt (first
/// message), the **first user message** — the original task/goal, so a long run
/// can never trim away its own objective and start wandering — and the two most
/// recent messages (the immediate task state).
/// Index of the first message that may be dropped/compacted: past any system
/// prompt (index 0) and the first user message right after it (the original
/// goal), both of which are always preserved verbatim so a long run can never
/// lose sight of its own objective.
fn protected_head(msg_json: &[serde_json::Value]) -> usize {
    let role_at = |i: usize| -> Option<&str> {
        msg_json
            .get(i)
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
    };
    let base_head = usize::from(role_at(0) == Some("system"));
    let pin_goal = role_at(base_head) == Some("user");
    base_head + usize::from(pin_goal)
}

pub fn trim_conversation(msg_json: &mut Vec<serde_json::Value>, budget: usize) {
    let content_len = |m: &serde_json::Value| -> usize {
        m.get("content")
            .and_then(|c| c.as_str())
            .map(|s| s.len())
            .unwrap_or(0)
    };

    let mut total: usize = msg_json.iter().map(content_len).sum();
    if total <= budget {
        return;
    }

    let protected_head = protected_head(msg_json);

    // Same trimming decisions as removing one message at a time from
    // `protected_head`, but tracked with a running total and applied as a
    // single batch `drain` instead of an O(n) `Vec::remove` per message.
    let mut dropped = 0usize;
    while total > budget && msg_json.len() - dropped > protected_head + 2 {
        total -= content_len(&msg_json[protected_head + dropped]);
        dropped += 1;
    }
    if dropped > 0 {
        msg_json.drain(protected_head..protected_head + dropped);
        tracing::info!(
            "small-model context: dropped {dropped} oldest message(s) to fit the {budget}-char conversation budget"
        );
        // Tell the model history was elided so it doesn't hallucinate it.
        msg_json.insert(
            protected_head,
            serde_json::json!({
                "role": "user",
                "content": format!("[{dropped} earlier message(s) were removed to fit your context window. Continue from the latest state below.]"),
            }),
        );
    }
}

/// Proactively compact the conversation when real reported usage crosses the
/// context strategy's [`ContextStrategy::compaction_threshold`] — replacing
/// the oldest non-recent history with a structured summary *before* the model
/// ever runs out of room, rather than reacting after the fact
/// ([`trim_conversation`]'s hard-drop, which remains the safety net if this
/// is disabled, fails, or there still isn't enough to compact).
///
/// Always preserves, verbatim: the system prompt, the pinned first user
/// message (the original goal — see [`protected_head`]), and the last
/// [`COMPACTION_KEEP_RECENT_MESSAGES`] raw messages (tool-call/result pairs
/// must stay adjacent, and recency matters most for immediate coherence).
///
/// `last_prompt_tokens` is the *real* prompt-token count reported by the most
/// recent completion — the actual measure of how full the window is, not a
/// char-count guess. `0` (no usage reported yet) is treated as "unknown,
/// nothing to do" rather than "0% full, trigger constantly".
async fn maybe_compact_conversation(
    client: &LlmClient,
    msg_json: &mut Vec<serde_json::Value>,
    cfg: &McpChatConfig,
    last_prompt_tokens: u32,
) {
    if last_prompt_tokens == 0 {
        return;
    }
    let strategy = cfg.context_strategy();
    let (Some(threshold), Some(context_length)) =
        (strategy.compaction_threshold(), cfg.context_length)
    else {
        return;
    };
    if context_length == 0 {
        return;
    }
    let fill = last_prompt_tokens as f32 / context_length as f32;
    if fill < threshold {
        return;
    }

    let protected_head = protected_head(msg_json);
    let keep_recent =
        COMPACTION_KEEP_RECENT_MESSAGES.min(msg_json.len().saturating_sub(protected_head));
    let compact_end = msg_json.len().saturating_sub(keep_recent);
    // Nothing meaningful to compact yet (conversation too short) — let it
    // grow; trim_conversation's hard limit is still there as a backstop.
    if compact_end <= protected_head {
        return;
    }

    let transcript = msg_json[protected_head..compact_end]
        .iter()
        .map(|m| {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("?");
            let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
            format!("[{role}] {content}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let dropped = compact_end - protected_head;

    // Isolated call: tools disabled, a fresh bounded prompt containing only
    // the material to compact — not "one more turn" appended to the
    // already-near-full conversation being compacted. Summarizing well is
    // itself hard; the same model already under context pressure is the
    // worst place to ask for it, so this deliberately does not reuse the
    // live msg_json / turn loop at all.
    let compaction_messages = vec![serde_json::json!({
        "role": "user",
        "content": format!("{COMPACTION_INSTRUCTIONS}{transcript}")
    })];

    match client
        .chat_completion_with_tools(compaction_messages, &[])
        .await
    {
        Ok(resp) if !resp.content.trim().is_empty() => {
            let summary_msg = serde_json::json!({
                "role": "user",
                "content": format!(
                    "[{dropped} earlier message(s) were proactively compacted to stay under \
                     {threshold:.0}% of the model's context window. Summary of what happened:]\n\n{}",
                    resp.content
                )
            });
            msg_json.splice(protected_head..compact_end, [summary_msg]);
            tracing::warn!(
                fill,
                threshold,
                dropped,
                "agent: proactively compacted conversation history"
            );
        }
        Ok(_) => {
            tracing::warn!(
                fill,
                threshold,
                "agent: proactive compaction returned an empty summary; leaving history as-is \
                 (reactive trim remains the backstop)"
            );
        }
        Err(e) => {
            tracing::warn!(
                fill,
                threshold,
                error = %e,
                "agent: proactive compaction call failed; leaving history as-is \
                 (reactive trim remains the backstop)"
            );
        }
    }
}

/// Convert internal `{role, content}` chat messages into the MCP sampling
/// wire format, where content is a typed `{type: "text", text}` object.
fn to_mcp_content_messages(messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    messages
        .into_iter()
        .map(|msg| {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
            serde_json::json!({
                "role": role,
                "content": {
                    "type": "text",
                    "text": content
                }
            })
        })
        .collect()
}

/// Concatenate the text of every `"text"`-typed content item in an MCP
/// sampling result's `content` array.
fn extract_sampling_completion_text(content_arr: &[serde_json::Value]) -> String {
    let mut completion_text = String::new();
    for item in content_arr {
        if item.get("type").and_then(|t| t.as_str()) == Some("text")
            && let Some(text) = item.get("text").and_then(|t| t.as_str())
        {
            completion_text.push_str(text);
        }
    }
    completion_text
}

/// Build the reqwest client and the `/mcp` URL to reach it, handling the
/// `unix://` base-url convention (a local Unix domain socket) separately
/// from a normal HTTP(S) base URL.
fn build_sampling_client(mcp: &McpChatConfig) -> Result<(reqwest::Client, String), String> {
    let builder = reqwest::Client::builder();
    let (request_base_url, builder) = if let Some(path) = mcp.base_url.strip_prefix("unix://") {
        #[cfg(unix)]
        {
            ("http://localhost".to_string(), builder.unix_socket(path))
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            (mcp.base_url.clone(), builder)
        }
    } else {
        (mcp.base_url.clone(), builder)
    };
    let client = builder.build().map_err(|e| e.to_string())?;
    let url = format!("{}/mcp", request_base_url);
    Ok((client, url))
}

async fn call_mcp_sampling_routed(
    mcp: &McpChatConfig,
    target_label: &str,
    messages: Vec<serde_json::Value>,
    system_prompt: Option<&str>,
) -> Result<ahma_llm_monitor::client::ChatCompletionResponse, String> {
    let (client, url) = build_sampling_client(mcp)?;
    let session_id = get_or_create_session(&client, &url, mcp).await?;

    let mcp_messages = to_mcp_content_messages(messages);

    let request_id = format!("route_tui_{}", uuid::Uuid::new_v4());
    let mut params = serde_json::json!({
        "messages": mcp_messages,
    });
    if let Some(sys) = system_prompt {
        params["systemPrompt"] = serde_json::json!(sys);
    }
    params["__route_target_label"] = serde_json::json!(target_label);

    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "sampling/createMessage",
        "params": params
    });

    let resp = client
        .post(&url)
        .header("mcp-session-id", &session_id)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("Failed to send sampling request: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {body_text}"));
    }

    let response_json = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    if let Some(error) = response_json.get("error") {
        let msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown error");
        return Err(msg.to_string());
    }

    let result = response_json
        .get("result")
        .ok_or_else(|| "Missing result in response".to_string())?;
    let content_arr = result
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| "Missing or invalid content in result".to_string())?;

    let completion_text = extract_sampling_completion_text(content_arr);

    // MCP sampling's stopReason: "endTurn" | "stopSequence" | "maxTokens" | ….
    // Normalize to the same "length" convention used for the direct HTTP
    // providers so callers have one truncation signal to check.
    let finish_reason = result
        .get("stopReason")
        .and_then(|r| r.as_str())
        .map(|r| if r == "maxTokens" { "length" } else { r }.to_string());

    Ok(ahma_llm_monitor::client::ChatCompletionResponse {
        content: completion_text,
        tool_calls: Vec::new(),
        assistant_message: serde_json::Value::Null,
        usage: None,
        finish_reason,
    })
}

/// Run one MCP-sampling-routed chat turn and forward the result (or error) onto
/// the agent event channel. Non-streaming: the response arrives whole.
async fn run_mcp_sampling_chat(
    mcp_cfg: McpChatConfig,
    target_label: String,
    messages: Vec<ChatMessage>,
    system_prompt: Option<String>,
    tx: Sender<AgentEvent>,
) {
    let msg_vals: Vec<serde_json::Value> = messages
        .into_iter()
        .map(|msg| serde_json::json!({"role": msg.role, "content": msg.content}))
        .collect();
    match call_mcp_sampling_routed(&mcp_cfg, &target_label, msg_vals, system_prompt.as_deref())
        .await
    {
        Ok(resp) => {
            let _ = tx.send(AgentEvent::Token(resp.content)).await;
            let _ = tx.send(AgentEvent::Done).await;
        }
        Err(e) => {
            let _ = tx.send(AgentEvent::Error(e)).await;
        }
    }
}

/// Run one plain (non-MCP) streaming chat turn, forwarding tokens onto the
/// agent event channel as they arrive.
async fn run_streaming_chat(
    client: LlmClient,
    messages: Vec<ChatMessage>,
    system_prompt: Option<String>,
    tx: Sender<AgentEvent>,
) {
    let base_url = client.base_url().to_string();
    info!(
        provider = %base_url,
        messages = messages.len(),
        "chat: starting stream"
    );
    let stream = client.chat_stream(messages, system_prompt.as_deref());
    tokio::pin!(stream);
    use futures::StreamExt;
    let mut first_token = true;
    while let Some(res) = stream.next().await {
        let token = match res {
            Ok(token) => token,
            Err(e) => {
                warn!(provider = %base_url, error = %e, "chat: stream error");
                let _ = tx.send(AgentEvent::Error(e.to_string())).await;
                return;
            }
        };
        if token.is_empty() {
            continue;
        }
        if first_token {
            info!(provider = %base_url, "chat: first token received");
            first_token = false;
        }
        let _ = tx.send(AgentEvent::Token(token)).await;
    }
    info!(provider = %base_url, "chat: stream complete");
    let _ = tx.send(AgentEvent::Done).await;
}

pub fn spawn_chat_task(
    client: LlmClient,
    messages: Vec<ChatMessage>,
    system_prompt: Option<String>,
    mcp: Option<McpChatConfig>,
    tx: Sender<AgentEvent>,
) {
    tokio::spawn(async move {
        if client.base_url().starts_with("mcp://") {
            let Some(mcp_cfg) = mcp else {
                let _ = tx
                    .send(AgentEvent::Error(
                        "MCP config missing for sampling".to_string(),
                    ))
                    .await;
                return;
            };
            let target_label = client
                .base_url()
                .strip_prefix("mcp://")
                .unwrap_or("")
                .to_string();
            run_mcp_sampling_chat(mcp_cfg, target_label, messages, system_prompt, tx).await;
            return;
        }

        run_streaming_chat(client, messages, system_prompt, tx).await;
    });
}

fn prepare_tool_definitions(
    available_tools: Vec<ahma_mcp::mcp_client::ToolInfo>,
) -> Vec<serde_json::Value> {
    available_tools
        .into_iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description.unwrap_or_else(|| "MCP tool callable from ahma".to_string()),
                    "parameters": tool.input_schema
                }
            })
        })
        .collect()
}

async fn execute_single_tool_call(
    call: ahma_llm_monitor::client::ChatToolCall,
    cfg: McpChatConfig,
    tx: Sender<AgentEvent>,
    gate: Arc<dyn AgentApprovalGate>,
) -> (String, String, serde_json::Value, bool) {
    let args_value = call.arguments;
    let args_str = serde_json::to_string(&args_value).unwrap_or_default();

    let approved = if needs_approval(&call.name, cfg.tool_approval) {
        // A previously granted "always allow" for this tool in this workspace
        // skips the prompt entirely.
        if crate::approvals::is_tool_approved(&cfg.workspace_root, &call.name).await {
            true
        } else {
            gate.request_approval(&call.id, &call.name, &args_str).await
        }
    } else {
        true
    };

    if !approved {
        let err_text = "Error: Tool execution rejected by user".to_string();
        let _ = tx
            .send(AgentEvent::ToolCallFinished {
                id: call.id.clone(),
                result: err_text.clone(),
                failed: true,
            })
            .await;
        return (
            call.id,
            call.name,
            serde_json::json!({"error": err_text}),
            true,
        );
    }

    let _ = tx
        .send(AgentEvent::ToolCallStarted {
            id: call.id.clone(),
            name: call.name.clone(),
            args: args_str,
        })
        .await;

    let result = dispatch_tool_execution(&call.name, args_value, &cfg).await;
    match result {
        Ok((text, failed)) => {
            let _ = tx
                .send(AgentEvent::ToolCallFinished {
                    id: call.id.clone(),
                    result: text.clone(),
                    failed,
                })
                .await;
            (
                call.id,
                call.name,
                serde_json::json!({"output": text}),
                failed,
            )
        }
        Err(e) => {
            let err_text = format!("Error: {e}");
            let _ = tx
                .send(AgentEvent::ToolCallFinished {
                    id: call.id.clone(),
                    result: err_text.clone(),
                    failed: true,
                })
                .await;
            (
                call.id,
                call.name,
                serde_json::json!({"error": err_text}),
                true,
            )
        }
    }
}

async fn dispatch_tool_execution(
    name: &str,
    args_value: serde_json::Value,
    cfg: &McpChatConfig,
) -> Result<(String, bool), String> {
    let Some((server, tool)) = name.split_once("::") else {
        return spawn_local_tool_call(cfg.clone(), name, args_value).await;
    };

    let conn = cfg.mcp_connections.clone();
    if conn.servers.iter().any(|s| s.name == server) {
        return conn
            .call_tool(name, args_value)
            .await
            .map_err(|e| format!("MCP tool error ({name}): {e}"));
    }

    let Some(base_url) = cfg.external_http_servers.get(server) else {
        return Err(format!("Unknown external MCP server `{server}`"));
    };
    let base = base_url.clone();
    spawn_external_tool_call_http(&base, tool, args_value).await
}

/// Fetch one assistant turn via MCP sampling (when the client targets an
/// `mcp://` base URL). Sampling is never streamed, so the caller still needs
/// to emit the returned content.
async fn fetch_completion_via_mcp_sampling(
    mcp: &Option<McpChatConfig>,
    target_label: &str,
    msg_json: &[serde_json::Value],
    tx: &Sender<AgentEvent>,
    system_prompt: &Option<String>,
) -> Option<(ahma_llm_monitor::client::ChatCompletionResponse, bool)> {
    let Some(mcp_cfg) = mcp else {
        let _ = tx
            .send(AgentEvent::Error(
                "MCP config missing for sampling".to_string(),
            ))
            .await;
        return None;
    };
    match call_mcp_sampling_routed(
        mcp_cfg,
        target_label,
        msg_json.to_vec(),
        system_prompt.as_deref(),
    )
    .await
    {
        Ok(c) => Some((c, false)),
        Err(e) => {
            let _ = tx.send(AgentEvent::Error(e)).await;
            None
        }
    }
}

/// Stream one chat-completion request over HTTP, forwarding each delta onto
/// `tx` as it arrives. Dropping the internal forwarder channel (when the call
/// returns) ends the forwarding loop.
async fn stream_completion_via_http(
    client: &LlmClient,
    msg_json: &[serde_json::Value],
    tool_defs: &[serde_json::Value],
    tx: &Sender<AgentEvent>,
) -> Result<ahma_llm_monitor::client::ChatCompletionResponse, ahma_llm_monitor::LlmMonitorError> {
    let (dtx, mut drx) = tokio::sync::mpsc::channel::<ahma_llm_monitor::client::StreamDelta>(64);
    let tx_fwd = tx.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(d) = drx.recv().await {
            let evt = match d {
                ahma_llm_monitor::client::StreamDelta::Content(s) => AgentEvent::Token(s),
                ahma_llm_monitor::client::StreamDelta::Thinking(s) => AgentEvent::Thinking(s),
            };
            if tx_fwd.send(evt).await.is_err() {
                break;
            }
        }
    });
    let result = client
        .chat_completion_with_tools_streaming(msg_json.to_vec(), tool_defs, dtx)
        .await;
    let _ = forwarder.await;
    result
}

/// Handle a failed HTTP chat-completion attempt: retry the turn without tools
/// when the provider rejected the request because of tool definitions,
/// otherwise report the error as non-recoverable and end the turn.
async fn handle_completion_stream_error(
    e: ahma_llm_monitor::LlmMonitorError,
    client: &LlmClient,
    messages: &[ChatMessage],
    system_prompt: &Option<String>,
    mcp: &Option<McpChatConfig>,
    tx: &Sender<AgentEvent>,
) {
    if e.is_tools_rejected() {
        info!(error = %e, "agent: model rejected tools — falling back to plain chat (no tool use this turn)");
        let _ = tx
            .send(AgentEvent::Error(
                "Model does not support tools. Falling back to standard chat.".to_string(),
            ))
            .await;
        spawn_chat_task(
            client.clone(),
            messages.to_vec(),
            system_prompt.clone(),
            mcp.clone(),
            tx.clone(),
        );
    } else {
        warn!(error = %e, "agent: chat completion failed (non-recoverable) — ending turn");
        let _ = tx.send(AgentEvent::Error(e.to_string())).await;
    }
}

/// Fetch one assistant turn. Returns `(response, content_streamed)` where
/// `content_streamed` is `true` when the visible content was already emitted to
/// `tx` as `AgentEvent::Token`s during the call (so callers must not re-send it).
async fn fetch_completion(
    client: &LlmClient,
    msg_json: &[serde_json::Value],
    tool_defs: &[serde_json::Value],
    mcp: &Option<McpChatConfig>,
    tx: &Sender<AgentEvent>,
    messages: &[ChatMessage],
    system_prompt: &Option<String>,
) -> Option<(ahma_llm_monitor::client::ChatCompletionResponse, bool)> {
    if client.base_url().starts_with("mcp://") {
        let target_label = client.base_url().strip_prefix("mcp://").unwrap_or("");
        return fetch_completion_via_mcp_sampling(mcp, target_label, msg_json, tx, system_prompt)
            .await;
    }

    // Stream the turn so the UI shows live tokens + reasoning instead of
    // blocking on one opaque request.
    match stream_completion_via_http(client, msg_json, tool_defs, tx).await {
        Ok(c) => Some((c, true)),
        Err(e) => {
            handle_completion_stream_error(e, client, messages, system_prompt, mcp, tx).await;
            None
        }
    }
}

/// The read-only tools whose results carry the "read before you edit" hint.
fn is_read_hint_tool(tool_name: &str) -> bool {
    tool_name == "read_file" || tool_name == "list_dir"
}

/// Decide which harness hints this batch of tool results is eligible for, as
/// `(inject_error_hint, inject_read_hint)`. Each hint is offered at most once
/// per run, so an already-sent hint (`*_hinted`) disqualifies its whole batch.
fn plan_harness_hints(
    tool_results: &[(String, String, serde_json::Value, bool)],
    error_hinted: bool,
    read_file_hinted: bool,
) -> (bool, bool) {
    let any_failed = tool_results.iter().any(|(_, _, _, failed)| *failed);
    let any_read_tool = tool_results
        .iter()
        .any(|(_, tool_name, _, _)| is_read_hint_tool(tool_name));
    (
        any_failed && !error_hinted,
        any_read_tool && !read_file_hinted,
    )
}

fn append_hint_to_field(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    hint: &str,
) {
    if let Some(serde_json::Value::String(s)) = obj.get_mut(key) {
        s.push_str(hint);
    }
}

/// True when a failed-tool-call hint should be appended: the call failed, the
/// caller opted into hinting this turn, and no failure hint has been sent yet.
fn should_inject_error_hint(failed: bool, inject_error_hint: bool, error_hinted: bool) -> bool {
    failed && inject_error_hint && !error_hinted
}

/// True when a read-file-tool hint should be appended: the tool is a read
/// tool, the caller opted into hinting this turn, and no read hint has been
/// sent yet.
fn should_inject_read_hint(
    tool_name: &str,
    inject_read_hint: bool,
    read_file_hinted: bool,
) -> bool {
    is_read_hint_tool(tool_name) && inject_read_hint && !read_file_hinted
}

#[allow(clippy::too_many_arguments)]
fn push_tool_message_with_hints(
    msg_json: &mut Vec<serde_json::Value>,
    tool_call_id: String,
    tool_name: &str,
    payload: serde_json::Value,
    failed: bool,
    inject_error_hint: bool,
    inject_read_hint: bool,
    error_hinted: &mut bool,
    read_file_hinted: &mut bool,
    result_char_cap: usize,
) {
    let mut final_payload = payload;
    if let Some(obj) = final_payload.as_object_mut() {
        inject_harness_hints_into_payload(
            obj,
            tool_name,
            failed,
            inject_error_hint,
            inject_read_hint,
            error_hinted,
            read_file_hinted,
        );
    }
    msg_json.push(serde_json::json!({
        "role": "tool",
        "tool_call_id": tool_call_id,
        "content": truncate_middle(&final_payload.to_string(), result_char_cap)
    }));
}

fn inject_harness_hints_into_payload(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    tool_name: &str,
    failed: bool,
    inject_error_hint: bool,
    inject_read_hint: bool,
    error_hinted: &mut bool,
    read_file_hinted: &mut bool,
) {
    if should_inject_error_hint(failed, inject_error_hint, *error_hinted) {
        *error_hinted = true;
        let key = if obj.contains_key("error") {
            "error"
        } else {
            "output"
        };
        append_hint_to_field(
            obj,
            key,
            "\n\u{1f4a1} [Harness Hint: The previous tool call failed. Carefully read the error output above. Ensure parameter values are correct, check for typo errors, and try a different approach.]",
        );
    }
    if should_inject_read_hint(tool_name, inject_read_hint, *read_file_hinted) {
        *read_file_hinted = true;
        append_hint_to_field(
            obj,
            "output",
            "\n\u{1f4a1} [Harness Hint: When modifying files that already exist, you MUST use `replace_in_file` with exact old/new string matching. Avoid using `write_file` for existing files.]",
        );
    }
}

/// Handle a possibly length-truncated completion, right after it is fetched
/// and before the caller decides how to continue the turn.
///
/// Returns `Some(true)` when the caller must immediately return `true` from
/// `execute_agent_turn` (a continuation turn was queued). Returns `None` when
/// the completion was not truncated, or was truncated mid tool-call (already
/// warned here) — either way the caller should keep processing normally.
async fn handle_length_truncated_completion(
    completion: &ahma_llm_monitor::client::ChatCompletionResponse,
    msg_json: &mut Vec<serde_json::Value>,
    tx: &Sender<AgentEvent>,
    content_streamed: bool,
) -> Option<bool> {
    if !completion.is_length_truncated() {
        return None;
    }

    if !completion.tool_calls.is_empty() {
        // Rarer: cut off while emitting a tool call, so its arguments may be
        // incomplete. Not auto-recoverable the same way (nothing to "continue"
        // that isn't already a malformed tool call) — proceed as-is, but make
        // the risk visible rather than silently trusting truncated JSON.
        warn!(
            tool_calls = completion.tool_calls.len(),
            "agent: response was cut off by a length limit while requesting tool call(s) — \
             arguments may be incomplete"
        );
        return None;
    }

    // The model was cut off by a length/context limit mid-response — the
    // classic "stuck thinking forever" failure for small local models with
    // small context windows. Never leave this silent: surface it, then
    // request a continuation on the next turn (bounded by the caller's
    // existing max_turns loop, same as any other turn).
    warn!(
        content_chars = completion.content.len(),
        "agent: response was cut off by a length/context limit — requesting a continuation"
    );
    let _ = tx
        .send(AgentEvent::Truncated {
            reason: "response was cut off by a length/context limit; continuing".to_string(),
        })
        .await;
    if !content_streamed && !completion.content.is_empty() {
        let _ = tx.send(AgentEvent::Token(completion.content.clone())).await;
    }
    msg_json.push(serde_json::json!({
        "role": "user",
        "content": "[Your previous response was cut off by a length limit. Continue \
                     exactly where you left off — do not repeat what you already said.]"
    }));
    Some(true)
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_agent_turn(
    client: &LlmClient,
    msg_json: &mut Vec<serde_json::Value>,
    tool_defs: &[serde_json::Value],
    mcp: &Option<McpChatConfig>,
    tx: &Sender<AgentEvent>,
    messages: &[ChatMessage],
    system_prompt: &Option<String>,
    read_file_hinted: &mut bool,
    error_hinted: &mut bool,
    gate: Arc<dyn AgentApprovalGate>,
    last_prompt_tokens: &mut u32,
) -> bool {
    if let Some(cfg) = mcp {
        maybe_compact_conversation(client, msg_json, cfg, *last_prompt_tokens).await;
        trim_conversation(msg_json, conversation_char_budget(cfg));
    }

    let Some((completion, content_streamed)) = fetch_completion(
        client,
        msg_json,
        tool_defs,
        mcp,
        tx,
        messages,
        system_prompt,
    )
    .await
    else {
        warn!("agent: turn aborted — no completion returned by the model");
        return false;
    };

    info!(
        conversation_messages = msg_json.len(),
        content_chars = completion.content.len(),
        tool_calls = completion.tool_calls.len(),
        "agent: model turn completed"
    );

    record_turn_usage(&completion.usage, tx, last_prompt_tokens).await;

    msg_json.push(serde_json::json!({
        "role": "assistant",
        "content": completion.content.clone(),
        "tool_calls": completion.assistant_message.get("tool_calls").cloned().unwrap_or(serde_json::Value::Null)
    }));

    if let Some(result) =
        handle_length_truncated_completion(&completion, msg_json, tx, content_streamed).await
    {
        return result;
    }

    if completion.tool_calls.is_empty() {
        return handle_final_agent_response(completion, content_streamed, tx).await;
    }

    let Some(mcp_cfg) = mcp.clone() else {
        let _ = tx
            .send(AgentEvent::Error(
                "Model requested tools but MCP is not configured".to_string(),
            ))
            .await;
        return false;
    };

    dispatch_turn_tool_calls(
        completion,
        mcp_cfg,
        tx.clone(),
        gate,
        msg_json,
        error_hinted,
        read_file_hinted,
    )
    .await;

    true
}

async fn record_turn_usage(
    usage: &Option<ahma_llm_monitor::client::TokenUsage>,
    tx: &Sender<AgentEvent>,
    last_prompt_tokens: &mut u32,
) {
    if let Some(usage) = usage {
        let _ = tx.send(AgentEvent::Usage(usage.clone())).await;
        if usage.prompt_tokens > 0 {
            *last_prompt_tokens = usage.prompt_tokens;
        }
    }
}

async fn handle_final_agent_response(
    completion: ahma_llm_monitor::client::ChatCompletionResponse,
    content_streamed: bool,
    tx: &Sender<AgentEvent>,
) -> bool {
    info!("agent: final answer received (no tool calls) — ending agentic loop");
    if !content_streamed && !completion.content.is_empty() {
        let _ = tx.send(AgentEvent::Token(completion.content)).await;
    }
    let _ = tx.send(AgentEvent::Done).await;
    false
}

async fn dispatch_turn_tool_calls(
    completion: ahma_llm_monitor::client::ChatCompletionResponse,
    mcp_cfg: McpChatConfig,
    tx: Sender<AgentEvent>,
    gate: Arc<dyn AgentApprovalGate>,
    msg_json: &mut Vec<serde_json::Value>,
    error_hinted: &mut bool,
    read_file_hinted: &mut bool,
) {
    let tool_names: Vec<&str> = completion
        .tool_calls
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    info!(
        count = tool_names.len(),
        tools = ?tool_names,
        "agent: model requested tool call(s)"
    );

    let call_futures = completion
        .tool_calls
        .into_iter()
        .map(|call| execute_single_tool_call(call, mcp_cfg.clone(), tx.clone(), gate.clone()));
    let tool_results = join_all(call_futures).await;

    let (inject_error_hint, inject_read_hint) = if mcp_cfg.small_model_harness {
        plan_harness_hints(&tool_results, *error_hinted, *read_file_hinted)
    } else {
        (false, false)
    };

    let result_char_cap = tool_result_char_cap(&mcp_cfg);
    for (tool_call_id, tool_name, payload, failed) in tool_results {
        push_tool_message_with_hints(
            msg_json,
            tool_call_id,
            &tool_name,
            payload,
            failed,
            inject_error_hint,
            inject_read_hint,
            error_hinted,
            read_file_hinted,
            result_char_cap,
        );
    }
}

/// The run's system prompt: the caller's, with the context strategy's suffix
/// appended (or promoted to the whole prompt when the caller supplied none).
fn system_prompt_for_run(
    system_prompt: Option<String>,
    mcp: &Option<McpChatConfig>,
) -> Option<String> {
    let suffix = mcp
        .as_ref()
        .and_then(|cfg| cfg.context_strategy().system_prompt_suffix());
    let Some(suffix) = suffix else {
        return system_prompt;
    };
    match system_prompt {
        Some(mut s) => {
            s.push_str(suffix);
            Some(s)
        }
        None => Some(suffix.trim().to_string()),
    }
}

/// Seed the wire-format message list: the system prompt (when present) followed
/// by the conversation so far.
fn initial_msg_json(
    sys_prompt: &Option<String>,
    messages: &[ChatMessage],
) -> Vec<serde_json::Value> {
    let mut msg_json: Vec<serde_json::Value> = Vec::new();
    if let Some(system) = sys_prompt {
        msg_json.push(serde_json::json!({"role": "system", "content": system}));
    }
    msg_json.extend(
        messages
            .iter()
            .map(|msg| serde_json::json!({"role": msg.role, "content": msg.content})),
    );
    msg_json
}

pub fn spawn_agent_task(
    client: LlmClient,
    messages: Vec<ChatMessage>,
    system_prompt: Option<String>,
    mcp: Option<McpChatConfig>,
    available_tools: Vec<ahma_mcp::mcp_client::ToolInfo>,
    tx: Sender<AgentEvent>,
    gate: Arc<dyn AgentApprovalGate>,
) {
    tokio::spawn(async move {
        let sys_prompt = system_prompt_for_run(system_prompt, &mcp);
        let mut msg_json = initial_msg_json(&sys_prompt, &messages);

        let tool_defs = prepare_tool_definitions(available_tools);

        let max_turns = mcp.as_ref().map(|c| c.max_turns).unwrap_or(8);
        let mut completed = false;
        let mut read_file_hinted = false;
        let mut error_hinted = false;
        let mut last_prompt_tokens: u32 = 0;

        info!(
            max_turns,
            initial_messages = msg_json.len(),
            tool_defs = tool_defs.len(),
            "agent: starting agentic loop"
        );

        for turn in 0..max_turns {
            info!(turn = turn + 1, max_turns, "agent: turn start");
            if !execute_agent_turn(
                &client,
                &mut msg_json,
                &tool_defs,
                &mcp,
                &tx,
                &messages,
                &sys_prompt,
                &mut read_file_hinted,
                &mut error_hinted,
                gate.clone(),
                &mut last_prompt_tokens,
            )
            .await
            {
                completed = true;
                break;
            }
        }

        if completed {
            return;
        }

        // Turn budget exhausted without the model ending its own run.
        finish_with_limit_summary(
            &client,
            &mut msg_json,
            &mcp,
            &tx,
            &messages,
            &sys_prompt,
            max_turns,
        )
        .await;
    });
}

/// Close out an agent run that exhausted its turn budget. Instead of dead-ending
/// on an opaque error, give the model one final turn with **no tools** so it
/// summarises what it accomplished, what is left, and the next step. The empty
/// tool list guarantees this turn ends the loop (the model cannot request
/// another tool), and the user gets a useful closing message instead of a raw
/// "reached max turns" error.
#[allow(clippy::too_many_arguments)]
async fn finish_with_limit_summary(
    client: &LlmClient,
    msg_json: &mut Vec<serde_json::Value>,
    mcp: &Option<McpChatConfig>,
    tx: &Sender<AgentEvent>,
    messages: &[ChatMessage],
    system_prompt: &Option<String>,
    max_turns: u32,
) {
    warn!(
        max_turns,
        "agent: loop reached max turns — requesting a tool-free summary of progress"
    );
    msg_json.push(serde_json::json!({
        "role": "user",
        "content": format!(
            "You have reached the maximum of {max_turns} tool-call turns for this task. \
             Do not call any more tools. In a few sentences, summarise what you accomplished, \
             what remains unfinished, and the single recommended next step."
        )
    }));
    if let Some(cfg) = mcp {
        trim_conversation(msg_json, conversation_char_budget(cfg));
    }

    match fetch_completion(client, msg_json, &[], mcp, tx, messages, system_prompt).await {
        Some((completion, content_streamed)) => {
            if let Some(usage) = &completion.usage {
                let _ = tx.send(AgentEvent::Usage(usage.clone())).await;
            }
            // Streamed paths already emitted the content token-by-token.
            if !content_streamed && !completion.content.is_empty() {
                let _ = tx.send(AgentEvent::Token(completion.content)).await;
            }
            let _ = tx.send(AgentEvent::Done).await;
        }
        None => {
            // fetch_completion already surfaced the concrete error to the UI.
            let _ = tx
                .send(AgentEvent::Error(format!(
                    "Agent stopped after {max_turns} tool-call turns without completing the task, \
                     and the closing summary could not be generated."
                )))
                .await;
        }
    }
}

/// Process-wide cache of `reqwest::Client`s keyed by bridge base URL.
/// Building a fresh client per tool call re-does TLS and connection-pool
/// setup every time and forfeits HTTP keep-alive reuse entirely; sharing one
/// client per base URL keeps connections warm across the agent loop's many
/// sequential tool calls. `reqwest::Client` clones are cheap `Arc` handles.
static HTTP_CLIENT_CACHE: std::sync::LazyLock<
    parking_lot::Mutex<HashMap<String, reqwest::Client>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Return the effective request base URL and a cached `reqwest::Client` for an
/// MCP bridge `base_url`. `unix://<path>` URLs (Unix only) yield a client
/// bound to that socket with `http://localhost` as the request base; all other
/// URLs get a plain client and are used as-is.
pub fn cached_http_client(base_url: &str) -> Result<(String, reqwest::Client), String> {
    let unix_path = if cfg!(unix) {
        base_url.strip_prefix("unix://")
    } else {
        None
    };
    let request_base = if unix_path.is_some() {
        "http://localhost".to_string()
    } else {
        base_url.to_string()
    };

    let mut cache = HTTP_CLIENT_CACHE.lock();
    if let Some(client) = cache.get(base_url) {
        return Ok((request_base, client.clone()));
    }

    let builder = reqwest::Client::builder();
    #[cfg(unix)]
    let builder = match unix_path {
        Some(path) => builder.unix_socket(path),
        None => builder,
    };
    let client = builder.build().map_err(|e| e.to_string())?;
    cache.insert(base_url.to_string(), client.clone());
    Ok((request_base, client))
}

async fn spawn_local_tool_call(
    mcp: McpChatConfig,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<(String, bool), String> {
    let (request_base_url, client) = cached_http_client(&mcp.base_url)?;
    let url = format!("{}/mcp", request_base_url);
    let session_id = get_or_create_session(&client, &url, &mcp).await?;
    call_mcp_tool_http(&client, &url, &session_id, tool, arguments).await
}

async fn spawn_external_tool_call_http(
    base_url: &str,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<(String, bool), String> {
    let (request_base, client) = cached_http_client(base_url.trim_end_matches('/'))?;
    let url = format!("{}/mcp", request_base);
    let sid = get_or_create_external_session(&client, &url).await?;
    call_mcp_tool_http(&client, &url, &sid, tool, arguments).await
}

/// Initialize a session against an *external* Streamable-HTTP MCP server via
/// the shared client's minimal handshake (initialize +
/// `notifications/initialized`). Intentionally no SSE stream and no roots
/// answering: external servers are not assumed to implement ahma's
/// roots/sandbox gate, and this path never performed that handshake —
/// preserved as-is from the pre-unification implementation.
async fn get_or_create_external_session(
    client: &reqwest::Client,
    url: &str,
) -> Result<String, String> {
    let connector = Connector {
        mcp_url: url.to_string(),
        post_client: client.clone(),
        sse_client: client.clone(),
    };
    // clientInfo.name is load-bearing (the server keys behaviour off it) —
    // this consumer keeps its own identity.
    let session = StreamableHttpMcpClient::connect_minimal(
        connector,
        "ahma-core-external-tool",
        env!("CARGO_PKG_VERSION"),
    )
    .await
    .map_err(|e| format!("Failed to initialize external session: {e:#}"))?;
    Ok(session.session_id().to_string())
}

/// Process-wide cache of MCP sessions this process has already negotiated
/// with a given bridge, keyed by (bridge URL, workspace root). Each
/// `dispatch_tool_execution` call used to receive its own fresh clone of
/// `McpChatConfig` with `session_id: None` — the agentic loop's `for turn in
/// 0..max_turns` (`spawn_agent_task`) and every `join_all`'d tool call within
/// a turn never wrote a negotiated id back into the shared config, so every
/// single tool call re-ran the full initialize/SSE/roots handshake and spun
/// up a brand-new bridge subprocess. That churn exhausted the bridge's
/// `max_sessions` cap within a normal chat turn. This cache lets any caller
/// in the process reuse the same session for the same (url, workspace)
/// instead (SPEC ahma_tui R25).
static SESSION_CACHE: std::sync::LazyLock<parking_lot::Mutex<HashMap<(String, PathBuf), String>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Drop a cached session for (url, workspace_root) so the next
/// `get_or_create_session` call negotiates a fresh one. Call this when a
/// cached session is discovered to be dead (e.g. an HTTP 403 on `tools/call`)
/// — otherwise the cache would keep handing out the same rejected id.
pub fn invalidate_cached_session(url: &str, workspace_root: &std::path::Path) {
    SESSION_CACHE
        .lock()
        .remove(&(url.to_string(), workspace_root.to_path_buf()));
}

/// Initialize (or reuse) an MCP session against the local bridge for a tool
/// call. Shared by the core agent loop and the TUI's manual tool-call path.
///
/// When `mcp.session_id` is already set (the TUI's `mcp_source` has negotiated a
/// sandbox-locked session) it is reused verbatim. Otherwise this reuses this
/// process's cached session for the same (url, workspace_root) if one exists
/// (see `SESSION_CACHE`), and only then performs the **complete** MCP
/// Streamable-HTTP handshake — `initialize`, open the GET `/mcp` SSE stream,
/// send `notifications/initialized`, answer the bridge's `roots/list`, and
/// wait for `notifications/sandbox/configured` — so the new session reaches
/// `Active` and its `tools/call`s are not rejected with HTTP 409.
/// Skipping the SSE/`roots/list` steps (the previous behaviour) left the session
/// stuck in `AwaitingRoots`, which is why `ahma tui` chat tool calls returned
/// "the sandbox is still initializing" indefinitely on startup or after a reset.
pub async fn get_or_create_session(
    client: &reqwest::Client,
    url: &str,
    mcp: &McpChatConfig,
) -> Result<String, String> {
    if let Some(sid) = &mcp.session_id
        && !sid.is_empty()
    {
        return Ok(sid.clone());
    }

    let cache_key = (url.to_string(), mcp.workspace_root.clone());
    if let Some(sid) = SESSION_CACHE.lock().get(&cache_key).cloned() {
        return Ok(sid);
    }

    // Full MCP Streamable-HTTP handshake via the shared client: initialize,
    // open the GET /mcp SSE stream BEFORE notifications/initialized, answer
    // the bridge's roots/list with the workspace scope, then wait (non-fatal)
    // for notifications/sandbox/configured so the first tools/call doesn't
    // race the lock. The SSE listener stays alive for the session lifetime.
    let connector = Connector {
        mcp_url: url.to_string(),
        post_client: client.clone(),
        sse_client: client.clone(),
    };
    let opts = ConnectOptions {
        // clientInfo.name is load-bearing: the bridge keys `supports_progress`
        // and `request_budget` off it.
        client_name: "ahma-core-tool".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        roots: vec![mcp.workspace_root.clone()],
        notifications: None,
        // Pre-unification values, preserved: 5 s for the SSE return stream to
        // open (fatal), 10 s for the sandbox-lock confirmation (non-fatal —
        // tools/call retries through the 409 gate).
        sse_open_timeout: std::time::Duration::from_secs(5),
        sandbox_lock_timeout: Some(std::time::Duration::from_secs(10)),
    };
    let session = StreamableHttpMcpClient::connect(connector, opts)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let sid = session.session_id().to_string();

    SESSION_CACHE.lock().insert(cache_key, sid.clone());

    Ok(sid)
}

/// Parse an MCP `tools/call` JSON-RPC response into `(text, is_error)`.
/// Shared by the core agent loop and the TUI's manual tool-call path.
pub fn parse_mcp_response(json_resp: &serde_json::Value) -> (String, bool) {
    let result_val = json_resp.get("result");
    let is_error = json_resp.get("error").is_some()
        || (result_val
            .and_then(|r| r.get("isError"))
            .and_then(|e| e.as_bool())
            .unwrap_or(false));

    let content_str = if let Some(err) = json_resp.get("error") {
        parse_error_message(err)
    } else if let Some(res) = result_val {
        extract_content_text(res)
    } else {
        "Empty result".to_string()
    };

    (content_str, is_error)
}

fn parse_error_message(err: &serde_json::Value) -> String {
    format!(
        "Error: {}",
        err.get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error")
    )
}

fn extract_content_text(res: &serde_json::Value) -> String {
    if let Some(content_array) = res.get("content").and_then(|c| c.as_array()) {
        let mut texts = Vec::new();
        for item in content_array {
            if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                texts.push(t.to_string());
            }
        }
        texts.join("\n")
    } else {
        serde_json::to_string_pretty(res).unwrap_or_default()
    }
}

/// POST a `tools/call` to the local MCP bridge and parse the response.
/// Shared by the core agent loop and the TUI's manual tool-call path.
pub async fn call_mcp_tool_http(
    client: &reqwest::Client,
    url: &str,
    session_id: &str,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<(String, bool), String> {
    // This call site only carries a bare cached session id (see
    // `SESSION_CACHE`), not the version that session's own `initialize`
    // negotiated, so it echoes the spec's own default. The bridge validates
    // the header against a fixed supported-set, not per-session negotiation
    // history, so this is a safe, spec-compliant value — never the exact
    // negotiated revision when it differs from the default.
    let session = StreamableHttpMcpClient::attach(
        client.clone(),
        url,
        session_id,
        ahma_common::mcp_protocol::DEFAULT_NEGOTIATED_PROTOCOL_VERSION,
    );

    // A 409 means the session's sandbox lock has not finalised yet (the
    // roots/list → lock round-trip completes a few ms after the handshake). The
    // bridge's own contract is "retry tools/call after handshake completes", so
    // retry briefly rather than surfacing a misleading "sandbox initializing"
    // error to the model on the very first call. Pre-unification values,
    // preserved: 5 retries × 300 ms.
    let retry = ConflictRetryPolicy {
        max_retries: 5,
        delay: std::time::Duration::from_millis(300),
    };
    match session
        .call_tool(tool, arguments, retry)
        .await
        .map_err(|e| format!("{e:#}"))?
    {
        ToolCallOutcome::Success(json_resp) => Ok(parse_mcp_response(&json_resp)),
        ToolCallOutcome::SandboxInitializing { body } => Err(format!("HTTP 409 Conflict: {body}")),
        ToolCallOutcome::HttpError { status, body } => Err(format!("HTTP {status}: {body}")),
    }
}

pub fn needs_approval(tool_name: &str, tool_approval_enabled: bool) -> bool {
    if tool_approval_enabled {
        return true;
    }
    tool_name == "write_file"
        || tool_name == "replace_in_file"
        || tool_name.ends_with("::write_file")
        || tool_name.ends_with("::replace_in_file")
}

pub fn get_mcp_base_url(app_config: Option<&ahma_mcp::shell::cli::AppConfig>) -> String {
    let Some(config) = app_config else {
        return "http://127.0.0.1:3000".to_string();
    };
    if !config.unix_socket_path.is_empty() {
        return format!("unix://{}", config.unix_socket_path);
    }
    let host = if config.http_host.is_empty() {
        "127.0.0.1"
    } else {
        &config.http_host
    };
    let port = if config.http_port == 0 {
        3000
    } else {
        config.http_port
    };
    format!("http://{}:{}", host, port)
}

/// Whether a `provider` string is a direct base URL rather than a configured
/// provider name. Auto-discovered local providers are addressed by URL and have
/// no `[[providers]]` entry to resolve, so callers may pass the URL directly.
fn provider_is_url(provider: &str) -> bool {
    let p = provider.trim();
    p.starts_with("http://") || p.starts_with("https://") || p.starts_with("unix://")
}

/// Resolve the `(base_url, model_name, api_key)` triple used to construct the
/// LLM client.
///
/// The TUI overloads `provider` with either a configured provider *name* (from
/// `~/.ahma/config.toml`) or a direct *base URL* — auto-discovered local
/// providers (Ollama, LM Studio, llama-server) have no config entry and are
/// addressed purely by URL. URL-shaped values are treated as a direct base URL
/// so local models remain usable without a named config entry. When no provider
/// is supplied, the first configured provider is used.
/// Everything needed to build an [`LlmClient`], resolved from a provider name,
/// a bare URL, or the configured default provider.
///
/// This deliberately carries `kind` alongside `num_ctx`. Both are declared in
/// `~/.ahma/config.toml` and both were previously dropped on the way to the
/// client — `num_ctx` made proactive compaction inert, and `kind` let the
/// URL heuristic override an explicit `kind = "anthropic"` (issue #484).
struct LlmConnection {
    base_url: String,
    model: String,
    api_key: Option<String>,
    num_ctx: Option<u32>,
    /// Explicit wire format from config. `None` means no entry claimed this
    /// URL, so [`LlmClient::new`]'s host heuristic should decide.
    kind: Option<ahma_common::config::ProviderKind>,
}

impl LlmConnection {
    fn from_resolved(
        resolved: ahma_common::config::ResolvedProvider,
        model_override: Option<String>,
    ) -> Self {
        Self {
            base_url: resolved.base_url,
            model: model_override.unwrap_or(resolved.default_model),
            api_key: resolved.api_key,
            num_ctx: resolved.num_ctx,
            kind: Some(resolved.kind),
        }
    }

    /// Build the client, honoring an explicit `kind` when config declared one
    /// and otherwise leaving `LlmClient::new`'s host heuristic in charge.
    fn into_client(self) -> LlmClient {
        let client = LlmClient::new(self.base_url, self.model, self.api_key);
        let client = match self.kind {
            Some(ahma_common::config::ProviderKind::Anthropic) => {
                client.with_flavor(ahma_llm_monitor::ApiFlavor::Anthropic)
            }
            Some(ahma_common::config::ProviderKind::OpenAi) => {
                client.with_flavor(ahma_llm_monitor::ApiFlavor::OpenAi)
            }
            None => client,
        };
        client.with_num_ctx(self.num_ctx)
    }
}

fn resolve_llm_connection(
    provider: Option<String>,
    model: Option<String>,
) -> Result<LlmConnection, String> {
    if let Some(p_name) = provider {
        if provider_is_url(&p_name) {
            // The TUI persists the selected provider by URL, so everything the
            // matching config entry declares must still be recovered here
            // (issue #484 — a lost `num_ctx` silently disables compaction, and
            // a lost `kind` sends the wrong wire format to a proxied provider).
            let config = ahma_common::config::AhmaConfig::load();
            let entry = config.provider_for_base_url(&p_name);
            return Ok(LlmConnection {
                base_url: p_name,
                model: model.unwrap_or_default(),
                api_key: None,
                num_ctx: entry.and_then(|e| e.num_ctx),
                kind: entry.map(|e| e.kind),
            });
        }
        let config = ahma_common::config::AhmaConfig::load();
        let resolved = config
            .resolve_provider(&p_name)
            .map_err(|e| format!("Failed to resolve provider '{}': {e}", p_name))?;
        return Ok(LlmConnection::from_resolved(resolved, model));
    }

    let config = ahma_common::config::AhmaConfig::load();
    let Some(first_provider) = config.providers.first() else {
        return Err("No LLM providers configured in ~/.ahma/config.toml".to_string());
    };
    let resolved = first_provider.resolve().map_err(|e| {
        format!(
            "Failed to resolve default provider '{}': {e}",
            first_provider.name
        )
    })?;
    Ok(LlmConnection::from_resolved(resolved, model))
}

/// A PromptRunner implementation that executes the agent loop inside ahma_core.
pub struct CorePromptRunner;

#[async_trait]
impl ahma_mcp::PromptRunner for CorePromptRunner {
    async fn run_prompt(
        &self,
        messages: Vec<DaemonChatMessage>,
        system_prompt: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        hub_tx: tokio::sync::mpsc::Sender<ClientMsg>,
        session: Arc<tokio::sync::Mutex<ActiveAgentSession>>,
    ) -> Result<(), String> {
        // Build the client/tools/config (tool_approval on → prompts the TUI),
        // and convert the inbound messages.
        let (client, mcp_config, available_tools) =
            build_agent_run_context(provider, model, None).await?;
        let chat_messages = daemon_messages_to_chat(messages);

        // Spawn the agent task with the hub approval gate and an event channel.
        let (tx, mut rx) = tokio::sync::mpsc::channel(100);
        let gate = Arc::new(HubApprovalGate {
            hub_tx: hub_tx.clone(),
            session,
            approval_mutex: tokio::sync::Mutex::new(()),
        });

        spawn_agent_task(
            client,
            chat_messages,
            system_prompt,
            Some(mcp_config),
            available_tools,
            tx,
            gate,
        );

        // Receive events from the agent loop and forward them to the hub daemon
        while let Some(evt) = rx.recv().await {
            let client_msg = match evt {
                AgentEvent::Token(t) => ClientMsg::ChatToken { token: t },
                AgentEvent::Thinking(t) => ClientMsg::ChatThinking { token: t },
                AgentEvent::Done => ClientMsg::AgentDone,
                AgentEvent::Error(e) => ClientMsg::AgentError { error: e },
                // Tool-call lifecycle events are NOT approval requests — the
                // approval prompt is raised separately by the HubApprovalGate.
                // These drive the TUI's live "which tool is running" display and
                // the token counter, so forward them over their own hub messages.
                AgentEvent::ToolCallStarted { id, name, args } => {
                    ClientMsg::ToolCallStarted { id, name, args }
                }
                AgentEvent::ToolCallFinished { id, result, failed } => {
                    ClientMsg::ToolCallFinished { id, result, failed }
                }
                AgentEvent::Usage(usage) => ClientMsg::Usage {
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    total_tokens: usage.total_tokens,
                },
                // No dedicated hub wire type for this yet — surfaced as a plain
                // visible chat token so it is never silent, same as the direct
                // (non-hub) llm_bridge path's dedicated note.
                AgentEvent::Truncated { reason } => ClientMsg::ChatToken {
                    token: format!("[{reason}]"),
                },
            };

            if hub_tx.send(client_msg).await.is_err() {
                break;
            }
        }

        Ok(())
    }

    async fn run_prompt_to_completion(
        &self,
        messages: Vec<DaemonChatMessage>,
        system_prompt: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        max_turns: Option<u32>,
    ) -> Result<String, String> {
        // Default the provider/model to the model the user last selected in
        // `ahma tui` (persisted in settings.agent). Prefer the resolved base URL
        // so we don't depend on the TUI's provider label matching a config name.
        let settings = ahma_common::config::AhmaSettings::load_async().await;
        let provider = provider.or_else(|| {
            settings
                .agent
                .provider_url
                .clone()
                .or_else(|| settings.agent.provider.clone())
        });
        let model = model.or_else(|| settings.agent.model.clone());

        let (client, mut mcp_config, available_tools) =
            build_agent_run_context(provider, model, max_turns).await?;
        // A delegated sub-agent has no interactive surface, so do not gate tool
        // calls on human approval — auto-approve them.
        mcp_config.tool_approval = false;

        let chat_messages = daemon_messages_to_chat(messages);

        let (tx, mut rx) = tokio::sync::mpsc::channel(100);
        spawn_agent_task(
            client,
            chat_messages,
            system_prompt,
            Some(mcp_config),
            available_tools,
            tx,
            Arc::new(AutoApproveAgentGate),
        );

        // Collect the assistant text; surface the loop's error verbatim.
        let mut out = String::new();
        while let Some(evt) = rx.recv().await {
            match evt {
                AgentEvent::Token(t) => out.push_str(&t),
                AgentEvent::Error(e) => return Err(e),
                AgentEvent::Done => break,
                _ => {}
            }
        }
        Ok(out)
    }
}

/// Build the per-run agent context shared by `CorePromptRunner::run_prompt`
/// and `CorePromptRunner::run_prompt_to_completion`: resolve the LLM client,
/// gather the active service's tools, and assemble the [`McpChatConfig`].
/// `max_turns_override` replaces the configured default when `Some`.
async fn build_agent_run_context(
    provider: Option<String>,
    model: Option<String>,
    max_turns_override: Option<u32>,
) -> Result<
    (
        LlmClient,
        McpChatConfig,
        Vec<ahma_mcp::mcp_client::ToolInfo>,
    ),
    String,
> {
    let service = ahma_mcp::get_active_service()
        .ok_or_else(|| "No active AhmaMcpService found in this process".to_string())?;

    let conn = resolve_llm_connection(provider, model)?;
    let num_ctx = conn.num_ctx;
    let client = conn.into_client();

    // These three are independent — a tool-list assembly, a settings file read and
    // a lock acquisition — and ran back to back, so every turn paid their latency
    // in series on the critical path before the model could be called.
    let (available_tools, settings, mcp_connections) = tokio::join!(
        service.get_all_available_tools(),
        ahma_common::config::AhmaSettings::load_async(),
        async { service.mcp_connections.read().await.clone() },
    );

    let workspace_root = service
        .adapter
        .sandbox()
        .scopes()
        .first()
        .cloned()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let local_mcp_base_url = {
        let app_config_guard = service.app_config.read();
        let app_config_ref = app_config_guard.as_ref().map(|arc| arc.as_ref());
        get_mcp_base_url(app_config_ref)
    };

    let mcp_config = assemble_hub_chat_config(
        local_mcp_base_url,
        workspace_root,
        mcp_connections,
        &settings,
        max_turns_override,
        num_ctx,
    );

    Ok((client, mcp_config, available_tools))
}

/// Assemble the hub-path [`McpChatConfig`] from already-resolved inputs.
///
/// `num_ctx` is the provider's declared context window from
/// `~/.ahma/config.toml`; it becomes [`McpChatConfig::context_length`] so
/// proactive compaction and the context budgets have a denominator on the
/// daemon-hub `SubmitPrompt` path (issue #484 — a `None` here made both
/// features silently inert for every hub-routed chat).
fn assemble_hub_chat_config(
    local_mcp_base_url: String,
    workspace_root: PathBuf,
    mcp_connections: ahma_mcp::mcp_client::McpConnectionManager,
    settings: &ahma_common::config::AhmaSettings,
    max_turns_override: Option<u32>,
    num_ctx: Option<u32>,
) -> McpChatConfig {
    McpChatConfig {
        base_url: local_mcp_base_url,
        workspace_root,
        session_id: None,
        external_http_servers: BTreeMap::new(),
        max_turns: max_turns_override.unwrap_or(settings.tools.max_turns),
        tool_approval: true,
        mcp_connections,
        minimize_tokens: settings.tools.minimize_tokens,
        small_model_harness: settings.tools.small_model_harness,
        context_length: num_ctx,
    }
}

/// Convert hub `DaemonChatMessage`s into agent `ChatMessage`s.
fn daemon_messages_to_chat(messages: Vec<DaemonChatMessage>) -> Vec<ChatMessage> {
    messages
        .into_iter()
        .map(|msg| {
            let role = match msg.role.to_lowercase().as_str() {
                "system" => ahma_llm_monitor::ChatRole::System,
                "assistant" => ahma_llm_monitor::ChatRole::Assistant,
                "tool" => ahma_llm_monitor::ChatRole::Tool,
                _ => ahma_llm_monitor::ChatRole::User,
            };
            ChatMessage {
                role,
                content: msg.content,
                tool_call_id: None,
            }
        })
        .collect()
}

/// Approval gate that approves every tool call — used by the headless MCP
/// `agent` sub-agent, which has no interactive surface to ask a human.
struct AutoApproveAgentGate;

#[async_trait]
impl AgentApprovalGate for AutoApproveAgentGate {
    async fn request_approval(&self, _id: &str, _tool: &str, _args: &str) -> bool {
        true
    }
}

struct HubApprovalGate {
    hub_tx: tokio::sync::mpsc::Sender<ClientMsg>,
    session: Arc<tokio::sync::Mutex<ActiveAgentSession>>,
    approval_mutex: tokio::sync::Mutex<()>,
}

#[async_trait]
impl AgentApprovalGate for HubApprovalGate {
    async fn request_approval(&self, id: &str, tool: &str, args: &str) -> bool {
        // Serialize approvals so concurrent tool calls prompt sequentially
        let _guard = self.approval_mutex.lock().await;

        let (tx, rx) = tokio::sync::oneshot::channel();
        let (legacy_tx, legacy_rx) = tokio::sync::oneshot::channel();
        {
            let mut session_guard = self.session.lock().await;
            session_guard.approval_tx = Some(legacy_tx);
            session_guard.approvals.insert(id.to_string(), tx);
        }

        let msg = ClientMsg::ApprovalRequested {
            id: id.to_string(),
            tool: tool.to_string(),
            args: args.to_string(),
        };
        if self.hub_tx.send(msg).await.is_err() {
            let mut session_guard = self.session.lock().await;
            session_guard.approvals.remove(id);
            session_guard.approval_tx = None;
            return false;
        }

        let approved = tokio::select! {
            res = rx => res.unwrap_or(false),
            res = legacy_rx => res.unwrap_or(false),
        };

        let mut session_guard = self.session.lock().await;
        session_guard.approvals.remove(id);
        session_guard.approval_tx = None;

        approved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    struct AutoApproveGate;
    #[async_trait::async_trait]
    impl AgentApprovalGate for AutoApproveGate {
        async fn request_approval(&self, _id: &str, _tool: &str, _args: &str) -> bool {
            true
        }
    }

    fn cfg_with(context_length: Option<u32>, small_model_harness: bool) -> McpChatConfig {
        McpChatConfig {
            base_url: "http://localhost:3000".to_string(),
            workspace_root: PathBuf::from("/tmp"),
            session_id: None,
            external_http_servers: BTreeMap::new(),
            max_turns: 8,
            tool_approval: false,
            mcp_connections: ahma_mcp::mcp_client::McpConnectionManager::default(),
            minimize_tokens: false,
            small_model_harness,
            context_length,
        }
    }

    /// Regression for issue #484: the hub `SubmitPrompt` path hardcoded
    /// `context_length: None`, silently disabling proactive compaction and the
    /// context budgets. The provider's resolved `num_ctx` must flow through to
    /// `McpChatConfig::context_length` whenever it is configured.
    #[test]
    fn hub_chat_config_carries_provider_num_ctx_as_context_length() {
        let settings = ahma_common::config::AhmaSettings::default();
        let cfg = assemble_hub_chat_config(
            "http://localhost:3000".to_string(),
            PathBuf::from("/tmp"),
            ahma_mcp::mcp_client::McpConnectionManager::default(),
            &settings,
            None,
            Some(16_384),
        );
        assert_eq!(cfg.context_length, Some(16_384));
        // And the compaction trigger actually engages with that denominator.
        assert!(
            cfg.context_strategy().compaction_threshold().is_some(),
            "a known context window must enable a compaction threshold"
        );

        // Unknown window stays None (compaction correctly stays inert).
        let cfg = assemble_hub_chat_config(
            "http://localhost:3000".to_string(),
            PathBuf::from("/tmp"),
            ahma_mcp::mcp_client::McpConnectionManager::default(),
            &settings,
            None,
            None,
        );
        assert_eq!(cfg.context_length, None);
    }

    #[test]
    fn provider_is_url_recognizes_direct_endpoints() {
        // Auto-discovered local providers are addressed by URL.
        assert!(provider_is_url("http://localhost:8000/v1"));
        assert!(provider_is_url("http://127.0.0.1:11434/v1"));
        assert!(provider_is_url("https://api.example.com/v1"));
        assert!(provider_is_url("unix:///tmp/ahma.sock"));
        assert!(provider_is_url("  http://localhost:8080/v1  "));
    }

    #[test]
    fn provider_is_url_rejects_config_names() {
        // Named providers resolve through ~/.ahma/config.toml.
        assert!(!provider_is_url("ollama-local"));
        assert!(!provider_is_url("anthropic"));
        assert!(!provider_is_url(""));
        assert!(!provider_is_url("my-http-provider"));
    }

    #[test]
    fn budgets_scale_with_context_length() {
        let cfg = cfg_with(Some(8192), false);
        assert_eq!(tool_result_char_cap(&cfg), 8192);
        assert_eq!(conversation_char_budget(&cfg), 24576);
    }

    #[test]
    fn small_model_harness_tightens_default_budgets() {
        let small = cfg_with(None, true);
        let normal = cfg_with(None, false);
        assert!(tool_result_char_cap(&small) < tool_result_char_cap(&normal));
        assert!(conversation_char_budget(&small) < conversation_char_budget(&normal));
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail() {
        let s = format!("{}MIDDLE{}", "a".repeat(5000), "z".repeat(5000));
        let out = truncate_middle(&s, 1000);
        assert!(out.starts_with("aaa"), "head preserved");
        assert!(out.ends_with("zzz"), "tail preserved");
        assert!(out.contains("characters elided"), "marker present");
        assert!(out.chars().count() < 1200, "got {}", out.chars().count());
        assert_eq!(truncate_middle("short", 1000), "short");
    }

    #[test]
    fn trim_conversation_preserves_system_and_recent() {
        let mut msgs = vec![serde_json::json!({"role": "system", "content": "SYS"})];
        for i in 0..20 {
            msgs.push(serde_json::json!({"role": "user", "content": format!("msg-{i}-{}", "x".repeat(500))}));
        }
        trim_conversation(&mut msgs, 2_000);

        assert_eq!(msgs[0]["role"], "system");
        // The first user message (the goal) is pinned right after the system
        // prompt, so the model never loses the original objective.
        assert!(
            msgs[1]["content"].as_str().unwrap().starts_with("msg-0"),
            "first user message (goal) preserved, got {:?}",
            msgs[1]["content"]
        );
        // The elision notice follows the pinned goal.
        assert!(
            msgs[2]["content"]
                .as_str()
                .unwrap()
                .contains("removed to fit"),
            "elision notice present after the goal"
        );
        let last = msgs.last().unwrap()["content"].as_str().unwrap();
        assert!(last.starts_with("msg-19"), "latest message preserved");
        let total: usize = msgs
            .iter()
            .map(|m| m["content"].as_str().map(|s| s.len()).unwrap_or(0))
            .sum();
        assert!(total < 3_000, "trimmed total = {total}");
    }

    #[test]
    fn trim_conversation_pins_goal_without_system_prompt() {
        // No system message: the first message is the goal and must survive.
        let mut msgs = vec![serde_json::json!({
            "role": "user",
            "content": format!("THE GOAL {}", "g".repeat(300))
        })];
        for i in 0..20 {
            msgs.push(serde_json::json!({"role": "user", "content": format!("msg-{i}-{}", "x".repeat(500))}));
        }
        trim_conversation(&mut msgs, 2_000);

        assert!(
            msgs[0]["content"].as_str().unwrap().starts_with("THE GOAL"),
            "goal preserved as the first message, got {:?}",
            msgs[0]["content"]
        );
        let last = msgs.last().unwrap()["content"].as_str().unwrap();
        assert!(last.starts_with("msg-19"), "latest message preserved");
    }

    #[test]
    fn trim_conversation_noop_under_budget() {
        let mut msgs = vec![
            serde_json::json!({"role": "system", "content": "SYS"}),
            serde_json::json!({"role": "user", "content": "hello"}),
        ];
        let before = msgs.clone();
        trim_conversation(&mut msgs, 10_000);
        assert_eq!(msgs, before);
    }

    #[test]
    fn budget_strategy_budgets_and_suffix() {
        let default = BudgetStrategy {
            context_length: None,
            small_model_harness: false,
            minimize_tokens: false,
        };
        assert_eq!(default.tool_result_char_cap(), DEFAULT_TOOL_RESULT_CHAR_CAP);
        assert_eq!(
            default.conversation_char_budget(),
            DEFAULT_CONVERSATION_CHAR_BUDGET
        );
        assert_eq!(default.system_prompt_suffix(), None);

        let small = BudgetStrategy {
            context_length: None,
            small_model_harness: true,
            minimize_tokens: true,
        };
        assert_eq!(
            small.tool_result_char_cap(),
            SMALL_MODEL_TOOL_RESULT_CHAR_CAP
        );
        assert_eq!(
            small.conversation_char_budget(),
            SMALL_MODEL_CONVERSATION_CHAR_BUDGET
        );
        assert!(
            small
                .system_prompt_suffix()
                .is_some_and(|s| s.contains("concisely"))
        );

        // A known context window drives both budgets and overrides the flag.
        let windowed = BudgetStrategy {
            context_length: Some(8_192),
            small_model_harness: true,
            minimize_tokens: false,
        };
        assert_eq!(windowed.tool_result_char_cap(), 8_192 * CHARS_PER_TOKEN / 4);
        assert_eq!(
            windowed.conversation_char_budget(),
            8_192 * CHARS_PER_TOKEN * 3 / 4
        );
    }

    #[test]
    fn context_strategy_maps_config_and_wrappers_delegate() {
        let cfg = McpChatConfig {
            base_url: "http://x".into(),
            workspace_root: PathBuf::from("/tmp"),
            session_id: None,
            external_http_servers: BTreeMap::new(),
            max_turns: 8,
            tool_approval: false,
            mcp_connections: ahma_mcp::mcp_client::McpConnectionManager::default(),
            minimize_tokens: true,
            small_model_harness: true,
            context_length: None,
        };
        let strat = cfg.context_strategy();
        assert!(strat.small_model_harness && strat.minimize_tokens);
        // The free-function wrappers must agree with the strategy.
        assert_eq!(
            tool_result_char_cap(&cfg),
            strat.tool_result_char_cap(),
            "wrapper must delegate"
        );
        assert_eq!(
            conversation_char_budget(&cfg),
            strat.conversation_char_budget()
        );
        assert!(strat.system_prompt_suffix().is_some());
    }

    #[tokio::test]
    async fn test_agent_task_tool_call_loop() {
        let llm_counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let llm_router = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(move || {
                let counter = llm_counter.clone();
                async move {
                    // The tool-calling path streams; reply with SSE. Turn 1 emits a
                    // tool call, turn 2 emits the final answer as one content delta.
                    let count = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let body = if count == 0 {
                        concat!(
                            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_123\",\"function\":{\"name\":\"test_tool\",\"arguments\":\"{\\\"arg\\\":\\\"value\\\"}\"}}]}}]}\n",
                            "data: [DONE]\n",
                        )
                        .to_string()
                    } else {
                        concat!(
                            "data: {\"choices\":[{\"delta\":{\"content\":\"Tool call was successful.\"}}]}\n",
                            "data: [DONE]\n",
                        )
                        .to_string()
                    };
                    axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(body)
                        .unwrap()
                }
            }),
        );

        let llm_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm_addr = llm_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(llm_listener, llm_router).await.unwrap();
        });

        let mcp_router = axum::Router::new().route(
            "/mcp",
            axum::routing::post(
                |axum::Json(body): axum::Json<serde_json::Value>| async move {
                    use axum::response::IntoResponse;
                    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    match method {
                        "initialize" => {
                            let mut headers = axum::http::HeaderMap::new();
                            headers.insert(
                                "mcp-session-id",
                                axum::http::HeaderValue::from_static("test-session-123"),
                            );
                            (headers, axum::Json(serde_json::json!({}))).into_response()
                        }
                        "tools/call" => axum::Json(serde_json::json!({
                            "result": {
                                "content": [{"type": "text", "text": "Tool executed"}]
                            }
                        }))
                        .into_response(),
                        _ => axum::http::StatusCode::OK.into_response(),
                    }
                },
            )
            // The full MCP handshake requires a GET /mcp SSE stream. Mirror it:
            // emit `notifications/sandbox/configured` immediately so the client's
            // ready + locked signals both fire, then hold the stream open.
            .get(|| async {
                use axum::response::IntoResponse;
                use futures::StreamExt;
                let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/sandbox/configured\"}\n\n";
                // A never-completing tail keeps the stream open like the real bridge.
                let head = futures::stream::once(async move {
                    Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(body))
                });
                let tail = futures::stream::pending::<Result<axum::body::Bytes, std::convert::Infallible>>();
                let stream = head.chain(tail);
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    axum::body::Body::from_stream(stream),
                )
                    .into_response()
            }),
        );

        let mcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mcp_addr = mcp_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(mcp_listener, mcp_router).await.unwrap();
        });

        let client = LlmClient::new(format!("http://{}", llm_addr), "test-model", None);
        let mcp = McpChatConfig {
            base_url: format!("http://{}", mcp_addr),
            workspace_root: PathBuf::from("/tmp"),
            session_id: None,
            external_http_servers: BTreeMap::new(),
            max_turns: 2,
            tool_approval: false,
            mcp_connections: ahma_mcp::mcp_client::McpConnectionManager::default(),
            minimize_tokens: false,
            small_model_harness: false,
            context_length: None,
        };

        let (tx, mut rx) = mpsc::channel(100);
        let messages = vec![ChatMessage::user("Do something")];

        spawn_agent_task(
            client,
            messages,
            None,
            Some(mcp),
            vec![ahma_mcp::mcp_client::ToolInfo {
                name: "test_tool".to_string(),
                description: None,
                input_schema: serde_json::json!({
                    "type": "object",
                    "additionalProperties": true
                }),
            }],
            tx,
            Arc::new(AutoApproveGate),
        );

        let mut events = Vec::new();
        while let Some(evt) = rx.recv().await {
            events.push(evt);
            if events.len() > 10 {
                break;
            }
        }

        let mut got_start = false;
        let mut got_finish = false;
        let mut got_token = false;
        let mut got_done = false;

        for evt in events {
            match evt {
                AgentEvent::ToolCallStarted { name, .. } => {
                    assert_eq!(name, "test_tool");
                    got_start = true;
                }
                AgentEvent::ToolCallFinished { result, failed, .. } => {
                    assert_eq!(result, "Tool executed");
                    assert!(!failed);
                    got_finish = true;
                }
                AgentEvent::Token(t) => {
                    assert_eq!(t, "Tool call was successful.");
                    got_token = true;
                }
                AgentEvent::Done => {
                    got_done = true;
                }
                AgentEvent::Error(e) => panic!("Unexpected error: {e}"),
                _ => {}
            }
        }

        assert!(got_start, "Missing ToolCallStarted");
        assert!(got_finish, "Missing ToolCallFinished");
        assert!(got_token, "Missing Token");
        assert!(got_done, "Missing Done");
    }

    /// When the model never stops requesting tools and the turn budget is
    /// exhausted, the loop must not dead-end on an opaque error: it runs one
    /// final **tool-free** turn so the model summarises progress, and the user
    /// sees that summary plus `Done` — never `Error`. The LLM mock returns a
    /// tool call whenever `tools` is present and a plain summary when the request
    /// carries no tools (the closing turn).
    #[tokio::test]
    async fn test_agent_task_summarises_when_turn_budget_exhausted() {
        let llm_router = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(
                |axum::Json(body): axum::Json<serde_json::Value>| async move {
                    let tools_empty = body
                        .get("tools")
                        .and_then(|t| t.as_array())
                        .map(|a| a.is_empty())
                        .unwrap_or(true);
                    let sse = if tools_empty {
                        // Closing summary turn: no tools offered → plain content.
                        concat!(
                            "data: {\"choices\":[{\"delta\":{\"content\":\"Reached the turn limit. I inspected the file; the build still fails; next run cargo build.\"}}]}\n",
                            "data: [DONE]\n",
                        )
                    } else {
                        // Every tool-enabled turn keeps requesting a tool, so the
                        // loop never completes on its own.
                        concat!(
                            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"test_tool\",\"arguments\":\"{}\"}}]}}]}\n",
                            "data: [DONE]\n",
                        )
                    };
                    axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(sse.to_string())
                        .unwrap()
                },
            ),
        );

        let llm_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm_addr = llm_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(llm_listener, llm_router).await.unwrap();
        });

        let mcp_router = axum::Router::new().route(
            "/mcp",
            axum::routing::post(
                |axum::Json(body): axum::Json<serde_json::Value>| async move {
                    use axum::response::IntoResponse;
                    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    match method {
                        "initialize" => {
                            let mut headers = axum::http::HeaderMap::new();
                            headers.insert(
                                "mcp-session-id",
                                axum::http::HeaderValue::from_static("sess-limit"),
                            );
                            (headers, axum::Json(serde_json::json!({}))).into_response()
                        }
                        "tools/call" => axum::Json(serde_json::json!({
                            "result": { "content": [{"type": "text", "text": "Tool executed"}] }
                        }))
                        .into_response(),
                        _ => axum::http::StatusCode::OK.into_response(),
                    }
                },
            )
            .get(|| async {
                use axum::response::IntoResponse;
                use futures::StreamExt;
                let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/sandbox/configured\"}\n\n";
                let head = futures::stream::once(async move {
                    Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(body))
                });
                let tail =
                    futures::stream::pending::<Result<axum::body::Bytes, std::convert::Infallible>>();
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    axum::body::Body::from_stream(head.chain(tail)),
                )
                    .into_response()
            }),
        );

        let mcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mcp_addr = mcp_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(mcp_listener, mcp_router).await.unwrap();
        });

        let client = LlmClient::new(format!("http://{}", llm_addr), "test-model", None);
        let mcp = McpChatConfig {
            base_url: format!("http://{}", mcp_addr),
            workspace_root: PathBuf::from("/tmp"),
            session_id: None,
            external_http_servers: BTreeMap::new(),
            max_turns: 1,
            tool_approval: false,
            mcp_connections: ahma_mcp::mcp_client::McpConnectionManager::default(),
            minimize_tokens: false,
            small_model_harness: false,
            context_length: None,
        };

        let (tx, mut rx) = mpsc::channel(100);
        spawn_agent_task(
            client,
            vec![ChatMessage::user("Fix the build")],
            None,
            Some(mcp),
            vec![ahma_mcp::mcp_client::ToolInfo {
                name: "test_tool".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object", "additionalProperties": true}),
            }],
            tx,
            Arc::new(AutoApproveGate),
        );

        let mut summary = String::new();
        let mut got_done = false;
        while let Some(evt) = rx.recv().await {
            match evt {
                AgentEvent::Token(t) => summary.push_str(&t),
                AgentEvent::Done => {
                    got_done = true;
                    break;
                }
                AgentEvent::Error(e) => panic!("Budget exhaustion must summarise, not error: {e}"),
                _ => {}
            }
        }

        assert!(got_done, "summary turn must end with Done");
        assert!(
            summary.contains("turn limit"),
            "expected a closing progress summary, got: {summary:?}"
        );
    }

    /// Regression: a freshly created MCP session must complete the full
    /// Streamable-HTTP handshake — open the SSE stream, answer `roots/list`, and
    /// reach `Active` — before `get_or_create_session` returns. The bridge gates
    /// `tools/call` per session and returns HTTP 409 until the session is locked,
    /// so a handshake that skips SSE/`roots/list` (the previous behaviour) leaves
    /// every tool call stuck at "Sandbox initializing". This mock reproduces the
    /// per-session gating: `tools/call` 409s until the client POSTs its
    /// `roots/list` response.
    #[tokio::test]
    async fn test_get_or_create_session_completes_handshake_before_tools_call() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let roots_answered = Arc::new(AtomicBool::new(false));
        let ra_post = roots_answered.clone();

        let router = axum::Router::new().route(
            "/mcp",
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let ra = ra_post.clone();
                async move {
                    use axum::response::IntoResponse;
                    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");

                    // A client roots/list *response* carries result.roots and no method.
                    if method.is_empty() && body.pointer("/result/roots").is_some() {
                        ra.store(true, Ordering::SeqCst);
                        return axum::http::StatusCode::ACCEPTED.into_response();
                    }

                    match method {
                        "initialize" => {
                            let mut headers = axum::http::HeaderMap::new();
                            headers.insert(
                                "mcp-session-id",
                                axum::http::HeaderValue::from_static("sess-handshake"),
                            );
                            (headers, axum::Json(serde_json::json!({}))).into_response()
                        }
                        "notifications/initialized" => {
                            axum::http::StatusCode::ACCEPTED.into_response()
                        }
                        "tools/call" => {
                            if ra.load(Ordering::SeqCst) {
                                axum::Json(serde_json::json!({
                                    "result": { "content": [{"type": "text", "text": "ok"}] }
                                }))
                                .into_response()
                            } else {
                                // Per-session gate: not locked yet.
                                (
                                    axum::http::StatusCode::CONFLICT,
                                    axum::Json(serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "error": { "code": -32001, "message": "Sandbox initializing" }
                                    })),
                                )
                                    .into_response()
                            }
                        }
                        _ => axum::http::StatusCode::OK.into_response(),
                    }
                }
            })
            .get(|| async {
                use axum::response::IntoResponse;
                use futures::StreamExt;
                // Server → client roots/list request, then sandbox/configured once
                // the client has had a chance to answer; then hold the stream open.
                let roots_req = "data: {\"jsonrpc\":\"2.0\",\"id\":\"r1\",\"method\":\"roots/list\"}\n\n";
                let configured =
                    "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/sandbox/configured\"}\n\n";
                let s1 = futures::stream::once(async move {
                    Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(roots_req))
                });
                let s2 = futures::stream::once(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(configured))
                });
                let tail = futures::stream::pending::<
                    Result<axum::body::Bytes, std::convert::Infallible>,
                >();
                let stream = s1.chain(s2).chain(tail);
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    axum::body::Body::from_stream(stream),
                )
                    .into_response()
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let base_url = format!("http://{}", addr);
        let mcp = McpChatConfig {
            base_url: base_url.clone(),
            workspace_root: PathBuf::from("/tmp"),
            session_id: None,
            external_http_servers: BTreeMap::new(),
            max_turns: 2,
            tool_approval: false,
            mcp_connections: ahma_mcp::mcp_client::McpConnectionManager::default(),
            minimize_tokens: false,
            small_model_harness: false,
            context_length: None,
        };

        let client = reqwest::Client::new();
        let url = format!("{}/mcp", base_url);

        let sid = get_or_create_session(&client, &url, &mcp)
            .await
            .expect("handshake should complete");
        assert_eq!(sid, "sess-handshake");
        assert!(
            roots_answered.load(Ordering::SeqCst),
            "client must answer roots/list during the handshake"
        );

        // The session is now locked, so tools/call succeeds.
        let (text, is_err) =
            call_mcp_tool_http(&client, &url, &sid, "status", serde_json::json!({}))
                .await
                .expect("tools/call should succeed after handshake");
        assert!(!is_err, "tool call should not be an error: {text}");
        assert_eq!(text, "ok");
    }

    #[test]
    fn test_get_mcp_base_url_resolution() {
        use ahma_mcp::shell::cli::AppConfig;

        // 1. Default (None)
        assert_eq!(get_mcp_base_url(None), "http://127.0.0.1:3000");

        // 2. HTTP host/port
        let config1 = AppConfig {
            http_host: "12.34.56.78".to_string(),
            http_port: 8888,
            ..AppConfig::default()
        };
        assert_eq!(get_mcp_base_url(Some(&config1)), "http://12.34.56.78:8888");

        // 3. Unix socket
        let config2 = AppConfig {
            unix_socket_path: "/path/to/socket".to_string(),
            ..AppConfig::default()
        };
        assert_eq!(get_mcp_base_url(Some(&config2)), "unix:///path/to/socket");
    }

    fn empty_mcp_config(base_url: &str) -> McpChatConfig {
        McpChatConfig {
            base_url: base_url.to_string(),
            workspace_root: PathBuf::from("/tmp"),
            session_id: None,
            external_http_servers: BTreeMap::new(),
            max_turns: 2,
            tool_approval: false,
            mcp_connections: ahma_mcp::mcp_client::McpConnectionManager::default(),
            minimize_tokens: false,
            small_model_harness: false,
            context_length: None,
        }
    }

    /// Routing guard: a `server::tool` name whose server is registered in
    /// neither `mcp_connections` nor `external_http_servers` must surface a
    /// clear "unknown server" error rather than silently falling through to the
    /// local bridge. This pins the `::` dispatch decision so a future change to
    /// `dispatch_tool_execution` can't quietly mis-route external calls.
    #[tokio::test]
    async fn dispatch_unknown_external_server_errors() {
        let cfg = empty_mcp_config("http://127.0.0.1:3000");
        let err = dispatch_tool_execution("ghost::do_thing", serde_json::json!({}), &cfg)
            .await
            .expect_err("unknown external server must error");
        assert!(
            err.contains("Unknown external MCP server") && err.contains("ghost"),
            "unexpected error: {err}"
        );
    }

    // ── Budget math edge cases (tool_result_char_cap / conversation_char_budget) ──

    #[test]
    fn budgets_clamp_to_minimum_for_tiny_context() {
        // 100 tokens * 4 chars / 4 = 100 → clamped up to the 1_000 floor.
        let cfg = cfg_with(Some(100), false);
        assert_eq!(tool_result_char_cap(&cfg), 1_000);
        // 100 * 4 * 3 / 4 = 300 → clamped up to the 4_000 floor.
        assert_eq!(conversation_char_budget(&cfg), 4_000);
    }

    #[test]
    fn budgets_zero_context_uses_floor() {
        let cfg = cfg_with(Some(0), false);
        assert_eq!(tool_result_char_cap(&cfg), 1_000);
        assert_eq!(conversation_char_budget(&cfg), 4_000);
    }

    #[test]
    fn budgets_default_caps_without_context_or_harness() {
        let cfg = cfg_with(None, false);
        assert_eq!(tool_result_char_cap(&cfg), DEFAULT_TOOL_RESULT_CHAR_CAP);
        assert_eq!(
            conversation_char_budget(&cfg),
            DEFAULT_CONVERSATION_CHAR_BUDGET
        );
    }

    #[test]
    fn budgets_small_harness_specific_caps() {
        let cfg = cfg_with(None, true);
        assert_eq!(tool_result_char_cap(&cfg), SMALL_MODEL_TOOL_RESULT_CHAR_CAP);
        assert_eq!(
            conversation_char_budget(&cfg),
            SMALL_MODEL_CONVERSATION_CHAR_BUDGET
        );
    }

    #[test]
    fn budgets_context_length_overrides_harness_flag() {
        // When context_length is Some, the harness flag is ignored entirely.
        let cfg = cfg_with(Some(8192), true);
        assert_eq!(tool_result_char_cap(&cfg), 8192);
        assert_eq!(conversation_char_budget(&cfg), 24_576);
    }

    // ── truncate_middle ──

    #[test]
    fn truncate_middle_exact_cap_and_empty_are_noops() {
        assert_eq!(truncate_middle("abcde", 5), "abcde");
        assert_eq!(truncate_middle("", 0), "");
        assert!(truncate_middle("a", 0).contains("characters elided"));
    }

    #[test]
    fn truncate_middle_respects_unicode_char_boundaries() {
        // Mix of 2-byte (é, ü) and 4-byte (😀) characters; truncation works on
        // char counts, so this must never split a multi-byte char or panic.
        let s: String = "é".repeat(1000) + "😀😀😀" + &"ü".repeat(1000);
        let out = truncate_middle(&s, 50);
        assert!(out.contains("characters elided"), "marker present");
        assert!(out.starts_with('é'), "head keeps leading multibyte char");
        assert!(out.ends_with('ü'), "tail keeps trailing multibyte char");
        // Valid UTF-8 round-trips losslessly through chars().
        assert_eq!(out.chars().collect::<String>(), out);
    }

    // ── trim_conversation without a system prompt ──

    #[test]
    fn trim_conversation_without_system_keeps_two_recent() {
        let mut msgs = Vec::new();
        for i in 0..10 {
            msgs.push(
                serde_json::json!({"role": "user", "content": format!("m{i}-{}", "y".repeat(500))}),
            );
        }
        trim_conversation(&mut msgs, 1_500);
        // No system prompt, so protected_head == 1: the first user message (the
        // goal, m0) is pinned, then the elision notice, then recent messages.
        assert!(
            msgs[0]["content"].as_str().unwrap().starts_with("m0"),
            "goal (first user message) pinned, got {:?}",
            msgs[0]["content"]
        );
        assert!(
            msgs[1]["content"]
                .as_str()
                .unwrap()
                .contains("removed to fit"),
            "elision notice present after the goal"
        );
        assert!(
            msgs.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .starts_with("m9"),
            "latest message preserved"
        );
    }

    #[test]
    fn trim_conversation_stops_at_protected_floor() {
        // Even an over-budget conversation never drops below the protected
        // head (system) + 2 most recent messages.
        let mut msgs = vec![
            serde_json::json!({"role": "system", "content": "S".repeat(2000)}),
            serde_json::json!({"role": "user", "content": "U".repeat(2000)}),
            serde_json::json!({"role": "assistant", "content": "A".repeat(2000)}),
        ];
        trim_conversation(&mut msgs, 10);
        // system + 2 recent are all protected; nothing droppable, no notice added.
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "system");
    }

    // ── prepare_tool_definitions ──

    #[test]
    fn prepare_tool_definitions_maps_and_defaults_description() {
        let tools = vec![
            ahma_mcp::mcp_client::ToolInfo {
                name: "with_desc".to_string(),
                description: Some("does X".to_string()),
                input_schema: serde_json::json!({"type": "object"}),
            },
            ahma_mcp::mcp_client::ToolInfo {
                name: "no_desc".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object", "required": ["a"]}),
            },
        ];
        let defs = prepare_tool_definitions(tools);
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0]["type"], "function");
        assert_eq!(defs[0]["function"]["name"], "with_desc");
        assert_eq!(defs[0]["function"]["description"], "does X");
        assert_eq!(defs[0]["function"]["parameters"]["type"], "object");
        // None description falls back to the canned text.
        assert_eq!(
            defs[1]["function"]["description"],
            "MCP tool callable from ahma"
        );
        assert_eq!(defs[1]["function"]["parameters"]["required"][0], "a");
    }

    #[test]
    fn prepare_tool_definitions_empty_input() {
        assert!(prepare_tool_definitions(Vec::new()).is_empty());
    }

    // ── system_prompt_for_run / initial_msg_json ──

    fn harness_cfg() -> McpChatConfig {
        McpChatConfig {
            base_url: "http://x".into(),
            workspace_root: PathBuf::from("/tmp"),
            session_id: None,
            external_http_servers: BTreeMap::new(),
            max_turns: 8,
            tool_approval: false,
            mcp_connections: ahma_mcp::mcp_client::McpConnectionManager::default(),
            minimize_tokens: true,
            small_model_harness: true,
            context_length: None,
        }
    }

    #[test]
    fn system_prompt_for_run_without_strategy_suffix_is_unchanged() {
        assert_eq!(
            system_prompt_for_run(Some("base".to_string()), &None),
            Some("base".to_string())
        );
        assert_eq!(system_prompt_for_run(None, &None), None);
    }

    #[test]
    fn system_prompt_for_run_appends_suffix_and_promotes_it_when_alone() {
        let cfg = Some(harness_cfg());
        let suffix = harness_cfg()
            .context_strategy()
            .system_prompt_suffix()
            .expect("harness config supplies a suffix");

        let appended = system_prompt_for_run(Some("base".to_string()), &cfg)
            .expect("caller prompt is preserved");
        assert_eq!(appended, format!("base{suffix}"));

        let promoted = system_prompt_for_run(None, &cfg).expect("suffix becomes the whole prompt");
        assert_eq!(promoted, suffix.trim());
    }

    #[test]
    fn initial_msg_json_prepends_system_prompt_then_conversation() {
        let messages = vec![ChatMessage::user("hi"), ChatMessage::user("again")];

        let with_system = initial_msg_json(&Some("sys".to_string()), &messages);
        assert_eq!(with_system.len(), 3);
        assert_eq!(with_system[0]["role"], "system");
        assert_eq!(with_system[0]["content"], "sys");
        assert_eq!(with_system[1]["content"], "hi");
        assert_eq!(with_system[2]["content"], "again");

        let without_system = initial_msg_json(&None, &messages);
        assert_eq!(without_system.len(), 2);
        assert_eq!(without_system[0]["content"], "hi");
    }

    // ── plan_harness_hints ──

    #[test]
    fn plan_harness_hints_flags_error_and_read() {
        let results = vec![
            (
                "id1".to_string(),
                "read_file".to_string(),
                serde_json::json!({}),
                false,
            ),
            (
                "id2".to_string(),
                "other".to_string(),
                serde_json::json!({}),
                true,
            ),
        ];
        let (err, read) = plan_harness_hints(&results, false, false);
        assert!(err, "a failed call triggers the error hint");
        assert!(read, "a read_file call triggers the read hint");
    }

    #[test]
    fn plan_harness_hints_list_dir_triggers_read() {
        let results = vec![(
            "id".to_string(),
            "list_dir".to_string(),
            serde_json::json!({}),
            false,
        )];
        let (err, read) = plan_harness_hints(&results, false, false);
        assert!(!err);
        assert!(read);
    }

    #[test]
    fn plan_harness_hints_respects_already_hinted() {
        let results = vec![(
            "id".to_string(),
            "read_file".to_string(),
            serde_json::json!({}),
            true,
        )];
        let (err, read) = plan_harness_hints(&results, true, true);
        assert!(!err, "error already hinted earlier");
        assert!(!read, "read already hinted earlier");
    }

    #[test]
    fn plan_harness_hints_no_triggers_for_plain_success() {
        let results = vec![(
            "id".to_string(),
            "build".to_string(),
            serde_json::json!({}),
            false,
        )];
        let (err, read) = plan_harness_hints(&results, false, false);
        assert!(!err);
        assert!(!read);
    }

    // ── append_hint_to_field ──

    #[test]
    fn append_hint_to_field_appends_only_to_existing_string() {
        let mut obj = serde_json::Map::new();
        obj.insert("output".to_string(), serde_json::json!("base"));
        obj.insert("num".to_string(), serde_json::json!(5));
        append_hint_to_field(&mut obj, "output", "+hint");
        assert_eq!(obj["output"], "base+hint");
        // Non-string field is untouched.
        append_hint_to_field(&mut obj, "num", "+hint");
        assert_eq!(obj["num"], 5);
        // Missing key is a no-op (no panic, no insertion).
        append_hint_to_field(&mut obj, "missing", "+hint");
        assert!(obj.get("missing").is_none());
    }

    // ── push_tool_message_with_hints ──

    #[test]
    fn push_tool_message_injects_error_hint_into_error_key() {
        let mut msgs = Vec::new();
        let mut error_hinted = false;
        let mut read_hinted = false;
        push_tool_message_with_hints(
            &mut msgs,
            "call1".to_string(),
            "build",
            serde_json::json!({"error": "boom"}),
            true, // failed
            true, // inject_error_hint
            false,
            &mut error_hinted,
            &mut read_hinted,
            100_000,
        );
        assert!(error_hinted, "error_hinted latch flips");
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "call1");
        let content = msgs[0]["content"].as_str().unwrap();
        assert!(content.contains("Harness Hint"), "hint injected: {content}");
        assert!(content.contains("boom"), "original error retained");
    }

    #[test]
    fn push_tool_message_error_hint_uses_output_key_when_no_error_key() {
        let mut msgs = Vec::new();
        let mut error_hinted = false;
        let mut read_hinted = false;
        push_tool_message_with_hints(
            &mut msgs,
            "c".to_string(),
            "build",
            serde_json::json!({"output": "stuff"}),
            true,
            true,
            false,
            &mut error_hinted,
            &mut read_hinted,
            100_000,
        );
        assert!(error_hinted);
        let content = msgs[0]["content"].as_str().unwrap();
        assert!(content.contains("Harness Hint"));
        assert!(content.contains("stuff"));
    }

    #[test]
    fn push_tool_message_injects_read_hint_for_read_tools() {
        for tool in ["read_file", "list_dir"] {
            let mut msgs = Vec::new();
            let mut error_hinted = false;
            let mut read_hinted = false;
            push_tool_message_with_hints(
                &mut msgs,
                "c".to_string(),
                tool,
                serde_json::json!({"output": "file contents"}),
                false,
                false,
                true,
                &mut error_hinted,
                &mut read_hinted,
                100_000,
            );
            assert!(read_hinted, "read latch flips for {tool}");
            let content = msgs[0]["content"].as_str().unwrap();
            assert!(
                content.contains("replace_in_file"),
                "read hint mentions replace_in_file for {tool}"
            );
        }
    }

    #[test]
    fn push_tool_message_no_hints_when_flags_off() {
        let mut msgs = Vec::new();
        let mut error_hinted = false;
        let mut read_hinted = false;
        push_tool_message_with_hints(
            &mut msgs,
            "c".to_string(),
            "build",
            serde_json::json!({"output": "ok"}),
            false,
            false,
            false,
            &mut error_hinted,
            &mut read_hinted,
            100_000,
        );
        assert!(!error_hinted);
        assert!(!read_hinted);
        let content = msgs[0]["content"].as_str().unwrap();
        assert!(!content.contains("Harness Hint"));
    }

    #[test]
    fn push_tool_message_skips_error_hint_when_already_hinted() {
        let mut msgs = Vec::new();
        let mut error_hinted = true; // a prior tool already emitted the hint
        let mut read_hinted = false;
        push_tool_message_with_hints(
            &mut msgs,
            "c".to_string(),
            "build",
            serde_json::json!({"error": "boom"}),
            true,
            true,
            false,
            &mut error_hinted,
            &mut read_hinted,
            100_000,
        );
        let content = msgs[0]["content"].as_str().unwrap();
        assert!(!content.contains("Harness Hint"), "no double hint");
        assert!(content.contains("boom"));
    }

    #[test]
    fn push_tool_message_truncates_large_payload() {
        let mut msgs = Vec::new();
        let mut error_hinted = false;
        let mut read_hinted = false;
        let big = "x".repeat(5000);
        push_tool_message_with_hints(
            &mut msgs,
            "c".to_string(),
            "build",
            serde_json::json!({"output": big}),
            false,
            false,
            false,
            &mut error_hinted,
            &mut read_hinted,
            500,
        );
        let content = msgs[0]["content"].as_str().unwrap();
        assert!(content.contains("characters elided"), "truncation applied");
        assert!(content.chars().count() < 800, "capped near the budget");
    }

    // encode_file_uri and the SSE framing helpers moved to `ahma_common`
    // (`file_uri` / `sse` modules) — their unit tests live there now.

    // ── parse_mcp_response / parse_error_message / extract_content_text ──

    #[test]
    fn parse_mcp_response_error_field() {
        let resp = serde_json::json!({"error": {"message": "bad"}});
        let (text, is_err) = parse_mcp_response(&resp);
        assert!(is_err);
        assert_eq!(text, "Error: bad");
    }

    #[test]
    fn parse_mcp_response_error_without_message_defaults() {
        let (text, is_err) = parse_mcp_response(&serde_json::json!({"error": {}}));
        assert!(is_err);
        assert_eq!(text, "Error: unknown error");
    }

    #[test]
    fn parse_mcp_response_is_error_flag_with_content() {
        let resp = serde_json::json!({
            "result": {"isError": true, "content": [{"type": "text", "text": "oops"}]}
        });
        let (text, is_err) = parse_mcp_response(&resp);
        assert!(is_err, "isError flag surfaces as error");
        assert_eq!(text, "oops");
    }

    #[test]
    fn parse_mcp_response_success_joins_content_lines() {
        let resp = serde_json::json!({
            "result": {"content": [
                {"type": "text", "text": "line1"},
                {"type": "text", "text": "line2"},
            ]}
        });
        let (text, is_err) = parse_mcp_response(&resp);
        assert!(!is_err);
        assert_eq!(text, "line1\nline2");
    }

    #[test]
    fn parse_mcp_response_empty_result_object() {
        let (text, is_err) = parse_mcp_response(&serde_json::json!({}));
        assert!(!is_err);
        assert_eq!(text, "Empty result");
    }

    #[test]
    fn parse_mcp_response_result_without_content_pretty_prints() {
        let resp = serde_json::json!({"result": {"foo": "bar"}});
        let (text, is_err) = parse_mcp_response(&resp);
        assert!(!is_err);
        assert!(text.contains("\"foo\""), "pretty JSON: {text}");
        assert!(text.contains("\"bar\""), "pretty JSON: {text}");
    }

    // ── needs_approval ──

    #[test]
    fn needs_approval_when_globally_enabled_is_always_true() {
        assert!(needs_approval("read_file", true));
        assert!(needs_approval("anything_at_all", true));
    }

    #[test]
    fn needs_approval_disabled_only_for_mutating_tools() {
        assert!(needs_approval("write_file", false));
        assert!(needs_approval("replace_in_file", false));
        assert!(needs_approval("server::write_file", false));
        assert!(needs_approval("server::replace_in_file", false));
        // Read-only / unrelated tools do not require approval.
        assert!(!needs_approval("read_file", false));
        assert!(!needs_approval("status", false));
        assert!(!needs_approval("server::read_file", false));
    }

    // ── resolve_llm_connection (URL fast-path) ──
    // Note: a URL provider never *requires* config, but `num_ctx` is enriched
    // from a matching configured provider when one exists (#484), so these
    // tests do not assert on `num_ctx` — its value depends on the local
    // ~/.ahma/config.toml. The URL-matching lookup itself is unit-tested as
    // `AhmaConfig::num_ctx_for_base_url` in ahma_common.

    #[test]
    fn resolve_llm_connection_url_provider_bypasses_config() {
        let conn = resolve_llm_connection(
            Some("http://localhost:1234/v1".to_string()),
            Some("my-model".to_string()),
        )
        .expect("URL provider resolves without config");
        assert_eq!(conn.base_url, "http://localhost:1234/v1");
        assert_eq!(conn.model, "my-model");
        assert!(conn.api_key.is_none());
    }

    #[test]
    fn resolve_llm_connection_url_provider_uses_empty_default_model() {
        let conn = resolve_llm_connection(Some("unix:///run/ahma.sock".to_string()), None)
            .expect("unix URL provider resolves");
        assert_eq!(conn.base_url, "unix:///run/ahma.sock");
        assert_eq!(conn.model, "", "missing model defaults to empty string");
        assert!(conn.api_key.is_none());
    }

    /// Regression for issue #484: an explicit `kind` from config must reach the
    /// client. `LlmClient::new`'s heuristic keys off the Anthropic *host*, so a
    /// `kind = "anthropic"` provider behind a proxy URL was silently built as
    /// an OpenAI client and spoke the wrong wire format.
    #[test]
    fn explicit_provider_kind_overrides_the_url_flavor_heuristic() {
        use ahma_common::config::ProviderKind;

        let proxied = LlmConnection {
            base_url: "https://llm-gateway.internal/v1".to_string(),
            model: "claude-opus-4-8".to_string(),
            api_key: Some("k".to_string()),
            num_ctx: None,
            kind: Some(ProviderKind::Anthropic),
        };
        assert_eq!(
            proxied.into_client().flavor(),
            ahma_llm_monitor::ApiFlavor::Anthropic,
            "configured kind must win over the host heuristic"
        );

        // No matching config entry → the heuristic stays in charge.
        let unknown = LlmConnection {
            base_url: "https://llm-gateway.internal/v1".to_string(),
            model: "m".to_string(),
            api_key: None,
            num_ctx: None,
            kind: None,
        };
        assert_eq!(
            unknown.into_client().flavor(),
            ahma_llm_monitor::ApiFlavor::OpenAi
        );
    }

    /// The context window must survive client construction alongside `kind` —
    /// `with_flavor` runs after `LlmClient::new`, so applying it in the wrong
    /// order would clobber `num_ctx` and re-break compaction (issue #484).
    #[test]
    fn provider_kind_and_num_ctx_both_reach_the_client() {
        use ahma_common::config::ProviderKind;

        let conn = LlmConnection {
            base_url: "http://localhost:11434/v1".to_string(),
            model: "ornith:35b".to_string(),
            api_key: None,
            num_ctx: Some(16_384),
            kind: Some(ProviderKind::OpenAi),
        };
        let client = conn.into_client();
        assert_eq!(client.flavor(), ahma_llm_monitor::ApiFlavor::OpenAi);
        assert!(
            client.sends_num_ctx(),
            "num_ctx must survive the with_flavor call"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Added coverage: MCP sampling routing, tool dispatch, HTTP tool calls,
    // session reuse, approval gates, agent-turn branches, completion fallback.
    // ─────────────────────────────────────────────────────────────────────────

    struct RejectGate;
    #[async_trait::async_trait]
    impl AgentApprovalGate for RejectGate {
        async fn request_approval(&self, _id: &str, _tool: &str, _args: &str) -> bool {
            false
        }
    }

    /// Bind an ephemeral loopback port and serve `router`, returning its base URL.
    /// The listener is bound before the accept loop spawns, so the OS backlog
    /// accepts client connections immediately (no startup race).
    async fn serve_router(router: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{}", addr)
    }

    /// A mock `/mcp` endpoint that always answers POSTs with a fixed JSON body.
    async fn mock_post_json(body: serde_json::Value) -> String {
        let router = axum::Router::new().route(
            "/mcp",
            axum::routing::post(move || {
                let body = body.clone();
                async move { axum::Json(body) }
            }),
        );
        serve_router(router).await
    }

    /// A mock `/mcp` endpoint that always answers POSTs with a fixed status.
    async fn mock_post_status(status: axum::http::StatusCode) -> String {
        let router = axum::Router::new().route(
            "/mcp",
            axum::routing::post(move || async move { (status, "boom-body") }),
        );
        serve_router(router).await
    }

    /// A minimal MCP tool server: initialize (optionally returning a session
    /// header), notifications/initialized, and a tools/call that returns text.
    fn mcp_tool_router(with_session_header: bool) -> axum::Router {
        axum::Router::new().route(
            "/mcp",
            axum::routing::post(
                move |axum::Json(body): axum::Json<serde_json::Value>| async move {
                    use axum::response::IntoResponse;
                    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    match method {
                        "initialize" => {
                            if with_session_header {
                                let mut headers = axum::http::HeaderMap::new();
                                headers.insert(
                                    "mcp-session-id",
                                    axum::http::HeaderValue::from_static("ext-sess"),
                                );
                                (headers, axum::Json(serde_json::json!({}))).into_response()
                            } else {
                                axum::Json(serde_json::json!({})).into_response()
                            }
                        }
                        "notifications/initialized" => {
                            axum::http::StatusCode::ACCEPTED.into_response()
                        }
                        "tools/call" => axum::Json(serde_json::json!({
                            "result": { "content": [{"type": "text", "text": "TOOL_OK"}] }
                        }))
                        .into_response(),
                        _ => axum::http::StatusCode::OK.into_response(),
                    }
                },
            ),
        )
    }

    /// Config whose `session_id` is preset, so `get_or_create_session` returns
    /// immediately without performing the SSE/roots handshake.
    fn cfg_session(base_url: &str) -> McpChatConfig {
        let mut c = empty_mcp_config(base_url);
        c.session_id = Some("preset-sid".to_string());
        c
    }

    // ── call_mcp_sampling_routed ──────────────────────────────────────────────

    #[tokio::test]
    async fn call_mcp_sampling_routed_joins_text_content() {
        let base = mock_post_json(serde_json::json!({
            "result": { "content": [
                {"type": "text", "text": "Hello"},
                {"type": "image", "data": "ignored"},
                {"type": "text", "text": " World"},
            ]}
        }))
        .await;
        let cfg = cfg_session(&base);
        let resp = call_mcp_sampling_routed(
            &cfg,
            "label",
            vec![serde_json::json!({"role": "user", "content": "hi"})],
            None,
        )
        .await
        .expect("sampling should succeed");
        assert_eq!(
            resp.content, "Hello World",
            "text items concatenated, non-text skipped"
        );
        assert!(resp.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn call_mcp_sampling_routed_http_error() {
        let base = mock_post_status(axum::http::StatusCode::INTERNAL_SERVER_ERROR).await;
        let cfg = cfg_session(&base);
        let err = call_mcp_sampling_routed(&cfg, "label", vec![], None)
            .await
            .expect_err("non-2xx must error");
        assert!(err.contains("HTTP 500"), "{err}");
        assert!(err.contains("boom-body"), "body surfaced: {err}");
    }

    #[tokio::test]
    async fn call_mcp_sampling_routed_json_error_field() {
        let base = mock_post_json(serde_json::json!({"error": {"message": "nope"}})).await;
        let cfg = cfg_session(&base);
        let err = call_mcp_sampling_routed(&cfg, "label", vec![], Some("sys"))
            .await
            .expect_err("JSON-RPC error must propagate");
        assert_eq!(err, "nope");
    }

    #[tokio::test]
    async fn call_mcp_sampling_routed_missing_result() {
        let base = mock_post_json(serde_json::json!({})).await;
        let cfg = cfg_session(&base);
        let err = call_mcp_sampling_routed(&cfg, "label", vec![], None)
            .await
            .expect_err("missing result must error");
        assert_eq!(err, "Missing result in response");
    }

    #[tokio::test]
    async fn call_mcp_sampling_routed_missing_content() {
        let base = mock_post_json(serde_json::json!({"result": {}})).await;
        let cfg = cfg_session(&base);
        let err = call_mcp_sampling_routed(&cfg, "label", vec![], None)
            .await
            .expect_err("missing content array must error");
        assert_eq!(err, "Missing or invalid content in result");
    }

    // ── call_mcp_tool_http error & retry paths ────────────────────────────────

    #[tokio::test]
    async fn call_mcp_tool_http_http_error_status() {
        let base = mock_post_status(axum::http::StatusCode::INTERNAL_SERVER_ERROR).await;
        let url = format!("{}/mcp", base);
        let err = call_mcp_tool_http(
            &reqwest::Client::new(),
            &url,
            "sid",
            "t",
            serde_json::json!({}),
        )
        .await
        .expect_err("500 must error");
        assert!(err.contains("HTTP 500"), "{err}");
        assert!(err.contains("boom-body"), "{err}");
    }

    #[tokio::test]
    async fn call_mcp_tool_http_malformed_json() {
        let router =
            axum::Router::new().route("/mcp", axum::routing::post(|| async { "not json at all" }));
        let base = serve_router(router).await;
        let url = format!("{}/mcp", base);
        let err = call_mcp_tool_http(
            &reqwest::Client::new(),
            &url,
            "sid",
            "t",
            serde_json::json!({}),
        )
        .await
        .expect_err("unparseable body must error");
        assert!(err.contains("failed to parse tools/call response"), "{err}");
    }

    #[tokio::test]
    async fn call_mcp_tool_http_retries_on_conflict() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let counter = Arc::new(AtomicU32::new(0));
        let c = counter.clone();
        let router = axum::Router::new().route(
            "/mcp",
            axum::routing::post(move || {
                let c = c.clone();
                async move {
                    use axum::response::IntoResponse;
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        // First attempt: sandbox not yet locked.
                        (
                            axum::http::StatusCode::CONFLICT,
                            axum::Json(serde_json::json!({
                                "jsonrpc": "2.0",
                                "error": { "code": -32001, "message": "Sandbox initializing" }
                            })),
                        )
                            .into_response()
                    } else {
                        axum::Json(serde_json::json!({
                            "result": { "content": [{"type": "text", "text": "retried-ok"}] }
                        }))
                        .into_response()
                    }
                }
            }),
        );
        let base = serve_router(router).await;
        let url = format!("{}/mcp", base);
        let (text, is_err) = call_mcp_tool_http(
            &reqwest::Client::new(),
            &url,
            "sid",
            "t",
            serde_json::json!({}),
        )
        .await
        .expect("retry should eventually succeed");
        assert!(!is_err);
        assert_eq!(text, "retried-ok");
        assert!(
            counter.load(Ordering::SeqCst) >= 2,
            "must have retried after 409"
        );
    }

    // ── dispatch_tool_execution (local / external / missing-header) ────────────

    #[tokio::test]
    async fn dispatch_local_tool_success() {
        let base = serve_router(mcp_tool_router(true)).await;
        let cfg = cfg_session(&base);
        let (text, failed) = dispatch_tool_execution("status", serde_json::json!({}), &cfg)
            .await
            .expect("local dispatch succeeds");
        assert!(!failed);
        assert_eq!(text, "TOOL_OK");
    }

    #[tokio::test]
    async fn dispatch_external_http_tool_success() {
        let base = serve_router(mcp_tool_router(true)).await;
        let mut cfg = empty_mcp_config("http://127.0.0.1:9");
        cfg.external_http_servers.insert("ext".to_string(), base);
        let (text, failed) = dispatch_tool_execution("ext::do", serde_json::json!({}), &cfg)
            .await
            .expect("external dispatch succeeds");
        assert!(!failed);
        assert_eq!(text, "TOOL_OK");
    }

    #[tokio::test]
    async fn dispatch_external_missing_session_header_errors() {
        let base = serve_router(mcp_tool_router(false)).await;
        let mut cfg = empty_mcp_config("http://127.0.0.1:9");
        cfg.external_http_servers.insert("ext".to_string(), base);
        let err = dispatch_tool_execution("ext::do", serde_json::json!({}), &cfg)
            .await
            .expect_err("missing session header must error");
        // The error must identify the *external* handshake and stay
        // diagnostic (missing session header), matching the shared client's
        // message wrapped by `get_or_create_external_session`.
        assert!(
            err.contains("Failed to initialize external session"),
            "{err}"
        );
        assert!(err.contains("No mcp-session-id header"), "{err}");
    }

    #[tokio::test]
    async fn dispatch_external_missing_session_header_surfaces_status_and_body() {
        // When the initialize reply has no session header, the diagnostic must
        // expose the real HTTP status and body instead of the bare, opaque
        // "missing header" string that made the agent loop retry blindly.
        let base = mock_post_status(axum::http::StatusCode::FORBIDDEN).await;
        let mut cfg = empty_mcp_config("http://127.0.0.1:9");
        cfg.external_http_servers.insert("ext".to_string(), base);
        let err = dispatch_tool_execution("ext::do", serde_json::json!({}), &cfg)
            .await
            .expect_err("missing session header must error");
        assert!(err.contains("HTTP 403"), "status missing from: {err}");
        assert!(err.contains("boom-body"), "body missing from: {err}");
    }

    // ── spawn_chat_task (mcp:// routing) ───────────────────────────────────────

    #[tokio::test]
    async fn spawn_chat_task_mcp_without_config_errors() {
        let client = LlmClient::new("mcp://lbl", "m", None);
        let (tx, mut rx) = mpsc::channel(8);
        spawn_chat_task(client, vec![ChatMessage::user("hi")], None, None, tx);
        match rx.recv().await.unwrap() {
            AgentEvent::Error(e) => assert!(e.contains("MCP config missing"), "{e}"),
            o => panic!("unexpected {o:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_chat_task_mcp_routes_and_completes() {
        let base = mock_post_json(serde_json::json!({
            "result": { "content": [{"type": "text", "text": "routed-reply"}] }
        }))
        .await;
        let cfg = cfg_session(&base);
        let client = LlmClient::new("mcp://lbl", "m", None);
        let (tx, mut rx) = mpsc::channel(8);
        spawn_chat_task(
            client,
            vec![ChatMessage::user("q")],
            Some("sys".to_string()),
            Some(cfg),
            tx,
        );
        match rx.recv().await.unwrap() {
            AgentEvent::Token(t) => assert_eq!(t, "routed-reply"),
            o => panic!("unexpected {o:?}"),
        }
        match rx.recv().await.unwrap() {
            AgentEvent::Done => {}
            o => panic!("unexpected {o:?}"),
        }
    }

    // ── get_or_create_session reuse fast-path ─────────────────────────────────

    #[tokio::test]
    async fn get_or_create_session_reuses_existing_id() {
        let mut cfg = empty_mcp_config("http://127.0.0.1:9");
        cfg.session_id = Some("already-have".to_string());
        // No server is contacted because the id is preset and non-empty.
        let sid = get_or_create_session(&reqwest::Client::new(), "http://127.0.0.1:9/mcp", &cfg)
            .await
            .expect("preset session id is returned verbatim");
        assert_eq!(sid, "already-have");
    }

    // ── HubApprovalGate ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn hub_approval_gate_returns_false_when_hub_closed() {
        let (hub_tx, hub_rx) = mpsc::channel(1);
        drop(hub_rx); // hub side gone → send fails → denied.
        let session = Arc::new(tokio::sync::Mutex::new(
            ahma_mcp::ActiveAgentSession::default(),
        ));
        let gate = HubApprovalGate {
            hub_tx,
            session,
            approval_mutex: tokio::sync::Mutex::new(()),
        };
        assert!(!gate.request_approval("id", "write_file", "{}").await);
    }

    #[tokio::test]
    async fn hub_approval_gate_resolves_with_session_decision() {
        let (hub_tx, mut hub_rx) = mpsc::channel(8);
        let session = Arc::new(tokio::sync::Mutex::new(
            ahma_mcp::ActiveAgentSession::default(),
        ));
        let gate = HubApprovalGate {
            hub_tx,
            session: session.clone(),
            approval_mutex: tokio::sync::Mutex::new(()),
        };
        let handle = tokio::spawn(async move {
            gate.request_approval("id7", "write_file", "{\"p\":1}")
                .await
        });

        // The gate emits an approval request before awaiting the decision.
        match hub_rx.recv().await.unwrap() {
            ClientMsg::ApprovalRequested { id, tool, args } => {
                assert_eq!(id, "id7");
                assert_eq!(tool, "write_file");
                assert_eq!(args, "{\"p\":1}");
            }
            _ => panic!("expected ApprovalRequested"),
        }

        // Answer via the oneshot the gate parked on the session.
        let tx = session
            .lock()
            .await
            .approval_tx
            .take()
            .expect("gate registered an approval_tx");
        tx.send(true).unwrap();

        assert!(handle.await.unwrap(), "decision propagates back to caller");
    }

    // ── execute_single_tool_call (rejection & dispatch-error branches) ─────────

    #[tokio::test]
    async fn execute_single_tool_call_rejection_path() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = empty_mcp_config("http://127.0.0.1:9");
        cfg.workspace_root = tmp.path().to_path_buf(); // fresh dir: no prior approvals
        let call = ahma_llm_monitor::client::ChatToolCall {
            id: "call_x".to_string(),
            name: "write_file".to_string(), // mutating → requires approval
            arguments: serde_json::json!({"path": "a"}),
            arguments_raw: "{}".to_string(),
        };
        let (tx, mut rx) = mpsc::channel(8);
        let (id, name, payload, failed) =
            execute_single_tool_call(call, cfg, tx, Arc::new(RejectGate)).await;
        assert!(failed);
        assert_eq!(id, "call_x");
        assert_eq!(name, "write_file");
        assert!(payload["error"].as_str().unwrap().contains("rejected"));
        match rx.recv().await.unwrap() {
            AgentEvent::ToolCallFinished { result, failed, .. } => {
                assert!(failed);
                assert!(result.contains("rejected"), "{result}");
            }
            o => panic!("unexpected {o:?}"),
        }
    }

    #[tokio::test]
    async fn execute_single_tool_call_dispatch_error_is_reported() {
        // Port 1 refuses connections, so the local tool dispatch fails fast.
        let cfg = empty_mcp_config("http://127.0.0.1:1");
        let call = ahma_llm_monitor::client::ChatToolCall {
            id: "cerr".to_string(),
            name: "status".to_string(), // read-only → auto-approved
            arguments: serde_json::json!({}),
            arguments_raw: "{}".to_string(),
        };
        let (tx, mut rx) = mpsc::channel(8);
        let (id, name, payload, failed) =
            execute_single_tool_call(call, cfg, tx, Arc::new(AutoApproveGate)).await;
        assert!(failed);
        assert_eq!(id, "cerr");
        assert_eq!(name, "status");
        assert!(payload["error"].as_str().unwrap().starts_with("Error:"));

        let mut saw_started = false;
        let mut saw_failed_finish = false;
        while let Ok(evt) = rx.try_recv() {
            match evt {
                AgentEvent::ToolCallStarted { .. } => saw_started = true,
                AgentEvent::ToolCallFinished { failed, .. } if failed => saw_failed_finish = true,
                _ => {}
            }
        }
        assert!(saw_started, "approved tool emits ToolCallStarted");
        assert!(
            saw_failed_finish,
            "dispatch error emits a failed ToolCallFinished"
        );
    }

    // ── execute_agent_turn branches ───────────────────────────────────────────

    #[tokio::test]
    async fn execute_agent_turn_mcp_content_completes() {
        let base = mock_post_json(serde_json::json!({
            "result": { "content": [{"type": "text", "text": "final answer"}] }
        }))
        .await;
        let cfg = cfg_session(&base);
        let client = LlmClient::new("mcp://lbl", "m", None);
        let (tx, mut rx) = mpsc::channel(16);
        let mut msg_json = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let mut read_hinted = false;
        let mut error_hinted = false;
        let mut last_prompt_tokens: u32 = 0;

        let cont = execute_agent_turn(
            &client,
            &mut msg_json,
            &[],
            &Some(cfg),
            &tx,
            &[],
            &None,
            &mut read_hinted,
            &mut error_hinted,
            Arc::new(AutoApproveGate),
            &mut last_prompt_tokens,
        )
        .await;

        assert!(!cont, "no tool calls → turn signals completion");
        let last = msg_json.last().unwrap();
        assert_eq!(last["role"], "assistant");
        assert_eq!(last["content"], "final answer");
        match rx.recv().await.unwrap() {
            AgentEvent::Token(t) => assert_eq!(t, "final answer"),
            o => panic!("unexpected {o:?}"),
        }
        match rx.recv().await.unwrap() {
            AgentEvent::Done => {}
            o => panic!("unexpected {o:?}"),
        }
    }

    #[tokio::test]
    async fn execute_agent_turn_tools_without_mcp_errors() {
        // The tool-calling path streams now, so serve an SSE body that carries a
        // tool call (split id/name vs arguments across chunks, like real servers).
        let router = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(|| async {
                let body = concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"t\"}}]}}]}\n",
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{}\"}}]}}]}\n",
                    "data: [DONE]\n",
                );
                axum::response::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(body.to_string())
                    .unwrap()
            }),
        );
        let base = serve_router(router).await;
        let client = LlmClient::new(base, "m", None);
        let (tx, mut rx) = mpsc::channel(16);
        let mut msg_json = vec![serde_json::json!({"role": "user", "content": "go"})];
        let mut read_hinted = false;
        let mut error_hinted = false;
        let mut last_prompt_tokens: u32 = 0;

        let cont = execute_agent_turn(
            &client,
            &mut msg_json,
            &[],
            &None, // model wants tools but MCP is not configured
            &tx,
            &[],
            &None,
            &mut read_hinted,
            &mut error_hinted,
            Arc::new(AutoApproveGate),
            &mut last_prompt_tokens,
        )
        .await;

        assert!(!cont);
        match rx.recv().await.unwrap() {
            AgentEvent::Error(e) => assert!(e.contains("MCP is not configured"), "{e}"),
            o => panic!("unexpected {o:?}"),
        }
    }

    #[tokio::test]
    async fn execute_agent_turn_length_truncated_requests_continuation() {
        // The classic small-model failure this fix targets: the model's
        // response is cut off by a length/context limit (finish_reason ==
        // "length"), with no tool calls. The turn must not treat that as a
        // final answer — it should surface it loudly and signal "loop again"
        // so the caller's existing turn loop drives one continuation.
        let router = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(|| async {
                let body = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"partial thin\"}}]}\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n",
                    "data: [DONE]\n",
                );
                axum::response::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(body.to_string())
                    .unwrap()
            }),
        );
        let base = serve_router(router).await;
        let client = LlmClient::new(base, "m", None);
        let (tx, mut rx) = mpsc::channel(16);
        let mut msg_json = vec![serde_json::json!({"role": "user", "content": "go"})];
        let mut read_hinted = false;
        let mut error_hinted = false;
        let mut last_prompt_tokens: u32 = 0;

        let cont = execute_agent_turn(
            &client,
            &mut msg_json,
            &[],
            &None,
            &tx,
            &[],
            &None,
            &mut read_hinted,
            &mut error_hinted,
            Arc::new(AutoApproveGate),
            &mut last_prompt_tokens,
        )
        .await;

        assert!(cont, "a truncated response must signal 'loop again'");
        // The partial content was recorded as an assistant turn, followed by a
        // continuation request — never silently dropped or lost.
        assert_eq!(msg_json.len(), 3);
        assert_eq!(msg_json[1]["role"], "assistant");
        assert_eq!(msg_json[1]["content"], "partial thin");
        assert_eq!(msg_json[2]["role"], "user");
        assert!(
            msg_json[2]["content"].as_str().unwrap().contains("cut off"),
            "{msg_json:?}"
        );

        let mut saw_truncated = false;
        let mut saw_partial_token = false;
        while let Ok(evt) = rx.try_recv() {
            match evt {
                AgentEvent::Truncated { reason } => {
                    saw_truncated = true;
                    assert!(reason.contains("length"), "{reason}");
                }
                AgentEvent::Token(t) if t == "partial thin" => saw_partial_token = true,
                _ => {}
            }
        }
        assert!(saw_truncated, "truncation must be surfaced, never silent");
        assert!(
            saw_partial_token,
            "streamed content was already emitted during streaming"
        );
    }

    #[tokio::test]
    async fn execute_agent_turn_truncated_with_tool_calls_proceeds_with_warning_not_silent_loss() {
        // Rarer: cut off mid tool-call. Not auto-recoverable the same way, but
        // must still proceed with whatever was parsed rather than silently
        // discarding it — existing tool-call handling takes over normally.
        let router = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(|| async {
                let body = concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"t\",\"arguments\":\"{}\"}}]}}]}\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n",
                    "data: [DONE]\n",
                );
                axum::response::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(body.to_string())
                    .unwrap()
            }),
        );
        let base = serve_router(router).await;
        let client = LlmClient::new(base, "m", None);
        let (tx, mut rx) = mpsc::channel(16);
        let mut msg_json = vec![serde_json::json!({"role": "user", "content": "go"})];
        let mut read_hinted = false;
        let mut error_hinted = false;
        let mut last_prompt_tokens: u32 = 0;

        let cont = execute_agent_turn(
            &client,
            &mut msg_json,
            &[],
            &None, // MCP not configured → tool dispatch reports an error, but the
            // turn must still proceed through the tool-call path, not the
            // truncation-continuation path.
            &tx,
            &[],
            &None,
            &mut read_hinted,
            &mut error_hinted,
            Arc::new(AutoApproveGate),
            &mut last_prompt_tokens,
        )
        .await;

        assert!(!cont, "tool-call path took over, not the continuation path");
        let mut saw_mcp_not_configured_error = false;
        while let Ok(evt) = rx.try_recv() {
            if let AgentEvent::Error(e) = evt
                && e.contains("MCP is not configured")
            {
                saw_mcp_not_configured_error = true;
            }
        }
        assert!(saw_mcp_not_configured_error, "{msg_json:?}");
    }

    // ── maybe_compact_conversation ────────────────────────────────────────────

    /// A mock `/chat/completions` endpoint that always answers with a fixed
    /// non-streaming JSON completion, recording how many times it was called.
    async fn mock_chat_completions_json(
        content: &str,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> String {
        let content = content.to_string();
        let router = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(move || {
                let content = content.clone();
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    axum::Json(serde_json::json!({
                        "choices": [{"message": {"role": "assistant", "content": content}}]
                    }))
                }
            }),
        );
        serve_router(router).await
    }

    /// A long-ish conversation: system + pinned goal, then several
    /// user/assistant pairs — enough for compaction to have real middle
    /// content to work with once the head and recent tail are excluded.
    fn long_conversation() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({"role": "system", "content": "sys"}),
            serde_json::json!({"role": "user", "content": "the original goal"}),
            serde_json::json!({"role": "user", "content": "step 1"}),
            serde_json::json!({"role": "assistant", "content": "did step 1"}),
            serde_json::json!({"role": "user", "content": "step 2"}),
            serde_json::json!({"role": "assistant", "content": "did step 2"}),
            serde_json::json!({"role": "user", "content": "step 3"}),
            serde_json::json!({"role": "assistant", "content": "did step 3"}),
            serde_json::json!({"role": "user", "content": "step 4 (recent)"}),
            serde_json::json!({"role": "assistant", "content": "did step 4 (recent)"}),
        ]
    }

    #[tokio::test]
    async fn maybe_compact_conversation_below_threshold_is_a_noop() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let base = mock_chat_completions_json("MUST NOT BE USED", calls.clone()).await;
        let client = LlmClient::new(&base, "m", None);
        let cfg = McpChatConfig {
            context_length: Some(10_000),
            small_model_harness: true, // threshold 0.70
            ..empty_mcp_config(&base)
        };
        let mut msg_json = long_conversation();
        let before = msg_json.clone();

        // 1_000 / 10_000 = 10% full, well under the 70% threshold.
        maybe_compact_conversation(&client, &mut msg_json, &cfg, 1_000).await;

        assert_eq!(msg_json, before, "must not touch history below threshold");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "must not call the LLM at all below threshold"
        );
    }

    #[tokio::test]
    async fn maybe_compact_conversation_zero_usage_is_treated_as_unknown() {
        // No usage reported yet (0) must not be read as "0% full → always
        // trigger" — it means "we don't know yet", so skip.
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let base = mock_chat_completions_json("MUST NOT BE USED", calls.clone()).await;
        let client = LlmClient::new(&base, "m", None);
        let cfg = McpChatConfig {
            context_length: Some(10_000),
            small_model_harness: true,
            ..empty_mcp_config(&base)
        };
        let mut msg_json = long_conversation();
        let before = msg_json.clone();

        maybe_compact_conversation(&client, &mut msg_json, &cfg, 0).await;

        assert_eq!(msg_json, before);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn maybe_compact_conversation_unknown_context_length_is_a_noop() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let base = mock_chat_completions_json("MUST NOT BE USED", calls.clone()).await;
        let client = LlmClient::new(&base, "m", None);
        let cfg = McpChatConfig {
            context_length: None, // unknown window → no denominator, no trigger
            small_model_harness: true,
            ..empty_mcp_config(&base)
        };
        let mut msg_json = long_conversation();
        let before = msg_json.clone();

        maybe_compact_conversation(&client, &mut msg_json, &cfg, 999_999).await;

        assert_eq!(msg_json, before);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn maybe_compact_conversation_above_threshold_summarizes_and_preserves_head_and_tail() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let base = mock_chat_completions_json(
            "Goal: ship the feature\nPending todos: none",
            calls.clone(),
        )
        .await;
        let client = LlmClient::new(&base, "m", None);
        let cfg = McpChatConfig {
            context_length: Some(10_000),
            small_model_harness: true, // threshold 0.70
            ..empty_mcp_config(&base)
        };
        let mut msg_json = long_conversation();
        let original_len = msg_json.len();

        // 8_000 / 10_000 = 80% full, over the 70% threshold.
        maybe_compact_conversation(&client, &mut msg_json, &cfg, 8_000).await;

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the isolated compaction call must have been made exactly once"
        );
        assert!(
            msg_json.len() < original_len,
            "history must have shrunk: {msg_json:?}"
        );
        // System prompt and pinned goal survive untouched.
        assert_eq!(msg_json[0]["role"], "system");
        assert_eq!(msg_json[0]["content"], "sys");
        assert_eq!(msg_json[1]["role"], "user");
        assert_eq!(msg_json[1]["content"], "the original goal");
        // The most recent messages survive untouched, verbatim, in order.
        let tail: Vec<_> = msg_json[msg_json.len() - COMPACTION_KEEP_RECENT_MESSAGES..].to_vec();
        let expected_tail =
            &long_conversation()[long_conversation().len() - COMPACTION_KEEP_RECENT_MESSAGES..];
        assert_eq!(tail, expected_tail);
        // The compacted middle became one summary message carrying the
        // mock's returned content.
        let summary = &msg_json[2];
        assert_eq!(summary["role"], "user");
        assert!(
            summary["content"]
                .as_str()
                .unwrap()
                .contains("ship the feature"),
            "{summary:?}"
        );
    }

    #[tokio::test]
    async fn maybe_compact_conversation_llm_failure_leaves_history_untouched() {
        let router = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(|| async {
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom")
            }),
        );
        let base = serve_router(router).await;
        let client = LlmClient::new(&base, "m", None);
        let cfg = McpChatConfig {
            context_length: Some(10_000),
            small_model_harness: true,
            ..empty_mcp_config(&base)
        };
        let mut msg_json = long_conversation();
        let before = msg_json.clone();

        // Must not panic, and must leave history exactly as it was —
        // trim_conversation (called right after, by the real turn loop)
        // remains the backstop.
        maybe_compact_conversation(&client, &mut msg_json, &cfg, 8_000).await;

        assert_eq!(
            msg_json, before,
            "a failed compaction call must not corrupt history"
        );
    }

    #[tokio::test]
    async fn maybe_compact_conversation_too_short_to_compact_is_a_noop() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let base = mock_chat_completions_json("MUST NOT BE USED", calls.clone()).await;
        let client = LlmClient::new(&base, "m", None);
        let cfg = McpChatConfig {
            context_length: Some(10_000),
            small_model_harness: true,
            ..empty_mcp_config(&base)
        };
        // Only the protected head plus a couple of recent messages — nothing
        // left in the "middle" once both are excluded.
        let mut msg_json = vec![
            serde_json::json!({"role": "system", "content": "sys"}),
            serde_json::json!({"role": "user", "content": "goal"}),
            serde_json::json!({"role": "assistant", "content": "a"}),
        ];
        let before = msg_json.clone();

        maybe_compact_conversation(&client, &mut msg_json, &cfg, 8_000).await;

        assert_eq!(msg_json, before);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn compaction_threshold_is_none_without_a_known_context_length() {
        let strategy = BudgetStrategy {
            context_length: None,
            small_model_harness: true,
            minimize_tokens: false,
        };
        assert_eq!(strategy.compaction_threshold(), None);
    }

    #[test]
    fn compaction_threshold_is_tighter_for_small_models() {
        let small = BudgetStrategy {
            context_length: Some(8_192),
            small_model_harness: true,
            minimize_tokens: false,
        };
        let normal = BudgetStrategy {
            context_length: Some(8_192),
            small_model_harness: false,
            minimize_tokens: false,
        };
        assert_eq!(
            small.compaction_threshold(),
            Some(SMALL_MODEL_COMPACTION_THRESHOLD)
        );
        assert_eq!(
            normal.compaction_threshold(),
            Some(DEFAULT_COMPACTION_THRESHOLD)
        );
        assert!(small.compaction_threshold() < normal.compaction_threshold());
    }

    // ── fetch_completion branches ─────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_completion_mcp_missing_config_errors() {
        let client = LlmClient::new("mcp://x", "m", None);
        let (tx, mut rx) = mpsc::channel(8);
        let res = fetch_completion(&client, &[], &[], &None, &tx, &[], &None).await;
        assert!(res.is_none());
        match rx.recv().await.unwrap() {
            AgentEvent::Error(e) => assert!(e.contains("MCP config missing"), "{e}"),
            o => panic!("unexpected {o:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_completion_mcp_routes_completion() {
        let base = mock_post_json(serde_json::json!({
            "result": { "content": [{"type": "text", "text": "ROUTED"}] }
        }))
        .await;
        let cfg = cfg_session(&base);
        let client = LlmClient::new("mcp://lbl", "m", None);
        let (tx, _rx) = mpsc::channel(8);
        let msgs = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let res = fetch_completion(
            &client,
            &msgs,
            &[],
            &Some(cfg),
            &tx,
            &[],
            &Some("sys".to_string()),
        )
        .await;
        let (resp, content_streamed) = res.expect("mcp routing returns a completion");
        assert_eq!(resp.content, "ROUTED");
        assert!(
            !content_streamed,
            "MCP sampling is not streamed; caller must emit the content"
        );
    }

    #[tokio::test]
    async fn fetch_completion_tool_unsupported_triggers_fallback() {
        let router = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    "tools are not supported by this model",
                )
            }),
        );
        let base = serve_router(router).await;
        let client = LlmClient::new(base, "m", None);
        let (tx, mut rx) = mpsc::channel(16);
        let msgs = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let res = fetch_completion(
            &client,
            &msgs,
            &[],
            &None,
            &tx,
            &[ChatMessage::user("hi")],
            &None,
        )
        .await;
        assert!(res.is_none());
        // The fallback emits an explanatory error before re-dispatching as chat.
        match rx.recv().await.unwrap() {
            AgentEvent::Error(e) => assert!(e.contains("does not support tools"), "{e}"),
            o => panic!("unexpected {o:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_completion_other_error_no_fallback() {
        let router = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "server exploded",
                )
            }),
        );
        let base = serve_router(router).await;
        let client = LlmClient::new(base, "m", None);
        let (tx, mut rx) = mpsc::channel(16);
        let msgs = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let res = fetch_completion(
            &client,
            &msgs,
            &[],
            &None,
            &tx,
            &[ChatMessage::user("hi")],
            &None,
        )
        .await;
        assert!(res.is_none());
        match rx.recv().await.unwrap() {
            AgentEvent::Error(e) => {
                assert!(e.contains("500"), "raw error surfaced: {e}");
                assert!(
                    !e.contains("does not support tools"),
                    "no tool fallback for 500"
                );
            }
            o => panic!("unexpected {o:?}"),
        }
    }
}
