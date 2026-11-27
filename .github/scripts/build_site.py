#!/usr/bin/env python3
"""Assembles the documentation site.

The SDK guide (Markdown files in docs/sdk) and the native API document are rendered to HTML,
and Dokka's HTML output is copied under api/. The result is a static site with relative links
only, so it works under any Pages base path. The look follows the usbdac brand system in
branding/ (tokens, fonts, icons); docs/site holds the stylesheet, a small script and the landing
section of index.html. Every page reads fine without the script: it adds the theme switch,
search, copy buttons and the "On this page" highlight.

    python build_site.py --guide docs/sdk --native-api docs/API.md \
        --dokka android/usbdac/build/dokka/html \
        --media3-dokka android/usbdac-media3/build/dokka/html --output _site

Pages can set explicit anchors with `{#id}` after a heading (the attr_list extension); the
decision pages and the reference list use that so links to them stay stable.
"""
import argparse
import html
import json
import pathlib
import re
import shutil
import sys

import markdown

ROOT = pathlib.Path(__file__).resolve().parents[2]

# Navigation sections: (section title, [(file name without extension, navigation title), ...]).
# The order is the navigation order. Every name is a Markdown file in the guide directory.
SECTIONS = [
    ("Start", [
        ("index", "Overview"),
        ("getting-started", "Quickstart"),
        ("improvements", "What usbdac improves"),
    ]),
    ("Integrate", [
        ("guide-kotlin", "Kotlin guide"),
        ("guide-rust", "Rust guide"),
        ("integration", "Integration guide"),
        ("integration-player", "The player engine"),
        ("integration-app", "App, service and settings"),
        ("automatic-output", "Automatic output"),
        ("media3", "Media3 integration"),
        ("support-diagnostics", "Collect a support report"),
        ("migration", "Upgrading from 0.1.10"),
    ]),
    ("Use", [
        ("controls", "Controls and configuration"),
        ("bit-perfect", "Bit-perfect output"),
        ("transports", "Transports"),
        ("troubleshooting", "Troubleshooting"),
    ]),
    ("How it works", [
        ("architecture", "Architecture"),
        ("usb-audio-primer", "USB audio in a page"),
    ]),
    ("Design decisions", [
        ("decisions", "All decisions at a glance"),
        ("decisions-stream", "Streaming and buffering"),
        ("decisions-clock", "Clock and pacing"),
        ("decisions-volume", "Volume"),
        ("decisions-reporting", "Reporting and conversion"),
        ("decisions-dsd-conversion", "DSD conversion"),
        ("decisions-library", "Outputs and library shape"),
    ]),
    ("Evidence", [
        ("evidence", "Evidence and status"),
        ("status", "Capability and result ledger"),
        ("source-backed-quirks", "Source-backed device compatibility"),
        ("drivers", "The three drivers studied"),
        ("references", "Public references"),
    ]),
]
NATIVE_API_PAGE = ("native-api", "Native API (Rust and JNI)")
REFERENCE_SECTION = ("Reference", [NATIVE_API_PAGE])
ALL_SECTIONS = SECTIONS + [REFERENCE_SECTION]
GUIDE_PAGES = [page for _, pages in SECTIONS for page in pages]
SOURCE_DATA_FILES = ("alsa-quirk-flags-v7.3.json", "alsa-descriptor-quirks-v7.3.json")

REPOSITORY = "https://github.com/johnneerdael/android-usbdac"
SITE_URL = "https://johnneerdael.github.io/android-usbdac/"
TAGLINE = "Direct USB audio. Clear evidence."
DEFAULT_DESCRIPTION = ("usbdac sends PCM, DoP or native DSD straight to a USB DAC from Android and Rust apps, "
                       "and reports whether the stream is bit-perfect. SDK guide, design decisions and API reference.")

# Top navigation: (label, target, page name it marks as current or None).
TOP_NAV = [
    ("Quickstart", "getting-started.html", "getting-started"),
    ("Kotlin", "guide-kotlin.html", "guide-kotlin"),
    ("Rust", "guide-rust.html", "guide-rust"),
    ("Decisions", "decisions.html", "decisions"),
    ("Evidence", "evidence.html", "evidence"),
    ("API reference", "api/index.html", None),
]

# The usbdac mark (branding/logos/usbdac-mark-*.svg); its colours come from the stylesheet.
MARK_PATHS = (
    '<path class="b" d="M920 60L710 183Q700 190 700 202V390Q700 403 686 411L528 503L746 630L878 554Q920 530 920 480Z"/>'
    '<path class="b" d="M80 601L470 826L318 914Q292 929 266 914L126 832Q80 805 80 752Z"/>'
    '<path class="a" d="M80 60L290 183Q300 190 300 202V390Q300 403 314 411L746 662Q760 670 773 662L920 578V822Q920 846 898 859L800 916Q774 931 752 918L126 556Q80 530 80 480Z"/>'
)


def mark(label: str = "") -> str:
    accessible = f'role="img" aria-label="{label}"' if label else 'aria-hidden="true"'
    return f'<svg class="mark" viewBox="0 0 1000 1000" {accessible}>{MARK_PATHS}</svg>'


ICONS = {
    "github": '<svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M12 .5a11.5 11.5 0 0 0-3.64 22.41c.58.1.79-.25.79-.56v-2c-3.2.7-3.88-1.37-3.88-1.37-.52-1.33-1.28-1.69-1.28-1.69-1.05-.71.08-.7.08-.7 1.16.08 1.77 1.2 1.77 1.2 1.03 1.77 2.7 1.26 3.36.96.1-.75.4-1.26.73-1.55-2.55-.29-5.24-1.28-5.24-5.69 0-1.26.45-2.29 1.19-3.1-.12-.29-.52-1.46.11-3.05 0 0 .97-.31 3.17 1.18a11 11 0 0 1 5.77 0c2.2-1.49 3.17-1.18 3.17-1.18.63 1.59.23 2.76.11 3.05.74.81 1.19 1.84 1.19 3.1 0 4.42-2.69 5.39-5.26 5.68.41.36.78 1.06.78 2.14v3.17c0 .31.21.67.8.56A11.5 11.5 0 0 0 12 .5Z"/></svg>',
    "theme": ('<svg class="i-auto" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><circle cx="12" cy="12" r="8"/><path d="M12 4a8 8 0 0 1 0 16Z" fill="currentColor"/></svg>'
              '<svg class="i-moon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round" aria-hidden="true"><path d="M20 14.5A8 8 0 0 1 9.5 4a8 8 0 1 0 10.5 10.5Z"/></svg>'
              '<svg class="i-sun" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/></svg>'),
    "search": '<svg class="glass" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/></svg>',
}

TEMPLATE = """<!doctype html>
<html lang="en" class="no-js">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<meta name="description" content="{description}">
<meta name="theme-color" content="#11151C" media="(prefers-color-scheme: dark)">
<meta name="theme-color" content="#F2F5FA" media="(prefers-color-scheme: light)">
<meta property="og:type" content="website">
<meta property="og:site_name" content="usbdac">
<meta property="og:title" content="{title}">
<meta property="og:description" content="{description}">
<meta property="og:url" content="{site_url}{page}.html">
<meta property="og:image" content="{site_url}assets/usbdac-social-dark.png">
<meta property="og:image:width" content="1200">
<meta property="og:image:height" content="630">
<meta name="twitter:card" content="summary_large_image">
<link rel="icon" href="assets/favicon.svg" type="image/svg+xml">
<link rel="icon" href="favicon.ico" sizes="any">
<link rel="apple-touch-icon" href="assets/apple-touch-icon.png">
<link rel="manifest" href="site.webmanifest">
<link rel="preload" href="assets/fonts/Manrope.woff" as="font" type="font/woff" crossorigin>
<link rel="stylesheet" href="assets/site.css?v={version}">
<script>try{{var t=localStorage.getItem("usbdac-theme");if(t==="dark"||t==="light")document.documentElement.setAttribute("data-theme",t)}}catch(e){{}}</script>
<script src="assets/site.js?v={version}" defer></script>
</head>
<body>
<a class="skip" href="#content">Skip to content</a>
<header class="site-header">
<div class="header-inner">
<a class="brand" href="index.html" aria-label="usbdac home">{mark}<span class="word">usbdac</span><span class="version">{version}</span></a>
<nav class="top-nav" aria-label="Main">{top_nav}</nav>
<div class="header-tools">
<div class="search" role="search">{search_icon}<input id="search-input" data-index="assets/search-index.js?v={version}" type="search" placeholder="Search the guide" aria-label="Search the guide" autocomplete="off" aria-controls="search-results" aria-expanded="false"><kbd>/</kbd><div id="search-results" class="search-results" role="listbox"></div></div>
<button class="icon-btn theme-btn" type="button" aria-label="Change theme">{theme_icon}</button>
<a class="icon-btn" href="{repository}" aria-label="usbdac on GitHub">{github_icon}</a>
</div>
</div>
</header>
{home}
<div class="layout">
<aside class="sidebar">
<details class="menu" open><summary>Guide menu</summary>
<nav aria-label="Guide">
{nav}
</nav>
</details>
</aside>
<main class="doc" id="content">
<div class="doc-meta"><p class="eyebrow">{section}</p><a href="{repository}/blob/main/{source}">Edit on GitHub</a></div>
<article>
{body}
</article>
{pager}
</main>
{toc}
</div>
<footer class="site-footer">
<div class="footer-inner">
<div><a class="brand" href="index.html">{mark}<span class="word">usbdac</span></a><p>{tagline}<br>MIT licensed. Copyright (c) 2026 Flick Player Contributors and usbdac contributors.</p></div>
<div><h4>Guide</h4><ul><li><a href="getting-started.html">Quickstart</a></li><li><a href="guide-kotlin.html">Kotlin guide</a></li><li><a href="guide-rust.html">Rust guide</a></li><li><a href="troubleshooting.html">Troubleshooting</a></li></ul></div>
<div><h4>Reference</h4><ul><li><a href="api/index.html">Core API (Dokka)</a></li><li><a href="media3-api/index.html">Media3 API (Dokka)</a></li><li><a href="native-api.html">Native API and JNI</a></li><li><a href="https://docs.rs/usbdac-core">usbdac-core on docs.rs</a></li></ul></div>
<div><h4>Project</h4><ul><li><a href="{repository}/releases">Releases</a></li><li><a href="{repository}">Source on GitHub</a></li><li><a href="https://crates.io/crates/usbdac-core">crates.io</a></li><li><a href="{repository}/blob/main/LICENSE">License</a> · <a href="{repository}/blob/main/NOTICE">Notice</a></li></ul></div>
</div>
</footer>
</body>
</html>
"""


def rewrite_links(source: str, pages: dict) -> str:
    """Points links to the Markdown files at the rendered pages."""

    def replace(match: re.Match) -> str:
        target, anchor = match.group(1), match.group(2) or ""
        name = pathlib.PurePosixPath(target).name
        return f"]({pages[name]}.html{anchor})" if name in pages else match.group(0)

    for filename in SOURCE_DATA_FILES:
        source = source.replace(f"](../{filename})", f"]({filename})")
    return re.sub(r"\]\(([^)#\s]+\.md)(#[^)\s]*)?\)", replace, source)


def strip_tags(fragment: str) -> str:
    return re.sub(r"\s+", " ", html.unescape(re.sub(r"<[^>]+>", " ", fragment))).strip()


def render(source: str, pages: dict):
    """Returns (HTML, [(level, id, text)] for the second- and third-level headings)."""
    converter = markdown.Markdown(
        extensions=["fenced_code", "codehilite", "tables", "toc", "sane_lists", "attr_list"],
        extension_configs={
            "codehilite": {"css_class": "hl", "guess_lang": False},
            "toc": {"permalink": "#", "permalink_class": "anchor", "permalink_title": "Link to this section"},
        },
        output_format="html",
    )
    # The guide's Gradle snippets are Kotlin script; Pygments has no "gradle" lexer.
    source = re.sub(r"^```gradle$", "```kotlin", source, flags=re.M)
    body = converter.convert(rewrite_links(source, pages))
    body = re.sub(r"<table>", '<div class="table-wrap"><table>', body)
    body = re.sub(r"</table>", "</table></div>", body)
    headings = []

    def walk(tokens):
        for token in tokens:
            if token["level"] in (2, 3):
                headings.append((token["level"], token["id"], strip_tags(token["name"])))
            walk(token["children"])

    walk(converter.toc_tokens)
    return body, headings


def first_paragraph(body: str) -> str:
    match = re.search(r"<p>(.*?)</p>", body, re.S)
    text = strip_tags(match.group(1)) if match else ""
    return (text[:157].rsplit(" ", 1)[0] + "...") if len(text) > 160 else text


def search_entries(name: str, title: str, section: str, body: str, headings) -> list:
    """One entry for the page and one per h2/h3 section, with the start of its text."""
    entries = [{"t": title, "p": title, "s": section, "u": f"{name}.html", "x": first_paragraph(body), "l": 1}]
    positions = [(m.start(), m.end(), m.group(1)) for m in re.finditer(r'<h[1-4][^>]*\sid="([^"]+)"[^>]*>', body)]
    texts = {}
    for i, (_, end, anchor) in enumerate(positions):
        stop = positions[i + 1][0] if i + 1 < len(positions) else len(body)
        section_html = body[end:stop]
        section_html = re.sub(r"^.*?</h[1-4]>", "", section_html, count=1, flags=re.S)
        texts[anchor] = strip_tags(section_html)
    for level, anchor, text in headings:
        full = texts.get(anchor, "")
        # x: the start of the section, shown under the result; b: the rest, searched but not shown.
        entries.append({"t": text, "p": title, "s": section, "u": f"{name}.html#{anchor}", "x": full[:200], "b": full[200:1600].lower(), "l": level})
    return entries


def nav_html(current: str, headings) -> str:
    out = []
    for section, items in ALL_SECTIONS:
        out.append(f'<div class="group">{html.escape(section)}</div>')
        for name, title in items:
            mark_current = ' aria-current="page"' if name == current else ""
            out.append(f'<a href="{name}.html"{mark_current}>{html.escape(title)}</a>')
            if name == current:
                level_two = [(anchor, text) for level, anchor, text in headings if level == 2]
                if level_two:
                    out.append('<div class="page-toc">')
                    out.extend(f'<a href="#{anchor}">{html.escape(text)}</a>' for anchor, text in level_two)
                    out.append("</div>")
    out.append('<div class="group">API reference</div>')
    out.append('<a href="api/index.html">Core API (Dokka)</a>')
    out.append('<a href="media3-api/index.html">Media3 API (Dokka)</a>')
    out.append('<a class="ext" href="https://docs.rs/usbdac-core">Rust API on docs.rs</a>')
    return "\n".join(out)


def toc_html(headings) -> str:
    level_two = [(anchor, text) for level, anchor, text in headings if level == 2]
    if not level_two:
        return '<aside class="toc" aria-hidden="true"></aside>'
    links = "\n".join(f'<a href="#{anchor}">{html.escape(text)}</a>' for anchor, text in level_two)
    return f'<aside class="toc" aria-label="On this page"><p class="label">On this page</p>\n{links}\n</aside>'


def pager_html(order, index: int) -> str:
    parts = []
    if index > 0:
        name, title = order[index - 1]
        parts.append(f'<a class="prev" href="{name}.html"><small>Previous</small><strong>{html.escape(title)}</strong></a>')
    if index + 1 < len(order):
        name, title = order[index + 1]
        parts.append(f'<a class="next" href="{name}.html"><small>Next</small><strong>{html.escape(title)}</strong></a>')
    return f'<nav class="pager" aria-label="Previous and next page">{"".join(parts)}</nav>' if parts else ""


def top_nav_html(current: str) -> str:
    links = []
    for label, target, name in TOP_NAV:
        mark_current = ' aria-current="page"' if name == current else ""
        links.append(f'<a href="{target}"{mark_current}>{label}</a>')
    links.append(f'<a href="{REPOSITORY}/releases">Releases</a>')
    return "".join(links)


def workspace_version() -> str:
    cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'^\[workspace\.package\][^\[]*?^version = "([^"]+)"', cargo, re.M | re.S)
    return match.group(1) if match else ""


def copy_assets(site_assets: pathlib.Path, brand: pathlib.Path, output: pathlib.Path, search_index: list) -> None:
    assets = output / "assets"
    (assets / "fonts").mkdir(parents=True)
    for name in ("site.css", "site.js"):
        shutil.copyfile(site_assets / name, assets / name)
    for name in ("Manrope.woff", "IBMPlexMono-Regular.woff", "manrope-OFL.txt", "ibmplexmono-OFL.txt"):
        shutil.copyfile(brand / "fonts" / name, assets / "fonts" / name)
    web = brand / "web"
    for name in ("favicon.svg", "apple-touch-icon.png", "icon-32.png", "icon-192.png", "icon-512.png", "maskable-192.png", "maskable-512.png"):
        shutil.copyfile(web / name, assets / name)
    shutil.copyfile(web / "favicon.ico", output / "favicon.ico")
    shutil.copyfile(brand / "marketing" / "usbdac-social-dark.png", assets / "usbdac-social-dark.png")
    manifest = json.loads((web / "site.webmanifest").read_text(encoding="utf-8"))
    manifest["start_url"] = "./"
    for icon in manifest["icons"]:
        icon["src"] = "assets/" + icon["src"]
    (output / "site.webmanifest").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    (assets / "search-index.js").write_text(
        "window.USBDAC_SEARCH=" + json.dumps(search_index, ensure_ascii=False, separators=(",", ":")) + ";\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--guide", required=True, type=pathlib.Path, help="directory with the guide's Markdown files")
    parser.add_argument("--native-api", required=True, type=pathlib.Path, help="docs/API.md")
    parser.add_argument("--dokka", required=True, type=pathlib.Path, help="Dokka's HTML output directory")
    parser.add_argument("--media3-dokka", required=True, type=pathlib.Path, help="Media3 Dokka HTML output directory")
    parser.add_argument("--site-assets", type=pathlib.Path, default=ROOT / "docs" / "site", help="stylesheet, script and landing section")
    parser.add_argument("--brand", type=pathlib.Path, default=ROOT / "branding", help="the brand kit (fonts, icons, artwork)")
    parser.add_argument("--output", required=True, type=pathlib.Path)
    args = parser.parse_args()

    if not (args.dokka / "index.html").is_file():
        print(f"no Dokka output at {args.dokka}", file=sys.stderr)
        return 1

    if not (args.media3_dokka / "index.html").is_file():
        print(f"no Media3 Dokka output at {args.media3_dokka}", file=sys.stderr)
        return 1

    pages = {f"{name}.md": name for name, _ in GUIDE_PAGES}
    pages["API.md"] = NATIVE_API_PAGE[0]
    version = workspace_version()

    if args.output.exists():
        shutil.rmtree(args.output)
    args.output.mkdir(parents=True)

    sources = [(name, title, args.guide / f"{name}.md", f"docs/sdk/{name}.md") for name, title in GUIDE_PAGES]
    sources.append((*NATIVE_API_PAGE, args.native_api, "docs/API.md"))
    section_of = {name: section for section, items in ALL_SECTIONS for name, _ in items}
    order = [(name, title) for name, title, _, _ in sources]
    home = (args.site_assets / "home.html").read_text(encoding="utf-8")
    home = home.replace("@@VERSION@@", version).replace("@@REPOSITORY@@", REPOSITORY).replace("@@MARK@@", mark())
    home = re.sub(r"<!--.*?-->\n?", "", home, count=1, flags=re.S)

    search_index = []
    for position, (name, title, path, repo_path) in enumerate(sources):
        if not path.is_file():
            print(f"missing {path}", file=sys.stderr)
            return 1
        body, headings = render(path.read_text(encoding="utf-8"), pages)
        section = section_of[name]
        search_index.extend(search_entries(name, title, section, body, headings))
        is_home = name == "index"
        page = TEMPLATE.format(
            title=html.escape(f"usbdac: {TAGLINE}" if is_home else f"{title} · usbdac"),
            description=html.escape(DEFAULT_DESCRIPTION if is_home else (first_paragraph(body) or DEFAULT_DESCRIPTION)),
            site_url=SITE_URL,
            page=name,
            version=html.escape(version),
            mark=mark(),
            top_nav=top_nav_html(name),
            search_icon=ICONS["search"],
            theme_icon=ICONS["theme"],
            github_icon=ICONS["github"],
            home=home if is_home else "",
            nav=nav_html(name, headings),
            section=html.escape(section),
            body=body,
            pager=pager_html(order, position),
            toc=toc_html(headings),
            tagline=TAGLINE,
            repository=REPOSITORY,
            source=repo_path,
        )
        (args.output / f"{name}.html").write_text(page, encoding="utf-8")

    for filename in SOURCE_DATA_FILES:
        shutil.copyfile(args.guide.parent / filename, args.output / filename)
    copy_assets(args.site_assets, args.brand, args.output, search_index)
    shutil.copytree(args.dokka, args.output / "api")
    shutil.copytree(args.media3_dokka, args.output / "media3-api")
    (args.output / ".nojekyll").write_text("", encoding="utf-8")
    print(f"site written to {args.output} ({len(sources)} pages, {len(search_index)} search entries and the API reference)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
