use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_lsp::concurrency::ConcurrencyLayer;
use async_lsp::panic::CatchUnwindLayer;
use async_lsp::server::LifecycleLayer;
use async_lsp::tracing::TracingLayer;
use async_lsp::{LanguageServer, ServerSocket};
use lsp_types::request::{CodeActionRequest, Rename, WorkspaceSymbolRequest};
use lsp_types::{
    ClientCapabilities, CodeActionClientCapabilities, CodeActionContext, CodeActionLiteralSupport,
    CodeActionParams, CodeActionResponse, DidOpenTextDocumentParams,
    DocumentSymbolClientCapabilities, Hover, HoverClientCapabilities, HoverParams,
    InitializeParams, InitializedParams, Location, MarkupKind, Position, Range, ReferenceContext,
    ReferenceParams, RenameParams, TextDocumentClientCapabilities, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, Url, WindowClientCapabilities,
    WorkDoneProgressParams, WorkspaceEdit, WorkspaceEditClientCapabilities, WorkspaceFolder,
    WorkspaceSymbolClientCapabilities, WorkspaceSymbolParams,
};
use serde_json::json;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tower::ServiceBuilder;
use tracing::{debug, info};

use super::change_notifier::ChangeNotifier;
use super::client_state::ClientState;
use crate::lsp::LspNotification;
use crate::project::Project;
use flume::Sender;

#[derive(Debug)]
pub struct RustAnalyzerLsp {
    project: Project,
    server: Arc<Mutex<ServerSocket>>,
    #[allow(dead_code)] // Keep the handle to ensure the mainloop runs
    mainloop_handle: Mutex<Option<JoinHandle<()>>>,
    indexed_rx: Mutex<flume::Receiver<()>>,
    #[allow(dead_code)] // Keep the handle to ensure the change notifier runs
    change_notifier: ChangeNotifier,
}

impl RustAnalyzerLsp {
    pub async fn new(project: &Project, notifier: Sender<LspNotification>) -> Result<Self> {
        let (indexed_tx, indexed_rx) = flume::unbounded();
        let (mainloop, server) = async_lsp::MainLoop::new_client(|_server| {
            ServiceBuilder::new()
                .layer(TracingLayer::default())
                .layer(LifecycleLayer::default()) // Handle init/shutdown automatically
                .layer(CatchUnwindLayer::default())
                .layer(ConcurrencyLayer::default())
                .service(ClientState::new_router(
                    indexed_tx,
                    notifier,
                    project.root().to_path_buf(),
                ))
        });

        let process = async_process::Command::new("rust-analyzer")
            .current_dir(project.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("Failed run rust-analyzer")?;

        let stdout = process.stdout.context("Failed to get stdout")?;
        let stdin = process.stdin.context("Failed to get stdin")?;

        let mainloop_handle = tokio::spawn(async move {
            match mainloop.run_buffered(stdout, stdin).await {
                Ok(()) => debug!("LSP mainloop finished gracefully."),
                Err(e) => tracing::error!("LSP mainloop finished with error: {}", e),
            }
        });

        let server = Arc::new(Mutex::new(server));

        // Get the current runtime handle
        let handle = tokio::runtime::Handle::current();
        let change_notifier = ChangeNotifier::new(server.clone(), project, handle)?;

        let client = Self {
            project: project.clone(),
            server,
            mainloop_handle: Mutex::new(Some(mainloop_handle)),
            indexed_rx: Mutex::new(indexed_rx),
            change_notifier,
        };

        // Initialize.
        let init_ret = client
            .server
            .lock()
            .await
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: project.uri()?,
                    name: "root".into(),
                }]),
                capabilities: ClientCapabilities {
                    workspace: Some(lsp_types::WorkspaceClientCapabilities {
                        symbol: Some(WorkspaceSymbolClientCapabilities {
                            dynamic_registration: Some(false),
                            ..Default::default()
                        }),
                        workspace_edit: Some(WorkspaceEditClientCapabilities {
                            document_changes: Some(true),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    window: Some(WindowClientCapabilities {
                        work_done_progress: Some(true), // Required for indexing progress
                        ..WindowClientCapabilities::default()
                    }),
                    text_document: Some(TextDocumentClientCapabilities {
                        document_symbol: Some(DocumentSymbolClientCapabilities {
                            // Flat symbols are easier to process for us
                            hierarchical_document_symbol_support: Some(false),
                            ..DocumentSymbolClientCapabilities::default()
                        }),
                        hover: Some(HoverClientCapabilities {
                            content_format: Some(vec![MarkupKind::Markdown]),
                            ..HoverClientCapabilities::default()
                        }),
                        code_action: Some(CodeActionClientCapabilities {
                            code_action_literal_support: Some(CodeActionLiteralSupport {
                                code_action_kind: lsp_types::CodeActionKindLiteralSupport {
                                    value_set: vec![
                                        lsp_types::CodeActionKind::EMPTY.as_str().to_string(),
                                        lsp_types::CodeActionKind::QUICKFIX.as_str().to_string(),
                                        lsp_types::CodeActionKind::REFACTOR.as_str().to_string(),
                                        lsp_types::CodeActionKind::REFACTOR_EXTRACT
                                            .as_str()
                                            .to_string(),
                                        lsp_types::CodeActionKind::REFACTOR_INLINE
                                            .as_str()
                                            .to_string(),
                                        lsp_types::CodeActionKind::REFACTOR_REWRITE
                                            .as_str()
                                            .to_string(),
                                        lsp_types::CodeActionKind::SOURCE.as_str().to_string(),
                                        lsp_types::CodeActionKind::SOURCE_ORGANIZE_IMPORTS
                                            .as_str()
                                            .to_string(),
                                    ],
                                },
                            }),
                            ..Default::default()
                        }),
                        ..TextDocumentClientCapabilities::default()
                    }),
                    experimental: Some(json!({
                        "hoverActions": true
                    })),
                    ..ClientCapabilities::default()
                },
                ..InitializeParams::default()
            })
            .await
            .context("LSP initialize failed")?;
        tracing::trace!("Initialized: {init_ret:?}");
        info!("LSP Initialized");

        client
            .server
            .lock()
            .await
            .initialized(InitializedParams {})
            .context("Sending Initialized notification failed")?;

        info!("Waiting for rust-analyzer indexing...");

        Ok(client)
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.server
            .lock()
            .await
            .shutdown(())
            .await
            .context("Sending Shutdown request failed")?;
        self.server
            .lock()
            .await
            .exit(())
            .context("Sending Exit notification failed")?;

        // Wait for the mainloop to finish. This implicitly waits for the process to exit.
        if let Some(handle) = self.mainloop_handle.lock().await.take() {
            if let Err(e) = handle.await {
                tracing::error!("Error joining LSP mainloop task: {:?}", e);
            }
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub async fn open_file(&self, relative_path: impl AsRef<Path>, text: String) -> Result<()> {
        let uri = self.project.file_uri(relative_path)?;
        self.server
            .lock()
            .await
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "rust".into(), // Assuming Rust, could be made generic
                    version: 0,                 // Start with version 0
                    text,
                },
            })
            .context("Sending DidOpen notification failed")?;
        self.indexed_rx
            .lock()
            .await
            .recv_async()
            .await
            .context("Failed waiting for index")?;
        Ok(())
    }

    pub async fn hover(
        &self,
        file_path: impl AsRef<Path>,
        position: Position,
    ) -> Result<Option<Hover>> {
        let uri = Url::from_file_path(file_path.as_ref())
            .map_err(|_| anyhow::anyhow!("Failed to create file URI from path"))?;
        self.server
            .lock()
            .await
            .hover(HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            })
            .await
            .context("Hover request failed")
    }

    pub async fn find_references(
        &self,
        file_path: impl AsRef<Path>,
        position: Position,
    ) -> Result<Option<Vec<Location>>> {
        let uri = Url::from_file_path(file_path.as_ref())
            .map_err(|_| anyhow::anyhow!("Failed to create file URI from path"))?;
        self.server
            .lock()
            .await
            .references(ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: Default::default(),
                context: ReferenceContext {
                    include_declaration: true,
                },
            })
            .await
            .context("References request failed")
    }

    pub async fn workspace_symbols(
        &self,
        query: String,
    ) -> Result<Option<lsp_types::WorkspaceSymbolResponse>> {
        self.server
            .lock()
            .await
            .request::<WorkspaceSymbolRequest>(WorkspaceSymbolParams {
                query,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await
            .context("Workspace symbols request failed")
    }

    pub async fn code_actions(
        &self,
        file_path: impl AsRef<Path>,
        range: Range,
    ) -> Result<Option<CodeActionResponse>> {
        let uri = Url::from_file_path(file_path.as_ref())
            .map_err(|_| anyhow::anyhow!("Failed to create file URI from path"))?;
        self.server
            .lock()
            .await
            .request::<CodeActionRequest>(CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range,
                context: CodeActionContext::default(),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await
            .context("Code action request failed")
    }

    pub async fn rename(
        &self,
        file_path: impl AsRef<Path>,
        position: Position,
        new_name: String,
    ) -> Result<Option<WorkspaceEdit>> {
        let uri = Url::from_file_path(file_path.as_ref())
            .map_err(|_| anyhow::anyhow!("Failed to create file URI from path"))?;
        self.server
            .lock()
            .await
            .request::<Rename>(RenameParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                new_name,
                work_done_progress_params: Default::default(),
            })
            .await
            .context("Rename request failed")
    }
}
