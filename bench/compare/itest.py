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
"""

import argparse
import json
import os
import sys

from score import Repo, label_name, matches_member, normalize_target

FEATURES = ("definition", "references", "completion")
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

def generate(arguments):
    repo = Repo(arguments.repo)
    queries = {query["id"]: query
               for kind in json.load(open(arguments.queries))["queries"].values()
               for query in kind
               if query["language"] == arguments.language}
    result = json.load(open(arguments.result))
    cases = {feature: [] for feature in FEATURES}

    for identifier, answer in answers_by_id(result, "definition").items():
        query = queries.get(identifier)
        if not query or answer.get("error") or not answer["locations"]:
            continue
        cases["definition"].append(case_of(query, {"expect": definition_targets(repo, answer, query["name"])}))

    for identifier, answer in answers_by_id(result, "references").items():
        query = queries.get(identifier)
        if not query or answer.get("error"):
            continue
        usages = in_repo_usages(repo, answer, query)
        if not usages:
            continue
        cases["references"].append(case_of(query, {"expect": sorted(list(usage) for usage in usages)}))

    for identifier, answer in answers_by_id(result, "completion").items():
        query = queries.get(identifier)
        if not query or answer.get("error"):
            continue
        rank = completion_rank(answer, query["name"])
        if rank is None or rank > COMPLETION_RANK:
            continue
        cases["completion"].append(case_of(query, {"expect_rank_within": COMPLETION_RANK}))

    for feature in FEATURES:
        cases[feature].sort(key=lambda case: case["id"])
    fixture = {
        "target": result.get("target"),
        "language": arguments.language,
        "reference": result["server"],
        "note": "generated from the reference server's own answers: it passes 100% by construction",
        "completion_rank_within": COMPLETION_RANK,
        "cases": cases,
        "baselines": {result["server"]: {feature: len(cases[feature]) for feature in FEATURES}},
    }
    with open(arguments.out, "w") as handle:
        json.dump(fixture, handle, indent=1)
    print(f"{arguments.out}: " + ", ".join(f"{len(cases[feature])} {feature}" for feature in FEATURES))

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

def score(repo, fixture, result):
    """feature → (passed, asked, [Failure])."""
    report = {}
    for feature in FEATURES:
        cases = fixture["cases"][feature]
        answers = answers_by_id(result, feature)
        passed, failures = 0, []
        for case in cases:
            answer = answers.get(case["id"])
            if answer is None or answer.get("error"):
                failures.append(Failure(case, feature, "an answer",
                                        answer.get("error") if answer else "the query was not asked"))
                continue
            if feature == "definition":
                failure = definition_failure(repo, case, answer)
            elif feature == "references":
                failure = references_failure(repo, case, answer)
            else:
                failure = completion_failure(case, answer)
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
    failed = []
    for feature, (passed, asked, failures) in report.items():
        minimum = (baseline or {}).get(feature)
        verdict = "no baseline" if minimum is None else ("PASS" if passed >= minimum else "FAIL")
        if verdict == "FAIL":
            failed.append(f"{feature}: {passed}/{asked}, baseline {minimum}")
        print(f"  {feature:12} {passed}/{asked}"
              + (f" ({100 * passed / asked:.0f}%)" if asked else "")
              + (f"  baseline {minimum}" if minimum is not None else "")
              + f"  {verdict}")
        for failure in failures[:arguments.failures]:
            print("\n".join(failure.render(repo, indent="    ")))
        if len(failures) > arguments.failures:
            print(f"    … {len(failures) - arguments.failures} more failures "
                  f"(--failures {len(failures)} to see them all)")

    if arguments.update_baseline:
        fixture.setdefault("baselines", {})[server] = {feature: report[feature][0] for feature in FEATURES}
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

    checker = commands.add_parser("check")
    checker.add_argument("--repo", required=True)
    checker.add_argument("--cases", required=True)
    checker.add_argument("--result", required=True)
    checker.add_argument("--failures", type=int, default=10, help="failures to list per feature")
    checker.add_argument("--update-baseline", action="store_true")

    arguments = parser.parse_args()
    if arguments.command == "generate":
        generate(arguments)
        return 0
    return check(arguments)

if __name__ == "__main__":
    sys.exit(main())
