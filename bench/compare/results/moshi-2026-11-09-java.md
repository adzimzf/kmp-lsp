# Results: square/moshi, Java side, 2026-11-09

Java accuracy of kmp-lsp against jdtls, before and after the Java fixes on
this branch. Same machine, same query set (seed 7), same warm Gradle cache.

## Conditions

| | |
|---|---|
| Machine | cloud container, 4 vCPU, 15 GiB RAM, Linux 6.18 |
| Target | square/moshi `889013ec2edb`, built once with `./gradlew testClasses` (skipping `:moshi:records-tests:compileTestJava`) |
| JDK | OpenJDK 21 **without `src.zip`** (Ubuntu without `openjdk-21-source`; `lib/src.zip` is a dangling symlink) — the usual Linux setup |
| jdtls | 1.64.0 snapshot `202611062158`, on OpenJDK 25 (Ubuntu package; kotlin-lsp, whose JBR `compare.sh` normally borrows, could not be downloaded) |
| kotlin-lsp | not run (download refused) — the Kotlin half has no reference and isn't scored |
| Baseline | `ccr-35fa55a7-ptcya8` at `a6e25f2` |
| Scorer | this branch's `score.py` for both columns (it now names a target in kmp-lsp's compiled-class stub listing by its class, as it already did for jdtls's `jdt://…/String.class`) |

## Java, before → after

| | jdtls | kmp-lsp before | kmp-lsp after | kmp-lsp before, no dep sync | kmp-lsp after, no dep sync |
|---|---|---|---|---|---|
| definition: right | reference | 56% (80/144) | **95% (137/144)** | 71% (102/144) | **94% (135/144)** |
| definition: wrong | — | 9 | **0** | 10 | 1 |
| definition: no answer | — | 55 | **7** | 32 | 8 |
| references: precision | reference | 58% | **69%** | 59% | 69% |
| references: recall | reference | 62% | 61% | 63% | 61% |
| completion: top 1 | 16% | 2% | 10% | 2% | 10% |
| completion: top 10 | 38% | 5% | **38%** | 5% | 38% |
| completion: anywhere | 90% | 35% | **93%** | 35% | 93% |
| completion: empty list | 5% | 0% | 3% | 0% | 3% |
| completion: median list size | 44 | 500 | 35 | 500 | 35 |

Speed is unchanged in shape: kmp-lsp ready in ~1.2 s cold / ~0.14 s warm
(jdtls 36 s / 4.6 s); definition p50 3 ms, hover 1 ms, completion 56 ms.

## What was wrong, and what changed

- **No JDK at all without `src.zip`.** `String`, `Long`, `List.add`,
  `Map.put`, `Arrays.asList` resolved nowhere, or to same-named project
  symbols (`String` → `fun String.parseIsoDate()` in `Iso8601Utils.kt`,
  `list.add` → `Moshi.Builder.add`). The JDK's `jmods/*.jmod` (the same
  modules the sources path indexes, `java.*`/`javax.*` only) now go through
  the JAR-indexer sidecar when no JDK has sources.
- **Java dot-completion had no receiver.** The marker-insertion walk only
  knew Kotlin's `navigation_expression`, so every Java `x.` completed as a
  bare word (500 unfiltered items). It now reads Java field accesses, method
  calls and the qualified type a missing `;` produces, typed from the Java
  tree.
- **Java method return types were unknown.** Return-type inference parsed
  only Kotlin's `): T`, so Java (and JDK) chains stopped typing after the
  first call.
- **Overloads.** A Java call of an overloaded method kept every overload
  (definition) or nothing (hover declined). Overloads are now chosen by
  argument types (literals, typed expressions, `null`, primitive widening),
  and hover resolves Java members through the CST identity.
- **Static imports.** `assertThat(x).isEqualTo(..)`: bare calls are typed
  through `import static`, with the overload the arguments fit
  (`assertThat(boolean)` → `BooleanSubject`).
- **Library sources resolve through their own imports**, and a class's
  members no longer include its nested classes' members
  (`StringSubject.isEqualTo` landed on
  `StringSubject.CaseInsensitiveStringComparison.isEqualTo`).
- The rg definition fallback no longer takes an extension's receiver
  (`fun String.x()`, `val List<T>.y`) for a declaration of that name.

## Still behind jdtls

- **References recall (61%).** jdtls's references of an overriding method
  include calls through the method it overrides: references of an anonymous
  `JsonAdapter`'s `fromJson` list every `adapter.fromJson(..)` (31 vs 2).
  Same for `equals`, `toString`, `getActualTypeArguments`. kmp-lsp doesn't
  climb to the overridden declaration.
- **Completion ranking (top 1: 10% vs 16%).** Members are sorted by kind
  then name; for a type receiver (`Moshi.`) nested types come last, and
  library-source classes list their non-public members too.
- **JDK signatures from class files** have no parameter names or generics
  (`fun add( p0: Object)`), and go-to-definition opens the per-jar stub
  listing. Installing the JDK's sources (`openjdk-NN-source`) gives real
  declarations.
- **Hover rate** in this report counts the "_JAR symbols are still
  indexing_" placeholder as an answer: the sources-JAR phase reports no
  `$/progress`, so queries start before it ends. Both columns are affected
  alike; treat kmp-lsp's 100% Java hover rate as an upper bound.
