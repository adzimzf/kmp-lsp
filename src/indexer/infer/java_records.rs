//! A Java record component and every name that is it, as a rename changes
//! them together (as jdtls does): the component in the record's header, the
//! accessor method that returns it (written out or not), the parameter of
//! the canonical constructor that sets it, the compact constructor's
//! implicit parameter, and the field reads inside the record.

use tree_sitter::Node;

use super::java_cst::java_local_at;

/// A component of a record: the record declaration and the component's
/// name in its header.
#[derive(Clone, Copy)]
pub(crate) struct RecordComponent<'tree> {
    pub(crate) record: Node<'tree>,
    pub(crate) declared_name: Node<'tree>,
}

impl<'tree> RecordComponent<'tree> {
    /// The component the name `node` is, or `None` when it isn't one:
    /// the component's own name in the header, its explicit accessor's name,
    /// the canonical constructor's parameter for it (or a use of that
    /// parameter), a compact constructor's use of it, or a read of the field
    /// inside the record.
    pub(crate) fn at(node: Node<'tree>, bytes: &[u8]) -> Option<Self> {
        if node.kind() != "identifier" {
            return None;
        }
        let name = node.utf8_text(bytes).ok()?;
        let record = enclosing_record(node)?;
        let component = Self::named(record, name, bytes)?;
        let is_one = component.declared_name.id() == node.id()
            || component
                .occurrences(bytes)
                .iter()
                .any(|occurrence| occurrence.id() == node.id());
        is_one.then_some(component)
    }

    /// The component of `record` called `name`.
    pub(crate) fn named(record: Node<'tree>, name: &str, bytes: &[u8]) -> Option<Self> {
        let declared_name = components(record)
            .into_iter()
            .filter_map(|parameter| parameter.child_by_field_name("name"))
            .find(|declared| declared.utf8_text(bytes).ok() == Some(name))?;
        Some(Self {
            record,
            declared_name,
        })
    }

    /// The component's name in the header first, then every other name in
    /// the record that is it, in source order.
    pub(crate) fn occurrences(&self, bytes: &[u8]) -> Vec<Node<'tree>> {
        let Ok(name) = self.declared_name.utf8_text(bytes) else {
            return Vec::new();
        };
        let mut found = vec![self.declared_name];
        let Some(body) = self.record.child_by_field_name("body") else {
            return found;
        };
        let mut cursor = body.walk();
        for member in body.named_children(&mut cursor) {
            match member.kind() {
                "method_declaration" => found.extend(self.in_method(member, name, bytes)),
                "constructor_declaration" => {
                    found.extend(self.in_constructor(member, name, bytes));
                }
                // Its parameters are implicit: a bare `name` is the component.
                "compact_constructor_declaration" => found.extend(field_uses(member, name, bytes)),
                _ => {}
            }
        }
        found.sort_by_key(|node| node.start_byte());
        found.dedup_by_key(|node| node.id());
        found
    }

    /// In a method: its name when it is the accessor, and the field reads.
    fn in_method(&self, method: Node<'tree>, name: &str, bytes: &[u8]) -> Vec<Node<'tree>> {
        let mut found = Vec::new();
        let is_the_accessor = method
            .child_by_field_name("name")
            .is_some_and(|method_name| method_name.utf8_text(bytes).ok() == Some(name))
            && method
                .child_by_field_name("parameters")
                .is_some_and(|parameters| parameters.named_child_count() == 0);
        if is_the_accessor {
            found.extend(method.child_by_field_name("name"));
        }
        found.extend(field_uses(method, name, bytes));
        found
    }

    /// In the canonical constructor: its parameter for the component, that
    /// parameter's uses, and `this.name`.
    fn in_constructor(
        &self,
        constructor: Node<'tree>,
        name: &str,
        bytes: &[u8],
    ) -> Vec<Node<'tree>> {
        let mut found = field_uses(constructor, name, bytes);
        let Some(parameter_name) = self.canonical_parameter(constructor, name, bytes) else {
            return found;
        };
        found.push(parameter_name);
        found.extend(
            identifiers_named(constructor, name, bytes)
                .into_iter()
                .filter(|use_site| {
                    java_local_at(*use_site, bytes)
                        .is_some_and(|local| local.declared_name.id() == parameter_name.id())
                }),
        );
        found
    }

    /// The name of `constructor`'s parameter for the component, when
    /// `constructor` is the canonical one: a parameter per component, the
    /// component's in the same place under the same name.
    fn canonical_parameter(
        &self,
        constructor: Node<'tree>,
        name: &str,
        bytes: &[u8],
    ) -> Option<Node<'tree>> {
        let parameters = constructor.child_by_field_name("parameters")?;
        let mut cursor = parameters.walk();
        let declared: Vec<Node<'tree>> = parameters
            .named_children(&mut cursor)
            .filter(|parameter| parameter.kind() == "formal_parameter")
            .collect();
        let components = components(self.record);
        if declared.len() != components.len() {
            return None;
        }
        let index = components.iter().position(|component| {
            component
                .child_by_field_name("name")
                .and_then(|declared| declared.utf8_text(bytes).ok())
                == Some(name)
        })?;
        declared[index]
            .child_by_field_name("name")
            .filter(|parameter_name| parameter_name.utf8_text(bytes).ok() == Some(name))
    }
}

/// The record declaration `node` is in, the innermost.
fn enclosing_record(node: Node<'_>) -> Option<Node<'_>> {
    std::iter::successors(node.parent(), |current| current.parent())
        .find(|current| current.kind() == "record_declaration")
}

/// The component parameters of `record`'s header.
fn components(record: Node<'_>) -> Vec<Node<'_>> {
    let Some(parameters) = record.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut cursor = parameters.walk();
    parameters
        .named_children(&mut cursor)
        .filter(|parameter| parameter.kind() == "formal_parameter")
        .collect()
}

/// The reads and writes of the component `name` in `member`: `this.name`,
/// a bare `name` no local binds (the field, or a compact constructor's
/// implicit parameter), and `name()` / `this.name()` accessor calls.
fn field_uses<'tree>(member: Node<'tree>, name: &str, bytes: &[u8]) -> Vec<Node<'tree>> {
    let mut found = Vec::new();
    for identifier in identifiers_named(member, name, bytes) {
        let Some(parent) = identifier.parent() else {
            continue;
        };
        let names_this_field = parent.kind() == "field_access"
            && parent
                .child_by_field_name("field")
                .is_some_and(|field| field.id() == identifier.id())
            && parent
                .child_by_field_name("object")
                .is_some_and(|object| object.kind() == "this");
        let calls_the_accessor = parent.kind() == "method_invocation"
            && parent
                .child_by_field_name("name")
                .is_some_and(|called| called.id() == identifier.id())
            && parent
                .child_by_field_name("object")
                .is_none_or(|object| object.kind() == "this")
            && parent
                .child_by_field_name("arguments")
                .is_some_and(|arguments| arguments.named_child_count() == 0);
        let is_a_bare_read = matches!(
            parent.kind(),
            "binary_expression"
                | "return_statement"
                | "assignment_expression"
                | "parenthesized_expression"
                | "expression_statement"
                | "argument_list"
                | "unary_expression"
                | "ternary_expression"
                | "variable_declarator"
                | "array_access"
                | "cast_expression"
        ) || (parent.kind() == "field_access"
            && parent
                .child_by_field_name("object")
                .is_some_and(|object| object.id() == identifier.id()))
            || (parent.kind() == "method_invocation"
                && parent
                    .child_by_field_name("object")
                    .is_some_and(|object| object.id() == identifier.id()));
        let unbound = java_local_at(identifier, bytes).is_none();
        let reads_the_field = is_a_bare_read && unbound;
        if names_this_field || calls_the_accessor || reads_the_field {
            found.push(identifier);
        }
    }
    found
}

/// Every identifier under `node` spelled `name`.
fn identifiers_named<'tree>(node: Node<'tree>, name: &str, bytes: &[u8]) -> Vec<Node<'tree>> {
    let mut found = Vec::new();
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        if current.kind() == "identifier" && current.utf8_text(bytes).ok() == Some(name) {
            found.push(current);
        }
        let mut cursor = current.walk();
        pending.extend(current.named_children(&mut cursor));
    }
    found
}
