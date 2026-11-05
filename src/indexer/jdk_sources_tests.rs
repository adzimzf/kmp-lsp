//! Tests for [`super::jdk_sources`]: which JDK is picked, and which of its
//! sources are indexed.

use std::path::{Path, PathBuf};

use super::jdk_sources::{
    find_jdk_sources, home_of_java_executable, is_indexed_jdk_entry, is_jdk_sources_archive,
    is_jdk_sources_uri, sources_archive, JdkOrigin,
};

/// A fake JDK at `home`: a `bin/java` launcher and, when `with_sources`,
/// a `lib/src.zip`.
fn fake_jdk(home: &Path, with_sources: bool) -> PathBuf {
    std::fs::create_dir_all(home.join("bin")).unwrap();
    std::fs::write(home.join("bin/java"), "").unwrap();
    if with_sources {
        std::fs::create_dir_all(home.join("lib")).unwrap();
        std::fs::write(home.join("lib/src.zip"), "").unwrap();
    }
    home.to_path_buf()
}

#[test]
fn indexes_public_packages_of_core_modules() {
    assert!(is_indexed_jdk_entry("java.base/java/util/ArrayList.java"));
    assert!(is_indexed_jdk_entry("java.sql/java/sql/Connection.java"));
    assert!(is_indexed_jdk_entry(
        "java.xml/javax/xml/parsers/DocumentBuilder.java"
    ));
}

#[test]
fn skips_internal_packages_and_other_modules() {
    assert!(!is_indexed_jdk_entry("java.base/sun/nio/ch/Net.java"));
    assert!(!is_indexed_jdk_entry(
        "java.base/jdk/internal/misc/Unsafe.java"
    ));
    assert!(!is_indexed_jdk_entry(
        "java.xml/com/sun/org/apache/xerces/Parser.java"
    ));
    assert!(!is_indexed_jdk_entry("java.desktop/java/awt/Frame.java"));
    assert!(!is_indexed_jdk_entry(
        "jdk.compiler/com/sun/tools/javac/Main.java"
    ));
}

#[test]
fn indexes_a_jdk8_archive_without_module_prefixes() {
    assert!(is_indexed_jdk_entry("java/util/ArrayList.java"));
    assert!(!is_indexed_jdk_entry("sun/misc/Unsafe.java"));
}

#[test]
fn only_src_zip_is_a_jdk_archive() {
    assert!(is_jdk_sources_archive(Path::new("/jdk/lib/src.zip")));
    assert!(!is_jdk_sources_archive(Path::new(
        "/gradle/okio-3.0.0-sources.jar"
    )));
}

#[test]
fn finds_the_archive_under_lib_or_at_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let modern = fake_jdk(&dir.path().join("jdk21"), true);
    assert_eq!(sources_archive(&modern), Some(modern.join("lib/src.zip")));

    let jdk8 = fake_jdk(&dir.path().join("jdk8"), false);
    std::fs::write(jdk8.join("src.zip"), "").unwrap();
    assert_eq!(sources_archive(&jdk8), Some(jdk8.join("src.zip")));
}

/// Debian/Ubuntu ship `lib/src.zip` as a symlink into the separate
/// `openjdk-NN-source` package: without it, the JDK has no sources.
#[cfg(unix)]
#[test]
fn a_dangling_src_zip_symlink_is_no_sources() {
    let dir = tempfile::tempdir().unwrap();
    let home = fake_jdk(&dir.path().join("jdk"), false);
    std::fs::create_dir_all(home.join("lib")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("missing/src.zip"), home.join("lib/src.zip"))
        .unwrap();
    assert_eq!(sources_archive(&home), None);
}

#[cfg(unix)]
#[test]
fn a_java_launcher_symlink_leads_to_its_jdk() {
    let dir = tempfile::tempdir().unwrap();
    let home = fake_jdk(&dir.path().join("jdk"), true);
    let alternatives = dir.path().join("usr/bin");
    std::fs::create_dir_all(&alternatives).unwrap();
    std::os::unix::fs::symlink(home.join("bin/java"), alternatives.join("java")).unwrap();
    assert_eq!(
        home_of_java_executable(&alternatives.join("java")),
        Some(std::fs::canonicalize(&home).unwrap())
    );
}

#[test]
fn a_jdk8_jre_launcher_belongs_to_the_jdk() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("jdk8");
    std::fs::create_dir_all(home.join("jre/bin")).unwrap();
    std::fs::write(home.join("jre/bin/java"), "").unwrap();
    assert_eq!(
        home_of_java_executable(&home.join("jre/bin/java")),
        Some(std::fs::canonicalize(&home).unwrap())
    );
}

#[test]
fn the_configured_jdk_wins_over_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let configured = fake_jdk(&dir.path().join("configured"), true);
    let environment = fake_jdk(&dir.path().join("environment"), true);
    let found = find_jdk_sources(Some(&configured), [(JdkOrigin::JavaHome, environment)]).unwrap();
    assert_eq!(found.origin, JdkOrigin::Configured);
    assert_eq!(found.archive, configured.join("lib/src.zip"));
}

#[test]
fn a_jdk_without_sources_falls_through_to_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let configured_jre = fake_jdk(&dir.path().join("jre"), false);
    let java_home_without_sources = fake_jdk(&dir.path().join("java-home"), false);
    let on_path = fake_jdk(&dir.path().join("on-path"), true);
    let found = find_jdk_sources(
        Some(&configured_jre),
        [
            (JdkOrigin::JavaHome, java_home_without_sources),
            (JdkOrigin::JavaOnPath, on_path.clone()),
        ],
    )
    .unwrap();
    assert_eq!(found.origin, JdkOrigin::JavaOnPath);
    assert_eq!(found.home, on_path);
}

#[test]
fn no_jdk_with_sources_finds_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let without_sources = fake_jdk(&dir.path().join("jre"), false);
    assert_eq!(
        find_jdk_sources(None, [(JdkOrigin::JavaHome, without_sources)]),
        None
    );
}

/// The environment is only consulted when nothing before it has sources —
/// asking the login shell spawns one.
#[test]
fn the_environment_is_not_asked_when_the_configured_jdk_has_sources() {
    let dir = tempfile::tempdir().unwrap();
    let configured = fake_jdk(&dir.path().join("configured"), true);
    let environment = std::iter::from_fn(|| -> Option<(JdkOrigin, PathBuf)> {
        panic!("the environment must not be asked");
    });
    assert!(find_jdk_sources(Some(&configured), environment).is_some());
}

#[test]
fn workspace_json_jdk_home_resolves_against_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("workspace.json"),
        r#"{"sourcePaths":[],"jdkHome":"<WORKSPACE>/tools/jdk"}"#,
    )
    .unwrap();
    assert_eq!(
        crate::workspace_json::load_configured_jdk_home(root),
        Some(root.join("tools/jdk"))
    );

    std::fs::write(
        root.join("workspace.json"),
        r#"{"sourcePaths":[],"jdkHome":"tools/jdk"}"#,
    )
    .unwrap();
    assert_eq!(
        crate::workspace_json::load_configured_jdk_home(root),
        Some(root.join("tools/jdk"))
    );
}

// ── The JDK's sources, mounted ──────────────────────────────────────────────

const JDK_ARRAY_LIST: (&str, &str) = (
    "java.base/java/util/ArrayList.java",
    "package java.util;\n\npublic class ArrayList<E> {\n    public boolean add(E e) { return true; }\n}\n",
);
const JDK_STRING_BUILDER: (&str, &str) = (
    "java.base/java/lang/StringBuilder.java",
    // The constructor matters: a same-named declaration makes the bare name
    // ambiguous to the by-name fallback, as in the real JDK.
    "package java.lang;\n\npublic final class StringBuilder {\n    public StringBuilder() {}\n    public StringBuilder append(String text) { return this; }\n}\n",
);
const JDK_INTERNAL: (&str, &str) = (
    "java.base/sun/nio/ch/HiddenChannel.java",
    "package sun.nio.ch;\n\npublic class HiddenChannel {}\n",
);
const JDK_DESKTOP: (&str, &str) = (
    "java.desktop/java/awt/DesktopFrame.java",
    "package java.awt;\n\npublic class DesktopFrame {}\n",
);

/// A fake JDK 9+ at `home` whose `lib/src.zip` holds `sources`.
fn jdk_with_sources(home: &Path, sources: &[(&str, &str)]) -> PathBuf {
    use std::io::Write;
    fake_jdk(home, false);
    std::fs::create_dir_all(home.join("lib")).unwrap();
    let archive = home.join("lib/src.zip");
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    for (path, text) in sources {
        writer
            .start_file(*path, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(text.as_bytes()).unwrap();
    }
    writer.finish().unwrap();
    archive
}

struct JdkWorkspace {
    _dir: tempfile::TempDir,
    indexer: std::sync::Arc<crate::indexer::Indexer>,
    uri: tower_lsp::lsp_types::Url,
    text: String,
}

/// A workspace with `kotlin` in `Use.kt`, a decoy `Bag` declaring the same
/// member names as the JDK classes, and the fake JDK's sources mounted.
fn jdk_workspace(kotlin: &str) -> JdkWorkspace {
    let dir = tempfile::tempdir().unwrap();
    let archive = jdk_with_sources(
        &dir.path().join("jdk"),
        &[
            JDK_ARRAY_LIST,
            JDK_STRING_BUILDER,
            JDK_INTERNAL,
            JDK_DESKTOP,
        ],
    );
    let root = dir.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    let indexer = std::sync::Arc::new(crate::indexer::Indexer::new());
    indexer.workspace_root.set(root.clone());
    let bag = "package app\n\nclass Bag {\n    fun add(item: String) {}\n    fun append(text: String) {}\n}\n";
    for (name, text) in [("Bag.kt", bag), ("Use.kt", kotlin)] {
        let path = root.join(name);
        std::fs::write(&path, text).unwrap();
        let uri = tower_lsp::lsp_types::Url::from_file_path(&path).unwrap();
        indexer.index_content(&uri, text);
        indexer.store_live_tree(&uri, text);
    }
    super::library_sources::mount(&indexer, &[archive], Some(&dir.path().join("cache")));
    let uri = tower_lsp::lsp_types::Url::from_file_path(root.join("Use.kt")).unwrap();
    JdkWorkspace {
        _dir: dir,
        indexer,
        uri,
        text: kotlin.to_owned(),
    }
}

impl JdkWorkspace {
    /// Go-to-definition from `token` on the first line containing `needle`.
    fn definition(&self, needle: &str, token: &str) -> Vec<String> {
        let (line_index, line) = self
            .text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(needle))
            .unwrap();
        let column = line.find(needle).unwrap() + needle.find(token).unwrap();
        let position = tower_lsp::lsp_types::Position::new(line_index as u32, column as u32);
        let symbol = crate::indexer::classify_cursor(&self.indexer, &self.uri, position)
            .expect("an identifier under the cursor");
        let crate::indexer::NavigationSource::CstResolved(definitions) =
            crate::indexer::resolve_identity(&symbol, &self.indexer, &self.uri)
        else {
            panic!("`{needle}` was not resolved through its receiver's type: {symbol:?}");
        };
        definitions
            .0
            .iter()
            .map(|location| location.uri.to_string())
            .collect()
    }
}

#[test]
fn mounts_only_public_packages_of_indexed_modules() {
    let workspace = jdk_workspace("package app\n");
    let names = &workspace.indexer.jar_bare_names;
    assert!(names.contains_key("ArrayList"));
    assert!(names.contains_key("StringBuilder"));
    assert!(!names.contains_key("HiddenChannel"), "sun.* is internal");
    assert!(
        !names.contains_key("DesktopFrame"),
        "java.desktop is not indexed"
    );
}

#[test]
fn a_member_of_an_imported_jdk_class_resolves_into_the_jdk() {
    let workspace = jdk_workspace(
        "package app\n\nimport java.util.ArrayList\n\nfun use(items: ArrayList<String>) {\n    items.add(\"x\")\n}\n",
    );
    let definitions = workspace.definition("items.add", "add");
    assert!(
        definitions
            .iter()
            .all(|uri| uri.ends_with("src.zip!/java.base/java/util/ArrayList.java")),
        "expected ArrayList.add, not the decoy Bag.add: {definitions:?}"
    );
}

#[test]
fn a_member_of_a_fully_qualified_jdk_class_resolves_into_the_jdk() {
    let workspace = jdk_workspace(
        "package app\n\nfun use(builder: java.lang.StringBuilder) {\n    builder.append(\"x\")\n}\n",
    );
    let definitions = workspace.definition("builder.append", "append");
    assert!(
        definitions
            .iter()
            .all(|uri| uri.ends_with("src.zip!/java.base/java/lang/StringBuilder.java")),
        "expected StringBuilder.append, not the decoy Bag.append: {definitions:?}"
    );
}

#[test]
fn jdk_sources_are_not_searched_for_usages() {
    assert!(is_jdk_sources_uri(
        "jar:file:///usr/lib/jvm/jdk-21/lib/src.zip!/java.base/java/util/ArrayList.java"
    ));
    assert!(!is_jdk_sources_uri(
        "jar:file:///gradle/okio-3.0.0-sources.jar!/okio/Buffer.kt"
    ));
    assert!(!is_jdk_sources_uri("file:///workspace/src.zip/Foo.kt"));
}

#[test]
fn a_member_of_a_java_lang_class_resolves_without_an_import() {
    let workspace = jdk_workspace(
        "package app\n\nfun use(builder: StringBuilder) {\n    builder.append(\"x\")\n}\n",
    );
    let definitions = workspace.definition("builder.append", "append");
    assert!(
        !definitions.is_empty()
            && definitions
                .iter()
                .all(|uri| uri.ends_with("src.zip!/java.base/java/lang/StringBuilder.java")),
        "expected StringBuilder.append, not the decoy Bag.append: {definitions:?}"
    );
}

/// Implicit imports bring a package's top-level declarations into scope,
/// never their members.
#[test]
fn a_bare_call_never_resolves_to_a_java_lang_member() {
    let workspace = jdk_workspace(
        "package app\n\nfun use() {\n    val builder = StringBuilder()\n    append(\"x\")\n}\n",
    );
    let locations = workspace
        .indexer
        .find_definition_qualified("append", None, &workspace.uri);
    assert!(
        locations
            .iter()
            .all(|location| !location.uri.as_str().contains("src.zip")),
        "a bare `append` is not StringBuilder.append: {locations:?}"
    );
}

#[test]
fn a_java_lang_type_resolves_without_an_import() {
    let workspace = jdk_workspace("package app\n\nval builder: StringBuilder = StringBuilder()\n");
    let declarations = crate::resolver::resolve_type_path_declarations(
        &workspace.indexer,
        "StringBuilder",
        &workspace.uri,
        crate::resolver::ResolveIo::Full,
    );
    assert_eq!(declarations.len(), 1, "{declarations:?}");
    assert!(declarations[0]
        .uri
        .as_str()
        .ends_with("src.zip!/java.base/java/lang/StringBuilder.java"));
}
