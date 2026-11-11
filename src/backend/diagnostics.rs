//! Pull diagnostics (`textDocument/diagnostic`, `workspace/diagnostic`) and
//! `textDocument/documentLink`.
//!
//! A pull answers what a push would publish for the file
//! ([`crate::workspace::document_handler::file_diagnostics`]). The workspace
//! report gives every open file that, and every other indexed workspace file
//! its syntax errors (already known from indexing; the semantic checks are
//! too costly to run over a whole workspace on request).

use std::sync::Arc;

use super::helpers::syntax_diagnostics;
use super::Backend;
use crate::workspace::document_handler::file_diagnostics;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

impl Backend {
    pub(super) async fn diagnostic_impl(
        &self,
        params: DocumentDiagnosticParams,
    ) -> Result<DocumentDiagnosticReportResult> {
        let uri = params.text_document.uri;
        if !self.indexer.is_library_uri(&uri) {
            self.indexer.ensure_indexed(&uri);
        }
        let indexer = Arc::clone(&self.indexer);
        let items = tokio::task::spawn_blocking(move || file_diagnostics(&indexer, &uri))
            .await
            .unwrap_or_default();
        Ok(DocumentDiagnosticReportResult::Report(
            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                related_documents: None,
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: None,
                    items,
                },
            }),
        ))
    }

    pub(super) async fn workspace_diagnostic_impl(
        &self,
        _params: WorkspaceDiagnosticParams,
    ) -> Result<WorkspaceDiagnosticReportResult> {
        let indexer = Arc::clone(&self.indexer);
        let items = tokio::task::spawn_blocking(move || {
            let open: Vec<String> = indexer
                .live_trees
                .iter()
                .map(|entry| entry.key().clone())
                .collect();
            let mut reports: Vec<(Url, Vec<Diagnostic>)> = indexer
                .files
                .iter()
                .filter(|entry| !open.contains(entry.key()))
                .filter_map(|entry| {
                    let uri = Url::parse(entry.key()).ok()?;
                    (uri.scheme() == "file" && !indexer.is_library_uri(&uri))
                        .then(|| (uri, syntax_diagnostics(&entry.value().syntax_errors)))
                })
                .collect();
            for key in open {
                if let Ok(uri) = Url::parse(&key) {
                    let diagnostics = file_diagnostics(&indexer, &uri);
                    reports.push((uri, diagnostics));
                }
            }
            reports
        })
        .await
        .unwrap_or_default();
        Ok(WorkspaceDiagnosticReportResult::Report(
            WorkspaceDiagnosticReport {
                items: items
                    .into_iter()
                    .map(|(uri, items)| {
                        WorkspaceDocumentDiagnosticReport::Full(
                            WorkspaceFullDocumentDiagnosticReport {
                                uri,
                                version: None,
                                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                                    result_id: None,
                                    items,
                                },
                            },
                        )
                    })
                    .collect(),
            },
        ))
    }

    pub(super) async fn document_link_impl(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
        let Some(lines) = self.indexer.lines_for(&params.text_document.uri) else {
            return Ok(None);
        };
        Ok(Some(crate::features::document_link::document_links(&lines)))
    }
}
