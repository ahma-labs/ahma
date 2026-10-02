//! In-process MCP client/server pairs for fast unit-level testing.
//!
//! Bypasses subprocess spawning by connecting `AhmaMcpService` directly to a
//! `RunningService<RoleClient, ()>` via a `tokio::io::duplex` channel.  The full
//! MCP handshake (initialize / initialized) still runs, so the behaviour is
//! identical to the stdio subprocess path – but without the forking overhead.

use crate::adapter::Adapter;
use crate::config::{ToolConfig, load_tool_configs};
use crate::mcp_service::{AhmaMcpService, GuidanceConfig};
use crate::operation_monitor::{MonitorConfig, OperationMonitor};
use crate::sandbox::{Sandbox, SandboxMode};
use crate::shell::cli::AppConfig;
use crate::shell_pool::{ShellPoolConfig, ShellPoolManager};
use anyhow::Result;
use rmcp::{
    ServiceExt,
    handler::client::ClientHandler,
    service::{RoleClient, RoleServer, RunningService},
    transport::async_rw::AsyncRwTransport,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Holds both sides of an in-process MCP connection.
///
/// The server handle must remain alive for the duration of the test so that
/// the background task loop can respond to client requests.  Drop this value
/// (or let it go out of scope) to cleanly shut down the connection.
///
/// `C` is the client-side handler. It defaults to `()` — a client that ignores
/// everything the server pushes — which is what most tests want. Use
/// [`crate::test_utils::recording_client::RecordingClient`] instead when the
/// assertion is about what the *client* received (progress notifications) or
/// about behaviour ahma keys off `clientInfo.name`.
pub struct InProcessMcp<C: ClientHandler = ()> {
    /// The MCP client – use this to call `list_all_tools`, `call_tool`, etc.
    pub client: RunningService<RoleClient, C>,
    /// The inner service implementation on the server side.
    pub service: AhmaMcpService,
    /// Keeps the server background tasks alive.
    pub _server: RunningService<RoleServer, AhmaMcpService>,
}

/// Create an in-process MCP pair using an empty tool config map.
///
/// Suitable for error-handling tests where the specific tool names don't matter.
pub async fn create_in_process_mcp_empty() -> Result<InProcessMcp> {
    create_in_process_mcp(HashMap::new()).await
}

/// Create an in-process MCP pair whose tool list is loaded from `tools_dir`.
pub async fn create_in_process_mcp_from_dir(tools_dir: &Path) -> Result<InProcessMcp> {
    let configs = load_tool_configs(&AppConfig::default(), Some(tools_dir))
        .await
        .unwrap_or_default();
    let mode = if super::client::is_nested_sandbox_environment() {
        SandboxMode::Test
    } else {
        SandboxMode::Strict
    };
    let sandbox = Sandbox::new(
        vec![default_scope_for_tools_dir(tools_dir)?],
        mode,
        false,
        false,
        false,
    )?;
    sandbox.set_roots_received(true);
    let _ = sandbox.commit_existing_scopes();
    wire_in_process_mcp(configs, sandbox).await
}

/// Core constructor: wire `AhmaMcpService` to a client over a duplex channel.
///
/// Both the MCP initialize/initialized handshake and any subsequent requests
/// go through the in-memory channel – no subprocess is spawned.
pub async fn create_in_process_mcp(configs: HashMap<String, ToolConfig>) -> Result<InProcessMcp> {
    let mode = if super::client::is_nested_sandbox_environment() {
        SandboxMode::Test
    } else {
        SandboxMode::Strict
    };
    let sandbox = Sandbox::new(vec![std::env::current_dir()?], mode, false, false, false)?;
    sandbox.set_roots_received(true);
    let _ = sandbox.commit_existing_scopes();
    wire_in_process_mcp(configs, sandbox).await
}

/// Create an in-process MCP pair with a strict sandbox scoped to `scopes`.
///
/// Unlike [`create_in_process_mcp_from_dir`], this constructor creates a
/// `SandboxMode::Strict` sandbox, so path-security tests that assert sandbox
/// enforcement still work correctly in-process.
///
/// # Why `SandboxMode::Strict` is always used here
///
/// `is_nested_sandbox_environment()` returns `true` on Windows (and macOS inside
/// Cursor / VS Code) because those environments can't run the OS-level kernel
/// sandbox (AppContainer / seatbelt).  But OS-level enforcement is a separate
/// concern from application-level path validation: `SandboxMode::Test` disables
/// *both*, which would cause `validate_path` to skip scope checks entirely and
/// make security tests vacuous.
///
/// Using `SandboxMode::Strict` here enforces path validation without requiring
/// any platform sandbox.  `Sandbox::new` with a real directory never fails on
/// any platform; `create_command` on Windows falls back to a plain Job-Object
/// command when AppContainer is not active.
pub async fn create_in_process_mcp_with_scope(
    tools_dir: &Path,
    scopes: Vec<PathBuf>,
) -> Result<InProcessMcp> {
    let configs = load_tool_configs(&AppConfig::default(), Some(tools_dir))
        .await
        .unwrap_or_default();
    // Always Strict: this function exists to test scope-based path security.
    // Do NOT downgrade to SandboxMode::Test based on is_nested_sandbox_environment()
    // — that check governs OS-level kernel enforcement (seatbelt/landlock/AppContainer),
    // not application-level validate_path checks.  SandboxMode::Test bypasses
    // validate_path completely, defeating these tests on Windows CI and macOS+Cursor.
    let sandbox = Sandbox::new(scopes, SandboxMode::Strict, false, false, false)?;
    sandbox.set_roots_received(true);
    let _ = sandbox.commit_existing_scopes();
    wire_in_process_mcp(configs, sandbox).await
}

/// Create an in-process pair whose *client* is a custom [`ClientHandler`].
///
/// Use this when the assertion is about what the server pushed to the client
/// (`notifications/progress`) or about behaviour ahma derives from
/// `clientInfo.name`. `scopes` is passed to the sandbox verbatim: pass an empty
/// vector to get a server whose scope is not yet settled, which is how the
/// `tools/call` gate (SPEC R5.1.2) becomes observable.
///
/// Unlike the `()`-client constructors this does **not** commit the scope when
/// `scopes` is empty — an uncommitted scope is precisely the state those tests
/// need, because the `tools/call` gate keys on the commit latch.
pub async fn create_in_process_mcp_with_client<C: ClientHandler>(
    client: C,
    configs: HashMap<String, ToolConfig>,
    scopes: Vec<PathBuf>,
) -> Result<InProcessMcp<C>> {
    let roots_settled = !scopes.is_empty();
    // Empty scopes must stay `Strict`: `SandboxMode::Test` reports
    // `is_ready_for_tool_calls() == true` regardless of scope, which is exactly
    // the state the gate test needs to be false. With real scopes, mirror the
    // other constructors and relax for nested sandboxes so commands can run.
    let mode = if roots_settled && super::client::is_nested_sandbox_environment() {
        SandboxMode::Test
    } else {
        SandboxMode::Strict
    };
    let sandbox = Sandbox::new(scopes, mode, false, false, false)?;
    sandbox.set_roots_received(roots_settled);
    if roots_settled {
        let _ = sandbox.commit_existing_scopes();
    }
    wire_in_process_mcp_with_client(client, configs, sandbox).await
}

/// An in-process pair whose scope-grant notifier is the real
/// [`PermissionBroker`](crate::sandbox::PermissionBroker), wired exactly as
/// `ahma serve` wires it: rung 1 is a
/// [`PeerElicitationSurface`](crate::sandbox::PeerElicitationSurface) over the
/// service's own peer slot, the live sandbox is installed so an approval is
/// applied and stamped with its workspace, and there is no hub (rung 2) unless
/// a test supplies one.
///
/// Use it when the assertion is about what the human is *shown* or what the
/// agent is *told* after a grant question — the seam fakes cannot see, because
/// a fake notifier is handed whatever context its test chose to give it.
pub async fn create_in_process_mcp_with_broker<C: ClientHandler>(
    client: C,
    scope: &Path,
    hub_tx: Option<tokio::sync::mpsc::UnboundedSender<ahma_common::scope_grant::ScopeGrantRequest>>,
) -> Result<(InProcessMcp<C>, Arc<crate::sandbox::PermissionBroker>)> {
    let sandbox = Sandbox::new(
        vec![scope.to_path_buf()],
        SandboxMode::Test,
        false,
        false,
        false,
    )?;
    sandbox.set_roots_received(true);
    let _ = sandbox.commit_existing_scopes();
    let sandbox = Arc::new(sandbox);

    let broker = Arc::new(crate::sandbox::PermissionBroker::new(
        Arc::new(ahma_common::scope_grant::GrantCoordinator::new()),
        hub_tx,
    ));
    broker.set_sandbox(sandbox.clone());

    let monitor_config = MonitorConfig::with_timeout(std::time::Duration::from_secs(300));
    let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let adapter = Arc::new(
        Adapter::new(Arc::clone(&operation_monitor), shell_pool, sandbox)?
            .with_scope_grant_notifier(broker.clone()),
    );
    let service = AhmaMcpService::new(
        adapter,
        operation_monitor,
        Arc::new(HashMap::new()),
        Arc::new(None::<GuidanceConfig>),
        false, // force_synchronous
        false, // defer_sandbox
    )
    .await?;
    broker.set_elicitation_surface(Arc::new(crate::sandbox::PeerElicitationSurface::new(
        service.peer.clone(),
    )));
    *service.grant_coordinator.write() = Some(broker.coordinator().clone());

    let mcp = serve_in_process(client, service).await?;
    // The peer slot (rung 1's only way to the client) and the client identity
    // (the prompt's "who is asking") are both recorded when the server handles
    // `notifications/initialized`, which can still be in flight when the
    // handshake returns. Wait for it, so a test that calls a handler directly
    // sees what production sees after its first request.
    let peer = mcp.service.peer.clone();
    let ready = super::concurrency::wait_for_condition(
        ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Handshake),
        ahma_common::timeouts::TestTimeouts::poll_interval(),
        || {
            let peer = peer.clone();
            async move { peer.read().is_some() }
        },
    )
    .await;
    anyhow::ensure!(
        ready,
        "the server never recorded its MCP peer after initialize"
    );
    Ok((mcp, broker))
}

/// Internal: wire a pre-built `Sandbox` and tool configs into an in-process pair.
async fn wire_in_process_mcp(
    configs: HashMap<String, ToolConfig>,
    sandbox: Sandbox,
) -> Result<InProcessMcp> {
    wire_in_process_mcp_with_client((), configs, sandbox).await
}

/// An in-process pair with the workspace write queue on (SPEC R2.7): its
/// rendezvous files go under `lock_dir` (so a test never touches the user's
/// runtime directory), the scope is `scope` (Test-mode sandbox — no kernel
/// enforcement, so there is no read-only lane and every command is exclusive),
/// and `app_config` is installed as the service's configuration — which is how a
/// test picks the execution mode and shrinks the inline window
/// (`request_budget_override_secs`).
pub async fn create_in_process_mcp_with_workspace_queue(
    scope: &Path,
    lock_dir: PathBuf,
    app_config: AppConfig,
) -> Result<InProcessMcp> {
    let sandbox = Sandbox::new(
        vec![scope.to_path_buf()],
        SandboxMode::Test,
        false,
        false,
        false,
    )?;
    sandbox.set_roots_received(true);
    let _ = sandbox.commit_existing_scopes();
    let mcp = wire_in_process_mcp_with_queue(
        (),
        HashMap::new(),
        sandbox,
        crate::adapter::workspace_queue::WorkspaceQueue::with_lock_dir(true, Some(lock_dir)),
    )
    .await?;
    mcp.service.set_app_config(Arc::new(app_config));
    Ok(mcp)
}

/// Internal: as [`wire_in_process_mcp`], with a caller-supplied client handler.
async fn wire_in_process_mcp_with_client<C: ClientHandler>(
    client_handler: C,
    configs: HashMap<String, ToolConfig>,
    sandbox: Sandbox,
) -> Result<InProcessMcp<C>> {
    wire_in_process_mcp_with_queue(
        client_handler,
        configs,
        sandbox,
        crate::adapter::workspace_queue::WorkspaceQueue::disabled(),
    )
    .await
}

/// Internal: the one constructor every helper funnels into.
async fn wire_in_process_mcp_with_queue<C: ClientHandler>(
    client_handler: C,
    configs: HashMap<String, ToolConfig>,
    sandbox: Sandbox,
    queue: crate::adapter::workspace_queue::WorkspaceQueue,
) -> Result<InProcessMcp<C>> {
    let monitor_config = MonitorConfig::with_timeout(std::time::Duration::from_secs(300));
    let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let adapter = Arc::new(
        Adapter::new(
            Arc::clone(&operation_monitor),
            shell_pool,
            Arc::new(sandbox),
        )?
        .with_workspace_queue(queue),
    );

    // NOTE: `set_roots_received` is the *caller's* decision, made on the sandbox
    // before it is handed here. It used to be forced to `true` at this point,
    // which silently made it impossible to build an in-process server whose
    // scope is not yet settled — and therefore impossible to observe the
    // `tools/call` gate (SPEC R5.1.2) in-process at all.
    let service = AhmaMcpService::new(
        adapter,
        operation_monitor,
        Arc::new(configs),
        Arc::new(None::<GuidanceConfig>),
        false, // force_synchronous
        false, // defer_sandbox
    )
    .await?;

    serve_in_process(client_handler, service).await
}

/// Internal: run the initialize handshake for `service` against
/// `client_handler` over an in-memory duplex channel.
async fn serve_in_process<C: ClientHandler>(
    client_handler: C,
    service: AhmaMcpService,
) -> Result<InProcessMcp<C>> {
    // Wire client and server through an in-memory duplex channel.
    let (client_stream, server_stream) = tokio::io::duplex(65536);
    let (client_read, client_write) = tokio::io::split(client_stream);
    let (server_read, server_write) = tokio::io::split(server_stream);

    let client_transport = AsyncRwTransport::new_client(client_read, client_write);
    let server_transport = AsyncRwTransport::new_server(server_read, server_write);

    // Run both handshakes concurrently; both futures complete once the
    // initialize / initialized exchange is done and both sides are ready.
    let (client_result, server_result) = tokio::join!(
        client_handler.serve(client_transport),
        service.clone().serve(server_transport),
    );

    Ok(InProcessMcp {
        client: client_result?,
        service,
        _server: server_result?,
    })
}

fn default_scope_for_tools_dir(tools_dir: &Path) -> Result<PathBuf> {
    if let Some(parent) = tools_dir.parent()
        && !parent.as_os_str().is_empty()
    {
        return Ok(parent.to_path_buf());
    }

    Ok(std::env::current_dir()?)
}

// ─── Convenience factories for unit tests ────────────────────────────────────

/// Build a bare [`AhmaMcpService`] in a fresh `TempDir` for unit tests.
///
/// This is the canonical factory for tests that need direct access to an
/// `AhmaMcpService` without the MCP wire protocol.  The sandbox is set to
/// `SandboxMode::Test` (no OS-level enforcement) so the test can run inside
/// nested sandboxes (Cursor, VS Code, Docker) without special setup.
///
/// The returned `TempDir` **must** be kept alive for the duration of the test;
/// dropping it removes the sandbox scope directory which the service still
/// references.
///
/// For tests that require the full in-process MCP wire protocol (initialize /
/// initialized handshake + tool calls), use [`create_in_process_mcp_empty`]
/// instead.
pub async fn build_test_service() -> Result<(AhmaMcpService, tempfile::TempDir)> {
    build_test_service_with_configs(HashMap::new()).await
}

/// Like [`build_test_service`] but pre-loads `configs` into the service.
///
/// Use this when a test specifically needs tools to be registered in the
/// service (e.g., to assert on `service.configs` or exercise tool-dispatch
/// logic in unit tests).
pub async fn build_test_service_with_configs(
    configs: HashMap<String, ToolConfig>,
) -> Result<(AhmaMcpService, tempfile::TempDir)> {
    build_test_service_inner(configs).await
}

async fn build_test_service_inner(
    configs: HashMap<String, ToolConfig>,
) -> Result<(AhmaMcpService, tempfile::TempDir)> {
    let temp_dir = tempfile::tempdir()?;

    let monitor_config = MonitorConfig::with_timeout(std::time::Duration::from_secs(300));
    let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let sandbox = Sandbox::new(
        vec![temp_dir.path().to_path_buf()],
        SandboxMode::Test,
        false,
        false,
        false,
    )?;
    sandbox.set_roots_received(true);
    let _ = sandbox.commit_existing_scopes();
    let sandbox = Arc::new(sandbox);
    let adapter = Arc::new(Adapter::new(
        Arc::clone(&operation_monitor),
        shell_pool,
        sandbox,
    )?);

    let service = AhmaMcpService::new(
        adapter,
        operation_monitor,
        Arc::new(configs),
        Arc::new(None::<GuidanceConfig>),
        false, // force_synchronous
        false, // defer_sandbox
    )
    .await?;

    Ok((service, temp_dir))
}
