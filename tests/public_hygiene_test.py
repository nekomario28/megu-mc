#!/usr/bin/env python3
"""Automated clean-room hygiene & anti-contamination gate (local + CI).

Guards against regressions that would leak personal paths, third-party
franchise trademarks, unredacted credentials, forbidden binaries, or stale
legacy project names into public-facing files.
"""
import pathlib
import re
import subprocess
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]

SKIP_PREFIXES = (
    "evidence/",
    "audit/",
    "assets/",
    "datapacks/",
    "tests/data/",
)

# Files whose unit tests deliberately embed synthetic secret patterns to prove
# redaction logic works.
SYNTHETIC_SECRET_ALLOWLIST = {
    "client-app/src/crash_reporter.rs",
    "client-app/tests/crash_reporter_tests.rs",
    "core/src/crash_report.rs",
    "tests/public_hygiene_test.py",
    "website/check_site.py",
    "website/test_functions.mjs",
}

ALLOWED_TRACKED_BINARIES = {
    "config_packets.bin",
    "website/og-card.png",
}

FORBIDDEN_BINARY_EXTS = {
    ".jar",
    ".zip",
    ".gz",
    ".ogg",
    ".so",
    ".dll",
    ".dylib",
    ".exe",
}

PERSONAL_PATH_RE = re.compile(
    r"/home/yuu\b|MeguminDesktop|/data/tmp/mc-spine|\.t3/worktrees"
)
TRADEMARK_RE = re.compile(
    r"\b(pokemon|pokémon|cobblemon|pixelmon|pikachu|nintendo)\b",
    re.IGNORECASE,
)
SECRET_RE = re.compile(
    r"ghp_[A-Za-z0-9]{20,}"
    r"|github_pat_[A-Za-z0-9_]{20,}"
    r"|AKIA[0-9A-Z]{16}"
    r"|-----BEGIN [A-Z ]*PRIVATE KEY-----"
    r"|sk-[A-Za-z0-9]{24,}"
)
LEGACY_NAME_RE = re.compile(r"mc-spine|MC_SPINE|mc_spine")


def tracked_files():
    out = subprocess.check_output(["git", "ls-files"], cwd=ROOT, text=True)
    return [line for line in out.splitlines() if line]


def iter_audited_text_files():
    for rel in tracked_files():
        if rel.startswith(SKIP_PREFIXES):
            continue
        path = ROOT / rel
        if not path.is_file():
            continue
        if path.suffix.lower() in FORBIDDEN_BINARY_EXTS or rel in ALLOWED_TRACKED_BINARIES:
            continue
        try:
            yield rel, path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue


class PublicHygieneTest(unittest.TestCase):
    def test_no_personal_paths_or_worktree_leaks(self):
        hits = []
        for rel, text in iter_audited_text_files():
            if rel == "tests/public_hygiene_test.py":
                continue
            for lineno, line in enumerate(text.splitlines(), 1):
                m = PERSONAL_PATH_RE.search(line)
                if m:
                    hits.append(f"{rel}:{lineno} ({m.group(0)})")
                    break
        self.assertEqual(hits, [], f"personal path leaks found: {hits}")

    def test_no_third_party_franchise_trademarks(self):
        hits = []
        for rel, text in iter_audited_text_files():
            if rel == "tests/public_hygiene_test.py":
                continue
            for lineno, line in enumerate(text.splitlines(), 1):
                m = TRADEMARK_RE.search(line)
                if m:
                    hits.append(f"{rel}:{lineno} ({m.group(0)})")
                    break
        self.assertEqual(hits, [], f"third-party trademark leaks found: {hits}")

    def test_no_real_secrets_outside_redaction_tests(self):
        hits = []
        for rel, text in iter_audited_text_files():
            if rel in SYNTHETIC_SECRET_ALLOWLIST:
                continue
            for lineno, line in enumerate(text.splitlines(), 1):
                m = SECRET_RE.search(line)
                if m:
                    hits.append(f"{rel}:{lineno} ({m.group(0)[:16]}...)")
                    break
        self.assertEqual(hits, [], f"secret patterns found: {hits}")

    def test_no_legacy_name_in_public_docs_website_or_workflows(self):
        public_roots = {
            "README.md",
            "TRADEMARK.md",
            "LICENSING.md",
            "CONTRIBUTING.md",
            "SECURITY.md",
        }
        hits = []
        for rel, text in iter_audited_text_files():
            is_public_doc = (
                rel in public_roots
                or rel.startswith(("website/", "docs/", ".github/workflows/"))
            )
            if not is_public_doc:
                continue
            for lineno, line in enumerate(text.splitlines(), 1):
                m = LEGACY_NAME_RE.search(line)
                if m:
                    hits.append(f"{rel}:{lineno} ({m.group(0)})")
                    break
        self.assertEqual(hits, [], f"stale legacy names in public docs/CI: {hits}")

    def test_no_forbidden_tracked_binaries(self):
        bad = []
        for rel in tracked_files():
            if rel.startswith(SKIP_PREFIXES) or rel in ALLOWED_TRACKED_BINARIES:
                continue
            ext = pathlib.Path(rel).suffix.lower()
            if ext in FORBIDDEN_BINARY_EXTS:
                bad.append(rel)
        self.assertEqual(bad, [], f"unexpected tracked binaries: {bad}")


if __name__ == "__main__":
    unittest.main()
