#!/usr/bin/env python3
"""Checks the links of the built documentation site.

    python check_site_links.py _site [--external]

Every relative link in the pages the guide produces (the HTML files in the top directory of the
site, not Dokka's api/ tree) must point at a file that exists, and a #fragment must name an id or
anchor in that file. Links to api/ are checked as files only. With --external the https links are
also requested (HEAD, then GET) and a status of 400 or more is reported; that needs the network
and is off by default because other people's servers fail at random.
"""
import argparse
import pathlib
import sys
import urllib.error
import urllib.parse
import urllib.request
from html.parser import HTMLParser


class Page(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.ids: set[str] = set()
        self.links: list[str] = []

    def handle_starttag(self, tag, attrs) -> None:
        values = dict(attrs)
        if values.get("id"):
            self.ids.add(values["id"])
        if tag == "a" and values.get("name"):
            self.ids.add(values["name"])
        if tag == "a" and values.get("href"):
            self.links.append(values["href"])


def parse(path: pathlib.Path) -> Page:
    page = Page()
    page.feed(path.read_text(encoding="utf-8"))
    return page


def check_external(url: str) -> str | None:
    for method in ("HEAD", "GET"):
        request = urllib.request.Request(url, method=method, headers={"User-Agent": "usbdac-docs-link-check"})
        try:
            with urllib.request.urlopen(request, timeout=20) as response:
                if response.status < 400:
                    return None
        except urllib.error.HTTPError as error:
            if method == "GET":
                return f"HTTP {error.code}"
        except (urllib.error.URLError, TimeoutError) as error:
            if method == "GET":
                return str(error)
    return "no answer"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("site", type=pathlib.Path)
    parser.add_argument("--external", action="store_true")
    args = parser.parse_args()

    pages = {path.name: parse(path) for path in sorted(args.site.glob("*.html"))}
    problems: list[str] = []
    external: dict[str, list[str]] = {}
    checked = 0
    for name, page in pages.items():
        for href in page.links:
            parts = urllib.parse.urlsplit(href)
            if parts.scheme in ("http", "https"):
                external.setdefault(href, []).append(name)
                continue
            if parts.scheme:
                continue  # mailto: and the like
            checked += 1
            target_name = parts.path or name
            target = (args.site / name).parent / target_name if parts.path else args.site / name
            target = target.resolve()
            if not target.exists():
                problems.append(f"{name}: {href}: no such file")
                continue
            if parts.fragment and target.suffix == ".html" and target.parent == args.site.resolve():
                known = pages.get(target.name)
                if known is None or parts.fragment not in known.ids:
                    problems.append(f"{name}: {href}: no anchor '{parts.fragment}' in {target.name}")
    print(f"{len(pages)} pages, {checked} relative links checked, {len(external)} distinct external links")
    if args.external:
        for url, sources in sorted(external.items()):
            failure = check_external(url)
            if failure:
                problems.append(f"{', '.join(sorted(set(sources)))}: {url}: {failure}")
    for problem in problems:
        print(problem, file=sys.stderr)
    print(f"{len(problems)} problems")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
