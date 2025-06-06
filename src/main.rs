mod cargo_remote;
mod config_watcher;
mod context;
mod lsp;
mod mcp;
mod project;

use anyhow::Result;
use clap::{Parser, Subcommand};
use context::{Context as ContextType, ContextNotification};
use lsp::LspNotification;
use mcp::run_server;
use std::path::PathBuf;
use tokio::signal;
use tracing::{error, info, warn};
use tracing_subscriber::{
    EnvFilter, Layer, fmt::format::PrettyFields, layer::SubscriberExt, util::SubscriberInitExt,
};

/// Beautify path display by converting long paths to a more concise format
pub fn beautify_path(path: &std::path::Path) -> String {
    let path_str = path.to_string_lossy();

    // Remove Windows \\?\\ prefix
    let cleaned = if path_str.starts_with("\\\\?\\") {
        &path_str[4..]
    } else {
        &path_str
    };

    // Get project name (last directory name)
    if let Some(project_name) = path.file_name() {
        let name = project_name.to_string_lossy();
        // If path is too long, only show project name and simplified parent path
        if cleaned.len() > 50 {
            if let Some(parent) = path.parent() {
                if let Some(grandparent) = parent.file_name() {
                    return format!("📁 {}/{}", grandparent.to_string_lossy(), name);
                }
            }
            return format!("📁 {}", name);
        }
    }

    // Replace backslashes with forward slashes (more aesthetic)
    let normalized = cleaned.replace('\\', "/");

    // If it's a subdirectory of current working directory, use relative path
    if let Ok(current_dir) = std::env::current_dir() {
        if let Ok(relative) = path.strip_prefix(&current_dir) {
            let rel_str = relative.to_string_lossy().replace('\\', "/");
            if !rel_str.is_empty() {
                return format!("📁 ./{}", rel_str);
            }
        }
    }

    format!("📁 {}", normalized)
}

/// A powerful suite of Rust development tools for the Model-Context Protocol (MCP)
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to the configuration file
    #[arg(long, global = true, default_value = "~/.rust-devtools-mcp.toml")]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the MCP server and listen for requests
    Serve(ServeArgs),
    /// Manage projects in the workspace
    #[command(subcommand)]
    Projects(ProjectCommands),
    /// Show configuration information
    Config(ConfigArgs),
}

#[derive(Parser, Debug)]
struct ServeArgs {
    /// Port to run the SSE server on
    #[arg(short, long, default_value_t = 4000)]
    port: u16,
}

#[derive(Subcommand, Debug)]
enum ProjectCommands {
    /// Add a new project to the workspace
    Add {
        /// The root path of the project to add
        #[arg(required = true)]
        path: PathBuf,
    },
    /// Remove a project from the workspace
    #[command(alias = "rm")]
    Remove {
        /// The root path of the project to remove
        #[arg(required = true)]
        path: PathBuf,
    },
    /// List all projects currently in the workspace
    #[command(alias = "ls")]
    List,
    /// Clear all projects from the workspace
    Clear,
}

#[derive(Parser, Debug)]
struct ConfigArgs {
    /// Port to use when generating MCP config (should match serve port)
    #[arg(short, long, default_value_t = 4000)]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Expand tilde in the config path
    let config_path = PathBuf::from(shellexpand::tilde(&cli.config).to_string());

    let log_layer = tracing_subscriber::fmt::layer()
        .event_format(tracing_subscriber::fmt::format().compact().without_time())
        .fmt_fields(PrettyFields::new())
        .boxed();

    tracing_subscriber::registry()
        .with(
            (EnvFilter::builder().try_from_env())
                .unwrap_or_else(|_| EnvFilter::new("rust_devtools_mcp=info")),
        )
        .with(log_layer)
        .init();

    match cli.command {
        Commands::Serve(args) => run_serve(args, config_path).await,
        Commands::Projects(subcommand) => handle_projects(subcommand, config_path).await,
        Commands::Config(args) => handle_config(args, config_path).await,
    }
}

async fn run_serve(args: ServeArgs, config_path: PathBuf) -> Result<()> {
    info!("run_serve: Starting function");
    let (sender, receiver) = flume::unbounded();
    info!("run_serve: Created channels");
    let context = ContextType::new(args.port, config_path, sender).await;
    info!("run_serve: Created context");
    context.load_config().await?;
    info!("run_serve: Loaded config");

    // Create config file watcher to support hot reloading
    let context_for_watcher = std::sync::Arc::new(tokio::sync::RwLock::new(context.clone()));
    let _config_watcher = config_watcher::ConfigWatcher::new(context_for_watcher)?;
    info!("Config file hot reloading enabled");

    let final_context = context.clone();

    // Run the MCP Server
    info!("run_serve: About to spawn MCP server task");
    let cloned_context = context.clone();
    let server_handle = tokio::spawn(async move {
        info!("Starting MCP server task...");
        if let Err(e) = run_server(cloned_context).await {
            error!("MCP Server exited with an error: {}", e);
        } else {
            info!("MCP Server task completed successfully");
        }
    });
    info!("run_serve: Spawned MCP server task");

    // Give the server task a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let main_loop_fut = async {
        info!(
            "Starting server on {}:{}",
            context.address_information().0,
            context.address_information().1
        );
        info!(
            "Using configuration file: {}",
            context.config_path().display()
        );
        if context.project_descriptions().await.is_empty() {
            warn!(
                "No projects found. Once connected, add one using the 'add_project' tool or the CLI: `rust-devtools-mcp projects add <path>`"
            );
        }
        info!(
            "Cursor MCP JSON (for .cursor/mcp.json):\n---\n{}\n---",
            context.mcp_configuration()
        );

        // Immediately show connection information
        println!();
        info!("-------------------------------------------------------------------");
        info!("🚀 Connection Ready! Your MCP server is now fully initialized.");
        info!("📋 To connect your MCP client (e.g., Cursor):");
        info!("1. Copy the JSON configuration from above (between the '---' lines).");
        info!("2. Paste it into your project's .cursor/mcp.json file.");
        info!("3. Start using Rust development tools in your editor!");
        info!("-------------------------------------------------------------------");
        println!();

        info!("⏳ Initializing LSP and indexing project... Please wait for completion.");

        context.request_project_descriptions();

        let mut is_indexing = false;
        let mut indexing_finished_sent = false;
        let mut last_indexing_activity = std::time::Instant::now();
        let mut any_stage_completed = false;
        
        info!("Initial state - indexing_finished_sent: {}", indexing_finished_sent);

        loop {
            while let Ok(notification) = receiver.try_recv() {
                let notification_path = notification.notification_path();

                // Clear the current line before processing any notification
                print!("\r\x1B[2K");
                use std::io::{self, Write};
                io::stdout().flush().unwrap();


                match &notification {
                    ContextNotification::Lsp(LspNotification::Indexing {
                        is_indexing: indexing,
                        progress,
                        project,
                    }) => {
                        if *indexing {
                            is_indexing = true;
                            last_indexing_activity = std::time::Instant::now();

                            let is_cache_priming = progress
                                .as_ref()
                                .map(|p| matches!(p.stage, crate::lsp::IndexingStage::CachePriming))
                                .unwrap_or(false);

                            if is_cache_priming {
                                print!(
                                    "[{}] {}",
                                    beautify_path(&notification_path),
                                    notification.description()
                                );
                                io::stdout().flush().unwrap();
                            } else {
                                info!(
                                    "[{}] {}",
                                    beautify_path(&notification_path),
                                    notification.description()
                                );
                            }
                        } else {
                            // This is a WorkDoneProgress::End event for a specific stage
                            let stage_name = progress.as_ref()
                                .map(|p| format!("{:?}", p.stage))
                                .unwrap_or_else(|| "Unknown".to_string());
                                                        
                            // Check if this is a known indexing stage completion
                            let is_indexing_stage = progress.as_ref()
                                .map(|p| matches!(p.stage, 
                                    crate::lsp::IndexingStage::CachePriming | 
                                    crate::lsp::IndexingStage::Indexing |
                                    crate::lsp::IndexingStage::Building
                                ))
                                .unwrap_or(false);
                            
                            // Always clear the current line for stage completion
                            print!("\r\x1B[2K");
                            io::stdout().flush().unwrap();
                            
                            // Show stage completion message
                            info!(
                                "[{}] ✅ {} Stage: Completed",
                                beautify_path(project),
                                stage_name
                            );
                            
                            // Mark indexing as finished for any known indexing stage completion
                            if is_indexing_stage {
                                is_indexing = false;
                                any_stage_completed = true;
                                last_indexing_activity = std::time::Instant::now();
                            }
                        }
                    }
                    other_notification => {
                        info!(
                            "[{}] {}",
                            beautify_path(&notification_path),
                            other_notification.description()
                        );
                    }
                }
            }
            
            // Check if indexing has been idle for a while and show final completion message
            if any_stage_completed && !indexing_finished_sent && !is_indexing {
                let idle_duration = last_indexing_activity.elapsed();
                if idle_duration >= std::time::Duration::from_secs(2) {
                    info!("✅ LSP Indexing: Finished");
                    indexing_finished_sent = true;
                }
            }
            
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    };

    tokio::select! {
        _ = main_loop_fut => {
            info!("Main loop finished unexpectedly.");
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

async fn handle_projects(command: ProjectCommands, config_path: PathBuf) -> Result<()> {
    use crate::context::{SerConfig, SerProject};
    use std::collections::HashMap;
    use std::fs;

    // For CLI commands, we work directly with the config file instead of starting services
    let mut config = if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        toml::from_str::<SerConfig>(&content)?
    } else {
        SerConfig {
            projects: HashMap::new(),
        }
    };

    match command {
        ProjectCommands::Add { path } => {
            let absolute_path = path.canonicalize()?;
            println!("✅ Adding project: {}", beautify_path(&absolute_path));

            let project = crate::project::Project::new(&absolute_path)?;
            let ser_project = SerProject {
                root: project.root().clone(),
                ignore_crates: project.ignore_crates().to_vec(),
            };

            config.projects.insert(absolute_path.clone(), ser_project);

            // Save config
            if let Some(parent) = config_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let content = toml::to_string_pretty(&config)?;
            fs::write(&config_path, content)?;

            println!("🎉 Project successfully added to workspace!");
        }
        ProjectCommands::Remove { path } => {
            let absolute_path = path.canonicalize()?;
            println!("🗑️  Removing project: {}", beautify_path(&absolute_path));

            if config.projects.remove(&absolute_path).is_some() {
                // Save config
                let content = toml::to_string_pretty(&config)?;
                fs::write(&config_path, content)?;
                println!("✅ Project successfully removed from workspace!");
            } else {
                println!("⚠️  Project not found: {}", beautify_path(&absolute_path));
            }
        }
        ProjectCommands::List => {
            if config.projects.is_empty() {
                println!("📭 No projects found in the workspace.");
                println!("💡 Add a project using: rust-devtools-mcp projects add <path>");
            } else {
                println!("📋 Projects in workspace:");
                for (root, project) in &config.projects {
                    let name = root
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("<unknown>");
                    println!("  • {} {}", name, beautify_path(&project.root));
                }
            }
        }
        ProjectCommands::Clear => {
            if config.projects.is_empty() {
                println!("📭 No projects found in the workspace. Nothing to clear.");
            } else {
                let project_count = config.projects.len();
                println!("🧹 Clearing {} project(s) from workspace...", project_count);
                
                config.projects.clear();
                
                // Save config
                let content = toml::to_string_pretty(&config)?;
                fs::write(&config_path, content)?;
                
                println!("✅ All projects successfully cleared from workspace!");
            }
        }
    }

    Ok(())
}

async fn handle_config(args: ConfigArgs, config_path: PathBuf) -> Result<()> {
    // We don't need a real notifier for config display
    let (sender, _) = flume::unbounded();
    let context = ContextType::new(args.port, config_path.clone(), sender).await;

    println!("⚙️  Configuration file: {}", beautify_path(&config_path));
    println!("📋 MCP Configuration for Cursor (.cursor/mcp.json):");
    println!("{}", context.mcp_configuration());

    Ok(())
}
