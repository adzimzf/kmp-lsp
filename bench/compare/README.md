# kmp-lsp vs jdtls vs JetBrains kotlin-lsp

A reproducible comparison of three language servers on a real repository:
the same seeded query set is sent to each server over plain LSP, every
answer and its latency recorded, then scored.

| Server | What it is |
|---|---|
| kmp-lsp | this repo: tree-sitter index, no compiler |
| [jdtls](https://github.com/eclipse-jdtls/eclipse.jdt.ls) | Eclipse JDT: Java compiler model, Gradle/Maven import (Buildship/m2e) |
| [kotlin-lsp](https://github.com/Kotlin/kotlin-lsp) | JetBrains: IntelliJ + Kotlin Analysis API, Gradle/Maven import |

## Target

`targets/moshi.json` — [square/moshi](https://github.com/square/moshi) pinned
at a commit: JVM-only Gradle build (kotlin-lsp can't import Kotlin
Multiplatform yet), 131 source files, Kotlin and Java mixed in the same
modules. Its core API (`JsonAdapter`, `JsonReader`, `Moshi`) is Kotlin and its
Java tests and examples call it, so cross-language navigation is exercised.

## What is measured

**Speed** (from process start; `run.py`)

- `initialize` answered;
- *ready*: each readiness probe in the target (a known cross-file
  definition, one Kotlin, one Java) returns the expected location — the moment
  the server is actually useful, which includes project import/indexing;
- per request after ready: p50 / p95 / max for definition, hover,
  references, completion, documentSymbol; timeouts;
- peak resident memory of the server's whole process tree (Gradle daemons
  it starts included) and memory at ready;
- **cold** (empty state directory) and **warm** (second run on the state the
  first left). The Gradle user home is shared and never cleared, so
  dependency downloads don't count as server time.

**Accuracy** (`score.py`)

- *Completion* — ground truth from the source: an existing `receiver.member`
  has `member` deleted and completion is asked after the `.`. Reported: top-1 /
  top-5 / top-10 / anywhere, MRR, empty lists (ranked as a client would sort:
  `sortText`, then `label`).
- *Go-to-definition* — the harness has no compiler, so the reference is the
  compiler-backed server that owns the file's language (kotlin-lsp for `.kt`,
  jdtls for `.java`), falling back to the other when the owner has no answer;
  a server is never scored against itself. Also: pairwise agreement, results
  by query shape (member call, type reference, …) and a list of disagreements
  to spot-check by hand.
- *Find-references* — reference set: `.java` usages from jdtls, `.kt` usages
  from kotlin-lsp. Precision, recall, exact-match rate.

Queries (`queries.py`) come from a comment- and string-aware lexer, not from
any server: identifier usages for definition/hover, declaration names for
references, member accesses for completion — 150 / 40 / 100 per language,
seed 7.

## Running

### One command

```sh
bench/compare/compare.sh                 # clone → install → run → report
```

It clones the target at its pinned commit and builds the query set, builds
kmp-lsp and its JAR-indexer sidecar (as a normal install has it),
downloads the newest jdtls snapshot and kotlin-lsp standalone archive for
your OS/CPU (checksums verified), builds the target once to fill the Gradle
cache (retrying rate-limited downloads), runs every server cold then warm,
and writes `~/lsp-compare/report-moshi.md`. kmp-lsp without dependency sync
runs first, before anything downloads sources jars into the shared Gradle
cache, so its result doesn't depend on run order. After each kmp-lsp run the
script prints how many dependencies came with sources and warns when few did
(Maven Central rate-limiting would understate kmp-lsp). Re-running skips
what's already installed. An expired kotlin-lsp build is detected and
skipped with a warning; point `KOTLIN_LSP_URL` at a newer archive to include
it. Options: `--workdir`, `--servers kmp-lsp,kmp-lsp-nosync,jdtls,kotlin-lsp`,
`--phases cold,warm`, `--steps clone,install,run`, `--fresh`; see `--help`.

Needs git, curl, tar, python3, cargo and a JDK 21 (moshi's own build);
jdtls's Java 25 comes from the JBR bundled with kotlin-lsp (or `JAVA25`).

### By hand

Needs: Python 3.8+ (standard library only), Rust (for kmp-lsp), a JDK 21
for moshi's build, and Java 25 for jdtls (kotlin-lsp bundles one — see
below). Linux and macOS. Run everything from this directory.

**1. Target and queries**

```sh
cd bench/compare
S=$HOME/lsp-compare            # anywhere with a few GB free
git clone https://github.com/square/moshi $S/moshi
git -C $S/moshi checkout 889013ec2edb8d8034902662a1dc8c4f3b3f8111
python3 queries.py $S/moshi targets/moshi.json $S/queries.json
```

**2. Servers**

```sh
(cd ../.. && cargo build --release)            # kmp-lsp → ../../target/release/kmp-lsp
```

- jdtls: the newest `jdt-language-server-*.tar.gz` from
  <https://download.eclipse.org/jdtls/snapshots/> (name in `latest.txt`),
  unpacked into `$S/jdtls`.
- kotlin-lsp: the "Standalone Kotlin LSP Archive" for your platform from
  [RELEASES.md](https://github.com/Kotlin/kotlin-lsp/blob/main/RELEASES.md),
  unpacked into `$S/kotlin-lsp` (`--kotlin-lsp` may point at the unpacked
  directory or the folder holding it; `bin/intellij-server` is found inside).
  Check it starts: `$S/kotlin-lsp/*/bin/intellij-server --stdio` must not
  print "This build of kotlin-server has expired".

**3. Warm the Gradle cache once** (dependency downloads are not server time)

```sh
(cd $S/moshi && GRADLE_USER_HOME=$S/gradle ./gradlew testClasses -x :moshi:records-tests:compileTestJava)
```

`records-tests` wants a JDK 17 toolchain; skip it unless you have one. No
query touches it.

**4. Run** — one server at a time, nothing else heavy running

```sh
common="--repo $S/moshi --target targets/moshi.json --queries $S/queries.json --gradle-home $S/gradle"
JAVA25=$(ls -d $S/kotlin-lsp/*/jbr/bin/java $S/kotlin-lsp/*/jbr/Contents/Home/bin/java 2>/dev/null | head -1)
for phase in cold warm; do
  python3 run.py --server kmp-lsp    $common --state $S/state/kmp    --out $S/results/kmp-lsp-$phase.json --kmp-lsp ../../target/release/kmp-lsp
  python3 run.py --server kmp-lsp    $common --state $S/state/kmp-nosync --out $S/results/kmp-lsp-nosync-$phase.json --kmp-lsp ../../target/release/kmp-lsp --no-dependency-sync
  python3 run.py --server jdtls      $common --state $S/state/jdtls  --out $S/results/jdtls-$phase.json --jdtls $S/jdtls --java "${JAVA25:-java}"
  python3 run.py --server kotlin-lsp $common --state $S/state/kotlin --out $S/results/kotlin-lsp-$phase.json --kotlin-lsp $S/kotlin-lsp --request-timeout 90
done
```

The first loop iteration is *cold* only if `$S/state` doesn't exist yet;
delete it to start over.

**5. Report**

```sh
python3 score.py --queries $S/queries.json --repo $S/moshi \
  $S/results/*-cold.json $S/results/*-warm.json > $S/report.md
```

jdtls and kotlin-lsp import the project by running its Gradle build, and
kmp-lsp's dependency sync runs it too (`--no-dependency-sync` turns that off,
at the cost of library types). Run them on one machine, one at a time.
Each run's server stderr is kept next to its result (`*.stderr.log`).

Notes from running it:

- jdtls snapshots need **Java 25** (`--java`); kotlin-lsp's bundled
  `jbr/bin/java` works when the machine has an older JDK.
- kotlin-lsp EAP builds **expire**; a build that prints "This build of
  kotlin-server has expired" needs replacing with a newer one.
- Results of a run: `results/moshi-2026-11-06.md`.

## Integration test: pass/fail, not a comparison

`compare.sh` answers "how does kmp-lsp compare"; `itest.sh` answers "did
anything break". It grades a run against a *committed fixture* of expected
answers instead of against another server's live run:

```sh
JDTLS=/path/to/jdtls JAVA=/path/to/java25 BUILD_JDK=/path/to/jdk21 \
  bench/compare/itest.sh                       # target → jdtls → kmp-lsp → grade
```

- Target: `targets/gson.json` — [google/gson](https://github.com/google/gson)
  pinned at a commit. Java only (no Kotlin file at all, so jdtls owns every
  query), 211 source files, a Maven build that compiles in well under a
  minute, and the Java shapes that break resolvers: generic types
  (`TypeToken<T>`, `TypeAdapter<T>`), heavy overload sets (`Gson.fromJson`,
  `JsonWriter.value`), anonymous and nested classes, enums, annotations,
  static imports (`assertThat`), varargs, reflection (`Field`,
  `Constructor`) and JDK types throughout.
- Fixture: `cases/gson-java.json`, generated with `--generate` from jdtls's
  own answers — only the queries jdtls got right become cases, so **jdtls
  passes 100% by construction** and regenerating can only shrink the set.
  147 definition, 19 references, 54 completion cases, plus the features
  below.
- Grading: *definition* — same target (file, line), with a compiled class
  accepted against that class's source file; *references* — the in-repo
  usage set must match exactly; *completion* — the deleted member must rank
  within the top 10. Every failure prints like a test runner's: the query's
  file, line and column, the source line it sits on, what was expected
  there and what the server answered instead (for references: which usages
  are missing and which are extra; for completion: the rank and the top of
  the list). `--failures N` sets how many per feature are shown.
- Every server is held to a baseline recorded in the fixture and the script
  exits non-zero below it: a ratchet, like `tests/architecture_ratchet.rs`.
  Raise a baseline with `--update-baseline` when a fix lands; never lower
  one without saying why.
- The recorded baseline is the score from an **empty state directory and a
  dependency cache holding only what the target's own build downloaded** —
  what CI reproduces. A state that has been run against for a while scores
  higher (more library sources jars have arrived: `assertThat(..).contains`
  and `hasMessageThat` resolve, `getActualTypeArguments` finds its usages),
  so record a baseline from a fresh `--workdir`, not from the directory you
  have been iterating in.

kmp-lsp's state directory is primed with one throw-away run first, so the
graded run starts from the dependency manifest and jar caches a first session
leaves behind. Both runs wait for the library phase (`kmp-lsp/libraries`
progress) to end before asking: without that progress, a run asked its
questions while the JDK and dependency jars were still being indexed, and the
score swung with the runner's speed (50% on one CI run, 89% on the next).

`itest_selfcheck.py` checks the gate itself on a synthetic result: that a
score below baseline exits non-zero and that a failure names both locations.

### Beyond definition, references and completion

`features.py` adds every other feature jdtls implements, and the standard LSP
features it does not, each with its own cases in the same fixture (`sources`
there says where a feature's expected answers come from):

| Feature | Asked on | Passes when |
|---|---|---|
| **From jdtls's answers** (jdtls passes 100% by construction) | | |
| hover | definition queries | the hover text names the symbol |
| declaration | definition queries | same target as jdtls (as for definition) |
| typeDefinition | definition queries | the same type declaration as jdtls |
| implementation | declarations (`references` queries) | the same in-repo implementations, exactly |
| documentHighlight | declarations | the same occurrences in the file, exactly |
| documentSymbol | `files` (25) | every named symbol jdtls lists is there, at its line |
| workspaceSymbol | `files`: the top-level type's name | the search finds its declaration |
| signatureHelp | calls, just inside the `(` | active signature of that method with jdtls's parameter count |
| prepareRename | declarations | the name's range |
| rename | declarations | the same edit positions as jdtls, exactly |
| foldingRange | `files` | every code region jdtls folds (not comments/imports) starts a range |
| selectionRange | definition queries | jdtls's innermost range, a nested chain of 3+ |
| semanticTokens | definition queries | the token type jdtls gives (legends mapped by name) |
| inlayHint | `files` (up to 80 hints) | each parameter-name hint at the same place, same name |
| callHierarchy | method declarations | the same incoming call sites, exactly |
| typeHierarchy | `files`: the top-level type | the same supertypes and subtypes |
| diagnostics | `files` | no error where jdtls publishes none (the target compiles) |
| codeAction | `imports`: one import line blanked | a quick fix at the usage adds `import <fqn>;` back |
| **From the sources** | | |
| formatting | 10 `files`, indentation stripped | code unchanged, indentation back |
| rangeFormatting | 8 method-body lines stripped | indented again, nothing else moves |
| onTypeFormatting | Enter after a statement | the new line indented like the statement |
| documentLink † | `links`: files with URLs in comments | every URL is a link at its position |
| pullDiagnostics † | `files` | `textDocument/diagnostic` reports no error |
| workspaceDiagnostics † | once | `workspace/diagnostic` reports no error |
| moniker † | declarations | a moniker naming `package.Type` and the symbol |

† standard LSP that jdtls does not implement. jdtls advertises formatting and
on-type formatting, but its output is a style choice (Eclipse's), so those
three are graded on properties any formatter must have. Not covered: code
lens (jdtls's are "N references", which references covers), document color
(no colors in Java), linked editing range (no paired tags in Java), inline
value (a debugger feature).

`--generate --generate-features hover,rename` rebuilds only those features'
cases; `--add-missing-baselines` records kmp-lsp's score for features that
have none yet without touching the others.

Adding a target: a `targets/<name>.json` with `roots`, `sample`, `probes`
(known cross-file definitions that mark the server ready) and `build`, then
`itest.sh --target <name> --generate`.

## Caveats

- The definition/references reference is a compiler-backed *server*, not the
  compiler. Where the two compiler-backed servers disagree with each other,
  the pairwise table shows it; check the disagreement list before trusting a
  small difference.
- jdtls reads no `.kt` sources: Java code calling Kotlin declarations
  resolves (if at all) to compiled classes, which the scorer counts as a
  library target.
- Latency is single-client, one request at a time, after ready.
