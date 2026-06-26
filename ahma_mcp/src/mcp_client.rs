use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpServerKind {
    Http { url: String },
    Stdio { command: String, args: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub enabled: bool,
    pub kind: McpServerKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpClientConfigFile {
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolInfo {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct McpClientHandler {
    pub workspace_root: PathBuf,
}

impl rmcp::ClientHandler for McpClientHandler {
    async fn list_roots(
        &self,
        _context: rmcp::service::RequestContext<rmcp::RoleClient>,
    ) -> Result<rmcp::model::ListRootsResult, rmcp::ErrorData> {
        let uri = format!("file://{}", self.workspace_root.display());
        let root = rmcp::model::Root::new(uri).with_name("workspace");
        Ok(rmcp::model::ListRootsResult::new(vec![root]))
    }
}

pub type StdioClient = Arc<rmcp::service::RunningService<rmcp::RoleClient, McpClientHandler>>;

#[derive(Clone)]
pub struct McpConnectionManager {
    pub servers: Vec<McpServerConfig>,
    pub tools_by_server: BTreeMap<String, Vec<ToolInfo>>,
    pub stdio_clients: Arc<Mutex<BTreeMap<String, StdioClient>>>,
    pub workspace_root: PathBuf,
}

impl Default for McpConnectionManager {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            tools_by_server: BTreeMap::new(),
            stdio_clients: Arc::new(Mutex::new(BTreeMap::new())),
            workspace_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

impl std::fmt::Debug for McpConnectionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnectionManager")
            .field("servers", &self.servers)
            .field("tools_by_server", &self.tools_by_server)
            .field("stdio_clients", &"<stdio clients>")
            .finish()
    }
}

impl McpConnectionManager {
    pub fn load(cwd: &Path) -> Result<Self> {
        let path = config_path(cwd);
        let mut servers = if path.exists() {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            let cfg: McpClientConfigFile = toml::from_str(&content)
                .with_context(|| format!("Failed to parse {}", path.display()))?;
            cfg.servers
        } else {
            Vec::new()
        };

        // Merge in servers discovered from IDE mcp.json configs (Cursor, VS Code, etc.).
        // Entries from the local `.ahma/mcp-clients.toml` take precedence (same name wins).
        let local_names: std::collections::BTreeSet<_> =
            servers.iter().map(|s| s.name.clone()).collect();
        for ide_server in discover_ide_servers() {
            if !local_names.contains(&ide_server.name) {
                servers.push(ide_server);
            }
        }

        Ok(Self {
            servers,
            tools_by_server: BTreeMap::new(),
            stdio_clients: Arc::new(Mutex::new(BTreeMap::new())),
            workspace_root: cwd.to_path_buf(),
        })
    }

    pub fn save(&self, cwd: &Path) -> Result<()> {
        let dir = cwd.join(".ahma");
        if !dir.exists() {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("Failed to create {}", dir.display()))?;
        }
        let path = config_path(cwd);
        let cfg = McpClientConfigFile {
            servers: self.servers.clone(),
        };
        let text = toml::to_string_pretty(&cfg).context("Failed to serialize MCP client config")?;
        std::fs::write(&path, text).with_context(|| format!("Failed to write {}", path.display()))
    }

    pub fn add_server(&mut self, server: McpServerConfig) {
        self.servers.retain(|s| s.name != server.name);
        self.servers.push(server);
        self.servers.sort_by(|a, b| a.name.cmp(&b.name));
    }

    pub fn remove_server(&mut self, name: &str) {
        self.servers.retain(|s| s.name != name);
        self.tools_by_server.remove(name);
    }

    pub fn list_servers(&self) -> Vec<&McpServerConfig> {
        self.servers.iter().collect()
    }

    pub fn aggregate_tool_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (server, tools) in &self.tools_by_server {
            for tool in tools {
                out.push(format!("{server}::{}", tool.name));
            }
        }
        out.sort();
        out
    }

    pub fn aggregate_tools(&self) -> Vec<ToolInfo> {
        let mut out = Vec::new();
        for (server, tools) in &self.tools_by_server {
            for tool in tools {
                let mut t = tool.clone();
                t.name = format!("{server}::{}", tool.name);
                out.push(t);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub async fn refresh_tools(&mut self) {
        self.tools_by_server.clear();
        for server in self.servers.iter().filter(|s| s.enabled).cloned() {
            if let Ok(tools) = self.fetch_server_tools(&server).await {
                self.tools_by_server.insert(server.name.clone(), tools);
            }
        }
    }

    pub fn resolve_tool_name<'a>(&'a self, raw: &'a str) -> Option<(&'a str, &'a str)> {
        raw.split_once("::")
    }

    pub async fn call_tool(&self, full_name: &str, arguments: Value) -> Result<(String, bool)> {
        let Some((server_name, tool_name)) = self.resolve_tool_name(full_name) else {
            return Err(anyhow!(
                "Tool name must include server namespace: <server>::<tool>"
            ));
        };

        let server = self
            .servers
            .iter()
            .find(|s| s.name == server_name)
            .ok_or_else(|| anyhow!("Unknown MCP server: {server_name}"))?;

        match &server.kind {
            McpServerKind::Http { url } => call_mcp_tool_http(url, tool_name, arguments).await,
            McpServerKind::Stdio { command, args } => {
                let client = self
                    .get_or_spawn_stdio_client(server_name, command, args)
                    .await?;
                call_mcp_tool_stdio(client, tool_name, arguments).await
            }
        }
    }

    pub async fn get_or_spawn_stdio_client(
        &self,
        name: &str,
        command: &str,
        args: &[String],
    ) -> Result<StdioClient> {
        let mut clients = self.stdio_clients.lock().await;
        if let Some(client) = clients.get(name) {
            return Ok(client.clone());
        }

        use rmcp::{
            ServiceExt,
            transport::{ConfigureCommandExt, TokioChildProcess},
        };
        use tokio::process::Command;

        let mut cmd = Command::new(command);
        cmd.args(args);
        cmd.kill_on_drop(true);
        // Defense in depth: stamp the spawn-depth backstop so that if a
        // self-referential MCP server ever slips past `is_self_ahma_serve`
        // (e.g. an explicit entry in mcp-clients.toml), the chain self-limits at
        // MAX_SPAWN_DEPTH instead of growing unbounded. Harmless for non-ahma
        // servers, which ignore the variable.
        cmd.env(
            ahma_common::process_guard::SPAWN_DEPTH_ENV,
            ahma_common::process_guard::child_spawn_depth(),
        );

        let handler = McpClientHandler {
            workspace_root: self.workspace_root.clone(),
        };

        let client = handler
            .serve(TokioChildProcess::new(cmd.configure(|_c| {}))?)
            .await
            .context("Failed to start stdio MCP server process")?;

        let client_arc = Arc::new(client);
        clients.insert(name.to_string(), client_arc.clone());
        Ok(client_arc)
    }

    async fn fetch_server_tools(&self, server: &McpServerConfig) -> Result<Vec<ToolInfo>> {
        match &server.kind {
            McpServerKind::Http { url } => {
                let client = reqwest::Client::new();
                let sid = initialize_mcp_session(&client, url).await?;
                let resp = client
                    .post(format!("{}/mcp", url.trim_end_matches('/')))
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json")
                    .header("mcp-session-id", sid)
                    .json(&json!({
                        "jsonrpc":"2.0",
                        "id": 2,
                        "method": "tools/list",
                        "params": {}
                    }))
                    .send()
                    .await
                    .with_context(|| format!("Failed tools/list for {}", server.name))?;

                if !resp.status().is_success() {
                    return Err(anyhow!("tools/list failed for {}", server.name));
                }

                let val = resp
                    .json::<Value>()
                    .await
                    .context("Failed to parse tools/list response")?;

                let mut tools = Vec::new();
                if let Some(arr) = val
                    .get("result")
                    .and_then(|r| r.get("tools"))
                    .and_then(|t| t.as_array())
                {
                    for t in arr {
                        if let Some(name) = t.get("name").and_then(|n| n.as_str()) {
                            let description = t
                                .get("description")
                                .and_then(|d| d.as_str())
                                .map(String::from);
                            let input_schema =
                                t.get("inputSchema").cloned().unwrap_or(serde_json::json!({
                                    "type": "object",
                                    "additionalProperties": true
                                }));
                            tools.push(ToolInfo {
                                name: name.to_string(),
                                description,
                                input_schema,
                            });
                        }
                    }
                }
                Ok(tools)
            }
            McpServerKind::Stdio { command, args } => {
                self.fetch_server_tools_stdio(&server.name, command, args)
                    .await
            }
        }
    }

    async fn fetch_server_tools_stdio(
        &self,
        name: &str,
        command: &str,
        args: &[String],
    ) -> Result<Vec<ToolInfo>> {
        let client = self.get_or_spawn_stdio_client(name, command, args).await?;
        let tools_res = client
            .list_tools(None)
            .await
            .context("Failed tools/list via stdio")?;
        let mut infos = Vec::new();
        for tool in tools_res.tools {
            let input_schema =
                serde_json::to_value(&tool.input_schema).unwrap_or(serde_json::json!({
                    "type": "object",
                    "additionalProperties": true
                }));
            infos.push(ToolInfo {
                name: tool.name.into_owned(),
                description: tool.description.map(|d| d.into_owned()),
                input_schema,
            });
        }
        Ok(infos)
    }
}

fn config_path(cwd: &Path) -> PathBuf {
    cwd.join(".ahma").join("mcp-clients.toml")
}

pub fn discover_ide_servers() -> Vec<McpServerConfig> {
    let mut results = Vec::new();

    let home = match home_dir() {
        Some(h) => h,
        None => return results,
    };

    let cursor_path = home.join(".cursor").join("mcp.json");
    parse_ide_mcp_json(&cursor_path, &mut results);

    #[cfg(target_os = "macos")]
    {
        let vscode_path = home
            .join("Library")
            .join("Application Support")
            .join("Code")
            .join("User")
            .join("mcp.json");
        parse_ide_mcp_json(&vscode_path, &mut results);
    }

    #[cfg(target_os = "linux")]
    {
        let vscode_path = home
            .join(".config")
            .join("Code")
            .join("User")
            .join("mcp.json");
        parse_ide_mcp_json(&vscode_path, &mut results);
    }

    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            let vscode_path = std::path::PathBuf::from(appdata)
                .join("Code")
                .join("User")
                .join("mcp.json");
            parse_ide_mcp_json(&vscode_path, &mut results);
        }
    }

    results
}

fn home_dir() -> Option<PathBuf> {
    #[allow(deprecated)]
    std::env::home_dir()
}

fn parse_ide_mcp_json(path: &Path, out: &mut Vec<McpServerConfig>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let val: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return,
    };

    for key in &["mcpServers", "servers"] {
        if let Some(Value::Object(map)) = val.get(*key) {
            for (name, entry) in map {
                if let Some(server) = ide_entry_to_server(name, entry)
                    && !out.iter().any(|s| &s.name == name)
                {
                    out.push(server);
                }
            }
        }
    }
}

fn ide_entry_to_server(name: &str, entry: &Value) -> Option<McpServerConfig> {
    if let Some(url) = entry.get("url").and_then(|u| u.as_str()) {
        return Some(McpServerConfig {
            name: name.to_string(),
            enabled: true,
            kind: McpServerKind::Http {
                url: url.to_string(),
            },
        });
    }

    let command = entry.get("command").and_then(|c| c.as_str())?;
    let args: Vec<String> = entry
        .get("args")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // CRITICAL: never register ahma's own `serve` command as an "external" MCP
    // server. The IDE mcp.json entry that launched *this* process points right
    // back at `ahma serve …`; treating it as an external server makes every
    // ahma instance spawn another ahma to list its tools, which spawns another,
    // … — an unbounded self-respawn chain that exhausts the process table
    // (observed: hundreds of stuck `ahma serve stdio … --log-monitor`
    // processes). Self-aggregation is meaningless anyway: this server already
    // exposes its own tools directly.
    if is_self_ahma_serve(command, &args) {
        tracing::debug!(
            server = name,
            command,
            "Skipping IDE MCP server entry that points at ahma itself (would self-recurse)"
        );
        return None;
    }

    Some(McpServerConfig {
        name: name.to_string(),
        enabled: true,
        kind: McpServerKind::Stdio {
            command: command.to_string(),
            args,
        },
    })
}

/// True when `command`+`args` would launch ahma's own `serve` mode — i.e. this
/// process's own binary. Matches the bare name `ahma`, an absolute/relative path
/// whose file stem is `ahma`, or the current executable's own file stem, in all
/// cases gated on an `serve` argument so non-server ahma subcommands (which do
/// not loop) are unaffected.
fn is_self_ahma_serve(command: &str, args: &[String]) -> bool {
    let runs_serve = args.iter().any(|a| a == "serve");
    if !runs_serve {
        return false;
    }
    let cmd_stem = Path::new(command)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(command);
    if cmd_stem.eq_ignore_ascii_case("ahma") {
        return true;
    }
    // Also match by the current executable's stem, in case the binary was
    // installed/renamed but the IDE config references it by that path.
    std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(Path::file_stem)
        .and_then(|s| s.to_str())
        .is_some_and(|exe_stem| exe_stem.eq_ignore_ascii_case(cmd_stem))
}

async fn call_mcp_tool_stdio(
    client: StdioClient,
    tool: &str,
    arguments: Value,
) -> Result<(String, bool)> {
    use rmcp::model::CallToolRequestParams;

    let map = if let Value::Object(m) = arguments {
        m
    } else {
        serde_json::Map::new()
    };

    let params = CallToolRequestParams::new(tool.to_string()).with_arguments(map);
    let res = client
        .call_tool(params)
        .await
        .context("Failed tools/call via stdio")?;

    let is_error = res.is_error.unwrap_or(false);
    let mut texts = Vec::new();
    for content in res.content {
        if let Some(txt) = content.as_text() {
            texts.push(txt.text.clone());
        }
    }
    let content_str = if texts.is_empty() {
        "Empty result".to_string()
    } else {
        texts.join("\n")
    };

    Ok((content_str, is_error))
}

async fn initialize_mcp_session(client: &reqwest::Client, base_url: &str) -> Result<String> {
    let url = format!("{}/mcp", base_url.trim_end_matches('/'));
    let init_resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"roots": {"listChanged": false}},
                "clientInfo": {"name": "ahma-mcp-client", "version": env!("CARGO_PKG_VERSION")}
            }
        }))
        .send()
        .await
        .context("Failed initialize request")?;

    let sid = init_resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow!("initialize response missing mcp-session-id"))?
        .to_string();

    let _ = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("mcp-session-id", &sid)
        .json(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}))
        .send()
        .await;

    Ok(sid)
}

async fn call_mcp_tool_http(
    base_url: &str,
    tool: &str,
    arguments: Value,
) -> Result<(String, bool)> {
    let client = reqwest::Client::new();
    let sid = initialize_mcp_session(&client, base_url).await?;
    let url = format!("{}/mcp", base_url.trim_end_matches('/'));

    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("mcp-session-id", &sid)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": tool,
                "arguments": arguments
            }
        }))
        .send()
        .await
        .context("Failed tools/call request")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("tools/call failed: HTTP {status}: {body}"));
    }

    let val = resp
        .json::<Value>()
        .await
        .context("Failed to parse tools/call response")?;

    let failed = val.get("error").is_some()
        || val
            .get("result")
            .and_then(|r| r.get("isError"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false);

    let text = if let Some(err) = val.get("error") {
        err.get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error")
            .to_string()
    } else if let Some(content) = val
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
    {
        content
            .iter()
            .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        serde_json::to_string_pretty(&val).unwrap_or_default()
    };

    Ok((text, failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Self-reference filter (process-table exhaustion regression) ───────────

    #[test]
    fn self_referential_ahma_serve_entry_is_rejected() {
        // The exact shape of the Cursor / Claude Desktop "Ahma" entry that
        // launched this process. Registering it as an external server made every
        // ahma spawn another ahma to list its tools → unbounded self-respawn.
        let entry = serde_json::json!({
            "type": "stdio",
            "command": "ahma",
            "args": ["serve", "stdio", "--tools", "simplify", "--sandbox", "--log-monitor"],
        });
        assert!(
            ide_entry_to_server("Ahma", &entry).is_none(),
            "ahma's own serve command must never be registered as an external MCP server"
        );
    }

    #[test]
    fn ahma_serve_by_absolute_path_is_rejected() {
        let entry = serde_json::json!({
            "command": "/Users/me/.local/bin/ahma",
            "args": ["serve", "stdio"],
        });
        assert!(ide_entry_to_server("Ahma", &entry).is_none());
    }

    #[test]
    fn non_serve_ahma_subcommand_is_allowed() {
        // `ahma` running a non-server subcommand does not loop, so it is a
        // legitimate external server and must NOT be filtered.
        let entry = serde_json::json!({
            "command": "ahma",
            "args": ["some-other-tool"],
        });
        assert!(ide_entry_to_server("ahma-tool", &entry).is_some());
    }

    #[test]
    fn other_mcp_servers_are_preserved() {
        let entry = serde_json::json!({
            "command": "npx",
            "args": ["-y", "@modelcontextprotocol/server-github", "serve"],
        });
        // `serve` appears in args but the command is not ahma → keep it.
        let server = ide_entry_to_server("github", &entry).expect("non-ahma server kept");
        assert_eq!(server.name, "github");
    }

    #[test]
    fn test_mcp_config_serialization() {
        let td = tempfile::tempdir().unwrap();
        let cwd = td.path().to_path_buf();

        let mut manager = McpConnectionManager::default();
        manager.add_server(McpServerConfig {
            name: "test-stdio".to_string(),
            enabled: true,
            kind: McpServerKind::Stdio {
                command: "echo".to_string(),
                args: vec!["hello".to_string()],
            },
        });
        manager.add_server(McpServerConfig {
            name: "test-http".to_string(),
            enabled: false,
            kind: McpServerKind::Http {
                url: "http://localhost:8080".to_string(),
            },
        });

        manager.save(&cwd).unwrap();

        let loaded = McpConnectionManager::load(&cwd).unwrap();

        let http = loaded
            .servers
            .iter()
            .find(|s| s.name == "test-http")
            .expect("test-http not found after load");
        let stdio = loaded
            .servers
            .iter()
            .find(|s| s.name == "test-stdio")
            .expect("test-stdio not found after load");

        assert!(!http.enabled, "test-http should be disabled");
        assert!(stdio.enabled, "test-stdio should be enabled");

        match &stdio.kind {
            McpServerKind::Stdio { command, args } => {
                assert_eq!(command, "echo");
                assert_eq!(args, &vec!["hello".to_string()]);
            }
            _ => panic!("Expected Stdio server"),
        }
    }

    #[test]
    fn test_aggregate_tool_names() {
        let mut manager = McpConnectionManager::default();
        manager.tools_by_server.insert(
            "srv1".to_string(),
            vec![
                ToolInfo {
                    name: "tool_a".to_string(),
                    description: None,
                    input_schema: serde_json::json!({}),
                },
                ToolInfo {
                    name: "tool_b".to_string(),
                    description: None,
                    input_schema: serde_json::json!({}),
                },
            ],
        );
        manager.tools_by_server.insert(
            "srv2".to_string(),
            vec![ToolInfo {
                name: "tool_c".to_string(),
                description: None,
                input_schema: serde_json::json!({}),
            }],
        );

        let tools = manager.aggregate_tool_names();
        assert_eq!(
            tools,
            vec![
                "srv1::tool_a".to_string(),
                "srv1::tool_b".to_string(),
                "srv2::tool_c".to_string(),
            ]
        );
    }

    #[test]
    fn test_resolve_tool_name() {
        let manager = McpConnectionManager::default();
        assert_eq!(
            manager.resolve_tool_name("srv::tool"),
            Some(("srv", "tool"))
        );
        assert_eq!(manager.resolve_tool_name("not_namespaced"), None);
    }

    // ── Env redirection helper (serializes HOME-touching tests) ───────────────

    use std::sync::{LazyLock, Mutex, MutexGuard};
    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Redirects the platform "home" lookup (`std::env::home_dir`) to a temp
    /// directory for the lifetime of the guard, then restores the prior values.
    /// Holds the env mutex so concurrent env-touching tests don't interleave.
    struct HomeGuard {
        _lock: MutexGuard<'static, ()>,
        prev_home: Option<std::ffi::OsString>,
        prev_userprofile: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn set(path: &Path) -> Self {
            let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev_home = std::env::var_os("HOME");
            let prev_userprofile = std::env::var_os("USERPROFILE");
            // SAFETY: env mutation is serialized by ENV_MUTEX held in `lock`.
            unsafe {
                std::env::set_var("HOME", path);
                std::env::set_var("USERPROFILE", path);
            }
            Self {
                _lock: lock,
                prev_home,
                prev_userprofile,
            }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            // SAFETY: still holding ENV_MUTEX via `_lock`.
            unsafe {
                match &self.prev_home {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
                match &self.prev_userprofile {
                    Some(v) => std::env::set_var("USERPROFILE", v),
                    None => std::env::remove_var("USERPROFILE"),
                }
            }
        }
    }

    // ── load / save ──────────────────────────────────────────────────────────

    #[test]
    fn load_absent_config_returns_empty() {
        // Redirect home to an empty temp dir so IDE discovery contributes nothing
        // and the result is deterministic.
        let home = tempfile::tempdir().unwrap();
        let _g = HomeGuard::set(home.path());

        let cwd = tempfile::tempdir().unwrap();
        let mgr = McpConnectionManager::load(cwd.path()).expect("load should succeed when absent");
        assert!(
            mgr.servers.is_empty(),
            "no config + empty home should yield zero servers"
        );
        assert_eq!(mgr.workspace_root, cwd.path());
        assert!(mgr.tools_by_server.is_empty());
    }

    #[test]
    fn save_writes_file_and_round_trips() {
        let home = tempfile::tempdir().unwrap();
        let _g = HomeGuard::set(home.path());

        let cwd = tempfile::tempdir().unwrap();
        let mut mgr = McpConnectionManager::default();
        mgr.add_server(McpServerConfig {
            name: "alpha".to_string(),
            enabled: true,
            kind: McpServerKind::Http {
                url: "http://localhost:1234".to_string(),
            },
        });
        mgr.save(cwd.path()).expect("save should succeed");

        // The config file must exist at the documented path.
        let expected = cwd.path().join(".ahma").join("mcp-clients.toml");
        assert!(expected.exists(), "save must write {}", expected.display());

        let loaded = McpConnectionManager::load(cwd.path()).unwrap();
        assert!(
            loaded.servers.iter().any(|s| s.name == "alpha"),
            "round-tripped config must contain 'alpha'"
        );
    }

    #[test]
    fn load_malformed_config_errors() {
        let cwd = tempfile::tempdir().unwrap();
        let dir = cwd.path().join(".ahma");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("mcp-clients.toml"),
            "this is = = not valid toml [[[",
        )
        .unwrap();

        let err = McpConnectionManager::load(cwd.path());
        assert!(err.is_err(), "malformed TOML must produce a parse error");
        let msg = format!("{:#}", err.unwrap_err());
        assert!(
            msg.contains("Failed to parse"),
            "error context should mention parse failure, got: {msg}"
        );
    }

    #[test]
    fn local_config_takes_precedence_over_ide() {
        // IDE config supplies "shared" (http) and "ideonly" (stdio).
        let home = tempfile::tempdir().unwrap();
        let cursor_dir = home.path().join(".cursor");
        std::fs::create_dir_all(&cursor_dir).unwrap();
        std::fs::write(
            cursor_dir.join("mcp.json"),
            serde_json::json!({
                "mcpServers": {
                    "shared": { "url": "http://ide-shared" },
                    "ideonly": { "command": "npx", "args": ["x"] }
                }
            })
            .to_string(),
        )
        .unwrap();
        let _g = HomeGuard::set(home.path());

        // Local config supplies "shared" (stdio) — it must win.
        let cwd = tempfile::tempdir().unwrap();
        let mut local = McpConnectionManager::default();
        local.add_server(McpServerConfig {
            name: "shared".to_string(),
            enabled: true,
            kind: McpServerKind::Stdio {
                command: "echo".to_string(),
                args: vec![],
            },
        });
        local.save(cwd.path()).unwrap();

        let merged = McpConnectionManager::load(cwd.path()).unwrap();
        let shared: Vec<_> = merged
            .servers
            .iter()
            .filter(|s| s.name == "shared")
            .collect();
        assert_eq!(shared.len(), 1, "'shared' must not be duplicated");
        assert!(
            matches!(shared[0].kind, McpServerKind::Stdio { .. }),
            "local stdio definition must win over IDE http"
        );
        assert!(
            merged.servers.iter().any(|s| s.name == "ideonly"),
            "IDE-only servers must be merged in"
        );
    }

    // ── add / remove / list ──────────────────────────────────────────────────

    #[test]
    fn add_remove_list_servers() {
        let mut mgr = McpConnectionManager::default();
        mgr.add_server(McpServerConfig {
            name: "b-srv".to_string(),
            enabled: true,
            kind: McpServerKind::Http {
                url: "http://b".to_string(),
            },
        });
        mgr.add_server(McpServerConfig {
            name: "a-srv".to_string(),
            enabled: true,
            kind: McpServerKind::Http {
                url: "http://a".to_string(),
            },
        });
        assert_eq!(mgr.list_servers().len(), 2);
        // add_server keeps the list sorted by name.
        assert_eq!(mgr.list_servers()[0].name, "a-srv");

        // Re-adding the same name replaces (deduplicates) rather than grows.
        mgr.add_server(McpServerConfig {
            name: "a-srv".to_string(),
            enabled: false,
            kind: McpServerKind::Http {
                url: "http://a2".to_string(),
            },
        });
        assert_eq!(mgr.list_servers().len(), 2, "re-add must dedupe by name");
        assert!(!mgr.list_servers()[0].enabled, "re-add replaces the entry");

        // remove_server also clears any cached tools for that server.
        mgr.tools_by_server.insert(
            "a-srv".to_string(),
            vec![ToolInfo {
                name: "t".to_string(),
                description: None,
                input_schema: serde_json::json!({}),
            }],
        );
        mgr.remove_server("a-srv");
        assert_eq!(mgr.list_servers().len(), 1);
        assert_eq!(mgr.list_servers()[0].name, "b-srv");
        assert!(!mgr.tools_by_server.contains_key("a-srv"));

        // Removing a non-existent server is a no-op.
        mgr.remove_server("does-not-exist");
        assert_eq!(mgr.list_servers().len(), 1);
    }

    // ── aggregation ──────────────────────────────────────────────────────────

    #[test]
    fn aggregate_tools_namespaces_and_sorts() {
        let mut mgr = McpConnectionManager::default();
        mgr.tools_by_server.insert(
            "zeta".to_string(),
            vec![ToolInfo {
                name: "build".to_string(),
                description: Some("desc".to_string()),
                input_schema: serde_json::json!({"type":"object"}),
            }],
        );
        mgr.tools_by_server.insert(
            "alpha".to_string(),
            vec![ToolInfo {
                name: "run".to_string(),
                description: None,
                input_schema: serde_json::json!({}),
            }],
        );

        let tools = mgr.aggregate_tools();
        let names: Vec<_> = tools.iter().map(|t| t.name.clone()).collect();
        assert_eq!(
            names,
            vec!["alpha::run".to_string(), "zeta::build".to_string()],
            "aggregate_tools must namespace and sort by full name"
        );
        // Non-name fields are preserved through aggregation.
        let zeta_build = tools.iter().find(|t| t.name == "zeta::build").unwrap();
        assert_eq!(zeta_build.description.as_deref(), Some("desc"));
        assert_eq!(
            zeta_build.input_schema,
            serde_json::json!({"type":"object"})
        );
    }

    // ── call_tool error paths (no network) ───────────────────────────────────

    #[tokio::test]
    async fn call_tool_rejects_unnamespaced_name() {
        let mgr = McpConnectionManager::default();
        let err = mgr
            .call_tool("noseparator", serde_json::json!({}))
            .await
            .expect_err("name without '::' must error before any network call");
        assert!(
            format!("{err}").contains("server namespace"),
            "error should explain the required namespace, got: {err}"
        );
    }

    #[tokio::test]
    async fn call_tool_rejects_unknown_server() {
        let mgr = McpConnectionManager::default(); // no servers registered
        let err = mgr
            .call_tool("ghost::tool", serde_json::json!({}))
            .await
            .expect_err("unknown server must error before any network call");
        assert!(
            format!("{err}").contains("Unknown MCP server"),
            "error should name the missing server, got: {err}"
        );
    }

    // ── IDE entry parsing ────────────────────────────────────────────────────

    #[test]
    fn ide_entry_http_url_becomes_http_kind() {
        let entry = serde_json::json!({ "url": "https://example.com/mcp" });
        let server = ide_entry_to_server("remote", &entry).expect("url entry should parse");
        assert_eq!(server.name, "remote");
        assert!(server.enabled);
        match server.kind {
            McpServerKind::Http { url } => assert_eq!(url, "https://example.com/mcp"),
            _ => panic!("expected Http kind"),
        }
    }

    #[test]
    fn ide_entry_without_url_or_command_is_none() {
        let entry = serde_json::json!({ "something": "else" });
        assert!(ide_entry_to_server("bad", &entry).is_none());
    }

    #[test]
    fn ide_entry_stdio_defaults_args_when_missing() {
        let entry = serde_json::json!({ "command": "mytool" });
        let server = ide_entry_to_server("t", &entry).expect("command-only entry should parse");
        match server.kind {
            McpServerKind::Stdio { command, args } => {
                assert_eq!(command, "mytool");
                assert!(args.is_empty(), "missing args should default to empty");
            }
            _ => panic!("expected Stdio kind"),
        }
    }

    #[test]
    fn parse_ide_mcp_json_reads_both_keys_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "mcpServers": { "one": { "url": "http://one" } },
                "servers": {
                    "one": { "url": "http://one-dup" },
                    "two": { "command": "two-cmd" }
                }
            })
            .to_string(),
        )
        .unwrap();

        let mut out = Vec::new();
        parse_ide_mcp_json(&path, &mut out);
        let names: std::collections::BTreeSet<_> = out.iter().map(|s| s.name.clone()).collect();
        assert!(names.contains("one"));
        assert!(names.contains("two"));
        assert_eq!(
            out.iter().filter(|s| s.name == "one").count(),
            1,
            "duplicate names across keys must be deduped (first wins)"
        );
        // The first-seen "one" (from mcpServers) wins.
        let one = out.iter().find(|s| s.name == "one").unwrap();
        match &one.kind {
            McpServerKind::Http { url } => assert_eq!(url, "http://one"),
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn parse_ide_mcp_json_missing_or_invalid_is_silent() {
        // Missing file: no entries, no panic.
        let mut out = Vec::new();
        parse_ide_mcp_json(Path::new("/no/such/path/mcp.json"), &mut out);
        assert!(out.is_empty());

        // Invalid JSON: no entries, no panic.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(&path, "{ not json").unwrap();
        parse_ide_mcp_json(&path, &mut out);
        assert!(out.is_empty());
    }

    // ── discover_ide_servers ─────────────────────────────────────────────────

    #[test]
    fn discover_ide_servers_empty_home_returns_empty() {
        let home = tempfile::tempdir().unwrap();
        let _g = HomeGuard::set(home.path());
        let servers = discover_ide_servers();
        assert!(
            servers.is_empty(),
            "no IDE config under home should yield no servers"
        );
    }

    #[test]
    fn discover_ide_servers_finds_cursor_config() {
        let home = tempfile::tempdir().unwrap();
        let cursor_dir = home.path().join(".cursor");
        std::fs::create_dir_all(&cursor_dir).unwrap();
        std::fs::write(
            cursor_dir.join("mcp.json"),
            serde_json::json!({
                "mcpServers": {
                    "myremote": { "url": "http://discovered" }
                }
            })
            .to_string(),
        )
        .unwrap();
        let _g = HomeGuard::set(home.path());

        let servers = discover_ide_servers();
        assert!(
            servers.iter().any(|s| s.name == "myremote"),
            "cursor mcp.json server should be discovered"
        );
    }

    // ── misc trait impls ─────────────────────────────────────────────────────

    #[test]
    fn manager_debug_redacts_stdio_clients() {
        let mgr = McpConnectionManager::default();
        let dbg = format!("{mgr:?}");
        assert!(dbg.contains("McpConnectionManager"));
        assert!(
            dbg.contains("<stdio clients>"),
            "Debug must redact live stdio clients"
        );
    }
}
