//! `textDocument/moniker`: a stable, qualified name for the symbol at the
//! cursor — `package.Outer.Inner` for a type, `package.Outer#member` for a
//! member — so that tools can match a symbol across files and indexes.
//!
//! The symbol is the declaration under the cursor or, from a use, the
//! declaration it resolves to (the backend's definition lookup).

use tower_lsp::lsp_types::{
    Location, Moniker, MonikerKind, Position, SymbolKind, UniquenessLevel, Url,
};

use crate::features::implementation::{is_class_kind, range_contains};
use crate::indexer::Indexer;
use crate::types::{SymbolEntry, Visibility};

/// The moniker of the symbol declared at `position` in `uri`, if its name is
/// there.
pub(crate) fn moniker_at(indexer: &Indexer, uri: &Url, position: Position) -> Option<Moniker> {
    let symbols = indexer.file_symbols(uri);
    let symbol = innermost(symbols.iter().filter(|symbol| {
        symbol.selection_range.start <= position && position <= symbol.selection_range.end
    }))?;
    Some(moniker(indexer, uri, &symbols, symbol))
}

/// The moniker of the symbol declared at `location` (its name): where a
/// definition lookup from a use lands.
pub(crate) fn moniker_declared_at(indexer: &Indexer, location: &Location) -> Option<Moniker> {
    let symbols = indexer.file_symbols(&location.uri);
    let symbol = innermost(
        symbols
            .iter()
            .filter(|symbol| range_contains(symbol.selection_range, location.range)),
    )?;
    Some(moniker(indexer, &location.uri, &symbols, symbol))
}

/// Of overlapping candidates, the most deeply nested declaration.
fn innermost<'a>(candidates: impl Iterator<Item = &'a SymbolEntry>) -> Option<&'a SymbolEntry> {
    candidates.max_by_key(|symbol| symbol.range.start)
}

fn moniker(indexer: &Indexer, uri: &Url, symbols: &[SymbolEntry], symbol: &SymbolEntry) -> Moniker {
    let local = symbol.visibility == Visibility::Private;
    Moniker {
        scheme: scheme(uri).to_owned(),
        identifier: qualified_name(indexer.package_of(uri).as_deref(), symbols, symbol),
        unique: if local {
            UniquenessLevel::Document
        } else {
            UniquenessLevel::Project
        },
        kind: Some(if local {
            MonikerKind::Local
        } else {
            MonikerKind::Export
        }),
    }
}

fn scheme(uri: &Url) -> &'static str {
    match crate::Language::from_path(uri.path()) {
        crate::Language::Java => "java",
        crate::Language::Swift => "swift",
        _ => "kotlin",
    }
}

/// `package.Outer.Inner` for a type, `package.Outer#member` for anything
/// declared inside a type, `package.member` for a top-level function.
pub(crate) fn qualified_name(
    package: Option<&str>,
    symbols: &[SymbolEntry],
    symbol: &SymbolEntry,
) -> String {
    let mut types: Vec<&SymbolEntry> = symbols
        .iter()
        .filter(|outer| {
            is_class_kind(outer.kind)
                && outer.range != symbol.range
                && range_contains(outer.range, symbol.range)
        })
        .collect();
    types.sort_by_key(|outer| outer.range.start);
    let mut name: String = package
        .filter(|package| !package.is_empty())
        .into_iter()
        .chain(types.iter().map(|outer| outer.name.as_str()))
        .collect::<Vec<_>>()
        .join(".");
    if !name.is_empty() {
        let member = !is_class_kind(symbol.kind) && !types.is_empty();
        name.push(if member { '#' } else { '.' });
    }
    name.push_str(&symbol.name);
    if is_callable(symbol.kind) {
        name.push_str("()");
    }
    name
}

fn is_callable(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::METHOD | SymbolKind::FUNCTION | SymbolKind::CONSTRUCTOR
    )
}

#[cfg(test)]
#[path = "moniker_tests.rs"]
mod tests;
