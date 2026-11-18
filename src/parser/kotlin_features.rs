//! Where a Kotlin file uses a construct only newer language versions
//! accept (see [`crate::language_version::KotlinFeature`]), at the place
//! the compiler's checkers report `UNSUPPORTED_FEATURE`.

use tree_sitter::Node;

use super::ts_to_lsp;
use crate::language_version::{KotlinFeature, LanguageFeature, LanguageFeatureUse};
use crate::types::SyntaxError;

pub(super) fn kotlin_features(root: Node<'_>, bytes: &[u8]) -> Vec<LanguageFeatureUse> {
    let mut found = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if let Some((feature, at)) = feature_at(node, root, bytes) {
            found.push(LanguageFeatureUse {
                feature: LanguageFeature::Kotlin(feature),
                position: ts_to_lsp(at.range()).start,
            });
        }
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
    found.sort_by_key(|feature_use| (feature_use.position.line, feature_use.position.character));
    found
}

fn text<'source>(node: Node<'_>, bytes: &'source [u8]) -> &'source str {
    std::str::from_utf8(&bytes[node.byte_range()]).unwrap_or("")
}

fn ancestors(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    std::iter::successors(node.parent(), |current| current.parent())
}

/// The construct `node` is, and the node it is reported at.
fn feature_at<'tree>(
    node: Node<'tree>,
    root: Node<'tree>,
    bytes: &[u8],
) -> Option<(KotlinFeature, Node<'tree>)> {
    match node.kind() {
        "guard_condition" => Some((KotlinFeature::WhenGuard, node)),
        "string_literal" if text(node, bytes).starts_with('$') => {
            Some((KotlinFeature::MultiDollarInterpolation, node))
        }
        "type_alias" => {
            let in_a_class = node
                .parent()
                .is_some_and(|parent| matches!(parent.kind(), "class_body" | "enum_class_body"));
            in_a_class.then_some((KotlinFeature::NestedTypeAlias, node))
        }
        "property_declaration" => explicit_backing_field(node, bytes)
            .map(|field| (KotlinFeature::ExplicitBackingField, field)),
        "jump_expression" if jumps_out_of_an_inline_lambda(node, root, bytes) => {
            Some((KotlinFeature::BreakContinueInInlineLambda, node))
        }
        _ => None,
    }
}

/// `field = 0` under `val total: Number`: tree-sitter-kotlin has no rule
/// for an explicit backing field and reads it as a property whose `val` is
/// missing, named `field`. The `field` word.
fn explicit_backing_field<'tree>(property: Node<'tree>, bytes: &[u8]) -> Option<Node<'tree>> {
    let binding = property
        .named_child(0)
        .filter(|binding| binding.kind() == "binding_pattern_kind")?;
    let val_is_missing = binding.child(0).is_some_and(|keyword| keyword.is_missing());
    let follows_a_property = property
        .prev_named_sibling()
        .is_some_and(|previous| previous.kind() == "property_declaration");
    let name = property
        .named_children(&mut property.walk())
        .find(|part| part.kind() == "variable_declaration")
        .and_then(|declaration| declaration.named_child(0))?;
    let named_field = text(name, bytes) == "field";
    (val_is_missing && follows_a_property && named_field).then_some(name)
}

/// The syntax errors of an explicit backing field, which is no error (its
/// version is checked as a [`KotlinFeature::ExplicitBackingField`]).
pub(super) fn describe_kotlin_node(node: Node<'_>, bytes: &[u8]) -> Option<Vec<SyntaxError>> {
    let property = node
        .parent()
        .and_then(|binding| binding.parent())
        .filter(|_| node.is_missing())?;
    explicit_backing_field(property, bytes).map(|_| Vec::new())
}

/// `break`/`continue` inside a lambda that leaves a loop around the lambda.
/// A lambda passed to a function this file declares without `inline` is
/// left alone: there the jump is an error at every version.
fn jumps_out_of_an_inline_lambda(jump: Node<'_>, root: Node<'_>, bytes: &[u8]) -> bool {
    let is_break_or_continue = jump.child(0).is_some_and(|keyword| {
        let word = text(keyword, bytes);
        word.starts_with("break") || word.starts_with("continue")
    });
    if !is_break_or_continue {
        return false;
    }
    let mut lambda = None;
    for ancestor in ancestors(jump) {
        match ancestor.kind() {
            "for_statement" | "while_statement" | "do_while_statement" => {
                return lambda
                    .is_some_and(|lambda| !passed_to_a_plain_function(lambda, root, bytes));
            }
            "lambda_literal" if lambda.is_none() => lambda = Some(ancestor),
            "function_declaration" | "anonymous_function" | "class_body" => return false,
            _ => {}
        }
    }
    false
}

/// Whether `lambda` is an argument of a call to a function declared in the
/// file without `inline`.
fn passed_to_a_plain_function(lambda: Node<'_>, root: Node<'_>, bytes: &[u8]) -> bool {
    let Some(call) = ancestors(lambda).find(|ancestor| ancestor.kind() == "call_expression") else {
        return false;
    };
    let Some(callee) = call
        .named_child(0)
        .filter(|callee| callee.kind() == "simple_identifier")
    else {
        return false;
    };
    let callee_name = text(callee, bytes);
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "function_declaration" {
            let named_like_the_callee = node
                .named_children(&mut node.walk())
                .find(|part| part.kind() == "simple_identifier")
                .is_some_and(|name| text(name, bytes) == callee_name);
            if named_like_the_callee {
                let declared_inline = node
                    .named_child(0)
                    .filter(|modifiers| modifiers.kind() == "modifiers")
                    .is_some_and(|modifiers| {
                        text(modifiers, bytes)
                            .split_whitespace()
                            .any(|word| word == "inline")
                    });
                return !declared_inline;
            }
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}
