use super::*;
use std::io::Write;
use zip::write::SimpleFileOptions;

fn make_jar(dir: &Path, name: &str, entries: &[(&str, &str)]) -> PathBuf {
    let path = dir.join(name);
    let file = std::fs::File::create(&path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    for (entry, content) in entries {
        zip.start_file(*entry, SimpleFileOptions::default())
            .unwrap();
        zip.write_all(content.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    path
}

#[test]
fn parse_jar_entry_uri_splits_entry() {
    // Build the jar URI from a real path via Url::from_file_path so it round-trips
    // through Url::to_file_path cross-platform (Windows needs a drive letter).
    let tmp = tempfile::tempdir().unwrap();
    let jar_path = tmp.path().join("foo-sources.jar");
    let jar_url = Url::from_file_path(&jar_path).unwrap();
    let (jar, entry) = parse_jar_entry_uri(&format!("jar:{jar_url}!/a/B.kt")).unwrap();
    assert_eq!(jar, jar_path);
    assert_eq!(entry, "a/B.kt");
    // Compiled-only JAR URI (no `!/entry`) → nothing to extract.
    assert!(parse_jar_entry_uri("jar:file:///x/foo.aar").is_none());
    // Not a jar: URI.
    assert!(parse_jar_entry_uri("file:///x.kt").is_none());
    // Empty entry and path-traversal / absolute entries are rejected (extraction safety).
    assert!(parse_jar_entry_uri("jar:file:///x/foo.jar!/").is_none());
    assert!(parse_jar_entry_uri("jar:file:///x/foo.jar!/../etc/passwd").is_none());
    assert!(parse_jar_entry_uri("jar:file:///x/foo.jar!//abs.kt").is_none());
}

#[test]
fn extract_round_trips_entry_to_file() {
    let _lock = crate::indexer::test_helpers::XDG_CACHE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = crate::indexer::test_helpers::EnvVarGuard::set("XDG_CACHE_HOME", tmp.path());

    let jar = make_jar(
        tmp.path(),
        "lib-sources.jar",
        &[("com/example/Foo.kt", "package com.example\nclass Foo")],
    );

    let url = extract_jar_entry_to_disk(&jar, "com/example/Foo.kt").unwrap();
    assert_eq!(url.scheme(), "file");
    let content = std::fs::read_to_string(url.to_file_path().unwrap()).unwrap();
    assert!(
        content.contains("class Foo"),
        "extracted content: {content:?}"
    );

    // Idempotent — second call returns the same path.
    let url_again = extract_jar_entry_to_disk(&jar, "com/example/Foo.kt").unwrap();
    assert_eq!(url, url_again);

    // Absent entry → None.
    assert!(extract_jar_entry_to_disk(&jar, "no/such.kt").is_none());
}

#[test]
fn rewrite_converts_jar_definition_to_file() {
    let _lock = crate::indexer::test_helpers::XDG_CACHE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = crate::indexer::test_helpers::EnvVarGuard::set("XDG_CACHE_HOME", tmp.path());

    let jar = make_jar(tmp.path(), "ui-sources.jar", &[("a/B.kt", "class B")]);
    let indexer = Indexer::new();
    let range = tower_lsp::lsp_types::Range::default();

    // Build the jar: URI via Url::from_file_path so it's well-formed cross-platform
    // (Windows drive letters / backslashes).
    let jar_file_url = Url::from_file_path(&jar).unwrap();
    let jar_uri = Url::parse(&format!("jar:{jar_file_url}!/a/B.kt")).unwrap();
    let rewritten = rewrite_jar_definitions(
        &indexer,
        GotoDefinitionResponse::Scalar(Location {
            uri: jar_uri,
            range,
        }),
    );
    let GotoDefinitionResponse::Scalar(loc) = rewritten else {
        panic!("expected scalar");
    };
    assert_eq!(loc.uri.scheme(), "file", "jar target must become file://");
    assert!(
        indexer.library_uris.contains(loc.uri.as_str()),
        "extracted source must be registered as a library URI"
    );

    // A compiled-only jar URI (no source entry) is left unchanged.
    let aar = Url::parse("jar:file:///x/foo.aar").unwrap();
    let passthrough = rewrite_jar_definitions(
        &indexer,
        GotoDefinitionResponse::Scalar(Location {
            uri: aar.clone(),
            range,
        }),
    );
    let GotoDefinitionResponse::Scalar(loc2) = passthrough else {
        panic!("expected scalar");
    };
    assert_eq!(loc2.uri, aar, "compiled-only jar URI must pass through");
}

/// A compiled-only JAR (no sources jar) has no entry to extract: its
/// definition must still open — as a stub of the signatures the sidecar
/// read, at the symbol's own line, cursor on its name.
#[test]
fn compiled_only_jar_definition_opens_a_signature_stub() {
    use crate::types::{FileData, SymbolEntry};
    use tower_lsp::lsp_types::SymbolKind;
    let _lock = crate::indexer::test_helpers::XDG_CACHE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = crate::indexer::test_helpers::EnvVarGuard::set("XDG_CACHE_HOME", tmp.path());

    let jar = make_jar(
        tmp.path(),
        "core-1.15.0.jar",
        &[("a/Client.class", "bytes")],
    );
    let jar_uri = Url::parse(&format!("jar:{}", Url::from_file_path(&jar).unwrap())).unwrap();
    let synthetic = |line: u32, name: &str| Range {
        start: Position { line, character: 0 },
        end: Position {
            line,
            character: name.len() as u32,
        },
    };
    let symbol = |line: u32, name: &str, kind: SymbolKind, container: Option<&str>| SymbolEntry {
        name: name.to_owned(),
        kind,
        range: synthetic(line, name),
        selection_range: synthetic(line, name),
        container: container.map(str::to_owned),
        visibility: crate::types::Visibility::Public,
        detail: String::new(),
        params: String::new(),
        param_counts: (0, 0),
        cold: None,
        trailing_lambda: false,
        deprecated: false,
    };
    let file_data = FileData {
        lines: std::sync::Arc::new(vec![
            "class travel.core.Client".to_owned(),
            "fun connect(timeout: Long): Session".to_owned(),
        ]),
        symbols: vec![
            symbol(0, "Client", SymbolKind::CLASS, None),
            symbol(1, "connect", SymbolKind::METHOD, Some("Client")),
        ],
        ..Default::default()
    };
    let indexer = Indexer::new();
    indexer
        .jar_files
        .insert(jar_uri.to_string(), std::sync::Arc::new(file_data));
    indexer.jar_symbol_packages.insert(
        jar_uri.to_string(),
        vec!["travel.core".to_owned(), "travel.core".to_owned()],
    );

    let response = rewrite_jar_definitions(
        &indexer,
        GotoDefinitionResponse::Scalar(Location {
            uri: jar_uri.clone(),
            range: synthetic(1, "connect"),
        }),
    );
    let GotoDefinitionResponse::Scalar(location) = response else {
        panic!("expected one location");
    };
    assert_eq!(
        location.uri.scheme(),
        "file",
        "editors can only open file://"
    );
    let text = std::fs::read_to_string(location.uri.to_file_path().unwrap()).unwrap();
    let line = text.lines().nth(1).unwrap();
    assert_eq!(
        line,
        "fun connect(timeout: Long): Session    // travel.core.Client"
    );
    assert_eq!(location.range.start.line, 1);
    assert_eq!(location.range.start.character, 4, "cursor on `connect`");
    assert_eq!(location.range.end.character, 11);
    assert!(text.contains("publishes no sources jar"), "{text}");
    assert!(
        indexer.library_uris.contains(location.uri.as_str()),
        "the stub is library code, never indexed as workspace"
    );
}

#[test]
fn sources_match_wins_over_compiled_only_match() {
    let compiled = Location {
        uri: Url::parse("jar:file:///m2/core-1.15.0.jar").unwrap(),
        range: Default::default(),
    };
    let sources = Location {
        uri: Url::parse("jar:file:///m2/core-1.15.0-sources.jar!/a/Client.kt").unwrap(),
        range: Default::default(),
    };
    let response = prefer_sources_over_compiled(GotoDefinitionResponse::Array(vec![
        compiled.clone(),
        sources.clone(),
    ]));
    assert_eq!(response, GotoDefinitionResponse::Scalar(sources));

    // Only compiled matches: kept (they become signature stubs).
    let response =
        prefer_sources_over_compiled(GotoDefinitionResponse::Array(vec![compiled.clone()]));
    assert_eq!(response, GotoDefinitionResponse::Array(vec![compiled]));
}
