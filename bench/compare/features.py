"""LSP features beyond definition / references / completion, for the
integration test (`itest.py`, `run.py --features all`).

Every feature knows three things:

* how to **ask** a server (run.py): which queries it uses, which request it
  sends, and what of the answer it records;
* where its **expected answers** come from:
  - `reference = "jdtls"` — the feature is one jdtls implements: a case is a
    query jdtls answered (and, where it applies, answered right), so jdtls
    passes 100% by construction, as for definition/references/completion;
  - `reference = "source"` — jdtls does not implement it (or its answer is a
    style choice, as for formatting): the expected answer is derived from the
    target's own sources, which compile, and every server — jdtls included —
    is graded against that;
* how to **grade** one answer against one case: `None` when it is right,
  else `(expected, actual, [detail lines])`.

Queries come from `queries.py`: `definition` (identifier usages),
`references` (declarations), `files` (a file and its top-level type),
`imports` (an import whose deletion breaks a usage), `links` (a file with
URLs in its comments).
"""

import os
import pathlib
import re
import time

from lsp import LspError, RequestTimeout
from queries import blank_comments_and_strings, literal_spans
from score import compiled_stub_class, external_name

# ── LSP plumbing ──────────────────────────────────────────────────────────


def uri_of(repo, relative):
    return pathlib.Path(repo, relative).resolve().as_uri()


def locations(result):
    """Location | Location[] | LocationLink[] → [{uri, line, character}]."""
    if result is None:
        return []
    if isinstance(result, dict):
        result = [result]
    found = []
    for item in result:
        uri = item.get("targetUri") or item.get("uri")
        selection = item.get("targetSelectionRange") or item.get("range") or item.get("targetRange")
        if uri and selection:
            start = selection["start"]
            found.append({"uri": uri, "line": start["line"], "character": start["character"]})
    return found


def timed(server, method, params, timeout):
    try:
        result, seconds = server.request(method, params, timeout=timeout)
        return {"result": result, "seconds": seconds}
    except RequestTimeout:
        return {"result": None, "seconds": timeout, "error": "timeout"}
    except LspError as error:
        return {"result": None, "seconds": None, "error": str(error)}


def start_of(range_):
    return [range_["start"]["line"], range_["start"]["character"]]


def span_of(range_):
    return [range_["start"]["line"], range_["start"]["character"],
            range_["end"]["line"], range_["end"]["character"]]


def text_of(contents):
    """Hover contents (MarkupContent | MarkedString | MarkedString[]) → text."""
    if contents is None:
        return ""
    if isinstance(contents, str):
        return contents
    if isinstance(contents, list):
        return "\n".join(text_of(part) for part in contents)
    return contents.get("value", "")


def workspace_edits(edit):
    """WorkspaceEdit → [(uri, TextEdit)], from `changes` or `documentChanges`."""
    found = []
    for uri, edits in ((edit or {}).get("changes") or {}).items():
        found.extend((uri, text_edit) for text_edit in edits)
    for change in (edit or {}).get("documentChanges") or []:
        if "textDocument" in change:
            found.extend((change["textDocument"]["uri"], text_edit) for text_edit in change.get("edits", []))
    return found


def offset_of(lines, position):
    line = min(position["line"], len(lines) - 1)
    return sum(len(text) + 1 for text in lines[:line]) + min(position["character"], len(lines[line]))


def apply_edits(text, edits):
    """`text` after a list of TextEdits (non-overlapping, any order)."""
    lines = text.split("\n")
    spans = sorted(((offset_of(lines, edit["range"]["start"]), offset_of(lines, edit["range"]["end"]),
                     edit["newText"]) for edit in edits), reverse=True)
    for start, end, new_text in spans:
        text = text[:start] + new_text + text[end:]
    return text


def without_whitespace(text):
    return re.sub(r"\s+", "", text)


def without_comments(text):
    """`text` minus its comments: a formatter may reflow a Javadoc (and add
    its `*` margins); it may not touch the code."""
    kept, last = [], 0
    for start, end, kind in literal_spans(text):
        if kind == "comment":
            kept.append(text[last:start])
            last = end
    kept.append(text[last:])
    return " ".join(kept)


def code_change(before, after):
    """None when only whitespace and comments differ, else the first
    difference of the two codes, whitespace removed (`…before… → …after…`)."""
    left, right = without_whitespace(without_comments(before)), without_whitespace(without_comments(after))
    if left == right:
        return None
    index = next((position for position, (one, other) in enumerate(zip(left, right)) if one != other),
                 min(len(left), len(right)))
    return f"…{left[max(0, index - 30):index + 30]}… → …{right[max(0, index - 30):index + 30]}…"


def indentation(line):
    return len(line) - len(line.lstrip())


def indented_share(lines):
    """Share of the non-blank code lines that start with whitespace (comment
    text is the formatter's to reflow, or not: Eclipse leaves the license
    header alone)."""
    code = blank_comments_and_strings("\n".join(lines), kotlin=False).split("\n")
    content = [original for original, blanked in zip(lines, code) if blanked.strip()]
    return sum(1 for line in content if indentation(line) > 0) / len(content) if content else 0.0


FORMATTING_OPTIONS = {"tabSize": 2, "insertSpaces": True}


class Context:
    """What a feature needs to ask a server: the open documents, the queries,
    the server's capabilities (semantic token legends live there)."""

    def __init__(self, server, documents, repo, query_set, timeout, capabilities):
        self.server, self.documents, self.repo = server, documents, repo
        self.query_set, self.timeout, self.capabilities = query_set, timeout, capabilities

    def queries(self, kind, categories=None):
        return [query for query in self.query_set.get(kind, [])
                if categories is None or query["category"] in categories]

    def text(self, relative):
        self.documents.open(relative)
        return self.documents.texts[relative]

    def position(self, query, line=None, character=None):
        self.documents.open(query["file"])
        return {"textDocument": {"uri": uri_of(self.repo, query["file"])},
                "position": {"line": query["line"] if line is None else line,
                             "character": query["character"] if character is None else character}}

    def document(self, relative):
        self.documents.open(relative)
        return {"textDocument": {"uri": uri_of(self.repo, relative)}}

    def ask(self, method, params):
        return timed(self.server, method, params, self.timeout)

    def edit(self, relative, text):
        """Replace the document's text; the diagnostics publication count
        before the change, to wait on with `diagnostics_after`."""
        uri = uri_of(self.repo, relative)
        before = self.server.diagnostics_version.get(uri, 0)
        self.documents.change(relative, text)
        return before

    def diagnostics_after(self, relative, before, timeout=20.0):
        """The diagnostics published for the document after `edit`, waiting
        up to `timeout` seconds for a new publication."""
        uri = uri_of(self.repo, relative)
        deadline = time.monotonic() + timeout
        while self.server.diagnostics_version.get(uri, 0) <= before and time.monotonic() < deadline:
            time.sleep(0.2)
        return self.server.diagnostics.get(uri, [])

    def restore(self, relative):
        """Put the document back to its text on disk."""
        with open(os.path.join(self.repo, relative), encoding="utf-8") as handle:
            original = handle.read()
        if self.documents.texts.get(relative) != original:
            self.documents.change(relative, original)
        self.documents.texts[relative] = original

    def semantic_legend(self):
        provider = self.capabilities.get("semanticTokensProvider")
        if not provider:
            for registration in self.server.registrations:
                if registration.get("method") == "textDocument/semanticTokens":
                    provider = registration.get("registerOptions")
        return (provider or {}).get("legend", {}).get("tokenTypes", [])


def answer(query, response, **fields):
    return dict({"id": query["id"], "seconds": response.get("seconds"), "error": response.get("error")}, **fields)


# ── grading helpers ───────────────────────────────────────────────────────

DECLARATION_KEYWORD = re.compile(r"\b(class|interface|enum|record|@interface)\b")


def snap_line(repo, relative, line, pattern):
    """The first line at or below `line` (within 8) matching `pattern`:
    servers point at a declaration's annotations, its modifiers or its name."""
    lines = repo.file_lines(relative)
    for candidate in range(line, min(line + 8, len(lines))):
        if pattern.search(lines[candidate]):
            return candidate
    return line


def target_of(repo, location, pattern):
    """(relative file, snapped line) for an in-repo location, ("external",
    class name) for a library or JDK one."""
    relative = repo.relative(location["uri"])
    if relative is None:
        return ("external", compiled_stub_class(location) or external_name(location["uri"]))
    return (relative, snap_line(repo, relative, location["line"], pattern))


def name_pattern(name):
    return re.compile(r"\b" + re.escape(name) + r"\b")


def same_target(mine, expected):
    """Same (file, line), or a compiled class against a line of its source."""
    if mine & expected:
        return True
    for one, other in ((mine, expected), (expected, mine)):
        compiled = {target[1] for target in one if target[0] == "external"}
        sources = {os.path.splitext(os.path.basename(target[0]))[0] for target in other if target[0] != "external"}
        if compiled & sources:
            return True
    return False


def place(target):
    where, what = target
    return f"<compiled {what}>" if where == "external" else f"{where}:{what + 1}"


def places(targets, limit=5):
    shown = [place(tuple(target)) for target in sorted(targets)[:limit]]
    if len(targets) > limit:
        shown.append(f"… {len(targets) - limit} more")
    return ", ".join(shown) if shown else "no answer"


def set_difference(expected, mine, render):
    detail = []
    if expected - mine:
        detail.append(f"missing  {render(expected - mine)}")
    if mine - expected:
        detail.append(f"extra    {render(mine - expected)}")
    return detail


def positions(items, limit=5):
    shown = [f"{line + 1}:{character + 1}" for line, character in sorted(items)[:limit]]
    if len(items) > limit:
        shown.append(f"… {len(items) - limit} more")
    return ", ".join(shown)


def case_of(query, **fields):
    """A case: where the query is (a file-level query sits on the file's
    top-level type) and what is expected there."""
    return dict({key: query[key] for key in ("id", "file", "line", "character", "name", "category")}, **fields)


# ── features ──────────────────────────────────────────────────────────────


class Feature:
    """One LSP feature. `name` keys the fixture's cases, the result's answers
    and the baselines."""

    name = ""
    method = ""
    reference = "jdtls"
    query_kind = "definition"

    def ask(self, context):
        raise NotImplementedError

    def expect(self, query, answer, repo):
        """The case for a query the reference answered, or None to leave it out."""
        raise NotImplementedError

    def source_cases(self, queries, repo):
        """Cases derived from the sources (reference = "source")."""
        raise NotImplementedError

    def grade(self, case, answer, repo):
        raise NotImplementedError

    def answer_id(self, case):
        """Which recorded answer grades the case."""
        return case.get("query", case["id"])


class LocationFeature(Feature):
    """declaration / typeDefinition: a location, graded like definition."""

    def ask(self, context):
        answers = []
        for query in context.queries("definition"):
            response = context.ask(self.method, context.position(query))
            answers.append(answer(query, response, locations=locations(response["result"])))
        return answers

    def pattern(self, case):
        return name_pattern(case["name"])

    def expect(self, query, answer_, repo):
        if not answer_["locations"]:
            return None
        targets = sorted({target_of(repo, location, self.pattern(query)) for location in answer_["locations"]})
        return case_of(query, expect=[list(target) for target in targets])

    def grade(self, case, answer_, repo):
        expected = {tuple(target) for target in case["expect"]}
        mine = {target_of(repo, location, self.pattern(case)) for location in answer_["locations"]}
        if mine and same_target(mine, expected):
            return None
        return places(expected), places(mine), []


class Declaration(LocationFeature):
    name, method = "declaration", "textDocument/declaration"


class TypeDefinition(LocationFeature):
    name, method = "typeDefinition", "textDocument/typeDefinition"

    def pattern(self, case):
        # The target is a type whose name the query does not carry.
        return DECLARATION_KEYWORD


class Hover(Feature):
    """Hover on an identifier usage must say what it is: its text names it."""

    name, method = "hover", "textDocument/hover"

    def ask(self, context):
        # Asked alongside definition in run.py, as it always was.
        return None

    def expect(self, query, answer_, repo):
        text = answer_.get("text")
        if text and name_pattern(query["name"]).search(text):
            return case_of(query, expect_mentions=query["name"])
        return None

    def grade(self, case, answer_, repo):
        text = answer_.get("text") or ""
        if name_pattern(case["expect_mentions"]).search(text):
            return None
        first_line = text.strip().split("\n")[0][:100] if text.strip() else "empty hover"
        return f"a hover mentioning `{case['expect_mentions']}`", first_line, []


class Implementation(Feature):
    """Implementations of a declared type or method: the in-repo set, exactly."""

    name, method, query_kind = "implementation", "textDocument/implementation", "references"

    def ask(self, context):
        answers = []
        for query in context.queries("references"):
            response = context.ask(self.method, context.position(query))
            answers.append(answer(query, response, locations=locations(response["result"])))
        return answers

    def targets(self, repo, query, answer_):
        pattern = name_pattern(query["name"]) if query["category"] == "method" else DECLARATION_KEYWORD
        found = {target_of(repo, location, pattern) for location in answer_["locations"]}
        return {target for target in found
                if target[0] != "external" and target != (query["file"], query["line"])}

    def expect(self, query, answer_, repo):
        targets = self.targets(repo, query, answer_)
        return case_of(query, expect=sorted(list(target) for target in targets)) if targets else None

    def grade(self, case, answer_, repo):
        expected = {tuple(target) for target in case["expect"]}
        mine = self.targets(repo, case, answer_)
        if mine == expected:
            return None
        return (f"{len(expected)} implementations", f"{len(mine)}, {len(mine & expected)} of them expected",
                set_difference(expected, mine, places))


class DocumentHighlight(Feature):
    """Occurrences of a declared name in its own file, exactly."""

    name, method, query_kind = "documentHighlight", "textDocument/documentHighlight", "references"

    def ask(self, context):
        answers = []
        for query in context.queries("references"):
            response = context.ask(self.method, context.position(query))
            ranges = sorted({tuple(start_of(item["range"])) for item in response["result"] or []})
            answers.append(answer(query, response, positions=[list(item) for item in ranges]))
        return answers

    def expect(self, query, answer_, repo):
        return case_of(query, expect=answer_["positions"]) if answer_["positions"] else None

    def grade(self, case, answer_, repo):
        expected = {tuple(item) for item in case["expect"]}
        mine = {tuple(item) for item in answer_["positions"]}
        if mine == expected:
            return None
        return (f"{len(expected)} highlights", f"{len(mine)}, {len(mine & expected)} of them expected",
                set_difference(expected, mine, positions))


SYMBOL_NAME = re.compile(r"[A-Za-z_$][\w$]*")


def symbol_name(raw):
    """`toJson` of jdtls's `toJson(Object)`; None for an anonymous class or
    initializer (`new TypeAdapter<T>() {...}`, `{...}`)."""
    match = SYMBOL_NAME.match(raw.strip())
    if not match or raw.strip().startswith("new "):
        return None
    return match.group(0)


SYMBOL_KIND_PACKAGE = 4


class DocumentSymbol(Feature):
    """Every named declaration of a file jdtls lists (type, field, method,
    constructor, enum constant), by name and line: none may be missing.
    jdtls's `package` symbol is left out — it names no declaration."""

    name, method, query_kind = "documentSymbol", "textDocument/documentSymbol", "files"

    def ask(self, context):
        answers = []
        for query in context.queries("files"):
            response = context.ask(self.method, context.document(query["file"]))
            lines = context.text(query["file"]).split("\n")
            symbols = []

            def visit(items):
                for item in items or []:
                    name = symbol_name(item.get("name", ""))
                    if "selectionRange" in item:
                        line = item["selectionRange"]["start"]["line"]
                    else:
                        line = item.get("location", {}).get("range", {}).get("start", {}).get("line", 0)
                        if name:
                            for candidate in range(line, min(line + 8, len(lines))):
                                if name_pattern(name).search(lines[candidate]):
                                    line = candidate
                                    break
                    if name:
                        symbols.append([name, line, item.get("kind")])
                    visit(item.get("children"))

            visit(response["result"])
            answers.append(answer(query, response, file=query["file"], count=len(response["result"] or []),
                                  symbols=sorted(symbols)))
        return answers

    def expect(self, query, answer_, repo):
        symbols = sorted({(name, line) for name, line, kind in answer_["symbols"] if kind != SYMBOL_KIND_PACKAGE})
        return case_of(query, expect=[list(symbol) for symbol in symbols]) if symbols else None

    def grade(self, case, answer_, repo):
        expected = {tuple(item) for item in case["expect"]}
        mine = {(name, line) for name, line, _ in answer_["symbols"]}
        missing = expected - mine
        if not missing:
            return None
        render = lambda items: ", ".join(f"{name}@{line + 1}" for name, line in sorted(items)[:6]) + (
            f", … {len(items) - 6} more" if len(items) > 6 else "")
        return (f"{len(expected)} symbols", f"{len(mine)} symbols, {len(expected & mine)} of the expected",
                [f"missing  {render(missing)}"])


class WorkspaceSymbol(Feature):
    """A search for a file's top-level type name finds its declaration."""

    name, method, query_kind = "workspaceSymbol", "workspace/symbol", "files"

    def ask(self, context):
        answers = []
        for query in context.queries("files"):
            response = context.ask(self.method, {"query": query["name"]})
            symbols = []
            for item in response["result"] or []:
                location = item.get("location") or {}
                relative = context_relative(context.repo, location.get("uri", ""))
                if relative is None or "range" not in location:
                    continue
                span = span_of(location["range"])
                symbols.append([item.get("name", ""), relative, span[0], span[2]])
            answers.append(answer(query, response, symbols=symbols, total=len(response["result"] or [])))
        return answers

    def found(self, case, answer_):
        for name, relative, first, last in answer_["symbols"]:
            if symbol_name(name) == case["name"] and relative == case["file"] and (
                    first <= case["line"] <= max(last, first) or abs(first - case["line"]) <= 8):
                return True
        return False

    def expect(self, query, answer_, repo):
        return case_of(query, expect=[query["file"], query["line"]]) if self.found(query, answer_) else None

    def grade(self, case, answer_, repo):
        if self.found(case, answer_):
            return None
        names = sorted({name for name, *_ in answer_["symbols"]})[:5]
        return (f"{case['name']} at {case['file']}:{case['line'] + 1}",
                f"{answer_.get('total', 0)} symbols" + (f": {', '.join(names)}" if names else ""), [])


def context_relative(repo, uri):
    """In-repo relative path of a file URI (run time: no Repo object yet)."""
    if not uri.startswith("file:"):
        return None
    import urllib.parse
    path = os.path.realpath(urllib.parse.unquote(urllib.parse.urlparse(uri).path))
    root = os.path.realpath(repo)
    return os.path.relpath(path, root) if path.startswith(root + os.sep) else None


def call_paren(line_text, query):
    """Column just inside the `(` that follows a call's name, or None."""
    after = query["character"] + len(query["name"])
    return after + 1 if line_text[after:after + 1] == "(" else None


class SignatureHelp(Feature):
    """Inside a call's parentheses: the active signature is that method's,
    with the parameter count of the overload the arguments select."""

    name, method = "signatureHelp", "textDocument/signatureHelp"

    def ask(self, context):
        answers = []
        for query in context.queries("definition", {"call", "member_call"}):
            line_text = context.text(query["file"]).split("\n")[query["line"]]
            column = call_paren(line_text, query)
            if column is None:
                continue
            params = dict(context.position(query, character=column),
                          context={"triggerKind": 2, "triggerCharacter": "(", "isRetrigger": False})
            response = context.ask(self.method, params)
            result = response["result"] or {}
            signatures = [{"label": item.get("label", ""), "parameters": len(item.get("parameters") or [])}
                          for item in result.get("signatures") or []]
            answers.append(answer(query, response, signatures=signatures,
                                  active=result.get("activeSignature") or 0))
        return answers

    def active(self, answer_):
        signatures = answer_["signatures"]
        if not signatures:
            return None
        return signatures[answer_["active"]] if answer_["active"] < len(signatures) else signatures[0]

    def expect(self, query, answer_, repo):
        active = self.active(answer_)
        if active and name_pattern(query["name"]).search(active["label"]):
            return case_of(query, expect_parameters=active["parameters"], expect_label=active["label"])
        return None

    def grade(self, case, answer_, repo):
        active = self.active(answer_)
        if active and name_pattern(case["name"]).search(active["label"]) \
                and active["parameters"] == case["expect_parameters"]:
            return None
        actual = f"{active['label']} ({active['parameters']} parameters)" if active else "no signature"
        return f"{case['expect_label']} ({case['expect_parameters']} parameters)", actual, []


class PrepareRename(Feature):
    """prepareRename on a declaration's name answers that name's range."""

    name, method, query_kind = "prepareRename", "textDocument/prepareRename", "references"

    def ask(self, context):
        answers = []
        for query in context.queries("references"):
            response = context.ask(self.method, context.position(query))
            result = response["result"]
            range_ = (result.get("range") if isinstance(result, dict) and "range" in result else
                      result if isinstance(result, dict) and "start" in result else None)
            answers.append(answer(query, response, range=span_of(range_) if range_ else None,
                                  default=bool(isinstance(result, dict) and result.get("defaultBehavior"))))
        return answers

    def expect(self, query, answer_, repo):
        expected = [query["line"], query["character"], query["line"], query["character"] + len(query["name"])]
        return case_of(query, expect=expected) if answer_["range"] == expected else None

    def grade(self, case, answer_, repo):
        if answer_["range"] == case["expect"] or answer_.get("default"):
            return None
        render = lambda span: f"{span[0] + 1}:{span[1] + 1}-{span[2] + 1}:{span[3] + 1}" if span else "no range"
        return render(case["expect"]), render(answer_["range"]), []


class Rename(Feature):
    """Renaming a declaration edits exactly the places jdtls edits."""

    name, method, query_kind = "rename", "textDocument/rename", "references"

    def ask(self, context):
        answers = []
        for query in context.queries("references"):
            params = dict(context.position(query), newName=query["name"] + "Renamed")
            response = context.ask(self.method, params)
            edits, outside = set(), 0
            for uri, text_edit in workspace_edits(response["result"]):
                relative = context_relative(context.repo, uri)
                if relative is None:
                    outside += 1
                    continue
                edits.add((relative, *start_of(text_edit["range"])))
            answers.append(answer(query, response, edits=sorted(list(edit) for edit in edits), outside=outside))
        return answers

    def expect(self, query, answer_, repo):
        return case_of(query, expect=answer_["edits"]) if answer_["edits"] else None

    def grade(self, case, answer_, repo):
        expected = {tuple(edit) for edit in case["expect"]}
        mine = {tuple(edit) for edit in answer_["edits"]}
        if mine == expected:
            return None
        render = lambda items: ", ".join(f"{file}:{line + 1}:{character + 1}" for file, line, character
                                         in sorted(items)[:5]) + (f", … {len(items) - 5} more" if len(items) > 5 else "")
        return (f"{len(expected)} edits", f"{len(mine)} edits, {len(mine & expected)} of them expected",
                set_difference(expected, mine, render))


class FoldingRange(Feature):
    """Every code region jdtls folds (comments, imports and one-line
    declarations aside — nothing to fold there) starts a folding range:
    matched by start line."""

    name, method, query_kind = "foldingRange", "textDocument/foldingRange", "files"

    def ask(self, context):
        answers = []
        for query in context.queries("files"):
            response = context.ask(self.method, context.document(query["file"]))
            ranges = [[item["startLine"], item["endLine"], item.get("kind") or "region"]
                      for item in response["result"] or []]
            answers.append(answer(query, response, ranges=ranges))
        return answers

    def expect(self, query, answer_, repo):
        starts = sorted({first for first, last, kind in answer_["ranges"]
                         if kind not in {"comment", "imports"} and last > first})
        return case_of(query, expect=starts) if starts else None

    def grade(self, case, answer_, repo):
        expected = set(case["expect"])
        mine = {first for first, _, _ in answer_["ranges"]}
        missing = expected - mine
        if not missing:
            return None
        return (f"{len(expected)} code ranges", f"{len(mine)} ranges, {len(expected & mine)} of the expected",
                ["missing  starting at line " + ", ".join(str(line + 1) for line in sorted(missing)[:8])])


class SelectionRange(Feature):
    """Expand selection from an identifier: the innermost range is the
    identifier jdtls selects, and every step contains the one before."""

    name, method = "selectionRange", "textDocument/selectionRange"

    def ask(self, context):
        answers = []
        for query in context.queries("definition"):
            params = dict(context.document(query["file"]),
                          positions=[{"line": query["line"], "character": query["character"]}])
            response = context.ask(self.method, params)
            chain = []
            node = (response["result"] or [None])[0]
            while node and len(chain) < 64:
                chain.append(span_of(node["range"]))
                node = node.get("parent")
            answers.append(answer(query, response, chain=chain))
        return answers

    def expect(self, query, answer_, repo):
        return case_of(query, expect_innermost=answer_["chain"][0], expect_levels=len(answer_["chain"])) \
            if answer_["chain"] else None

    def grade(self, case, answer_, repo):
        chain = answer_["chain"]
        nested = all(tuple(outer[:2]) <= tuple(inner[:2]) and tuple(outer[2:]) >= tuple(inner[2:])
                     for inner, outer in zip(chain, chain[1:]))
        render = lambda span: f"{span[0] + 1}:{span[1] + 1}-{span[2] + 1}:{span[3] + 1}"
        if chain and chain[0] == case["expect_innermost"] and nested and len(chain) >= 3:
            return None
        actual = (f"innermost {render(chain[0])}, {len(chain)} levels" + ("" if nested else ", not nested")
                  if chain else "no selection range")
        return f"innermost {render(case['expect_innermost'])}, {case['expect_levels']} levels", actual, []


# Token types that name the same thing in different legends.
TOKEN_SYNONYMS = {"record": "class", "annotationMember": "method", "decorator": "annotation",
                  "annotation": "annotation", "struct": "class", "function": "method",
                  "enumConstant": "enumMember", "field": "property"}


def token_kind(token_type):
    return TOKEN_SYNONYMS.get(token_type, token_type)


class SemanticTokens(Feature):
    """The semantic token on an identifier usage has jdtls's type (class,
    method, property, parameter, …); legends are mapped by name."""

    name, method = "semanticTokens", "textDocument/semanticTokens/full"

    def ask(self, context):
        legend = context.semantic_legend()
        by_file, answers = {}, []
        for query in context.queries("definition"):
            if query["file"] not in by_file:
                by_file[query["file"]] = context.ask(self.method, context.document(query["file"]))
            response = by_file[query["file"]]
            token = None
            data = (response["result"] or {}).get("data") or []
            line = character = 0
            for index in range(0, len(data) - 4, 5):
                delta_line, delta_start, length, type_index = data[index:index + 4]
                line, character = line + delta_line, (character + delta_start if delta_line == 0 else delta_start)
                if line == query["line"] and character <= query["character"] < character + length:
                    token = legend[type_index] if type_index < len(legend) else f"#{type_index}"
                    break
            answers.append(answer(query, response, token=token))
        return answers

    def expect(self, query, answer_, repo):
        return case_of(query, expect=answer_["token"]) if answer_["token"] else None

    def grade(self, case, answer_, repo):
        mine, expected = answer_["token"], case["expect"]
        if mine and (token_kind(mine) == token_kind(expected)
                     or (mine == "type" and expected in {"class", "interface", "enum", "record"})):
            return None
        return expected, mine or "no token", []


class InlayHint(Feature):
    """Each parameter-name hint jdtls shows (its default: literal arguments)
    appears at the same position with the same name. Extra hints are fine."""

    name, method, query_kind = "inlayHint", "textDocument/inlayHint", "files"
    limit = 80

    def ask(self, context):
        answers = []
        for query in context.queries("files"):
            lines = context.text(query["file"]).split("\n")
            params = dict(context.document(query["file"]),
                          range={"start": {"line": 0, "character": 0},
                                 "end": {"line": len(lines) - 1, "character": len(lines[-1])}})
            response = context.ask(self.method, params)
            hints = []
            for item in response["result"] or []:
                label = item.get("label")
                text = label if isinstance(label, str) else "".join(part.get("value", "") for part in label or [])
                hints.append([item["position"]["line"], item["position"]["character"], hint_name(text)])
            answers.append(answer(query, response, hints=hints))
        return answers

    def cases_from(self, query, answer_):
        return [dict(case_of(query, id=f"{query['id']}-{line}-{character}", line=line,
                               character=character, name=name, category="parameter_hint"),
                     query=query["id"])
                for line, character, name in answer_["hints"] if name]

    def grade(self, case, answer_, repo):
        if [case["line"], case["character"], case["name"]] in answer_["hints"]:
            return None
        nearby = [name for line, _, name in answer_["hints"] if line == case["line"]]
        return f"`{case['name']}:` hint here", (f"hints on the line: {', '.join(nearby)}" if nearby
                                                 else f"{len(answer_['hints'])} hints in the file, none on the line"), []


def hint_name(text):
    return text.strip().rstrip(":").strip()


class CodeActionImport(Feature):
    """With one import deleted, a quick fix at the now-unresolved usage adds
    `import <fqn>;` back."""

    name, method, query_kind = "codeAction", "textDocument/codeAction", "imports"

    def ask(self, context):
        answers = []
        for query in context.queries("imports"):
            original = context.text(query["file"])
            lines = original.split("\n")
            lines[query["import_line"]] = ""  # line numbers stay put
            before = context.edit(query["file"], "\n".join(lines))
            diagnostics = [item for item in context.diagnostics_after(query["file"], before)
                           if item["range"]["start"]["line"] <= query["line"] <= item["range"]["end"]["line"]]
            usage = {"start": {"line": query["line"], "character": query["character"]},
                     "end": {"line": query["line"], "character": query["character"] + len(query["name"])}}
            response = context.ask(self.method, dict(context.document(query["file"]), range=usage,
                                                     context={"diagnostics": diagnostics}))
            actions = [{"title": item.get("title", ""), "body": repr(item)} for item in response["result"] or []]
            context.restore(query["file"])
            answers.append(answer(query, response, actions=actions, diagnostics=len(diagnostics)))
        return answers

    def fixes(self, case, answer_):
        package = case["fqn"].rsplit(".", 1)[0]
        inserts = f"import {case['fqn']};"
        return [action["title"] for action in answer_["actions"]
                if inserts in action["body"] or (case["name"] in action["title"] and package in action["title"]
                                                 and "mport" in action["title"])]

    def expect(self, query, answer_, repo):
        return case_of(query, fqn=query["fqn"]) if self.fixes(query, answer_) else None

    def grade(self, case, answer_, repo):
        if self.fixes(case, answer_):
            return None
        titles = [action["title"] for action in answer_["actions"]][:5]
        return (f"a quick fix adding `import {case['fqn']};`",
                f"{len(answer_['actions'])} actions" + (f": {'; '.join(titles)}" if titles else ""),
                [f"diagnostics at the usage: {answer_.get('diagnostics', 0)}"])


class CallHierarchy(Feature):
    """prepareCallHierarchy on a method, then its incoming calls: the
    in-repo call sites, exactly."""

    name, method, query_kind = "callHierarchy", "callHierarchy/incomingCalls", "references"

    def ask(self, context):
        answers = []
        for query in context.queries("references", {"method"}):
            prepared = context.ask("textDocument/prepareCallHierarchy", context.position(query))
            items = prepared["result"] or []
            if prepared.get("error") or not items:
                answers.append(answer(query, prepared, calls=[]))
                continue
            response = context.ask(self.method, {"item": items[0]})
            calls = set()
            for call in response["result"] or []:
                relative = context_relative(context.repo, call["from"]["uri"])
                for range_ in call.get("fromRanges") or []:
                    if relative:
                        calls.add((relative, range_["start"]["line"]))
            answers.append(answer(query, response, calls=sorted(list(call) for call in calls)))
        return answers

    def expect(self, query, answer_, repo):
        return case_of(query, expect=answer_["calls"]) if answer_["calls"] else None

    def grade(self, case, answer_, repo, ungraded=frozenset()):
        """`ungraded`: call sites the reference server is known to miss (the
        fixture's `reference_misses` for the same query) — neither required
        nor counted as extra."""
        expected = {tuple(call) for call in case["expect"]} - ungraded
        mine = {tuple(call) for call in answer_["calls"]} - ungraded
        if mine == expected:
            return None
        return (f"{len(expected)} call sites", f"{len(mine)}, {len(mine & expected)} of them expected",
                set_difference(expected, mine, places))


class TypeHierarchy(Feature):
    """prepareTypeHierarchy on a file's top-level type: its supertypes and
    subtypes, by name, exactly."""

    name, method, query_kind = "typeHierarchy", "typeHierarchy/supertypes", "files"

    def ask(self, context):
        answers = []
        for query in context.queries("files"):
            prepared = context.ask("textDocument/prepareTypeHierarchy", context.position(query))
            items = prepared["result"] or []
            if prepared.get("error") or not items:
                answers.append(answer(query, prepared, supertypes=None, subtypes=None))
                continue
            names = {}
            for direction in ("supertypes", "subtypes"):
                response = context.ask(f"typeHierarchy/{direction}", {"item": items[0]})
                names[direction] = sorted({symbol_name(item["name"]) or item["name"]
                                           for item in response["result"] or []})
            answers.append(answer(query, prepared, **names))
        return answers

    def expect(self, query, answer_, repo):
        if not answer_["supertypes"] and not answer_["subtypes"]:
            return None
        return case_of(query, supertypes=answer_["supertypes"], subtypes=answer_["subtypes"])

    def grade(self, case, answer_, repo):
        if answer_["supertypes"] is None:
            return "a type hierarchy item", "none", []
        detail = []
        for direction in ("supertypes", "subtypes"):
            expected, mine = set(case[direction] or []), set(answer_[direction] or [])
            if expected != mine:
                detail.extend(f"{direction}: {line}" for line in set_difference(
                    expected, mine, lambda items: ", ".join(sorted(items)[:6])))
        if not detail:
            return None
        return "supertypes and subtypes as jdtls", "different", detail


class Diagnostics(Feature):
    """Published diagnostics: a file jdtls finds no error in (the target
    compiles) has no error from the server either."""

    name, method, query_kind = "diagnostics", "textDocument/publishDiagnostics", "files"

    def ask(self, context):
        for query in context.queries("files"):
            context.documents.open(query["file"])
        time.sleep(5)  # let the last publications arrive
        answers = []
        for query in context.queries("files"):
            published = context.server.diagnostics.get(uri_of(context.repo, query["file"]))
            errors = [f"{item['range']['start']['line'] + 1}: {item.get('message', '')[:80]}"
                      for item in published or [] if item.get("severity", 1) == 1]
            answers.append({"id": query["id"], "seconds": None, "error": None,
                            "published": published is not None, "errors": errors})
        return answers

    def expect(self, query, answer_, repo):
        return case_of(query, expect_errors=0) if not answer_["errors"] else None

    def grade(self, case, answer_, repo):
        if not answer_["errors"]:
            return None
        return "no error", f"{len(answer_['errors'])} errors", answer_["errors"][:5]


# ── formatting: graded against properties of the source, not a style ─────


def misindent(lines, first, last):
    """`lines` with the indentation of lines first..last removed."""
    return lines[:first] + [line.lstrip() for line in lines[first:last + 1]] + lines[last + 1:]


def method_block(lines):
    """(first, last) of the first run of 8 indented code lines (no comment
    lines) past the middle of the file: inside a method body."""
    start = len(lines) // 2
    run = []
    for number in range(start, len(lines)):
        stripped = lines[number].strip()
        is_code = (stripped and indentation(lines[number]) >= 4
                   and not stripped.startswith(("*", "/", "@")))
        run = run + [number] if is_code else []
        if len(run) == 8:
            return run[0], run[-1]
    return None


class Formatting(Feature):
    """Whole-document formatting of a file with its indentation stripped:
    the code (everything but whitespace) is unchanged and the indentation is
    back (at least 80% of the share of indented lines the file had)."""

    name, method, query_kind, reference = "formatting", "textDocument/formatting", "files", "source"
    files = 10

    def ask(self, context):
        answers = []
        for query in context.queries("files")[:self.files]:
            original = context.text(query["file"])
            lines = original.split("\n")
            broken = "\n".join(misindent(lines, 0, len(lines) - 1))
            context.edit(query["file"], broken)
            response = context.ask(self.method, dict(context.document(query["file"]), options=FORMATTING_OPTIONS))
            formatted = apply_edits(broken, response["result"] or [])
            context.restore(query["file"])
            change = code_change(original, formatted)
            answers.append(answer(query, response, edits=len(response["result"] or []),
                                  code_kept=change is None, change=change,
                                  indented=indented_share(formatted.split("\n")),
                                  indented_before=indented_share(lines)))
        return answers

    def source_cases(self, queries, repo):
        return [case_of(query, expect="code kept, indentation restored")
                for query in queries.get("files", [])[:self.files]]

    def grade(self, case, answer_, repo):
        restored = answer_["indented"] >= 0.8 * answer_["indented_before"]
        if answer_["edits"] and answer_["code_kept"] and restored:
            return None
        return (case["expect"], f"{answer_['edits']} edits, code {'kept' if answer_['code_kept'] else 'CHANGED'}, "
                f"{answer_['indented']:.0%} of lines indented (file had {answer_['indented_before']:.0%})",
                [f"change   {answer_['change']}"] if answer_.get("change") else [])


class RangeFormatting(Feature):
    """Range formatting of 8 method-body lines with their indentation
    stripped: they are indented again, the code is unchanged, and nothing
    outside the range moves."""

    name, method, query_kind, reference = "rangeFormatting", "textDocument/rangeFormatting", "files", "source"

    def ask(self, context):
        answers = []
        for query in context.queries("files"):
            lines = context.text(query["file"]).split("\n")
            block = method_block(lines)
            if block is None:
                continue
            first, last = block
            broken_lines = misindent(lines, first, last)
            broken = "\n".join(broken_lines)
            context.edit(query["file"], broken)
            range_ = {"start": {"line": first, "character": 0},
                      "end": {"line": last, "character": len(broken_lines[last])}}
            response = context.ask(self.method, dict(context.document(query["file"]), range=range_,
                                                     options=FORMATTING_OPTIONS))
            formatted = apply_edits(broken, response["result"] or []).split("\n")
            context.restore(query["file"])
            tail = len(lines) - last - 1
            answers.append(answer(query, response, first=first, last=last,
                                  edits=len(response["result"] or []),
                                  code_kept=code_change("\n".join(lines), "\n".join(formatted)) is None,
                                  outside_kept=formatted[:first] == lines[:first]
                                  and formatted[len(formatted) - tail:] == lines[len(lines) - tail:],
                                  indented=indented_share(formatted[first:len(formatted) - tail])))
        return answers

    def source_cases(self, queries, repo):
        cases = []
        for query in queries.get("files", []):
            block = method_block(repo.file_lines(query["file"]))
            if block:
                cases.append(case_of(query, line=block[0], character=0,
                                       expect=f"lines {block[0] + 1}-{block[1] + 1} indented again, code and the rest kept"))
        return cases

    def grade(self, case, answer_, repo):
        if answer_["edits"] and answer_["code_kept"] and answer_["outside_kept"] and answer_["indented"] == 1.0:
            return None
        return (case["expect"],
                f"{answer_['edits']} edits, code {'kept' if answer_['code_kept'] else 'CHANGED'}, "
                f"outside {'kept' if answer_['outside_kept'] else 'CHANGED'}, {answer_['indented']:.0%} indented", [])


def statement_line(lines):
    """A line ending a statement (`…;`) in a method body, followed by another
    statement at the same indentation, past the middle of the file."""
    for number in range(len(lines) // 2, len(lines) - 1):
        line, following = lines[number], lines[number + 1]
        if (line.rstrip().endswith(";") and indentation(line) >= 4 and following.strip()
                and indentation(following) == indentation(line)
                and not line.strip().startswith(("*", "/", "return", "throw", "break", "continue"))):
            return number
    return None


class OnTypeFormatting(Feature):
    """Typing Enter after a statement in a method body: the new line is
    indented like the statement (the next statement's indentation), and the
    code is unchanged."""

    name, method, query_kind, reference = "onTypeFormatting", "textDocument/onTypeFormatting", "files", "source"

    def ask(self, context):
        answers = []
        for query in context.queries("files"):
            lines = context.text(query["file"]).split("\n")
            number = statement_line(lines)
            if number is None:
                continue
            typed = lines[:number + 1] + [""] + lines[number + 1:]
            context.edit(query["file"], "\n".join(typed))
            response = context.ask(self.method, dict(context.document(query["file"]), ch="\n",
                                                     position={"line": number + 1, "character": 0},
                                                     options=FORMATTING_OPTIONS))
            formatted = apply_edits("\n".join(typed), response["result"] or []).split("\n")
            context.restore(query["file"])
            new_line = formatted[number + 1] if number + 1 < len(formatted) else ""
            answers.append(answer(query, response, statement=number, edits=len(response["result"] or []),
                                  code_kept=code_change("\n".join(lines), "\n".join(formatted)) is None,
                                  indent=len(new_line) - len(new_line.lstrip(" \t"))))
        return answers

    def source_cases(self, queries, repo):
        cases = []
        for query in queries.get("files", []):
            lines = repo.file_lines(query["file"])
            number = statement_line(lines)
            if number is not None:
                cases.append(case_of(query, line=number, character=len(lines[number]),
                                       expect_indent=indentation(lines[number])))
        return cases

    def grade(self, case, answer_, repo):
        if answer_["code_kept"] and answer_["indent"] == case["expect_indent"]:
            return None
        return (f"new line indented {case['expect_indent']}",
                f"indented {answer_['indent']} ({answer_['edits']} edits"
                + ("" if answer_["code_kept"] else ", code CHANGED") + ")", [])


# ── standard LSP features jdtls does not implement: graded from source ────


class DocumentLink(Feature):
    """Every http(s) URL in a comment is a document link starting at the
    URL (its target, when given eagerly, is that URL)."""

    name, method, query_kind, reference = "documentLink", "textDocument/documentLink", "links", "source"

    def ask(self, context):
        answers = []
        for query in context.queries("links"):
            response = context.ask(self.method, context.document(query["file"]))
            links = [start_of(item["range"]) + [item.get("target")] for item in response["result"] or []]
            answers.append(answer(query, response, links=links))
        return answers

    def source_cases(self, queries, repo):
        return [case_of(query, expect=query["urls"]) for query in queries.get("links", [])]

    def grade(self, case, answer_, repo):
        found = {(line, character): target for line, character, target in answer_["links"]}
        missing = [f"{line + 1}:{character + 1} {url}" for line, character, url in case["expect"]
                   if (line, character) not in found or (found[(line, character)] or url).rstrip("/") != url.rstrip("/")]
        if not missing:
            return None
        return f"{len(case['expect'])} links", f"{len(answer_['links'])} links", [f"missing  {item}" for item in missing[:5]]


class PullDiagnostics(Feature):
    """`textDocument/diagnostic` (LSP 3.17 pull model) on a file of a target
    that compiles: a full report with no error."""

    name, method, query_kind, reference = "pullDiagnostics", "textDocument/diagnostic", "files", "source"

    def ask(self, context):
        answers = []
        for query in context.queries("files"):
            response = context.ask(self.method, context.document(query["file"]))
            report = response["result"] or {}
            errors = [f"{item['range']['start']['line'] + 1}: {item.get('message', '')[:80]}"
                      for item in report.get("items") or [] if item.get("severity", 1) == 1]
            answers.append(answer(query, response, kind=report.get("kind"), errors=errors))
        return answers

    def source_cases(self, queries, repo):
        return [case_of(query, expect="a full report without errors") for query in queries.get("files", [])]

    def grade(self, case, answer_, repo):
        if answer_["kind"] in {"full", "unchanged"} and not answer_["errors"]:
            return None
        return case["expect"], (f"{len(answer_['errors'])} errors" if answer_["kind"] else "no report"), answer_["errors"][:5]


class WorkspaceDiagnostics(Feature):
    """`workspace/diagnostic` (pull, whole workspace): a report, and no
    error in any in-repo file — the target compiles."""

    name, method, query_kind, reference = "workspaceDiagnostics", "workspace/diagnostic", "files", "source"

    def ask(self, context):
        response = context.ask(self.method, {"previousResultIds": []})
        report = response["result"] or {}
        errors = []
        for item in report.get("items") or []:
            relative = context_relative(context.repo, item.get("uri", ""))
            errors.extend(f"{relative}:{diagnostic['range']['start']['line'] + 1}: {diagnostic.get('message', '')[:80]}"
                          for diagnostic in item.get("items") or [] if relative and diagnostic.get("severity", 1) == 1)
        return [{"id": "workspace", "seconds": response.get("seconds"), "error": response.get("error"),
                 "reported": response["result"] is not None, "files": len(report.get("items") or []),
                 "errors": errors}]

    def source_cases(self, queries, repo):
        first = (queries.get("files") or [{}])[0]
        return [dict(case_of(first, expect="a report without errors"), id="workspace", name="workspace",
                     category="workspace")] if first else []

    def grade(self, case, answer_, repo):
        if answer_["reported"] and not answer_["errors"]:
            return None
        return case["expect"], (f"{len(answer_['errors'])} errors in {answer_['files']} files"
                                if answer_["reported"] else "no report"), answer_["errors"][:5]


class Moniker(Feature):
    """`textDocument/moniker` on a declaration: a moniker whose identifier
    names it fully qualified — the package, the top-level type, the name."""

    name, method, query_kind, reference = "moniker", "textDocument/moniker", "references", "source"

    def ask(self, context):
        answers = []
        for query in context.queries("references"):
            response = context.ask(self.method, context.position(query))
            monikers = [{"scheme": item.get("scheme"), "identifier": item.get("identifier", ""),
                         "kind": item.get("kind")} for item in response["result"] or []]
            answers.append(answer(query, response, monikers=monikers))
        return answers

    def source_cases(self, queries, repo):
        cases = []
        for query in queries.get("references", []):
            lines = repo.file_lines(query["file"])
            package = next((match.group(1) for match in (re.match(r"\s*package\s+([\w.]+)", line) for line in lines)
                            if match), None)
            if package:
                owner = f"{package}.{os.path.splitext(os.path.basename(query['file']))[0]}"
                cases.append(case_of(query, expect_owner=owner))
        return cases

    def grade(self, case, answer_, repo):
        for moniker in answer_["monikers"]:
            identifier = moniker["identifier"]
            if case["expect_owner"] in identifier.replace("/", ".").replace("#", ".") \
                    and name_pattern(case["name"]).search(identifier):
                return None
        shown = ", ".join(moniker["identifier"] for moniker in answer_["monikers"][:3]) or "no moniker"
        return f"an identifier with {case['expect_owner']} and {case['name']}", shown, []


# Order matters only for the run: formatting and code actions edit
# documents (and put them back), so they go last.
FEATURES = [
    Hover(), Declaration(), TypeDefinition(), Implementation(), DocumentHighlight(), DocumentSymbol(),
    WorkspaceSymbol(), SignatureHelp(), PrepareRename(), Rename(), FoldingRange(), SelectionRange(),
    SemanticTokens(), InlayHint(), CallHierarchy(), TypeHierarchy(), Diagnostics(),
    DocumentLink(), PullDiagnostics(), WorkspaceDiagnostics(), Moniker(),
    CodeActionImport(), Formatting(), RangeFormatting(), OnTypeFormatting(),
]
BY_NAME = {feature.name: feature for feature in FEATURES}


def reference_cases(feature, query, answer_, repo):
    """The cases the reference server's answer to `query` yields (inlay hints
    make one case per hint; everything else one or none)."""
    if isinstance(feature, InlayHint):
        return feature.cases_from(query, answer_)
    case = feature.expect(query, answer_, repo)
    return [case] if case else []
