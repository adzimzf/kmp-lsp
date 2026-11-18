use super::*;

fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("a temporary directory");
    for (path, text) in files {
        let path = directory.path().join(path);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the parent exists");
        std::fs::write(path, text).expect("the file is written");
    }
    directory
}

fn java_release_of(files: &[(&str, &str)]) -> Option<JavaRelease> {
    let directory = project(files);
    detect_versions(directory.path(), Vec::new()).java
}

fn kotlin_version_of(files: &[(&str, &str)]) -> Option<KotlinLanguageVersion> {
    let directory = project(files);
    detect_versions(directory.path(), Vec::new()).kotlin
}

#[test]
fn java_releases_as_builds_spell_them() {
    let parsed = |text: &str| JavaRelease::parse(text).map(|release| release.0);
    assert_eq!(parsed("17"), Some(17));
    assert_eq!(parsed("1.8"), Some(8));
    assert_eq!(parsed("\"21\""), Some(21));
    assert_eq!(parsed("'11'"), Some(11));
    assert_eq!(parsed("JavaVersion.VERSION_1_8"), Some(8));
    assert_eq!(parsed("JavaVersion.VERSION_17"), Some(17));
    assert_eq!(parsed("JavaLanguageVersion.of(21)"), Some(21));
    assert_eq!(parsed("\"21.0.2\""), Some(21));
    assert_eq!(parsed("\"1.8.0_392\""), Some(8));
    assert_eq!(parsed("${java.version}"), None);
    assert_eq!(parsed("{"), None);
}

#[test]
fn kotlin_versions_as_builds_spell_them() {
    let parsed = KotlinLanguageVersion::parse;
    assert_eq!(parsed("2.1"), Some(KotlinLanguageVersion::new(2, 1)));
    assert_eq!(parsed("\"2.1.20\""), Some(KotlinLanguageVersion::new(2, 1)));
    assert_eq!(
        parsed("KotlinVersion.KOTLIN_2_2"),
        Some(KotlinLanguageVersion::new(2, 2))
    );
    assert_eq!(
        parsed("2.3.0-Beta1"),
        Some(KotlinLanguageVersion::new(2, 3))
    );
    assert_eq!(parsed("latest"), None);
}

#[test]
fn the_editor_setting_takes_a_number_or_a_string() {
    let options = serde_json::json!({"javaRelease": 17, "kotlinLanguageVersion": "2.1"});
    let versions = configured_versions(Some(&options));
    assert_eq!(versions.java, Some(JavaRelease(17)));
    assert_eq!(versions.kotlin, Some(KotlinLanguageVersion::new(2, 1)));
    let as_text = serde_json::json!({"javaRelease": "1.8"});
    assert_eq!(
        configured_versions(Some(&as_text)).java,
        Some(JavaRelease(8))
    );
    assert_eq!(configured_versions(None), LanguageVersions::default());
}

#[test]
fn the_editor_setting_wins_over_what_the_project_says() {
    let versions = ProjectLanguageVersions::default();
    versions.set_detected(LanguageVersions {
        java: Some(JavaRelease(11)),
        kotlin: Some(KotlinLanguageVersion::new(2, 0)),
    });
    versions.configure(LanguageVersions {
        java: Some(JavaRelease(21)),
        kotlin: None,
    });
    let current = versions.current();
    assert_eq!(current.java, Some(JavaRelease(21)));
    assert_eq!(current.kotlin, Some(KotlinLanguageVersion::new(2, 0)));
}

#[test]
fn maven_release_property() {
    let pom = "<project><properties>\
        <maven.compiler.release>17</maven.compiler.release>\
        </properties></project>";
    assert_eq!(java_release_of(&[("pom.xml", pom)]), Some(JavaRelease(17)));
}

#[test]
fn maven_compiler_plugin_release_resolves_its_property() {
    let pom = "<project><properties><jdk>21</jdk></properties>\
        <build><plugins><plugin>\
        <artifactId>maven-compiler-plugin</artifactId>\
        <configuration><release>${jdk}</release></configuration>\
        </plugin></plugins></build></project>";
    assert_eq!(java_release_of(&[("pom.xml", pom)]), Some(JavaRelease(21)));
}

#[test]
fn maven_source_level_when_no_release_is_set() {
    let pom = "<project><properties>\
        <maven.compiler.source>1.8</maven.compiler.source>\
        <maven.compiler.target>1.8</maven.compiler.target>\
        </properties></project>";
    assert_eq!(java_release_of(&[("pom.xml", pom)]), Some(JavaRelease(8)));
}

#[test]
fn spring_boot_java_version() {
    let pom = "<project><properties><java.version>17</java.version></properties></project>";
    assert_eq!(java_release_of(&[("pom.xml", pom)]), Some(JavaRelease(17)));
}

#[test]
fn maven_wins_over_gradle() {
    let pom = "<project><properties><maven.compiler.release>11</maven.compiler.release></properties></project>";
    let gradle = "java { toolchain { languageVersion = JavaLanguageVersion.of(21) } }";
    assert_eq!(
        java_release_of(&[("pom.xml", pom), ("build.gradle.kts", gradle)]),
        Some(JavaRelease(11))
    );
}

#[test]
fn gradle_toolchain() {
    let gradle = "java {\n    toolchain {\n        languageVersion.set(JavaLanguageVersion.of(17))\n    }\n}\n";
    assert_eq!(
        java_release_of(&[("build.gradle.kts", gradle)]),
        Some(JavaRelease(17))
    );
}

#[test]
fn gradle_kotlin_jvm_toolchain() {
    let gradle = "kotlin {\n    jvmToolchain(21)\n}\n";
    assert_eq!(
        java_release_of(&[("build.gradle.kts", gradle)]),
        Some(JavaRelease(21))
    );
}

#[test]
fn gradle_source_compatibility_in_a_module() {
    let gradle = "android {\n    buildTypes {\n        release {\n            minifyEnabled false\n        }\n    }\n    compileOptions {\n        sourceCompatibility = JavaVersion.VERSION_1_8\n    }\n}\n";
    assert_eq!(
        java_release_of(&[("settings.gradle", ""), ("app/build.gradle", gradle)]),
        Some(JavaRelease(8))
    );
}

#[test]
fn gradle_groovy_source_compatibility_string() {
    let gradle = "sourceCompatibility = '11'\n";
    assert_eq!(
        java_release_of(&[("build.gradle", gradle)]),
        Some(JavaRelease(11))
    );
}

#[test]
fn gradle_compile_options_release() {
    let gradle = "tasks.withType(JavaCompile) {\n    options.release = 17\n}\n";
    assert_eq!(
        java_release_of(&[("build.gradle", gradle)]),
        Some(JavaRelease(17))
    );
}

#[test]
fn a_commented_out_setting_is_not_read() {
    let gradle = "// sourceCompatibility = JavaVersion.VERSION_1_8\nsourceCompatibility = JavaVersion.VERSION_17\n";
    assert_eq!(
        java_release_of(&[("build.gradle.kts", gradle)]),
        Some(JavaRelease(17))
    );
}

#[test]
fn the_newest_release_of_the_modules() {
    let old = "java { sourceCompatibility = JavaVersion.VERSION_11 }";
    let new = "java { sourceCompatibility = JavaVersion.VERSION_17 }";
    assert_eq!(
        java_release_of(&[("a/build.gradle.kts", old), ("b/build.gradle.kts", new)]),
        Some(JavaRelease(17))
    );
}

#[test]
fn the_jdk_when_the_build_says_nothing() {
    let directory = project(&[
        ("build.gradle.kts", "plugins { java }"),
        (
            "jdk/release",
            "IMPLEMENTOR=\"Eclipse\"\nJAVA_VERSION=\"21.0.2\"\n",
        ),
    ]);
    let jdk = directory.path().join("jdk");
    let versions = detect_versions(
        directory.path(),
        vec![directory.path().join("nowhere"), jdk],
    );
    assert_eq!(versions.java, Some(JavaRelease(21)));
}

#[test]
fn the_jdk_is_not_asked_when_the_build_says() {
    let directory = project(&[
        (
            "build.gradle.kts",
            "java { toolchain { languageVersion = JavaLanguageVersion.of(17) } }",
        ),
        ("jdk/release", "JAVA_VERSION=\"21.0.2\"\n"),
    ]);
    let versions = detect_versions(directory.path(), vec![directory.path().join("jdk")]);
    assert_eq!(versions.java, Some(JavaRelease(17)));
}

#[test]
fn a_java_8_jdk() {
    let directory = project(&[("release", "JAVA_VERSION=\"1.8.0_392\"\n")]);
    assert_eq!(jdk_release(directory.path()), Some(JavaRelease(8)));
}

#[test]
fn nothing_known_without_a_build_or_a_jdk() {
    assert_eq!(java_release_of(&[]), None);
    assert_eq!(kotlin_version_of(&[]), None);
}

#[test]
fn kotlin_compiler_options_language_version() {
    let gradle = "kotlin {\n    compilerOptions {\n        languageVersion.set(org.jetbrains.kotlin.gradle.dsl.KotlinVersion.KOTLIN_2_1)\n    }\n}\nplugins { kotlin(\"jvm\") version \"2.3.0\" }\n";
    assert_eq!(
        kotlin_version_of(&[("build.gradle.kts", gradle)]),
        Some(KotlinLanguageVersion::new(2, 1))
    );
}

#[test]
fn kotlin_language_version_is_not_the_java_toolchain() {
    let gradle = "java { toolchain { languageVersion = JavaLanguageVersion.of(17) } }\n";
    assert_eq!(kotlin_version_of(&[("build.gradle.kts", gradle)]), None);
}

#[test]
fn kotlin_plugin_version_when_no_language_version_is_set() {
    let gradle = "plugins {\n    kotlin(\"jvm\") version \"2.1.20\"\n}\n";
    assert_eq!(
        kotlin_version_of(&[("build.gradle.kts", gradle)]),
        Some(KotlinLanguageVersion::new(2, 1))
    );
}

#[test]
fn kotlin_version_from_the_version_catalog() {
    let catalog = "[versions]\nagp = \"8.5.0\"\nkotlin = \"2.0.21\"\n";
    assert_eq!(
        kotlin_version_of(&[("gradle/libs.versions.toml", catalog)]),
        Some(KotlinLanguageVersion::new(2, 0))
    );
}

#[test]
fn kotlin_version_from_a_pom() {
    let pom = "<project><properties><kotlin.version>2.2.0</kotlin.version></properties></project>";
    assert_eq!(
        kotlin_version_of(&[("pom.xml", pom)]),
        Some(KotlinLanguageVersion::new(2, 2))
    );
}
