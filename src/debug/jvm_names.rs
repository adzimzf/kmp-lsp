//! Source position ⇄ JVM class name, from the tree-sitter index alone.
//!
//! The debugger thinks in JVM binary class names (`com.example.Outer$Inner`,
//! `com.example.MainKt`); the editor thinks in files and lines. jdtls answers
//! both directions from the JDT Java model. Here the same answers come from
//! the per-file symbol list (`FileData`), which already carries the package,
//! every class-like declaration with its full range, and the source lines.

use tower_lsp::lsp_types::SymbolKind;

use crate::types::{FileData, Language, SymbolEntry};

/// The JVM class that holds the code on a source line, as java-debug's
/// breakpoint installer wants it.
///
/// This is the *outermost* class at the line, not the innermost: java-debug
/// installs a breakpoint on `className` and every `className$*` class as they
/// load, and searches nested types of already-loaded classes, so naming the
/// top-level class reaches nested classes, companion objects, anonymous
/// classes and lambdas without having to predict the compiler's naming of
/// each one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BreakpointClass {
    /// The line is inside this top-level class (or the Kotlin file facade).
    Class(String),
    /// The line is outside every class in a Java file (imports, blank lines
    /// between top-level types): there is no code to stop on.
    NoClass,
}

/// The class java-debug should install a breakpoint on for `line` (0-based).
pub(crate) fn breakpoint_class_at(path: &str, file_data: &FileData, line: u32) -> BreakpointClass {
    let package = file_data.package.as_deref();
    if let Some(outermost) = file_data.enclosing_classes_at(line).first() {
        return BreakpointClass::Class(qualify(package, &outermost.name));
    }
    match Language::from_path(path) {
        Language::Kotlin => BreakpointClass::Class(qualify(
            package,
            &kotlin_facade_class_name(path, &file_data.lines),
        )),
        Language::Java | Language::Swift => BreakpointClass::NoClass,
    }
}

/// A runnable entry point, in the shape of jdtls' `ResolutionItem`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MainClass {
    pub main_class: String,
    pub project_name: String,
    pub file_path: String,
}

/// JVM binary names of every class in this file that the `java` launcher
/// can start: the class holding a `main` method the JVM sees as static.
pub(crate) fn main_class_names(path: &str, file_data: &FileData) -> Vec<String> {
    let language = Language::from_path(path);
    let mut names: Vec<String> = file_data
        .symbols
        .iter()
        .filter(|symbol| is_main_function(symbol))
        .filter_map(|main| main_class_of(path, language, file_data, main))
        .collect();
    names.sort();
    names.dedup();
    names
}

fn is_main_function(symbol: &SymbolEntry) -> bool {
    let is_function = matches!(symbol.kind, SymbolKind::FUNCTION | SymbolKind::METHOD);
    let is_extension = !symbol.extension_receiver().is_empty();
    symbol.name == "main" && is_function && !is_extension
}

fn main_class_of(
    path: &str,
    language: Language,
    file_data: &FileData,
    main: &SymbolEntry,
) -> Option<String> {
    let package = file_data.package.as_deref();
    let modifiers = declaration_modifiers(file_data, main);
    let enclosing = file_data.enclosing_classes_at(main.selection_range.start.line);
    match language {
        Language::Java => {
            let is_static = modifiers.split_whitespace().any(|word| word == "static");
            if !is_static || enclosing.is_empty() {
                return None;
            }
            Some(qualify(package, &binary_name(&enclosing)))
        }
        Language::Kotlin => {
            let Some(innermost) = enclosing.last() else {
                return Some(qualify(
                    package,
                    &kotlin_facade_class_name(path, &file_data.lines),
                ));
            };
            // Inside a class body only `@JvmStatic` in an object or a
            // companion object compiles to a static `main`.
            let is_jvm_static = modifiers.contains("@JvmStatic");
            if !is_jvm_static || innermost.kind != SymbolKind::OBJECT {
                return None;
            }
            // A companion's `@JvmStatic` member is emitted as a static method
            // of the class that owns the companion.
            let owner = if is_companion_object(file_data, innermost) {
                &enclosing[..enclosing.len() - 1]
            } else {
                &enclosing[..]
            };
            if owner.is_empty() {
                return None;
            }
            Some(qualify(package, &binary_name(owner)))
        }
        Language::Swift => None,
    }
}

/// Source text from the start of a declaration up to its name: annotations
/// and modifiers (`public static void`, `@JvmStatic fun`).
fn declaration_modifiers(file_data: &FileData, symbol: &SymbolEntry) -> String {
    let start = symbol.range.start;
    let name = symbol.selection_range.start;
    let mut text = String::new();
    for line_number in start.line..=name.line {
        let Some(line) = file_data.lines.get(line_number as usize) else {
            break;
        };
        let from = if line_number == start.line {
            start.character as usize
        } else {
            0
        };
        let to = if line_number == name.line {
            name.character as usize
        } else {
            line.len()
        };
        if let Some(slice) = line.get(from.min(line.len())..to.min(line.len())) {
            text.push_str(slice);
            text.push(' ');
        }
    }
    text
}

/// `companion object` or `companion object Factory`. The unnamed form's
/// symbol is synthesized with the implicit name and its selection range on
/// the `companion object` keywords themselves.
fn is_companion_object(file_data: &FileData, object: &SymbolEntry) -> bool {
    let is_unnamed_companion = object.name == crate::parser::COMPANION_OBJECT_NAME;
    let has_companion_modifier = declaration_modifiers(file_data, object)
        .split_whitespace()
        .any(|word| word == "companion");
    is_unnamed_companion || has_companion_modifier
}

/// `Outer$Inner` for a chain of nested classes, outermost first.
fn binary_name(chain: &[&SymbolEntry]) -> String {
    chain
        .iter()
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>()
        .join("$")
}

fn qualify(package: Option<&str>, class_name: &str) -> String {
    match package {
        Some(package) if !package.is_empty() => format!("{package}.{class_name}"),
        _ => class_name.to_owned(),
    }
}

/// The class Kotlin compiles a file's top-level declarations into: the
/// `@file:JvmName` when there is one, else `<CapitalizedFileName>Kt` with
/// characters that can't appear in a Java identifier replaced by `_`.
pub(crate) fn kotlin_facade_class_name(path: &str, lines: &[String]) -> String {
    if let Some(jvm_name) = lines.iter().find_map(|line| file_jvm_name(line)) {
        return jvm_name;
    }
    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let stem = file_name.strip_suffix(".kts").unwrap_or(file_name);
    let stem = stem.strip_suffix(".kt").unwrap_or(stem);
    let mut class_name = String::with_capacity(stem.len() + 2);
    for (index, character) in stem.chars().enumerate() {
        let valid = if index == 0 {
            character.is_alphabetic() || character == '_' || character == '$'
        } else {
            character.is_alphanumeric() || character == '_' || character == '$'
        };
        if !valid {
            class_name.push('_');
        } else if index == 0 {
            class_name.extend(character.to_uppercase());
        } else {
            class_name.push(character);
        }
    }
    class_name.push_str("Kt");
    class_name
}

/// `X` from a `@file:JvmName("X")` line.
fn file_jvm_name(line: &str) -> Option<String> {
    let after = line.trim_start().strip_prefix("@file:JvmName")?;
    let after = after.trim_start().strip_prefix('(')?.trim_start();
    let after = after.strip_prefix('"')?;
    let end = after.find('"')?;
    Some(after[..end].to_owned())
}

/// What a JDI source path (`com/example/Main.kt`: the package directories
/// plus the class file's `SourceFile` attribute) says about the source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourcePath {
    pub package: String,
    pub file_name: String,
}

impl SourcePath {
    pub(crate) fn parse(source_path: &str) -> Option<Self> {
        let normalized = source_path.replace('\\', "/");
        let (directories, file_name) = match normalized.rsplit_once('/') {
            Some((directories, file_name)) => (directories.to_owned(), file_name.to_owned()),
            None => (String::new(), normalized),
        };
        if file_name.is_empty() {
            return None;
        }
        Some(Self {
            package: directories.replace('/', "."),
            file_name,
        })
    }

    /// The `SourcePath` a class name implies when the debugger didn't get one
    /// from the class file: `com.example.Outer$Inner` → `com/example/Outer.java`.
    pub(crate) fn from_class_name(class_name: &str, extension: &str) -> Self {
        let top_level = class_name.split('$').next().unwrap_or(class_name);
        let (package, simple_name) = top_level.rsplit_once('.').unwrap_or(("", top_level));
        Self {
            package: package.to_owned(),
            file_name: format!("{simple_name}.{extension}"),
        }
    }

    /// Whether the file at `path` with this package declaration is the one.
    pub(crate) fn matches(&self, path: &str, package: Option<&str>) -> bool {
        let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        file_name == self.file_name && package.unwrap_or("") == self.package
    }
}

#[cfg(test)]
#[path = "jvm_names_tests.rs"]
mod tests;
