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
  5. When `node` is on PATH, a small unit harness requires website/app.js
     (and site-config.js first, exactly like the browser) and exercises
     analyzeModpackJson against Modrinth + CurseForge fixtures, the
     version-range engine, id inference, and path-traversal rejection.

Exit status: 0 clean, 1 findings.
"""

import html.parser
import json
import re
import shutil
import subprocess
import sys
import tempfile
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
    """Check 4: site-config.js declares every required key."""
    cfg = WEBSITE / "site-config.js"
    if not cfg.is_file():
        return ["website/site-config.js is missing"]
    text = cfg.read_text(encoding="utf-8")
    return [
        f"site-config.js does not declare '{key}'"
        for key in REQUIRED_CONFIG_KEYS
        if not re.search(rf"\b{re.escape(key)}\s*:", text)
    ]


JS_HARNESS = r"""
"use strict";
const assert = require("assert");
require(process.env.SITE_CONFIG_JS); // sets globalThis.SITE_CONFIG
const m = require(process.env.APP_JS);

// Catalog sanity.
assert.strictEqual(m.BUILTIN_SUPERSEDED_PERF_MODS.length, 36);

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

console.log("app.js unit check: OK");
"""


def check_js_unit():
    """Check 5: node unit harness for analyzeModpackJson."""
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


def main():
    checks = (
        ("local assets & zero external deps", check_local_assets),
        ("BUILTIN_SUPERSEDED_PERF_MODS sync", check_perf_mod_sync),
        ("forbidden asset names", check_forbidden_names),
        ("site-config.js keys", check_site_config_keys),
        ("app.js unit check (node)", check_js_unit),
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
