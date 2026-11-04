use tower_lsp::lsp_types::SymbolKind;

use crate::language::java::JavaParser;
use crate::language::LanguageParser;

#[test]
fn language_id_is_java() {
    assert_eq!(JavaParser.language_id(), "java");
}

#[test]
fn extensions_include_java() {
    assert!(JavaParser.file_extensions().contains(&"java"));
}

#[test]
fn parse_extracts_class_symbol() {
    let data = JavaParser.parse("public class Bar {}");
    assert!(data.symbols.iter().any(|s| s.name == "Bar"));
}

#[test]
fn symbol_keyword_function_is_fun() {
    // Java provider inherits Kotlin-style keywords; Swift is the only deviation.
    assert_eq!(JavaParser.symbol_keyword(SymbolKind::FUNCTION), "fun");
}

#[test]
fn symbol_keyword_class_is_class() {
    assert_eq!(JavaParser.symbol_keyword(SymbolKind::CLASS), "class");
}

#[test]
fn parse_records_declared_types_of_parameters_locals_fields_and_for_variables() {
    let data = JavaParser.parse(concat!(
        "class Holder {\n",
        "    private Account account;\n",
        "    void run(User user, java.util.List<Order> orders) {\n",
        "        Payment payment = null;\n",
        "        var inferred = payment;\n",
        "        for (Order order : orders) {}\n",
        "    }\n",
        "}\n",
    ));
    let typed: Vec<(&str, &str)> = data
        .type_annotations
        .iter()
        .map(|(_, name, type_text)| (name.as_str(), type_text.as_str()))
        .collect();
    for expected in [
        ("account", "Account"),
        ("user", "User"),
        ("orders", "java.util.List<Order>"),
        ("payment", "Payment"),
        ("order", "Order"),
    ] {
        assert!(
            typed.contains(&expected),
            "{expected:?} missing from {typed:?}"
        );
    }
    assert!(
        !typed.iter().any(|(name, _)| *name == "inferred"),
        "a `var` local has no written type: {typed:?}"
    );
}

#[test]
fn parse_indexes_record_components_as_public_fields() {
    let data = JavaParser
        .parse("public record Point(int x, int y) {\n    int sum() { return x + y; }\n}\n");
    for component in ["x", "y"] {
        let symbol = data
            .symbols
            .iter()
            .find(|symbol| symbol.name == component)
            .unwrap_or_else(|| panic!("record component {component} not indexed"));
        assert_eq!(symbol.kind, SymbolKind::FIELD);
        assert_eq!(symbol.visibility, crate::types::Visibility::Public);
    }
}
