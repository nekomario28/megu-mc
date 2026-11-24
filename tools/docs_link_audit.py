#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Relative Markdown/HTML link and pre-release download-link audit for the docs.

The defect class this gate exists for is the reader-facing dead end: a
`[guide](docs/operator-guide.md)` whose file was renamed, an `<a href>` a
Markdown rewrite left pointing at a directory that no longer exists, or a
`.../releases/latest` download link on a repository whose newest release is a
pre-release - GitHub answers `/releases/latest` with HTTP 404 there, so every
"Downloads" button on a pre-release-only project is a broken link that no test
ever opened. Each one still renders, still passes every Rust test, and is only
discovered by a human clicking it.

The audit scans `README.md`, `README.ja.md` and every `docs/**/*.md` under
`--root` and checks:

* `relative_link` - every relative inline Markdown link `[text](target)` and
  HTML anchor `<a href="target">` must resolve to a path that exists inside the
  repository root. `#fragment` and `?query` are stripped before resolution,
  percent-escapes are decoded, a trailing `/` is a directory, and a target
  starting `/` is root-relative (GitHub's semantics). External targets
  (`http://`, `https://`, `mailto:` and any other `scheme:`), protocol-relative
  `//host` targets and anchor-only `#section` links are out of scope: they are
  counted as skipped, not as passes.
* `pre_release_latest` - any `github.com/<owner>/<repo>/releases/latest` URL.
  A pre-release is not "latest", so that endpoint 404s; the link must name
  `/releases` (all releases) or a concrete `/releases/tag/<version>`.

Fenced code blocks (``` or ~~~, any run length) and inline code spans are
blanked before analysis, with offsets preserved so reported line numbers are
the file's own. HTML comments are blanked too: a commented-out link is not a
link. A code sample showing a link therefore cannot fail the gate, and a link
hidden inside a code fence cannot pass it.

Usage:
    tools/docs_link_audit.py               # human PASS/FAIL report
    tools/docs_link_audit.py --json        # machine-readable report
    tools/docs_link_audit.py --root DIR    # hermetic fixture / another checkout

Exit status: 0 no violations, 1 violations, 2 the audit could not run (root is
not a directory).
"""

import argparse
import json
from pathlib import Path
import re
import sys
from urllib.parse import unquote

REPO_ROOT = Path(__file__).resolve().parents[1]
# The only repository whose release state this gate can reason about.
REPO_SLUG = "nekomario28/megu-mc"
# Read-mostly entry points: the files a reader lands on before anything else.
DOC_PATTERNS = ("README.md", "README.ja.md", "docs/**/*.md")

FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})")
INLINE_CODE = re.compile(r"(?<!`)(`+)(?!`)(.+?)(?<!`)\1(?!`)")
HTML_COMMENT = re.compile(r"(?s)<!--.*?-->")
HTML_HREF = re.compile(r"<a\b[^>]*?\bhref\s*=\s*(?:\"([^\"]*)\"|'([^']*)')", re.IGNORECASE)
SCHEME = re.compile(r"^[A-Za-z][A-Za-z0-9+.\-]*:")
PRE_RELEASE_URL = re.compile(re.escape(f"github.com/{REPO_SLUG}/releases/latest"))


def blank(chunk):
    """Same length, no newlines removed: offsets and line numbers stay the file's own."""
    return re.sub(r"[^\n]", " ", chunk)


def mask_code(text):
    """Blank fenced code blocks, inline code spans and HTML comments."""
    masked = []
    fence = None
    for line in text.splitlines(keepends=True):
        match = FENCE.match(line)
        if fence is None:
            if match:
                fence = match.group(1)
                masked.append(blank(line))
            else:
                masked.append(line)
            continue
        closing = (match is not None and match.group(1)[0] == fence[0]
                   and len(match.group(1)) >= len(fence))
        masked.append(blank(line))
        if closing:
            fence = None
    text = "".join(masked)
    text = INLINE_CODE.sub(lambda m: blank(m.group(0)), text)
    return HTML_COMMENT.sub(lambda m: blank(m.group(0)), text)


def link_target(raw):
    """The destination of a `](...)` payload, without its optional title."""
    raw = raw.strip()
    if not raw:
        return ""
    if raw.startswith("<"):
        end = raw.find(">")
        return raw[1:end] if end > 0 else raw[1:]
    # Markdown requires angle brackets around a destination containing spaces, so the
    # first whitespace-separated token is the whole destination and the rest is a title.
    target = re.split(r"\s+", raw, maxsplit=1)[0]
    return re.sub(r"\\([\\`*_{}\[\]()#+\-.!|])", r"\1", target)


def markdown_links(masked):
    """Yield ``(offset, target)`` for every inline `[text](target)` link."""
    position = 0
    length = len(masked)
    while True:
        marker = masked.find("](", position)
        if marker < 0:
            return
        position = marker + 2
        line_start = masked.rfind("\n", 0, marker) + 1
        # `](x)` is only a link when a `[` opens the text on the same line; a bare
        # parenthesised string in prose is not a destination to validate.
        if masked.rfind("[", line_start, marker) < 0:
            continue
        cursor = position
        depth = 1
        angled = False
        while cursor < length:
            char = masked[cursor]
            if char == "\\" and cursor + 1 < length:
                cursor += 2
                continue
            if char == "\n":
                break
            if angled:
                if char == ">":
                    angled = False
            elif char == "<":
                angled = True
            elif char == "(":
                depth += 1
            elif char == ")":
                depth -= 1
                if depth == 0:
                    break
            cursor += 1
        if depth:
            continue
        yield marker, link_target(masked[position:cursor])
        position = cursor + 1


def classify(target):
    """``relative`` for a path this gate can resolve, ``skip`` for anything external."""
    target = target.strip()
    if not target or target.startswith("#") or target.startswith("//"):
        return "skip", target
    if SCHEME.match(target):
        return "skip", target
    return "relative", target


def resolve(root, source, target, kind):
    """Return ``(violation reason or None, resolved path or None)``."""
    if kind == "skip":
        return None, None
    cleaned = target.split("#", 1)[0].split("?", 1)[0]
    cleaned = unquote(cleaned)
    if not cleaned:
        return None, None
    # A leading `/` is root-relative (GitHub's semantics); everything else is from the file.
    path = (root / cleaned.lstrip("/") if cleaned.startswith("/")
            else source.parent / cleaned).resolve()
    try:
        path.relative_to(root)
    except ValueError:
        return f"relative link escapes the repository root: {target}", path
    if not path.exists():
        return f"relative link target does not exist: {target}", path
    return None, path


def audit(root):
    """Scan every document under ``root``; return the report dictionary."""
    files = []
    for pattern in DOC_PATTERNS:
        for path in sorted(root.glob(pattern)):
            if path.is_file() and path not in files:
                files.append(path)
    violations = []
    links_checked = 0
    external = 0
    for path in files:
        relative = path.relative_to(root).as_posix()
        text = path.read_text(encoding="utf-8")
        masked = mask_code(text)
        seen = []
        for offset, target in markdown_links(masked):
            seen.append((offset, target))
        for match in HTML_HREF.finditer(masked):
            seen.append((match.start(), match.group(1) if match.group(1) is not None
                         else match.group(2)))
        for offset, target in sorted(seen, key=lambda item: item[0]):
            kind, _ = classify(target)
            if kind == "skip":
                external += 1
                continue
            links_checked += 1
            reason, _ = resolve(root, path, target, kind)
            if reason:
                violations.append({
                    "kind": "relative_link",
                    "file": relative,
                    "line": masked.count("\n", 0, offset) + 1,
                    "target": target,
                    "reason": reason,
                })
        for match in PRE_RELEASE_URL.finditer(masked):
            violations.append({
                "kind": "pre_release_latest",
                "file": relative,
                "line": masked.count("\n", 0, match.start()) + 1,
                "target": match.group(0),
                "reason": ("pre-release repository: /releases/latest answers 404 when the "
                           "newest release is a pre-release; link /releases or "
                           "/releases/tag/<version> instead"),
            })
    violations.sort(key=lambda item: (item["file"], item["line"], item["target"]))
    return {
        "root": str(root),
        "ok": not violations,
        "files": [path.relative_to(root).as_posix() for path in files],
        "links_checked": links_checked,
        "external_links": external,
        "violations": violations,
        "count": len(violations),
    }


def print_report(report):
    affected = sorted({item["file"] for item in report["violations"]})
    print(f"docs link audit: {report['root']} - {len(report['files'])} file(s), "
          f"{report['links_checked']} relative link(s), "
          f"{report['external_links']} external/anchor link(s) skipped")
    if report["violations"]:
        for file in affected:
            print(f"\nUNRESOLVED {file}:")
            for item in report["violations"]:
                if item["file"] == file:
                    print(f"  {item['file']}:{item['line']}: {item['reason']}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", default=str(REPO_ROOT),
                        help="repository root to audit (default: this tree)")
    parser.add_argument("--json", action="store_true",
                        help="machine-readable report")
    options = parser.parse_args(argv)
    root = Path(options.root).resolve()
    if not root.is_dir():
        print(f"docs link audit: --root is not a directory: {root}", file=sys.stderr)
        return 2
    report = audit(root)
    if options.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    if not report["files"]:
        # A scan that finds nothing is blind, not clean: a wrong --root must not pass.
        print(f"docs link audit: no documents under {root}; expected README.md, "
              f"README.ja.md or docs/**/*.md", file=sys.stderr)
        return 2
    if not options.json:
        print_report(report)
        if report["violations"]:
            print()
            print(f"FAIL: {report['count']} unresolved doc link(s)")
        else:
            print(f"PASS: every relative link in {len(report['files'])} doc(s) resolves and "
                  f"no pre-release /releases/latest URL remains")
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
