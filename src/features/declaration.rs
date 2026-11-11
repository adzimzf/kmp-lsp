//! `textDocument/declaration` for Java: where the method a name resolves to
//! is first declared. A method that overrides another is declared by the
//! topmost method it overrides — `json.getAsBigDecimal()` on a
//! `JsonPrimitive` is declared by `JsonElement.getAsBigDecimal`, and
//! `set.iterator()` by `Iterable.iterator`. Anything else is declared where
//! it is defined.

use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, SymbolKind, Url};

use crate::backend::cursor::CursorContext;
use crate::indexer::Indexer;

/// The declarations of what the name at `position` in a Java file resolves
/// to; `None` when it resolves to nothing, or the file is not Java.
pub(crate) async fn java_declaration(
    indexer: &Indexer,
    uri: &Url,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    if crate::Language::from_path(uri.path()) != crate::Language::Java {
        return None;
    }
    let context = CursorContext::build(indexer, uri, position)?;
    let response =
        crate::features::definition::find_definition(&context, indexer, uri, position).await?;
    let mut declarations: Vec<Location> = Vec::new();
    for definition in locations_of(response) {
        let declaration = indexer
            .symbol_declared_at(&context.word, &definition)
            .filter(|symbol| matches!(symbol.kind, SymbolKind::METHOD | SymbolKind::FUNCTION))
            .and_then(|symbol| {
                indexer
                    .overridden_method_declarations(&symbol, &definition)
                    .pop()
            })
            .unwrap_or(definition);
        if !declarations.contains(&declaration) {
            declarations.push(declaration);
        }
    }
    match declarations.len() {
        0 => None,
        1 => declarations.pop().map(GotoDefinitionResponse::Scalar),
        _ => Some(GotoDefinitionResponse::Array(declarations)),
    }
}

fn locations_of(response: GotoDefinitionResponse) -> Vec<Location> {
    match response {
        GotoDefinitionResponse::Scalar(location) => vec![location],
        GotoDefinitionResponse::Array(locations) => locations,
        GotoDefinitionResponse::Link(links) => links
            .into_iter()
            .map(|link| Location::new(link.target_uri, link.target_selection_range))
            .collect(),
    }
}

#[cfg(test)]
#[path = "declaration_tests.rs"]
mod tests;
