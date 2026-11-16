//! Signature help feature — extracts the active call site via CST and returns parameter info.
//!
//! The CST (`cst_call_info`) is authoritative: it walks up the live tree-sitter parse tree to
//! find the enclosing `call_expression`, counts `value_argument` children for `active_param`,
//! and handles multiline calls naturally.
//!
//! When the closing `)` is absent (live typing mid-argument) the CST cannot form a
//! `call_expression`, so `call_info_at` uses a text-scan fallback on the current line.
//!
//! When the cursor is inside a nested call (e.g. `setOf()`) whose signature cannot be resolved,
//! the outer call is tried as a fallback so the user still sees helpful parameter info.
//!
//! Java calls are answered by [`java_signature_help`] instead: the callee is
//! resolved as go-to-definition resolves it (receiver type, imports, overloads
//! narrowed by the argument types), and every overload declared beside it is
//! listed, the resolved one active — as jdtls answers.

use tower_lsp::lsp_types::{
    ParameterInformation, ParameterLabel, Position, SignatureHelp, SignatureInformation, Url,
};

use tower_lsp::lsp_types::Location;
use tree_sitter::{Node, Point};

use super::traits::{LiveTreeAccess, SignatureIndex};
use crate::features::call_hierarchy::callee_name;
use crate::features::parameter_hints::callee_declarations;
use crate::features::selection_range::point_of;
use crate::indexer::{split_params_at_depth_zero, Indexer};
use crate::inlay_hints::{line_starts, ts_pos_to_lsp};
use crate::types::SymbolEntry;

/// Compute signature help for the call under the cursor at `pos` in `uri`.
///
/// Returns `None` when:
/// - no live parse tree exists for the file (not yet opened/edited),
/// - the cursor is not inside a `call_expression` (e.g. inside a trailing lambda body), or
/// - the function name cannot be resolved to a known signature.
pub(crate) fn compute_signature_help(
    uri: &Url,
    pos: Position,
    index: &(impl SignatureIndex + LiveTreeAccess),
) -> Option<SignatureHelp> {
    let ci = index.call_info_at(pos, uri)?;

    if let Some(params_text) =
        index.find_fun_signature_with_receiver(uri, &ci.fn_name, ci.qualifier.as_deref())
    {
        return build_signature_help(&ci.fn_name, &params_text, ci.active_param);
    }

    // Inner call's signature not found (e.g. stdlib overloaded function like `setOf`).
    // Try the enclosing call expression so the user still sees the outer parameter info.
    let outer = index.outer_call_info_at(pos, uri)?;
    let params_text =
        index.find_fun_signature_with_receiver(uri, &outer.fn_name, outer.qualifier.as_deref())?;
    build_signature_help(&outer.fn_name, &params_text, outer.active_param)
}

/// Signature help for the Java call at `site` (see [`java_call_site`]);
/// `None` when its callee doesn't resolve.
pub(crate) async fn java_signature_help(
    indexer: &Indexer,
    uri: &Url,
    site: JavaCallSite,
) -> Option<SignatureHelp> {
    let declarations = callee_declarations(indexer, uri, site.callee).await;
    let (overloads, resolved_index, substitution) = match declarations.first() {
        Some(resolved) => {
            let (overloads, resolved_index) = overloads_of(indexer, resolved)?;
            let substitution =
                receiver_substitution(indexer, resolved, site.receiver_type.as_deref());
            (overloads, resolved_index, substitution)
        }
        None => {
            let overloads = same_file_overloads(uri, &site)?;
            (overloads, 0, Vec::new())
        }
    };
    let active = match site.argument {
        Some(argument) => with_room_for(&overloads, resolved_index, argument),
        None => resolved_index,
    };
    let signatures: Vec<SignatureInformation> = overloads
        .iter()
        .map(|symbol| java_signature(symbol, site.argument, &substitution))
        .collect();
    let active_parameter = signatures[active].active_parameter;
    Some(SignatureHelp {
        signatures,
        active_signature: Some(u32::try_from(active).ok()?),
        active_parameter,
    })
}

/// The methods named like the callee of a call still being typed, read from
/// the file with that call closed (see [`JavaCallSite::closed_source`]).
fn same_file_overloads(uri: &Url, site: &JavaCallSite) -> Option<Vec<SymbolEntry>> {
    let closed_source = site.closed_source.as_deref()?;
    let line = closed_source.lines().nth(site.callee.line as usize)?;
    let start: usize = line
        .char_indices()
        .scan(0u32, |units, (byte, character)| {
            let at = *units;
            *units += character.len_utf16() as u32;
            Some((byte, at))
        })
        .find(|(_, units)| *units == site.callee.character)
        .map(|(byte, _)| byte)?;
    let name: String = line[start..]
        .chars()
        .take_while(|character| character.is_alphanumeric() || matches!(character, '_' | '$'))
        .collect();
    let file = crate::parser::parse_by_extension(uri.path(), closed_source);
    let mut overloads: Vec<SymbolEntry> = file
        .symbols
        .into_iter()
        .filter(|symbol| {
            symbol.name == name
                && matches!(
                    symbol.kind,
                    tower_lsp::lsp_types::SymbolKind::METHOD
                        | tower_lsp::lsp_types::SymbolKind::CONSTRUCTOR
                )
        })
        .collect();
    overloads.sort_by_key(|symbol| symbol.range.start);
    (!overloads.is_empty()).then_some(overloads)
}

/// The overload to show active for the cursor in argument `argument`: the
/// resolved one when it takes that many arguments, else the first one in
/// source order that does (jdtls moves on to `foo(int, String)` once a
/// second argument is typed after `foo(int)`'s one).
fn with_room_for(overloads: &[SymbolEntry], resolved: usize, argument: u32) -> usize {
    let has_room = |symbol: &SymbolEntry| {
        let parameters = java_parameters(symbol);
        let is_varargs = parameters.last().is_some_and(|last| last.contains("..."));
        // The first argument may still be empty: `V()` fits `new V(|)`.
        argument == 0 || is_varargs || (argument as usize) < parameters.len()
    };
    if overloads.get(resolved).is_some_and(has_room) {
        return resolved;
    }
    overloads.iter().position(has_room).unwrap_or(resolved)
}

/// A Java call around the cursor: where its callee is named, which of its
/// arguments the cursor is in, and the type of its receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JavaCallSite {
    pub(crate) callee: Position,
    /// The argument the cursor is in, from 0; `None` with the cursor on the
    /// callee's name, where no parameter is active.
    pub(crate) argument: Option<u32>,
    /// The receiver's type as written, with its type arguments
    /// (`Shelf<String, Circle>`), when the call has a receiver of known type.
    pub(crate) receiver_type: Option<String>,
    /// For a call still being typed, the file with that call closed: the
    /// parser reads what follows an open `(` as part of the call, so the
    /// declarations after it are only seen in this text.
    pub(crate) closed_source: Option<String>,
}

/// The innermost Java call whose argument list holds `pos` (after its `(`,
/// up to its `)`), or whose method name `pos` is on. A call still being
/// typed (no `)` yet, so the parser has no call node) is found in the text.
/// `None` for any other language.
pub(crate) fn java_call_site(indexer: &Indexer, uri: &Url, pos: Position) -> Option<JavaCallSite> {
    if crate::Language::from_path(uri.path()) != crate::Language::Java {
        return None;
    }
    let doc = indexer.live_doc_or_parse(uri)?;
    let point = point_of(&doc.bytes, pos)?;
    let starts = line_starts(&doc.bytes);
    let mut node = doc
        .tree
        .root_node()
        .descendant_for_point_range(point, point);
    let on_a_name = node.filter(|found| matches!(found.kind(), "identifier" | "type_identifier"));
    if let Some(call) = on_a_name.and_then(call_named_by) {
        let callee = called_name(call)?;
        return Some(JavaCallSite {
            callee: ts_pos_to_lsp(callee.start_position(), &starts, &doc.bytes),
            argument: None,
            receiver_type: receiver_type_of(call, &doc, indexer, uri),
            closed_source: None,
        });
    }
    while let Some(current) = node {
        if current.kind() == "argument_list" && holds(current, point) {
            if let Some(call) = current.parent() {
                if let Some(callee) = called_name(call) {
                    return Some(JavaCallSite {
                        callee: ts_pos_to_lsp(callee.start_position(), &starts, &doc.bytes),
                        argument: Some(argument_index(current, point)),
                        receiver_type: receiver_type_of(call, &doc, indexer, uri),
                        closed_source: None,
                    });
                }
            }
        }
        node = current.parent();
    }
    unclosed_call_site(&doc.bytes, &starts, point)
}

/// The call whose method name (or `new`'s type) is `name`.
fn call_named_by(name: Node<'_>) -> Option<Node<'_>> {
    let mut call = name.parent()?;
    if call.kind() == "generic_type" {
        call = call.parent()?;
    }
    let names_it = matches!(
        call.kind(),
        "method_invocation" | "object_creation_expression"
    ) && called_name(call).is_some_and(|callee| callee.id() == name.id());
    names_it.then_some(call)
}

/// The type of `call`'s receiver (`shelf` of `shelf.put(..)`), as written
/// with its type arguments.
fn receiver_type_of(
    call: Node<'_>,
    doc: &crate::indexer::live_tree::LiveDoc,
    indexer: &Indexer,
    uri: &Url,
) -> Option<String> {
    let receiver = call
        .child_by_field_name("object")
        .filter(|_| call.kind() == "method_invocation")?;
    let query = crate::indexer::CstQuery::new(receiver, doc, indexer, uri);
    match query.expr_type() {
        crate::indexer::Resolution::Resolved(resolved) => Some(resolved.as_type_str().to_owned()),
        _ => None,
    }
}

/// The call being typed before `point` that the parser could not make a
/// call of: the name before the nearest `(` left open, and how many `,`
/// follow it. `None` when no call is open in the statement.
fn unclosed_call_site(bytes: &[u8], starts: &[usize], point: Point) -> Option<JavaCallSite> {
    let cursor = starts.get(point.row)? + point.column;
    let before = bytes.get(..cursor)?;
    let mut depth = 0usize;
    let mut commas = 0u32;
    for (index, byte) in before.iter().enumerate().rev() {
        match byte {
            b')' | b']' => depth += 1,
            b'(' if depth > 0 => depth -= 1,
            b'[' if depth > 0 => depth -= 1,
            b',' if depth == 0 => commas += 1,
            b';' | b'{' | b'}' if depth == 0 => return None,
            b'(' => {
                let name_end = before[..index]
                    .iter()
                    .rposition(|byte| !byte.is_ascii_whitespace())?
                    + 1;
                let name_start = before[..name_end]
                    .iter()
                    .rposition(|byte| {
                        !(byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'$')
                    })
                    .map_or(0, |separator| separator + 1);
                if name_start == name_end {
                    return None;
                }
                let row = starts.iter().rposition(|start| *start <= name_start)?;
                let column = name_start - starts[row];
                let callee = ts_pos_to_lsp(Point { row, column }, starts, bytes);
                let mut closed_source = String::from_utf8_lossy(before).into_owned();
                closed_source.push_str(");");
                closed_source.push_str(&String::from_utf8_lossy(&bytes[cursor..]));
                return Some(JavaCallSite {
                    callee,
                    argument: Some(commas),
                    receiver_type: None,
                    closed_source: Some(closed_source),
                });
            }
            _ => {}
        }
    }
    None
}

/// Whether `point` is inside `arguments`' parentheses: after the `(`, at the
/// `)` at the latest.
fn holds(arguments: Node<'_>, point: Point) -> bool {
    let after_open = arguments.start_position() < point;
    let closed = arguments
        .child(u32::try_from(arguments.child_count().saturating_sub(1)).unwrap_or(0))
        .filter(|last| last.kind() == ")" && !last.is_missing());
    let before_close = closed.map_or(point <= arguments.end_position(), |close| {
        point <= close.start_position()
    });
    after_open && before_close
}

/// The name a call node calls: the method of an invocation, the type of a
/// `new`, the `this` / `super` of a constructor call.
pub(crate) fn called_name(call: Node<'_>) -> Option<Node<'_>> {
    match call.kind() {
        "explicit_constructor_invocation" => call.child_by_field_name("constructor"),
        _ => callee_name(call),
    }
}

/// How many `,` of `arguments` come before `point` — those the parser put
/// in an error node too (`foo(2, |)` has no second argument yet).
fn argument_index(arguments: Node<'_>, point: Point) -> u32 {
    let mut cursor = arguments.walk();
    let mut commas = 0usize;
    for child in arguments.children(&mut cursor) {
        if child.kind() == "," && child.end_position() <= point {
            commas += 1;
        } else if child.kind() == "ERROR" {
            let mut error_cursor = child.walk();
            commas += child
                .children(&mut error_cursor)
                .filter(|inner| inner.kind() == "," && inner.end_position() <= point)
                .count();
        }
    }
    u32::try_from(commas).unwrap_or(u32::MAX)
}

/// The methods (or constructors) declared beside the one at `resolved` under
/// its name, in source order, and the index of the resolved one.
fn overloads_of(indexer: &Indexer, resolved: &Location) -> Option<(Vec<SymbolEntry>, usize)> {
    let file = indexer
        .files
        .get(resolved.uri.as_str())
        .map(|file| std::sync::Arc::clone(file.value()))
        .or_else(|| {
            indexer
                .jar_files
                .get(resolved.uri.as_str())
                .map(|file| std::sync::Arc::clone(file.value()))
        })?;
    let target = file
        .symbols
        .iter()
        .find(|symbol| symbol.selection_range == resolved.range)?;
    if matches!(
        target.kind,
        tower_lsp::lsp_types::SymbolKind::CLASS | tower_lsp::lsp_types::SymbolKind::STRUCT
    ) {
        return implicit_constructor(&file.symbols, target);
    }
    if !matches!(
        target.kind,
        tower_lsp::lsp_types::SymbolKind::METHOD | tower_lsp::lsp_types::SymbolKind::CONSTRUCTOR
    ) {
        return None;
    }
    let mut overloads: Vec<SymbolEntry> = file
        .symbols
        .iter()
        .filter(|symbol| {
            symbol.kind == target.kind
                && symbol.name == target.name
                && symbol.container == target.container
        })
        .cloned()
        .collect();
    overloads.sort_by_key(|symbol| symbol.range.start);
    let active = overloads
        .iter()
        .position(|symbol| symbol.selection_range == target.selection_range)?;
    Some((overloads, active))
}

/// The constructor a class with none declared has: a record's canonical
/// one, taking its components, or a class's no-argument default one. `None`
/// when the class declares constructors (the lookup then named one of them)
/// or isn't a class.
fn implicit_constructor(
    symbols: &[SymbolEntry],
    class: &SymbolEntry,
) -> Option<(Vec<SymbolEntry>, usize)> {
    let declares_one = symbols.iter().any(|symbol| {
        symbol.kind == tower_lsp::lsp_types::SymbolKind::CONSTRUCTOR
            && symbol.name == class.name
            && symbol.container.as_deref() == Some(class.name.as_str())
    });
    if declares_one {
        return None;
    }
    let mut constructor = class.clone();
    constructor.kind = tower_lsp::lsp_types::SymbolKind::CONSTRUCTOR;
    // A record's components are its header: `record Edge(int from, ..)`.
    constructor.params = if class.kind == tower_lsp::lsp_types::SymbolKind::STRUCT {
        record_components(&class.detail).to_owned()
    } else {
        String::new()
    };
    Some((vec![constructor], 0))
}

/// `int from, int to` of `record Edge(int from, int to)`.
fn record_components(header: &str) -> &str {
    let Some(open) = header.find('(') else {
        return "";
    };
    let after = &header[open..];
    let close = closing_end(after, '(', ')');
    after.get(1..close.saturating_sub(1)).unwrap_or_default()
}

/// What a type variable of the callee's class stands for at the call:
/// `K` → `String` for `shelf.put(..)` on a `Shelf<String, Circle>`.
fn receiver_substitution(
    indexer: &Indexer,
    resolved: &Location,
    receiver_type: Option<&str>,
) -> Vec<(String, String)> {
    let Some(receiver_type) = receiver_type else {
        return Vec::new();
    };
    let Some(open) = receiver_type.find('<') else {
        return Vec::new();
    };
    let arguments_text = receiver_type[open + 1..].trim_end().trim_end_matches('>');
    let arguments = split_params_at_depth_zero(arguments_text);
    let symbols = indexer.file_symbols(&resolved.uri);
    let Some(member) = symbols
        .iter()
        .find(|symbol| symbol.selection_range == resolved.range)
    else {
        return Vec::new();
    };
    let Some(class) = symbols.iter().find(|symbol| {
        Some(symbol.name.as_str()) == member.container.as_deref()
            && !symbol.type_params().is_empty()
    }) else {
        return Vec::new();
    };
    class
        .type_params()
        .iter()
        .zip(arguments)
        .map(|(parameter, argument)| {
            let variable = parameter.split_whitespace().next().unwrap_or(parameter);
            (variable.to_owned(), argument.trim().to_owned())
        })
        .collect()
}

/// `parameter` with each type variable of `substitution` replaced, whole
/// words only (`Key` is not `K`).
fn substituted(parameter: &str, substitution: &[(String, String)]) -> String {
    if substitution.is_empty() {
        return parameter.to_owned();
    }
    let mut result = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, result: &mut String| {
        let replaced = substitution
            .iter()
            .find(|(variable, _)| variable == word)
            .map_or(word.as_str(), |(_, argument)| argument.as_str());
        result.push_str(replaced);
        word.clear();
    };
    for character in parameter.chars() {
        if character.is_alphanumeric() || character == '_' || character == '$' {
            word.push(character);
        } else {
            flush(&mut word, &mut result);
            result.push(character);
        }
    }
    flush(&mut word, &mut result);
    result
}

/// `symbol`'s parameters as signature help labels them.
fn java_parameters(symbol: &SymbolEntry) -> Vec<String> {
    let params = symbol.params.trim();
    let params = params.strip_prefix('(').unwrap_or(params);
    let params = params.strip_suffix(')').unwrap_or(params);
    split_params_at_depth_zero(params)
        .into_iter()
        .map(java_parameter_label)
        .filter(|label| !label.is_empty())
        .collect()
}

/// `assertThat(String actual)` for `assertThat(@Nullable String actual)`,
/// the cursor's argument active (the last parameter for any argument past it,
/// as a varargs parameter takes them all); none active on the method's name.
fn java_signature(
    symbol: &SymbolEntry,
    argument: Option<u32>,
    substitution: &[(String, String)],
) -> SignatureInformation {
    let parameters: Vec<String> = java_parameters(symbol)
        .iter()
        .map(|parameter| substituted(parameter, substitution))
        .collect();
    let label = format!("{}({})", symbol.name, parameters.join(", "));
    let active = argument.map(|argument| {
        argument.min(u32::try_from(parameters.len().saturating_sub(1)).unwrap_or(0))
    });
    SignatureInformation {
        label,
        documentation: None,
        parameters: Some(
            parameters
                .into_iter()
                .map(|parameter| ParameterInformation {
                    label: ParameterLabel::Simple(parameter),
                    documentation: None,
                })
                .collect(),
        ),
        active_parameter: active,
    }
}

/// A Java parameter as signature help shows it: its type and name, without
/// annotations or `final`, whitespace collapsed.
pub(crate) fn java_parameter_label(parameter: &str) -> String {
    strip_declaration_prefix(parameter)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` past its leading annotations (`@Nullable`, `@SuppressWarnings("x")`),
/// modifiers (`public static final`) and type parameters (`<T>`): from a
/// declaration's text, where its type starts.
pub(crate) fn strip_declaration_prefix(text: &str) -> &str {
    let mut rest = text.trim_start();
    loop {
        if let Some(annotated) = rest.strip_prefix('@') {
            let name_end = annotated
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '$'))
                .unwrap_or(annotated.len());
            rest = annotated[name_end..].trim_start();
            if rest.starts_with('(') {
                rest = rest[closing_end(rest, '(', ')')..].trim_start();
            }
        } else if rest.starts_with('<') {
            rest = rest[closing_end(rest, '<', '>')..].trim_start();
        } else if let Some(word) = rest
            .split_whitespace()
            .next()
            .filter(|word| is_modifier(word))
        {
            rest = rest[word.len()..].trim_start();
        } else {
            return rest;
        }
    }
}

fn is_modifier(word: &str) -> bool {
    matches!(
        word,
        "public"
            | "protected"
            | "private"
            | "static"
            | "final"
            | "abstract"
            | "synchronized"
            | "native"
            | "default"
            | "transient"
            | "volatile"
            | "strictfp"
    )
}

/// The byte just past the `close` matching the `open` that `text` starts with.
fn closing_end(text: &str, open: char, close: char) -> usize {
    let mut depth = 0usize;
    for (index, c) in text.char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return index + c.len_utf8();
            }
        }
    }
    text.len()
}

fn build_signature_help(
    fn_name: &str,
    params_text: &str,
    active_param: u32,
) -> Option<SignatureHelp> {
    let raw = params_text.trim_matches(|c| c == '(' || c == ')');
    let param_parts: Vec<String> = split_params_at_depth_zero(raw)
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let parameters: Vec<ParameterInformation> = param_parts
        .iter()
        .map(|p| ParameterInformation {
            label: ParameterLabel::Simple(p.clone()),
            documentation: None,
        })
        .collect();
    let label = format!("{}({})", fn_name, param_parts.join(", "));
    let active_param = active_param.min(parameters.len().saturating_sub(1) as u32);
    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation: None,
            parameters: Some(parameters),
            active_parameter: Some(active_param),
        }],
        active_signature: Some(0),
        active_parameter: Some(active_param),
    })
}

#[cfg(test)]
#[path = "signature_help_tests.rs"]
mod tests;
