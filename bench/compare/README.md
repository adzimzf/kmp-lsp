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

```sh
S=/tmp/lsp-compare
git clone https://github.com/square/moshi $S/moshi && git -C $S/moshi checkout 889013ec2edb
python3 queries.py $S/moshi targets/moshi.json $S/queries.json

# Servers: kmp-lsp release build, jdtls snapshot, kotlin-lsp standalone archive.
cargo build --release
#   https://download.eclipse.org/jdtls/snapshots/  → $S/jdtls
#   kotlin-lsp RELEASES.md "Standalone Kotlin LSP Archive" → $S/kotlin-lsp

common="--repo $S/moshi --target targets/moshi.json --queries $S/queries.json --gradle-home $S/gradle"
for phase in cold warm; do
  python3 run.py --server kmp-lsp    $common --state $S/state/kmp    --out $S/results/kmp-lsp-$phase.json --kmp-lsp ../../target/release/kmp-lsp
  python3 run.py --server jdtls      $common --state $S/state/jdtls  --out $S/results/jdtls-$phase.json --jdtls $S/jdtls
  python3 run.py --server kotlin-lsp $common --state $S/state/kotlin --out $S/results/kotlin-lsp-$phase.json --kotlin-lsp $S/kotlin-lsp
done
python3 score.py --queries $S/queries.json --repo $S/moshi $S/results/*-cold.json $S/results/*-warm.json > report.md
```

jdtls and kotlin-lsp import the project by running its Gradle build, and
kmp-lsp's dependency sync runs it too (`--no-dependency-sync` turns that off,
at the cost of library types). Run them on one machine, one at a time.
Each run's server stderr is kept next to its result (`*.stderr.log`).

## Caveats

- The definition/references reference is a compiler-backed *server*, not the
  compiler. Where the two compiler-backed servers disagree with each other,
  the pairwise table shows it; check the disagreement list before trusting a
  small difference.
- jdtls reads no `.kt` sources: Java code calling Kotlin declarations
  resolves (if at all) to compiled classes, which the scorer counts as a
  library target.
- Latency is single-client, one request at a time, after ready.
