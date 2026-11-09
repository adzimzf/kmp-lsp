use super::{derive_dot_receiver, CompletionContext, LambdaScope, ScopeContext};
use crate::indexer::Indexer;
use crate::resolver::complete::DotReceiver;
use tower_lsp::lsp_types::{Position, Url};

#[test]
fn scope_resolve_it_returns_innermost_it_type() {
    let scope = ScopeContext {
        enclosing_class: None,
        lambda_scopes: vec![
            LambdaScope {
                it_type: Some("OuterType".into()),
                named_params: vec![],
                label: Some("map".into()),
            },
            LambdaScope {
                it_type: Some("InnerType".into()),
                named_params: vec![],
                label: Some("forEach".into()),
            },
        ],
        lambda_this_type: None,
        bare_this_type: None,
    };

    assert_eq!(scope.resolve_receiver("it"), Some("InnerType"));
}

#[test]
fn scope_resolve_this_at_label() {
    let scope = ScopeContext {
        enclosing_class: Some("MyClass".into()),
        lambda_scopes: vec![LambdaScope {
            it_type: Some("Element".into()),
            named_params: vec![],
            label: Some("forEach".into()),
        }],
        lambda_this_type: None,
        bare_this_type: Some("MyClass".into()),
    };

    assert_eq!(scope.resolve_receiver("this@forEach"), Some("Element"));
    assert_eq!(scope.resolve_receiver("this@MyClass"), Some("MyClass"));
}

fn uri(path: &str) -> Url {
    Url::parse(&format!("file:///test{path}")).unwrap()
}

fn indexed_with_live(path: &str, src: &str) -> (Url, Indexer) {
    let uri = uri(path);
    let index = Indexer::new();
    index.index_content(&uri, src);
    index.store_live_tree(&uri, src);
    (uri, index)
}

fn call_paren_col(src: &str, line_no: usize, fn_name: &str) -> u32 {
    let line = src.lines().nth(line_no).expect("line out of range");
    let needle = format!("{fn_name}(");
    let pos = line
        .find(&needle)
        .unwrap_or_else(|| panic!("no `{needle}` on line"));
    (pos + needle.len()) as u32
}

#[test]
fn lambda_scope_found_beyond_backward_scan_window() {
    // The enclosing lambda opens more than 50 lines above the cursor: a
    // bounded backward text-scan never sees it, while the CST ancestor-walk
    // finds every enclosing lambda regardless of distance.
    let mut src = String::from(
        "package com.example\n\
         class Item { val price: Int = 0 }\n\
         fun main() {\n\
         \x20   val items: List<Item> = listOf()\n\
         \x20   items.forEach {\n",
    );
    for filler in 0..60 {
        src.push_str(&format!("        val filler{filler} = {filler}\n"));
    }
    src.push_str("        \n    }\n}\n");
    let (uri, index) = indexed_with_live("/FarLambda.kt", &src);
    let cursor_line = 65u32; // the blank body line, 61 lines below the `{`

    let scope = ScopeContext::build(Position::new(cursor_line, 8), &index, &uri);

    assert_eq!(scope.resolve_receiver("it"), Some("Item"));
    assert_eq!(scope.resolve_receiver("this@forEach"), Some("Item"));
}

#[test]
fn call_info_expected_name_at_first_arg() {
    let src =
        "package com.example\nfun greet(name: String, age: Int) {}\nfun main() {\n    greet()\n}\n";
    let (uri, index) = indexed_with_live("/CallInfo.kt", src);
    let position = Position::new(3, call_paren_col(src, 3, "greet"));
    let before_prefix = src.lines().nth(3).unwrap()[..position.character as usize].to_owned();

    let wants_receiver = before_prefix.trim_end().ends_with('.');
    let ctx = CompletionContext::analyse(position, &index, &uri, false, wants_receiver);

    let call_info = ctx.call_info.expect("call_info should be populated");
    assert_eq!(call_info.callee, "greet");
    assert_eq!(call_info.arg_index, 0);
    assert_eq!(call_info.expected_name.as_deref(), Some("name"));
    assert_eq!(call_info.expected_type.as_deref(), Some("String"));
}

#[test]
fn call_info_expected_name_none_when_not_in_call() {
    let src = "package com.example\nfun main() {\n    val value = 1\n    value\n}\n";
    let (uri, index) = indexed_with_live("/NoCallInfo.kt", src);
    let position = Position::new(3, 9);
    let before_prefix = src.lines().nth(3).unwrap()[..position.character as usize].to_owned();

    let wants_receiver = before_prefix.trim_end().ends_with('.');
    let ctx = CompletionContext::analyse(position, &index, &uri, false, wants_receiver);

    assert!(
        ctx.call_info.is_none(),
        "call_info should be None outside calls"
    );
}

// ─── derive_dot_receiver (CST speculative parse) ─────────────────────────────

/// Fixture with a `|` caret marking the completion cursor.
fn derive_at_caret(path: &str, src_with_caret: &str) -> Option<DotReceiver> {
    let caret = src_with_caret.find('|').expect("caret");
    let src: String = src_with_caret.replace('|', "");
    let line = src_with_caret[..caret].matches('\n').count();
    let line_start = src_with_caret[..caret].rfind('\n').map_or(0, |p| p + 1);
    let col = src_with_caret[line_start..caret].encode_utf16().count();
    let (uri, index) = indexed_with_live(path, &src);
    derive_dot_receiver(&index, &uri, Position::new(line as u32, col as u32))
}

#[test]
fn derives_a_simple_identifier_receiver_with_no_early_resolution() {
    let recv = derive_at_caret(
        "/SimpleRecv.kt",
        "class User\nfun f() {\n    val user = User()\n    user.|\n}\n",
    )
    .unwrap();
    assert_eq!(
        recv,
        DotReceiver::Expr {
            text: "user".into(),
            is_call: false,
            resolved: None
        }
    );
}

#[test]
fn chain_receiver_resolves_at_analysis_time() {
    let recv = derive_at_caret(
        "/ChainRecv.kt",
        "package com.example\n\
         class Palette { fun swap() {} }\n\
         class Theme { val colors: Palette = Palette() }\n\
         fun f() {\n\
         \x20   val theme = Theme()\n\
         \x20   theme.colors.|\n\
         }\n",
    )
    .unwrap();
    match recv {
        DotReceiver::Expr {
            is_call: false,
            resolved: Some(resolved_type),
            ..
        } => assert_eq!(resolved_type, "Palette"),
        other => panic!("expected resolved chain receiver, got {other:?}"),
    }
}

#[test]
fn derives_a_call_receiver_with_callee_text() {
    let recv = derive_at_caret("/CallRecv.kt", "fun f() {\n    productFlow(x).|\n}\n").unwrap();
    match recv {
        DotReceiver::Expr {
            text,
            is_call: true,
            ..
        } => assert_eq!(text, "productFlow"),
        other => panic!("expected call receiver, got {other:?}"),
    }
}

#[test]
fn classifies_scope_receivers() {
    let it_recv = derive_at_caret("/It.kt", "fun f() { items.map { it.| } }\n").unwrap();
    assert_eq!(it_recv, DotReceiver::Scope("it".into()));

    let labeled = derive_at_caret(
        "/Labeled.kt",
        "fun f() { items.forEach { this@forEach.| } }\n",
    )
    .unwrap();
    assert_eq!(labeled, DotReceiver::Scope("this@forEach".into()));

    let bare_this = derive_at_caret("/This.kt", "class A { fun f() { this.| } }\n").unwrap();
    assert_eq!(bare_this, DotReceiver::Scope("this".into()));
}

#[test]
fn classifies_a_super_receiver() {
    let recv = derive_at_caret("/Super.kt", "class A { fun f() { super.| } }\n").unwrap();
    assert_eq!(recv, DotReceiver::Super);
}

#[test]
fn multiline_fluent_chain_derives_a_call_receiver() {
    let recv = derive_at_caret(
        "/Fluent.kt",
        "fun f() {\n\
         \x20   val m = Modifier\n\
         \x20       .fillMaxSize()\n\
         \x20       .|\n\
         }\n",
    )
    .unwrap();
    match recv {
        DotReceiver::Expr {
            text,
            is_call: true,
            ..
        } => {
            assert!(text.contains("Modifier"), "text: {text}");
            assert!(text.contains("fillMaxSize"), "text: {text}");
        }
        other => panic!("expected multiline chain receiver, got {other:?}"),
    }
}

#[test]
fn no_receiver_for_bare_word_or_string_interior() {
    assert!(derive_at_caret("/Bare.kt", "fun f() { Modif| }\n").is_none());
    assert!(derive_at_caret("/Str.kt", "fun f() { val s = \"foo.|\" }\n").is_none());
}

// ─── repair-wired scope walk ─────────────────────────────────────────────────

#[test]
fn scope_walk_survives_an_unclosed_lambda() {
    // Lambda AND function braces both unclosed: the broken tree forms no
    // lambda_literal, so without brace repair the scope stack comes back
    // empty in exactly the mid-typing states completion cares about.
    let src = "package com.example\n\
               class Item { val price: Int = 0 }\n\
               fun main(items: List<Item>) {\n\
               \x20   items.forEach {\n\
               \x20       \n";
    let (uri, index) = indexed_with_live("/Unclosed.kt", src);
    let scope = ScopeContext::build(Position::new(4, 8), &index, &uri);
    assert_eq!(
        scope.resolve_receiver("it"),
        Some("Item"),
        "brace repair must recover the lambda scope stack mid-typing"
    );
}

// ─── Java receivers ──────────────────────────────────────────────────────────

#[test]
fn java_local_receiver_is_typed_from_its_declaration() {
    // A same-named Kotlin-style local elsewhere must not leak in: the Java
    // tree's own binding decides.
    let recv = derive_at_caret(
        "/JavaLocal.java",
        "class Reader { int peek() { return 0; } }\n\
         class Writer { void flush() {} }\n\
         class A {\n\
         \x20 Writer reader;\n\
         \x20 void f() {\n\
         \x20   Reader reader = new Reader();\n\
         \x20   reader.|\n\
         \x20 }\n\
         }\n",
    )
    .unwrap();
    assert_eq!(
        recv,
        DotReceiver::Expr {
            text: "reader".into(),
            is_call: false,
            resolved: Some("Reader".into()),
        }
    );
}

#[test]
fn java_receiver_before_an_argument_list_is_found() {
    // The query deletes `peek` from `reader.peek()`, leaving `reader.()`.
    let recv = derive_at_caret(
        "/JavaCallSite.java",
        "class Reader { int peek() { return 0; } }\n\
         class A {\n\
         \x20 void f(Reader reader) {\n\
         \x20   int x = reader.|();\n\
         \x20 }\n\
         }\n",
    )
    .unwrap();
    assert_eq!(
        recv,
        DotReceiver::Expr {
            text: "reader".into(),
            is_call: false,
            resolved: Some("Reader".into()),
        }
    );
}

#[test]
fn java_receiver_without_semicolon_before_a_declaration_is_found() {
    // `reader.` then a declaration on the next line parses as the qualified
    // type `reader.<marker>` of that declaration.
    let recv = derive_at_caret(
        "/JavaNoSemicolon.java",
        "class Reader { int peek() { return 0; } }\n\
         class A {\n\
         \x20 void f(Reader reader) {\n\
         \x20   reader.|\n\
         \x20   int y = 1;\n\
         \x20 }\n\
         }\n",
    )
    .unwrap();
    match recv {
        DotReceiver::Expr { text, resolved, .. } => {
            assert_eq!(text, "reader");
            assert_eq!(resolved.as_deref(), Some("Reader"));
        }
        other => panic!("expected an expression receiver, got {other:?}"),
    }
}

#[test]
fn java_call_and_super_receivers() {
    let call = derive_at_caret(
        "/JavaCall.java",
        "class Reader {\n\
         \x20 Reader next() { return this; }\n\
         \x20 int peek() { return 0; }\n\
         }\n\
         class A {\n\
         \x20 void f(Reader reader) {\n\
         \x20   reader.next().|;\n\
         \x20 }\n\
         }\n",
    )
    .unwrap();
    match call {
        DotReceiver::Expr {
            text,
            is_call: true,
            resolved,
        } => {
            assert_eq!(text, "next");
            assert_eq!(resolved.as_deref(), Some("Reader"));
        }
        other => panic!("expected a call receiver, got {other:?}"),
    }

    let super_receiver = derive_at_caret(
        "/JavaSuper.java",
        "class A extends B { void f() { super.|; } }\n",
    )
    .unwrap();
    assert_eq!(super_receiver, DotReceiver::Super);
}

#[test]
fn java_bare_word_has_no_receiver() {
    assert!(derive_at_caret("/JavaBare.java", "class A { void f() { rea| } }\n").is_none());
}

#[test]
fn java_receiver_inside_an_argument_list_is_found() {
    let source = "class Reader { int peek() { return 0; } }\n\
                  class A {\n\
                  \x20 void f(Reader reader) {\n\
                  \x20   assertThat(reader.|).isEqualTo(1);\n\
                  \x20 }\n\
                  }\n";
    let recv = derive_at_caret("/JavaArgument.java", source).unwrap();
    assert_eq!(
        recv,
        DotReceiver::Expr {
            text: "reader".into(),
            is_call: false,
            resolved: Some("Reader".into()),
        }
    );
    let second_argument = source.replace("assertThat(reader.|)", "assertEquals(5, reader.|())");
    let recv = derive_at_caret("/JavaArgument2.java", &second_argument).unwrap();
    assert!(
        matches!(recv, DotReceiver::Expr { ref text, .. } if text == "reader"),
        "{recv:?}"
    );
}

#[test]
fn java_receiver_is_found_when_completing_inside_an_existing_name() {
    let recv = derive_at_caret(
        "/JavaInsideName.java",
        "class Reader { int peek() { return 0; } }\n\
         class A {\n\
         \x20 void f(Reader reader) {\n\
         \x20   assertThat(reader.|peek()).isEqualTo(1);\n\
         \x20 }\n\
         }\n",
    )
    .unwrap();
    assert!(
        matches!(recv, DotReceiver::Expr { ref text, resolved: Some(ref resolved), .. } if text == "reader" && resolved == "Reader"),
        "{recv:?}"
    );
}

#[test]
fn java_statically_imported_overload_types_the_receiver_by_its_argument() {
    let index = Indexer::new();
    let truth_uri = uri("/com/example/truth/Truth.java");
    let truth = "package com.example.truth;\n\
                 public final class Truth {\n\
                 \x20 public static Subject assertThat(Object actual) { return null; }\n\
                 \x20 public static StringSubject assertThat(String actual) { return null; }\n\
                 \x20 public static BooleanSubject assertThat(boolean actual) { return null; }\n\
                 }\n";
    index.index_content(&truth_uri, truth);
    let caller = "import static com.example.truth.Truth.assertThat;\n\
                  class A {\n\
                  \x20 void f(Reader reader) {\n\
                  \x20   assertThat(true).|\n\
                  \x20   assertThat(\"s\").isEqualTo(\"s\");\n\
                  \x20 }\n\
                  }\n";
    let caret = caller.find('|').unwrap();
    let src = caller.replace('|', "");
    let line = caller[..caret].matches('\n').count();
    let col = caret - caller[..caret].rfind('\n').unwrap() - 1;
    let caller_uri = uri("/A.java");
    index.index_content(&caller_uri, &src);
    index.store_live_tree(&caller_uri, &src);
    let recv = derive_dot_receiver(&index, &caller_uri, Position::new(line as u32, col as u32));
    assert!(
        matches!(recv, Some(DotReceiver::Expr { resolved: Some(ref resolved), .. }) if resolved == "BooleanSubject"),
        "{recv:?}"
    );
}
