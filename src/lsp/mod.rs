mod change_notifier;
mod client_state;
mod rust_analyzer_lsp;
mod utils;

pub(super) struct Stop;

use std::path::PathBuf;

pub use rust_analyzer_lsp::RustAnalyzerLsp;
pub use utils::*;

#[derive(Debug, Clone)]
pub enum LspNotification {
    Indexing { 
        project: PathBuf, 
        is_indexing: bool,
        progress: Option<IndexingProgress>,
    },
}

#[derive(Debug, Clone)]
pub struct IndexingProgress {
    pub current_crate: Option<String>,
    pub current_count: Option<u32>,
    pub total_count: Option<u32>,
    pub stage: IndexingStage,
    pub percentage: Option<f32>,
}

#[derive(Debug, Clone)]
pub enum IndexingStage {
    Building,
    CachePriming,
    Indexing,
    Unknown(String),
}
