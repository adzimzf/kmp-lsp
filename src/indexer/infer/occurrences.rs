//! Classify many text matches of a name at once — find-references' per-hit
//! question, answered for every `rg` hit of a request.
//!
//! Classifying a hit needs its file's parse tree. Files that aren't open are
//! parsed on demand and only the most recent few are remembered, so a
//! request whose hits span more files than that parsed each of them again on
//! every pass, one after another. Here the hits are grouped by file and the
//! files are spread over worker threads: each file is parsed once, by one
//! worker, and every hit in it is classified while its tree is at hand.

use tower_lsp::lsp_types::Location;

use crate::indexer::{Indexer, ResolvedType};
use crate::types::CursorPos;

use super::cst_symbol::{
    classify_occurrence_at, implicit_receiver_of, is_local_declaration_at,
    is_local_value_reference, OccurrenceSite, SymbolRole,
};

/// Fewer hits than this are classified on the calling thread: starting
/// workers costs more than it saves.
const MIN_HITS_FOR_WORKERS: usize = 32;

/// Most worker threads one request uses.
const MAX_WORKERS: usize = 8;

/// How much to find out about each hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OccurrenceDetail {
    /// What the hit is ([`OccurrenceSite`]) only.
    SiteOnly,
    /// Also where its name is bound ([`OccurrenceScope`]), for a member
    /// declaration or a bare reference — what verifying a member query
    /// against it needs.
    WithScope,
}

/// Where a classified hit's name is bound.
#[derive(Debug, Clone)]
pub(crate) enum OccurrenceScope {
    /// Not looked at: [`OccurrenceDetail::SiteOnly`], or a role the scope
    /// says nothing about (a member access, an import segment).
    Unchecked,
    /// A declaration in a function, method or lambda body, or a bare name
    /// that reads a local value or a parameter.
    Local,
    /// A bare name reached through this implicit receiver (an enclosing
    /// class, an extension or lambda receiver) that has a member of the name.
    ImplicitReceiver(ResolvedType),
    /// A member declaration, or a bare name no receiver in scope has.
    NotLocal,
}

/// One classified hit.
#[derive(Debug, Clone)]
pub(crate) struct ClassifiedOccurrence {
    pub(crate) site: OccurrenceSite,
    pub(crate) scope: OccurrenceScope,
}

/// Classify every location in `locations`, in order.
pub(crate) fn classify_occurrences(
    indexer: &Indexer,
    locations: &[&Location],
    detail: OccurrenceDetail,
) -> Vec<ClassifiedOccurrence> {
    let worker_count = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_WORKERS);
    if locations.len() < MIN_HITS_FOR_WORKERS || worker_count < 2 {
        return locations
            .iter()
            .map(|location| classify_occurrence(indexer, location, detail))
            .collect();
    }
    let assignments = assign_files_to_workers(locations, worker_count);
    let mut results: Vec<Option<ClassifiedOccurrence>> = vec![None; locations.len()];
    std::thread::scope(|scope| {
        let workers: Vec<_> = assignments
            .into_iter()
            .filter(|indices| !indices.is_empty())
            .map(|indices| {
                let handle = std::thread::Builder::new()
                    .name("kmp-lsp-references".to_owned())
                    .stack_size(crate::ANALYSIS_STACK_SIZE)
                    .spawn_scoped(scope, {
                        let indices = indices.clone();
                        move || {
                            indices
                                .iter()
                                .map(|&index| {
                                    classify_occurrence(indexer, locations[index], detail)
                                })
                                .collect::<Vec<_>>()
                        }
                    });
                (indices, handle)
            })
            .collect();
        for (indices, handle) in workers {
            let classified = match handle {
                Ok(handle) => handle.join().ok(),
                Err(error) => {
                    log::warn!("references: could not start a classification worker: {error}");
                    None
                }
            };
            match classified {
                Some(classified) => {
                    for (index, occurrence) in indices.into_iter().zip(classified) {
                        results[index] = Some(occurrence);
                    }
                }
                // A worker that panicked (or never started) leaves its hits
                // unclassified: kept, never rejected.
                None => {
                    for index in indices {
                        results[index] = Some(ClassifiedOccurrence {
                            site: OccurrenceSite::Unclassified,
                            scope: OccurrenceScope::Unchecked,
                        });
                    }
                }
            }
        }
    });
    results
        .into_iter()
        .map(|occurrence| {
            occurrence.unwrap_or(ClassifiedOccurrence {
                site: OccurrenceSite::Unclassified,
                scope: OccurrenceScope::Unchecked,
            })
        })
        .collect()
}

/// The hit indices each worker classifies: whole files, the file with the
/// most hits first, each to the worker with the fewest hits so far.
fn assign_files_to_workers(locations: &[&Location], worker_count: usize) -> Vec<Vec<usize>> {
    let mut files: Vec<(&str, Vec<usize>)> = Vec::new();
    let mut file_of_uri: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (index, location) in locations.iter().enumerate() {
        let uri = location.uri.as_str();
        let file = *file_of_uri.entry(uri).or_insert_with(|| {
            files.push((uri, Vec::new()));
            files.len() - 1
        });
        files[file].1.push(index);
    }
    files.sort_by_key(|(_, indices)| std::cmp::Reverse(indices.len()));
    let mut workers: Vec<Vec<usize>> = vec![Vec::new(); worker_count];
    for (_, indices) in files {
        let Some(lightest) = workers.iter_mut().min_by_key(|assigned| assigned.len()) else {
            break;
        };
        lightest.extend(indices);
    }
    workers
}

fn classify_occurrence(
    indexer: &Indexer,
    location: &Location,
    detail: OccurrenceDetail,
) -> ClassifiedOccurrence {
    let position = CursorPos {
        line: location.range.start.line as usize,
        utf16_col: location.range.start.character as usize,
    };
    let site = classify_occurrence_at(indexer, &location.uri, position);
    let scope = match (&site, detail) {
        (_, OccurrenceDetail::SiteOnly) => OccurrenceScope::Unchecked,
        (OccurrenceSite::Symbol(symbol), OccurrenceDetail::WithScope) => match &symbol.role {
            SymbolRole::Declaration { indexed: true } => {
                if is_local_declaration_at(indexer, &location.uri, position) {
                    OccurrenceScope::Local
                } else {
                    OccurrenceScope::NotLocal
                }
            }
            SymbolRole::Reference {
                receiver_type: None,
                ..
            } => bare_reference_scope(indexer, location, position),
            SymbolRole::Declaration { indexed: false }
            | SymbolRole::Reference {
                receiver_type: Some(_),
                ..
            }
            | SymbolRole::ImportSegment => OccurrenceScope::Unchecked,
        },
        (OccurrenceSite::NotCode(_) | OccurrenceSite::Unclassified, _) => {
            OccurrenceScope::Unchecked
        }
    };
    ClassifiedOccurrence { site, scope }
}

fn bare_reference_scope(
    indexer: &Indexer,
    location: &Location,
    position: CursorPos,
) -> OccurrenceScope {
    if is_local_value_reference(indexer, &location.uri, position) {
        return OccurrenceScope::Local;
    }
    match implicit_receiver_of(indexer, &location.uri, position) {
        Some(receiver) => OccurrenceScope::ImplicitReceiver(receiver),
        None => OccurrenceScope::NotLocal,
    }
}

#[cfg(test)]
#[path = "occurrences_tests.rs"]
mod tests;
