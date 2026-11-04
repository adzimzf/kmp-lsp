use super::*;

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}

#[test]
fn getter_maps_to_decapitalized_property() {
    let accessor = parse_java_accessor("getName").unwrap();
    assert_eq!(accessor.property_names, vec!["name"]);
    assert_eq!(accessor.access, PropertyAccess::Read);
}

#[test]
fn getter_with_leading_acronym_keeps_capitals() {
    assert_eq!(
        parse_java_accessor("getURL").unwrap().property_names,
        vec!["URL"]
    );
}

#[test]
fn setter_maps_to_plain_and_boolean_property_names() {
    let accessor = parse_java_accessor("setActive").unwrap();
    assert_eq!(accessor.property_names, vec!["active", "isActive"]);
    assert_eq!(accessor.access, PropertyAccess::Write);
}

#[test]
fn boolean_getter_keeps_its_own_name() {
    assert_eq!(
        parse_java_accessor("isLocked").unwrap().property_names,
        vec!["isLocked"]
    );
}

#[test]
fn non_accessor_names_are_rejected() {
    for name in ["get", "set", "is", "getaway", "settle", "island", "size"] {
        assert_eq!(parse_java_accessor(name), None, "{name}");
    }
}

#[test]
fn kotlin_property_compiles_to_java_accessors() {
    assert_eq!(java_accessor_name("name", PropertyAccess::Read), "getName");
    assert_eq!(
        java_accessor_name("email", PropertyAccess::Write),
        "setEmail"
    );
    assert_eq!(
        java_accessor_name("isActive", PropertyAccess::Read),
        "isActive"
    );
    assert_eq!(
        java_accessor_name("isActive", PropertyAccess::Write),
        "setActive"
    );
    // `island` is not a boolean `is` property.
    assert_eq!(
        java_accessor_name("island", PropertyAccess::Read),
        "getIsland"
    );
}

#[test]
fn java_accessor_is_seen_by_kotlin_as_synthetic_property() {
    assert_eq!(
        kotlin_synthetic_property("getBalance"),
        Some(("balance".to_owned(), PropertyAccess::Read))
    );
    assert_eq!(
        kotlin_synthetic_property("setBalance"),
        Some(("balance".to_owned(), PropertyAccess::Write))
    );
    assert_eq!(
        kotlin_synthetic_property("isLocked"),
        Some(("isLocked".to_owned(), PropertyAccess::Read))
    );
    assert_eq!(kotlin_synthetic_property("translate"), None);
}

#[test]
fn jvm_name_is_read_from_annotation_above_declaration() {
    let source = lines("object Registry {\n    @JvmName(\"removeAll\")\n    fun clear() {}\n}\n");
    assert_eq!(jvm_name_at_line(&source, 2).as_deref(), Some("removeAll"));
    assert_eq!(jvm_name_at_line(&source, 0), None);
}

#[test]
fn jvm_name_is_read_from_same_line_annotation() {
    let source = lines("@JvmName(\"bar\") fun foo() {}\n");
    assert_eq!(jvm_name_at_line(&source, 0).as_deref(), Some("bar"));
}

#[test]
fn jvm_name_of_previous_declaration_is_not_borrowed() {
    let source = lines("@JvmName(\"bar\") fun foo() {}\nfun baz() {}\n");
    assert_eq!(jvm_name_at_line(&source, 1), None);
}

#[test]
fn facade_class_defaults_to_file_name_with_kt_suffix() {
    let uri = Url::parse("file:///src/com/acme/util/StringUtils.kt").unwrap();
    let source = lines("package com.acme.util\n\nfun slugify() {}\n");
    assert_eq!(
        file_facade_class_name(&uri, &source).as_deref(),
        Some("StringUtilsKt")
    );
}

#[test]
fn facade_class_honours_file_jvm_name() {
    let uri = Url::parse("file:///src/com/acme/util/Text.kt").unwrap();
    let source = lines("@file:JvmName(\"TextHelper\")\n\npackage com.acme.util\n");
    assert_eq!(
        file_facade_class_name(&uri, &source).as_deref(),
        Some("TextHelper")
    );
}

#[test]
fn java_files_have_no_facade() {
    let uri = Url::parse("file:///src/Account.java").unwrap();
    assert_eq!(file_facade_class_name(&uri, &[]), None);
}
