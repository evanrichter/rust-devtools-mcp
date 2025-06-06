use crate::context::Context;
use crate::lsp::{format_marked_string, get_location_contents};
use crate::mcp::McpNotification;
use crate::mcp::utils::{
    RequestExtension, error_response_v2, find_symbol_position_in_file, get_file_lines,
    get_info_from_request,
};
use fuzzt::get_top_n;
use lsp_types::HoverContents;
use rmcp::{ServerHandler, model::*, schemars, service::RequestContext, service::RoleServer, tool};
use std::collections::HashMap;
use std::path::PathBuf;

// Use include_str! to load the guidance prompt from an external file at compile time.
const GUIDANCE_PROMPT: &str = include_str!("guidance_prompt.md");

#[derive(Clone)]
pub struct DevToolsServer {
    context: Context,
}

impl DevToolsServer {
    pub fn new(context: Context) -> Self {
        Self { context }
    }
}

// Helper for notifications
async fn notify_req(ctx: &Context, req: &CallToolRequestParam, path: PathBuf) {
    tracing::info!("MCP Request for project {}: {}", path.display(), req.name);
    let _ = ctx
        .send_mcp_notification(McpNotification::Request {
            content: req.clone(),
            project: path,
        })
        .await;
}
async fn notify_resp(ctx: &Context, resp: &CallToolResult, path: PathBuf) {
    tracing::info!(
        "MCP Response for project {}: success={}",
        path.display(),
        resp.is_error.is_none()
    );
    let _ = ctx
        .send_mcp_notification(McpNotification::Response {
            content: resp.clone(),
            project: path,
        })
        .await;
}

#[tool(tool_box)]
impl DevToolsServer {
    // --- Project Management ---
    #[tool(
        name = "ensure_project_is_loaded",
        description = "Ensures a project is loaded into the workspace. If not already present, it will be added. This is safe to call multiple times."
    )]
    async fn ensure_project_is_loaded(
        &self,
        #[tool(param)]
        #[schemars(description = "The absolute root path of the project to load.")]
        project_path: String,
    ) -> Result<CallToolResult, rmcp::Error> {
        let path = match PathBuf::from(shellexpand::tilde(&project_path).to_string()).canonicalize()
        {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid project path '{}': {}",
                    project_path, e
                ))]));
            }
        };

        // 1. Check if project is already loaded
        if self.context.get_project(&path).await.is_some() {
            let message = format!("Project {} is already loaded.", path.display());
            return Ok(CallToolResult::success(vec![Content::text(message)]));
        }

        // 2. If not loaded, perform validation and add it
        if !path.is_dir() {
            return Ok(CallToolResult::error(vec![Content::text(
                "Project path must be a directory".to_string(),
            )]));
        }

        let project = match crate::project::Project::new(&path) {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Failed to create project: {}",
                    e
                ))]));
            }
        };

        match self.context.add_project(project).await {
            Ok(_) => {
                let message = format!("Successfully loaded new project: {}", path.display());
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
        description = "Remove a project from the workspace by specifying its root path or project name"
    )]
    async fn remove_project(
        &self,
        #[tool(param)]
        #[schemars(description = "The root path or project name to remove")]
        project_path: String,
    ) -> Result<CallToolResult, rmcp::Error> {
        match self.context.remove_project_by_path_or_name(&project_path).await {
            Some(_) => {
                let message = format!("Successfully removed project: {}", project_path);
                Ok(CallToolResult::success(vec![Content::text(message)]))
            }
            #[allow(non_snake_case)]
            None => Ok(CallToolResult::error(vec![Content::text(format!(
                "Project not found: '{}'. Use 'list_projects' to see available projects.",
                project_path
            ))])),
        }
    }

    #[tool(
        name = "list_projects",
        description = "List all projects currently in the workspace"
    )]
    async fn list_projects(&self) -> Result<CallToolResult, rmcp::Error> {
        let projects = self.context.project_descriptions().await;

        if projects.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No projects found in workspace. Use 'ensure_project_is_loaded' to add one."
                    .to_string(),
            )]));
        }

        let mut result = String::from("Projects in workspace:\n");
        for project in projects {
            let status = if project.is_indexing_lsp {
                " (indexing...)"
            } else {
                " (ready)"
            };
            result.push_str(&format!(
                "- {} ({}){}\n",
                project.name,
                project.root.display(),
                status
            ));
        }

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    // --- cargo_check ---
    #[tool(
        name = "cargo_check",
        description = "Run the cargo check command in this project. Returns the response in JSON format"
    )]
    async fn cargo_check(
        &self,
        #[tool(aggr)] args: CallToolRequestParam,
    ) -> Result<CallToolResult, rmcp::Error> {
        let file = args.get_file()?;
        let (project, _, absolute_file) = match get_info_from_request(&self.context, &file).await {
            Ok(info) => info,
            Err(e) => return Ok(error_response_v2(&e)),
        };
        notify_req(&self.context, &args, absolute_file.clone()).await;

        let only_errors = args
            .arguments
            .as_ref()
            .and_then(|a| a.get("only_errors"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let messages = project
            .cargo_remote
            .check(only_errors)
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;

        let response_message = serde_json::to_string_pretty(&messages)
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;
        let result = CallToolResult::success(vec![Content::text(response_message)]);
        notify_resp(&self.context, &result, absolute_file).await;
        Ok(result)
    }

    // --- cargo_test ---
    #[tool(
        name = "cargo_test",
        description = "Run the cargo test command in this project. Returns the response in JSON format"
    )]
    async fn cargo_test(
        &self,
        #[tool(aggr)] args: CallToolRequestParam,
    ) -> Result<CallToolResult, rmcp::Error> {
        let file = args.get_file()?;
        let (project, _, absolute_file) = match get_info_from_request(&self.context, &file).await {
            Ok(info) => info,
            Err(e) => return Ok(error_response_v2(&e)),
        };
        notify_req(&self.context, &args, absolute_file.clone()).await;

        let test = args
            .arguments
            .as_ref()
            .and_then(|a| a.get("test"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let backtrace = args
            .arguments
            .as_ref()
            .and_then(|a| a.get("backtrace"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let messages = project
            .cargo_remote
            .test(test, backtrace)
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;

        let result = CallToolResult::success(vec![Content::text(messages.join("\n\n"))]);
        notify_resp(&self.context, &result, absolute_file).await;
        Ok(result)
    }

    // --- symbol_docs ---
    #[tool(
        name = "symbol_docs",
        description = "Get the documentation for a symbol"
    )]
    async fn symbol_docs(
        &self,
        #[tool(aggr)] args: CallToolRequestParam,
    ) -> Result<CallToolResult, rmcp::Error> {
        let file = args.get_file()?;
        let (project, relative_file, absolute_file) =
            match get_info_from_request(&self.context, &file).await {
                Ok(info) => info,
                Err(e) => return Ok(error_response_v2(&e)),
            };

        notify_req(&self.context, &args, absolute_file.clone()).await;

        let line = args.get_line()?;
        let symbol = args.get_symbol()?;
        let position = find_symbol_position_in_file(&project, &relative_file, &symbol, line)
            .await
            .map_err(|e| rmcp::Error::invalid_params(e, None))?;

        let hover = project
            .lsp
            .hover(&relative_file, position)
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?
            .ok_or_else(|| rmcp::Error::internal_error("No hover information found", None))?;

        let response_text = match hover.contents {
            HoverContents::Scalar(s) => format_marked_string(&s),
            HoverContents::Array(a) => a
                .into_iter()
                .map(|s| format_marked_string(&s))
                .collect::<Vec<_>>()
                .join("\n"),
            HoverContents::Markup(m) => m.value,
        };

        let result = CallToolResult::success(vec![Content::text(response_text)]);
        notify_resp(&self.context, &result, absolute_file).await;
        Ok(result)
    }

    // --- symbol_impl ---
    #[tool(
        name = "symbol_impl",
        description = "Get the implementation for a symbol. If the implementation is in multiple files, will return multiple files. Will return the full file that contains the implementation including other contents of the file."
    )]
    async fn symbol_impl(
        &self,
        #[tool(aggr)] args: CallToolRequestParam,
    ) -> Result<CallToolResult, rmcp::Error> {
        let file = args.get_file()?;
        let (project, relative_file, absolute_file) =
            match get_info_from_request(&self.context, &file).await {
                Ok(info) => info,
                Err(e) => return Ok(error_response_v2(&e)),
            };
        notify_req(&self.context, &args, absolute_file.clone()).await;

        let line = args.get_line()?;
        let symbol = args.get_symbol()?;
        let position = find_symbol_position_in_file(&project, &relative_file, &symbol, line)
            .await
            .map_err(|e| rmcp::Error::invalid_params(e, None))?;

        let type_definition = project
            .lsp
            .type_definition(&relative_file, position)
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?
            .ok_or_else(|| rmcp::Error::internal_error("No type definition found", None))?;

        let contents = get_location_contents(type_definition)
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?
            .iter()
            .map(|(content, path)| format!("## {}\n```rust\n{}\n```", path.display(), content))
            .collect::<Vec<_>>()
            .join("\n");

        let result = CallToolResult::success(vec![Content::text(contents)]);
        notify_resp(&self.context, &result, absolute_file).await;
        Ok(result)
    }

    // --- symbol_references ---
    #[tool(
        name = "symbol_references",
        description = "Get all the references for a symbol. Will return a list of files that contain the symbol including a preview of the usage."
    )]
    async fn symbol_references(
        &self,
        #[tool(aggr)] args: CallToolRequestParam,
    ) -> Result<CallToolResult, rmcp::Error> {
        let file = args.get_file()?;
        let (project, relative_file, absolute_file) =
            match get_info_from_request(&self.context, &file).await {
                Ok(info) => info,
                Err(e) => return Ok(error_response_v2(&e)),
            };
        notify_req(&self.context, &args, absolute_file.clone()).await;

        let line = args.get_line()?;
        let symbol = args.get_symbol()?;
        let position = find_symbol_position_in_file(&project, &relative_file, &symbol, line)
            .await
            .map_err(|e| rmcp::Error::invalid_params(e, None))?;

        let references = project
            .lsp
            .find_references(&relative_file, position)
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?
            .ok_or_else(|| rmcp::Error::internal_error("No references found", None))?;

        let mut contents = String::new();
        for reference in references {
            let Ok(Some(lines)) = get_file_lines(
                reference.uri.path(),
                reference.range.start.line,
                reference.range.end.line,
                4,
                4,
            ) else {
                continue;
            };
            contents.push_str(&format!("## {}\n```rust\n{}\n```\n", reference.uri, lines));
        }

        let result = CallToolResult::success(vec![Content::text(contents)]);
        notify_resp(&self.context, &result, absolute_file).await;
        Ok(result)
    }

    // --- symbol_resolve ---
    #[tool(
        name = "symbol_resolve",
        description = "Resolve a symbol based on its name. Provide any symbol from the file and it will try to resolve it and return documentation about it."
    )]
    async fn symbol_resolve(
        &self,
        #[tool(aggr)] args: CallToolRequestParam,
    ) -> Result<CallToolResult, rmcp::Error> {
        let file = args.get_file()?;
        let (project, relative_file, absolute_file) =
            match get_info_from_request(&self.context, &file).await {
                Ok(info) => info,
                Err(e) => return Ok(error_response_v2(&e)),
            };
        notify_req(&self.context, &args, absolute_file.clone()).await;

        let symbol = args.get_symbol()?;
        let symbols = project
            .lsp
            .document_symbols(&relative_file)
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?
            .ok_or_else(|| rmcp::Error::internal_error("No symbols found", None))?;

        let mut symbol_map = HashMap::new();
        for file_symbol in symbols {
            symbol_map.insert(file_symbol.name.clone(), file_symbol);
        }

        let keys = symbol_map.keys().map(|s| s.as_str()).collect::<Vec<_>>();
        let Some(best_match) = get_top_n(&symbol, &keys, None, Some(1), None, None)
            .into_iter()
            .next()
        else {
            return Err(rmcp::Error::internal_error(
                "No match for symbol found",
                None,
            ));
        };

        let Some(symbol_match) = symbol_map.get(&best_match.to_string()) else {
            return Err(rmcp::Error::internal_error(
                "No match for symbol found",
                None,
            ));
        };

        let position = symbol_match.location.range.start;
        let hover = project
            .lsp
            .hover(&relative_file, position)
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?
            .ok_or_else(|| rmcp::Error::internal_error("No hover information found", None))?;

        let response_text = match hover.contents {
            HoverContents::Scalar(s) => format_marked_string(&s),
            HoverContents::Array(a) => a
                .into_iter()
                .map(|s| format_marked_string(&s))
                .collect::<Vec<_>>()
                .join("\n"),
            HoverContents::Markup(m) => m.value,
        };

        let result = CallToolResult::success(vec![Content::text(response_text)]);
        notify_resp(&self.context, &result, absolute_file).await;
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
                version: "0.1.0".to_string(),
            },
            instructions: Some(GUIDANCE_PROMPT.to_string()),
            ..Default::default()
        }
    }

    fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListPromptsResult, rmcp::Error>> + Send + '_ {
        std::future::ready(Ok(ListPromptsResult {
            prompts: vec![Prompt {
                name: "rust_development_guidance".to_string(),
                description: Some(
                    "Comprehensive guidance for Rust development using rust-devtools-mcp"
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
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<GetPromptResult, rmcp::Error>> + Send + '_ {
        match request.name.as_str() {
            "rust_development_guidance" => std::future::ready(Ok(GetPromptResult {
                description: Some(
                    "Guidance for using Rust development tools effectively".to_string(),
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
