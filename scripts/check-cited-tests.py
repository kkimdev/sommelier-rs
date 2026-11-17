#!/usr/bin/env python3
"""Check that every test the design documents cite still exists.

docs/INVARIANTS.md and docs/DESIGN.md cite the tests that pin a rule in
"Test:" and "Tests:" clauses. A clause runs to the end of its sentence,
its paragraph or the parenthesis it sits in, and every backticked name in
it must be a test: a function under a test attribute in a tracked Rust
file. A renamed or removed test must take its citation with it.

Usage: python3 -I scripts/check-cited-tests.py
"""

import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DOCS = [ROOT / "docs" / "INVARIANTS.md", ROOT / "docs" / "DESIGN.md"]
CLAUSE = re.compile(r"\b[Tt]ests?:")
NAME = re.compile(r"[a-z_][a-z0-9_]*")
# A function under `#[test]` or `#[tokio::test(...)]`, past any other
# attributes and doc comments.
TEST = re.compile(
    r"#\[(?:tokio::)?test\b[^\]]*\]"
    r"\s*(?:(?:#\[[^\]]*\]|///[^\n]*)\s*)*"
    r"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)
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
        if at_sentence_end or PARAGRAPH_END.match(text, position):
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


def defined_tests() -> set[str]:
    files = subprocess.run(
        ["git", "ls-files", "*.rs"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.split()
    return {
        name
        for file in files
        for name in TEST.findall((ROOT / file).read_text(encoding="utf-8"))
    }


def main() -> int:
    tests = defined_tests()
    failed = False
    total = 0
    for doc in DOCS:
        relative = doc.relative_to(ROOT)
        for name in cited_names(doc):
            total += 1
            if not NAME.fullmatch(name):
                print(f"{relative} cites `{name}`, which is not a test name")
                failed = True
            elif name not in tests:
                print(f"{relative} cites `{name}`, which no Rust test defines")
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
