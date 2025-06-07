mod server;
mod utils;

use std::path::PathBuf;

use crate::context::Context;
use crate::project::TransportType;
use anyhow::Result;
use rmcp::model::CallToolResult;
use rmcp::{ServiceExt, transport::SseServer, transport::stdio, transport::streamable_http_server};
use server::DevToolsServer;

#[derive(Debug, Clone)]
pub(super) enum McpNotification {
    Response {
        content: CallToolResult,
        project: PathBuf,
    },
}

pub async fn run_server(context: Context) -> Result<()> {
    let dev_tools_server = DevToolsServer::new(context.clone());

    match context.transport() {
        TransportType::Stdio => {
            let service = dev_tools_server.serve(stdio()).await?;
            service.waiting().await?;
        }
        TransportType::Sse { host, port } => {
            let addr = format!("{}:{}", host, port).parse()?;
            let sse_server = SseServer::serve(addr).await?;
            // with_service takes a factory, so we clone context for each new connection
            let cancel_token =
                sse_server.with_service(move || DevToolsServer::new(context.clone()));
            cancel_token.cancelled().await;
        }
        TransportType::StreamableHttp { host, port } => {
            use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
            let addr = format!("{}:{}", host, port);

            let service = streamable_http_server::StreamableHttpService::new(
                move || DevToolsServer::new(context.clone()),
                LocalSessionManager::default().into(),
                Default::default(),
            );

            let router = axum::Router::new().nest_service("/mcp", service);
            let tcp_listener = tokio::net::TcpListener::bind(&addr).await?;
            tracing::info!("StreamableHttp server listening on {}", addr);
            if let Err(e) = axum::serve(tcp_listener, router)
                .with_graceful_shutdown(async { tokio::signal::ctrl_c().await.unwrap() })
                .await
            {
                tracing::error!("StreamableHttp server error: {}", e);
                return Err(e.into());
            }
            tracing::info!("StreamableHttp server shutdown gracefully");
        }
    }
    Ok(())
}
