//! End-to-end Java ↔ Kotlin interop tests over the real `kmp-lsp --stdio` server.
//!
//! The fixture project in `tests/fixtures/interop/` is ordinary, compilable
//! Kotlin + Java code where each language calls the other through the names the
//! JVM actually exposes:
//!
//! - Java → Kotlin: property accessors (`getName()`, `setEmail()`), file facades
//!   (`StringUtilsKt.slugify()`, `@file:JvmName("TextHelper")`), `@JvmName`,
//!   `object` singletons (`Registry.INSTANCE`) and companions (`User.Companion`).
//! - Kotlin → Java: synthetic properties for getters/setters (`account.balance`,
//!   `account.balance = …`) and record components (`point.x`).
//!
//! Every test drives go-to-definition or find-references from one language and
//! asserts the result lands on the declaration (or usage) in the other.

mod common;

use std::path::Path;

use common::*;
use serde_json::json;

const FIXTURE_DIR: &str = "tests/fixtures/interop";

const USER_KT: &str = "src/main/kotlin/com/acme/model/User.kt";
const STRING_UTILS_KT: &str = "src/main/kotlin/com/acme/util/StringUtils.kt";
const TEXT_KT: &str = "src/main/kotlin/com/acme/util/Text.kt";
const REGISTRY_KT: &str = "src/main/kotlin/com/acme/registry/Registry.kt";
const KOTLIN_CLIENT_KT: &str = "src/main/kotlin/com/acme/app/KotlinClient.kt";
const ACCOUNT_JAVA: &str = "src/main/java/com/acme/legacy/Account.java";
const POINT_JAVA: &str = "src/main/java/com/acme/legacy/Point.java";
const JAVA_CLIENT_JAVA: &str = "src/main/java/com/acme/app/JavaClient.java";

// ── fixture workspace ─────────────────────────────────────────────────────────

/// A running server over a temp copy of the interop fixture project.
struct InteropWorkspace {
    // Held so the directory outlives the server.
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    client: LspClient,
}

/// A point in a fixture file: 0-based line and UTF-16 column of `token`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Spot {
    line: u64,
    column: u64,
}

impl InteropWorkspace {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(dir.path());
        copy_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR),
            &root,
        );
        write(&root, "workspace.json", r#"{"sourcePaths":[]}"#);

        let mut client = LspClient::spawn(&root);
        client.initialize(&root);
        client.wait_for_indexing();
        InteropWorkspace {
            _dir: dir,
            root,
            client,
        }
    }

    fn text(&self, relative_path: &str) -> String {
        std::fs::read_to_string(self.root.join(relative_path)).unwrap()
    }

    fn uri(&self, relative_path: &str) -> String {
        file_uri(&self.root, relative_path)
    }

    /// Position of `token` on the first line of `relative_path` containing
    /// `line_needle`. Panics if either is missing so a fixture edit can't
    /// silently turn a test into a no-op.
    fn spot(&self, relative_path: &str, line_needle: &str, token: &str) -> Spot {
        let text = self.text(relative_path);
        let (line_index, line) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(line_needle))
            .unwrap_or_else(|| panic!("{relative_path}: no line contains {line_needle:?}"));
        let needle_start = line.find(line_needle).unwrap();
        let token_offset = line[needle_start..]
            .find(token)
            .unwrap_or_else(|| panic!("{relative_path}: {token:?} not inside {line_needle:?}"));
        let byte_column = needle_start + token_offset;
        let column = line[..byte_column].encode_utf16().count();
        Spot {
            line: line_index as u64,
            column: column as u64,
        }
    }

    fn open(&mut self, relative_path: &str) -> String {
        let uri = self.uri(relative_path);
        let language = if relative_path.ends_with(".java") {
            "java"
        } else {
            "kotlin"
        };
        let text = self.text(relative_path);
        self.client.open_file(&uri, language, &text);
        uri
    }

    fn definition(&mut self, relative_path: &str, at: Spot) -> Vec<(String, u64)> {
        let uri = self.open(relative_path);
        let response = self.client.request(
            "textDocument/definition",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": at.line, "character": at.column},
            }),
        );
        locations(&response["result"])
    }

    fn references(&mut self, relative_path: &str, at: Spot) -> Vec<(String, u64)> {
        let uri = self.open(relative_path);
        let response = self.client.request(
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": at.line, "character": at.column},
                "context": {"includeDeclaration": false},
            }),
        );
        locations(&response["result"])
    }

    /// Go-to-definition from `token` on the `from_needle` line of `from` must
    /// land on the `to_needle` line of `to`.
    fn assert_definition(
        &mut self,
        (from, from_needle, token): (&str, &str, &str),
        (to, to_needle): (&str, &str),
    ) {
        let at = self.spot(from, from_needle, token);
        let expected = (self.uri(to), self.spot(to, to_needle, to_needle).line);
        let found = self.definition(from, at);
        assert!(
            found.contains(&expected),
            "definition of `{token}` in {from} ({from_needle:?}) must reach {to}:{} ({to_needle:?}); got {found:?}",
            expected.1
        );
    }

    /// Find-references on `token` on the `from_needle` line of `from` must
    /// include the `usage_needle` line of `usage_file`.
    fn assert_reference(
        &mut self,
        (from, from_needle, token): (&str, &str, &str),
        (usage_file, usage_needle): (&str, &str),
    ) {
        let at = self.spot(from, from_needle, token);
        let expected = (
            self.uri(usage_file),
            self.spot(usage_file, usage_needle, usage_needle).line,
        );
        let found = self.references(from, at);
        assert!(
            found.contains(&expected),
            "references of `{token}` in {from} must include {usage_file}:{} ({usage_needle:?}); got {found:?}",
            expected.1
        );
    }

    /// Find-references on `token` must NOT include the `usage_needle` line.
    fn assert_no_reference(
        &mut self,
        (from, from_needle, token): (&str, &str, &str),
        (usage_file, usage_needle): (&str, &str),
    ) {
        let at = self.spot(from, from_needle, token);
        let unexpected = (
            self.uri(usage_file),
            self.spot(usage_file, usage_needle, usage_needle).line,
        );
        let found = self.references(from, at);
        assert!(
            !found.contains(&unexpected),
            "references of `{token}` in {from} must not include {usage_file}:{} ({usage_needle:?}); got {found:?}",
            unexpected.1
        );
    }
}

// ── baseline: same name on both sides (already supported) ────────────────────

#[test]
fn java_calls_kotlin_boolean_is_property_by_its_own_name() {
    let mut workspace = InteropWorkspace::start();
    workspace.assert_definition(
        (JAVA_CLIENT_JAVA, "user.isActive()", "isActive"),
        (USER_KT, "val isActive"),
    );
}

#[test]
fn kotlin_reads_java_getter_as_property() {
    let mut workspace = InteropWorkspace::start();
    workspace.assert_definition(
        (KOTLIN_CLIENT_KT, "else account.balance", "balance"),
        (ACCOUNT_JAVA, "public long getBalance()"),
    );
}

// ── Java → Kotlin: property accessors ────────────────────────────────────────

#[test]
fn java_getter_call_resolves_to_kotlin_val() {
    let mut workspace = InteropWorkspace::start();
    workspace.assert_definition(
        (JAVA_CLIENT_JAVA, "user.getName()", "getName"),
        (USER_KT, "class User(val name"),
    );
}

#[test]
fn java_setter_and_getter_calls_resolve_to_kotlin_var() {
    let mut workspace = InteropWorkspace::start();
    workspace.assert_definition(
        (JAVA_CLIENT_JAVA, "user.setEmail(", "setEmail"),
        (USER_KT, "class User(val name"),
    );
    workspace.assert_definition(
        (JAVA_CLIENT_JAVA, "user.getEmail()", "getEmail"),
        (USER_KT, "class User(val name"),
    );
    workspace.assert_definition(
        (JAVA_CLIENT_JAVA, "user.setNickname(", "setNickname"),
        (USER_KT, "var nickname"),
    );
}

#[test]
fn references_of_kotlin_property_include_java_accessor_calls() {
    let mut workspace = InteropWorkspace::start();
    workspace.assert_reference(
        (USER_KT, "var nickname", "nickname"),
        (JAVA_CLIENT_JAVA, "user.setNickname("),
    );
    workspace.assert_reference(
        (USER_KT, "class User(val name", "name"),
        (JAVA_CLIENT_JAVA, "user.getName()"),
    );
    workspace.assert_reference(
        (USER_KT, "var email", "email"),
        (JAVA_CLIENT_JAVA, "user.setEmail("),
    );
    workspace.assert_reference(
        (USER_KT, "var email", "email"),
        (JAVA_CLIENT_JAVA, "user.getEmail()"),
    );
}

// ── Java → Kotlin: objects and companions ───────────────────────────────────

#[test]
fn java_call_through_companion_resolves_to_kotlin_companion_member() {
    let mut workspace = InteropWorkspace::start();
    workspace.assert_definition(
        (JAVA_CLIENT_JAVA, "User.Companion.anonymous()", "anonymous"),
        (USER_KT, "fun anonymous()"),
    );
}

#[test]
fn java_call_through_object_instance_resolves_to_kotlin_object_member() {
    let mut workspace = InteropWorkspace::start();
    workspace.assert_definition(
        (JAVA_CLIENT_JAVA, "Registry.INSTANCE.register(", "register"),
        (REGISTRY_KT, "fun register("),
    );
    workspace.assert_definition(
        (JAVA_CLIENT_JAVA, "Registry.INSTANCE.register(", "INSTANCE"),
        (REGISTRY_KT, "object Registry"),
    );
}

// ── Java → Kotlin: @JvmName ─────────────────────────────────────────────────

#[test]
fn java_call_by_jvm_name_resolves_to_kotlin_function() {
    let mut workspace = InteropWorkspace::start();
    workspace.assert_definition(
        (
            JAVA_CLIENT_JAVA,
            "Registry.INSTANCE.removeAll()",
            "removeAll",
        ),
        (REGISTRY_KT, "fun clear()"),
    );
}

#[test]
fn references_of_kotlin_function_include_java_calls_by_jvm_name() {
    let mut workspace = InteropWorkspace::start();
    workspace.assert_reference(
        (REGISTRY_KT, "fun clear()", "clear"),
        (JAVA_CLIENT_JAVA, "Registry.INSTANCE.removeAll()"),
    );
}

// ── Java → Kotlin: file facades ─────────────────────────────────────────────

#[test]
fn java_facade_class_resolves_to_kotlin_file() {
    let mut workspace = InteropWorkspace::start();
    workspace.assert_definition(
        (JAVA_CLIENT_JAVA, "StringUtilsKt.slugify(", "StringUtilsKt"),
        (STRING_UTILS_KT, "package com.acme.util"),
    );
    workspace.assert_definition(
        (JAVA_CLIENT_JAVA, "TextHelper.wordCount(", "TextHelper"),
        (TEXT_KT, "@file:JvmName(\"TextHelper\")"),
    );
}

#[test]
fn java_call_through_facade_resolves_to_kotlin_top_level_function() {
    let mut workspace = InteropWorkspace::start();
    workspace.assert_definition(
        (JAVA_CLIENT_JAVA, "StringUtilsKt.slugify(", "slugify"),
        (STRING_UTILS_KT, "fun slugify("),
    );
    workspace.assert_definition(
        (JAVA_CLIENT_JAVA, "StringUtilsKt.shout(", "shout"),
        (STRING_UTILS_KT, "fun String.shout()"),
    );
    workspace.assert_definition(
        (JAVA_CLIENT_JAVA, "TextHelper.wordCount(", "wordCount"),
        (TEXT_KT, "fun wordCount("),
    );
}

#[test]
fn references_of_kotlin_top_level_function_include_java_facade_calls() {
    let mut workspace = InteropWorkspace::start();
    workspace.assert_reference(
        (STRING_UTILS_KT, "fun slugify(", "slugify"),
        (JAVA_CLIENT_JAVA, "StringUtilsKt.slugify("),
    );
    workspace.assert_reference(
        (TEXT_KT, "fun wordCount(", "wordCount"),
        (JAVA_CLIENT_JAVA, "TextHelper.wordCount("),
    );
}

// ── Kotlin → Java: synthetic properties ─────────────────────────────────────

#[test]
fn kotlin_property_assignment_resolves_to_java_setter() {
    let mut workspace = InteropWorkspace::start();
    workspace.assert_definition(
        (KOTLIN_CLIENT_KT, "account.balance = 100L", "balance"),
        (ACCOUNT_JAVA, "public void setBalance("),
    );
    workspace.assert_definition(
        (KOTLIN_CLIENT_KT, "account.owner = \"acme\"", "owner"),
        (ACCOUNT_JAVA, "public void setOwner("),
    );
}

#[test]
fn references_of_java_accessors_include_kotlin_property_syntax() {
    let mut workspace = InteropWorkspace::start();
    workspace.assert_reference(
        (ACCOUNT_JAVA, "public long getBalance()", "getBalance"),
        (KOTLIN_CLIENT_KT, "else account.balance"),
    );
    workspace.assert_reference(
        (ACCOUNT_JAVA, "public void setBalance(", "setBalance"),
        (KOTLIN_CLIENT_KT, "account.balance = 100L"),
    );
    // A read is not a call of the setter, and a write is not a call of the getter.
    workspace.assert_no_reference(
        (ACCOUNT_JAVA, "public void setBalance(", "setBalance"),
        (KOTLIN_CLIENT_KT, "else account.balance"),
    );
    workspace.assert_no_reference(
        (ACCOUNT_JAVA, "public long getBalance()", "getBalance"),
        (KOTLIN_CLIENT_KT, "account.balance = 100L"),
    );
}

// ── Kotlin → Java: records ──────────────────────────────────────────────────

#[test]
fn kotlin_property_on_java_record_resolves_to_record_component() {
    let mut workspace = InteropWorkspace::start();
    workspace.assert_definition(
        (KOTLIN_CLIENT_KT, "moved.x + point.y", "x"),
        (POINT_JAVA, "public record Point("),
    );
}
