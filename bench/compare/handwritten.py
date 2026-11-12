"""Hand-written cases of the integration test.

The generated fixture (`cases/<target>-java.json`) is a random sample of the
target's identifiers, kept where jdtls answered: it says "nothing that used to
work broke", but it never aims at a shape on purpose. The cases here do. Each
one was picked by reading the target's code for a Java rule a resolver can get
wrong — the overload `value(123)` selects, a field shadowed by a parameter, an
enum constant in a `switch` label, a method of an anonymous class — and its
expected answer was written by hand from the Java Language Specification, not
taken from any server. jdtls must still pass every one of them (a case it
fails is a case to re-read, not to drop silently), and kmp-lsp is held to its
own baseline, recorded next to the cases.

Authoring format (`cases/<target>-java-handwritten.json`), lines 1-based as an
editor shows them:

  {"id": "definition-value-int-picks-long",
   "kind": "definition",                # definition | references | completion
   "file": "gson/src/test/…/JsonWriterTest.java", "line": 95,
   "at": "value", "nth": 1,             # the identifier asked about; nth on the line
   "category": "member_call",
   "why": "an int argument widens to long: value(long), not value(double)",
   "expect": {"definition": "gson/src/main/…/JsonWriter.java:614",
              "signatureHelp": 1}}

`expect` keys are features; a case is graded only on the ones it names:

  definition, declaration, typeDefinition
             "path:line" or a list of them; "external:Class" for a library or
             JDK class (graded like the generated cases: a server without the
             sources may answer with the compiled class)
  references, implementation, callHierarchy
             the complete in-repo list of "path:line" (the asked line excluded)
  completion true (the deleted member ranks in the top 10) or the rank to beat
  signatureHelp
             the parameter count of the overload the arguments select
  hover      a word the hover text must contain
"""

import json
import re

from features import BY_NAME

PREFIX = "hand-"
COMPLETION_RANK = 10

# Which query kind each feature is asked on (run.py / features.py) — a case
# can only expect what its kind is asked.
ASKED_ON = {
    "definition": {"definition", "hover", "declaration", "typeDefinition", "signatureHelp"},
    "references": {"references", "implementation", "callHierarchy"},
    "completion": {"completion"},
}
LOCATION_FEATURES = {"definition", "declaration", "typeDefinition"}
SET_FEATURES = {"references", "implementation", "callHierarchy"}
DECLARATION_KEYWORD = re.compile(r"\b(class|interface|enum|record|@interface)\b")


class AuthoringError(Exception):
    pass


def word_columns(text, word):
    return [match.start() for match in re.finditer(r"(?<![\w$])" + re.escape(word) + r"(?![\w$])", text)]


def load(path, repo):
    """(queries, fixture) from the hand-written file: queries in queries.py's
    shape (`hand-` ids, 0-based positions), and a fixture `itest.score`
    grades like the generated one. Raises AuthoringError on a case whose
    position or expectation does not match the sources."""
    with open(path) as handle:
        authored = json.load(handle)
    queries = {kind: [] for kind in ASKED_ON}
    cases = {}
    errors = []
    seen = set()
    for entry in authored["cases"]:
        try:
            if entry["id"] in seen:
                raise AuthoringError("duplicate id")
            seen.add(entry["id"])
            query = query_of(entry, repo)
            queries[entry["kind"]].append(query)
            for feature, expected in entry["expect"].items():
                cases.setdefault(feature, []).append(case_of(entry, query, feature, expected, repo))
        except (AuthoringError, KeyError) as error:
            errors.append(f"{entry.get('id', '?')}: {error}")
    if errors:
        raise AuthoringError("hand-written cases do not match the sources:\n  " + "\n  ".join(errors))
    return queries, {"cases": cases, "baselines": authored.get("baselines", {})}


def query_of(entry, repo):
    kind = entry["kind"]
    if kind not in ASKED_ON:
        raise AuthoringError(f"unknown kind {kind!r}")
    lines = repo.file_lines(entry["file"])
    if not lines:
        raise AuthoringError(f"no file {entry['file']}")
    if not 1 <= entry["line"] <= len(lines):
        raise AuthoringError(f"line {entry['line']} outside {entry['file']}")
    text = lines[entry["line"] - 1]
    columns = word_columns(text, entry["at"])
    nth = entry.get("nth", 1)
    if len(columns) < nth:
        raise AuthoringError(f"`{entry['at']}` (#{nth}) not on line {entry['line']}: {text.strip()}")
    return {"id": PREFIX + entry["id"], "language": "java", "file": entry["file"],
            "line": entry["line"] - 1, "character": columns[nth - 1],
            "name": entry["at"], "category": entry["category"]}


def target(spec, repo, name, feature):
    """[file, 0-based line] or ["external", Class] for a "path:line" /
    "external:Class" spec, checked against the sources: the line must hold
    the name (or, for typeDefinition, a type declaration) — the line a
    grader snaps a server's answer to."""
    where, _, what = spec.rpartition(":")
    if where == "external":
        return ["external", what]
    lines = repo.file_lines(where)
    if not lines:
        raise AuthoringError(f"no file {where}")
    line = int(what)
    if not 1 <= line <= len(lines):
        raise AuthoringError(f"{spec}: outside the file")
    if not holds_target(lines[line - 1], name, feature):
        raise AuthoringError(f"{feature} {spec} does not hold `{name}`: {lines[line - 1].strip()}")
    return [where, line - 1]


def holds_target(text, name, feature):
    """What the expected line must show: the type declaration a
    typeDefinition lands on, an implementing type's declaration (an
    implementation of a type, whose name differs), else the name itself."""
    if feature == "typeDefinition" or (feature == "implementation" and name[:1].isupper()):
        return bool(DECLARATION_KEYWORD.search(text))
    return bool(word_columns(text, name))


def case_of(entry, query, feature, expected, repo):
    if feature not in ASKED_ON[entry["kind"]]:
        raise AuthoringError(f"{feature} is not asked on {entry['kind']} queries")
    case = {key: query[key] for key in ("id", "file", "line", "character", "name", "category")}
    case["why"] = entry["why"]
    if feature in LOCATION_FEATURES:
        specs = [expected] if isinstance(expected, str) else expected
        case["expect"] = [target(spec, repo, query["name"], feature) for spec in specs]
    elif feature in SET_FEATURES:
        usages = [target(spec, repo, query["name"], feature) for spec in expected]
        if len({tuple(usage) for usage in usages}) != len(usages):
            raise AuthoringError(f"{feature}: a line listed twice")
        case["expect"] = sorted(usages)
    elif feature == "completion":
        case["expect_rank_within"] = COMPLETION_RANK if expected is True else int(expected)
    elif feature == "signatureHelp":
        if query["category"] not in ("call", "member_call"):
            raise AuthoringError("signatureHelp is asked on calls only")
        line = repo.file_lines(query["file"])[query["line"]]
        after = query["character"] + len(query["name"])
        if line[after:after + 1] != "(":
            raise AuthoringError("signatureHelp needs `(` right after the name")
        case["expect_parameters"], case["expect_label"] = int(expected), query["name"]
    elif feature == "hover":
        case["expect_mentions"] = expected
    else:
        raise AuthoringError(f"no hand-written grading for {feature}")
    if feature not in BY_NAME and feature not in ("definition", "references", "completion"):
        raise AuthoringError(f"unknown feature {feature}")
    return case


def merge_queries(query_file, hand_queries):
    """Add the hand-written queries to a queries.py file (replacing any added
    before), so every run asks them alongside the sampled ones."""
    with open(query_file) as handle:
        query_set = json.load(handle)
    for kind, added in hand_queries.items():
        kept = [query for query in query_set["queries"].get(kind, []) if not query["id"].startswith(PREFIX)]
        query_set["queries"][kind] = kept + added
    with open(query_file, "w") as handle:
        json.dump(query_set, handle, indent=1)
    return sum(len(added) for added in hand_queries.values())
