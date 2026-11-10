use super::*;

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

// ── detection ────────────────────────────────────────────────────────────────

fn launcher_name(tool: Option<BuildTool>) -> Option<String> {
    let launcher = match tool? {
        BuildTool::Gradle { launcher } | BuildTool::Maven { launcher } => launcher,
    };
    Some(launcher.file_name()?.to_string_lossy().into_owned())
}

#[test]
fn detects_gradle_and_prefers_its_wrapper() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "settings.gradle.kts", "");
    let detected = BuildTool::detect(dir.path());
    assert!(
        matches!(detected, Some(BuildTool::Gradle { .. })),
        "{detected:?}"
    );
    // Without a wrapper: an installed `gradle` (or the bare name, when none is found).
    assert!(
        launcher_name(detected).is_some_and(|name| name.starts_with("gradle")),
        "launcher must be gradle"
    );
    write(dir.path(), "gradlew", "#!/bin/sh\n");
    if !cfg!(windows) {
        assert_eq!(
            BuildTool::detect(dir.path()),
            Some(BuildTool::Gradle {
                launcher: dir.path().join("gradlew")
            })
        );
    }
}

#[test]
fn detects_maven_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(BuildTool::detect(dir.path()), None);
    write(dir.path(), "src/Main.kt", "fun main() {}");
    assert_eq!(BuildTool::detect(dir.path()), None);
    write(dir.path(), "pom.xml", "<project/>");
    let detected = BuildTool::detect(dir.path());
    assert!(
        matches!(detected, Some(BuildTool::Maven { .. })),
        "{detected:?}"
    );
    assert!(launcher_name(detected).is_some_and(|name| name.starts_with("mvn")));
}

#[cfg(unix)]
#[test]
fn installed_tool_is_found_outside_path_and_must_be_executable() {
    use std::os::unix::fs::PermissionsExt;
    let not_executable = tempfile::tempdir().unwrap();
    let installed = tempfile::tempdir().unwrap();
    write(not_executable.path(), "mvn", "#!/bin/sh\n");
    write(installed.path(), "mvn", "#!/bin/sh\n");
    std::fs::set_permissions(
        installed.path().join("mvn"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let directories = vec![
        PathBuf::from("/definitely/not/here"),
        not_executable.path().to_path_buf(),
        installed.path().to_path_buf(),
    ];
    assert_eq!(
        installed_tool("mvn", &directories),
        Some(installed.path().join("mvn"))
    );
    assert_eq!(installed_tool("gradle", &directories), None);
}

#[test]
fn search_dirs_cover_sdkman_and_package_managers() {
    let directories = search_dirs(&MAVEN_INSTALL);
    let has = |suffix: &str| {
        directories
            .iter()
            .any(|directory| directory.ends_with(suffix))
    };
    assert!(
        has(".sdkman/candidates/maven/current/bin"),
        "{directories:?}"
    );
    assert!(has("/opt/homebrew/bin") || cfg!(windows));
    assert!(has("/usr/share/maven/bin"));
}

#[test]
fn missing_launcher_is_reported_as_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.txt");
    let failure = maven::resolve(dir.path(), Path::new("/definitely/not/here/mvn"), &output)
        .expect_err("a missing launcher can't succeed");
    assert!(
        matches!(&failure, BuildToolFailure::NotFound(program) if program.ends_with("mvn")),
        "{failure:?}"
    );
}

#[test]
fn sync_without_a_build_tool_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/Main.kt", "fun main() {}");
    assert_eq!(
        sync(dir.path(), false, SyncOptions::default()),
        SyncOutcome::NoBuildTool
    );
}

#[test]
fn sync_mode_parses_case_insensitively() {
    assert_eq!(SyncMode::parse("AUTO"), Some(SyncMode::Auto));
    assert_eq!(SyncMode::parse("off"), Some(SyncMode::Off));
    assert_eq!(SyncMode::parse("sometimes"), None);
}

// ── fingerprint ──────────────────────────────────────────────────────────────

#[test]
fn fingerprint_follows_build_files_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "settings.gradle.kts", "include(\":app\")\n");
    write(root, "app/build.gradle.kts", "dependencies {}\n");
    write(root, "app/src/main/kotlin/Main.kt", "fun main() {}\n");
    let original = build_fingerprint(root).unwrap();

    write(
        root,
        "app/src/main/kotlin/Main.kt",
        "fun main() { println() }\n",
    );
    write(
        root,
        "app/build/generated/build.gradle.kts",
        "ignored output\n",
    );
    assert_eq!(
        build_fingerprint(root).unwrap(),
        original,
        "sources and build output don't change the dependencies"
    );

    write(
        root,
        "gradle/libs.versions.toml",
        "[versions]\nkotlin = \"2.1.0\"\n",
    );
    let with_catalog = build_fingerprint(root).unwrap();
    assert_ne!(with_catalog, original, "a version catalog does");

    write(
        root,
        "app/build.gradle.kts",
        "dependencies { implementation(\"a:b:1\") }\n",
    );
    assert_ne!(
        build_fingerprint(root).unwrap(),
        with_catalog,
        "a build script does"
    );
}

#[test]
fn build_file_names_cover_gradle_maven_and_catalogs() {
    for name in [
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle.kts",
        "gradle.properties",
        "gradle-wrapper.properties",
        "libs.versions.toml",
        "pom.xml",
    ] {
        assert!(is_build_file_name(name), "{name}");
    }
    for name in ["Main.kt", "build.xml", "versions.txt"] {
        assert!(!is_build_file_name(name), "{name}");
    }
}

// ── tool output ──────────────────────────────────────────────────────────────

#[test]
fn gradle_output_lines_become_dependencies() {
    let parsed = gradle::parse_output(concat!(
        "{\"coordinate\":\"com.acme:textkit:1.0.0\",\"jar\":\"/c/textkit-1.0.0.jar\",\"sources\":\"/c/textkit-1.0.0-sources.jar\"}\n",
        "{\"coordinate\":\"org.x:y:2\",\"jar\":\"/c/y-2.jar\",\"sources\":null}\n",
        "not json\n",
    ));
    assert!(!parsed.incomplete_sources);
    assert_eq!(
        parsed.dependencies,
        vec![
            ResolvedDependency {
                coordinate: "com.acme:textkit:1.0.0".to_owned(),
                jar: Some(PathBuf::from("/c/textkit-1.0.0.jar")),
                sources: Some(PathBuf::from("/c/textkit-1.0.0-sources.jar")),
                decompiled: None,
            },
            ResolvedDependency {
                coordinate: "org.x:y:2".to_owned(),
                jar: Some(PathBuf::from("/c/y-2.jar")),
                sources: None,
                decompiled: None,
            },
        ]
    );
}

#[test]
fn maven_output_pairs_jars_with_their_sources() {
    // Real `dependency:sources` + `dependency:list` output (both goals write
    // to the same file, with differently ordered fields).
    let parsed = maven::parse_output(concat!(
        "\n",
        "The following files have been resolved:\n",
        "   com.acme:textkit:jar:sources:1.0.0:/m2/com/acme/textkit/1.0.0/textkit-1.0.0-sources.jar -- module textkit (auto)\n",
        "\n",
        "The following files have NOT been resolved:\n",
        "   com.acme:textkit:jar:1.0.0:compile:/m2/com/acme/textkit/1.0.0/textkit-1.0.0.jar -- module textkit (auto)\n",
        "The following files have been resolved:\n",
        "   com.acme:textkit:jar:1.0.0:compile:/m2/com/acme/textkit/1.0.0/textkit-1.0.0.jar -- module textkit (auto)\n",
        "   org.jetbrains:annotations:jar:13.0:compile:/m2/org/jetbrains/annotations/13.0/annotations-13.0.jar\n",
        "   org.example:native:jar:linux:1.2:runtime:/m2/org/example/native/1.2/native-1.2-linux.jar\n",
    ));
    let textkit = parsed
        .iter()
        .find(|dependency| dependency.coordinate == "com.acme:textkit:1.0.0")
        .unwrap();
    assert_eq!(
        textkit.jar.as_deref(),
        Some(Path::new("/m2/com/acme/textkit/1.0.0/textkit-1.0.0.jar"))
    );
    assert_eq!(
        textkit.sources.as_deref(),
        Some(Path::new(
            "/m2/com/acme/textkit/1.0.0/textkit-1.0.0-sources.jar"
        ))
    );
    let annotations = parsed
        .iter()
        .find(|dependency| dependency.coordinate == "org.jetbrains:annotations:13.0")
        .unwrap();
    assert_eq!(annotations.sources, None);
    assert!(
        parsed
            .iter()
            .any(|dependency| dependency.coordinate == "org.example:native:1.2"),
        "a classifier jar keeps its real version: {parsed:?}"
    );
}

#[test]
fn maven_failure_in_one_module_keeps_the_other_modules_dependencies() {
    // gson's shape: `gson` resolves; `test-jpms` depends on the
    // never-installed `gson` SNAPSHOT and fails the whole run.
    let listed = maven::parse_output(concat!(
        "The following files have been resolved:\n",
        "   com.google.truth:truth:jar:1.4.5:test:/m2/com/google/truth/truth/1.4.5/truth-1.4.5.jar -- module truth (auto)\n",
        "   junit:junit:jar:4.13.2:test:/m2/junit/junit/4.13.2/junit-4.13.2.jar -- module junit [auto]\n",
    ));
    let failure =
        "[ERROR] Failed to execute goal on project test-jpms: Could not resolve dependencies";
    let resolved = maven::resolved_from(
        listed,
        maven::ModulesResolved::Some {
            failure: failure.to_owned(),
        },
    )
    .expect("the modules that resolved still count");
    let coordinates: Vec<&str> = resolved
        .dependencies
        .iter()
        .map(|dependency| dependency.coordinate.as_str())
        .collect();
    assert_eq!(
        coordinates,
        ["com.google.truth:truth:1.4.5", "junit:junit:4.13.2"]
    );
    assert!(
        resolved.incomplete_sources,
        "a partial result is retried later"
    );

    let nothing_listed = maven::resolved_from(
        Vec::new(),
        maven::ModulesResolved::Some {
            failure: failure.to_owned(),
        },
    );
    match nothing_listed {
        Err(BuildToolFailure::Failed(message)) => assert_eq!(message, failure),
        other => panic!("a failed run that listed nothing is a failure: {other:?}"),
    }

    let complete = maven::resolved_from(Vec::new(), maven::ModulesResolved::All).unwrap();
    assert!(!complete.incomplete_sources);
}

#[test]
fn maven_output_accepts_windows_paths() {
    let parsed = maven::parse_output(
        "   com.acme:textkit:jar:1.0.0:compile:C:\\Users\\me\\.m2\\repository\\com\\acme\\textkit\\1.0.0\\textkit-1.0.0.jar\n",
    );
    assert_eq!(parsed.len(), 1, "{parsed:?}");
    assert_eq!(parsed[0].coordinate, "com.acme:textkit:1.0.0");
}

#[test]
fn gradle_marker_line_flags_incomplete_sources() {
    let parsed = gradle::parse_output(concat!(
        "{\"coordinate\":\"a:b:1\",\"jar\":\"/c/b-1.jar\",\"sources\":null}\n",
        "{\"incompleteSources\":true}\n",
    ));
    assert!(parsed.incomplete_sources);
    assert_eq!(parsed.dependencies.len(), 1);
}

#[test]
fn incomplete_manifest_is_retried_only_after_the_back_off() {
    let manifest = DependencyManifest {
        format: MANIFEST_FORMAT,
        fingerprint: "f".to_owned(),
        build_tool: "gradle".to_owned(),
        dependencies: vec![],
        incomplete_sources: true,
        saved_at: 1_000,
    };
    assert!(!manifest.is_due_for_retry(1_000 + 60));
    assert!(manifest.is_due_for_retry(1_000 + INCOMPLETE_RETRY_AFTER.as_secs()));
    let complete = DependencyManifest {
        incomplete_sources: false,
        ..manifest
    };
    assert!(!complete.is_due_for_retry(u64::MAX / 2));
}

#[test]
fn login_shell_output_is_read_between_markers_only() {
    let output = format!(
        "Welcome to zsh!\n{SHELL_PATH_MARKER}/opt/zerobrew/bin:/usr/bin::/bin{SHELL_PATH_MARKER}/opt/zerobrew/opt/openjdk{SHELL_PATH_MARKER}\nbye"
    );
    let environment = parse_login_shell_output(&output);
    assert_eq!(
        environment.path,
        vec![
            PathBuf::from("/opt/zerobrew/bin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ]
    );
    assert_eq!(
        environment.java_home.as_deref(),
        Some("/opt/zerobrew/opt/openjdk")
    );

    let no_java_home = parse_login_shell_output(&format!(
        "{SHELL_PATH_MARKER}/usr/bin{SHELL_PATH_MARKER}{SHELL_PATH_MARKER}"
    ));
    assert_eq!(no_java_home.java_home, None);
    assert_eq!(
        parse_login_shell_output("no markers here"),
        LoginShellEnvironment::default()
    );
}

#[test]
fn failure_details_pick_the_error_lines() {
    let maven = "[INFO] Scanning for projects...\n\
        [ERROR] Failed to execute goal org.apache.maven.plugins:maven-dependency-plugin:3.8.1:sources: \
        Could not resolve dependencies: com.acme:missing:jar:9 was not found\n\
        [ERROR] \n\
        [ERROR] To see the full stack trace of the errors, re-run Maven with the -e switch.\n\
        [ERROR] -> [Help 1]\n";
    let details = failure_details(maven);
    assert!(
        details.starts_with("[ERROR] Failed to execute goal"),
        "{details}"
    );
    assert!(!details.contains("stack trace"), "{details}");

    let no_java = "Picked up JAVA_TOOL_OPTIONS: -Dx\nThe JAVA_HOME environment variable is not defined correctly,\n\
        this environment variable is needed to run this program.\n";
    assert!(failure_details(no_java).contains("JAVA_HOME"));
    assert!(!failure_details(no_java).contains("Picked up"));

    let plain = "line one\nline two\n";
    assert_eq!(failure_details(plain), "line one\nline two");
}

#[test]
fn zerobrew_prefix_is_a_known_location() {
    let directories = search_dirs(&MAVEN_INSTALL);
    assert!(directories
        .iter()
        .any(|directory| directory.ends_with("/opt/zerobrew/bin")));
}

#[test]
fn gradle_decompiler_line_is_read() {
    let parsed = gradle::parse_output(concat!(
        "{\"coordinate\":\"a:b:1\",\"jar\":\"/c/b-1.jar\",\"sources\":null}\n",
        "{\"decompiler\":\"/c/vineflower-1.11.1.jar\"}\n",
    ));
    assert_eq!(
        parsed.decompiler.as_deref(),
        Some(Path::new("/c/vineflower-1.11.1.jar"))
    );
    assert_eq!(parsed.dependencies.len(), 1);
}

#[test]
fn decompile_option_is_part_of_the_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "pom.xml", "<project/>");
    let on = sync_fingerprint(dir.path(), SyncOptions { decompile: true }).unwrap();
    let off = sync_fingerprint(dir.path(), SyncOptions { decompile: false }).unwrap();
    assert_ne!(on, off, "toggling decompileLibraries must re-sync");
}

#[test]
fn decompiled_sources_stand_in_for_missing_sources() {
    let dir = tempfile::tempdir().unwrap();
    let decompiled = dir.path().join("greeter-decompiled-sources.jar");
    std::fs::write(&decompiled, b"zip").unwrap();
    let manifest = DependencyManifest {
        format: MANIFEST_FORMAT,
        fingerprint: "f".to_owned(),
        build_tool: "maven".to_owned(),
        dependencies: vec![ResolvedDependency {
            coordinate: "com.acme:greeter:1.0.0".to_owned(),
            jar: None,
            sources: None,
            decompiled: Some(decompiled.clone()),
        }],
        incomplete_sources: false,
        saved_at: 0,
    };
    assert_eq!(
        manifest.sources_jars_by_artifact(),
        vec![("com.acme".to_owned(), "greeter".to_owned(), decompiled)]
    );
}

#[test]
fn decompiled_files_are_packaged_as_a_sources_jar() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "out/com/acme/Greeter.java",
        "package com.acme;\nclass Greeter {}\n",
    );
    write(
        dir.path(),
        "out/com/acme/Util.kt",
        "package com.acme\nfun util() {}\n",
    );
    write(
        dir.path(),
        "out/META-INF/MANIFEST.MF",
        "Manifest-Version: 1.0\n",
    );
    let jar = dir.path().join("greeter-decompiled-sources.jar");
    decompile::package_sources(&dir.path().join("out"), &jar).unwrap();
    let mut archive = zip::ZipArchive::new(std::fs::File::open(&jar).unwrap()).unwrap();
    let mut names: Vec<String> = (0..archive.len())
        .map(|index| archive.by_index(index).unwrap().name().to_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["com/acme/Greeter.java", "com/acme/Util.kt"]);
}

#[test]
fn maven_reactor_retry_runs_the_compile_phase_without_compiling() {
    use maven::SiblingModules;
    let output = Path::new("/tmp/deps.txt");

    let plain = maven::dependency_goal_arguments(output, SiblingModules::FromRepository);
    let reactor = maven::dependency_goal_arguments(output, SiblingModules::FromReactor);

    assert!(!plain.iter().any(|argument| argument == "compile"));
    // The phase must come before the dependency goals, so the reactor has
    // every sibling's output in place when they resolve.
    let compile = reactor.iter().position(|argument| argument == "compile");
    let list = reactor
        .iter()
        .position(|argument| argument.ends_with(":list"));
    assert!(compile.is_some() && compile < list, "{reactor:?}");
    assert!(reactor
        .iter()
        .any(|argument| argument == "-Dmaven.main.skip=true"));
    assert!(reactor.iter().any(|argument| argument == "--fail-at-end"));
}
