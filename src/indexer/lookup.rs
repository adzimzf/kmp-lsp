//! Lookup phase: query the index for symbol information.
//!
//! This module owns the "read path" of the indexer for symbol resolution:
//!
//! - [`Indexer::is_declared_in`]             — test if a name is declared in a file
//! - [`Indexer::find_definition`]            — resolve definition locations by name
//! - [`Indexer::find_definition_qualified`]  — resolve with optional dot-qualifier
//! - [`Indexer::file_symbols`]               — all symbols declared in a file
//! - [`Indexer::package_of`]                 — package declared in a file
//! - [`Indexer::declared_package_of`]        — package in which a name is declared
//! - [`Indexer::declared_parent_class_of`]   — enclosing class at declaration site
//! - [`Indexer::resolve_symbol_via_import`]  — resolve parent class / package via imports

use tower_lsp::lsp_types::*;

use super::infer::lambda::SCOPE_FUNCTIONS;
use super::{CallShape, Indexer, ResolvedType};
use crate::resolver::{resolve_symbol_in_type_scope, split_package_prefix, ResolveIo, TypeScope};
use crate::types::SymbolEntry;
use crate::StrExt;

/// Who owns a member declaration: a type it is declared in, or the receiver
/// type of an extension (which overrides nothing).
enum MemberOwner {
    Type(String),
    Extension(String),
}

/// Throttle counter for [`Indexer::jar_declaration_scope`]'s side-table
/// misalignment warning — see [`crate::util::throttled_warn`].
static JAR_SYMBOL_PACKAGES_MISALIGNED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

impl Indexer {
    /// Returns true if `name` has at least one definition location inside `uri`.
    pub(crate) fn is_declared_in(&self, uri: &Url, name: &str) -> bool {
        self.definitions
            .get(name)
            .map(|locs| {
                locs.iter()
                    .any(|l| self.file_table.url(l.file).is_some_and(|url| *url == *uri))
            })
            .unwrap_or(false)
    }

    /// Resolve definition locations for `name` (with optional dot-qualifier).
    #[allow(dead_code)]
    pub(crate) fn find_definition(&self, name: &str, from_uri: &Url) -> Vec<Location> {
        self.resolve_symbol(name, None, from_uri)
    }

    /// Resolve `name` as accessed through `qualifier`. When the receiver-scoped
    /// search finds nothing and `name` is a universal scope function
    /// (`SCOPE_FUNCTIONS`, e.g. `apply`/`let`) — a receiver-generic stdlib
    /// extension that can never be a member of any specific type — fall back
    /// to the same bare-name resolution a plain reference to that stdlib
    /// function would use. A receiver type that declares its own member by
    /// that name still wins, since the fallback only runs once the normal
    /// search has already failed.
    pub(crate) fn find_definition_qualified(
        &self,
        name: &str,
        qualifier: Option<&str>,
        from_uri: &Url,
    ) -> Vec<Location> {
        self.find_definition_qualified_with_io(name, qualifier, from_uri, false)
    }

    /// Like `find_definition_qualified` but never spawns `rg`/`fd` — see
    /// `resolve_symbol_index_only`'s own doc comment for why.
    pub(crate) fn find_definition_qualified_index_only(
        &self,
        name: &str,
        qualifier: Option<&str>,
        from_uri: &Url,
    ) -> Vec<Location> {
        self.find_definition_qualified_with_io(name, qualifier, from_uri, true)
    }

    /// `name` as a member of `receiver`'s type. The type's NAME is resolved in
    /// the file it was written in ([`ResolvedType::declaring_uri`]), not the
    /// caller's: an inferred receiver (`holder.model.text`) is spelled in the
    /// callee's file, which imports it — the caller usually does not.
    pub(crate) fn find_member_of_type(
        &self,
        name: &str,
        receiver: &ResolvedType,
        from_uri: &Url,
    ) -> Vec<Location> {
        self.find_member_of_type_with_io(name, receiver, from_uri, false)
    }

    /// Like `find_member_of_type` but never spawns `rg`/`fd`.
    pub(crate) fn find_member_of_type_index_only(
        &self,
        name: &str,
        receiver: &ResolvedType,
        from_uri: &Url,
    ) -> Vec<Location> {
        self.find_member_of_type_with_io(name, receiver, from_uri, true)
    }

    fn find_member_of_type_with_io(
        &self,
        name: &str,
        receiver: &ResolvedType,
        from_uri: &Url,
        index_only: bool,
    ) -> Vec<Location> {
        let type_path = receiver.type_path();
        let (package, bare_path) = split_package_prefix(&type_path);
        let type_scope = TypeScope {
            file: receiver.resolution_scope(from_uri),
            package,
        };
        self.find_qualified_in_type_scope(name, bare_path, &type_scope, from_uri, index_only)
    }

    fn find_definition_qualified_with_io(
        &self,
        name: &str,
        qualifier: Option<&str>,
        from_uri: &Url,
        index_only: bool,
    ) -> Vec<Location> {
        match qualifier {
            Some(qualifier) => self.find_qualified_in_type_scope(
                name,
                qualifier,
                &TypeScope::in_file(from_uri),
                from_uri,
                index_only,
            ),
            None if index_only => self.resolve_symbol_index_only(name, None, from_uri),
            None => self.resolve_symbol(name, None, from_uri),
        }
    }

    fn find_qualified_in_type_scope(
        &self,
        name: &str,
        qualifier: &str,
        type_scope: &TypeScope<'_>,
        from_uri: &Url,
        index_only: bool,
    ) -> Vec<Location> {
        let io = if index_only {
            ResolveIo::IndexOnly
        } else {
            ResolveIo::Full
        };
        let locations =
            resolve_symbol_in_type_scope(self, name, qualifier, type_scope, from_uri, io);
        if !locations.is_empty() {
            return locations;
        }
        if SCOPE_FUNCTIONS.contains(&name) {
            return if index_only {
                self.resolve_symbol_index_only(name, None, from_uri)
            } else {
                self.resolve_symbol(name, None, from_uri)
            };
        }
        locations
    }

    /// Resolve an unqualified call's callee name, filtering same-file
    /// candidates whose arity can't satisfy `shape` — see
    /// [`crate::resolver::resolve_callee_definition`]. Not part of
    /// `find_definition_qualified`'s signature: every other caller of that
    /// function has no call shape to offer, and this is the one goto-definition
    /// path that does.
    pub(crate) fn find_definition_for_call(
        &self,
        name: &str,
        uri: &Url,
        shape: crate::indexer::CallShape,
    ) -> Vec<Location> {
        crate::resolver::resolve_callee_definition(self, name, uri, shape)
    }

    /// Resolve `name(...)` as an implicit `this.name(...)` against
    /// `receiver_base` — see [`crate::resolver::resolve_implicit_receiver_callee`].
    /// A separate entry point rather than folded into `find_definition_for_call`:
    /// only callers that already know they're inside an extension function's
    /// body (via `parser::enclosing_extension_receiver_at`) have a receiver to
    /// offer, and it's tried only as a fallback after the plain bare-name
    /// lookup comes up empty.
    pub(crate) fn find_definition_for_implicit_receiver_call(
        &self,
        receiver_base: &str,
        name: &str,
        uri: &Url,
        shape: crate::indexer::CallShape,
    ) -> Vec<Location> {
        crate::resolver::resolve_implicit_receiver_callee(self, receiver_base, name, uri, shape)
    }

    /// The type that declares the member `name` reached through `receiver`,
    /// climbed to the base declaration when that member overrides another:
    /// the member's own class/interface, or an extension's receiver type.
    ///
    /// This is often a supertype of `receiver` — `flow.collect {}` on a
    /// `MutableStateFlow` reaches `SharedFlow.collect`, which overrides
    /// `Flow.collect` — so it, not `receiver`, is the identity a call site
    /// shares with every other caller of the same member. `None` when the
    /// member is not in the index. Never spawns `rg`/`fd`.
    ///
    /// A call's `shape` picks the overload it can reach: `continuation.resume(body)`
    /// on a `CancellableContinuation` is the stdlib `Continuation<T>.resume(value)`
    /// extension, not the two-parameter member `resume(value, onCancellation)`.
    pub(crate) fn member_declaring_type(
        &self,
        name: &str,
        receiver: &ResolvedType,
        shape: Option<CallShape>,
        from_uri: &Url,
    ) -> Option<String> {
        let candidates = self.find_member_of_type_index_only(name, receiver, from_uri);
        // An arity no candidate accepts says more about the index than the
        // call: keep every candidate rather than none.
        let candidates = match shape {
            Some(shape) => {
                let reachable =
                    super::shape_filter_locations(self, shape, candidates.clone()).resolved();
                if reachable.is_empty() {
                    candidates
                } else {
                    reachable
                }
            }
            None => candidates,
        };
        let (owner, location) = candidates
            .into_iter()
            .find_map(|location| Some((self.member_owner_at(name, &location)?, location)))?;
        match owner {
            MemberOwner::Extension(receiver_type) => Some(receiver_type),
            MemberOwner::Type(type_name) => {
                Some(self.base_declaring_type(name, type_name, location.uri.to_string()))
            }
        }
    }

    /// Of `candidates` for a bare (receiver-less) name used at `position` in
    /// `uri`, the ones Java binds it to: members of the innermost class
    /// enclosing `position` that has any, else of the next class out. A
    /// method of a class *nested* in the caller's is not in scope there, even
    /// though it is declared in the same file. Candidates are returned
    /// unchanged when no enclosing class declares one of them (inherited or
    /// imported targets), or when `uri` isn't a Java file.
    pub(crate) fn narrow_to_enclosing_java_class(
        &self,
        uri: &Url,
        position: Position,
        candidates: Vec<Location>,
    ) -> Vec<Location> {
        if candidates.len() < 2 || crate::Language::from_path(uri.path()) != crate::Language::Java {
            return candidates;
        }
        let Some(file_data) = self.files.get(uri.as_str()) else {
            return candidates;
        };
        let is_type = |symbol: &&SymbolEntry| {
            matches!(
                symbol.kind,
                SymbolKind::CLASS
                    | SymbolKind::INTERFACE
                    | SymbolKind::ENUM
                    | SymbolKind::STRUCT
                    | SymbolKind::OBJECT
            )
        };
        let mut enclosing: Vec<&SymbolEntry> = file_data
            .symbols
            .iter()
            .filter(is_type)
            .filter(|symbol| symbol.range.start <= position && position <= symbol.range.end)
            .collect();
        // Innermost first: a nested class starts after the class around it.
        enclosing.sort_by_key(|symbol| std::cmp::Reverse(symbol.range.start));
        let symbol_of = |candidate: &Location| -> Option<&SymbolEntry> {
            if candidate.uri != *uri {
                return None;
            }
            file_data
                .symbols
                .iter()
                .find(|symbol| symbol.selection_range == candidate.range)
        };
        // Only a member name is bound this way. A type name (`Node` inside
        // `class Node`) names the type, never one of its constructors.
        let names_a_type = candidates.iter().filter_map(symbol_of).any(|symbol| {
            is_type(&symbol) || symbol.container.as_deref() == Some(symbol.name.as_str())
        });
        if names_a_type {
            return candidates;
        }
        let container_of =
            |candidate: &Location| symbol_of(candidate).and_then(|symbol| symbol.container.clone());
        for class in enclosing {
            let members: Vec<Location> = candidates
                .iter()
                .filter(|candidate| container_of(candidate).as_deref() == Some(class.name.as_str()))
                .cloned()
                .collect();
            if !members.is_empty() {
                return members;
            }
        }
        candidates
    }

    /// The symbol named `name` declared on `location`'s line, in a workspace
    /// or JAR file. Matched by line and name, like [`Self::member_owner_at`].
    pub(crate) fn symbol_declared_at(
        &self,
        name: &str,
        location: &Location,
    ) -> Option<SymbolEntry> {
        let file_data = self
            .files
            .get(location.uri.as_str())
            .or_else(|| self.jar_files.get(location.uri.as_str()))?;
        file_data
            .symbols
            .iter()
            .find(|symbol| {
                symbol.name == name
                    && symbol.selection_range.start.line == location.range.start.line
            })
            .cloned()
    }

    /// Who owns the declaration of `name` at `location`, in a workspace or
    /// JAR file.
    ///
    /// Matched by line and name, not the exact range: a JAR symbol's indexed
    /// range starts at column 0 of its synthetic line, while a resolved
    /// location points at the name inside the signature (`val value`).
    fn member_owner_at(&self, name: &str, location: &Location) -> Option<MemberOwner> {
        let file_data = self
            .files
            .get(location.uri.as_str())
            .or_else(|| self.jar_files.get(location.uri.as_str()))?;
        let symbol = file_data.symbols.iter().find(|symbol| {
            symbol.name == name && symbol.selection_range.start.line == location.range.start.line
        })?;
        let extension_receiver = symbol.extension_receiver_type();
        if !extension_receiver.is_empty() {
            return Some(MemberOwner::Extension(extension_receiver.to_owned()));
        }
        symbol
            .container
            .clone()
            .or_else(|| self.enclosing_class_at(&location.uri, location.range.start.line))
            .map(MemberOwner::Type)
    }

    /// The topmost supertype of `type_name` (declared in `type_uri`) that
    /// also declares a member `name` — the declaration every override of
    /// `name` below it overrides. `type_name` itself when none does.
    fn base_declaring_type(&self, name: &str, type_name: String, type_uri: String) -> String {
        /// Bounds the climb on a malformed (cyclic) hierarchy.
        const MAX_OVERRIDE_HOPS: usize = 16;
        let mut base = (type_name, type_uri);
        for _ in 0..MAX_OVERRIDE_HOPS {
            let nearest_declaring_supertype = crate::resolver::walk_hierarchy_breadth_first(
                self,
                &base.0,
                &base.1,
                crate::types::CallerContext::default(),
                12,
                crate::resolver::MAX_SYNC_JAR_PROMOTIONS_PER_HIERARCHY_WALK,
                |indexer, super_name, super_uri, _| {
                    if indexer.declares_member(super_uri, super_name, name) {
                        vec![(super_name.to_owned(), super_uri.to_owned())]
                    } else {
                        vec![]
                    }
                },
            )
            .into_iter()
            .next();
            match nearest_declaring_supertype {
                Some(supertype) if supertype != base => base = supertype,
                _ => break,
            }
        }
        base.0
    }

    /// Whether the type `type_name` declared in `uri` declares a member
    /// `member` itself (not an extension).
    pub(crate) fn declares_member(&self, uri: &str, type_name: &str, member: &str) -> bool {
        let Some(file_data) = self.files.get(uri).or_else(|| self.jar_files.get(uri)) else {
            return false;
        };
        file_data.symbols.iter().any(|symbol| {
            symbol.name == member
                && symbol.container.as_deref() == Some(type_name)
                && symbol.extension_receiver_type().is_empty()
        })
    }

    /// All symbols declared in the given file (for `documentSymbol`).
    pub(crate) fn file_symbols(&self, uri: &Url) -> Vec<SymbolEntry> {
        self.files
            .get(uri.as_str())
            .map(|d| d.symbols.clone())
            .unwrap_or_default()
    }

    /// Return the package declared in the given file, if any.
    pub(crate) fn package_of(&self, uri: &Url) -> Option<String> {
        self.files.get(uri.as_str())?.package.clone()
    }

    /// The fully qualified names of the top-level types called `name`, from
    /// the workspace, library sources and compiled libraries: what an import
    /// of `name` could name. Nested types (`ProcessBuilder.Redirect.Type`) are
    /// left out, since a plain `import package.Name;` can't reach them, and so
    /// are package-private ones.
    pub(crate) fn top_level_types_named(&self, name: &str) -> Vec<ImportableType> {
        let mut types: Vec<ImportableType> = Vec::new();
        for location in self.lookup_definitions(name) {
            let Some(package) = self.top_level_type_package(name, &location) else {
                continue;
            };
            let fqn = format!("{package}.{name}");
            if types.iter().any(|known| known.fqn == fqn) {
                continue;
            }
            types.push(ImportableType {
                fqn,
                in_workspace: location.uri.scheme() == "file",
            });
        }
        types
    }

    /// The package of the type `name` declared at `location`, when that
    /// declaration is a top-level type.
    fn top_level_type_package(&self, name: &str, location: &Location) -> Option<String> {
        let uri = location.uri.as_str();
        if let Some(file_data) = self.files.get(uri) {
            let declared = file_data.symbols.iter().any(|symbol| {
                symbol.name == name
                    && symbol.selection_range == location.range
                    && is_importable_type(symbol)
            });
            return declared
                .then(|| file_data.package.clone())
                .flatten()
                .filter(|package| !package.is_empty());
        }
        // A compiled library's symbols sit one per synthetic line, and the
        // side table holds each one's package at the same index.
        let symbol_index = location.range.start.line as usize;
        let file_data = self.jar_files.get(uri)?;
        let symbol = file_data.symbols.get(symbol_index)?;
        if symbol.name != name || !is_importable_type(symbol) {
            return None;
        }
        self.jar_symbol_packages
            .get(uri)
            .and_then(|packages| packages.get(symbol_index).cloned())
            .or_else(|| file_data.package.clone())
            .filter(|package| !package.is_empty())
    }

    /// If `name` resolves to a compiled/sources JAR definition, return its
    /// `(package, container)` — e.g. `("androidx.compose.runtime", None)` for the
    /// top-level `remember`, or `("androidx.compose.ui", Some("Modifier"))` for a
    /// member declared inside `Modifier`. Returns `None` for workspace-only names.
    ///
    /// The package is read from the per-symbol `jar_symbol_packages` side table
    /// (index-aligned with the symbol's synthetic line number), falling back to the
    /// per-jar inferred package. The container is read from the JAR symbol entry.
    pub(crate) fn jar_declaration_scope(&self, name: &str) -> Option<(String, Option<String>)> {
        let mut unbudgeted = usize::MAX;
        crate::indexer::jar::ensure_jar_definitions_for(self, name, &mut unbudgeted);
        let locs = self.jar_definitions.get(name)?;
        for loc in locs.iter() {
            let uri_str = loc.uri.as_str();
            let symbol_index = loc.range.start.line as usize;
            let package = self
                .jar_symbol_packages
                .get(uri_str)
                .and_then(|packages| {
                    let entry = packages
                        .get(symbol_index)
                        .filter(|p| !p.is_empty())
                        .cloned();
                    // `jar_symbol_packages` is populated index-aligned with the jar's
                    // `FileData.symbols` (synthetic line number == symbol index) —
                    // see `jar.rs`'s population comment. A missing map entry for
                    // `uri_str` is a legitimate fallback (pre-v8 sidecars never
                    // recorded per-symbol packages at all), but an in-bounds map
                    // whose vector is too short for `symbol_index` means the two
                    // "index-aligned" collections drifted apart — the same class of
                    // bug already caused `padding` to vanish from chained-call
                    // completion once (see the population-site comment).
                    if entry.is_none() && symbol_index >= packages.len() {
                        crate::util::throttled_warn(&JAR_SYMBOL_PACKAGES_MISALIGNED, 5, || {
                            format!(
                                "jar_symbol_packages misaligned for {uri_str}: symbol index \
                                 {symbol_index} out of bounds (side table has {} entries) — \
                                 falling back to the per-jar inferred package, which can be \
                                 wrong for a multi-package jar",
                                packages.len(),
                            )
                        });
                    }
                    entry
                })
                .or_else(|| {
                    self.jar_files
                        .get(uri_str)
                        .and_then(|fd| fd.package.clone())
                });
            let Some(package) = package else {
                continue;
            };
            // A top-level Kotlin declaration is registered in `qualified` under
            // `pkg.name`, while a member is registered under `pkg.Container.name`.
            // The symbol's stored `container` for a top-level fun/val is its JVM
            // facade class (e.g. `ComposablesKt`), which is not a usable Kotlin
            // type qualifier — so report `None` for top-level symbols.
            let is_top_level = self.qualified.contains_key(&format!("{package}.{name}"));
            let container = if is_top_level {
                None
            } else {
                self.jar_files.get(uri_str).and_then(|fd| {
                    fd.symbols
                        .get(symbol_index)
                        .and_then(|s| s.container.clone())
                })
            };
            return Some((package, container));
        }
        None
    }

    /// Workspace files (`file://`, non-library) that import `fqn` — either via an
    /// explicit import of the symbol or a star import of its declaring package.
    ///
    /// `fqn` is the fully-qualified name, e.g. `"androidx.compose.runtime.remember"`.
    /// Reuses [`crate::types::ImportEntry::covers`] for the import-matching rules.
    /// Library/JAR files are excluded so results are workspace usages only.
    pub(crate) fn workspace_importers_of(&self, fqn: &str) -> Vec<Url> {
        let Some((def_pkg, symbol_name)) = fqn.rsplit_once('.') else {
            return Vec::new();
        };
        let mut result = Vec::new();
        for entry in self.files.iter() {
            let uri_str = entry.key();
            if self.library_uris.contains(uri_str) {
                continue;
            }
            if !uri_str.starts_with("file://") {
                continue;
            }
            // Library source files (sources-jars *and* configured `sourcePaths`) are
            // indexed as `file://` but are not workspace code — they must never count
            // as importers. `library_uris` only catches the former, so also exclude
            // anything classified into the Library source set.
            if entry.value().source_set == crate::types::SourceSet::Library {
                continue;
            }
            let imports_symbol = entry
                .value()
                .imports
                .iter()
                .any(|imp| imp.covers(def_pkg, symbol_name));
            if imports_symbol {
                if let Ok(url) = Url::parse(uri_str) {
                    result.push(url);
                }
            }
        }
        result
    }

    /// Return the package in which `name` is declared, by looking up its
    /// definition locations and reading the `package` field of those files.
    pub(crate) fn declared_package_of(&self, name: &str, preferred_uri: &Url) -> Option<String> {
        let locs = self.definitions.get(name)?;
        // Prefer the declaration in the current file (mirrors declared_parent_class_of).
        for loc in locs.iter() {
            let Some(url) = self.file_table.url(loc.file) else {
                continue;
            };
            if *url == *preferred_uri {
                if let Some(f) = self.files.get(url.as_str()) {
                    if let Some(pkg) = &f.package {
                        return Some(pkg.clone());
                    }
                }
            }
        }
        // Fall back to first definition in any file.
        for loc in locs.iter() {
            let Some(url) = self.file_table.url(loc.file) else {
                continue;
            };
            if let Some(f) = self.files.get(url.as_str()) {
                if let Some(pkg) = &f.package {
                    return Some(pkg.clone());
                }
            }
        }
        None
    }

    /// If `name` is declared as an inner/nested class, return the name of its
    /// enclosing class at the declaration site in `preferred_uri` (if found there),
    /// otherwise the first definition site.
    pub(crate) fn declared_parent_class_of(
        &self,
        name: &str,
        preferred_uri: &Url,
    ) -> Option<String> {
        let locs = self.definitions.get(name)?;
        // Try declaration in the preferred (current) file first.
        for loc in locs.iter() {
            let Some(url) = self.file_table.url(loc.file) else {
                continue;
            };
            if *url == *preferred_uri {
                return self.enclosing_class_at(&url, loc.range.start.line);
            }
        }
        // Fall back to first definition in any file.
        for loc in locs.iter() {
            let Some(url) = self.file_table.url(loc.file) else {
                continue;
            };
            if let Some(parent) = self.enclosing_class_at(&url, loc.range.start.line) {
                return Some(parent);
            }
        }
        None
    }

    /// Scan imports in `uri` for `name` and return (parent_class, declared_pkg)
    /// as resolved from the import statement.  E.g.:
    ///   `import com.example.DashboardViewModel.Effect`
    ///   → parent_class = Some("DashboardViewModel"), pkg = Some("com.example.DashboardViewModel")
    ///
    ///   `import com.example.DashboardViewModel.*` (star import)
    ///   → if Effect is a known nested class of DashboardViewModel in the index,
    ///     parent_class = Some("DashboardViewModel"), pkg = Some("com.example.DashboardViewModel")
    pub(crate) fn resolve_symbol_via_import(
        &self,
        uri: &Url,
        name: &str,
    ) -> (Option<String>, Option<String>) {
        let file = match self.files.get(uri.as_str()) {
            Some(f) => f,
            None => return (None, None),
        };
        for line in file.lines.iter() {
            let t = line.trim();
            if !t.starts_with("import ") {
                continue;
            }
            // Handle `import a.b.c.Name` and `import a.b.c.Name as Alias`
            let import_path = t["import ".len()..].split_whitespace().next().unwrap_or("");
            let segments: Vec<&str> = import_path.split('.').collect();
            // Last segment should match `name` (or be `*`).
            let last = *segments.last().unwrap_or(&"");
            if last != name && last != "*" {
                continue;
            }

            // Star import: `import a.b.ClassName.*`
            // If the segment before `*` is an uppercase class name and `name` is a known
            // nested class of that class, resolve to it.
            if last == "*" && segments.len() >= 2 {
                let maybe_parent = segments[segments.len() - 2];
                if maybe_parent.starts_with_uppercase() {
                    // Check if `name` is actually declared inside `maybe_parent`.
                    let decl_uri = self.definitions.get(name).and_then(|locs| {
                        locs.iter().find_map(|loc| {
                            let url = self.file_table.url(loc.file)?;
                            if self
                                .enclosing_class_at(&url, loc.range.start.line)
                                .as_deref()
                                == Some(maybe_parent)
                            {
                                Some((*url).clone())
                            } else {
                                None
                            }
                        })
                    });
                    if let Some(decl_uri) = decl_uri {
                        let pkg = self
                            .files
                            .get(decl_uri.as_str())
                            .and_then(|f| f.package.clone())
                            .map(|p| format!("{p}.{maybe_parent}"));
                        return (Some(maybe_parent.to_string()), pkg);
                    }
                }
                continue;
            }

            // Found a matching import. The declared package is everything up to (not incl.) `name`.
            // The parent class is the segment immediately before `name` if it starts uppercase.
            if last == name && segments.len() >= 2 {
                let pkg = segments[..segments.len() - 1].join(".");
                let parent = segments
                    .get(segments.len() - 2)
                    .filter(|s| s.starts_with_uppercase())
                    .map(|s| s.to_string());
                return (parent, Some(pkg));
            }
        }
        (None, None)
    }
}

pub(crate) fn symbol_kw(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::CLASS => "class",
        SymbolKind::INTERFACE => "interface",
        SymbolKind::FUNCTION => "fun",
        SymbolKind::METHOD => "fun",
        SymbolKind::VARIABLE => "var",
        SymbolKind::CONSTANT => "val",
        SymbolKind::OBJECT => "object",
        SymbolKind::TYPE_PARAMETER => "typealias",
        SymbolKind::ENUM => "enum class",
        SymbolKind::FIELD => "field",
        _ => "symbol",
    }
}

pub(crate) fn symbol_kw_for_lang(kind: SymbolKind, lang: &str) -> &'static str {
    // Resolve to a Language and delegate to its provider so keyword logic stays
    // in one place. Unknown lang strings fall back to Kotlin (default path).
    let language = match lang {
        "java" => crate::Language::Java,
        "swift" => crate::Language::Swift,
        _ => crate::Language::Kotlin,
    };
    language.parser().symbol_keyword(kind)
}

pub(crate) fn lang_str(path: &str) -> &'static str {
    crate::Language::from_path(path).code_fence()
}

// ─── Generic type parameter substitution ─────────────────────────────────────

/// Apply a type-parameter substitution map to a type string.
///
/// Only replaces whole-word occurrences (character boundaries), so `EventType`
/// is not partially replaced when substituting `Event`. A segment of a
/// qualified path (`model.A`, `A.Nested`) names a class, never a type
/// parameter, and stays as written; `T.() -> R` still replaces its `T`.
///
/// Re-exported as `crate::indexer::apply_type_subst` for use by inlay_hints,
/// backend handlers, and the resolution module.
pub(crate) fn apply_type_subst(
    sig: &str,
    subst: &std::collections::HashMap<String, String>,
) -> String {
    if subst.is_empty() {
        return sig.to_owned();
    }
    let mut result = String::with_capacity(sig.len() + 16);
    let chars: Vec<char> = sig.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_alphabetic() || ch == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let ident: String = chars[start..i].iter().collect();
            let after_dot = start > 0 && chars[start - 1] == '.';
            let before_member = chars.get(i) == Some(&'.')
                && chars
                    .get(i + 1)
                    .is_some_and(|next| next.is_alphabetic() || *next == '_');
            let is_qualified_path_segment = after_dot || before_member;
            if is_qualified_path_segment {
                result.push_str(&ident);
            } else if let Some(replacement) = subst.get(&ident) {
                result.push_str(replacement);
                // `T?` with `T = Apple?` is `Apple?`, not `Apple??`.
                let already_nullable = replacement.ends_with('?') && chars.get(i) == Some(&'?');
                if already_nullable {
                    i += 1;
                }
            } else {
                result.push_str(&ident);
            }
        } else {
            result.push(ch);
            i += 1;
        }
    }
    result
}

#[cfg(test)]
#[path = "lookup_tests.rs"]
mod tests;

/// A top-level type an import can name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportableType {
    pub fqn: String,
    /// Declared in the workspace's own sources rather than in a library.
    pub in_workspace: bool,
}

/// A top-level type other packages can see.
fn is_importable_type(symbol: &SymbolEntry) -> bool {
    let hidden = matches!(
        symbol.visibility,
        crate::types::Visibility::Private | crate::types::Visibility::Package
    );
    is_type_kind(symbol.kind) && symbol.container.is_none() && !hidden
}

fn is_type_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::CLASS
            | SymbolKind::INTERFACE
            | SymbolKind::ENUM
            | SymbolKind::STRUCT
            | SymbolKind::OBJECT
    )
}
