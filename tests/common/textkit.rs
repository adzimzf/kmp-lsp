//! The `textkit` fixture library (`tests/fixtures/library-refs/textkit`),
//! packaged the way a build tool publishes it: `textkit-1.0.0.jar`,
//! `textkit-1.0.0-sources.jar` (the real Kotlin + Java sources) and a POM —
//! placed either straight into a Gradle cache or into a Maven repository the
//! build tool downloads from.

use std::io::Write;
use std::path::{Path, PathBuf};

pub const GROUP: &str = "com.acme";
pub const ARTIFACT: &str = "textkit";
pub const VERSION: &str = "1.0.0";

/// The fixture project: `textkit/` (library) and `app/` (its consumer).
pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/library-refs")
}

/// Write `textkit-1.0.0-sources.jar` (every `.kt`/`.java` under
/// `textkit/src/main/{kotlin,java}`, at its package path) to `destination`.
pub fn write_sources_jar(destination: &Path) {
    let source_roots =
        ["kotlin", "java"].map(|language| fixture_dir().join("textkit/src/main").join(language));
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for root in &source_roots {
        collect_files(root, root, &mut entries);
    }
    write_zip(destination, &entries);
}

/// Write a compiled-artifact stand-in: a valid jar holding only a manifest.
/// Build tools resolve and download it like any jar; nothing in these tests
/// reads its classes.
pub fn write_binary_jar(destination: &Path) {
    write_zip(
        destination,
        &[(
            "META-INF/MANIFEST.MF".to_owned(),
            b"Manifest-Version: 1.0\r\n\r\n".to_vec(),
        )],
    );
}

pub fn pom() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <project xmlns=\"http://maven.apache.org/POM/4.0.0\">\n\
         <modelVersion>4.0.0</modelVersion>\n\
         <groupId>{GROUP}</groupId><artifactId>{ARTIFACT}</artifactId>\
         <version>{VERSION}</version><packaging>jar</packaging>\n\
         </project>\n"
    )
}

/// Lay `textkit` out as a Maven repository under `repository_root`
/// (`com/acme/textkit/1.0.0/…`).
pub fn publish_to_maven_repository(repository_root: &Path) {
    let directory = repository_root
        .join(GROUP.replace('.', "/"))
        .join(ARTIFACT)
        .join(VERSION);
    std::fs::create_dir_all(&directory).unwrap();
    let base = format!("{ARTIFACT}-{VERSION}");
    write_binary_jar(&directory.join(format!("{base}.jar")));
    write_sources_jar(&directory.join(format!("{base}-sources.jar")));
    std::fs::write(directory.join(format!("{base}.pom")), pom()).unwrap();
}

/// Put `textkit-1.0.0-sources.jar` where Gradle caches a downloaded sources
/// jar (`caches/modules-2/files-2.1/<group>/<artifact>/<version>/<hash>/`).
/// Returns the jar's path.
pub fn install_sources_in_gradle_cache(gradle_user_home: &Path) -> PathBuf {
    let directory = gradle_user_home
        .join("caches/modules-2/files-2.1")
        .join(GROUP)
        .join(ARTIFACT)
        .join(VERSION)
        .join("9f4268b52e0cf1faf5fba4135c22e35ae1d55501");
    std::fs::create_dir_all(&directory).unwrap();
    let jar = directory.join(format!("{ARTIFACT}-{VERSION}-sources.jar"));
    write_sources_jar(&jar);
    jar
}

fn collect_files(root: &Path, directory: &Path, entries: &mut Vec<(String, Vec<u8>)>) {
    let mut children: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    children.sort();
    for path in children {
        if path.is_dir() {
            collect_files(root, &path, entries);
        } else {
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            entries.push((name, std::fs::read(&path).unwrap()));
        }
    }
}

fn write_zip(destination: &Path, entries: &[(String, Vec<u8>)]) {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let file = std::fs::File::create(destination).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    for (name, bytes) in entries {
        zip.start_file(name.as_str(), options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
}

// ── `greeter`: a library published WITHOUT a sources jar ─────────────────────

pub const GREETER_ARTIFACT: &str = "greeter";

/// Compile the `greeter` fixture library with `javac` and publish only its
/// classes (`greeter-1.0.0.jar` + POM, no sources jar) to the Maven
/// repository at `repository_root`. `false` when `javac` isn't available.
pub fn publish_compiled_greeter(repository_root: &Path) -> bool {
    let classes = tempfile::tempdir().unwrap();
    let source = fixture_dir().join("greeter/src/main/java/com/acme/greeter/Greeter.java");
    let compiled = std::process::Command::new("javac")
        .arg("-d")
        .arg(classes.path())
        .arg(&source)
        .output()
        .is_ok_and(|output| output.status.success());
    if !compiled {
        eprintln!("skipping: `javac` is not available to compile the greeter fixture");
        return false;
    }
    let mut entries = vec![(
        "META-INF/MANIFEST.MF".to_owned(),
        b"Manifest-Version: 1.0\r\n\r\n".to_vec(),
    )];
    collect_files(classes.path(), classes.path(), &mut entries);
    let directory = repository_root
        .join(GROUP.replace('.', "/"))
        .join(GREETER_ARTIFACT)
        .join(VERSION);
    std::fs::create_dir_all(&directory).unwrap();
    let base = format!("{GREETER_ARTIFACT}-{VERSION}");
    write_zip(&directory.join(format!("{base}.jar")), &entries);
    std::fs::write(
        directory.join(format!("{base}.pom")),
        pom().replace(
            &format!("<artifactId>{ARTIFACT}</artifactId>"),
            &format!("<artifactId>{GREETER_ARTIFACT}</artifactId>"),
        ),
    )
    .unwrap();
    true
}
