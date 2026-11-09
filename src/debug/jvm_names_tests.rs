use super::*;
use crate::parser::{parse_java, parse_kotlin};

const KOTLIN_APP: &str = r#"package com.example.app

import kotlin.math.max

fun main(args: Array<String>) {
    println(helper(1))
}

fun helper(value: Int): Int = max(value, 2)

class Greeter(private val name: String) {
    fun greet(): String {
        return "Hello $name"
    }

    companion object {
        @JvmStatic
        fun main(args: Array<String>) {
            println(Greeter("x").greet())
        }
    }

    inner class Nested {
        fun nested() = 1
    }
}

object Tool {
    @JvmStatic fun main(args: Array<String>) {}
}

object NotRunnable {
    fun main(args: Array<String>) {}
}

fun String.main() {}
"#;

const JAVA_APP: &str = r#"package com.example.app;

import java.util.List;

public class Launcher {
    public static void main(String[] args) {
        System.out.println("hi");
    }

    void main() {}

    static class Inner {
        public static void main(String[] args) {
            System.out.println("inner");
        }
    }
}

class Helper {
    void run() {}
}
"#;

#[test]
fn kotlin_top_level_line_maps_to_file_facade() {
    let file_data = parse_kotlin(KOTLIN_APP);
    let class = breakpoint_class_at("/src/app/Main.kt", &file_data, 5);
    assert_eq!(
        class,
        BreakpointClass::Class("com.example.app.MainKt".into())
    );
}

#[test]
fn kotlin_member_line_maps_to_outermost_class_not_companion() {
    let file_data = parse_kotlin(KOTLIN_APP);
    // `println(Greeter("x").greet())` inside the companion object.
    let line = KOTLIN_APP
        .lines()
        .position(|line| line.contains("Greeter(\"x\")"))
        .unwrap() as u32;
    let class = breakpoint_class_at("/src/app/Main.kt", &file_data, line);
    assert_eq!(
        class,
        BreakpointClass::Class("com.example.app.Greeter".into())
    );
}

#[test]
fn kotlin_object_line_maps_to_object_class() {
    let file_data = parse_kotlin(KOTLIN_APP);
    let line = KOTLIN_APP
        .lines()
        .position(|line| line.contains("@JvmStatic fun main"))
        .unwrap() as u32;
    let class = breakpoint_class_at("/src/app/Main.kt", &file_data, line);
    assert_eq!(class, BreakpointClass::Class("com.example.app.Tool".into()));
}

#[test]
fn java_line_maps_to_enclosing_top_level_class() {
    let file_data = parse_java(JAVA_APP);
    let inner_line = JAVA_APP
        .lines()
        .position(|line| line.contains("\"inner\""))
        .unwrap() as u32;
    let helper_line = JAVA_APP
        .lines()
        .position(|line| line.contains("void run()"))
        .unwrap() as u32;
    assert_eq!(
        breakpoint_class_at("/src/Launcher.java", &file_data, inner_line),
        BreakpointClass::Class("com.example.app.Launcher".into())
    );
    assert_eq!(
        breakpoint_class_at("/src/Launcher.java", &file_data, helper_line),
        BreakpointClass::Class("com.example.app.Helper".into())
    );
}

#[test]
fn java_import_line_has_no_class() {
    let file_data = parse_java(JAVA_APP);
    assert_eq!(
        breakpoint_class_at("/src/Launcher.java", &file_data, 2),
        BreakpointClass::NoClass
    );
}

#[test]
fn kotlin_main_classes_cover_facade_companion_and_object() {
    let file_data = parse_kotlin(KOTLIN_APP);
    let names = main_class_names("/src/app/Main.kt", &file_data);
    assert_eq!(
        names,
        vec![
            "com.example.app.Greeter".to_owned(),
            "com.example.app.MainKt".to_owned(),
            "com.example.app.Tool".to_owned(),
        ]
    );
}

#[test]
fn java_main_classes_need_static_and_use_binary_names() {
    let file_data = parse_java(JAVA_APP);
    let names = main_class_names("/src/Launcher.java", &file_data);
    assert_eq!(
        names,
        vec![
            "com.example.app.Launcher".to_owned(),
            "com.example.app.Launcher$Inner".to_owned(),
        ]
    );
}

#[test]
fn facade_name_honours_file_jvm_name() {
    let lines = vec![
        "@file:JvmName(\"Entry\")".to_owned(),
        "package com.example".to_owned(),
    ];
    assert_eq!(kotlin_facade_class_name("/a/Main.kt", &lines), "Entry");
}

#[test]
fn facade_name_capitalizes_and_sanitizes() {
    assert_eq!(kotlin_facade_class_name("/a/my-tool.kt", &[]), "My_toolKt");
    assert_eq!(kotlin_facade_class_name("C:\\a\\app.kt", &[]), "AppKt");
}

#[test]
fn source_path_parses_package_and_file() {
    let source_path = SourcePath::parse("com/example/app/Main.kt").unwrap();
    assert_eq!(source_path.package, "com.example.app");
    assert_eq!(source_path.file_name, "Main.kt");
    assert!(source_path.matches("/w/src/Main.kt", Some("com.example.app")));
    // Same file name in a different package is a different file.
    assert!(!source_path.matches("/w/other/Main.kt", Some("com.example.other")));
}

#[test]
fn source_path_from_nested_class_name_uses_top_level_class() {
    let source_path = SourcePath::from_class_name("com.example.Outer$Inner$1", "java");
    assert_eq!(source_path.package, "com.example");
    assert_eq!(source_path.file_name, "Outer.java");
}

#[test]
fn named_companion_main_belongs_to_the_owning_class() {
    let source = "package p\n\nclass Server {\n    companion object Factory {\n        @JvmStatic\n        fun main(args: Array<String>) {}\n    }\n}\n";
    let file_data = parse_kotlin(source);
    assert_eq!(
        main_class_names("/s/Server.kt", &file_data),
        vec!["p.Server".to_owned()]
    );
}

#[test]
fn main_in_nested_object_uses_binary_name() {
    // A `main` nested in a plain object inside a class lives in `Outer$Tool`.
    let source = "package p\n\nclass Outer {\n    object Tool {\n        @JvmStatic fun main(args: Array<String>) {}\n    }\n}\n";
    let file_data = parse_kotlin(source);
    assert_eq!(
        main_class_names("/s/Outer.kt", &file_data),
        vec!["p.Outer$Tool".to_owned()]
    );
}
