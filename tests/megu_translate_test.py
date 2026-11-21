#!/usr/bin/env python3
"""Local-only, end-to-end tests for the opt-in resource-pack translator."""

from contextlib import contextmanager
import importlib.util
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import subprocess
import sys
import tempfile
from threading import Thread
import unittest
from unittest import mock
import zipfile


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "tools" / "megu_translate.py"
SPEC = importlib.util.spec_from_file_location("megu_translate", SCRIPT)
translator = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(translator)


class FakeChatHandler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        size = int(self.headers["Content-Length"])
        request = json.loads(self.rfile.read(size))
        self.server.requests.append((self.path, request))
        if self.path == "/api/generate":
            response = {"done": True}
        else:
            source = json.loads(request["messages"][1]["content"])
            translations = {}
            for key, english in source.items():
                if key.endswith(".bad"):
                    translations[key] = "所有者"  # Corrupts the required %1$s.
                else:
                    translations[key] = "翻訳 " + english
            response = {"choices": [{"message": {
                "content": json.dumps(translations, ensure_ascii=False),
            }}]}
        payload = json.dumps(response).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


@contextmanager
def local_server():
    server = HTTPServer(("127.0.0.1", 0), FakeChatHandler)
    server.requests = []
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server, f"http://127.0.0.1:{server.server_port}/v1/chat/completions"
    finally:
        server.shutdown()
        thread.join(timeout=3)
        server.server_close()


class MeguTranslateTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="megu-translate-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.mods = self.root / "mods"
        self.mods.mkdir()
        self.pack = self.root / "megu-auto-lang-ja_jp.zip"
        self.cache = self.root / "translate-cache.json"

    def jar(self, name, members):
        path = self.mods / name
        with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
            for filename, content in members.items():
                if isinstance(content, dict):
                    content = json.dumps(content, ensure_ascii=False)
                archive.writestr(filename, content)
        return path

    def fixtures(self):
        self.jar("already_ja.jar", {
            "assets/donemod/lang/en_us.json": {"ui.done": "Ready"},
            "assets/donemod/lang/ja_jp.json": {"ui.done": "準備完了"},
        })
        self.jar("partial_mod.jar", {
            "assets/partialmod/lang/en_us.json": {
                "ui.existing.one": "Already done",
                "ui.existing.two": "Also done",
                "ui.missing.simple": "Open menu",
                "ui.missing.quantity": "Count %d §a!",
                "ui.missing.bad": "Owner %1$s",
            },
            "assets/partialmod/lang/ja_jp.json": {
                "ui.existing.one": "完了",
                "ui.existing.two": "完了済み",
            },
        })

    def cli(self, *extra):
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--mods-dir", str(self.mods),
             "--out", str(self.pack), "--cache", str(self.cache), "--json", *extra],
            capture_output=True, text=True, timeout=30, check=False,
        )

    def test_only_missing_keys_translated_bad_format_falls_back_and_cache_reused(self):
        self.fixtures()
        with local_server() as (server, endpoint):
            first = self.cli("--endpoint", endpoint)
            self.assertEqual(first.returncode, 0, first.stderr)
            info = json.loads(first.stdout)
            self.assertEqual(info["mods_scanned"], 2)
            self.assertEqual(info["already_translated_keys"], 3)
            self.assertEqual(info["missing_keys"], 3)
            self.assertEqual(info["cache_hits"], 0)
            self.assertEqual(info["translated_count"], 2)
            self.assertEqual(info["fallback_count"], 1)
            self.assertEqual(len(server.requests), 1)
            _path, request = server.requests[0]
            submitted = json.loads(request["messages"][1]["content"])
            self.assertEqual(set(submitted), {
                "partialmod:ui.missing.simple",
                "partialmod:ui.missing.quantity",
                "partialmod:ui.missing.bad",
            })
            self.assertEqual(submitted["partialmod:ui.missing.bad"], "Owner %1$s")

            with zipfile.ZipFile(self.pack) as archive:
                self.assertEqual(archive.namelist(), [
                    "assets/partialmod/lang/ja_jp.json", "pack.mcmeta",
                ])
                metadata = json.loads(archive.read("pack.mcmeta"))
                self.assertEqual(metadata["pack"]["pack_format"], 34)
                self.assertEqual(metadata["pack"]["supported_formats"], [34, 64])
                translated = json.loads(archive.read("assets/partialmod/lang/ja_jp.json"))
                self.assertEqual(translated, {
                    "ui.missing.simple": "翻訳 Open menu",
                    "ui.missing.quantity": "翻訳 Count %d §a!",
                    "ui.missing.bad": "Owner %1$s",
                })
                self.assertEqual(archive.getinfo("pack.mcmeta").date_time,
                                 (1980, 1, 1, 0, 0, 0))
            first_zip = self.pack.read_bytes()

            second = self.cli("--endpoint", endpoint)
            self.assertEqual(second.returncode, 0, second.stderr)
            repeated = json.loads(second.stdout)
            self.assertEqual(repeated["cache_hits"], 3)
            self.assertEqual(repeated["to_translate"], 0)
            self.assertEqual(repeated["fallback_count"], 1)
            self.assertEqual(len(server.requests), 1, "cache run performed a network call")
            self.assertEqual(self.pack.read_bytes(), first_zip)

    def test_fully_translated_jars_generate_metadata_only_without_network(self):
        self.jar("already_ja.jar", {
            "assets/donemod/lang/en_us.json": {"ui.done": "Ready"},
            "assets/donemod/lang/ja_jp.json": {"ui.done": "準備完了"},
        })
        with local_server() as (server, endpoint):
            result = self.cli("--endpoint", endpoint)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)["missing_keys"], 0)
            self.assertEqual(server.requests, [])
        with zipfile.ZipFile(self.pack) as archive:
            self.assertEqual(archive.namelist(), ["pack.mcmeta"])

    def test_english_target_does_not_retranslate_original_entries(self):
        self.fixtures()
        with local_server() as (server, endpoint):
            result = self.cli("--lang", "en_us", "--endpoint", endpoint)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)["missing_keys"], 0)
            self.assertEqual(server.requests, [])

    def test_dry_run_reports_exact_delta_and_ram_tier_without_network_or_writes(self):
        self.fixtures()
        with local_server() as (server, endpoint):
            result = self.cli("--dry-run", "--endpoint", endpoint)
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(result.stdout)
            self.assertTrue(report["dry_run"])
            self.assertEqual(report["missing_keys"], 3)
            self.assertEqual(report["already_translated_keys"], 3)
            self.assertEqual(report["cache_hits"], 0)
            self.assertEqual(report["to_translate"], 3)
            self.assertIn(report["recommended_model_tier"], {
                "too_low_ram", "small_2b_4b", "medium_8b_12b", "large_ok", "unknown",
            })
            self.assertEqual(server.requests, [])
            self.assertFalse(self.cache.exists())
            self.assertFalse(self.pack.exists())

    def test_non_loopback_endpoint_rejected_before_any_network(self):
        self.fixtures()
        result = self.cli("--endpoint", "https://evil.example/v1")
        self.assertEqual(result.returncode, 2)
        self.assertIn("Non-loopback", result.stderr)
        self.assertFalse(self.cache.exists())
        self.assertFalse(self.pack.exists())
        translator.validate_endpoint("https://evil.example/v1", allow_remote=True)
        for endpoint in ("http://localhost:11434/v1/chat/completions",
                         "http://192.168.1.2:11434/v1/chat/completions",
                         "file:///tmp/llm", "http://127.0.0.1.evil.example/v1"):
            with self.subTest(endpoint=endpoint):
                with self.assertRaises(translator.TranslationError):
                    translator.validate_endpoint(endpoint, allow_remote=False)

    def test_default_endpoint_is_numeric_loopback_and_remote_requires_opt_in(self):
        self.assertEqual(translator.DEFAULT_ENDPOINT,
                         "http://127.0.0.1:11434/v1/chat/completions")
        self.assertEqual(translator.validate_endpoint(
            translator.DEFAULT_ENDPOINT, allow_remote=False).hostname, "127.0.0.1")
        self.fixtures()
        remote = "https://example.invalid/v1/chat/completions"
        refused = self.cli("--dry-run", "--endpoint", remote)
        self.assertEqual(refused.returncode, 2)
        allowed = self.cli("--dry-run", "--endpoint", remote, "--allow-remote-llm")
        self.assertEqual(allowed.returncode, 0, allowed.stderr)
        self.assertEqual(json.loads(allowed.stdout)["to_translate"], 3)
        self.assertFalse(self.pack.exists())
        self.assertFalse(self.cache.exists())

    def test_unsafe_zip_members_fail_closed_and_keep_existing_output(self):
        self.pack.write_bytes(b"keep-this-archive")
        for member in ("../escape", "/absolute", "assets\\evil", "assets/../escape"):
            with self.subTest(member=member):
                path = self.jar("evil_traversal.jar", {
                    "assets/partialmod/lang/en_us.json": {"key": "Value"},
                    member: "malicious content",
                })
                result = self.cli("--dry-run")
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("Unsafe ZIP member", result.stderr)
                self.assertEqual(self.pack.read_bytes(), b"keep-this-archive")
                path.unlink()
        with self.assertRaises(translator.TranslationError):
            translator.validate_member(zipfile.ZipInfo("bad\x00path"))

    def test_oversized_member_is_rejected(self):
        member = zipfile.ZipInfo("assets/partialmod/lang/en_us.json")
        member.file_size = translator.MAX_MEMBER_BYTES + 1
        with self.assertRaises(translator.TranslationError):
            translator.validate_member(member)

    def test_format_guard_preserves_token_multisets_and_controls(self):
        good = "項目 %1$s / %2$d / %d / %f / %.2f / %s %% §a\\n\n"
        source = "Item %1$s / %2$d / %d / %f / %.2f / %s %% §a\\n\n"
        self.assertTrue(translator.validate_translation(source, good))
        for broken in (
            good.replace("%1$s", ""),
            good.replace("%.2f", "%f"),
            good.replace("§a", "§b"),
            good.replace("\\n", ""),
            good.replace("\n", ""),
            good + "%s",
            good + "§",
            good + "\x00",
            " ",
            "x" * 9000,
        ):
            with self.subTest(broken=repr(broken[:40])):
                self.assertFalse(translator.validate_translation(source, broken))

    def test_explicit_ollama_unload_hint_sent_only_after_translation(self):
        self.fixtures()
        with local_server() as (server, endpoint):
            result = self.cli("--endpoint", endpoint, "--unload-after")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(json.loads(result.stdout)["unload_hint_sent"])
            self.assertEqual([path for path, _ in server.requests], [
                "/v1/chat/completions", "/api/generate",
            ])
            self.assertEqual(server.requests[-1][1]["keep_alive"], 0)

    def test_cache_key_is_stable_and_includes_locale_and_model(self):
        expected = translator.cache_key("key", "English", "ja_jp", "plamo-2-translate")
        self.assertEqual(len(expected), 64)
        self.assertNotEqual(expected,
                            translator.cache_key("key", "English", "de_de", "plamo-2-translate"))
        self.assertNotEqual(expected,
                            translator.cache_key("key", "English", "ja_jp", "other-model"))

    def test_conflicting_english_keys_are_rejected(self):
        self.jar("one.jar", {"assets/shared/lang/en_us.json": {"name": "One"}})
        self.jar("two.jar", {"assets/shared/lang/en_us.json": {"name": "Two"}})
        result = self.cli("--dry-run")
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("Conflicting English text", result.stderr)


if __name__ == "__main__":
    unittest.main()
