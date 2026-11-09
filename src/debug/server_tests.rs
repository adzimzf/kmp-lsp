use super::*;

fn indexed(indexer: &Indexer, path: &Path, content: &str) -> String {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
    let uri = Url::from_file_path(path).unwrap();
    indexer.index_content(&uri, content);
    uri.to_string()
}

const MAIN_KT: &str = "package com.example\n\nfun main() {\n    Greeter().greet()\n}\n\nclass Greeter {\n    fun greet() {\n        println(\"hi\")\n    }\n}\n";

#[test]
fn breakpoint_classes_answer_each_one_based_line() {
    let root = tempfile::tempdir().unwrap();
    let indexer = Indexer::new();
    let uri = indexed(&indexer, &root.path().join("src/Main.kt"), MAIN_KT);

    let answer = answer(
        &indexer,
        "breakpointClasses",
        &serde_json::json!({ "uri": uri, "lines": [4, 9] }),
    );
    assert_eq!(
        answer,
        serde_json::json!({ "classNames": ["com.example.MainKt", "com.example.Greeter"] })
    );
}

#[test]
fn breakpoint_classes_parse_unindexed_files_from_disk() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("Launcher.java");
    std::fs::write(
        &path,
        "package a;\n\nclass Launcher {\n    public static void main(String[] args) {\n        run();\n    }\n}\n",
    )
    .unwrap();
    let indexer = Indexer::new();
    // A plain path, as a client that sends paths rather than URIs gives it.
    let classes = breakpoint_classes(&indexer, &path.to_string_lossy(), &[5, 1]);
    assert_eq!(classes, vec![Some("a.Launcher".to_owned()), None]);
}

#[test]
fn source_uri_matches_package_not_just_file_name() {
    let root = tempfile::tempdir().unwrap();
    let indexer = Indexer::new();
    // Same file name in two packages; Kotlin files need not follow their package dirs.
    indexed(
        &indexer,
        &root.path().join("one/Main.kt"),
        "package com.other\n\nfun main() {}\n",
    );
    let wanted = indexed(&indexer, &root.path().join("two/Main.kt"), MAIN_KT);

    let from_source_path = source_uri(&indexer, "com.example.Greeter", Some("com/example/Main.kt"));
    assert_eq!(from_source_path, Some(wanted));
}

#[test]
fn source_uri_falls_back_to_class_name() {
    let root = tempfile::tempdir().unwrap();
    let indexer = Indexer::new();
    let wanted = indexed(
        &indexer,
        &root.path().join("src/com/example/Outer.java"),
        "package com.example;\n\nclass Outer {\n    class Inner {}\n}\n",
    );
    assert_eq!(
        source_uri(&indexer, "com.example.Outer$Inner", None),
        Some(wanted)
    );
    assert_eq!(source_uri(&indexer, "com.example.Missing", None), None);
}

#[test]
fn unknown_questions_answer_null() {
    let indexer = Indexer::new();
    assert_eq!(
        answer(&indexer, "noSuchMethod", &serde_json::Value::Null),
        serde_json::Value::Null
    );
}
