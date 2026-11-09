use super::*;
use tower_lsp::lsp_types::Url;

/// A two-module Gradle workspace: `app` (runnable) depends on `core`.
fn workspace() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let path = root.path();
    std::fs::write(
        path.join("settings.gradle.kts"),
        "include(\":app\", \":core\")",
    )
    .unwrap();
    for module in ["app", "core"] {
        std::fs::create_dir_all(path.join(module).join("src/main/kotlin")).unwrap();
        std::fs::write(path.join(module).join("build.gradle.kts"), "").unwrap();
        std::fs::create_dir_all(path.join(module).join("build/classes/kotlin/main")).unwrap();
        std::fs::create_dir_all(path.join(module).join("build/classes/kotlin/test")).unwrap();
    }
    // Generated output must never be mistaken for a module.
    std::fs::create_dir_all(path.join("app/build/generated")).unwrap();
    std::fs::write(path.join("app/build/generated/build.gradle.kts"), "").unwrap();
    root
}

fn index(indexer: &Indexer, path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
    indexer.index_content(&Url::from_file_path(path).unwrap(), content);
}

#[test]
fn module_of_a_source_file_is_its_nearest_build_directory() {
    let root = workspace();
    let file = root.path().join("app/src/main/kotlin/Main.kt");
    assert_eq!(module_dir_of(&file, root.path()), root.path().join("app"));
    assert_eq!(project_name(&root.path().join("app"), root.path()), "app");
    let root_name = root.path().file_name().unwrap().to_string_lossy();
    assert_eq!(project_name(root.path(), root.path()), root_name);
}

#[test]
fn module_dirs_skip_build_output() {
    let root = workspace();
    let mut modules = module_dirs(root.path());
    modules.sort();
    assert_eq!(
        modules,
        vec![
            root.path().to_path_buf(),
            root.path().join("app"),
            root.path().join("core"),
        ]
    );
}

#[test]
fn main_classes_carry_their_module_as_project_name() {
    let root = workspace();
    let indexer = Indexer::new();
    indexer.workspace_root.set(root.path().to_path_buf());
    index(
        &indexer,
        &root.path().join("app/src/main/kotlin/Main.kt"),
        "package com.example\n\nfun main() {\n    println(1)\n}\n",
    );
    index(
        &indexer,
        &root.path().join("core/src/main/kotlin/Util.kt"),
        "package com.example.core\n\nfun util() = 1\n",
    );

    let found = resolve_main_classes(&indexer, None);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].main_class, "com.example.MainKt");
    assert_eq!(found[0].project_name, "app");

    let under_core = resolve_main_classes(&indexer, Some(&root.path().join("core")));
    assert!(under_core.is_empty());
}

#[test]
fn classpath_puts_own_module_first_then_other_modules() {
    let root = workspace();
    let indexer = Indexer::new();
    indexer.workspace_root.set(root.path().to_path_buf());
    index(
        &indexer,
        &root.path().join("core/src/main/kotlin/Tool.kt"),
        "package com.example.core\n\nfun main() {}\n",
    );

    let classpath = resolve_classpath(
        &indexer,
        "com.example.core.ToolKt",
        None,
        ClasspathScope::Runtime,
    );
    let core_main = root.path().join("core/build/classes/kotlin/main");
    let app_main = root.path().join("app/build/classes/kotlin/main");
    assert_eq!(
        classpath,
        vec![
            core_main.to_string_lossy().into_owned(),
            app_main.to_string_lossy().into_owned(),
        ]
    );
}

#[test]
fn test_scope_adds_own_test_classes_only() {
    let root = workspace();
    let indexer = Indexer::new();
    indexer.workspace_root.set(root.path().to_path_buf());
    let classpath = resolve_classpath(
        &indexer,
        "com.example.AppTest",
        Some("app"),
        ClasspathScope::Test,
    );
    let app_test = root.path().join("app/build/classes/kotlin/test");
    let core_test = root.path().join("core/build/classes/kotlin/test");
    assert!(classpath.contains(&app_test.to_string_lossy().into_owned()));
    assert!(!classpath.contains(&core_test.to_string_lossy().into_owned()));
}

#[test]
fn gradle_style_project_name_resolves_to_module() {
    let root = workspace();
    assert_eq!(
        module_dir_for_project(":core", root.path()),
        Some(root.path().join("core"))
    );
    assert_eq!(module_dir_for_project("missing", root.path()), None);
}
