#!/usr/bin/env python3
"""Check the Java-version cases of src/itest against javac.

Every case under src/itest/cases/java/ with `//= expected java <range>`
blocks is compiled with `javac --release N` for every release of every
range. An expectation holds when javac reports an error at exactly the
lines and columns it lists for the file the cursor is in (the wording is
kmp-lsp's own, so only positions are compared), and none where it says
`(nothing)`. Warnings are not compared.

    scripts/check-java-version-cases.py [--javac PATH] [CASE ...]

The javac has to accept `--release` for every version the cases name
(8 to 25 needs a JDK 25 or newer). Exits non-zero if any case disagrees.
"""

import argparse
import pathlib
import re
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
CASES = ROOT / "src/itest/cases/java"
OLDEST, NEWEST = 8, 25
HEADER = "//= expected"
FILE_HEADER = "//- /"
CURSOR = "$0"
EXPECTED_LINE = re.compile(r"^/(\S+):(\d+):(\d+)  ")
JAVAC_ERROR = re.compile(r"^(.+\.java):(\d+):(\d+): compiler\.err")


def parse(text):
    """(files {path: text}, cursor path, [(first, last, {(line, column)})])."""
    fixture, *blocks = text.split(HEADER)
    files, current = {}, None
    for line in fixture.strip().splitlines():
        if line.startswith(FILE_HEADER):
            current = line[len(FILE_HEADER):].strip()
            files[current] = ""
        else:
            files[current] += line + "\n"
    cursor = next(path for path, body in files.items() if CURSOR in body)
    files = {path: body.replace(CURSOR, "") for path, body in files.items()}
    ranges = []
    for block in blocks:
        header, _, body = block.partition("\n")
        header = header.strip()
        if not header.startswith("java "):
            continue
        spelled = header[len("java "):].strip()
        if ".." in spelled:
            first, last = spelled.split("..")
            first, last = int(first or OLDEST), int(last or NEWEST)
        else:
            first = last = int(spelled)
        expected = set()
        for line in body.strip().splitlines():
            match = EXPECTED_LINE.match(line)
            if match and match.group(1) == cursor:
                expected.add((int(match.group(2)), int(match.group(3))))
        ranges.append((first, last, expected))
    return files, cursor, ranges


def errors(javac, files, cursor, release):
    """The (line, column) of every error javac reports in `cursor`."""
    with tempfile.TemporaryDirectory() as directory:
        root = pathlib.Path(directory)
        for path, body in files.items():
            target = root / "src" / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(body)
        sources = [str(root / "src" / path) for path in files]
        result = subprocess.run(
            [javac, "--release", str(release), "-proc:none", "-XDrawDiagnostics",
             "-nowarn", "-d", str(root / "out"), *sources],
            capture_output=True, text=True,
        )
        # -XDrawDiagnostics names a file by its base name alone.
        names = [pathlib.PurePosixPath(path).name for path in files]
        cursor_name = pathlib.PurePosixPath(cursor).name
        if names.count(cursor_name) > 1:
            sys.exit(f"{cursor}: another file of the case is also named {cursor_name}")
        found = set()
        for line in (result.stdout + result.stderr).splitlines():
            match = JAVAC_ERROR.match(line)
            if match and match.group(1) == cursor_name:
                found.add((int(match.group(2)), int(match.group(3))))
        return found


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--javac", default="javac")
    parser.add_argument("cases", nargs="*", type=pathlib.Path)
    arguments = parser.parse_args()
    cases = arguments.cases or sorted(CASES.rglob("*.case"))
    disagreements = checked = 0
    for case in cases:
        files, cursor, ranges = parse(case.read_text())
        for first, last, expected in ranges:
            for release in range(first, last + 1):
                checked += 1
                found = errors(arguments.javac, files, cursor, release)
                if found != expected:
                    disagreements += 1
                    print(f"{case.relative_to(ROOT)} (java {release}): "
                          f"expected {sorted(expected)}, javac {sorted(found)}")
    print(f"{checked} case releases compiled, {disagreements} disagree")
    return 1 if disagreements else 0


if __name__ == "__main__":
    sys.exit(main())
