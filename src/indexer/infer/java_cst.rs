//! Java counterpart of the Kotlin CST classification and expression typing
//! (`cst_symbol.rs`, `expr_type.rs`).
//!
//! The Kotlin walkers match Kotlin node kinds (`simple_identifier`,
//! `navigation_expression`, `call_expression`), so a Java file used to
//! classify nothing: every Java occurrence stayed an unverified name match,
//! and every Java receiver was typed by a whole-file, by-name text scan.
//! This module answers the same three questions from the Java tree:
//!
//! - what an identifier is ([`classify_java_identifier`]): a declaration, an
//!   import segment, a member reference with its receiver's type, or a bare
//!   reference;
//! - which declaration binds a name at a use site ([`java_local_binding_at`]):
//!   a local variable, a parameter, a `for`/`catch`/resource variable;
//! - the type of an expression ([`java_expr_type`]): identifiers, `this`,
//!   `new T(..)`, casts, string literals, and member chains (`a.b().c`), with
//!   the receiver's type arguments substituted into the member's type.

use tower_lsp::lsp_types::Url;
use tree_sitter::Node;

use crate::indexer::NodeExt;
use crate::resolver::jvm_interop::PropertyAccess;
use crate::types::Language;
use crate::StrExt as _;

use super::chain::{resolve_field_type_on, resolve_method_return_type_on};
use super::cst_symbol::{SymbolAtCursor, SymbolRole};
use super::deps::{CallShape, InferDeps};

// ─── node kinds (tree-sitter-java) ───────────────────────────────────────────

const IDENTIFIER: &str = "identifier";
const TYPE_IDENTIFIER: &str = "type_identifier";
const METHOD_INVOCATION: &str = "method_invocation";
const FIELD_ACCESS: &str = "field_access";
const OBJECT_CREATION: &str = "object_creation_expression";
const CAST_EXPRESSION: &str = "cast_expression";
const PARENTHESIZED_EXPRESSION: &str = "parenthesized_expression";
const ASSIGNMENT_EXPRESSION: &str = "assignment_expression";
const STRING_LITERAL: &str = "string_literal";
const THIS: &str = "this";
const LOCAL_VARIABLE_DECLARATION: &str = "local_variable_declaration";
const FIELD_DECLARATION: &str = "field_declaration";
const VARIABLE_DECLARATOR: &str = "variable_declarator";
const FORMAL_PARAMETERS: &str = "formal_parameters";
const FORMAL_PARAMETER: &str = "formal_parameter";
const SPREAD_PARAMETER: &str = "spread_parameter";
const INFERRED_PARAMETERS: &str = "inferred_parameters";
const CATCH_CLAUSE: &str = "catch_clause";
const CATCH_FORMAL_PARAMETER: &str = "catch_formal_parameter";
const ENHANCED_FOR: &str = "enhanced_for_statement";
const FOR_STATEMENT: &str = "for_statement";
const RESOURCE: &str = "resource";
const RESOURCE_SPECIFICATION: &str = "resource_specification";
const BLOCK: &str = "block";
const CONSTRUCTOR_BODY: &str = "constructor_body";
const SWITCH_BLOCK_GROUP: &str = "switch_block_statement_group";
const METHOD_DECLARATION: &str = "method_declaration";
const CONSTRUCTOR_DECLARATION: &str = "constructor_declaration";
const COMPACT_CONSTRUCTOR: &str = "compact_constructor_declaration";
const LAMBDA_EXPRESSION: &str = "lambda_expression";
const CLASS_BODY: &str = "class_body";
const INTERFACE_BODY: &str = "interface_body";
const ENUM_BODY: &str = "enum_body";
const ENUM_BODY_DECLARATIONS: &str = "enum_body_declarations";
const ANNOTATION_TYPE_BODY: &str = "annotation_type_body";
const PROGRAM: &str = "program";
const IMPORT_DECLARATION: &str = "import_declaration";
const PACKAGE_DECLARATION: &str = "package_declaration";
const ENUM_CONSTANT: &str = "enum_constant";
const TYPE_PARAMETER: &str = "type_parameter";
const CLASS_DECLARATION: &str = "class_declaration";
const INTERFACE_DECLARATION: &str = "interface_declaration";
const ENUM_DECLARATION: &str = "enum_declaration";
const RECORD_DECLARATION: &str = "record_declaration";
const ANNOTATION_TYPE_DECLARATION: &str = "annotation_type_declaration";
const LINE_COMMENT: &str = "line_comment";
const BLOCK_COMMENT: &str = "block_comment";

/// Java's `var` local: no written type, the initializer decides.
const INFERRED_LOCAL_TYPE: &str = crate::queries::JAVA_INFERRED_LOCAL_TYPE;

/// Deepest member chain / parenthesization typed before giving up.
const MAX_EXPRESSION_DEPTH: usize = 32;

/// Whether `uri` is a Java source file — the files this module classifies.
pub(crate) fn is_java_file(uri: &Url) -> bool {
    Language::from_path(uri.path()) == Language::Java
}

/// Whether `node` is a Java identifier this module classifies.
pub(crate) fn is_java_identifier(node: Node<'_>) -> bool {
    matches!(node.kind(), IDENTIFIER | TYPE_IDENTIFIER)
}

fn is_class_like_declaration(kind: &str) -> bool {
    matches!(
        kind,
        CLASS_DECLARATION
            | INTERFACE_DECLARATION
            | ENUM_DECLARATION
            | RECORD_DECLARATION
            | ANNOTATION_TYPE_DECLARATION
    )
}

fn is_class_like_body(kind: &str) -> bool {
    matches!(
        kind,
        CLASS_BODY | INTERFACE_BODY | ENUM_BODY | ENUM_BODY_DECLARATIONS | ANNOTATION_TYPE_BODY
    )
}

fn is_field_named(node: Node<'_>, field: &str, child: Node<'_>) -> bool {
    node.child_by_field_name(field)
        .is_some_and(|named| named.id() == child.id())
}

// ─── classification ──────────────────────────────────────────────────────────

/// What a Java declaration-site identifier declares.
enum JavaDeclaration {
    /// A type, method, field or enum constant — what the Java indexer
    /// records in `f.symbols`.
    Indexed,
    /// A local variable, parameter or type parameter — never indexed.
    Local,
}

fn declaration_at(node: Node<'_>, parent: Node<'_>) -> Option<JavaDeclaration> {
    let parent_kind = parent.kind();
    let is_name = is_field_named(parent, "name", node);
    if is_class_like_declaration(parent_kind)
        || matches!(
            parent_kind,
            METHOD_DECLARATION | CONSTRUCTOR_DECLARATION | COMPACT_CONSTRUCTOR | ENUM_CONSTANT
        )
    {
        return is_name.then_some(JavaDeclaration::Indexed);
    }
    if parent_kind == VARIABLE_DECLARATOR {
        if !is_name {
            return None;
        }
        let declares_field = parent
            .parent()
            .is_some_and(|declaration| declaration.kind() == FIELD_DECLARATION);
        return Some(if declares_field {
            JavaDeclaration::Indexed
        } else {
            JavaDeclaration::Local
        });
    }
    let declares_local = matches!(
        parent_kind,
        FORMAL_PARAMETER | CATCH_FORMAL_PARAMETER | ENHANCED_FOR | RESOURCE
    ) && is_name;
    let is_lambda_parameter = parent_kind == INFERRED_PARAMETERS
        || (parent_kind == LAMBDA_EXPRESSION && is_field_named(parent, "parameters", node));
    let is_type_parameter = parent_kind == TYPE_PARAMETER;
    let is_spread_parameter = parent_kind == SPREAD_PARAMETER && node.kind() == IDENTIFIER;
    (declares_local || is_lambda_parameter || is_type_parameter || is_spread_parameter)
        .then_some(JavaDeclaration::Local)
}

fn is_import_segment(node: Node<'_>) -> bool {
    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        match current.kind() {
            IMPORT_DECLARATION | PACKAGE_DECLARATION => return true,
            "scoped_identifier" => ancestor = current.parent(),
            _ => return false,
        }
    }
    false
}

/// The member access `node` names: the enclosing `obj.member(..)` /
/// `obj.member` node and its receiver, when `node` is its member name.
struct MemberAccess<'tree> {
    receiver: Node<'tree>,
    invocation: Option<Node<'tree>>,
    access: PropertyAccess,
}

fn member_access_of(node: Node<'_>) -> Option<MemberAccess<'_>> {
    let parent = node.parent()?;
    match parent.kind() {
        METHOD_INVOCATION if is_field_named(parent, "name", node) => Some(MemberAccess {
            receiver: parent.child_by_field_name("object")?,
            invocation: Some(parent),
            access: PropertyAccess::Read,
        }),
        FIELD_ACCESS if is_field_named(parent, "field", node) => {
            let is_assignment_target = parent.parent().is_some_and(|grandparent| {
                grandparent.kind() == ASSIGNMENT_EXPRESSION
                    && is_field_named(grandparent, "left", parent)
            });
            Some(MemberAccess {
                receiver: parent.child_by_field_name("object")?,
                invocation: None,
                access: if is_assignment_target {
                    PropertyAccess::Write
                } else {
                    PropertyAccess::Read
                },
            })
        }
        _ => None,
    }
}

/// The argument count of a Java `method_invocation` / `new T(..)`.
fn java_call_shape(invocation: Node<'_>) -> CallShape {
    let arg_count = invocation
        .child_by_field_name("arguments")
        .map(|arguments| {
            let mut cursor = arguments.walk();
            arguments
                .named_children(&mut cursor)
                .filter(|argument| !matches!(argument.kind(), LINE_COMMENT | BLOCK_COMMENT))
                .count()
        })
        .unwrap_or(0);
    CallShape {
        arg_count,
        trailing_lambda: false,
    }
}

/// Classify a Java identifier: the Java arm of `classify_symbol_at`.
///
/// `known_type` gates a receiver's inferred type the way the Kotlin arm does
/// (`names_a_known_type`): a type the index doesn't know is no receiver
/// type, so verification keeps the candidate instead of rejecting it.
pub(crate) fn classify_java_identifier(
    node: Node<'_>,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
    known_type: impl Fn(&str, &Url) -> bool,
) -> Option<SymbolAtCursor> {
    if !is_java_identifier(node) {
        return None;
    }
    let name = node.utf8_text_owned(bytes)?;
    if let Some(declaration) = node
        .parent()
        .and_then(|parent| declaration_at(node, parent))
    {
        return Some(SymbolAtCursor {
            name,
            role: SymbolRole::Declaration {
                indexed: matches!(declaration, JavaDeclaration::Indexed),
            },
        });
    }
    if is_import_segment(node) {
        return Some(SymbolAtCursor {
            name,
            role: SymbolRole::ImportSegment,
        });
    }
    if let Some(member) = member_access_of(node) {
        let receiver_type = java_expr_type(member.receiver, bytes, deps, uri)
            .filter(|(type_name, origin)| {
                let resolution_scope = if origin.scheme() == "file" {
                    origin
                } else {
                    uri
                };
                known_type(&type_name.dotted_ident_prefix(), resolution_scope)
            })
            .map(|(type_name, origin)| {
                crate::indexer::ResolvedType::from_inferred(type_name, origin)
            });
        return Some(SymbolAtCursor {
            name,
            role: SymbolRole::Reference {
                receiver_type,
                is_call: member.invocation.is_some(),
                shape: member.invocation.map(java_call_shape),
                access: member.access,
            },
        });
    }
    let bare_invocation = node.parent().filter(|parent| {
        parent.kind() == METHOD_INVOCATION && is_field_named(*parent, "name", node)
    });
    Some(SymbolAtCursor {
        name,
        role: SymbolRole::Reference {
            receiver_type: None,
            is_call: bare_invocation.is_some(),
            shape: bare_invocation.map(java_call_shape),
            access: PropertyAccess::Read,
        },
    })
}

/// Whether the Java declaration whose name is `node` is local: declared in a
/// method, constructor, initializer or lambda body, not a member.
pub(crate) fn is_java_local_declaration(node: Node<'_>) -> bool {
    let mut ancestor = node.parent();
    while let Some(scope) = ancestor {
        let kind = scope.kind();
        if matches!(kind, BLOCK | CONSTRUCTOR_BODY | LAMBDA_EXPRESSION) {
            return true;
        }
        if is_class_like_body(kind) || kind == PROGRAM {
            return false;
        }
        ancestor = scope.parent();
    }
    false
}

/// Whether a bare Java identifier reads a local value (a local variable or
/// a parameter) rather than a member.
pub(crate) fn is_java_local_value_reference(node: Node<'_>, bytes: &[u8]) -> bool {
    node.kind() == IDENTIFIER
        && member_access_of(node).is_none()
        && !is_bare_method_name(node)
        && !matches!(
            java_local_binding_at(node, bytes),
            JavaLocalBinding::NotLocal
        )
}

fn is_bare_method_name(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == METHOD_INVOCATION && is_field_named(parent, "name", node)
    })
}

/// The names of the classes enclosing `node`, innermost first — the
/// implicit receivers a bare Java member name is looked up on.
pub(crate) fn enclosing_java_class_names(node: Node<'_>, bytes: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        if is_class_like_declaration(current.kind()) {
            if let Some(name) = current
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text_owned(bytes))
            {
                names.push(name);
            }
        }
        ancestor = current.parent();
    }
    names
}

// ─── local bindings ──────────────────────────────────────────────────────────

/// Which declaration binds a Java name at a use site.
pub(crate) enum JavaLocalBinding<'tree> {
    /// A local or parameter with a written type (as written: `List<User>`).
    Typed(String),
    /// A `var` local: its type is its initializer's.
    Inferred(Node<'tree>),
    /// A local whose type isn't written (an inferred lambda parameter).
    Untyped,
    /// No local or parameter binds the name here: a member, or unknown.
    NotLocal,
}

/// The declaration binding the identifier `use_site` in its scope, nearest
/// first: earlier locals of each enclosing block, then `for`/`catch`/
/// resource variables, then the enclosing method's or lambda's parameters.
/// A class body ends the walk — past it the name is a member.
pub(crate) fn java_local_binding_at<'tree>(
    use_site: Node<'tree>,
    bytes: &[u8],
) -> JavaLocalBinding<'tree> {
    let Some(name) = use_site.utf8_text_owned(bytes) else {
        return JavaLocalBinding::NotLocal;
    };
    let mut inner = use_site;
    while let Some(scope) = inner.parent() {
        let kind = scope.kind();
        if is_class_like_body(kind) || kind == PROGRAM {
            return JavaLocalBinding::NotLocal;
        }
        if let Some(binding) = binding_in_scope(scope, inner, use_site.start_byte(), &name, bytes) {
            return binding;
        }
        inner = scope;
    }
    JavaLocalBinding::NotLocal
}

fn binding_in_scope<'tree>(
    scope: Node<'tree>,
    inner: Node<'tree>,
    use_start: usize,
    name: &str,
    bytes: &[u8],
) -> Option<JavaLocalBinding<'tree>> {
    match scope.kind() {
        BLOCK | CONSTRUCTOR_BODY | SWITCH_BLOCK_GROUP => {
            earlier_local_declaration(scope, use_start, name, bytes)
        }
        FOR_STATEMENT => {
            let mut cursor = scope.walk();
            let initializers: Vec<Node<'tree>> = scope
                .children_by_field_name("init", &mut cursor)
                .filter(|init| init.kind() == LOCAL_VARIABLE_DECLARATION)
                .collect();
            initializers
                .into_iter()
                .find_map(|declaration| declared_local(declaration, use_start, name, bytes))
        }
        ENHANCED_FOR => {
            let declares_it = scope
                .child_by_field_name("name")
                .and_then(|declared| declared.utf8_text(bytes).ok())
                == Some(name);
            let in_body = scope
                .child_by_field_name("body")
                .is_some_and(|body| body.id() == inner.id());
            (declares_it && in_body).then(|| typed_binding_of(scope, bytes))
        }
        CATCH_CLAUSE => {
            let parameter = scope.first_child_of_kind(CATCH_FORMAL_PARAMETER)?;
            let declares_it = parameter
                .child_by_field_name("name")
                .and_then(|declared| declared.utf8_text(bytes).ok())
                == Some(name);
            declares_it.then(|| {
                parameter
                    .first_child_of_kind("catch_type")
                    .and_then(|catch_type| catch_type.utf8_text_owned(bytes))
                    .map_or(JavaLocalBinding::Untyped, |catch_type| {
                        // A multi-catch `A | B` has no single type.
                        if catch_type.contains('|') {
                            JavaLocalBinding::Untyped
                        } else {
                            JavaLocalBinding::Typed(catch_type)
                        }
                    })
            })
        }
        RESOURCE_SPECIFICATION | "try_with_resources_statement" => {
            let specification = if scope.kind() == RESOURCE_SPECIFICATION {
                scope
            } else {
                scope.child_by_field_name("resources")?
            };
            specification
                .children_of_kind(RESOURCE)
                .into_iter()
                .find(|resource| {
                    resource
                        .child_by_field_name("name")
                        .and_then(|declared| declared.utf8_text(bytes).ok())
                        == Some(name)
                })
                .map(|resource| typed_binding_of(resource, bytes))
        }
        METHOD_DECLARATION | CONSTRUCTOR_DECLARATION => {
            parameter_binding(scope.child_by_field_name("parameters")?, name, bytes)
        }
        LAMBDA_EXPRESSION => lambda_parameter_binding(scope, name, bytes),
        _ => None,
    }
}

/// The nearest local `name` declared in `block` before the use site at
/// `use_start`. A declarator is in scope only once it has ended: `Foo foo =
/// foo.bar()` doesn't see itself, `int a = 1, b = a` sees `a`.
fn earlier_local_declaration<'tree>(
    block: Node<'tree>,
    use_start: usize,
    name: &str,
    bytes: &[u8],
) -> Option<JavaLocalBinding<'tree>> {
    let mut nearest = None;
    let mut cursor = block.walk();
    for statement in block.named_children(&mut cursor) {
        if statement.start_byte() >= use_start {
            break;
        }
        if statement.kind() == LOCAL_VARIABLE_DECLARATION {
            if let Some(binding) = declared_local(statement, use_start, name, bytes) {
                nearest = Some(binding);
            }
        }
    }
    nearest
}

/// The declarator of the local variable `declaration` that declares `name`
/// and ends before `use_start`.
fn declared_local<'tree>(
    declaration: Node<'tree>,
    use_start: usize,
    name: &str,
    bytes: &[u8],
) -> Option<JavaLocalBinding<'tree>> {
    let type_text = declaration
        .child_by_field_name("type")
        .and_then(|type_node| type_node.utf8_text_owned(bytes))?;
    declaration
        .children_of_kind(VARIABLE_DECLARATOR)
        .into_iter()
        .filter(|declarator| declarator.end_byte() <= use_start)
        .find(|declarator| {
            declarator
                .child_by_field_name("name")
                .and_then(|declared| declared.utf8_text(bytes).ok())
                == Some(name)
        })
        .map(|declarator| binding_of_written_type(&type_text, declarator))
}

fn binding_of_written_type<'tree>(
    type_text: &str,
    declarator: Node<'tree>,
) -> JavaLocalBinding<'tree> {
    if type_text == INFERRED_LOCAL_TYPE {
        return declarator
            .child_by_field_name("value")
            .map_or(JavaLocalBinding::Untyped, JavaLocalBinding::Inferred);
    }
    JavaLocalBinding::Typed(type_text.to_owned())
}

/// The binding of a node that carries its own `type` and `name` fields (an
/// enhanced-`for` variable, a resource, a formal parameter).
fn typed_binding_of<'tree>(declaration: Node<'tree>, bytes: &[u8]) -> JavaLocalBinding<'tree> {
    let Some(type_text) = declaration
        .child_by_field_name("type")
        .and_then(|type_node| type_node.utf8_text_owned(bytes))
    else {
        return JavaLocalBinding::Untyped;
    };
    if type_text == INFERRED_LOCAL_TYPE {
        return declaration.child_by_field_name("value").map_or(
            JavaLocalBinding::Untyped,
            |value| {
                // An enhanced-`for`'s `var` is the iterated element, not the
                // iterable: leave it untyped rather than guess.
                if declaration.kind() == ENHANCED_FOR {
                    JavaLocalBinding::Untyped
                } else {
                    JavaLocalBinding::Inferred(value)
                }
            },
        );
    }
    JavaLocalBinding::Typed(type_text)
}

fn parameter_binding<'tree>(
    parameters: Node<'tree>,
    name: &str,
    bytes: &[u8],
) -> Option<JavaLocalBinding<'tree>> {
    let mut cursor = parameters.walk();
    let found = parameters
        .named_children(&mut cursor)
        .find_map(|parameter| {
            match parameter.kind() {
                FORMAL_PARAMETER => {
                    let declares_it = parameter
                        .child_by_field_name("name")
                        .and_then(|declared| declared.utf8_text(bytes).ok())
                        == Some(name);
                    declares_it.then(|| typed_binding_of(parameter, bytes))
                }
                SPREAD_PARAMETER => {
                    let declares_it = parameter
                        .first_child_of_kind(VARIABLE_DECLARATOR)
                        .and_then(|declarator| declarator.child_by_field_name("name"))
                        .and_then(|declared| declared.utf8_text(bytes).ok())
                        == Some(name);
                    // `T... values` is a `T[]` inside the method.
                    declares_it.then(|| {
                        parameter
                            .named_child(0)
                            .filter(|element| element.kind() != VARIABLE_DECLARATOR)
                            .and_then(|element| element.utf8_text_owned(bytes))
                            .map_or(JavaLocalBinding::Untyped, |element| {
                                JavaLocalBinding::Typed(format!("{element}[]"))
                            })
                    })
                }
                _ => None,
            }
        });
    found
}

fn lambda_parameter_binding<'tree>(
    lambda: Node<'tree>,
    name: &str,
    bytes: &[u8],
) -> Option<JavaLocalBinding<'tree>> {
    let parameters = lambda.child_by_field_name("parameters")?;
    match parameters.kind() {
        IDENTIFIER => {
            (parameters.utf8_text(bytes).ok() == Some(name)).then_some(JavaLocalBinding::Untyped)
        }
        INFERRED_PARAMETERS => {
            let mut cursor = parameters.walk();
            let declares_it = parameters
                .named_children(&mut cursor)
                .any(|parameter| parameter.utf8_text(bytes).ok() == Some(name));
            declares_it.then_some(JavaLocalBinding::Untyped)
        }
        FORMAL_PARAMETERS => parameter_binding(parameters, name, bytes),
        _ => None,
    }
}

// ─── member receivers ────────────────────────────────────────────────────────

/// The receiver of the member access whose member name is `member`: `foo`
/// in `foo.member`, `foo.member(..)`, and in the `foo.member` a missing `;`
/// turns into a declaration's type (`foo.member\n bar = 1;`). `None` when
/// `member` isn't the member half of a qualified access.
pub(crate) fn java_member_receiver(member: Node<'_>) -> Option<Node<'_>> {
    let parent = member.parent()?;
    let (member_field, receiver_field) = match parent.kind() {
        FIELD_ACCESS => ("field", "object"),
        METHOD_INVOCATION => ("name", "object"),
        "scoped_type_identifier" | "scoped_identifier" => {
            let is_last = parent
                .named_child(u32::try_from(parent.named_child_count().checked_sub(1)?).ok()?)
                .is_some_and(|last| last.id() == member.id());
            return is_last.then(|| parent.named_child(0)).flatten();
        }
        _ => return None,
    };
    if !is_field_named(parent, member_field, member) {
        return None;
    }
    parent.child_by_field_name(receiver_field)
}

// ─── expression types ────────────────────────────────────────────────────────

/// The type of the Java expression `node`, as written (type arguments
/// kept), with the file that spelled it — the anchor its name resolves in.
pub(crate) fn java_expr_type(
    node: Node<'_>,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<(String, Url)> {
    expr_type_at_depth(node, bytes, deps, uri, 0)
}

fn expr_type_at_depth(
    node: Node<'_>,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
    depth: usize,
) -> Option<(String, Url)> {
    if depth > MAX_EXPRESSION_DEPTH {
        return None;
    }
    let here = |type_text: String| Some((type_text, uri.clone()));
    match node.kind() {
        STRING_LITERAL | "text_block" => here("String".to_owned()),
        THIS => here(enclosing_java_class_names(node, bytes).into_iter().next()?),
        // A `type_identifier` here is a receiver the parser misread as a
        // type: `foo.bar` with no `;` before the next line's declaration
        // parses as the type `foo.bar` of that declaration.
        IDENTIFIER | TYPE_IDENTIFIER => identifier_type(node, bytes, deps, uri, depth),
        OBJECT_CREATION => {
            let created = node.child_by_field_name("type")?.utf8_text_owned(bytes)?;
            here(without_diamond(&created))
        }
        CAST_EXPRESSION => here(node.child_by_field_name("type")?.utf8_text_owned(bytes)?),
        PARENTHESIZED_EXPRESSION => {
            expr_type_at_depth(node.named_child(0)?, bytes, deps, uri, depth + 1)
        }
        METHOD_INVOCATION => method_invocation_type(node, bytes, deps, uri, depth),
        FIELD_ACCESS => {
            let receiver = node.child_by_field_name("object")?;
            let field = node.child_by_field_name("field")?.utf8_text_owned(bytes)?;
            let (receiver_type, receiver_uri) =
                expr_type_at_depth(receiver, bytes, deps, uri, depth + 1)?;
            let lookup_name = receiver_type.dotted_ident_prefix();
            resolve_field_type_on(&lookup_name, &receiver_type, &field, deps, &receiver_uri)
        }
        _ => None,
    }
}

/// `ArrayList<>` → `ArrayList`: a diamond carries no type arguments.
fn without_diamond(type_text: &str) -> String {
    type_text.strip_suffix("<>").unwrap_or(type_text).to_owned()
}

fn identifier_type(
    node: Node<'_>,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
    depth: usize,
) -> Option<(String, Url)> {
    match java_local_binding_at(node, bytes) {
        JavaLocalBinding::Typed(type_text) => Some((type_text, uri.clone())),
        JavaLocalBinding::Inferred(initializer) => {
            expr_type_at_depth(initializer, bytes, deps, uri, depth + 1)
        }
        JavaLocalBinding::Untyped => None,
        JavaLocalBinding::NotLocal => member_or_type_named(node, bytes, deps, uri),
    }
}

/// A name no local binds: a field of an enclosing class (declared in this
/// file, else inherited), or — capitalized — a type (`Repo.create()`).
fn member_or_type_named(
    node: Node<'_>,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<(String, Url)> {
    let name = node.utf8_text_owned(bytes)?;
    if let Some(type_text) = field_declared_in_enclosing_classes(node, &name, bytes) {
        return Some((type_text, uri.clone()));
    }
    for class_name in enclosing_java_class_names(node, bytes) {
        if let Some(found) = deps.find_field_type(&class_name, &name, uri) {
            return Some(found);
        }
    }
    let names_a_type = name.starts_with_uppercase() && deps.has_type_definition(&name);
    names_a_type.then(|| (name, uri.clone()))
}

fn field_declared_in_enclosing_classes(node: Node<'_>, name: &str, bytes: &[u8]) -> Option<String> {
    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        if is_class_like_body(current.kind()) {
            let declared = current
                .children_of_kind(FIELD_DECLARATION)
                .into_iter()
                .find_map(|field| {
                    let declares_it =
                        field
                            .children_of_kind(VARIABLE_DECLARATOR)
                            .into_iter()
                            .any(|declarator| {
                                declarator
                                    .child_by_field_name("name")
                                    .and_then(|declared| declared.utf8_text(bytes).ok())
                                    == Some(name)
                            });
                    declares_it
                        .then(|| field.child_by_field_name("type"))
                        .flatten()
                        .and_then(|type_node| type_node.utf8_text_owned(bytes))
                });
            if declared.is_some() {
                return declared;
            }
        }
        ancestor = current.parent();
    }
    None
}

fn method_invocation_type(
    node: Node<'_>,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
    depth: usize,
) -> Option<(String, Url)> {
    let method = node.child_by_field_name("name")?.utf8_text_owned(bytes)?;
    let Some(receiver) = node.child_by_field_name("object") else {
        return bare_method_return_type(node, &method, bytes, deps, uri);
    };
    let (receiver_type, receiver_uri) = expr_type_at_depth(receiver, bytes, deps, uri, depth + 1)?;
    let lookup_name = receiver_type.dotted_ident_prefix();
    resolve_method_return_type_on(&lookup_name, &receiver_type, &method, deps, &receiver_uri)
}

/// `foo()` with no receiver: a method of an enclosing class — declared in
/// this file, else inherited.
fn bare_method_return_type(
    node: Node<'_>,
    method: &str,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<(String, Url)> {
    if let Some(type_text) = method_declared_in_enclosing_classes(node, method, bytes) {
        return Some((type_text, uri.clone()));
    }
    enclosing_java_class_names(node, bytes)
        .into_iter()
        .find_map(|class_name| {
            deps.find_method_return_type_with_origin_for_type(&class_name, method, uri)
        })
}

fn method_declared_in_enclosing_classes(
    node: Node<'_>,
    method: &str,
    bytes: &[u8],
) -> Option<String> {
    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        if is_class_like_body(current.kind()) {
            let declared = current
                .children_of_kind(METHOD_DECLARATION)
                .into_iter()
                .find(|declaration| {
                    declaration
                        .child_by_field_name("name")
                        .and_then(|declared| declared.utf8_text(bytes).ok())
                        == Some(method)
                })
                .and_then(|declaration| declaration.child_by_field_name("type"))
                .and_then(|type_node| type_node.utf8_text_owned(bytes))
                .filter(|type_text| type_text != "void");
            if declared.is_some() {
                return declared;
            }
        }
        ancestor = current.parent();
    }
    None
}

#[cfg(test)]
#[path = "java_cst_tests.rs"]
mod tests;
