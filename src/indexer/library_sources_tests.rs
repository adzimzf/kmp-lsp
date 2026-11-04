use std::io::Write;
use std::path::{Path, PathBuf};

use super::*;
use crate::indexer::Indexer;

/// A sources jar holding `sources` (`(entry path, text)`).
fn write_jar(directory: &Path, name: &str, sources: &[(&str, &str)]) -> PathBuf {
    let jar = directory.join(format!("{name}-1.0-sources.jar"));
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&jar).unwrap());
    let options = zip::write::SimpleFileOptions::default();
    for (path, text) in sources {
        writer.start_file(*path, options).unwrap();
        writer.write_all(text.as_bytes()).unwrap();
    }
    writer.finish().unwrap();
    jar
}

const GREETER: (&str, &str) = (
    "com/example/greet/Greeter.kt",
    "package com.example.greet\n\nclass Greeter {\n    fun greet(name: String): String = \"Hello, $name\"\n}\n\nfun String.shout(): String = uppercase()\n",
);

const CALENDAR: (&str, &str) = (
    "com/example/time/Calendar.java",
    "package com.example.time;\n\npublic class Calendar {\n    public int today() { return 1; }\n}\n",
);

fn library_files(indexer: &Indexer) -> usize {
    indexer
        .files
        .iter()
        .filter(|entry| entry.value().source_set == SourceSet::Library)
        .count()
}

#[test]
fn mount_registers_names_without_loading_the_library() {
    let jars_dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let jar = write_jar(jars_dir.path(), "greeter", &[GREETER]);
    let indexer = Indexer::new();

    let names = mount(&indexer, std::slice::from_ref(&jar), Some(cache.path()));

    assert!(names > 0);
    assert_eq!(library_files(&indexer), 0, "nothing is loaded at mount");
    assert!(indexer.definitions.get("Greeter").is_none());
    assert!(indexer.jar_bare_names.contains_key("Greeter"));
    assert!(indexer.jar_bare_names.contains_key("greet"));
    assert!(indexer
        .jar_qualified
        .contains_key("com.example.greet.Greeter"));
    assert!(indexer
        .jar_qualified
        .contains_key("com.example.greet.shout"));
    assert!(indexer.jar_extension_receivers.contains_key("String"));
    let directory = cache_directory(Some(cache.path()));
    assert!(index_path(&directory, &jar).is_file());
    assert!(parse_path(&directory, &jar).is_file());
}

#[test]
fn first_use_of_a_name_loads_its_library() {
    let jars_dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let greeter = write_jar(jars_dir.path(), "greeter", &[GREETER]);
    let calendar = write_jar(jars_dir.path(), "calendar", &[CALENDAR]);
    let indexer = Indexer::new();
    mount(&indexer, &[greeter, calendar], Some(cache.path()));

    let locations = indexer.lookup_definitions("Greeter");

    assert_eq!(locations.len(), 1, "{locations:?}");
    assert!(locations[0]
        .uri
        .as_str()
        .ends_with("!/com/example/greet/Greeter.kt"));
    assert_eq!(locations[0].range.start.line, 2, "real source line");
    assert_eq!(
        library_files(&indexer),
        1,
        "only the library that declares it"
    );
    assert!(indexer.definitions.get("Calendar").is_none());
}

#[test]
fn a_second_session_loads_from_the_cache() {
    let jars_dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let jar = write_jar(jars_dir.path(), "greeter", &[GREETER]);
    mount(
        &Indexer::new(),
        std::slice::from_ref(&jar),
        Some(cache.path()),
    );

    let indexer = Indexer::new();
    mount(&indexer, std::slice::from_ref(&jar), Some(cache.path()));
    let jar_id = indexer.jar_table.intern(&jar.to_string_lossy());

    assert!(indexer.library_sources.is_cached(jar_id));
    assert!(load(&indexer, jar_id));
    assert!(indexer.definitions.get("greet").is_some());
    assert!(load(&indexer, jar_id), "loading twice is a no-op");
    assert_eq!(
        indexer
            .definitions
            .get("Greeter")
            .map(|locations| locations.len()),
        Some(1),
        "not applied twice"
    );
}

#[test]
fn a_changed_jar_is_parsed_again() {
    let jars_dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let jar = write_jar(jars_dir.path(), "greeter", &[GREETER]);
    mount(
        &Indexer::new(),
        std::slice::from_ref(&jar),
        Some(cache.path()),
    );
    // Rewritten with different content (and so a different size).
    write_jar(jars_dir.path(), "greeter", &[GREETER, CALENDAR]);

    let indexer = Indexer::new();
    mount(&indexer, std::slice::from_ref(&jar), Some(cache.path()));

    assert!(indexer.jar_bare_names.contains_key("Calendar"));
    assert!(!indexer.lookup_definitions("Calendar").is_empty());
}

#[test]
fn references_load_only_libraries_that_mention_the_name() {
    let jars_dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let user = (
        "com/example/app/Welcome.kt",
        "package com.example.app\n\nimport com.example.greet.Greeter\n\nfun welcome() = Greeter().greet(\"you\")\n",
    );
    let unrelated = (
        "com/example/other/Other.kt",
        "package com.example.other\n\nfun greet() = 1\n",
    );
    let greeter = write_jar(jars_dir.path(), "greeter", &[GREETER]);
    let app = write_jar(jars_dir.path(), "app", &[user]);
    let other = write_jar(jars_dir.path(), "other", &[unrelated]);
    let calendar = write_jar(jars_dir.path(), "calendar", &[CALENDAR]);
    let indexer = Indexer::new();
    mount(
        &indexer,
        &[greeter, app, other, calendar],
        Some(cache.path()),
    );

    // `Greeter.greet`: a user has to mention `Greeter` (or the package).
    let loaded = load_libraries_mentioning(&indexer, &["greet"], &["Greeter"]);

    assert_eq!(loaded, 2, "the declaring library and its user");
    assert!(
        indexer.definitions.get("welcome").is_some(),
        "the user is loaded"
    );
    assert!(
        indexer
            .files
            .iter()
            .all(|entry| !entry.key().contains("Other.kt")),
        "`other` mentions `greet` but never `Greeter`"
    );
    assert!(
        indexer.definitions.get("Calendar").is_none(),
        "calendar never mentions it"
    );

    let unscoped = load_libraries_mentioning(&indexer, &["today"], &[]);
    assert_eq!(unscoped, 1);
}

#[test]
fn caches_of_deleted_jars_are_pruned() {
    let jars_dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let old = write_jar(jars_dir.path(), "old", &[CALENDAR]);
    let kept = write_jar(jars_dir.path(), "kept", &[GREETER]);
    mount(
        &Indexer::new(),
        &[old.clone(), kept.clone()],
        Some(cache.path()),
    );
    std::fs::remove_file(&old).unwrap();

    mount(
        &Indexer::new(),
        std::slice::from_ref(&kept),
        Some(cache.path()),
    );

    let directory = cache_directory(Some(cache.path()));
    assert!(!index_path(&directory, &old).exists());
    assert!(!parse_path(&directory, &old).exists());
    assert!(index_path(&directory, &kept).exists());
}

#[test]
fn identifiers_split_on_punctuation_and_skip_numbers() {
    let words: Vec<&str> = identifiers("val x2 = foo.bar_baz($name) + 42 // héllo").collect();
    assert_eq!(words, ["val", "x2", "foo", "bar_baz", "$name", "héllo"]);
}

#[test]
fn mode_parses_the_option_values() {
    assert_eq!(
        LibrarySourcesMode::parse("onDemand"),
        Some(LibrarySourcesMode::OnDemand)
    );
    assert_eq!(
        LibrarySourcesMode::parse("eager"),
        Some(LibrarySourcesMode::Eager)
    );
    assert_eq!(LibrarySourcesMode::parse("sometimes"), None);
    let sources = LibrarySources::default();
    assert_eq!(
        sources.mode(),
        LibrarySourcesMode::OnDemand,
        "on demand by default"
    );
    sources.set_mode(LibrarySourcesMode::Eager);
    assert_eq!(sources.mode(), LibrarySourcesMode::Eager);
}

/// Timings on real jars: `KMP_LSP_MEASURE_JARS=a.jar:b.jar cargo test
/// --release measure_real_libraries -- --ignored --nocapture`.
#[test]
#[ignore]
fn measure_real_libraries() {
    let jars: Vec<PathBuf> = std::env::var("KMP_LSP_MEASURE_JARS")
        .expect("KMP_LSP_MEASURE_JARS")
        .split(':')
        .map(PathBuf::from)
        .collect();
    let cache = tempfile::tempdir().unwrap();
    for jar in &jars {
        let name = jar.file_name().unwrap().to_string_lossy().into_owned();
        let started = std::time::Instant::now();
        mount(
            &Indexer::new(),
            std::slice::from_ref(jar),
            Some(cache.path()),
        );
        let first_mount = started.elapsed();

        let indexer = Indexer::new();
        let started = std::time::Instant::now();
        mount(&indexer, std::slice::from_ref(jar), Some(cache.path()));
        let warm_mount = started.elapsed();
        let jar_id = indexer.jar_table.intern(&jar.to_string_lossy());
        let started = std::time::Instant::now();
        assert!(load(&indexer, jar_id));
        let first_use = started.elapsed();
        let directory = cache_directory(Some(cache.path()));
        let size = |path: PathBuf| std::fs::metadata(path).map(|m| m.len() / 1024).unwrap_or(0);
        println!(
            "{name}: first startup (parse + cache) {first_mount:?}, later startup {warm_mount:?}, first use {first_use:?}; index {} KB, parse {} KB",
            size(index_path(&directory, jar)),
            size(parse_path(&directory, jar)),
        );
    }
}
