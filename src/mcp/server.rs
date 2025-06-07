use crate::context::Context as AppContext;
use crate::lsp::format_marked_string;
use crate::mcp::McpNotification;
use crate::mcp::utils::{
    error_response, get_file_lines, resolve_symbol_in_project,
};

use dashmap::DashMap;
use lsp_types::HoverContents;
use rmcp::{
    ServerHandler, model::*, schemars, service::RequestContext as RmcpRequestContext,
    service::RoleServer, tool,
};
use serde::Serialize;
use std::path::PathBuf;

const GUIDANCE_PROMPT: &str = include_str!("guidance_prompt.md");

// Code actions that can be executed
#[derive(Debug, Clone, Serialize)]
struct CodeAction {
    id: String,
    title: String,
    kind: Option<lsp_types::CodeActionKind>,
    workspace_edit: Option<lsp_types::WorkspaceEdit>,
    project_name: String,
    description: String,
}

#[derive(Clone)]
pub struct DevToolsServer {
    context: AppContext,
    last_project: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    code_actions: std::sync::Arc<DashMap<String, CodeAction>>,
    diagnostics: std::sync::Arc<DashMap<String, DiagnosticWithFixes>>,
}

impl DevToolsServer {
    pub fn new(context: AppContext) -> Self {
        Self { 
            context,
            last_project: std::sync::Arc::new(std::sync::RwLock::new(None)),
            code_actions: std::sync::Arc::new(DashMap::new()),
            diagnostics: std::sync::Arc::new(DashMap::new()),
        }
    }
    
    /// 自动更新指定项目的code actions
    async fn auto_update_code_actions(&self, project_path: &PathBuf) -> Result<(), rmcp::Error> {
        let project = self.context.get_project(project_path).await
            .ok_or_else(|| rmcp::Error::internal_error("Project not found".to_string(), None))?;
        
        // 清理该项目的旧code actions
        let project_name = project_path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
            
        // 移除该项目的所有旧actions和diagnostics
        let old_actions: Vec<String> = self.code_actions
            .iter()
            .filter(|entry| entry.value().project_name == project_name)
            .map(|entry| entry.key().clone())
            .collect();
            
        for action_id in old_actions {
            self.code_actions.remove(&action_id);
        }
        
        // 清理该项目的旧diagnostics
        let old_diagnostics: Vec<String> = self.diagnostics
            .iter()
            .filter(|entry| entry.value().file_path.starts_with(&project_name))
            .map(|entry| entry.key().clone())
            .collect();
            
        for diagnostic_id in old_diagnostics {
            self.diagnostics.remove(&diagnostic_id);
        }
        
        // 获取项目诊断信息并生成新的code actions
        match project.cargo_remote.check_structured().await {
            Ok(diagnostics) => {
                let action_count = 0;
                
                for diagnostic in diagnostics {
                    // Extract primary span information
                    if let Some(primary_span) = diagnostic.spans.iter().find(|span| span.is_primary) {
                        // Store diagnostic information
                        let diagnostic_id = format!(
                            "diagnostic_{}_{}_line_{}",
                            primary_span.file_name.replace("\\", "_").replace("/", "_"),
                            primary_span.line_start,
                            primary_span.column_start
                        );
                        
                        let diagnostic_with_fixes = DiagnosticWithFixes {
                            file_path: primary_span.file_name.clone(),
                            severity: diagnostic.level.clone(),
                            message: diagnostic.rendered.clone(),
                            line: primary_span.line_start,
                            character: primary_span.column_start,
                            available_fixes: Vec::new(), // CompilerMessage doesn't have fixes field
                        };
                        
                        self.diagnostics.insert(diagnostic_id, diagnostic_with_fixes);
                    }
                }
                
                // 通知客户端code actions已更新
                let _ = self.context.send_mcp_notification(McpNotification::CodeActionsUpdated {
                    project: project_path.clone(),
                    action_count,
                }).await;
                
                tracing::info!("Auto-updated {} code actions for project: {:?}", action_count, project_path);
                Ok(())
            }
            Err(e) => {
                tracing::warn!("Failed to get diagnostics for auto-update: {}", e);
                Ok(()) // 不要因为诊断失败而中断整个流程
            }
        }
    }
    
    /// 手动刷新所有项目的code actions
    pub async fn refresh_all_code_actions(&self) -> Result<(), rmcp::Error> {
        // 清空所有现有的code actions和diagnostics
        self.code_actions.clear();
        self.diagnostics.clear();
        
        // 暂时跳过全局刷新，因为需要访问私有字段
        // TODO: 在Context中添加公开方法来获取所有项目
        let projects: Vec<PathBuf> = Vec::new();
        
        for project_path in projects {
            if let Err(e) = self.auto_update_code_actions(&project_path).await {
                tracing::error!("Failed to update code actions for {:?}: {}", project_path, e);
            }
        }
        
        Ok(())
    }
    
    // Generate a unique ID for code actions
    fn generate_action_id(&self, operation: &str, target: &str) -> String {
        // Generate simple, descriptive action IDs that LLMs can understand
        format!("{}_{}", operation, target.replace(" ", "_").replace("::", "_"))
    }
    
    async fn get_project_name(&self, project_name: Option<String>) -> Result<String, rmcp::Error> {
        match project_name {
            Some(name) => {
                // Update last_project when a project_name is explicitly provided
                // Use try_write to avoid potential deadlocks
                if let Ok(mut last_project) = self.last_project.try_write() {
                    *last_project = Some(name.clone());
                } else {
                    // If we can't get the write lock immediately, just continue without updating
                    // This prevents deadlocks while still providing the functionality
                    tracing::warn!("Could not update last_project due to lock contention");
                }
                Ok(name)
            },
            None => {
                // First try to get from last_project
                if let Ok(last_project) = self.last_project.try_read() {
                    if let Some(ref name) = *last_project {
                        return Ok(name.clone());
                    }
                }
                
                // If no last_project, try to get any available project
                let projects = self.context.project_descriptions().await;
                if let Some(project) = projects.first() {
                    let project_name = project.name.clone();
                    
                    // Try to update last_project with the found project
                    if let Ok(mut last_project) = self.last_project.try_write() {
                        *last_project = Some(project_name.clone());
                    }
                    
                    Ok(project_name)
                } else {
                    Err(rmcp::Error::invalid_params(
                        "No project_name provided and no projects available. Please use manage_projects with add_project_path parameter to load a project first.".to_string(),
                        None
                    ))
                }
            }
        }
    }


    
    // Smart target location finder using identifier and context

 }

async fn notify_resp(ctx: &AppContext, resp: &CallToolResult, project_path: &PathBuf) {
    let _ = ctx
        .send_mcp_notification(McpNotification::Response {
            content: resp.clone(),
            project: project_path.clone(),
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
        name = "manage_projects",
        description = "Manage projects in the workspace: list all projects, optionally add a new project, or remove an existing project."
    )]
    async fn manage_projects(
        &self,
        #[tool(param)]
        #[schemars(description = "Optional: The absolute root path of a project to add to the workspace. If provided, the project will be loaded first.")]
        add_project_path: Option<String>,
        #[tool(param)]
        #[schemars(description = "Optional: The name of the project to remove from the workspace (e.g., 'cursor-rust-tools'). If not provided, no project will be removed.")]
        remove_project_name: Option<String>,
    ) -> Result<CallToolResult, rmcp::Error> {
        let mut operation_messages = Vec::new();
        
        // Handle project removal first
        if let Some(ref project_name) = remove_project_name {
            let Some(root) = self.context.find_project_by_name(project_name).await else {
                return Ok(error_response(&format!(
                    "Project '{}' not found. Cannot remove non-existent project.",
                    project_name
                )));
            };

            match self.context.remove_project(&root).await {
                Some(_) => {
                    operation_messages.push(format!("Successfully removed project: {}", project_name));
                    
                    // Clear last_project if it was the removed project
                    if let Ok(mut last_project) = self.last_project.try_write() {
                        if let Some(ref last_name) = *last_project {
                            if last_name == project_name {
                                *last_project = None;
                            }
                        }
                    }
                }
                None => {
                    return Ok(error_response(&format!(
                        "Failed to remove project '{}', it might have been removed already.",
                        project_name
                    )));
                }
            }
        }
        
        // Handle project addition
        if let Some(ref path) = add_project_path {
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

            if self.context.get_project(&canonical_path).await.is_none() {
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
                        // Update last_project with the newly added project
                        let project_name = canonical_path.file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("unknown")
                            .to_string();
                        {
                            let mut last_project = self.last_project.write().unwrap();
                            *last_project = Some(project_name.clone());
                        }
                        operation_messages.push(format!("Successfully added project: {} (set as current project)", project_name));
                    }
                    Err(e) => {
                        return Ok(CallToolResult::error(vec![Content::text(format!(
                            "Failed to load project: {}",
                            e
                        ))]));
                    }
                }
            } else {
                operation_messages.push(format!("Project {} is already loaded.", canonical_path.display()));
            }
        }

        // List all projects
        let projects = self.context.project_descriptions().await;

        let mut messages = Vec::new();
        
        // Add operation messages
        for msg in operation_messages {
            messages.push(Content::text(msg));
        }
        
        if projects.is_empty() {
            messages.push(Content::text(
                "No projects currently loaded. Use 'manage_projects' with add_project_path parameter to load one.".to_string(),
            ));
        } else {
            if !messages.is_empty() {
                messages.push(Content::text("".to_string())); // Empty line separator
            }
            messages.push(Content::text("Currently loaded projects:".to_string()));
            
            for project in projects {
                let status = if project.is_indexing_lsp {
                    " (indexing...)"
                } else {
                    " (ready)"
                };
                messages.push(Content::text(format!(
                    "- {} ({}){}",
                    project.name,
                    project.root.display(),
                    status
                )));
            }
        }

        Ok(CallToolResult::success(messages))
    }

    // --- Code Analysis ---

    #[tool(
        name = "get_symbol_info",
        description = "Get comprehensive information (documentation, definition, location) for a symbol within a project."
    )]
    async fn get_symbol_info(
        &self,
        #[tool(param)]
        #[schemars(description = "The name of the project to search in. If not provided, uses the most recently used project.")]
        project_name: Option<String>,
        #[tool(param)]
        #[schemars(description = "The name of the symbol to get information for.")]
        symbol_name: String,
        #[tool(param)]
        #[schemars(description = "Optional file path hint to help locate the symbol more efficiently.")]
        file_hint: Option<String>,
    ) -> Result<CallToolResult, rmcp::Error> {
        let project_name = self.get_project_name(project_name).await?;
        
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
        notify_resp(&self.context, &result, &project_path).await;
        
        // 自动更新该项目的code actions
        if let Err(e) = self.auto_update_code_actions(&project_path).await {
            tracing::warn!("Failed to auto-update code actions after get_symbol_info: {}", e);
        }
        
        Ok(result)
    }

    #[tool(
        name = "find_symbol_usages",
        description = "Find all usages of a symbol across the entire project."
    )]
    async fn find_symbol_usages(
        &self,
        #[tool(param)]
        #[schemars(description = "The name of the project to search in. If not provided, uses the most recently used project.")]
        project_name: Option<String>,
        #[tool(param)]
        #[schemars(description = "The name of the symbol to find usages for.")]
        symbol_name: String,
        #[tool(param)]
        #[schemars(description = "Optional file path hint to help locate the symbol more efficiently.")]
        file_hint: Option<String>,
    ) -> Result<CallToolResult, rmcp::Error> {
        let project_name = self.get_project_name(project_name).await?;
        
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

        notify_resp(&self.context, &result, &project_path).await;
        Ok(result)
    }

    // --- Project Health ---
    #[tool(
        name = "check_project",
        description = "Checks the project for errors/warnings. Returns human-readable messages by default, or structured diagnostics with fixes when include_fixes=true."
    )]
    async fn check_project(
        &self,
        #[tool(param)]
        #[schemars(description = "The name of the project to check for errors and warnings. If not provided, uses the most recently used project.")]
        project_name: Option<String>,
        #[tool(param)]
        #[schemars(description = "Whether to include structured diagnostics with available fixes. Default is false for human-readable output.")]
        include_fixes: Option<bool>,
    ) -> Result<CallToolResult, rmcp::Error> {        
        let project_name = self.get_project_name(project_name).await?;
        
        let Some(project_path) = self.context.find_project_by_name(&project_name).await else {
            return Ok(error_response(&format!(
                "Project '{}' not found.",
                project_name
            )));
        };
        let project = self.context.get_project(&project_path).await.unwrap();

        let include_fixes = include_fixes.unwrap_or(false);

        if include_fixes {
            // Return structured diagnostics with fixes
            let diagnostics = project
                .cargo_remote
                .check_structured()
                .await
                .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;



            // Process diagnostics sequentially for now (async LSP calls don't benefit from rayon)
            // Future optimization: batch LSP requests for better performance
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

            if results.is_empty() {
                let result = CallToolResult::success(vec![Content::text(
                    "Project check passed. No diagnostics found.".to_string(),
                )]);
                notify_resp(&self.context, &result, &project_path).await;
                return Ok(result);
            }

            let result_json = serde_json::to_value(results).map_err(|e| {
                rmcp::Error::internal_error(format!("Failed to serialize results: {}", e), None)
            })?;

            let result = CallToolResult::success(vec![Content::json(result_json)?]);
            notify_resp(&self.context, &result, &project_path).await;
            Ok(result)
        } else {
            // Return human-readable messages
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
            notify_resp(&self.context, &result, &project_path).await;
            Ok(result)
        }
    }

    // apply_workspace_edit tool removed - functionality integrated into confirm_operation

    #[tool(
        name = "rename_symbol",
        description = "Renames a symbol across the entire project. Can either execute immediately or return a preview for confirmation."
    )]
    async fn rename_symbol(
        &self,
        #[tool(param)]
        #[schemars(description = "The name of the project containing the symbol to rename. If not provided, uses the most recently used project.")]
        project_name: Option<String>,
        #[tool(param)]
        #[schemars(description = "The name of the symbol to rename (e.g., function name, struct name, variable name).")]
        symbol_name: String,
        #[tool(param)]
        #[schemars(description = "The new name for the symbol.")]
        new_name: String,
        #[tool(param)]
        #[schemars(description = "Optional file path hint to help locate the symbol more efficiently when there are multiple symbols with the same name.")]
        file_hint: Option<String>,
        #[tool(param)]
        #[schemars(description = "If true, executes the rename immediately. If false, creates a preview that can be executed later with execute_code_action.")]
        execute_immediately: Option<bool>,
    ) -> Result<CallToolResult, rmcp::Error> {
        // No state checking needed anymore
        
        let project_name = self.get_project_name(project_name).await?;
        
        let Some(project_path) = self.context.find_project_by_name(&project_name).await else {
            return Ok(error_response(&format!(
                "Project '{}' not found.",
                project_name
            )));
        };
        let project = self.context.get_project(&project_path).await.unwrap();
        
        // Use resolve_symbol_in_project to find the symbol location
        let symbol_info = match crate::mcp::utils::resolve_symbol_in_project(
            &project,
            &symbol_name,
            file_hint.as_deref(),
        ).await {
            Ok(info) => info,
            Err(e) => {
                return Ok(error_response(&format!(
                    "Failed to locate symbol '{}': {}",
                    symbol_name, e
                )));
            }
        };
        
        // Extract position from symbol location
        let absolute_path = match symbol_info.location.uri.to_file_path() {
            Ok(path) => path,
            Err(_) => {
                return Ok(error_response(&format!(
                    "Invalid file path for symbol '{}'",
                    symbol_name
                )));
            }
        };
        
        let position = symbol_info.location.range.start;

        let edit = project.lsp.rename(&absolute_path, position, new_name.clone()).await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?
            .ok_or_else(|| rmcp::Error::internal_error("Could not perform rename operation. The symbol at the given location may not be renameable.", None))?;

        let execute_now = execute_immediately.unwrap_or(false);
        
        if execute_now {
            // Execute immediately
            match crate::mcp::utils::apply_workspace_edit(&edit) {
                Ok(()) => {
                    let result_json = serde_json::json!({
                        "status": "completed",
                        "operation": "rename",
                        "symbol_name": symbol_name,
                        "new_name": new_name,
                        "changes_count": edit.changes.as_ref().map(|c| c.len()).unwrap_or(0),
                        "files_affected": edit.changes.as_ref().map(|changes| {
                            changes.keys().map(|uri| uri.to_string()).collect::<Vec<_>>()
                        }).unwrap_or_default(),
                        "message": format!("✓ Successfully renamed '{}' to '{}'", symbol_name, new_name)
                    });
                    
                    let result = CallToolResult::success(vec![Content::json(result_json)?]);
                    notify_resp(&self.context, &result, &project_path).await;
                    
                    // 自动更新该项目的code actions，因为代码已被修改
                    if let Err(e) = self.auto_update_code_actions(&project_path).await {
                        tracing::warn!("Failed to auto-update code actions after rename_symbol: {}", e);
                    }
                    
                    Ok(result)
                }
                Err(e) => {
                    Ok(error_response(&format!(
                        "Failed to rename '{}' to '{}': {}",
                        symbol_name, new_name, e
                    )))
                }
            }
        } else {
            // Create preview for later execution
            let action_id = self.generate_action_id("rename", &format!("{}_to_{}", symbol_name, new_name));
            let code_action = CodeAction {
                id: action_id.clone(),
                title: format!("Rename '{}' to '{}'", symbol_name, new_name),
                kind: Some(lsp_types::CodeActionKind::REFACTOR),
                workspace_edit: Some(edit.clone()),
                project_name: project_name.clone(),
                description: format!("Rename symbol '{}' to '{}'", symbol_name, new_name),
            };
            
            // Store the code action
            self.code_actions.insert(action_id.clone(), code_action);

            let result_json = serde_json::json!({
                "status": "preview",
                "action_id": action_id,
                "operation": "rename",
                "symbol_name": symbol_name,
                "new_name": new_name,
                "changes_count": edit.changes.as_ref().map(|c| c.len()).unwrap_or(0),
                "files_affected": edit.changes.as_ref().map(|changes| {
                    changes.keys().map(|uri| uri.to_string()).collect::<Vec<_>>()
                }).unwrap_or_default(),
                "message": format!("Created rename preview. Use execute_code_action('{}') to apply changes.", action_id)
            });

            let result = CallToolResult::success(vec![Content::json(result_json)?]);
            notify_resp(&self.context, &result, &project_path).await;
            Ok(result)
        }
    }

    #[tool(
        name = "refresh_code_actions",
        description = "Manually refresh code actions for a project by analyzing current diagnostics and generating available fixes."
    )]
    async fn refresh_code_actions(
        &self,
        #[tool(param)]
        #[schemars(description = "The name of the project to refresh code actions for. If not provided, refreshes all projects.")]
        project_name: Option<String>,
    ) -> Result<CallToolResult, rmcp::Error> {
        if let Some(name) = project_name {
            let project_name = self.get_project_name(Some(name)).await?;
            
            let Some(project_path) = self.context.find_project_by_name(&project_name).await else {
                return Ok(error_response(&format!(
                    "Project '{}' not found.",
                    project_name
                )));
            };
            
            self.auto_update_code_actions(&project_path).await?;
            
            let action_count = self.code_actions
                .iter()
                .filter(|entry| entry.value().project_name == project_name)
                .count();
                
            let result_json = serde_json::json!({
                "status": "success",
                "project": project_name,
                "action_count": action_count,
                "message": format!("Refreshed {} code actions for project '{}'", action_count, project_name)
            });
            
            let result = CallToolResult::success(vec![Content::json(result_json)?]);
            notify_resp(&self.context, &result, &project_path).await;
            Ok(result)
        } else {
            self.refresh_all_code_actions().await?;
            
            let total_actions = self.code_actions.len();
            let result_json = serde_json::json!({
                "status": "success",
                "action_count": total_actions,
                "message": format!("Refreshed code actions for all projects. Total: {} actions", total_actions)
            });
            
            let result = CallToolResult::success(vec![Content::json(result_json)?]);
            // 对于全局刷新，我们不发送项目特定的通知
            Ok(result)
        }
    }

    #[tool(
        name = "test_project",
        description = "Runs `cargo test` on a project. Can run all tests or a specific one."
    )]
    async fn test_project(
        &self,
        #[tool(param)]
        #[schemars(description = "The name of the project to run tests for. If not provided, uses the most recently used project.")]
        project_name: Option<String>,
        #[tool(param)]
        #[schemars(description = "Optional specific test name to run. If not provided, all tests will be run.")]
        test_name: Option<String>,
        #[tool(param)]
        #[schemars(description = "Whether to enable backtrace for test failures. Defaults to false.")]
        backtrace: Option<bool>,
    ) -> Result<CallToolResult, rmcp::Error> {
        // No state checking needed anymore
        
        let project_name = self.get_project_name(project_name).await?;
        
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
        notify_resp(&self.context, &result, &project_path).await;
        Ok(result)
    }
    
    #[tool(
        name = "list_code_actions",
        description = "List all available code actions that can be executed."
    )]
    async fn list_code_actions(&self) -> Result<CallToolResult, rmcp::Error> {
        if self.code_actions.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No code actions available.".to_string()
            )]));
        }
        
        let actions_list: Vec<serde_json::Value> = self.code_actions.iter().map(|entry| {
            let action = entry.value();
            serde_json::json!({
                "id": action.id,
                "title": action.title,
                "description": action.description,
                "kind": action.kind,
                "project_name": action.project_name
            })
        }).collect();
        
        let result_json = serde_json::json!({
            "code_actions": actions_list,
            "count": self.code_actions.len()
        });
        
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result_json).unwrap()
        )]))
    }
    


    #[tool(
        name = "execute_code_action",
        description = "Execute a code action by its ID. This applies the workspace edit associated with the code action."
    )]
    async fn execute_code_action(
        &self,
        #[tool(param)]
        #[schemars(description = "The ID of the code action to execute.")]
        action_id: String,
    ) -> Result<CallToolResult, rmcp::Error> {
        let Some((_, action)) = self.code_actions.remove(&action_id) else {
            return Ok(error_response(&format!("Code action with ID '{}' not found.", action_id)));
        };
        
        let Some(workspace_edit) = action.workspace_edit else {
            return Ok(error_response(&format!("Code action '{}' has no workspace edit to apply.", action_id)));
        };
        
        match crate::mcp::utils::apply_workspace_edit(&workspace_edit) {
            Ok(()) => {
                let result = CallToolResult::success(vec![Content::text(format!(
                    "✓ Executed code action '{}': {}",
                    action.title, action.description
                ))]);
                
                // Find project path for notification and auto-update
                if let Some(project_path) = self.context.find_project_by_name(&action.project_name).await {
                    notify_resp(&self.context, &result, &project_path).await;
                    
                    // 自动更新该项目的code actions，因为代码已被修改
                    if let Err(e) = self.auto_update_code_actions(&project_path).await {
                        tracing::warn!("Failed to auto-update code actions after execute_code_action: {}", e);
                    }
                }
                
                Ok(result)
            }
            Err(e) => {
                Ok(error_response(&format!(
                    "Failed to execute code action '{}': {}",
                    action.title, e
                )))
            }
        }
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
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: Some(true),
                }),
                ..Default::default()
            },
            instructions: Some(GUIDANCE_PROMPT.to_string()),
            ..Default::default()
        }
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RmcpRequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, rmcp::Error> {
        let mut resources = Vec::new();
        
        // Add code actions as resources
        for entry in self.code_actions.iter() {
            let action = entry.value();
            resources.push(Resource {
                raw: RawResource {
                    uri: format!("code-action://{}", action.id),
                    name: action.title.clone(),
                    description: Some(action.description.clone()),
                    mime_type: Some("application/json".to_string()),
                    size: None,
                },
                annotations: None,
            });
        }
        
        // Add diagnostics as resources
        for entry in self.diagnostics.iter() {
            let diagnostic = entry.value();
            resources.push(Resource {
                raw: RawResource {
                    uri: format!("diagnostic://{}", entry.key()),
                    name: format!("Diagnostic: {}", diagnostic.message),
                    description: Some(format!("{}:{} - {} ({})", 
                        diagnostic.file_path, 
                        diagnostic.line, 
                        diagnostic.message,
                        diagnostic.severity
                    )),
                    mime_type: Some("application/json".to_string()),
                    size: None,
                },
                annotations: None,
            });
        }
        
        Ok(ListResourcesResult {
            resources,
            next_cursor: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParam,
        _context: RmcpRequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, rmcp::Error> {
        if let Some(action_id) = request.uri.strip_prefix("code-action://") {
            if let Some(action_ref) = self.code_actions.get(action_id) {
                let action = action_ref.value();
                let content = serde_json::to_string_pretty(action)
                    .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;
                
                Ok(ReadResourceResult {
                    contents: vec![ResourceContents::TextResourceContents {
                        uri: request.uri,
                        mime_type: Some("application/json".to_string()),
                        text: content,
                    }],
                })
            } else {
                Err(rmcp::Error::invalid_params(
                    format!("Code action with ID '{}' not found", action_id),
                    None,
                ))
            }
        } else if let Some(diagnostic_id) = request.uri.strip_prefix("diagnostic://") {
            if let Some(diagnostic_ref) = self.diagnostics.get(diagnostic_id) {
                let diagnostic = diagnostic_ref.value();
                let content = serde_json::to_string_pretty(diagnostic)
                    .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;
                
                Ok(ReadResourceResult {
                    contents: vec![ResourceContents::TextResourceContents {
                        uri: request.uri,
                        mime_type: Some("application/json".to_string()),
                        text: content,
                    }],
                })
            } else {
                Err(rmcp::Error::invalid_params(
                    format!("Diagnostic with ID '{}' not found", diagnostic_id),
                    None,
                ))
            }
        } else {
            Err(rmcp::Error::invalid_params(
                format!("Invalid resource URI: {}", request.uri),
                None,
            ))
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
