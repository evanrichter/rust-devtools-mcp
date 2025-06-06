use anyhow::Result;
use notify_debouncer_mini::{
    DebounceEventResult, DebouncedEvent, Debouncer, new_debouncer, notify::*,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::context::Context;

/// Configuration file watcher, responsible for monitoring config file changes and triggering hot reload
#[derive(Debug)]
pub struct ConfigWatcher {
    #[allow(dead_code)] // Keep handle to ensure watcher keeps running
    debouncer: Debouncer<RecommendedWatcher>,
}

impl ConfigWatcher {
    /// Create a new config file watcher
    pub fn new(context: Arc<RwLock<Context>>) -> Result<Self> {
        let config_path = {
            let ctx = context
                .try_read()
                .map_err(|e| anyhow::anyhow!("Failed to read context: {}", e))?;
            ctx.config_path().clone()
        };

        let context_clone = context.clone();
        let mut debouncer = new_debouncer(
            Duration::from_secs(1), // 1 second debounce
            move |res: DebounceEventResult| {
                if let Err(e) = handle_config_change(res, context_clone.clone()) {
                    error!("Error handling config file change: {}", e);
                }
            },
        )?;

        // Watch the directory containing the config file
        if let Some(config_dir) = config_path.parent() {
            debouncer
                .watcher()
                .watch(config_dir, RecursiveMode::NonRecursive)?;
            info!("Started watching config directory: {:?}", config_dir);
        } else {
            warn!(
                "Could not determine config file directory: {:?}",
                config_path
            );
        }

        Ok(Self { debouncer })
    }
}

/// Handle config file change events
fn handle_config_change(result: DebounceEventResult, context: Arc<RwLock<Context>>) -> Result<()> {
    match result {
        Ok(events) => {
            for event in events {
                if should_reload_config(&event, &context)? {
                    info!("Detected config file change, starting hot reload...");
                    tokio::spawn(async move {
                        if let Err(e) = reload_config(context).await {
                            error!("Config hot reload failed: {}", e);
                        } else {
                            info!("Config hot reload completed successfully");
                        }
                    });
                    break; // Only need to handle one reload
                }
            }
        }
        Err(e) => {
            error!("File watch error: {:?}", e);
        }
    }
    Ok(())
}

/// Determine if config should be reloaded
fn should_reload_config(event: &DebouncedEvent, context: &Arc<RwLock<Context>>) -> Result<bool> {
    let config_path = {
        let ctx = context
            .try_read()
            .map_err(|e| anyhow::anyhow!("Failed to read context: {}", e))?;
        ctx.config_path().clone()
    };

    // Check if event involves our config file
    let is_config_file = event.path == config_path;

    if is_config_file {
        debug!("Config file event: {:?} for {:?}", event.kind, event.path);

        // Reload for any changes to the config file
        debug!("Config file event: {:?} for {:?}", event.kind, event.path);
        return Ok(true);
    }

    Ok(false)
}

/// Execute config reload
async fn reload_config(context: Arc<RwLock<Context>>) -> Result<()> {
    let ctx = context.read().await;

    // Reload the config
    ctx.load_config().await?;

    info!("Config file reloaded");
    Ok(())
}
