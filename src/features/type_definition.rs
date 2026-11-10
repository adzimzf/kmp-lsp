//! `goto_type_definition` feature: the declaration of the *type* of the
//! symbol under the cursor.
//!
//! Built on go-to-definition: the symbol's own declaration is found first,
//! then what that declaration says about its type
//! ([`crate::indexer::type_definition_step`]) decides the answer — a type is
//! its own type definition, a field, variable, parameter or method goes to
//! the type written in its declaration (resolved where it is written, so
//! that file's imports apply), an enum constant or constructor to the type
//! declaring it.

use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, Url};

use crate::backend::cursor::CursorContext;
use crate::features::definition::{find_definition, locs_to_opt_response};
use crate::indexer::{type_definition_step, Indexer, TypeDefinitionStep};

/// Resolve go-to-type-definition at `position` in `uri`.
pub(crate) async fn find_type_definition(
    ctx: &CursorContext,
    index: &Indexer,
    uri: &Url,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    let definitions = response_locations(find_definition(ctx, index, uri, position).await?);
    let mut found: Vec<Location> = Vec::new();
    for definition in definitions {
        for location in type_definitions_of(index, definition).await {
            if !found.contains(&location) {
                found.push(location);
            }
        }
    }
    locs_to_opt_response(found)
}

/// The type definitions of the declaration at `definition`.
async fn type_definitions_of(index: &Indexer, definition: Location) -> Vec<Location> {
    match type_definition_step(index, &definition) {
        TypeDefinitionStep::Itself => vec![definition],
        TypeDefinitionStep::DeclaredAt(declaration) => vec![declaration],
        TypeDefinitionStep::TypeNamedAt(type_name) => definition_of_name_at(index, type_name).await,
        TypeDefinitionStep::Untyped | TypeDefinitionStep::Unreadable => Vec::new(),
    }
}

/// Go-to-definition on the type name at `type_name`.
async fn definition_of_name_at(index: &Indexer, type_name: Location) -> Vec<Location> {
    let position = type_name.range.start;
    let Some(ctx) = CursorContext::build(index, &type_name.uri, position) else {
        return Vec::new();
    };
    find_definition(&ctx, index, &type_name.uri, position)
        .await
        .map(response_locations)
        .unwrap_or_default()
}

fn response_locations(response: GotoDefinitionResponse) -> Vec<Location> {
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
#[path = "type_definition_tests.rs"]
mod tests;
