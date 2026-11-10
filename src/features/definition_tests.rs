//! Tests for [`find_definition`] — CST-resolved-first, NameScan-fallback.

use tower_lsp::lsp_types::{GotoDefinitionResponse, Position, Url};

use crate::backend::cursor::CursorContext;
use crate::features::definition::find_definition;
use crate::indexer::Indexer;

/// House decoy: a call-expression receiver (`getUser().save()`). The
/// string/word-based qualifier extraction (`word_and_qualifier_at`) only
/// captures a simple identifier immediately before the dot — it can't
/// capture `getUser()` as a qualifier at all (the char before the dot is
/// `)`, not an identifier char), so today's path treats `save` as a BARE,
/// receiver-less reference and falls through to an unqualified same-name
/// scan that can't distinguish `User.save` from `Admin.save`. The CST path
/// walks the actual `navigation_expression` and resolves the call
/// receiver's return type directly, so it must land on `User.save` only.
#[tokio::test]
async fn goto_definition_resolves_call_expression_receiver_via_cst() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/D.kt").unwrap();
    let src = "class User { fun save() {} }\n\
               class Admin { fun save() {} }\n\
               fun getUser(): User = User()\n\
               fun f() { getUser().save() }\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);
    let col = src.lines().nth(3).unwrap().find("save").unwrap() as u32;
    let ctx = CursorContext::build(&idx, &uri, Position::new(3, col)).unwrap();
    let response = find_definition(&ctx, &idx, &uri, Position::new(3, col))
        .await
        .unwrap();
    let loc = match response {
        GotoDefinitionResponse::Scalar(l) => l,
        other => panic!("expected a single location, got {other:?}"),
    };
    assert_eq!(
        loc.range.start.line, 0,
        "must jump to User.save, not Admin.save"
    );
}

/// The reported bug: `fun <T> Flow<T>.collect(scope, block) { collect(block) }`
/// — the inner 1-arg `collect(block)` must not goto-definition back to the
/// enclosing 2-required-arg declaration just because it's the only same-named
/// symbol in the file. Exercises the whole path: `call_shape_at_callee`'s CST
/// classification, `find_definition_for_call`, and `resolve_callee_definition`'s
/// arity filter together — not just the resolver internals directly.
#[tokio::test]
async fn goto_definition_does_not_resolve_wrong_arity_call_to_enclosing_self() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/Flow.kt").unwrap();
    let src = "class CoroutineScope\n\
               fun <T : Any> Flow<T>.collect(scope: CoroutineScope, block: (T) -> Unit) {\n\
                   collect(block)\n\
               }\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);
    let col = src.lines().nth(2).unwrap().find("collect").unwrap() as u32;
    let position = Position::new(2, col);
    let ctx = CursorContext::build(&idx, &uri, position).unwrap();
    let response = find_definition(&ctx, &idx, &uri, position).await;
    if let Some(GotoDefinitionResponse::Scalar(loc)) = &response {
        assert_ne!(
            loc.range.start.line, 1,
            "must not jump back to the enclosing declaration itself, got: {response:?}"
        );
    }
    if let Some(GotoDefinitionResponse::Array(locs)) = &response {
        assert!(
            !locs.iter().any(|loc| loc.range.start.line == 1),
            "must not include the enclosing declaration among the results, got: {response:?}"
        );
    }
}

/// Goto-definition runs constantly on mid-edit, ERROR-recovered buffers.
/// `call_shape_at_callee` is the first caller of `call_shape_of` that starts
/// from an arbitrary live cursor position rather than a node already known to
/// enclose complete syntax — verify an unterminated call argument list (the
/// user is still typing) degrades safely rather than miscounting and wrongly
/// excluding the one legitimate candidate.
#[tokio::test]
async fn goto_definition_on_unterminated_call_still_resolves() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/Incomplete.kt").unwrap();
    let src = "fun greet(name: String) {}\n\
               fun test() {\n\
                   greet(name\n\
               }\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);
    let col = src.lines().nth(2).unwrap().find("greet").unwrap() as u32;
    let position = Position::new(2, col);
    let ctx = CursorContext::build(&idx, &uri, position).unwrap();
    let response = find_definition(&ctx, &idx, &uri, position).await;
    match response {
        Some(GotoDefinitionResponse::Scalar(loc)) => {
            assert_eq!(
                loc.range.start.line, 0,
                "an unterminated call must still resolve its one legitimate \
                 candidate, not be wrongly excluded by a miscounted shape"
            );
        }
        other => panic!("expected a single resolved location, got: {other:?}"),
    }
}

/// The follow-up reported bug, once the self-shadow above was suppressed:
/// with the self-declaration correctly excluded, goto-definition landed on
/// nothing at all, because nothing ever tried "this bare call is really
/// `this.collect(...)` against the enclosing extension function's own
/// receiver" — the real target here is `Flow`'s own JAR-indexed interface
/// member, reachable only via implicit-receiver resolution (Kotlin SAM-
/// converts `block` to `FlowCollector<T>`). Exercises the whole path end to
/// end through `find_definition`, not `resolve_implicit_receiver_callee`
/// directly.
#[tokio::test]
async fn goto_definition_resolves_implicit_receiver_call_to_jar_member() {
    use crate::types::{FileData, SourceSet, SymbolEntry, Visibility};
    use std::sync::Arc;

    let idx = Indexer::new();
    let uri = Url::parse("file:///t/Flow.kt").unwrap();
    let src = "package com.example\n\
               import kotlinx.coroutines.flow.Flow\n\
               class CoroutineScope\n\
               fun <T : Any> Flow<T>.collect(scope: CoroutineScope, block: (T) -> Unit) {\n\
                   collect(block)\n\
               }\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);

    let jar_uri_str = "jar:file:///fake-coroutines.jar!/Flow.kt".to_string();
    let jar_uri = Url::parse(&jar_uri_str).unwrap();
    let range = tower_lsp::lsp_types::Range {
        start: Position::new(0, 0),
        end: Position::new(0, 7),
    };
    let member = SymbolEntry {
        name: "collect".to_owned(),
        kind: tower_lsp::lsp_types::SymbolKind::METHOD,
        visibility: Visibility::Public,
        range,
        selection_range: range,
        detail: "suspend fun collect(collector: FlowCollector<T>)".to_owned(),
        container: Some("Flow".to_owned()),
        params: "collector: FlowCollector<T>".to_owned(),
        param_counts: (1, 1),
        cold: crate::types::pack_cold_fields(vec![], String::new(), String::new(), String::new()),
        trailing_lambda: false,
        deprecated: false,
    };
    let flow_type = SymbolEntry {
        name: "Flow".to_owned(),
        kind: tower_lsp::lsp_types::SymbolKind::INTERFACE,
        visibility: Visibility::Public,
        range,
        selection_range: range,
        detail: "interface Flow<T>".to_owned(),
        container: None,
        params: String::new(),
        param_counts: (0, 0),
        cold: crate::types::pack_cold_fields(vec![], String::new(), String::new(), String::new()),
        trailing_lambda: false,
        deprecated: false,
    };
    idx.jar_files.insert(
        jar_uri_str.clone(),
        Arc::new(FileData {
            symbols: vec![flow_type, member],
            source_set: SourceSet::Library,
            package: Some("kotlinx.coroutines.flow".to_owned()),
            lines: Arc::new(vec![]),
            ..Default::default()
        }),
    );
    idx.jar_definitions
        .entry("Flow".to_owned())
        .or_default()
        .push(tower_lsp::lsp_types::Location {
            uri: jar_uri.clone(),
            range,
        });

    let col = src.lines().nth(4).unwrap().find("collect").unwrap() as u32;
    let position = Position::new(4, col);
    let ctx = CursorContext::build(&idx, &uri, position).unwrap();
    let response = find_definition(&ctx, &idx, &uri, position).await;
    let loc = match response {
        Some(GotoDefinitionResponse::Scalar(loc)) => loc,
        Some(GotoDefinitionResponse::Array(mut locs)) if locs.len() == 1 => locs.remove(0),
        other => panic!("expected exactly one resolved location, got: {other:?}"),
    };
    assert_eq!(
        loc.uri, jar_uri,
        "must resolve to Flow's own JAR-indexed collect member via the \
         implicit `this` receiver, not the arity-incompatible self-declaration, \
         got: {loc:?}"
    );
}

/// A second, distinct manifestation of the same self-shadow bug, reported
/// after the implicit-receiver fix above shipped: an *explicit*-receiver call
/// (`triggers.collect { trigger -> }`, trailing-lambda-only — no `scope` arg
/// at all) goes through a completely different pipeline
/// (`classify_cursor`/`resolve_identity`'s CST-resolved path, not
/// `call_shape_at_callee`/`find_definition_for_call`) that had no arity
/// awareness whatsoever: `resolve_identity` resolved the receiver's type
/// (`Flow`) and searched extensions-in-scope on it completely unfiltered,
/// finding the arity-incompatible self-declaration before ever considering
/// the real member.
#[tokio::test]
async fn goto_definition_resolves_explicit_receiver_call_to_jar_member_not_self() {
    use crate::types::{FileData, SourceSet, SymbolEntry, Visibility};
    use std::sync::Arc;

    let idx = Indexer::new();
    let uri = Url::parse("file:///t/Flow.kt").unwrap();
    let src = "package com.example\n\
               import kotlinx.coroutines.flow.Flow\n\
               class CoroutineScope\n\
               fun <T : Any> Flow<T>.collect(scope: CoroutineScope, block: (T) -> Unit) {\n\
                   collect(block)\n\
               }\n\
               fun useTriggers(triggers: Flow<String>) {\n\
                   triggers.collect { trigger -> println(trigger) }\n\
               }\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);

    let jar_uri_str = "jar:file:///fake-coroutines.jar!/Flow.kt".to_string();
    let jar_uri = Url::parse(&jar_uri_str).unwrap();
    let type_range = tower_lsp::lsp_types::Range {
        start: Position::new(0, 0),
        end: Position::new(0, 4),
    };
    let member_range = tower_lsp::lsp_types::Range {
        start: Position::new(1, 0),
        end: Position::new(1, 7),
    };
    let member = SymbolEntry {
        name: "collect".to_owned(),
        kind: tower_lsp::lsp_types::SymbolKind::METHOD,
        visibility: Visibility::Public,
        range: member_range,
        selection_range: member_range,
        detail: "suspend fun collect(collector: FlowCollector<T>)".to_owned(),
        container: Some("Flow".to_owned()),
        params: "collector: FlowCollector<T>".to_owned(),
        param_counts: (1, 1),
        cold: crate::types::pack_cold_fields(vec![], String::new(), String::new(), String::new()),
        trailing_lambda: false,
        deprecated: false,
    };
    let flow_type = SymbolEntry {
        name: "Flow".to_owned(),
        kind: tower_lsp::lsp_types::SymbolKind::INTERFACE,
        visibility: Visibility::Public,
        range: type_range,
        selection_range: type_range,
        detail: "interface Flow<T>".to_owned(),
        container: None,
        params: String::new(),
        param_counts: (0, 0),
        cold: crate::types::pack_cold_fields(vec![], String::new(), String::new(), String::new()),
        trailing_lambda: false,
        deprecated: false,
    };
    idx.jar_files.insert(
        jar_uri_str.clone(),
        Arc::new(FileData {
            symbols: vec![flow_type, member],
            source_set: SourceSet::Library,
            package: Some("kotlinx.coroutines.flow".to_owned()),
            lines: Arc::new(vec![]),
            ..Default::default()
        }),
    );
    idx.jar_definitions
        .entry("Flow".to_owned())
        .or_default()
        .push(tower_lsp::lsp_types::Location {
            uri: jar_uri.clone(),
            range: type_range,
        });

    let col = src.lines().nth(7).unwrap().find("collect").unwrap() as u32;
    let position = Position::new(7, col);
    let ctx = CursorContext::build(&idx, &uri, position).unwrap();
    let response = find_definition(&ctx, &idx, &uri, position).await;
    let loc = match response {
        Some(GotoDefinitionResponse::Scalar(loc)) => loc,
        Some(GotoDefinitionResponse::Array(mut locs)) if locs.len() == 1 => locs.remove(0),
        other => panic!("expected exactly one resolved location, got: {other:?}"),
    };
    assert_eq!(
        loc.uri, jar_uri,
        "must resolve `triggers.collect {{ trigger -> }}` to Flow's own \
         JAR-indexed collect member, not the arity-incompatible \
         collect(scope, block) self-declaration, got: {loc:?}"
    );
}

/// Real Moneta bug (`navController.navigate(route = ...)`, `NavHostController`
/// extends `NavController`): the receiver's CONCRETE type declares its own
/// same-named member with a DIFFERENT, incompatible arity (`act()`, no args —
/// standing in for `NavController.navigate(Uri)`/`navigate(NavDirections)`),
/// while the call actually wants a same-named extension declared on an
/// ancestor type (`Base.act(label: String)`, standing in for the Kotlin KTX
/// `NavController.navigate(route: String, ...)` extension). Before the fix,
/// `resolve_qualified` returned the wrong-arity member alone and never even
/// looked at the supertype-walk extension fallback, so — once the caller's
/// arity filter (correctly) rejected that member — resolution came up empty
/// entirely instead of falling through to the extension. The receiver here is
/// a plain lowercase local parameter (`d: Derived`), exercising both the
/// CST-resolved path (`classify_cursor`/`resolve_identity`) AND `find_definition`'s
/// own final string-qualifier fallback, since both now need the appended
/// extension candidate to pick the right one via shape filtering.
#[tokio::test]
async fn goto_definition_prefers_supertype_extension_over_wrong_arity_concrete_member() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/Nav.kt").unwrap();
    let src = "open class Base\n\
               fun Base.act(label: String): Unit = TODO()\n\
               class Derived : Base() {\n\
                   fun act(): Unit = TODO()\n\
               }\n\
               fun f(d: Derived) {\n\
                   d.act(\"x\")\n\
               }\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);

    let col = src.lines().nth(6).unwrap().find("act").unwrap() as u32;
    let position = Position::new(6, col);
    let ctx = CursorContext::build(&idx, &uri, position).unwrap();
    let response = find_definition(&ctx, &idx, &uri, position).await;
    let loc = match response {
        Some(GotoDefinitionResponse::Scalar(loc)) => loc,
        Some(GotoDefinitionResponse::Array(mut locs)) if locs.len() == 1 => locs.remove(0),
        other => panic!("expected exactly one resolved location, got: {other:?}"),
    };
    assert_eq!(
        loc.range.start.line, 1,
        "d.act(\"x\") (1 arg) must resolve to Base's extension act(label: \
         String) on line 1, not Derived's own 0-arg act() on line 3, got: {loc:?}"
    );
}

/// Definition lines of `find_definition` at `needle`'s first occurrence on
/// `line` of `uri`'s source.
async fn definition_lines(
    idx: &Indexer,
    uri: &Url,
    src: &str,
    line: u32,
    needle: &str,
) -> Vec<u32> {
    let col = src
        .lines()
        .nth(line as usize)
        .unwrap()
        .find(needle)
        .unwrap() as u32;
    let position = Position::new(line, col);
    let ctx = CursorContext::build(idx, uri, position).unwrap();
    let mut lines: Vec<u32> = match find_definition(&ctx, idx, uri, position).await {
        Some(GotoDefinitionResponse::Scalar(location)) => vec![location.range.start.line],
        Some(GotoDefinitionResponse::Array(locations)) => locations
            .iter()
            .map(|location| location.range.start.line)
            .collect(),
        Some(GotoDefinitionResponse::Link(links)) => links
            .iter()
            .map(|link| link.target_selection_range.start.line)
            .collect(),
        None => Vec::new(),
    };
    lines.sort_unstable();
    lines
}

/// Kotlin overloads called from Java: the argument's type picks the one,
/// as javac would (`writer.value(9.0)` → `value(Double)`).
#[tokio::test]
async fn java_call_picks_the_overload_its_argument_types_fit() {
    let idx = Indexer::new();
    let writer_uri = Url::parse("file:///t/Writer.kt").unwrap();
    let writer = "class Writer {\n\
                  \x20 fun value(value: String?): Writer = this\n\
                  \x20 fun value(value: Boolean): Writer = this\n\
                  \x20 fun value(value: Boolean?): Writer = this\n\
                  \x20 fun value(value: Double): Writer = this\n\
                  \x20 fun value(value: Long): Writer = this\n\
                  \x20 fun value(value: Number?): Writer = this\n\
                  }\n";
    idx.index_content(&writer_uri, writer);
    let caller_uri = Url::parse("file:///t/Caller.java").unwrap();
    let caller = "class Caller {\n\
                  \x20 void run(Writer writer, Integer boxed) {\n\
                  \x20   writer.value(9.0);\n\
                  \x20   writer.value(\"two\");\n\
                  \x20   writer.value(true);\n\
                  \x20   writer.value(3);\n\
                  \x20   writer.value(boxed);\n\
                  \x20   writer.value(null);\n\
                  \x20 }\n\
                  }\n";
    idx.index_content(&caller_uri, caller);
    idx.store_live_tree(&caller_uri, caller);

    assert_eq!(
        definition_lines(&idx, &caller_uri, caller, 2, "value").await,
        vec![4]
    );
    assert_eq!(
        definition_lines(&idx, &caller_uri, caller, 3, "value").await,
        vec![1]
    );
    // `Boolean` and `Boolean?` are one Java signature: the first is kept.
    assert_eq!(
        definition_lines(&idx, &caller_uri, caller, 4, "value").await,
        vec![2]
    );
    // `int` widens to `long` (and not to `Double`'s `double` first: `long`
    // is the narrower widening — both fit, and so does `Number`).
    let widened = definition_lines(&idx, &caller_uri, caller, 5, "value").await;
    assert!(
        widened.contains(&5) && !widened.contains(&1),
        "got {widened:?}"
    );
    // An unknown-to-overloads argument type rules out nothing but String/Boolean/…
    let boxed = definition_lines(&idx, &caller_uri, caller, 6, "value").await;
    assert!(!boxed.contains(&1), "an Integer is not a String: {boxed:?}");
    // `null` fits every reference parameter, no primitive one.
    let null = definition_lines(&idx, &caller_uri, caller, 7, "value").await;
    assert!(null.contains(&1) && !null.contains(&4), "got {null:?}");
}

/// `new Writer(..)` goes to the constructor the arguments pick, as javac
/// (and jdtls) would; a class that declares none keeps the class itself.
#[tokio::test]
async fn java_object_creation_goes_to_the_constructor_its_arguments_fit() {
    let idx = Indexer::new();
    let writer_uri = Url::parse("file:///t/Writer.java").unwrap();
    let writer = "package t;\n\
                  public class Writer {\n\
                  \x20 public Writer(String name) {}\n\
                  \x20 public Writer(int size) {}\n\
                  \x20 protected Writer() {}\n\
                  \x20 static class Nested {\n\
                  \x20   Nested(String name) {}\n\
                  \x20 }\n\
                  }\n";
    idx.index_content(&writer_uri, writer);
    let plain_uri = Url::parse("file:///t/Plain.java").unwrap();
    let plain = "package t;\n\
                 public class Plain {\n\
                 \x20 void run() {}\n\
                 }\n";
    idx.index_content(&plain_uri, plain);
    let token_uri = Url::parse("file:///t/Token.java").unwrap();
    let token = "package t;\n\
                 public class Token<T> {\n\
                 \x20 Token(Class<T> type) {}\n\
                 \x20 protected Token() {}\n\
                 }\n";
    idx.index_content(&token_uri, token);
    let caller_uri = Url::parse("file:///t/Caller.java").unwrap();
    let caller = "package t;\n\
                  class Caller {\n\
                  \x20 void run() {\n\
                  \x20   Writer named = new Writer(\"a\");\n\
                  \x20   Writer sized = new Writer(3);\n\
                  \x20   Writer anonymous = new Writer() {};\n\
                  \x20   Plain plain = new Plain();\n\
                  \x20   Object token = new Token<String>() {};\n\
                  \x20 }\n\
                  }\n";
    idx.index_content(&caller_uri, caller);
    idx.store_live_tree(&caller_uri, caller);

    assert_eq!(
        definition_lines(&idx, &caller_uri, caller, 3, "Writer(").await,
        vec![2]
    );
    assert_eq!(
        definition_lines(&idx, &caller_uri, caller, 4, "Writer(").await,
        vec![3]
    );
    assert_eq!(
        definition_lines(&idx, &caller_uri, caller, 5, "Writer(").await,
        vec![4]
    );
    // The declared type of the local is a type reference, not a creation.
    assert_eq!(
        definition_lines(&idx, &caller_uri, caller, 3, "Writer").await,
        vec![1]
    );
    assert_eq!(
        definition_lines(&idx, &caller_uri, caller, 6, "Plain(").await,
        vec![1]
    );
    assert_eq!(
        definition_lines(&idx, &caller_uri, caller, 7, "Token<").await,
        vec![3]
    );
}

/// A statically imported overloaded call: the qualified lookup finds one
/// overload; the argument picks the right sibling.
#[tokio::test]
async fn java_static_import_call_picks_the_overload_its_argument_fits() {
    let idx = Indexer::new();
    let truth_uri = Url::parse("file:///t/com/example/truth/Truth.java").unwrap();
    let truth = "package com.example.truth;\n\
                 public final class Truth {\n\
                 \x20 public static Subject assertThat(Object actual) { return null; }\n\
                 \x20 public static StringSubject assertThat(String actual) { return null; }\n\
                 \x20 public static BooleanSubject assertThat(boolean actual) { return null; }\n\
                 }\n";
    idx.index_content(&truth_uri, truth);
    let caller_uri = Url::parse("file:///t/A.java").unwrap();
    let caller = "import static com.example.truth.Truth.assertThat;\n\
                  class A {\n\
                  \x20 void f() {\n\
                  \x20   assertThat(\"s\");\n\
                  \x20   assertThat(false);\n\
                  \x20 }\n\
                  }\n";
    idx.index_content(&caller_uri, caller);
    idx.store_live_tree(&caller_uri, caller);
    assert_eq!(
        definition_lines(&idx, &caller_uri, caller, 3, "assertThat").await,
        vec![3]
    );
    assert_eq!(
        definition_lines(&idx, &caller_uri, caller, 4, "assertThat").await,
        vec![4]
    );
}

/// `StringSubject` inherits `isEqualTo` from `Subject`; its nested
/// `CaseInsensitiveStringComparison` declares its own. A call on a
/// `StringSubject` must reach `Subject.isEqualTo`, not the nested class's.
#[tokio::test]
async fn java_member_of_a_class_is_not_its_nested_classs_member() {
    let idx = Indexer::new();
    let subject_uri = Url::parse("file:///t/Subject.java").unwrap();
    let subject = "public class Subject {\n\
                   \x20 public void isEqualTo(Object expected) {}\n\
                   }\n";
    idx.index_content(&subject_uri, subject);
    let string_subject_uri = Url::parse("file:///t/StringSubject.java").unwrap();
    let string_subject = "public class StringSubject extends Subject {\n\
                          \x20 public void contains(String part) {}\n\
                          \x20 public final class CaseInsensitiveStringComparison {\n\
                          \x20   public void isEqualTo(String expected) {}\n\
                          \x20 }\n\
                          }\n";
    idx.index_content(&string_subject_uri, string_subject);
    let caller_uri = Url::parse("file:///t/A.java").unwrap();
    let caller = "class A {\n\
                  \x20 void f(StringSubject subject) {\n\
                  \x20   subject.isEqualTo(\"a\");\n\
                  \x20 }\n\
                  }\n";
    idx.index_content(&caller_uri, caller);
    idx.store_live_tree(&caller_uri, caller);
    let col = caller.lines().nth(2).unwrap().find("isEqualTo").unwrap() as u32;
    let position = Position::new(2, col);
    let ctx = CursorContext::build(&idx, &caller_uri, position).unwrap();
    let uris: Vec<(String, u32)> = match find_definition(&ctx, &idx, &caller_uri, position).await {
        Some(GotoDefinitionResponse::Scalar(location)) => {
            vec![(location.uri.path().to_owned(), location.range.start.line)]
        }
        Some(GotoDefinitionResponse::Array(locations)) => locations
            .iter()
            .map(|location| (location.uri.path().to_owned(), location.range.start.line))
            .collect(),
        _ => Vec::new(),
    };
    assert_eq!(uris, vec![("/t/Subject.java".to_owned(), 1)]);
}

/// `assertThat(e)` on an exception picks `assertThat(Throwable)` — more
/// specific than `assertThat(Object)` — so the chain continues on its
/// `ThrowableSubject`.
#[tokio::test]
async fn java_overload_prefers_the_parameter_the_argument_class_extends() {
    let idx = Indexer::new();
    let files = [
        (
            "file:///t/com/example/truth/Subject.java",
            "package com.example.truth;\n\
             public class Subject {\n\
             \x20 public void isEqualTo(Object expected) {}\n\
             }\n",
        ),
        (
            "file:///t/com/example/truth/ThrowableSubject.java",
            "package com.example.truth;\n\
             public class ThrowableSubject extends Subject {\n\
             \x20 public StringSubject hasMessageThat() { return null; }\n\
             }\n",
        ),
        (
            "file:///t/com/example/truth/Truth.java",
            "package com.example.truth;\n\
             public final class Truth {\n\
             \x20 public static Subject assertThat(Object actual) { return null; }\n\
             \x20 public static ThrowableSubject assertThat(Throwable actual) { return null; }\n\
             }\n",
        ),
        (
            "file:///t/com/example/app/Failure.java",
            "package com.example.app;\n\
             public class Failure extends Problem {}\n",
        ),
        (
            "file:///t/java/lang/Throwable.java",
            "package java.lang;\n\
             public class Throwable {}\n",
        ),
        (
            "file:///t/com/example/app/Problem.java",
            "package com.example.app;\n\
             public class Problem extends Throwable {}\n",
        ),
    ];
    for (uri, text) in files {
        idx.index_content(&Url::parse(uri).unwrap(), text);
    }
    let caller_uri = Url::parse("file:///t/com/example/app/A.java").unwrap();
    let caller = "package com.example.app;\n\
                  import static com.example.truth.Truth.assertThat;\n\
                  class A {\n\
                  \x20 void f(Failure e) {\n\
                  \x20   assertThat(e).hasMessageThat();\n\
                  \x20 }\n\
                  }\n";
    idx.index_content(&caller_uri, caller);
    idx.store_live_tree(&caller_uri, caller);
    assert_eq!(
        definition_lines(&idx, &caller_uri, caller, 4, "assertThat").await,
        vec![3]
    );
    assert_eq!(
        definition_lines(&idx, &caller_uri, caller, 4, "hasMessageThat").await,
        vec![2]
    );
}

/// A Java type parameter goes to its declaration in the enclosing class or
/// method — never to a same-named class elsewhere.
#[tokio::test]
async fn java_type_parameter_goes_to_its_declaration() {
    let idx = Indexer::new();
    let elsewhere_uri = Url::parse("file:///t/other/V.java").unwrap();
    idx.index_content(&elsewhere_uri, "package other;\npublic class V {}\n");
    let box_uri = Url::parse("file:///t/app/Box.java").unwrap();
    let source = "package app;\n\
                  public class Box<K, V extends Comparable<V>> {\n\
                  \x20 Node<K, V> root;\n\
                  \x20 <V> V swap(V value) {\n\
                  \x20   return value;\n\
                  \x20 }\n\
                  \x20 void put(K key, V value) {\n\
                  \x20   Node<K, V> left = root;\n\
                  \x20 }\n\
                  }\n";
    idx.index_content(&box_uri, source);
    idx.store_live_tree(&box_uri, source);
    // Field and local types: the class's own `V` (line 1).
    assert_eq!(
        definition_lines(&idx, &box_uri, source, 2, "V>").await,
        vec![1]
    );
    assert_eq!(
        definition_lines(&idx, &box_uri, source, 7, "V>").await,
        vec![1]
    );
    // The bound's own `V` refers to the parameter it bounds.
    assert_eq!(
        definition_lines(&idx, &box_uri, source, 1, "V>>").await,
        vec![1]
    );
    // The method's `<V>` shadows the class's (line 3).
    assert_eq!(
        definition_lines(&idx, &box_uri, source, 3, "V value").await,
        vec![3]
    );
}

/// A Java local or parameter is found by its scope, never by name: a
/// same-named method, a field it shadows and a later, inner-block local
/// of the same name are all decoys.
#[tokio::test]
async fn java_local_and_parameter_go_to_their_own_declarations() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/Box.java").unwrap();
    let source = "class Box {\n\
                  \x20 Object engine;\n\
                  \x20 Object engine() { return null; }\n\
                  \x20 void run(Object engine) {\n\
                  \x20   Object first = engine;\n\
                  \x20   {\n\
                  \x20     Object engine2 = first;\n\
                  \x20   }\n\
                  \x20   Object again = first;\n\
                  \x20 }\n\
                  }\n";
    idx.index_content(&uri, source);
    idx.store_live_tree(&uri, source);
    // The parameter (line 3), not the field (1) or the method (2).
    assert_eq!(
        definition_lines(&idx, &uri, source, 4, "engine;").await,
        vec![3]
    );
    // The local declared on line 4.
    assert_eq!(
        definition_lines(&idx, &uri, source, 6, "first").await,
        vec![4]
    );
    assert_eq!(
        definition_lines(&idx, &uri, source, 8, "first").await,
        vec![4]
    );
}

/// A bare call inside a class binds to the class's own method (`this.read`),
/// never to a same-named method of a class nested in it: `TypeAdapter.fromJson`
/// calls its own abstract `read`, not `NullSafeTypeAdapter.read` declared
/// further down the file. `TypeAdapter.this.read(..)` inside the nested class
/// reaches the outer one too.
#[tokio::test]
async fn goto_definition_java_bare_call_binds_to_enclosing_class_not_nested_class() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/TypeAdapter.java").unwrap();
    let src = "public abstract class TypeAdapter<T> {\n\
               \x20 public abstract T read(JsonReader in) throws IOException;\n\
               \x20 public final T fromJson(Reader in) throws IOException {\n\
               \x20   JsonReader reader = new JsonReader(in);\n\
               \x20   return read(reader);\n\
               \x20 }\n\
               \x20 private final class NullSafeTypeAdapter extends TypeAdapter<T> {\n\
               \x20   @Override\n\
               \x20   public T read(JsonReader reader) throws IOException {\n\
               \x20     return TypeAdapter.this.read(reader);\n\
               \x20   }\n\
               \x20 }\n\
               }\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);

    for (line, expected_line) in [(4u32, 1u32), (9, 1)] {
        let text = src.lines().nth(line as usize).unwrap();
        let col = text.rfind("read(").unwrap() as u32;
        let position = Position::new(line, col);
        let ctx = CursorContext::build(&idx, &uri, position).unwrap();
        let response = find_definition(&ctx, &idx, &uri, position).await;
        let lines: Vec<u32> = match response {
            Some(GotoDefinitionResponse::Scalar(location)) => vec![location.range.start.line],
            Some(GotoDefinitionResponse::Array(locations)) => locations
                .iter()
                .map(|location| location.range.start.line)
                .collect(),
            other => panic!("line {line}: expected a location, got {other:?}"),
        };
        assert_eq!(lines, vec![expected_line], "`read` on line {line}");
    }
}

/// An element of an array has the array's element type: `users[0].save()`
/// is `User.save`, not the same-named `Admin.save`.
#[tokio::test]
async fn goto_definition_java_call_on_array_element_uses_element_type() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/Users.java").unwrap();
    let src = "class User { void save() {} }\n\
               class Admin { void save() {} }\n\
               class Users {\n\
               \x20 void run(User[] users) {\n\
               \x20   users[0].save();\n\
               \x20 }\n\
               }\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);
    let col = src.lines().nth(4).unwrap().find("save").unwrap() as u32;
    let position = Position::new(4, col);
    let ctx = CursorContext::build(&idx, &uri, position).unwrap();
    let response = find_definition(&ctx, &idx, &uri, position).await;
    let lines: Vec<u32> = match response {
        Some(GotoDefinitionResponse::Scalar(location)) => vec![location.range.start.line],
        Some(GotoDefinitionResponse::Array(locations)) => locations
            .iter()
            .map(|location| location.range.start.line)
            .collect(),
        other => panic!("expected a location, got {other:?}"),
    };
    assert_eq!(lines, vec![0], "must jump to User.save");
}
