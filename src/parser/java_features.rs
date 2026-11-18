//! Where a Java file uses a construct only some releases accept (see
//! [`crate::language_version::JavaFeature`]), at the place javac reports it
//! when the release lacks it. Checked with `javac --release` of JDK 25 by
//! `scripts/check-java-version-cases.py` over the itest cases.

use tree_sitter::Node;

use super::ts_to_lsp;
use crate::language_version::{JavaFeature, LanguageFeature, LanguageFeatureUse};
use tower_lsp::lsp_types::Position;

pub(super) fn java_features(root: Node<'_>, bytes: &[u8]) -> Vec<LanguageFeatureUse> {
    let mut found = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        found.extend(feature_at(node, bytes));
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
    found.extend(compact_source_file(root));
    found.sort_by_key(|feature_use| (feature_use.position.line, feature_use.position.character));
    found
}

fn feature_use(feature: JavaFeature, position: Position) -> LanguageFeatureUse {
    LanguageFeatureUse {
        feature: LanguageFeature::Java(feature),
        position,
    }
}

fn start_of(node: Node<'_>) -> Position {
    ts_to_lsp(node.range()).start
}

fn end_of(node: Node<'_>) -> Position {
    ts_to_lsp(node.range()).end
}

fn text<'source>(node: Node<'_>, bytes: &'source [u8]) -> &'source str {
    std::str::from_utf8(&bytes[node.byte_range()]).unwrap_or("")
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

/// The constructs `node` itself is (several for a lambda's parameters).
fn feature_at(node: Node<'_>, bytes: &[u8]) -> Vec<LanguageFeatureUse> {
    let single = |found: Option<LanguageFeatureUse>| found.into_iter().collect();
    match node.kind() {
        "method_declaration" => single(private_interface_method(node)),
        "resource" => resource_features(node, bytes),
        "local_variable_declaration" | "enhanced_for_statement" => {
            single(var_type(node.child_by_field_name("type"), bytes))
        }
        "lambda_expression" => var_lambda_parameters(node, bytes),
        "switch_expression" => single(switch_expression(node)),
        "switch_rule" => single(
            children(node)
                .into_iter()
                .find(|part| part.kind() == "->")
                .map(|arrow| feature_use(JavaFeature::ArrowCaseLabel, start_of(arrow))),
        ),
        "string_literal" if text(node, bytes).starts_with("\"\"\"") => {
            single(Some(feature_use(JavaFeature::TextBlock, start_of(node))))
        }
        "instanceof_expression" => single(
            node.child_by_field_name("name")
                .map(|name| feature_use(JavaFeature::InstanceofPattern, start_of(name))),
        ),
        "record_declaration" => children(node)
            .into_iter()
            .find(|part| part.kind() == "record")
            .map(|keyword| feature_use(JavaFeature::Record, start_of(keyword)))
            .into_iter()
            .chain(type_named_var(node, bytes))
            .collect(),
        "sealed" | "non-sealed" | "permits" => {
            single(Some(feature_use(JavaFeature::SealedType, start_of(node))))
        }
        "switch_label" => single(
            children(node)
                .into_iter()
                .find(|part| part.kind() == "pattern")
                .map(|pattern| feature_use(JavaFeature::SwitchPattern, start_of(pattern))),
        ),
        "record_pattern" => single(
            children(node)
                .into_iter()
                .find(|part| part.kind() == "record_pattern_body")
                .map(|body| feature_use(JavaFeature::RecordPattern, start_of(body))),
        ),
        "underscore_pattern" => single(Some(underscore(node))),
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "annotation_type_declaration" => single(type_named_var(node, bytes)),
        "import_declaration" => single(module_import(node, bytes)),
        "constructor_body" => single(statement_before_super(node, bytes)),
        _ => Vec::new(),
    }
}

/// `private String prefix()` in an interface: javac reports it at the
/// parameter list.
fn private_interface_method(method: Node<'_>) -> Option<LanguageFeatureUse> {
    let in_interface = method
        .parent()
        .is_some_and(|body| body.kind() == "interface_body");
    let is_private = children(method)
        .into_iter()
        .filter(|part| part.kind() == "modifiers")
        .flat_map(children)
        .any(|modifier| modifier.kind() == "private");
    if !in_interface || !is_private {
        return None;
    }
    let parameters = method.child_by_field_name("parameters")?;
    Some(feature_use(
        JavaFeature::PrivateInterfaceMethod,
        start_of(parameters),
    ))
}

/// `try (resource)` names a variable instead of declaring one (javac
/// reports it after the name); `try (var reader = …)` declares one with
/// `var`.
fn resource_features(resource: Node<'_>, bytes: &[u8]) -> Vec<LanguageFeatureUse> {
    if let Some(declared_type) = resource.child_by_field_name("type") {
        return var_type(Some(declared_type), bytes).into_iter().collect();
    }
    let names_a_variable = resource.named_child_count() == 1
        && resource
            .named_child(0)
            .is_some_and(|only| matches!(only.kind(), "identifier" | "field_access"));
    if !names_a_variable {
        return Vec::new();
    }
    vec![feature_use(JavaFeature::ResourceVariable, end_of(resource))]
}

/// A type written `var`.
fn var_type(declared_type: Option<Node<'_>>, bytes: &[u8]) -> Option<LanguageFeatureUse> {
    let declared_type = declared_type?;
    let is_var = declared_type.kind() == "type_identifier" && text(declared_type, bytes) == "var";
    is_var.then(|| feature_use(JavaFeature::VarType, start_of(declared_type)))
}

/// `(var a, var b) ->`: each `var`, and the lambda form at the first
/// parameter's name.
fn var_lambda_parameters(lambda: Node<'_>, bytes: &[u8]) -> Vec<LanguageFeatureUse> {
    let Some(parameters) = lambda
        .child_by_field_name("parameters")
        .filter(|parameters| parameters.kind() == "formal_parameters")
    else {
        return Vec::new();
    };
    let var_parameters: Vec<Node<'_>> = children(parameters)
        .into_iter()
        .filter(|parameter| parameter.kind() == "formal_parameter")
        .filter(|parameter| var_type(parameter.child_by_field_name("type"), bytes).is_some())
        .collect();
    let mut found: Vec<LanguageFeatureUse> = var_parameters
        .iter()
        .filter_map(|parameter| var_type(parameter.child_by_field_name("type"), bytes))
        .collect();
    let first_name = var_parameters
        .first()
        .and_then(|parameter| parameter.child_by_field_name("name"));
    if let Some(name) = first_name {
        found.push(feature_use(
            JavaFeature::VarLambdaParameters,
            start_of(name),
        ));
    }
    found
}

/// A `switch` whose value is used: tree-sitter parses a switch statement as
/// a `switch_expression` too, standing where a statement stands.
fn switch_expression(switch: Node<'_>) -> Option<LanguageFeatureUse> {
    const STATEMENT_PLACES: &[&str] = &[
        "block",
        "constructor_body",
        "switch_block_statement_group",
        "switch_rule",
        "labeled_statement",
        "if_statement",
        "while_statement",
        "for_statement",
        "enhanced_for_statement",
        "do_statement",
        "expression_statement",
        "program",
    ];
    let is_statement = switch
        .parent()
        .is_some_and(|parent| STATEMENT_PLACES.contains(&parent.kind()));
    if is_statement {
        return None;
    }
    let keyword = switch
        .child(0)
        .filter(|keyword| keyword.kind() == "switch")?;
    Some(feature_use(
        JavaFeature::SwitchExpression,
        start_of(keyword),
    ))
}

/// `_`: an unnamed pattern inside a record pattern (javac reports it after
/// the `_`), else a variable named `_`.
fn underscore(node: Node<'_>) -> LanguageFeatureUse {
    let in_record_pattern = node
        .parent()
        .is_some_and(|parent| parent.kind() == "record_pattern_component");
    if in_record_pattern {
        feature_use(JavaFeature::UnnamedPattern, end_of(node))
    } else {
        feature_use(JavaFeature::UnderscoreVariable, start_of(node))
    }
}

/// `class var`: `var` is a restricted type name from Java 10 on.
fn type_named_var(declaration: Node<'_>, bytes: &[u8]) -> Option<LanguageFeatureUse> {
    let name = declaration.child_by_field_name("name")?;
    (text(name, bytes) == "var").then(|| feature_use(JavaFeature::VarAsTypeName, start_of(name)))
}

/// `import module java.base;`, which tree-sitter-java reads as an import of
/// `module` with an error after it: at the word `module`.
fn module_import(import: Node<'_>, bytes: &[u8]) -> Option<LanguageFeatureUse> {
    let module_word = module_import_word(import, bytes)?;
    Some(feature_use(
        JavaFeature::ModuleImport,
        start_of(module_word),
    ))
}

/// The `module` of `import module <name>;`.
pub(super) fn module_import_word<'tree>(import: Node<'tree>, bytes: &[u8]) -> Option<Node<'tree>> {
    if import.kind() != "import_declaration" {
        return None;
    }
    let after_import = text(import, bytes).strip_prefix("import")?;
    let is_module_import = after_import
        .trim_start()
        .strip_prefix("module")
        .is_some_and(|rest| rest.starts_with(char::is_whitespace));
    if !is_module_import {
        return None;
    }
    let first_word =
        std::iter::successors(import.named_child(0), |current| current.child(0)).last()?;
    (text(first_word, bytes) == "module").then_some(first_word)
}

/// A statement before `super(…)` or `this(…)` in a constructor: at the
/// call's `(`. tree-sitter-java only parses the call as the first
/// statement; after another, `super` is an error and `(…)` a statement.
fn statement_before_super(body: Node<'_>, bytes: &[u8]) -> Option<LanguageFeatureUse> {
    let statements: Vec<Node<'_>> = children(body)
        .into_iter()
        .filter(|part| part.is_named() || part.is_error())
        .collect();
    statements
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, statement)| {
            if statement.kind() == "explicit_constructor_invocation" {
                let arguments = statement.child_by_field_name("arguments")?;
                return Some(start_of(arguments));
            }
            let arguments =
                constructor_call_after_error(*statement, statements.get(index + 1), bytes)?;
            Some(start_of(arguments))
        })
        .map(|position| feature_use(JavaFeature::StatementBeforeSuper, position))
}

/// The `(…)` statement after an error node holding only `super` or `this`.
pub(super) fn constructor_call_after_error<'tree>(
    error: Node<'tree>,
    next: Option<&Node<'tree>>,
    bytes: &[u8],
) -> Option<Node<'tree>> {
    let keyword_only = error.is_error()
        && error.child_count() == 1
        && error
            .child(0)
            .is_some_and(|keyword| matches!(text(keyword, bytes), "super" | "this"));
    if !keyword_only {
        return None;
    }
    let call = next.filter(|next| next.kind() == "expression_statement")?;
    let arguments = call.named_child(0)?;
    (arguments.kind() == "parenthesized_expression").then_some(arguments)
}

/// A file of methods and fields with no class around them: at its first
/// such member.
fn compact_source_file(root: Node<'_>) -> Option<LanguageFeatureUse> {
    let first_member = children(root)
        .into_iter()
        .find(|part| matches!(part.kind(), "method_declaration" | "field_declaration"))?;
    Some(feature_use(
        JavaFeature::CompactSourceFile,
        start_of(first_member),
    ))
}
