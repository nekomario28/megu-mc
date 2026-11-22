#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
"""Offline verification for the zero-build landing page in website/.

  python3 website/check_site.py

Checks, all fail-closed:

  1. Every local asset index.html references (<script src>, <link href>,
     media src) exists under website/, and no script/stylesheet/media tag
     pulls from an external http(s):// or protocol-relative CDN. style.css
     must contain no @import or external url() either.
  2. BUILTIN_SUPERSEDED_PERF_MODS in website/app.js is the exact same set
     as the authoritative catalog in mod-adapter-core/src/fallback.rs.
  3. None of the removed-asset file names guarded by
     tools/provenance_check.py appear anywhere under website/ (they are
     assembled below without writing them contiguously, so this script
     does not flag itself).
  4. website/site-config.js declares every required SITE_CONFIG key.
  5. When `node` is on PATH, a unit harness exercises the modpack checker,
     bounded ZIP extraction, and the feedback report builder, client-side
     redaction, 4 KiB cap, and pre-filled GitHub fallback links.
  6. When `node` is on PATH, website/test_functions.mjs exercises the
     Cloudflare Pages Functions in functions/api/ (payload validation,
     redaction, milestone comments, KV dedup, Turnstile, /api/latest)
     against fully mocked GitHub/KV/fetch — no network access.
  7. SEO surface: <link rel="canonical">, robots.txt, sitemap.xml,
     Open Graph / Twitter Card meta, og-card.svg, and a parseable
     schema.org SoftwareApplication JSON-LD block.

Exit status: 0 clean, 1 findings.
"""

import html.parser
import json
import re
import shutil
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
from pathlib import Path

WEBSITE = Path(__file__).resolve().parent
ROOT = WEBSITE.parent

REQUIRED_FILES = ("index.html", "style.css", "site-config.js", "app.js")

REQUIRED_CONFIG_KEYS = (
    "projectName",
    "cliPrefix",
    "tagline",
    "statusBadge",
    "mcTargets",
    "githubUrl",
    "releasesUrl",
    "docsBaseUrl",
    "customDomain",
)

# Removed-asset names that must never reappear anywhere in website/. Built
# piecewise so this scanner does not trip over its own source.
FORBIDDEN_NAMES = (
    "p" "blocks" ".json",
    "prop" "erties" ".json",
    "resource" "pack" ".zip",
)

EXTERNAL_REF = re.compile(r"^(?:https?:)?//", re.IGNORECASE)
CSS_EXTERNAL = re.compile(
    r"@import|url\(\s*['\"]?\s*(?:https?:)?//", re.IGNORECASE)


class AssetCollector(html.parser.HTMLParser):
    """Collect (tag, attr, value) for elements that EMBED resources —
    scripts, stylesheets, media, objects. Plain <a href> anchors navigate
    rather than embed, so external links in prose stay allowed."""

    EMBED_ATTRS = {
        "script": {"src"},
        "link": {"href"},
        "img": {"src"},
        "source": {"src", "srcset"},
        "audio": {"src"},
        "video": {"src", "poster"},
        "iframe": {"src"},
        "embed": {"src"},
        "object": {"data"},
    }

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.refs = []  # [(tag, attr, value)]

    def handle_starttag(self, tag, attrs):
        wanted = self.EMBED_ATTRS.get(tag)
        if not wanted:
            return
        if tag == "link":
            # <link rel="canonical"> names the canonical origin URL; it is
            # metadata, not an embedded resource, so an absolute https://
            # href is expected and allowed.
            rel = next((v for n, v in attrs if n == "rel"), "")
            if "canonical" in rel.split():
                return
        for name, value in attrs:
            if value and name in wanted:
                self.refs.append((tag, name, value))

    handle_startendtag = handle_starttag


def check_local_assets():
    """Check 1: local files exist; zero external/CDN dependencies."""
    findings = []
    index = WEBSITE / "index.html"
    if not index.is_file():
        return ["index.html is missing"]

    for name in REQUIRED_FILES:
        if not (WEBSITE / name).is_file():
            findings.append(f"required file missing: website/{name}")

    collector = AssetCollector()
    collector.feed(index.read_text(encoding="utf-8"))
    for tag, attr, value in collector.refs:
        # srcset carries comma-separated "url descriptor" candidates.
        candidates = (
            [c.strip().split(" ")[0] for c in value.split(",") if c.strip()]
            if attr == "srcset" else [value])
        for ref in candidates:
            if ref.startswith(("#", "data:", "mailto:", "tel:", "javascript:")):
                continue
            if EXTERNAL_REF.match(ref):
                findings.append(
                    f"external dependency <{tag} {attr}=\"{value}\"> — "
                    "the page must be zero-CDN")
                continue
            target = (WEBSITE / ref.split("#", 1)[0].split("?", 1)[0])
            if not target.is_file():
                findings.append(
                    f"<{tag} {attr}=\"{value}\"> resolves to "
                    f"{target.relative_to(WEBSITE)} which does not exist")

    css = WEBSITE / "style.css"
    if css.is_file():
        for n, line in enumerate(css.read_text(encoding="utf-8").splitlines(), 1):
            if CSS_EXTERNAL.search(line):
                findings.append(
                    f"style.css:{n}: external/imported resource ({line.strip()})")
    return findings


def _quoted_block(text, pattern):
    """Extract the quoted-string entries inside the first block matching
    `pattern` (a regex locating the array body)."""
    match = re.search(pattern, text, re.DOTALL)
    if not match:
        return None
    return re.findall(r'"([^"]+)"', match.group(1))


def check_perf_mod_sync():
    """Check 2: app.js catalog == fallback.rs catalog, entry for entry."""
    rust_path = ROOT / "mod-adapter-core" / "src" / "fallback.rs"
    js_path = WEBSITE / "app.js"
    if not rust_path.is_file():
        return [f"cannot find {rust_path.relative_to(ROOT)} to sync against"]
    if not js_path.is_file():
        return ["website/app.js is missing"]

    rust = _quoted_block(
        rust_path.read_text(encoding="utf-8"),
        r"BUILTIN_SUPERSEDED_PERF_MODS[^=]*=\s*&\[(.*?)\];")
    js = _quoted_block(
        js_path.read_text(encoding="utf-8"),
        r"BUILTIN_SUPERSEDED_PERF_MODS\s*=\s*\[(.*?)\];")
    findings = []
    if rust is None:
        findings.append("could not locate BUILTIN_SUPERSEDED_PERF_MODS in "
                        "mod-adapter-core/src/fallback.rs")
    if js is None:
        findings.append("could not locate BUILTIN_SUPERSEDED_PERF_MODS in "
                        "website/app.js")
    if rust is None or js is None:
        return findings

    rust_set, js_set = set(rust), set(js)
    if len(rust) != len(rust_set):
        findings.append("fallback.rs catalog has duplicate entries")
    if len(js) != len(js_set):
        findings.append("app.js catalog has duplicate entries")
    for missing in sorted(rust_set - js_set):
        findings.append(f"app.js is missing catalog entry '{missing}'")
    for extra in sorted(js_set - rust_set):
        findings.append(f"app.js has extra catalog entry '{extra}'")
    if rust_set != js_set:
        findings.append(
            f"catalog mismatch: fallback.rs={len(rust_set)} entries, "
            f"app.js={len(js_set)} entries (expected an identical set)")
    return findings


def check_forbidden_names():
    """Check 3: removed-asset names absent everywhere under website/."""
    findings = []
    for path in sorted(WEBSITE.rglob("*")):
        if not path.is_file():
            continue
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError as error:
            findings.append(f"cannot read {path.relative_to(WEBSITE)}: {error}")
            continue
        for n, line in enumerate(text.splitlines(), 1):
            for name in FORBIDDEN_NAMES:
                if name in line:
                    findings.append(
                        f"{path.relative_to(WEBSITE)}:{n}: forbidden asset "
                        f"name '{name[:6]}…{name[-4:]}' present")
    return findings


def check_site_config_keys():
    """Check 4: site configuration and EN/JA markup copies are complete."""
    cfg = WEBSITE / "site-config.js"
    if not cfg.is_file():
        return ["website/site-config.js is missing"]
    text = cfg.read_text(encoding="utf-8")
    findings = [
        f"site-config.js does not declare '{key}'"
        for key in REQUIRED_CONFIG_KEYS
        if not re.search(rf"\b{re.escape(key)}\s*:", text)
    ]
    markup = (WEBSITE / "index.html").read_text(encoding="utf-8")
    ja = re.search(r"var JA\s*=\s*\{(.*?)\n\s*\};", markup, re.S)
    if not ja:
        return findings + ["index.html is missing the JA dictionary"]
    keys = set(re.findall(
        r"(?m)^\s*([a-z][a-z0-9_]*):\s*", ja.group(1)))
    referenced = set(re.findall(
        r'data-i18n(?:-placeholder|-aria-label)?="([a-z0-9_]+)"', markup))
    for key in sorted(referenced - keys):
        findings.append(f"index.html is missing JA copy for '{key}'")
    for required in (
        "Runs 100% locally in your browser — nothing is uploaded.",
        "ブラウザ内で完結します — ファイルは一切送信されません。",
        "mc-spine-mod-check --scan-mods-dir",
        "Mixin/.class",
        'id="paste-json"',
        'id="paste-analyze-btn"',
        'id="sample-pack-btn"',
        'id="report-stats"',
    ):
        if required not in markup:
            findings.append(f"index.html is missing checker UX contract: {required}")
    return findings


CANONICAL_URL = "https://megu.megumin.tech/"
SITEMAP_URL = "https://megu.megumin.tech/sitemap.xml"
OG_IMAGE_URL = "https://megu.megumin.tech/og-card.svg"


def _tag_attrs(markup, tag):
    """Yield {attr: value} dicts for every `<tag ...>` start tag."""
    for match in re.finditer(r"<" + tag + r"\b[^>]*>", markup):
        yield dict(re.findall(r'([\w-]+)="([^"]*)"', match.group(0)))


def _meta_content(markup, attr, key):
    for attrs in _tag_attrs(markup, "meta"):
        if attrs.get(attr) == key:
            return attrs.get("content")
    return None


def _ld_json_blocks(markup):
    """Return (parsed, raw) pairs for each application/ld+json script."""
    blocks = []
    for match in re.finditer(
            r"<script\b[^>]*>(.*?)</script>", markup, re.DOTALL):
        open_tag = match.group(0)[:match.group(0).index(">") + 1]
        attrs = dict(re.findall(r'([\w-]+)="([^"]*)"', open_tag))
        if attrs.get("type") != "application/ld+json":
            continue
        try:
            blocks.append(json.loads(match.group(1)))
        except json.JSONDecodeError as error:
            blocks.append(error)
    return blocks


def check_seo_metadata():
    """Check 7: canonical URL, crawler files, social meta, JSON-LD."""
    findings = []
    index = WEBSITE / "index.html"
    if not index.is_file():
        return ["index.html is missing"]
    markup = index.read_text(encoding="utf-8")

    canonical = [
        attrs.get("href") for attrs in _tag_attrs(markup, "link")
        if "canonical" in (attrs.get("rel") or "").split()
    ]
    if canonical != [CANONICAL_URL]:
        findings.append(
            "index.html must declare exactly one "
            f'<link rel="canonical" href="{CANONICAL_URL}"> '
            f"(found: {canonical or 'none'})")

    required_meta = {
        ("property", "og:type"): "website",
        ("property", "og:site_name"): "Megu (mc-spine)",
        ("property", "og:title"): None,
        ("property", "og:description"): None,
        ("property", "og:url"): CANONICAL_URL,
        ("property", "og:locale"): "en_US",
        ("property", "og:locale:alternate"): "ja_JP",
        ("property", "og:image"): OG_IMAGE_URL,
        ("name", "twitter:card"): "summary_large_image",
        ("name", "twitter:title"): None,
        ("name", "twitter:description"): None,
    }
    for (attr, key), expected in required_meta.items():
        content = _meta_content(markup, attr, key)
        if content is None:
            findings.append(f"index.html is missing <meta {attr}=\"{key}\">")
        elif expected is not None and content != expected:
            findings.append(
                f'<meta {attr}="{key}"> is "{content}", '
                f'expected "{expected}"')

    app = None
    for block in _ld_json_blocks(markup):
        if isinstance(block, json.JSONDecodeError):
            findings.append(f"JSON-LD block does not parse: {block}")
            continue
        if isinstance(block, dict) and block.get("@type") == "SoftwareApplication":
            app = block
    if app is None:
        findings.append("index.html has no parseable SoftwareApplication JSON-LD")
    else:
        expected_fields = {
            "name": "Megu (mc-spine)",
            "applicationCategory": "GameApplication",
            "operatingSystem": "Linux, Windows, macOS",
            "license": "https://www.gnu.org/licenses/lgpl-3.0.html",
            "isAccessibleForFree": True,
            "url": CANONICAL_URL,
            "codeRepository": "https://github.com/nekomario28/mc-spine",
        }
        for key, expected in expected_fields.items():
            if app.get(key) != expected:
                findings.append(
                    f"JSON-LD SoftwareApplication.{key} is "
                    f"{app.get(key)!r}, expected {expected!r}")

    card = WEBSITE / "og-card.svg"
    if not card.is_file():
        findings.append("website/og-card.svg is missing")
    else:
        svg = card.read_text(encoding="utf-8", errors="replace")
        for needle in ('width="1200"', 'height="630"', "<svg"):
            if needle not in svg:
                findings.append(f"og-card.svg lacks {needle}")

    robots = WEBSITE / "robots.txt"
    if not robots.is_file():
        findings.append("website/robots.txt is missing")
    else:
        text = robots.read_text(encoding="utf-8")
        for needle in ("User-agent: *", "Allow: /", "Disallow: /api/",
                       f"Sitemap: {SITEMAP_URL}"):
            if needle not in text:
                findings.append(f"robots.txt lacks '{needle}'")

    sitemap = WEBSITE / "sitemap.xml"
    if not sitemap.is_file():
        findings.append("website/sitemap.xml is missing")
    else:
        try:
            root = ET.parse(sitemap).getroot()
        except ET.ParseError as error:
            findings.append(f"sitemap.xml is not well-formed XML: {error}")
        else:
            locs = [el.text for el in root.iter() if el.tag.endswith("loc")]
            if CANONICAL_URL not in locs:
                findings.append(
                    f"sitemap.xml does not list {CANONICAL_URL} "
                    f"(found: {locs})")
            lastmods = [
                el.text for el in root.iter() if el.tag.endswith("lastmod")]
            if not lastmods:
                findings.append("sitemap.xml entries lack <lastmod>")
    return findings


JS_HARNESS = r"""
"use strict";
const assert = require("assert");
require(process.env.SITE_CONFIG_JS); // sets globalThis.SITE_CONFIG
const m = require(process.env.APP_JS);

// Catalog sanity.
assert.strictEqual(m.BUILTIN_SUPERSEDED_PERF_MODS.length, 52);

// --- Masked feedback /api/report contract and fallback -----------------
const sensitive = "Crash in /home/alice/.minecraft/logs/latest.log on " +
  "192.168.1.25, C:\\Users\\Bob\\Desktop\\crash.txt, " +
  "Bearer secret_ABC123 token=very-secret " +
  "github_pat_12345678901234567890";
const compact = m.buildCompactReport({
  kind: "crash", fingerprint: "megu-e-ab12cd34", mc_version: "1.21.1",
  summary: sensitive,
});
assert.deepStrictEqual(JSON.parse(compact.json), compact.report);
assert.strictEqual(compact.report.schema, "megu-report/1");
assert.strictEqual(compact.report.kind, "crash");
assert.strictEqual(compact.report.fingerprint, "MEGU-E-AB12CD34");
assert.strictEqual(compact.report.mc_version, "1.21.1");
assert.strictEqual(compact.report.engine_version, "");
assert.strictEqual(compact.report.os, "");
assert.strictEqual(compact.bytes, Buffer.byteLength(compact.json, "utf8"));
assert.ok(compact.bytes <= 4096);
for (const secret of ["alice", "192.168.1.25", "Bob", "secret_ABC123",
                      "very-secret", "github_pat_"]) {
  assert.ok(!compact.json.includes(secret), secret + " leaked");
}
assert.ok(compact.json.includes("[REDACTED PATH]"));
assert.ok(compact.json.includes("[REDACTED IP]"));
assert.ok(compact.json.includes("[REDACTED]"));
assert.ok(!m.redactReportText("~/.minecraft/config token=12345").includes("12345"));
assert.ok(!m.redactReportText("/var/log/minecraft.log").includes("/var/log"));
assert.ok(!m.redactReportText("[2001:db8::1]").includes("2001:db8"));
assert.strictEqual(m.redactReportText("fe80::1"), "[REDACTED IP]");
assert.strictEqual(m.redactReportText("::1"), "[REDACTED IP]");
assert.strictEqual(m.redactReportText("https://example.com/docs"),
                   "https://example.com/docs");
assert.ok(!m.redactReportText('{"token":"superSecret"}').includes("superSecret"));
assert.throws(() => m.buildCompactReport({
  kind: "idea", mc_version: "26.3", summary: "a".repeat(241),
}), /240/);
assert.throws(() => m.buildCompactReport({
  kind: "bug", mc_version: "1.21.1", summary: "ok",
  fingerprint: "MEGU-E-invalid",
}), /fingerprint/);
const idea = m.buildCompactReport({
  kind: "idea", mc_version: "26.3", summary: "Better gamepad support",
});
assert.ok(!Object.hasOwn(idea.report, "fingerprint"));
const crashNoFingerprint = m.buildCompactReport({
  kind: "crash", mc_version: "1.21.1", summary: "Crash without a log ID",
});
assert.ok(!Object.hasOwn(crashNoFingerprint.report, "fingerprint"));
assert.ok(m.enforceReportByteLimit("界".repeat(1365)) <= 4096);
assert.throws(() => m.enforceReportByteLimit("界".repeat(1366)), /4 KiB/);
const fallback = new URL(m.makeGitHubIssueUrl(
  "https://github.com/nekomario28/mc-spine", compact));
assert.strictEqual(fallback.origin, "https://github.com");
assert.strictEqual(fallback.pathname, "/nekomario28/mc-spine/issues/new");
assert.ok(fallback.searchParams.get("title").startsWith(
  "[Crash] MEGU-E-AB12CD34"));
assert.ok(fallback.searchParams.get("body").includes(compact.json));
assert.ok(!fallback.searchParams.get("body").includes("alice"));
const search = new URL(m.makeFingerprintSearchUrl(
  "https://github.com/nekomario28/mc-spine", compact.report.fingerprint));
assert.strictEqual(search.pathname, "/nekomario28/mc-spine/issues");
assert.strictEqual(search.searchParams.get("q"), "MEGU-E-AB12CD34");

// --- Sample Modrinth pack: every classification exercised --------------
const report = m.analyzeModpackJson(m.SAMPLE_MODRINTH_INDEX_JSON, {
  fileName: "modrinth.index.json",
  target: "1.21.1",
});
assert.strictEqual(report.schema, "mc-spine-modpack-preflight/1");
assert.strictEqual(report.format, "modrinth");
assert.strictEqual(report.mc_version_declared, "1.21.1");
assert.strictEqual(report.target, "1.21.1");
assert.strictEqual(report.version_verdict.kind, "compatible");
assert.strictEqual(report.readiness, "needs_jar_inspection");
assert.deepStrictEqual(
  report.skipped_perf_mods,
  ["distanthorizons", "ferritecore", "lithium", "modernfix", "sodium"]);

const cls = {};
report.entries.forEach(e => { cls[e.path] = e["class"]; });
assert.strictEqual(cls["mods/create-1.21.1-6.0.6.jar"], "native_flagship");
assert.strictEqual(
  cls["mods/sodium-fabric-0.6.13+mc1.21.1.jar"], "superseded_perf_mod");
assert.strictEqual(
  cls["mods/lithium-fabric-0.15.0+mc1.21.1.jar"], "superseded_perf_mod");
assert.strictEqual(
  cls["mods/modernfix-neoforge-5.20.0+mc1.21.1.jar"], "superseded_perf_mod");
assert.strictEqual(
  cls["mods/ferritecore-7.0.2-neoforge.jar"], "superseded_perf_mod");
assert.strictEqual(
  cls["mods/DistantHorizons-2.3.2-b-1.21.1-fabric-neoforge.jar"],
  "superseded_perf_mod");
assert.strictEqual(
  cls["mods/iris-1.8.0-beta.4+1.21.1.jar"], "client_only_skipped");
assert.strictEqual(
  cls["mods/waystones-neoforge-1.21.1-21.1.10.jar"], "needs_inspection");
assert.strictEqual(
  cls["datapacks/create-compat-tweaks.zip"], "universal_native");
assert.strictEqual(
  cls["resourcepacks/faithful-32x.zip"], "universal_native");

// --- Real-shape Modrinth manifest: loader, files and conservative IDs ---
const realMr = JSON.stringify({
  formatVersion: 1,
  game: "minecraft",
  name: "Real-shape pack",
  versionId: "1.0",
  dependencies: {minecraft: "1.21.1", "fabric-loader": "0.16.10"},
  files: [
    {path: "mods/sodium-fabric-0.6.3.jar", hashes: {sha1: "1234"}},
    {path: "mods/lithium-fabric-0.15.0.jar"},
    {path: "mods/create-6.0.6.jar"},
    {path: "mods/unknown-library-1.0.jar"},
  ],
});
const realMrResult = m.analyzeModpackJson(realMr);
assert.strictEqual(realMrResult.version_verdict.kind, "compatible");
assert.deepStrictEqual(realMrResult.loaders,
  [{id: "fabric-loader", version: "0.16.10", primary: true}]);
assert.deepStrictEqual(realMrResult.entries.map(e => e["class"]), [
  "superseded_perf_mod", "superseded_perf_mod", "native_flagship",
  "needs_inspection",
]);
assert.deepStrictEqual(realMrResult.skipped_perf_mods, ["lithium", "sodium"]);
assert.strictEqual(realMrResult.needs_inspection_mods.length, 1);

// Only specific known native/adapter family IDs are recognized.
const knownFamilies = [
  "farmers-delight", "openpac", "ftb-chunks", "ae2", "twilightforest",
  "the-aether", "terrablender", "appleskin", "xaeros-minimap",
  "xaeros-world-map", "simplevoicechat",
];
const familyPack = JSON.stringify({
  formatVersion: 1, game: "minecraft", name: "Known adapters", versionId: "1",
  dependencies: {minecraft: "1.21.1", neoforge: "21.1.93"},
  files: knownFamilies.map(id => ({path: "mods/" + id + ".jar"})).concat([
    {path: "mods/unrelated-create-compat.jar"},
  ]),
});
const familyResult = m.analyzeModpackJson(familyPack);
assert.deepStrictEqual(familyResult.entries.map(e => e["class"]),
  knownFamilies.map(() => "native_flagship").concat(["needs_inspection"]));
assert.ok(familyResult.entries[0].note.includes("local inspection"));
const prototypeName = JSON.stringify({
  formatVersion: 1, game: "minecraft", name: "Unrecognized", versionId: "1",
  dependencies: {minecraft: "1.21.1"},
  files: [{path: "mods/constructor.jar"}],
});
assert.strictEqual(m.analyzeModpackJson(prototypeName).entries[0]["class"],
  "needs_inspection");

// --- CurseForge manifest ------------------------------------------------
const cf = JSON.stringify({
  manifestType: "minecraftModpack",
  manifestVersion: 1,
  name: "CF Sample",
  version: "2.0",
  minecraft: {
    version: "1.21.1",
    modLoaders: [{id: "neoforge-21.1.93", primary: true}],
  },
  files: [
    {projectID: 1, fileID: 2, slug: "sodium"},
    {projectID: 3, fileID: 4, slug: "create"},
    {projectID: 5, fileID: 6},
  ],
});
const cfReport = m.analyzeModpackJson(cf, {fileName: "manifest.json"});
assert.strictEqual(cfReport.format, "curseforge");
assert.strictEqual(cfReport.readiness, "needs_jar_inspection");
const cfCls = {};
cfReport.entries.forEach(e => { cfCls[e.path] = e["class"]; });
assert.strictEqual(cfCls["mods/sodium.jar"], "superseded_perf_mod");
assert.strictEqual(cfCls["mods/create.jar"], "native_flagship");
assert.strictEqual(
  cfCls["mods/curseforge-project-5-file-6.jar"], "needs_inspection");
assert.deepStrictEqual(cfReport.loaders,
  [{id: "neoforge", version: "21.1.93", primary: true}]);

// Actual CurseForge exports contain numeric IDs, often without slugs.
const cf26 = JSON.stringify({
  minecraft: {version: "26.3", modLoaders: [
    {id: "neoforge-26.3.0", primary: true},
  ]},
  manifestType: "minecraftModpack",
  manifestVersion: 1,
  name: "26.3 export",
  version: "1.0",
  author: "Example",
  files: [
    {projectID: 4321, fileID: 1234, required: true},
    {projectID: 4322, fileID: 1235, fileName: "appleskin-26.3.jar"},
  ],
  overrides: "overrides",
});
const cf26Report = m.analyzeModpackJson(cf26, {fileName: "manifest.json", target: "26.3"});
assert.strictEqual(cf26Report.version_verdict.kind, "compatible");
assert.deepStrictEqual(cf26Report.entries.map(e => e["class"]),
  ["needs_inspection", "native_flagship"]);
assert.strictEqual(m.analyzeModpackJson(cf26, {target: "1.20.1"})
  .readiness, "version_mismatch");
const cfOld = JSON.stringify({
  manifestType: "minecraftModpack", manifestVersion: 1,
  name: "Old CF pack", version: "1",
  minecraft: {version: "1.20.1", modLoaders: [{id: "forge-47.4.0", primary: true}]},
  files: [{projectID: 700, fileID: 900}],
});
assert.strictEqual(m.analyzeModpackJson(cfOld, {target: "26.3"})
  .readiness, "version_mismatch");

// --- Version gate --------------------------------------------------------
const oldPack = JSON.stringify({
  formatVersion: 1, game: "minecraft", name: "Old", versionId: "0.1",
  dependencies: {minecraft: "1.20.1"},
  files: [],
});
const mismatch = m.analyzeModpackJson(oldPack, {target: "1.21.1"});
assert.strictEqual(mismatch.version_verdict.kind, "major_mismatch");
assert.strictEqual(mismatch.readiness, "version_mismatch");

const rangedPack = JSON.stringify({
  formatVersion: 1, game: "minecraft", name: "Ranged", versionId: "0.1",
  dependencies: {minecraft: ">=1.21 <1.22"},
  files: [],
});
assert.strictEqual(
  m.analyzeModpackJson(rangedPack, {target: "1.21.1"})
    .version_verdict.kind, "compatible");
assert.strictEqual(
  m.analyzeModpackJson(rangedPack, {target: "26.3"})
    .readiness, "version_mismatch");
assert.strictEqual(
  m.checkModMcVersion([1, 21, 1], ">=1.21.3").kind, "minor_mismatch");
assert.strictEqual(
  m.checkModMcVersion([1, 21, 1], "[1.20,1.21.4]").kind, "compatible");
assert.strictEqual(
  m.checkModMcVersion([1, 21, 1], "not-a-version").kind, "invalid_range");

// --- JAR-name mod-id inference -------------------------------------------
assert.strictEqual(
  m.inferCandidateModId("mods/DistantHorizons-2.3.2-b-1.21.1-fabric-neoforge.jar"),
  "distanthorizons");
assert.strictEqual(
  m.inferCandidateModId("mods/fabric-api-0.102.0+1.21.1.jar"), "fabric-api");
assert.strictEqual(
  m.inferCandidateModId("mods/sodium-fabric-0.6.13+mc1.21.1.jar"), "sodium");
assert.strictEqual(
  m.inferCandidateModId("mods/ferritecore-7.0.2-neoforge.jar"), "ferritecore");
assert.strictEqual(m.inferCandidateModId("docs/readme.txt"), null);

// --- Fail-closed: path traversal / bad format ----------------------------
function expectThrow(text, fileName, needle) {
  try {
    m.analyzeModpackJson(text, {fileName});
  } catch (e) {
    assert.ok(
      e instanceof m.ModpackError, "expected ModpackError, got " + e);
    assert.ok(
      e.message.includes(needle),
      "error '" + e.message + "' lacks '" + needle + "'");
    return;
  }
  assert.fail("expected a ModpackError");
}

expectThrow(JSON.stringify({
  formatVersion: 1, game: "minecraft", name: "Evil", versionId: "1",
  dependencies: {minecraft: "1.21.1"},
  files: [{path: "../evil.jar"}],
}), "modrinth.index.json", "traversal");
expectThrow(JSON.stringify({
  formatVersion: 1, game: "minecraft", name: "Evil", versionId: "1",
  dependencies: {minecraft: "1.21.1"},
  files: [{path: "/abs/evil.jar"}],
}), "modrinth.index.json", "absolute path");
expectThrow(JSON.stringify({
  formatVersion: 2, game: "minecraft", name: "X", versionId: "1",
  dependencies: {minecraft: "1.21.1"}, files: [],
}), "modrinth.index.json", "formatVersion");
expectThrow("{not json", "manifest.json", "invalid JSON");
expectThrow("{}", "pack.json", "cannot determine pack format");
expectThrow(" ".repeat(2 * 1024 * 1024 + 1), "manifest.json", "2 MiB");
expectThrow("界".repeat(700000), "manifest.json", "2 MiB");

// --- .mrpack / .zip archive extraction (pure-JS ZIP reader) --------------
const zlib = require("zlib");

const CRC_TABLE = new Uint32Array(256);
for (let n = 0; n < 256; n++) {
  let c = n;
  for (let k = 0; k < 8; k++) c = (c & 1) ? (0xedb88320 ^ (c >>> 1)) : c >>> 1;
  CRC_TABLE[n] = c >>> 0;
}
function crc32buf(buf) {
  let crc = 0xffffffff;
  for (const b of buf) crc = CRC_TABLE[(crc ^ b) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
}

// Minimal ZIP writer for fixtures: local headers + central dir + EOCD,
// Stored (method 0) or deflateRaw (method 8) members.
function buildZip(members) {
  const parts = [];
  const central = [];
  let offset = 0;
  for (const mdef of members) {
    const nameBytes = Buffer.from(mdef.name, "utf8");
    const data = Buffer.isBuffer(mdef.data)
      ? mdef.data : Buffer.from(mdef.data);
    const stored = mdef.method === 8 ? zlib.deflateRawSync(data) : data;
    const crc = crc32buf(data);
    const flag = mdef.flag || 0;
    const method = mdef.method || 0;

    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(flag, 6);
    local.writeUInt16LE(method, 8);
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(stored.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(nameBytes.length, 26);
    parts.push(local, nameBytes, stored);

    const cd = Buffer.alloc(46);
    cd.writeUInt32LE(0x02014b50, 0);
    cd.writeUInt16LE(20, 4);
    cd.writeUInt16LE(20, 6);
    cd.writeUInt16LE(flag, 8);
    cd.writeUInt16LE(method, 10);
    cd.writeUInt32LE(crc, 16);
    cd.writeUInt32LE(stored.length, 20);
    cd.writeUInt32LE(data.length, 24);
    cd.writeUInt16LE(nameBytes.length, 28);
    cd.writeUInt32LE(offset, 42);
    central.push(cd, nameBytes);
    offset += 30 + nameBytes.length + stored.length;
  }
  const cdBuf = Buffer.concat(central);
  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0);
  eocd.writeUInt16LE(members.length, 8);
  eocd.writeUInt16LE(members.length, 10);
  eocd.writeUInt32LE(cdBuf.length, 12);
  eocd.writeUInt32LE(offset, 16);
  return Buffer.concat([...parts, cdBuf, eocd]);
}

const CD_SIG = Buffer.from([0x50, 0x4b, 0x01, 0x02]);

// Offset of the named member's central-directory record inside a fixture
// built by buildZip (names are unique per fixture).
function findCdEntry(zip, name) {
  let pos = zip.indexOf(CD_SIG);
  assert.ok(pos > 0);
  while (zip.toString("utf8", pos + 46, pos + 46 + name.length) !== name) {
    pos = zip.indexOf(CD_SIG, pos + 1);
    assert.ok(pos > 0);
  }
  return pos;
}

async function expectExtractThrow(bytes, fileName, needle) {
  try {
    await m.extractManifestFromZipBytes(bytes, fileName);
  } catch (e) {
    assert.ok(
      e instanceof m.ModpackError,
      "expected ModpackError for " + fileName + ", got " +
        (e && e.stack || e));
    const hit = needle instanceof RegExp
      ? needle.test(e.message) : e.message.includes(needle);
    assert.ok(hit, "error '" + e.message + "' lacks '" + needle + "'");
    return;
  }
  assert.fail("expected a ModpackError for " + fileName);
}

(async function () {
  const indexBytes = Buffer.from(m.SAMPLE_MODRINTH_INDEX_JSON, "utf8");

  // A real ZIP_DEFLATED .mrpack: the deflated index plus decoy members,
  // exactly like a Modrinth export lays them out.
  const mrpack = buildZip([
    {name: "overrides/mods/sodium-fabric-0.6.13+mc1.21.1.jar",
     data: Buffer.from("decoy-jar"), method: 0},
    {name: "modrinth.index.json", data: indexBytes, method: 8},
    {name: "overrides/config/client.cfg",
     data: Buffer.from("x=1"), method: 8},
  ]);
  const fromMrpack = await m.extractManifestFromZipBytes(
    new Uint8Array(mrpack), "sample.mrpack");
  assert.strictEqual(fromMrpack.name, "modrinth.index.json");
  const zippedReport = m.analyzeModpackJson(
    fromMrpack.text, {fileName: "sample.mrpack", target: "1.21.1"});
  assert.deepStrictEqual(zippedReport.entries, report.entries);
  assert.strictEqual(zippedReport.readiness, report.readiness);
  assert.strictEqual(zippedReport.format, "modrinth");

  // A Stored (uncompressed) CurseForge .zip — classic CF export layout.
  const cfZip = buildZip([
    {name: "overrides/mods/readme.txt",
     data: Buffer.from("hi"), method: 0},
    {name: "manifest.json", data: Buffer.from(cf, "utf8"), method: 0},
  ]);
  const fromZip = await m.extractManifestFromZipBytes(
    new Uint8Array(cfZip), "cf-pack.zip");
  assert.strictEqual(fromZip.name, "manifest.json");
  const zipCf = m.analyzeModpackJson(
    fromZip.text, {fileName: "cf-pack.zip"});
  assert.deepStrictEqual(zipCf.entries, cfReport.entries);
  assert.strictEqual(zipCf.format, "curseforge");

  // modrinth.index.json is preferred when both members exist.
  const both = buildZip([
    {name: "manifest.json", data: Buffer.from(cf, "utf8"), method: 0},
    {name: "modrinth.index.json", data: indexBytes, method: 8},
  ]);
  const picked = await m.extractManifestFromZipBytes(
    new Uint8Array(both), "both.mrpack");
  assert.strictEqual(picked.name, "modrinth.index.json");

  // Fail closed: garbage, truncated, corrupt, oversized, encrypted,
  // foreign-method, ZIP64-flagged, and manifest-less archives.
  await expectExtractThrow(
    Buffer.from("definitely not a zip"), "bad.zip", "not a ZIP");
  await expectExtractThrow(
    new Uint8Array(mrpack.subarray(0, mrpack.length - 10)),
    "truncated.mrpack", "central directory");

  const corrupt = Buffer.from(mrpack);
  // Flip one byte inside the deflated index member's payload (right after
  // its 30-byte local header + name): inflate, size, or CRC must catch it.
  const nameLen = Buffer.from("overrides/mods/sodium-fabric-0.6.13+mc1.21.1.jar").length;
  const second = 30 + nameLen + Buffer.from("decoy-jar").length +
    30 + "modrinth.index.json".length;
  corrupt[second] ^= 0xff;
  await expectExtractThrow(
    new Uint8Array(corrupt), "corrupt.mrpack",
    /crc|corrupt|deflate|mismatch|size/i);

  // Declared >2 MiB uncompressed: rejected before a byte is inflated.
  const declaredBomb = Buffer.from(mrpack);
  declaredBomb.writeUInt32LE(
    3 * 1024 * 1024,
    findCdEntry(declaredBomb, "modrinth.index.json") + 24);
  await expectExtractThrow(
    new Uint8Array(declaredBomb), "declared-bomb.mrpack", "2 MiB");

  // Lying about the size: real output is 3 MiB while the central
  // directory claims 100 — the streaming cap must still stop it.
  const streamBomb = Buffer.from(buildZip([
    {name: "modrinth.index.json",
     data: Buffer.alloc(3 * 1024 * 1024), method: 8},
  ]));
  streamBomb.writeUInt32LE(
    100, findCdEntry(streamBomb, "modrinth.index.json") + 24);
  await expectExtractThrow(
    new Uint8Array(streamBomb), "stream-bomb.mrpack", "2 MiB");

  const overArchive = buildZip([
    {name: "manifest.json", data: Buffer.alloc(2 * 1024 * 1024), method: 0},
  ]);
  await expectExtractThrow(new Uint8Array(overArchive),
    "oversized.zip", "file exceeds the 2 MiB limit");

  const encrypted = Buffer.from(cfZip);
  encrypted.writeUInt16LE(
    0x0001, findCdEntry(encrypted, "manifest.json") + 8);
  await expectExtractThrow(
    new Uint8Array(encrypted), "encrypted.zip", "encrypted");

  const imploded = Buffer.from(cfZip);
  imploded.writeUInt16LE(
    99, findCdEntry(imploded, "manifest.json") + 10);
  await expectExtractThrow(
    new Uint8Array(imploded), "imploded.zip", "compression method");

  const zip64 = Buffer.from(cfZip);
  zip64.writeUInt16LE(0xffff, zip64.length - 22 + 10);
  await expectExtractThrow(
    new Uint8Array(zip64), "zip64.mrpack", "ZIP64");

  const noManifest = buildZip([
    {name: "overrides/readme.txt", data: Buffer.from("hi"), method: 0},
  ]);
  await expectExtractThrow(
    new Uint8Array(noManifest), "empty.zip", "manifest.json");

  console.log("app.js unit check: OK");
})().catch(function (err) {
  console.error(err && err.stack || err);
  process.exit(1);
});
"""


def check_js_unit():
    """Check 5: node unit harness for analyzeModpackJson +
    extractManifestFromZipBytes."""
    node = shutil.which("node")
    if node is None:
        return ["node not on PATH — skipped the app.js unit check "
                "(install node to run it)"]
    app_js = WEBSITE / "app.js"
    cfg_js = WEBSITE / "site-config.js"
    if not app_js.is_file() or not cfg_js.is_file():
        return ["website/app.js or site-config.js missing for the node check"]
    with tempfile.NamedTemporaryFile(
            "w", suffix=".cjs", delete=False, encoding="utf-8") as harness:
        harness.write(JS_HARNESS)
        harness_path = harness.name
    try:
        result = subprocess.run(
            [node, harness_path],
            env={
                "PATH": __import__("os").environ.get("PATH", ""),
                "APP_JS": str(app_js),
                "SITE_CONFIG_JS": str(cfg_js),
            },
            capture_output=True,
            text=True,
            timeout=60,
        )
    finally:
        Path(harness_path).unlink(missing_ok=True)
    if result.returncode != 0:
        return [
            "node unit check failed:\n"
            + (result.stdout + result.stderr).strip()
        ]
    return []


def check_functions_unit():
    """Check 6: node unit tests for the Pages Functions in functions/api/."""
    node = shutil.which("node")
    if node is None:
        return ["node not on PATH — skipped the functions unit check "
                "(install node to run it)"]
    suite = WEBSITE / "test_functions.mjs"
    if not suite.is_file():
        return ["website/test_functions.mjs is missing"]
    result = subprocess.run(
        [node, str(suite)],
        capture_output=True,
        text=True,
        timeout=60,
    )
    if result.returncode != 0:
        return [
            "functions unit check failed:\n"
            + (result.stdout + result.stderr).strip()
        ]
    return []


def main():
    checks = (
        ("local assets & zero external deps", check_local_assets),
        ("BUILTIN_SUPERSEDED_PERF_MODS sync", check_perf_mod_sync),
        ("forbidden asset names", check_forbidden_names),
        ("site config & EN/JA copy", check_site_config_keys),
        ("app.js unit check (node)", check_js_unit),
        ("functions unit check (node)", check_functions_unit),
        ("SEO metadata & crawler files", check_seo_metadata),
    )
    failures = []
    skipped = 0
    for label, run in checks:
        findings = run()
        if findings and all(f.endswith("run it)") for f in findings):
            skipped += 1
            print(f"SKIP  {label}: {findings[0]}")
            continue
        if findings:
            failures.extend(findings)
            print(f"FAIL  {label}")
            for finding in findings:
                print(f"      - {finding}")
        else:
            print(f"PASS  {label}")
    if failures:
        print(f"\n{len(failures)} finding(s) — check_site.py FAILED")
        return 1
    print("\nAll checks passed" + (f" ({skipped} skipped)" if skipped else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
