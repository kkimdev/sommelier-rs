#!/usr/bin/env python3
"""Check that every test the documentation cites still exists.

The repository's Markdown documents, outside third_party/, cite the tests
that pin a rule in "Test:" and "Tests:" clauses ("Row tests:" too). A
clause runs to the end of its sentence, its paragraph, the parenthesis
it sits in or its table cell, and every backticked name in it must be a
test: a function under a test attribute in a tracked Rust file. A
renamed or removed test must take its citation with it. The name must be
the only test of that name, and not a test of the DST's models or harness
in sommelier/src/sim/, which pins those rather than the proxy; the DST's
runs and its regressions may be cited.

Usage: python3 -I scripts/check-cited-tests.py
"""

import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
CLAUSE = re.compile(r"\b[Tt]ests?:")
NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
# A function under `#[test]` or `#[tokio::test(...)]` at the start of a
# line, not in a comment, past any other attributes and doc comments.
TEST = re.compile(
    r"^[ \t]*#\[(?:tokio::)?test\b[^\]]*\]"
    r"\s*(?:(?:#\[[^\]]*\]|///[^\n]*)\s*)*"
    r"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)",
    re.MULTILINE,
)
# The DST lives here; the tests of its models and harness pin those, not
# the proxy, so a document cites only the DST's runs and its regressions.
MODELS = "sommelier/src/sim/"
DST = {
    "ime_reaches_the_specified_text",
    "ime_reaches_the_intended_text",
    "every_regression_passes_the_oracle",
}
PARAGRAPH_END = re.compile(r"\n\s*(?:\n|- )")


def clause_names(text: str, start: int) -> list[str]:
    """The backticked names of the clause that starts at `start`."""
    names = []
    depth = 0
    position = start
    while position < len(text):
        character = text[position]
        if character == "`":
            end = text.index("`", position + 1)
            names.append(text[position + 1 : end])
            position = end + 1
            continue
        at_sentence_end = character == "." and (
            position + 1 == len(text) or text[position + 1].isspace()
        )
        at_cell_end = character == "|"
        if at_sentence_end or at_cell_end or PARAGRAPH_END.match(text, position):
            break
        if character == "(":
            depth += 1
        elif character == ")":
            if depth == 0:
                break
            depth -= 1
        position += 1
    return names


def cited_names(doc: pathlib.Path) -> list[str]:
    text = doc.read_text(encoding="utf-8")
    return [
        name
        for clause in CLAUSE.finditer(text)
        for name in clause_names(text, clause.end())
    ]


def tracked(*pathspecs: str) -> list[pathlib.Path]:
    files = subprocess.run(
        ["git", "ls-files", "-z", "--", *pathspecs],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.split("\0")
    return [ROOT / file for file in files if file]


def defined_tests() -> dict[str, list[str]]:
    """Each test's name, with the files that define a test of that name."""
    tests: dict[str, list[str]] = {}
    for file in tracked("*.rs"):
        for name in TEST.findall(file.read_text(encoding="utf-8")):
            tests.setdefault(name, []).append(file.relative_to(ROOT).as_posix())
    return tests


def main() -> int:
    tests = defined_tests()
    failed = False
    total = 0
    for doc in tracked("*.md", ":(exclude)third_party"):
        relative = doc.relative_to(ROOT)
        for name in cited_names(doc):
            total += 1
            if not NAME.fullmatch(name):
                print(f"{relative} cites `{name}`, which is not a test name")
                failed = True
            elif name not in tests:
                print(f"{relative} cites `{name}`, which no Rust test defines")
                failed = True
            elif len(tests[name]) > 1:
                files = ", ".join(tests[name])
                print(f"{relative} cites `{name}`, which more than one test defines: {files}")
                failed = True
            elif tests[name][0].startswith(MODELS) and name not in DST:
                what = "the DST's models or harness, not the proxy"
                print(f"{relative} cites `{name}`, which tests {what}")
                failed = True
    if total == 0:
        print("The documents cite no tests; the check is broken")
        return 1
    if failed:
        return 1
    print(f"All {total} test citations name existing tests.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
