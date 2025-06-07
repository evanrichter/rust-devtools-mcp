use crate::context::Context as AppContext;
use crate::lsp::format_marked_string;
use crate::mcp::McpNotification;
use crate::mcp::utils::{
    apply_workspace_edit, error_response, get_file_lines, resolve_symbol_in_project,
};
use lsp_types::{HoverContents, WorkspaceEdit};
use rmcp::{
    ServerHandler, model::*, schemars, service::RequestContext as RmcpRequestContext,
    service::RoleServer, tool,
};
use serde::Serialize;
use std::path::PathBuf;

const GUIDANCE_PROMPT: &str = include_str!("guidance_prompt.md");

#[derive(Clone)]
pub struct DevToolsServer {
    context: AppContext,
}

impl DevToolsServer {
    pub fn new(context: AppContext) -> Self {
        Self { context }
    }
}

async fn notify_resp(ctx: &AppContext, resp: &CallToolResult, project_path: PathBuf) {
    let _ = ctx
        .send_mcp_notification(McpNotification::Response {
            content: resp.clone(),
            project: project_path,
        })
        .await;
}

#[derive(Serialize)]
struct Fix {
    title: String,
    kind: Option<lsp_types::CodeActionKind>,
    edit_to_apply: Option<lsp_types::WorkspaceEdit>,
}

#[derive(Serialize)]
struct DiagnosticWithFixes {
    file_path: String,
    severity: String,
    message: String,
    line: usize,
    character: usize,
    available_fixes: Vec<Fix>,
}

#[tool(tool_box)]
impl DevToolsServer {
    // --- Project Management ---
    #[tool(
        name = "add_project",
        description = "Loads a new Rust project into the workspace by its absolute root path. This is required before other tools can operate on it."
    )]
    async fn add_project(
        &self,
        #[tool(param)]
        #[schemars(description = "The absolute root path of the project to load.")]
        path: String,
    ) -> Result<CallToolResult, rmcp::Error> {
        let canonical_path =
            match PathBuf::from(shellexpand::tilde(&path).to_string()).canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid project path '{}': {}",
                        path, e
                    ))]));
                }
            };

        if self.context.get_project(&canonical_path).await.is_some() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Project {} is already loaded.",
                canonical_path.display()
            ))]));
        }

        let project = match crate::project::Project::new(&canonical_path) {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Failed to initialize project: {}",
                    e
                ))]));
            }
        };

        match self.context.add_project(project).await {
            Ok(_) => {
                let message = format!(
                    "Successfully loaded new project: {}",
                    canonical_path.display()
                );
                Ok(CallToolResult::success(vec![Content::text(message)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Failed to load project: {}",
                e
            ))])),
        }
    }

    #[tool(
        name = "remove_project",
        description = "Remove a project from the workspace by its name."
    )]
    async fn remove_project(
        &self,
        #[tool(param)]
        #[schemars(description = "The name of the project to remove (e.g., 'cursor-rust-tools').")]
        project_name: String,
    ) -> Result<CallToolResult, rmcp::Error> {
        let Some(root) = self.context.find_project_by_name(&project_name).await else {
            return Ok(error_response(&format!(
                "Project '{}' not found. Use 'list_projects' to see available projects.",
                project_name
            )));
        };

        match self.context.remove_project(&root).await {
            Some(_) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Successfully removed project: {}",
                project_name
            ))])),
            None => Ok(error_response(&format!(
                "Failed to remove project '{}', it might have been removed already.",
                project_name
            ))),
        }
    }

    #[tool(
        name = "list_projects",
        description = "List all projects currently loaded in the workspace."
    )]
    async fn list_projects(&self) -> Result<CallToolResult, rmcp::Error> {
        let projects = self.context.project_descriptions().await;

        if projects.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No projects loaded. Use 'add_project' to load one.".to_string(),
            )]));
        }

        let messages = projects
            .into_iter()
            .map(|project| {
                let status = if project.is_indexing_lsp {
                    " (indexing...)"
                } else {
                    " (ready)"
                };
                Content::text(format!(
                    "- {} ({}){}",
                    project.name,
                    project.root.display(),
                    status
                ))
            })
            .collect::<Vec<Content>>();

        let result = CallToolResult::success(messages);
        Ok(result)
    }

    // --- Code Analysis ---

    #[tool(
        name = "get_symbol_info",
        description = "Get comprehensive information (documentation, definition, location) for a symbol within a project."
    )]
    async fn get_symbol_info(
        &self,
        #[tool(param)] project_name: String,
        #[tool(param)] symbol_name: String,
        #[tool(param)] file_hint: Option<String>,
    ) -> Result<CallToolResult, rmcp::Error> {
        let Some(project_path) = self.context.find_project_by_name(&project_name).await else {
            return Ok(error_response(&format!(
                "Project '{}' not found.",
                project_name
            )));
        };
        let project = self.context.get_project(&project_path).await.unwrap();

        let symbol_info =
            match resolve_symbol_in_project(&project, &symbol_name, file_hint.as_deref()).await {
                Ok(info) => info,
                Err(e) => return Ok(error_response(&e)),
            };

        let file_path = symbol_info.location.uri.to_file_path().map_err(|_| {
            rmcp::Error::internal_error("Invalid file path in symbol location", None)
        })?;

        let hover = project
            .lsp
            .hover(&file_path, symbol_info.location.range.start)
            .await
            .unwrap_or(None);
        let documentation = hover.map_or_else(
            || "No documentation found.".to_string(),
            |h| match h.contents {
                HoverContents::Scalar(s) => format_marked_string(&s),
                HoverContents::Array(a) => a
                    .into_iter()
                    .map(|s| format_marked_string(&s))
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n"),
                HoverContents::Markup(m) => m.value,
            },
        );

        let definition_code = get_file_lines(
            &file_path,
            symbol_info.location.range.start.line,
            symbol_info.location.range.end.line,
            2,
            5,
        )
        .unwrap_or(None)
        .unwrap_or_else(|| "Could not read source file.".to_string());

        let result_json = serde_json::json!({
            "symbol": symbol_info.name,
            "kind": format!("{:?}", symbol_info.kind),
            "file_path": file_path.display().to_string(),
            "position": {
                "start_line": symbol_info.location.range.start.line,
                "end_line": symbol_info.location.range.end.line,
            },
            "documentation": documentation,
            "definition_code": definition_code,
        });

        let result = CallToolResult::success(vec![Content::json(result_json)?]);
        notify_resp(&self.context, &result, project_path).await;
        Ok(result)
    }

    #[tool(
        name = "find_symbol_usages",
        description = "Find all usages of a symbol across the entire project."
    )]
    async fn find_symbol_usages(
        &self,
        #[tool(param)] project_name: String,
        #[tool(param)] symbol_name: String,
        #[tool(param)] file_hint: Option<String>,
    ) -> Result<CallToolResult, rmcp::Error> {
        let Some(project_path) = self.context.find_project_by_name(&project_name).await else {
            return Ok(error_response(&format!(
                "Project '{}' not found.",
                project_name
            )));
        };
        let project = self.context.get_project(&project_path).await.unwrap();

        let symbol_info =
            match resolve_symbol_in_project(&project, &symbol_name, file_hint.as_deref()).await {
                Ok(info) => info,
                Err(e) => return Ok(error_response(&e)),
            };

        let symbol_file_path = symbol_info.location.uri.to_file_path().map_err(|_| {
            rmcp::Error::internal_error("Invalid file path in symbol location", None)
        })?;

        let references = project
            .lsp
            .find_references(&symbol_file_path, symbol_info.location.range.start)
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?
            .ok_or_else(|| rmcp::Error::internal_error("No references found", None))?;

        let messages = references
            .into_iter()
            .filter_map(|reference| {
                let Ok(ref_path) = reference.uri.to_file_path() else {
                    return None;
                };
                let Ok(Some(lines)) = get_file_lines(
                    &ref_path,
                    reference.range.start.line,
                    reference.range.end.line,
                    3,
                    3,
                ) else {
                    return None;
                };
                Some(Content::text(format!(
                    "### {}\n(Line: {})\n```rust\n{}\n```",
                    ref_path.display(),
                    reference.range.start.line + 1,
                    lines
                )))
            })
            .collect::<Vec<Content>>();

        let result = if messages.is_empty() {
            CallToolResult::success(vec![Content::text("No usages found.".to_string())])
        } else {
            CallToolResult::success(messages)
        };

        notify_resp(&self.context, &result, project_path).await;
        Ok(result)
    }

    // --- Project Health ---
    #[tool(
        name = "check_project",
        description = "Runs `cargo check` and returns a human-readable list of errors and warnings. For programmatic access to fixes, use the more powerful `get_diagnostics_with_fixes` tool."
    )]
    async fn check_project(
        &self,
        #[tool(param)] project_name: String,
    ) -> Result<CallToolResult, rmcp::Error> {
        let Some(project_path) = self.context.find_project_by_name(&project_name).await else {
            return Ok(error_response(&format!(
                "Project '{}' not found.",
                project_name
            )));
        };
        let project = self.context.get_project(&project_path).await.unwrap();

        let rendered_messages = project
            .cargo_remote
            .check_rendered()
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;

        if rendered_messages.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Project check passed. No errors or warnings.".to_string(),
            )]));
        }

        let result =
            CallToolResult::success(rendered_messages.into_iter().map(Content::text).collect());
        notify_resp(&self.context, &result, project_path).await;
        Ok(result)
    }

    #[tool(
        name = "get_diagnostics_with_fixes",
        description = "Checks the project for errors/warnings and automatically finds available quick fixes for each. This is the primary tool for identifying and fixing problems."
    )]
    async fn get_diagnostics_with_fixes(
        &self,
        #[tool(param)] project_name: String,
    ) -> Result<CallToolResult, rmcp::Error> {
        let Some(project_path) = self.context.find_project_by_name(&project_name).await else {
            return Ok(error_response(&format!(
                "Project '{}' not found.",
                project_name
            )));
        };
        let project = self.context.get_project(&project_path).await.unwrap();

        let diagnostics = project
            .cargo_remote
            .check_structured()
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;

        if diagnostics.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Project check passed. No diagnostics found.".to_string(),
            )]));
        }

        let mut results = Vec::new();

        for diag in diagnostics {
            if let Some(span) = diag.spans.iter().find(|s| s.is_primary) {
                let absolute_path = project.project.root().join(&span.file_name);
                let range = lsp_types::Range {
                    start: lsp_types::Position {
                        line: span.line_start.saturating_sub(1) as u32,
                        character: span.column_start.saturating_sub(1) as u32,
                    },
                    end: lsp_types::Position {
                        line: span.line_end.saturating_sub(1) as u32,
                        character: span.column_end.saturating_sub(1) as u32,
                    },
                };

                let available_fixes = if let Ok(Some(actions)) =
                    project.lsp.code_actions(&absolute_path, range).await
                {
                    actions
                        .into_iter()
                        .filter_map(|action_or_cmd| {
                            if let lsp_types::CodeActionOrCommand::CodeAction(action) =
                                action_or_cmd
                            {
                                Some(Fix {
                                    title: action.title,
                                    kind: action.kind,
                                    edit_to_apply: action.edit,
                                })
                            } else {
                                None
                            }
                        })
                        .collect()
                } else {
                    vec![]
                };

                results.push(DiagnosticWithFixes {
                    file_path: span.file_name.clone(),
                    severity: diag.level.clone(),
                    message: diag.rendered.clone(),
                    line: span.line_start,
                    character: span.column_start,
                    available_fixes,
                });
            }
        }

        let result_json = serde_json::to_value(results).map_err(|e| {
            rmcp::Error::internal_error(format!("Failed to serialize results: {}", e), None)
        })?;

        let result = CallToolResult::success(vec![Content::json(result_json)?]);
        notify_resp(&self.context, &result, project_path).await;
        Ok(result)
    }

    #[tool(
        name = "apply_workspace_edit",
        description = "Applies a `WorkspaceEdit` JSON object to the workspace. This is the final step for code modification tools like `rename_symbol` or `get_diagnostics_with_fixes`."
    )]
    async fn apply_workspace_edit(
        &self,
        #[tool(param)]
        #[schemars(description = "A JSON object representing the LSP `WorkspaceEdit` to apply.")]
        edit: serde_json::Value,
    ) -> Result<CallToolResult, rmcp::Error> {
        let workspace_edit: WorkspaceEdit = serde_json::from_value(edit.clone()).map_err(|e| {
            rmcp::Error::invalid_params(format!("Invalid WorkspaceEdit JSON: {}", e), Some(edit))
        })?;

        match apply_workspace_edit(&workspace_edit) {
            Ok(_) => Ok(CallToolResult::success(vec![Content::text(
                "Workspace edit applied successfully.".to_string(),
            )])),
            Err(e) => Ok(error_response(&format!(
                "Failed to apply workspace edit: {}",
                e
            ))),
        }
    }

    #[tool(
        name = "rename_symbol",
        description = "Prepares a `WorkspaceEdit` for renaming a symbol across the entire project. The returned edit must be applied with `apply_workspace_edit`."
    )]
    async fn rename_symbol(
        &self,
        #[tool(param)] project_name: String,
        #[tool(param)] file_path: String,
        #[tool(param)] line: u32,
        #[tool(param)] character: u32,
        #[tool(param)] new_name: String,
    ) -> Result<CallToolResult, rmcp::Error> {
        let Some(project_path) = self.context.find_project_by_name(&project_name).await else {
            return Ok(error_response(&format!(
                "Project '{}' not found.",
                project_name
            )));
        };
        let project = self.context.get_project(&project_path).await.unwrap();
        let absolute_path = project.project.root().join(&file_path);
        let position = lsp_types::Position { line, character };

        let edit = project.lsp.rename(&absolute_path, position, new_name).await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?
            .ok_or_else(|| rmcp::Error::internal_error("Could not perform rename operation. The symbol at the given location may not be renameable.", None))?;

        let result_json = serde_json::json!({
            "description": "WorkspaceEdit to perform the rename operation. Apply this with the `apply_workspace_edit` tool.",
            "edit_to_apply": edit,
        });

        let result = CallToolResult::success(vec![Content::json(result_json)?]);
        notify_resp(&self.context, &result, project_path).await;
        Ok(result)
    }

    #[tool(
        name = "test_project",
        description = "Runs `cargo test` on a project. Can run all tests or a specific one."
    )]
    async fn test_project(
        &self,
        #[tool(param)] project_name: String,
        #[tool(param)] test_name: Option<String>,
        #[tool(param)] backtrace: Option<bool>,
    ) -> Result<CallToolResult, rmcp::Error> {
        let Some(project_path) = self.context.find_project_by_name(&project_name).await else {
            return Ok(error_response(&format!(
                "Project '{}' not found.",
                project_name
            )));
        };
        let project = self.context.get_project(&project_path).await.unwrap();

        let messages = project
            .cargo_remote
            .test(test_name, backtrace.unwrap_or(false))
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?
            .into_iter()
            .map(Content::text)
            .collect::<Vec<Content>>();

        let result = CallToolResult::success(messages);
        notify_resp(&self.context, &result, project_path).await;
        Ok(result)
    }
}

#[tool(tool_box)]
impl ServerHandler for DevToolsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::default(),
            server_info: Implementation {
                name: "rust-devtools-mcp".to_string(),
                version: "0.3.0-smart-diagnostics".to_string(),
            },
            instructions: Some(GUIDANCE_PROMPT.to_string()),
            ..Default::default()
        }
    }

    fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RmcpRequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListPromptsResult, rmcp::Error>> + Send + '_ {
        std::future::ready(Ok(ListPromptsResult {
            prompts: vec![Prompt {
                name: "rust_development_guidance".to_string(),
                description: Some(
                    "Comprehensive guidance for Rust development using the refactored rust-devtools-mcp"
                        .to_string(),
                ),
                arguments: None,
            }],
            next_cursor: None,
        }))
    }

    fn get_prompt(
        &self,
        request: GetPromptRequestParam,
        _context: RmcpRequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<GetPromptResult, rmcp::Error>> + Send + '_ {
        match request.name.as_str() {
            "rust_development_guidance" => std::future::ready(Ok(GetPromptResult {
                description: Some(
                    "Guidance for using the refactored Rust development tools effectively"
                        .to_string(),
                ),
                messages: vec![PromptMessage {
                    role: PromptMessageRole::User,
                    content: PromptMessageContent::Text {
                        text: GUIDANCE_PROMPT.to_string(),
                    },
                }],
            })),
            _ => std::future::ready(Err(rmcp::Error::internal_error(
                format!("Unknown prompt: {}", request.name),
                None,
            ))),
        }
    }
}
