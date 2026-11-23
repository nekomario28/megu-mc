"""Hermetic tests for portable Java toolchain discovery."""

from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))

from java_toolchain import executable_name, require_java_home, resolve_java_home, tool_path


class JavaToolchainTests(unittest.TestCase):
    def make_jdk(self, parent: Path, *, windows: bool = False) -> Path:
        home = parent / "jdk"
        (home / "bin").mkdir(parents=True)
        for name in ("java", "javac"):
            tool_path(home, name, "win32" if windows else "linux").touch()
        return home

    def test_megu_java_home_precedes_java_home(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            megu = self.make_jdk(root / "megu")
            standard = self.make_jdk(root / "standard")
            found = resolve_java_home(environ={"MEGU_JAVA_HOME": str(megu),
                                               "JAVA_HOME": str(standard)},
                                      platform="linux", which=lambda _: None,
                                      globber=lambda _: [])
        self.assertEqual(found, megu)

    def test_java_home_is_used_when_megu_override_is_absent(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = self.make_jdk(Path(tmp))
            found = resolve_java_home(environ={"JAVA_HOME": str(home)}, platform="linux",
                                      which=lambda _: None, globber=lambda _: [])
        self.assertEqual(found, home)

    def test_path_lookup_finds_paired_java_and_javac(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = self.make_jdk(Path(tmp))
            lookup = {"java": str(tool_path(home, "java", "linux")),
                      "javac": str(tool_path(home, "javac", "linux"))}
            found = resolve_java_home(environ={}, platform="linux",
                                      which=lambda name: lookup.get(name), globber=lambda _: [])
        self.assertEqual(found, home)

    def test_windows_uses_executable_suffix(self):
        self.assertEqual(executable_name("java", "win32"), "java.exe")
        with tempfile.TemporaryDirectory() as tmp:
            home = self.make_jdk(Path(tmp), windows=True)
            lookup = {"java.exe": str(tool_path(home, "java", "win32")),
                      "javac.exe": str(tool_path(home, "javac", "win32"))}
            found = resolve_java_home(environ={}, platform="win32",
                                      which=lambda name: lookup.get(name), globber=lambda _: [])
        self.assertEqual(found, home)

    def test_missing_jdk_is_reported_clearly(self):
        self.assertIsNone(resolve_java_home(environ={}, platform="linux",
                                            which=lambda _: None, globber=lambda _: []))
        with patch("java_toolchain.resolve_java_home", return_value=None):
            with self.assertRaisesRegex(RuntimeError, "MEGU_JAVA_HOME or JAVA_HOME"):
                require_java_home()


if __name__ == "__main__":
    unittest.main()
