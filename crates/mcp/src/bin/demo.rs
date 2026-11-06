//! A small MCP server over stdio with sample data, for demos and tests.

use rmcp::ServiceExt;
use rmcp::transport::stdio;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let service = lighthouse_mcp::demo::DemoServer.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
