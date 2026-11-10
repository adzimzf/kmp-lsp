# Features

All features work immediately, even in a brand-new workspace — an `rg`/`fd` fallback
handles lookups before the background index finishes, then results get faster and
more precise (superclass hierarchy, cross-file resolution) as indexing completes.

## LSP capabilities

| LSP capability | Notes |
|---|---|
| `textDocument/definition` | Index lookup → superclass hierarchy → `rg` fallback |
| `textDocument/hover` | Declaration kind, source line, lambda param types, Kotlin stdlib signatures |
| `textDocument/documentSymbol` | All symbols in the current file (outline view) |
| `textDocument/completion` | Dot-completion (`it.`, `this.`, named params), bare-word, stdlib entries |
| `completionItem/resolve` | Lazy KDoc/Javadoc + signature on item selection; keeps initial list fast |
| `textDocument/references` | Project-wide `rg --word-regexp` + in-memory scan of open buffers, Java ↔ Kotlin spellings, and usages inside library sources jars |
| `textDocument/signatureHelp` | Active function signature + highlighted parameter as you type |
| `textDocument/rename` | Renames symbol across all files via `WorkspaceEdit`; index updated via file watcher |
| `textDocument/foldingRange` | Brace-based region folds + consecutive comment block folds |
| `textDocument/selectionRange` | Expand selection through the enclosing syntax nodes: name → call → statement → block → method → class → file |
| `textDocument/inlayHint` | Type hints for lambda `it`, named lambda params, `this`, untyped `val`/`var`; Java parameter names on literal arguments (`add(number: 1)`) |
| `textDocument/semanticTokens/full` | Two-phase: Phase 1 CST classification + Phase 2 cross-file resolution. Kotlin, Java, Swift. Java references: calls, types (class/interface/enum/annotation/type parameter), constructors, fields and enum constants |
| `textDocument/publishDiagnostics` | Syntax errors from tree-sitter (ERROR/MISSING nodes) — not type checking |
| `textDocument/implementation` | Transitive subtype lookup (interface → all implementing classes, BFS; Java anonymous classes included) |
| `textDocument/documentHighlight` | Highlights all in-file occurrences; declaration sites marked WRITE, usages READ |
| `workspace/symbol` | Fuzzy substring search; supports dot-qualified queries for extension functions |
| `$/progress` | Spinner while workspace is indexed; non-blocking |
| `textDocument/didSave` | Re-indexes the saved file so external formatters/codegen are picked up |
| `workspace/executeCommand` `vscode.java.*` | java-debug's jdtls delegate commands (`startDebugSession`, `resolveMainClass`, `resolveClasspath`, …) — Java and Kotlin/JVM debugging, see [Debugging](debugging.md) |

## Not yet implemented

The following LSP features are commonly supported by full language servers and
are candidates for future work. Rough effort estimates assume tree-sitter
parsing only (no type resolution):

| LSP capability | Effort | Notes |
|---|---|---|
| ~~`textDocument/semanticTokens/full`~~ | ~~High~~ | ✅ **Implemented in 0.11.0.** Two-phase pipeline: Phase 1 (CST classification) + Phase 2 (cross-file index resolution). Kotlin, Java, Swift. |
| `textDocument/prepareCallHierarchy` + `callHierarchy/incomingCalls` + `callHierarchy/outgoingCalls` | Medium | Call tree viewer. Would need `rg`-based caller search similar to `references`. |
| ~~`textDocument/selectionRange`~~ | ~~Medium~~ | ✅ **Implemented.** Expands by CST node boundaries. |
| `completionItem` — `deprecated` tag (`CompletionItemTag::DEPRECATED`) | Medium | Strikethrough for `@Deprecated`/`@deprecated` symbols. Requires detecting the annotation at index time and storing a flag on `SymbolEntry`. |
| `completionItem` — `label_details` | Medium | Inline param list + right-aligned return type in the completion list (RA-style). Would require splitting `SymbolEntry.detail` into params + return type at parse time. |
| `textDocument/gotoDeclaration` | Trivial | Identical to `goto_definition` for our use-case (no separate declaration/definition concept in Kotlin/Java). |
| ~~`textDocument/typeDefinition`~~ | ~~Medium~~ | ✅ **Implemented for Java.** Go-to-definition, then the declaration's written type (`src/features/type_definition.rs`). Kotlin declarations answer nothing yet. |
| `textDocument/codeAction` — quick-fixes | Medium | Currently only "introduce local variable" and "add import alias" are implemented. Missing: add missing import, generate override stubs, suppress warning. |
| `textDocument/onTypeFormatting` | Low | Auto-indent / brace matching as you type. |
| `textDocument/formatting` | Low | Delegate to `ktfmt` / `google-java-format` subprocess if available on `$PATH`. |

## Known UX gaps (scouted from JetBrains kmp-lsp, 2026-05)

Small improvements identified by comparing against the JetBrains reference implementation. None require type resolution.

| Area | What to change | Effort |
|---|---|---|
| **Hover — backtick identifiers** | Change ` ```kotlin ` to ` ````kotlin ` (quadruple backtick fence) in `src/backend/format.rs`. Kotlin identifiers can contain backticks (`` `my fun` ``); a triple-backtick fence breaks the Markdown block. | Trivial |
| **Completion item kind — METHOD** | `symbol_kind_to_completion` in `src/resolver/complete.rs` maps both `SymbolKind::FUNCTION` and `SymbolKind::METHOD` to `CompletionItemKind::FUNCTION`. Should map METHOD → `CompletionItemKind::METHOD` so editors show the correct icon. | Trivial |
| **FoldingRange — import block** | Detect `import` blocks (consecutive lines starting with `import`) and emit `FoldingRangeKind::Imports` instead of `Region`. Currently all folds are `Region`. (`src/backend/handlers.rs`) | Low |
| **FoldingRange — block comments** | Detect `/* … */` multi-line comments and fold with `FoldingRangeKind::Comment`. Currently only `//` line-comment blocks are folded. | Low |
| **FoldingRange — collapsedText** | Set `collapsed_text` on every fold range (e.g. `"..."` for blocks, `"imports"` for import folds). Improves editor display when a region is collapsed. | Trivial |

## CLI subcommands

`kmp-lsp` ships with a standalone CLI in addition to the LSP server.

```
kmp-lsp sources [--root <dir>] [--json]
```
Lists every source root that would be auto-discovered for a project (from `workspace.json` or standard Gradle/Maven build layout). Marks which paths exist on disk. Run this to verify the indexer will find your sources without starting the server.

If source roots are missing, the command suggests `extract-sources` as a next step.

```
kmp-lsp refs <name> [--no-deps] [--exclude-imports] [-C <n>] [--json] [--root <dir>]
kmp-lsp hover <file> <line> <col> [--no-deps] [--json] [--root <dir>]
```
In index mode, `find`, `refs` and `hover` also load the project's dependencies like the LSP does: compiled JARs (Gradle cache, Android SDK, dependency-sync jars, configured `jarPaths`; read by the `kmp-jar-indexer` sidecar) and library sources jars (no sidecar needed), so library types, members and hierarchies resolve. `--no-deps` skips them for a faster, workspace-only index. `diagnose` always loads them.

```
kmp-lsp complete <file> <line> [--dot] [--eol] [--no-stdlib] [--json] [--root <dir>]
```
Returns completion candidates for the cursor position without starting the LSP daemon — useful for shell integrations, editor plugins, and testing.

| Flag | Description |
|---|---|
| `--dot` | Auto-place cursor immediately after the last `.` on the line |
| `--eol` | Auto-place cursor at the end of the line |
| `--no-stdlib` | Skip `~/.kmp-lsp/sources` for project-only results (~5× faster) |
| `--json` | Output as JSON `[{label, kind, detail?, import?}]` |

Output (plain text):
```
Column     (function)  fun Column(modifier: Modifier, content: @Composable () -> Unit)
Row        (function)  fun Row(modifier: Modifier, content: @Composable () -> Unit)
```

```
kmp-lsp extract-sources [PATTERN…] [OPTIONS]
```
Extracts `*-sources.jar` files from the Gradle module cache to disk (`~/.kmp-lsp/sources`).

> **Note:** As of v0.21.0 this step is usually unnecessary. `kmp-lsp` now auto-mounts `*-sources.jar` files in-memory at startup directly from `~/.gradle/caches`. Hover docs, completions, and go-to-definition into library code all work automatically as long as a `*-sources.jar` is present in the Gradle cache (populated by a normal Gradle sync).
>
> Use `extract-sources` only if: (a) Gradle sources are not cached locally and you want to pre-populate them from a tarball, or (b) you need a persistent on-disk copy for offline use.

| Option | Default | Description |
|---|---|---|
| `PATTERN…` | (all) | Substring filter on artifact path, e.g. `androidx.compose` `org.jetbrains.kotlin` |
| `--gradle-home <dir>` | `$GRADLE_USER_HOME` or `~/.gradle` | Gradle home directory |
| `--output <dir>` | `~/.kmp-lsp/sources` | Extraction root |
| `--dry-run` | off | Print what would be extracted; write nothing |

**Check auto-discovered source roots:**

```sh
kmp-lsp sources --root ./android
```

Android SDK, `workspace.json`, and the Gradle cache are all picked up automatically. No manual extraction needed.

The extractor deduplicates by artifact — when multiple versions are cached, only the latest is extracted.

**No build system?** For projects without a Gradle cache (Make/Bazel/manual builds), set `jarPaths` in `workspace.json` to point the indexer directly at your compiled dependency jars — files or directories, `<WORKSPACE>` and relative paths supported, indexed in addition to the Gradle cache:

```json
{ "jarPaths": ["<WORKSPACE>/build/libs", "/opt/deps/some-library.jar"] }
```

The same list can be passed via LSP `initializationOptions.indexingOptions.jarPaths`. Hover docs come from a sibling `*-sources.jar` when present; otherwise you still get signatures, completions, and go-to-definition to the compiled symbol.

## What gets indexed

**Kotlin:** `class`, `interface`, `object`, `fun`, `val`, `var`, `typealias`, constructor parameters, enum entries  
**Java:** `class`, `interface`, `enum`, `method`, `field`, `enum_constant`  
**Swift:** `class`, `struct`, `enum`, `protocol`, `func`, `let`, `var`, `typealias`, `extension`, `init`, enum cases

## Resolution chain

Go-to-definition resolves symbols in this order:

1. **Local file** — indexed symbols in the same file
2. **Local variables / parameters** — line-scanned, catches un-annotated `fun` params
3. **Explicit imports** — exact FQN lookup, then package-filtered index, then `fd` on-demand
4. **Same package** — symbols in files sharing the same `package` declaration
5. **Star imports** — `import com.example.*` checked in the package dir
6. **Superclass hierarchy** — inherited methods from `extends`/`implements`/Kotlin delegation specifiers, up to 4 levels deep, cycle-safe
7. **Project-wide `rg`** — last resort; always finds symbols not yet indexed

`this.member` searches the current class + its supers.  
`super.member` skips the current class and walks the hierarchy directly.

## Completion details

- **Dot-completion** (`repo.`) — resolves the variable's declared type, finds the matching file, returns its public members. Private members are hidden.
- **Bare-word completion** — matches symbols from the current file and the workspace index by prefix (case-aware: lowercase prefix → lowercase suggestions first).
- **Kotlin stdlib** — scope functions (`run`, `apply`, `let`, `also`, `with`), collection extensions (`map`, `filter`, `find`, …), string extensions, and nullable helpers all appear in completion with proper signatures. They sort after project symbols.
- **Lazy loading** — files beyond the initial index limit are parsed on-demand the first time you trigger completion on one of their types.
- **Pre-warming** — when you open a file, its injected/constructor types are pre-warmed in the background so the first dot-completion is instant.
- **Live line scanning** — dot-detection uses the current document text (not the debounced index) so typing `.`, deleting it, and re-typing it always works correctly.
- **Visibility filtering** — `private`/`protected` members are hidden from dot-completion (except in the declaring file). `internal` members are shown for workspace code but hidden for library symbols, which are inaccessible from your module.
- **Deprecated filtering** — `@Deprecated` library symbols (from sources JARs and the compiled-JAR sidecar) are hidden. `@Deprecated` workspace symbols are kept but marked with the LSP `Deprecated` tag (struck-through) and sorted to the bottom, since you may still call your own code during a migration.
- **Overload dedup** — overloads of the same extension collapse to a single completion entry (the call form plus its `name { }` trailing-lambda form when applicable), matching IDE behaviour. Avoids duplicate `launch` / `launch { }` rows from version skew or binary-compat shims; signature help disambiguates the overloads.

## Completion ranking

Completions are scored by match quality:

| Score | Match type | Example |
|---|---|---|
| 0 | Exact prefix (case-insensitive) | `Col` → **Col**umn |
| 1 | CamelCase acronym | `CB` → **C**olumn**B**utton |
| 2 | Substring (same-file/package only) | `View` → RecyclerView |

Results are capped at 500 items; `isIncomplete: true` is returned so the client re-queries as you type.

**Context-aware filtering:**
- Lowercase prefix → only functions, vars, params
- Uppercase prefix → only classes, objects, types
- `@` prefix → only annotation/class kinds
- Cross-package symbols require prefix ≥ 2 characters

## Auto-import

When completing an unimported symbol:

- Start typing a class name (uppercase, ≥ 2 chars) → candidates appear from all indexed files including `sourcePaths`
- Select a candidate → symbol inserted **and** `import pkg.ClassName` added at the correct position
- Same-named classes from different packages appear as separate items with the package in the detail column
- Already-imported symbols appear without a duplicate edit
- Star imports (`import pkg.*`) are respected — no redundant explicit import added

## Configuration

### Workspace root

Resolved in order:

1. `KMP_LSP_WORKSPACE_ROOT` env var
2. LSP client `rootUri` / `workspaceFolders`
3. `~/.config/kmp-lsp/workspace` file (for clients that send no root)

### Ignore patterns

```toml
# ~/.config/helix/languages.toml
[language-server.kmp-lsp.config.indexingOptions]
ignorePatterns = ["bazel-*", "build/**", "third-party/**"]
```

Patterns follow gitignore glob rules and apply to both `fd` and `walkdir` fallback.

#### Ignore pattern semantics

| Pattern | Matches |
|---|---|
| `bazel-*` | Any dir/file named `bazel-*` at **any depth** |
| `third-party/**` | Everything inside `third-party/` relative to workspace root |
| `/abs/path/**` | Absolute path — normalized to relative before matching |

Patterns apply to both `fd` (fast path) and the `walkdir` fallback, and filter the warm-start cached manifest so newly added patterns take effect without clearing the cache.

### Source paths

Library sources are resolved automatically — no manual config needed in most cases:

| Source | How it's discovered |
|---|---|
| Android SDK (`Activity`, `Context`, …) | `sdk.dir` in `local.properties` → `$ANDROID_HOME` → `$ANDROID_SDK_ROOT` |
| Gradle library sources (Compose, coroutines, …) | `*-sources.jar` files auto-mounted from `~/.gradle/caches` at startup |
| The project's own dependencies (Gradle or Maven) | [Dependency sync](#dependency-sync) asks the build tool, which downloads missing jars and sources |
| IntelliJ/Android Studio project roots | `workspace.json` at project root (exported by IDE) |
| Standard Gradle/Maven layouts | `src/main/kotlin`, `src/test/kotlin`, per-module subprojects |

**`workspace.json`** — JetBrains IDEs export this file to the project root. It describes every module's source roots and lets you override library source directories:

```json
{
  "sourcePaths": [
    "<WORKSPACE>/custom-stubs",
    "/absolute/path/to/generated-sources"
  ]
}
```

When `sourcePaths` is present (even as `[]`), it overrides the `~/.kmp-lsp/sources` default. Use `[]` to disable all library sources for a specific project.

**`jarPaths`** — for projects **without a Gradle cache** (Make/Bazel/manual builds), where the automatic `~/.gradle/caches` scan finds nothing, point the indexer directly at your compiled dependency jars. Entries may be `.jar`/`.aar` files **or directories** (recursively expanded); `<WORKSPACE>` and relative paths resolve against the project root. These are indexed *in addition to* anything found in the Gradle cache.

```json
{
  "jarPaths": [
    "<WORKSPACE>/build/libs",
    "/opt/deps/some-library.jar"
  ]
}
```

Hover docs for these come from a sibling `*-sources.jar` when one is present next to the jar; otherwise you still get signatures, completions, and go-to-definition to the compiled symbol.

**Manual override** via LSP config (for custom stubs or generated code):

```toml
# ~/.config/helix/languages.toml
[language-server.kmp-lsp.config.indexingOptions]
sourcePaths = ["buildSrc/src", "/path/to/generated-stubs"]
jarPaths = ["/opt/deps/some-library.jar", "build/libs"]
```

Source path files are indexed for hover and completions but excluded from `findReferences` and `rename`.

#### Source path behaviour

| Behaviour | `sourcePaths` files |
|---|---|
| Hover / go-to-definition | ✓ |
| Autocomplete | ✓ |
| `findReferences` | ✗ (excluded) |
| `rename` | ✗ (excluded) |

Paths can be absolute (including `~/…`) or relative to the workspace root. The full path is trusted — standard excludes (`.gradle`, `build`, `target`) are not applied.

- Single-hop: `ClassName`, `functionName`, `CONSTANT`
- Multi-hop field chains: `account.profile.email`
- Constructor parameter declarations (without `val`/`var`)
- Lambda parameters: `{ account -> account.name }` jumps to the `account ->` binding
- `this.method()` and `super.method()` qualifier handling
- Precise `fd --full-path` search uses the full package path from the import, not just the filename — dramatically faster in multi-module projects
- Cross-file fallback via `rg` for symbols not yet in the index

## Dependency sync

Like jdtls (Buildship/m2e) and IntelliJ, kmp-lsp asks the project's own build tool which libraries it depends on, and has it **download** them — sources jars included — so go-to-definition and find-references reach into library code even when no IDE ever fetched those sources.

| Build | What runs |
|---|---|
| Gradle (`settings.gradle(.kts)` / `build.gradle(.kts)` at the root) | `./gradlew` (or `gradle`) with a small init script: resolves every project's compile/runtime classpath, requests each module's sources jar (downloaded into `~/.gradle/caches`) and lists the paths |
| Maven (`pom.xml` at the root) | `./mvnw` (or `mvn`) `dependency:sources dependency:list` — downloads into `~/.m2/repository` |

- **Automatic, in the background.** Runs on startup and again when a build file changes — `build.gradle(.kts)`, `settings.gradle(.kts)`, `gradle.properties`, `*.versions.toml`, `gradle-wrapper.properties`, `pom.xml` — noticed from the editor's save / file-watch notifications, with a 30 s poll as a fallback. The server works immediately; library results appear when the sync finishes (progress token `kmp-lsp/dependency-sync`), followed by a reindex.
- **Cached.** The result is saved per workspace under `~/.cache/kmp-lsp/dependencies/`, keyed by a hash of the build files: reopening an unchanged project doesn't run the build tool at all.
- **Exact versions.** The JARs and sources jars the build tool resolved are indexed in place of "newest version of that artifact in the Gradle cache".
- **Finding the build tool.** The project's wrapper (`./gradlew`, `./mvnw`) first, then `gradle`/`mvn` on the language server's `PATH`, then `$GRADLE_HOME`/`$MAVEN_HOME`/`$M2_HOME`, SDKMAN (`~/.sdkman/candidates/*/current/bin`), Homebrew, zerobrew and the usual system directories, and finally the `PATH` your login shell (`$SHELL -i -l`) sets up — editors launched from a desktop often don't pass on a shell profile's `PATH`.
- **Fails quietly.** A broken build, no network or no JVM shows one warning and leaves everything else working; it isn't retried until the build files change. Sources that failed to download for a transient reason (network, rate limiting) are retried after 10 minutes.
- **Runs your build scripts.** Build scripts are code — set `dependencySync` to `"off"` for projects you don't trust.
- `kmp-lsp/syncDependencies` (execute-command) forces a sync.

| `indexingOptions` key | Values | Default |
|---|---|---|
| `dependencySync` | `"auto"` / `"off"` | `"auto"` |
| `referencesIncludeLibraries` | `true` / `false` | `true` |
| `decompileLibraries` | `true` / `false` | `true` |
| `librarySources` | `"onDemand"` / `"eager"` | `"onDemand"` |
| `jdkHome` | path to a JDK | found in the environment (see [JDK sources](#jdk-sources)) |

### Library sources load on demand

Library sources are **not** all kept in memory. Each sources jar (downloaded or decompiled) is parsed once into two cache files under `~/.cache/kmp-lsp/library-sources-…/`: a small **index** (the names it declares, plus a hash of every identifier it mentions) and the full **parse**. Only the index stays in memory. A library is loaded from its parse cache the first time one of its names is used: an import in a file you open, go-to-definition, hover, completion on one of its types, or a find-references that may reach it.

| Library | First startup (parse once) | Later startups | First use (from cache) |
|---|---|---|---|
| gson 2.11.0 | 53 ms | 0.4 ms | 6 ms |
| okio-jvm 3.9.0 | 240 ms | 0.4 ms | 8 ms |
| kotlin-stdlib 1.9.21 | 3.3 s | 1.6 ms | 55 ms |

(Release build, 4 cores.)

- **Completion keeps working.** Names and auto-import FQNs come from the index, so a library's classes and top-level functions are offered before it's loaded. Members after a dot load the library first, which takes the "first use" time above, once.
- **Find-references** loads the libraries whose text mentions the name being searched and its class or package, up to 64 per request, then searches them as before.
- **Memory** grows with the libraries you use, not with every dependency.
- `librarySources: "eager"` restores loading everything at startup.

### JDK sources

The JDK's own classes (`ArrayList`, `StringBuilder`, `java.time.Instant`, …) are indexed from the project's JDK sources (`lib/src.zip`), like a library sources jar: receivers typed with a JDK class resolve, go-to-definition opens the JDK source, and find-references on `list.add(…)` no longer returns an unrelated `Bag.add`. Kotlin's default imports (`java.lang.*` included) and Java's `java.lang` are honored, so `StringBuilder` needs no import.

Nothing is hard-coded — the first JDK that ships its sources is used:

1. `indexingOptions.jdkHome`, then `"jdkHome"` in `workspace.json` (`<WORKSPACE>` and relative paths supported);
2. `$JAVA_HOME` (the editor's, then your login shell's), then the `java` on the `PATH` (the editor's, then the login shell's), then on macOS `/usr/libexec/java_home`.

If none has its sources, the JDK isn't indexed and nothing else changes. A JRE has none, and Debian/Ubuntu ship them separately: `apt install openjdk-21-source` (match your JDK's version). `KMP_LSP_JDK_SOURCES=off` turns the lookup off.

- **Public API only.** `java/` and `javax/` packages of `java.base`, `java.logging`, `java.net.http`, `java.prefs`, `java.sql` and `java.xml` — about 1,850 files. AWT/Swing (`java.desktop`) and internal packages (`sun.*`, `jdk.internal.*`) are left out.
- **Loaded on demand**, like library sources: JDK 21 takes 1.8 s to parse once, 9 ms on later startups and 0.24 s on first use (release build, 4 cores).
- **Not searched for usages.** Find-references on a JDK member returns your project's calls, not the JDK's own internal ones.

### Libraries without sources: decompiled

A dependency that publishes no `-sources.jar` (typical for internal company artifacts) is decompiled after the sync with [Vineflower](https://github.com/Vineflower/vineflower) — the maintained fork of IntelliJ's Fernflower decompiler — so navigation lands on real code: Kotlin classes come back as Kotlin, Java classes as Java (method bodies included, without comments or original local names).

- The decompiler (`org.vineflower:vineflower:1.11.1`) is fetched **through your build tool** — Gradle resolves it in the sync's init script from the project's repositories, Maven runs `dependency:copy` — so company mirrors and credentials apply. It is cached under `~/.cache/kmp-lsp/decompiler/`.
- It runs on your `java` (`$JAVA_HOME`, your login shell's `JAVA_HOME`, or `java` on `PATH`), up to 4 libraries at a time, 5 minutes and 64 MB per jar at most, 100 jars per sync.
- Results are cached per jar version under `~/.cache/kmp-lsp/decompiled/` and mounted like a sources jar; go-to-definition, hover and find-references work on them. A library that can't be decompiled falls back to a stub of its signatures.
- `decompileLibraries: false` turns it off.

## References into library sources

Find-references also returns usages **inside dependency sources** — every mounted `*-sources.jar` (from the Gradle cache or a dependency sync), Kotlin and Java alike, including the [interop spellings](#java--kotlin-interop) (`Normalizer.INSTANCE.normalize()` in a Java library file is a reference of Kotlin's `object Normalizer`'s `normalize`). It works from a usage in your code and from the declaration inside the library (after go-to-definition took you there). Library hits come back as read-only `file://` copies extracted from the jar, so any editor can open them.

Only library files that can see the declaration are searched (same package, an import from its package, or a mention of its class), each occurrence must be an identifier in the parse tree (not a comment or string), and it must resolve back to the declaration — so a same-named method on another class is never included. Set `referencesIncludeLibraries` to `false` to search only the workspace. Compiled-only jars (no sources jar) have no source to search.

## Java ↔ Kotlin interop

Go-to-definition and find-references follow the names each language actually uses for the other's declarations (`src/resolver/jvm_interop.rs`):

| Written in | Usage | Resolves to |
|---|---|---|
| Java | `user.getName()` / `user.setEmail(…)` | Kotlin `val name` / `var email` (a setter never reaches a `val`) |
| Java | `user.isActive()` | Kotlin `val isActive` |
| Java | `registry.removeAll()` | Kotlin `@JvmName("removeAll") fun clear()` |
| Java | `Registry.INSTANCE.register(…)` / `Registry.INSTANCE` | Kotlin `object Registry` member / the object |
| Java | `User.Companion.anonymous()` | Kotlin companion member |
| Java | `StringUtilsKt.slugify(…)` / `TextHelper.wordCount(…)` | Kotlin top-level function; the facade class (`StringUtilsKt`, `@file:JvmName("TextHelper")`) goes to the Kotlin file |
| Kotlin | `account.balance` | Java `getBalance()` (a `private` Java field is skipped — Kotlin can't see it) |
| Kotlin | `account.balance = 1` | Java `setBalance(long)` |
| Kotlin | `point.x` | Java record component `x` |

Find-references on either side includes the other language's spelling: references of Kotlin `val name` include Java `getName()` calls, references of Java `setBalance` include Kotlin `account.balance = …` writes (and not reads), references of a `@JvmName` function include Java calls by the JVM name. Each extra hit is kept only when go-to-definition from it lands on the queried declaration.

Not covered: `rename` across spellings (renaming `val name` does not rewrite Java `getName()`), `@get:JvmName`/`@set:JvmName`, Java nullability annotations, SAM conversion.

## Limitations

- **No type inference** for generic lambda parameters — use explicit annotations for unresolvable cases
- **No type checking** — syntax errors only; use Gradle/Xcode/CI for semantic diagnostics
- **Swift support is structural** — all symbols indexed; no module boundaries or closure type inference
- **Java completion** is less refined than Kotlin
- **`findReferences` on common names** returns noise — name-based search via `rg`, no import filtering yet
- **Rename doesn't follow Java ↔ Kotlin spellings** — see [Java ↔ Kotlin interop](#java--kotlin-interop)
- **Binary `.aar`/`.jar`** — hover and completions work from compiled symbol metadata; go-to-definition and find-references into library code need a `*-sources.jar` (fetched by [dependency sync](#dependency-sync) when the library publishes one). Direct class-file indexing is [planned](https://github.com/Hessesian/kmp-lsp/issues/79).
- **Library references across overloads** — a usage of any overload of a library method counts as a reference; the call's own arity isn't used to pick the overload yet

## vs. Official Kotlin LSP

| | **kmp-lsp** | **[Kotlin/kotlin-language-server](https://github.com/Kotlin/kotlin-language-server)** (JetBrains) |
|---|---|---|
| **Runtime** | Native Rust, no JVM | JVM 17+, ~500 MB |
| **Startup** | Instant | Gradle import (slow) |
| **Memory** | < 200 MB | 1+ GB |
| **Accuracy** | Syntactic (tree-sitter) | Full IntelliJ Analysis API |
| **Editor support** | Any LSP editor | VS Code (official) |
| **Swift** | ✓ | ✗ |

They can coexist — use kmp-lsp for fast navigation, the official one for type-checked diagnostics.
