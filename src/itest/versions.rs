//! The language versions a case can expect different answers at.
//!
//! What compiles depends on the version of the language a project is built
//! with: a record is an error in Java 15 and a declaration from 16 on, a
//! nested `typealias` an error in Kotlin 2.2 and a declaration from 2.3 on. A case
//! says which answer holds for which versions with a range after its
//! `//= expected` line:
//!
//! ```text
//! //= expected java ..15
//! /src/main/java/com/demo/Point.java:3:1  records need Java 16 or later
//! //= expected java 16..
//! (nothing)
//! ```
//!
//! A range is `8..15`, `..15` (from the oldest version), `16..` (to the
//! newest) or one version, `21`; Kotlin's are written as its language
//! versions are, `1.9..2.1`. The ranges of a case name every version of
//! [`Language::versions`] exactly once, so no version goes unasked by
//! accident, and each range is asked at both its ends, where a feature
//! placed one version off shows. The server is told the version the way an
//! editor would tell it, in `initializationOptions.indexingOptions`
//! ([`Language::setting`]).
//!
//! A `//= expected` without a range is asked without the setting: the
//! project's version is not known. A case may hold one beside its ranges,
//! and every case written before versions were is one.

use serde_json::{json, Value};

/// A language whose version a case can depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Language {
    Java,
    Kotlin,
}

impl Language {
    const ALL: [Self; 2] = [Self::Java, Self::Kotlin];

    /// How a range names the language: `java 16..`.
    fn name(self) -> &'static str {
        match self {
            Self::Java => "java",
            Self::Kotlin => "kotlin",
        }
    }

    /// Every version a range can name, oldest first.
    ///
    /// What a current compiler accepts, so every case can be checked with
    /// one: Java, the releases `javac --release` of JDK 25 takes (8 to 25);
    /// Kotlin, the language versions `kotlinc -language-version` of Kotlin
    /// 2.4 takes (2.0 to 2.4; `languageVersion` in Gradle).
    pub(super) fn versions(self) -> &'static [&'static str] {
        match self {
            Self::Java => &[
                "8", "9", "10", "11", "12", "13", "14", "15", "16", "17", "18", "19", "20", "21",
                "22", "23", "24", "25",
            ],
            Self::Kotlin => &["2.0", "2.1", "2.2", "2.3", "2.4"],
        }
    }

    /// The `indexingOptions` key the version is sent as.
    pub(super) fn setting(self) -> &'static str {
        match self {
            Self::Java => "javaRelease",
            Self::Kotlin => "kotlinLanguageVersion",
        }
    }
}

/// One version of a language, as the server is told it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Version {
    pub(super) language: Language,
    index: usize,
}

impl Version {
    pub(super) fn spelled(self) -> &'static str {
        self.language.versions()[self.index]
    }

    /// `initializationOptions` telling the server this version: a Java
    /// release is a number (`17`), as in `--release 17`; a Kotlin language
    /// version a string (`"2.1"`), as in `languageVersion = "2.1"`.
    pub(super) fn initialization_options(self) -> Value {
        let value = match self.language {
            Language::Java => json!(self
                .spelled()
                .parse::<u32>()
                .expect("a Java release is a number")),
            Language::Kotlin => json!(self.spelled()),
        };
        let mut options = serde_json::Map::new();
        options.insert(self.language.setting().to_owned(), value);
        json!({"indexingOptions": options})
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} {}", self.language.name(), self.spelled())
    }
}

/// Which versions an expectation holds for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Versions {
    /// The project's version is not known: the server is not told one.
    Unknown,
    /// The versions `first..=last` of `language`, as indices into its
    /// [`Language::versions`].
    Range {
        language: Language,
        first: usize,
        last: usize,
    },
}

impl Versions {
    /// What follows `//= expected`: nothing, or `<language> <range>`.
    pub(super) fn parse(header: &str) -> Result<Self, String> {
        let header = header.trim();
        if header.is_empty() {
            return Ok(Self::Unknown);
        }
        let (name, range) = header.split_once(' ').unwrap_or((header, ""));
        let language = Language::ALL
            .into_iter()
            .find(|language| language.name() == name)
            .ok_or_else(|| {
                format!("`{header}`: a range starts with `java` or `kotlin`, not `{name}`")
            })?;
        let range = range.trim();
        let versions = language.versions();
        let index = |text: &str, otherwise: usize| -> Result<usize, String> {
            if text.is_empty() {
                return Ok(otherwise);
            }
            versions
                .iter()
                .position(|version| *version == text)
                .ok_or_else(|| {
                    format!(
                        "`{name} {text}` is not a version the cases know ({})",
                        versions.join(", ")
                    )
                })
        };
        let newest = versions.len() - 1;
        let (first, last) = match range.split_once("..") {
            Some((first, last)) => (index(first, 0)?, index(last, newest)?),
            None if range.is_empty() => return Err(format!("`{name}` without a version")),
            None => {
                let only = index(range, 0)?;
                (only, only)
            }
        };
        if first > last {
            return Err(format!("`{name} {range}` runs backwards"));
        }
        Ok(Self::Range {
            language,
            first,
            last,
        })
    }

    /// The versions the server is told, one session each: `None` for no
    /// setting at all; a range at both its ends.
    pub(super) fn asked_at(self) -> Vec<Option<Version>> {
        match self {
            Self::Unknown => vec![None],
            Self::Range {
                language,
                first,
                last,
            } => {
                let mut ends = vec![first];
                if last != first {
                    ends.push(last);
                }
                ends.into_iter()
                    .map(|index| Some(Version { language, index }))
                    .collect()
            }
        }
    }
}

/// The expectations of one case cover the versions consistently: at most
/// one without a range; ranges of one language only, naming each of its
/// versions exactly once.
pub(super) fn check_cover(all: &[Versions]) -> Result<(), String> {
    let unknown = all
        .iter()
        .filter(|versions| **versions == Versions::Unknown)
        .count();
    if unknown > 1 {
        return Err("more than one `//= expected` without a version".to_owned());
    }
    let ranges: Vec<(Language, usize, usize)> = all
        .iter()
        .filter_map(|versions| match *versions {
            Versions::Unknown => None,
            Versions::Range {
                language,
                first,
                last,
            } => Some((language, first, last)),
        })
        .collect();
    let Some(&(language, _, _)) = ranges.first() else {
        return Ok(());
    };
    if let Some(&(other, _, _)) = ranges.iter().find(|range| range.0 != language) {
        return Err(format!(
            "ranges of both {} and {}: a case depends on one language's version",
            language.name(),
            other.name()
        ));
    }
    for (index, version) in language.versions().iter().enumerate() {
        let naming = ranges
            .iter()
            .filter(|(_, first, last)| (*first..=*last).contains(&index))
            .count();
        match naming {
            1 => {}
            0 => return Err(format!("no expectation for {} {version}", language.name())),
            _ => {
                return Err(format!(
                    "{} {version} is expected {naming} times",
                    language.name()
                ))
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(header: &str) -> (Language, &'static str, &'static str) {
        match Versions::parse(header) {
            Ok(Versions::Range {
                language,
                first,
                last,
            }) => (
                language,
                language.versions()[first],
                language.versions()[last],
            ),
            other => panic!("`{header}` is a range, not {other:?}"),
        }
    }

    #[test]
    fn a_range_names_a_language_and_its_versions() {
        assert_eq!(Versions::parse(""), Ok(Versions::Unknown));
        assert_eq!(range("java ..15"), (Language::Java, "8", "15"));
        assert_eq!(range("java 16.."), (Language::Java, "16", "25"));
        assert_eq!(range("java 14..15"), (Language::Java, "14", "15"));
        assert_eq!(range("java 21"), (Language::Java, "21", "21"));
        assert_eq!(range("kotlin ..2.1"), (Language::Kotlin, "2.0", "2.1"));
        assert_eq!(range("kotlin 2.1..2.3"), (Language::Kotlin, "2.1", "2.3"));
        assert_eq!(range("kotlin 2.2.."), (Language::Kotlin, "2.2", "2.4"));
    }

    #[test]
    fn a_range_outside_the_known_versions_is_refused() {
        for header in [
            "java 7..",
            "java 26",
            "java 16..15",
            "java",
            "jdk 16",
            "kotlin 1.9",
            "kotlin 2.10",
            "kotlin 2.3..2.1",
        ] {
            assert!(Versions::parse(header).is_err(), "`{header}` is refused");
        }
    }

    #[test]
    fn ranges_cover_every_version_once() {
        let parsed = |headers: &[&str]| -> Vec<Versions> {
            headers
                .iter()
                .map(|header| Versions::parse(header).expect("a valid header"))
                .collect()
        };
        assert!(check_cover(&parsed(&[""])).is_ok());
        assert!(check_cover(&parsed(&["java ..15", "java 16.."])).is_ok());
        assert!(check_cover(&parsed(&["", "java ..15", "java 16.."])).is_ok());
        assert!(check_cover(&parsed(&["java ..8", "java 9..21", "java 22.."])).is_ok());
        assert!(check_cover(&parsed(&["kotlin ..2.1", "kotlin 2.2.."])).is_ok());
        assert!(check_cover(&parsed(&["java ..14", "java 16.."])).is_err());
        assert!(check_cover(&parsed(&["java ..16", "java 16.."])).is_err());
        assert!(check_cover(&parsed(&["", ""])).is_err());
        assert!(check_cover(&parsed(&["java ..15", "kotlin 2.2.."])).is_err());
    }

    #[test]
    fn a_range_is_asked_at_both_its_ends() {
        let spelled = |header: &str| -> Vec<String> {
            Versions::parse(header)
                .expect("a valid header")
                .asked_at()
                .into_iter()
                .map(|version| version.map_or("unknown".to_owned(), |version| version.to_string()))
                .collect()
        };
        assert_eq!(spelled(""), ["unknown"]);
        assert_eq!(spelled("java ..15"), ["java 8", "java 15"]);
        assert_eq!(spelled("java 21"), ["java 21"]);
        assert_eq!(spelled("kotlin 2.2.."), ["kotlin 2.2", "kotlin 2.4"]);
    }

    #[test]
    fn the_server_is_told_the_version_as_its_build_would_spell_it() {
        let told = |header: &str| {
            Versions::parse(header).expect("a valid header").asked_at()[0]
                .expect("a version")
                .initialization_options()
        };
        assert_eq!(
            told("java 17"),
            json!({"indexingOptions": {"javaRelease": 17}})
        );
        assert_eq!(
            told("kotlin 2.1"),
            json!({"indexingOptions": {"kotlinLanguageVersion": "2.1"}})
        );
    }
}
