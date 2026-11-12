//! Tests for [`find_type_definition`]: the declaration of the type of the
//! symbol under the cursor, never the symbol's own declaration.

use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, Url};

use crate::backend::cursor::CursorContext;
use crate::features::type_definition::find_type_definition;
use crate::indexer::Indexer;

/// Java sources indexed together, each `(file name, source)`.
fn java_workspace(files: &[(&str, &str)]) -> Indexer {
    let indexer = Indexer::new();
    for (name, source) in files {
        let uri = java_uri(name);
        indexer.index_content(&uri, source);
        indexer.store_live_tree(&uri, source);
    }
    indexer
}

fn java_uri(name: &str) -> Url {
    Url::parse(&format!("file:///t/{name}")).unwrap()
}

/// The position of the first `marker` in `source`.
fn position_of(source: &str, marker: &str) -> Position {
    for (line_number, line) in source.lines().enumerate() {
        if let Some(column) = line.find(marker) {
            return Position::new(line_number as u32, column as u32);
        }
    }
    panic!("{marker:?} not in source");
}

/// The (file name, 0-based line) of every type definition at `marker` in
/// file `name`.
async fn type_definitions_at(
    indexer: &Indexer,
    name: &str,
    source: &str,
    marker: &str,
) -> Vec<(String, u32)> {
    let uri = java_uri(name);
    let position = position_of(source, marker);
    let ctx = CursorContext::build(indexer, &uri, position).unwrap();
    let locations: Vec<Location> = match find_type_definition(&ctx, indexer, &uri, position).await {
        None => Vec::new(),
        Some(GotoDefinitionResponse::Scalar(location)) => vec![location],
        Some(GotoDefinitionResponse::Array(locations)) => locations,
        Some(other) => panic!("unexpected response {other:?}"),
    };
    locations
        .into_iter()
        .map(|location| {
            let file = location.uri.path().rsplit('/').next().unwrap().to_owned();
            (file, location.range.start.line)
        })
        .collect()
}

const PART: &str = "package app;\npublic class Part {\n  String name;\n}\n";
const ENGINE: &str = "package app;\npublic class Engine {\n  Part part;\n}\n";
const GADGET: &str = "package app;\n\
public class Gadget {\n\
  Part part;\n\
  int count;\n\
  Gadget(int count) { this.count = count; }\n\
  Engine engine() { return null; }\n\
}\n";

#[tokio::test]
async fn a_type_name_is_its_own_type_definition() {
    let caller = "package app;\nclass Caller {\n  void run(Part input) {}\n}\n";
    let indexer = java_workspace(&[("Part.java", PART), ("Caller.java", caller)]);
    let found = type_definitions_at(&indexer, "Caller.java", caller, "Part input").await;
    assert_eq!(found, vec![("Part.java".to_owned(), 1)]);
}

/// The field's own declaration is in `Gadget`; its type is `Part`. The decoy
/// `Engine.part` field has the same name and must not be confused with it.
#[tokio::test]
async fn a_field_goes_to_the_type_it_is_declared_with() {
    let caller = "package app;\nclass Caller {\n  void run(Gadget gadget) {\n    String n = gadget.part.name;\n  }\n}\n";
    let indexer = java_workspace(&[
        ("Part.java", PART),
        ("Engine.java", ENGINE),
        ("Gadget.java", GADGET),
        ("Caller.java", caller),
    ]);
    let found = type_definitions_at(&indexer, "Caller.java", caller, "part.name").await;
    assert_eq!(found, vec![("Part.java".to_owned(), 1)]);
}

/// A parameter is typed by its declaration, not resolved by its name.
#[tokio::test]
async fn a_parameter_goes_to_its_declared_type() {
    let caller = "package app;\nclass Caller {\n  void run(Gadget engine) {\n    Object o = engine;\n  }\n}\n";
    let indexer = java_workspace(&[
        ("Engine.java", ENGINE),
        ("Gadget.java", GADGET),
        ("Caller.java", caller),
    ]);
    let found = type_definitions_at(&indexer, "Caller.java", caller, "engine;").await;
    assert_eq!(found, vec![("Gadget.java".to_owned(), 1)]);
}

/// `new Gadget(1)` goes to the constructor for go-to-definition; its type
/// is the class.
#[tokio::test]
async fn an_object_creation_goes_to_the_class_not_the_constructor() {
    let caller =
        "package app;\nclass Caller {\n  Object make() {\n    return new Gadget(1);\n  }\n}\n";
    let indexer = java_workspace(&[("Gadget.java", GADGET), ("Caller.java", caller)]);
    let found = type_definitions_at(&indexer, "Caller.java", caller, "Gadget(1)").await;
    assert_eq!(found, vec![("Gadget.java".to_owned(), 1)]);
}

#[tokio::test]
async fn a_method_call_goes_to_its_return_type() {
    let caller =
        "package app;\nclass Caller {\n  void run(Gadget gadget) {\n    gadget.engine();\n  }\n}\n";
    let indexer = java_workspace(&[
        ("Engine.java", ENGINE),
        ("Gadget.java", GADGET),
        ("Caller.java", caller),
    ]);
    let found = type_definitions_at(&indexer, "Caller.java", caller, "engine()").await;
    assert_eq!(found, vec![("Engine.java".to_owned(), 1)]);
}

#[tokio::test]
async fn an_enum_constant_goes_to_its_enum() {
    let color = "package app;\npublic enum Color {\n  RED,\n  GREEN\n}\n";
    let caller =
        "package app;\nclass Caller {\n  Object pick() {\n    return Color.GREEN;\n  }\n}\n";
    let indexer = java_workspace(&[("Color.java", color), ("Caller.java", caller)]);
    let found = type_definitions_at(&indexer, "Caller.java", caller, "GREEN").await;
    assert_eq!(found, vec![("Color.java".to_owned(), 1)]);
}

/// A field typed by a type parameter goes to that type parameter, declared
/// on the class line — not to a same-named class elsewhere.
#[tokio::test]
async fn a_field_typed_by_a_type_parameter_goes_to_the_type_parameter() {
    let decoy = "package app;\npublic class S {}\n";
    let holder = "package app;\n\
public class Holder<S> {\n\
  S value;\n\
  void set(S next) { this.value = next; }\n\
}\n";
    let indexer = java_workspace(&[("S.java", decoy), ("Holder.java", holder)]);
    let found = type_definitions_at(&indexer, "Holder.java", holder, "value = next").await;
    assert_eq!(found, vec![("Holder.java".to_owned(), 1)]);
}

/// A primitive has no declaration to go to.
#[tokio::test]
async fn a_primitive_field_has_no_type_definition() {
    let caller = "package app;\nclass Caller {\n  int run(Gadget gadget) {\n    return gadget.count;\n  }\n}\n";
    let indexer = java_workspace(&[("Gadget.java", GADGET), ("Caller.java", caller)]);
    let found = type_definitions_at(&indexer, "Caller.java", caller, "count;").await;
    assert!(found.is_empty(), "got {found:?}");
}

/// `Map.Entry<K, V>[]` is typed by its last segment, `Entry`.
#[tokio::test]
async fn a_qualified_generic_array_type_goes_to_its_last_segment() {
    let outer = "package app;\npublic class Outer {\n  public static class Entry<K, V> {}\n}\n";
    let caller = "package app;\nclass Caller {\n  Outer.Entry<String, Part>[] entries;\n  Object first() {\n    return entries;\n  }\n}\n";
    let indexer = java_workspace(&[
        ("Outer.java", outer),
        ("Part.java", PART),
        ("Caller.java", caller),
    ]);
    let found = type_definitions_at(&indexer, "Caller.java", caller, "entries;").await;
    assert_eq!(found, vec![("Outer.java".to_owned(), 2)]);
}

/// An untyped lambda parameter's type comes from the lambda's written
/// target type: `src` of `(Serializer<Base>) (src, context) -> ..` fills
/// `serialize(T src, ..)`'s `T` with `Base`; `context` is the `Context` the
/// interface writes for it.
#[tokio::test]
async fn lambda_parameter_goes_to_the_type_its_target_gives_it() {
    let serializer = "interface Serializer<T> {\n  Object serialize(T src, Context context);\n}\n";
    let caller = "class Caller {\n\
                  \x20 Object make() {\n\
                  \x20   return (Serializer<Base>) (src, context) -> src.name + context.id;\n\
                  \x20 }\n\
                  }\n";
    let indexer = java_workspace(&[
        ("Serializer.java", serializer),
        ("Base.java", "class Base {\n  String name;\n}\n"),
        (
            "Context.java",
            "// context\nclass Context {\n  int id;\n}\n",
        ),
        ("Caller.java", caller),
    ]);

    assert_eq!(
        type_definitions_at(&indexer, "Caller.java", caller, "src.name").await,
        vec![("Base.java".to_owned(), 0)]
    );
    assert_eq!(
        type_definitions_at(&indexer, "Caller.java", caller, "context.id").await,
        vec![("Context.java".to_owned(), 1)]
    );
}
