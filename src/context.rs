use anyhow::Result;
use dashmap::DashMap;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::cargo_remote::CargoRemote;
use crate::lsp::LspNotification;
use crate::mcp::McpNotification;
use crate::{
    lsp::RustAnalyzerLsp,
    project::{Project, TransportType},
};
use flume::Sender;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDescription {
    pub root: PathBuf,
    pub name: String,
    pub is_indexing_lsp: bool,
}

#[derive(Debug, Clone)]
pub enum ContextNotification {
    Lsp(LspNotification),
    Mcp(McpNotification),
    ProjectAdded(PathBuf),
    ProjectRemoved(PathBuf),
    ProjectDescriptions(Vec<ProjectDescription>),
}

impl ContextNotification {
    pub fn notification_path(&self) -> PathBuf {
        match self {
            ContextNotification::Lsp(LspNotification::Indexing { project, .. }) => project.clone(),
            ContextNotification::Mcp(McpNotification::Request { project, .. }) => project.clone(),
            ContextNotification::Mcp(McpNotification::Response { project, .. }) => project.clone(),
            ContextNotification::ProjectAdded(project) => project.clone(),
            ContextNotification::ProjectRemoved(project) => project.clone(),
            ContextNotification::ProjectDescriptions(_) => PathBuf::from("project_descriptions"),
        }
    }

    pub fn description(&self) -> String {
        match self {
            ContextNotification::Lsp(LspNotification::Indexing {
                is_indexing,
                progress,
                ..
            }) => {
                if *is_indexing {
                    if let Some(progress) = progress {
                        let stage_icon = match progress.stage {
                            crate::lsp::IndexingStage::Building => "🔨",
                            crate::lsp::IndexingStage::CachePriming => "⚡",
                            crate::lsp::IndexingStage::Indexing => "📚",
                            crate::lsp::IndexingStage::Unknown(_) => "⚙️",
                        };

                        let stage_name = match &progress.stage {
                            crate::lsp::IndexingStage::Building => "Building",
                            crate::lsp::IndexingStage::CachePriming => "Cache Priming",
                            crate::lsp::IndexingStage::Indexing => "Indexing",
                            crate::lsp::IndexingStage::Unknown(s) => s,
                        };

                        let mut parts = vec![format!("{} {}", stage_icon, stage_name)];

                        if let (Some(current), Some(total)) =
                            (progress.current_count, progress.total_count)
                        {
                            let percentage = (current as f32 / total as f32 * 100.0) as u32;
                            parts.push(format!("[{}/{}] {}%", current, total, percentage));
                        } else if let Some(percentage) = progress.percentage {
                            parts.push(format!("{}%", percentage as u32));
                        }

                        if let Some(crate_name) = &progress.current_crate {
                            parts.push(format!("📦 {}", crate_name));
                        }

                        parts.join(" ")
                    } else {
                        "🔄 LSP Indexing: Started".to_string()
                    }
                } else {
                    "✅ LSP Indexing: Finished".to_string()
                }
            }
            ContextNotification::Mcp(McpNotification::Request { content, .. }) => {
                format!("MCP Request: {:?}", content)
            }
            ContextNotification::Mcp(McpNotification::Response { content, .. }) => {
                format!("MCP Response: {:?}", content)
            }
            ContextNotification::ProjectAdded(_) => "Project Added".to_string(),
            ContextNotification::ProjectRemoved(_) => "Project Removed".to_string(),
            ContextNotification::ProjectDescriptions(descriptions) => {
                if descriptions.is_empty() {
                    "No projects loaded".to_string()
                } else {
                    let project_count = descriptions.len();
                    let indexing_count = descriptions.iter().filter(|d| d.is_indexing_lsp).count();
                    let ready_count = project_count - indexing_count;

                    let mut parts = vec![
                        format!("Projects: {} total", project_count),
                        format!("🚀 Ready: {}", ready_count),
                    ];

                    if indexing_count > 0 {
                        parts.push(format!("🔄 Indexing: {}", indexing_count));
                    }

                    parts.join(", ")
                }
            }
        }
    }
}



#[derive(Debug)]
pub struct ProjectContext {
    pub project: Project,
    pub lsp: RustAnalyzerLsp,
    pub cargo_remote: CargoRemote,
    pub is_indexing_lsp: AtomicBool,
}

#[derive(Clone)]
pub struct Context {
    projects: Arc<DashMap<PathBuf, Arc<ProjectContext>>>,
    transport: TransportType,
    lsp_sender: Sender<LspNotification>,
    mcp_sender: Sender<McpNotification>,
    notifier: Sender<ContextNotification>,
    config_path: PathBuf,
}

impl Context {
    pub async fn new(
        transport: TransportType,
        config_path: PathBuf,
        notifier: Sender<ContextNotification>,
    ) -> Self {
        let (lsp_sender, lsp_receiver) = flume::unbounded();
        let (mcp_sender, mcp_receiver) = flume::unbounded();

        let projects = Arc::new(DashMap::<PathBuf, Arc<ProjectContext>>::new());

        let cloned_projects = projects.clone();
        let cloned_notifier = notifier.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Ok(notification) = mcp_receiver.recv_async() => {
                        if let Err(e) = cloned_notifier.send(ContextNotification::Mcp(notification)) {
                            tracing::error!("Failed to send MCP notification: {}", e);
                        }
                    }
                    Ok(ref notification @ LspNotification::Indexing { ref project, is_indexing, .. }) = lsp_receiver.recv_async() => {
                        if let Err(e) = cloned_notifier.send(ContextNotification::Lsp(notification.clone())) {
                            tracing::error!("Failed to send LSP notification: {}", e);
                        }
                        if let Some(project) = cloned_projects.get(project) {
                            project.value().is_indexing_lsp.store(is_indexing, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    else => {
                        // All receivers are closed, break the loop
                        tracing::debug!("All notification receivers closed, stopping notification handler");
                        break;
                    }
                }
            }
        });

        Self {
            projects,
            transport,
            lsp_sender,
            mcp_sender,
            notifier,
            config_path,
        }
    }

    pub fn address_information(&self) -> (String, u16) {
        match &self.transport {
            TransportType::Stdio => ("stdio".to_string(), 0),
            TransportType::Sse { host, port } => (host.clone(), *port),
            TransportType::StreamableHttp { host, port } => (host.clone(), *port),
        }
    }

    pub fn mcp_configuration(&self) -> String {
        let (host, port) = self.address_information();
        
        let template = match &self.transport {
            TransportType::Stdio => CONFIG_TEMPLATE_STDIO,
            TransportType::Sse { .. } => CONFIG_TEMPLATE_SSE,
            TransportType::StreamableHttp { .. } => CONFIG_TEMPLATE_STREAMABLE_HTTP,
        };
        
        template
            .replace("{{HOST}}", &host)
            .replace("{{PORT}}", &port.to_string())
    }

    pub fn config_path(&self) -> &PathBuf {
        &self.config_path
    }

    pub async fn project_descriptions(&self) -> Vec<ProjectDescription> {
        project_descriptions(&self.projects).await
    }

    pub fn transport(&self) -> &TransportType {
        &self.transport
    }

    pub async fn send_mcp_notification(&self, notification: McpNotification) -> Result<()> {
        self.mcp_sender.send_async(notification).await?;
        Ok(())
    }

    async fn write_config(&self) -> Result<()> {
        let projects_to_save: HashMap<PathBuf, SerProject> = self
            .projects
            .iter()
            .map(|entry| {
                let path = entry.key().clone();
                let pc = entry.value().clone();
                let ser_project = SerProject {
                    root: pc.project.root().clone(),
                    ignore_crates: pc.project.ignore_crates().to_vec(),
                };
                (path, ser_project)
            })
            .collect();
        let config = SerConfig {
            projects: projects_to_save,
        };

        let config_path = self.config_path();

        let toml_string = toml::to_string_pretty(&config)?;
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&config_path, toml_string)?;
        tracing::debug!("Wrote config file to {:?}", config_path);
        Ok(())
    }

    pub async fn load_config(&self) -> Result<()> {
        let config_path = self.config_path();

        if !config_path.exists() {
            tracing::warn!(
                "Configuration file not found at {:?}, skipping load.",
                config_path
            );
            return Ok(());
        }

        let toml_string = match fs::read_to_string(&config_path) {
            Ok(content) => content,
            Err(e) => {
                tracing::error!("Failed to read config file {:?}: {}", config_path, e);
                return Err(e.into()); // Propagate read error
            }
        };

        if toml_string.trim().is_empty() {
            tracing::warn!(
                "Configuration file {:?} is empty, skipping load.",
                config_path
            );
            return Ok(());
        }

        let loaded_config: SerConfig = match toml::from_str(&toml_string) {
            Ok(config) => config,
            Err(e) => {
                tracing::error!(
                    "Failed to parse TOML from config file {:?}: {}",
                    config_path,
                    e
                );
                // Don't return error here, maybe the file is corrupt but we can continue
                return Ok(());
            }
        };

        for (_, ser_project) in loaded_config.projects {
            let project = Project {
                root: ser_project.root.clone(),
                ignore_crates: ser_project.ignore_crates,
            };
            // Validate project root before adding
            if !project.root().exists() || !project.root().is_dir() {
                tracing::warn!(
                    "Project root {:?} from config does not exist or is not a directory, skipping.",
                    project.root()
                );
                continue;
            }
            // We need to canonicalize again as the stored path might be relative or different
            match Project::new(project.root()) {
                Ok(new_project) => {
                    if let Err(e) = self.add_project(new_project).await {
                        tracing::error!(
                            "Failed to add project {:?} from config: {}",
                            project.root(),
                            e
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to create project for root {:?} from config: {}",
                        project.root(),
                        e
                    );
                }
            }
        }

        Ok(())
    }

    /// Add a new project to the context
    pub async fn add_project(&self, project: Project) -> Result<()> {
        let root = project.root().clone();
        let lsp = RustAnalyzerLsp::new(&project, self.lsp_sender.clone()).await?;
        let cargo_remote = CargoRemote::new(project.clone());
        let project_context = Arc::new(ProjectContext {
            project,
            lsp,
            cargo_remote,
            is_indexing_lsp: AtomicBool::new(true),
        });

        self.projects.insert(root.clone(), project_context);

        self.request_project_descriptions();

        // Write config after successfully adding
        if let Err(e) = self.write_config().await {
            tracing::error!("Failed to write config after adding project: {}", e);
        }

        if let Err(e) = self.notifier.send(ContextNotification::ProjectAdded(root)) {
            tracing::error!("Failed to send project added notification: {}", e);
        }

        Ok(())
    }

    /// Remove a project from the context
    pub async fn remove_project(&self, root: &PathBuf) -> Option<Arc<ProjectContext>> {
        let project = self.projects.remove(root).map(|(_, v)| v);

        if project.is_some() {
            if let Err(e) = self
                .notifier
                .send(ContextNotification::ProjectRemoved(root.clone()))
            {
                tracing::error!("Failed to send project removed notification: {}", e);
            }
            // Write config after successfully removing
            if let Err(e) = self.write_config().await {
                tracing::error!("Failed to write config after removing project: {}", e);
            }
        }
        project
    }

    pub fn request_project_descriptions(&self) {
        let projects = self.projects.clone();
        let notifier = self.notifier.clone();
        tokio::spawn(async move {
            let project_descriptions = project_descriptions(&projects).await;
            if let Err(e) = notifier.send(ContextNotification::ProjectDescriptions(
                project_descriptions,
            )) {
                tracing::error!("Failed to send project descriptions: {}", e);
            }
        });
    }

    /// Get a reference to a project context by its root path
    pub async fn get_project(&self, root: &PathBuf) -> Option<Arc<ProjectContext>> {
        self.projects.get(root).map(|entry| entry.value().clone())
    }

    /// Get a reference to a project context by any path within the project
    /// Will traverse up the path hierarchy until it finds a matching project root
    pub async fn get_project_by_path(&self, path: &Path) -> Option<Arc<ProjectContext>> {
        let mut current_path = path.to_path_buf();

        if let Some(project) = self.projects.get(&current_path) {
            return Some(project.value().clone());
        }

        while let Some(parent) = current_path.parent() {
            current_path = parent.to_path_buf();
            if let Some(project) = self.projects.get(&current_path) {
                return Some(project.value().clone());
            }
        }

        None
    }

    pub async fn shutdown_all(&self) {
        for entry in self.projects.iter() {
            let p = entry.value();
            if let Err(e) = p.lsp.shutdown().await {
                tracing::error!(
                    "Failed to shutdown LSP for project {:?}: {}",
                    crate::beautify_path(p.project.root()),
                    e
                );
            }
        }
    }
}

const CONFIG_TEMPLATE_SSE: &str = r#"
{
    "mcpServers": {
        "rust-devtools-mcp": {
            "url": "http://{{HOST}}:{{PORT}}/sse"
        }
    }
}
"#;

const CONFIG_TEMPLATE_STREAMABLE_HTTP: &str = r#"
{
    "mcpServers": {
        "rust-devtools-mcp": {
            "url": "http://{{HOST}}:{{PORT}}/mcp"
        }
    }
}
"#;

const CONFIG_TEMPLATE_STDIO: &str = r#"
{
    "mcpServers": {
        "rust-devtools-mcp": {
            "command": "rust-devtools-mcp",
            "args": ["serve", "--transport", "stdio"]
        }
    }
}
"#;

#[derive(Serialize, Deserialize, Debug)]
pub struct SerConfig {
    pub projects: HashMap<PathBuf, SerProject>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SerProject {
    pub root: PathBuf,
    pub ignore_crates: Vec<String>,
}

async fn project_descriptions(
    projects: &DashMap<PathBuf, Arc<ProjectContext>>,
) -> Vec<ProjectDescription> {
    projects
        .iter()
        .map(|entry| {
            let project = entry.value();
            ProjectDescription {
                root: project.project.root().clone(),
                name: project
                    .project
                    .root()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
                is_indexing_lsp: project
                    .is_indexing_lsp
                    .load(std::sync::atomic::Ordering::Relaxed),
            }
        })
        .collect()
}
