#!/usr/bin/env python3
"""Pass/fail integration test of a language server on a real repository.

Unlike `score.py` (which compares servers against each other), this scores a
run against a *committed fixture* of expected answers: `cases/<target>-<language>.json`.

The fixture is generated from a compiler-backed reference server's own run and
keeps only the queries that server got right, so the reference passes 100% by
construction. Every other server is measured against the same cases and is
held to a recorded baseline (a ratchet: lower it never, raise it when a fix
lands).

  generate — build the fixture from a reference server's result
    itest.py generate --queries Q.json --repo R --result results/jdtls-cold.json \
                      --language java --out cases/moshi-java.json

  check — score a result against the fixture, exit 1 below the baseline
    itest.py check --repo R --cases cases/moshi-java.json \
                   --result results/kmp-lsp-cold.json [--update-baseline]

Definition, references and completion are graded here; every other feature
(hover, rename, call hierarchy, formatting, document links, …) lives in
`features.py`, which also says where each one's expected answers come from:
jdtls's own answers, or — for what jdtls does not implement — the sources.
"""

import argparse
import json
import os
import sys

import features as extra
from score import Repo, label_name, matches_member, normalize_target

BASE_FEATURES = ("definition", "references", "completion")
FEATURES = BASE_FEATURES + tuple(feature.name for feature in extra.FEATURES)
INLAY_HINT_CASES = 80
COMPLETION_RANK = 10

def source_class(target):
    """`TypeToken` for gson/…/reflect/TypeToken.java — what a compiled-class
    target of the same declaration is named."""
    return os.path.splitext(os.path.basename(target[0]))[0]

def same_definition(mine, expected):
    """True when both name the same (file, line), or one names a compiled
    class and the other a line in that class's source file (a server without
    the sources jar answers with the class)."""
    if mine & expected:
        return True
    for one, other in ((mine, expected), (expected, mine)):
        compiled = {target[1] for target in one if target[0] == "external"}
        sources = {source_class(target) for target in other if target[0] != "external"}
        if compiled & sources:
            return True
    return False

def answers_by_id(result, feature):
    return {answer["id"]: answer for answer in result["answers"].get(feature, [])}

def definition_targets(repo, answer, name):
    return [list(target) for target in sorted(normalize_target(repo, location, name)
                                              for location in answer["locations"])]

def in_repo_usages(repo, answer, query):
    """In-repo usages of a references answer, the query's own line excluded."""
    found = set()
    for location in answer["locations"]:
        relative = repo.relative(location["uri"])
        if relative is None or (relative == query["file"] and location["line"] == query["line"]):
            continue
        found.add((relative, location["line"]))
    return found

def completion_rank(answer, expected):
    """1-based rank of `expected` in the answer as a client would sort it."""
    items = sorted(answer["items"],
                   key=lambda item: ((item.get("sortText") or item.get("label") or ""),
                                     item.get("label") or ""))
    for index, item in enumerate(items):
        if matches_member(label_name(item), expected):
            return index + 1
    return None

def case_of(query, extra):
    return dict({key: query[key] for key in ("id", "file", "line", "character", "name", "category")}, **extra)

def selected_features(selection):
    if selection == "all":
        return list(FEATURES)
    names = [name.strip() for name in selection.split(",") if name.strip()]
    unknown = set(names) - set(FEATURES)
    if unknown:
        raise SystemExit(f"unknown features: {', '.join(sorted(unknown))}")
    return names

def base_cases(feature, repo, queries, result):
    """Cases of definition / references / completion from the reference's answers."""
    cases = []
    for identifier, answer in answers_by_id(result, feature).items():
        query = queries.get(identifier)
        if not query or answer.get("error"):
            continue
        if feature == "definition" and answer["locations"]:
            cases.append(case_of(query, {"expect": definition_targets(repo, answer, query["name"])}))
        elif feature == "references":
            usages = in_repo_usages(repo, answer, query)
            if usages:
                cases.append(case_of(query, {"expect": sorted(list(usage) for usage in usages)}))
        elif feature == "completion":
            rank = completion_rank(answer, query["name"])
            if rank is not None and rank <= COMPLETION_RANK:
                cases.append(case_of(query, {"expect_rank_within": COMPLETION_RANK}))
    return cases

def reference_feature_cases(feature, repo, queries, result):
    """Cases of a features.py feature jdtls implements, from its answers."""
    import random
    cases = []
    for identifier, answer in answers_by_id(result, feature.name).items():
        query = queries.get(identifier)
        if query and not answer.get("error"):
            # A case keeps only what the grade checks and the reference got
            # right: the reference passes its own cases by construction.
            cases.extend(case for case in extra.reference_cases(feature, query, answer, repo)
                         if feature.grade(case, answer, repo) is None)
    if isinstance(feature, extra.InlayHint) and len(cases) > INLAY_HINT_CASES:
        cases = random.Random(7).sample(cases, INLAY_HINT_CASES)
    return cases

def generate(arguments):
    """Write (or update) the fixture: the selected features' cases are
    rebuilt, every other feature's cases and baselines are kept."""
    repo = Repo(arguments.repo)
    query_kinds = {kind: [query for query in queries if query["language"] == arguments.language]
                   for kind, queries in json.load(open(arguments.queries))["queries"].items()}
    queries = {query["id"]: query for kind in query_kinds.values() for query in kind}
    result = json.load(open(arguments.result))
    reference = result["server"]
    fixture = {}
    if os.path.exists(arguments.out):
        with open(arguments.out) as handle:
            fixture = json.load(handle)
    cases = fixture.get("cases", {})
    sources = fixture.get("sources", {feature: reference for feature in BASE_FEATURES})
    reference_baseline = fixture.get("baselines", {}).get(reference, {})

    for name in selected_features(arguments.features):
        feature = extra.BY_NAME.get(name)
        if feature is None:
            cases[name] = base_cases(name, repo, queries, result)
            sources[name] = reference
        elif feature.reference == "source":
            cases[name] = feature.source_cases(query_kinds, repo)
            sources[name] = "source"
        else:
            cases[name] = reference_feature_cases(feature, repo, queries, result)
            sources[name] = reference
        cases[name].sort(key=lambda case: case["id"])

    ordered = {name: cases[name] for name in FEATURES if name in cases}
    # The reference passes its own cases by construction; on cases from the
    # sources it is graded like any other server.
    report = score(repo, {"cases": ordered}, result)
    for name in ordered:
        reference_baseline[name] = report[name][0] if sources.get(name) == "source" else len(ordered[name])
    baselines = fixture.get("baselines", {})
    baselines[reference] = {name: reference_baseline[name] for name in ordered}
    fixture = {
        "target": result.get("target"),
        "language": arguments.language,
        "reference": reference,
        "note": "cases of the features the reference server implements are generated from its own answers "
                "(it passes them 100% by construction); features it does not implement, and formatting, "
                "are graded against the sources (`sources`, features.py)",
        "completion_rank_within": COMPLETION_RANK,
        "sources": {name: sources.get(name, reference) for name in ordered},
        "cases": ordered,
        "baselines": baselines,
    }
    with open(arguments.out, "w") as handle:
        json.dump(fixture, handle, indent=1)
    print(f"{arguments.out}: " + ", ".join(f"{len(ordered[name])} {name}" for name in ordered))

class Failure:
    """One failed case, printed the way a test runner would: where the query
    is, what was expected there, what the server answered instead."""

    def __init__(self, case, feature, expected, actual, detail=None):
        self.case, self.feature = case, feature
        self.expected, self.actual, self.detail = expected, actual, detail

    def render(self, repo, indent="  "):
        case = self.case
        lines = [f"{indent}FAIL {case['id']}  {self.feature}",
                 f"{indent}  at {case['file']}:{case['line'] + 1}:{case['character'] + 1}"
                 f"  {case['name']} ({case['category']})"]
        source = repo.file_lines(case["file"])
        if case["line"] < len(source):
            lines.append(f"{indent}  source   {source[case['line']].strip()}")
        lines.append(f"{indent}  expected {self.expected}")
        lines.append(f"{indent}  actual   {self.actual}")
        if self.detail:
            lines.extend(f"{indent}  {line}" for line in self.detail)
        return lines

def place(target):
    """`gson/…/Gson.java:850` for an in-repo target, `<compiled Gson>` for a
    library class a server answered with."""
    where, what = target
    return f"<compiled {what}>" if where == "external" else f"{where}:{what + 1}"

def places(targets):
    return ", ".join(place(target) for target in sorted(targets)) if targets else "no answer"

def usage_lines(usages, limit=5):
    shown = [f"{relative}:{line + 1}" for relative, line in sorted(usages)[:limit]]
    if len(usages) > limit:
        shown.append(f"… {len(usages) - limit} more")
    return ", ".join(shown) if shown else "none"

def definition_failure(repo, case, answer):
    """The case's Failure, or None when the answer is right."""
    expected = {tuple(target) for target in case["expect"]}
    mine = {normalize_target(repo, location, case["name"]) for location in answer["locations"]}
    if mine and same_definition(mine, expected):
        return None
    return Failure(case, "definition", places(expected), places(mine))

def references_failure(repo, case, answer):
    expected = {tuple(usage) for usage in case["expect"]}
    mine = in_repo_usages(repo, answer, case)
    if mine == expected:
        return None
    missing, extra = expected - mine, mine - expected
    detail = []
    if missing:
        detail.append(f"missing  {usage_lines(missing)}")
    if extra:
        detail.append(f"extra    {usage_lines(extra)}")
    return Failure(case, "references", f"{len(expected)} usages",
                   f"{len(mine)} usages, {len(mine & expected)} of them expected", detail)

def completion_failure(case, answer):
    rank = completion_rank(answer, case["name"])
    within = case["expect_rank_within"]
    if rank is not None and rank <= within:
        return None
    top = ", ".join(label_name(item) for item in sorted(
        answer["items"], key=lambda item: ((item.get("sortText") or item.get("label") or ""),
                                           item.get("label") or ""))[:5]) or "empty list"
    return Failure(case, "completion", f"`{case['name']}` within the top {within}",
                   f"rank {rank}" if rank else f"absent from {answer['total']} items",
                   [f"top      {top}"])

def grade(repo, feature, case, answer):
    """The case's Failure, or None when the answer is right."""
    if feature == "definition":
        return definition_failure(repo, case, answer)
    if feature == "references":
        return references_failure(repo, case, answer)
    if feature == "completion":
        return completion_failure(case, answer)
    verdict = extra.BY_NAME[feature].grade(case, answer, repo)
    if verdict is None:
        return None
    expected, actual, detail = verdict
    return Failure(case, feature, expected, actual, detail)

def answer_id(feature, case):
    return extra.BY_NAME[feature].answer_id(case) if feature in extra.BY_NAME else case["id"]

def score(repo, fixture, result):
    """feature → (passed, asked, [Failure]), for every feature with cases."""
    report = {}
    for feature in FEATURES:
        if feature not in fixture["cases"]:
            continue
        cases = fixture["cases"][feature]
        answers = answers_by_id(result, feature)
        passed, failures = 0, []
        for case in cases:
            answer = answers.get(answer_id(feature, case))
            if answer is None or answer.get("error"):
                failures.append(Failure(case, feature, "an answer",
                                        answer.get("error") if answer else "the query was not asked"))
                continue
            failure = grade(repo, feature, case, answer)
            if failure is None:
                passed += 1
            else:
                failures.append(failure)
        report[feature] = (passed, len(cases), failures)
    return report

def check(arguments):
    repo = Repo(arguments.repo)
    with open(arguments.cases) as handle:
        fixture = json.load(handle)
    result = json.load(open(arguments.result))
    server = result["server"] + ("" if result.get("dependency_sync", True) else " (no dep sync)")
    report = score(repo, fixture, result)

    print(f"{server} on {fixture['target']} ({fixture['language']}), "
          f"cases from {fixture['reference']}\n")
    baseline = fixture.get("baselines", {}).get(server)
    sources = fixture.get("sources", {})
    failed = []
    from_reference = lambda feature: sources.get(feature, fixture["reference"]) == fixture["reference"]
    groups = [(f"features {fixture['reference']} implements (cases from its answers)", True),
              (f"graded against the sources (formatting, and standard LSP {fixture['reference']} lacks)", False)]
    for title, reference_group in groups:
        members = [feature for feature in report if from_reference(feature) == reference_group]
        if members:
            print(f"  ── {title}")
        for feature in members:
            print_feature(feature, report[feature], baseline, failed, arguments, repo)
    print_totals(report)
    return finish(arguments, fixture, report, server, baseline, failed)

def print_feature(feature, scored, baseline, failed, arguments, repo):
    """One feature's score against its baseline, then its first failures."""
    passed, asked, failures = scored
    minimum = (baseline or {}).get(feature)
    verdict = "no baseline" if minimum is None else ("PASS" if passed >= minimum else "FAIL")
    if verdict == "FAIL":
        failed.append(f"{feature}: {passed}/{asked}, baseline {minimum}")
    print(f"  {feature:20} {passed:>3}/{asked:<3}"
          + (f" ({100 * passed / asked:3.0f}%)" if asked else "       ")
          + (f"  baseline {minimum:<3}" if minimum is not None else "")
          + f"  {verdict}")
    for failure in failures[:arguments.failures]:
        print("\n".join(failure.render(repo, indent="    ")))
    if len(failures) > arguments.failures:
        print(f"    … {len(failures) - arguments.failures} more failures "
              f"(--failures {len(failures)} to see them all)")

def print_totals(report):
    total_passed = sum(passed for passed, _, _ in report.values())
    total_asked = sum(asked for _, asked, _ in report.values())
    print(f"\n  {'total':20} {total_passed:>3}/{total_asked}"
          + (f" ({100 * total_passed / total_asked:.0f}%)" if total_asked else ""))

def finish(arguments, fixture, report, server, baseline, failed):
    """Record baselines when asked; else the exit status of the gate."""
    add_missing = getattr(arguments, "add_missing_baselines", False)
    if arguments.update_baseline or add_missing:
        recorded = fixture.setdefault("baselines", {}).setdefault(server, {})
        for feature in report:
            if arguments.update_baseline or feature not in recorded:
                recorded[feature] = report[feature][0]
        with open(arguments.cases, "w") as handle:
            json.dump(fixture, handle, indent=1)
        print(f"\nbaseline for {server} written to {arguments.cases}")
        return 0
    if baseline is None:
        print(f"\nno baseline for {server} in {arguments.cases} — record one with --update-baseline")
        return 1
    if failed:
        print("\nbelow baseline: " + "; ".join(failed))
        return 1
    print("\nall features at or above baseline")
    return 0

def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    commands = parser.add_subparsers(dest="command", required=True)

    generator = commands.add_parser("generate")
    generator.add_argument("--queries", required=True)
    generator.add_argument("--repo", required=True)
    generator.add_argument("--result", required=True, help="run.py result of the reference server")
    generator.add_argument("--language", required=True, choices=["java", "kotlin"])
    generator.add_argument("--out", required=True)
    generator.add_argument("--features", default="all",
                           help="features whose cases to (re)build, comma-separated; the rest are kept")

    checker = commands.add_parser("check")
    checker.add_argument("--repo", required=True)
    checker.add_argument("--cases", required=True)
    checker.add_argument("--result", required=True)
    checker.add_argument("--failures", type=int, default=10, help="failures to list per feature")
    checker.add_argument("--update-baseline", action="store_true")
    checker.add_argument("--add-missing-baselines", action="store_true",
                         help="record the score of features without a baseline; leave the others alone")

    arguments = parser.parse_args()
    if arguments.command == "generate":
        generate(arguments)
        return 0
    return check(arguments)

if __name__ == "__main__":
    sys.exit(main())
