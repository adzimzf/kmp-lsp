//! `textDocument/formatting`, `textDocument/rangeFormatting` and
//! `textDocument/onTypeFormatting`: Java files are re-indented by
//! [`crate::features::java_formatting`]; Kotlin and Swift files are left to
//! their own formatters.

use super::Backend;
use crate::features::{java_formatting, on_type_formatting};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

fn is_java(uri: &Url) -> bool {
    uri.path().ends_with(".java")
}

impl Backend {
    pub(super) async fn formatting_impl(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = &params.text_document.uri;
        if !is_java(uri) {
            return Ok(None);
        }
        let Some(lines) = self.indexer.lines_for(uri) else {
            return Ok(None);
        };
        Ok(Some(java_formatting::format_document(
            &lines,
            &params.options,
        )))
    }

    pub(super) async fn range_formatting_impl(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = &params.text_document.uri;
        if !is_java(uri) {
            return Ok(None);
        }
        let Some(lines) = self.indexer.lines_for(uri) else {
            return Ok(None);
        };
        Ok(Some(java_formatting::format_range(
            &lines,
            params.range,
            &params.options,
        )))
    }

    pub(super) async fn on_type_formatting_impl(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let Some(lines) = self.indexer.lines_for(uri) else {
            log::debug!("on_type_formatting: no lines for {uri}");
            return Ok(None);
        };
        let result = on_type_formatting::compute_on_type_formatting(
            &lines,
            position,
            &params.ch,
            &params.options,
        );
        // Enter after a Java statement: the new line lines up with the
        // statements around it.
        if result.is_none() && params.ch == "\n" && is_java(uri) {
            return Ok(java_formatting::indent_new_line(
                &lines,
                position.line as usize,
                &params.options,
            ));
        }
        Ok(result)
    }
}
