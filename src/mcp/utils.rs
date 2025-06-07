use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::context::ProjectContext;
use anyhow::Result;
use lsp_types::{Position, TextEdit, WorkspaceEdit};
use rmcp::model::{CallToolResult, Content};

pub fn error_response(message: &str) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.to_string())])
}

/// Resolves a symbol name within a project, handling ambiguity.
pub async fn resolve_symbol_in_project(
    project: &Arc<ProjectContext>,
    symbol_name: &str,
    file_hint: Option<&str>,
) -> Result<lsp_types::SymbolInformation, String> {
    let workspace_response = project
        .lsp
        .workspace_symbols(symbol_name.to_string())
        .await
        .map_err(|e| format!("LSP error while searching for symbol: {}", e))?
        .unwrap_or(lsp_types::WorkspaceSymbolResponse::Flat(vec![]));

    let symbols: Vec<lsp_types::SymbolInformation> = match workspace_response {
        lsp_types::WorkspaceSymbolResponse::Flat(symbols) => symbols,
        lsp_types::WorkspaceSymbolResponse::Nested(workspace_symbols) => {
            // Convert WorkspaceSymbol to SymbolInformation
            workspace_symbols
                .into_iter()
                .filter_map(|ws| {
                    if let lsp_types::OneOf::Left(location) = ws.location {
                        Some(lsp_types::SymbolInformation {
                            name: ws.name,
                            kind: ws.kind,
                            tags: ws.tags,
                            #[allow(deprecated)]
                            deprecated: None,
                            location,
                            container_name: ws.container_name,
                        })
                    } else {
                        None
                    }
                })
                .collect()
        }
    };

    if symbols.is_empty() {
        return Err(format!("Symbol '{}' not found in project.", symbol_name));
    }

    if symbols.len() == 1 {
        return Ok(symbols.into_iter().next().unwrap());
    }

    // More than one match, try to use file_hint to disambiguate.
    if let Some(hint) = file_hint {
        let hint_path = Path::new(hint);
        for symbol in &symbols {
            if let Ok(symbol_path) = symbol.location.uri.to_file_path() {
                if symbol_path.ends_with(hint_path) || symbol_path.to_string_lossy().contains(hint)
                {
                    return Ok(symbol.clone());
                }
            }
        }
    }

    // Still ambiguous, return a list for the LLM to handle.
    let candidates = symbols
        .iter()
        .filter_map(|s| {
            s.location.uri.to_file_path().ok().and_then(|path| {
                Some(format!(
                    "- `{}` (kind: {:?}) in `{}`",
                    s.name,
                    s.kind,
                    path.display()
                ))
            })
        })
        .collect::<Vec<_>>()
        .join("\n");

    Err(format!(
        "Symbol '{}' is ambiguous. Please provide a more specific file_hint or ask the user to clarify from the following candidates:\n{}",
        symbol_name, candidates
    ))
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

    if lines.is_empty() {
        return Ok(Some(String::new()));
    }

    // Calculate actual line range accounting for prefix/suffix
    let start = start_line.saturating_sub(prefix as u32) as usize;
    let end = (end_line.saturating_add(suffix as u32) as usize).min(lines.len() - 1);

    if start > end {
        return Ok(None);
    }

    // Extract and join the requested lines
    let selected_lines = lines[start..=end].join("\n");
    Ok(Some(selected_lines))
}

/// Applies a `WorkspaceEdit` to the file system.
/// This function is critical for any code modification tools.
pub fn apply_workspace_edit(edit: &WorkspaceEdit) -> std::result::Result<(), String> {
    let Some(changes) = &edit.changes else {
        // TODO: Handle documentChanges field as well for more complex edits
        return Ok(());
    };

    for (uri, text_edits) in changes {
        let path = uri
            .to_file_path()
            .map_err(|_| format!("Invalid file URI in WorkspaceEdit: {}", uri))?;

        apply_edits_to_file(&path, text_edits)
            .map_err(|e| format!("Failed to apply edits to {}: {}", path.display(), e))?;
    }

    Ok(())
}

/// Helper function to apply a series of `TextEdit`s to a single file.
fn apply_edits_to_file(path: &PathBuf, edits: &[TextEdit]) -> std::io::Result<()> {
    let original_content = fs::read_to_string(path)?;
    let mut content = original_content.clone();

    // The LSP spec says edits should be applied from bottom to top to avoid invalidating ranges.
    let mut sorted_edits = edits.to_vec();
    sorted_edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));

    // Helper to convert LSP position to a byte offset in the original text.
    // This is more robust than manipulating lines, especially with multi-line edits.
    let pos_to_offset = |pos: Position, content: &str| -> Option<usize> {
        let lines: Vec<&str> = content.lines().collect();
        let mut offset = 0;
        for (i, line) in lines.iter().enumerate() {
            if i == pos.line as usize {
                // Check if character is within the line bounds
                if pos.character as usize <= line.chars().count() {
                    let char_offset: usize = line
                        .chars()
                        .take(pos.character as usize)
                        .map(|c| c.len_utf8())
                        .sum();
                    return Some(offset + char_offset);
                } else {
                    return None; // Invalid character position
                }
            }
            offset += line.len() + 1; // +1 for the newline character
        }
        None
    };

    for edit in &sorted_edits {
        if let (Some(start_offset), Some(end_offset)) = (
            pos_to_offset(edit.range.start, &original_content),
            pos_to_offset(edit.range.end, &original_content),
        ) {
            if start_offset <= end_offset && end_offset <= content.len() {
                content.replace_range(start_offset..end_offset, &edit.new_text);
            } else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Invalid range in text edit.",
                ));
            }
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Could not convert LSP position to byte offset.",
            ));
        }
    }

    fs::write(path, content)?;
    Ok(())
}
