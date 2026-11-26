#!/usr/bin/env python3
"""Assembles the documentation site.

The SDK guide (Markdown files in docs/sdk) and the native API document are rendered to HTML,
and Dokka's HTML output is copied under api/. The result is a plain static site: relative
links only, no JavaScript of its own, so it works under any Pages base path.

    python build_site.py --guide docs/sdk --native-api docs/API.md \
        --dokka android/usbdac/build/dokka/html --output _site
"""
import argparse
import html
import pathlib
import re
import shutil
import sys

import markdown

# (file name without extension, navigation title). The order is the navigation order.
GUIDE_PAGES = [
    ("index", "Overview"),
    ("improvements", "What usbdac improves"),
    ("getting-started", "Getting started"),
    ("controls", "Controls and configuration"),
    ("bit-perfect", "Bit-perfect output"),
    ("transports", "Transports"),
    ("media3", "Media3 integration"),
    ("troubleshooting", "Troubleshooting"),
]
NATIVE_API_PAGE = ("native-api", "Native API (Rust and JNI)")

TEMPLATE = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - usbdac</title>
<style>
:root {{ --bg: #ffffff; --fg: #1d2127; --muted: #5c6670; --line: #d9dee3; --accent: #0b5cad; --code-bg: #f3f5f7; }}
@media (prefers-color-scheme: dark) {{
  :root {{ --bg: #14171a; --fg: #e3e7eb; --muted: #9aa5b0; --line: #2b3238; --accent: #6db3f2; --code-bg: #1d2227; }}
}}
* {{ box-sizing: border-box; }}
body {{ margin: 0; background: var(--bg); color: var(--fg); font: 16px/1.6 system-ui, sans-serif; }}
.layout {{ display: flex; flex-wrap: wrap; max-width: 1100px; margin: 0 auto; }}
nav {{ flex: 0 0 15rem; padding: 1.5rem 1rem; border-right: 1px solid var(--line); }}
nav a {{ display: block; padding: .25rem 0; color: var(--fg); text-decoration: none; }}
nav a[aria-current] {{ color: var(--accent); font-weight: 600; }}
nav .brand {{ font-weight: 700; font-size: 1.15rem; margin-bottom: .75rem; }}
main {{ flex: 1 1 30rem; min-width: 0; padding: 1.5rem 1.5rem 4rem; }}
a {{ color: var(--accent); }}
pre {{ background: var(--code-bg); padding: .85rem 1rem; overflow-x: auto; border-radius: 6px; font-size: .9rem; line-height: 1.45; }}
code {{ background: var(--code-bg); padding: .1rem .3rem; border-radius: 4px; font-size: .92em; }}
pre code {{ background: none; padding: 0; }}
table {{ border-collapse: collapse; display: block; overflow-x: auto; }}
th, td {{ border: 1px solid var(--line); padding: .4rem .6rem; text-align: left; vertical-align: top; }}
th {{ background: var(--code-bg); }}
h1, h2, h3 {{ line-height: 1.25; }}
h2 {{ margin-top: 2.2rem; border-bottom: 1px solid var(--line); padding-bottom: .2rem; }}
@media (max-width: 700px) {{ nav {{ flex-basis: 100%; border-right: 0; border-bottom: 1px solid var(--line); }} main {{ padding: 1rem 16px 3rem; }} }}
</style>
</head>
<body>
<div class="layout">
<nav aria-label="Guide">
<div class="brand">usbdac</div>
{nav}
</nav>
<main>
{body}
</main>
</div>
</body>
</html>
"""


def rewrite_links(source: str, pages: dict) -> str:
    """Points links to the Markdown files at the rendered pages."""

    def replace(match: re.Match) -> str:
        target, anchor = match.group(1), match.group(2) or ""
        name = pathlib.PurePosixPath(target).name
        return f"]({pages[name]}.html{anchor})" if name in pages else match.group(0)

    return re.sub(r"\]\(([^)#\s]+\.md)(#[^)\s]*)?\)", replace, source)


def render(source: str, pages: dict) -> str:
    return markdown.markdown(
        rewrite_links(source, pages),
        extensions=["fenced_code", "tables", "toc", "sane_lists"],
        output_format="html",
    )


def nav_html(current: str) -> str:
    items = GUIDE_PAGES + [NATIVE_API_PAGE]
    links = []
    for name, title in items:
        mark = ' aria-current="page"' if name == current else ""
        links.append(f'<a href="{name}.html"{mark}>{html.escape(title)}</a>')
    links.append('<a href="api/index.html">API reference</a>')
    return "\n".join(links)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--guide", required=True, type=pathlib.Path, help="directory with the guide's Markdown files")
    parser.add_argument("--native-api", required=True, type=pathlib.Path, help="docs/API.md")
    parser.add_argument("--dokka", required=True, type=pathlib.Path, help="Dokka's HTML output directory")
    parser.add_argument("--output", required=True, type=pathlib.Path)
    args = parser.parse_args()

    if not (args.dokka / "index.html").is_file():
        print(f"no Dokka output at {args.dokka}", file=sys.stderr)
        return 1

    pages = {f"{name}.md": name for name, _ in GUIDE_PAGES}
    pages["API.md"] = NATIVE_API_PAGE[0]

    if args.output.exists():
        shutil.rmtree(args.output)
    args.output.mkdir(parents=True)

    sources = [(name, title, args.guide / f"{name}.md") for name, title in GUIDE_PAGES]
    sources.append((*NATIVE_API_PAGE, args.native_api))
    for name, title, path in sources:
        if not path.is_file():
            print(f"missing {path}", file=sys.stderr)
            return 1
        page = TEMPLATE.format(title=html.escape(title), nav=nav_html(name), body=render(path.read_text(encoding="utf-8"), pages))
        (args.output / f"{name}.html").write_text(page, encoding="utf-8")

    shutil.copytree(args.dokka, args.output / "api")
    (args.output / ".nojekyll").write_text("", encoding="utf-8")
    print(f"site written to {args.output} ({len(sources)} pages and the API reference)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
