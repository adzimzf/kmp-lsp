#!/usr/bin/env bash
# kmp-lsp vs jdtls vs JetBrains kotlin-lsp on a real repository, end to end:
#
#   1. clone   — the target repository at its pinned commit, plus the query set
#   2. install — build kmp-lsp, download jdtls and kotlin-lsp (checksummed),
#                warm the Gradle cache with one build of the target
#   3. run     — every server cold, then warm, one at a time; then the report
#
# Usage: bench/compare/compare.sh [options]
#   --workdir DIR     where everything goes (default: ~/lsp-compare)
#   --target NAME     targets/NAME.json (default: moshi)
#   --servers LIST    comma-separated (default: kmp-lsp,kmp-lsp-nosync,jdtls,kotlin-lsp)
#   --phases LIST     comma-separated (default: cold,warm)
#   --steps LIST      comma-separated subset of clone,install,run (default: all)
#   --fresh           delete previous server state and results first
#   -h, --help        this text
#
# Environment overrides:
#   KOTLIN_LSP_URL    a kotlin-lsp standalone archive to use instead of the
#                     newest one listed in kotlin-lsp's RELEASES.md
#   JDTLS_URL         a jdtls tar.gz to use instead of the newest snapshot
#   JAVA25            a Java 25+ `java` for jdtls (default: the JBR bundled
#                     with kotlin-lsp)
#
# Linux and macOS. Needs git, curl, tar, python3, cargo and a JDK 21 (the
# target's own build). Re-running skips what is already there.

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)

WORKDIR="$HOME/lsp-compare"
TARGET_NAME="moshi"
SERVERS="kmp-lsp,kmp-lsp-nosync,jdtls,kotlin-lsp"
PHASES="cold,warm"
STEPS="clone,install,run"
FRESH=0

while [ $# -gt 0 ]; do
  case "$1" in
    --workdir) WORKDIR=$2; shift 2 ;;
    --target) TARGET_NAME=$2; shift 2 ;;
    --servers) SERVERS=$2; shift 2 ;;
    --phases) PHASES=$2; shift 2 ;;
    --steps) STEPS=$2; shift 2 ;;
    --fresh) FRESH=1; shift ;;
    -h|--help) sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
  esac
done

mkdir -p "$WORKDIR"
WORKDIR=$(cd "$WORKDIR" && pwd)
TARGET_JSON="$SCRIPT_DIR/targets/$TARGET_NAME.json"
REPO="$WORKDIR/$TARGET_NAME"
QUERIES="$WORKDIR/$TARGET_NAME-queries.json"
SERVERS_DIR="$WORKDIR/servers"
STATE="$WORKDIR/state"
RESULTS="$WORKDIR/results"
GRADLE_HOME="$WORKDIR/gradle"
KMP_LSP="$REPO_ROOT/target/release/kmp-lsp"
REPORT="$WORKDIR/report-$TARGET_NAME.md"

log() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
has() { case ",$1," in *",$2,"*) return 0 ;; *) return 1 ;; esac; }
json() { python3 -c "import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])" "$TARGET_JSON" "$1"; }

sha256_of() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# Download $1 to $2 and check it against $1.sha256 when the server publishes one.
fetch() {
  local url=$1 file=$2 expected
  [ -s "$file" ] && return 0
  echo "downloading $url"
  curl -fL --retry 3 -o "$file.part" "$url"
  if expected=$(curl -fsL "$url.sha256" 2>/dev/null | awk '{print $1}') && [ -n "$expected" ]; then
    [ "$(sha256_of "$file.part")" = "$expected" ] || die "checksum mismatch for $url"
    echo "checksum ok"
  else
    warn "no published checksum for $url"
  fi
  mv "$file.part" "$file"
}

platform() {
  local os arch
  case "$(uname -s)" in
    Darwin) os=macOS ;;
    Linux) os=Linux ;;
    *) die "unsupported OS: $(uname -s) (Linux and macOS only)" ;;
  esac
  case "$(uname -m)" in
    arm64|aarch64) arch=arm64 ;;
    x86_64|amd64) arch=x64 ;;
    *) die "unsupported CPU: $(uname -m)" ;;
  esac
  echo "$os-$arch"
}

check_tools() {
  local missing=()
  for tool in git curl tar python3 "$@"; do
    command -v "$tool" >/dev/null || missing+=("$tool")
  done
  [ ${#missing[@]} -eq 0 ] || die "missing tools: ${missing[*]}"
}

# ── 1. clone ──────────────────────────────────────────────────────────────
step_clone() {
  log "1/3 clone $TARGET_NAME"
  local url commit
  url=$(json git)
  commit=$(json commit)
  if [ ! -d "$REPO/.git" ]; then
    git clone "$url" "$REPO"
  fi
  git -C "$REPO" fetch -q origin "$commit" 2>/dev/null || true
  git -C "$REPO" checkout -q "$commit"
  echo "$TARGET_NAME at $(git -C "$REPO" rev-parse --short HEAD)"
  python3 "$SCRIPT_DIR/queries.py" "$REPO" "$TARGET_JSON" "$QUERIES"
}

# ── 2. install ────────────────────────────────────────────────────────────
install_kmp_lsp() {
  log "2/3 build kmp-lsp (release)"
  (cd "$REPO_ROOT" && cargo build --release)
  [ -x "$KMP_LSP" ] || die "kmp-lsp build did not produce $KMP_LSP"
  # The JAR-indexer sidecar is part of a normal install (symbols of compiled
  # library jars); kmp-lsp finds it next to its own executable.
  local sidecar="$REPO_ROOT/java-sidecar/build/libs/kmp-jar-indexer.jar" attempt
  if [ ! -f "$sidecar" ]; then
    for attempt in 1 2 3 4 5; do
      (cd "$REPO_ROOT/java-sidecar" && ./gradlew --console=plain -q shadowJar) && break
      warn "sidecar build failed (attempt $attempt/5), retrying in $((attempt * 20)) s"
      sleep $((attempt * 20))
    done
  fi
  [ -f "$sidecar" ] || die "could not build the kmp-jar-indexer sidecar"
  cp "$sidecar" "$(dirname "$KMP_LSP")/kmp-jar-indexer.jar"
  echo "sidecar: $(dirname "$KMP_LSP")/kmp-jar-indexer.jar"
}

install_jdtls() {
  log "2/3 install jdtls"
  local base=https://download.eclipse.org/jdtls/snapshots url name
  if [ -n "${JDTLS_URL:-}" ]; then
    url=$JDTLS_URL
  else
    name=$(curl -fsL "$base/latest.txt" | tr -d '[:space:]')
    [ -n "$name" ] || die "could not read $base/latest.txt"
    url="$base/$name"
  fi
  name=$(basename "$url")
  mkdir -p "$SERVERS_DIR"
  fetch "$url" "$SERVERS_DIR/$name"
  if [ ! -f "$SERVERS_DIR/jdtls/.installed-$name" ]; then
    rm -rf "$SERVERS_DIR/jdtls" && mkdir -p "$SERVERS_DIR/jdtls"
    tar -xzf "$SERVERS_DIR/$name" -C "$SERVERS_DIR/jdtls"
    touch "$SERVERS_DIR/jdtls/.installed-$name"
  fi
  echo "jdtls: $name"
}

kotlin_lsp_url() {
  if [ -n "${KOTLIN_LSP_URL:-}" ]; then
    echo "$KOTLIN_LSP_URL"
    return
  fi
  local releases label
  label="Download for $(platform)"
  releases=$(curl -fsL https://raw.githubusercontent.com/Kotlin/kotlin-lsp/main/RELEASES.md) \
    || die "could not fetch kotlin-lsp RELEASES.md (set KOTLIN_LSP_URL)"
  # The newest release is first; take its standalone archive for this platform.
  printf '%s\n' "$releases" \
    | awk '/Standalone Kotlin LSP Archive/ {found=1} found' \
    | grep -F "[$label]" | head -1 \
    | sed -E 's/^[^(]*\((https:[^)]*)\).*/\1/'
  # (The line also links the archive's `.sha256`: take the first link only.)
}

kotlin_lsp_executable() {
  find "$SERVERS_DIR/kotlin-lsp" -path '*bin/intellij-server' -type f 2>/dev/null | head -1
}

# Pre-release kotlin-lsp builds stop working after a date; find out now.
kotlin_lsp_expired() {
  local executable log pid waited=0
  executable=$(kotlin_lsp_executable)
  log="$SERVERS_DIR/kotlin-lsp-start-check.log"
  "$executable" --stdio </dev/null >"$log" 2>&1 &
  pid=$!
  while kill -0 "$pid" 2>/dev/null && [ $waited -lt 30 ]; do
    grep -q "has expired" "$log" && break
    sleep 1; waited=$((waited + 1))
  done
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  grep -q "has expired" "$log"
}

install_kotlin_lsp() {
  log "2/3 install kotlin-lsp"
  local url name
  url=$(kotlin_lsp_url)
  [ -n "$url" ] || die "no kotlin-lsp archive for $(platform) in RELEASES.md (set KOTLIN_LSP_URL)"
  name=$(basename "$url")
  mkdir -p "$SERVERS_DIR"
  fetch "$url" "$SERVERS_DIR/$name"
  if [ ! -f "$SERVERS_DIR/kotlin-lsp/.installed-$name" ]; then
    rm -rf "$SERVERS_DIR/kotlin-lsp" && mkdir -p "$SERVERS_DIR/kotlin-lsp"
    case "$name" in
      *.tar.gz) tar -xzf "$SERVERS_DIR/$name" -C "$SERVERS_DIR/kotlin-lsp" ;;
      # JetBrains' macOS .sit archives are zip files.
      *.sit|*.zip)
        if command -v ditto >/dev/null; then ditto -x -k "$SERVERS_DIR/$name" "$SERVERS_DIR/kotlin-lsp"
        else unzip -q "$SERVERS_DIR/$name" -d "$SERVERS_DIR/kotlin-lsp"; fi ;;
      *) die "don't know how to unpack $name" ;;
    esac
    touch "$SERVERS_DIR/kotlin-lsp/.installed-$name"
  fi
  [ -n "$(kotlin_lsp_executable)" ] || die "no bin/intellij-server inside $name"
  echo "kotlin-lsp: $name"
  if kotlin_lsp_expired; then
    warn "this kotlin-lsp build has expired ('This build of kotlin-server has expired')."
    warn "it will be skipped; set KOTLIN_LSP_URL to a newer standalone archive to include it."
    touch "$SERVERS_DIR/kotlin-lsp/.expired"
  else
    rm -f "$SERVERS_DIR/kotlin-lsp/.expired"
  fi
}

java25() {
  if [ -n "${JAVA25:-}" ]; then echo "$JAVA25"; return; fi
  local bundled
  bundled=$(find "$SERVERS_DIR/kotlin-lsp" -path '*jbr*/bin/java' -type f 2>/dev/null | head -1)
  if [ -n "$bundled" ]; then echo "$bundled"; return; fi
  # No kotlin-lsp (expired or not downloadable): an installed JDK 25+.
  local installed
  installed=$(ls -d /usr/lib/jvm/*-2[5-9]*/bin/java /Library/Java/JavaVirtualMachines/*-2[5-9]*/Contents/Home/bin/java 2>/dev/null | head -1)
  if [ -n "$installed" ]; then echo "$installed"; else echo java; fi
}

warm_gradle() {
  log "2/3 build $TARGET_NAME once (fills the Gradle cache; downloads aren't server time)"
  local excludes=()
  # moshi's records-tests module wants a JDK 17 toolchain; no query touches it.
  [ "$TARGET_NAME" = moshi ] && excludes=(-x :moshi:records-tests:compileTestJava)
  if [ -f "$WORKDIR/.gradle-warmed-$TARGET_NAME" ]; then
    echo "already done"
    return
  fi
  # Maven Central / the plugin portal rate-limit (HTTP 429) a cold Gradle
  # home: a failed resolution is retried, a compile error is not.
  local attempt build_log="$WORKDIR/gradle-warmup-$TARGET_NAME.log"
  for attempt in 1 2 3 4 5 6 7 8; do
    # `${a[@]+"${a[@]}"}`: an empty array under `set -u` breaks macOS's bash 3.2.
    if (cd "$REPO" && GRADLE_USER_HOME="$GRADLE_HOME" ./gradlew --console=plain testClasses \
          ${excludes[@]+"${excludes[@]}"}) >"$build_log" 2>&1; then
      touch "$WORKDIR/.gradle-warmed-$TARGET_NAME"
      echo "built (log: $build_log)"
      return
    fi
    if ! grep -qE "could not resolve|Could not resolve|Could not GET|Could not HEAD|status code 429|was not found in any of the following sources" "$build_log"; then
      break
    fi
    warn "dependency download failed (attempt $attempt/8), retrying in $((attempt * 20)) s"
    sleep $((attempt * 20))
  done
  tail -30 "$build_log" >&2
  die "the $TARGET_NAME build failed (log: $build_log) — jdtls and kotlin-lsp would fail to import it too"
}

step_install() {
  check_tools cargo
  has "$SERVERS" kmp-lsp || has "$SERVERS" kmp-lsp-nosync && install_kmp_lsp
  has "$SERVERS" jdtls && install_jdtls
  has "$SERVERS" kotlin-lsp && install_kotlin_lsp
  warm_gradle
}

# ── 3. run ────────────────────────────────────────────────────────────────
# kmp-lsp's accuracy depends on how many sources jars its dependency sync got:
# say so, and warn when downloads failed (Maven Central rate-limits).
report_dependency_sync() {
  local log=$1 phase=$2 summary
  summary=$(grep -oE "dependency sync: [0-9]+ dependencies, [0-9]+ with sources[^—]*" "$log" | tail -1 || true)
  if [ -z "$summary" ]; then
    # Warm: the build files are unchanged, so kmp-lsp reuses the cold run's
    # dependency list and logs nothing.
    if [ "$phase" = warm ]; then
      echo "dependency sync: up to date (reused the cold run's)"
    else
      warn "kmp-lsp's dependency sync reported no result (see $log)"
    fi
    return 0
  fi
  echo "$summary"
  local dependencies with_sources
  dependencies=$(echo "$summary" | sed -E 's/.*: ([0-9]+) dependencies.*/\1/')
  with_sources=$(echo "$summary" | sed -E 's/.*, ([0-9]+) with sources.*/\1/')
  if [ "$dependencies" -gt 0 ] && [ $((with_sources * 2)) -lt "$dependencies" ]; then
    warn "only $with_sources of $dependencies dependencies came with sources — downloads may have been"
    warn "rate-limited (HTTP 429); kmp-lsp's library navigation is understated. Re-run the run step later."
  fi
}

run_server() {
  local server=$1 phase=$2
  local common=(--repo "$REPO" --target "$TARGET_JSON" --queries "$QUERIES" --gradle-home "$GRADLE_HOME")
  local out="$RESULTS/$server-$phase.json"
  log "3/3 $server ($phase)"
  case "$server" in
    kmp-lsp)
      python3 "$SCRIPT_DIR/run.py" --server kmp-lsp "${common[@]}" --state "$STATE/kmp-lsp" --out "$out" \
        --kmp-lsp "$KMP_LSP"
      report_dependency_sync "$out.stderr.log" "$phase" ;;
    kmp-lsp-nosync)
      python3 "$SCRIPT_DIR/run.py" --server kmp-lsp "${common[@]}" --state "$STATE/kmp-lsp-nosync" --out "$out" \
        --kmp-lsp "$KMP_LSP" --no-dependency-sync ;;
    jdtls)
      python3 "$SCRIPT_DIR/run.py" --server jdtls "${common[@]}" --state "$STATE/jdtls" --out "$out" \
        --jdtls "$SERVERS_DIR/jdtls" --java "$(java25)" --ready-timeout 1500 ;;
    kotlin-lsp)
      if [ -f "$SERVERS_DIR/kotlin-lsp/.expired" ]; then
        warn "skipping kotlin-lsp: the installed build has expired"
        return 0
      fi
      python3 "$SCRIPT_DIR/run.py" --server kotlin-lsp "${common[@]}" --state "$STATE/kotlin-lsp" --out "$out" \
        --kotlin-lsp "$SERVERS_DIR/kotlin-lsp" --ready-timeout 1800 --request-timeout 90 ;;
    *) die "unknown server: $server" ;;
  esac
}

run_phase() {
  local server=$1 phase=$2
  # A "cold" run needs an empty state directory.
  if [ "$phase" = cold ]; then
    rm -rf "${STATE:?}/$server"
  fi
  run_server "$server" "$phase" || warn "$server ($phase) failed — see $RESULTS/$server-$phase.json.stderr.log"
}

step_run() {
  [ -f "$QUERIES" ] || die "no query set at $QUERIES — run the clone step first"
  if [ $FRESH = 1 ]; then
    rm -rf "$STATE" "$RESULTS"
  fi
  mkdir -p "$STATE" "$RESULTS"
  local server phase
  # kmp-lsp without dependency sync still reads whatever sources jars are in
  # the Gradle cache, and a dependency-syncing run downloads more of them. So
  # the no-sync variant runs first, both phases, before anything syncs —
  # otherwise its result would depend on what ran before it.
  if has "$SERVERS" kmp-lsp-nosync; then
    for phase in ${PHASES//,/ }; do
      run_phase kmp-lsp-nosync "$phase"
    done
  fi
  # Then phases outermost: every server's cold run before any warm one.
  for phase in ${PHASES//,/ }; do
    for server in ${SERVERS//,/ }; do
      [ "$server" = kmp-lsp-nosync ] && continue
      run_phase "$server" "$phase"
    done
  done

  log "report"
  # Every result for these phases in the work directory, so a re-run of one
  # server is still scored against the others' earlier results.
  local results=() file
  for phase in ${PHASES//,/ }; do
    for file in "$RESULTS"/*-"$phase".json; do
      [ -f "$file" ] && results+=("$file")
    done
  done
  [ ${#results[@]} -gt 0 ] || die "no results to score"
  python3 "$SCRIPT_DIR/score.py" --queries "$QUERIES" --repo "$REPO" "${results[@]}" > "$REPORT"
  echo "report: $REPORT"
  echo "raw results: $RESULTS"
}

echo "workdir: $WORKDIR   target: $TARGET_NAME   servers: $SERVERS   phases: $PHASES   platform: $(platform)"
has "$STEPS" clone && step_clone
has "$STEPS" install && step_install
has "$STEPS" run && step_run
log "done"
