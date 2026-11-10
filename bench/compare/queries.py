#!/usr/bin/env python3
"""Extract a fixed, seeded query set from a target repository.

Every server is asked exactly the same questions. Positions come from a small
comment/string-aware lexer over the sources (no language server is involved,
so the query set favours none of them):

* definition — identifier *usages*: `.member`, `call(`, and type references
  (`: Type`, `new Type`, `<Type>`, `is Type`, `@Annotation`);
* references — *declarations*: Kotlin `fun/class/interface/object/val/var`,
  Java `class/interface/enum/record` names and method declarations;
* completion — an existing member access `receiver.member` with `member`
  deleted; the deleted name is the ground truth (it is what the author typed).

Usage: queries.py <repo> <target.json> <out.json>
"""

import json
import os
import random
import re
import sys

KOTLIN_KEYWORDS = set("""
as as? break class continue do else false for fun if in !in interface is !is null object package
return super this throw true try typealias typeof val var when while by catch constructor delegate
dynamic field file finally get import init param property receiver set setparam where actual
abstract annotation companion const crossinline data enum expect external final infix inline inner
internal lateinit noinline open operator out override private protected public reified sealed
suspend tailrec vararg it value
""".split())

JAVA_KEYWORDS = set("""
abstract assert boolean break byte case catch char class const continue default do double else enum
extends final finally float for goto if implements import instanceof int interface long native new
package private protected public return short static strictfp super switch synchronized this throw
throws transient try void volatile while true false null var record yield sealed permits non-sealed
""".split())

KOTLIN_DECLARERS = {"fun", "class", "interface", "object", "val", "var", "typealias"}
JAVA_DECLARERS = {"class", "interface", "enum", "record"}

IDENTIFIER = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def literal_spans(text: str):
    """(start, end, kind) of every comment (`comment`) and string/char
    literal (`string`) in `text`."""
    spans = []
    i, n = 0, len(text)
    while i < n:
        if text.startswith("//", i):
            end = text.find("\n", i)
            end = n if end < 0 else end
            spans.append((i, end, "comment"))
            i = end
        elif text.startswith("/*", i):
            end = text.find("*/", i + 2)
            end = n if end < 0 else end + 2
            spans.append((i, end, "comment"))
            i = end
        elif text.startswith('"""', i):
            end = text.find('"""', i + 3)
            end = n if end < 0 else end + 3
            spans.append((i, end, "string"))
            i = end
        elif text[i] in "\"'":
            quote, j = text[i], i + 1
            while j < n and text[j] != quote and text[j] != "\n":
                j += 2 if text[j] == "\\" else 1
            spans.append((i, min(j + 1, n), "string"))
            i = j + 1
        else:
            i += 1
    return spans


def blank_comments_and_strings(text: str, kotlin: bool) -> str:
    """`text` with comments and string/char literals replaced by spaces
    (newlines kept), so offsets and line numbers stay valid."""
    out = list(text)
    for start, end, _ in literal_spans(text):
        for k in range(start, end):
            if out[k] != "\n":
                out[k] = " "
    return "".join(out)


class Token:
    __slots__ = ("text", "line", "column", "offset")

    def __init__(self, text, line, column, offset):
        self.text, self.line, self.column, self.offset = text, line, column, offset


def tokens(code: str):
    """Identifiers and single punctuation characters, with 0-based line/column."""
    line_starts = [0]
    for index, char in enumerate(code):
        if char == "\n":
            line_starts.append(index + 1)
    import bisect

    result = []
    i, n = 0, len(code)
    while i < n:
        char = code[i]
        if char.isspace():
            i += 1
            continue
        match = IDENTIFIER.match(code, i)
        end = match.end() if match else i + 1
        line = bisect.bisect_right(line_starts, i) - 1
        result.append(Token(code[i:end], line, i - line_starts[line], i))
        i = end
    return result


def is_identifier(token, keywords):
    return IDENTIFIER.fullmatch(token.text) is not None and token.text not in keywords


def header_lines(code: str):
    """Lines of `package`/`import` statements — never queried."""
    lines = set()
    for number, line in enumerate(code.split("\n")):
        stripped = line.lstrip()
        if stripped.startswith("package ") or stripped.startswith("import "):
            lines.add(number)
    return lines


def classify(tokens_, index, kotlin):
    """('definition', category) for a usage, ('declaration', kind) for a
    declaration name, None otherwise."""
    keywords = KOTLIN_KEYWORDS if kotlin else JAVA_KEYWORDS
    token = tokens_[index]
    if not is_identifier(token, keywords):
        return None
    previous = tokens_[index - 1].text if index > 0 else ""
    before_previous = tokens_[index - 2].text if index > 1 else ""
    following = tokens_[index + 1].text if index + 1 < len(tokens_) else ""
    capitalized = token.text[0].isupper()

    if kotlin:
        # `fun Receiver.name(` / `val Receiver.name` — the receiver is a type
        # usage; the name after the dot is the declaration.
        if previous in KOTLIN_DECLARERS and following == ".":
            return ("definition", "type_reference") if capitalized else None
        if previous in KOTLIN_DECLARERS:
            return ("declaration", previous)
        if previous == "." and index > 2 and tokens_[index - 3].text in KOTLIN_DECLARERS:
            return ("declaration", tokens_[index - 3].text)
    else:
        if previous in JAVA_DECLARERS:
            return ("declaration", previous)
        # `Type name(` / `<T> T name(` / `int[] name(` at member level: a method declaration.
        previous_token = tokens_[index - 1] if index > 0 else None
        looks_like_type = previous_token is not None and (
            (is_identifier(previous_token, set()) and previous not in {"return", "new", "throw", "else", "case", "yield"})
            or previous in {">", "]"}
        )
        if following == "(" and looks_like_type and previous not in JAVA_KEYWORDS - {"void", "int", "long", "boolean", "byte", "char", "short", "float", "double"}:
            return ("declaration", "method")
        if following == "(" and previous in {"void", "int", "long", "boolean", "byte", "char", "short", "float", "double"}:
            return ("declaration", "method")

    if previous == "@":
        return ("definition", "annotation")
    if previous == ".":
        return ("definition", "member_call" if following == "(" else "member_access")
    if following == "(" and previous not in {"fun", "new"}:
        return ("definition", "call")
    if capitalized and previous in {":", "new", "<", "is", "as", "(", ","} and following not in {"=", ":"}:
        return ("definition", "type_reference")
    return None


def source_files(repo, roots, extensions):
    for root in roots:
        for directory, subdirectories, files in os.walk(os.path.join(repo, root)):
            subdirectories[:] = sorted(d for d in subdirectories if d not in {"build", ".gradle", ".git"})
            for name in sorted(files):
                if os.path.splitext(name)[1] in extensions:
                    yield os.path.join(directory, name)


def extract(repo, target):
    rng = random.Random(target.get("seed", 7))
    candidates = {"definition": {}, "references": {}, "completion": {}}
    files = list(source_files(repo, target.get("roots", ["."]), {".kt", ".java"}))
    for path in files:
        kotlin = path.endswith(".kt")
        language = "kotlin" if kotlin else "java"
        relative = os.path.relpath(path, repo)
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
        code = blank_comments_and_strings(text, kotlin)
        skipped = header_lines(code)
        stream = tokens(code)
        for index, token in enumerate(stream):
            if token.line in skipped:
                continue
            verdict = classify(stream, index, kotlin)
            if verdict is None:
                continue
            role, category = verdict
            query = {"file": relative, "language": language, "line": token.line,
                     "character": token.column, "name": token.text, "category": category}
            if role == "declaration":
                candidates["references"].setdefault(language, []).append(query)
                continue
            candidates["definition"].setdefault(language, []).append(query)
            # A member access on a plain receiver is a completion question.
            receiver = stream[index - 2] if index > 1 else None
            receiver_is_simple = receiver is not None and (
                is_identifier(receiver, set()) or receiver.text in {")", "]"})
            if category in {"member_call", "member_access"} and receiver_is_simple:
                candidates["completion"].setdefault(language, []).append(query)

    sizes = target.get("sample", {"definition": 150, "references": 40, "completion": 100})
    selected = {}
    for kind, by_language in candidates.items():
        selected[kind] = []
        for language in sorted(by_language):
            pool = by_language[language]
            count = min(sizes[kind], len(pool))
            picked = rng.sample(pool, count)
            picked.sort(key=lambda q: (q["file"], q["line"], q["character"]))
            for number, query in enumerate(picked):
                query["id"] = f"{kind}-{language}-{number:03d}"
            selected[kind].extend(picked)
    stats = {kind: {language: len(pool) for language, pool in by_language.items()}
             for kind, by_language in candidates.items()}
    # File-level questions draw from their own generator, so adding them left
    # the definition/references/completion sample (and every fixture built
    # from it) exactly as it was.
    file_candidates = file_level_candidates(repo, files)
    file_rng = random.Random(target.get("seed", 7) + 1)
    for kind, by_language in file_candidates.items():
        selected[kind] = []
        for language in sorted(by_language):
            pool = by_language[language]
            picked = file_rng.sample(pool, min(sizes.get(kind, FILE_LEVEL_SAMPLE[kind]), len(pool)))
            picked.sort(key=lambda q: (q["file"], q["line"], q["character"]))
            for number, query in enumerate(picked):
                query["id"] = f"{kind}-{language}-{number:03d}"
            selected[kind].extend(picked)
        stats[kind] = {language: len(pool) for language, pool in by_language.items()}
    return {"repo": os.path.abspath(repo), "files": len(files), "candidates": stats, "queries": selected}


# How many of each file-level question a target gets unless its `sample` says.
FILE_LEVEL_SAMPLE = {"files": 25, "imports": 20, "links": 15}

IMPORT = re.compile(r"^\s*import\s+((?:[a-z_][\w]*\.)+)([A-Z]\w*)\s*;?\s*$")
LICENSE_URL = "apache.org/licenses"
URL = re.compile(r"https?://[^\s<>\"'(){}\[\]]+")


def top_level_declaration(stream, stem, kotlin):
    """The token naming the file's own top-level type (`class Gson` in
    Gson.java), with the keyword that declares it; None for a file without one."""
    declarers = KOTLIN_DECLARERS if kotlin else JAVA_DECLARERS
    for index in range(1, len(stream)):
        if stream[index].text == stem and stream[index - 1].text in declarers:
            return stream[index], stream[index - 1].text
    return None


def removable_import(text, code, stream, skipped):
    """(import line, package, simple name, first usage token) of the first
    single-type import whose type is used in the body: deleting that line
    leaves a usage the "add import" quick fix must repair."""
    lines = text.split("\n")
    for number, line in enumerate(lines):
        match = IMPORT.match(line)
        if not match or " static " in f" {line} ":
            continue
        package, simple = match.group(1).rstrip("."), match.group(2)
        for token in stream:
            if token.line not in skipped and token.text == simple:
                return number, package, simple, token
    return None


def comment_urls(text):
    """[line, character, url] of every http(s) URL inside a comment."""
    line_starts = [0] + [index + 1 for index, char in enumerate(text) if char == "\n"]
    import bisect
    found = []
    for start, end, kind in literal_spans(text):
        if kind != "comment":
            continue
        for match in URL.finditer(text, start, end):
            url = match.group(0).rstrip(".,;:*")
            line = bisect.bisect_right(line_starts, match.start()) - 1
            found.append([line, match.start() - line_starts[line], url])
    return found


def file_level_candidates(repo, files):
    """Per-file questions: `files` (a file and its top-level type: symbols,
    folding, diagnostics, formatting, hierarchy), `imports` (an import whose
    deletion breaks a usage), `links` (a file with URLs in its comments)."""
    candidates = {"files": {}, "imports": {}, "links": {}}
    for path in files:
        kotlin = path.endswith(".kt")
        language = "kotlin" if kotlin else "java"
        relative = os.path.relpath(path, repo)
        stem = os.path.splitext(os.path.basename(path))[0]
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
        code = blank_comments_and_strings(text, kotlin)
        skipped = header_lines(code)
        stream = tokens(code)
        declaration = top_level_declaration(stream, stem, kotlin)
        if declaration:
            token, declarer = declaration
            candidates["files"].setdefault(language, []).append(
                {"file": relative, "language": language, "line": token.line, "character": token.column,
                 "name": stem, "category": declarer})
        removable = removable_import(text, code, stream, skipped)
        if removable:
            import_line, package, simple, usage = removable
            candidates["imports"].setdefault(language, []).append(
                {"file": relative, "language": language, "line": usage.line, "character": usage.column,
                 "name": simple, "category": "import", "import_line": import_line,
                 "fqn": f"{package}.{simple}"})
        urls = comment_urls(text)
        # Nearly every file carries the same license-header URL; a file is a
        # link question only when its comments point somewhere else as well.
        if any(LICENSE_URL not in url for _, _, url in urls):
            candidates["links"].setdefault(language, []).append(
                {"file": relative, "language": language, "line": urls[0][0], "character": urls[0][1],
                 "name": stem, "category": "link", "urls": urls})
    return candidates


def main():
    repo, target_path, out = sys.argv[1:4]
    with open(target_path) as handle:
        target = json.load(handle)
    result = extract(repo, target)
    with open(out, "w") as handle:
        json.dump(result, handle, indent=1)
    counts = {kind: len(queries) for kind, queries in result["queries"].items()}
    print(f"{result['files']} files; candidates {result['candidates']}; selected {counts}")


if __name__ == "__main__":
    main()
