#!/usr/bin/env bash
# Integration test: a Java target, graded against a committed fixture.
#
#   1. the reference server (jdtls) answers the query set; the queries it got
#      right become `cases/<target>-java.json` — so jdtls passes 100% by
#      construction and a fixture regenerated later can only shrink
#   2. kmp-lsp answers the same cases and must stay at or above its recorded
#      baseline in that fixture
#
# Usage: bench/compare/itest.sh [options]
#   --target NAME     targets/NAME.json (default: gson)
#   --servers LIST    comma-separated subset of jdtls,kmp-lsp (default: both)
#   --generate        regenerate the fixture from the jdtls run
#   --generate-features LIST
#                     with --generate: only these features' cases (comma-
#                     separated, default all); the others are kept as they are
#   --features LIST   what the servers are asked (default all: every feature
#                     in features.py besides definition/references/completion)
#   --update-baseline record kmp-lsp's score as its new baseline
#   --add-missing-baselines
#                     record kmp-lsp's score only for features without a baseline
#   --workdir DIR     where the clone, caches and results go (default: ~/lsp-compare)
#
# Environment:
#   JDTLS       jdtls installation (the directory holding plugins/ and config_*)
#   JAVA        a Java 21+ `java` for jdtls
#   BUILD_JDK   JAVA_HOME for the target's own Maven build

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)

TARGET_NAME=gson
SERVERS=jdtls,kmp-lsp
WORKDIR="$HOME/lsp-compare"
GENERATE=0
GENERATE_FEATURES=all
FEATURES=all
UPDATE_BASELINE=0
ADD_MISSING_BASELINES=0

while [ $# -gt 0 ]; do
  case "$1" in
    --target) TARGET_NAME=$2; shift 2 ;;
    --servers) SERVERS=$2; shift 2 ;;
    --workdir) WORKDIR=$2; shift 2 ;;
    --generate) GENERATE=1; shift ;;
    --generate-features) GENERATE_FEATURES=$2; shift 2 ;;
    --features) FEATURES=$2; shift 2 ;;
    --update-baseline) UPDATE_BASELINE=1; shift ;;
    --add-missing-baselines) ADD_MISSING_BASELINES=1; shift ;;
    -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
  esac
done

mkdir -p "$WORKDIR"
WORKDIR=$(cd "$WORKDIR" && pwd)
TARGET_JSON="$SCRIPT_DIR/targets/$TARGET_NAME.json"
REPO="$WORKDIR/$TARGET_NAME"
QUERIES="$WORKDIR/$TARGET_NAME-queries.json"
CASES="$SCRIPT_DIR/cases/$TARGET_NAME-java.json"
RESULTS="$WORKDIR/itest-results"
KMP_LSP="$REPO_ROOT/target/release/kmp-lsp"

log() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
has() { case ",$1," in *",$2,"*) return 0 ;; *) return 1 ;; esac; }
json() { python3 -c "import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])" "$TARGET_JSON" "$1"; }

[ -f "$TARGET_JSON" ] || die "no target at $TARGET_JSON"
mkdir -p "$RESULTS" "$SCRIPT_DIR/cases"

# ── target ────────────────────────────────────────────────────────────────
log "target $TARGET_NAME"
if [ ! -d "$REPO/.git" ]; then
  git clone "$(json git)" "$REPO"
fi
git -C "$REPO" fetch -q origin "$(json commit)" 2>/dev/null || true
git -C "$REPO" checkout -q "$(json commit)"
python3 "$SCRIPT_DIR/queries.py" "$REPO" "$TARGET_JSON" "$QUERIES"

# jdtls imports the project by running its build: a filled dependency cache
# keeps downloads out of the measurement (and out of the failure modes).
if [ "$(json build)" = maven ]; then
  log "warm the Maven cache (one build of the target)"
  # The local repository is Maven's default (~/.m2): jdtls's m2e importer
  # reads that one, so the build has to fill it.
  read -r -a build_args <<< "$(python3 -c \
    "import json,sys; print(' '.join(json.load(open(sys.argv[1]))['build_args']))" "$TARGET_JSON")"
  (cd "$REPO" && JAVA_HOME="${BUILD_JDK:-$JAVA_HOME}" mvn -B -q "${build_args[@]}") \
    || die "the target's own build failed — jdtls could not import it either"
fi

# ── runs ──────────────────────────────────────────────────────────────────
run() {
  local server=$1
  local out="$RESULTS/$server.json"
  log "run $server"
  case "$server" in
    jdtls)
      [ -n "${JDTLS:-}" ] || die "set JDTLS to a jdtls installation"
      python3 "$SCRIPT_DIR/run.py" --server jdtls --repo "$REPO" --target "$TARGET_JSON" \
        --queries "$QUERIES" --state "$WORKDIR/itest-state/jdtls" --out "$out" \
        --jdtls "$JDTLS" --java "${JAVA:-java}" --ready-timeout 900 --features "$FEATURES" ;;
    kmp-lsp)
      [ -x "$KMP_LSP" ] || die "no kmp-lsp at $KMP_LSP (cargo build --release)"
      # Prime the state: the graded run then starts from the dependency
      # manifest and jar caches a first run leaves behind, as an editor
      # session after the first one does. (The library phase reports
      # `kmp-lsp/libraries` progress, so neither run asks before it ends.)
      if [ ! -d "$WORKDIR/itest-state/kmp-lsp" ]; then
        python3 "$SCRIPT_DIR/run.py" --server kmp-lsp --repo "$REPO" --target "$TARGET_JSON" \
          --queries "$QUERIES" --state "$WORKDIR/itest-state/kmp-lsp" --out "$out.priming" \
          --kmp-lsp "$KMP_LSP" --features base
      fi
      python3 "$SCRIPT_DIR/run.py" --server kmp-lsp --repo "$REPO" --target "$TARGET_JSON" \
        --queries "$QUERIES" --state "$WORKDIR/itest-state/kmp-lsp" --out "$out" \
        --kmp-lsp "$KMP_LSP" --features "$FEATURES" ;;
    *) die "unknown server: $server" ;;
  esac
}

has "$SERVERS" jdtls && run jdtls
if [ $GENERATE = 1 ]; then
  log "fixture from jdtls"
  python3 "$SCRIPT_DIR/itest.py" generate --queries "$QUERIES" --repo "$REPO" \
    --result "$RESULTS/jdtls.json" --language java --out "$CASES" --features "$GENERATE_FEATURES"
fi
[ -f "$CASES" ] || die "no fixture at $CASES — run with --generate"
has "$SERVERS" kmp-lsp && run kmp-lsp

# ── grading ───────────────────────────────────────────────────────────────
status=0
for server in ${SERVERS//,/ }; do
  log "check $server"
  # Only kmp-lsp has a baseline to move: the reference server's score is the
  # fixture itself.
  update=""
  [ "$server" = kmp-lsp ] && [ $UPDATE_BASELINE = 1 ] && update=--update-baseline
  [ "$server" = kmp-lsp ] && [ $ADD_MISSING_BASELINES = 1 ] && update=--add-missing-baselines
  python3 "$SCRIPT_DIR/itest.py" check --repo "$REPO" --cases "$CASES" \
    --result "$RESULTS/$server.json" $update || status=1
done
exit $status
