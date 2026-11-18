//! `goto_implementation` feature — find all implementors/subtypes of a symbol.
//!
//! Entry point: [`find_implementation`] dispatches to two paths:
//!
//! - **Type path** (`IService`, `BaseClass`): BFS the subtype graph to find all
//!   implementing/extending classes, with an rg cold-start fallback.
//! - **Method path** (`fun load()` inside an interface): resolve the declaring
//!   class, BFS its subtypes, then locate `override fun load` within each
//!   implementor's FileData.  Falls back to rg when the index is cold.

use std::collections::HashSet;
use std::sync::Arc;

use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, Range, SymbolKind, Url};

use crate::features::definition::locs_to_opt_response;
use crate::features::traits::{DocumentAccess, SearchAccess, SymbolIndex};
use crate::indexer::{classify_cursor, Indexer, SymbolRole};
use crate::rg;
use crate::types::{FileData, SymbolEntry};

/// Throttle counters for the join-panic warnings below — see
/// [`crate::util::throttled_warn`]. A panic here means goto-implementation
/// silently returns "no implementations", indistinguishable from a genuine
/// zero-implementors result.
static RG_FIND_IMPLEMENTORS_JOIN_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static RG_FIND_METHOD_OVERRIDES_JOIN_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Find implementations for the symbol at `position` — CST-resolved first
/// (works from a CALL SITE via the receiver's type, not just the declaration
/// line), falling back to the existing name+line path.
///
/// Returns `None` — never an error — whenever the CST can't classify the
/// cursor position, or classification succeeds but doesn't land on a typed
/// call reference; either case means "fall through to the string-first path
/// below."
pub(crate) async fn find_implementation_at(
    indexer: &Indexer,
    uri: &Url,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    if let Some(found) = java_use_site_implementations(indexer, uri, position).await {
        return locs_to_opt_response(found);
    }
    if let Some(sym) = classify_cursor(indexer, uri, position) {
        if let SymbolRole::Reference {
            receiver_type: Some(receiver_type),
            is_call: true,
            ..
        } = &sym.role
        {
            if let Some(response) = find_method_implementations(
                &sym.name,
                &receiver_type.type_path(),
                None,
                indexer,
                uri,
            )
            .await
            {
                return Some(response);
            }
        }
    }
    let (word, _) = indexer.word_and_qualifier_at(uri, position)?;
    find_implementation(&word, indexer, uri, position.line).await
}

/// Implementations of what a Java name used (not declared) at `position`
/// names, as jdtls answers them:
/// - `super.method()` is the superclass's method itself;
/// - a class's own method or the class itself is one of its
///   implementations, unless it is abstract (or an interface's) and
///   something implements it;
/// - an interface's method or the interface itself is only its
///   implementations, or itself when nothing implements it.
///
/// `None` when the cursor is not on a used Java type or method.
async fn java_use_site_implementations(
    indexer: &Indexer,
    uri: &Url,
    position: Position,
) -> Option<Vec<Location>> {
    if crate::Language::from_path(uri.path()) != crate::Language::Java {
        return None;
    }
    let symbol = classify_cursor(indexer, uri, position)?;
    if matches!(symbol.role, SymbolRole::Declaration { .. }) {
        return None;
    }
    let context = crate::backend::cursor::CursorContext::build(indexer, uri, position)?;
    let declarations: Vec<Location> =
        match crate::features::definition::find_definition(&context, indexer, uri, position).await?
        {
            GotoDefinitionResponse::Scalar(location) => vec![location],
            GotoDefinitionResponse::Array(locations) => locations,
            GotoDefinitionResponse::Link(_) => return None,
        };
    if context.qualifier.as_deref() == Some("super") {
        return Some(declarations);
    }
    let [declaration] = declarations.as_slice() else {
        return None;
    };
    let declared = indexer
        .file_symbols(&declaration.uri)
        .into_iter()
        .find(|entry| entry.selection_range == declaration.range)?;
    if is_class_kind(declared.kind) {
        let below = find_type_implementations(&declared.name, indexer, uri)
            .await
            .map(response_locations)
            .unwrap_or_default();
        let is_interface = declared.kind == SymbolKind::INTERFACE;
        return Some(with_declaration(declaration, below, !is_interface));
    }
    if declared.kind != SymbolKind::METHOD {
        return None;
    }
    let container = declared.container.as_deref()?;
    let overridden = OverriddenMethod::of(indexer, &declared, &declaration.uri);
    let overriding =
        find_method_implementations(&declared.name, container, Some(&overridden), indexer, uri)
            .await
            .map(response_locations)
            .unwrap_or_default();
    let in_an_interface = indexer
        .file_symbols(&declaration.uri)
        .iter()
        .any(|entry| entry.name == container && entry.kind == SymbolKind::INTERFACE);
    let has_a_body = !in_an_interface
        && !declared
            .detail
            .split_whitespace()
            .any(|word| word == "abstract");
    Some(with_declaration(declaration, overriding, has_a_body))
}

/// Implementations of the Java method declared at `line` named `word`, as
/// jdtls answers them: the methods overriding it with its parameters, and
/// the method itself when it has a body (a `default` method of an
/// interface, a method of a class that is not `abstract`) or nothing
/// overrides it. `None` when no Java method is declared there.
async fn java_declaration_site_implementations(
    word: &str,
    index: &(impl SymbolIndex + DocumentAccess + SearchAccess),
    uri: &Url,
    line: u32,
) -> Option<Vec<Location>> {
    if crate::Language::from_path(uri.path()) != crate::Language::Java {
        return None;
    }
    let symbols = index.file_symbols(uri);
    let declared = symbols.iter().find(|symbol| {
        symbol.name == word
            && symbol.kind == SymbolKind::METHOD
            && symbol.container.is_some()
            && symbol.selection_range.start.line == line
    })?;
    let container = declared.container.as_deref()?;
    let overridden = OverriddenMethod::of(index, declared, uri);
    let overriding = find_method_implementations(word, container, Some(&overridden), index, uri)
        .await
        .map(response_locations)
        .unwrap_or_default();
    let in_an_interface = symbols
        .iter()
        .any(|symbol| symbol.name == container && symbol.kind == SymbolKind::INTERFACE);
    let modifiers: Vec<&str> = declared.detail.split_whitespace().collect();
    let has_a_body = if in_an_interface {
        modifiers.contains(&"default")
    } else {
        !modifiers.contains(&"abstract")
    };
    let declaration = Location::new(uri.clone(), declared.selection_range);
    Some(with_declaration(&declaration, overriding, has_a_body))
}

/// The signature an overriding Java method must have: the overridden
/// method's parameter types, a type variable of it or its class standing
/// for any type (`T find(int id)` of `Repository<T>`), and where it is
/// declared (an enum's constant bodies override it there).
struct OverriddenMethod {
    parameter_keys: Vec<String>,
    type_variables: Vec<String>,
    uri: Url,
}

impl OverriddenMethod {
    fn of(index: &impl SymbolIndex, declared: &SymbolEntry, uri: &Url) -> Self {
        let mut type_variables: Vec<String> = declared.type_params().to_vec();
        let class_type_variables = index
            .file_symbols(uri)
            .into_iter()
            .filter(|symbol| Some(symbol.name.as_str()) == declared.container.as_deref())
            .filter(|symbol| is_class_kind(symbol.kind))
            .flat_map(|class| class.type_params().to_vec());
        type_variables.extend(class_type_variables);
        Self {
            parameter_keys: crate::indexer::parameter_type_keys(&declared.params),
            type_variables,
            uri: uri.clone(),
        }
    }

    /// Whether a method with parameters `params` overrides this one.
    fn is_overridden_by(&self, params: &str) -> bool {
        let keys = crate::indexer::parameter_type_keys(params);
        keys.len() == self.parameter_keys.len()
            && keys
                .iter()
                .zip(&self.parameter_keys)
                .all(|(overriding, overridden)| {
                    let element = overridden.trim_end_matches("[]");
                    overriding == overridden
                        || self
                            .type_variables
                            .iter()
                            .any(|variable| variable == element)
                })
    }
}

/// `found` with `declaration` in front when it is `itself_one` or nothing
/// else was found.
fn with_declaration(
    declaration: &Location,
    found: Vec<Location>,
    itself_one: bool,
) -> Vec<Location> {
    let mut found: Vec<Location> = found
        .into_iter()
        .filter(|location| location != declaration)
        .collect();
    if itself_one || found.is_empty() {
        found.insert(0, declaration.clone());
    }
    found
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

/// Find all implementations/subtypes of the symbol under the cursor at `uri`.
///
/// - If `word` names a method/function declared inside a class or interface,
///   returns the `override fun` locations across all implementors.
/// - Otherwise, returns the class/struct locations that implement the named type.
pub(crate) async fn find_implementation(
    word: &str,
    index: &(impl SymbolIndex + DocumentAccess + SearchAccess),
    uri: &Url,
    line: u32,
) -> Option<GotoDefinitionResponse> {
    if let Some(found) = java_declaration_site_implementations(word, index, uri, line).await {
        return locs_to_opt_response(found);
    }
    if let Some(declaring_class) = declaring_class_of_method(index, uri, word, line) {
        find_method_implementations(word, &declaring_class, None, index, uri).await
    } else {
        find_type_implementations(word, index, uri).await
    }
}

// ─── Type-level implementation ────────────────────────────────────────────────

/// BFS over the subtype graph to find all classes that implement/extend `type_name`.
/// rg fallback when the index is cold (no subtypes indexed yet).
async fn find_type_implementations(
    type_name: &str,
    index: &(impl SymbolIndex + DocumentAccess + SearchAccess),
    uri: &Url,
) -> Option<GotoDefinitionResponse> {
    let mut locs: Vec<Location> = index.subtypes_of(type_name);

    if locs.is_empty() {
        let file_path = uri.to_file_path().ok();
        let (root_opt, source_roots, matcher) = index.rg_scope_for_path(file_path.as_deref());
        let word_clone = type_name.to_string();
        let join_result = tokio::task::spawn_blocking(move || {
            rg::rg_find_implementors(
                &word_clone,
                root_opt.as_deref(),
                &source_roots,
                matcher.as_deref(),
            )
        })
        .await;
        if let Err(ref e) = join_result {
            crate::util::throttled_warn(&RG_FIND_IMPLEMENTORS_JOIN_FAILURES, 5, || {
                crate::util::join_failure_message(
                    &format!(
                        "running the rg goto-implementation fallback for `{type_name}` from {}",
                        uri.path()
                    ),
                    e,
                )
            });
        }
        let rg_impls = join_result.unwrap_or_default();
        if !rg_impls.is_empty() {
            return locs_to_opt_response(rg_impls);
        }
    }

    let mut queue: Vec<String> = locs
        .iter()
        .filter_map(|loc| {
            let data = index.file_data_for(loc.uri.as_str())?;
            data.symbols
                .iter()
                .find(|s| s.selection_range == loc.range)
                .map(|s| s.name.clone())
        })
        .collect();
    let mut visited: HashSet<String> = HashSet::from([type_name.to_string()]);
    while let Some(name) = queue.pop() {
        if visited.contains(&name) {
            continue;
        }
        visited.insert(name.clone());
        for loc in index.subtypes_of(&name) {
            if locs
                .iter()
                .any(|l| l.uri == loc.uri && l.range == loc.range)
            {
                continue;
            }
            if let Some(data) = index.file_data_for(loc.uri.as_str()) {
                if let Some(sym) = data.symbols.iter().find(|s| s.selection_range == loc.range) {
                    queue.push(sym.name.clone());
                }
            }
            locs.push(loc);
        }
    }

    locs_to_opt_response(locs)
}

// ─── Method-level implementation ──────────────────────────────────────────────

/// If `word` is a function/method declared inside a class or interface at `uri`,
/// returns the name of the declaring class.  Returns `None` for top-level
/// functions, constructors, and non-function symbols.
fn declaring_class_of_method(
    index: &impl SymbolIndex,
    uri: &Url,
    word: &str,
    line: u32,
) -> Option<String> {
    index
        .file_symbols(uri)
        .into_iter()
        .find(|s| {
            s.name == word
                && matches!(s.kind, SymbolKind::FUNCTION | SymbolKind::METHOD)
                && s.container.is_some()
                && s.selection_range.start.line == line
        })
        .and_then(|s| s.container)
}

/// Where a subtype found by [`SymbolIndex::subtypes_of`] declares its own
/// members, inside its file.
enum SubtypeBody<'data> {
    /// A declared class: its members name it as their container.
    Named(&'data SymbolEntry),
    /// A Java anonymous class: its members lie inside its body, though their
    /// container is the named class around it.
    Anonymous(Range),
    /// The subtype's declaration is not in its file's data: every member of
    /// the file may be its.
    Unlocated,
}

impl<'data> SubtypeBody<'data> {
    fn of(data: &'data FileData, subtype: &Location) -> Self {
        if let Some(class) = data
            .symbols
            .iter()
            .find(|symbol| symbol.selection_range == subtype.range)
        {
            return SubtypeBody::Named(class);
        }
        match data.anonymous_class_at(subtype.range) {
            Some(anonymous) => SubtypeBody::Anonymous(anonymous.body),
            None => SubtypeBody::Unlocated,
        }
    }

    /// Whether `member` (a symbol of the same file) is declared by this
    /// subtype itself, not by an enclosing or a nested class.
    fn declares(&self, member: &SymbolEntry, data: &FileData) -> bool {
        match self {
            SubtypeBody::Named(class) => {
                member.container.as_deref() == Some(class.name.as_str())
                    && range_contains(class.range, member.range)
            }
            SubtypeBody::Anonymous(body) => {
                let in_nested_class = data.symbols.iter().any(|symbol| {
                    is_class_kind(symbol.kind)
                        && range_contains(*body, symbol.range)
                        && range_contains(symbol.range, member.range)
                });
                range_contains(*body, member.range) && !in_nested_class
            }
            SubtypeBody::Unlocated => true,
        }
    }
}

pub(crate) fn is_class_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::CLASS
            | SymbolKind::INTERFACE
            | SymbolKind::STRUCT
            | SymbolKind::ENUM
            | SymbolKind::OBJECT
    )
}

pub(crate) fn range_contains(outer: Range, inner: Range) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

/// Collect `override fun method_name` (Kotlin) or same-named method (Java)
/// locations that the subtype at `subtype` declares itself — not a
/// same-named method of the class around it (`ReflectionHelper.isRecord`
/// next to its nested `RecordHelper` subclasses) or of another class in the
/// file.
///
/// For Kotlin, `detail` must contain `"override"` to exclude same-named
/// non-overriding overloads. For Java, `@Override` lives on the preceding line
/// and is not in the indexed detail, so all same-named methods of a confirmed
/// subtype are included.
fn method_overrides_in(
    method_name: &str,
    overridden: Option<&OverriddenMethod>,
    data: &Arc<FileData>,
    subtype: &Location,
) -> Vec<Location> {
    let lang = crate::Language::from_path(subtype.uri.path());
    let body = SubtypeBody::of(data, subtype);
    let java_signature_fits = |symbol: &SymbolEntry| match overridden {
        Some(overridden) if lang == crate::Language::Java => {
            overridden.is_overridden_by(&symbol.params)
        }
        _ => true,
    };
    data.symbols
        .iter()
        .filter(|s| {
            s.name == method_name
                && matches!(s.kind, SymbolKind::FUNCTION | SymbolKind::METHOD)
                && lang.detail_is_override(&s.detail)
                && body.declares(s, data)
                && java_signature_fits(s)
        })
        .map(|s| Location {
            uri: subtype.uri.clone(),
            range: s.selection_range,
        })
        .collect()
}

/// BFS the subtype graph of `declaring_class`, collecting `override fun method_name`
/// locations from each implementor's FileData.
/// rg fallback when the index is cold.
async fn find_method_implementations(
    method_name: &str,
    declaring_class: &str,
    overridden: Option<&OverriddenMethod>,
    index: &(impl SymbolIndex + DocumentAccess + SearchAccess),
    uri: &Url,
) -> Option<GotoDefinitionResponse> {
    let mut override_locs: Vec<Location> =
        enum_constant_overrides(method_name, declaring_class, overridden, index);
    let mut queue: Vec<String> = vec![declaring_class.to_string()];
    let mut visited: HashSet<String> = HashSet::from([declaring_class.to_string()]);

    while let Some(type_name) = queue.pop() {
        for class_loc in index.subtypes_of(&type_name) {
            let Some(data) = index.file_data_for(class_loc.uri.as_str()) else {
                continue;
            };

            // Enqueue this subtype's name for transitive BFS.
            if let Some(class_sym) = data
                .symbols
                .iter()
                .find(|s| s.selection_range == class_loc.range)
            {
                let class_name = class_sym.name.clone();
                if !visited.contains(&class_name) {
                    visited.insert(class_name.clone());
                    queue.push(class_name);
                }
            }

            // Collect all override sites for the method in this file.
            for loc in method_overrides_in(method_name, overridden, &data, &class_loc) {
                if !override_locs
                    .iter()
                    .any(|l| l.uri == loc.uri && l.range == loc.range)
                {
                    override_locs.push(loc);
                }
            }
        }
    }

    // rg fallback when index is cold (implementors not yet indexed).
    if override_locs.is_empty() {
        let file_path = uri.to_file_path().ok();
        let (root_opt, source_roots, matcher) = index.rg_scope_for_path(file_path.as_deref());
        let method = method_name.to_string();
        let class = declaring_class.to_string();
        let join_result = tokio::task::spawn_blocking(move || {
            rg::rg_find_method_overrides(
                &method,
                &class,
                root_opt.as_deref(),
                &source_roots,
                matcher.as_deref(),
            )
        })
        .await;
        if let Err(ref e) = join_result {
            crate::util::throttled_warn(&RG_FIND_METHOD_OVERRIDES_JOIN_FAILURES, 5, || {
                crate::util::join_failure_message(
                    &format!(
                        "running the rg override-search fallback for `{method_name}` in \
                         `{declaring_class}` from {}",
                        uri.path()
                    ),
                    e,
                )
            });
        }
        let rg_locs = join_result.unwrap_or_default();
        return locs_to_opt_response(rg_locs);
    }

    locs_to_opt_response(override_locs)
}

/// The methods of the Java enum `declaring_class`'s constant bodies
/// (`PLUS { int apply(..) {..} }`) that override its `method_name`: each
/// constant with a body is an anonymous subclass of the enum.
fn enum_constant_overrides(
    method_name: &str,
    declaring_class: &str,
    overridden: Option<&OverriddenMethod>,
    index: &impl SymbolIndex,
) -> Vec<Location> {
    let Some(overridden) = overridden else {
        return Vec::new();
    };
    let symbols = index.file_symbols(&overridden.uri);
    let is_an_enum = symbols
        .iter()
        .any(|symbol| symbol.name == declaring_class && symbol.kind == SymbolKind::ENUM);
    if !is_an_enum {
        return Vec::new();
    }
    let constants: Vec<&SymbolEntry> = symbols
        .iter()
        .filter(|symbol| {
            symbol.kind == SymbolKind::ENUM_MEMBER
                && symbol.container.as_deref() == Some(declaring_class)
        })
        .collect();
    symbols
        .iter()
        .filter(|symbol| symbol.name == method_name && symbol.kind == SymbolKind::METHOD)
        .filter(|method| {
            constants
                .iter()
                .any(|constant| range_contains(constant.range, method.range))
        })
        .filter(|method| overridden.is_overridden_by(&method.params))
        .map(|method| Location::new(overridden.uri.clone(), method.selection_range))
        .collect()
}

#[cfg(test)]
#[path = "implementation_tests.rs"]
mod tests;
