use tower_lsp::lsp_types::Url;
use tree_sitter::Node;

use super::*;
use crate::indexer::infer::deps::TestDeps;
use crate::indexer::live_tree::{parse_live, LiveDoc};

fn java_doc(source: &str) -> LiveDoc {
    let Some(doc) = parse_live(source, tree_sitter_java::LANGUAGE.into()) else {
        panic!("parse_live failed");
    };
    doc
}

fn uri() -> Url {
    Url::parse("file:///app/Caller.java").unwrap()
}

/// The named leaf that starts at the `occurrence`-th (0-based) match of
/// `marker` in the source.
fn identifier_at<'tree>(doc: &'tree LiveDoc, marker: &str, occurrence: usize) -> Node<'tree> {
    let source = std::str::from_utf8(&doc.bytes).unwrap();
    let Some((start, _)) = source.match_indices(marker).nth(occurrence) else {
        panic!("marker {marker:?} #{occurrence} not in source");
    };
    let Some(node) = doc
        .tree
        .root_node()
        .named_descendant_for_byte_range(start, start + marker.len())
    else {
        panic!("no node at {marker:?}");
    };
    node
}

/// The receiver type the member identifier `marker` is classified with.
fn receiver_type_of(
    source: &str,
    deps: &TestDeps,
    marker: &str,
    occurrence: usize,
) -> Option<String> {
    let doc = java_doc(source);
    let node = identifier_at(&doc, marker, occurrence);
    let symbol = classify_java_identifier(node, &doc.bytes, deps, &uri(), |_, _| true)?;
    match symbol.role {
        SymbolRole::Reference { receiver_type, .. } => {
            receiver_type.map(|resolved| resolved.type_path())
        }
        other => panic!("{marker:?} is not a reference: {other:?}"),
    }
}

fn role_of(source: &str, marker: &str, occurrence: usize) -> SymbolRole {
    let doc = java_doc(source);
    let node = identifier_at(&doc, marker, occurrence);
    let Some(symbol) =
        classify_java_identifier(node, &doc.bytes, &TestDeps::new(), &uri(), |_, _| true)
    else {
        panic!("{marker:?} did not classify");
    };
    symbol.role
}

#[test]
fn typed_local_types_its_member_call() {
    let source =
        "class Caller {\n  void run() {\n    Repo repo = make();\n    repo.save(\"a\");\n  }\n}\n";
    assert_eq!(
        receiver_type_of(source, &TestDeps::new(), "save", 0).as_deref(),
        Some("Repo")
    );
}

#[test]
fn var_local_takes_its_initializers_type() {
    let source = "class Caller {\n  void run() {\n    var repo = new Repo();\n    repo.save(\"a\");\n  }\n}\n";
    assert_eq!(
        receiver_type_of(source, &TestDeps::new(), "save", 0).as_deref(),
        Some("Repo")
    );
}

#[test]
fn diamond_creation_drops_the_empty_type_arguments() {
    let source = "class Caller {\n  void run() {\n    var items = new ArrayList<>();\n    items.add(1);\n  }\n}\n";
    assert_eq!(
        receiver_type_of(source, &TestDeps::new(), "add", 0).as_deref(),
        Some("ArrayList")
    );
}

#[test]
fn parameter_types_its_member_call() {
    let source = "class Caller {\n  void run(Other other) {\n    other.save(\"b\");\n  }\n}\n";
    assert_eq!(
        receiver_type_of(source, &TestDeps::new(), "save", 0).as_deref(),
        Some("Other")
    );
}

#[test]
fn field_of_the_enclosing_class_types_its_member_call() {
    let source =
        "class Caller {\n  private Repo repo;\n  void run() {\n    repo.save(\"c\");\n  }\n}\n";
    assert_eq!(
        receiver_type_of(source, &TestDeps::new(), "save", 0).as_deref(),
        Some("Repo")
    );
}

#[test]
fn local_shadows_a_same_named_field() {
    let source = "class Caller {\n  private Repo repo;\n  void run() {\n    Other repo = make();\n    repo.save(\"c\");\n  }\n  void later() {\n    repo.save(\"d\");\n  }\n}\n";
    let deps = TestDeps::new();
    assert_eq!(
        receiver_type_of(source, &deps, "save", 0).as_deref(),
        Some("Other")
    );
    // The local is out of scope in another method: the field again.
    assert_eq!(
        receiver_type_of(source, &deps, "save", 1).as_deref(),
        Some("Repo")
    );
}

#[test]
fn a_declaration_does_not_see_itself_in_its_initializer() {
    let source = "class Caller {\n  private Repo repo;\n  void run() {\n    Other repo = repo.convert();\n  }\n}\n";
    assert_eq!(
        receiver_type_of(source, &TestDeps::new(), "convert", 0).as_deref(),
        Some("Repo")
    );
}

#[test]
fn a_later_local_is_not_in_scope_yet() {
    let source = "class Caller {\n  private Repo repo;\n  void run() {\n    repo.save(\"x\");\n    Other repo = make();\n  }\n}\n";
    assert_eq!(
        receiver_type_of(source, &TestDeps::new(), "save", 0).as_deref(),
        Some("Repo")
    );
}

#[test]
fn chained_call_uses_the_methods_return_type() {
    let source = "class Caller {\n  void run(Repo repo) {\n    repo.next().save(\"x\");\n  }\n}\n";
    let deps = TestDeps::new().with_method_return_for_type("Repo", "next", "Session");
    assert_eq!(
        receiver_type_of(source, &deps, "save", 0).as_deref(),
        Some("Session")
    );
}

#[test]
fn chained_field_access_uses_the_fields_type() {
    let source = "class Caller {\n  void run(Repo repo) {\n    repo.session.close();\n  }\n}\n";
    let deps = TestDeps::new().with_field("Repo", "session", "Session");
    assert_eq!(
        receiver_type_of(source, &deps, "close", 0).as_deref(),
        Some("Session")
    );
}

#[test]
fn bare_call_of_a_method_in_the_same_class_types_its_result() {
    let source = "class Caller {\n  Repo repo() { return null; }\n  void run() {\n    repo().save(\"x\");\n  }\n}\n";
    assert_eq!(
        receiver_type_of(source, &TestDeps::new(), "save", 0).as_deref(),
        Some("Repo")
    );
}

#[test]
fn this_is_the_enclosing_class() {
    let source =
        "class Caller {\n  void save(String s) {}\n  void run() {\n    this.save(\"x\");\n  }\n}\n";
    assert_eq!(
        receiver_type_of(source, &TestDeps::new(), "save", 1).as_deref(),
        Some("Caller")
    );
}

#[test]
fn cast_and_parentheses_type_the_receiver() {
    let source =
        "class Caller {\n  void run(Object value) {\n    ((Repo) value).save(\"x\");\n  }\n}\n";
    assert_eq!(
        receiver_type_of(source, &TestDeps::new(), "save", 0).as_deref(),
        Some("Repo")
    );
}

#[test]
fn string_literal_receiver_is_a_string() {
    let source = "class Caller {\n  void run() {\n    \"abc\".length();\n  }\n}\n";
    assert_eq!(
        receiver_type_of(source, &TestDeps::new(), "length", 0).as_deref(),
        Some("String")
    );
}

#[test]
fn capitalized_receiver_naming_a_type_is_a_static_access() {
    let source = "class Caller {\n  void run() {\n    Repo.create();\n  }\n}\n";
    let deps = TestDeps::new().with_type("Repo");
    assert_eq!(
        receiver_type_of(source, &deps, "create", 0).as_deref(),
        Some("Repo")
    );
}

#[test]
fn enhanced_for_variable_types_its_member_call() {
    let source = "class Caller {\n  void run(List<Repo> repos) {\n    for (Repo each : repos) {\n      each.save(\"x\");\n    }\n  }\n}\n";
    assert_eq!(
        receiver_type_of(source, &TestDeps::new(), "save", 0).as_deref(),
        Some("Repo")
    );
}

#[test]
fn catch_parameter_types_its_member_call() {
    let source = "class Caller {\n  void run() {\n    try { go(); } catch (IOException error) {\n      error.getMessage();\n    }\n  }\n}\n";
    assert_eq!(
        receiver_type_of(source, &TestDeps::new(), "getMessage", 0).as_deref(),
        Some("IOException")
    );
}

#[test]
fn untyped_lambda_parameter_has_no_receiver_type() {
    let source = "class Caller {\n  private Repo item;\n  void run(List<Repo> repos) {\n    repos.forEach(item -> item.save(\"x\"));\n  }\n}\n";
    // `item` is the lambda's parameter, not the field: unknown, not `Repo`.
    assert_eq!(receiver_type_of(source, &TestDeps::new(), "save", 0), None);
}

#[test]
fn typed_lambda_parameter_types_its_member_call() {
    let source = "class Caller {\n  void run(List<Repo> repos) {\n    repos.forEach((Repo item) -> item.save(\"x\"));\n  }\n}\n";
    assert_eq!(
        receiver_type_of(source, &TestDeps::new(), "save", 0).as_deref(),
        Some("Repo")
    );
}

#[test]
fn unknown_receiver_type_is_gated_out() {
    let source = "class Caller {\n  void run(Repo repo) {\n    repo.save(\"x\");\n  }\n}\n";
    let doc = java_doc(source);
    let node = identifier_at(&doc, "save", 0);
    let Some(symbol) =
        classify_java_identifier(node, &doc.bytes, &TestDeps::new(), &uri(), |_, _| false)
    else {
        panic!("did not classify");
    };
    assert!(matches!(
        symbol.role,
        SymbolRole::Reference {
            receiver_type: None,
            ..
        }
    ));
}

#[test]
fn member_call_records_its_argument_count() {
    let source = "class Caller {\n  void run(Repo repo) {\n    repo.save(\"a\", 2);\n  }\n}\n";
    match role_of(source, "save", 0) {
        SymbolRole::Reference {
            is_call: true,
            shape: Some(shape),
            ..
        } => assert_eq!(shape.arg_count, 2),
        other => panic!("expected a call: {other:?}"),
    }
}

#[test]
fn field_assignment_target_is_a_write() {
    let source = "class Caller {\n  void run(Repo repo) {\n    repo.count = 1;\n    int read = repo.count;\n  }\n}\n";
    assert!(matches!(
        role_of(source, "count", 0),
        SymbolRole::Reference {
            access: PropertyAccess::Write,
            ..
        }
    ));
    assert!(matches!(
        role_of(source, "count", 1),
        SymbolRole::Reference {
            access: PropertyAccess::Read,
            ..
        }
    ));
}

#[test]
fn declarations_are_indexed_only_for_members() {
    let source = "class Repo {\n  private int count;\n  void save(String name) {\n    int local = 0;\n  }\n}\n";
    let indexed = |marker: &str| match role_of(source, marker, 0) {
        SymbolRole::Declaration { indexed } => indexed,
        other => panic!("{marker:?} is not a declaration: {other:?}"),
    };
    assert!(indexed("Repo"));
    assert!(indexed("count"));
    assert!(indexed("save"));
    assert!(!indexed("name"));
    assert!(!indexed("local"));
}

#[test]
fn import_path_segments_are_import_segments() {
    let source = "import com.example.Repo;\nclass Caller {}\n";
    assert!(matches!(
        role_of(source, "example", 0),
        SymbolRole::ImportSegment
    ));
    assert!(matches!(
        role_of(source, "Repo", 0),
        SymbolRole::ImportSegment
    ));
}

#[test]
fn bare_local_read_is_a_local_value_reference_but_a_field_read_is_not() {
    let source = "class Caller {\n  private int count;\n  void run(int size) {\n    use(size);\n    use(count);\n  }\n}\n";
    let doc = java_doc(source);
    assert!(is_java_local_value_reference(
        identifier_at(&doc, "size", 1),
        &doc.bytes
    ));
    assert!(!is_java_local_value_reference(
        identifier_at(&doc, "count", 1),
        &doc.bytes
    ));
    assert!(!is_java_local_value_reference(
        identifier_at(&doc, "use", 0),
        &doc.bytes
    ));
}

#[test]
fn local_declaration_is_told_apart_from_a_field() {
    let source = "class Caller {\n  private int count;\n  void run() {\n    int total = 0;\n    Runnable task = new Runnable() {\n      int inner;\n      public void run() {}\n    };\n  }\n}\n";
    let doc = java_doc(source);
    assert!(!is_java_local_declaration(identifier_at(&doc, "count", 0)));
    assert!(is_java_local_declaration(identifier_at(&doc, "total", 0)));
    // A field of an anonymous class is a member of that class.
    assert!(!is_java_local_declaration(identifier_at(&doc, "inner", 0)));
}

#[test]
fn enclosing_class_names_are_innermost_first() {
    let source = "class Outer {\n  class Inner {\n    void run() { go(); }\n  }\n}\n";
    let doc = java_doc(source);
    assert_eq!(
        enclosing_java_class_names(identifier_at(&doc, "go", 0), &doc.bytes),
        vec!["Inner".to_owned(), "Outer".to_owned()]
    );
}

#[test]
fn receiver_type_arguments_substitute_the_members_type() {
    let source =
        "class Caller {\n  void run(List<Repo> repos) {\n    repos.get(0).save(\"x\");\n  }\n}\n";
    let deps = TestDeps::new()
        .with_class_params("List", &["E"])
        .with_method_return_for_type("List", "get", "E");
    assert_eq!(
        receiver_type_of(source, &deps, "save", 0).as_deref(),
        Some("Repo")
    );
}

// ─── overload selection ──────────────────────────────────────────────────────

#[test]
fn type_keys_fold_kotlin_boxed_and_annotated_spellings() {
    assert_eq!(overload_type_key("Int"), "int");
    assert_eq!(overload_type_key("java.lang.Integer"), "int");
    assert_eq!(overload_type_key("Double?"), "double");
    assert_eq!(overload_type_key("@Nullable final String"), "String");
    assert_eq!(overload_type_key("Map<String, List<Int>>"), "Map");
    assert_eq!(overload_type_key("String..."), "String[]");
    assert_eq!(overload_type_key("okio.BufferedSource"), "BufferedSource");
}

#[test]
fn parameter_keys_read_java_and_kotlin_parameter_lists() {
    assert_eq!(
        parameter_type_keys("@Nullable String name, final int count, Object... rest"),
        vec!["String", "int", "Object[]"]
    );
    assert_eq!(
        parameter_type_keys("name: String?, count: Int = 0, vararg rest: Any"),
        vec!["String", "int", "Any[]"]
    );
    assert!(parameter_type_keys("").is_empty());
}

#[test]
fn exact_beats_widening_beats_catch_all_and_closed_classes_rule_out() {
    let double = [ArgumentType::Known("double".into())];
    let string = [ArgumentType::Known("String".into())];
    let int = [ArgumentType::Known("int".into())];
    let score = |arguments: &[ArgumentType], parameter: &str| {
        overload_fit_score(arguments, &[parameter.to_owned()])
    };
    assert!(score(&double, "double") > score(&double, "Number"));
    assert_eq!(score(&double, "String"), None);
    assert_eq!(score(&double, "long"), None, "no narrowing");
    assert!(score(&int, "long") > score(&int, "Object"));
    assert_eq!(score(&string, "BufferedSource"), None);
    assert!(score(&string, "CharSequence").is_some());
    let reader = [ArgumentType::Known("JsonReader".into())];
    assert!(
        score(&reader, "BufferedSource").is_some(),
        "a class may extend another"
    );
    assert_eq!(score(&[ArgumentType::Null], "int"), None);
}

#[test]
fn argument_types_read_literals_and_typed_locals() {
    let source = "class Caller {\n  void run(JsonReader reader) {\n    sink.put(1, 2L, 3.0f, 'c', true, null, -4, \"s\", reader);\n  }\n}\n";
    let doc = java_doc(source);
    let name = identifier_at(&doc, "put", 0);
    let invocation = invocation_of_method_name(name).unwrap();
    let types = java_argument_types(invocation, &doc.bytes, &TestDeps::new(), &uri());
    let known = |key: &str| ArgumentType::Known(key.to_owned());
    assert_eq!(
        types,
        vec![
            known("int"),
            known("long"),
            known("float"),
            known("char"),
            known("boolean"),
            ArgumentType::Null,
            known("int"),
            known("String"),
            known("JsonReader"),
        ]
    );
}
