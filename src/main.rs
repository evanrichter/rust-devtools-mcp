mod cargo_remote;
mod context;
mod docs;
mod lsp;
mod mcp;
mod project;

use anyhow::Result;
use context::Context as ContextType;
use mcp::run_server;
use tokio::signal;
use tracing::{error, info};
use tracing_subscriber::{
    EnvFilter, Layer, fmt::format::PrettyFields, layer::SubscriberExt, util::SubscriberInitExt,
};

#[tokio::main]
async fn main() -> Result<()> {
    let log_layer = tracing_subscriber::fmt::layer()
        .event_format(tracing_subscriber::fmt::format().compact())
        .fmt_fields(PrettyFields::new())
        .boxed();

    tracing_subscriber::registry()
        .with(
            (EnvFilter::builder().try_from_env())
                .unwrap_or(EnvFilter::new("cursor_rust_tools=info")),
        )
        .with(log_layer)
        .init();

    let (sender, receiver) = flume::unbounded();
    let context = ContextType::new(4000, sender).await;
    context.load_config().await?;

    let final_context = context.clone();

    // Run the MCP Server
    let cloned_context = context.clone();
    let server_handle = tokio::spawn(async move {
        run_server(cloned_context).await.unwrap();
    });

    let main_loop_fut = async {
        info!(
            "Running in CLI mode on port {}:{}",
            context.address_information().0,
            context.address_information().1
        );
        info!("Configuration file: {}", context.configuration_file());
        if context.project_descriptions().await.is_empty() {
            error!("No projects found, please edit configuration file");
            return Ok::<(), anyhow::Error>(()); // Early return for no projects in CLI mode
        }
        info!(
            "Cursor mcp json (project/.cursor.mcp.json):\n```json\n{}\n```",
            context.mcp_configuration()
        );
        
        // Request project descriptions to populate notifications
        context.request_project_descriptions();
        // Keep the CLI mode running indefinitely until Ctrl+C
        loop {
            while let Ok(notification) = receiver.try_recv() {
                let notification_path = notification.notification_path();
                info!("[{}] {}", notification_path.display(), notification.description());
                
                // Handle specific notification types
                match &notification {
                    context::ContextNotification::ProjectDescriptions(descriptions) => {
                        info!("Received project descriptions: {} projects", descriptions.len());
                        for desc in descriptions {
                            info!("  - {}: {}", desc.name, desc.root.display());
                        }
                    }
                    context::ContextNotification::ProjectAdded(path) => {
                        info!("Project added: {}", path.display());
                    }
                    context::ContextNotification::ProjectRemoved(path) => {
                        info!("Project removed: {}", path.display());
                    }
                    context::ContextNotification::Lsp(_lsp_notif) => {
                        info!("LSP notification for project: {}", notification_path.display());
                    }
                    context::ContextNotification::Docs(_docs_notif) => {
                        info!("Docs notification for project: {}", notification_path.display());
                    }
                    context::ContextNotification::Mcp(_mcp_notif) => {
                        info!("MCP notification for project: {}", notification_path.display());
                    }
                }
            }
            // Add a small sleep to avoid busy-waiting if desired, or just rely on Ctrl+C
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    };

    tokio::select! {
        res = main_loop_fut => {
            if let Err(e) = res {
                error!("Main loop finished with error: {}", e);
            } else {
                info!("Main loop finished normally.");
            }
        },
        _ = signal::ctrl_c() => {
            info!("Ctrl+C received, shutting down...");
        }
        _ = server_handle => {
             info!("Server task finished unexpectedly.");
        }
    }

    final_context.shutdown_all().await;

    Ok(())
}
