"""Portable discovery for a Java development kit."""

from __future__ import annotations

import glob
import os
from pathlib import Path
import shutil
import subprocess
import sys


def executable_name(name: str, platform: str | None = None) -> str:
    """Return a Java tool filename for the selected platform."""
    platform = sys.platform if platform is None else platform
    return name + (".exe" if platform == "win32" else "")


def tool_path(java_home: Path | str, name: str, platform: str | None = None) -> Path:
    return Path(java_home) / "bin" / executable_name(name, platform)


def _candidate_homes(environ: dict[str, str], platform: str, globber, runner) -> list[Path]:
    homes: list[Path] = []
    for key in ("MEGU_JAVA_HOME", "JAVA_HOME"):
        value = environ.get(key)
        if value:
            homes.append(Path(value).expanduser())

    if platform == "darwin":
        for arguments in (("/usr/libexec/java_home", "-v", "21"), ("/usr/libexec/java_home",)):
            try:
                result = runner(arguments, check=False, capture_output=True, text=True, timeout=5)
            except (OSError, subprocess.SubprocessError):
                continue
            if result.returncode == 0 and result.stdout.strip():
                homes.append(Path(result.stdout.strip()))
    elif platform.startswith("linux"):
        patterns = (
            "/usr/lib/jvm/java-21-*", "/usr/lib/jvm/default-java", "/usr/lib/jvm/*",
            "/usr/lib64/jvm/*", "/usr/java/*", "/opt/java/*", "/opt/jdk/*",
        )
        for pattern in patterns:
            homes.extend(Path(item) for item in sorted(globber(pattern)))
    elif platform == "win32":
        program_files = environ.get("ProgramFiles", r"C:\Program Files")
        patterns = (
            os.path.join(program_files, "Java", "*"),
            os.path.join(program_files, "Eclipse Adoptium", "*"),
            os.path.join(program_files, "Microsoft", "jdk-*"),
        )
        for pattern in patterns:
            homes.extend(Path(item) for item in sorted(globber(pattern)))
    return homes


def resolve_java_home(*, environ: dict[str, str] | None = None,
                      platform: str | None = None,
                      which=shutil.which, globber=glob.glob,
                      runner=subprocess.run) -> Path | None:
    """Find a JDK containing java and javac, honoring explicit homes first."""
    environ = os.environ if environ is None else environ
    platform = sys.platform if platform is None else platform
    name = lambda tool: executable_name(tool, platform)

    discovered_homes = _candidate_homes(environ, platform, globber, runner)
    explicit_count = sum(bool(environ.get(key)) for key in ("MEGU_JAVA_HOME", "JAVA_HOME"))
    # A PATH installation is a valid candidate after explicit overrides.
    java_on_path = which(name("java"))
    javac_on_path = which(name("javac"))
    if java_on_path and javac_on_path:
        java_bin = Path(java_on_path).parent
        javac_bin = Path(javac_on_path).parent
        if java_bin == javac_bin:
            path_home = java_bin.parent
        else:
            path_home = None
    else:
        path_home = None

    homes = discovered_homes[:explicit_count]
    if path_home is not None:
        homes.append(path_home)
    homes.extend(discovered_homes[explicit_count:])

    seen: set[str] = set()
    for home in homes:
        identity = os.path.normcase(os.path.abspath(home))
        if identity in seen:
            continue
        seen.add(identity)
        if all(tool_path(home, tool, platform).is_file() for tool in ("java", "javac")):
            return home
    return None


def require_java_home(**kwargs) -> Path:
    """Return the discovered JDK home or raise an actionable error."""
    home = resolve_java_home(**kwargs)
    if home is None:
        raise RuntimeError(
            "Java JDK not found: set MEGU_JAVA_HOME or JAVA_HOME, or install java and javac on PATH"
        )
    return home
