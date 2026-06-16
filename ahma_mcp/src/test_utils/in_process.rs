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
pub struct InProcessMcp {
    /// The MCP client – use this to call `list_all_tools`, `call_tool`, etc.
    pub client: RunningService<RoleClient, ()>,
    // Keeps the server background tasks alive.
    _server: RunningService<RoleServer, AhmaMcpService>,
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
    wire_in_process_mcp(configs, sandbox).await
}

/// Internal: wire a pre-built `Sandbox` and tool configs into an in-process pair.
async fn wire_in_process_mcp(
    configs: HashMap<String, ToolConfig>,
    sandbox: Sandbox,
) -> Result<InProcessMcp> {
    let monitor_config = MonitorConfig::with_timeout(std::time::Duration::from_secs(300));
    let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let adapter = Arc::new(Adapter::new(
        Arc::clone(&operation_monitor),
        shell_pool,
        Arc::new(sandbox),
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

    // Wire client and server through an in-memory duplex channel.
    let (client_stream, server_stream) = tokio::io::duplex(65536);
    let (client_read, client_write) = tokio::io::split(client_stream);
    let (server_read, server_write) = tokio::io::split(server_stream);

    let client_transport = AsyncRwTransport::new_client(client_read, client_write);
    let server_transport = AsyncRwTransport::new_server(server_read, server_write);

    // Run both handshakes concurrently; both futures complete once the
    // initialize / initialized exchange is done and both sides are ready.
    let (client_result, server_result) =
        tokio::join!(().serve(client_transport), service.serve(server_transport),);

    Ok(InProcessMcp {
        client: client_result?,
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
    let temp_dir = tempfile::tempdir()?;

    let monitor_config = MonitorConfig::with_timeout(std::time::Duration::from_secs(300));
    let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let sandbox = Arc::new(Sandbox::new(
        vec![temp_dir.path().to_path_buf()],
        SandboxMode::Test,
        false,
        false,
        false,
    )?);
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
