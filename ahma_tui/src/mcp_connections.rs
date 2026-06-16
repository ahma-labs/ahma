pub use ahma_mcp::mcp_client::{
    McpClientConfigFile, McpConnectionManager, McpServerConfig, McpServerKind, StdioClient,
    ToolInfo, discover_ide_servers,
};

pub type TuiMcpClientHandler = ahma_mcp::mcp_client::McpClientHandler;
