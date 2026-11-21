#!/usr/bin/env python3
"""Build the published documentation site from docs/agent with pandoc.

Adds to the plain per-page conversion:
  * a sidebar generated from the ordered manifest docs/agent/manifest.txt,
  * client-side search over a JSON index of headings and paragraphs,
  * a config-key reference generated from src/config/keys.rs,
  * docs/vscode-acp-setup.md published beside the other pages,
  * build-time checks that fail the build: a manifest entry without a file, a page not in the
    manifest, a config key missing from the generated reference, or a broken internal link.

Usage: build_docs_site.py --out site [--src docs/agent] [--no-pandoc]
`--no-pandoc` runs every check and writes the index files without converting pages (used by tests).
"""

from __future__ import annotations

import argparse
import html
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
GENERATED_KEYS_PAGE = "CONFIG_KEYS.md"
EXTRA_PAGES = {"vscode-acp-setup.md": ROOT / "docs/vscode-acp-setup.md"}
SITE_BASE = "https://sebahrens.github.io/mini-agent"


class BuildError(Exception):
    pass


# ---------------------------------------------------------------- config keys


def parse_config_keys(keys_rs: str) -> list[dict]:
    """Every constructor entry of CONFIG_KEYS: name, aliases, feature, description."""
    start = keys_rs.index("pub(crate) const CONFIG_KEYS")
    end = keys_rs.index("\n];", start)
    body = keys_rs[start:end]
    entries = re.split(r"(?m)^    (?=(?:key|aliased|gated|gated_aliased)\()", body)[1:]
    keys = []
    for entry in entries:
        ctor = re.match(r"(key|aliased|gated|gated_aliased)\(\s*((?:\"[^\"]*\"\s*,?\s*|&\[[^\]]*\]\s*,?\s*)+)\)", entry)
        if ctor is None:
            raise BuildError(f"cannot parse a CONFIG_KEYS entry: {entry[:80]!r}")
        kind = ctor.group(1)
        strings = re.findall(r"\"([^\"]*)\"", ctor.group(2))
        if kind == "key":
            feature, name, aliases = None, strings[0], []
        elif kind == "aliased":
            feature, name, aliases = None, strings[0], strings[1:]
        elif kind == "gated":
            feature, name, aliases = strings[0], strings[1], []
        else:
            feature, name, aliases = strings[0], strings[1], strings[2:]
        described = re.search(r"\.described\(\s*\"((?:[^\"\\]|\\.)*)\"\s*\)", entry)
        description = described.group(1).replace('\\"', '"').replace("\\n", " ") if described else ""
        keys.append(
            {
                "name": name,
                "aliases": aliases,
                "feature": feature,
                "live": ".live()" in entry,
                "description": description,
            }
        )
    expected = len(re.findall(r"(?m)^    (?:key|aliased|gated|gated_aliased)\(", body))
    if len(keys) != expected or len(keys) == 0:
        raise BuildError(f"parsed {len(keys)} config keys but keys.rs declares {expected}")
    return keys


def render_config_keys(keys: list[dict]) -> str:
    lines = [
        "---",
        'title: "Configuration key reference"',
        'description: "Every top-level configuration key mini-agent owns, generated from the source catalogue."',
        "---",
        "",
        "# Configuration key reference",
        "",
        "Generated from `src/config/keys.rs` at build time, so every top-level key is listed. Keys in",
        "tables (such as `[goal]` or `[lsp]`) are described in [CONFIG.md](CONFIG.md), which also covers",
        "files, precedence, trust and managed policy. A key marked with a feature exists only in builds with",
        "that Cargo feature; other builds keep it without acting on it.",
        "",
        "| Key | Alternative spellings | Feature | Applies | Description |",
        "| --- | --- | --- | --- | --- |",
    ]
    for key in sorted(keys, key=lambda k: k["name"]):
        aliases = ", ".join(f"`{a}`" for a in key["aliases"]) or ""
        feature = f"`{key['feature']}`" if key["feature"] else ""
        applies = "live" if key["live"] else "after restart"
        description = key["description"].replace("|", "\\|") or "See CONFIG.md."
        lines.append(f"| `{key['name']}` | {aliases} | {feature} | {applies} | {description} |")
    return "\n".join(lines) + "\n"


def check_reference_covers(keys: list[dict], page: str) -> None:
    missing = [k["name"] for k in keys if f"| `{k['name']}` |" not in page]
    if missing:
        raise BuildError(f"config keys missing from the generated reference: {missing}")


# ------------------------------------------------------------------- manifest


def parse_manifest(text: str) -> list[tuple[str, str]]:
    entries = []
    for number, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = [p.strip() for p in line.split("|")]
        if len(parts) != 2 or not parts[0] or not parts[1].endswith(".md"):
            raise BuildError(f"manifest line {number} must be 'Section | file.md': {raw!r}")
        entries.append((parts[0], parts[1]))
    seen = set()
    for _section, path in entries:
        if path in seen:
            raise BuildError(f"manifest lists {path} twice")
        seen.add(path)
    return entries


def check_manifest(entries: list[tuple[str, str]], workdir: Path) -> None:
    for _section, path in entries:
        if not (workdir / path).is_file():
            raise BuildError(f"manifest entry {path} has no file")
    listed = {path for _s, path in entries}
    for page in sorted(workdir.rglob("*.md")):
        relative = page.relative_to(workdir).as_posix()
        if relative == "index.md":
            continue
        if relative not in listed:
            raise BuildError(f"{relative} is not in docs/agent/manifest.txt; add it to a section")


# -------------------------------------------------------------- page handling

FRONT_MATTER = re.compile(r"\A---\n(.*?)\n---\n", re.S)


def split_front_matter(text: str) -> tuple[dict, str]:
    match = FRONT_MATTER.match(text)
    if not match:
        return {}, text
    meta = {}
    for line in match.group(1).splitlines():
        key, _, value = line.partition(":")
        if key.strip() and value.strip():
            meta[key.strip()] = value.strip().strip('"')
    return meta, text[match.end():]


def page_title(body: str, fallback: str) -> str:
    match = re.search(r"(?m)^# (.+)$", body)
    return match.group(1).strip() if match else fallback


REPO_BLOB = "https://github.com/sebahrens/mini-agent/blob/main/"
REPO_TREE = "https://github.com/sebahrens/mini-agent/tree/main/"
LINK = re.compile(r"\]\(([^)\s]+)\)")


def rewrite_links(text: str, page: Path, origin: Path, work: Path) -> str:
    """Point links between published pages at `.html`; point links to other repository files at GitHub.

    A relative link that resolves to neither raises BuildError, so a dead link cannot ship.
    """

    def replace(match: re.Match) -> str:
        target = match.group(1)
        if re.match(r"^([a-z]+:|//|#)", target):
            return match.group(0)
        path, sep, fragment = target.partition("#")
        if not path:
            return match.group(0)
        published = (page.parent / path).resolve()
        if published.is_file() and published.is_relative_to(work.resolve()):
            if published.suffix == ".md":
                path = path.removesuffix(".md") + ".html"
            return f"]({path}{sep}{fragment})"
        repo = (origin.parent / path).resolve()
        if repo.exists() and repo.is_relative_to(ROOT):
            base = REPO_TREE if repo.is_dir() else REPO_BLOB
            return f"]({base}{repo.relative_to(ROOT).as_posix()}{sep}{fragment})"
        raise BuildError(f"{page.relative_to(work).as_posix()} links to {target}, which exists nowhere in the repository")

    return LINK.sub(replace, text)


def anchor(heading: str) -> str:
    text = re.sub(r"[`*_]", "", heading).lower()
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
    text = re.sub(r"[^\w\- .]", "", text, flags=re.U).strip().replace(" ", "-")
    text = re.sub(r"^[^a-z]+", "", text)
    return text or "section"


def search_entries(page: str, title: str, body: str) -> list[dict]:
    entries = []
    heading, anch, buffer, in_code = title, "", [], False

    def flush() -> None:
        text = re.sub(r"\s+", " ", " ".join(buffer)).strip()
        entries.append({"p": page, "t": title, "h": heading, "a": anch, "x": text[:300]})
        buffer.clear()

    for line in body.splitlines():
        if line.startswith("```"):
            in_code = not in_code
            continue
        if not in_code and (m := re.match(r"^(#{1,4}) +(.+)$", line)):
            flush()
            heading, anch = m.group(2).strip(), anchor(m.group(2))
            continue
        if len(" ".join(buffer)) < 600:
            buffer.append(re.sub(r"[`*|]", " ", line))
    flush()
    return [e for e in entries if e["x"] or e["h"]]


def nav_html(sections: list[tuple[str, list[tuple[str, str]]]], current: str, root: str) -> str:
    parts = ['<aside class="sidebar"><div class="search"><input id="q" type="search" placeholder="Search docs" aria-label="Search docs" autocomplete="off"><ul id="results" hidden></ul></div><nav class="toc">']
    for section, pages in sections:
        parts.append(f"<h2>{html.escape(section)}</h2><ul>")
        for href, title in pages:
            cls = ' class="current"' if href == current else ""
            parts.append(f'<li><a{cls} href="{root}{html.escape(href)}">{html.escape(title)}</a></li>')
        parts.append("</ul>")
    parts.append("</nav></aside>")
    return "".join(parts)


def index_markdown(sections: list[tuple[str, list[tuple[str, str]]]]) -> str:
    lines = ["# mini-agent documentation", "", "Use the search box in the sidebar, or browse by section.", ""]
    for section, pages in sections:
        lines += [f"## {section}", ""]
        lines += [f"- [{title}]({href.removesuffix('.html')}.md)" for href, title in pages]
        lines.append("")
    return "\n".join(lines)


# ------------------------------------------------------------------ link check


def check_links(site: Path) -> None:
    broken = []
    for page in site.rglob("*.html"):
        for href in re.findall(r'href="([^"#?]+)', page.read_text(encoding="utf-8")):
            if re.match(r"^[a-z]+:|^//|^\./?$|^\.\./?$", href) or href.endswith("/"):
                continue
            target = (page.parent / href).resolve()
            if not target.exists():
                broken.append(f"{page.relative_to(site)} -> {href}")
    if broken:
        raise BuildError("broken internal links:\n  " + "\n  ".join(sorted(set(broken))[:50]))


# ----------------------------------------------------------------------- build


def build(src: Path, out: Path, pandoc: str | None, keys_rs: Path) -> None:
    work = out / ".work"
    if out.exists():
        shutil.rmtree(out)
    work.mkdir(parents=True)
    origins: dict[str, Path] = {}
    for md in src.rglob("*.md"):
        target = work / md.relative_to(src)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(md, target)
        origins[md.relative_to(src).as_posix()] = md
    for name, source in EXTRA_PAGES.items():
        shutil.copy(source, work / name)
        origins[name] = source
    origins[GENERATED_KEYS_PAGE] = src / GENERATED_KEYS_PAGE
    origins["index.md"] = src / "index.md"
    keys = parse_config_keys(keys_rs.read_text(encoding="utf-8"))
    reference = render_config_keys(keys)
    check_reference_covers(keys, reference)
    (work / GENERATED_KEYS_PAGE).write_text(reference, encoding="utf-8")

    entries = parse_manifest((src / "manifest.txt").read_text(encoding="utf-8"))
    check_manifest(entries, work)

    pages: dict[str, tuple[dict, str, str]] = {}
    for _section, path in entries:
        text = (work / path).read_text(encoding="utf-8")
        meta, body = split_front_matter(text)
        title = meta.get("title") or page_title(body, Path(path).stem)
        pages[path] = (meta, body, title)

    sections: list[tuple[str, list[tuple[str, str]]]] = []
    for section, path in entries:
        link = path.removesuffix(".md") + ".html"
        if not sections or sections[-1][0] != section:
            sections.append((section, []))
        sections[-1][1].append((link, pages[path][2]))

    index = []
    for path, (_meta, body, title) in pages.items():
        index += search_entries(path.removesuffix(".md") + ".html", title, rewrite_links(body, work / path, origins[path], work))
    out.mkdir(parents=True, exist_ok=True)
    (out / "search-index.json").write_text(json.dumps(index, separators=(",", ":")), encoding="utf-8")
    (work / "index.md").write_text(index_markdown(sections), encoding="utf-8")
    if pandoc is None:
        shutil.rmtree(work)
        return

    template = src / "template.html"
    if not template.is_file():
        raise BuildError("template.html is missing")
    for md in sorted(work.rglob("*.md")):
        relative = md.relative_to(work).as_posix()
        name = relative.removesuffix(".md")
        root = "../" * relative.count("/")
        if relative == "index.md":
            title = "mini-agent documentation"
        else:
            title = pages[relative][2]
        nav_file = work / "nav.html"
        nav_file.write_text(nav_html(sections, name + ".html", root), encoding="utf-8")
        target = out / f"{name}.html"
        target.parent.mkdir(parents=True, exist_ok=True)
        text = rewrite_links(md.read_text(encoding="utf-8"), md, origins[relative], work)
        command = [
            pandoc, "-f", "markdown", "--template", str(template), "--wrap=none",
            "--include-before-body", str(nav_file),
            "--metadata", f"title={title}",
            "--metadata", f"pagetitle={title} — mini-agent",
            "--metadata", f"name={name}",
            "--variable", f"root={root}",
            "-o", str(target),
        ]
        result = subprocess.run(command, input=text, text=True, capture_output=True)
        if result.returncode != 0:
            raise BuildError(f"pandoc failed for {relative}: {result.stderr.strip()}")
    shutil.rmtree(work)
    check_links(out)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--src", type=Path, default=ROOT / "docs/agent")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--keys", type=Path, default=ROOT / "src/config/keys.rs")
    parser.add_argument("--pandoc", default="pandoc")
    parser.add_argument("--no-pandoc", action="store_true")
    args = parser.parse_args()
    try:
        build(args.src.resolve(), args.out.resolve(), None if args.no_pandoc else args.pandoc, args.keys)
    except BuildError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"built {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
