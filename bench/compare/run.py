#!/usr/bin/env python3
"""Run one server over a query set and record every answer and its latency.

Usage:
  run.py --server kmp-lsp --repo <repo> --target targets/moshi.json \
         --queries queries.json --state <dir> --out results/kmp-lsp-cold.json \
         [--kmp-lsp PATH] [--jdtls DIR] [--kotlin-lsp DIR] [--gradle-home DIR] \
         [--no-dependency-sync] [--ready-timeout 1800]

Phases, each timed from process start:
  initialize   — `initialize` answered
  ready        — every readiness probe (a known cross-file definition in a
                 language the server handles) returns the expected location
  settled      — after ready, the last `$/progress` activity once nothing
                 has been in progress for --settle-quiet seconds
Then each query is asked once, in a fixed order, and timed.
"""

import argparse
import json
import os
import pathlib
import sys
import time

from features import BY_NAME, FEATURES, Context, locations, text_of, timed, uri_of
from lsp import LanguageServer
from servers import SERVERS


def language_id(path):
    return "kotlin" if path.endswith(".kt") else "java"


def completion_labels(result, limit=300):
    items = result.get("items", []) if isinstance(result, dict) else (result or [])
    labels = []
    for item in items[:limit]:
        labels.append({"label": item.get("label", ""), "filterText": item.get("filterText"),
                       "insertText": item.get("insertText"), "sortText": item.get("sortText"),
                       "kind": item.get("kind")})
    return labels, len(items)


class Documents:
    """Open files once and keep them open; track versions for edits."""

    def __init__(self, server, repo):
        self.server, self.repo = server, repo
        self.versions, self.texts = {}, {}

    def open(self, relative):
        if relative in self.versions:
            return
        with open(os.path.join(self.repo, relative), encoding="utf-8") as handle:
            text = handle.read()
        self.texts[relative], self.versions[relative] = text, 1
        self.server.notify("textDocument/didOpen", {"textDocument": {
            "uri": uri_of(self.repo, relative), "languageId": language_id(relative),
            "version": 1, "text": text}})

    def change(self, relative, text):
        self.versions[relative] += 1
        self.server.notify("textDocument/didChange", {
            "textDocument": {"uri": uri_of(self.repo, relative), "version": self.versions[relative]},
            "contentChanges": [{"text": text}]})


def wait_until_ready(server, documents, repo, probes, timeout):
    """Seconds since start when every probe answers correctly, or None."""
    for probe in probes:
        documents.open(probe["file"])
    deadline = time.monotonic() + timeout
    pending = list(probes)
    while pending and time.monotonic() < deadline and not server.closed.is_set():
        still = []
        for probe in pending:
            line_text = documents.texts[probe["file"]].split("\n")[probe["line"] - 1]
            character = line_text.index(probe["name"])
            answer = timed(server, "textDocument/definition", {
                "textDocument": {"uri": uri_of(repo, probe["file"])},
                "position": {"line": probe["line"] - 1, "character": character}}, timeout=30)
            expected_uri = uri_of(repo, probe["expect_file"])
            hit = any(location["uri"] == expected_uri and abs(location["line"] - (probe["expect_line"] - 1)) <= 3
                      for location in locations(answer["result"]))
            if not hit:
                still.append(probe)
        pending = still
        if pending:
            time.sleep(1.0)
    return None if pending else time.monotonic() - server.started


def wait_until_settled(server, quiet_seconds, timeout):
    """Seconds since start of the last progress activity once nothing has
    been in progress for `quiet_seconds`; None if it never got quiet."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline and not server.closed.is_set():
        last_event = server.progress_events[-1][0] if server.progress_events else 0.0
        now = time.monotonic() - server.started
        if not server.active_progress and now - last_event >= quiet_seconds:
            return max(last_event, 0.0)
        time.sleep(0.5)
    return None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--server", required=True, choices=sorted(SERVERS))
    parser.add_argument("--repo", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--queries", required=True)
    parser.add_argument("--state", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--kmp-lsp", default="kmp-lsp")
    parser.add_argument("--jdtls")
    parser.add_argument("--kotlin-lsp")
    parser.add_argument("--java", default="java")
    parser.add_argument("--gradle-home")
    parser.add_argument("--no-dependency-sync", action="store_true")
    parser.add_argument("--ready-timeout", type=float, default=1800)
    parser.add_argument("--request-timeout", type=float, default=60)
    parser.add_argument("--symbols-files", type=int, default=30)
    parser.add_argument("--settle-quiet", type=float, default=5,
                        help="seconds without $/progress activity that count as settled")
    parser.add_argument("--settle-timeout", type=float, default=900)
    parser.add_argument("--features", default="base",
                        help="base (definition, hover, references, completion, documentSymbol), "
                             "all (also every feature in features.py), or a comma-separated list of those")
    arguments = parser.parse_args()
    extra = extra_features(arguments.features)

    repo = os.path.abspath(arguments.repo)
    target = json.load(open(arguments.target))
    query_set = json.load(open(arguments.queries))["queries"]
    os.makedirs(arguments.state, exist_ok=True)
    cold = not os.listdir(arguments.state)
    paths = {"kmp_lsp": arguments.kmp_lsp, "jdtls": arguments.jdtls,
             "kotlin_lsp": arguments.kotlin_lsp, "java": arguments.java}
    config = SERVERS[arguments.server](paths, os.path.abspath(arguments.state), arguments.gradle_home,
                                        {"dependency_sync": not arguments.no_dependency_sync})
    languages = set(config["languages"])

    stderr_path = arguments.out + ".stderr.log"
    os.makedirs(os.path.dirname(os.path.abspath(arguments.out)), exist_ok=True)
    server = LanguageServer(config["command"], repo, config["env"], stderr_path, config["settings"])
    record = {"server": arguments.server, "start": "cold" if cold else "warm",
              "command": config["command"], "repo": repo, "target": target.get("name"),
              "dependency_sync": not arguments.no_dependency_sync, "phases": {}, "answers": {}}

    root_uri = pathlib.Path(repo).as_uri()
    capabilities = {
        "workspace": {"configuration": True, "workspaceFolders": True,
                      "didChangeConfiguration": {"dynamicRegistration": True}},
        "window": {"workDoneProgress": True},
        "textDocument": {
            "synchronization": {"didSave": True},
            "definition": {"linkSupport": True},
            "declaration": {"linkSupport": True},
            "typeDefinition": {"linkSupport": True},
            "implementation": {"linkSupport": True},
            "references": {},
            "hover": {"contentFormat": ["markdown", "plaintext"]},
            "documentSymbol": {"hierarchicalDocumentSymbolSupport": True},
            "documentHighlight": {},
            "completion": {"completionItem": {"snippetSupport": False}},
            "signatureHelp": {"signatureInformation": {"parameterInformation": {"labelOffsetSupport": True},
                                                       "activeParameterSupport": True},
                              "contextSupport": True},
            "codeAction": {"codeActionLiteralSupport": {"codeActionKind": {"valueSet": [
                "", "quickfix", "refactor", "refactor.extract", "refactor.inline", "refactor.rewrite",
                "source", "source.organizeImports"]}}},
            "rename": {"prepareSupport": True},
            "foldingRange": {"lineFoldingOnly": True},
            "selectionRange": {},
            "formatting": {}, "rangeFormatting": {}, "onTypeFormatting": {},
            "semanticTokens": {"requests": {"full": True, "range": False},
                               "tokenTypes": [], "tokenModifiers": [], "formats": ["relative"]},
            "inlayHint": {},
            "callHierarchy": {}, "typeHierarchy": {},
            "documentLink": {"tooltipSupport": True},
            "moniker": {},
            "diagnostic": {"relatedDocumentSupport": False},
            "publishDiagnostics": {},
        },
    }
    try:
        initialize = timed(server, "initialize", {
            "processId": os.getpid(), "rootUri": root_uri, "rootPath": repo,
            "workspaceFolders": [{"uri": root_uri, "name": os.path.basename(repo)}],
            "capabilities": capabilities,
            "initializationOptions": config["initialization_options"]}, timeout=300)
        record["phases"]["initialize"] = time.monotonic() - server.started
        record["capabilities"] = (initialize["result"] or {}).get("capabilities", {})
        if initialize.get("error"):
            record["error"] = f"initialize: {initialize['error']}"
            return
        server.notify("initialized", {})
        server.notify("workspace/didChangeConfiguration", {"settings": config["settings"]})

        documents = Documents(server, repo)
        probes = [probe for probe in target["probes"] if probe["language"] in languages]
        record["phases"]["ready"] = wait_until_ready(server, documents, repo, probes, arguments.ready_timeout)
        record["rss_at_ready_kib"] = server.rss_kib()
        if record["phases"]["ready"] is None:
            record["error"] = "never ready"
        # Answering the probes is not "done": background work (dependency
        # sync, library indexing, a Gradle import's later stages) can still
        # change answers. Settled = no `$/progress` activity for a while.
        record["phases"]["settled"] = wait_until_settled(server, arguments.settle_quiet, arguments.settle_timeout)
        record["rss_at_settled_kib"] = server.rss_kib()

        ask = lambda query: query["language"] in languages
        position = lambda query: {"textDocument": {"uri": uri_of(repo, query["file"])},
                                  "position": {"line": query["line"], "character": query["character"]}}

        definitions, hovers = [], []
        for query in filter(ask, query_set["definition"]):
            documents.open(query["file"])
            answer = timed(server, "textDocument/definition", position(query), arguments.request_timeout)
            definitions.append({"id": query["id"], "seconds": answer["seconds"], "error": answer.get("error"),
                                "locations": locations(answer["result"])})
            hover = timed(server, "textDocument/hover", position(query), arguments.request_timeout)
            text = text_of((hover["result"] or {}).get("contents"))
            hovers.append({"id": query["id"], "seconds": hover["seconds"], "error": hover.get("error"),
                           "empty": not text.strip(), "text": text[:2000]})
        record["answers"]["definition"], record["answers"]["hover"] = definitions, hovers

        references = []
        for query in filter(ask, query_set["references"]):
            documents.open(query["file"])
            params = dict(position(query), context={"includeDeclaration": False})
            answer = timed(server, "textDocument/references", params, arguments.request_timeout)
            references.append({"id": query["id"], "seconds": answer["seconds"], "error": answer.get("error"),
                               "locations": locations(answer["result"])})
        record["answers"]["references"] = references

        completions = []
        for query in filter(ask, query_set["completion"]):
            documents.open(query["file"])
            original = documents.texts[query["file"]]
            lines = original.split("\n")
            line = lines[query["line"]]
            # Delete the member name: `receiver.member(` → `receiver.(`.
            lines[query["line"]] = line[:query["character"]] + line[query["character"] + len(query["name"]):]
            documents.change(query["file"], "\n".join(lines))
            answer = timed(server, "textDocument/completion", dict(position(query), context={
                "triggerKind": 2, "triggerCharacter": "."}), arguments.request_timeout)
            labels, total = completion_labels(answer["result"])
            completions.append({"id": query["id"], "seconds": answer["seconds"], "error": answer.get("error"),
                                "items": labels, "total": total})
            documents.change(query["file"], original)
        record["answers"]["completion"] = completions

        if "documentSymbol" not in extra:
            ask_document_symbols(server, documents, repo, query_set, ask, arguments, record)
        asked = {kind: [query for query in queries if ask(query)] for kind, queries in query_set.items()}
        context = Context(server, documents, repo, asked, arguments.request_timeout, record["capabilities"])
        for feature in FEATURES:
            if feature.name in extra and feature.name != "hover":
                started = time.monotonic()
                record["answers"][feature.name] = feature.ask(context)
                print(f"  {feature.name}: {len(record['answers'][feature.name])} asked "
                      f"in {time.monotonic() - started:.0f}s", file=sys.stderr)
    finally:
        record["peak_rss_kib"] = server.peak_rss_kib
        record["progress"] = server.progress_events[-200:]
        record["total_seconds"] = time.monotonic() - server.started
        server.stop()
        with open(arguments.out, "w") as handle:
            json.dump(record, handle, indent=1)
        print(f"{arguments.server} ({record['start']}): ready={record['phases'].get('ready')} "
              f"peak={record['peak_rss_kib'] // 1024} MiB → {arguments.out}", file=sys.stderr)


def extra_features(selection):
    """Names of the features.py features `--features` selects."""
    if selection == "base":
        return set()
    if selection == "all":
        return set(BY_NAME)
    names = {name.strip() for name in selection.split(",") if name.strip()}
    unknown = names - set(BY_NAME) - {"base"}
    if unknown:
        raise SystemExit(f"unknown features: {', '.join(sorted(unknown))} (known: {', '.join(BY_NAME)})")
    return names - {"base"}


def ask_document_symbols(server, documents, repo, query_set, ask, arguments, record):
    """Latency of documentSymbol on the first files the queries touch (the
    comparison report's column; the integration test asks features.py's)."""
    symbols = []
    files = sorted({query["file"] for kind in query_set.values() for query in kind if ask(query)})
    for relative in files[:arguments.symbols_files]:
        documents.open(relative)
        answer = timed(server, "textDocument/documentSymbol",
                       {"textDocument": {"uri": uri_of(repo, relative)}}, arguments.request_timeout)
        symbols.append({"file": relative, "seconds": answer["seconds"], "error": answer.get("error"),
                        "count": len(answer["result"] or [])})
    record["answers"]["documentSymbol"] = symbols


if __name__ == "__main__":
    main()
