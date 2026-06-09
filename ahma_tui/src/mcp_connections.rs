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

pub type StdioClient = Arc<rmcp::service::RunningService<rmcp::RoleClient, ()>>;

#[derive(Clone)]
pub struct McpConnectionManager {
    pub servers: Vec<McpServerConfig>,
    pub tools_by_server: BTreeMap<String, Vec<ToolInfo>>,
    pub stdio_clients: Arc<Mutex<BTreeMap<String, StdioClient>>>,
}

impl Default for McpConnectionManager {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            tools_by_server: BTreeMap::new(),
            stdio_clients: Arc::new(Mutex::new(BTreeMap::new())),
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

        let client =
            ().serve(TokioChildProcess::new(cmd.configure(|_c| {}))?)
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

/// Discover servers defined in IDE mcp.json config files (Cursor, VS Code).
///
/// Reads from well-known IDE paths and converts `mcpServers` / `servers` entries
/// into `McpServerConfig` values that the TUI can connect to.
pub fn discover_ide_servers() -> Vec<McpServerConfig> {
    let mut results = Vec::new();

    let home = match home_dir() {
        Some(h) => h,
        None => return results,
    };

    // Cursor: ~/.cursor/mcp.json  with "mcpServers" key
    let cursor_path = home.join(".cursor").join("mcp.json");
    parse_ide_mcp_json(&cursor_path, &mut results);

    // VS Code (macOS): ~/Library/Application Support/Code/User/mcp.json  with "servers" key
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

    // VS Code (Linux): ~/.config/Code/User/mcp.json
    #[cfg(target_os = "linux")]
    {
        let vscode_path = home
            .join(".config")
            .join("Code")
            .join("User")
            .join("mcp.json");
        parse_ide_mcp_json(&vscode_path, &mut results);
    }

    // VS Code (Windows): %APPDATA%\Code\User\mcp.json
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
    // std::env::home_dir is deprecated but works fine on all platforms.
    #[allow(deprecated)]
    std::env::home_dir()
}

/// Parse one IDE mcp.json file (Cursor or VS Code format) and push discovered
/// servers into `out`.  Both formats are tried; the function is silent on errors.
fn parse_ide_mcp_json(path: &Path, out: &mut Vec<McpServerConfig>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let val: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return,
    };

    // Try both "mcpServers" (Cursor) and "servers" (VS Code) top-level keys.
    for key in &["mcpServers", "servers"] {
        if let Some(Value::Object(map)) = val.get(*key) {
            for (name, entry) in map {
                if let Some(server) = ide_entry_to_server(name, entry) {
                    // Skip duplicates from the same file.
                    if !out.iter().any(|s| &s.name == name) {
                        out.push(server);
                    }
                }
            }
        }
    }
}

fn ide_entry_to_server(name: &str, entry: &Value) -> Option<McpServerConfig> {
    // HTTP server entry: { "url": "..." }
    if let Some(url) = entry.get("url").and_then(|u| u.as_str()) {
        return Some(McpServerConfig {
            name: name.to_string(),
            enabled: true,
            kind: McpServerKind::Http {
                url: url.to_string(),
            },
        });
    }

    // Stdio server entry: { "command": "...", "args": [...] }
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

    Some(McpServerConfig {
        name: name.to_string(),
        enabled: true,
        kind: McpServerKind::Stdio {
            command: command.to_string(),
            args,
        },
    })
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
                "clientInfo": {"name": "ahma-tui", "version": env!("CARGO_PKG_VERSION")}
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

        // The two explicitly-saved servers must be present; IDE auto-discovery
        // may add more entries, so we look up by name rather than asserting an
        // exact count.
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
}
