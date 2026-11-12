//! Tests for [`find_java_references`]: real `.java` files in a temp
//! directory (the occurrence search runs `rg`), each with decoys that only a
//! name-based search would report.

use std::sync::Arc;

use tower_lsp::lsp_types::{Location, Position, Url};

use super::*;

fn write(dir: &std::path::Path, name: &str, content: &str) -> Url {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    Url::from_file_path(&path).unwrap()
}

/// `file:line` (1-based) of every location, sorted.
fn hits(locations: &[Location]) -> Vec<String> {
    let mut hits: Vec<String> = locations
        .iter()
        .map(|location| {
            let path = location.uri.to_file_path().unwrap();
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            format!("{file}:{}", location.range.start.line + 1)
        })
        .collect();
    hits.sort();
    hits
}

fn position_of(source: &str, line: u32, name: &str) -> Position {
    let text = source.lines().nth(line as usize).unwrap();
    Position::new(line, text.find(name).unwrap() as u32)
}

struct Workspace {
    _dir: tempfile::TempDir,
    index: Arc<Indexer>,
}

impl Workspace {
    fn new(files: &[(&str, &str)]) -> (Self, Vec<Url>) {
        let dir = tempfile::tempdir().unwrap();
        let index = Arc::new(Indexer::new());
        index.workspace_root.set(dir.path().to_path_buf());
        let uris = files
            .iter()
            .map(|(name, source)| {
                let uri = write(dir.path(), name, source);
                index.index_content(&uri, source);
                uri
            })
            .collect();
        (Self { _dir: dir, index }, uris)
    }
}

const READER: &str = "package p;\n\
public class Reader {\n\
  /** Ends the object. See {@link #endObject()}. */\n\
  public void endObject() {}\n\
  public void endObject(int depth) {}\n\
}\n";

const TREE_READER: &str = "package p;\n\
public class TreeReader extends Reader {\n\
  @Override\n\
  public void endObject() {}\n\
}\n";

const WRITER: &str = "package p;\n\
public class Writer {\n\
  public void endObject() {}\n\
}\n";

const USES: &str = "package p;\n\
class Uses {\n\
  void run(Reader reader, Writer writer, TreeReader tree) {\n\
    reader.endObject();\n\
    writer.endObject();\n\
    tree.endObject();\n\
    String name = \"endObject\";\n\
  }\n\
  /** Like {@link TreeReader#endObject()} and {@link Writer#endObject()}. */\n\
  void documented() {}\n\
}\n";

/// From an override, a usage is a call that may run it: on its own type, or
/// through the method it overrides (`reader.endObject()` on a `Reader`). Not
/// a same-named method of an unrelated type, an overload, another
/// declaration, a string, or a javadoc link to an unrelated type.
#[tokio::test]
async fn method_references_follow_overrides_not_names() {
    let (workspace, uris) = Workspace::new(&[
        ("p/Reader.java", READER),
        ("p/TreeReader.java", TREE_READER),
        ("p/Writer.java", WRITER),
        ("p/Uses.java", USES),
    ]);
    let tree_reader = &uris[1];

    let found = find_java_references(
        &workspace.index,
        tree_reader,
        position_of(TREE_READER, 3, "endObject"),
        false,
    )
    .await
    .expect("a Java method is answered here");

    assert_eq!(
        hits(&found),
        vec!["Uses.java:4", "Uses.java:6", "Uses.java:9"]
    );
}

const FOO: &str = "package a;\n\
public class Foo {\n\
  public Foo() {}\n\
  public Foo(int size) {}\n\
}\n";

const OTHER_FOO: &str = "package b;\n\
public class Foo {}\n";

const FOO_USER: &str = "package c;\n\
\n\
import a.Foo;\n\
\n\
/** Builds a {@link Foo}; see also {@link b.Foo}. */\n\
class FooUser {\n\
  Foo make() {\n\
    return new Foo(3);\n\
  }\n\
  String label = \"Foo\";\n\
}\n";

/// A type's usages: its import, a constructor call, a javadoc link that
/// names it. Not its constructors' declarations, a same-named type of
/// another package, or a string.
#[tokio::test]
async fn type_references_count_imports_constructor_calls_and_javadoc() {
    let (workspace, uris) = Workspace::new(&[
        ("a/Foo.java", FOO),
        ("b/Foo.java", OTHER_FOO),
        ("c/FooUser.java", FOO_USER),
    ]);
    let foo = &uris[0];

    let found = find_java_references(&workspace.index, foo, position_of(FOO, 1, "Foo"), false)
        .await
        .expect("a Java type is answered here");

    assert_eq!(
        hits(&found),
        vec![
            "FooUser.java:3",
            "FooUser.java:5",
            "FooUser.java:7",
            "FooUser.java:8"
        ]
    );
}

#[test]
fn javadoc_reference_splits_type_member_and_parameters() {
    let line = "   * Uses {@link GsonBuilder#setDateFormat(int, int)} instead.";
    let column = line.find("setDateFormat").unwrap();

    let reference = javadoc_reference_at(line, column, "setDateFormat").unwrap();

    assert_eq!(reference.type_part, "GsonBuilder");
    assert_eq!(reference.member.as_deref(), Some("setDateFormat"));
    assert_eq!(
        reference.parameters,
        Some(vec!["int".to_owned(), "int".to_owned()])
    );
    assert!(reference.names_member);
    assert!(!reference.names_type);
}

#[test]
fn javadoc_reference_is_absent_outside_a_reference_tag() {
    let line = "   * The setDateFormat method, see {@link Other}.";
    let column = line.find("setDateFormat").unwrap();

    assert_eq!(javadoc_reference_at(line, column, "setDateFormat"), None);
}

#[test]
fn declared_parameter_types_keep_only_simple_type_names() {
    assert_eq!(
        declared_parameter_types(
            "final java.util.List<String> names, int... values, @Nullable Foo foo"
        ),
        vec!["List", "int[]", "Foo"]
    );
}

const CURRENT_WRITE: &str = "package p;\n\
class CurrentWrite implements CharSequence {\n\
  @Override public String toString() { return \"\"; }\n\
}\n";

const JDK_OBJECT: &str = "package java.lang;\n\
public class Object {\n\
  public String toString() { return null; }\n\
}\n";

/// A call bound to `Object.toString()` may run the target's override, also
/// when the JDK's sources are indexed beside the workspace's (so
/// `Object.java` has symbols but is not a workspace file).
#[tokio::test]
async fn a_call_bound_to_an_indexed_jdk_object_method_may_run_the_override() {
    let (workspace, uris) = Workspace::new(&[("p/CurrentWrite.java", CURRENT_WRITE)]);
    let object =
        Url::parse("jar:file:///jdk/lib/src.zip!/java.base/java/lang/Object.java").unwrap();
    workspace.index.index_content(&object, JDK_OBJECT);
    let target = JavaTarget::at(
        &workspace.index,
        &uris[0],
        position_of(CURRENT_WRITE, 2, "toString"),
    )
    .await
    .expect("the override is a Java method target");
    let mut matcher = TargetMatcher {
        index: &workspace.index,
        target: &target,
        exact: false,
        resolved: HashMap::new(),
    };
    let object_to_string = Location::new(
        object,
        Range::new(Position::new(2, 16), Position::new(2, 24)),
    );
    assert!(matcher.definition_is_target(&object_to_string));
}

const LOCAL_READER: &str = "package p;\n\
public class LocalReader extends Reader {\n\
  @Override\n\
  public void endObject() {}\n\
  void run(Reader reader, LocalReader local) {\n\
    reader.endObject();\n\
    local.endObject();\n\
  }\n\
}\n";

/// A highlight marks what binds to the method itself: `reader.endObject()`
/// on a `Reader` binds to `Reader.endObject`, even if it may run this one.
#[tokio::test]
async fn highlights_mark_only_uses_bound_to_the_method() {
    let (workspace, uris) = Workspace::new(&[
        ("p/Reader.java", READER),
        ("p/LocalReader.java", LOCAL_READER),
    ]);
    let highlights = java_document_highlights(
        &workspace.index,
        &uris[1],
        position_of(LOCAL_READER, 3, "endObject"),
    )
    .await
    .expect("a Java method is answered here");
    let mut lines: Vec<(u32, bool)> = highlights
        .iter()
        .map(|highlight| {
            (
                highlight.range.start.line + 1,
                highlight.kind == Some(DocumentHighlightKind::WRITE),
            )
        })
        .collect();
    lines.sort();
    assert_eq!(lines, vec![(4, true), (7, false)]);
}

fn renamed(rename: Option<JavaRename>) -> Vec<String> {
    match rename {
        Some(JavaRename::Names(locations)) => hits(&locations),
        Some(JavaRename::Refused(reason)) => panic!("refused: {reason}"),
        None => panic!("not answered"),
    }
}

/// Renaming an override renames its whole family — the method it overrides,
/// the other override of that, and the uses of each — but not the overload
/// or the unrelated `Writer.endObject`.
#[tokio::test]
async fn renaming_an_override_renames_its_family() {
    let (workspace, uris) = Workspace::new(&[
        ("p/Reader.java", READER),
        ("p/TreeReader.java", TREE_READER),
        ("p/Writer.java", WRITER),
        ("p/Uses.java", USES),
    ]);
    let rename = java_rename(
        &workspace.index,
        &uris[1],
        position_of(TREE_READER, 3, "endObject"),
    )
    .await;
    assert_eq!(
        renamed(rename),
        vec![
            "Reader.java:3",
            "Reader.java:4",
            "TreeReader.java:4",
            "Uses.java:4",
            "Uses.java:6",
            "Uses.java:9",
        ]
    );
}

/// A type's rename changes its constructors' names with it.
#[tokio::test]
async fn renaming_a_type_renames_its_constructors() {
    let (workspace, uris) = Workspace::new(&[
        ("a/Foo.java", FOO),
        ("b/Foo.java", OTHER_FOO),
        ("c/FooUser.java", FOO_USER),
    ]);
    let rename = java_rename(&workspace.index, &uris[0], position_of(FOO, 1, "Foo")).await;
    assert_eq!(
        renamed(rename),
        vec![
            "Foo.java:2",
            "Foo.java:3",
            "Foo.java:4",
            "FooUser.java:3",
            "FooUser.java:5",
            "FooUser.java:7",
            "FooUser.java:8",
        ]
    );
}

const ANONYMOUS: &str = "package p;\n\
class Anonymous {\n\
  Reader reader = new Reader() {\n\
    @Override\n\
    public void endObject() {}\n\
  };\n\
}\n";

/// An anonymous class's method belongs to its supertype's family, which a
/// rename from here can't see whole: refused, rather than half-renamed.
#[tokio::test]
async fn renaming_an_anonymous_class_method_is_refused() {
    let (workspace, uris) =
        Workspace::new(&[("p/Reader.java", READER), ("p/Anonymous.java", ANONYMOUS)]);
    let rename = java_rename(
        &workspace.index,
        &uris[1],
        position_of(ANONYMOUS, 4, "endObject"),
    )
    .await;
    assert!(matches!(rename, Some(JavaRename::Refused(_))));
}

const STATE: &str = "package s;\n\
public class State {\n\
  public static final int EMPTY = 1;\n\
  private Object delegate;\n\
  void setDelegate(Object value) { delegate = value; }\n\
  Object delegate() {\n\
    Object delegate = this.delegate;\n\
    return delegate;\n\
  }\n\
  int count(State other) { return other.delegate == null ? EMPTY : 0; }\n\
}\n";

const STATE_USER: &str = "package u;\n\
\n\
import static s.State.EMPTY;\n\
import s.State;\n\
\n\
class StateUser {\n\
  int EMPTY_TOO = EMPTY;\n\
  void run(State state) {\n\
    state.setDelegate(state.delegate());\n\
    int EMPTY = 2;\n\
    int copy = EMPTY;\n\
  }\n\
}\n";

/// A field's usages: its bare and qualified reads and writes, an
/// `import static`, and a call of its setter (jdtls's default
/// `includeAccessors`). Not the same-named method `delegate()`, a local
/// that shadows it, or another class's same-named local.
#[tokio::test]
async fn field_references_follow_bindings_and_include_accessor_calls() {
    let (workspace, uris) =
        Workspace::new(&[("s/State.java", STATE), ("u/StateUser.java", STATE_USER)]);
    let state = &uris[0];

    let delegate = find_java_references(
        &workspace.index,
        state,
        position_of(STATE, 3, "delegate"),
        false,
    )
    .await
    .expect("a Java field is answered here");
    assert_eq!(
        hits(&delegate),
        vec![
            "State.java:10",
            "State.java:5",
            "State.java:7",
            "StateUser.java:9"
        ]
    );

    let empty = find_java_references(
        &workspace.index,
        state,
        position_of(STATE, 2, "EMPTY"),
        false,
    )
    .await
    .expect("a Java constant is answered here");
    assert_eq!(
        hits(&empty),
        vec!["State.java:10", "StateUser.java:3", "StateUser.java:7"]
    );
}

/// Renaming a field changes its name, not the calls of its setter.
#[tokio::test]
async fn renaming_a_field_leaves_its_accessor_calls() {
    let (workspace, uris) =
        Workspace::new(&[("s/State.java", STATE), ("u/StateUser.java", STATE_USER)]);
    let rename = java_rename(
        &workspace.index,
        &uris[0],
        position_of(STATE, 3, "delegate"),
    )
    .await;
    assert_eq!(
        renamed(rename),
        vec![
            "State.java:10",
            "State.java:4",
            "State.java:5",
            "State.java:7"
        ]
    );
}

/// A constructor's usages are the creations its arguments select: not the
/// other constructor's, nor the class name used as a qualifier.
#[tokio::test]
async fn constructor_references_are_the_creations_that_select_it() {
    let builder = "package b;\n\
public class Builder {\n\
  static final Builder DEFAULT = new Builder();\n\
  public Builder() {}\n\
  Builder(Builder from) {}\n\
  Builder copy() { return new Builder(this); }\n\
  static Builder fresh() { return new Builder(Builder.DEFAULT); }\n\
}\n";
    let (workspace, uris) = Workspace::new(&[("b/Builder.java", builder)]);

    let found = find_java_references(
        &workspace.index,
        &uris[0],
        position_of(builder, 4, "Builder"),
        false,
    )
    .await
    .expect("a Java constructor is answered here");

    assert_eq!(hits(&found), vec!["Builder.java:6", "Builder.java:7"]);
}

/// A local's usages are in its own scope: not a same-named field, a
/// same-named local of another method, or a member access `other.src`; a
/// lambda parameter's are in its lambda.
#[tokio::test]
async fn local_references_stay_in_their_scope() {
    let scopes = "package l;\n\
class Scopes {\n\
  Object src;\n\
  void first(Scopes other) {\n\
    Object src = other.src;\n\
    use(src);\n\
  }\n\
  void second() {\n\
    java.util.function.Function<Object, Object> f = src -> src;\n\
    Object src = this.src;\n\
    use(src);\n\
  }\n\
  void use(Object value) {}\n\
}\n";
    let (workspace, uris) = Workspace::new(&[("l/Scopes.java", scopes)]);
    let local = |line: u32, nth: usize| {
        let text = scopes.lines().nth(line as usize).unwrap();
        let column = text.match_indices("src").nth(nth).unwrap().0;
        Position::new(line, column as u32)
    };

    let first = find_java_references(&workspace.index, &uris[0], local(4, 0), false)
        .await
        .expect("a Java local is answered here");
    assert_eq!(hits(&first), vec!["Scopes.java:6"]);

    let lambda_parameter = find_java_references(&workspace.index, &uris[0], local(8, 0), false)
        .await
        .expect("a lambda parameter is answered here");
    assert_eq!(lambda_parameter.len(), 1);
    assert_eq!(hits(&lambda_parameter), vec!["Scopes.java:9"]);
}
