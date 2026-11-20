#!/usr/bin/env python3
"""Check that every cross-reference in the repository still resolves.

Two kinds of reference are checked, outside third_party/:

- A relative Markdown link, `[text](path#anchor)` or `[text](#anchor)`,
  in a tracked Markdown file must name a tracked file or directory, and
  its anchor a heading of the Markdown file it names, as GitHub derives
  heading anchors.
- A quoted rule title, `INVARIANTS "Title"` or `IME.md "Title"` (with a
  section name, a comma or a colon allowed between), in any tracked text
  file, code comments included, must be the start of a rule title (the
  bold lead of a bullet) or a section heading of that document. A
  trailing "..." or "…" marks a shortened title. A quote must name the
  rule's owner: an INVARIANTS bullet that only points to an IME rule
  (its body links to IME.md) is not cited, and each such pointer's title
  must be an IME.md rule title.

Usage: python3 -I scripts/check-doc-links.py
"""

import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DOCS = {"INVARIANTS": "docs/INVARIANTS.md", "IME.md": "docs/IME.md"}
TEXT_FILES = ("*.md", "*.rs", "*.toml", "*.py", "*.nix", "*.yaml", "*.sh")
FENCE = re.compile(r"^ {0,3}(```|~~~).*?^ {0,3}\1", re.MULTILINE | re.DOTALL)
CODE_SPAN = re.compile(r"(`+)(?:(?!\1).)+?\1", re.DOTALL)
LINK = re.compile(r"\[(?:[^\]\\]|\\.)*\]\(([^)\s]+)\)")
HEADING = re.compile(r"^ {0,3}#{1,6}[ \t]+(.+?)[ \t#]*$", re.MULTILINE)
RULE = re.compile(r"^[ \t]*- \*\*([^*]+?)\*\*", re.MULTILINE)
# A document's name, then at most a section name, then the quote of a
# title, which starts with a capital or a code span.
QUOTE = re.compile(
    r"\b(?:docs/)?(INVARIANTS|IME\.md)(?:\.md)?[\w ,:]{0,30}?\\?\"([A-Z`][^\"]*?)\\?\""
)
# Comment leaders a quote can run across.
CONTINUATION = re.compile(r"[ \t]*\n[ \t]*(?://[/!]?|#)?[ \t]*")
# The section of IME.md that owns the IME rules.
POINTER = "IME.md#26-rules"


def tracked(*pathspecs: str, everywhere: bool = False) -> list[pathlib.Path]:
    exclude = [] if everywhere else [":(exclude)third_party"]
    files = subprocess.run(
        ["git", "ls-files", "-z", "--", *pathspecs, *exclude],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.split("\0")
    # A file deleted in the work tree is not checked.
    return [ROOT / file for file in files if file and (ROOT / file).exists()]


def unfenced(text: str) -> str:
    """Markdown with its code blocks blanked out."""
    return FENCE.sub(lambda found: "\n" * found.group().count("\n"), text)


def prose(text: str) -> str:
    """Markdown with its code blocks and code spans blanked out."""
    return CODE_SPAN.sub(lambda found: " " * len(found.group()), unfenced(text))


def slug(heading: str) -> str:
    """The anchor GitHub gives a heading."""
    heading = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", heading)
    heading = re.sub(r"[^\w\- ]", "", heading.strip().lower())
    return heading.replace(" ", "-")


def anchors(doc: pathlib.Path) -> set[str]:
    seen: dict[str, int] = {}
    result = set()
    for found in HEADING.finditer(unfenced(doc.read_text(encoding="utf-8"))):
        base = slug(found.group(1))
        count = seen.get(base, 0)
        seen[base] = count + 1
        result.add(base if count == 0 else f"{base}-{count}")
    return result


def normal(text: str) -> str:
    return " ".join(text.split()).rstrip(".").rstrip()


def rules(doc: pathlib.Path) -> dict[str, str]:
    """Each rule title and section heading, with the rule's first lines."""
    text = doc.read_text(encoding="utf-8")
    titles = {normal(found.group(1)): "" for found in HEADING.finditer(text)}
    for found in RULE.finditer(text):
        body = text[found.end() : found.end() + 400].split("\n- ")[0]
        titles[normal(found.group(1))] = body
    return titles


def check_links(files: list[str], failures: list[str]) -> int:
    count = 0
    cache: dict[pathlib.Path, set[str]] = {}
    for doc in tracked("*.md"):
        relative = doc.relative_to(ROOT).as_posix()
        for found in LINK.finditer(prose(doc.read_text(encoding="utf-8"))):
            target = found.group(1)
            if re.match(r"[a-z][a-z0-9+.-]*:", target):
                continue
            count += 1
            path, _, anchor = target.partition("#")
            resolved = (doc.parent / path).resolve() if path else doc
            name = resolved.relative_to(ROOT).as_posix() if resolved.is_relative_to(ROOT) else None
            if name is None or not (
                name in files or any(file.startswith(name + "/") for file in files)
            ):
                failures.append(f"{relative} links to {target}, which is no tracked file")
                continue
            if anchor:
                if resolved.suffix != ".md":
                    failures.append(f"{relative} links to {target}: only Markdown has anchors")
                    continue
                if resolved not in cache:
                    cache[resolved] = anchors(resolved)
                if anchor not in cache[resolved]:
                    failures.append(f"{relative} links to {target}, which is no heading there")
    return count


def check_quotes(failures: list[str]) -> int:
    titles = {name: rules(ROOT / path) for name, path in DOCS.items()}
    for title, body in titles["INVARIANTS"].items():
        if POINTER in body and title not in titles["IME.md"]:
            failures.append(f'INVARIANTS points to "{title}", which is no IME.md rule')
    count = 0
    for file in tracked(*TEXT_FILES):
        relative = file.relative_to(ROOT).as_posix()
        if file == pathlib.Path(__file__).resolve():
            continue
        text = CONTINUATION.sub(" ", file.read_text(encoding="utf-8"))
        for found in QUOTE.finditer(text):
            doc, quote = found.group(1), found.group(2)
            count += 1
            shortened = quote.endswith(("…", "..."))
            quote = normal(quote.rstrip("…").rstrip("."))
            matches = [
                title
                for title in titles[doc]
                if title == quote or (shortened and title.startswith(quote))
            ]
            if not matches:
                failures.append(f'{relative} quotes {doc} "{quote}", which is no rule title there')
            elif doc == "INVARIANTS" and all(
                POINTER in titles[doc][title] for title in matches
            ):
                failures.append(f'{relative} quotes INVARIANTS "{quote}"; quote its owner, IME.md')
    return count


def main() -> int:
    files = [path.relative_to(ROOT).as_posix() for path in tracked(everywhere=True)]
    failures: list[str] = []
    links = check_links(files, failures)
    quotes = check_quotes(failures)
    if links == 0 or quotes == 0:
        print("The documents have no links or no quoted rules; the check is broken")
        return 1
    for failure in failures:
        print(failure)
    if failures:
        return 1
    print(f"All {links} links and {quotes} quoted rule titles resolve.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
