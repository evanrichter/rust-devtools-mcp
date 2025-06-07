use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::collections::HashMap;
use rayon::prelude::*;

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

    // Try to deduplicate symbols that are essentially the same type
    let deduplicated_symbols = deduplicate_symbols(&symbols);
    
    if deduplicated_symbols.len() == 1 {
        return Ok(deduplicated_symbols.into_iter().next().unwrap());
    }

    // More than one match, try to use file_hint to disambiguate.
    if let Some(hint) = file_hint {
        let hint_path = Path::new(hint);
        for symbol in &deduplicated_symbols {
            if let Ok(symbol_path) = symbol.location.uri.to_file_path() {
                if symbol_path.ends_with(hint_path) || symbol_path.to_string_lossy().contains(hint)
                {
                    return Ok(symbol.clone());
                }
            }
        }
    }

    // Still ambiguous, return a list for the LLM to handle.
    let candidates = deduplicated_symbols
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
    if start < lines.len() && end < lines.len() {
        // Use parallel processing for large line ranges
        let selected_lines = if (end - start) > 1000 {
            // Parallel processing for large line ranges
            lines[start..=end]
                .par_iter()
                .map(|&line| line.to_string())
                .collect::<Vec<String>>()
                .join("\n")
        } else {
            // Sequential processing for small line ranges
            lines[start..=end].join("\n")
        };
        Ok(Some(selected_lines))
    } else {
        Ok(None)
    }
}

/// Deduplicates symbols that are essentially the same type
/// Prioritizes symbols based on file type and location preferences
fn deduplicate_symbols(symbols: &[lsp_types::SymbolInformation]) -> Vec<lsp_types::SymbolInformation> {
    // Use parallel processing for symbol grouping when we have enough symbols
    let symbol_groups: HashMap<String, Vec<lsp_types::SymbolInformation>> = if symbols.len() > 50 {
        // Parallel grouping for large symbol sets
        symbols
            .par_iter()
            .map(|symbol| {
                let key = format!("{}-{:?}", symbol.name, symbol.kind);
                (key, symbol.clone())
            })
            .fold(
                HashMap::new,
                |mut acc: HashMap<String, Vec<lsp_types::SymbolInformation>>, (key, symbol)| {
                    acc.entry(key).or_insert_with(Vec::new).push(symbol);
                    acc
                },
            )
            .reduce(
                HashMap::new,
                |mut acc, map| {
                    for (key, mut symbols) in map {
                        acc.entry(key).or_insert_with(Vec::new).append(&mut symbols);
                    }
                    acc
                },
            )
    } else {
        // Sequential processing for small symbol sets
        let mut symbol_groups = HashMap::new();
        for symbol in symbols {
            let key = format!("{}-{:?}", symbol.name, symbol.kind);
            symbol_groups.entry(key).or_insert_with(Vec::new).push(symbol.clone());
        }
        symbol_groups
    };
    
    // Use parallel processing for symbol selection when we have multiple groups
    let groups: Vec<_> = symbol_groups.into_iter().collect();
    if groups.len() > 10 {
        groups
            .par_iter()
            .map(|(_, group)| {
                if group.len() == 1 {
                    group[0].clone()
                } else {
                    choose_best_symbol(group)
                }
            })
            .collect()
    } else {
        groups
            .iter()
            .map(|(_, group)| {
                if group.len() == 1 {
                    group[0].clone()
                } else {
                    choose_best_symbol(group)
                }
            })
            .collect()
    }
}

/// Chooses the best symbol from a group of symbols with the same name and kind
/// Prioritizes based on file type and location preferences
fn choose_best_symbol(symbols: &[lsp_types::SymbolInformation]) -> lsp_types::SymbolInformation {
    // Priority order:
    // 1. Main source files (.rs, .ts, .js, .py, etc.) over generated/test files
    // 2. Files in src/ over files in tests/ or target/
    // 3. Files with shorter paths (likely more central)
    // 4. Files that don't contain "test", "spec", "generated", "build" in their path
    
    let mut best_symbol = &symbols[0];
    let mut best_score = calculate_symbol_score(best_symbol);
    
    for symbol in &symbols[1..] {
        let score = calculate_symbol_score(symbol);
        if score > best_score {
            best_symbol = symbol;
            best_score = score;
        }
    }
    
    best_symbol.clone()
}

/// Calculates a score for a symbol based on its file location
/// Higher score means better/more preferred symbol
fn calculate_symbol_score(symbol: &lsp_types::SymbolInformation) -> i32 {
    let Ok(file_path) = symbol.location.uri.to_file_path() else {
        return 0;
    };
    
    let path_str = file_path.to_string_lossy().to_lowercase();
    let mut score = 100; // Base score
    
    // Prefer main source files
    if path_str.contains("/src/") || path_str.contains("\\src\\") {
        score += 50;
    }
    
    // Prefer non-test files
    if path_str.contains("test") || path_str.contains("spec") {
        score -= 30;
    }
    
    // Prefer non-generated files
    if path_str.contains("generated") || path_str.contains("build") || path_str.contains("target") {
        score -= 40;
    }
    
    // Prefer files with common source extensions
    if path_str.ends_with(".rs") || path_str.ends_with(".ts") || path_str.ends_with(".js") 
        || path_str.ends_with(".py") || path_str.ends_with(".java") || path_str.ends_with(".cpp") 
        || path_str.ends_with(".c") || path_str.ends_with(".h") {
        score += 20;
    }
    
    // Prefer shorter paths (more central files)
    let path_depth = path_str.matches('/').count() + path_str.matches('\\').count();
    score -= (path_depth as i32) * 2;
    
    // Prefer files in lib.rs, main.rs, mod.rs (Rust specific)
    if path_str.ends_with("lib.rs") || path_str.ends_with("main.rs") {
        score += 30;
    } else if path_str.ends_with("mod.rs") {
        score += 10;
    }
    
    score
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
#[allow(dead_code)]
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
            let scope_text = if start_line <= end_line && end_line < content_lines.len() {
                content_lines[start_line..=end_line].join("\n")
            } else {
                String::new()
            };
            
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
                let scope_text = if start_line <= end_line && end_line < content_lines.len() {
                    content_lines[start_line..=end_line].join("\n")
                } else {
                    String::new()
                };
                
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
#[allow(dead_code)]
fn determine_code_scope(lines: &[&str], target_line: usize) -> (usize, usize) {
    let mut start_line = target_line;
    let mut brace_count = 0;
    let mut found_opening = false;
    
    // Look backwards for the start of the scope
    for i in (0..=target_line).rev() {
        if let Some(line) = lines.get(i) {
            let line = line.trim();
            
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
    }
    
    // Reset and look forwards for the end of the scope
    brace_count = 0;
    found_opening = false;
    
    for i in target_line..lines.len() {
        if let Some(line) = lines.get(i) {
            let line = line.trim();
            
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
    }
    
    // If no clear scope found, return a reasonable range around the target
    let start = target_line.saturating_sub(2);
    let end = (target_line + 2).min(lines.len().saturating_sub(1));
    (start, end)
}

// Helper function to calculate similarity between two strings
#[allow(dead_code)]
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
#[allow(dead_code)]
fn line_to_byte_offset(content: &str, line: usize) -> usize {
    content.lines().take(line).map(|l| l.len() + 1).sum()
}

// Helper function to convert byte positions to LSP positions
#[allow(dead_code)]
pub fn byte_positions_to_lsp_positions(content: &str, start_byte: usize, end_byte: usize) -> (Position, Position) {
    let start_byte = start_byte.min(content.len());
    let end_byte = end_byte.min(content.len());
    
    let lines_before_start = if start_byte == 0 {
        0
    } else {
        content.get(..start_byte).map_or(0, |s| s.lines().count())
    };
    let start_line = if start_byte == 0 { 0 } else { lines_before_start };
    let start_char = if start_line == 0 {
        start_byte
    } else {
        let prefix = content.get(..start_byte).unwrap_or("");
        match prefix.rfind('\n') {
            Some(last_newline) => start_byte.saturating_sub(last_newline + 1),
            None => start_byte,
        }
    };
    
    let lines_before_end = if end_byte == 0 {
        0
    } else {
        content.get(..end_byte).map_or(0, |s| s.lines().count())
    };
    let end_line = if end_byte == 0 { 0 } else { lines_before_end };
    let end_char = if end_line == 0 {
        end_byte
    } else {
        let prefix = content.get(..end_byte).unwrap_or("");
        match prefix.rfind('\n') {
            Some(last_newline) => end_byte.saturating_sub(last_newline + 1),
            None => end_byte,
        }
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

/// Finds files by name or pattern within a project.
/// Supports fuzzy matching and returns the best matches sorted by relevance.
/// Uses parallel processing for improved performance on large projects.
#[allow(dead_code)]
pub async fn find_files_by_name(
    project: &Arc<ProjectContext>,
    filename_pattern: &str,
    max_results: usize,
) -> Result<Vec<PathBuf>, String> {
    use rayon::prelude::*;
    use std::collections::HashSet;
    use std::sync::Mutex;
    
    let project_root = &project.project.root;
    let matching_files = Mutex::new(HashSet::new());
    
    // Use glob pattern to find files
    let search_patterns = vec![
        format!("**/{}", filename_pattern),  // Exact match
        format!("**/*{}*", filename_pattern), // Contains pattern
        format!("**/{}.rs", filename_pattern), // Rust file with exact name
        format!("**/*{}.rs", filename_pattern), // Rust file containing pattern
    ];
    
    // Process patterns in parallel
    let pattern_results: Vec<_> = search_patterns
        .into_par_iter()
        .filter_map(|pattern| {
            let full_pattern = project_root.join(&pattern);
            glob::glob(full_pattern.to_string_lossy().as_ref()).ok()
        })
        .collect();
    
    // Process glob results in parallel
    pattern_results.into_par_iter().for_each(|entries| {
        let local_files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .filter(|path| path.is_file())
            .collect();
        
        if let Ok(mut files) = matching_files.lock() {
            files.extend(local_files);
        }
    });
    
    let mut matching_files: Vec<PathBuf> = matching_files
        .into_inner()
        .map_err(|_| "Failed to collect matching files")?
        .into_iter()
        .collect();
    
    // Sort by relevance using parallel sorting for large collections
    if matching_files.len() > 100 {
        matching_files.par_sort_by(|a, b| {
            let a_name = a.file_name().unwrap_or_default().to_string_lossy();
            let b_name = b.file_name().unwrap_or_default().to_string_lossy();
            
            // Exact filename match gets highest priority
            let a_exact = a_name == filename_pattern;
            let b_exact = b_name == filename_pattern;
            
            if a_exact && !b_exact {
                return std::cmp::Ordering::Less;
            }
            if !a_exact && b_exact {
                return std::cmp::Ordering::Greater;
            }
            
            // Then by path depth (shorter paths first)
            let a_depth = a.components().count();
            let b_depth = b.components().count();
            a_depth.cmp(&b_depth)
        });
    } else {
        // Use regular sort for smaller collections
        matching_files.sort_by(|a, b| {
            let a_name = a.file_name().unwrap_or_default().to_string_lossy();
            let b_name = b.file_name().unwrap_or_default().to_string_lossy();
            
            // Exact filename match gets highest priority
            let a_exact = a_name == filename_pattern;
            let b_exact = b_name == filename_pattern;
            
            if a_exact && !b_exact {
                return std::cmp::Ordering::Less;
            }
            if !a_exact && b_exact {
                return std::cmp::Ordering::Greater;
            }
            
            // Then by path depth (shorter paths first)
            let a_depth = a.components().count();
            let b_depth = b.components().count();
            a_depth.cmp(&b_depth)
        });
    }
    
    // Limit results
    matching_files.truncate(max_results);
    
    Ok(matching_files)
}

/// Resolves a file path that might be incomplete or just a filename.
/// Returns the best matching absolute path within the project.
#[allow(dead_code)]
pub async fn resolve_file_path(
    project: &Arc<ProjectContext>,
    file_path_or_name: &str,
) -> Result<PathBuf, String> {
    let path = Path::new(file_path_or_name);
    
    // If it's already an absolute path and exists, return it
    if path.is_absolute() && path.exists() {
        return Ok(path.to_path_buf());
    }
    
    // If it's a relative path from project root and exists, return absolute path
    let project_relative = project.project.root.join(path);
    if project_relative.exists() {
        return Ok(project_relative);
    }
    
    // If it's just a filename or partial path, search for it
    let filename = path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    
    let matches = find_files_by_name(project, &filename, 5).await?;
    
    if matches.is_empty() {
        return Err(format!(
            "No files found matching '{}' in project '{}'",
            file_path_or_name,
            project.project.root.display()
        ));
    }
    
    if matches.len() == 1 {
        return Ok(matches[0].clone());
    }
    
    // Multiple matches found, return the best one but suggest alternatives
    let best_match = &matches[0];
    let alternatives: Vec<String> = matches[1..]
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    
    eprintln!(
        "Multiple files found for '{}'. Using: {}. Alternatives: {}",
        file_path_or_name,
        best_match.display(),
        alternatives.join(", ")
    );
    
    Ok(best_match.clone())
}
