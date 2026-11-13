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

/// The names in scope where `|` sits in `source` (the marker is removed).
fn scope_local_names(source: &str) -> Vec<String> {
    let line = source.lines().position(|line| line.contains('|')).unwrap();
    let column = source.lines().nth(line).unwrap().find('|').unwrap();
    let doc = java_doc(&source.replacen('|', "", 1));
    java_scope_locals_at(&doc, tree_sitter::Point { row: line, column })
        .into_iter()
        .map(|local| local.name)
        .collect()
}

#[test]
fn scope_locals_are_the_bindings_that_reach_the_cursor() {
    let names = scope_local_names(
        "class Caller {
           private int field;
           void use(String html, int count) {
             String slug = html;
             for (Entry entry : entries) {
               try (Reader reader = open()) {
                 |
               } catch (IOException failure) {}
             }
             String later = slug;
           }
           void other(String elsewhere) {}
         }
",
    );
    for reaching in ["html", "count", "slug", "entry", "reader"] {
        assert!(names.contains(&reaching.to_owned()), "{names:?}");
    }
    for unreachable in ["later", "failure", "elsewhere", "field"] {
        assert!(!names.contains(&unreachable.to_owned()), "{names:?}");
    }
}

#[test]
fn scope_local_carries_its_written_type() {
    let doc = java_doc(
        "class Caller {
  void use(java.util.List<User> users) {
    var count = 1;
    
  }
}
",
    );
    let locals = java_scope_locals_at(&doc, tree_sitter::Point { row: 3, column: 4 });
    let typed = |name: &str| {
        locals
            .iter()
            .find(|local| local.name == name)
            .map(|local| local.type_text.clone())
    };
    assert_eq!(typed("users"), Some("java.util.List<User>".to_owned()));
    // `var` writes no type — the completion offers the name without a detail.
    assert_eq!(typed("count"), Some(String::new()));
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

/// No hierarchy at hand: no class is known to extend another.
struct UnknownHierarchy;

impl ClassHierarchy for UnknownHierarchy {
    fn extends(&self, _class: &str, _anchor: &Url, _supertype: &str) -> bool {
        false
    }
}

#[test]
fn exact_beats_widening_beats_catch_all_and_closed_classes_rule_out() {
    let double = [ArgumentType::Known("double".into())];
    let string = [ArgumentType::Known("String".into())];
    let int = [ArgumentType::Known("int".into())];
    let score = |arguments: &[ArgumentType], parameter: &str| {
        overload_fit_score(arguments, &[parameter.to_owned()], &UnknownHierarchy)
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
    let declared = |key: &str| ArgumentType::Declared {
        key: key.to_owned(),
        anchor: uri(),
    };
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
            declared("String"),
            declared("JsonReader"),
        ]
    );
}

#[test]
fn class_literal_is_a_class_of_its_type() {
    let source = "class Caller {\n  boolean run(Class<?> other) {\n    return Factory.class.isAssignableFrom(other);\n  }\n}\n";
    assert_eq!(
        receiver_type_of(source, &TestDeps::new(), "isAssignableFrom", 0).as_deref(),
        Some("Class")
    );
}

#[test]
fn statically_imported_call_types_its_result() {
    let source = "import static com.google.common.truth.Truth.assertThat;\n\
                  import static org.junit.Assert.*;\n\
                  class Caller {\n  void run() {\n    assertThat(1).isEqualTo(1);\n    other().go();\n  }\n}\n";
    let deps = TestDeps::new()
        .with_method_return_for_type("Truth", "assertThat", "Subject")
        .with_method_return_for_type("Assert", "other", "Thing");
    assert_eq!(
        receiver_type_of(source, &deps, "isEqualTo", 0).as_deref(),
        Some("Subject")
    );
    assert_eq!(
        receiver_type_of(source, &deps, "go", 1).as_deref(),
        Some("Thing")
    );
}

#[test]
fn a_sources_jar_entry_resolves_names_through_its_own_imports() {
    let entry =
        Url::parse("jar:file:///g/truth-1.4.5-sources.jar!/com/google/common/truth/Truth.java")
            .unwrap();
    let compiled_stub = Url::parse("jar:file:///g/truth-1.4.5.jar").unwrap();
    assert!(super::super::has_own_imports(&entry));
    assert!(!super::super::has_own_imports(&compiled_stub));
    assert!(super::super::has_own_imports(&uri()));
}

// ─── parameter-name hints ────────────────────────────────────────────────────

fn parameter_names(params: &str) -> Option<Vec<(String, bool)>> {
    super::declared_parameters(params).map(|parameters| {
        parameters
            .into_iter()
            .map(|parameter| (parameter.name, parameter.is_varargs))
            .collect()
    })
}

#[test]
fn declared_parameters_read_java_and_kotlin_parameter_text() {
    let named = |name: &str, is_varargs: bool| (name.to_owned(), is_varargs);
    assert_eq!(
        parameter_names("final @Nullable Map<K, V> map, int... counts"),
        Some(vec![named("map", false), named("counts", true)])
    );
    assert_eq!(
        parameter_names("name: String? = null, vararg counts: Int"),
        Some(vec![named("name", false), named("counts", true)])
    );
    assert_eq!(parameter_names(""), Some(Vec::new()));
}

/// A compiled class without parameter names names nothing: no hint is
/// better than `p0:`.
#[test]
fn declared_parameters_reject_synthetic_names() {
    assert_eq!(parameter_names("p0: Int, p1: String"), None);
    assert_eq!(parameter_names("int arg0"), None);
    assert_eq!(
        parameter_names("int p, String args"),
        Some(vec![("p".to_owned(), false), ("args".to_owned(), false)])
    );
}

#[test]
fn parameter_for_argument_follows_varargs_and_arity() {
    let format_and_args = super::declared_parameters("String format, Object... args").unwrap();
    let name_of = |count: usize, index: usize| {
        super::parameter_for_argument(&format_and_args, count, index)
            .map(|parameter| parameter.name.clone())
    };
    assert_eq!(name_of(3, 0).as_deref(), Some("format"));
    assert_eq!(name_of(3, 1).as_deref(), Some("args"));
    assert_eq!(
        name_of(3, 2),
        None,
        "a later varargs argument is not named again"
    );
    assert_eq!(
        name_of(0, 0),
        None,
        "too few arguments for this declaration"
    );

    let one = super::declared_parameters("int count").unwrap();
    assert!(super::parameter_for_argument(&one, 2, 0).is_none());
}

#[test]
fn literal_argument_calls_list_each_call_with_its_literals() {
    let src = "class A {\n\
               \x20 A() { this(1, name); }\n\
               \x20 void f(B b, int x) { b.go(x, \"s\", null, A.class, -1); new C(true); }\n\
               }\n";
    let doc = java_doc(src);
    let calls = super::literal_argument_calls(doc.tree.root_node(), 0..=3);
    let summary: Vec<(usize, usize, Vec<usize>)> = calls
        .iter()
        .map(|call| {
            (
                call.callee.row,
                call.callee.column,
                call.literals.iter().map(|literal| literal.index).collect(),
            )
        })
        .collect();
    let line_2 = src.lines().nth(2).unwrap();
    assert_eq!(
        summary,
        vec![
            (
                1,
                src.lines().nth(1).unwrap().find("this").unwrap(),
                vec![0]
            ),
            (2, line_2.find("go").unwrap(), vec![1, 2, 3]),
            (2, line_2.find("C(").unwrap(), vec![0]),
        ]
    );
    assert!(super::literal_argument_calls(doc.tree.root_node(), 3..=3).is_empty());
}

#[test]
fn java_type_keys_keep_boxes_apart_from_primitives() {
    assert_eq!(java_overload_type_key("Boolean"), "Boolean");
    assert_eq!(java_overload_type_key("java.lang.Integer"), "Integer");
    assert_eq!(java_overload_type_key("boolean"), "boolean");
    assert_eq!(java_overload_type_key("@Nullable Double..."), "Double[]");
    assert_eq!(
        parameter_type_keys("Boolean value, long count"),
        vec!["Boolean", "long"]
    );
    // Kotlin's `Boolean` is both: still folded.
    assert_eq!(parameter_type_keys("value: Boolean"), vec!["boolean"]);
}

#[test]
fn boxing_is_tried_only_after_widening_and_subtyping() {
    let score = |argument: &str, parameter: &str| {
        overload_fit_score(
            &[ArgumentType::Known(argument.into())],
            &[parameter.to_owned()],
            &UnknownHierarchy,
        )
    };
    assert!(score("boolean", "boolean") > score("boolean", "Boolean"));
    assert!(score("Boolean", "Boolean") > score("Boolean", "boolean"));
    assert!(score("int", "long") > score("int", "Integer"));
    assert!(score("Double", "Number") > score("Double", "double"));
    assert_eq!(score("int", "Long"), None, "boxing never widens");
    assert_eq!(
        score("Integer", "short"),
        None,
        "no narrowing after unboxing"
    );
    assert!(
        score("Integer", "long").is_some(),
        "unboxing, then widening"
    );
    assert!(score("char", "int").is_some());
}

#[test]
fn more_specific_overload_is_the_one_whose_parameters_are_subtypes() {
    let keys = |keys: &[&str]| keys.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>();
    let specific = keys(&["String", "Appendable"]);
    let general = keys(&["Object", "Appendable"]);
    assert!(is_more_specific(
        &specific,
        &general,
        &uri(),
        &UnknownHierarchy
    ));
    assert!(!is_more_specific(
        &general,
        &specific,
        &uri(),
        &UnknownHierarchy
    ));
    assert!(is_more_specific(
        &keys(&["int"]),
        &keys(&["long"]),
        &uri(),
        &UnknownHierarchy
    ));
    assert!(!is_more_specific(
        &keys(&["int"]),
        &keys(&["Integer"]),
        &uri(),
        &UnknownHierarchy
    ));
}
