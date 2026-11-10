#!/usr/bin/env python3
"""Does the gate actually fail? `python3 itest_selfcheck.py` — no framework.

Scoring is only useful if a regression exits non-zero, so check that on a
synthetic result: one right answer, one wrong, one missing, against a
fixture whose baseline demands all three.
"""

import json
import os
import pathlib
import tempfile

import itest

REPO = pathlib.Path(tempfile.mkdtemp()) / "repo"
(REPO / "src").mkdir(parents=True)
(REPO / "src" / "Target.java").write_text("class Target {\n  void call() {}\n}\n")
(REPO / "src" / "Caller.java").write_text("class Caller {\n  void run() { new Target().call(); }\n}\n")

def uri(relative):
    return (REPO / relative).as_uri()

def case(identifier, line, name, extra):
    return dict({"id": identifier, "file": "src/Caller.java", "line": line, "character": 20,
                 "name": name, "category": "member_call"}, **extra)

fixture = {
    "target": "selfcheck", "language": "java", "reference": "reference",
    "completion_rank_within": 10,
    "cases": {
        "definition": [case("right", 1, "call", {"expect": [["src/Target.java", 1]]}),
                       case("wrong", 1, "call", {"expect": [["src/Target.java", 1]]}),
                       case("missing", 1, "call", {"expect": [["src/Target.java", 1]]})],
        "references": [], "completion": [],
    },
    "baselines": {"server": {"definition": 3, "references": 0, "completion": 0}},
}

def location(relative, line):
    return {"uri": uri(relative), "line": line, "character": 7}

# The wrong answer has to be in another file: `normalize_target` snaps a
# location onto the line holding the name, so any line of Target.java near
# `void call()` normalizes onto it — and would count as right.
result = {"server": "server", "answers": {"definition": [
    {"id": "right", "locations": [location("src/Target.java", 1)]},
    {"id": "wrong", "locations": [location("src/Caller.java", 1)]},
]}}

report = itest.score(itest.Repo(str(REPO)), fixture, result)
passed, asked, failures = report["definition"]
assert (passed, asked) == (1, 3), (passed, asked)
assert [failure.case["id"] for failure in failures] == ["wrong", "missing"]

wrong = "\n".join(failures[0].render(itest.Repo(str(REPO))))
assert "src/Target.java:2" in wrong, wrong      # expected, 1-based
assert "src/Caller.java:2" in wrong, wrong      # actual
assert "new Target().call();" in wrong, wrong   # the source line queried
assert "the query was not asked" in "\n".join(failures[1].render(itest.Repo(str(REPO))))

path = os.path.join(tempfile.mkdtemp(), "cases.json")
json.dump(fixture, open(path, "w"))
json.dump(result, open(path + ".result", "w"))
arguments = type("Arguments", (), {"repo": str(REPO), "cases": path, "result": path + ".result",
                                   "failures": 0, "update_baseline": False})()
assert itest.check(arguments) == 1, "a score below baseline must exit non-zero"

arguments.update_baseline = True
assert itest.check(arguments) == 0
assert json.load(open(path))["baselines"]["server"]["definition"] == 1

print("ok: failures name the expected and the actual location, and a drop exits 1")

# A features.py feature goes through the same gate: a highlight set that
# misses an occurrence fails, and the failure names what is missing.
fixture["cases"]["documentHighlight"] = [case("highlight", 1, "call", {"expect": [[1, 20], [1, 30]]})]
fixture["baselines"]["server"] = {"documentHighlight": 1}
result["answers"]["documentHighlight"] = [{"id": "highlight", "positions": [[1, 20]]}]
report = itest.score(itest.Repo(str(REPO)), fixture, result)
passed, asked, failures = report["documentHighlight"]
assert (passed, asked) == (0, 1), (passed, asked)
assert "missing  2:31" in "\n".join(failures[0].render(itest.Repo(str(REPO)))), failures[0].render(itest.Repo(str(REPO)))
json.dump(fixture, open(path, "w"))
json.dump(result, open(path + ".result", "w"))
arguments.update_baseline = False
assert itest.check(arguments) == 1, "a features.py feature below baseline must exit non-zero"

print("ok: features.py features are gated the same way")

# A usage the reference is known to miss (`reference_misses`) is neither
# required nor counted as extra; any other extra usage still fails.
fixture["cases"]["references"] = [case("usages", 1, "call", {"expect": [["src/Caller.java", 2]]})]
fixture["reference_misses"] = {"references": {"usages": [
    {"file": "src/Target.java", "line": 0, "why": "selfcheck: the reference misses it"}]}}
result["answers"]["references"] = [{"id": "usages", "locations": [
    location("src/Caller.java", 2), location("src/Target.java", 0)]}]
passed, asked, failures = itest.score(itest.Repo(str(REPO)), fixture, result)["references"]
assert (passed, asked) == (1, 1), [failure.render(itest.Repo(str(REPO))) for failure in failures]

result["answers"]["references"][0]["locations"].append(location("src/Target.java", 2))
passed, asked, failures = itest.score(itest.Repo(str(REPO)), fixture, result)["references"]
assert (passed, asked) == (0, 1), (passed, asked)
assert "extra    src/Target.java:3" in "\n".join(failures[0].render(itest.Repo(str(REPO))))

print("ok: a known reference miss is ungraded, any other extra usage still fails")
