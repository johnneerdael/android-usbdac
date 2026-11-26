#!/usr/bin/env python3
"""Checks that the code samples in the SDK guide come from the sample app.

Every ```kotlin and ```xml block in docs/sdk must be an excerpt of a source file of the sample
(android/sample/src/main), line by line and in order, ignoring indentation and blank lines. A line
that is only `// ...` (or `<!-- ... -->` in XML) splits a block into excerpts that are each checked.
A block preceded by the line `<!-- uncompiled -->` is guidance that is not compiled anywhere and is
skipped; the script lists those so they stay visible. Other languages (gradle, text) are not checked.
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
GUIDE = ROOT / "docs" / "sdk"
SAMPLE = ROOT / "android" / "sample" / "src" / "main"
SPLIT = {"kotlin": "// ...", "xml": "<!-- ... -->"}


def lines_of(text: str) -> list[str]:
    return [line.strip() for line in text.splitlines() if line.strip()]


def contains(haystack: list[str], needle: list[str]) -> bool:
    n = len(needle)
    return any(haystack[i:i + n] == needle for i in range(len(haystack) - n + 1))


def blocks(path: pathlib.Path):
    """Yields (language, first line number, lines, uncompiled) for each fenced block."""
    text = path.read_text(encoding="utf-8").splitlines()
    i = 0
    previous = ""
    while i < len(text):
        match = re.match(r"^```(\w+)\s*$", text[i])
        if match:
            start = i + 1
            end = start
            while end < len(text) and not text[end].startswith("```"):
                end += 1
            yield match.group(1), start + 1, text[start:end], previous == "<!-- uncompiled -->"
            i = end + 1
            previous = ""
            continue
        if text[i].strip():
            previous = text[i].strip()
        i += 1


def main() -> int:
    sources = {
        path: lines_of(path.read_text(encoding="utf-8"))
        for path in sorted(SAMPLE.rglob("*"))
        if path.suffix in {".kt", ".xml"}
    }
    checked = skipped = 0
    failures = []
    for page in sorted(GUIDE.glob("*.md")):
        for language, line, body, uncompiled in blocks(page):
            if language not in SPLIT:
                continue
            if uncompiled:
                skipped += 1
                print(f"uncompiled: {page.name}:{line} ({language})")
                continue
            segments, current = [], []
            for raw in body:
                if raw.strip() == SPLIT[language]:
                    segments.append(current)
                    current = []
                else:
                    current.append(raw)
            segments.append(current)
            for segment in segments:
                needle = lines_of("\n".join(segment))
                if not needle:
                    continue
                checked += 1
                if not any(contains(source, needle) for source in sources.values()):
                    failures.append(f"{page.name}:{line}: not found in the sample: {needle[0]!r} ...")
    for failure in failures:
        print(failure, file=sys.stderr)
    print(f"{checked} excerpts checked against the sample, {skipped} blocks marked uncompiled, {len(failures)} failures")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
