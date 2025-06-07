use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::context::ProjectContext;
use anyhow::Result;
use lsp_types::{Position, Range, TextEdit, WorkspaceEdit};
use std::collections::HashMap;
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

// Smart target location finder using identifier and context
pub fn find_target_location(
    content: &str,
    target_identifier: &str,
    context_hint: Option<&str>,
    threshold: f64,
) -> Result<Option<(usize, usize, String)>, String> {
    let content_lines: Vec<&str> = content.lines().collect();
    let mut candidates: Vec<(usize, usize, String, f64)> = Vec::new();
    
    // Strategy 1: Find exact identifier matches
    for (line_idx, line) in content_lines.iter().enumerate() {
        if line.contains(target_identifier) {
            // Try to determine the scope of this identifier (function, struct, etc.)
            let (start_line, end_line) = determine_code_scope(&content_lines, line_idx);
            let scope_text = content_lines[start_line..=end_line].join("\n");
            
            let mut score = 0.8; // Base score for exact identifier match
            
            // Boost score if context hint matches
            if let Some(hint) = context_hint {
                if scope_text.contains(hint) {
                    score += 0.15;
                }
            }
            
            let start_byte = line_to_byte_offset(content, start_line);
            let end_byte = line_to_byte_offset(content, end_line + 1);
            candidates.push((start_byte, end_byte, scope_text, score));
        }
    }
    
    // Strategy 2: If no exact matches, try fuzzy matching on identifier
    if candidates.is_empty() {
        for (line_idx, line) in content_lines.iter().enumerate() {
            let line_similarity = calculate_similarity(target_identifier, line);
            if line_similarity >= threshold * 0.7 { // Lower threshold for fuzzy matching
                let (start_line, end_line) = determine_code_scope(&content_lines, line_idx);
                let scope_text = content_lines[start_line..=end_line].join("\n");
                
                let mut score = line_similarity * 0.6; // Lower base score for fuzzy match
                
                if let Some(hint) = context_hint {
                    if scope_text.contains(hint) {
                        score += 0.2;
                    }
                }
                
                let start_byte = line_to_byte_offset(content, start_line);
                let end_byte = line_to_byte_offset(content, end_line + 1);
                candidates.push((start_byte, end_byte, scope_text, score));
            }
        }
    }
    
    // Return the best candidate that meets the threshold
    candidates.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    
    if let Some((start, end, text, score)) = candidates.first() {
        if *score >= threshold {
            return Ok(Some((*start, *end, text.clone())));
        }
    }
    
    Ok(None)
}

// Determine the scope of code around a given line (function, struct, impl block, etc.)
fn determine_code_scope(lines: &[&str], target_line: usize) -> (usize, usize) {
    let mut start_line = target_line;
    let mut brace_count = 0;
    let mut found_opening = false;
    
    // Look backwards for the start of the scope
    for i in (0..=target_line).rev() {
        let line = lines[i].trim();
        
        // Count braces
        for ch in line.chars().rev() {
            match ch {
                '}' => brace_count += 1,
                '{' => {
                    brace_count -= 1;
                    if brace_count < 0 {
                        found_opening = true;
                        break;
                    }
                }
                _ => {}
            }
        }
        
        // Check for function/struct/impl declarations
        if line.starts_with("fn ") || line.starts_with("struct ") || 
           line.starts_with("impl ") || line.starts_with("enum ") ||
           line.starts_with("trait ") || line.contains(" fn ") {
            start_line = i;
            break;
        }
        
        if found_opening {
            start_line = i;
            break;
        }
    }
    
    // Reset and look forwards for the end of the scope
    brace_count = 0;
    found_opening = false;
    
    for i in target_line..lines.len() {
        let line = lines[i].trim();
        
        // Count braces
        for ch in line.chars() {
            match ch {
                '{' => {
                    brace_count += 1;
                    found_opening = true;
                }
                '}' => {
                    brace_count -= 1;
                    if found_opening && brace_count == 0 {
                        return (start_line, i);
                    }
                }
                _ => {}
            }
        }
    }
    
    // If no clear scope found, return a reasonable range around the target
    let start = target_line.saturating_sub(2);
    let end = (target_line + 2).min(lines.len().saturating_sub(1));
    (start, end)
}

// Helper function to calculate similarity between two strings
fn calculate_similarity(a: &str, b: &str) -> f64 {
    let a_words: std::collections::HashSet<&str> = a.split_whitespace().collect();
    let b_words: std::collections::HashSet<&str> = b.split_whitespace().collect();
    
    if a_words.is_empty() && b_words.is_empty() {
        return 1.0;
    }
    
    let intersection = a_words.intersection(&b_words).count();
    let union = a_words.union(&b_words).count();
    
    intersection as f64 / union as f64
}

// Helper function to convert line number to byte offset
fn line_to_byte_offset(content: &str, line: usize) -> usize {
    content.lines().take(line).map(|l| l.len() + 1).sum()
}

// Helper function to convert byte positions to LSP positions
pub fn byte_positions_to_lsp_positions(content: &str, start_byte: usize, end_byte: usize) -> (Position, Position) {
    let lines_before_start = content[..start_byte].lines().count();
    let start_line = if start_byte == 0 { 0 } else { lines_before_start };
    let start_char = if start_line == 0 {
        start_byte
    } else {
        start_byte - content[..start_byte].rfind('\n').unwrap_or(0) - 1
    };
    
    let lines_before_end = content[..end_byte].lines().count();
    let end_line = if end_byte == 0 { 0 } else { lines_before_end };
    let end_char = if end_line == 0 {
        end_byte
    } else {
        end_byte - content[..end_byte].rfind('\n').unwrap_or(0) - 1
    };

    let start_pos = Position {
        line: start_line as u32,
        character: start_char as u32,
    };
    let end_pos = Position {
        line: end_line as u32,
        character: end_char as u32,
    };
    
    (start_pos, end_pos)
}

// Convert simple edits to workspace edit using smart targeting
pub fn convert_simple_edits_to_workspace_edit(
    edits: &[crate::mcp::server::SimpleFileEdit],
) -> Result<WorkspaceEdit, String> {
    let mut changes: HashMap<lsp_types::Url, Vec<TextEdit>> = HashMap::new();

    for edit in edits {
        // Read file content
        let content = std::fs::read_to_string(&edit.file_path)
            .map_err(|e| format!("Failed to read file {}: {}", edit.file_path, e))?;

        // Use smart targeting based on identifier and context
        let match_result = find_target_location(
            &content,
            &edit.target_identifier,
            edit.context_hint.as_deref(),
            edit.similarity_threshold,
        )?;
        
        if let Some((start_byte, end_byte, _matched_text)) = match_result {
            // Convert byte positions to LSP positions
            let (start_pos, end_pos) = byte_positions_to_lsp_positions(&content, start_byte, end_byte);

            let range = Range {
                start: start_pos,
                end: end_pos,
            };

            let text_edit = TextEdit {
                range,
                new_text: edit.new_content.clone(),
            };

            // Convert file path to URI
            let uri = lsp_types::Url::from_file_path(&edit.file_path).map_err(|_| {
                format!("Invalid file path: {}", edit.file_path)
            })?;

            changes.entry(uri).or_insert_with(Vec::new).push(text_edit);
        } else {
            return Err(format!(
                "No suitable match found for target '{}' (similarity threshold: {})", 
                edit.target_identifier, edit.similarity_threshold
            ));
        }
    }

    Ok(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}
