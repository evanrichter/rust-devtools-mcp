use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::context::{Context, ProjectContext};
use anyhow::Result;
use lsp_types::Position;
use rmcp::model::{CallToolRequestParam, CallToolResult, Content};

pub fn error_response_v2(message: &str) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.to_string())])
}

pub(super) trait RequestExtension {
    fn get_line(&self) -> Result<u64, rmcp::Error>;
    fn get_symbol(&self) -> Result<String, rmcp::Error>;
    fn get_file(&self) -> Result<String, rmcp::Error>;
}

impl RequestExtension for CallToolRequestParam {
    fn get_line(&self) -> Result<u64, rmcp::Error> {
        let number = self
            .arguments
            .as_ref()
            .and_then(|args| args.get("line"))
            .and_then(|v| v.as_u64())
            .ok_or_else(|| rmcp::Error::invalid_params("Line is required", None))?;
        Ok(number)
    }

    fn get_symbol(&self) -> Result<String, rmcp::Error> {
        self.arguments
            .as_ref()
            .and_then(|args| args.get("symbol"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| rmcp::Error::invalid_params("Symbol is required", None))
            .map(|s| s.to_string())
    }

    fn get_file(&self) -> Result<String, rmcp::Error> {
        self.arguments
            .as_ref()
            .and_then(|args| args.get("file"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| rmcp::Error::invalid_params("File is required", None))
            .map(|s| s.to_string())
    }
}

/// Returns the project, the relative file path and the absolute file path
pub async fn get_info_from_request(
    context: &Context,
    file_path: &str,
) -> Result<(Arc<ProjectContext>, String, PathBuf), String> {
    let absolute_path = PathBuf::from(file_path);
    let Some(project) = context.get_project_by_path(&absolute_path).await else {
        return Err(format!("No project found for file {}", file_path));
    };

    let relative_path = project
        .project
        .relative_path(file_path)
        .map_err(|e| e.to_string())?;

    Ok((project, relative_path, absolute_path))
}

pub async fn find_symbol_position_in_file(
    project: &Arc<ProjectContext>,
    relative_file: &str,
    symbol: &str,
    line: u64,
) -> Result<Position, String> {
    let symbols = match project.lsp.document_symbols(relative_file).await {
        Ok(Some(symbols)) => symbols,
        Ok(None) => return Err("No symbols found".to_string()),
        Err(e) => return Err(e.to_string()),
    };
    for s in symbols {
        if s.name == symbol && s.location.range.start.line == line as u32 {
            return Ok(s.location.range.start);
        }
    }
    Err(format!("Symbol {symbol} not found in file {relative_file}"))
}

/// Returns the lines between start_line and end_line (inclusive) from the given file path
/// Optionally includes prefix lines before start_line and suffix lines after end_line
/// Line numbers are 0-based
/// Returns None if any line number is out of bounds after adjusting for prefix/suffix
pub fn get_file_lines(
    file_path: impl AsRef<Path>,
    start_line: u32,
    end_line: u32,
    prefix: u8,
    suffix: u8,
) -> std::io::Result<Option<String>> {
    let content = std::fs::read_to_string(file_path)?;
    let lines: Vec<&str> = content.lines().collect();

    // Calculate actual line range accounting for prefix/suffix
    let start = start_line.saturating_sub(prefix as u32);
    let mut end = end_line.saturating_add(suffix as u32);

    if end > lines.len() as u32 {
        end = lines.len() as u32;
    }

    // Check if line range is valid
    if start > end || end >= lines.len() as u32 {
        return Ok(None);
    }

    // Extract and join the requested lines
    let selected_lines = lines[start as usize..=end as usize].join("\n");
    Ok(Some(selected_lines))
}
