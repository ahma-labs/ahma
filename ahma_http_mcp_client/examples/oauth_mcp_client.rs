use ahma_http_mcp_client::client::HttpMcpTransport;
use clap::Parser;
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult, Tool},
    service::RunningService,
};
use url::Url;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// MCP server SSE endpoint (defaults to Atlassian)
    #[arg(long, default_value = "https://mcp.atlassian.com/v1/sse")]
    server_url: Url,

    /// Atlassian Client ID
    #[arg(long)]
    atlassian_client_id: String,

    /// Atlassian Client Secret
    #[arg(long)]
    atlassian_client_secret: String,

    /// Query string to send to the Confluence search tool
    #[arg(long, default_value = "MCP")]
    query: String,
}

fn print_tools(tools: &[Tool]) {
    println!("Found {} tools:", tools.len());
    for tool in tools {
        println!(
            "- {}: {}",
            tool.name,
            tool.description.as_deref().unwrap_or("No description")
        );
    }
}

fn print_tool_result(result: CallToolResult) {
    println!("Search result:");
    for content in result.content {
        match content.as_text() {
            Some(text_content) => println!("{}", text_content.text),
            None => println!("[Non-text content]"),
        }
    }
}

async fn run_search_if_available(
    service: &RunningService<RoleClient, ()>,
    tools: &[Tool],
    query: &str,
) {
    let Some(tool) = tools.iter().find(|t| t.name.contains("search")) else {
        println!("\nNo tool with 'search' in its name found.");
        return;
    };

    println!("\nFound search tool: {}", tool.name);
    println!("Searching for '{}'...", query);

    let params = CallToolRequestParams::new(tool.name.clone()).with_arguments(
        serde_json::json!({ "query": query })
            .as_object()
            .unwrap()
            .clone(),
    );

    match service.call_tool(params).await {
        Ok(result) => print_tool_result(result),
        Err(e) => println!("Error calling search tool: {}", e),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    println!("Connecting to Atlassian MCP server at {}", args.server_url);

    let transport = HttpMcpTransport::new(
        args.server_url.clone(),
        Some(args.atlassian_client_id),
        Some(args.atlassian_client_secret),
    )?;

    println!("Authenticating...");
    transport.ensure_authenticated().await?;
    println!("Authentication successful!");

    let service = ().serve(transport).await?;

    println!("Listing tools...");
    let tools_result = service.list_tools(None).await?;
    print_tools(&tools_result.tools);

    run_search_if_available(&service, &tools_result.tools, &args.query).await;

    Ok(())
}
