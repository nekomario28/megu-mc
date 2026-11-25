#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Regression tests for tools/megu_mods.py.

A real loopback `http.server.ThreadingHTTPServer` mocks the Modrinth v2 API
(`/v2/search`, `/v2/project/{slug}/version`) and the JAR artifact download
endpoint. Every test drives the real CLI via subprocess with
`--api-base http://127.0.0.1:<port>/v2`, so the loopback-only download
override, consent gate, hash verification, and atomic install paths all run
end-to-end.
"""

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
import urllib.parse
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "tools" / "megu_mods.py"

MC = "1.21.1"


def make_file_entry(base, name, data, sha512=True, sha256=True, primary=True):
    hashes = {}
    if sha512:
        hashes["sha512"] = hashlib.sha512(data).hexdigest()
    if sha256:
        hashes["sha256"] = hashlib.sha256(data).hexdigest()
    return {
        "filename": name,
        "url": f"{base}/files/{name}",
        "size": len(data),
        "primary": primary,
        "hashes": hashes,
    }


def make_version(project_id, number, files, deps=(), mc=MC, loader="neoforge",
                 status="listed", vtype="release",
                 date="2026-01-01T00:00:00Z"):
    return {
        "id": f"VER-{project_id}-{number}",
        "project_id": project_id,
        "version_number": number,
        "version_type": vtype,
        "status": status,
        "date_published": date,
        "game_versions": [mc],
        "loaders": [loader],
        "dependencies": [
            {"project_id": dep, "dependency_type": "required"}
            for dep in deps
        ],
        "files": files,
    }


class MockModrinthHandler(BaseHTTPRequestHandler):
    """Serves `server.pages[path] -> (status, bytes)`; logs every request."""

    def do_GET(self):  # noqa: N802
        path = urllib.parse.urlsplit(self.path).path
        self.server.requests.append(self.path)
        entry = self.server.pages.get(path)
        if entry is None:
            self.send_error(404, "mock route not found")
            return
        status, body = entry
        self.send_response(status)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):  # silence default stderr logging
        pass


class MeguModsTestBase(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="megu-mods-")
        self.addCleanup(self.temp.cleanup)
        self.mods_dir = Path(self.temp.name) / "mods"
        self.mods_dir.mkdir()
        self.server = ThreadingHTTPServer(("127.0.0.1", 0),
                                          MockModrinthHandler)
        self.server.pages = {}
        self.server.requests = []
        self.thread = threading.Thread(target=self.server.serve_forever,
                                       daemon=True)
        self.thread.start()
        self.addCleanup(self.server.shutdown)
        self.addCleanup(self.server.server_close)
        self.base = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.api_base = f"{self.base}/v2"

    def cli(self, *args):
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--api-base", self.api_base, *args],
            capture_output=True, text=True, stdin=subprocess.DEVNULL,
            timeout=30, check=False)

    def route_json(self, path, doc):
        self.server.pages[path] = (
            200, json.dumps(doc).encode("utf-8"))

    def route_file(self, name, data):
        self.server.pages[f"/files/{name}"] = (200, data)

    def file_requests(self):
        return [r for r in self.server.requests if r.startswith("/files/")]

    def installed_jars(self):
        return sorted(p.name for p in self.mods_dir.iterdir()
                      if p.suffix == ".jar")


class SearchTest(MeguModsTestBase):
    HITS = {
        "hits": [
            {
                "slug": "sodium",
                "title": "Sodium",
                "project_id": "AANobbMI",
                "downloads": 33000000,
                "versions": [MC],
                "categories": ["optimization", "fabric"],
            },
            {
                "slug": "mymod",
                "title": "My Mod",
                "project_id": "MYMODID",
                "downloads": 4200,
                "versions": [MC],
                "categories": ["adventure", "neoforge"],
            },
        ],
        "total_hits": 2,
        "offset": 0,
        "limit": 10,
    }

    def setUp(self):
        super().setUp()
        self.route_json("/v2/search", self.HITS)

    def test_search_human_table_and_builtin_tag(self):
        proc = self.cli("search", "sodium")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("sodium [built-in]", proc.stdout)
        self.assertIn("Sodium", proc.stdout)
        self.assertIn("mymod", proc.stdout)
        self.assertIn("slug", proc.stdout.splitlines()[0])
        # The superseded marker must never appear on a normal hit.
        self.assertNotIn("mymod [built-in]", proc.stdout)

    def test_search_json_output(self):
        proc = self.cli("search", "sodium", "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        doc = json.loads(proc.stdout)
        self.assertEqual(doc["schema"], "megu-mods-search/1")
        by_slug = {h["slug"]: h for h in doc["hits"]}
        self.assertTrue(by_slug["sodium"]["builtin_superseded"])
        self.assertFalse(by_slug["mymod"]["builtin_superseded"])
        self.assertTrue(by_slug["sodium"]["mc_match"])
        self.assertEqual(by_slug["mymod"]["loaders"], ["neoforge"])


class AddTest(MeguModsTestBase):
    MYMOD_JAR = b"PK-mymod-bytes-" + b"\x01" * 1024
    MYLIB_JAR = b"PK-mylib-bytes-" + b"\x02" * 512

    def route_mymod(self, files=None, deps=("MYLIBID",), **kwargs):
        files = files or [
            make_file_entry(self.base, "mymod-1.0.jar", self.MYMOD_JAR)]
        self.route_json("/v2/project/mymod/version", [
            make_version("MYMODID", "1.0", files, deps=deps, **kwargs)])

    def route_mylib(self, files=None, deps=(), **kwargs):
        files = files or [
            make_file_entry(self.base, "mylib-2.0.jar", self.MYLIB_JAR)]
        self.route_json("/v2/project/MYLIBID/version", [
            make_version("MYLIBID", "2.0", files, deps=deps, **kwargs)])

    def test_add_superseded_sodium_skips_download(self):
        proc = self.cli("add", "sodium", "--mods-dir", str(self.mods_dir),
                        "--yes")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("already covered by megu built-in optimizations",
                      proc.stdout)
        self.assertIn("--force", proc.stdout)
        self.assertEqual(self.file_requests(), [])
        self.assertEqual(self.installed_jars(), [])
        # The version endpoint must not even be queried before the skip.
        self.assertFalse(any("sodium" in r for r in self.server.requests))

    def test_add_refuses_without_yes_on_non_tty(self):
        self.route_mymod(deps=())
        proc = self.cli("add", "mymod", "--mods-dir", str(self.mods_dir))
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("--yes", proc.stderr)
        self.assertEqual(self.file_requests(), [])
        self.assertEqual(self.installed_jars(), [])

    def test_add_dry_run_prints_plan_without_download(self):
        self.route_mymod(deps=())
        proc = self.cli("add", "mymod", "--mods-dir", str(self.mods_dir),
                        "--dry-run")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("mymod-1.0.jar", proc.stdout)
        self.assertIn("dry run", proc.stdout)
        self.assertEqual(self.file_requests(), [])
        self.assertEqual(self.installed_jars(), [])

    def test_add_yes_installs_transitive_dep_verified(self):
        self.route_mymod()
        self.route_mylib()
        self.route_file("mymod-1.0.jar", self.MYMOD_JAR)
        self.route_file("mylib-2.0.jar", self.MYLIB_JAR)
        proc = self.cli("add", "mymod", "--mods-dir", str(self.mods_dir),
                        "--yes")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("install plan", proc.stdout)
        self.assertIn("mylib-2.0.jar", proc.stdout)
        self.assertIn("mymod-1.0.jar", proc.stdout)
        self.assertEqual(
            (self.mods_dir / "mymod-1.0.jar").read_bytes(), self.MYMOD_JAR)
        self.assertEqual(
            (self.mods_dir / "mylib-2.0.jar").read_bytes(), self.MYLIB_JAR)
        # Dependency downloads and installs before the dependent mod.
        self.assertEqual(self.file_requests(),
                         ["/files/mylib-2.0.jar", "/files/mymod-1.0.jar"])
        # No partial .tmp artifacts remain.
        self.assertEqual(
            [p.name for p in self.mods_dir.iterdir()
             if p.suffix == ".tmp" or p.name.startswith(".megu-mods-")],
            [])

    def test_add_rejects_corrupt_sha512(self):
        bad_entry = make_file_entry(self.base, "mymod-1.0.jar",
                                    self.MYMOD_JAR)
        bad_entry["hashes"]["sha512"] = "f" * 128  # wrong digest
        self.route_mymod(files=[bad_entry], deps=())
        self.route_file("mymod-1.0.jar", self.MYMOD_JAR)
        proc = self.cli("add", "mymod", "--mods-dir", str(self.mods_dir),
                        "--yes")
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("sha512", proc.stderr)
        self.assertEqual(self.installed_jars(), [])
        self.assertEqual(
            [p.name for p in self.mods_dir.iterdir() if ".tmp" in p.name],
            [])

    def test_add_rejects_untrusted_download_host(self):
        entry = make_file_entry(self.base, "mymod-1.0.jar", self.MYMOD_JAR)
        entry["url"] = "https://evil.example/a.jar"
        self.route_mymod(files=[entry], deps=())
        proc = self.cli("add", "mymod", "--mods-dir", str(self.mods_dir),
                        "--yes")
        self.assertNotEqual(proc.returncode, 0)
        self.assertEqual(self.file_requests(), [])
        self.assertEqual(self.installed_jars(), [])

    def test_add_rejects_path_traversal_filename(self):
        entry = make_file_entry(self.base, "escape.jar", self.MYMOD_JAR)
        entry["filename"] = "../evil.jar"
        self.route_mymod(files=[entry], deps=())
        proc = self.cli("add", "mymod", "--mods-dir", str(self.mods_dir),
                        "--yes")
        self.assertNotEqual(proc.returncode, 0)
        self.assertEqual(self.file_requests(), [])
        self.assertEqual(self.installed_jars(), [])
        self.assertFalse((Path(self.temp.name) / "evil.jar").exists())


class ResolveDepsTest(MeguModsTestBase):
    MYLIB_JAR = b"PK-mylib-bytes-" + b"\x03" * 256

    def write_fabric_jar(self, name, mod_id, depends):
        manifest = {
            "schemaVersion": 1,
            "id": mod_id,
            "version": "1.0.0",
            "depends": depends,
        }
        path = self.mods_dir / name
        with zipfile.ZipFile(path, "w") as archive:
            archive.writestr("fabric.mod.json", json.dumps(manifest))
        return path

    def write_neoforge_jar(self, name, mod_id, deps_toml):
        manifest = (
            "modLoader=\"javafml\"\nloaderVersion=\"[1,)\"\n"
            "[[mods]]\nmodId=\"%s\"\nversion=\"1.0.0\"\n%s"
            % (mod_id, deps_toml))
        path = self.mods_dir / name
        with zipfile.ZipFile(path, "w") as archive:
            archive.writestr("META-INF/neoforge.mods.toml", manifest)
        return path

    def route_mylib(self):
        entry = make_file_entry(self.base, "mylib-2.0.jar", self.MYLIB_JAR)
        self.route_json("/v2/project/mylib/version", [
            make_version("MYLIBID", "2.0", [entry])])
        self.route_file("mylib-2.0.jar", self.MYLIB_JAR)

    def test_resolve_deps_fabric_missing_dep_installs(self):
        self.write_fabric_jar("mymod.jar", "mymod", {"mylib": "*"})
        self.route_mylib()
        proc = self.cli("resolve-deps", "--mods-dir", str(self.mods_dir),
                        "--yes")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("missing: 'mylib' required by 'mymod'", proc.stdout)
        self.assertIn("mylib-2.0.jar", proc.stdout)
        self.assertEqual(
            (self.mods_dir / "mylib-2.0.jar").read_bytes(), self.MYLIB_JAR)
        self.assertEqual(self.file_requests(), ["/files/mylib-2.0.jar"])

    def test_resolve_deps_neoforge_toml_missing_dep(self):
        deps_toml = (
            "[[dependencies.mymod]]\n"
            "modId=\"mylib\"\n"
            "type=\"required\"\n"
            "versionRange=\"[2.0,)\"\n"
            "side=\"BOTH\"\n")
        self.write_neoforge_jar("mymod.jar", "mymod", deps_toml)
        self.route_mylib()
        proc = self.cli("resolve-deps", "--mods-dir", str(self.mods_dir),
                        "--yes")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("mylib-2.0.jar", proc.stdout)
        self.assertTrue((self.mods_dir / "mylib-2.0.jar").is_file())

    def test_resolve_deps_clean_when_nothing_missing(self):
        self.write_fabric_jar("mymod.jar", "mymod",
                              {"minecraft": "*", "neoforge": "*"})
        proc = self.cli("resolve-deps", "--mods-dir", str(self.mods_dir))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("No missing mod dependencies", proc.stdout)
        self.assertEqual(self.file_requests(), [])

    def test_resolve_deps_runtime_provided_never_missing(self):
        self.write_fabric_jar("mymod.jar", "mymod", {
            "minecraft": "*", "java": ">=21", "fabricloader": "*",
            "fabric-api": "*", "neoforge": "*"})
        proc = self.cli("resolve-deps", "--mods-dir", str(self.mods_dir))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("No missing mod dependencies", proc.stdout)
        self.assertEqual(self.file_requests(), [])


if __name__ == "__main__":
    unittest.main()
