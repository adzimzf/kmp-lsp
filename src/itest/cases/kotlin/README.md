# Kotlin cases

## diagnostics

The whole file's report (`textDocument/diagnostic`), written as
`path:line:column  message`, as the Java diagnostics cases are (see
`../java/README.md`): positions are the compiler's, the wording is
kmp-lsp's.

### Kotlin language versions

A case whose answer depends on the language version the project is built
with (`languageVersion` in Gradle, `-language-version` for `kotlinc`) has
one `//= expected kotlin <range>` block per range, covering every version
from 2.0 to 2.4 once (the versions Kotlin 2.4's compiler accepts), and a
plain `//= expected` beside them: the answer when the version is not known,
which is the newest's. Each range is asked at both its ends, the server
told the version as `indexingOptions.kotlinLanguageVersion` (a string,
`"2.1"`).

The version each feature is stable from is its `sinceVersion` in
`LanguageFeature` (`core/language.version-settings/.../LanguageVersionSettings.kt`
at the `v2.4.0` tag of JetBrains/kotlin). **The positions are not yet
checked with `kotlinc`**: they follow where the compiler's checkers report
`UNSUPPORTED_FEATURE`, read from their source, and are to be confirmed with
`kotlinc -language-version <v>` at both ends of each range.

| case | releases | where the compiler reports it |
| --- | --- | --- |
| `a_guard_in_a_when_branch_needs_kotlin_2_2` | `is Circle if …`: an error up to 2.1 | the guard (`FirWhenGuardChecker`, `WHEN_GUARD` positioning) |
| `multi_dollar_interpolation_needs_kotlin_2_2` | `$$"…"`: an error up to 2.1 | the string (`FirMultiDollarInterpolationChecker`) |
| `break_inside_an_inline_lambda_needs_kotlin_2_2` | `break` out of an inline lambda: an error up to 2.1 | the `break` (`FirBreakOrContinueJumpsAcrossFunctionBoundaryChecker`) |
| `a_nested_type_alias_needs_kotlin_2_3` | `typealias` in a class: an error up to 2.2 | the declaration (`FirAnyTypeAliasChecker`) |
| `an_explicit_backing_field_needs_kotlin_2_4` | `field = 0` under a property: an error up to 2.3 | the backing field (`FirExplicitBackingFieldsUnsupportedChecker`) |
