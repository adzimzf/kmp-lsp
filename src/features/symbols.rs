//! Document symbols feature — maps indexed `SymbolEntry` to LSP `DocumentSymbol`.

use tower_lsp::lsp_types::{DocumentSymbol, DocumentSymbolResponse, SymbolKind, Url};

use crate::types::SymbolEntry;

/// The kind a client is told `symbol` (declared in `uri`) is. A Java record
/// is indexed as a `STRUCT` (what tells it from a class to the call checks
/// and signature help), but is a class to Java tooling: jdtls reports it as
/// one.
pub(crate) fn reported_kind(uri: &Url, symbol: &SymbolEntry) -> SymbolKind {
    let is_java_record = symbol.kind == SymbolKind::STRUCT && uri.path().ends_with(".java");
    if is_java_record {
        SymbolKind::CLASS
    } else {
        symbol.kind
    }
}

/// Build the LSP document symbols response from a pre-fetched symbol list.
///
/// Returns `None` when `symbols` is empty.
/// On-demand indexing (disk fallback) and symbol fetching are handled by the
/// backend adapter before this function is called.
pub(crate) fn compute_document_symbols(
    uri: &Url,
    symbols: Vec<SymbolEntry>,
) -> Option<DocumentSymbolResponse> {
    if symbols.is_empty() {
        return None;
    }
    let doc_symbols = symbols
        .into_iter()
        .map(|symbol| symbol_to_document_symbol(uri, symbol))
        .collect();
    Some(DocumentSymbolResponse::Nested(doc_symbols))
}

#[allow(deprecated)] // `deprecated` field superseded by `tags` in LSP 3.16+
fn symbol_to_document_symbol(uri: &Url, s: SymbolEntry) -> DocumentSymbol {
    let kind = reported_kind(uri, &s);
    DocumentSymbol {
        name: s.name,
        detail: if s.detail.is_empty() {
            None
        } else {
            Some(s.detail)
        },
        kind,
        tags: None,
        deprecated: None,
        range: s.range,
        selection_range: s.selection_range,
        children: None,
    }
}
