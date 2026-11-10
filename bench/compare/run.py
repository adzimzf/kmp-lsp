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

from lsp import LanguageServer, LspError, RequestTimeout
from score import Repo, normalize_target
from servers import SERVERS


def uri_of(repo, relative):
    return pathlib.Path(repo, relative).resolve().as_uri()


def language_id(path):
    return "kotlin" if path.endswith(".kt") else "java"


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


def timed(server, method, params, timeout):
    try:
        result, seconds = server.request(method, params, timeout=timeout)
        return {"result": result, "seconds": seconds}
    except RequestTimeout:
        return {"result": None, "seconds": timeout, "error": "timeout"}
    except LspError as error:
        return {"result": None, "seconds": None, "error": str(error)}


def probe_satisfied(repo, probe, answer):
    """Does this probe's definition answer land where the target says?

    `expect_file`/`expect_line`: that file, within three lines of that line.
    `expect_class`: a library class of that name — the JDK and the
    dependencies are indexed in the background without reporting progress,
    so a probe on `Long` or `Map` is what tells a client they are there."""
    found = locations(answer["result"])
    if "expect_class" in probe:
        return any(normalize_target(repo, location, probe["name"])
                   == ("external", probe["expect_class"]) for location in found)
    expected_uri = uri_of(repo.root, probe["expect_file"])
    return any(location["uri"] == expected_uri
               and abs(location["line"] - (probe["expect_line"] - 1)) <= 3
               for location in found)

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
                "textDocument": {"uri": uri_of(repo.root, probe["file"])},
                "position": {"line": probe["line"] - 1, "character": character}}, timeout=30)
            if not probe_satisfied(repo, probe, answer):
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
    arguments = parser.parse_args()

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
            "references": {},
            "hover": {"contentFormat": ["markdown", "plaintext"]},
            "documentSymbol": {"hierarchicalDocumentSymbolSupport": True},
            "completion": {"completionItem": {"snippetSupport": False}},
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
        if initialize.get("error"):
            record["error"] = f"initialize: {initialize['error']}"
            return
        server.notify("initialized", {})
        server.notify("workspace/didChangeConfiguration", {"settings": config["settings"]})

        documents = Documents(server, repo)
        probes = [probe for probe in target["probes"] if probe["language"] in languages]
        record["phases"]["ready"] = wait_until_ready(server, documents, Repo(repo), probes,
                                                     arguments.ready_timeout)
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
            hovers.append({"id": query["id"], "seconds": hover["seconds"], "error": hover.get("error"),
                           "empty": not hover["result"] or not hover["result"].get("contents")})
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

        symbols = []
        files = sorted({query["file"] for kind in query_set.values() for query in kind if ask(query)})
        for relative in files[:arguments.symbols_files]:
            documents.open(relative)
            answer = timed(server, "textDocument/documentSymbol",
                           {"textDocument": {"uri": uri_of(repo, relative)}}, arguments.request_timeout)
            symbols.append({"file": relative, "seconds": answer["seconds"], "error": answer.get("error"),
                            "count": len(answer["result"] or [])})
        record["answers"]["documentSymbol"] = symbols
    finally:
        record["peak_rss_kib"] = server.peak_rss_kib
        record["progress"] = server.progress_events[-200:]
        record["total_seconds"] = time.monotonic() - server.started
        server.stop()
        with open(arguments.out, "w") as handle:
            json.dump(record, handle, indent=1)
        print(f"{arguments.server} ({record['start']}): ready={record['phases'].get('ready')} "
              f"peak={record['peak_rss_kib'] // 1024} MiB → {arguments.out}", file=sys.stderr)


if __name__ == "__main__":
    main()
