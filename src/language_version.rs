//! The language versions a project is built with: the Java release
//! (`javac --release`) and the Kotlin language version (`-language-version`).
//!
//! What compiles depends on them — a record is an error in Java 15, a guard
//! in a `when` branch an error in Kotlin 2.1 — so the diagnostics of
//! [`crate::types::LanguageFeatureUse`] are reported against them.
//!
//! Each version is the first of:
//!
//! 1. the editor's setting, `initializationOptions.indexingOptions`
//!    `javaRelease` (`17`) / `kotlinLanguageVersion` (`"2.1"`);
//! 2. the project's build: Maven's `pom.xml`, then Gradle's build scripts
//!    (and, for Kotlin, the Kotlin plugin version Gradle applies);
//! 3. for Java, the JDK the project builds with: the configured `jdkHome`,
//!    then `$JAVA_HOME` and the other places [`crate::indexer::jdk_sources`]
//!    looks, read from the JDK's `release` file.
//!
//! When none says, the version is not known and the newest is assumed: a
//! construct is only reported when the project's version is known to lack
//! it, or when it stopped compiling in the newest version.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

#[cfg(test)]
#[path = "language_version_tests.rs"]
mod tests;

/// A Java release, as `javac --release` takes it: `8`, `17`, `25`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct JavaRelease(pub(crate) u32);

impl JavaRelease {
    /// The newest release the diagnostics know: assumed when the project's
    /// release is not known.
    pub(crate) const NEWEST: Self = Self(25);
    const OLDEST: Self = Self(1);

    /// A release as builds spell it: `17`, `1.8`, `"21"`, `VERSION_1_8`,
    /// `JavaVersion.VERSION_17`, `JavaLanguageVersion.of(17)`, or a JDK
    /// version (`21.0.2`, `1.8.0_392`).
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_matches(|char| char == '"' || char == '\'');
        let text = text
            .rsplit_once("VERSION_")
            .map_or(text, |(_, version)| version);
        let text = text
            .strip_prefix("JavaLanguageVersion.of(")
            .map_or(text, |inner| inner.trim_end_matches(')'));
        let mut numbers = text
            .split(['.', '_'])
            .map(|part| part.trim().parse::<u32>().ok());
        let first = numbers.next()??;
        let release = if first == 1 { numbers.next()?? } else { first };
        let release = Self(release);
        (Self::OLDEST <= release && release.0 < 100).then_some(release)
    }
}

impl std::fmt::Display for JavaRelease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// A Kotlin language version: `2.1` (a compiler's `2.1.20` speaks `2.1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct KotlinLanguageVersion {
    pub(crate) major: u32,
    pub(crate) minor: u32,
}

impl KotlinLanguageVersion {
    /// The newest version the diagnostics know: assumed when the project's
    /// version is not known.
    pub(crate) const NEWEST: Self = Self::new(2, 4);

    pub(crate) const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// A version as builds spell it: `2.1`, `"2.1.20"`, `KOTLIN_2_1`,
    /// `KotlinVersion.KOTLIN_2_1`.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_matches(|char| char == '"' || char == '\'');
        let text = text
            .rsplit_once("KOTLIN_")
            .map_or(text, |(_, version)| version);
        let mut numbers = text.split(['.', '_', '-']);
        let major = numbers.next()?.trim().parse().ok()?;
        let minor = numbers.next()?.trim().parse().ok()?;
        Some(Self::new(major, minor))
    }
}

impl std::fmt::Display for KotlinLanguageVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

/// The versions a project is built with, each when known.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LanguageVersions {
    pub(crate) java: Option<JavaRelease>,
    pub(crate) kotlin: Option<KotlinLanguageVersion>,
}

impl LanguageVersions {
    /// Each version from `self`, else from `fallback`.
    fn or(self, fallback: Self) -> Self {
        Self {
            java: self.java.or(fallback.java),
            kotlin: self.kotlin.or(fallback.kotlin),
        }
    }
}

/// The versions the editor set and those found in the project, kept apart
/// so a new workspace root replaces what was found without losing what the
/// editor said.
#[derive(Debug, Default)]
pub(crate) struct ProjectLanguageVersions {
    configured: RwLock<LanguageVersions>,
    detected: RwLock<LanguageVersions>,
}

impl ProjectLanguageVersions {
    pub(crate) fn configure(&self, versions: LanguageVersions) {
        *self
            .configured
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = versions;
    }

    pub(crate) fn set_detected(&self, versions: LanguageVersions) {
        *self
            .detected
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = versions;
    }

    /// The versions the project is built with: the editor's, else the
    /// project's own, each `None` when nothing says.
    pub(crate) fn current(&self) -> LanguageVersions {
        let configured = *self
            .configured
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let detected = *self
            .detected
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        configured.or(detected)
    }
}

/// `indexingOptions.javaRelease` (a number, or a string such as `"1.8"`)
/// and `indexingOptions.kotlinLanguageVersion` (`"2.1"`).
pub(crate) fn configured_versions(
    indexing_options: Option<&serde_json::Value>,
) -> LanguageVersions {
    let setting = |name: &str| -> Option<String> {
        let value = indexing_options?.get(name)?;
        match value {
            serde_json::Value::Number(number) => Some(number.to_string()),
            serde_json::Value::String(text) => Some(text.clone()),
            _ => None,
        }
    };
    LanguageVersions {
        java: setting("javaRelease").and_then(|text| JavaRelease::parse(&text)),
        kotlin: setting("kotlinLanguageVersion")
            .and_then(|text| KotlinLanguageVersion::parse(&text)),
    }
}

/// The versions `root`'s build says, then (for Java) the release of the
/// first JDK `jdk_homes` names that has a `release` file. `jdk_homes` is
/// lazy: no JDK is looked for when the build says the release.
pub(crate) fn detect_versions(
    root: &Path,
    jdk_homes: impl IntoIterator<Item = PathBuf>,
) -> LanguageVersions {
    let build = BuildFiles::read(root);
    let java = build
        .maven_java_release()
        .or_else(|| build.gradle_java_release())
        .or_else(|| jdk_homes.into_iter().find_map(|home| jdk_release(&home)));
    let kotlin = build
        .gradle_kotlin_language_version()
        .or_else(|| build.maven_kotlin_language_version())
        .or_else(|| build.kotlin_plugin_version());
    LanguageVersions { java, kotlin }
}

/// The release a JDK is: `JAVA_VERSION` in its `release` file
/// (`JAVA_VERSION="21.0.2"`, `"1.8.0_392"`).
pub(crate) fn jdk_release(home: &Path) -> Option<JavaRelease> {
    let release_file = std::fs::read_to_string(home.join("release")).ok()?;
    release_file.lines().find_map(|line| {
        let value = line.trim().strip_prefix("JAVA_VERSION=")?;
        JavaRelease::parse(value)
    })
}

/// The build files of a project: its root's, then those one directory down
/// (the modules of a multi-module build).
struct BuildFiles {
    maven: Vec<String>,
    gradle: Vec<String>,
    version_catalogs: Vec<String>,
}

const GRADLE_SCRIPTS: &[&str] = &["build.gradle.kts", "build.gradle"];

impl BuildFiles {
    fn read(root: &Path) -> Self {
        let mut directories = vec![root.to_path_buf()];
        if let Ok(entries) = std::fs::read_dir(root) {
            let mut modules: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .filter(|path| {
                    let hidden = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with('.'));
                    !hidden
                })
                .collect();
            modules.sort();
            directories.extend(modules);
        }
        let read_all = |names: &[&str]| -> Vec<String> {
            directories
                .iter()
                .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
                .filter_map(|path| std::fs::read_to_string(path).ok())
                .collect()
        };
        let version_catalogs = std::fs::read_to_string(root.join("gradle/libs.versions.toml"))
            .ok()
            .into_iter()
            .collect();
        Self {
            maven: read_all(&["pom.xml"]),
            gradle: read_all(GRADLE_SCRIPTS),
            version_catalogs,
        }
    }

    /// The newest release any `pom.xml` compiles for. The newest, so a
    /// module built for an older release never flags what a newer one uses.
    fn maven_java_release(&self) -> Option<JavaRelease> {
        self.maven
            .iter()
            .filter_map(|pom| pom_java_release(pom))
            .max()
    }

    fn gradle_java_release(&self) -> Option<JavaRelease> {
        self.gradle
            .iter()
            .filter_map(|script| gradle_java_release(script))
            .max()
    }

    fn gradle_kotlin_language_version(&self) -> Option<KotlinLanguageVersion> {
        self.gradle
            .iter()
            .filter_map(|script| gradle_kotlin_language_version(script))
            .max()
    }

    fn maven_kotlin_language_version(&self) -> Option<KotlinLanguageVersion> {
        self.maven
            .iter()
            .filter_map(|pom| {
                let properties = PomProperties::read(pom);
                ["kotlin.compiler.languageVersion", "languageVersion"]
                    .iter()
                    .find_map(|tag| properties.tag_value(pom, tag))
                    .and_then(|text| KotlinLanguageVersion::parse(&text))
            })
            .max()
    }

    /// The version of the Kotlin plugin the build applies: a compiler
    /// speaks its own language version unless told otherwise.
    fn kotlin_plugin_version(&self) -> Option<KotlinLanguageVersion> {
        let from_scripts = self
            .gradle
            .iter()
            .filter_map(|script| kotlin_plugin_version(script));
        let from_catalogs = self
            .version_catalogs
            .iter()
            .filter_map(|catalog| catalog_kotlin_version(catalog));
        let from_poms = self.maven.iter().filter_map(|pom| {
            PomProperties::read(pom)
                .tag_value(pom, "kotlin.version")
                .and_then(|text| KotlinLanguageVersion::parse(&text))
        });
        from_scripts.chain(from_catalogs).chain(from_poms).max()
    }
}

/// The `<properties>` of a pom, to resolve `${name}` in other tags.
struct PomProperties<'pom> {
    properties: &'pom str,
}

impl<'pom> PomProperties<'pom> {
    fn read(pom: &'pom str) -> Self {
        let properties = tag_contents(pom, "properties").next().unwrap_or("");
        Self { properties }
    }

    /// The text of the first `<tag>` of `pom`, with a `${property}` in it
    /// resolved.
    fn tag_value(&self, pom: &str, tag: &str) -> Option<String> {
        let text = tag_contents(pom, tag).next()?.trim();
        self.resolve(text)
    }

    fn resolve(&self, text: &str) -> Option<String> {
        let Some(name) = text
            .strip_prefix("${")
            .and_then(|rest| rest.strip_suffix('}'))
        else {
            return Some(text.to_owned());
        };
        let value = tag_contents(self.properties, name).next()?.trim();
        let refers_again = value.starts_with("${");
        (!refers_again).then(|| value.to_owned())
    }
}

/// The release a pom compiles for: `maven.compiler.release`, the compiler
/// plugin's `<release>`, then the source level (`maven.compiler.source`,
/// `<source>`, Spring Boot's `java.version`).
fn pom_java_release(pom: &str) -> Option<JavaRelease> {
    let properties = PomProperties::read(pom);
    const TAGS: &[&str] = &[
        "maven.compiler.release",
        "release",
        "maven.compiler.source",
        "source",
        "java.version",
        "maven.compiler.target",
        "target",
    ];
    TAGS.iter()
        .filter_map(|tag| properties.tag_value(pom, tag))
        .find_map(|text| JavaRelease::parse(&text))
}

/// The text inside each `<tag>…</tag>` of `xml`.
fn tag_contents<'xml>(xml: &'xml str, tag: &str) -> impl Iterator<Item = &'xml str> + 'xml {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut rest = xml;
    std::iter::from_fn(move || {
        let start = rest.find(&open)? + open.len();
        let length = rest[start..].find(&close)?;
        let contents = &rest[start..start + length];
        rest = &rest[start + length + close.len()..];
        Some(contents)
    })
}

/// The release a Gradle script compiles Java for: the toolchain
/// (`JavaLanguageVersion.of(17)`, `jvmToolchain(17)`), `options.release`,
/// then `sourceCompatibility`.
fn gradle_java_release(script: &str) -> Option<JavaRelease> {
    let code = without_comments(script);
    let toolchain = arguments_of(&code, "JavaLanguageVersion.of(")
        .chain(arguments_of(&code, "jvmToolchain("))
        .find_map(|argument| JavaRelease::parse(&argument));
    toolchain
        .or_else(|| {
            assigned_values(&code, "release")
                .into_iter()
                .find_map(|value| JavaRelease::parse(&value))
        })
        .or_else(|| {
            assigned_values(&code, "sourceCompatibility")
                .into_iter()
                .find_map(|value| JavaRelease::parse(&value))
        })
}

/// `languageVersion` of a Gradle script's Kotlin compiler options:
/// `KotlinVersion.KOTLIN_2_1` or `"2.1"`. A `JavaLanguageVersion` is the
/// Java toolchain's, not Kotlin's.
fn gradle_kotlin_language_version(script: &str) -> Option<KotlinLanguageVersion> {
    let code = without_comments(script);
    let assigned = assigned_values(&code, "languageVersion");
    let set = arguments_of(&code, "languageVersion.set(").collect::<Vec<_>>();
    assigned
        .into_iter()
        .chain(set)
        .filter(|value| !value.contains("JavaLanguageVersion"))
        .find_map(|value| KotlinLanguageVersion::parse(&value))
        .or_else(|| {
            arguments_of(&code, "-language-version")
                .find_map(|value| KotlinLanguageVersion::parse(&value))
        })
}

/// `kotlin("jvm") version "2.1.0"`, `id("org.jetbrains.kotlin.jvm") version
/// "2.1.0"`, `id 'org.jetbrains.kotlin.android' version '2.1.0'`.
fn kotlin_plugin_version(script: &str) -> Option<KotlinLanguageVersion> {
    without_comments(script).lines().find_map(|line| {
        let applies_kotlin = line.contains("kotlin(") || line.contains("org.jetbrains.kotlin");
        if !applies_kotlin {
            return None;
        }
        let (_, version) = line.split_once(" version ")?;
        KotlinLanguageVersion::parse(version.split_whitespace().next()?)
    })
}

/// `kotlin = "2.1.0"` in a version catalog's `[versions]`.
fn catalog_kotlin_version(catalog: &str) -> Option<KotlinLanguageVersion> {
    catalog.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == "kotlin").then(|| KotlinLanguageVersion::parse(value))?
    })
}

/// The script without its `//` and `/* */` comments, so a commented-out
/// setting is not read.
fn without_comments(script: &str) -> String {
    let mut code = String::with_capacity(script.len());
    let mut rest = script;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("/*") {
            rest = after.find("*/").map_or("", |end| &after[end + 2..]);
        } else if rest.starts_with("//") {
            rest = rest.find('\n').map_or("", |end| &rest[end..]);
        } else {
            let mut chars = rest.chars();
            if let Some(char) = chars.next() {
                code.push(char);
            }
            rest = chars.as_str();
        }
    }
    code
}

/// The text inside the parentheses after each `call` (which ends in `(`,
/// or is a flag whose value follows it).
fn arguments_of<'code>(code: &'code str, call: &'code str) -> impl Iterator<Item = String> + 'code {
    code.match_indices(call).filter_map(move |(index, _)| {
        let after = &code[index + call.len()..];
        let after = after.trim_start_matches([' ', '"', ',', '=']);
        let end = after.find([')', '"', '\n', ','])?;
        Some(after[..end].trim().to_owned())
    })
}

/// The values assigned to `name` as a whole word: `name = 17`,
/// `name.set(17)` and `name 17` (Groovy).
fn assigned_values(code: &str, name: &str) -> Vec<String> {
    code.match_indices(name)
        .filter(|(index, _)| {
            let before = code[..*index].chars().next_back();
            before.is_none_or(|char| !char.is_alphanumeric() && char != '_')
        })
        .filter_map(|(index, _)| {
            let after = &code[index + name.len()..];
            let first = after.chars().next()?;
            if first.is_alphanumeric() || first == '_' {
                return None;
            }
            let after = after
                .strip_prefix(".set(")
                .unwrap_or(after)
                .trim_start_matches([' ', '\t', '=', '(']);
            let end = after
                .find(|char: char| char.is_whitespace() || matches!(char, ')' | ';' | '}'))
                .unwrap_or(after.len());
            let value = after[..end].trim();
            (!value.is_empty()).then(|| value.to_owned())
        })
        .collect()
}

/// Finds the versions `indexer`'s workspace is built with, as
/// [`detect_versions`] does, and keeps them for its diagnostics. Reads build
/// files and may ask a login shell for `JAVA_HOME`: call it off the async
/// runtime.
pub(crate) fn detect_for_workspace(indexer: &crate::indexer::Indexer) {
    let Some(root) = indexer.workspace_root.get() else {
        return;
    };
    let configured_jdk = crate::indexer::jdk_sources::configured_jdk_home(indexer);
    let environment_jdks =
        crate::indexer::jdk_sources::environment_jdk_homes().map(|(_, home)| home);
    let versions = detect_versions(&root, configured_jdk.into_iter().chain(environment_jdks));
    log::info!(
        "language versions of {}: java {:?}, kotlin {:?}",
        root.display(),
        versions.java,
        versions.kotlin
    );
    indexer.language_versions.set_detected(versions);
}

/// A Java construct whose meaning depends on the release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JavaFeature {
    /// `private` methods in an interface (9).
    PrivateInterfaceMethod,
    /// `try (resource)`: an existing variable as a resource (9).
    ResourceVariable,
    /// `var` as the type of a local or a lambda parameter: a class named
    /// `var` until 10, so an error for each use.
    VarType,
    /// `(var a, var b) ->` (11); below 10 each `var` is [`Self::VarType`].
    VarLambdaParameters,
    /// `switch` used as an expression (14).
    SwitchExpression,
    /// `case 1 ->` (14).
    ArrowCaseLabel,
    /// `"""` text blocks (15).
    TextBlock,
    /// `instanceof String text` (16).
    InstanceofPattern,
    /// `record` declarations (16).
    Record,
    /// `sealed` classes and interfaces (17).
    SealedType,
    /// `case String name` (21).
    SwitchPattern,
    /// `Point(int x, int y)` (21).
    RecordPattern,
    /// `_` as a variable's name: a name in 8, an error from 9, an unnamed
    /// variable from 22.
    UnderscoreVariable,
    /// `_` in a record pattern (22).
    UnnamedPattern,
    /// `class var`: a name until 9, an error from 10 on.
    VarAsTypeName,
    /// A file of only methods and fields, no class (25).
    CompactSourceFile,
    /// `import module java.base;` (25).
    ModuleImport,
    /// A statement before `super(…)` in a constructor (25).
    StatementBeforeSuper,
}

impl JavaFeature {
    /// Whether the construct is an error at `release`.
    pub(crate) fn is_error_at(self, release: JavaRelease) -> bool {
        let release = release.0;
        match self {
            Self::UnderscoreVariable => (9..22).contains(&release),
            Self::VarAsTypeName => release >= 10,
            Self::VarLambdaParameters => release == 10,
            _ => release < self.since(),
        }
    }

    /// The release the construct compiles from.
    fn since(self) -> u32 {
        match self {
            Self::PrivateInterfaceMethod | Self::ResourceVariable => 9,
            Self::VarType | Self::VarAsTypeName => 10,
            Self::VarLambdaParameters => 11,
            Self::SwitchExpression | Self::ArrowCaseLabel => 14,
            Self::TextBlock => 15,
            Self::InstanceofPattern | Self::Record => 16,
            Self::SealedType => 17,
            Self::SwitchPattern | Self::RecordPattern => 21,
            Self::UnderscoreVariable | Self::UnnamedPattern => 22,
            Self::CompactSourceFile | Self::ModuleImport | Self::StatementBeforeSuper => 25,
        }
    }

    /// Whether javac reports the construct once per file, at its first use
    /// (a feature the release lacks), rather than at each use (a name the
    /// release reads differently).
    pub(crate) fn reported_once_per_file(self) -> bool {
        !matches!(
            self,
            Self::VarType | Self::VarAsTypeName | Self::UnderscoreVariable
        )
    }

    pub(crate) fn message(self) -> String {
        let what = match self {
            Self::VarAsTypeName => return "`var` cannot name a type from Java 10 on".to_owned(),
            Self::PrivateInterfaceMethod => "private interface methods need",
            Self::ResourceVariable => "resource variables in `try` need",
            Self::VarType => "`var` needs",
            Self::VarLambdaParameters => "`var` lambda parameters need",
            Self::SwitchExpression => "switch expressions need",
            Self::ArrowCaseLabel => "`case ->` labels need",
            Self::TextBlock => "text blocks need",
            Self::InstanceofPattern => "patterns in `instanceof` need",
            Self::Record => "records need",
            Self::SealedType => "sealed classes need",
            Self::SwitchPattern => "patterns in `switch` need",
            Self::RecordPattern => "record patterns need",
            Self::UnderscoreVariable | Self::UnnamedPattern => "unnamed variables need",
            Self::CompactSourceFile => "compact source files need",
            Self::ModuleImport => "module imports need",
            Self::StatementBeforeSuper => "statements before `super()` need",
        };
        format!("{what} Java {} or later", self.since())
    }
}

/// A Kotlin construct that needs a newer language version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KotlinFeature {
    /// `is Circle if …` in a `when` branch (2.2).
    WhenGuard,
    /// `$$"…"` (2.2).
    MultiDollarInterpolation,
    /// `break`/`continue` inside an inline lambda (2.2).
    BreakContinueInInlineLambda,
    /// A `typealias` inside a class (2.3).
    NestedTypeAlias,
    /// `field = …` under a property (2.4).
    ExplicitBackingField,
}

impl KotlinFeature {
    pub(crate) fn since(self) -> KotlinLanguageVersion {
        match self {
            Self::WhenGuard
            | Self::MultiDollarInterpolation
            | Self::BreakContinueInInlineLambda => KotlinLanguageVersion::new(2, 2),
            Self::NestedTypeAlias => KotlinLanguageVersion::new(2, 3),
            Self::ExplicitBackingField => KotlinLanguageVersion::new(2, 4),
        }
    }

    pub(crate) fn is_error_at(self, version: KotlinLanguageVersion) -> bool {
        version < self.since()
    }

    pub(crate) fn message(self) -> String {
        let what = match self {
            Self::WhenGuard => "guard conditions in `when` need",
            Self::MultiDollarInterpolation => "multi-dollar interpolation needs",
            Self::BreakContinueInInlineLambda => "`break` and `continue` in inline lambdas need",
            Self::NestedTypeAlias => "nested type aliases need",
            Self::ExplicitBackingField => "explicit backing fields need",
        };
        format!("{what} Kotlin {} or later", self.since())
    }
}

/// A construct of either language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LanguageFeature {
    Java(JavaFeature),
    Kotlin(KotlinFeature),
}

/// Where a file uses a construct that depends on the language version
/// (`FileData::language_features`), found when it is parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LanguageFeatureUse {
    pub(crate) feature: LanguageFeature,
    pub(crate) position: tower_lsp::lsp_types::Position,
}

/// The uses of `uses` that are errors at `versions` (the newest version
/// standing in for one not known), each feature javac reports once per file
/// kept at its first use only, as diagnostics.
pub(crate) fn feature_diagnostics(
    uses: &[LanguageFeatureUse],
    versions: LanguageVersions,
) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Range};
    let java_release = versions.java.unwrap_or(JavaRelease::NEWEST);
    let kotlin_version = versions.kotlin.unwrap_or(KotlinLanguageVersion::NEWEST);
    let mut in_source_order = uses.to_vec();
    in_source_order
        .sort_by_key(|feature_use| (feature_use.position.line, feature_use.position.character));
    let mut reported_features: Vec<LanguageFeature> = Vec::new();
    let mut diagnostics = Vec::new();
    for feature_use in in_source_order {
        let (is_error, once_per_file, message) = match feature_use.feature {
            LanguageFeature::Java(feature) => (
                feature.is_error_at(java_release),
                feature.reported_once_per_file(),
                feature.message(),
            ),
            LanguageFeature::Kotlin(feature) => {
                (feature.is_error_at(kotlin_version), true, feature.message())
            }
        };
        if !is_error {
            continue;
        }
        if once_per_file {
            if reported_features.contains(&feature_use.feature) {
                continue;
            }
            reported_features.push(feature_use.feature);
        }
        diagnostics.push(Diagnostic {
            range: Range::new(feature_use.position, feature_use.position),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("kmp-lsp".into()),
            message,
            ..Default::default()
        });
    }
    diagnostics
}
