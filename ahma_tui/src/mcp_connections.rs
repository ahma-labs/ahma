use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, Default)]
pub struct McpConnectionManager {
    pub servers: Vec<McpServerConfig>,
    pub tools_by_server: BTreeMap<String, Vec<String>>,
}

impl McpConnectionManager {
    pub fn load(cwd: &Path) -> Result<Self> {
        let path = config_path(cwd);
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let cfg: McpClientConfigFile = toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;

        Ok(Self {
            servers: cfg.servers,
            tools_by_server: BTreeMap::new(),
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
                out.push(format!("{server}::{tool}"));
            }
        }
        out.sort();
        out
    }

    pub async fn refresh_tools(&mut self) {
        self.tools_by_server.clear();
        for server in self.servers.iter().filter(|s| s.enabled).cloned() {
            if let Ok(tools) = fetch_server_tools(&server).await {
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
                call_mcp_tool_stdio(command, args, tool_name, arguments).await
            }
        }
    }
}

fn config_path(cwd: &Path) -> PathBuf {
    cwd.join(".ahma").join("mcp-clients.toml")
}

async fn fetch_server_tools(server: &McpServerConfig) -> Result<Vec<String>> {
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
                        tools.push(name.to_string());
                    }
                }
            }
            Ok(tools)
        }
        McpServerKind::Stdio { command, args } => fetch_server_tools_stdio(command, args).await,
    }
}

async fn fetch_server_tools_stdio(command: &str, args: &[String]) -> Result<Vec<String>> {
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

    let tools_res = client
        .list_tools(None)
        .await
        .context("Failed tools/list via stdio")?;
    let mut names = Vec::new();
    for tool in tools_res.tools {
        names.push(tool.name.into_owned());
    }
    Ok(names)
}

async fn call_mcp_tool_stdio(
    command: &str,
    args: &[String],
    tool: &str,
    arguments: Value,
) -> Result<(String, bool)> {
    use rmcp::{
        ServiceExt,
        model::CallToolRequestParams,
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
        assert_eq!(loaded.servers.len(), 2);
        assert_eq!(loaded.servers[0].name, "test-http");
        assert_eq!(loaded.servers[1].name, "test-stdio");

        match &loaded.servers[1].kind {
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
            vec!["tool_a".to_string(), "tool_b".to_string()],
        );
        manager
            .tools_by_server
            .insert("srv2".to_string(), vec!["tool_c".to_string()]);

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
