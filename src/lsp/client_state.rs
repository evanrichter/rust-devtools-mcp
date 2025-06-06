use std::ops::ControlFlow;
use std::path::PathBuf;

use super::Stop;
use crate::lsp::{LspNotification, IndexingProgress, IndexingStage};
use async_lsp::router::Router;
use async_lsp::{LanguageClient, ResponseError};
use lsp_types::{
    NumberOrString, ProgressParams, ProgressParamsValue, PublishDiagnosticsParams,
    ShowMessageParams, WorkDoneProgress,
};
use regex::Regex;
use std::sync::OnceLock;

// Old and new token names.
const RA_INDEXING_TOKENS: &[&str] = &[
    "rustAnalyzer/Indexing",
    "rustAnalyzer/cachePriming",
    "rustAnalyzer/Building",
];

pub struct ClientState {
    project: PathBuf,
    indexed_tx: Option<flume::Sender<()>>,
    notifier: flume::Sender<LspNotification>,
}

impl LanguageClient for ClientState {
    type Error = ResponseError;
    type NotifyResult = ControlFlow<async_lsp::Result<()>>;

    fn progress(&mut self, params: ProgressParams) -> Self::NotifyResult {
        tracing::trace!("{:?} {:?}", params.token, params.value);
        let is_indexing =
            matches!(params.token, NumberOrString::String(ref s) if RA_INDEXING_TOKENS.contains(&s.as_str()));
        let is_work_done = matches!(
            params.value,
            ProgressParamsValue::WorkDone(WorkDoneProgress::End(_))
        );
        
        if is_indexing {
            let progress = self.parse_progress(&params);
            
            if !is_work_done {
                if let Err(e) = self.notifier.send(LspNotification::Indexing {
                    project: self.project.clone(),
                    is_indexing: true,
                    progress,
                }) {
                    tracing::error!("Failed to send indexing notification: {}", e);
                }
            } else {
                // Parse progress even for end events to know which stage finished
                let progress = self.parse_progress(&params);
                if let Err(e) = self.notifier.send(LspNotification::Indexing {
                    project: self.project.clone(),
                    is_indexing: false,
                    progress,
                }) {
                    tracing::error!("Failed to send indexing notification: {}", e);
                }

                if let Some(tx) = &self.indexed_tx {
                    if let Err(e) = tx.try_send(()) {
                        tracing::error!("Failed to send indexing completion signal: {}", e);
                    }
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn publish_diagnostics(&mut self, _: PublishDiagnosticsParams) -> Self::NotifyResult {
        ControlFlow::Continue(())
    }

    fn show_message(&mut self, params: ShowMessageParams) -> Self::NotifyResult {
        tracing::debug!("Message {:?}: {}", params.typ, params.message);
        ControlFlow::Continue(())
    }
}

impl ClientState {
    fn parse_progress(&self, params: &ProgressParams) -> Option<IndexingProgress> {
        static CRATE_REGEX: OnceLock<Regex> = OnceLock::new();
        static COUNT_REGEX: OnceLock<Regex> = OnceLock::new();
        
        let crate_regex = CRATE_REGEX.get_or_init(|| {
            Regex::new(r"(?:indexing|building|loading)\s+([\w-]+)").unwrap()
        });
        let count_regex = COUNT_REGEX.get_or_init(|| {
            Regex::new(r"(\d+)/(\d+)").unwrap()
        });
        
        // Add debug logging to see actual token values
        let stage = match &params.token {
            NumberOrString::String(s) => {
                // Use exact matching instead of contains() to properly distinguish stages
                match s.as_str() {
                    "rustAnalyzer/Building" => IndexingStage::Building,
                    "rustAnalyzer/cachePriming" => IndexingStage::CachePriming,
                    "rustAnalyzer/Indexing" => IndexingStage::Indexing,
                    _ => {
                        tracing::warn!("Unknown indexing token: '{}'", s);
                        IndexingStage::Unknown(s.clone())
                    }
                }
            }
            NumberOrString::Number(n) => {
                IndexingStage::Unknown(format!("numeric_{}", n))
            }
        };
        
        let (message, percentage) = match &params.value {
            ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(begin)) => {
                (begin.title.as_str(), begin.percentage.map(|p| p as f32))
            }
            ProgressParamsValue::WorkDone(WorkDoneProgress::Report(report)) => {
                (report.message.as_deref().unwrap_or(""), report.percentage.map(|p| p as f32))
            }
            _ => ("", None),
        };
        
        let current_crate = crate_regex
            .captures(message)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_string());
            
        let (current_count, total_count) = count_regex
            .captures(message)
            .and_then(|caps| {
                let current = caps.get(1)?.as_str().parse::<u32>().ok()?;
                let total = caps.get(2)?.as_str().parse::<u32>().ok()?;
                Some((Some(current), Some(total)))
            })
            .unwrap_or((None, None));
            
        Some(IndexingProgress {
            current_crate,
            current_count,
            total_count,
            stage,
            percentage,
        })
    }
    
    pub fn new_router(
        indexed_tx: flume::Sender<()>,
        notifier: flume::Sender<LspNotification>,
        project: PathBuf,
    ) -> Router<Self> {
        let mut router = Router::from_language_client(ClientState {
            indexed_tx: Some(indexed_tx),
            notifier,
            project,
        });
        router.event(Self::on_stop);
        router
    }

    pub fn on_stop(&mut self, _: Stop) -> ControlFlow<async_lsp::Result<()>> {
        ControlFlow::Break(Ok(()))
    }
}
