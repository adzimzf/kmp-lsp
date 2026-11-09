#!/usr/bin/env python3
"""Score runs against each other and against source-derived ground truth.

Usage: score.py --queries queries.json --repo <repo> RESULT.json... > report.md

Accuracy
  definition  — no compiler is available to the harness, so the reference is
                the compiler-backed server that owns the query's language:
                JetBrains kotlin-lsp for `.kt`, jdtls for `.java`; when the
                owner has no answer (or wasn't run) the other compiler-backed
                server is used. A server is right when one of its targets is a
                reference target. Targets are normalized to (file, line of the
                declared name); library targets to their class/file name.
                Agreement with each server separately is reported too.
  references  — reference set per query: usages in `.java` files from jdtls,
                usages in `.kt` files from kotlin-lsp (falling back to the
                other when one wasn't run). Precision and recall over in-repo
                (file, line) pairs, the declaration's own line excluded.
  completion  — ground truth from the source: the member the author wrote
                was deleted and completion asked after the `.`; rank of that
                name in the list as the client would sort it (sortText, then
                label). Java getters match Kotlin property names and back.
Speed: phases from process start, per-request p50/p95/max, timeouts, memory.
"""

import argparse
import json
import os
import re
import statistics
import urllib.parse
from collections import defaultdict

OWNER = {"kotlin": "kotlin-lsp", "java": "jdtls"}
OTHER = {"kotlin-lsp": "jdtls", "jdtls": "kotlin-lsp"}


class Repo:
    def __init__(self, root):
        self.root = os.path.realpath(root)
        self.lines = {}

    def relative(self, uri):
        """Repository-relative path for an in-repo file URI, else None."""
        parsed = urllib.parse.urlparse(uri)
        if parsed.scheme != "file":
            return None
        path = os.path.realpath(urllib.parse.unquote(parsed.path))
        if not path.startswith(self.root + os.sep):
            return None
        relative = os.path.relpath(path, self.root)
        if "/build/" in "/" + relative:
            return None
        return relative

    def file_lines(self, relative):
        if relative not in self.lines:
            try:
                with open(os.path.join(self.root, relative), encoding="utf-8") as handle:
                    self.lines[relative] = handle.read().split("\n")
            except OSError:
                self.lines[relative] = []
        return self.lines[relative]


def external_name(uri):
    """`String` from jdt://…/String.class?…, jar:…!/java/lang/String.java, …/String.kt."""
    path = urllib.parse.unquote(uri.split("?", 1)[0]).replace("!", "/")
    segment = re.split(r"[/\\]", path.rstrip("/"))[-1]
    segment = re.sub(r"\.(class|java|kt|kts)$", "", segment)
    return segment.split("$")[0]


COMPILED_STUB_DIR = "/compiled-stubs/"


def compiled_stub_class(location):
    """kmp-lsp opens a compiled library class in a per-jar stub listing
    (`…/compiled-stubs/java.base-<hash>.kt`), one declaration per line:
    `class String    // java.lang` or `fun endsWith(…)    // java.lang.String`.
    The class the target line belongs to, or None for any other file."""
    parsed = urllib.parse.urlparse(location["uri"])
    path = urllib.parse.unquote(parsed.path)
    if parsed.scheme != "file" or COMPILED_STUB_DIR not in path:
        return None
    try:
        with open(path, encoding="utf-8") as handle:
            for number, text in enumerate(handle):
                if number == location["line"]:
                    break
            else:
                return None
    except OSError:
        return None
    declaration, _, owner = text.partition("//")
    declared = re.match(r"\s*(?:class|interface|object|enum class|annotation class)\s+(\w+)", declaration)
    if declared:
        return declared.group(1)
    owner = owner.strip()
    return owner.rsplit(".", 1)[-1].split("$")[0] if owner else None


def normalize_target(repo, location, name):
    relative = repo.relative(location["uri"])
    if relative is None:
        stub_class = compiled_stub_class(location)
        return ("external", stub_class or external_name(location["uri"]))
    lines = repo.file_lines(relative)
    line = location["line"]
    # Some servers point at the declaration's first line (annotations,
    # modifiers), others at the name: settle on the line holding the name.
    word = re.compile(r"\b" + re.escape(name) + r"\b")
    for candidate in range(line, min(line + 8, len(lines))):
        if word.search(lines[candidate]):
            line = candidate
            break
    return (relative, line)


def percentile(values, fraction):
    if not values:
        return None
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, int(round(fraction * (len(ordered) - 1)))))
    return ordered[index]


def fmt_seconds(value):
    if value is None:
        return "—"
    return f"{value * 1000:.0f} ms" if value < 1 else f"{value:.1f} s"


def fmt_percent(part, whole):
    return "—" if not whole else f"{100 * part / whole:.0f}% ({part}/{whole})"


def label_name(item):
    for text in (item.get("filterText"), item.get("label"), item.get("insertText")):
        if text:
            return re.split(r"[(\s:<]", text.strip(), maxsplit=1)[0]
    return ""


def matches_member(candidate, expected):
    if candidate == expected:
        return True
    capitalized = lambda text: text[:1].upper() + text[1:]
    getters = {"get" + capitalized(expected), "is" + capitalized(expected), "set" + capitalized(expected)}
    if candidate in getters:
        return True
    for prefix in ("get", "is", "set"):
        if expected.startswith(prefix) and expected[len(prefix):len(prefix) + 1].isupper():
            if candidate == expected[len(prefix)].lower() + expected[len(prefix) + 1:]:
                return True
    return False


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--queries", required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--disagreements", type=int, default=15)
    parser.add_argument("results", nargs="+")
    arguments = parser.parse_args()

    repo = Repo(arguments.repo)
    query_set = json.load(open(arguments.queries))["queries"]
    queries = {query["id"]: query for kind in query_set.values() for query in kind}
    runs = [json.load(open(path)) for path in arguments.results]
    label = lambda run: f"{run['server']} ({run['start']}{'' if run.get('dependency_sync', True) else ', no dep sync'})"
    # Accuracy uses each server's first run (answers don't depend on cold/warm).
    # A server run without dependency sync is scored as its own variant.
    variant = lambda run: run["server"] + ("" if run.get("dependency_sync", True) else " (no dep sync)")
    by_server = {}
    for run in runs:
        by_server.setdefault(variant(run), run)

    out = []
    emit = out.append
    emit("# kmp-lsp vs jdtls vs JetBrains kotlin-lsp\n")
    emit(f"Target: `{runs[0].get('target')}` — {len(queries)} queries "
         f"({', '.join(f'{len(v)} {k}' for k, v in query_set.items())}).\n")

    # ── speed ──────────────────────────────────────────────────────────────
    emit("## Speed\n")
    emit("| Server | initialize | ready (probes answer correctly) | settled (no background work for 5 s) | peak RSS | RSS at settled | total run |")
    emit("|---|---|---|---|---|---|---|")
    for run in runs:
        phases = run["phases"]
        emit(f"| {label(run)} | {fmt_seconds(phases.get('initialize'))} | "
             f"{fmt_seconds(phases.get('ready')) if phases.get('ready') else 'never: ' + run.get('error', '')} | "
             f"{fmt_seconds(phases.get('settled'))} | "
             f"{run['peak_rss_kib'] // 1024} MiB | {run.get('rss_at_settled_kib', run.get('rss_at_ready_kib', 0)) // 1024} MiB | "
             f"{fmt_seconds(run.get('total_seconds'))} |")
    emit("")
    emit("Per-request latency after ready (p50 / p95 / max; timeouts):\n")
    kinds = ["definition", "hover", "references", "completion", "documentSymbol"]
    emit("| Server | " + " | ".join(kinds) + " |")
    emit("|---|" + "---|" * len(kinds))
    for run in runs:
        cells = []
        for kind in kinds:
            answers = run["answers"].get(kind, [])
            seconds = [answer["seconds"] for answer in answers if answer.get("seconds") is not None and not answer.get("error")]
            timeouts = sum(1 for answer in answers if answer.get("error") == "timeout")
            if not answers:
                cells.append("not asked")
                continue
            cell = f"{fmt_seconds(percentile(seconds, 0.5))} / {fmt_seconds(percentile(seconds, 0.95))} / {fmt_seconds(max(seconds) if seconds else None)}"
            if timeouts:
                cell += f"; {timeouts} timeouts"
            cells.append(cell)
        emit(f"| {label(run)} | " + " | ".join(cells) + " |")
    emit("")

    # ── definition accuracy ────────────────────────────────────────────────
    targets = defaultdict(dict)  # id → server → set
    errors = defaultdict(dict)
    for server, run in by_server.items():
        for answer in run["answers"].get("definition", []):
            query = queries[answer["id"]]
            targets[answer["id"]][server] = {normalize_target(repo, location, query["name"]) for location in answer["locations"]}
            errors[answer["id"]][server] = answer.get("error")

    def file_class(target):
        """`JsonReader` for moshi/…/JsonReader.kt — what a compiled-class
        target of the same declaration is named."""
        path, _ = target
        return os.path.splitext(os.path.basename(path))[0]

    def compiled_name(target):
        name = target[1]
        return name[:-2] if name.endswith("Kt") else name

    def match(left, right):
        """'exact' when both name the same (file, line); 'class' when one
        names a compiled class and the other a line in that class's source
        file (jdtls can't open Kotlin sources, so it answers with the
        compiled class); None otherwise."""
        if left & right:
            return "exact"
        for one, other in ((left, right), (right, left)):
            compiled = {compiled_name(target) for target in one if target[0] == "external"}
            sources = {file_class(target) for target in other if target[0] != "external"}
            if compiled & sources:
                return "class"
        return None

    def same(left, right):
        return match(left, right) is not None

    def reference_for(query_id, exclude=None):
        query = queries[query_id]
        owner = OWNER[query["language"]]
        for candidate in (owner, OTHER[owner]):
            if candidate == exclude:
                continue
            answer = targets[query_id].get(candidate)
            if answer:
                return candidate, answer
        return None, set()

    emit("## Go-to-definition accuracy\n")
    emit("Reference: the compiler-backed server owning the language (kotlin-lsp for `.kt`, jdtls for `.java`), "
         "falling back to the other one. A server is never its own fallback reference.\n")
    emit("*right* counts exact matches plus *class-level* ones: one server names a compiled class "
         "(jdtls on moshi's Kotlin core) and the other a line in that class's source file; "
         "the class-level share is shown separately.\n")
    emit("| Server | language | asked | reference had an answer | right | of which class-level | wrong | no answer | errors |")
    emit("|---|---|---|---|---|---|---|---|---|")
    disagreements = defaultdict(list)
    for server in by_server:
        for language in ("kotlin", "java"):
            ids = [qid for qid in targets if server in targets[qid] and queries[qid]["language"] == language]
            if not ids:
                continue
            with_reference = right = class_level = wrong = empty = failed = 0
            for qid in ids:
                reference_server, reference = reference_for(qid)
                if reference_server == server:
                    reference_server, reference = reference_for(qid, exclude=server)
                if errors[qid].get(server):
                    failed += 1
                if not reference:
                    continue
                with_reference += 1
                mine = targets[qid][server]
                if not mine:
                    empty += 1
                elif same(mine, reference):
                    right += 1
                    class_level += match(mine, reference) == "class"
                else:
                    wrong += 1
                    disagreements[server].append((qid, reference_server, sorted(mine), sorted(reference)))
            emit(f"| {server} | {language} | {len(ids)} | {with_reference} | {fmt_percent(right, with_reference)} | "
                 f"{class_level} | {wrong} | {empty} | {failed} |")
    emit("")
    servers = list(by_server)
    if len(servers) > 1:
        emit("Pairwise agreement (queries both answered → same target):\n")
        emit("| | " + " | ".join(servers) + " |")
        emit("|---|" + "---|" * len(servers))
        for left in servers:
            cells = []
            for right_server in servers:
                both = [qid for qid in targets if targets[qid].get(left) and targets[qid].get(right_server)]
                agree = sum(1 for qid in both if same(targets[qid][left], targets[qid][right_server]))
                cells.append("—" if left == right_server else fmt_percent(agree, len(both)))
            emit(f"| {left} | " + " | ".join(cells) + " |")
        emit("")
    by_category = defaultdict(lambda: defaultdict(lambda: [0, 0]))
    for qid in targets:
        for server in by_server:
            if server not in targets[qid]:
                continue
            reference_server, reference = reference_for(qid)
            if reference_server == server:
                reference_server, reference = reference_for(qid, exclude=server)
            if not reference:
                continue
            key = f"{queries[qid]['language']} {queries[qid]['category']}"
            cell = by_category[key][server]
            cell[1] += 1
            cell[0] += same(targets[qid][server], reference)
    if by_category:
        emit("Right answers by query shape:\n")
        emit("| shape | " + " | ".join(servers) + " |")
        emit("|---|" + "---|" * len(servers))
        for key in sorted(by_category):
            emit(f"| {key} | " + " | ".join(fmt_percent(*by_category[key][s]) if by_category[key][s][1] else "—" for s in servers) + " |")
        emit("")

    emit("Hover answered (non-empty) on the same positions:\n")
    emit("| Server | kotlin | java |")
    emit("|---|---|---|")
    for server, run in by_server.items():
        cells = []
        for language in ("kotlin", "java"):
            hovers = [answer for answer in run["answers"].get("hover", []) if queries[answer["id"]]["language"] == language]
            cells.append(fmt_percent(sum(1 for answer in hovers if not answer["empty"]), len(hovers)) if hovers else "not asked")
        emit(f"| {server} | " + " | ".join(cells) + " |")
    emit("")

    # ── references accuracy ────────────────────────────────────────────────
    usages = defaultdict(dict)
    for server, run in by_server.items():
        for answer in run["answers"].get("references", []):
            query = queries[answer["id"]]
            found = set()
            for location in answer["locations"]:
                relative = repo.relative(location["uri"])
                if relative is None:
                    continue
                if relative == query["file"] and location["line"] == query["line"]:
                    continue
                found.add((relative, location["line"]))
            usages[answer["id"]][server] = found

    def reference_usages(qid, exclude):
        reference, sources = set(), []
        for extension, owner in ((".java", "jdtls"), (".kt", "kotlin-lsp")):
            chosen = owner if owner != exclude and owner in usages[qid] else OTHER[owner]
            if chosen == exclude or chosen not in usages[qid]:
                continue
            reference |= {usage for usage in usages[qid][chosen] if usage[0].endswith(extension)}
            sources.append(f"{extension}:{chosen}")
        return reference, sources

    emit("## Find-references accuracy\n")
    emit("Reference set: `.java` usages from jdtls, `.kt` usages from kotlin-lsp (in-repo, declaration line excluded). "
         "Micro-averaged over queries whose reference found at least one usage.\n")
    emit("| Server | language | queries scored | precision | recall | exact match |")
    emit("|---|---|---|---|---|---|")
    for server in by_server:
        for language in ("kotlin", "java"):
            ids = [qid for qid in usages if server in usages[qid] and queries[qid]["language"] == language]
            hits = found_total = reference_total = exact = scored = 0
            for qid in ids:
                reference, sources = reference_usages(qid, exclude=server)
                if not reference:
                    continue
                mine = usages[qid][server]
                scored += 1
                hits += len(mine & reference)
                found_total += len(mine)
                reference_total += len(reference)
                exact += mine == reference
            if scored:
                emit(f"| {server} | {language} | {scored} | {fmt_percent(hits, found_total)} | "
                     f"{fmt_percent(hits, reference_total)} | {fmt_percent(exact, scored)} |")
    emit("")

    # ── completion accuracy ────────────────────────────────────────────────
    emit("## Completion accuracy (ground truth from the source)\n")
    emit("`receiver.member` with `member` deleted; where does `member` rank?\n")
    emit("| Server | language | asked | top 1 | top 5 | top 10 | anywhere | MRR | empty list | median list size |")
    emit("|---|---|---|---|---|---|---|---|---|---|")
    for server, run in by_server.items():
        for language in ("kotlin", "java"):
            answers = [answer for answer in run["answers"].get("completion", []) if queries[answer["id"]]["language"] == language]
            if not answers:
                continue
            ranks, empty, sizes = [], 0, []
            for answer in answers:
                expected = queries[answer["id"]]["name"]
                items = sorted(answer["items"], key=lambda item: ((item.get("sortText") or item.get("label") or ""), item.get("label") or ""))
                sizes.append(answer["total"])
                if not items:
                    empty += 1
                rank = next((index + 1 for index, item in enumerate(items) if matches_member(label_name(item), expected)), None)
                ranks.append(rank)
            asked = len(answers)
            top = lambda k: sum(1 for rank in ranks if rank is not None and rank <= k)
            mrr = sum(1 / rank for rank in ranks if rank) / asked
            emit(f"| {server} | {language} | {asked} | {fmt_percent(top(1), asked)} | {fmt_percent(top(5), asked)} | "
                 f"{fmt_percent(top(10), asked)} | {fmt_percent(top(10**9), asked)} | {mrr:.2f} | {fmt_percent(empty, asked)} | "
                 f"{statistics.median(sizes) if sizes else 0:.0f} |")
    emit("")

    # ── disagreements to spot-check ────────────────────────────────────────
    if arguments.disagreements and disagreements:
        emit("## Definition disagreements to spot-check\n")
        for server, rows in disagreements.items():
            emit(f"<details><summary>{server}: {len(rows)} answers differ from the reference</summary>\n")
            emit("| query | source line | answered | reference |")
            emit("|---|---|---|---|")
            for qid, reference_server, mine, reference in rows[:arguments.disagreements]:
                query = queries[qid]
                source = repo.file_lines(query["file"])[query["line"]].strip().replace("|", "\\|")[:70]
                show = lambda values: "<br>".join(f"{path}:{line + 1}" if isinstance(line, int) else f"{path}:{line}" for path, line in values[:3])
                emit(f"| `{query['name']}` {query['file'].split('/')[-1]}:{query['line'] + 1} | `{source}` | {show(mine)} | {reference_server}: {show(reference)} |")
            emit("\n</details>\n")
    print("\n".join(out))


if __name__ == "__main__":
    main()
